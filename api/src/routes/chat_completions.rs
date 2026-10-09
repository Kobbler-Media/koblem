//! Transparent proxy for `/v1/chat/completions` with MeiliSearch tool rewriting.
//!
//! Exposed at two paths:
//! - `/v1/chat/completions` — unauthenticated, for in-mesh callers (kobbler's
//!   meilisearch tool path hits this directly via the docker network).
//! - `/api/v1/chat/completions` — auth-required, externally reachable through
//!   nginx for debugging, playground tooling, and external OpenAI clients.

use crate::llama::LlmLease;
use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use futures::StreamExt;
use koblem_client::types::SamplingConfig;
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;
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
    Json(mut body): Json<Value>,
) -> impl IntoResponse {
    // No heavy-slot pre-check any more — the LLM is a normal budget-scheduled
    // service. It co-resides with renders on separate cards, or blocks briefly on
    // its own card's budget; it only 503s if the LLM service is administratively
    // disabled (handled on the `HeavyBusy` error below).
    let start = std::time::Instant::now();
    let caller = identify_caller(&headers);

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
    let db_default =
        crate::sampling::resolve_sampling(&state.db, "sampling_completions_default").await;
    let sampling = db_default.overlay(&request_override);
    // Strip the `sampling_preset` tag from the outgoing body — llama-server
    // doesn't know that key. The resolved numeric fields are re-applied below.
    if let Some(obj) = body.as_object_mut() {
        obj.remove("sampling_preset");
    }
    sampling.apply_to(&mut body);
    prompts::apply_grammar_to_body(&state.db, &mut body, &sampling).await;

    if has_tools {
        tool_rewrite::rewrite_request_tools(&mut body);
    }

    let (permit, resp) = match state.llama.chat_completion(&body).await {
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
        handle_streaming(resp, permit, has_tools, log_base, start, state.db.clone())
            .await
            .into_response()
    } else {
        handle_non_streaming(resp, permit, has_tools, log_base, start, state.db.clone())
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
    has_tools: bool,
    log: CreateLog,
    start: std::time::Instant,
    db: sqlx::PgPool,
) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(64);

    tokio::spawn(async move {
        let _permit = permit; // held until this task completes

        let mut byte_stream = resp.bytes_stream();
        let mut sse_buffer = String::new();

        // When tools are present, we buffer all chunks to detect and rewrite
        // tool call arguments. Content-only responses stream through immediately.
        let mut buffered_tool_chunks: Vec<Value> = Vec::new();
        let mut saw_tool_call = false;

        // Accumulate the full response content for logging.
        let mut accumulated_content = String::new();
        // Same for chain-of-thought when thinking is enabled — llama-server
        // emits delta.reasoning_content fragments alongside delta.content.
        let mut accumulated_reasoning = String::new();
        let mut finish_reason: Option<String> = None;

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk_bytes = match chunk_result {
                Ok(b) => b,
                Err(e) => {
                    error!("error reading llama-server stream: {e}");
                    break;
                }
            };

            sse_buffer.push_str(&String::from_utf8_lossy(&chunk_bytes));

            while let Some(line_end) = sse_buffer.find('\n') {
                let line = sse_buffer[..line_end].trim().to_string();
                sse_buffer = sse_buffer[line_end + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                // Pass through [DONE] sentinel
                if line == "data: [DONE]" {
                    // If we buffered tool call chunks, rewrite and emit them now
                    if saw_tool_call {
                        emit_buffered_tool_calls(&tx, &mut buffered_tool_chunks).await;
                    }
                    let _ = tx.send(Ok("data: [DONE]\n\n".to_string())).await;

                    // Fire-and-forget log write.
                    let mut log = log;
                    log.duration_ms = Some(start.elapsed().as_millis() as i64);
                    if !accumulated_content.is_empty() {
                        log.response_content = Some(accumulated_content);
                    }
                    if !accumulated_reasoning.is_empty() {
                        log.response_reasoning_content = Some(accumulated_reasoning);
                    }
                    log.finish_reason = finish_reason;
                    let db = db.clone();
                    tokio::spawn(async move {
                        if let Err(e) = llm_logs::create_log(&db, &log).await {
                            error!("failed to store llm log: {e}");
                        }
                    });
                    return;
                }

                let json_str = line.strip_prefix("data: ").unwrap_or(&line);

                let chunk: Value = match serde_json::from_str(json_str) {
                    Ok(c) => c,
                    Err(_) => {
                        // Unknown line format — pass through as-is
                        let _ = tx.send(Ok(format!("{line}\n\n"))).await;
                        continue;
                    }
                };

                // Extract content and finish_reason from every chunk for logging.
                if let Some(choices) = chunk.get("choices").and_then(|v| v.as_array()) {
                    for choice in choices {
                        if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                            finish_reason = Some(fr.to_string());
                        }
                        if let Some(delta) = choice.get("delta") {
                            if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                                accumulated_content.push_str(content);
                            }
                            if let Some(reasoning) =
                                delta.get("reasoning_content").and_then(|v| v.as_str())
                            {
                                accumulated_reasoning.push_str(reasoning);
                            }
                        }
                    }
                }

                if !has_tools {
                    // No tools — pure passthrough
                    let serialized = serde_json::to_string(&chunk).unwrap_or_else(|e| {
                        error!("chunk serialize error: {e}");
                        "{}".into()
                    });
                    let _ = tx.send(Ok(format!("data: {serialized}\n\n"))).await;
                    continue;
                }

                // Tools present — check if this chunk has tool_call deltas
                let has_tool_delta =
                    chunk
                        .get("choices")
                        .and_then(|c| c.as_array())
                        .is_some_and(|choices| {
                            choices
                                .iter()
                                .any(|c| c.get("delta").and_then(|d| d.get("tool_calls")).is_some())
                        });

                if has_tool_delta {
                    saw_tool_call = true;
                    buffered_tool_chunks.push(chunk);
                } else if saw_tool_call {
                    // We're in tool-call mode but this chunk doesn't have tool deltas
                    // (e.g. finish_reason chunk) — buffer it too
                    buffered_tool_chunks.push(chunk);
                } else {
                    // Content delta — stream through immediately
                    let serialized = serde_json::to_string(&chunk).unwrap_or_else(|e| {
                        error!("chunk serialize error: {e}");
                        "{}".into()
                    });
                    let _ = tx.send(Ok(format!("data: {serialized}\n\n"))).await;
                }
            }
        }

        // Stream ended without [DONE] — flush any buffered chunks
        if saw_tool_call {
            emit_buffered_tool_calls(&tx, &mut buffered_tool_chunks).await;
        }
        let _ = tx.send(Ok("data: [DONE]\n\n".to_string())).await;

        // Log even when stream ends without [DONE].
        let mut log = log;
        log.duration_ms = Some(start.elapsed().as_millis() as i64);
        if !accumulated_content.is_empty() {
            log.response_content = Some(accumulated_content);
        }
        if !accumulated_reasoning.is_empty() {
            log.response_reasoning_content = Some(accumulated_reasoning);
        }
        log.finish_reason = finish_reason;
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = llm_logs::create_log(&db, &log).await {
                error!("failed to store llm log: {e}");
            }
        });
    });

    let stream = ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());

    (headers, body)
}

/// Accumulate tool call argument fragments from buffered SSE chunks,
/// rewrite the complete arguments, and emit as SSE events.
async fn emit_buffered_tool_calls(
    tx: &tokio::sync::mpsc::Sender<Result<String, std::io::Error>>,
    chunks: &mut [Value],
) {
    // Accumulate complete tool call arguments from deltas.
    // Each delta may have partial argument strings that need concatenation.
    let mut accumulated_args: std::collections::HashMap<usize, String> = Default::default();
    let mut accumulated_names: std::collections::HashMap<usize, String> = Default::default();
    let mut accumulated_ids: std::collections::HashMap<usize, String> = Default::default();

    // Track which chunk has the finish_reason so we can emit it with rewritten args
    let mut finish_chunk: Option<Value> = None;

    for chunk in chunks.iter() {
        let Some(choices) = chunk.get("choices").and_then(|v| v.as_array()) else {
            continue;
        };
        for choice in choices {
            // Check for finish_reason
            if choice
                .get("finish_reason")
                .and_then(|f| f.as_str())
                .is_some()
            {
                finish_chunk = Some(chunk.clone());
            }

            let Some(delta) = choice.get("delta") else {
                continue;
            };
            let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
                continue;
            };
            for tc in tool_calls {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                // Preserve the original tool call ID from the first delta
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    accumulated_ids.entry(idx).or_insert_with(|| id.to_string());
                }
                if let Some(func) = tc.get("function") {
                    if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                        accumulated_names.insert(idx, name.to_string());
                    }
                    if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                        accumulated_args.entry(idx).or_default().push_str(args);
                    }
                }
            }
        }
    }

    // Build a single chunk with complete, rewritten tool calls
    if let Some(mut chunk) = finish_chunk {
        // Build rewritten tool_calls array
        let mut tool_calls = Vec::new();
        let mut indices: Vec<usize> = accumulated_names.keys().copied().collect();
        indices.sort();

        for idx in indices {
            let name = accumulated_names
                .get(&idx)
                .map(|s| s.as_str())
                .unwrap_or("");
            let id = accumulated_ids
                .get(&idx)
                .map(|s| s.as_str())
                .unwrap_or("call_0");
            let raw_args = accumulated_args
                .get(&idx)
                .map(|s| s.as_str())
                .unwrap_or("{}");
            let rewritten_args = tool_rewrite::rewrite_tool_call_args(name, raw_args);

            tool_calls.push(serde_json::json!({
                "index": idx,
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": rewritten_args,
                }
            }));
        }

        // Replace the delta in the finish chunk
        if let Some(choices) = chunk.get_mut("choices").and_then(|v| v.as_array_mut())
            && let Some(choice) = choices.first_mut()
            && let Some(obj) = choice.as_object_mut()
        {
            obj.insert(
                "delta".into(),
                serde_json::json!({
                    "role": "assistant",
                    "tool_calls": tool_calls,
                }),
            );
        }

        let serialized = serde_json::to_string(&chunk).unwrap_or_else(|e| {
            error!("chunk serialize error: {e}");
            "{}".into()
        });
        let _ = tx.send(Ok(format!("data: {serialized}\n\n"))).await;
    }
}
