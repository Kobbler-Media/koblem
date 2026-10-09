//! `GET /v1/models` — the OpenAI-compatible model list.
//!
//! Reports what koblem is ACTUALLY talking to, by asking llama-server. It used to
//! answer a hardcoded `"qwen-3.5-vl-9b"` with `context_length` from the
//! `CONTEXT_SIZE` env, both of which drifted: the deployed model has been
//! gemma-4-12b for months, and Beefy mode swaps it for Qwen3.8-27B at a different
//! context length. A model id that was true two models ago is worse than no id at
//! all, because callers route on it.
//!
//! 🔴 Asking is cheap and safe: `/v1/models` on the worker-isolation parent is
//! answered WITHOUT spawning the child (`proxy_noflight`), so this never drags
//! weights into VRAM. The flip side is that it returns nothing while the child is
//! unloaded — hence the cache below, so a metadata poll during a Beefy switch
//! reports the last known identity instead of blinking to a placeholder.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{Json, extract::State};
use serde::Serialize;

use crate::AppState;

#[derive(Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<Model>,
}

#[derive(Serialize)]
pub struct Model {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: &'static str,
    pub context_length: u32,
}

/// Last identity llama-server reported, and when. Short TTL: the id only changes
/// when an operator swaps models, but the cache also has to cover the seconds
/// during a swap when the child is down and `/v1/models` answers nothing.
static CACHE: OnceLock<Mutex<Option<(String, Instant)>>> = OnceLock::new();
const TTL: Duration = Duration::from_secs(60);

fn cached() -> Option<String> {
    let m = CACHE.get_or_init(|| Mutex::new(None)).lock().ok()?;
    m.as_ref()
        .filter(|(_, at)| at.elapsed() < TTL)
        .map(|(id, _)| id.clone())
}

/// Remember an id, and keep remembering it past the TTL — a stale-but-real name
/// beats a placeholder while the model is mid-swap.
fn remember(id: &str) {
    if let Some(c) = CACHE.get_or_init(|| Mutex::new(None)).lock().ok().as_mut() {
        **c = Some((id.to_string(), Instant::now()));
    }
}

fn last_known() -> Option<String> {
    let m = CACHE.get_or_init(|| Mutex::new(None)).lock().ok()?;
    m.as_ref().map(|(id, _)| id.clone())
}

pub async fn list_models(State(state): State<AppState>) -> Json<ModelsResponse> {
    let id = match cached() {
        Some(id) => id,
        None => match state.llama.model_name().await {
            Some(name) => {
                // llama-server reports the full GGUF path; the leaf is the useful part.
                let leaf = name
                    .rsplit('/')
                    .next()
                    .unwrap_or(&name)
                    .trim_end_matches(".gguf")
                    .to_string();
                remember(&leaf);
                leaf
            }
            // Unreachable or child unloaded: last known, else the OpenAI-ish
            // placeholder every caller of ours already sends.
            None => last_known().unwrap_or_else(|| "local".to_string()),
        },
    };
    // 🔴 Beefy runs a different context length (131072) from gemma (73728), and
    // `CONTEXT_SIZE` describes neither once the model is swapped. Prefer what the
    // client is configured against; fall back to the env only when unknown.
    let context_length = state
        .llama
        .context_length()
        .await
        .unwrap_or(state.config.context_size);
    Json(ModelsResponse {
        object: "list",
        data: vec![Model {
            id,
            object: "model",
            created: 1744300000,
            owned_by: "local",
            context_length,
        }],
    })
}
