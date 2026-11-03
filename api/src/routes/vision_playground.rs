//! Tier-2 embeddings playground (EmbeddingGemma-2). Auth-gated, logs to
//! gpu_logs. Image work goes through the `vision` gate line; text through the
//! always-on text instance.

use std::time::Instant;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use koblem_client::embed::{EG2_MODEL, EmbedTask};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::tier1_gpu::{classify_images, embed_err};
use crate::routes::tier2_helpers::{extract_caller, log_async, save_sample};
use crate::vision::{Error, ImagePart, cosine_matrix};

const VISION_BODY_LIMIT: usize = 100 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/vision/embed", post(embed))
        .route("/api/v1/vision/classify", post(classify))
        .route("/api/v1/vision/text-embed", post(text_embed))
        .route(
            "/api/v1/vision/classify-from-embeddings",
            post(classify_from_embeddings),
        )
        .layer(DefaultBodyLimit::max(VISION_BODY_LIMIT))
}

/// `images` parts, `prompts` fields and the `save_sample` switch. The first
/// image is kept as a replay sample when asked.
struct Upload {
    images: Vec<ImagePart>,
    prompts: Vec<String>,
}

async fn read_upload(state: &AppState, mut multipart: Multipart) -> Result<Upload, Response> {
    let mut images: Vec<ImagePart> = Vec::new();
    let mut prompts = Vec::new();
    let mut save = false;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(bad_request(e.to_string())),
        };
        match field.name().unwrap_or("") {
            "images" => {
                let filename = field.file_name().unwrap_or("image.png").to_string();
                let mime = field.content_type().map(str::to_string);
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| bad_request(e.to_string()))?;
                if save && images.is_empty() {
                    let mime = mime.clone().unwrap_or_else(|| "image/png".to_string());
                    let ext = mime.split('/').nth(1).unwrap_or("png").to_string();
                    let _ = save_sample(
                        &state.db,
                        &state.samples_dir,
                        "image_in",
                        &mime,
                        &ext,
                        &bytes,
                    )
                    .await;
                }
                images.push(ImagePart {
                    filename,
                    mime,
                    bytes,
                });
            }
            "prompts" => {
                if let Ok(t) = field.text().await {
                    prompts.push(t);
                }
            }
            "save_sample" => {
                save = matches!(
                    field.text().await.unwrap_or_default().as_str(),
                    "true" | "1"
                );
            }
            _ => {
                let _ = field.text().await;
            }
        }
    }
    Ok(Upload { images, prompts })
}

async fn authed(state: &AppState, headers: &HeaderMap) -> bool {
    require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_ok()
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg.into()}))).into_response()
}

/// Log the call either way and turn the outcome into the response.
fn finish(
    state: &AppState,
    caller: Option<String>,
    endpoint: &str,
    request_meta: Value,
    start: Instant,
    gate_wait_ms: i64,
    outcome: Result<(Value, Value), Error>,
) -> Response {
    let duration_ms = start.elapsed().as_millis() as i64;
    match outcome {
        Ok((mut body, response_meta)) => {
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: endpoint.to_string(),
                    request_meta,
                    response_meta,
                    sample_id: None,
                    gate_wait_ms,
                    duration_ms,
                    status: 200,
                    caller,
                },
            );
            body["duration_ms"] = json!(duration_ms);
            body["gate_wait_ms"] = json!(gate_wait_ms);
            body["model"] = json!(EG2_MODEL);
            Json(body).into_response()
        }
        Err(e) => {
            let error = e.to_string();
            let response = embed_err(e);
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: endpoint.to_string(),
                    request_meta,
                    response_meta: json!({"error": error}),
                    sample_id: None,
                    gate_wait_ms: 0,
                    duration_ms,
                    status: response.status().as_u16() as i32,
                    caller,
                },
            );
            warn!(endpoint, status = %response.status(), "embeddings playground call failed");
            response
        }
    }
}

fn shape(rows: &[Vec<f32>]) -> Value {
    json!([rows.len(), rows.first().map(Vec::len).unwrap_or(0)])
}

async fn embed(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if !authed(&state, &headers).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let caller = extract_caller(&headers);
    let start = Instant::now();
    let upload = match read_upload(&state, multipart).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    if upload.images.is_empty() {
        return bad_request("need images");
    }
    let request_meta = json!({"image_count": upload.images.len()});
    let mut gate_wait_ms = 0;
    let outcome = state
        .vision
        .embed_images(&upload.images)
        .await
        .map(|(_guard, vectors, wait)| {
            gate_wait_ms = wait.as_millis() as i64;
            let meta = json!({"embeddings_shape": shape(&vectors)});
            (json!({"embeddings": vectors}), meta)
        });
    finish(
        &state,
        caller,
        "/api/v1/vision/embed",
        request_meta,
        start,
        gate_wait_ms,
        outcome,
    )
}

async fn classify(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if !authed(&state, &headers).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let caller = extract_caller(&headers);
    let start = Instant::now();
    let upload = match read_upload(&state, multipart).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    if upload.images.is_empty() || upload.prompts.is_empty() {
        return bad_request("need images and prompts");
    }
    let request_meta = json!({
        "image_count": upload.images.len(),
        "prompts": upload.prompts,
    });
    let mut gate_wait_ms = 0;
    let outcome = classify_images(&state, &upload.images, &upload.prompts)
        .await
        .map(|(scores, wait)| {
            gate_wait_ms = wait.as_millis() as i64;
            let meta = json!({"scores_shape": shape(&scores)});
            (json!({"scores": scores}), meta)
        });
    finish(
        &state,
        caller,
        "/api/v1/vision/classify",
        request_meta,
        start,
        gate_wait_ms,
        outcome,
    )
}

#[derive(Deserialize)]
struct TextEmbedReq {
    prompts: Vec<String>,
    /// Default `raw`: embed each prompt exactly as typed.
    #[serde(default)]
    task: Option<EmbedTask>,
}

async fn text_embed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TextEmbedReq>,
) -> Response {
    if !authed(&state, &headers).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if req.prompts.is_empty() {
        return bad_request("prompts must be non-empty");
    }
    let caller = extract_caller(&headers);
    let start = Instant::now();
    let task = req.task.unwrap_or(EmbedTask::Raw);
    let request_meta =
        json!({"prompt_count": req.prompts.len(), "prompts": req.prompts, "task": task});
    let mut gate_wait_ms = 0;
    let outcome = state
        .vision
        .embed_text_inputs(&req.prompts, task, &[], None)
        .await
        .map(|out| {
            gate_wait_ms = out.served.gate_wait_ms().unwrap_or(0) as i64;
            let meta = json!({"embeddings_shape": shape(&out.embeddings)});
            (json!({"embeddings": out.embeddings}), meta)
        });
    finish(
        &state,
        caller,
        "/api/v1/vision/text-embed",
        request_meta,
        start,
        gate_wait_ms,
        outcome,
    )
}

#[derive(Deserialize)]
struct ClassifyFromEmbedReq {
    image_embeddings: Vec<Vec<f32>>,
    prompts: Vec<String>,
}

async fn classify_from_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ClassifyFromEmbedReq>,
) -> Response {
    if !authed(&state, &headers).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if req.image_embeddings.is_empty() || req.prompts.is_empty() {
        return bad_request("image_embeddings and prompts must both be non-empty");
    }
    let caller = extract_caller(&headers);
    let start = Instant::now();
    let request_meta = json!({
        "embeddings_shape": shape(&req.image_embeddings),
        "prompts": req.prompts,
    });
    let outcome = match state.vision.prompt_vectors(&req.prompts).await {
        Ok(prompt_vecs) => cosine_matrix(&req.image_embeddings, &prompt_vecs),
        Err(e) => Err(e),
    }
    .map(|scores| {
        let meta = json!({"scores_shape": shape(&scores)});
        (json!({"scores": scores}), meta)
    });
    finish(
        &state,
        caller,
        "/api/v1/vision/classify-from-embeddings",
        request_meta,
        start,
        0,
        outcome,
    )
}
