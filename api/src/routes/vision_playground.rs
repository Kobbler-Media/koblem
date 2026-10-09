//! Tier-2 Vision playground — expose max_num_patches / pooling / return_logits /
//! return_last_hidden. Auth-gated, logs to gpu_logs.

use std::time::Instant;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::tier2_helpers::{extract_caller, log_async, save_sample};

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

async fn embed(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let caller = extract_caller(&headers);
    let start = Instant::now();
    let mut images: Vec<(String, Bytes)> = Vec::new();
    let mut max_num_patches: Option<u32> = None;
    let mut pooling: Option<String> = None;
    let mut return_last_hidden = false;
    let mut save = false;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        };
        match field.name().unwrap_or("") {
            "images" => {
                let filename = field.file_name().unwrap_or("image.png").to_string();
                let mime = field.content_type().unwrap_or("image/png").to_string();
                let bytes = match field.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": e.to_string()})),
                        )
                            .into_response();
                    }
                };
                // Save as-is for replay if requested (we hold the first image only — keep it simple).
                if save && images.is_empty() {
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
                images.push((filename, bytes));
            }
            "max_num_patches" => {
                max_num_patches = field.text().await.ok().and_then(|v| v.parse().ok());
            }
            "pooling" => pooling = field.text().await.ok(),
            "return_last_hidden" => {
                return_last_hidden = matches!(
                    field.text().await.unwrap_or_default().as_str(),
                    "true" | "1"
                );
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

    if images.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "need images"})),
        )
            .into_response();
    }

    let result = state
        .vision
        .embed(
            &images,
            max_num_patches,
            pooling.as_deref(),
            return_last_hidden,
        )
        .await;

    let request_meta = json!({
        "image_count": images.len(),
        "max_num_patches": max_num_patches,
        "pooling": pooling,
        "return_last_hidden": return_last_hidden,
    });

    match result {
        Ok((_permit, resp, gate_wait)) => {
            let gate_wait_ms = gate_wait.as_millis() as i64;
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: "/api/v1/vision/embed".to_string(),
                    request_meta,
                    response_meta: json!({
                        "embeddings_shape": [resp.embeddings.len(), resp.embeddings.first().map(|e| e.len()).unwrap_or(0)],
                    }),
                    sample_id: None,
                    gate_wait_ms,
                    duration_ms,
                    status: 200,
                    caller,
                },
            );
            let mut body = json!({
                "embeddings": resp.embeddings,
                "duration_ms": duration_ms,
                "gate_wait_ms": gate_wait_ms,
            });
            if let Some(lhs) = resp.last_hidden_state {
                body["last_hidden_state"] = json!(lhs);
            }
            Json(body).into_response()
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: "/api/v1/vision/embed".to_string(),
                    request_meta,
                    response_meta: json!({"error": e.to_string()}),
                    sample_id: None,
                    gate_wait_ms: 0,
                    duration_ms,
                    status: 502,
                    caller,
                },
            );
            warn!(error = %e, "vision embed failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("vision: {e}")})),
            )
                .into_response()
        }
    }
}

async fn classify(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let caller = extract_caller(&headers);
    let start = Instant::now();
    let mut images: Vec<(String, Bytes)> = Vec::new();
    let mut prompts: Vec<String> = Vec::new();
    let mut max_num_patches: Option<u32> = None;
    let mut return_logits = false;
    let mut save = false;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        };
        match field.name().unwrap_or("") {
            "images" => {
                let filename = field.file_name().unwrap_or("image.png").to_string();
                let mime = field.content_type().unwrap_or("image/png").to_string();
                let bytes = match field.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": e.to_string()})),
                        )
                            .into_response();
                    }
                };
                if save && images.is_empty() {
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
                images.push((filename, bytes));
            }
            "prompts" => {
                if let Ok(t) = field.text().await {
                    prompts.push(t);
                }
            }
            "max_num_patches" => {
                max_num_patches = field.text().await.ok().and_then(|v| v.parse().ok());
            }
            "return_logits" => {
                return_logits = matches!(
                    field.text().await.unwrap_or_default().as_str(),
                    "true" | "1"
                );
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

    if images.is_empty() || prompts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "need images and prompts"})),
        )
            .into_response();
    }

    let result = state
        .vision
        .classify(&images, &prompts, max_num_patches, return_logits)
        .await;

    let request_meta = json!({
        "image_count": images.len(),
        "prompts": prompts,
        "max_num_patches": max_num_patches,
        "return_logits": return_logits,
    });

    match result {
        Ok((_permit, resp, gate_wait)) => {
            let gate_wait_ms = gate_wait.as_millis() as i64;
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: "/api/v1/vision/classify".to_string(),
                    request_meta,
                    response_meta: json!({
                        "scores_shape": [resp.scores.len(), resp.scores.first().map(|r| r.len()).unwrap_or(0)],
                    }),
                    sample_id: None,
                    gate_wait_ms,
                    duration_ms,
                    status: 200,
                    caller,
                },
            );
            let mut body = json!({
                "scores": resp.scores,
                "duration_ms": duration_ms,
                "gate_wait_ms": gate_wait_ms,
            });
            if let Some(logits) = resp.logits {
                body["logits"] = json!(logits);
            }
            Json(body).into_response()
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: "/api/v1/vision/classify".to_string(),
                    request_meta,
                    response_meta: json!({"error": e.to_string()}),
                    sample_id: None,
                    gate_wait_ms: 0,
                    duration_ms,
                    status: 502,
                    caller,
                },
            );
            warn!(error = %e, "vision classify failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("vision: {e}")})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct TextEmbedReq {
    prompts: Vec<String>,
}

async fn text_embed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TextEmbedReq>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if req.prompts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "prompts must be non-empty"})),
        )
            .into_response();
    }

    let caller = extract_caller(&headers);
    let start = Instant::now();
    let request_meta = json!({"prompt_count": req.prompts.len(), "prompts": req.prompts});

    match state.vision.text_embeddings(&req.prompts).await {
        Ok((_permit, resp, gate_wait)) => {
            let gate_wait_ms = gate_wait.as_millis() as i64;
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: "/api/v1/vision/text-embed".to_string(),
                    request_meta,
                    response_meta: json!({
                        "embeddings_shape": [resp.embeddings.len(), resp.embeddings.first().map(|e| e.len()).unwrap_or(0)],
                    }),
                    sample_id: None,
                    gate_wait_ms,
                    duration_ms,
                    status: 200,
                    caller,
                },
            );
            Json(json!({
                "embeddings": resp.embeddings,
                "duration_ms": duration_ms,
                "gate_wait_ms": gate_wait_ms,
            }))
            .into_response()
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: "/api/v1/vision/text-embed".to_string(),
                    request_meta,
                    response_meta: json!({"error": e.to_string()}),
                    sample_id: None,
                    gate_wait_ms: 0,
                    duration_ms,
                    status: 502,
                    caller,
                },
            );
            warn!(error = %e, "vision text-embed failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("vision: {e}")})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct ClassifyFromEmbedReq {
    image_embeddings: Vec<Vec<f32>>,
    prompts: Vec<String>,
    #[serde(default)]
    return_logits: bool,
}

async fn classify_from_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ClassifyFromEmbedReq>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if req.image_embeddings.is_empty() || req.prompts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "image_embeddings and prompts must both be non-empty"})),
        )
            .into_response();
    }

    let caller = extract_caller(&headers);
    let start = Instant::now();
    let embed_dim = req.image_embeddings.first().map(|e| e.len()).unwrap_or(0);
    let request_meta = json!({
        "embeddings_shape": [req.image_embeddings.len(), embed_dim],
        "prompts": req.prompts,
        "return_logits": req.return_logits,
    });

    match state
        .vision
        .classify_from_embeddings(&req.image_embeddings, &req.prompts, req.return_logits)
        .await
    {
        Ok((_permit, resp, gate_wait)) => {
            let gate_wait_ms = gate_wait.as_millis() as i64;
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: "/api/v1/vision/classify-from-embeddings".to_string(),
                    request_meta,
                    response_meta: json!({
                        "scores_shape": [resp.scores.len(), resp.scores.first().map(|r| r.len()).unwrap_or(0)],
                    }),
                    sample_id: None,
                    gate_wait_ms,
                    duration_ms,
                    status: 200,
                    caller,
                },
            );
            let mut body = json!({
                "scores": resp.scores,
                "duration_ms": duration_ms,
                "gate_wait_ms": gate_wait_ms,
            });
            if let Some(logits) = resp.logits {
                body["logits"] = json!(logits);
            }
            Json(body).into_response()
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "vision".to_string(),
                    endpoint: "/api/v1/vision/classify-from-embeddings".to_string(),
                    request_meta,
                    response_meta: json!({"error": e.to_string()}),
                    sample_id: None,
                    gate_wait_ms: 0,
                    duration_ms,
                    status: 502,
                    caller,
                },
            );
            warn!(error = %e, "vision classify-from-embeddings failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("vision: {e}")})),
            )
                .into_response()
        }
    }
}
