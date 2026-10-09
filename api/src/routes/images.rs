//! FLUX.2 image-generation routes (the GPU "flux" HEAVY engine). Auth-gated.
//!
//! The "smart model API": one `/generate` endpoint that resolves base-vs-edit
//! automatically from whether any reference input (init image / mask / ref
//! images) is present, fills model-appropriate sampling defaults, and hands the
//! resolved sdcpp body to the global GPU queue. The render itself lives in
//! `queue::adapters::flux2`; what stayed here is request parsing, body building,
//! prompt assist and props.
//!
//! 🔴 **`/generate` is SYNC-IF-FAST.** The row is created, and if the job is
//! admissible immediately it is waited on inline and answered exactly as it
//! always was — otherwise `202 { job_id }` and the page polls
//! `GET /api/v1/images/jobs/{id}` (or `/active` to re-find a job it lost). "One
//! quick image" must not become a queue ticket, and a render must not be lost to
//! a closed tab; this is both, from one implementation.
//!
//! 🔴 **No submit path 503s any more.** A busy or administratively disabled
//! engine is a `blocked_reason` on the row, not a refusal, so the render button
//! is never disabled.
//!
//! The renders (and any chained cutouts) are written to the gallery and served
//! from `/api/v1/generations/{id}/file/{n}`, never inlined as base64. That makes
//! gallery storage a hard requirement here, not an option — an id pointing at
//! nothing is worse than a loud 503, so `/generate` refuses before it queues
//! when `generations_dir` is unset.
//!
//! Plus a vision-capable `/suggest-prompt` that asks koblem's own VL model to
//! write a FLUX.2-good prompt from a brief (optionally matching reference
//! images), `/props` for the UI bootstrap, and `/cancel`.
//!
//! Mirrors routes/music3.rs — read that for the queue conventions.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine as _;
use bytes::Bytes;
use serde_json::{Value, json};
use tracing::warn;

use koblem_client::types::{
    BackgroundRemoval, GeneratedImage, ImageGenerateRequest as GenerateRequest,
    ImageGenerateResponse as GenerateResponse, ImageProps, Job, JobKind, JobState,
    SuggestPromptRequest, SuggestPromptResponse,
};

use crate::AppState;
use crate::auth::require_auth;
use crate::director_assets::{self, AssetJson};
use crate::matting::RemoveOptions;
use crate::repositories::gpu_jobs as queue_store;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::engine_cancel;
use crate::routes::tier2_helpers::{
    OutputBytes, extract_caller, generation_file_url, generations_root, log_async,
};

/// 64 MB body cap — comfortably covers a base64 init image + mask + a handful of
/// reference images, or the equivalent multipart upload.
const IMAGES_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/images/generate", post(generate))
        .route("/api/v1/images/jobs/{id}", get(job))
        .route("/api/v1/images/active", get(active))
        .route("/api/v1/images/suggest-prompt", post(suggest_prompt))
        .route("/api/v1/images/props", get(props))
        .route("/api/v1/images/cancel", post(cancel_current))
        .layer(DefaultBodyLimit::max(IMAGES_BODY_LIMIT))
}

// Request/response wire types live in `koblem_client::types` (imported above,
// shared with the `kob` CLI). The conversion of the chained background-removal
// params into the matting client's `RemoveOptions` stays server-side because
// `RemoveOptions` carries the upstream query-building logic.
fn bg_to_remove_options(bg: &BackgroundRemoval) -> RemoveOptions {
    RemoveOptions {
        backend: bg.backend.clone(),
        process_res: bg.process_res,
        sensitivity: bg.sensitivity,
        mask_blur: bg.mask_blur,
        mask_offset: bg.mask_offset,
        refine: bg.refine,
        invert: bg.invert,
        bg_mode: bg.bg_mode.clone(),
        bg_color: bg.bg_color.clone(),
        mask_only: None,
    }
}

/// Outcome of the chained matting step — the cutouts, a non-fatal error and/or
/// the matting service's reported backend + wall time. Shared with
/// `routes::krea2`, which chains the identical post-render step onto its own
/// images.
#[derive(Default)]
pub(crate) struct BgRemovalOutcome {
    /// One cutout per input image, in order. Empty whenever the step did not run
    /// or did not fully succeed — a partial set would pair a cutout with the
    /// wrong render, which is worse than no cutout at all.
    pub(crate) cutouts: Vec<Bytes>,
    pub(crate) error: Option<String>,
    pub(crate) backend: Option<String>,
    pub(crate) elapsed_seconds: Option<f64>,
}

impl BgRemovalOutcome {
    fn failed(msg: String) -> Self {
        Self {
            error: Some(msg),
            ..Default::default()
        }
    }
}

// ── defaults ────────────────────────────────────────────────────────────

/// Default render dimensions when the caller leaves them unset. 512² is the
/// quality/VRAM sweet spot for FLUX.2-Klein (a ~1 MP model) and leaves headroom
/// for a concurrent light GPU peer on the 12 GB card.
const DEFAULT_WIDTH: u32 = 512;
const DEFAULT_HEIGHT: u32 = 512;
/// Hard ceiling per side. klein degrades above ~1 MP, and large renders can OOM
/// if a light GPU peer is co-resident on the 12 GB card — but leave room for
/// oversized outputs when the operator wants them; we clamp rather than fault.
const MAX_DIM: u32 = 2048;
const MIN_DIM: u32 = 16;

/// Clamp a requested dimension into [MIN_DIM, MAX_DIM] and snap down to the ×16
/// the diffusion pipeline requires.
fn clamp_dim(v: u32) -> u32 {
    (v.clamp(MIN_DIM, MAX_DIM) / 16) * 16
}
/// Model-appropriate sampling defaults, applied ONLY where the caller left the
/// field unset. The FLUX.2 base runs full-step at moderate cfg; the edit DiT is
/// tuned for very-low-step, cfg≈1 image-conditioned edits.
const BASE_STEPS: u32 = 50;
const BASE_CFG: f32 = 4.0;
const EDIT_STEPS: u32 = 4;
const EDIT_CFG: f32 = 1.0;
const EDIT_SAMPLER: &str = "euler";

// ── handlers ────────────────────────────────────────────────────────────

/// `POST /api/v1/images/generate` — JSON or multipart. The smart model API.
async fn generate(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|ct| ct.starts_with("multipart/"));

    let req = if is_multipart {
        let mp = match Multipart::from_request(request, &state).await {
            Ok(mp) => mp,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
        };
        match parse_generate_multipart(&state, mp).await {
            Ok(Some(r)) => r,
            Ok(None) => return err(StatusCode::BAD_REQUEST, "missing `request` part"),
            Err(resp) => return resp,
        }
    } else {
        let bytes = match to_bytes(request.into_body(), IMAGES_BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
        };
        match serde_json::from_slice::<GenerateRequest>(&bytes) {
            Ok(r) => r,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
        }
    };

    enqueue_generate(&state, &headers, req).await
}

async fn enqueue_generate(
    state: &AppState,
    headers: &HeaderMap,
    mut req: GenerateRequest,
) -> Response {
    if !state.flux2.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "flux2 service not configured (set FLUX2_SERVICE_URL or the DB setting)",
        );
    }
    if req.prompt.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "prompt is required");
    }
    // The renders are delivered as gallery URLs, so refuse before queueing rather
    // than render something with nowhere to live.
    if generations_root(state, "images").await.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "gallery storage is not configured — set `generations_dir` in Settings; \
             image renders are served from the gallery",
        );
    }
    if let Err(msg) = capture_inputs(state, &mut req).await {
        return err(StatusCode::BAD_REQUEST, &msg);
    }

    // SMART MODEL RESOLUTION — the core feature. "auto" → "edit" when any
    // reference input is present, else "base". Explicit base/edit overrides.
    let has_ref = req.init_image.is_some()
        || req.mask.is_some()
        || req.reference_images.as_ref().is_some_and(|v| !v.is_empty());
    let model_used = match req.model.trim().to_ascii_lowercase().as_str() {
        "base" => "base",
        "edit" => "edit",
        // "auto" or anything unrecognised → resolve from inputs.
        _ => {
            if has_ref {
                "edit"
            } else {
                "base"
            }
        }
    };

    // Fill model-appropriate defaults ONLY where the caller left them unset.
    let width = clamp_dim(req.width.unwrap_or(DEFAULT_WIDTH));
    let height = clamp_dim(req.height.unwrap_or(DEFAULT_HEIGHT));
    let (steps, cfg, default_sampler) = match model_used {
        "edit" => (
            req.steps.unwrap_or(EDIT_STEPS),
            req.cfg.unwrap_or(EDIT_CFG),
            Some(EDIT_SAMPLER),
        ),
        _ => (
            req.steps.unwrap_or(BASE_STEPS),
            req.cfg.unwrap_or(BASE_CFG),
            None,
        ),
    };
    // Caller-supplied sampler always wins; edit defaults to euler.
    let sampler = req
        .sampler
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| default_sampler.map(str::to_string));

    let mut payload = build_img_gen_body(
        &req,
        model_used,
        width,
        height,
        steps,
        cfg,
        sampler.as_deref(),
    );
    // Not an engine field. It rides in the payload because the payload IS the queue
    // row, and the adapter that chains the cutout runs hours later in a different
    // process — there is nowhere else for a request-time choice to live.
    if let Some(bg) = req.remove_background.as_ref().filter(|bg| bg.enabled)
        && let Some(obj) = payload.as_object_mut()
        && let Ok(v) = serde_json::to_value(bg)
    {
        obj.insert("remove_background".into(), v);
    }

    // 🔴 The 503 is gone, and this is now the ONLY path to the engine. "flux busy
    // or administratively disabled" was the gate refusing rather than queueing;
    // an unavailable engine is a `blocked_reason` on the row. A direct route left
    // alive beside the queue is two writers to one engine.
    let who = match crate::routes::queue::principal_of(state, headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let enqueued = crate::routes::queue::enqueue_and_wait(
        state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(headers).request(JobKind::Flux2, payload),
    )
    .await;
    let (accepted, finished) = match enqueued {
        Ok(v) => v,
        Err((code, msg)) => return err(code, &msg),
    };

    let caller = extract_caller(headers);
    let Some(job) = finished else {
        log_flux(
            state,
            caller,
            model_used,
            steps,
            0,
            0,
            StatusCode::ACCEPTED.as_u16() as i32,
            json!({ "job_id": accepted.job_id, "state": "queued" }),
        );
        return (StatusCode::ACCEPTED, Json(accepted)).into_response();
    };
    inline_response(state, caller, &job).await
}

/// 🔴 INPUT CAPTURE AT ENQUEUE. An enqueued job can sit for hours and the browser
/// that POSTed the multipart is long gone by dispatch; the bytes have to be in
/// job-owned storage before the row exists. Every image field becomes an
/// `asset:<hash>` the adapter stages back into engine parts at dispatch.
async fn capture_inputs(state: &AppState, req: &mut GenerateRequest) -> Result<(), String> {
    let dir = state.director_assets_dir.as_ref();
    for field in [&mut req.init_image, &mut req.mask] {
        let Some(s) = field
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        *field = Some(director_assets::capture_ref(dir, &state.db, &s).await?);
    }
    if let Some(refs) = req.reference_images.take() {
        let mut captured = Vec::with_capacity(refs.len());
        for s in refs.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            captured.push(director_assets::capture_ref(dir, &state.db, s).await?);
        }
        req.reference_images = (!captured.is_empty()).then_some(captured);
    }
    Ok(())
}

/// Serve a job that reached a terminal state inside the sync window exactly as the
/// synchronous route did.
async fn inline_response(state: &AppState, caller: Option<String>, job: &Job) -> Response {
    let (payload, result) = detail(state, &job.id).await;
    let model_used = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("base")
        .to_string();
    let steps = payload_steps(&payload);

    match job.state {
        JobState::Done => {
            let body = flux_response(&payload, &result, job);
            log_flux(
                state,
                caller,
                &model_used,
                steps,
                job.admit_ms.unwrap_or(0) as u64,
                job.render_ms.unwrap_or(0) as u64,
                200,
                json!({
                    "images": body.images.len(),
                    "model_used": &model_used,
                    "generation_id": &body.generation_id,
                }),
            );
            Json(body).into_response()
        }
        JobState::Cancelled => {
            log_flux(
                state,
                caller,
                &model_used,
                steps,
                job.admit_ms.unwrap_or(0) as u64,
                job.render_ms.unwrap_or(0) as u64,
                499,
                json!({ "error": "cancelled" }),
            );
            err(StatusCode::from_u16(499).unwrap(), "render cancelled")
        }
        _ => {
            let msg = job.error.clone().unwrap_or_else(|| "render failed".into());
            warn!(job = %job.id, error = %msg, "flux render failed");
            log_flux(
                state,
                caller,
                &model_used,
                steps,
                job.admit_ms.unwrap_or(0) as u64,
                job.render_ms.unwrap_or(0) as u64,
                502,
                json!({ "error": &msg }),
            );
            err(StatusCode::BAD_GATEWAY, &format!("flux2-server: {msg}"))
        }
    }
}

/// `GET /api/v1/images/jobs/{id}` — poll a render.
async fn job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such FLUX.2 job");
    };
    if row.kind != JobKind::Flux2 {
        return err(StatusCode::NOT_FOUND, "that job is not a FLUX.2 render");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/images/active` — the render currently in flight, if any. Lets a
/// page that was reloaded find the job it lost without having kept the id.
async fn active(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match live_job(&state, JobKind::Flux2).await {
        Some(row) => Json(json!({ "job": view(&state, &row).await })).into_response(),
        None => Json(json!({ "job": Value::Null })).into_response(),
    }
}

/// Render a queue row in the shape this engine's page speaks: the terminal answer
/// is byte-for-byte the fields `/generate` returns, plus the queue's own state.
pub(crate) async fn view(state: &AppState, row: &Job) -> Value {
    let (payload, result) = detail(state, &row.id).await;
    let body = flux_response(&payload, &result, row);
    json!({
        "id": row.id,
        "state": job_state(row.state),
        "server_job_id": row.server_job_id,
        "progress": {
            "phase": row.progress.phase,
            "progress": row.progress.progress,
            "images_done": row.units_done,
            "image_count": row.units_total,
        },
        "images": body.images,
        "generation_id": body.generation_id,
        "model_used": body.model_used,
        "width": body.width,
        "height": body.height,
        "steps": body.steps,
        "cfg": body.cfg,
        "background_removal_error": body.background_removal_error,
        "background_removal_backend": body.background_removal_backend,
        "background_removal_seconds": body.background_removal_seconds,
        "warnings": row.warnings,
        "error": row.error,
        "queued_ms": row.queued_ms,
        "admit_ms": row.admit_ms,
        "duration_ms": row.render_ms,
        "blocked_reason": row.blocked_reason,
        "eta_start": row.eta_start,
    })
}

/// `collecting` has no equivalent in these pages' vocabulary, and the card is free
/// while the images are still being saved — so it reads as rendering.
pub(crate) fn job_state(state: JobState) -> &'static str {
    match state {
        JobState::Queued | JobState::Held => "queued",
        JobState::Running | JobState::Collecting => "rendering",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

/// The payload and the result of one row, both defaulted to `null` rather than
/// failing: a row that has not finished has no result, which is not an error.
pub(crate) async fn detail(state: &AppState, id: &str) -> (Value, Value) {
    match queue_store::detail(&state.db, id).await {
        Ok(Some((payload, _, result))) => (payload, result.unwrap_or(Value::Null)),
        _ => (Value::Null, Value::Null),
    }
}

/// The live or pending row for one engine, if there is one.
pub(crate) async fn live_job(state: &AppState, kind: JobKind) -> Option<Job> {
    let live = queue_store::live(&state.db).await.unwrap_or_default();
    let pending = queue_store::pending(&state.db).await.unwrap_or_default();
    live.into_iter().chain(pending).find(|j| j.kind == kind)
}

/// The images a finished row delivered.
///
/// 🔴 The result JSONB carries the gallery id and the FILE SLOTS, not URLs: the
/// slot pair `(image index, cutout index)` is what keeps `images[i].url` pointing
/// at image i once a cutout has shifted every position after it. Everything else
/// is read back from the payload, which is the request that ran.
pub(crate) fn flux_response(payload: &Value, result: &Value, row: &Job) -> GenerateResponse {
    let generation_id = result
        .get("generation_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let slots: Vec<(usize, Option<usize>)> = result
        .get("slots")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    // sdcpp echoes no per-image seed, so it is the request seed plus the batch
    // index — the same convention the music routes use.
    let base_seed = payload.get("seed").and_then(Value::as_i64).unwrap_or(-1);
    let seeds: Vec<i64> = (0..slots.len())
        .map(|i| {
            if base_seed >= 0 {
                base_seed + i as i64
            } else {
                -1
            }
        })
        .collect();

    GenerateResponse {
        images: images_from_slots(&generation_id, &slots, &seeds),
        model_used: payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("base")
            .to_string(),
        width: payload.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
        height: payload.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
        steps: payload_steps(payload),
        cfg: payload
            .pointer("/sample_params/guidance/txt_cfg")
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as f32,
        render_ms: row.render_ms.unwrap_or(0) as u64,
        gate_wait_ms: row.admit_ms.unwrap_or(0) as u64,
        background_removal_error: bg_field(result, "background_removal_error"),
        background_removal_backend: bg_field(result, "background_removal_backend"),
        background_removal_seconds: result
            .get("background_removal_seconds")
            .and_then(Value::as_f64),
        generation_id,
    }
}

fn bg_field(result: &Value, key: &str) -> Option<String> {
    result.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The queue row's `result` for one finished image render — the other half of
/// [`flux_response`] and of `krea2`'s equivalent, and the only record of what a
/// render produced once the process that ran it is gone.
///
/// 🔴 SLOTS, not URLs. The pair `(image index, cutout index)` is what keeps
/// `images[i].url` pointing at image i after a cutout has shifted every file
/// position behind it; everything else is read back from the payload, which is
/// the request that ran.
pub(crate) fn gallery_result(
    generation_id: &str,
    slots: &[(usize, Option<usize>)],
    bg: &BgRemovalOutcome,
) -> Value {
    json!({
        "generation_id": generation_id,
        "slots": slots,
        "background_removal_error": bg.error,
        "background_removal_backend": bg.backend,
        "background_removal_seconds": bg.elapsed_seconds,
    })
}

/// FLUX.2 nests its step count under `sample_params`.
fn payload_steps(payload: &Value) -> u32 {
    payload
        .pointer("/sample_params/sample_steps")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

/// The engine's images as a checked list. `Err` is the 502 message: an engine reply this route
/// cannot turn into files is a bad gateway, not a silently short response. Shared with
/// `routes::krea2` — the two engines have their own `RenderedImage`, so this takes the payloads,
/// not the structs.
///
/// It used to decode base64 here; the clients hand over bytes now (B5), so what is left is the
/// two checks that were always the point — nothing empty, and nothing missing.
pub(crate) fn engine_images(images: impl IntoIterator<Item = Bytes>) -> Result<Vec<Bytes>, String> {
    let collected: Vec<Bytes> = images.into_iter().collect();
    if collected.is_empty() {
        return Err("returned no images".to_string());
    }
    if let Some(index) = collected.iter().position(Bytes::is_empty) {
        return Err(format!("returned an empty image at index {index}"));
    }
    Ok(collected)
}

/// Interleave renders and their cutouts into the gallery's flat file list, and
/// report where each landed: `(image index, cutout index)` per render. The pair
/// is what turns a file position into a URL without anyone re-deriving the
/// interleaving. Shared with `routes::krea2`.
pub(crate) fn gallery_files(
    rendered: &[Bytes],
    cutouts: &[Bytes],
    img_ext: &str,
    img_mime: &str,
) -> (Vec<OutputBytes>, Vec<(usize, Option<usize>)>) {
    let mut files: Vec<OutputBytes> = Vec::with_capacity(rendered.len() + cutouts.len());
    let mut slots: Vec<(usize, Option<usize>)> = Vec::with_capacity(rendered.len());
    for (i, bytes) in rendered.iter().enumerate() {
        files.push(OutputBytes {
            ext: img_ext.to_string(),
            mime: img_mime.to_string(),
            kind: "image".to_string(),
            bytes: bytes.clone(),
        });
        let image_at = files.len() - 1;
        let cutout_at = cutouts.get(i).map(|cut| {
            files.push(OutputBytes {
                ext: "png".to_string(),
                mime: "image/png".to_string(),
                kind: "image".to_string(),
                bytes: cut.clone(),
            });
            files.len() - 1
        });
        slots.push((image_at, cutout_at));
    }
    (files, slots)
}

/// Turn persisted file positions into the response's gallery URLs. Shared with
/// `routes::krea2`.
pub(crate) fn images_from_slots(
    generation_id: &str,
    slots: &[(usize, Option<usize>)],
    seeds: &[i64],
) -> Vec<GeneratedImage> {
    slots
        .iter()
        .enumerate()
        .map(|(i, (image_at, cutout_at))| GeneratedImage {
            url: generation_file_url(generation_id, *image_at),
            seed: seeds.get(i).copied().unwrap_or(-1),
            cutout_url: cutout_at.map(|at| generation_file_url(generation_id, at)),
        })
        .collect()
}

/// Pull a string list out of an sdcpp `/sdcpp/v1/capabilities` body. Shared with
/// `routes::krea2` (same engine, same body).
///
/// The engine builds `samplers`/`schedulers`/`supported_modes` as flat arrays of
/// `const char*`, so anything else is drift — and the failure mode is an empty
/// dropdown with no error, hence per-entry salvage plus a log instead of a
/// whole-list `unwrap_or_default()`.
pub(crate) fn caps_str_list(caps: Option<&Value>, key: &str) -> Vec<String> {
    let Some(v) = caps.and_then(|c| c.get(key)) else {
        return Vec::new();
    };
    let Some(arr) = v.as_array() else {
        warn!(field = key, "engine capabilities field is not an array");
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| match e.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                warn!(field = key, "engine capabilities entry is not a string");
                None
            }
        })
        .collect()
}

/// Map sdcpp's reported output format to a file extension + mime. Shared with
/// `routes::krea2` (same engine family, same `output_format` vocabulary).
pub(crate) fn image_ext_mime(format: Option<&str>) -> (&'static str, &'static str) {
    match format.unwrap_or("png").to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => ("jpg", "image/jpeg"),
        "webp" => ("webp", "image/webp"),
        _ => ("png", "image/png"),
    }
}

/// Batch-pass every rendered image through the matting service and hand back one
/// cutout per input, in order. Any shortfall — a short result set, an
/// undecodable cutout — drops the whole set and reports the failure instead:
/// this step is non-fatal (the render already succeeded), but a partial set
/// would pair a cutout with the wrong render, so it is all or nothing.
///
/// Engine-agnostic (it only ever sees decoded image bytes), so `routes::krea2`
/// reuses it verbatim for its own chained background removal.
pub(crate) async fn run_background_removal(
    state: &AppState,
    bg: &BackgroundRemoval,
    images: &[Bytes],
) -> BgRemovalOutcome {
    if images.is_empty() {
        return BgRemovalOutcome::default();
    }
    if !state.matting.is_configured() {
        return BgRemovalOutcome::failed("background-removal service not configured".to_string());
    }

    let batch: Vec<(String, Bytes)> = images
        .iter()
        .enumerate()
        .map(|(i, bytes)| (format!("image-{i}.png"), bytes.clone()))
        .collect();

    let r = match state
        .matting
        .remove_batch(&batch, &bg_to_remove_options(bg))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "background removal after render failed");
            return BgRemovalOutcome::failed(format!("background removal failed: {e}"));
        }
    };
    if r.results.len() != images.len() {
        return BgRemovalOutcome::failed(format!(
            "background removal returned {} cutouts for {} images",
            r.results.len(),
            images.len()
        ));
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let cutouts: Result<Vec<Bytes>, _> = r
        .results
        .iter()
        .map(|res| b64.decode(&res.png_base64).map(Bytes::from))
        .collect();
    match cutouts {
        Ok(cutouts) => BgRemovalOutcome {
            cutouts,
            error: None,
            backend: r.backend,
            elapsed_seconds: r.elapsed_seconds,
        },
        Err(e) => BgRemovalOutcome::failed(format!(
            "background removal returned an undecodable cutout: {e}"
        )),
    }
}

/// `POST /api/v1/images/cancel` — cancel the render on the card, or the next one queued.
async fn cancel_current(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(
        &state,
        JobKind::Flux2,
        "no FLUX.2 render is queued or running",
    )
    .await
}

/// `GET /api/v1/images/props` — UI bootstrap: capabilities + health.
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.flux2.is_configured() {
        return Json(unconfigured_props()).into_response();
    }
    let healthy = state.flux2.healthy().await;
    let caps = state.flux2.capabilities().await.ok();
    let str_list = |key: &str| caps_str_list(caps.as_ref(), key);
    let limits = caps
        .as_ref()
        .and_then(|c| c.get("limits").cloned())
        .unwrap_or_else(|| json!({}));
    Json(ImageProps {
        healthy,
        samplers: str_list("samplers"),
        schedulers: str_list("schedulers"),
        limits,
        supported_modes: str_list("supported_modes"),
        // koblem's smart-model variant defaults — what the UI should prefill
        // per variant before the caller overrides anything.
        variants: variant_defaults(),
        default_width: Some(DEFAULT_WIDTH),
        default_height: Some(DEFAULT_HEIGHT),
        max_dim: Some(MAX_DIM),
        min_dim: Some(MIN_DIM),
        ..Default::default()
    })
    .into_response()
}

/// The body served when no engine URL is set — every engine-reported field falls
/// to its `Default`, so only the koblem-side prefills survive.
pub(crate) fn unconfigured_props() -> ImageProps {
    ImageProps {
        healthy: false,
        limits: json!({}),
        variants: variant_defaults(),
        ..Default::default()
    }
}

/// The smart-model variant defaults the frontend mirrors.
fn variant_defaults() -> Value {
    json!({
        "base": { "default_steps": BASE_STEPS, "default_cfg": BASE_CFG, "default_sampler": null },
        "edit": { "default_steps": EDIT_STEPS, "default_cfg": EDIT_CFG, "default_sampler": EDIT_SAMPLER },
    })
}

// ── prompt-assist (vision-capable) ────────────────────────────────────────

/// FLUX.2 prompt-engineering system prompt. Encodes Black Forest Labs' documented FLUX.2 best
/// practice (docs.bfl.ml/guides/prompting_guide_flux2) tuned for the FLUX.2-Klein engine this
/// service runs: detailed NATURAL-LANGUAGE sentences (Klein reads a Qwen text encoder) — never
/// keyword tags and never JSON (JSON structured prompting is a [dev]/[flex] feature, not Klein).
/// FLUX.2 does NOT support negative prompts, so the assist only ever writes an affirmative prompt.
const SUGGEST_SYSTEM_PROMPT: &str = "You are a prompt engineer for FLUX.2, a \
text-to-image diffusion model. Turn the user's brief into ONE excellent image prompt that follows \
FLUX.2 best practice:\n\
- Write it as full natural-language sentences (this model reads a Qwen text encoder, not tags) — \
never comma-separated keyword tags and never JSON. Follow the order FLUX.2 weights most heavily — \
word order matters, so lead with what matters most: SUBJECT first (the main focus, with concrete \
attributes: who/what, appearance, clothing, materials), then the ACTION or pose, then the STYLE or \
medium (e.g. \"photorealistic editorial photograph\", \"flat vector illustration\", \"oil \
painting\", \"3D render\", plus an optional artist or movement reference), then the CONTEXT — \
setting, lighting direction, colour palette, mood, composition and any camera or lens.\n\
- Be specific and multi-attribute: name the colours, textures, technique, lighting and composition.\n\
- Keep it detailed but focused — roughly 40-100 words, every word earning its place. Do not pad \
past what the scene needs; overlong prompts confuse the model.\n\
- Use AFFIRMATIVE phrasing only. FLUX.2 does NOT support negative prompts, so never state what you \
do not want — describe the positive instead (e.g. \"sharp focus\" not \"no blur\"; \"the subject \
centered on a flat, solid cream-white backdrop\" not \"no background\").\n\
- Output ONLY the prompt text — no markdown, headings, labels, commentary or preamble.";

/// `POST /api/v1/images/suggest-prompt` — ask koblem's own VL model to write a
/// FLUX.2-good prompt from a brief, optionally matching reference images.
/// Ungated GPU-wise (the LLM is the always-resident model).
async fn suggest_prompt(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<SuggestPromptRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let brief = req.brief.trim();
    let refine = crate::routes::refine::Refine::from_opts(
        req.refine_source.as_deref(),
        req.refine_instruction.as_deref(),
    );
    if brief.is_empty() && refine.is_none() {
        return err(StatusCode::BAD_REQUEST, "brief is required");
    }

    let refs = req
        .reference_images
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>();

    // Inject the current date — LLMs reject post-cutoff dates; pin it like the
    // rest of koblem's prompts do.
    let today = today_iso();
    let user_text = if let Some(r) = &refine {
        // Improve an existing prompt under the same FLUX.2 rules; brief + refs
        // ride along as context so the revision keeps the intended subject/style.
        let mut context = String::new();
        if !brief.is_empty() {
            context.push_str(&format!("Brief / intent: {brief}"));
        }
        if !refs.is_empty() {
            if !context.is_empty() {
                context.push_str("\n\n");
            }
            context.push_str(
                "The attached reference image(s) show the subject and/or style to match.",
            );
        }
        format!(
            "CURRENT DATE: {today}\n\n{}",
            crate::routes::refine::refine_user_msg("image prompt", &context, r)
        )
    } else if refs.is_empty() {
        format!("CURRENT DATE: {today}\n\nWrite a FLUX.2 image prompt for this brief:\n\n{brief}")
    } else {
        format!(
            "CURRENT DATE: {today}\n\nThe attached reference image(s) show the subject or style I want. \
             Make an image like this — match the subject and/or style of the reference(s) — while \
             incorporating my brief. Write a FLUX.2 image prompt for it.\n\nBrief: {brief}"
        )
    };

    // Build a vision message: text + image_url parts (data URIs), exactly as the
    // chat route feeds the VL model.
    let user_content = if refs.is_empty() {
        json!(user_text)
    } else {
        let mut parts = Vec::with_capacity(refs.len() + 1);
        parts.push(json!({"type": "text", "text": user_text}));
        for b64 in &refs {
            parts.push(json!({
                "type": "image_url",
                "image_url": { "url": as_data_uri(b64) },
            }));
        }
        json!(parts)
    };

    let mut body = json!({
        "messages": [
            {"role": "system", "content": SUGGEST_SYSTEM_PROMPT},
            {"role": "user", "content": user_content},
        ],
        "max_tokens": 600,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_images", &mut body).await;

    let resp = match state.llama.chat_completion_json(&body).await {
        Ok((_permit, r)) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("suggest failed: {e}")),
    };
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let suggestion = parse_suggestion(content);
    if suggestion.prompt.trim().is_empty() {
        return err(StatusCode::BAD_GATEWAY, "model returned no prompt");
    }
    Json(suggestion).into_response()
}

/// Clean the assist reply into a bare FLUX.2 prompt. FLUX.2 does not support negative prompts, so
/// the assist never returns one — `negative_prompt` stays `None` (the UI's manual field is a
/// separate power-user escape hatch). Strips any stray code fences / quotes / label line.
fn parse_suggestion(content: &str) -> SuggestPromptResponse {
    SuggestPromptResponse {
        prompt: crate::routes::ltx_prompting::clean_prompt(content),
        negative_prompt: None,
    }
}

// ── sdcpp body building ───────────────────────────────────────────────────

/// Build the flux2 `img_gen` body from the resolved request. Maps koblem's flat
/// fields onto sdcpp's shape: width/height/seed/batch_count flat; steps + cfg +
/// sampler/scheduler nested under `sample_params`; images as base64 fields.
#[allow(clippy::too_many_arguments)]
fn build_img_gen_body(
    req: &GenerateRequest,
    model_used: &str,
    width: u32,
    height: u32,
    steps: u32,
    cfg: f32,
    sampler: Option<&str>,
) -> Value {
    let mut body = json!({
        "model": model_used,
        "prompt": req.prompt,
        "width": width,
        "height": height,
        "batch_count": req.batch.unwrap_or(1).max(1),
        "seed": req.seed.unwrap_or(-1),
    });
    let obj = body.as_object_mut().unwrap();

    if let Some(n) = req.negative_prompt.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("negative_prompt".into(), json!(n));
    }
    if let Some(s) = req.strength {
        obj.insert("strength".into(), json!(s));
    }
    if let Some(t) = req.vae_tiling {
        obj.insert("vae_tiling_params".into(), json!({ "enabled": t }));
    }

    // Reference inputs. sdcpp reads raw base64 strings from these JSON fields
    // (init_image / mask_image / ref_images).
    if let Some(init) = req.init_image.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("init_image".into(), json!(init));
    }
    if let Some(mask) = req.mask.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("mask_image".into(), json!(mask));
    }
    if let Some(refs) = req.reference_images.as_ref()
        && !refs.is_empty()
    {
        let cleaned: Vec<&String> = refs.iter().filter(|s| !s.trim().is_empty()).collect();
        if !cleaned.is_empty() {
            obj.insert("ref_images".into(), json!(cleaned));
        }
    }

    // steps + cfg + sampler/scheduler ride inside `sample_params`.
    let mut sample_params = serde_json::Map::new();
    sample_params.insert("sample_steps".into(), json!(steps));
    sample_params.insert("guidance".into(), json!({ "txt_cfg": cfg }));
    if let Some(s) = sampler.filter(|s| !s.is_empty()) {
        sample_params.insert("sample_method".into(), json!(s));
    }
    if let Some(sched) = req.scheduler.as_deref().filter(|s| !s.is_empty()) {
        sample_params.insert("scheduler".into(), json!(sched));
    }
    obj.insert("sample_params".into(), Value::Object(sample_params));

    body
}

// ── multipart parsing ─────────────────────────────────────────────────────

/// Parse a `/generate` multipart body: a `request` JSON part (without the image
/// b64s) + file parts `init_image`, `mask`, and repeated `reference_images`.
///
/// 🔴 Every file part is written into the asset store HERE and referenced by
/// `asset:<hash>` — see [`capture_inputs`]. The uploaded bytes have to outlive the
/// request that carried them, because the render may not start for hours.
async fn parse_generate_multipart(
    state: &AppState,
    mut mp: Multipart,
) -> Result<Option<GenerateRequest>, Response> {
    let mut request: Option<GenerateRequest> = None;
    let mut init_image: Option<String> = None;
    let mut mask: Option<String> = None;
    let mut reference_images: Vec<String> = Vec::new();

    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                match name.as_str() {
                    "request" => {
                        let t = field.text().await.map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("reading request field: {e}"),
                            )
                        })?;
                        // Director storyboard stills pass `reference_images` straight off a shot,
                        // so they arrive as `asset:<hash>` refs — which is already what the queue
                        // wants, and `capture_inputs` only checks the store still holds them.
                        request = Some(serde_json::from_str(&t).map_err(|e| {
                            err(StatusCode::BAD_REQUEST, &format!("bad request: {e}"))
                        })?);
                    }
                    "init_image" => init_image = Some(capture_part(state, field, &name).await?),
                    "mask" => mask = Some(capture_part(state, field, &name).await?),
                    "reference_images" => {
                        reference_images.push(capture_part(state, field, &name).await?);
                    }
                    _ => {
                        let _ = field.bytes().await;
                    }
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

    let Some(mut req) = request else {
        return Ok(None);
    };
    // Multipart file parts override / supply the b64 fields. Only overwrite when
    // the JSON didn't already carry them.
    if init_image.is_some() {
        req.init_image = init_image;
    }
    if mask.is_some() {
        req.mask = mask;
    }
    if !reference_images.is_empty() {
        match &mut req.reference_images {
            Some(existing) => existing.extend(reference_images),
            None => req.reference_images = Some(reference_images),
        }
    }
    Ok(Some(req))
}

/// One uploaded file part, straight into the asset store. Shared with
/// `routes::krea2`, whose `/generate` takes the same `reference_images` parts.
pub(crate) async fn capture_part(
    state: &AppState,
    field: axum::extract::multipart::Field<'_>,
    what: &str,
) -> Result<String, Response> {
    let bytes = field.bytes().await.map_err(|e| {
        err(
            StatusCode::BAD_REQUEST,
            &format!("reading {what} field: {e}"),
        )
    })?;
    director_assets::capture_bytes(state.director_assets_dir.as_ref(), &state.db, &bytes)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, &e))
}

// ── helpers ───────────────────────────────────────────────────────────────

/// Wrap a raw base64 payload as a data URI for the VL model. If the caller
/// already passed a `data:` URI we leave it alone; otherwise default the mime to
/// PNG (the VL model only needs a decodable image, the exact mime is advisory).
fn as_data_uri(b64: &str) -> String {
    if b64.starts_with("data:") {
        b64.to_string()
    } else {
        format!("data:image/png;base64,{b64}")
    }
}

pub(crate) fn today_iso() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        now.month() as u8,
        now.day()
    )
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

#[allow(clippy::too_many_arguments)]
fn log_flux(
    state: &AppState,
    caller: Option<String>,
    model_used: &str,
    steps: u32,
    gate_wait_ms: u64,
    duration_ms: u64,
    status: i32,
    mut response_meta: Value,
) {
    // §9.2: what this render actually put on the wire. See the same stamp in `routes/ltx_video.rs`.
    if let Some(obj) = response_meta.as_object_mut() {
        obj.insert("wire".to_string(), state.flux2.wire().as_meta());
    }
    log_async(
        state.db.clone(),
        CreateLog {
            service: "flux".into(),
            endpoint: "generate".into(),
            request_meta: json!({
                "model_used": model_used,
                "steps": steps,
            }),
            response_meta,
            sample_id: None,
            gate_wait_ms: gate_wait_ms as i64,
            duration_ms: duration_ms as i64,
            status,
            caller,
        },
    );
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::{caps_str_list, engine_images, gallery_files, images_from_slots, parse_suggestion};
    use bytes::Bytes;
    use serde_json::json;

    fn png(tag: &str) -> Bytes {
        Bytes::from(tag.to_string())
    }

    /// The renders and their cutouts share ONE flat file list, so a cutout shifts
    /// every url after it. The slot pairs are what keep `images[i].url` pointing
    /// at image i rather than at its predecessor's cutout.
    #[test]
    fn cutouts_interleave_without_shifting_the_render_urls() {
        let rendered = vec![png("a"), png("b")];
        let cutouts = vec![png("ca"), png("cb")];
        let (files, slots) = gallery_files(&rendered, &cutouts, "png", "image/png");
        assert_eq!(files.len(), 4);
        assert_eq!(slots, vec![(0, Some(1)), (2, Some(3))]);

        let images = images_from_slots("gid", &slots, &[7, 8]);
        assert_eq!(images[0].url, "/api/v1/generations/gid/file/0");
        assert_eq!(
            images[0].cutout_url.as_deref(),
            Some("/api/v1/generations/gid/file/1")
        );
        assert_eq!(images[1].url, "/api/v1/generations/gid/file/2");
        assert_eq!(
            images[1].cutout_url.as_deref(),
            Some("/api/v1/generations/gid/file/3")
        );
        assert_eq!(images[1].seed, 8);
    }

    /// No chained matting → renders are consecutive and no cutout url is claimed.
    #[test]
    fn without_cutouts_the_files_are_just_the_renders() {
        let rendered = vec![png("a"), png("b"), png("c")];
        let (files, slots) = gallery_files(&rendered, &[], "jpg", "image/jpeg");
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].ext, "jpg");
        assert_eq!(slots, vec![(0, None), (1, None), (2, None)]);
        let images = images_from_slots("gid", &slots, &[-1, -1, -1]);
        assert_eq!(images[2].url, "/api/v1/generations/gid/file/2");
        assert!(images.iter().all(|i| i.cutout_url.is_none()));
    }

    /// An engine reply the route cannot turn into files is a 502, never a short
    /// response: dropping entry 0 would move every url onto the wrong image.
    #[test]
    fn an_empty_or_short_engine_reply_is_an_error() {
        assert!(engine_images(std::iter::empty()).is_err());
        // A hole in the middle is the dangerous one — it is the case that used to shift URLs.
        let hole = engine_images([Bytes::from_static(b"AAA"), Bytes::new()]);
        assert!(
            hole.as_ref().is_err_and(|m| m.contains("index 1")),
            "{hole:?}"
        );
        let ok = engine_images([Bytes::from_static(b"AAA"), Bytes::from_static(b"BBB")])
            .expect("both present");
        assert_eq!(ok.len(), 2);
        assert_eq!(&ok[0][..], b"AAA");
        assert_eq!(&ok[1][..], b"BBB");
    }

    /// Pins the real `/sdcpp/v1/capabilities` shape: `samplers`, `schedulers` and
    /// `supported_modes` are flat `array<string>` (built from `sd_sample_method_name`
    /// / `sd_scheduler_name` / the `"img_gen"`,`"vid_gen"` literals), while `loras`
    /// and `upscalers` — the object-shaped fields — are read raw elsewhere and must
    /// never be fed through this coercion.
    #[test]
    fn caps_str_list_reads_the_engine_capabilities_shape() {
        let caps = json!({
            "samplers": ["euler", "euler_a", "dpm++2m"],
            "schedulers": ["discrete", "normal", "karras"],
            "supported_modes": ["img_gen", "vid_gen"],
            "loras": [{ "name": "krea2-identity-edit-v1_2", "path": "a.gguf" }],
            "limits": { "max_width": 4096 },
        });
        assert_eq!(
            caps_str_list(Some(&caps), "samplers"),
            vec!["euler", "euler_a", "dpm++2m"]
        );
        assert_eq!(
            caps_str_list(Some(&caps), "supported_modes"),
            vec!["img_gen", "vid_gen"]
        );
        assert!(caps_str_list(Some(&caps), "missing_key").is_empty());
        assert!(caps_str_list(None, "samplers").is_empty());
    }

    /// A shape drift must not wipe the list wholesale — an empty `samplers` is an
    /// empty dropdown with no error anywhere.
    #[test]
    fn caps_str_list_salvages_per_entry_on_shape_drift() {
        let caps = json!({
            "samplers": ["euler", { "name": "dpm++2m" }, null, "heun"],
            "schedulers": { "default": "karras" },
            "supported_modes": "img_gen",
        });
        assert_eq!(
            caps_str_list(Some(&caps), "samplers"),
            vec!["euler", "heun"]
        );
        assert!(caps_str_list(Some(&caps), "schedulers").is_empty());
        assert!(caps_str_list(Some(&caps), "supported_modes").is_empty());
    }

    // FLUX.2 does not support negative prompts: the assist returns a bare positive prompt only.
    #[test]
    fn suggestion_is_bare_prompt_no_negative() {
        let s = parse_suggestion("A black cat behind a watermelon slice, studio shot.");
        assert_eq!(
            s.prompt,
            "A black cat behind a watermelon slice, studio shot."
        );
        assert!(s.negative_prompt.is_none());
    }

    #[test]
    fn suggestion_strips_quotes_and_label() {
        let s = parse_suggestion("Prompt:\n\"a red plane banking between jungle trees\"");
        assert_eq!(s.prompt, "a red plane banking between jungle trees");
        assert!(s.negative_prompt.is_none());
    }

    // The UI seeds per-model step/cfg defaults from these fields, so the JSON key
    // names (default_steps / default_cfg) are a contract with the frontend +
    // ImageVariant client type — a rename here silently reverts the UI to its
    // hardcoded fallback. Base = full-step / cfg 4; edit = few-step / cfg 1.
    #[test]
    fn variant_defaults_expose_per_model_step_cfg() {
        let v = super::variant_defaults();
        assert_eq!(v["base"]["default_steps"], 50);
        assert_eq!(v["base"]["default_cfg"], 4.0);
        assert_eq!(v["edit"]["default_steps"], 4);
        assert_eq!(v["edit"]["default_cfg"], 1.0);
    }
}
