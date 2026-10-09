//! `Video → Upscale` — the LTX-2.5 restore tab.
//!
//! An mp4 goes in and a bigger mp4 comes out. That is the whole feature, and it is deliberately its
//! own section rather than a post-phase on a render: the upscaler is a fourth plugin on the ONE
//! comfy-api process, whose queue is single-slot, and an H3 chain is ONE prompt for its whole job.
//! An upscale submitted mid-render does not overlap it — it queues behind it. Director hands a take
//! over by deep-linking this tab (`/ltx-video?tab=upscale&...`), which keeps the render path
//! untouched.
//!
//! # 🔴 Large files: three hops, and none of them may buffer a whole video
//!
//! ```text
//!   browser ──multipart──▶ koblem          streamed to disk, 8 MiB at a time (`upload`)
//!   koblem  ──hard-link──▶ shared dir      same filesystem ⇒ free; a streamed copy otherwise
//!   koblem  ──JSON path──▶ comfy-api       NO BYTES CROSS HTTP AT ALL
//!   comfy-api ─stream───▶ ComfyUI/input    `core/staging.py`, 4 MiB chunks
//! ```
//!
//! The `part:` route on the far side exists and this module never uses it: it would buffer the
//! clip in koblem, again in comfy-api's multipart parser, and again in the probe's temp copy. What
//! makes the path route work is one shared bind mount at the SAME absolute path in both containers
//! (`UPSCALE_STAGING_HOST`, default `/var/lib/koblem/upscale-staging`). ⚠️ **If that mount is missing,
//! comfy-api answers 503 naming it** — that is the first thing to check when a large upscale dies
//! instantly and a small one works.
//!
//! # 🔴 Capture at enqueue, stage at dispatch
//!
//! `POST /generate` no longer renders anything: it plans the split and enqueues a
//! [`JobKind::Upscale`] row, and `queue::adapters::upscale` drives the segments hours later. So the
//! source has to be CAPTURED on the way in — the browser that posted the multipart is long gone by
//! dispatch — while the hand-over into the shared mount stays with the adapter, which links it for
//! the life of one run and unlinks it on every exit. Staging here instead would park five whole
//! videos in a directory both containers mount, overnight, routinely.
//!
//! An upload lands in the capture directory (`queue::files`) and its token names it; a Director
//! take is left where it already lives and only its path travels. Neither is copied a second time —
//! the capture root defaults to a subdirectory of the staging mount so the adapter's hand-over is
//! still a hard link rather than a whole-video copy.
//!
//! # Segments and seams
//!
//! ComfyUI holds video as fp32, so one minute of 1080p output is ~36 GB per live copy and the
//! graph holds about four. The engine refuses a window past its published RAM guard; this module
//! reads that guard and computes a split BEFORE enqueueing, so the caller sees the segment count
//! (and its seam count) from `/plan` rather than a 400 at 03:00.
//!
//! 🔴 **A seam is real.** each segment is its own refine, so the model can resolve
//! ambiguous texture differently on either side of a join. `SegmentPlan.seams` is published for
//! exactly that reason and the panel shows it. A single-segment upscale — the case to aim for — has
//! none.

use std::path::{Path as FsPath, PathBuf};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use koblem_client::types::{Job, JobKind, JobState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_jobs as queue_store;
//: The engine client, aliased because this module is ALSO called `upscale` — `routes::upscale`
//: is the HTTP surface and `crate::upscale` is the thing it talks to.
use crate::upscale as upscale_engine;
use crate::upscale::{ScaleMode, SourceGeometry, UpscaleRequest};

/// 🔴 THE UPLOAD CAP, AND IT IS NOT A BODY LIMIT ON A BUFFER. The multipart body is streamed to
/// disk field-by-chunk, so this bounds DISK and how long a stranger can make koblem write, never
/// RSS. 8 GB is a comfortably long 4K source; the staging mount has ~380 GB.
const MAX_UPLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Body limit for the JSON routes. Every one of them is a handful of numbers.
const JSON_BODY_LIMIT: usize = 64 * 1024;

/// The gallery bucket the adapter files a finished upscale under. Same string as
/// `queue::adapters::upscale::SERVICE_TAG` — the coupling is real: this route resolves the
/// deliverable by looking that bucket up.
const SERVICE_TAG: &str = "upscale";

/// How far back `/jobs/{id}/file` looks for the gallery row belonging to a job. The adapter stamps
/// `job_id` into the row's meta, so the match is exact; the window only bounds the scan.
const GALLERY_SCAN: i64 = 200;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/upscale/capabilities", get(capabilities))
        .route(
            "/api/v1/upscale/plan",
            post(plan).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route(
            "/api/v1/upscale/generate",
            post(generate).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route("/api/v1/upscale/jobs/{id}", get(job))
        .route("/api/v1/upscale/jobs/{id}/cancel", post(cancel))
        .route("/api/v1/upscale/jobs/{id}/file", get(serve_output))
        .route("/api/v1/upscale/jobs/{id}/segments/{n}", get(serve_segment))
        .route("/api/v1/upscale/active", get(active))
        .route(
            "/api/v1/upscale/upload",
            // 🔴 The limit is enforced by the streaming loop, not by this layer: `DefaultBodyLimit`
            // is a byte counter, and a `disable()` here is what lets the body arrive as chunks
            // rather than being collected. Removing the counter without the loop's own check would
            // make this an unbounded write.
            post(upload).layer(DefaultBodyLimit::disable()),
        )
}

// ── the wire ─────────────────────────────────────────────────────────────────

/// Where the source comes from. Both arms resolve to a path that outlives the queue wait.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpscaleSource {
    /// A file already captured by `POST /api/v1/upscale/upload`, named by its staging token.
    Upload { token: String },
    /// A Director take, addressed the way the rest of the Director API addresses one.
    ///
    /// 🔴 COORDINATES, NOT A FILENAME. `routes::ltx_director::get_take` resolves
    /// `(project, tier, shot, variant)` to a row and reads `row.file_name` from it; doing the same
    /// here means the on-disk name never reaches the browser and a caller cannot name a file this
    /// service did not write. `project_render_dir` does the traversal check, once, for both.
    DirectorTake {
        project_id: String,
        /// `proxy` | `final`.
        tier: String,
        shot: i32,
        variant: i32,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpscaleGenerateRequest {
    pub source: UpscaleSource,
    #[serde(default)]
    pub scale_mode: Option<ScaleMode>,
    #[serde(default)]
    pub multiplier: Option<f64>,
    #[serde(default)]
    pub shorter_size: Option<u32>,
    #[serde(default)]
    pub longer_size: Option<u32>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// Absent means the plugin's own default. See [`UpscaleRequest::noise`].
    #[serde(default)]
    pub noise: Option<f64>,
    #[serde(default)]
    pub detailer: bool,
    /// Absent means the plugin's own `lanczos`. See [`UpscaleRequest::scale_method`].
    #[serde(default)]
    pub scale_method: Option<String>,
    #[serde(default)]
    pub start_time: Option<f64>,
    #[serde(default)]
    pub duration: Option<f64>,
    /// Override the computed split. Normally absent — see [`upscale_engine::plan_segments`].
    #[serde(default)]
    pub segment_seconds: Option<f64>,
}

impl UpscaleGenerateRequest {
    fn to_engine(&self, source_path: PathBuf) -> UpscaleRequest {
        UpscaleRequest {
            source_path,
            scale_mode: self.scale_mode.unwrap_or(ScaleMode::Multiplier),
            multiplier: self
                .multiplier
                .unwrap_or(upscale_engine::DEFAULT_MULTIPLIER),
            shorter_size: self.shorter_size,
            longer_size: self.longer_size,
            width: self.width,
            height: self.height,
            noise: self.noise,
            detailer: self.detailer,
            scale_method: self.scale_method.clone(),
            start_time: self.start_time,
            duration: self.duration,
            segment_seconds: self.segment_seconds,
        }
    }
}

#[derive(Debug, Serialize)]
struct UploadAccepted {
    /// Opaque handle for `source: {kind: "upload", token}`. Not a path: a client must never be able
    /// to name an arbitrary file on this box.
    token: String,
    filename: String,
    bytes: u64,
    geometry: Option<ProbedGeometry>,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct ProbedGeometry {
    width: u32,
    height: u32,
    fps: f64,
    duration_seconds: f64,
    has_audio: bool,
}

impl From<ProbedGeometry> for SourceGeometry {
    fn from(p: ProbedGeometry) -> Self {
        Self {
            width: p.width,
            height: p.height,
            fps: p.fps,
            duration_seconds: p.duration_seconds,
        }
    }
}

// ── routes ───────────────────────────────────────────────────────────────────

/// `GET /api/v1/upscale/capabilities` — forwarded from the engine, unparsed.
///
/// The noise ladder with its measurements, the geometry rules, and the `ram_guard` block. ⚠️ The
/// panel does not read this — koblem computes the split and the panel reads the plan. It is here
/// for a client that wants to explain the knobs without submitting anything, and keeping a second
/// copy of any of it in koblem is how the two drift.
async fn capabilities(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.upscale.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the upscaler is not configured — set `upscale_server_url` in Settings",
        );
    }
    match state.upscale.capabilities().await {
        Ok(caps) => Json(caps).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("upscale: {e}")),
    }
}

/// `POST /api/v1/upscale/upload` — capture a source clip so it outlives the queue wait.
///
/// 🔴 STREAMED, NEVER COLLECTED. `DefaultBodyLimit::disable()` on this route is what makes the body
/// arrive as chunks; the size cap is enforced in the loop below and a file that exceeds it is
/// unlinked rather than left as a truncated video somebody later tries to upscale.
///
/// 🔴 The capture root, not the staging root. A capture whose job is cancelled — or never submitted
/// at all — is nobody's to unlink at run time, so `jobs::gpu_retention` sweeps this directory by
/// age against the live queue rows. That is only safe because nothing else writes here.
async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let dir = state.queue_files.captures_root();
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!(
                "the upscale capture directory {} is not writable ({e}). It defaults to a \
                 subdirectory of the mount koblem and comfy-api both see at the SAME absolute \
                 path — without it a source clip cannot be captured or handed over at all.",
                dir.display()
            ),
        );
    }

    let token = format!("up_{}", uuid_like());
    let mut written = 0_u64;
    let mut original = String::new();
    let mut dest: Option<PathBuf> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                cleanup(dest.as_deref()).await;
                return err(StatusCode::BAD_REQUEST, &format!("malformed upload: {e}"));
            }
        };
        if field.name() != Some("file") {
            continue;
        }
        original = field
            .file_name()
            .unwrap_or("source.mp4")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .collect();
        if original.is_empty() {
            original = "source.mp4".to_string();
        }
        let suffix = FsPath::new(&original)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4")
            .to_ascii_lowercase();
        let target = state.queue_files.capture_path(&token, &suffix);
        let mut file = match tokio::fs::File::create(&target).await {
            Ok(f) => f,
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("cannot open {} for writing: {e}", target.display()),
                );
            }
        };
        dest = Some(target.clone());

        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    written += chunk.len() as u64;
                    if written > MAX_UPLOAD_BYTES {
                        cleanup(dest.as_deref()).await;
                        return err(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            &format!(
                                "the source exceeds {} GiB. That is a disk and wall-clock bound, \
                                 not a memory one — nothing here buffers the file — but an upscale \
                                 of something this long would be days of GPU on a single-slot \
                                 queue. Trim it first.",
                                MAX_UPLOAD_BYTES / 1024 / 1024 / 1024
                            ),
                        );
                    }
                    if let Err(e) = file.write_all(&chunk).await {
                        cleanup(dest.as_deref()).await;
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("writing the upload failed after {written} bytes: {e}"),
                        );
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    cleanup(dest.as_deref()).await;
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("upload stream broke: {e}"),
                    );
                }
            }
        }
        if let Err(e) = file.flush().await {
            cleanup(dest.as_deref()).await;
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("flushing the upload failed: {e}"),
            );
        }
        break;
    }

    let Some(path) = dest else {
        return err(
            StatusCode::BAD_REQUEST,
            "no `file` part in the upload. Send it as multipart/form-data with a part named `file`.",
        );
    };
    if written == 0 {
        cleanup(Some(&path)).await;
        return err(StatusCode::BAD_REQUEST, "the uploaded file is empty");
    }

    // Probe now, not at submit: an unreadable container is the caller's problem and they should
    // hear about it while they still have the file open, not after choosing settings.
    let geometry = probe_geometry(&path).await.ok();
    if geometry.is_none() {
        cleanup(Some(&path)).await;
        return err(
            StatusCode::BAD_REQUEST,
            "ffprobe could not read a video stream from that file. The upscaler needs width, \
             height, frame rate and duration to plan a render, and will not guess any of them.",
        );
    }

    info!(token = %token, bytes = written, path = %path.display(), "captured an upscale source");
    (
        StatusCode::OK,
        Json(UploadAccepted {
            token,
            filename: original,
            bytes: written,
            geometry,
        }),
    )
        .into_response()
}

/// `POST /api/v1/upscale/plan` — the pre-flight. Probe the source, resolve the target geometry and
/// compute the split, **without touching the GPU**.
///
/// 🔴 This exists so the panel can show "1 prompt, no seams" or "6 segments, 5 seams" *before* the
/// button is pressed. The seam count is the single most important thing to know about an upscale
/// and discovering it after twenty minutes of GPU is the wrong time.
async fn plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpscaleGenerateRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let (source_path, _label) = match resolve_source(&state, &req.source).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let geometry = match probe_geometry(&source_path).await {
        Ok(g) => g,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("cannot probe the source: {e}"),
            );
        }
    };
    let guard = state.upscale.ram_guard().await;
    let engine_req = req.to_engine(source_path);
    match upscale_engine::plan_segments(geometry.into(), &engine_req, guard) {
        Ok(plan) => Json(json!({
            "source": geometry,
            "plan": plan,
            "guard": { "max_estimate_bytes": guard.max_estimate_bytes, "copies": guard.copies },
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

/// `POST /api/v1/upscale/generate` — plan the split and enqueue it. 202 and a job id.
///
/// 🔴 The 409 is gone. "upscale {id} is already running" was the single-slot store refusing rather
/// than queueing, and refusing is the behaviour this change exists to remove. The engine's own
/// single slot is still single — it is the queue's lane now, and a second upscale waits in it.
///
/// 🔴 And this is now the ONLY path to the engine: a direct route left alive beside the queue is
/// two writers to one engine.
async fn generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpscaleGenerateRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.upscale.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the upscaler is not configured — set `upscale_server_url` in Settings",
        );
    }
    let (source_path, label) = match resolve_source(&state, &req.source).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let geometry = match probe_geometry(&source_path).await {
        Ok(g) => g,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("cannot probe the source: {e}"),
            );
        }
    };
    // 🔴 The split is computed HERE and carried on the row. `JobRunner::units`/`shape_key` are
    // synchronous and this needs an ffprobe plus the engine's `/capabilities`; re-deriving it at
    // dispatch would also silently contradict the seam count `/plan` already showed the user.
    let guard = state.upscale.ram_guard().await;
    let engine_req = req.to_engine(source_path);
    let plan = match upscale_engine::plan_segments(geometry.into(), &engine_req, guard) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    let who = match crate::routes::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let enqueued = crate::routes::queue::enqueue(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(&headers).request(
            JobKind::Upscale,
            json!({
                "request": engine_req,
                "plan": plan,
                "source_label": label,
            }),
        ),
    )
    .await;

    match enqueued {
        Ok(resp) => (StatusCode::ACCEPTED, Json(resp)).into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

/// `GET /api/v1/upscale/jobs/{id}` — poll a run, off the queue row.
async fn job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such upscale job");
    };
    if row.kind != JobKind::Upscale {
        return err(StatusCode::NOT_FOUND, "that job is not an upscale");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/upscale/active` — the run in flight or next in line, so a reloaded page can re-find
/// a job whose id it lost.
async fn active(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let live = queue_store::live(&state.db).await.unwrap_or_default();
    let pending = queue_store::pending(&state.db).await.unwrap_or_default();
    let found = live
        .into_iter()
        .chain(pending)
        .find(|j| j.kind == JobKind::Upscale);
    match found {
        Some(row) => Json(json!({ "active": view(&state, &row).await })).into_response(),
        None => Json(json!({ "active": Value::Null })).into_response(),
    }
}

/// `POST /api/v1/upscale/jobs/{id}/cancel`.
///
/// 🔴 Through the dispatcher, not straight at the engine. The dispatcher owns the run, so it is the
/// only thing that can stop it AND file the terminal state; reaching past it would leave a row
/// `running` with nothing running.
async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such upscale job");
    };
    if row.kind != JobKind::Upscale {
        return err(StatusCode::NOT_FOUND, "that job is not an upscale");
    }
    match row.state {
        JobState::Queued | JobState::Held => {
            state.queue.abandon_pending(&state.db, &id).await;
            Json(json!({ "cancelled": id })).into_response()
        }
        JobState::Running => match state.queue.get() {
            Some(d) if d.interrupt(&id, crate::queue::runner::Interrupt::Cancel) => {
                (StatusCode::ACCEPTED, Json(json!({ "cancelled": id }))).into_response()
            }
            _ => err(
                StatusCode::CONFLICT,
                "that upscale is not running on this process, or has already finished on the GPU",
            ),
        },
        // 🔴 Past the engine/disk boundary: every segment is fetched, the card is free, and what is
        // left is an ffmpeg concat. Killing it here loses the only copy.
        JobState::Collecting => err(
            StatusCode::CONFLICT,
            "this upscale has finished on the GPU and is joining its segments — it cannot be \
             cancelled now",
        ),
        _ => err(StatusCode::CONFLICT, "this upscale has already finished"),
    }
}

/// The joined output. Served through `byte_serve` so a browser can range-request a large video
/// rather than downloading it whole to scrub.
///
/// 🔴 SIGNATURE-AUTHED, not header-authed. A `<video src>` cannot set an Authorization header, so
/// the client asks `POST /api/v1/generations/sign` for this path and uses the signed URL — the
/// same mechanism the gallery uses, and the reason this path is on that route's allowlist. Header
/// auth still works for a CLI; `?token=` still works and is still the thing to avoid.
async fn serve_output(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<MediaQuery>,
) -> Response {
    let route = format!("/api/v1/upscale/jobs/{id}/file");
    if let Some(resp) = media_gate(&state, &headers, &q, &route).await {
        return resp;
    }
    let Some((path, _)) = locate_output(&state, &id).await else {
        return err(
            StatusCode::CONFLICT,
            "this job has no output yet — poll it until it reports `done`",
        );
    };
    match crate::routes::byte_serve::serve_file(&headers, &path, upscale_engine::OUTPUT_MIME).await
    {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "the output file is gone"),
    }
}

/// One delivered segment, before the join. Worth serving: when a seam looks wrong, the question is
/// always "which side of it", and that is answered by playing the two segments.
///
/// ⚠️ Only while the run's working directory exists. The adapter clears it the moment the gallery
/// holds its own link to the joined deliverable, so this is a during-and-just-after view.
async fn serve_segment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, n)): Path<(String, u32)>,
    axum::extract::Query(q): axum::extract::Query<MediaQuery>,
) -> Response {
    let route = format!("/api/v1/upscale/jobs/{id}/segments/{n}");
    if let Some(resp) = media_gate(&state, &headers, &q, &route).await {
        return resp;
    }
    let path = work_dir(&state, &id).join(seg_name(n));
    match crate::routes::byte_serve::serve_file(&headers, &path, upscale_engine::OUTPUT_MIME).await
    {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "the segment file is gone"),
    }
}

// ── the queue row, in this page's vocabulary ─────────────────────────────────

/// Render a queue row as the `UpscaleJob` body the panel polls.
///
/// Live phase is overlaid from the engine client rather than trusted from the row: the client keeps
/// the authoritative snapshot between the row's one-second writes.
async fn view(state: &AppState, row: &Job) -> Value {
    let payload = queue_store::payload(&state.db, &row.id)
        .await
        .ok()
        .flatten()
        .map(|(p, _)| p)
        .unwrap_or(Value::Null);
    let plan = payload.get("plan").cloned().unwrap_or(Value::Null);
    let count = row.units_total.unwrap_or(0).max(0) as u32;
    let done = row.units_done.max(0) as u32;

    let mut progress = json!({
        "phase": row.progress.phase,
        "current_step": row.progress.current_step,
        "total_steps": row.progress.total_steps,
        "progress": row.progress.progress,
        "queue_position": row.progress.queue_position,
        "segment": (row.state == JobState::Running && count > 0).then(|| (done + 1).min(count)),
        "segment_count": (count > 0).then_some(count),
    });
    if !row.state.is_terminal() {
        let live = state.upscale.progress();
        progress["phase"] = json!(live.phase);
        progress["current_step"] = json!(live.current_step);
        progress["total_steps"] = json!(live.total_steps);
        progress["progress"] = json!(live.progress);
        progress["queue_position"] = json!(live.queue_position);
    }

    // Only once the run is over: the miss path scans the gallery, and this body is polled every
    // second for as long as the upscale takes.
    let output = match row.state {
        JobState::Done => locate_output(state, &row.id).await,
        _ => None,
    };
    let (output_filename, output_bytes) = match output {
        Some((path, bytes)) => (
            json!(path.file_name().map(|n| n.to_string_lossy().to_string())),
            json!(bytes),
        ),
        None => (Value::Null, Value::Null),
    };

    json!({
        "id": row.id,
        // `collecting` IS the join: every segment is fetched and the card is free, which is exactly
        // what this engine's `joining` has always meant.
        "state": match row.state {
            JobState::Queued | JobState::Held => "queued",
            JobState::Running => "rendering",
            JobState::Collecting => "joining",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        },
        "server_job_id": row.server_job_id,
        "segments_done": done,
        "segment_count": count,
        "progress": progress,
        "segments": delivered_segments(state, &row.id, done, &plan).await,
        "output_filename": output_filename,
        "output_bytes": output_bytes,
        "plan": plan,
        // The engine's accept echo belonged to the per-segment submit and the row does not carry
        // it. Its warnings do reach `warnings`, which is the half anyone reads.
        "submit_plan": Value::Null,
        "source_label": payload.get("source_label").cloned().unwrap_or(Value::Null),
        "warnings": row.warnings,
        "error": row.error,
        "gate_wait_ms": row.admit_ms,
        "duration_ms": row.render_ms,
        "cancel_requested": false,
        "blocked_reason": row.blocked_reason,
        "eta_start": row.eta_start,
    })
}

/// The segments already on koblem's disk. Sized by `stat` rather than remembered, because the files
/// are the authority the adapter's own resume trusts.
async fn delivered_segments(state: &AppState, job_id: &str, done: u32, plan: &Value) -> Value {
    let dir = work_dir(state, job_id);
    let mut out = Vec::with_capacity(done as usize);
    for index in 0..done {
        let name = seg_name(index);
        let Ok(meta) = tokio::fs::metadata(dir.join(&name)).await else {
            continue;
        };
        out.push(json!({
            "index": index,
            "filename": name,
            "bytes": meta.len(),
            "start_time": Value::Null,
            "duration": Value::Null,
            "width": plan.get("output_width").cloned().unwrap_or(Value::Null),
            "height": plan.get("output_height").cloned().unwrap_or(Value::Null),
            "duration_seconds": Value::Null,
        }));
    }
    Value::Array(out)
}

fn work_dir(state: &AppState, job_id: &str) -> PathBuf {
    state.queue_files.work_dir(job_id)
}

/// The adapter's segment filename. Duplicated rather than shared because the adapter owns the
/// writing and this route owns the serving, and the name is the only thing crossing between them.
fn seg_name(index: u32) -> String {
    format!("seg{index:04}.{}", upscale_engine::OUTPUT_FORMAT)
}

/// The joined deliverable and its size.
///
/// 🔴 Two places, in this order, and the second is the one that lasts: the adapter joins into the
/// working directory, files the result in the gallery, and then clears the directory — so a
/// finished upscale lives in the gallery and nowhere else. That is the fix for the old 7-day TTL
/// that expired the only copy of the deliverable.
async fn locate_output(state: &AppState, job_id: &str) -> Option<(PathBuf, u64)> {
    let joined =
        work_dir(state, job_id).join(format!("upscaled.{}", upscale_engine::OUTPUT_FORMAT));
    if let Ok(meta) = tokio::fs::metadata(&joined).await {
        return Some((joined, meta.len()));
    }
    let root = crate::routes::tier2_helpers::generations_root(state, SERVICE_TAG).await?;
    let rows =
        crate::repositories::generations::list(&state.db, Some(SERVICE_TAG), GALLERY_SCAN, 0)
            .await
            .ok()?;
    let row = rows
        .into_iter()
        .find(|r| r.request_meta.get("job_id").and_then(Value::as_str) == Some(job_id))?;
    let out = row.outputs.into_iter().find(|o| o.kind == "video")?;
    Some((root.join(&out.filename), out.size_bytes.max(0) as u64))
}

/// ffprobe the source for the four numbers the split is computed from.
///
/// 🔴 All four are required and none is defaulted. The RAM guard, the segment split and the target
/// geometry are all keyed on them, and a default would make all three wrong in the same direction —
/// the same rule the engine's own `check_probe` states.
async fn probe_geometry(path: &FsPath) -> Result<ProbedGeometry, String> {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,width,height,avg_frame_rate,r_frame_rate:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .await
        .map_err(|e| format!("ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let body: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe output: {e}"))?;
    let streams = body
        .get("streams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let video = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
        .ok_or_else(|| "the file has no video stream".to_string())?;
    let has_audio = streams
        .iter()
        .any(|s| s.get("codec_type").and_then(Value::as_str) == Some("audio"));

    let width = video
        .get("width")
        .and_then(Value::as_u64)
        .ok_or_else(|| "no width".to_string())? as u32;
    let height = video
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| "no height".to_string())? as u32;
    // `avg_frame_rate` first: it is the container's average over the whole file, which is what a
    // frame-count estimate wants. `r_frame_rate` is the lowest rate the timebase can express and
    // reads as 1000 fps on some variable-rate captures.
    let fps = rate(video.get("avg_frame_rate"))
        .or_else(|| rate(video.get("r_frame_rate")))
        .ok_or_else(|| "no usable frame rate".to_string())?;
    let duration_seconds = body
        .pointer("/format/duration")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .ok_or_else(|| "no duration".to_string())?;

    Ok(ProbedGeometry {
        width,
        height,
        fps,
        duration_seconds,
        has_audio,
    })
}

/// ffprobe writes rates as `"24000/1001"`. `0/0` means unknown.
fn rate(v: Option<&Value>) -> Option<f64> {
    let s = v.and_then(Value::as_str)?;
    let (num, den) = s.split_once('/')?;
    let (num, den) = (num.parse::<f64>().ok()?, den.parse::<f64>().ok()?);
    if den == 0.0 || num == 0.0 {
        return None;
    }
    Some(num / den)
}

// ── source resolution ────────────────────────────────────────────────────────

/// Turn a request's `source` into an absolute path that will still be there at dispatch.
///
/// 🔴 CAPTURE, NOT STAGING. Neither arm copies anything into the shared mount: an upload is already
/// captured by `/upload`, and a Director take is left in the project's own renders directory and
/// named by path. The adapter links whichever it gets into `UPSCALE_STAGING_HOST` for the life of
/// one run and unlinks it on every exit — doing that here would park a whole video in a directory
/// both containers see for as long as the queue is deep, which is overnight, routinely.
async fn resolve_source(
    state: &AppState,
    source: &UpscaleSource,
) -> Result<(PathBuf, String), Response> {
    match source {
        UpscaleSource::Upload { token } => {
            // The token names a file this service wrote. Anything with a separator in it is a
            // caller trying to name something else.
            if token.contains('/') || token.contains('\\') || token.contains("..") {
                return Err(err(StatusCode::BAD_REQUEST, "invalid upload token"));
            }
            match state.queue_files.find_capture(token).await {
                Some(p) => Ok((p, "upload".to_string())),
                None => Err(err(
                    StatusCode::NOT_FOUND,
                    "that upload is no longer captured — upload the file again",
                )),
            }
        }
        UpscaleSource::DirectorTake {
            project_id,
            tier,
            shot,
            variant,
        } => {
            // 🔴 The SAME lookup `get_take` does, so this cannot address a file that route would
            // not serve. `project_render_dir` is where the traversal check lives.
            let Some(tier) = ["proxy", "final"].iter().find(|t| *t == tier) else {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "tier must be `proxy` or `final`",
                ));
            };
            let Some(render_dir) =
                crate::routes::ltx_director::project_render_dir(state, project_id)
            else {
                return Err(err(StatusCode::BAD_REQUEST, "bad project id"));
            };
            let row = match crate::repositories::director_shot_renders::get_take(
                &state.db, project_id, tier, *shot, *variant,
            )
            .await
            {
                Ok(Some(r)) => r,
                Ok(None) => return Err(err(StatusCode::NOT_FOUND, "take not found")),
                Err(e) => {
                    warn!(error = %e, "director take lookup failed");
                    return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "take lookup failed"));
                }
            };
            let src = render_dir.join(&row.file_name);
            if !tokio::fs::try_exists(&src).await.unwrap_or(false) {
                return Err(err(
                    StatusCode::NOT_FOUND,
                    "the take's file is gone from the renders directory",
                ));
            }
            Ok((
                src,
                format!("{project_id} · {tier} shot {shot} take {}", variant + 1),
            ))
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

/// The query a signed media URL carries. Mirrors `routes::generations::FileQuery`.
#[derive(Debug, Default, Deserialize)]
pub struct MediaQuery {
    exp: Option<i64>,
    sig: Option<String>,
    /// The `?token=` fallback. Works, and is the thing signed URLs exist to
    /// remove — it puts a real API key in browser history and access logs. A client that has
    /// prefetched a signature never sends it.
    token: Option<String>,
}

/// Auth for a media route: a valid signature for THIS path, or a real credential.
///
/// 🔴 The signature is checked against the path it was issued for, so it grants this one file and
/// nothing else. Same rule as the gallery's `authorised_row`, stated once here rather than inline
/// in two handlers.
async fn media_gate(
    state: &AppState,
    headers: &HeaderMap,
    q: &MediaQuery,
    path: &str,
) -> Option<Response> {
    let signed = match (q.exp, q.sig.as_deref()) {
        (Some(exp), Some(sig)) => state.media_signer.verify(path, exp, sig),
        _ => false,
    };
    if signed {
        return None;
    }
    if let Some(token) = q.token.as_deref().map(str::trim).filter(|s| !s.is_empty())
        && crate::auth::validate_token(&state.db, state.api_key.as_deref(), token)
            .await
            .is_ok()
    {
        return None;
    }
    auth_gate(state, headers).await
}

async fn cleanup(path: Option<&FsPath>) {
    if let Some(p) = path {
        let _ = tokio::fs::remove_file(p).await;
    }
}

/// A short unique-enough token. The crate already depends on `uuid` for generation ids.
fn uuid_like() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_parse_the_way_ffprobe_writes_them() {
        assert_eq!(
            rate(Some(&json!("24000/1001"))).map(|f| (f * 1000.0).round()),
            Some(23976.0)
        );
        assert_eq!(rate(Some(&json!("25/1"))), Some(25.0));
        // `0/0` is ffprobe for "unknown" and must not become a division or a zero fps.
        assert_eq!(rate(Some(&json!("0/0"))), None);
        assert_eq!(rate(Some(&json!(""))), None);
        assert_eq!(rate(None), None);
    }

    /// 🔴 A Director source is COORDINATES, never a filename — so the browser cannot name a file
    /// this service did not write, and the on-disk name never leaves the server. The tier is an
    /// allowlist rather than a passthrough for the same reason.
    #[test]
    fn a_director_source_is_coordinates_and_the_tier_is_an_allowlist() {
        let parsed: UpscaleSource = serde_json::from_value(json!({
            "kind": "director_take",
            "project_id": "p1", "tier": "final", "shot": 3, "variant": 1
        }))
        .expect("coordinates parse");
        match parsed {
            UpscaleSource::DirectorTake {
                tier,
                shot,
                variant,
                ..
            } => {
                assert_eq!(tier, "final");
                assert_eq!((shot, variant), (3, 1));
            }
            _ => panic!("wrong arm"),
        }
        // There is no `filename` arm to abuse: a body carrying one does not parse at all.
        assert!(
            serde_json::from_value::<UpscaleSource>(json!({
                "kind": "director_take", "project_id": "p1", "filename": "../../etc/passwd"
            }))
            .is_err()
        );
        // And the tier is checked against a fixed pair, not passed through.
        assert!(["proxy", "final"].iter().find(|t| **t == "../x").is_none());
    }

    /// The engine request carries the defaults the panel did not set, and `to_engine` must not
    /// invent values for the ones the plugin owns — see `upscale_engine::UpscaleRequest`.
    #[test]
    fn unset_options_stay_unset_through_the_conversion() {
        let req: UpscaleGenerateRequest = serde_json::from_value(json!({
            "source": { "kind": "upload", "token": "up_abc" }
        }))
        .expect("minimal request");
        let engine = req.to_engine(PathBuf::from("/var/lib/koblem/upscale-staging/up_abc.mp4"));
        assert_eq!(engine.multiplier, upscale_engine::DEFAULT_MULTIPLIER);
        assert!(engine.noise.is_none());
        assert!(!engine.detailer);
        assert!(engine.segment_seconds.is_none());
    }

    /// 🔴 The name is the only thing crossing between the adapter that writes a segment and this
    /// route that serves it. `queue::adapters::upscale::seg_name` must agree character for
    /// character or `/segments/{n}` 404s on files that are on disk.
    #[test]
    fn the_segment_filename_matches_the_adapters() {
        assert_eq!(
            seg_name(0),
            format!("seg0000.{}", upscale_engine::OUTPUT_FORMAT)
        );
        assert_eq!(
            seg_name(12),
            format!("seg0012.{}", upscale_engine::OUTPUT_FORMAT)
        );
    }
}
