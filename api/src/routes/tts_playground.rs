//! Tier-2 TTS playground routes — exposes the full `tts-breeze` knob set.
//! Auth-gated.
//!
//! The generate handler streams the upstream response straight to the client
//! via an mpsc channel, so TTFA is real. A spawned forwarder also accumulates
//! the bytes for `save_sample` and the gpu_logs row — and emits the
//! `x-koblem-ttfa-ms` HTTP trailer once the first byte lands.

use std::time::Instant;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use bytes::Bytes;
use futures::StreamExt;
use koblem_client::types::{
    TtsGenerateRequest, TtsRegisterFromLibraryRequest, TtsUploadResponse, TtsVoice,
    TtsVoicesResponse,
};
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::tier2_helpers::{
    OutputBytes, extract_caller, log_async, persist_generation, save_sample,
};
use crate::tts_breeze::SpeechRequest;

const TTS_BODY_LIMIT: usize = 100 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/tts/generate", post(generate))
        .route("/api/v1/tts/voices", get(list_voices).post(upload_voice))
        .route(
            "/api/v1/tts/voices/from-library",
            post(register_from_library),
        )
        .route("/api/v1/tts/voices/{name}", delete(delete_voice))
        .route(
            "/api/v1/tts/voices/{name}/sample.wav",
            get(download_voice_sample),
        )
        .route(
            "/api/v1/tts/voices/{name}/ref_text",
            get(download_voice_ref_text),
        )
        .layer(DefaultBodyLimit::max(TTS_BODY_LIMIT))
}

fn sample_extension(content_type: &str, response_format: &str) -> &'static str {
    if content_type.contains("mpeg") || response_format == "mp3" {
        "mp3"
    } else if content_type.contains("ogg") || response_format == "ogg" {
        "ogg"
    } else if content_type.contains("aac") || response_format == "aac" {
        "aac"
    } else if response_format == "pcm" {
        "pcm"
    } else {
        "wav"
    }
}

async fn generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TtsGenerateRequest>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let caller = extract_caller(&headers);
    let start = Instant::now();

    let upstream = upstream_request(&req);
    let response_format = upstream.response_format.clone();

    let handle = match state.tts.synthesize_stream(&upstream).await {
        Ok(h) => h,
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            log_async(
                state.db.clone(),
                CreateLog {
                    service: "tts".to_string(),
                    endpoint: "/api/v1/tts/generate".to_string(),
                    request_meta: serde_json::to_value(&req).unwrap_or(json!({})),
                    response_meta: json!({"error": e.to_string()}),
                    sample_id: None,
                    gate_wait_ms: 0,
                    duration_ms,
                    status: 502,
                    caller,
                },
            );
            warn!(error = %e, "tts generate failed");
            // Not an upstream failure: the caller asked for the one transport that
            // cannot run beside Beefy, and streaming it would.
            let status = if matches!(e, crate::tts_breeze::Error::BufferedDuringBeefy) {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_GATEWAY
            };
            return (status, Json(json!({"error": format!("tts: {e}")}))).into_response();
        }
    };

    let gate_wait_ms = handle.gate_wait.as_millis() as u64;
    let truncated = handle.truncated;
    let speed_applied = handle.speed_applied;
    let ct = handle.content_type.clone();
    let response_format_owned = response_format.clone();
    let save = req.save_sample;
    let req_json = serde_json::to_value(&req).unwrap_or(json!({}));
    let req_json_gen = req_json.clone();
    let db = state.db.clone();
    let samples_dir = state.samples_dir.clone();
    let state_for_gen = state.clone();

    // Tee the upstream stream: each chunk goes both to the client (via mpsc)
    // and into a Vec we use afterwards for save_sample + gpu_logs. Channel
    // size 16 keeps memory bounded; backpressure pushes back on reqwest if
    // the client is slow.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);

    tokio::spawn(async move {
        // Hold the GPU guard for the forwarding loop; dropped explicitly once
        // the stream drains (below), releasing the lock + arming the TTS hold
        // window before the post-stream disk/DB work runs.
        let _guard = handle.guard;

        let mut byte_stream = handle.response.bytes_stream();
        let mut accumulated: Vec<u8> = Vec::new();
        let mut ttfa_ms: Option<i64> = None;
        let mut errored = false;

        while let Some(item) = byte_stream.next().await {
            match item {
                Ok(chunk) => {
                    if ttfa_ms.is_none() {
                        ttfa_ms = Some(start.elapsed().as_millis() as i64);
                    }
                    accumulated.extend_from_slice(&chunk);
                    if tx.send(Ok(chunk)).await.is_err() {
                        // Client disconnected — abandon the stream but still
                        // log what we managed to read.
                        break;
                    }
                }
                Err(e) => {
                    errored = true;
                    let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                    break;
                }
            }
        }
        drop(tx);

        let total_ms = start.elapsed().as_millis() as i64;
        let byte_size = accumulated.len() as i64;
        let bytes = Bytes::from(accumulated);

        // GPU work is done once the stream drains — release the lock before the
        // disk/DB work (sample save, gallery persist, log) so it doesn't extend
        // the hold and block the next TTS/music render.
        drop(_guard);

        let sample_id = if save && !bytes.is_empty() && !errored {
            let ext = sample_extension(&ct, &response_format_owned);
            save_sample(&db, &samples_dir, "audio_out", &ct, ext, &bytes).await
        } else {
            None
        };

        let audio_duration_ms = audio_duration_ms_for(&response_format_owned, &bytes);

        // Gallery record — persisted ONLY when generations_dir is configured.
        if !errored && !bytes.is_empty() {
            let ext = sample_extension(&ct, &response_format_owned);
            persist_generation(
                &state_for_gen,
                "tts",
                "/api/v1/tts/generate",
                req_json_gen,
                &[OutputBytes {
                    ext: ext.to_string(),
                    mime: ct.clone(),
                    kind: "audio".to_string(),
                    bytes: bytes.clone(),
                }],
            )
            .await;
        }

        log_async(
            db,
            CreateLog {
                service: "tts".to_string(),
                endpoint: "/api/v1/tts/generate".to_string(),
                request_meta: req_json,
                response_meta: json!({
                    "byte_size": byte_size,
                    "content_type": ct,
                    "ttfa_ms": ttfa_ms,
                    "audio_duration_ms": audio_duration_ms,
                    "errored": errored,
                    "speed_applied": speed_applied,
                }),
                sample_id,
                gate_wait_ms: gate_wait_ms as i64,
                duration_ms: total_ms,
                status: if errored { 502 } else { 200 },
                caller,
            },
        );
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, handle.content_type)
        .header("x-koblem-gate-wait-ms", gate_wait_ms.to_string())
        // Forwarded from the engine. Only meaningful on the buffered path; the
        // streamed one carries truncation on `speech.audio.done` instead.
        .header("x-truncated", if truncated { "1" } else { "0" })
        // What the render actually ran at — not necessarily what was asked for.
        .header("x-speed-applied", speed_applied.to_string())
        // Defeat nginx response buffering — we want chunks delivered as they
        // arrive so the browser measures real TTFA.
        .header("x-accel-buffering", "no")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap()
}

/// Best-effort audio-duration computation for the gpu_logs row. Returns ms.
/// WAV: parse header (sample-rate, bits/sample, channels) and use the byte
/// count of the PCM payload (skips header). PCM: `tts-breeze` emits 24 kHz
/// mono 16-bit. Compressed (mp3/ogg/aac) returns None — the browser is the
/// authoritative duration for those.
fn audio_duration_ms_for(response_format: &str, bytes: &Bytes) -> Option<i64> {
    match response_format {
        "wav" => parse_wav_duration_ms(bytes),
        "pcm" => {
            // 24 kHz 16-bit mono = 48000 bytes/s.
            Some(((bytes.len() as f64) / 48.0) as i64)
        }
        _ => None,
    }
}

fn parse_wav_duration_ms(bytes: &Bytes) -> Option<i64> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]) as u32;
    let sample_rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    let bits_per_sample = u16::from_le_bytes([bytes[34], bytes[35]]) as u32;
    if channels == 0 || sample_rate == 0 || bits_per_sample == 0 {
        return None;
    }
    // Streaming WAV writes 0xFFFFFFFF in the data-size field; fall back to
    // bytes-after-header in that case.
    let data_size = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
    let payload_bytes = if data_size == u32::MAX || data_size == 0 {
        bytes.len().saturating_sub(44) as u64
    } else {
        data_size as u64
    };
    let bytes_per_sec = (sample_rate * channels * bits_per_sample / 8) as u64;
    if bytes_per_sec == 0 {
        return None;
    }
    Some(((payload_bytes * 1000) / bytes_per_sec) as i64)
}

async fn list_voices(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match state.tts.list_voices().await {
        Ok(payload) => {
            let out = TtsVoicesResponse {
                voices: payload
                    .voices
                    .into_iter()
                    .map(|v| TtsVoice {
                        id: v.id,
                        name: Some(v.name),
                        builtin: v.builtin,
                        frames: v.frames,
                        codebooks: v.codebooks,
                        has_ref_text: v.has_ref_text,
                        has_sample: v.has_sample,
                    })
                    .collect(),
                model_id: payload.model_id,
            };
            Json(out).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("tts: {e}")})),
        )
            .into_response(),
    }
}

async fn upload_voice(
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

    let mut name: Option<String> = None;
    let mut wav_bytes: Option<bytes::Bytes> = None;
    let mut ref_text: Option<String> = None;

    while let Some(field) = match multipart.next_field().await {
        Ok(Some(f)) => Some(f),
        Ok(None) => None,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    } {
        match field.name().unwrap_or("") {
            // Accept both engine-native ("audio_sample"/"name") and chatterbox-
            // shape ("voice_file"/"voice_name") so tooling can submit either.
            "name" | "voice_name" => {
                name = field.text().await.ok();
            }
            "audio_sample" | "voice_file" => {
                wav_bytes = field.bytes().await.ok();
            }
            "ref_text" => {
                ref_text = field.text().await.ok();
            }
            _ => {}
        }
    }

    let (Some(name), Some(wav_bytes)) = (name, wav_bytes) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "need name + audio_sample"})),
        )
            .into_response();
    };

    let wav_bytes = peak_normalize_voice_clip(wav_bytes);

    match state
        .tts
        .upload_voice(&name, wav_bytes, ref_text.as_deref())
        .await
    {
        Ok(result) => Json(TtsUploadResponse {
            id: result.id,
            name: result.name,
            mode: result.mode,
            ref_frames: result.ref_frames,
            frames: result.ref_frames,
            codebooks: None,
        })
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("tts: {e}")})),
        )
            .into_response(),
    }
}

async fn delete_voice(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match state.tts.delete_voice(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("tts: {e}")})),
        )
            .into_response(),
    }
}

async fn download_voice_sample(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match state.tts.get_voice_sample(&name).await {
        Ok((bytes, ct)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, ct)
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}.wav\"", sanitize_filename(&name)),
            )
            .body(axum::body::Body::from(bytes))
            .unwrap(),
        Err(crate::tts_breeze::Error::Upstream(404, _)) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("tts: {e}")})),
        )
            .into_response(),
    }
}

async fn download_voice_ref_text(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match state.tts.get_voice_ref_text(&name).await {
        Ok(text) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{}-ref.txt\"",
                    sanitize_filename(&name)
                ),
            )
            .body(axum::body::Body::from(text))
            .unwrap(),
        Err(crate::tts_breeze::Error::Upstream(404, _)) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("tts: {e}")})),
        )
            .into_response(),
    }
}

/// Peak-normalise a WAV voice clip to -1 dBFS before it lands at tts-breeze.
/// Quiet reference clips (e.g. phone-mic recordings) make the engine clone a
/// quiet voice — this boosts envelope while never clipping. Idempotent.
/// On any parse failure we hand the original bytes through unchanged.
fn peak_normalize_voice_clip(bytes: bytes::Bytes) -> bytes::Bytes {
    let mut buf = bytes.to_vec();
    match kob_audio_norm::peak_normalize_wav_in_place(
        &mut buf,
        kob_audio_norm::DEFAULT_TARGET_PEAK_DBFS,
    ) {
        Ok(report) if report.scale > 1.0 => {
            tracing::info!(
                scale = report.scale,
                peak_before = report.peak_before,
                peak_after = report.peak_after,
                "peak-normalised TTS voice clip"
            );
            bytes::Bytes::from(buf)
        }
        Ok(_) => bytes,
        Err(e) => {
            tracing::warn!(error = %e, "skipping peak normalisation; uploading raw clip");
            bytes
        }
    }
}

/// Strip path-traversal chars from a voice name before stuffing it into a
/// Content-Disposition filename. The upstream service already validates the
/// name shape; this is belt-and-braces for the response header.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `POST /api/v1/tts/voices/from-library` — register a clone whose
/// audio source is a sliced kobbler audiobook chapter. Calls
/// `library_provider.fetch_audiobook_clip` (HTTP to kobbler), optionally
/// transcribes the slice via parakeet for ICL `ref_text`, then forwards to
/// the existing voice upload path.
async fn register_from_library(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TtsRegisterFromLibraryRequest>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

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

    if req.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name is required"})),
        )
            .into_response();
    }

    let wav_bytes = match library
        .fetch_audiobook_clip(
            &req.audiobook_id,
            req.chapter_index,
            req.offset_seconds,
            req.duration_seconds,
        )
        .await
    {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "library fetch_audiobook_clip failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("library: {e}")})),
            )
                .into_response();
        }
    };

    // Defence in depth — kobbler normalises voice clips at write time, but if
    // a future caller bypasses that path we still want a sensible reference
    // envelope for the reference transcript. Idempotent: loud clips are skipped.
    let wav_bytes = peak_normalize_voice_clip(wav_bytes);

    // Best-effort STT for ref_text — the upload path treats ref_text as
    // optional, so a parakeet failure shouldn't sink the whole register.
    let ref_text = if req.transcribe {
        let filename = format!("{}.wav", req.name);
        match state
            .stt
            .transcribe_upload(
                &filename,
                wav_bytes.clone(),
                &crate::stt::SttParams::default(),
                None,
            )
            .await
        {
            Ok((_permit, parsed, _)) => Some(parsed.text),
            Err(e) => {
                warn!(error = %e, "stt transcribe_upload failed during from-library register; continuing without ref_text");
                None
            }
        }
    } else {
        None
    };

    match state
        .tts
        .upload_voice(&req.name, wav_bytes, ref_text.as_deref())
        .await
    {
        Ok(result) => Json(TtsUploadResponse {
            id: result.id,
            name: result.name,
            mode: result.mode,
            ref_frames: result.ref_frames,
            frames: result.ref_frames,
            codebooks: None,
        })
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("tts: {e}")})),
        )
            .into_response(),
    }
}

/// Pick the transport pair for a synth request.
///
/// Streaming is the default: streamed peak VRAM is flat in output length while
/// buffered grows with it (~1 GB on a 4.5-minute render), and alignment only
/// works on the SSE path — a buffered `align: true` request is rejected 400.
///
/// The engine offers two streaming transports. **`sse` is the one to reach for**:
/// it is the only one that can carry forced alignment, so it is what every
/// read-along path uses. `audio` streams raw chunks with no envelope — cheaper
/// to parse, but no alignment and no `speech.audio.done`, which is also where
/// the truncation flag lives.
///
/// The one case that must stay buffered is "give me a file": a caller that asks
/// for a container format without naming a transport wants bytes it can save,
/// not an SSE stream. `pcm` is not a container, so it does not trigger that.
///
/// `None` out means BUFFERED. The engine has no third transport: it validates
/// `stream_format` against exactly `sse` and `audio` and 400s on anything else,
/// so koblem's own wire word for buffered (`"none"`) must not reach it. Buffered
/// is the ABSENCE of the field — see [`upstream_request`] for the second half of
/// that, which is easier to get wrong.
fn resolve_transport(
    stream_format: Option<&str>,
    response_format: Option<&str>,
) -> (Option<String>, String) {
    match (stream_format, response_format) {
        (Some(sf), rf) if is_buffered(sf) => (None, rf.unwrap_or("wav").to_string()),
        (Some(sf), Some(rf)) => (Some(sf.to_string()), rf.to_string()),
        // Both streaming transports default to raw PCM. A container's header
        // carries a length nobody knows until the render ends, so a streamed wav
        // declares 0xFFFFFFFF — which a length-trusting player reads as 2^31
        // frames, i.e. a ~25-hour file with a broken seek bar. (ffmpeg copes, so
        // this bites players rather than pipelines.)
        (Some(sf), None) => (Some(sf.to_string()), "pcm".to_string()),
        (None, Some(rf)) if rf != "pcm" => (None, rf.to_string()),
        (None, _) => (Some("sse".to_string()), "pcm".to_string()),
    }
}

/// koblem's own wire words for "one buffered blob". Deliberately a closed set
/// rather than "not streaming": an unrecognised transport keeps travelling to
/// the engine so the caller gets its 400 naming the supported values, which is
/// what `TtsGenerateRequest::stream_format` documents.
fn is_buffered(stream_format: &str) -> bool {
    matches!(stream_format, "none" | "")
}

/// Translate a tier-2 request into the engine's `/v1/audio/speech` body.
fn upstream_request(req: &TtsGenerateRequest) -> SpeechRequest {
    let (stream_format, response_format) =
        resolve_transport(req.stream_format.as_deref(), req.response_format.as_deref());
    // The batch knobs are not merely inert on the buffered path: the engine
    // treats a batch-size knob with no `stream_format` as a request to stream
    // and promotes the request to `stream_format="audio"`. So a buffered render
    // that still carries `stream_first_batch_size` comes back CHUNKED, and a
    // chunked wav's header declares 0xFFFFFFFF frames. Buffered means every
    // stream field is off the wire, not just the transport.
    let streaming = stream_format.is_some();
    SpeechRequest {
        input: req.input.clone(),
        // No voice means "let the engine pick one and hold it for the request",
        // which `long` keeps stable across chunks. Do NOT substitute a
        // "default" voice id — the engine has no such clone.
        voice: req.voice.clone().filter(|v| !v.is_empty()),
        response_format,
        instructions: req.instructions.clone(),
        speaker: req.speaker.clone(),
        bitrate_kbps: req.bitrate_kbps,
        // The client owns nothing about rate: the engine renders at it, and the
        // aligner reports against the rendered audio. A rate the engine can't do
        // falls back to 1.0 inside `synthesize_stream`, reported by the caller.
        speed: req.speed,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        repetition_penalty: req.repetition_penalty,
        seed: req.seed,
        max_new_frames: req.max_new_frames,
        stream_format,
        stream_batch_size: req.stream_batch_size.filter(|_| streaming),
        stream_first_batch_size: streaming.then(|| req.stream_first_batch_size.unwrap_or(1)),
        // Long-form is the default: it chunks internally with rolling context,
        // which is both the lowest-VRAM path and what holds one voice across a
        // whole document. Callers must not pre-chunk.
        long: Some(req.long.unwrap_or(true)),
        chunk_words: req.chunk_words,
        buffer: req.buffer,
        align: req.align,
        align_stream: req.align_stream.clone(),
        // Unset by default so `synthesize_stream` can answer it against what
        // the card is actually doing; a caller that names a device is obeyed.
        align_device: req.align_device.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_transport, upstream_request};
    use koblem_client::types::TtsGenerateRequest;

    fn streamed(sf: &str, rf: &str) -> (Option<String>, String) {
        (Some(sf.to_string()), rf.to_string())
    }

    fn buffered(rf: &str) -> (Option<String>, String) {
        (None, rf.to_string())
    }

    #[test]
    fn defaults_to_streamed_pcm() {
        assert_eq!(resolve_transport(None, None), streamed("sse", "pcm"));
    }

    /// Asking for a container format alone means "give me a file".
    #[test]
    fn a_bare_container_format_stays_buffered() {
        assert_eq!(resolve_transport(None, Some("wav")), buffered("wav"));
        assert_eq!(resolve_transport(None, Some("mp3")), buffered("mp3"));
    }

    /// pcm is not a container — it still streams.
    #[test]
    fn bare_pcm_still_streams() {
        assert_eq!(resolve_transport(None, Some("pcm")), streamed("sse", "pcm"));
    }

    /// `audio` is a streaming transport too, so it defaults to raw PCM rather
    /// than a container. A wav header written before the render ends declares
    /// 0xFFFFFFFF, which a length-trusting player reads as a ~25-hour file.
    #[test]
    fn the_audio_transport_defaults_to_raw_pcm() {
        assert_eq!(
            resolve_transport(Some("audio"), None),
            streamed("audio", "pcm")
        );
        // Still overridable — the caller owns the container choice.
        assert_eq!(
            resolve_transport(Some("audio"), Some("wav")),
            streamed("audio", "wav")
        );
    }

    #[test]
    fn an_explicit_transport_is_honoured() {
        assert_eq!(resolve_transport(Some("sse"), None), streamed("sse", "pcm"));
        assert_eq!(
            resolve_transport(Some("sse"), Some("wav")),
            streamed("sse", "wav")
        );
    }

    /// `"none"` is koblem's word for buffered, not the engine's — the engine
    /// validates against exactly `sse`/`audio` and 400s on everything else.
    #[test]
    fn the_none_transport_resolves_to_buffered_not_a_literal() {
        assert_eq!(resolve_transport(Some("none"), None), buffered("wav"));
        assert_eq!(
            resolve_transport(Some("none"), Some("mp3")),
            buffered("mp3")
        );
        assert_eq!(resolve_transport(Some(""), None), buffered("wav"));
    }

    fn wire(req: TtsGenerateRequest) -> serde_json::Value {
        serde_json::to_value(upstream_request(&req)).unwrap()
    }

    /// The 400 this replaced: `stream_format: "none"` went out verbatim and the
    /// engine rejected it. Buffered is the ABSENCE of the field.
    #[test]
    fn a_buffered_request_omits_every_stream_field() {
        for sf in [Some("none".to_string()), None] {
            let body = wire(TtsGenerateRequest {
                input: "hi".to_string(),
                stream_format: sf.clone(),
                response_format: Some("wav".to_string()),
                // A caller that leaves the playground's batch knobs populated
                // must still get a buffered render: the engine promotes a bare
                // batch knob to `stream_format="audio"`, so leaking either one
                // turns the saved file into a chunked wav with a 0xFFFFFFFF
                // header — a ~25-hour clip with a broken seek bar.
                stream_batch_size: Some(30),
                stream_first_batch_size: Some(1),
                ..Default::default()
            });
            let obj = body.as_object().unwrap();
            assert!(!obj.contains_key("stream_format"), "{sf:?}: {body}");
            assert!(!obj.contains_key("stream_batch_size"), "{sf:?}: {body}");
            assert!(
                !obj.contains_key("stream_first_batch_size"),
                "{sf:?}: {body}"
            );
            assert_eq!(obj["response_format"], "wav");
        }
    }

    #[test]
    fn a_streamed_request_keeps_the_batch_knobs() {
        let body = wire(TtsGenerateRequest {
            input: "hi".to_string(),
            stream_batch_size: Some(30),
            ..Default::default()
        });
        assert_eq!(body["stream_format"], "sse");
        assert_eq!(body["stream_batch_size"], 30);
        // Lowest TTFA, and the engine's own default is higher — so it is sent
        // even when the caller named no value.
        assert_eq!(body["stream_first_batch_size"], 1);
    }
}
