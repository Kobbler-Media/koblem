//! Tier-1 OpenAI-compatible GPU endpoints (mirrors of the Python services).
//!
//! Drop-in replacements for callers that today point at kokoro / openai-tts /
//! whisper / siglip — request/response shapes match the Python services
//! byte-for-byte, with the GPU gate applied so multiple clients can share one
//! GPU safely.
//!
//! There is exactly one TTS engine (`tts-breeze`), so the historical
//! `x-koblem-tts-engine` header is ignored rather than validated — an older
//! caller still pinning `qwen3` is served instead of 400'd, which is what lets
//! koblem and its callers deploy in either order. The tier-2 routes at
//! `/api/v1/tts/*` expose the full streaming TTFA / alignment control surface.
//!
//! Endpoints (both `/v1/...` open and `/api/v1/...` auth-gated mirror,
//! matching the chat-completions pattern):
//!   POST /v1/audio/speech          (TTS)
//!   POST /v1/audio/transcriptions  (STT)
//!   POST /v1/embeddings            (Vision)
//!   POST /v1/classify              (Vision)

use axum::{
    Router,
    extract::{DefaultBodyLimit, Json, Multipart, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::Bytes;
use koblem_client::types::{
    VisionClassifyFromEmbedRequest, VisionClassifyFromEmbedResponse, VisionTextEmbedRequest,
    VisionTextEmbedResponse,
};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::stt::SttParams;
use crate::tts_breeze::SpeechRequest;

// There is one TTS engine. The historical `x-koblem-tts-engine` header is
// therefore vestigial and deliberately NOT read: an older caller still sending
// `qwen3` gets served rather than a 400, so the two services can deploy in
// either order.

/// 200 MB covers long audiobook chunks; images are much smaller.
const TIER1_BODY_LIMIT: usize = 200 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    use axum::routing::get;
    // Auth-gated mirrors: forward straight to the same handler via inner fn.
    let router = Router::new()
        .route("/v1/audio/speech", post(tier1_speech))
        .route("/v1/audio/transcriptions", post(tier1_transcribe))
        .route("/v1/embeddings", post(tier1_embed))
        .route("/v1/classify", post(tier1_classify))
        .route("/v1/text_embeddings", post(tier1_text_embeddings))
        .route(
            "/v1/classify_from_embeddings",
            post(tier1_classify_from_embeddings),
        )
        // /voices mirrors the OpenAI-TTS shape so kobbler (and other
        // OpenAI-TTS-style callers) can manage voice presets through koblem.
        .route(
            "/v1/audio/voices",
            get(tier1_list_voices).post(tier1_upload_voice),
        )
        .route("/voices", get(tier1_list_voices).post(tier1_upload_voice))
        .route("/api/v1/audio/speech", post(tier1_speech_authed))
        .route(
            "/api/v1/audio/transcriptions",
            post(tier1_transcribe_authed),
        )
        .route("/api/v1/embeddings", post(tier1_embed_authed))
        .route("/api/v1/classify", post(tier1_classify_authed))
        .route(
            "/api/v1/text_embeddings",
            post(tier1_text_embeddings_authed),
        )
        .route(
            "/api/v1/classify_from_embeddings",
            post(tier1_classify_from_embeddings_authed),
        )
        .route(
            "/api/v1/audio/voices",
            get(tier1_list_voices_authed).post(tier1_upload_voice_authed),
        );

    router.layer(DefaultBodyLimit::max(TIER1_BODY_LIMIT))
}

// ── Voices passthrough (list + upload) ────────────────────────────────

async fn tier1_list_voices(State(state): State<AppState>, _headers: HeaderMap) -> Response {
    // Mirrors the engine's legacy model-id-keyed shape `{model_id: [names]}`,
    // which kobbler's `parse_voices_body` understands.
    match state.tts.list_voices().await {
        Ok(payload) => {
            let model_id = payload
                .model_id
                .unwrap_or_else(|| crate::tts_breeze::MODEL_ID.to_string());
            let names: Vec<&str> = payload.voices.iter().map(|v| v.id.as_str()).collect();
            axum::Json(json!({ model_id: names })).into_response()
        }
        Err(e) => upstream_err("tts", e.to_string()),
    }
}

async fn tier1_list_voices_authed(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(code) = require_auth(&state.db, state.api_key.as_deref(), &headers).await {
        return code.into_response();
    }
    tier1_list_voices(State(state), headers).await
}

async fn tier1_upload_voice(
    State(state): State<AppState>,
    _headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let mut name: Option<String> = None;
    let mut wav: Option<Bytes> = None;
    let mut ref_text: Option<String> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return bad_req(e),
        };
        // Accept both shapes so callers don't have to know which field name to
        // send: `voice_name`/`voice_file` (legacy chatterbox-style, what
        // kobbler still sends) and `name`/`audio_sample` (engine-native).
        match field.name().unwrap_or("") {
            "voice_name" | "name" => name = field.text().await.ok(),
            "voice_file" | "audio_sample" => wav = field.bytes().await.ok(),
            "ref_text" => ref_text = field.text().await.ok(),
            _ => {
                let _ = field.text().await;
            }
        }
    }
    let (Some(name), Some(wav)) = (name, wav) else {
        return bad_req("need name/voice_name + audio_sample/voice_file");
    };
    match state
        .tts
        .upload_voice(&name, wav, ref_text.as_deref())
        .await
    {
        Ok(result) => axum::Json(json!({
            "id": result.id,
            "name": result.name,
            "mode": result.mode,
            "ref_frames": result.ref_frames,
        }))
        .into_response(),
        Err(e) => upstream_err("tts", e.to_string()),
    }
}

async fn tier1_upload_voice_authed(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Err(code) = require_auth(&state.db, state.api_key.as_deref(), &headers).await {
        return code.into_response();
    }
    tier1_upload_voice(State(state), headers, multipart).await
}

// ── TTS: POST /v1/audio/speech ────────────────────────────────────────

#[derive(Deserialize)]
struct OpenAiSpeechRequest {
    input: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    response_format: Option<String>,
    /// Render rate. `1.0` (or absent) is natural rate. A rate the engine cannot
    /// do falls back to 1.0 rather than failing — read `x-speed-applied`.
    #[serde(default)]
    speed: Option<f64>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    top_k: Option<u32>,
    #[serde(default)]
    repetition_penalty: Option<f64>,
    #[serde(default)]
    instructions: Option<String>,
    /// Built-in speaker token `"S0"`..`"S9"`.
    #[serde(default)]
    speaker: Option<String>,
    #[serde(default)]
    seed: Option<u64>,
    /// 12.5 Hz frames, not audio tokens.
    #[serde(default)]
    max_new_frames: Option<u32>,
    #[serde(default)]
    stream_batch_size: Option<u32>,
    #[serde(default)]
    stream_first_batch_size: Option<u32>,
    /// Long-form mode; defaults on. Callers must not pre-chunk.
    #[serde(default)]
    long: Option<bool>,
    #[serde(default)]
    chunk_words: Option<u32>,
}

/// Buffered OpenAI-mirror synth.
///
/// The response is a single blob (that is what the mirror promises), but the
/// UPSTREAM request always streams: streamed peak VRAM is flat in output
/// length where buffered grows with it (~1 GB on a long render), and the
/// gate's TTS reservation is sized on the streamed figure — buffering upstream
/// would silently overrun it.
///
/// `long` is on, so callers must NOT pre-chunk; the engine chunks internally
/// with rolling context, which is what holds one voice across a whole passage.
///
/// Only `wav` (default) and `pcm` are offered here. Compressed containers would
/// need a re-encode of the streamed PCM; callers wanting one should use the
/// tier-2 `/api/v1/tts/generate` endpoint with an explicit buffered transport.
async fn tier1_speech(
    State(state): State<AppState>,
    _headers: HeaderMap,
    axum::Json(req): axum::Json<OpenAiSpeechRequest>,
) -> Response {
    let want = req.response_format.as_deref().unwrap_or("wav");
    if !matches!(want, "wav" | "pcm") {
        return bad_req(format!(
            "response_format '{want}' is not available on /v1/audio/speech \
             (supported: wav, pcm); use /api/v1/tts/generate for {want}"
        ));
    }

    let upstream = SpeechRequest {
        input: req.input,
        // `default` is a real selectable voice (the engine's own narrator), so
        // it passes through; only an empty string means "say nothing about it".
        voice: req.voice.filter(|v| !v.is_empty()),
        response_format: "pcm".to_string(),
        instructions: req.instructions,
        speaker: req.speaker,
        bitrate_kbps: None,
        speed: req.speed,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        repetition_penalty: req.repetition_penalty,
        seed: req.seed,
        max_new_frames: req.max_new_frames,
        stream_format: Some("sse".to_string()),
        stream_batch_size: req.stream_batch_size,
        stream_first_batch_size: Some(req.stream_first_batch_size.unwrap_or(1)),
        long: Some(req.long.unwrap_or(true)),
        chunk_words: req.chunk_words,
        buffer: None,
        // Alignment is a tier-2 concern; this mirror returns audio only.
        align: None,
        align_stream: None,
        align_device: None,
    };

    let handle = match state.tts.synthesize_stream(&upstream).await {
        Ok(h) => h,
        Err(e @ crate::tts_breeze::Error::Disabled) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
        Err(e) => return upstream_err("tts", e.to_string()),
    };

    // Hold the GPU guard until the upstream stream has fully drained.
    let _guard = handle.guard;
    let speed_applied = handle.speed_applied;
    let (pcm, truncated) = match crate::tts_breeze::collect_sse(handle.response).await {
        Ok(p) => p,
        Err(e) => return upstream_err("tts", e.to_string()),
    };
    drop(_guard);

    let (content_type, body) = if want == "pcm" {
        ("audio/L16; rate=24000; channels=1".to_string(), pcm)
    } else {
        (
            "audio/wav".to_string(),
            crate::tts_breeze::wav_from_pcm(&pcm),
        )
    };

    // Mirrors the engine's own signal. `long` is on here so this should never
    // fire; if it does, the caller is holding a short mid-sentence clip that
    // otherwise looks like a complete render.
    if truncated {
        warn!("tts truncated a render despite long-form being on");
    }

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("x-truncated", if truncated { "1" } else { "0" })
        // What the render actually ran at — not necessarily what was asked for.
        .header("x-speed-applied", speed_applied.to_string())
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn tier1_speech_authed(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<OpenAiSpeechRequest>,
) -> Response {
    if let Err(code) = require_auth(&state.db, state.api_key.as_deref(), &headers).await {
        return code.into_response();
    }
    tier1_speech(State(state), headers, axum::Json(req)).await
}

// ── STT: POST /v1/audio/transcriptions ────────────────────────────────

async fn tier1_transcribe(State(state): State<AppState>, multipart: Multipart) -> Response {
    match do_transcribe(state, multipart, None).await {
        Ok(resp) => resp,
        Err(e) => e,
    }
}

async fn tier1_transcribe_authed(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Err(code) = require_auth(&state.db, state.api_key.as_deref(), &headers).await {
        return code.into_response();
    }
    tier1_transcribe(State(state), multipart).await
}

async fn do_transcribe(
    state: AppState,
    mut multipart: Multipart,
    cancel_url_override: Option<String>,
) -> Result<Response, Response> {
    let mut file_bytes: Option<(String, Bytes)> = None;
    let mut file_path: Option<String> = None;
    let mut params = SttParams::default();
    let mut cancel_url = cancel_url_override;

    while let Some(field) = multipart.next_field().await.map_err(bad_req)? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                let filename = field.file_name().unwrap_or("upload.wav").to_string();
                let bytes = field.bytes().await.map_err(bad_req)?;
                file_bytes = Some((filename, bytes));
            }
            "file_path" => {
                file_path = Some(field.text().await.map_err(bad_req)?);
            }
            "timestamp_granularity" => {
                params.timestamp_granularity = Some(field.text().await.map_err(bad_req)?);
            }
            "beam_size" => {
                let v = field.text().await.map_err(bad_req)?;
                params.beam_size = v.parse().ok();
            }
            "language" => {
                params.language = Some(field.text().await.map_err(bad_req)?);
            }
            "cancel_url" => {
                cancel_url = Some(field.text().await.map_err(bad_req)?);
            }
            _ => {
                // Silently ignore unknown fields (model, response_format, etc.)
                let _ = field.text().await;
            }
        }
    }

    let result = if let Some((name, bytes)) = file_bytes {
        state
            .stt
            .transcribe_upload(&name, bytes, &params, cancel_url.as_deref())
            .await
    } else if let Some(path) = file_path {
        state
            .stt
            .transcribe_file_path(&path, &params, cancel_url.as_deref())
            .await
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "provide file or file_path"})),
        )
            .into_response());
    };

    match result {
        Ok((_permit, resp, _gate_wait)) => Ok(axum::Json(resp).into_response()),
        Err(e) => Ok(upstream_err("stt", e.to_string())),
    }
}

fn bad_req<E: std::fmt::Display>(e: E) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"error": e.to_string()})),
    )
        .into_response()
}

// ── Vision embeddings: POST /v1/embeddings ────────────────────────────

async fn tier1_embed(State(state): State<AppState>, multipart: Multipart) -> Response {
    match do_embed(state, multipart).await {
        Ok(resp) => resp,
        Err(e) => e,
    }
}

async fn tier1_embed_authed(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Err(code) = require_auth(&state.db, state.api_key.as_deref(), &headers).await {
        return code.into_response();
    }
    tier1_embed(State(state), multipart).await
}

async fn do_embed(state: AppState, mut multipart: Multipart) -> Result<Response, Response> {
    let mut images: Vec<(String, Bytes)> = Vec::new();
    let mut max_num_patches: Option<u32> = None;
    let mut pooling: Option<String> = None;
    let mut return_last_hidden = false;

    while let Some(field) = multipart.next_field().await.map_err(bad_req)? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "images" => {
                let filename = field.file_name().unwrap_or("image.png").to_string();
                let bytes = field.bytes().await.map_err(bad_req)?;
                images.push((filename, bytes));
            }
            "max_num_patches" => {
                max_num_patches = field.text().await.map_err(bad_req)?.parse().ok();
            }
            "pooling" => pooling = Some(field.text().await.map_err(bad_req)?),
            "return_last_hidden" => {
                let v = field.text().await.map_err(bad_req)?;
                return_last_hidden = matches!(v.as_str(), "true" | "1" | "yes");
            }
            _ => {
                let _ = field.text().await;
            }
        }
    }

    if images.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "no images provided"})),
        )
            .into_response());
    }

    match state
        .vision
        .embed(
            &images,
            max_num_patches,
            pooling.as_deref(),
            return_last_hidden,
        )
        .await
    {
        Ok((_permit, resp, _gate_wait)) => {
            // OpenAI-compatible wrap: {data: [{embedding: [...]}, ...]}
            let data: Vec<_> = resp
                .embeddings
                .iter()
                .enumerate()
                .map(|(i, emb)| json!({"object": "embedding", "embedding": emb, "index": i}))
                .collect();
            let mut body = json!({"object": "list", "data": data, "model": "siglip2-naflex"});
            if let Some(lhs) = resp.last_hidden_state {
                body["last_hidden_state"] = json!(lhs);
            }
            Ok(axum::Json(body).into_response())
        }
        Err(e) => Ok(upstream_err("vision", e.to_string())),
    }
}

// ── Vision classify: POST /v1/classify ────────────────────────────────

async fn tier1_classify(State(state): State<AppState>, multipart: Multipart) -> Response {
    match do_classify(state, multipart).await {
        Ok(resp) => resp,
        Err(e) => e,
    }
}

async fn tier1_classify_authed(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Err(code) = require_auth(&state.db, state.api_key.as_deref(), &headers).await {
        return code.into_response();
    }
    tier1_classify(State(state), multipart).await
}

async fn do_classify(state: AppState, mut multipart: Multipart) -> Result<Response, Response> {
    let mut images: Vec<(String, Bytes)> = Vec::new();
    let mut prompts: Vec<String> = Vec::new();
    let mut max_num_patches: Option<u32> = None;
    let mut return_logits = false;

    while let Some(field) = multipart.next_field().await.map_err(bad_req)? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "images" => {
                let filename = field.file_name().unwrap_or("image.png").to_string();
                let bytes = field.bytes().await.map_err(bad_req)?;
                images.push((filename, bytes));
            }
            "prompts" => prompts.push(field.text().await.map_err(bad_req)?),
            "max_num_patches" => {
                max_num_patches = field.text().await.map_err(bad_req)?.parse().ok();
            }
            "return_logits" => {
                let v = field.text().await.map_err(bad_req)?;
                return_logits = matches!(v.as_str(), "true" | "1" | "yes");
            }
            _ => {
                let _ = field.text().await;
            }
        }
    }

    if images.is_empty() || prompts.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "need images and prompts"})),
        )
            .into_response());
    }

    match state
        .vision
        .classify(&images, &prompts, max_num_patches, return_logits)
        .await
    {
        Ok((_permit, resp, _gate_wait)) => {
            let mut body = json!({"scores": resp.scores});
            if let Some(logits) = resp.logits {
                body["logits"] = json!(logits);
            }
            Ok(axum::Json(body).into_response())
        }
        Err(e) => Ok(upstream_err("vision", e.to_string())),
    }
}

// ── Vision text embeddings: POST /v1/text_embeddings ───────────────────

async fn tier1_text_embeddings(
    State(state): State<AppState>,
    Json(req): Json<VisionTextEmbedRequest>,
) -> Response {
    do_text_embeddings(state, req).await
}

async fn tier1_text_embeddings_authed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<VisionTextEmbedRequest>,
) -> Response {
    if let Err(code) = require_auth(&state.db, state.api_key.as_deref(), &headers).await {
        return code.into_response();
    }
    do_text_embeddings(state, req).await
}

async fn do_text_embeddings(state: AppState, req: VisionTextEmbedRequest) -> Response {
    if req.prompts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "prompts must be non-empty"})),
        )
            .into_response();
    }
    match state.vision.text_embeddings(&req.prompts).await {
        Ok((_permit, resp, gate_wait)) => {
            let body = VisionTextEmbedResponse {
                embeddings: resp.embeddings,
                duration_ms: None,
                gate_wait_ms: Some(gate_wait.as_millis() as u64),
                gpu_log_id: None,
            };
            axum::Json(body).into_response()
        }
        Err(e) => upstream_err("vision", e.to_string()),
    }
}

// ── Vision classify-from-embeddings: POST /v1/classify_from_embeddings ──

async fn tier1_classify_from_embeddings(
    State(state): State<AppState>,
    Json(req): Json<VisionClassifyFromEmbedRequest>,
) -> Response {
    do_classify_from_embeddings(state, req).await
}

async fn tier1_classify_from_embeddings_authed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<VisionClassifyFromEmbedRequest>,
) -> Response {
    if let Err(code) = require_auth(&state.db, state.api_key.as_deref(), &headers).await {
        return code.into_response();
    }
    do_classify_from_embeddings(state, req).await
}

async fn do_classify_from_embeddings(
    state: AppState,
    req: VisionClassifyFromEmbedRequest,
) -> Response {
    if req.image_embeddings.is_empty() || req.prompts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "image_embeddings and prompts must both be non-empty"})),
        )
            .into_response();
    }
    let return_logits = req.return_logits.unwrap_or(false);
    match state
        .vision
        .classify_from_embeddings(&req.image_embeddings, &req.prompts, return_logits)
        .await
    {
        Ok((_permit, resp, gate_wait)) => {
            let body = VisionClassifyFromEmbedResponse {
                scores: resp.scores,
                logits: resp.logits,
                duration_ms: None,
                gate_wait_ms: Some(gate_wait.as_millis() as u64),
                gpu_log_id: None,
            };
            axum::Json(body).into_response()
        }
        Err(e) => upstream_err("vision", e.to_string()),
    }
}

fn upstream_err(service: &str, detail: String) -> Response {
    warn!(service, detail, "tier1 upstream error");
    (
        StatusCode::BAD_GATEWAY,
        axum::Json(json!({"error": format!("{service} upstream error: {detail}")})),
    )
        .into_response()
}
