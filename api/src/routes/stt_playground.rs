//! Tier-2 STT playground — expose timestamp_granularity/beam_size/language,
//! plus library-backed "transcribe a slice of this audiobook".
//! All auth-gated. Fire-and-forget logs to gpu_logs.

use std::time::Instant;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use koblem_client::types::SttLibraryTranscribeRequest;
use serde_json::json;
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::tier2_helpers::{
    OutputBytes, extract_caller, log_async, persist_generation, save_sample, stt_cancel_url,
};
use crate::stt::SttParams;

const STT_BODY_LIMIT: usize = 500 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/stt/transcribe", post(transcribe))
        .route(
            "/api/v1/stt/transcribe-from-library",
            post(transcribe_from_library),
        )
        .layer(DefaultBodyLimit::max(STT_BODY_LIMIT))
}

async fn transcribe(
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
    let cancel_url = stt_cancel_url(&headers);
    let start = Instant::now();

    let mut file_bytes: Option<(String, bytes::Bytes)> = None;
    let mut params = SttParams::default();
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
            "file" => {
                let filename = field.file_name().unwrap_or("upload.wav").to_string();
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
                file_bytes = Some((filename, bytes));
            }
            "timestamp_granularity" => {
                params.timestamp_granularity = field.text().await.ok();
            }
            "beam_size" => {
                params.beam_size = field.text().await.ok().and_then(|v| v.parse().ok());
            }
            "language" => {
                params.language = field.text().await.ok();
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

    let Some((name, bytes)) = file_bytes else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "need file"}))).into_response();
    };

    let sample_id = if save {
        save_sample(
            &state.db,
            &state.samples_dir,
            "audio_in",
            "audio/wav",
            "wav",
            &bytes,
        )
        .await
    } else {
        None
    };

    let result = state
        .stt
        .transcribe_upload(&name, bytes, &params, Some(&cancel_url))
        .await;

    finalize_stt(
        &state,
        result,
        "/api/v1/stt/transcribe",
        &params,
        sample_id,
        start,
        caller,
    )
    .await
}

async fn transcribe_from_library(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SttLibraryTranscribeRequest>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let caller = extract_caller(&headers);
    let cancel_url = stt_cancel_url(&headers);
    let start = Instant::now();

    let library = match state.library_provider.as_ref() {
        Some(l) => l.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "library provider not configured"})),
            )
                .into_response();
        }
    };

    let detail = match library.get_audiobook(&req.library_audiobook_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "audiobook not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("library: {e}")})),
            )
                .into_response();
        }
    };

    let Some(chapter) = detail.chapters.first() else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "audiobook has no chapters"})),
        )
            .into_response();
    };

    let params = SttParams {
        timestamp_granularity: req.timestamp_granularity.clone(),
        beam_size: req.beam_size,
        language: req.language.clone(),
    };

    let result = state
        .stt
        .transcribe_file_path(&chapter.file_path, &params, Some(&cancel_url))
        .await;

    finalize_stt(
        &state,
        result,
        "/api/v1/stt/transcribe-from-library",
        &params,
        None,
        start,
        caller,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finalize_stt(
    state: &AppState,
    result: Result<
        (
            kob_gpu_gate::GpuSttGuard,
            crate::stt::TranscribeResponse,
            std::time::Duration,
        ),
        crate::stt::Error,
    >,
    endpoint: &str,
    params: &SttParams,
    sample_id: Option<i64>,
    start: Instant,
    caller: Option<String>,
) -> Response {
    match result {
        Ok((_permit, resp, gate_wait)) => {
            // Transcription is done — release the GPU before the disk/DB work
            // (gallery persist + log) so it doesn't extend the lock hold.
            drop(_permit);
            let gate_wait_ms = gate_wait.as_millis() as i64;
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "stt".to_string(),
                    endpoint: endpoint.to_string(),
                    request_meta: serde_json::to_value(params).unwrap_or(json!({})),
                    response_meta: json!({
                        "text_len": resp.text.len(),
                        "segments": resp.segments.len(),
                        "words": resp.words.len(),
                        "chars": resp.chars.len(),
                        "preempted": resp.preempted,
                    }),
                    sample_id,
                    gate_wait_ms,
                    duration_ms,
                    status: 200,
                    caller,
                },
            );

            let body = json!({
                "text": resp.text,
                "segments": resp.segments,
                "words": resp.words,
                "chars": resp.chars,
                "language": resp.language,
                "preempted": resp.preempted,
                "duration_ms": duration_ms,
                "gate_wait_ms": gate_wait_ms,
            });

            // Gallery record — the transcript is written as a JSON file ONLY
            // when generations_dir is configured; otherwise response-only.
            let transcript_bytes = serde_json::to_vec(&body).unwrap_or_default();
            if !transcript_bytes.is_empty() {
                persist_generation(
                    state,
                    "stt",
                    endpoint,
                    json!({ "endpoint": endpoint, "params": params, "had_audio_inputs": true }),
                    &[OutputBytes {
                        ext: "json".to_string(),
                        mime: "application/json".to_string(),
                        kind: "transcript".to_string(),
                        bytes: bytes::Bytes::from(transcript_bytes),
                    }],
                )
                .await;
            }

            Json(body).into_response()
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "stt".to_string(),
                    endpoint: endpoint.to_string(),
                    request_meta: serde_json::to_value(params).unwrap_or(json!({})),
                    response_meta: json!({"error": e.to_string()}),
                    sample_id,
                    gate_wait_ms: 0,
                    duration_ms,
                    status: 502,
                    caller,
                },
            );
            warn!(error = %e, "stt transcribe failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("stt: {e}")})),
            )
                .into_response()
        }
    }
}
