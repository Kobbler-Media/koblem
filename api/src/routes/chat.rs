use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use koblem_client::types::{ChatRequest, ChatResponse, SamplingConfig};
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::AppState;
use crate::auth::require_auth;
use crate::prompts;
use crate::repositories::llm_logs::{self, CreateLog};
use crate::think::extract_reasoning_content;

/// Cap for raw chat requests. Large enough for multi-image vision tests
/// (base64-inflated PNGs) plus a few MB of base64-inflated audio, small enough
/// to reject runaway uploads.
const CHAT_BODY_LIMIT: usize = 40 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/chat", post(chat))
        .layer(DefaultBodyLimit::max(CHAT_BODY_LIMIT))
}

async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ChatRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let start = std::time::Instant::now();

    let mut messages = Vec::new();
    if let Some(ref system) = payload.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    // 🔴 The turn goes out either way. `warnings` carries what was lost back to the caller so the
    // chat box can say so on the message, instead of the user watching a model answer a question
    // about audio it was never sent.
    let (audio, audio_warning) = admit_audio(&state, &payload.audio);
    messages.push(build_user_message(&payload.prompt, &payload.images, audio));

    let mut body = json!({
        "model": "local",
        "messages": messages,
    });

    let baseline = SamplingConfig {
        enable_thinking: Some(false),
        ..SamplingConfig::default()
    };
    let db_default = crate::sampling::resolve_sampling(&state.db, "sampling_chat_default").await;
    let sampling = baseline.overlay(&db_default).overlay(&payload.sampling);
    sampling.apply_to(&mut body);
    prompts::apply_grammar_to_body(&state.db, &mut body, &sampling).await;

    // Build the log-safe view of the request messages before handing the real
    // body off to llama-server — base64 images would otherwise bloat llm_logs.
    let log_messages =
        sanitize_messages_for_log(body.get("messages").cloned().unwrap_or(json!([])));

    let (_permit, resp_body) = match state.llama.chat_completion_json(&body).await {
        Ok(r) => r,
        Err(crate::llama::Error::HeavyBusy) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "60")],
                "GPU busy with a heavy render — retry shortly",
            )
                .into_response();
        }
        Err(e) => {
            error!("llama-server error: {e}");
            return (StatusCode::BAD_GATEWAY, format!("llama-server error: {e}")).into_response();
        }
    };

    let duration_ms = start.elapsed().as_millis() as i64;

    let content = resp_body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    let prompt_tokens = resp_body
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let completion_tokens = resp_body
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);

    let finish_reason = resp_body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Store log asynchronously.
    let mut log = CreateLog::new(Some("raw_chat".to_string()), log_messages);
    log.set_sampling_from_body(
        &body,
        preset_tag(&sampling),
        grammar_tag(&sampling).as_deref(),
    );
    log.response_content = Some(content.clone());
    log.response_reasoning_content = extract_reasoning_content(&resp_body);
    log.finish_reason = finish_reason;
    log.duration_ms = Some(duration_ms);
    log.prompt_tokens = prompt_tokens;
    log.completion_tokens = completion_tokens;
    let db = state.db.clone();
    tokio::spawn(async move {
        if let Err(e) = llm_logs::create_log(&db, &log).await {
            warn!("failed to store chat log: {e}");
        }
    });

    Json(ChatResponse {
        response: content,
        prompt_tokens,
        completion_tokens,
        duration_ms,
        warnings: audio_warning.into_iter().collect(),
    })
    .into_response()
}

pub(crate) fn build_user_message(prompt: &str, images: &[String], audio: &[String]) -> Value {
    // Plain text stays a bare string so text-only chats keep the simplest
    // wire shape (and cache identically to how they always have).
    if images.is_empty() && audio.is_empty() {
        return json!({"role": "user", "content": prompt});
    }
    let mut parts = Vec::with_capacity(images.len() + audio.len() + 1);
    parts.push(json!({"type": "text", "text": prompt}));
    for url in images {
        parts.push(json!({
            "type": "image_url",
            "image_url": {"url": url},
        }));
    }
    for url in audio {
        parts.push(audio_content_part(url));
    }
    json!({"role": "user", "content": parts})
}

/// One OpenAI-style `input_audio` content part, from a data URL or bare base64.
///
/// 🔴 **THE TRAP, and the reason this is a function rather than a `json!` at each call site.**
/// Unlike `image_url` — which llama-server's mtmd accepts as a full `data:` URI — the `input_audio`
/// path is parsed with `accept_base64_uri = false`, so it must receive RAW base64. A `data:…` prefix
/// is not rejected; it is base64-decoded as garbage, and what comes back is a model confidently
/// describing a sound that was never sent.
///
/// Requires an audio-capable mmproj on the loaded model (Gemma 4's F16 mmproj handles audio +
/// vision: `clip.has_audio_encoder = true`, `clip.audio.projector_type = gemma4ua`).
///
/// `pub(crate)` because the Director's writer paths build their content array by hand — the image
/// order there is load-bearing against positional clauses in the user turn, so they cannot go
/// through [`build_user_message`] wholesale. They can and must share THIS.
///
/// ⚠️ Every caller must clear [`audio_input_supported`] first (or go through [`admit_audio`]).
/// Sending an `input_audio` part to a vision-only mmproj does not fail loudly; it is a silent
/// nothing, or worse, a model inventing a description of a sound it never received.
pub(crate) fn audio_content_part(url: &str) -> Value {
    let (format, data) = split_audio_data_url(url);
    json!({
        "type": "input_audio",
        "input_audio": {"data": data, "format": format},
    })
}

/// Can the LLM koblem is pointed at read an audio attachment right now?
///
/// One line on purpose: the derivation, and the reason it IS a derivation rather than a probe,
/// live on [`crate::llama::LlamaClient::audio_input_supported`]. Everything that stages, forwards
/// or advertises audio reads it through here or through `/health`'s `llama_audio_input`, which
/// publishes the same call — so the UI's greyed-out button and the server's drop decision can
/// never disagree.
pub(crate) fn audio_input_supported(state: &AppState) -> bool {
    state.llama.audio_input_supported()
}

/// The wording every path uses when it drops audio. Shared so the chat box, the agent transcript
/// and the logs all say the same sentence.
pub(crate) fn audio_dropped_warning(n: usize) -> String {
    format!(
        "{n} audio attachment{} dropped: the loaded model has no audio encoder (Beefy LLM mode \
         runs Qwen3.8, whose mmproj is vision-only). The message was sent without it; images are \
         unaffected. Turn Beefy off on the GPU Services page to get audio back.",
        if n == 1 { "" } else { "s" }
    )
}

/// Decide what audio actually goes on the wire.
///
/// 🔴 **DROP AND NOTIFY, never refuse.** Returns the parts to send plus a caller-facing warning
/// when it dropped some. Failing the request instead would mean a model swap silently converts
/// every audio-carrying turn into an error, and the doctrine here is already settled:
/// `agent::run::inline_attachments` drops an attachment whose bytes are gone because "losing the
/// picture is better than losing the turn". A silently-ignored `input_audio` part would be worse
/// than either — the model answers about a sound it never heard.
///
/// Borrows on the happy path rather than cloning: these are base64 data URLs and a single chat turn
/// is capped at 25 MiB of them.
pub(crate) fn admit_audio<'a>(
    state: &AppState,
    audio: &'a [String],
) -> (&'a [String], Option<String>) {
    if audio.is_empty() || audio_input_supported(state) {
        return (audio, None);
    }
    warn!(
        dropped = audio.len(),
        llm = %state.llama.base_url(),
        "chat: the loaded LLM has no audio encoder — dropping audio attachments and answering anyway"
    );
    (&[], Some(audio_dropped_warning(audio.len())))
}

/// Split a client-supplied audio attachment into `(format, raw_base64)` for the
/// OpenAI `input_audio` part. Accepts either a `data:audio/<fmt>;base64,<b64>`
/// URL (what the web UI + CLI send) or bare base64. The `format` tag is derived
/// from the MIME when present (`wav` fallback) — llama-server sniffs the real
/// container anyway, so it's cosmetic; the raw base64 payload is what matters.
fn split_audio_data_url(url: &str) -> (&'static str, &str) {
    if let Some(rest) = url.strip_prefix("data:") {
        let mime = rest.split(';').next().unwrap_or("");
        let format = match mime {
            "audio/mpeg" | "audio/mp3" => "mp3",
            "audio/ogg" => "ogg",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/mp4" | "audio/aac" | "audio/x-m4a" => "m4a",
            "audio/webm" => "webm",
            _ => "wav",
        };
        // Everything after the first comma is the base64 payload.
        let data = url.split_once(',').map(|(_, b)| b).unwrap_or("");
        (format, data)
    } else {
        // Already raw base64 — pass through unchanged.
        ("wav", url)
    }
}

/// Replace base64 media payloads with a short placeholder so llm_logs stays
/// compact. The OpenAI vision format puts the base64 in
/// `content[].image_url.url`; audio in `content[].input_audio.data`. We rewrite
/// only those fields and preserve the rest.
pub(crate) fn sanitize_messages_for_log(mut messages: Value) -> Value {
    let Some(array) = messages.as_array_mut() else {
        return messages;
    };
    for msg in array {
        let Some(parts) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts {
            if let Some(url_field) = part.get_mut("image_url").and_then(|u| u.get_mut("url"))
                && let Some(url) = url_field.as_str()
            {
                *url_field = Value::String(media_placeholder("image", url));
            }
            if let Some(data_field) = part.get_mut("input_audio").and_then(|a| a.get_mut("data"))
                && let Some(data) = data_field.as_str()
            {
                *data_field = Value::String(media_placeholder("audio", data));
            }
        }
    }
    messages
}

/// Serialise the `SamplingConfig`'s preset (if any) to the snake_case tag
/// used in the `llm_logs.sampling_preset` column. Returns `None` when no
/// preset is set — the resolved numeric values in the other columns still
/// reflect the effective sampling.
pub(crate) fn preset_tag(cfg: &SamplingConfig) -> Option<&'static str> {
    use koblem_client::types::SamplingPreset;
    cfg.preset.map(|p| match p {
        SamplingPreset::Deterministic => "deterministic",
        SamplingPreset::QwenInstructGeneral => "qwen_instruct_general",
        SamplingPreset::QwenInstructReasoning => "qwen_instruct_reasoning",
    })
}

/// Resolve the effective grammar mode tag for `llm_logs.request_grammar`.
/// Returns the wire string (`"off"`, `"review"`, or a user-defined name)
/// when a mode is set, `None` when the caller didn't pick one.
pub(crate) fn grammar_tag(cfg: &SamplingConfig) -> Option<String> {
    cfg.resolve()
        .grammar
        .as_ref()
        .map(|g| g.as_wire().to_string())
}

fn media_placeholder(kind: &str, url: &str) -> String {
    // Count only the base64 payload so the number reflects media size, not
    // the data-URL prefix overhead.
    let payload_len = url
        .split_once(',')
        .map(|(_, b)| b.len())
        .unwrap_or(url.len());
    format!("[{kind} omitted: {payload_len} b64 bytes]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_only_message_stays_a_plain_string() {
        let msg = build_user_message("hello", &[], &[]);
        assert_eq!(msg["content"], json!("hello"));
    }

    #[test]
    fn audio_is_sent_as_raw_base64_not_a_data_url() {
        // llama-server's input_audio path decodes the field as raw base64, so
        // the `data:` wrapper MUST be stripped — otherwise it decodes garbage.
        let audio = vec!["data:audio/wav;base64,QUJD".to_string()];
        let msg = build_user_message("what is this sound?", &[], &audio);
        let parts = msg["content"].as_array().expect("content is parts array");
        assert_eq!(parts[0]["type"], "text");
        let ap = &parts[1];
        assert_eq!(ap["type"], "input_audio");
        assert_eq!(ap["input_audio"]["data"], "QUJD");
        assert_eq!(ap["input_audio"]["format"], "wav");
    }

    #[test]
    fn audio_format_is_read_from_the_mime() {
        let audio = vec!["data:audio/mpeg;base64,QUJD".to_string()];
        let msg = build_user_message("", &[], &audio);
        assert_eq!(msg["content"][1]["input_audio"]["format"], "mp3");
    }

    #[test]
    fn bare_base64_audio_passes_through() {
        let (format, data) = split_audio_data_url("QUJD");
        assert_eq!(format, "wav");
        assert_eq!(data, "QUJD");
    }

    /// 🔴 The shared part builder the Director's writer paths use. Same rule, one definition: a
    /// `data:` prefix that survives here is decoded as garbage by llama-server and never errors.
    #[test]
    fn the_shared_audio_part_strips_the_data_url_wrapper() {
        let part = audio_content_part("data:audio/wav;base64,QUJD");
        assert_eq!(part["type"], "input_audio");
        assert_eq!(part["input_audio"]["data"], "QUJD");
        assert!(
            !part["input_audio"]["data"]
                .as_str()
                .expect("data is a string")
                .starts_with("data:"),
            "{part}"
        );
        // Bare base64 is already correct and must pass through untouched.
        assert_eq!(audio_content_part("QUJD")["input_audio"]["data"], "QUJD");
    }

    #[test]
    fn images_still_send_the_full_data_url() {
        // The image_url path DOES accept a data: URI, so it's preserved verbatim.
        let images = vec!["data:image/png;base64,iVBORw0KGgo=".to_string()];
        let msg = build_user_message("look", &images, &[]);
        assert_eq!(
            msg["content"][1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn log_sanitizer_strips_both_image_and_audio_payloads() {
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "input_audio", "input_audio": {"data": "QUJDRA", "format": "wav"}},
            ]
        }]);
        let sanitized = sanitize_messages_for_log(messages);
        let parts = &sanitized[0]["content"];
        assert_eq!(parts[1]["image_url"]["url"], "[image omitted: 4 b64 bytes]");
        assert_eq!(
            parts[2]["input_audio"]["data"],
            "[audio omitted: 6 b64 bytes]"
        );
    }
}
