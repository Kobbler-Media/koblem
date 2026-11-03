//! Transparent proxy for `/v1/chat/completions` with MeiliSearch tool rewriting.
//!
//! Exposed at two paths:
//! - `/v1/chat/completions` — unauthenticated, for in-mesh callers (kobbler's
//!   meilisearch tool path hits this directly via the docker network).
//! - `/api/v1/chat/completions` — auth-required, externally reachable through
//!   nginx for debugging, playground tooling, and external OpenAI clients.

use crate::llama::{LlamaClient, LlmLease};
use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use koblem_client::types::SamplingConfig;
use serde_json::Value;
use tracing::{debug, error};

use crate::AppState;
use crate::auth::require_auth;
use crate::prompts;
use crate::repositories::llm_logs::{self, CreateLog};
use crate::routes::chat::{grammar_tag, preset_tag};
use crate::think::{extract_reasoning_content, strip_think_in_response};
use crate::tool_rewrite;

/// Authenticated wrapper around [`chat_completions`] for the `/api/v1/...`
/// path. Same body, same response — just gated by an API token check.
pub async fn chat_completions_authed(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<Value>,
) -> axum::response::Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    chat_completions(State(state), headers, body)
        .await
        .into_response()
}

/// Identify the caller from request headers.
fn identify_caller(headers: &HeaderMap) -> Option<String> {
    if let Some(caller) = headers.get("x-caller").and_then(|v| v.to_str().ok())
        && !caller.is_empty()
    {
        return Some(caller.to_string());
    }
    if let Some(ua) = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        && !ua.is_empty()
    {
        return Some(ua.to_string());
    }
    None
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    proxy(&state.llama, &state.db, &headers, body).await
}

async fn proxy(
    llama: &LlamaClient,
    db: &sqlx::PgPool,
    headers: &HeaderMap,
    mut body: Value,
) -> axum::response::Response {
    // No heavy-slot pre-check any more — the LLM is a normal budget-scheduled
    // service. It co-resides with renders on separate cards, or blocks briefly on
    // its own card's budget; it only 503s if the LLM service is administratively
    // disabled (handled on the `HeavyBusy` error below).
    let start = std::time::Instant::now();
    let caller = identify_caller(headers);

    let is_streaming = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_tools = body
        .get("tools")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());

    debug!(
        is_streaming,
        has_tools,
        num_messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        "chat completion request"
    );

    // Capture request messages + tools before tool rewrite mutates the body.
    let request_messages = body
        .get("messages")
        .cloned()
        .unwrap_or(Value::Array(vec![]));
    let request_tools = body.get("tools").cloned();

    // Resolve sampling: parse whatever top-level keys the caller sent
    // (temperature/top_p/…/sampling_preset), merge with the DB default
    // (sampling_completions_default), apply back onto the body.
    // `apply_to` only writes fields it can resolve, so keys the caller
    // didn't set and that have no DB default stay absent.
    let mut request_override: SamplingConfig =
        serde_json::from_value(body.clone()).unwrap_or_default();
    // OpenAI-compatible callers convey thinking via the nested
    // `chat_template_kwargs.enable_thinking` (not as a flat top-level key),
    // so promote it into the SamplingConfig for the layered overlay to see.
    if request_override.enable_thinking.is_none() {
        request_override.enable_thinking = body
            .get("chat_template_kwargs")
            .and_then(|k| k.get("enable_thinking"))
            .and_then(Value::as_bool);
    }
    let db_default = crate::sampling::resolve_sampling(db, "sampling_completions_default").await;
    let sampling = db_default.overlay(&request_override);
    // Strip the `sampling_preset` tag from the outgoing body — llama-server
    // doesn't know that key. The resolved numeric fields are re-applied below.
    if let Some(obj) = body.as_object_mut() {
        obj.remove("sampling_preset");
    }
    sampling.apply_to(&mut body);
    prompts::apply_grammar_to_body(db, &mut body, &sampling).await;

    if has_tools {
        tool_rewrite::rewrite_request_tools(&mut body);
    }

    let (permit, resp) = match llama.chat_completion(&body).await {
        Ok(r) => r,
        // The LLM service is administratively disabled — 503 + Retry-After.
        Err(crate::llama::Error::HeavyBusy) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "60")],
                Json(serde_json::json!({
                    "error": "LLM service is disabled — retry shortly",
                    "retry_after_seconds": 60,
                })),
            )
                .into_response();
        }
        Err(e) => {
            error!("llama-server error: {e}");
            return (StatusCode::BAD_GATEWAY, format!("llama-server error: {e}")).into_response();
        }
    };

    let mut log_base = CreateLog::new(caller, request_messages);
    log_base.request_tools = request_tools;
    log_base.streaming = is_streaming;
    log_base.tool_rewrite_applied = has_tools;
    log_base.set_sampling_from_body(
        &body,
        preset_tag(&sampling),
        grammar_tag(&sampling).as_deref(),
    );

    if is_streaming {
        handle_streaming(resp, permit, log_base, start, db.clone())
            .await
            .into_response()
    } else {
        handle_non_streaming(resp, permit, has_tools, log_base, start, db.clone())
            .await
            .into_response()
    }
}

async fn handle_non_streaming(
    resp: reqwest::Response,
    _permit: LlmLease,
    has_tools: bool,
    mut log: CreateLog,
    start: std::time::Instant,
    db: sqlx::PgPool,
) -> impl IntoResponse {
    match resp.json::<Value>().await {
        Ok(mut body) => {
            strip_think_in_response(&mut body);
            if has_tools {
                tool_rewrite::rewrite_response_tool_calls(&mut body);
            }

            // Extract logging fields from the response.
            log.duration_ms = Some(start.elapsed().as_millis() as i64);
            if let Some(usage) = body.get("usage") {
                log.prompt_tokens = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);
                log.completion_tokens = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);
            }
            if let Some(choices) = body.get("choices").and_then(|v| v.as_array())
                && let Some(choice) = choices.first()
            {
                log.finish_reason = choice
                    .get("finish_reason")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some(msg) = choice.get("message") {
                    log.response_content = msg
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    log.response_tool_calls = msg.get("tool_calls").cloned();
                }
            }
            log.response_reasoning_content = extract_reasoning_content(&body);

            tokio::spawn(async move {
                if let Err(e) = llm_logs::create_log(&db, &log).await {
                    error!("failed to store llm log: {e}");
                }
            });

            Json(body).into_response()
        }
        Err(e) => {
            error!("failed to read llama-server response: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("failed to read response: {e}"),
            )
                .into_response()
        }
    }
}

async fn handle_streaming(
    resp: reqwest::Response,
    permit: LlmLease,
    mut log: CreateLog,
    start: std::time::Instant,
    db: sqlx::PgPool,
) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(64);

    tokio::spawn(async move {
        let summary = stream::relay(resp.bytes_stream(), &tx).await;
        drop(permit);
        if let Some(e) = &summary.error {
            error!("chat completion stream failed: {e}");
        }

        log.duration_ms = Some(start.elapsed().as_millis() as i64);
        log.response_content = Some(summary.content).filter(|s| !s.is_empty());
        log.response_reasoning_content = Some(summary.reasoning).filter(|s| !s.is_empty());
        log.finish_reason = match summary.error {
            Some(_) => Some("error".to_string()),
            None => summary.finish_reason,
        };
        if let Err(e) = llm_logs::create_log(&db, &log).await {
            error!("failed to store llm log: {e}");
        }
    });

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());

    (headers, Body::from_stream(stream::flush_before_abort(rx)))
}

mod stream;
#[cfg(test)]
mod tests;
