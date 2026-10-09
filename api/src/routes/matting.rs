//! Background-removal (matting) routes. Auth-gated `/remove`, ungated
//! `/health`. Thin proxy onto `matting-server` (Acly/vision.cpp BiRefNet /
//! RMBG-2.0) via `crate::matting::MattingClient`.
//!
//! `/remove` accepts EITHER a single raw image body (Content-Type image/*) OR a
//! multipart body with one-or-more repeated `images` file parts (batch). All the
//! tuning knobs ride as query-string params and are forwarded to the upstream.
//! The response is JSON `{ count, results:[{ url }], generation_id, backend,
//! elapsed_seconds }`: the cutouts are written to the gallery and served from
//! `/api/v1/generations/{id}/file/{n}`, never inlined as base64.
//!
//! That makes gallery storage a hard requirement here, not an option — an id
//! pointing at nothing is worse than a loud 503, so `/remove` refuses before it
//! touches the engine when `generations_dir` is unset.
//!
//! Mirrors routes/images.rs for the request-shape detection + multipart parsing,
//! but without the GPU-gate / heavy-slot dance (matting is ungated).

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine as _;
use bytes::Bytes;
use koblem_client::types::{MattingRemoveResponse, MattingResultImage};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::auth::require_auth;
use crate::matting::RemoveOptions;
use crate::routes::tier2_helpers::{
    OutputBytes, collect_multipart_images, generation_file_url, generations_root,
    image_filename_for, persist_generation,
};

/// 64 MB body cap — covers a handful of large source images in one batch.
const MATTING_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/matting/remove", post(remove))
        .route("/api/v1/matting/health", get(health))
        .layer(DefaultBodyLimit::max(MATTING_BODY_LIMIT))
}

/// Query knobs for `/remove`, parsed from the URL. Every field is optional and
/// maps 1:1 onto `RemoveOptions`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct RemoveQuery {
    backend: Option<String>,
    process_res: Option<u32>,
    sensitivity: Option<f32>,
    mask_blur: Option<i32>,
    mask_offset: Option<i32>,
    refine: Option<bool>,
    invert: Option<bool>,
    bg_mode: Option<String>,
    bg_color: Option<String>,
    mask_only: Option<bool>,
}

impl From<RemoveQuery> for RemoveOptions {
    fn from(q: RemoveQuery) -> Self {
        RemoveOptions {
            backend: q.backend,
            process_res: q.process_res,
            sensitivity: q.sensitivity,
            mask_blur: q.mask_blur,
            mask_offset: q.mask_offset,
            refine: q.refine,
            invert: q.invert,
            bg_mode: q.bg_mode,
            bg_color: q.bg_color,
            mask_only: q.mask_only,
        }
    }
}

/// `POST /api/v1/matting/remove` — single raw image or multipart batch.
async fn remove(
    State(state): State<AppState>,
    Query(query): Query<RemoveQuery>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.matting.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "matting service not configured (set MATTING_SERVICE_URL)",
        );
    }
    // The cutouts are delivered as gallery URLs, so refuse now rather than
    // render something with nowhere to live and hand back a dangling id.
    if generations_root(&state, "matting").await.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "gallery storage is not configured — set `generations_dir` in Settings; \
             background removal serves its cutouts from the gallery",
        );
    }

    // Snapshot the restorable knobs before `query` is consumed into RemoveOptions.
    // The image itself rides in the body and can't be replayed into the form.
    let restore_meta = crate::routes::tier2_helpers::meta_from_request(
        &query,
        &[("had_image_inputs", json!(true))],
    );
    let opts: RemoveOptions = query.into();

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let is_multipart = content_type.starts_with("multipart/");

    // Collect the input images: a multipart batch (repeated `images`/`image`
    // file parts) or a single raw body.
    let images: Vec<(String, Bytes)> = if is_multipart {
        let mp = match Multipart::from_request(request, &state).await {
            Ok(mp) => mp,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
        };
        match collect_multipart_images(mp).await {
            Ok(imgs) if !imgs.is_empty() => imgs,
            Ok(_) => return err(StatusCode::BAD_REQUEST, "no image parts in multipart body"),
            Err(e) => return err(StatusCode::BAD_REQUEST, &e),
        }
    } else {
        let bytes = match to_bytes(request.into_body(), MATTING_BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
        };
        if bytes.is_empty() {
            return err(StatusCode::BAD_REQUEST, "empty image body");
        }
        vec![(image_filename_for(&content_type), bytes)]
    };

    match state.matting.remove_batch(&images, &opts).await {
        Ok(r) => {
            // One file per result, in order — `results[i]` is `outputs[i]`, so a
            // single undecodable entry fails the request rather than silently
            // shifting every url after it onto the wrong cutout.
            let b64 = base64::engine::general_purpose::STANDARD;
            let mut files: Vec<OutputBytes> = Vec::with_capacity(r.results.len());
            for x in &r.results {
                match b64.decode(&x.png_base64) {
                    Ok(bytes) => files.push(OutputBytes {
                        ext: "png".to_string(),
                        mime: "image/png".to_string(),
                        kind: "image".to_string(),
                        bytes: Bytes::from(bytes),
                    }),
                    Err(e) => {
                        return err(
                            StatusCode::BAD_GATEWAY,
                            &format!("matting-server returned an undecodable cutout: {e}"),
                        );
                    }
                }
            }
            if files.is_empty() {
                return err(
                    StatusCode::BAD_GATEWAY,
                    "matting-server returned no cutouts",
                );
            }
            let Some(generation_id) =
                persist_generation(&state, "matting", "remove", restore_meta, &files).await
            else {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "background removed but saving to the gallery failed — check the configured \
                     directory is writable",
                );
            };
            Json(MattingRemoveResponse {
                count: r.count,
                results: (0..files.len())
                    .map(|i| MattingResultImage {
                        url: generation_file_url(&generation_id, i),
                    })
                    .collect(),
                backend: r.backend,
                elapsed_seconds: r.elapsed_seconds,
                generation_id,
            })
            .into_response()
        }
        Err(crate::matting::Error::NotConfigured) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "matting service not configured",
        ),
        Err(crate::matting::Error::Upstream(status, body)) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            err(code, &format!("matting-server: {body}"))
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("matting-server: {e}")),
    }
}

/// `GET /api/v1/matting/health` — ungated liveness probe (UI bootstrap).
async fn health(State(state): State<AppState>) -> Response {
    if !state.matting.is_configured() {
        return Json(json!({
            "healthy": false,
            "worker_alive": false,
            "configured": false,
        }))
        .into_response();
    }
    match state.matting.health().await {
        Ok(h) => Json(json!({
            "healthy": h.healthy,
            "status": h.status,
            "worker_alive": h.worker_alive,
            "backend": h.backend,
            "native_size": h.native_size,
            "in_flight": h.in_flight,
            "configured": true,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "healthy": false,
            "worker_alive": false,
            "configured": true,
        }))
        .into_response(),
    }
}

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}
