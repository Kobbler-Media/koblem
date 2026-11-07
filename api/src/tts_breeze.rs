//! HTTP client for the `tts-breeze` service (BreezeTTS 2) — koblem's only TTS
//! engine. Acquires the shared GPU lock as the TTS engine tagged `"breeze"`.
//!
//! Two engine behaviours drive the defaults callers should use:
//! `long` defaults to true server-side (the engine chunks internally with
//! rolling context, so callers must NOT pre-chunk), and streaming keeps peak
//! VRAM flat in output length where buffering grows with it.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, GpuTtsGuard};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

/// Streaming synth response — caller owns the GPU guard, the upstream HTTP
/// response (still open, body unread), the content-type header, and the
/// gate-wait duration. Drop the guard after the stream finishes (or when the
/// caller decides to abandon mid-stream) so the GPU can be released.
pub struct StreamHandle {
    pub guard: GpuTtsGuard,
    pub response: reqwest::Response,
    pub content_type: String,
    pub gate_wait: Duration,
    /// The engine's `X-Truncated` header, captured before the body is consumed.
    /// Set on the BUFFERED path only — a streamed render reports truncation on
    /// its `speech.audio.done` event instead (see [`collect_sse`]).
    pub truncated: bool,
    /// The rate this render actually ran at. Equals the requested `speed`
    /// unless the engine rejected it, in which case the request was retried
    /// once at 1.0 — see [`BreezeTtsClient::synthesize_stream`]. Callers must
    /// report this rather than the one they asked for.
    pub speed_applied: f64,
}

pub const TAG: &str = "breeze";
pub const UNLOAD_PATH: &str = "/v1/admin/unload";

/// The engine's model id, sent as `model` on every synth request.
pub const MODEL_ID: &str = "breeze-tts-2";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("TTS HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("TTS service returned {0}: {1}")]
    Upstream(u16, String),
    #[error("TTS service not configured")]
    NotConfigured,
    #[error("TTS administratively disabled")]
    Disabled,
    #[error(
        "buffered synthesis is refused while Beefy holds the box — its peak grows with output \
         length and will not fit beside the 27B; use stream_format \"sse\" or \"audio\""
    )]
    BufferedDuringBeefy,
}

/// Full tts-breeze knob set. Mirrors
/// `koblem_client::types::TtsGenerateRequest` field-for-field minus the
/// koblem-only `save_sample` book-keeping.
///
/// Deliberately absent: `language` (the engine is en/zh and infers from the
/// text). `speed` IS sent — it is a real server-side render parameter — but an
/// engine build that cannot do the requested rate rejects it, so it is only
/// ever set through [`BreezeTtsClient::synthesize_stream`], which owns the
/// fallback.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SpeechRequest {
    pub input: String,
    /// Omit to let the engine pick a voice and hold it for the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    pub response_format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Built-in speaker token `"S0"`..`"S9"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    /// Render rate. `None` or `1.0` renders at natural rate; anything else is a
    /// real render at that rate, so the alignment that comes back is already in
    /// the rendered timebase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repetition_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Classifier-free guidance strength for an `instructions` prompt. `1` (or
    /// absent) is a single pass; `4` is what the model recommends and makes the
    /// instruction land (clearly audible on emotion) at ~+30% render time. The
    /// engine ignores it on a prompt with no instruction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cfg_scale: Option<f64>,
    /// 12.5 Hz FRAMES, not audio tokens. Raising it past `n_ctx` converts a
    /// silent truncation into a hard `context overflow` — use `long` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_new_frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_batch_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_first_batch_size: Option<u32>,
    /// Long-form mode. Defaults to true server-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_words: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer: Option<u32>,
    /// Requires `stream_format = "sse"`; a buffered aligned request is 400.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align_stream: Option<String>,
    /// Where the forced aligner runs: "gpu", "cpu" or "off". Left unset, koblem
    /// chooses — see `synthesize_stream`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align_device: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceEntry {
    pub id: String,
    pub name: String,
    /// Shipped by the engine rather than cloned by the operator. `default` is
    /// the built-in — a real selectable value meaning "the model's own
    /// narrator" — and has no sample/transcript and cannot be deleted.
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub frames: Option<u32>,
    #[serde(default)]
    pub codebooks: Option<u32>,
    #[serde(default)]
    pub has_ref_text: bool,
    #[serde(default)]
    pub has_sample: bool,
}

#[derive(Debug, Clone)]
pub struct VoicesPayload {
    pub voices: Vec<VoiceEntry>,
    pub model_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct HealthInfo {
    pub healthy: bool,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UploadResult {
    pub id: String,
    pub name: String,
    pub mode: Option<String>,
    pub ref_frames: Option<u32>,
}

#[derive(Clone)]
pub struct BreezeTtsClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

impl BreezeTtsClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            base_url: Arc::new(RwLock::new(
                base_url.map(|s| s.trim_end_matches('/').to_string()),
            )),
            gate,
        }
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let mut guard = self.base_url.write().expect("TTS base_url poisoned");
        *guard = base_url.map(|s| s.trim_end_matches('/').to_string());
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("TTS base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    async fn acquire(&self) -> Option<(GpuTtsGuard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_tts().await?;
        Some((guard, start.elapsed()))
    }

    pub async fn health_info(&self) -> Result<HealthInfo, Error> {
        let url = self.url("/health")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Ok(HealthInfo::default());
        }
        // `/health` returns `{ "status": "ok", "model": "...", "loaded": bool }`.
        // `loaded` is lazy-load state, NOT health — the engine reports
        // `loaded: false` until the first synth and is still serving.
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let healthy = body
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s == "ok")
            .unwrap_or(true);
        let model = body
            .get("model_id")
            .or_else(|| body.get("model"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(HealthInfo { healthy, model })
    }

    /// Synthesize speech, returning a still-open upstream response. Caller
    /// drives `response.chunk()` / `bytes_stream()` to stream the body to its
    /// own client; this is what makes TTFA measurable and lets the playground
    /// browser start playback before synthesis finishes.
    ///
    /// The returned `GpuTtsGuard` holds the GPU lock — keep it alive until the
    /// upstream stream finishes (drop it in the spawned forwarder task).
    ///
    /// ## Speed falls back rather than failing
    ///
    /// `speed` is a real render parameter, but an engine build that cannot do
    /// the requested rate rejects the whole request with 400 — which would turn
    /// "I wanted 1.5x" into no audio at all. So a 400 on a speed-carrying
    /// request is retried ONCE with `speed` dropped, and the handle reports
    /// `speed_applied = 1.0`. A 400 for any other reason survives the retry and
    /// is returned with the engine's own message.
    pub async fn synthesize_stream(&self, req: &SpeechRequest) -> Result<StreamHandle, Error> {
        // 🔴 Beefy admits TTS beside the 27B against a 3000 MiB reservation sized on
        // the STREAMED peak (2,243 + 634 aligner). Buffered measured 4,309 and grows
        // with output length — admitting it would overrun the 3060 the 27B shares.
        // Every call funnels through here; only the playground ever goes buffered.
        if req.stream_format.is_none() && self.gate.exclusive().is_some() {
            return Err(Error::BufferedDuringBeefy);
        }
        let (guard, gate_wait) = self.acquire().await.ok_or(Error::Disabled)?;
        let url = self.url("/v1/audio/speech")?;
        // Serialise to a Value so transport-only fields (`model`, and the
        // gate-chosen card) can be added without bloating SpeechRequest.
        let mut body = serde_json::to_value(req).unwrap_or(Value::Null);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), Value::String(MODEL_ID.to_string()));
            // Every gated submit must carry `gpu` or a disabled card is ignored.
            if let Some(g) = guard.chosen_gpu() {
                obj.insert("gpu".into(), Value::String(g.to_string()));
            }
            // 🔴 Alignment costs ~630 MiB of VRAM on top of the worker's ~2,400.
            // Beside Beefy there is not room for both, and alignment is the half
            // of a read-along that can degrade without silencing the voice — so
            // it moves to the CPU (same timings, ~2.6 s per chunk, no VRAM)
            // rather than taking the request down with it. A caller that names a
            // device has already decided and is left alone.
            if req.align == Some(true)
                && req.align_device.is_none()
                && self.gate.exclusive().is_some()
            {
                obj.insert("align_device".into(), Value::String("cpu".into()));
            }
        }

        let requested_speed = req.speed.unwrap_or(1.0);
        let mut speed_applied = requested_speed;
        let mut response = self.http.post(&url).json(&body).send().await?;

        if retry_at_natural_speed(response.status().as_u16(), requested_speed) {
            warn!(
                requested_speed,
                "tts engine rejected the requested speed; re-rendering at 1.0"
            );
            if let Some(obj) = body.as_object_mut() {
                obj.remove("speed");
            }
            speed_applied = 1.0;
            response = self.http.post(&url).json(&body).send().await?;
        }

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("audio/wav")
            .to_string();
        let truncated = response
            .headers()
            .get("x-truncated")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        Ok(StreamHandle {
            guard,
            response,
            content_type,
            gate_wait,
            truncated,
            speed_applied,
        })
    }

    /// List voices.
    ///
    /// The engine returns BOTH shapes in one body: a legacy
    /// `{"<model_id>": ["default", ...]}` map, and a richer
    /// `{"model_id", "voices":[{id, builtin, frames, ...}]}` array. They list
    /// the same ids; the array just carries more per voice. Prefer the array;
    /// fall back to the map for older builds.
    pub async fn list_voices(&self) -> Result<VoicesPayload, Error> {
        let url = self.url("/v1/audio/voices")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let raw: Value = resp.json().await?;
        Ok(parse_voices_payload(&raw))
    }

    /// Upload a clone-target voice. `ref_text` is the known transcript of the
    /// clip; supplying it materially improves cloning fidelity.
    pub async fn upload_voice(
        &self,
        name: &str,
        wav_bytes: Bytes,
        ref_text: Option<&str>,
    ) -> Result<UploadResult, Error> {
        let url = self.url("/v1/audio/voices")?;
        let mut form = Form::new().text("name", name.to_string()).part(
            "audio_sample",
            Part::bytes(wav_bytes.to_vec())
                .file_name(format!("{}.wav", name))
                .mime_str("audio/wav")
                .unwrap(),
        );
        if let Some(t) = ref_text
            && !t.is_empty()
        {
            form = form.text("ref_text", t.to_string());
        }
        let resp = self.http.post(url).multipart(form).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        #[derive(Deserialize)]
        struct Raw {
            id: String,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            mode: Option<String>,
            /// The engine reports the encoded reference length as both
            /// `ref_frames` and `frames`; either is acceptable.
            #[serde(default)]
            ref_frames: Option<u32>,
            #[serde(default)]
            frames: Option<u32>,
        }
        let raw: Raw = resp.json().await?;
        let name = raw.name.unwrap_or_else(|| raw.id.clone());
        Ok(UploadResult {
            id: raw.id,
            name,
            mode: raw.mode,
            ref_frames: raw.ref_frames.or(raw.frames),
        })
    }

    /// Fetch the original ref.wav for a registered clone. Returns the raw
    /// WAV bytes; the upstream content-type is preserved as the second tuple
    /// item. 404 propagates as `Error::Upstream(404, ..)`.
    pub async fn get_voice_sample(&self, name: &str) -> Result<(Bytes, String), Error> {
        let encoded = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
        let url = self.url(&format!("/v1/audio/voices/{}/sample.wav", encoded))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("audio/wav")
            .to_string();
        let bytes = resp.bytes().await?;
        Ok((bytes, content_type))
    }

    /// Fetch the reference transcript for a registered clone (text/plain).
    /// 404 when the clone was registered without one.
    pub async fn get_voice_ref_text(&self, name: &str) -> Result<String, Error> {
        let encoded = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
        let url = self.url(&format!("/v1/audio/voices/{}/ref_text", encoded))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        Ok(resp.text().await?)
    }

    /// Delete a registered clone.
    pub async fn delete_voice(&self, name: &str) -> Result<(), Error> {
        let encoded = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
        let url = self.url(&format!("/v1/audio/voices/{}", encoded))?;
        let resp = self.http.delete(url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        Ok(())
    }
}

/// Sample rate of every `tts-breeze` render. A mismatch against this is a pitch
/// shift, not an exception, so it is named once here and referenced.
pub const SAMPLE_RATE_HZ: u32 = 24_000;

/// Drain an SSE synth response into one contiguous PCM buffer, reporting
/// whether the engine truncated.
///
/// Used by the buffered callers: the response they hand on is a single blob,
/// but the UPSTREAM request still streams, because streamed peak VRAM is flat
/// in output length where buffered grows with it — and the gate's TTS
/// reservation is sized on the streamed figure.
///
/// The bool is the engine's own truncation flag from `speech.audio.done`.
/// `long: false` stops at `max_new_frames` mid-sentence and returns a valid,
/// SHORT clip — a failure that decodes and plays like a success, so a caller
/// that drops the flag ships half a script without noticing.
pub async fn collect_sse(response: reqwest::Response) -> Result<(Bytes, bool), Error> {
    use base64::Engine as _;
    use futures::StreamExt;

    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut pcm: Vec<u8> = Vec::new();
    let mut truncated = false;

    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        // SSE frames are separated by a blank line.
        while let Some(sep) = find_frame_end(&buf) {
            let frame = buf.drain(..sep.0).collect::<Vec<u8>>();
            buf.drain(..sep.1);
            match frame_event(&frame) {
                Some(FrameEvent::Audio(b64)) => {
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        pcm.extend_from_slice(&decoded);
                    }
                }
                Some(FrameEvent::Done { truncated: t }) => truncated |= t,
                None => {}
            }
        }
    }
    Ok((Bytes::from(pcm), truncated))
}

enum FrameEvent {
    Audio(String),
    Done { truncated: bool },
}

/// Offset of the first frame separator and its length (`\n\n` or `\r\n\r\n`).
fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

/// Classify an SSE frame: an audio delta, the terminal `done` (which carries
/// the truncation flag), or something this caller does not care about.
fn frame_event(frame: &[u8]) -> Option<FrameEvent> {
    let text = std::str::from_utf8(frame).ok()?;
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim());
        }
    }
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(&data).ok()?;
    match v.get("type").and_then(|t| t.as_str())? {
        "speech.audio.delta" => v
            .get("audio")
            .and_then(|a| a.as_str())
            .map(|s| FrameEvent::Audio(s.to_string())),
        "speech.audio.done" => Some(FrameEvent::Done {
            truncated: v
                .get("truncated")
                .and_then(|t| t.as_bool())
                .unwrap_or(false),
        }),
        _ => None,
    }
}

/// Whether a failed synth should be retried with `speed` dropped.
///
/// Only a 400 on a request that asked for a non-natural rate qualifies. Every
/// other status is a real failure (502 upstream, 503 unloaded, …) and retrying
/// would just burn a second render; a 400 on a request that never asked for a
/// rate has nothing to drop and would loop on the same error.
fn retry_at_natural_speed(status: u16, requested_speed: f64) -> bool {
    status == 400 && (requested_speed - 1.0).abs() > f64::EPSILON
}

/// Wrap raw s16le mono PCM in a RIFF/WAVE header.
pub fn wav_from_pcm(pcm: &[u8]) -> Bytes {
    let channels: u16 = 1;
    let bits: u16 = 16;
    let byte_rate = SAMPLE_RATE_HZ * channels as u32 * (bits / 8) as u32;
    let block_align = channels * (bits / 8);
    let data_len = pcm.len() as u32;

    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE_HZ.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    Bytes::from(out)
}

/// Split out of [`BreezeTtsClient::list_voices`] so the dual-shape handling is
/// unit-testable without HTTP.
fn parse_voices_payload(raw: &Value) -> VoicesPayload {
    let model_id = raw
        .get("model_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if let Some(arr) = raw.get("voices").and_then(|v| v.as_array()) {
        let voices = arr
            .iter()
            .filter_map(|v| {
                let id = v.get("id")?.as_str()?.to_string();
                let name = v
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or(&id)
                    .to_string();
                Some(VoiceEntry {
                    id,
                    name,
                    builtin: v.get("builtin").and_then(|b| b.as_bool()).unwrap_or(false),
                    frames: v.get("frames").and_then(|f| f.as_u64()).map(|f| f as u32),
                    codebooks: v
                        .get("codebooks")
                        .and_then(|f| f.as_u64())
                        .map(|f| f as u32),
                    has_ref_text: v
                        .get("has_ref_text")
                        .and_then(|f| f.as_bool())
                        .unwrap_or(false),
                    has_sample: v
                        .get("has_sample")
                        .and_then(|f| f.as_bool())
                        .unwrap_or(false),
                })
            })
            .collect();
        return VoicesPayload { voices, model_id };
    }

    // Legacy map shape: the first key whose value is a string array.
    let mut out = VoicesPayload {
        voices: Vec::new(),
        model_id,
    };
    if let Some(map) = raw.as_object() {
        for (k, v) in map {
            if let Some(arr) = v.as_array() {
                out.model_id = Some(k.clone());
                out.voices = arr
                    .iter()
                    .filter_map(|n| n.as_str())
                    .map(|s| VoiceEntry {
                        // The legacy map has no per-voice detail, so the only
                        // built-in we can name is the one the engine documents.
                        builtin: s == "default",
                        id: s.to_string(),
                        name: s.to_string(),
                        frames: None,
                        codebooks: None,
                        has_ref_text: false,
                        has_sample: false,
                    })
                    .collect();
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Refused before any GPU guard or network call — the URL is unroutable on
    /// purpose, so reaching the engine would fail differently.
    #[tokio::test]
    async fn buffered_synthesis_is_refused_while_the_box_is_held() {
        let gate = GpuLock::new();
        gate.set_exclusive(Some(kob_gpu_gate::ServiceKind::Llm));
        let client = BreezeTtsClient::new(Some("http://127.0.0.1:9".into()), gate);
        let buffered = SpeechRequest {
            input: "hello".into(),
            response_format: "wav".into(),
            ..Default::default()
        };
        assert!(matches!(
            client.synthesize_stream(&buffered).await,
            Err(Error::BufferedDuringBeefy)
        ));
    }

    /// The engine returns both shapes at once and they list the SAME ids; the
    /// array just carries more per voice. It wins because only it distinguishes
    /// a built-in from a clone.
    #[test]
    fn prefers_the_rich_array_over_the_legacy_map() {
        let raw: Value = serde_json::from_str(
            r#"{"breeze-tts-2":["default","gaiman"],"model_id":"breeze-tts-2",
                "voices":[{"id":"default","builtin":true},
                          {"id":"gaiman","frames":150,"codebooks":16,
                           "has_ref_text":true,"has_sample":true}]}"#,
        )
        .unwrap();
        let p = parse_voices_payload(&raw);
        assert_eq!(p.model_id.as_deref(), Some("breeze-tts-2"));
        assert_eq!(
            p.voices.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            ["default", "gaiman"],
            "`default` is a real selectable voice, not a pseudo-entry to filter"
        );
        assert!(p.voices[0].builtin, "default is the engine's own narrator");
        assert!(!p.voices[1].builtin, "a clone is not a built-in");
        assert_eq!(
            p.voices[1].name, "gaiman",
            "id mirrors into name for the UI"
        );
        assert_eq!(p.voices[1].frames, Some(150));
        assert!(p.voices[1].has_ref_text);
    }

    /// An empty `voices` array is authoritative — it must not fall back to the
    /// legacy map and resurrect entries the array deliberately omitted.
    #[test]
    fn empty_rich_array_does_not_fall_back() {
        let raw: Value = serde_json::from_str(
            r#"{"breeze-tts-2":["default"],"model_id":"breeze-tts-2","voices":[]}"#,
        )
        .unwrap();
        assert!(parse_voices_payload(&raw).voices.is_empty());
    }

    #[test]
    fn sse_frames_decode_to_pcm_and_ignore_non_audio_events() {
        let b64 = |b: &[u8]| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(b)
        };
        let frame = format!(
            "event: speech.audio.delta\ndata: {{\"type\":\"speech.audio.delta\",\"audio\":\"{}\"}}",
            b64(&[1u8, 2, 3, 4])
        );
        assert!(matches!(
            frame_event(frame.as_bytes()),
            Some(FrameEvent::Audio(a)) if a == b64(&[1u8, 2, 3, 4])
        ));
        // alignment events carry no audio and are not terminal
        assert!(
            frame_event(b"data: {\"type\":\"speech.audio.alignment.partial\",\"words\":[]}")
                .is_none()
        );
        assert!(frame_event(b"data: [DONE]").is_none());
    }

    /// `long: false` stops mid-sentence and returns a valid short clip. The
    /// flag on `speech.audio.done` is the only thing separating that from a
    /// complete render, so it must survive the frame decode.
    #[test]
    fn done_frame_carries_the_truncation_flag() {
        assert!(matches!(
            frame_event(b"data: {\"type\":\"speech.audio.done\",\"truncated\":true}"),
            Some(FrameEvent::Done { truncated: true })
        ));
        assert!(matches!(
            frame_event(b"data: {\"type\":\"speech.audio.done\",\"truncated\":false}"),
            Some(FrameEvent::Done { truncated: false })
        ));
        // An older engine omits the field entirely; absence is "not truncated".
        assert!(matches!(
            frame_event(b"data: {\"type\":\"speech.audio.done\"}"),
            Some(FrameEvent::Done { truncated: false })
        ));
    }

    /// The whole point of the retry is that a rejected rate degrades to audio
    /// at 1x instead of no audio. It must NOT fire for a 400 that has nothing
    /// to do with speed, or the real error message gets replaced by a second
    /// identical failure.
    #[test]
    fn only_a_speed_carrying_400_is_retried() {
        assert!(retry_at_natural_speed(400, 1.5));
        assert!(retry_at_natural_speed(400, 0.75));
        // Nothing to drop — retrying would repeat the same failure.
        assert!(!retry_at_natural_speed(400, 1.0));
        // Not a rejected argument; a re-render would just burn GPU time.
        assert!(!retry_at_natural_speed(500, 1.5));
        assert!(!retry_at_natural_speed(503, 1.5));
        assert!(!retry_at_natural_speed(200, 1.5));
    }

    /// `speed` has to reach the wire when set and stay off it when not: an
    /// always-present `speed: 1.0` would be harmless today but turns into a
    /// rejected request the moment an engine tightens its validation.
    #[test]
    fn speed_is_only_serialised_when_asked_for() {
        let mut req = SpeechRequest {
            input: "hello".to_string(),
            response_format: "pcm".to_string(),
            ..Default::default()
        };
        let bare = serde_json::to_value(&req).unwrap();
        assert!(bare.get("speed").is_none());

        req.speed = Some(1.5);
        let with_speed = serde_json::to_value(&req).unwrap();
        assert_eq!(with_speed.get("speed").and_then(|v| v.as_f64()), Some(1.5));
    }

    #[test]
    fn wav_header_declares_24k_mono_s16le() {
        let w = wav_from_pcm(&[0u8; 96]);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 1, "mono");
        assert_eq!(
            u32::from_le_bytes([w[24], w[25], w[26], w[27]]),
            SAMPLE_RATE_HZ
        );
        assert_eq!(u16::from_le_bytes([w[34], w[35]]), 16, "s16");
        assert_eq!(w.len(), 44 + 96);
    }

    /// Older builds answer with only the map. It has no per-voice detail, so
    /// `default` is the one built-in we can still identify.
    #[test]
    fn falls_back_to_the_legacy_map_when_there_is_no_array() {
        let raw: Value = serde_json::from_str(r#"{"breeze-tts-2":["default","gaiman"]}"#).unwrap();
        let p = parse_voices_payload(&raw);
        assert_eq!(p.model_id.as_deref(), Some("breeze-tts-2"));
        assert_eq!(
            p.voices.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            ["default", "gaiman"]
        );
        assert!(p.voices[0].builtin);
        assert!(!p.voices[1].builtin);
    }
}
