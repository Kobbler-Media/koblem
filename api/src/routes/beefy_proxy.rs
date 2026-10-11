//! OpenAI-compatible front door to Beefy for an external coding agent (pi).
//!
//! `POST /api/v1/beefy/chat/completions` and `GET /api/v1/beefy/models`. pi's
//! `baseUrl` is `http://<host>:8090/api/v1/beefy`; it appends the rest itself.
//! `POST /api/v1/beefy/systemone` shares the gate below; see `beefy_systemone.rs`.
//!
//! Why neither existing path will do:
//! - **`:8103` directly bypasses the gate.** With Beefy off, the first POST makes the
//!   exl3 supervisor load 21.7 GiB across both cards on top of whatever is running.
//! - **`/api/v1/chat/completions` follows koblem's ONE LLM URL**, so with Beefy off
//!   an agent silently gets gemma-4-12b at 73k context. It also writes every body to
//!   `llm_logs`, buffers tool-call deltas until the end, and rewrites thinking.
//!
//! This one answers 503 unless the box is held for the 27B, and is otherwise a byte
//! pipe: no body rewriting, no `llm_logs` row (a 190k-token prompt a turn would
//! bloat it), no request timeout (a cold 195k prefill is ~590 s to the first token;
//! TabbyAPI's 15 s SSE pings keep the socket warm meanwhile). One tracing line per
//! request carries the metadata worth keeping — `usage.prompt_time` above all, the
//! cache-hit oracle (seconds = prefix reused, ~590 = it was invalidated).
//!
//! 🔴 It does NOT take `LlamaClient`'s semaphore(1). That serialises koblem's own
//! calls, and an agent's ten-minute prefill queued behind it would stall the media
//! library; exl3 runs `max_batch_size: 2`. It does hold an LLM gate guard, so the
//! scheduler page shows the LLM busy while an agent is using it.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::StreamExt;
use kob_gpu_gate::{Acquisition, ServiceKind};
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::AppState;
use crate::auth::require_auth;

/// A deep context carrying a few base64 screenshots. axum's 2 MiB default is not
/// enough — a 195k-token text prompt alone is most of a megabyte of JSON.
const MAX_BODY: usize = 64 * 1024 * 1024;
/// How much of the response tail is kept to read `usage` from once it ends.
const TAIL_BYTES: usize = 64 * 1024;
/// What a client is told to wait when Beefy is off. Entering takes ~30 s.
const RETRY_AFTER_S: &str = "60";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/beefy/chat/completions", post(chat_completions))
        .route("/api/v1/beefy/models", get(models))
        .route(
            "/api/v1/beefy/systemone",
            post(super::beefy_systemone::systemone),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

pub(super) fn upstream() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // 🔴 No overall timeout — see the module doc. Connect is bounded so a dead
        // container fails in seconds rather than never.
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build beefy proxy HTTP client")
    })
}

/// An error in the OpenAI shape, so the agent surfaces the message verbatim.
pub(super) fn oai_error(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message, "type": kind } })),
    )
        .into_response()
}

/// Auth, then "is the box held for the 27B", then the LLM gate guard. `Err` is the
/// response to send as-is.
async fn admit(state: &AppState, headers: &HeaderMap) -> Result<(String, Acquisition), Response> {
    authenticate(&state.db, state.api_key.as_deref(), headers).await?;
    hold(state).await
}

pub(super) async fn authenticate(
    db: &PgPool,
    api_key: Option<&str>,
    headers: &HeaderMap,
) -> Result<(), Response> {
    match require_auth(db, api_key, headers).await {
        Ok(_) => Ok(()),
        Err(_) => Err(oai_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid bearer token",
        )),
    }
}

/// [`admit`] past auth: the beefy URL plus the LLM gate guard, held for as long as
/// the caller keeps it.
pub(super) async fn hold(state: &AppState) -> Result<(String, Acquisition), Response> {
    let Some(base) = crate::beefy::serving_url(state).await else {
        let mut resp = oai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "beefy_off",
            "Beefy LLM mode is off. Turn it on (POST /api/v1/gpu/beefy {\"on\":true}); \
             this endpoint never falls back to the normal LLM.",
        );
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_S));
        return Err(resp);
    };
    // The box is held but the 27B may not be resident: a beefy container restart
    // (a runtime update) leaves the supervisor honest at `loaded:false` while
    // koblem's persisted mode survives. Make it resident here rather than pass
    // the 503 to pi and let its retry loop be the layer that pays for a ~40 s
    // load. Single-flight in `beefy::ensure_loaded`; steady state is one local
    // /health GET.
    if let Err(e) = crate::beefy::ensure_loaded(state).await {
        return Err(oai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "beefy_not_loaded",
            &format!("the 27B is not resident and could not be loaded: {e}"),
        ));
    }
    // While the keeper holds the box this reuses its standing placement and never
    // parks. `None` only when an operator has disabled the LLM service.
    let Some(guard) = state.gpu.acquire(ServiceKind::Llm).await else {
        return Err(oai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "llm_disabled",
            "the LLM service is administratively disabled",
        ));
    };
    Ok((base, guard))
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (base, guard) = match admit(&state, &headers).await {
        Ok(admitted) => admitted,
        Err(resp) => return resp,
    };
    let meter = Meter {
        started: Instant::now(),
        caller: headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
        status: 0,
        request_bytes: body.len(),
        response_bytes: 0,
        tail: Vec::new(),
        _gpu: guard,
    };
    let resp = match upstream()
        .post(format!(
            "{}/v1/chat/completions",
            base.trim_end_matches('/')
        ))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "beefy proxy: upstream unreachable");
            return oai_error(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                &format!("beefy did not answer: {e}"),
            );
        }
    };
    relay(resp, meter)
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (base, _guard) = match admit(&state, &headers).await {
        Ok(admitted) => admitted,
        Err(resp) => return resp,
    };
    let resp = match upstream()
        .get(format!("{}/v1/models", base.trim_end_matches('/')))
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return oai_error(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                &format!("beefy did not answer: {e}"),
            );
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match resp.bytes().await {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(e) => oai_error(
            StatusCode::BAD_GATEWAY,
            "upstream_unreachable",
            &format!("beefy's model list did not arrive: {e}"),
        ),
    }
}

/// Stream the upstream answer back untouched — status, content type and bytes.
fn relay(resp: reqwest::Response, mut meter: Meter) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    meter.status = status.as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok());
    // The meter rides inside the stream: its Drop fires when the body ends OR the
    // client hangs up, and it keeps the LLM guard alive for exactly that long.
    let stream = resp.bytes_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            meter.observe(bytes);
        }
        chunk
    });
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    if let Some(ct) = content_type {
        out.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    // Nothing in front of koblem may sit on an SSE stream.
    out.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    out
}

/// One log line per request, written on Drop.
struct Meter {
    started: Instant,
    caller: String,
    status: u16,
    request_bytes: usize,
    response_bytes: usize,
    /// The last [`TAIL_BYTES`]-ish of the response, where `usage` lives.
    tail: Vec<u8>,
    _gpu: Acquisition,
}

impl Meter {
    fn observe(&mut self, chunk: &[u8]) {
        self.response_bytes += chunk.len();
        self.tail.extend_from_slice(chunk);
        // Trim in bulk, not per chunk: a long stream is thousands of tiny chunks.
        if self.tail.len() > 2 * TAIL_BYTES {
            let cut = self.tail.len() - TAIL_BYTES;
            self.tail.drain(..cut);
        }
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        let usage = last_usage(&self.tail);
        let field = |k: &str| {
            usage
                .as_ref()
                .and_then(|u| u.get(k))
                .and_then(Value::as_f64)
        };
        info!(
            caller = %self.caller,
            status = self.status,
            elapsed_s = self.started.elapsed().as_secs_f64(),
            request_bytes = self.request_bytes,
            response_bytes = self.response_bytes,
            prompt_tokens = field("prompt_tokens"),
            completion_tokens = field("completion_tokens"),
            prompt_time_s = field("prompt_time"),
            "beefy proxy: request finished"
        );
    }
}

/// The last non-null `"usage": {…}` in the response tail — the final SSE chunk of a
/// stream, or the body's own field otherwise. Parses only that object, so a tail
/// that starts mid-token does not matter.
fn last_usage(tail: &[u8]) -> Option<Value> {
    const KEY: &[u8] = b"\"usage\":";
    let mut end = tail.len();
    while let Some(pos) = tail[..end].windows(KEY.len()).rposition(|w| w == KEY) {
        let rest = &tail[pos + KEY.len()..];
        if let Some(Ok(v)) = serde_json::Deserializer::from_slice(rest)
            .into_iter::<Value>()
            .next()
            && v.is_object()
        {
            return Some(v);
        }
        end = pos;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::last_usage;

    #[test]
    fn usage_comes_from_the_final_sse_chunk() {
        let tail = b"data: {\"choices\":[],\"usage\":null}\n\n\
                     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":195055,\"prompt_time\":1.98}}\n\n\
                     data: [DONE]\n\n";
        let u = last_usage(tail).expect("usage");
        assert_eq!(u["prompt_tokens"], 195055);
        assert_eq!(u["prompt_time"], 1.98);
    }

    #[test]
    fn a_null_usage_is_not_an_answer() {
        assert!(last_usage(b"data: {\"usage\": null}\n\n").is_none());
    }

    #[test]
    fn a_tail_cut_mid_body_still_finds_it() {
        let tail = b"t of a long answer\"}}],\"usage\": {\"prompt_tokens\":7}}";
        assert_eq!(last_usage(tail).expect("usage")["prompt_tokens"], 7);
    }
}
