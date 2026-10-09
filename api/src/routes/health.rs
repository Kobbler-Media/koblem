//! Aggregated health probe.
//!
//! Checks llama + vision + TTS + STT in parallel. llama probe keeps
//! the original `llama_server` bool so existing callers (kobbler) don't
//! break; the Python-service booleans are additive, as are the `*_model`
//! strings so the frontend can render the actually-loaded model name without
//! hardcoding it.
//!
//! The llama probe (`/health` + `/v1/models`) is safe to call even while an
//! Avatar/Flux render holds the GPU and the keep-warm LLM is unloaded: the
//! worker-isolation parent serves both endpoints itself and `/v1/models` no
//! longer lazily respawns the child (fixed in the hbd-llama-cpp-turboquant
//! fork — `proxy_noflight` does not spawn). So a poll can't re-fork the worker
//! mid-render.

use axum::{Json, extract::State};
use koblem_client::types::HealthResponse;

use crate::AppState;

pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let llama_fut = state.llama.health();
    let llama_model_fut = state.llama.model_name();
    let vision_fut = state.vision.health_info();
    let tts_fut = state.tts.health_info();
    let stt_fut = state.stt.health_info();

    let (llama, llama_model, vision, tts, stt) =
        tokio::join!(llama_fut, llama_model_fut, vision_fut, tts_fut, stt_fut);

    let llama_ok = llama.unwrap_or(false);
    // "degraded" = llama is down (that's the core service). Other Python
    // services being down just means that particular endpoint 503s — not a
    // whole-koblem outage.
    let status = if llama_ok { "ok" } else { "degraded" };

    let vision = vision.unwrap_or_default();
    let tts = tts.unwrap_or_default();
    let stt = stt.unwrap_or_default();

    Json(HealthResponse {
        status: status.to_string(),
        llama_server: llama_ok,
        vision: vision.healthy,
        tts: tts.healthy,
        stt: stt.healthy,
        llama_model,
        vision_model: vision.model,
        tts_model: tts.model,
        stt_model: stt.model,
        // 🔴 Not a probe and not a `tokio::join!` arm — nothing is asked over the
        // network, because nothing on llama-server ANSWERS this. `/v1/models`
        // reports an id and no mmproj capabilities at all. It is derived from
        // which server koblem is pointed at; see
        // `LlamaClient::audio_input_supported`. Published here so the chat and
        // agent composers can grey out the audio attach button BEFORE a user
        // stages 20 MB of wav that the server would then drop.
        llama_audio_input: state.llama.audio_input_supported(),
    })
}
