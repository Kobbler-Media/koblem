//! Stable Audio 3 (sa3-server) music routes — the LIGHT music engine offered
//! alongside acestep/songgen on the music surface. Auth-gated.
//!
//! Two modes:
//!   - `generate` (text→audio, t2a): JSON body — `prompt` (required) + optional
//!     `seconds`/`seed`/`sampler`/`steps`/`model`. Game SFX / short music.
//!   - `edit` (audio→audio): multipart with an `input` wav + a `mode` of
//!     `a2a` (SDEdit restyle), `inpaint` (regenerate `mask` regions), or
//!     `continuation` (keep the input as a prefix, extend to `seconds`).
//!
//! 🔴 **`generate` is a queued job; `edit` is not.** `POST /generate` builds the sa3-server body and
//! hands it to the global GPU queue (202 + a job id, polled at `GET /api/v1/sa3/jobs/{id}`);
//! `queue::adapters::sa3` owns the render from there. `edit` takes its source audio as a multipart
//! part and a queue payload is JSON that may sit for hours — there is nowhere for those bytes to
//! live, so it keeps the synchronous path: acquire the LIGHT `music` slot (shares acestep's lane —
//! TTS preempts; STT/Vision + peer music engines get evicted), poll, download, save under
//! `SONGS_DIR`, release. A preempt mid-render returns 503 so the client can retry.

use std::time::Instant;

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, Multipart, Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use koblem_client::types::{
    GenerationOutput, Job as JobRow, JobKind, JobState, MusicTrack, Sa3GenerateRequest,
    Sa3GenerateResponse, Sa3Props,
};
use serde_json::{Value, json};
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_jobs as queue_store;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::engine_cancel;
use crate::routes::tier2_helpers::{extract_caller, log_async, record_generation};
use crate::sa3::Error as Sa3Error;

/// 64 MB cap — `edit` accepts an input-audio upload (short clips).
const SA3_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/sa3/generate", post(generate))
        .route("/api/v1/sa3/jobs/{id}", get(job))
        .route("/api/v1/sa3/active", get(active))
        .route("/api/v1/sa3/cancel", post(cancel))
        .route("/api/v1/sa3/edit", post(edit))
        .route("/api/v1/sa3/props", get(props))
        .route("/api/v1/sa3/file/{name}", get(serve_file))
        .layer(DefaultBodyLimit::max(SA3_BODY_LIMIT))
}

// ── handlers ──────────────────────────────────────────────────────────────────

/// `POST /api/v1/sa3/generate` — text→audio (t2a). JSON body. 202 and a job id.
///
/// 🔴 The 503 is gone. "sa3 disabled or GPU unavailable" was the gate refusing rather than
/// queueing, and refusing is the behaviour this change exists to remove.
async fn generate(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let bytes = match to_bytes(request.into_body(), SA3_BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
    };
    let req = match serde_json::from_slice::<Sa3GenerateRequest>(&bytes) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid request JSON: {e}"),
            );
        }
    };
    if req.prompt.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "prompt is required");
    }
    if !state.sa3.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "sa3 service not configured (set SA3_SERVER_URL or the DB setting)",
        );
    }

    // 🔴 The payload IS the sa3-server body, so nothing may be added to it — a stray key rides
    // straight into `POST /generate`. `save` therefore has nowhere to live: the adapter keeps and
    // files every queued clip, because a render nobody kept is GPU minutes for nothing.
    let who = match crate::routes::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let enqueued = crate::routes::queue::enqueue(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(&headers)
            .request(JobKind::Sa3, build_generate_body(&req)),
    )
    .await;
    match enqueued {
        Ok(resp) => (StatusCode::ACCEPTED, Json(resp)).into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

/// `GET /api/v1/sa3/jobs/{id}` — poll a queued render.
async fn job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such sa3 job");
    };
    if row.kind != JobKind::Sa3 {
        return err(StatusCode::NOT_FOUND, "that job is not an sa3 render");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/sa3/active` — the queued or running render, so a reloaded page can find the job it
/// lost without having kept the id.
async fn active(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let live = queue_store::live(&state.db).await.unwrap_or_default();
    let pending = queue_store::pending(&state.db).await.unwrap_or_default();
    let found = live
        .into_iter()
        .chain(pending)
        .find(|j| j.kind == JobKind::Sa3);
    match found {
        Some(row) => Json(json!({ "job": view(&state, &row).await })).into_response(),
        None => Json(json!({ "job": Value::Null })).into_response(),
    }
}

/// `POST /api/v1/sa3/cancel` — cancel the in-flight render, through the dispatcher.
///
/// 🔴 Not straight at the engine: the dispatcher owns the run, so it is the only thing that can
/// stop it AND file the terminal state. A job in `collecting` is deliberately unreachable.
async fn cancel(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(&state, JobKind::Sa3, "no sa3 render is queued or running").await
}

/// Render a queue row in the shape this page already speaks.
async fn view(state: &AppState, row: &JobRow) -> Value {
    let result = queue_store::detail(&state.db, &row.id)
        .await
        .ok()
        .flatten()
        .and_then(|(_, _, result)| result)
        .unwrap_or(Value::Null);
    json!({
        "id": row.id,
        // `collecting` has no equivalent in this engine's vocabulary, and the clip is not saved
        // yet — the card is free but the page still has nothing to play.
        "state": match row.state {
            JobState::Queued | JobState::Held => "queued",
            JobState::Running | JobState::Collecting => "rendering",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        },
        "server_job_id": row.server_job_id,
        "phase": row.progress.phase,
        "tracks": result.get("tracks").cloned().unwrap_or(json!([])),
        "warnings": row.warnings,
        "error": row.error,
        "queued_ms": row.queued_ms,
        "gate_wait_ms": row.admit_ms,
        "duration_ms": row.render_ms,
        "blocked_reason": row.blocked_reason,
        "eta_start": row.eta_start,
    })
}

/// `POST /api/v1/sa3/edit` — audio→audio (a2a / inpaint / continuation). Multipart.
async fn edit(State(state): State<AppState>, headers: HeaderMap, mp: Multipart) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let mut parsed = match parse_edit_multipart(mp).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(input) = parsed.input.take() else {
        return err(StatusCode::BAD_REQUEST, "missing `input` audio file");
    };
    let mode = parsed.field("mode").unwrap_or_default();
    if mode != "a2a" && mode != "inpaint" && mode != "continuation" {
        return err(
            StatusCode::BAD_REQUEST,
            "mode must be a2a, inpaint, or continuation",
        );
    }
    let prompt = parsed.field("prompt").unwrap_or_default();
    if prompt.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "prompt is required");
    }
    if mode == "inpaint"
        && parsed
            .field("mask")
            .map(|m| m.trim().is_empty())
            .unwrap_or(true)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "inpaint mode requires `mask` (\"s0:e0,s1:e1\" seconds)",
        );
    }
    if mode == "continuation" && parsed.field("seconds").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "continuation mode requires `seconds` (total length)",
        );
    }

    let fields = build_edit_fields(&parsed);
    let seconds_used = parsed.field("seconds").and_then(|s| s.parse::<f32>().ok());
    let seed = parsed
        .field("seed")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let save = parsed
        .field("save")
        .map(|s| s != "false" && s != "0")
        .unwrap_or(true);
    run_pipeline(
        &state,
        &headers,
        "edit",
        seed,
        save,
        seconds_used,
        json!({
            "engine": "sa3", "mode": mode, "prompt": prompt, "seed": seed,
            "seconds": seconds_used, "had_audio_inputs": true,
        }),
        fields,
        input,
    )
    .await
}

/// `GET /api/v1/sa3/props` — reachability for the UI to enable/disable the SA3 tab.
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.sa3.is_configured() {
        return Json(Sa3Props::default()).into_response();
    }
    let healthy = state.sa3.healthy().await;
    let version = if healthy {
        state.sa3.version().await
    } else {
        None
    };
    Json(Sa3Props { healthy, version }).into_response()
}

/// `GET /api/v1/sa3/file/{name}` — serve a rendered clip from `SONGS_DIR`.
async fn serve_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "invalid filename");
    }
    let path = state.songs_dir.join(&name);
    match crate::routes::byte_serve::serve_file(&headers, &path, "audio/wav").await {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "clip not found"),
    }
}

// ── pipeline ──────────────────────────────────────────────────────────────────

/// The synchronous edit path. 🔴 Not queued, and not queueable: the source audio is a multipart
/// part and a queue payload is JSON that may sit for hours.
///
/// Acquire the light music gate, drive the async sa3 job to a wav, save it, log.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    state: &AppState,
    headers: &HeaderMap,
    mode_label: &str,
    seed: i64,
    save: bool,
    seconds_used: Option<f32>,
    request_meta: Value,
    fields: Vec<(String, String)>,
    input: (Bytes, String),
) -> Response {
    let caller = extract_caller(headers);

    // Acquire the LIGHT music slot (shared with acestep: TTS preempts, STT/Vision
    // + peer music engines evicted). None = music administratively disabled.
    let Some((guard, gate_wait)) = state.sa3.acquire().await else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "sa3 disabled or GPU unavailable — try again later",
        );
    };
    let started = Instant::now();

    let outcome = drive(state, fields, input, guard.chosen_gpu()).await;

    // Always release the GPU before disk/DB work.
    drop(guard);

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let gate_wait_ms = gate_wait.as_millis() as u64;
    let endpoint = endpoint_label(mode_label);

    let (content_type, audio) = match outcome {
        Ok(v) => v,
        Err(Sa3Error::Preempted) => {
            log_sa3(
                state,
                caller,
                endpoint,
                &request_meta,
                gate_wait_ms,
                elapsed_ms,
                503,
                json!({"error": "preempted"}),
            );
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "render preempted by a higher-priority GPU consumer — retry shortly",
            );
        }
        Err(e) => {
            warn!(error = %e, "sa3 render failed");
            log_sa3(
                state,
                caller,
                endpoint,
                &request_meta,
                gate_wait_ms,
                elapsed_ms,
                502,
                json!({"error": e.to_string()}),
            );
            return err(StatusCode::BAD_GATEWAY, &format!("sa3-server: {e}"));
        }
    };

    // Persist under SONGS_DIR (shared gallery). SA3 always emits wav.
    let _ = content_type;
    let filename = format!("{}-sa3-{}.wav", file_stamp(), short_seed(seed));
    if let Err(e) = save_clip(state, &filename, &audio).await {
        warn!(error = %e, %filename, "failed to save sa3 clip");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to save rendered clip",
        );
    }

    let track = MusicTrack {
        url: format!("/api/v1/sa3/file/{filename}"),
        filename: filename.clone(),
        seed,
        content_type: "audio/wav".to_string(),
        bytes: audio.len() as u64,
    };

    log_sa3(
        state,
        caller,
        endpoint,
        &request_meta,
        gate_wait_ms,
        elapsed_ms,
        200,
        json!({"filename": filename, "bytes": track.bytes}),
    );

    if save {
        record_generation(
            state,
            "music",
            endpoint,
            request_meta.clone(),
            vec![GenerationOutput {
                filename: track.filename.clone(),
                mime: track.content_type.clone(),
                size_bytes: track.bytes as i64,
                kind: "audio".to_string(),
            }],
        )
        .await;
    }

    let resp = Sa3GenerateResponse {
        tracks: vec![track],
        seconds_used,
        duration_ms: Some(elapsed_ms),
        gate_wait_ms: Some(gate_wait_ms),
    };
    Json(resp).into_response()
}

/// Submit the edit, await it (TTS-yielding), download the wav. `gpu` = the
/// gate-chosen card UUID (threaded into the C++ `gpu` field); `None` when the
/// placement layer is unconfigured.
async fn drive(
    state: &AppState,
    mut fields: Vec<(String, String)>,
    input: (Bytes, String),
    gpu: Option<&str>,
) -> Result<(String, Bytes), Sa3Error> {
    let client = &state.sa3;
    crate::routes::tier2_helpers::push_gpu_field(&mut fields, gpu);
    let job_id = client.submit_edit(&fields, input).await?;
    client.await_job(&job_id).await?;
    client.download(&job_id).await
}

// ── request building ──────────────────────────────────────────────────────────

/// JSON body for sa3-server `POST /generate`. Omits unset optionals so the server
/// applies its own defaults.
pub(crate) fn build_generate_body(req: &Sa3GenerateRequest) -> Value {
    let mut body = json!({ "prompt": req.prompt });
    let obj = body.as_object_mut().unwrap();
    if let Some(s) = req.seconds {
        obj.insert("seconds".into(), json!(s));
    }
    if let Some(s) = req.seed {
        obj.insert("seed".into(), json!(s));
    }
    if let Some(s) = &req.sampler {
        obj.insert("sampler".into(), json!(s));
    }
    if let Some(s) = req.steps {
        obj.insert("steps".into(), json!(s));
    }
    if let Some(m) = &req.model {
        obj.insert("model".into(), json!(m));
    }
    body
}

/// Multipart text fields for `POST /edit` — `mode`/`prompt` (required) plus the
/// per-mode knobs, forwarded verbatim from the request (the server validates).
fn build_edit_fields(parsed: &ParsedEdit) -> Vec<(String, String)> {
    let mut f = Vec::new();
    for key in [
        "mode", "prompt", "model", "seed", "sampler", "steps", "strength", "mask", "seconds",
    ] {
        if let Some(v) = parsed.field(key) {
            f.push((key.to_string(), v));
        }
    }
    f
}

// ── multipart parsing ─────────────────────────────────────────────────────────

struct ParsedEdit {
    fields: Vec<(String, String)>,
    input: Option<(Bytes, String)>,
}

impl ParsedEdit {
    fn field(&self, key: &str) -> Option<String> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

async fn parse_edit_multipart(mut mp: Multipart) -> Result<ParsedEdit, Response> {
    let mut out = ParsedEdit {
        fields: Vec::new(),
        input: None,
    };
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                let filename = field.file_name().map(|s| s.to_string());
                if name == "input" {
                    let b = field.bytes().await.map_err(|e| {
                        err(StatusCode::BAD_REQUEST, &format!("reading input: {e}"))
                    })?;
                    out.input = Some((b, filename.unwrap_or_else(|| "input.wav".into())));
                } else {
                    let t = field.text().await.map_err(|e| {
                        err(
                            StatusCode::BAD_REQUEST,
                            &format!("reading field {name}: {e}"),
                        )
                    })?;
                    out.fields.push((name, t));
                }
            }
            Ok(None) => break,
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    &format!("malformed multipart: {e}"),
                ));
            }
        }
    }
    Ok(out)
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn endpoint_label(mode: &str) -> &'static str {
    match mode {
        "edit" => "sa3-edit",
        _ => "sa3-generate",
    }
}

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

async fn save_clip(state: &AppState, filename: &str, bytes: &Bytes) -> std::io::Result<()> {
    tokio::fs::create_dir_all(&*state.songs_dir).await?;
    let path = state.songs_dir.join(filename);
    tokio::fs::write(path, bytes).await
}

#[allow(clippy::too_many_arguments)]
fn log_sa3(
    state: &AppState,
    caller: Option<String>,
    endpoint: &str,
    request_meta: &Value,
    gate_wait_ms: u64,
    duration_ms: u64,
    status: i32,
    response_meta: Value,
) {
    log_async(
        state.db.clone(),
        CreateLog {
            service: "music".into(),
            endpoint: endpoint.into(),
            request_meta: request_meta.clone(),
            response_meta,
            sample_id: None,
            gate_wait_ms: gate_wait_ms as i64,
            duration_ms: duration_ms as i64,
            status,
            caller,
        },
    );
}

/// Compact UTC timestamp for filenames: `20260608-134501`.
fn file_stamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Short, filename-safe seed tag (`r` for random/-1).
fn short_seed(seed: i64) -> String {
    if seed < 0 {
        "r".to_string()
    } else {
        seed.to_string()
    }
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}
