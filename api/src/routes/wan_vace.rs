//! Wan2.2-VACE video routes — T2V / I2V / continuation renders driven by the warm
//! wan-vace video-server, saved to the generations gallery. Auth-gated.
//!
//! 🔴 **`POST /generate` is an ENQUEUE.** It validates, captures whatever images the caller
//! attached, builds the engine chain and files a [`JobKind::WanVace`] row; `queue::adapters::
//! wan_vace` submits, polls and collects it whenever the card comes free. There is no background
//! task here and no GPU acquire — a direct route left alive beside the queue is two writers to one
//! engine.
//!
//! 🔴 **Capture at enqueue, submit at dispatch.** An enqueued render can sit for eight hours, so an
//! uploaded init frame or identity anchor is written into the content-addressed asset store on the
//! way in and the row carries an `asset:<hash>`. The browser that posted the multipart is long gone
//! by the time the adapter resolves it.
//!
//! The internal VACE tuning flags (skip-blocks / strength-tail / anchor-frames) are baked
//! server-side and never crossed here.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use koblem_client::types::{
    Job, JobKind, JobState, VideoJobListResponse, VideoJobState, VideoJobStatus, WanImageSource,
    WanPromptFromLlmRequest, WanPromptFromLlmResponse, WanScriptFromLlmRequest,
    WanScriptFromLlmResponse, WanVaceGenerateRequest, WanVaceMode, WanVaceProps,
};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::require_auth;
use crate::director_assets::{self, AssetJson};
use crate::repositories::gpu_jobs as queue_store;
use crate::routes::{engine_cancel, ltx_prompting, wan_prompting};

/// 256 MB body cap — a reference/init image well under the server's own limit.
const WAN_BODY_LIMIT: usize = 256 * 1024 * 1024;

/// How far back `GET /jobs` looks. The list is a reconnect aid, not history — the queue page is
/// where the whole record lives now.
const RECENT_JOBS: i64 = 50;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/wan-vace/generate", post(generate))
        .route("/api/v1/wan-vace/script-from-llm", post(script_from_llm))
        .route("/api/v1/wan-vace/prompt-from-llm", post(prompt_from_llm))
        .route("/api/v1/wan-vace/props", get(props))
        .route("/api/v1/wan-vace/cancel", post(cancel_current))
        // Async job model: submit returns 202 + {job_id}; the browser polls status and
        // re-fetches the finished clip, surviving disconnect/navigation/reconnect.
        .route("/api/v1/wan-vace/jobs", get(list_jobs))
        .route("/api/v1/wan-vace/jobs/{id}", get(job_status))
        .route("/api/v1/wan-vace/jobs/{id}/media", get(job_media))
        .layer(DefaultBodyLimit::max(WAN_BODY_LIMIT))
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

/// `GET /api/v1/wan-vace/props` — UI bootstrap (health + defaults).
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let healthy = state.wan_vace.is_configured() && state.wan_vace.healthy().await;
    let last_render_sec = state
        .wan_vace
        .health()
        .await
        .as_ref()
        .and_then(|h| h.get("last_render_sec"))
        .and_then(|v| v.as_f64());
    let p = WanVaceProps {
        healthy,
        default_frames: 65,
        default_fps: 24,
        default_resolution: "1280x720".to_string(),
        default_quant: "nvfp4".to_string(),
        last_render_sec,
    };
    Json(p).into_response()
}

/// `POST /api/v1/wan-vace/cancel` — cancel the render on the card, or the next one queued.
///
/// 🔴 Through the dispatcher, not straight at the engine's `/v1/admin/unload`.
async fn cancel_current(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(
        &state,
        JobKind::WanVace,
        "no wan-vace render is queued or running",
    )
    .await
}

/// `POST /api/v1/wan-vace/script-from-llm` — expand a brief (+ optional opening/anchor image)
/// into N per-segment continuation prompts via the shared LLM path.
///
/// The load-bearing part is the system prompt's CONTINUATION-IDENTITY guidance: Wan2.2-VACE
/// temporal extension drifts to a NEW subject the moment per-segment prompts diverge (a shot
/// like "the woman turns and smiles" reframes to a fresh subject), so every shot must describe
/// the SAME continuous subject — same person, clothing, colours, setting — simply continuing to
/// move. Kept terse on purpose: the local LLM follows one clear instruction better than a long
/// list, but the identity-continuity clause is mandatory.
async fn script_from_llm(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<WanScriptFromLlmRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let n = req.n_segments.clamp(1, 24);
    let secs = req.seconds_per_segment.unwrap_or(3.0).max(1.0);
    let image = req
        .image
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let brief = req.brief.trim();
    if brief.is_empty() && image.is_none() {
        return err(StatusCode::BAD_REQUEST, "brief or image is required");
    }

    // Detailed Wan 2.2 prompting (see routes/wan_prompting.rs): motion-first with aesthetic
    // control + the mandatory VACE identity-continuity clause. i2v reads the attached frame and
    // animates only what's visible; t2v works from the brief. Output is a strict JSON array so the
    // multi-sentence paragraphs survive parsing.
    let system = wan_prompting::script_system_prompt(n, Some(secs), image.is_some());

    let mut user = format!(
        "Break this into {n} shots that continue one unbroken action in order, keeping the same \
         subject throughout. Each shot lasts about {secs:.0}s."
    );
    if !brief.is_empty() {
        user.push_str(&format!("\nBrief: {brief}"));
    } else {
        user.push_str(
            "\nThere is no written brief — base every shot on the subject in the attached image.",
        );
    }

    // i2v/anchor: send the frame as an image_url content part, exactly as routes/images.rs feeds
    // the VL model. t2v sends plain text.
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
        // Detailed paragraphs are far longer than the old one-liners — budget generously.
        "max_tokens": (n * 300).max(1024),
        "temperature": 0.7,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_wan_vace", &mut body).await;
    let text = match state.llama.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("llm: {e}")),
    };
    let segments = ltx_prompting::parse_shots(&text, n as usize);
    Json(WanScriptFromLlmResponse { segments }).into_response()
}

/// `POST /api/v1/wan-vace/prompt-from-llm` — write ONE detailed Wan 2.2 shot prompt for a single
/// prompt field (the per-shot / per-beat "draft" buttons). Image-aware (init frame / identity
/// anchor ride as VL `image_url` parts) and mode-aware (i2v animates only what's visible;
/// continuation adds the identity-continuity clause).
async fn prompt_from_llm(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<WanPromptFromLlmRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let has_images = req.images.iter().any(|im| !im.image_b64.trim().is_empty());
    let idea = req.idea.trim();
    let prev = req
        .prev_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let next = req
        .next_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let rewrite = req
        .rewrite
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let instruction = req
        .instruction
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if idea.is_empty()
        && rewrite.is_none()
        && instruction.is_none()
        && !has_images
        && prev.is_none()
        && next.is_none()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "provide an idea, an image, or neighbouring shots to draft from",
        );
    }

    let system = wan_prompting::shot_system_prompt(req.mode, req.seconds, has_images);

    let mut user = if let Some(instr) = instruction {
        // Rework: apply a directed change. With an existing prompt, revise in place;
        // otherwise write fresh toward the instruction. Either way, only touch what
        // the change calls for.
        match rewrite.or((!idea.is_empty()).then_some(idea)) {
            Some(base) => format!(
                "Revise this Wan shot prompt to apply the requested change. Keep everything the \
                 change does NOT touch — the subject's identity and look, the setting, and \
                 continuity with the neighbouring shots. Change only what the instruction \
                 asks.\n\nChange to apply: {instr}\n\nExisting shot prompt:\n{base}"
            ),
            None => format!(
                "Write a full Wan shot prompt for this shot, following this direction: {instr}"
            ),
        }
    } else if let Some(rw) = rewrite {
        format!(
            "Rewrite this existing Wan shot prompt. Keep the same scene, subject, action and \
             intent, but vary the wording, framing and detail for a fresh take:\n{rw}"
        )
    } else if idea.is_empty() {
        String::from("Write a full Wan shot prompt for this shot.")
    } else {
        format!("Turn this idea into a full Wan shot prompt:\n{idea}")
    };
    if let Some(p) = prev {
        user.push_str(&format!("\n\nThe previous shot was: {p}"));
    }
    if let Some(nx) = next {
        user.push_str(&format!("\nThe next shot will be: {nx}"));
    }
    if has_images {
        let labels: Vec<(&str, Option<f64>)> = req
            .images
            .iter()
            .filter(|im| !im.image_b64.trim().is_empty())
            .map(|im| (im.role.as_str(), im.seconds))
            .collect();
        user.push_str(&format!(
            "\n\nAttached images: {}. Describe how the subject and camera move.",
            ltx_prompting::label_images(&labels)
        ));
    }

    let mut content: Vec<Value> = vec![json!({"type": "text", "text": user})];
    for im in req
        .images
        .iter()
        .filter(|im| !im.image_b64.trim().is_empty())
    {
        content.push(json!({
            "type": "image_url",
            "image_url": {"url": as_data_uri(im.image_b64.trim())},
        }));
    }

    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": content},
        ],
        "max_tokens": 512,
        "temperature": 0.7,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_wan_vace", &mut body).await;
    let text = match state.llama.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("llm: {e}")),
    };
    let prompt = ltx_prompting::clean_prompt(&text);
    Json(WanPromptFromLlmResponse { prompt }).into_response()
}

/// Wrap a bare base64 image as a data URI for the VL model (passes a `data:` prefix through).
fn as_data_uri(b64: &str) -> String {
    if b64.starts_with("data:") {
        b64.to_string()
    } else {
        format!("data:image/png;base64,{b64}")
    }
}

/// `POST /api/v1/wan-vace/generate` — validate, capture, enqueue. 202 + `{job_id}`.
///
/// 🔴 The 503 "busy or disabled — retry shortly" is gone. An unavailable engine is a
/// `blocked_reason` on the row, not a refusal: the job waits in the queue and starts when the card
/// comes free, which is the whole point of having a queue.
async fn generate(State(state): State<AppState>, headers: HeaderMap, req: Request) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.wan_vace.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "wan-vace service not configured",
        );
    }
    let (gen_req, init_part, ref_part) = match parse_request(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let prompts: Vec<String> = gen_req
        .segments
        .iter()
        .map(|s| s.prompt.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if prompts.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "at least one segment prompt is required",
        );
    }

    let init = match capture_image(&state, &gen_req.init_source, init_part).await {
        Ok(v) => v,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &format!("init image: {msg}")),
    };
    let reference = match capture_image(&state, &gen_req.reference_source, ref_part).await {
        Ok(v) => v,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &format!("reference image: {msg}")),
    };

    let mode = match gen_req.mode {
        WanVaceMode::T2v => "t2v",
        WanVaceMode::I2v => "i2v",
        WanVaceMode::Continuation => "continuation",
    };
    let mut chain = json!({
        "mode": mode,
        "segments": prompts.iter().map(|p| json!({"prompt": p})).collect::<Vec<_>>(),
        "n_segments": prompts.len(),
        "width": gen_req.width.unwrap_or(1280),
        "height": gen_req.height.unwrap_or(720),
        "fps": gen_req.fps.unwrap_or(24).max(1),
        "frames": gen_req.frames.unwrap_or(65),
        "seed": gen_req.seed.unwrap_or(-1),
        "quant": gen_req.quant.clone().unwrap_or_else(|| "nvfp4".to_string()),
        "output_format": "webm",
    });
    if let Some(n) = &gen_req.negative_prompt {
        chain["negative_prompt"] = json!(n);
    }
    if let Some(rid) = gen_req.resume_job_id.as_deref().filter(|s| !s.is_empty()) {
        chain["resume_job_id"] = json!(rid);
    }
    // 🔴 Refs, not bytes and not `part:` markers. The adapter reads these two as NAMED multipart
    // fields at dispatch; a marker's bytes live in one request's memory and would be gone by then.
    if let Some(r) = init {
        chain["init_image"] = json!(r);
    }
    if let Some(r) = reference {
        chain["ref_image"] = json!(r);
    }

    let who = match crate::routes::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let enqueued = crate::routes::queue::enqueue(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(&headers).request(JobKind::WanVace, chain),
    )
    .await;

    match enqueued {
        Ok(resp) => (StatusCode::ACCEPTED, Json(resp)).into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

/// `GET /api/v1/wan-vace/jobs/{id}` — current status of a submitted render, off the queue row.
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
    if row.kind != JobKind::WanVace {
        return err(StatusCode::NOT_FOUND, "that job is not a wan-vace render");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/wan-vace/jobs` — in-flight + recently-finished jobs, for reconnect.
async fn list_jobs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let rows = queue_store::recent(&state.db, RECENT_JOBS)
        .await
        .unwrap_or_default();
    let mut jobs: Vec<VideoJobStatus> = Vec::new();
    for row in rows.into_iter().filter(|j| j.kind == JobKind::WanVace) {
        jobs.push(view(&state, &row).await);
    }
    Json(VideoJobListResponse { jobs }).into_response()
}

/// `GET /api/v1/wan-vace/jobs/{id}/media` — stream the clip the adapter put on disk.
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
        None if staged => Some(format!("/api/v1/wan-vace/jobs/{}/media", row.id)),
        None => None,
    };

    VideoJobStatus {
        job_id: row.id.clone(),
        created_at: time::OffsetDateTime::parse(
            &row.created_at,
            &time::format_description::well_known::Rfc3339,
        )
        .ok(),
        server_job_id: row.server_job_id.clone(),
        resume_job_id: row.server_job_id.clone(),
        resumable: row.server_job_id.is_some()
            && matches!(status, VideoJobState::Failed | VideoJobState::Cancelled),
        segments: row.units_total.unwrap_or(1).max(0) as u32,
        scripts,
        gate_wait_ms: row.admit_ms.unwrap_or(0).max(0) as u64,
        render_ms: row.render_ms.unwrap_or(0).max(0) as u64,
        error: row.error.clone(),
        result_url,
        status,
        // The Wan engine never emits progressive shots.
        partials: Vec::new(),
        // Wan renders one clip per job with no bank, no resume and no concatenation step — there is
        // nothing its `/media` could be a fragment of.
        job_is_partial: false,
        // Numeric progress is a MiniMax-H3 capability (WanGP surfaces per-step callbacks); no
        // sd.cpp-fork engine has one, and reporting zeros would draw a queued render as finished.
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

type Parts = (WanVaceGenerateRequest, Option<Bytes>, Option<Bytes>);

/// Deserialize a submit body, REJECTING any field this request type does not have. The rejection is
/// what stops an LTX-shaped body 202ing here and rendering a t2v clip from an i2v ask — see
/// `crate::strict_json`.
///
/// 🔴 Refs are left alone. `asset:<hash>` is what the queue row must carry: the adapter stages them
/// into `part:` markers at dispatch, and doing it here would put bytes in a request that is about
/// to end and a marker naming them in a row that outlives it.
fn parse_generate_json(raw: &[u8]) -> Result<WanVaceGenerateRequest, String> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| format!("bad json: {e}"))?;
    crate::strict_json::from_value_strict(value, "wan-vace /generate")
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
        let mut gen_req: Option<WanVaceGenerateRequest> = None;
        let mut init_img = None;
        let mut ref_img = None;
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("multipart field: {e}")))?
        {
            match field.name().unwrap_or("") {
                "request" => {
                    let txt = field.text().await.unwrap_or_default();
                    gen_req = Some(
                        parse_generate_json(txt.as_bytes())
                            .map_err(|e| err(StatusCode::BAD_REQUEST, &e))?,
                    );
                }
                "init_image" => init_img = field.bytes().await.ok(),
                "ref_image" => ref_img = field.bytes().await.ok(),
                _ => {}
            }
        }
        let gen_req =
            gen_req.ok_or_else(|| err(StatusCode::BAD_REQUEST, "missing 'request' part"))?;
        Ok((gen_req, init_img, ref_img))
    } else {
        let bytes = axum::body::to_bytes(req.into_body(), WAN_BODY_LIMIT)
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("body: {e}")))?;
        let gen_req = parse_generate_json(&bytes).map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
        Ok((gen_req, None, None))
    }
}

// ── input capture ─────────────────────────────────────────────────────────

/// The `asset:<hash>` ref for whatever this slot points at, or `None` when the slot is empty.
///
/// 🔴 THE CAPTURE. An enqueued render can sit for eight hours, so neither an upload's multipart
/// part nor a remote URL can be deferred to dispatch: the part dies with this request and the URL
/// is somebody else's server. Both are read here and written into the content-addressed store,
/// which de-duplicates a re-used reference for free.
async fn capture_image(
    state: &AppState,
    source: &WanImageSource,
    part: Option<Bytes>,
) -> Result<Option<String>, String> {
    let bytes = match source {
        WanImageSource::None => return Ok(None),
        WanImageSource::Upload => {
            part.ok_or_else(|| "source=upload needs the matching image part".to_string())?
        }
        WanImageSource::Url { url } => {
            let resp = reqwest::get(url)
                .await
                .map_err(|e| format!("fetch image: {e}"))?;
            resp.bytes().await.map_err(|e| format!("read image: {e}"))?
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
    use crate::strict_json::test_bodies;

    /// The shipped bug: an LTX-shaped body parsed cleanly as [`WanVaceGenerateRequest`], every
    /// unmatched field was dropped, and the submit 202'd — an i2v ask came back as a bare t2v
    /// clip after a full render. It must now fail at the parse, naming the field.
    #[test]
    fn a_foreign_engine_body_is_rejected_and_names_the_field() {
        let ok = super::parse_generate_json(&test_bodies::wan())
            .expect("wan's own body must still parse");
        assert!(matches!(ok.init_source, super::WanImageSource::Upload));

        let msg = super::parse_generate_json(&test_bodies::ltx())
            .expect_err("an LTX body must not render here");
        assert!(msg.contains("wan-vace /generate"), "{msg}");
        assert!(
            msg.contains("unknown field `image_source`, expected `init_source`"),
            "{msg}"
        );
    }
}
