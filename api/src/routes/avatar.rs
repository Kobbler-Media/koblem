//! LongCat-Avatar routes — image + audio → talking video. Auth-gated.
//!
//! Pipeline (the kid-presses-Go flow):
//!   1. Materialise the reference portrait (upload OR url OR data-uri).
//!   2. Resolve the spoken script. If the request asks for `Tts { script }`
//!      we use it as-is; `Tts { script_from_brief }` calls the main 9B first.
//!   3. Render TTS audio via tts-breeze (the engine field on the request is
//!      ignored — chatterbox was retired) — picks up its own GPU guard
//!      internally and drops it.
//!   4. Build the supervisor `/generate` body and hand it to the global GPU
//!      queue. 202 and a job id; `queue::adapters::avatar` renders and saves.
//!
//! 🔴 Steps 1–3 stay HERE and stay synchronous. Both blobs go into the
//! content-addressed asset store before the row exists and the payload carries
//! `asset:<hash>` refs the adapter resolves at dispatch — an upload's bytes went
//! with the request, and a `script_from_brief` that ran hours later would have no
//! LLM step left to run.
//!
//! 🔴 **There is no reattach for this engine and there must not be one.**
//! `/generate` is synchronous, keeps no job record and writes no file, so a
//! restart mid-render loses the bytes; the adapter fails the row saying exactly
//! that rather than silently re-rendering minutes of GPU.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use koblem_client::types::{
    AvatarAsset, AvatarAssetsResponse, AvatarAudioSource, AvatarGenerateRequest, AvatarImageSource,
    AvatarOffload, AvatarOrientation, AvatarProps, AvatarScriptFromLlmRequest,
    AvatarScriptFromLlmResponse, Job as JobRow, JobKind, JobState,
};
use serde_json::{Value, json};
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::director_assets;
use crate::repositories::avatar_assets;
use crate::repositories::gpu_jobs as queue_store;
use crate::routes::engine_cancel;

/// 64 MB body cap — portrait + audio. Generous enough for a 16-bit 16 kHz
/// 60-second wav + a 4K PNG, well under the supervisor's own limits.
const AVATAR_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/avatar/generate", post(generate))
        .route("/api/v1/avatar/jobs/{id}", get(job))
        .route("/api/v1/avatar/active", get(active))
        .route("/api/v1/avatar/script-from-llm", post(script_from_llm))
        .route("/api/v1/avatar/props", get(props))
        .route("/api/v1/avatar/file/{name}", get(serve_file))
        .route("/api/v1/avatar/cancel", post(cancel_current))
        .route("/api/v1/avatar/assets", get(list_assets).post(upload_asset))
        .route(
            "/api/v1/avatar/assets/{id}",
            axum::routing::delete(delete_asset),
        )
        .route("/api/v1/avatar/assets/{id}/file", get(serve_asset_file))
        .layer(DefaultBodyLimit::max(AVATAR_BODY_LIMIT))
}

/// `POST /api/v1/avatar/cancel` — cancel the render on the card, or the next one queued.
///
/// 🔴 Not the supervisor's `/v1/admin/unload {force:true}` any more. That is a force-unload of
/// *whatever it is rendering*, not a cancel of a named job, and with no job model koblem cannot
/// prove the in-flight render is this one — SIGKILLing someone else's is worse than letting this
/// one finish. A cancel that lands mid-render therefore stops the queue from waiting, not the
/// supervisor from rendering; the adapter keeps the clip and says so in a warning.
async fn cancel_current(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(
        &state,
        JobKind::Avatar,
        "no avatar render is queued or running",
    )
    .await
}

/// `GET /api/v1/avatar/jobs/{id}` — poll a queued render.
async fn job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such avatar job");
    };
    if row.kind != JobKind::Avatar {
        return err(StatusCode::NOT_FOUND, "that job is not an avatar render");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/avatar/active` — the queued or running render, so a reloaded page can find the job
/// it lost without having kept the id.
async fn active(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let live = queue_store::live(&state.db).await.unwrap_or_default();
    let pending = queue_store::pending(&state.db).await.unwrap_or_default();
    let found = live
        .into_iter()
        .chain(pending)
        .find(|j| j.kind == JobKind::Avatar);
    match found {
        Some(row) => Json(json!({ "job": view(&state, &row).await })).into_response(),
        None => Json(json!({ "job": Value::Null })).into_response(),
    }
}

/// Render a queue row in the shape this page already speaks.
async fn view(state: &AppState, row: &JobRow) -> Value {
    let result = queue_store::detail(&state.db, &row.id)
        .await
        .ok()
        .flatten()
        .and_then(|(_, _, result)| result)
        .unwrap_or(Value::Null);
    json!({
        "id": row.id,
        // `collecting` has no equivalent in this engine's vocabulary, and the clip is not saved
        // yet — the card is free but the page still has nothing to play.
        "state": match row.state {
            JobState::Queued | JobState::Held => "queued",
            JobState::Running | JobState::Collecting => "rendering",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        },
        "phase": row.progress.phase,
        "clip": result.get("clip").cloned().unwrap_or(Value::Null),
        "warnings": row.warnings,
        "error": row.error,
        "queued_ms": row.queued_ms,
        "gate_wait_ms": row.admit_ms,
        "render_ms": row.render_ms,
        "blocked_reason": row.blocked_reason,
        "eta_start": row.eta_start,
    })
}

// ── handlers ──────────────────────────────────────────────────────────

/// `POST /api/v1/avatar/generate` — JSON or multipart (`request` JSON part +
/// optional `image` / `audio` file parts).
async fn generate(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|ct| ct.starts_with("multipart/"));

    let (req, image_bytes, audio_bytes) = if is_multipart {
        let mp = match Multipart::from_request(request, &state).await {
            Ok(mp) => mp,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
        };
        match parse_avatar_multipart(mp).await {
            Ok(parsed) => match parsed.request {
                Some(r) => (r, parsed.image, parsed.audio),
                None => return err(StatusCode::BAD_REQUEST, "missing `request` part"),
            },
            Err(resp) => return resp,
        }
    } else {
        let bytes = match to_bytes(request.into_body(), AVATAR_BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
        };
        match serde_json::from_slice::<AvatarGenerateRequest>(&bytes) {
            Ok(r) => (r, None, None),
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid request JSON: {e}"),
                );
            }
        }
    };

    run_pipeline(&state, &headers, req, image_bytes, audio_bytes).await
}

/// `POST /api/v1/avatar/script-from-llm` — ungated 9B writes a short spoken
/// script from a brief. Mirrors music's `draft-lyrics`.
async fn script_from_llm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AvatarScriptFromLlmRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let refine = crate::routes::refine::Refine::from_opts(
        req.refine_source.as_deref(),
        req.refine_instruction.as_deref(),
    );
    if req.brief.trim().is_empty() && refine.is_none() {
        return err(StatusCode::BAD_REQUEST, "brief is required");
    }
    match write_script_with_llm(&state, &req.brief, req.duration_sec, refine).await {
        Ok(script) => Json(AvatarScriptFromLlmResponse { script }).into_response(),
        Err(e) => err(
            StatusCode::BAD_GATEWAY,
            &format!("script draft failed: {e}"),
        ),
    }
}

/// `GET /api/v1/avatar/props` — UI bootstrap (defaults + supervisor health).
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.avatar.is_configured() {
        return Json(AvatarProps::default()).into_response();
    }
    let healthy = state.avatar.healthy().await;
    let health = state.avatar.health().await;
    let last_render_sec = health
        .as_ref()
        .and_then(|h| h.get("last_render_sec"))
        .and_then(|v| v.as_f64());
    Json(AvatarProps {
        healthy,
        default_segment_frames: 53,
        default_cont_cond_frames: 13,
        default_steps: 8,
        default_resolution: "480p".to_string(),
        last_render_sec,
    })
    .into_response()
}

/// `GET /api/v1/avatar/file/{name}` — serve a rendered clip from AVATARS_DIR.
async fn serve_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "invalid filename");
    }
    let path = state.avatars_dir.join(&name);
    match crate::routes::byte_serve::serve_file(&headers, &path, "video/webm").await {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "clip not found"),
    }
}

// ── saved-character library (avatar_assets) ───────────────────────────

/// Subdir under AVATARS_DIR where uploaded character portraits live.
const ASSETS_SUBDIR: &str = "_assets";

/// `GET /api/v1/avatar/assets` — list saved character portraits.
async fn list_assets(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let rows = match avatar_assets::list(&state.db).await {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("listing avatar assets: {e}"),
            );
        }
    };
    let assets: Vec<AvatarAsset> = rows
        .into_iter()
        .map(|r| AvatarAsset {
            id: r.id.clone(),
            filename: r.original_filename,
            url: format!("/api/v1/avatar/assets/{}/file", r.id),
            created_at: r.created_at.unix_timestamp(),
        })
        .collect();
    Json(AvatarAssetsResponse { assets }).into_response()
}

/// `POST /api/v1/avatar/assets` — multipart upload (single `image` part).
async fn upload_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    mp: Multipart,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let (bytes, original_filename, content_type) = match read_image_part(mp).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // Sanity caps — the body limit (64 MB) already protects against huge
    // uploads, but a few extra checks save head-scratching downstream.
    if bytes.is_empty() {
        return err(StatusCode::BAD_REQUEST, "image is empty");
    }
    let id = uuid::Uuid::now_v7().to_string();
    let ext = guess_extension(&original_filename, &content_type);
    let rel_path = format!("{ASSETS_SUBDIR}/{id}.{ext}");
    let abs = state.avatars_dir.join(&rel_path);
    if let Some(parent) = abs.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("creating asset dir: {e}"),
        );
    }
    if let Err(e) = tokio::fs::write(&abs, &bytes).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("saving asset to disk: {e}"),
        );
    }
    let row = match avatar_assets::create(
        &state.db,
        &avatar_assets::NewAvatarAsset {
            id: &id,
            original_filename: &original_filename,
            path: &rel_path,
            content_type: &content_type,
            size_bytes: bytes.len() as i64,
        },
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            // Best-effort rollback so we don't orphan the file.
            let _ = tokio::fs::remove_file(&abs).await;
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("inserting avatar asset: {e}"),
            );
        }
    };
    Json(AvatarAsset {
        id: row.id.clone(),
        filename: row.original_filename,
        url: format!("/api/v1/avatar/assets/{}/file", row.id),
        created_at: row.created_at.unix_timestamp(),
    })
    .into_response()
}

/// `DELETE /api/v1/avatar/assets/{id}` — remove row + file.
async fn delete_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let row = match avatar_assets::delete(&state.db, &id).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "asset not found"),
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("deleting asset: {e}"),
            );
        }
    };
    let abs = state.avatars_dir.join(&row.path);
    if let Err(e) = tokio::fs::remove_file(&abs).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(path = %abs.display(), error = %e, "failed to remove avatar asset file");
    }
    Json(json!({ "ok": true, "id": id })).into_response()
}

/// `GET /api/v1/avatar/assets/{id}/file` — serve raw bytes.
async fn serve_asset_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let row = match avatar_assets::get_by_id(&state.db, &id).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "asset not found"),
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("looking up asset: {e}"),
            );
        }
    };
    let abs = state.avatars_dir.join(&row.path);
    match crate::routes::byte_serve::serve_file(&headers, &abs, &row.content_type).await {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "asset file missing on disk"),
    }
}

/// Read a single `image` multipart part. Returns (bytes, filename, content-type).
async fn read_image_part(mut mp: Multipart) -> Result<(Bytes, String, String), Response> {
    while let Some(field) = mp.next_field().await.map_err(|e| {
        err(
            StatusCode::BAD_REQUEST,
            &format!("malformed multipart: {e}"),
        )
    })? {
        let name = field.name().unwrap_or_default().to_string();
        if name != "image" {
            let _ = field.bytes().await;
            continue;
        }
        let filename = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "character.png".to_string());
        let content_type = field
            .content_type()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "image/png".to_string());
        let bytes = field.bytes().await.map_err(|e| {
            err(
                StatusCode::BAD_REQUEST,
                &format!("reading image field: {e}"),
            )
        })?;
        return Ok((bytes, filename, content_type));
    }
    Err(err(
        StatusCode::BAD_REQUEST,
        "missing `image` multipart field",
    ))
}

/// Pick an extension for the saved file. Original filename > content-type.
fn guess_extension(filename: &str, content_type: &str) -> String {
    if let Some(idx) = filename.rfind('.') {
        let ext = &filename[idx + 1..];
        if !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return ext.to_ascii_lowercase();
        }
    }
    match content_type.to_ascii_lowercase().as_str() {
        "image/png" => "png".to_string(),
        "image/jpeg" | "image/jpg" => "jpg".to_string(),
        "image/webp" => "webp".to_string(),
        "image/gif" => "gif".to_string(),
        _ => "bin".to_string(),
    }
}

// ── pipeline ──────────────────────────────────────────────────────────

async fn run_pipeline(
    state: &AppState,
    headers: &HeaderMap,
    req: AvatarGenerateRequest,
    image_part: Option<Bytes>,
    audio_part: Option<Bytes>,
) -> Response {
    if !state.avatar.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "avatar service not configured (set AVATAR_SERVER_URL or the DB setting)",
        );
    }

    // 1. Materialise the reference portrait → bytes.
    let image_bytes = match resolve_image(state, &req.image_source, image_part).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    // 2 + 3. Resolve the spoken script + render TTS (or just take the
    // caller-provided audio).
    let mut script_used: Option<String> = None;
    let audio_bytes =
        match resolve_audio(state, &req.audio_source, audio_part, &mut script_used).await {
            Ok(b) => b,
            Err(resp) => return resp,
        };

    // 🔴 INPUT CAPTURE AT ENQUEUE. The bytes go into job-owned storage and the payload carries
    // only the ref: `gpu_jobs.payload` is `to_string()`-hashed into the job id and compared WHOLE
    // against every recent row on every enqueue, so tens of megabytes of base64 there is paid for
    // on each of them.
    let dir = state.director_assets_dir.as_ref();
    let image_ref = match director_assets::capture_bytes(dir, &state.db, &image_bytes).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let audio_ref = match director_assets::capture_bytes(dir, &state.db, &audio_bytes).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };

    // 🔴 The 503 with its Retry-After is gone. "avatar busy or administratively disabled" was the
    // gate refusing rather than queueing — the render waits its turn now instead of asking the
    // caller to come back later.
    //
    // The payload IS the supervisor's `/generate` body bar the two refs the adapter swaps for
    // base64 at dispatch. Nothing else may be added to it: a stray key rides straight into the
    // render. `save_clip` therefore has nowhere to live — a queued render has no HTTP response to
    // stream bytes into, so the adapter always keeps the clip.
    let payload = build_supervisor_body(&req, &image_ref, &audio_ref);
    let who = match crate::routes::queue::principal_of(state, headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let enqueued = crate::routes::queue::enqueue(
        state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(headers).request(JobKind::Avatar, payload),
    )
    .await;
    match enqueued {
        // The script rides back on the accept: the 9B wrote it a moment ago and the row keeps no
        // copy the page could read later.
        Ok(resp) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "job_id": resp.job_id,
                "state": resp.state,
                "position": resp.position,
                "deduplicated": resp.deduplicated,
                "blocked_reason": resp.blocked_reason,
                "script_used": script_used,
            })),
        )
            .into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

// ── input resolution ──────────────────────────────────────────────────

async fn resolve_image(
    state: &AppState,
    source: &AvatarImageSource,
    multipart_part: Option<Bytes>,
) -> Result<Bytes, Response> {
    match source {
        AvatarImageSource::Upload => multipart_part.ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "image_source=upload requires an `image` multipart part",
            )
        }),
        AvatarImageSource::Url { url } => fetch_url(url)
            .await
            .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("fetching image: {e}"))),
        AvatarImageSource::LibraryAssetId { id } => {
            let row = match avatar_assets::get_by_id(&state.db, id).await {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return Err(err(
                        StatusCode::NOT_FOUND,
                        &format!("avatar asset {id} not found"),
                    ));
                }
                Err(e) => {
                    return Err(err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("avatar asset lookup failed: {e}"),
                    ));
                }
            };
            let abs = state.avatars_dir.join(&row.path);
            tokio::fs::read(&abs).await.map(Bytes::from).map_err(|e| {
                err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("reading saved character: {e}"),
                )
            })
        }
    }
}

async fn resolve_audio(
    state: &AppState,
    source: &AvatarAudioSource,
    multipart_part: Option<Bytes>,
    script_used_out: &mut Option<String>,
) -> Result<Bytes, Response> {
    match source {
        AvatarAudioSource::Upload => multipart_part.ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "audio_source=upload requires an `audio` multipart part",
            )
        }),
        AvatarAudioSource::Url { url } => fetch_url(url)
            .await
            .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("fetching audio: {e}"))),
        AvatarAudioSource::LibraryClipId { id: _ } => Err(err(
            StatusCode::NOT_IMPLEMENTED,
            "library_clip_id audio not implemented yet — upload the wav",
        )),
        AvatarAudioSource::Tts {
            script,
            script_from_brief,
            voice,
            engine: _,
        } => {
            // The `engine` field is kept as a forward-compat hook but ignored —
            // `tts-breeze` is the only TTS backend koblem speaks.
            // 9B writes the script first if asked.
            let final_script = match script
                .clone()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                Some(s) => s,
                None => match script_from_brief
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(brief) => match write_script_with_llm(state, brief, None, None).await {
                        Ok(s) if !s.trim().is_empty() => s,
                        Ok(_) => {
                            return Err(err(StatusCode::BAD_GATEWAY, "9B returned empty script"));
                        }
                        Err(e) => {
                            return Err(err(
                                StatusCode::BAD_GATEWAY,
                                &format!("9B script draft failed: {e}"),
                            ));
                        }
                    },
                    None => {
                        return Err(err(
                            StatusCode::BAD_REQUEST,
                            "tts audio_source needs either `script` or `script_from_brief`",
                        ));
                    }
                },
            };
            *script_used_out = Some(final_script.clone());
            synth_tts(state, &final_script, voice.as_deref()).await
        }
    }
}

/// Render speech via `tts-breeze`, assembling the streamed PCM into a wav.
///
/// Streams upstream rather than asking for a buffered wav: peak VRAM is flat in
/// output length when streaming and grows with it when buffering, and an avatar
/// script can be long. `long` is on so the engine chunks internally and holds
/// one voice for the whole script.
async fn synth_tts(state: &AppState, script: &str, voice: Option<&str>) -> Result<Bytes, Response> {
    let req = crate::tts_breeze::SpeechRequest {
        input: script.to_string(),
        // No voice = the engine picks one and holds it; there is no
        // "default" clone to name.
        voice: voice.map(str::to_string).filter(|v| !v.is_empty()),
        response_format: "pcm".to_string(),
        stream_format: Some("sse".to_string()),
        stream_first_batch_size: Some(1),
        long: Some(true),
        ..Default::default()
    };
    let handle = state
        .tts
        .synthesize_stream(&req)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("TTS: {e}")))?;
    let guard = handle.guard;
    let (pcm, truncated) = crate::tts_breeze::collect_sse(handle.response)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("TTS stream: {e}")))?;
    drop(guard); // release TTS guard before avatar acquires
    // A truncated render is a short mid-sentence clip that still decodes, so
    // it would silently become a lip-synced avatar of half the script.
    if truncated {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            "TTS truncated the script mid-sentence",
        ));
    }
    // The avatar supervisor's ffmpeg downstream chops to 16 kHz mono regardless.
    Ok(crate::tts_breeze::wav_from_pcm(&pcm))
}

/// A fetched reference asset (portrait or audio clip) this large would be
/// unreasonable at either end of the pipeline; the cap exists so an
/// unbounded or malicious response can't be streamed into memory forever.
const FETCH_URL_MAX_BYTES: usize = 50 * 1024 * 1024;

/// Scheme allowlist, pulled out so it is unit-testable without touching the
/// network. `data:`/`file:` and friends never reach `reqwest` from here.
fn validated_url(raw: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(raw).map_err(|e| format!("invalid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(format!("unsupported URL scheme: {other}")),
    }
}

/// Loopback and link-local (169.254.0.0/16, fe80::/10 — this is what covers
/// cloud metadata endpoints like 169.254.169.254) are blocked. RFC1918 and
/// docker-network ranges are deliberately NOT blocked: koblem fetching an
/// avatar reference from kobbler over the LAN or a shared docker network is a
/// real, supported deployment shape, not an SSRF target.
fn is_blocked_fetch_ip(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// HTTP(S) GET → Bytes, with a short timeout, a size cap, and SSRF guards:
/// scheme-checked ([`validated_url`]), no redirects (a redirect could retarget
/// a blocked host after the resolve check below), and the resolved host must
/// not be loopback or link-local ([`is_blocked_fetch_ip`]).
///
/// 🔴 The resolve check happens once, here, at call time — not pinned to the
/// IP the TCP connection actually lands on. A DNS-rebinding attacker who
/// controls the authoritative server for the hostname could still race this
/// check. Accepted for now: the route is authenticated, and closing that gap
/// needs a custom resolver/connector rather than a check-then-connect.
async fn fetch_url(url: &str) -> Result<Bytes, String> {
    let parsed = validated_url(url)?;
    let host = parsed.host_str().ok_or("URL has no host")?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let resolved: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("resolving {host}: {e}"))?
        .collect();
    if resolved.is_empty() {
        return Err(format!("{host} did not resolve to any address"));
    }
    if let Some(blocked) = resolved.iter().find(|a| is_blocked_fetch_ip(a.ip())) {
        return Err(format!(
            "{host} resolves to a blocked address ({})",
            blocked.ip()
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(parsed)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;

    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if buf.len() + chunk.len() > FETCH_URL_MAX_BYTES {
            return Err(format!(
                "response exceeded the {FETCH_URL_MAX_BYTES}-byte cap"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

// ── LLM script-from-brief ─────────────────────────────────────────────

const SCRIPT_SYSTEM_PROMPT: &str = "You write short spoken scripts for an \
animated character avatar to recite aloud. The constraint is the target \
duration at a normal speaking pace (~2.5 words/sec). Capture the user's \
brief faithfully — no stage directions, no markdown, no role labels, no \
narrator framing. Output ONLY the words to be spoken, in one or two short \
paragraphs.";

async fn write_script_with_llm(
    state: &AppState,
    brief: &str,
    duration_sec: Option<f64>,
    refine: Option<crate::routes::refine::Refine<'_>>,
) -> Result<String, crate::llama::Error> {
    let secs = duration_sec.unwrap_or(10.0).max(2.0);
    let target_words = (secs * 2.5).round() as u32;
    let user = match &refine {
        // Improve an existing script under the same rules; the brief + target
        // length ride along as context so the revision stays on topic and length.
        Some(r) => {
            let mut context = format!("Target: ~{secs:.0}s (~{target_words} words)");
            if !brief.trim().is_empty() {
                context.push_str(&format!("\nBrief: {brief}"));
            }
            crate::routes::refine::refine_user_msg("spoken script", &context, r)
        }
        None => format!(
            "Write a ~{secs:.0}-second spoken script (~{target_words} words) for an avatar to \
             recite.\n\nBrief: {brief}"
        ),
    };
    let mut body = json!({
        "messages": [
            {"role": "system", "content": SCRIPT_SYSTEM_PROMPT},
            {"role": "user", "content": user},
        ],
        "max_tokens": (target_words * 3).max(120),
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_avatar", &mut body).await;
    let (_permit, resp) = state.llama.chat_completion_json(&body).await?;
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(content)
}

// ── supervisor body building ──────────────────────────────────────────

/// Map a resolution preset to the avatar model's sanctioned tier dimensions
/// (portrait talking-head). The LongCat model is trained for 480P/720P only;
/// `720p` ≈ 2.7× the activations of `480p` and OOMs a 12 GB card. Also accepts
/// a raw `WxH` string for back-compat.
fn parse_resolution(r: &str) -> Option<(u32, u32)> {
    match r.trim() {
        "480p" | "480" => Some((480, 832)),
        "720p" | "720" => Some((720, 1248)),
        other => {
            let (w, h) = other.split_once('x').or_else(|| other.split_once('X'))?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        }
    }
}

fn build_supervisor_body(req: &AvatarGenerateRequest, image: &str, audio: &str) -> Value {
    let mut body = json!({
        "image": image,
        "audio": audio,
        "return": "bytes",
    });
    let obj = body.as_object_mut().unwrap();

    if let Some(p) = req.prompt.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("prompt".into(), json!(p));
    }
    // The avatar server only honours explicit width/height (its from_json_str
    // never reads "resolution"). Map the preset to the model's sanctioned tier
    // dimensions so the worker actually renders at the requested size — and so
    // the reference image is scaled to a trained resolution instead of running
    // at its raw dimensions (off-tier dims OOM and break continuation re-encode).
    // 480p/720p are quality tiers, not fixed shapes: Landscape transposes the
    // resolved tier so the same activation budget renders wide instead of tall.
    let (mut w, mut h) = req
        .resolution
        .as_deref()
        .and_then(parse_resolution)
        .unwrap_or((480, 832));
    if req.orientation == AvatarOrientation::Landscape {
        std::mem::swap(&mut w, &mut h);
    }
    obj.insert("width".into(), json!(w));
    obj.insert("height".into(), json!(h));
    if let Some(d) = req.duration_sec {
        obj.insert("duration_sec".into(), json!(d));
    }
    if let Some(s) = req.segments {
        obj.insert("segments".into(), json!(s));
    }
    if let Some(s) = req.segment_frames {
        obj.insert("segment_frames".into(), json!(s));
    }
    if let Some(c) = req.cont_cond_frames {
        obj.insert("cont_cond_frames".into(), json!(c));
    }
    // steps + cfg_scale ride inside `sample_params` — the nested shape the
    // worker's from_json_str reads (sample_params.sample_steps /
    // sample_params.guidance.txt_cfg). Sending them flat was a no-op.
    // `quant` is baked into the model loaded at server launch (q4_k), so it
    // can't be selected per-request and is intentionally not sent.
    let mut sample_params = serde_json::Map::new();
    if let Some(s) = req.steps {
        sample_params.insert("sample_steps".into(), json!(s));
    }
    if let Some(c) = req.cfg_scale {
        sample_params.insert("guidance".into(), json!({ "txt_cfg": c }));
    }
    if !sample_params.is_empty() {
        obj.insert("sample_params".into(), Value::Object(sample_params));
    }
    if let Some(s) = req.seed {
        obj.insert("seed".into(), json!(s));
    }
    if let Some(m) = req.audio_mouth_scale {
        obj.insert("audio_mouth_scale".into(), json!(m));
    }
    if let Some(l) = req.audio_lowpass_hz {
        obj.insert("audio_lowpass_hz".into(), json!(l));
    }
    match req.offload {
        Some(AvatarOffload::True) => {
            obj.insert("offload".into(), json!(true));
        }
        Some(AvatarOffload::False) => {
            obj.insert("offload".into(), json!(false));
        }
        Some(AvatarOffload::Auto) => {
            obj.insert("offload".into(), json!("auto"));
        }
        None => {} // supervisor default = true (post §4.3 patch)
    }
    if let Some(t) = req.vae_tiling {
        // Worker reads vae_tiling_params.enabled, not a flat "vae_tiling" bool.
        obj.insert("vae_tiling_params".into(), json!({ "enabled": t }));
    }
    if let Some(bsa) = &req.bsa {
        obj.insert("bsa".into(), serde_json::to_value(bsa).unwrap_or(json!({})));
    }
    if let Some(env) = &req.debug_env {
        obj.insert(
            "debug_env".into(),
            serde_json::to_value(env).unwrap_or(json!({})),
        );
    }
    body
}

// ── multipart helpers ─────────────────────────────────────────────────

struct ParsedAvatar {
    request: Option<AvatarGenerateRequest>,
    image: Option<Bytes>,
    audio: Option<Bytes>,
}

async fn parse_avatar_multipart(mut mp: Multipart) -> Result<ParsedAvatar, Response> {
    let mut out = ParsedAvatar {
        request: None,
        image: None,
        audio: None,
    };
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                match name.as_str() {
                    "request" => {
                        let t = field.text().await.map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("reading request field: {e}"),
                            )
                        })?;
                        let r = serde_json::from_str::<AvatarGenerateRequest>(&t).map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("invalid request JSON: {e}"),
                            )
                        })?;
                        out.request = Some(r);
                    }
                    "image" => {
                        let b = field.bytes().await.map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("reading image field: {e}"),
                            )
                        })?;
                        out.image = Some(b);
                    }
                    "audio" => {
                        let b = field.bytes().await.map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("reading audio field: {e}"),
                            )
                        })?;
                        out.audio = Some(b);
                    }
                    _ => {
                        let _ = field.bytes().await;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    &format!("malformed multipart: {e}"),
                ));
            }
        }
    }
    Ok(out)
}

// ── auth + disk + logs ────────────────────────────────────────────────

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

pub(crate) async fn save_clip(
    state: &AppState,
    filename: &str,
    bytes: &Bytes,
) -> std::io::Result<()> {
    tokio::fs::create_dir_all(&*state.avatars_dir).await?;
    let path = state.avatars_dir.join(filename);
    tokio::fs::write(path, bytes).await
}

// ── filename ──────────────────────────────────────────────────────────

pub(crate) fn file_stamp_filename(seed: i64) -> String {
    let now = time::OffsetDateTime::now_utc();
    let stamp = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let s = if seed < 0 {
        "rnd".to_string()
    } else {
        seed.to_string()
    };
    format!("{stamp}-{s}.webm")
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(resolution: Option<&str>, orientation: AvatarOrientation) -> (u64, u64) {
        let req = AvatarGenerateRequest {
            resolution: resolution.map(str::to_string),
            orientation,
            ..Default::default()
        };
        let body = build_supervisor_body(&req, "asset:aa", "asset:bb");
        let obj = body.as_object().unwrap();
        (
            obj["width"].as_u64().unwrap(),
            obj["height"].as_u64().unwrap(),
        )
    }

    #[test]
    fn orientation_transposes_tier_dims() {
        use AvatarOrientation::{Landscape, Portrait};
        // (resolution, orientation, expected w×h)
        let cases = [
            (Some("480p"), Portrait, (480, 832)),
            (Some("480p"), Landscape, (832, 480)),
            (Some("720p"), Portrait, (720, 1248)),
            (Some("720p"), Landscape, (1248, 720)),
            // No resolution → 480p tier; orientation still applies.
            (None, Portrait, (480, 832)),
            (None, Landscape, (832, 480)),
        ];
        for (res, orient, expected) in cases {
            assert_eq!(dims(res, orient), expected, "res={res:?} orient={orient:?}");
        }
    }

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_and_link_local_are_blocked() {
        for blocked in [
            "127.0.0.1",
            "127.0.0.53",
            "169.254.169.254", // cloud metadata
            "::1",
            "fe80::1",
        ] {
            assert!(
                is_blocked_fetch_ip(ip(blocked)),
                "{blocked} should be blocked"
            );
        }
    }

    /// RFC1918 and public addresses stay allowed — koblem fetching an avatar
    /// from kobbler over the LAN or a shared docker network must keep working.
    #[test]
    fn private_and_public_ranges_stay_allowed() {
        for allowed in ["10.0.0.5", "172.17.0.2", "192.168.1.50", "8.8.8.8"] {
            assert!(
                !is_blocked_fetch_ip(ip(allowed)),
                "{allowed} should be allowed"
            );
        }
    }

    #[test]
    fn only_http_and_https_schemes_are_accepted() {
        assert!(validated_url("https://example.com/a.png").is_ok());
        assert!(validated_url("http://example.com/a.png").is_ok());
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/a.png",
            "gopher://example.com",
            "not a url",
        ] {
            assert!(validated_url(bad).is_err(), "{bad} should be rejected");
        }
    }
}
