use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures::StreamExt;
use kob_gpu_gate::{GpuLlmGuard, GpuLock, HeavyKind, HeavyService};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::repositories::llm_logs;
use crate::think::strip_think_in_response;

/// Client for llama-server's OpenAI-compatible `/v1/chat/completions` endpoint.
///
/// Includes a concurrency gate (semaphore with 1 permit) since llama-server
/// processes one request at a time. The caller holds the permit until the
/// response body is fully consumed.
#[derive(Clone)]
pub struct LlamaClient {
    http: reqwest::Client,
    /// Interior-mutable so the LLM URL can be hot-reloaded from the settings
    /// page (matching the other GPU clients) without a koblem restart.
    base_url: Arc<RwLock<String>>,
    gate: Arc<Semaphore>,
    gpu: GpuLock,
    /// 🔴 True when the llama-server behind `base_url` spans BOTH cards
    /// (Beefy mode: Qwen3.8-27B, `-sm tensor -ts 37,63`). Suppresses the
    /// `X-Worker-Gpu` header.
    ///
    /// That header reaches `worker_isolation::resolve_target_gpu`, which feeds
    /// `ensure_loaded(target)` and re-execs the worker child with a SINGLE-card
    /// `CUDA_VISIBLE_DEVICES`. 16.7 GiB of weights do not fit on one card, and
    /// `llama_params_fit` is unimplemented for SPLIT_MODE_TENSOR (it aborts
    /// rather than shrinking), so the child fails its health poll and every
    /// request 503s. Owned by `beefy.rs`; moves with the URL.
    multi_gpu: Arc<AtomicBool>,
}

/// Held for the full lifetime of a llama-server request (and its streamed body).
/// Bundles the local concurrency permit (llama-server is single-slot) with the
/// gate's LLM budget guard. The guard keeps the LLM's budget reservation live for
/// the whole request; an evicting service coordinates with the in-flight request
/// via llama-server's own `POST /v1/admin/drain` (finish in-flight before unload),
/// not via any koblem-side in-flight counter.
pub struct LlmLease {
    _permit: OwnedSemaphorePermit,
    _gpu: GpuLlmGuard,
}

impl LlamaClient {
    pub fn new(base_url: String, gpu: GpuLock) -> Self {
        // No request timeout: llama-server runs under a semaphore(1) so
        // requests serialize, and a long-running review of 500+ candidates
        // can legitimately take many minutes. Preference is a slow response
        // over no response after waiting. If llama-server itself hangs,
        // other layers (Docker healthcheck, the semaphore's OwnedPermit
        // being held by the task tree) will still surface the failure.
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            base_url: Arc::new(RwLock::new(base_url)),
            gate: Arc::new(Semaphore::new(1)),
            gpu,
            multi_gpu: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Current llama-server base URL.
    fn base(&self) -> String {
        self.base_url
            .read()
            .expect("llama base_url poisoned")
            .clone()
    }

    /// Hot-reload the llama-server URL from a settings edit. Also re-registers
    /// the gate's always-resident LLM heavy service so the drain/unload dance
    /// targets the new host. An empty/blank value is ignored — the LLM is
    /// foundational infra and must always have a URL (unlike the optional
    /// engines, clearing it is never meaningful).
    pub fn set_base_url(&self, base_url: Option<String>) {
        let Some(url) = base_url
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        self.gpu.set_llm_service(HeavyService {
            kind: HeavyKind::Llm,
            url: url.clone(),
            unload_path: "/v1/admin/unload".to_string(),
            drain_path: "/v1/admin/drain".to_string(),
        });
        *self.base_url.write().expect("llama base_url poisoned") = url;
    }

    /// Current llama-server base URL. Public so `beefy::status` can report what
    /// this process is ACTUALLY talking to, rather than what the setting says —
    /// the two disagree if someone hand-edits the URL while Beefy mode is on.
    pub fn base_url(&self) -> String {
        self.base()
    }

    /// Declare whether the server behind the current URL spans both cards.
    ///
    /// 🔴 Ordering matters: set this BEFORE `set_base_url` when entering Beefy
    /// and AFTER it when leaving, so no in-flight request can ever observe the
    /// beefy URL with the flag off (which would pin a two-card model to one card).
    pub fn set_multi_gpu(&self, multi: bool) {
        self.multi_gpu.store(multi, Ordering::Relaxed);
    }

    pub fn is_multi_gpu(&self) -> bool {
        self.multi_gpu.load(Ordering::Relaxed)
    }

    /// Does the model behind the current URL accept `input_audio` content parts?
    ///
    /// 🔴 **DERIVED, NOT PROBED, and it cannot be probed.** `/v1/models` reports
    /// an id and nothing else — no mmproj, no encoder list — so there is no
    /// endpoint that answers this question. The only fact available is WHICH
    /// server we are pointed at, and that is exactly what [`Self::multi_gpu`]
    /// tracks: it is set by `beefy::enter`/`exit`/`apply_at_boot` and moves with
    /// the URL, so "both cards" and "the beefy server" are the same bit today.
    ///
    /// - normal (gemma-4-12b): unified vision **+ audio** mmproj → `true`
    /// - Beefy (Qwen3.8-27B): `clip.has_audio_encoder = false`, VISION ONLY →
    ///   `false`. Images still work.
    ///
    /// 🔴 If a third llama-server ever joins that is multi-card AND audio-capable
    /// (or single-card and not), this stops being derivable from `multi_gpu` and
    /// needs its own atomic set alongside it. Change it HERE — `routes::chat`
    /// and `routes::health` both read this one method.
    pub fn audio_input_supported(&self) -> bool {
        !self.is_multi_gpu()
    }

    /// Forward a chat completion request to llama-server.
    ///
    /// Single chokepoint for every LLM path (json + streaming both funnel here).
    /// Acquires the gate's LLM budget guard FIRST: it returns `None` only when the
    /// LLM is administratively disabled, in which case we bounce with
    /// [`Error::HeavyBusy`] (a legacy variant name — it now means "LLM disabled")
    /// before touching the network. Then takes the local concurrency permit
    /// (llama-server is single-slot) and returns both bundled in the [`LlmLease`]
    /// so the caller holds them until the body is consumed. A competing service
    /// that wants the card drains this request via llama-server's `/v1/admin/drain`
    /// before unloading, so the guard never has to preempt an in-flight call.
    pub async fn chat_completion(
        &self,
        body: &Value,
    ) -> Result<(LlmLease, reqwest::Response), Error> {
        // Acquire a card under the budget scheduler. `None` only when the LLM is
        // administratively disabled — bounce before touching the network.
        let gpu_guard = self.gpu.acquire_for_llm().await.ok_or(Error::HeavyBusy)?;

        let permit = self
            .gate
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed");

        // 🔴 Beefy mode: this process believes the box is handed to the 27B —
        // `multi_gpu` moves with the URL and is owned by `beefy.rs` (enter/exit/
        // apply_at_boot) — but the 27B may not be resident. A beefy container
        // restart (a runtime update) resets the supervisor to `loaded:false` while
        // every persisted half of the mode survives, and exl3 does NOT lazy-load
        // on a bare inference POST. Without this, the first chat after every beefy
        // update dies with 503 `model_not_loaded` and the mode reads stuck.
        //
        // One local /health GET per request; the load itself is single-flight in
        // `beefy::ensure_loaded_url`, and we are inside the semaphore, so only one
        // request pays it.
        if self.multi_gpu.load(Ordering::Relaxed) {
            crate::beefy::ensure_loaded_url(&self.base())
                .await
                .map_err(|e| Error::Upstream(503, e.to_string()))?;
        }

        // 🔴 The surface marker `crate::sampling::apply_sampling` stamps on the body is
        // koblem-private bookkeeping — it names the settings key the call's sampling came
        // from so `llm_logs` can attribute a bad row. It must NOT go on the wire: this is
        // the single chokepoint every LLM path funnels through, so it is stripped here and
        // nowhere else. Only bodies that actually carry it pay for the clone.
        let scrubbed = body
            .get(crate::sampling::SAMPLING_KEY_FIELD)
            .is_some()
            .then(|| {
                let mut copy = body.clone();
                if let Some(obj) = copy.as_object_mut() {
                    obj.remove(crate::sampling::SAMPLING_KEY_FIELD);
                }
                copy
            });
        let body = scrubbed.as_ref().unwrap_or(body);

        // Thread the gate-chosen card to llama-server via the X-Worker-Gpu header
        // — it's an OpenAI-style resident server (no `gpu` body field); the worker
        // relocates to that card before serving. No-op when the scheduler is
        // unconfigured (header omitted → server keeps its current/default card).
        let mut req = self
            .http
            .post(format!("{}/v1/chat/completions", self.base()))
            .json(&body);
        // 🔴 Never on a both-cards server: naming a GPU makes the worker re-exec
        // its child with a single-card CUDA_VISIBLE_DEVICES, which a 16.7 GiB
        // tensor-split model cannot fit. See `multi_gpu`.
        if let Some(g) = gpu_guard.chosen_gpu()
            && !self.multi_gpu.load(Ordering::Relaxed)
        {
            req = req.header("X-Worker-Gpu", g);
        }

        let resp = req.send().await.map_err(Error::Request)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status.as_u16(), body));
        }

        Ok((
            LlmLease {
                _permit: permit,
                _gpu: gpu_guard,
            },
            resp,
        ))
    }

    /// Forward a chat completion request and return the parsed JSON body with
    /// any `<think>…</think>` reasoning blocks stripped from every choice's
    /// `message.content`. Use this for non-streaming calls — it saves every
    /// call site from repeating the extract-and-strip dance.
    pub async fn chat_completion_json(&self, body: &Value) -> Result<(LlmLease, Value), Error> {
        let (permit, resp) = self.chat_completion(body).await?;
        let mut parsed: Value = resp.json().await.map_err(Error::Request)?;
        strip_think_in_response(&mut parsed);
        Ok((permit, parsed))
    }

    /// [`chat_completion_json`], with the call recorded in `llm_logs`.
    ///
    /// 🔴 **Use this for every internal call that composes a prompt in Rust.** The streaming path
    /// has been logged since it had a UI to tick; the non-streaming ones were not, so the composed
    /// system prompt — the thing that is hardest to reconstruct and easiest to get wrong — existed
    /// only inside a request that had already gone. Diagnosing a bad draft on 2026-08-09 meant
    /// rebuilding the prompt from the source strings twice, and being wrong once, before someone
    /// replayed the real endpoint and read the answer off it in one attempt.
    ///
    /// ⚠️ Logging is BEST EFFORT and never fails the request: a broken log row must not cost a
    /// render. Failures warn and are dropped.
    ///
    /// The row's `status` distinguishes the outcomes the way the streaming path does — `complete`
    /// or `error` — so a caller reading these back can tell a refusal from a bad prompt.
    pub async fn chat_completion_json_logged(
        &self,
        body: &Value,
        db: &PgPool,
        caller: &str,
    ) -> Result<(LlmLease, Value), Error> {
        let start = Instant::now();
        let num = |k: &str| body.get(k).and_then(Value::as_f64);
        let int = |k: &str| body.get(k).and_then(Value::as_i64).map(|v| v as i32);
        let log_id = llm_logs::start_log(
            db,
            &llm_logs::CreateLog {
                caller: Some(caller.to_string()),
                // The whole conversation — system prompt included. That IS the point.
                //
                // 🔴 Verbatim EXCEPT for media payloads, which are replaced by a
                // `[image omitted: N b64 bytes]` placeholder. The text is what makes this row worth
                // having; the base64 is what makes it unreadable and enormous. A single H3 draft
                // carrying three reference stills was already writing megabytes into `llm_logs` per
                // call, and the audio attachment added by `ltx_video::song_slice_for_writer` would
                // add ~600 KB more per draft — for a payload nobody can read back out of a log.
                //
                // `/api/v1/chat` has always sanitised its own rows (`routes::chat`); this path is
                // simply catching up, and shares that one implementation rather than growing a
                // second.
                request_messages: crate::routes::chat::sanitize_messages_for_log(
                    body.get("messages").cloned().unwrap_or(Value::Null),
                ),
                request_tools: body.get("tools").cloned(),
                request_temperature: num("temperature"),
                request_max_tokens: int("max_tokens"),
                request_top_p: num("top_p"),
                request_top_k: int("top_k"),
                request_min_p: num("min_p"),
                request_presence_penalty: num("presence_penalty"),
                request_repetition_penalty: num("repetition_penalty"),
                // Where koblem actually puts it — a template kwarg, not a top-level field. Read it
                // from the same place it is written or every row claims thinking is unset.
                request_enable_thinking: body
                    .pointer("/chat_template_kwargs/enable_thinking")
                    .and_then(Value::as_bool),
                // Which knob shaped this call: the surface key
                // `crate::sampling::apply_sampling` stamped on the body, so a
                // regression traced to a row is traceable back to a setting. Falls
                // back to the resolved preset name for bodies built the older way.
                sampling_preset: body
                    .get(crate::sampling::SAMPLING_KEY_FIELD)
                    .or_else(|| body.get("sampling_preset"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                request_grammar: body
                    .get("grammar")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                streaming: false,
                response_content: None,
                response_reasoning_content: None,
                response_tool_calls: None,
                finish_reason: None,
                tool_rewrite_applied: false,
                duration_ms: None,
                prompt_tokens: None,
                completion_tokens: None,
            },
        )
        .await
        .map_err(|e| tracing::warn!(error = %e, caller, "llm_logs: could not open a row"))
        .ok();

        let result = self.chat_completion_json(body).await;
        let ms = start.elapsed().as_millis() as i64;

        if let Some(id) = log_id {
            let outcome = match &result {
                Ok((_, parsed)) => {
                    let choice = parsed.pointer("/choices/0");
                    let msg = choice.and_then(|c| c.get("message"));
                    let str_at = |v: Option<&Value>, k: &str| {
                        v.and_then(|v| v.get(k))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    };
                    let usage = |k: &str| {
                        parsed
                            .pointer(&format!("/usage/{k}"))
                            .and_then(Value::as_i64)
                            .map(|v| v as i32)
                    };
                    // Bound to locals, not inlined: each is an owned `String` borrowed as `&str`
                    // by the call, and the call is awaited — so the temporaries have to outlive the
                    // await point rather than the argument list.
                    let content = str_at(msg, "content");
                    // Kept even though `chat_completion_json` strips `<think>` from content: a
                    // model that spends its whole budget reasoning returns EMPTY content, and this
                    // column is the only thing that says so rather than "it wrote nothing".
                    let reasoning = str_at(msg, "reasoning_content");
                    let finish = str_at(choice, "finish_reason");
                    llm_logs::finalize_log_success(
                        db,
                        id,
                        content.as_deref(),
                        reasoning.as_deref(),
                        msg.and_then(|m| m.get("tool_calls")),
                        finish.as_deref(),
                        usage("prompt_tokens"),
                        usage("completion_tokens"),
                        ms,
                    )
                    .await
                }
                Err(e) => llm_logs::finalize_log_error(db, id, &e.to_string(), ms).await,
            };
            if let Err(e) = outcome {
                tracing::warn!(error = %e, caller, "llm_logs: could not finalize the row");
            }
        }
        result
    }

    /// Stream a chat completion to llama-server while writing live progress
    /// to a pre-existing `llm_logs` row (the caller must have already inserted
    /// the row via [`llm_logs::start_log`]). Force-enables `stream=true` and
    /// `stream_options.include_usage` on the outgoing body, then accumulates
    /// SSE deltas into a non-streaming-shaped response so callers can swap
    /// from `chat_completion_json` 1:1.
    ///
    /// **Logging behaviour**:
    /// - Every `flush_every`, UPDATE the row with accumulated content,
    ///   reasoning_content, and completion_tokens (so the UI can tick).
    /// - On success, finalize the row as `complete` with the assembled
    ///   response + final usage + finish_reason.
    /// - On error, finalize as `error` with the failure message.
    /// - If the future is dropped before completion (caller cancelled, HTTP
    ///   client disconnect), the drop guard finalizes as `cancelled`.
    pub async fn chat_completion_streaming_with_log(
        &self,
        body: &Value,
        db: PgPool,
        log_id: Uuid,
        flush_every: Duration,
    ) -> Result<(LlmLease, Value), Error> {
        // Force streaming on the outgoing request — even if the caller passed
        // a non-streaming body. include_usage so the final chunk carries
        // prompt_tokens / completion_tokens.
        let mut body = body.clone();
        body["stream"] = json!(true);
        match body.get_mut("stream_options") {
            Some(Value::Object(opts)) => {
                opts.entry("include_usage").or_insert(json!(true));
            }
            _ => {
                body["stream_options"] = json!({"include_usage": true});
            }
        }

        let start = Instant::now();
        // Drop guard fires `cancelled` finalize if we exit without a clean
        // success/error finalize call (i.e. the parent task was cancelled).
        let mut guard = LogDropGuard {
            db: db.clone(),
            log_id,
            start,
            finalized: false,
        };

        let result = self
            .stream_and_assemble(&body, &db, log_id, start, flush_every)
            .await;

        match result {
            Ok((permit, assembled, finish_reason, prompt_tokens, completion_tokens)) => {
                let content = extract_str(&assembled, "content");
                let reasoning = extract_str(&assembled, "reasoning_content");
                let elapsed = start.elapsed().as_millis() as i64;
                if let Err(e) = llm_logs::finalize_log_success(
                    &db,
                    log_id,
                    content.as_deref(),
                    reasoning.as_deref(),
                    None,
                    finish_reason.as_deref(),
                    prompt_tokens,
                    completion_tokens,
                    elapsed,
                )
                .await
                {
                    tracing::warn!(%log_id, "failed to finalize llm log: {e}");
                }
                guard.finalized = true;
                Ok((permit, assembled))
            }
            Err(e) => {
                let elapsed = start.elapsed().as_millis() as i64;
                if let Err(fe) =
                    llm_logs::finalize_log_error(&db, log_id, &e.to_string(), elapsed).await
                {
                    tracing::warn!(%log_id, "failed to finalize errored llm log: {fe}");
                }
                guard.finalized = true;
                Err(e)
            }
        }
    }

    /// Inner streaming loop. Returns assembled response + final metadata.
    /// Split out of [`chat_completion_streaming_with_log`] so the public method
    /// can wrap a single match around success/error finalize.
    async fn stream_and_assemble(
        &self,
        body: &Value,
        db: &PgPool,
        log_id: Uuid,
        start: Instant,
        flush_every: Duration,
    ) -> Result<(LlmLease, Value, Option<String>, Option<i32>, Option<i32>), Error> {
        let (permit, resp) = self.chat_completion(body).await?;

        let mut byte_stream = resp.bytes_stream();
        let mut sse_buffer = String::new();

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut finish_reason: Option<String> = None;
        let mut prompt_tokens: Option<i32> = None;
        let mut completion_tokens: Option<i32> = None;
        let mut last_flush = start;

        let flush = |db: &PgPool, log_id: Uuid, content: &str, reasoning: &str, ct: Option<i32>| {
            let db = db.clone();
            let content = if content.is_empty() {
                None
            } else {
                Some(content.to_string())
            };
            let reasoning = if reasoning.is_empty() {
                None
            } else {
                Some(reasoning.to_string())
            };
            async move {
                if let Err(e) = llm_logs::update_log_progress(
                    &db,
                    log_id,
                    content.as_deref(),
                    reasoning.as_deref(),
                    ct,
                )
                .await
                {
                    tracing::warn!(%log_id, "progress update failed: {e}");
                }
            }
        };

        let mut done = false;
        while !done {
            let chunk_bytes = match byte_stream.next().await {
                Some(Ok(b)) => b,
                Some(Err(e)) => return Err(Error::Request(e)),
                None => break,
            };

            sse_buffer.push_str(&String::from_utf8_lossy(&chunk_bytes));

            while let Some(line_end) = sse_buffer.find('\n') {
                let line = sse_buffer[..line_end].trim().to_string();
                sse_buffer = sse_buffer[line_end + 1..].to_string();

                if line.is_empty() {
                    continue;
                }
                if line == "data: [DONE]" {
                    done = true;
                    break;
                }
                let json_str = line.strip_prefix("data: ").unwrap_or(&line);
                let chunk: Value = match serde_json::from_str(json_str) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                if let Some(choices) = chunk.get("choices").and_then(|v| v.as_array()) {
                    for choice in choices {
                        if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                            finish_reason = Some(fr.to_string());
                        }
                        if let Some(delta) = choice.get("delta") {
                            if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                                content.push_str(c);
                            }
                            if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str())
                            {
                                reasoning.push_str(r);
                            }
                        }
                    }
                }
                if let Some(usage) = chunk.get("usage") {
                    prompt_tokens = usage
                        .get("prompt_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                    completion_tokens = usage
                        .get("completion_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                }
            }

            if last_flush.elapsed() >= flush_every {
                flush(db, log_id, &content, &reasoning, completion_tokens).await;
                last_flush = Instant::now();
            }
        }

        // Final progress flush before assembling — captures the last sliver
        // of tokens between the last interval flush and stream end.
        flush(db, log_id, &content, &reasoning, completion_tokens).await;

        let mut assembled = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                    "reasoning_content": reasoning,
                },
                "finish_reason": finish_reason,
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
            }
        });
        // Same think-stripping + JSON-from-reasoning recovery as the
        // non-streaming path so callers see identical shapes.
        strip_think_in_response(&mut assembled);

        Ok((
            permit,
            assembled,
            finish_reason,
            prompt_tokens,
            completion_tokens,
        ))
    }

    /// Check llama-server health.
    pub async fn health(&self) -> Result<bool, Error> {
        let resp = self
            .http
            .get(format!("{}/health", self.base()))
            .send()
            .await
            .map_err(Error::Request)?;

        if !resp.status().is_success() {
            return Ok(false);
        }

        #[derive(serde::Deserialize)]
        struct HealthResponse {
            status: String,
        }

        let health: HealthResponse = resp.json().await.map_err(Error::Request)?;
        Ok(health.status == "ok")
    }

    /// Best-effort lookup of the loaded model name from llama.cpp's
    /// OpenAI-compatible `/v1/models` endpoint. Returns None if the server
    /// is unreachable or returns an unexpected shape.
    pub async fn model_name(&self) -> Option<String> {
        let resp = self
            .http
            .get(format!("{}/v1/models", self.base()))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        #[derive(serde::Deserialize)]
        struct ModelEntry {
            id: String,
        }
        #[derive(serde::Deserialize)]
        struct ModelsList {
            data: Vec<ModelEntry>,
        }
        let list: ModelsList = resp.json().await.ok()?;
        let id = list.data.into_iter().next()?.id;
        // llama.cpp sometimes exposes the full path — strip to the leaf.
        Some(
            id.rsplit(['/', '\\'])
                .next()
                .unwrap_or(&id)
                .trim_end_matches(".gguf")
                .to_string(),
        )
    }

    /// The context length llama-server is actually running, from `/props`.
    ///
    /// 🔴 Needed because koblem's `CONTEXT_SIZE` env describes neither model once
    /// Beefy can swap them (gemma 73728, Qwen3.8 131072), and `/v1/models` must
    /// not advertise a number the server will reject.
    ///
    /// Like `model_name`, this rides `proxy_get_no_drain` on the worker-isolation
    /// parent, so it NEVER spawns the child — and returns `None` while the child
    /// is unloaded rather than loading 25 GiB to answer a metadata question.
    pub async fn context_length(&self) -> Option<u32> {
        let resp = self
            .http
            .get(format!("{}/props", self.base()))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        // llama.cpp reports it under the slot defaults; fall back to a top-level
        // n_ctx for older/newer shapes rather than guessing.
        v.get("default_generation_settings")
            .and_then(|d| d.get("n_ctx"))
            .or_else(|| v.get("n_ctx"))
            .and_then(|n| n.as_u64())
            .map(|n| n as u32)
    }
}

fn extract_str(assembled: &Value, key: &str) -> Option<String> {
    assembled
        .get("choices")?
        .get(0)?
        .get("message")?
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

struct LogDropGuard {
    db: PgPool,
    log_id: Uuid,
    start: Instant,
    finalized: bool,
}

impl Drop for LogDropGuard {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let db = self.db.clone();
        let id = self.log_id;
        let elapsed = self.start.elapsed().as_millis() as i64;
        // Spawn a one-shot task to mark the row cancelled. We're in a Drop
        // impl so we can't .await; the spawn detaches and the runtime drives
        // it to completion. Best-effort — if the runtime is shutting down too,
        // the row stays as `running` and a future GC sweep will clean it.
        tokio::spawn(async move {
            if let Err(e) = llm_logs::finalize_log_cancelled(&db, id, elapsed).await {
                tracing::warn!(%id, "cancellation finalize failed: {e}");
            }
        });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("llama-server returned {0}: {1}")]
    Upstream(u16, String),
    #[error("LLM service is administratively disabled")]
    HeavyBusy,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The LLM is now a normal budget-scheduled service: it does NOT bounce just
    // because another service is rendering (the budget admits both on separate
    // cards, or blocks rather than failing). It only refuses (HeavyBusy) when the
    // LLM service is administratively disabled. The URL is deliberately
    // unroutable: if the disable gate regressed and let the request through, we'd
    // get Error::Request, not HeavyBusy.
    #[tokio::test]
    async fn llm_bounces_when_disabled() {
        let gpu = GpuLock::new();
        let llama = LlamaClient::new("http://127.0.0.1:1/unused".to_string(), gpu.clone());

        gpu.set_service_enabled(kob_gpu_gate::ServiceKind::Llm, false)
            .await;

        let result = llama.chat_completion(&json!({"messages": []})).await;

        assert!(
            matches!(result, Err(Error::HeavyBusy)),
            "LLM must bounce with HeavyBusy while the LLM service is disabled"
        );
    }
}
