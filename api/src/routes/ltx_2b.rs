//! LTX-Video 0.9.8 2B routes — the fast, video-only "fun toy". A director script of
//! per-segment prompts rendered as one resident-DiT chain and stitched into a single clip.
//! Auth-gated.
//!
//! This is the SAME director request flow as `routes/ltx_video.rs` (the LTX-2.3 service),
//! pointed at a SEPARATE ltx-2b container running the identical ltx-video-server binary +
//! `/ltx/v1/generate` async API. It is deliberately STRIPPED of everything the 2B doesn't do:
//!   - NO model/variant selection (single model — never sends `chain["model"]`, never swaps).
//!   - NO hires/upscaler pass.
//!   - NO relip / lip-dub.
//!   - NO audio-vae lip-sync (the 2B is video-only; the user muxes audio externally).
//!
//! What it KEEPS from the director flow: per-segment prompts, per-shot scene images (i2v),
//! keyframes (frame-pinned images), multi-scene cuts, negative_prompt, scheduler,
//! cont_latent_frames, per-request frames/width/height/fps/steps/seed.
//!
//! Pipeline:
//!   1. Capture the optional reference portrait + per-shot scene images into the asset store.
//!   2. Build the engine chain and enqueue it as a [`JobKind::Ltx2b`] row.
//!   3. `queue::adapters::ltx_2b` submits, polls and files the result whenever the card is free.
//!
//! 🔴 **`POST /generate` is an ENQUEUE.** There is no background task here and no GPU acquire — a
//! direct route left alive beside the queue is two writers to one engine. And the capture is on the
//! way IN: an enqueued render can sit for eight hours, so an uploaded opener or per-shot still is
//! written into the content-addressed store and the row carries an `asset:<hash>`.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use koblem_client::types::{
    Job, JobKind, JobState, LtxImageSource, LtxKeyframe, LtxScriptFromLlmRequest,
    LtxScriptFromLlmResponse, LtxVideoGenerateRequest, LtxVideoProps, VideoJobListResponse,
    VideoJobState, VideoJobStatus,
};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::require_auth;
use crate::director_assets::{self, AssetJson};
use crate::repositories::gpu_jobs as queue_store;
use crate::routes::engine_cancel;

/// 256 MB body cap — a 4K portrait + per-shot scene images, well under the server's own limit.
const LTX_BODY_LIMIT: usize = 256 * 1024 * 1024;

/// How far back `GET /jobs` looks. The list is a reconnect aid, not history — the queue page is
/// where the whole record lives now.
const RECENT_JOBS: i64 = 50;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/ltx-2b/generate", post(generate))
        .route("/api/v1/ltx-2b/script-from-llm", post(script_from_llm))
        .route("/api/v1/ltx-2b/props", get(props))
        .route("/api/v1/ltx-2b/cancel", post(cancel_current))
        // Async job model: submit returns 202 + {job_id}; the browser polls status and
        // re-fetches the finished clip, surviving disconnect/navigation/reconnect.
        .route("/api/v1/ltx-2b/jobs", get(list_jobs))
        .route("/api/v1/ltx-2b/jobs/{id}", get(job_status))
        .route("/api/v1/ltx-2b/jobs/{id}/media", get(job_media))
        .layer(DefaultBodyLimit::max(LTX_BODY_LIMIT))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
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

/// `GET /api/v1/ltx-2b/props` — UI bootstrap (health + defaults). The 2B is a single model,
/// so `variants` is always empty (no model-selector dropdown).
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let healthy = state.ltx_2b.is_configured() && state.ltx_2b.healthy().await;
    let last_render_sec = state
        .ltx_2b
        .health()
        .await
        .as_ref()
        .and_then(|h| h.get("last_render_sec"))
        .and_then(|v| v.as_f64());
    let p = LtxVideoProps {
        healthy,
        default_segment_frames: 97,
        default_cont_latent_frames: 3,
        default_steps: 8,
        default_fps: 24,
        default_resolution: "1216x704".to_string(),
        last_render_sec,
        variants: Vec::new(),
        // The 2B toy runs a single fixed DiT with no --lora-model-dir, so it offers none.
        loras: Vec::new(),
        // No latent bank on this binary, and no capabilities block that could say so — absent means
        // "no live fact", which is the truth here rather than a claim that it is disabled. Prompt
        // Relay is the same: it is a ComfyUI custom node, and this engine is not ComfyUI.
        bank_enabled: None,
        beats_enabled: None,
    };
    Json(p).into_response()
}

/// `POST /api/v1/ltx-2b/cancel` — cancel the render on the card, or the next one queued.
///
/// 🔴 Through the dispatcher, not straight at the engine's `/v1/admin/unload`.
async fn cancel_current(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(
        &state,
        JobKind::Ltx2b,
        "no ltx-2b render is queued or running",
    )
    .await
}

/// `POST /api/v1/ltx-2b/script-from-llm` — expand a brief into N director prompts. The 2B is
/// video-only, so the audio clause always tells the model NOT to describe sound.
async fn script_from_llm(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<LtxScriptFromLlmRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let n = req.n_segments.clamp(1, 24);
    let secs = req.seconds_per_segment.unwrap_or(4.0).max(1.0);
    let image = req
        .image
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let brief = req.brief.trim();
    if brief.is_empty() && image.is_none() {
        return err(StatusCode::BAD_REQUEST, "brief or image is required");
    }

    let base = if image.is_some() {
        "You write shots for an AI video generator. The attached image is the opening frame — \
         study its subject, setting, lighting and composition. Write shots that begin from that \
         exact frame and bring it to life, keeping the subject, style and setting consistent. \
         Each shot is one vivid sentence describing a single continuous action: how the subject \
         moves, where it is, and the camera. Describe motion, not keywords. One shot per line, \
         no numbering or commentary."
    } else {
        "You write shots for an AI video generator. Each shot is one vivid sentence describing a \
         single continuous action: the subject, how it moves, where it is, and the camera. \
         Describe motion, not keywords. One shot per line, no numbering or commentary."
    };
    // The 2B is video-only — never ask the model to weave in a soundscape.
    let system = format!(
        "{base} The audio is added separately — do not describe sound, music, or dialogue."
    );

    let mut user = format!(
        "Break this into {n} shots that tell the story in order. Each shot lasts about \
         {secs:.0}s — one action that fits that time."
    );
    if !brief.is_empty() {
        user.push_str(&format!("\nBrief: {brief}"));
    } else {
        user.push_str(
            "\nThere is no written brief — base the shots entirely on the attached opening frame.",
        );
    }

    let user_content = match image {
        Some(b64) => json!([
            {"type": "text", "text": user},
            {"type": "image_url", "image_url": {"url": as_data_uri(b64)}},
        ]),
        None => json!(user),
    };
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_content},
        ],
        "max_tokens": (n * 130).max(512),
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_ltx_2b", &mut body).await;
    let text = match state.llama.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("llm: {e}")),
    };
    let segments: Vec<String> = text
        .lines()
        .map(|l| strip_list_marker(l.trim()).to_string())
        .filter(|l| !l.is_empty() && !l.ends_with(':'))
        .take(n as usize)
        .collect();
    // `h3_generations` is the MiniMax-H3 structured-prompt draft and nothing else produces one —
    // 2B is an LTX engine and writes prose paragraphs.
    Json(LtxScriptFromLlmResponse {
        segments,
        h3_generations: Vec::new(),
    })
    .into_response()
}

/// Wrap a bare base64 image as a data URI for the VL model (passes a `data:` prefix through).
fn as_data_uri(b64: &str) -> String {
    if b64.starts_with("data:") {
        b64.to_string()
    } else {
        format!("data:image/png;base64,{b64}")
    }
}

/// Strip a single leading enumeration marker (`1.`, `2)`, `-`, `*`, `•`) without
/// eating a natural leading number like "3 dancers spin…".
fn strip_list_marker(s: &str) -> &str {
    let s = s.trim_start();
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0
        && let Some(rest) = s[digits..]
            .strip_prefix('.')
            .or_else(|| s[digits..].strip_prefix(')'))
    {
        return rest.trim_start();
    }
    s.strip_prefix(['-', '*', '•'])
        .map(|r| r.trim_start())
        .unwrap_or(s)
}

/// `POST /api/v1/ltx-2b/generate` — capture, build the chain, enqueue. 202 + `{job_id}`.
///
/// 🔴 The 503 "busy or disabled — retry shortly" is gone. An unavailable engine is a
/// `blocked_reason` on the row, not a refusal: the job waits in the queue and starts when the card
/// comes free, which is the whole point of having a queue.
async fn generate(State(state): State<AppState>, headers: HeaderMap, req: Request) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.ltx_2b.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "ltx-2b service not configured",
        );
    }
    // Parse JSON-or-multipart (request JSON + optional image + per-shot scene parts).
    let (gen_req, image, seg_images) = match parse_request(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Keep prompt + optional per-shot scene image + keyframes aligned through the empty-prompt
    // filter, so a shot's uploaded image (multipart `seg_image_<raw index>`) and inline keyframes
    // still map to the right rendered segment. Spec = (prompt, image source, upload bytes, keyframes).
    let seg_specs: Vec<(String, LtxImageSource, Option<Bytes>, Vec<LtxKeyframe>)> = gen_req
        .segments
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let p = s.prompt.trim().to_string();
            if p.is_empty() {
                return None;
            }
            let img = s.image.clone().unwrap_or_default();
            let bytes = seg_images.get(i).cloned().flatten();
            let kfs = s.keyframes.clone().unwrap_or_default();
            Some((p, img, bytes, kfs))
        })
        .collect();
    if seg_specs.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "at least one segment prompt is required",
        );
    }
    let prompts: Vec<String> = seg_specs.iter().map(|(p, _, _, _)| p.clone()).collect();
    let n_segments = prompts.len() as u32;

    let fps = gen_req.fps.unwrap_or(24).max(1);
    let segment_frames = gen_req.segment_frames.unwrap_or(97).max(9);
    let cont_latent_frames = gen_req.cont_latent_frames.unwrap_or(3).max(1);

    // 1. Reference image → an `asset:<hash>` ref (top-level seg-0 i2v opener). The adapter turns it
    //    into a binary part at dispatch; the store is what makes it survive the wait.
    let init_ref = match capture_image(&state, &gen_req.image_source, image).await {
        Ok(v) => v,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &format!("image: {msg}")),
    };

    // 1b. Per-shot scene images (i2v mid-chain). Each shot with an image starts a NEW scene from it
    //     (the engine resets video continuation at the cut). Shot 0's per-shot image, if any, is the
    //     opener and overrides the top-level `image_source`.
    let mut seg_refs: Vec<Option<String>> = Vec::with_capacity(seg_specs.len());
    for (idx, (_, img, bytes, _)) in seg_specs.iter().enumerate() {
        match capture_image(&state, img, bytes.clone()).await {
            Ok(r) => seg_refs.push(r),
            Err(msg) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!("shot {} image: {msg}", idx + 1),
                );
            }
        }
    }
    // Shot 0's own image wins as the opener; else the legacy top-level reference.
    let opener = seg_refs.first().cloned().flatten().or(init_ref);

    // 2. Build the server chain request. Per-shot chain objects: prompt + optional scene image /
    //    keyframes. A shot i>=1 with an image is a mid-chain scene cut (fresh i2v); shot 0's image
    //    is the opener, folded into the top-level `init_image` below. A shot's keyframes win over a
    //    plain scene image — a keyframe shot renders fresh via the engine keyframe branch. Each
    //    keyframe's `seconds` maps to a frame index within the shot, clamped to [0, segment_frames).
    let seg_objs: Vec<Value> = prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut o = json!({ "prompt": p });
            let kfs = seg_specs.get(i).map(|s| &s.3).filter(|k| !k.is_empty());
            if let Some(kfs) = kfs {
                let pins: Vec<Value> = kfs
                    .iter()
                    .map(|kf| {
                        let frame = ((kf.seconds * fps as f64).round() as i64)
                            .clamp(0, segment_frames as i64 - 1);
                        json!({ "image": kf.image_b64, "frame": frame })
                    })
                    .collect();
                o["keyframes"] = json!(pins);
            } else if i >= 1
                && let Some(Some(marker)) = seg_refs.get(i)
            {
                // Scene-cut image only when there are no keyframes for this shot.
                o["init_image"] = json!(marker);
            }
            o
        })
        .collect();
    // NO model/variant field: the 2B is a single model. NO hires/upscaler. NO audio.
    let mut chain = json!({
        "segments": seg_objs,
        "n_segments": n_segments,
        "width": gen_req.width.unwrap_or(1216),
        "height": gen_req.height.unwrap_or(704),
        "fps": fps,
        "segment_frames": segment_frames,
        "cont_latent_frames": cont_latent_frames,
        "steps": gen_req.steps.unwrap_or(8),
        "cfg_scale": gen_req.cfg_scale.unwrap_or(1.0),
        "sampling_method": gen_req.sampling_method.as_deref().unwrap_or("euler_a"),
        "seed": gen_req.seed.unwrap_or(-1),
        "output_format": "webm",
    });
    if let Some(s) = &gen_req.scheduler {
        chain["scheduler"] = json!(s);
    }
    if let Some(n) = &gen_req.negative_prompt {
        chain["negative_prompt"] = json!(n);
    }
    if let Some(marker) = &opener {
        chain["init_image"] = json!(marker);
    }
    // Image-pin hold strength (keyframes / scene images / opener). Only sent when the render pins
    // an image. Default = crisp (1.0); the UI can soften.
    let pins_any_image = opener.is_some()
        || seg_specs
            .iter()
            .any(|s| !s.3.is_empty() || !matches!(s.1, LtxImageSource::None));
    if pins_any_image {
        chain["strength"] = json!(gen_req.pin_strength.unwrap_or(1.0));
    }
    // Resume a prior render from its banked segments (the server skips done segments and continues
    // into the same artifact dir). Empty/absent = a fresh full render. Forwarded verbatim: it is a
    // caller-driven field, not a queue mechanism — see the adapter's `preempt`.
    if let Some(rid) = gen_req.resume_job_id.as_deref().filter(|s| !s.is_empty()) {
        chain["resume_job_id"] = json!(rid);
    }

    let who = match crate::routes::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let enqueued = crate::routes::queue::enqueue(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(&headers).request(JobKind::Ltx2b, chain),
    )
    .await;

    match enqueued {
        Ok(resp) => (StatusCode::ACCEPTED, Json(resp)).into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

/// `GET /api/v1/ltx-2b/jobs/{id}` — current status of a submitted render, off the queue row.
async fn job_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such job");
    };
    if row.kind != JobKind::Ltx2b {
        return err(StatusCode::NOT_FOUND, "that job is not an ltx-2b render");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/ltx-2b/jobs` — in-flight + recently-finished jobs, for reconnect.
async fn list_jobs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let rows = queue_store::recent(&state.db, RECENT_JOBS)
        .await
        .unwrap_or_default();
    let mut jobs: Vec<VideoJobStatus> = Vec::new();
    for row in rows.into_iter().filter(|j| j.kind == JobKind::Ltx2b) {
        jobs.push(view(&state, &row).await);
    }
    Json(VideoJobListResponse { jobs }).into_response()
}

/// `GET /api/v1/ltx-2b/jobs/{id}/media` — stream the clip the adapter put on disk.
///
/// ⚠️ Only while it is there. The adapter files the clip in the gallery and unlinks its staging
/// name the moment the gallery holds its own link, so this serves the render as it lands and the
/// gallery owns it afterwards — which is why `result_url` is published from the file's existence
/// rather than from the job's state.
async fn job_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    // serve_file honours Range requests so the <video> element can seek/scrub without the whole
    // clip being read into RAM per request.
    let path = clip_path(&state, &id);
    match crate::routes::byte_serve::serve_file(&headers, &path, "video/webm").await {
        Ok(resp) => resp,
        Err(e) => err(StatusCode::NOT_FOUND, &format!("clip file missing: {e}")),
    }
}

fn clip_path(state: &AppState, job_id: &str) -> std::path::PathBuf {
    state.queue_files.clip_path(job_id)
}

/// Render a queue row as the `VideoJobStatus` body the panel and the `kob` CLI already speak.
async fn view(state: &AppState, row: &Job) -> VideoJobStatus {
    let (payload, result) = queue_store::detail(&state.db, &row.id)
        .await
        .ok()
        .flatten()
        .map(|(p, _, r)| (p, r.unwrap_or(Value::Null)))
        .unwrap_or((Value::Null, Value::Null));
    let scripts: Vec<String> = payload
        .get("segments")
        .and_then(Value::as_array)
        .map(|segs| {
            segs.iter()
                .filter_map(|s| s.get("prompt").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let status = match row.state {
        JobState::Queued | JobState::Held => VideoJobState::Queued,
        // `collecting` is still rendering as far as this page is concerned: the card is free but
        // the clip is not saved yet, and this vocabulary has no word for that.
        JobState::Running | JobState::Collecting => VideoJobState::Rendering,
        JobState::Done => VideoJobState::Completed,
        JobState::Failed => VideoJobState::Failed,
        JobState::Cancelled => VideoJobState::Cancelled,
    };
    // 🔴 Two places the deliverable can be, and the STAGING one is the short-lived
    // half. The adapter streams the container to `clip_path` so the gallery can
    // hard-link rather than copy a 589 MB file, then unlinks that name — so past
    // `done` the staging file is gone and only the gallery has it. Reading the
    // file's existence alone reported `result_url: null` for every finished job.
    let staged = tokio::fs::try_exists(clip_path(state, &row.id))
        .await
        .unwrap_or(false);
    let result_url = match result.get("url").and_then(Value::as_str) {
        Some(url) => Some(url.to_string()),
        None if staged => Some(format!("/api/v1/ltx-2b/jobs/{}/media", row.id)),
        None => None,
    };
    // A resume submits the previous engine job id back; the 2B banks into the job dir it always
    // writes, so there is no directory a deployment could leave unset.
    let resumable = row.server_job_id.is_some()
        && matches!(status, VideoJobState::Failed | VideoJobState::Cancelled);

    VideoJobStatus {
        job_id: row.id.clone(),
        created_at: time::OffsetDateTime::parse(
            &row.created_at,
            &time::format_description::well_known::Rfc3339,
        )
        .ok(),
        server_job_id: row.server_job_id.clone(),
        resume_job_id: row.server_job_id.clone(),
        resumable,
        segments: row.units_total.unwrap_or(1).max(0) as u32,
        scripts,
        gate_wait_ms: row.admit_ms.unwrap_or(0).max(0) as u64,
        render_ms: row.render_ms.unwrap_or(0).max(0) as u64,
        error: row.error.clone(),
        result_url,
        status,
        // The 2B engine never emits progressive shots.
        partials: Vec::new(),
        // 🔴 A resumed render delivers only the WINDOW it rendered, so its `/media` is not the
        // timeline. `resume_job_id` on the payload is the only thing that says so here.
        job_is_partial: payload
            .get("resume_job_id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        // Numeric progress is a MiniMax-H3 capability; no sd.cpp-fork engine has one, and
        // reporting zeros would draw a queued render as finished.
        total_eta_seconds: None,
        progress: None,
        current_step: None,
        total_steps: None,
        phase: None,
        preview_seq: None,
        generation_index: None,
        generations_done: None,
    }
}

// Axum re-exports so the handler can take a raw Request (JSON or multipart).
use axum::extract::FromRequest;
use axum::extract::Request;

type Parts = (
    LtxVideoGenerateRequest,
    Option<Bytes>,
    // Per-shot scene images (i2v), indexed by RAW segment index (multipart `seg_image_<i>`).
    // Sparse: a slot is `Some` only when that shot uploaded an image. Empty on the JSON path.
    Vec<Option<Bytes>>,
);

/// Deserialize a submit body, REJECTING any field this request type does not have — see
/// `crate::strict_json`.
///
/// 🔴 Refs are left alone. `asset:<hash>` is what the queue row must carry: the adapter stages them
/// into `part:` markers at dispatch, and doing it here would put bytes in a request that is about
/// to end and a marker naming them in a row that outlives it.
fn parse_generate_json(raw: &[u8]) -> Result<LtxVideoGenerateRequest, String> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| format!("bad json: {e}"))?;
    crate::strict_json::from_value_strict(value, "ltx-2b /generate")
}

async fn parse_request(req: Request) -> Result<Parts, Response> {
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.starts_with("multipart/form-data") {
        let mut multipart = Multipart::from_request(req, &())
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")))?;
        let mut gen_req: Option<LtxVideoGenerateRequest> = None;
        let mut image = None;
        // Sparse per-shot scene images by raw segment index; grown on demand.
        let mut seg_images: Vec<Option<Bytes>> = Vec::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("multipart field: {e}")))?
        {
            let name = field.name().unwrap_or("").to_string();
            match name.as_str() {
                "request" => {
                    let txt = field.text().await.unwrap_or_default();
                    gen_req = Some(
                        parse_generate_json(txt.as_bytes())
                            .map_err(|e| err(StatusCode::BAD_REQUEST, &e))?,
                    );
                }
                "image" => image = field.bytes().await.ok(),
                // Per-shot scene image: `seg_image_<i>` (i = raw segment index).
                _ if name.starts_with("seg_image_") => {
                    if let Ok(i) = name["seg_image_".len()..].parse::<usize>()
                        && let Ok(b) = field.bytes().await
                    {
                        if seg_images.len() <= i {
                            seg_images.resize(i + 1, None);
                        }
                        seg_images[i] = Some(b);
                    }
                }
                _ => {}
            }
        }
        let gen_req =
            gen_req.ok_or_else(|| err(StatusCode::BAD_REQUEST, "missing 'request' part"))?;
        Ok((gen_req, image, seg_images))
    } else {
        let bytes = axum::body::to_bytes(req.into_body(), LTX_BODY_LIMIT)
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("body: {e}")))?;
        let gen_req = parse_generate_json(&bytes).map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
        Ok((gen_req, None, Vec::new()))
    }
}

// ── input capture ─────────────────────────────────────────────────────────

/// The `asset:<hash>` ref for whatever this slot points at, or `None` when the slot is empty.
///
/// 🔴 THE CAPTURE. An enqueued render can sit for eight hours, so neither an upload's multipart
/// part nor a remote URL can be deferred to dispatch: the part dies with this request and the URL
/// is somebody else's server. Both are read here and written into the content-addressed store,
/// which de-duplicates a still re-used across ten shots into one file.
async fn capture_image(
    state: &AppState,
    source: &LtxImageSource,
    part: Option<Bytes>,
) -> Result<Option<String>, String> {
    let bytes = match source {
        LtxImageSource::None => return Ok(None),
        LtxImageSource::Upload => {
            part.ok_or_else(|| "image_source=upload needs an `image` part".to_string())?
        }
        LtxImageSource::Url { url } => {
            let resp = reqwest::get(url)
                .await
                .map_err(|e| format!("fetch image: {e}"))?;
            resp.bytes().await.map_err(|e| format!("read image: {e}"))?
        }
        LtxImageSource::LibraryAssetId { .. } => {
            return Err(
                "library_asset_id not supported yet for ltx-2b — upload or url".to_string(),
            );
        }
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    director_assets::ref_for_bytes(&state.director_assets_dir, &state.db, &bytes)
        .await
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::strip_list_marker;
    use crate::strict_json::test_bodies;

    #[test]
    fn a_foreign_engine_body_is_rejected_and_names_the_field() {
        let ok = super::parse_generate_json(&test_bodies::ltx())
            .expect("ltx-2b's own body must still parse");
        assert_eq!(ok.segments.len(), 1);

        let msg = super::parse_generate_json(&test_bodies::wan())
            .expect_err("a wan body must not render here");
        assert!(msg.contains("ltx-2b /generate"), "{msg}");
        assert!(
            msg.contains("unknown field `init_source`, expected `image_source`"),
            "{msg}"
        );
    }

    /// 🔴 An `asset:<hash>` must survive the parse as a REF. The queue row outlives the request, so
    /// a `part:` marker written here would name bytes nothing holds by the time the adapter submits.
    #[test]
    fn asset_refs_survive_the_parse_untouched() {
        let hash = "a".repeat(64);
        let body = serde_json::json!({
            "segments": [{
                "prompt": "a dog runs",
                "keyframes": [{"seconds": 0.0, "image_b64": format!("asset:{hash}")}],
            }],
        });
        let parsed = super::parse_generate_json(&serde_json::to_vec(&body).unwrap())
            .expect("an ltx-2b body parses");
        let kfs = parsed.segments[0].keyframes.clone().expect("keyframes");
        assert_eq!(kfs[0].image_b64, format!("asset:{hash}"));
    }

    #[test]
    fn strips_enumeration_markers() {
        assert_eq!(strip_list_marker("1. A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("12) A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("- A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("* A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("• A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("  2.  A dog runs"), "A dog runs");
    }

    #[test]
    fn keeps_natural_leading_numbers() {
        assert_eq!(strip_list_marker("3 dancers spin"), "3 dancers spin");
        assert_eq!(strip_list_marker("A dog runs"), "A dog runs");
    }
}
