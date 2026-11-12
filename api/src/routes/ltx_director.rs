//! Director 2 — saved, editable, re-runnable video-shot timelines.
//!
//! The `/ltx-director` web editor is a timeline over the SAME LTX chain render
//! the `/ltx-video` page drives. This module adds two things on top of that
//! render path:
//!
//!   1. Project persistence (`director_projects` table): CRUD over an opaque
//!      timeline doc so an in-progress "directed" music video can be saved,
//!      reopened, edited, and re-run.
//!   2. A `/run` endpoint that compiles a saved timeline into an
//!      [`LtxVideoGenerateRequest`] + inline assets and ENQUEUES it on the global
//!      GPU queue (job lands in the shared `state.ltx_jobs` store, so the existing
//!      `/api/v1/ltx-video/jobs/{id}` polling covers it unchanged). 🔴 A cut is ONE
//!      job — `segments`, `generations_done` and `partials[]` all live on one row —
//!      so it is one enqueue however many shots it holds. Retakes are their own jobs.
//!   3. A `/transcribe` endpoint that word-aligns an audio track (Parakeet STT,
//!      `timestamp_granularity=word`) so the editor can snap cuts/keyframes to
//!      lyric onsets.
//!
//! The persisted timeline doc shape (frontend-authored, JSONB):
//! ```json
//! {
//!   "request": { ...LtxVideoGenerateRequest... },   // compiled render spec
//!   "assets": {
//!     "opener_image_b64": "<base64|data-uri|null>",
//!     "audio_b64":        "<base64|null>",
//!     "bgm_b64":          "<base64|null>",
//!     "seg_images_b64":   ["<base64|null>", ...]     // aligned to request.segments
//!   },
//!   "editor": { ... }                                // opaque clip layout / lyrics
//! }
//! ```

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine as _;
use bytes::Bytes;
use koblem_client::types::{
    H3_CONTINUATION_FROM_PREV, H3ContinuationMode, LtxAudioSource, LtxBeat, LtxCastMember,
    LtxImageSource, LtxRuntimeLora, LtxSegment, LtxVideoGenerateRequest, StoryContext,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::AppState;
use crate::auth::require_auth;
use crate::director_assets::{self, AssetJson};
use crate::repositories::director_assets as repo_assets;
use crate::repositories::director_projects as repo;
use crate::repositories::director_shot_renders as repo_renders;
use crate::routes::byte_serve;
use crate::stt::SttParams;

/// 256 MB body cap — a saved timeline can carry several inline base64 clips +
/// keyframes. Matches the ltx-video limit.
const DIRECTOR_BODY_LIMIT: usize = 256 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/ltx-director/projects",
            get(list_projects).post(create_project),
        )
        .route(
            "/api/v1/ltx-director/projects/{id}",
            get(get_project)
                .put(update_project)
                .patch(patch_project)
                .delete(delete_project),
        )
        // What is rendering for this project RIGHT NOW — derived from the job store, so a second
        // device converges on a render it never dispatched.
        .route(
            "/api/v1/ltx-director/projects/{id}/active",
            get(list_active),
        )
        .route("/api/v1/ltx-director/projects/{id}/run", post(run_project))
        .route(
            "/api/v1/ltx-director/projects/{id}/render-plan",
            get(render_plan),
        )
        .route(
            "/api/v1/ltx-director/projects/{id}/retake",
            post(retake_segment),
        )
        .route(
            "/api/v1/ltx-director/projects/{id}/renders",
            get(list_renders),
        )
        .route(
            "/api/v1/ltx-director/projects/{id}/renders/{tier}/{shot}",
            get(get_render),
        )
        // Takes: several rolls of one shot, kept side by side. `select` moves the pointer; `rebuild`
        // re-derives the stitched cut from whichever takes are selected.
        .route(
            "/api/v1/ltx-director/projects/{id}/renders/{tier}/{shot}/takes",
            get(list_takes),
        )
        .route(
            "/api/v1/ltx-director/projects/{id}/renders/{tier}/{shot}/takes/{variant}",
            get(get_take),
        )
        .route(
            "/api/v1/ltx-director/projects/{id}/renders/{tier}/{shot}/select",
            post(select_take),
        )
        // Promote one frame of a rendered shot into the asset store — how a character that was
        // INTRODUCED in a t2v shot acquires the reference every later shot conditions on.
        .route(
            "/api/v1/ltx-director/projects/{id}/renders/{tier}/{shot}/still",
            post(promote_still),
        )
        .route(
            "/api/v1/ltx-director/projects/{id}/renders/{tier}/rebuild",
            post(rebuild_cut),
        )
        // Vision-in-the-loop dailies review (see routes::ltx_critic).
        .route(
            "/api/v1/ltx-director/projects/{id}/critique",
            post(super::ltx_critic::critique),
        )
        .route("/api/v1/ltx-director/plan", post(plan))
        .route("/api/v1/ltx-director/summarize", post(summarize))
        .route("/api/v1/ltx-director/rework", post(rework))
        .route("/api/v1/ltx-director/review", post(review))
        .route("/api/v1/ltx-director/gist-shots", post(gist_shots))
        .route("/api/v1/ltx-director/extend", post(extend))
        .route("/api/v1/ltx-director/title", post(title))
        .route("/api/v1/ltx-director/chat", post(chat))
        .route("/api/v1/ltx-director/transcribe", post(transcribe))
        .route("/api/v1/ltx-director/assets", post(upload_asset))
        // Registered BEFORE the `{hash}` route: axum matches literal segments first, but keeping
        // them adjacent in this order also stops a later reorder from turning the literal into a
        // 64-hex-char hash lookup that always 404s.
        .route(
            "/api/v1/ltx-director/assets/prepare-reference",
            post(prepare_reference),
        )
        .route("/api/v1/ltx-director/assets/{hash}", get(get_asset))
        .layer(DefaultBodyLimit::max(DIRECTOR_BODY_LIMIT))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

pub(crate) async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

// ─── Project CRUD ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_projects(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    match (
        repo::list(&state.db, limit, offset).await,
        repo::count(&state.db).await,
    ) {
        (Ok(items), Ok(total)) => Json(json!({ "items": items, "total": total })).into_response(),
        (Err(e), _) | (_, Err(e)) => {
            warn!(error = %e, "director list failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "list failed")
        }
    }
}

/// Body for create/update. `metadata` defaults to `{}`.
#[derive(Deserialize)]
struct ProjectPayload {
    name: String,
    timeline: Value,
    #[serde(default)]
    metadata: Option<Value>,
}

/// Body for `PUT …/projects/{id}` — a full-document replace, so it carries the version the client
/// last read. Optional in the struct only so a client that omits it gets a legible 400 instead of
/// axum's bare 422 from a failed deserialise.
#[derive(Deserialize)]
struct ProjectUpdate {
    #[serde(flatten)]
    project: ProjectPayload,
    #[serde(default)]
    version: Option<i64>,
}

/// 409, with the version the caller needs to adopt (or reload from) to make progress.
fn conflict(current_version: i64) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "project was changed elsewhere",
            "current_version": current_version,
        })),
    )
        .into_response()
}

const METADATA_NOT_OBJECT: &str = "metadata must be an object";

/// `metadata` must be an OBJECT on every path that can set it, or the row is corrupted for good:
/// Postgres evaluates `'null'::jsonb || '{…}'::jsonb` as an ARRAY rather than erroring, so one
/// `{"metadata": null}` left a row where each later render stamp appended an element and every read
/// of a key returned nothing, forever. The database now refuses the shape (migration 0034); this is
/// what turns that refusal into a legible 400 instead of a 500. Absent is fine — that is `{}`.
fn metadata_is_object(metadata: Option<&Value>) -> bool {
    metadata.is_none_or(Value::is_object)
}

/// The three outcomes of a conditional write, as status codes. A stale token is 409 — NOT the 404
/// the "no rows updated" shape would suggest, and not a 500.
fn update_response(outcome: repo::UpdateOutcome) -> Response {
    match outcome {
        repo::UpdateOutcome::Updated(row) => Json(row).into_response(),
        repo::UpdateOutcome::Conflict { current_version } => conflict(current_version),
        repo::UpdateOutcome::Missing => err(StatusCode::NOT_FOUND, "project not found"),
    }
}

async fn create_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProjectPayload>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let name = body.name.trim();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "name is required");
    }
    let id = uuid::Uuid::new_v4().to_string();
    if !metadata_is_object(body.metadata.as_ref()) {
        return err(StatusCode::BAD_REQUEST, METADATA_NOT_OBJECT);
    }
    let metadata = body.metadata.unwrap_or_else(|| json!({}));
    match repo::create(&state.db, &id, name, &body.timeline, &metadata).await {
        Ok(row) => (StatusCode::CREATED, Json(row)).into_response(),
        Err(e) => {
            warn!(error = %e, "director create failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "create failed")
        }
    }
}

async fn get_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match repo::get(&state.db, &id).await {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "project not found"),
        Err(e) => {
            warn!(error = %e, "director get failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "get failed")
        }
    }
}

async fn update_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ProjectUpdate>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let name = body.project.name.trim();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "name is required");
    }
    // No version, no blind overwrite: an unversioned PUT is a client old enough to predate the
    // check, and letting it through is exactly the silent revert this endpoint now guards.
    let Some(version) = body.version else {
        return err(StatusCode::BAD_REQUEST, "version is required");
    };
    if !metadata_is_object(body.project.metadata.as_ref()) {
        return err(StatusCode::BAD_REQUEST, METADATA_NOT_OBJECT);
    }
    let metadata = body.project.metadata.unwrap_or_else(|| json!({}));
    match repo::update(
        &state.db,
        &id,
        name,
        &body.project.timeline,
        &metadata,
        version,
    )
    .await
    {
        Ok(outcome) => update_response(outcome),
        Err(e) => {
            warn!(error = %e, "director update failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "update failed")
        }
    }
}

/// Body for `PATCH …/projects/{id}` — every field optional, absent means "leave alone".
#[derive(Deserialize)]
struct ProjectPatch {
    #[serde(default)]
    name: Option<String>,
    /// Shallow-merged into the existing metadata object (`||` in Postgres).
    #[serde(default)]
    metadata: Option<Value>,
}

/// `PATCH /api/v1/ltx-director/projects/{id}` — change a project's name and/or metadata without
/// resending the timeline. A saved timeline carries inline assets, so a `PUT` to rename one costs a
/// round trip of the whole (multi-megabyte) doc — and races any concurrent edit of it.
///
/// Carries no version and answers with the summary, not the row: the request never names the
/// timeline and the response never returns it, so a rename is small in BOTH directions. It still
/// bumps the version (see [`repo::patch`]) and hands the new one back, which is what lets the
/// renaming client's own pending timeline autosave go through instead of conflicting with itself.
async fn patch_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ProjectPatch>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let name = body.name.as_deref().map(str::trim);
    if name.is_some_and(str::is_empty) {
        return err(StatusCode::BAD_REQUEST, "name cannot be blank");
    }
    if name.is_none() && body.metadata.is_none() {
        return err(StatusCode::BAD_REQUEST, "nothing to patch");
    }
    if !metadata_is_object(body.metadata.as_ref()) {
        return err(StatusCode::BAD_REQUEST, METADATA_NOT_OBJECT);
    }
    match repo::patch(&state.db, &id, name, body.metadata.as_ref()).await {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "project not found"),
        Err(e) => {
            warn!(error = %e, "director patch failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "patch failed")
        }
    }
}

async fn delete_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    // Collect the engine job ids BEFORE the delete: the FK CASCADE drops the render rows that point
    // at them, and the C++ persist store those ids name is never auto-swept — so for that engine
    // this is the only chance. Best-effort — a failed engine delete just leaves an orphan dir.
    let chains = repo_renders::latent_chains_by_project(&state.db, &id)
        .await
        .unwrap_or_default();
    // The project's CURRENT engine, which is only the fallback for a chain `0039` could not place.
    // Routing every id here was the bug: a project that switched engine had the previous one's dirs
    // asked of a service that has never heard of them, so nothing ever collected them.
    //
    // ⚠️ Nothing here is the last chance any more. Both shims run one `JobStore` (kobbler
    // `docker/comfyui/api/core/jobs.py`), which `rmtree`s a terminal job's dir `COMFY_API_JOB_TTL`
    // after it finishes — 600 s by default — so a delete here is a courtesy that usually 404s. It
    // is still asked, because a dir that IS still there is the project's. (The H3 latent BANK is
    // separate, byte-capped and evicted oldest-first, so it is not koblem's to clean either way.)
    let engine = repo::get(&state.db, &id)
        .await
        .ok()
        .flatten()
        .map(|p| project_engine(&p.timeline))
        .unwrap_or(ENGINE_LTX25);
    match repo::delete(&state.db, &id).await {
        Ok(true) => {
            // The FK CASCADE dropped the render rows; remove the project's owned webm dir too.
            if let Some(dir) = project_render_dir(&state, &id) {
                let _ = tokio::fs::remove_dir_all(&dir).await;
            }
            // Drop the project's persisted latents from the engine that banked each one. Three
            // engines, three job stores: an id belongs to exactly one of them and the other two
            // answer 404.
            for chain in &chains {
                let owner = chain.engine.as_deref().unwrap_or(engine);
                if let Err(e) = drop_engine_job(&state, owner, &chain.id).await {
                    warn!(error = %e, project = %id, job = %chain.id, engine = owner, "director delete: engine job cleanup failed");
                }
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "project not found"),
        Err(e) => {
            warn!(error = %e, "director delete failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "delete failed")
        }
    }
}

// ─── In-flight renders ───────────────────────────────────────────────────────

/// `GET /api/v1/ltx-director/projects/{id}/active` — the renders in flight for this project, from
/// whichever device started them.
///
/// Derived on demand from the job store, never mirrored into a table: `LtxJobStore::sweep` drops
/// only TERMINAL jobs (both the TTL check and the MAX_JOBS eviction filter on `is_terminal()`), so
/// a non-terminal job is always still there to be found. A DB mirror would be a second source of
/// truth that goes stale the moment the process dies between the write and the clear.
async fn list_active(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let mut jobs = state.ltx_jobs.list();
    jobs.retain(|(_, j)| j.director_project_id.as_deref() == Some(id.as_str()));
    // One row per job rather than one listing read: this is already narrowed to a single project,
    // so it is a handful of rows at most — and an exact lookup still resolves a record that has sat
    // non-terminal long enough to fall off the tail of the recent listing. A record that reached a
    // terminal state of its own is not asked about: the overlay would decline to touch it anyway,
    // and this route is polled every 12 s by every tab with the project open.
    let mut rows = Vec::new();
    for (_, j) in jobs.iter().filter(|(_, j)| !j.state.is_terminal()) {
        if let Some(row) = crate::routes::ltx_video::queue_row_for(&state, j).await {
            rows.push(row);
        }
    }
    let items = active_jobs_for_project(&id, &with_queue_state(jobs, &rows), &pending_harvests());
    Json(json!({ "items": items })).into_response()
}

/// Fold the QUEUE's account of each job into the store record.
///
/// 🔴 The store record is not the authority on a job that never reached the adapter. A cancel goes
/// through the queue, and `LtxJobStore::request_cancel` only sets the cooperative flag — the
/// background render task is what writes the terminal state, and a job cancelled before it ever
/// dispatched has no such task. So `ltx_jobs` says `Queued` forever, and `sweep` drops terminal
/// jobs only: the row sits in `/active` for the life of the process.
///
/// That was a live defect, not a theoretical one. Every tab with the project open re-adopted the
/// dead job on each 12 s poll — revoking every shot clip on the way in, reading `cancelled` off
/// `/jobs/{id}` (which has always overlaid) one poll later, finishing, and re-downloading the lot.
/// The Director reloaded its own video and stills every twelve seconds.
fn with_queue_state(
    mut jobs: Vec<(String, crate::ltx_jobs::LtxJob)>,
    rows: &[koblem_client::types::Job],
) -> Vec<(String, crate::ltx_jobs::LtxJob)> {
    for (_, job) in &mut jobs {
        if let Some(qid) = job.queue_job_id.as_deref()
            && let Some(row) = rows.iter().find(|r| r.id == qid)
        {
            crate::routes::ltx_video::overlay_queue_state(job, row);
        }
    }
    jobs
}

/// Director renders dispatched but not yet harvested into `director_shot_renders`.
///
/// A job leaves the store's unfinished set the instant it goes terminal, but its output only reaches
/// `/renders` when the harvest that follows completes — and a full run's harvest replaces the tier's
/// whole set on the way. A second device switching from active-polling to render-listing across that
/// transition could read a half-populated set. Staying in `/active` until the harvest lands makes
/// the handoff unambiguous on the SERVER, instead of every client having to guess with a sleep.
static PENDING_HARVEST: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// Recovers from poisoning rather than propagating it: a panic elsewhere must not silently stop
/// every later render from being tracked (the set holds ids, nothing an aborted write can corrupt).
fn pending_lock() -> std::sync::MutexGuard<'static, std::collections::HashSet<String>> {
    PENDING_HARVEST.lock().unwrap_or_else(|e| e.into_inner())
}

fn pending_harvests() -> std::collections::HashSet<String> {
    pending_lock().clone()
}

/// Marks a job as awaiting harvest for as long as it is held. A guard rather than a pair of calls
/// because the render it covers can panic or be cancelled, and a job that never cleared would sit in
/// `/active` forever — every client that opened the project would then wait out its whole harvest
/// timeout before showing the renders it already had.
pub(crate) struct HarvestPending(String);

impl HarvestPending {
    pub(crate) fn new(job_id: &str) -> Self {
        pending_lock().insert(job_id.to_string());
        Self(job_id.to_string())
    }
}

impl Drop for HarvestPending {
    fn drop(&mut self) {
        pending_lock().remove(&self.0);
    }
}

/// The renders tagged for `project_id` that a client still has to wait on, newest first (the store
/// lists in that order) — unfinished, or finished but not yet harvested.
fn active_jobs_for_project(
    project_id: &str,
    jobs: &[(String, crate::ltx_jobs::LtxJob)],
    pending_harvest: &std::collections::HashSet<String>,
) -> Vec<Value> {
    jobs.iter()
        .filter(|(id, j)| {
            j.director_project_id.as_deref() == Some(project_id)
                && (!j.state.is_terminal() || pending_harvest.contains(id.as_str()))
        })
        .map(|(id, j)| active_job_json(id, j, pending_harvest.contains(id.as_str())))
        .collect()
}

/// The two fields a Director client needs that the shared `VideoJobStatus` does not carry —
/// `project_id` and `tier` — plus enough progress to adopt the render mid-flight. `harvesting` says
/// the render itself is over and only its persistence is outstanding, so a client waits for the
/// durable rows rather than showing a render that is no longer happening.
fn active_job_json(job_id: &str, job: &crate::ltx_jobs::LtxJob, pending_harvest: bool) -> Value {
    let mut out = json!({
        "job_id": job_id,
        "project_id": job.director_project_id,
        "tier": job.director_tier,
        "status": job.state,
        // Pending from dispatch, but only OVER once the job is terminal — a client reads this as
        // "the durable rows are mid-write", which a still-running render is not.
        "harvesting": pending_harvest && job.state.is_terminal(),
        "segments": job.segments,
        "completed_segments": job.partials.len(),
        // The prompt-filtered scripts AS SUBMITTED — an adopting client needs them to know which
        // shots this render covers without waiting for the harvest.
        "scripts": job.scripts,
        "server_job_id": job.server_job_id,
        "seed": job.seed,
        "partials": job.partials,
        // Which engine is rendering. A client adopting a render started on another device cannot
        // read this off the project (the project may have been switched since), and the two
        // engines offer different controls mid-flight — LTX can be resumed from its bank, H3
        // cannot.
        "engine": job.engine_name(),
        // Present only on a PARTIAL render (an H3 generation retake): the project unit its one take
        // belongs to. Absent = the job's indices are already the project's. ⚠️ The UNIT is
        // per-engine — a shot on LTX, a GENERATION on H3.
        "shot_offset": job.director_shot_offset,
        // 🔴 The same question the status body answers, asked the same way — an adopting client
        // predicts this at DISPATCH and a client that reloaded never dispatched anything, so
        // without it the tab attaches with "this is the whole cut" until the first poll corrects
        // it. `shot_offset` alone is only H3's half: an LTX-2.5 resume is a fragment with no shift.
        "job_is_partial": job.delivers_fragment(),
        // What a client adopting a render started elsewhere counts elapsed time from.
        "created_at": job.created_at()
            .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()),
    });
    // H3 only: the numeric progress WanGP surfaces and the LTX stack has never had. Inserted only
    // when present, matching `VideoJobStatus`: on an LTX render the keys are ABSENT, and absence
    // means "this engine does not report progress" — which an explicit null does not say.
    let obj = out.as_object_mut().expect("json! built an object");
    let mut put = |key: &str, v: Value| {
        if !v.is_null() {
            obj.insert(key.to_string(), v);
        }
    };
    put("total_eta_seconds", json!(job.total_eta_seconds));
    put("progress", json!(job.progress));
    put("current_step", json!(job.current_step));
    put("total_steps", json!(job.total_steps));
    put("phase", json!(job.phase));
    // Bumps when a new preview frame is ready at `/api/v1/minimax-h3/jobs/{id}/preview`.
    put("preview_seq", json!(job.preview_seq));
    put("generation_index", json!(job.generation_index));
    put("generations_done", json!(job.generations_done));
    out
}

// ─── Render ──────────────────────────────────────────────────────────────────

/// Strip an optional `data:<mime>;base64,` prefix and decode. Returns None for
/// empty / null / undecodable input (an absent asset is not an error).
fn decode_b64(s: &str) -> Option<Bytes> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let payload = s.rsplit_once(";base64,").map(|(_, b)| b).unwrap_or(s);
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()
        .map(Bytes::from)
}

/// Everything [`decode_assets`] could not turn into bytes, kept apart by KIND.
///
/// Both kinds fail the render and for the same reason — a scene image or keyframe pin that quietly
/// resolves to nothing changes what comes out, i2v degrading to t2v with a 200 and no note — but
/// they are fixed in different places. A lost ref is re-attached in the editor; an undecodable
/// field means the bytes never survived the edge, and pointing that one at the asset store is how
/// the H3 soundtrack bug burned a day.
#[derive(Default)]
struct AssetFaults {
    /// Hashes the store no longer holds, deduplicated, document order.
    missing: Vec<String>,
    /// One line per field that held something which is neither a ref nor base64. There is no hash
    /// to name, so the FIELD is named instead, with what was actually in it.
    unusable: Vec<String>,
}

impl AssetFaults {
    fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.unusable.is_empty()
    }

    /// The 400 body. `missing_refs_message` still owns the wording for lost refs — it is shared with
    /// every other route that rejects on one, and the two lists read as one sentence.
    fn message(&self) -> String {
        let mut parts = Vec::new();
        if !self.missing.is_empty() {
            parts.push(director_assets::missing_refs_message(&self.missing));
        }
        if !self.unusable.is_empty() {
            parts.push(format!(
                "unusable asset field(s): {}",
                self.unusable.join("; ")
            ));
        }
        parts.join(" — ")
    }

    /// Matched exhaustively on purpose: a wildcard would silently file a future fault kind under
    /// "unusable", which is exactly the collapse this whole change exists to undo.
    fn note(&mut self, field: &str, fault: director_assets::MediaFault) {
        match &fault {
            director_assets::MediaFault::Missing(hash) => {
                if !self.missing.contains(hash) {
                    self.missing.push(hash.clone());
                }
            }
            director_assets::MediaFault::Undecodable { .. } => {
                self.unusable.push(format!("{field}: {fault}"));
            }
        }
    }
}

/// Resolve one `assets` field — an `asset:<hash>` ref or literal base64 — to bytes, recording
/// whichever way it failed. `None` here means the field was absent or blank, and nothing else.
async fn asset_bytes(
    dir: &std::path::Path,
    assets: &Value,
    key: &str,
    faults: &mut AssetFaults,
) -> Option<Bytes> {
    let s = assets.get(key).and_then(Value::as_str)?;
    match director_assets::resolve_media(dir, s).await {
        Ok(b) => b,
        Err(fault) => {
            faults.note(key, fault);
            None
        }
    }
}

/// The same, for a per-shot array. Slots stay aligned to the RAW segment index, so a null / absent
/// / unusable entry is `None` rather than dropped — and the index is carried into the fault, since
/// "one of the scene images" is not an answer the author can act on.
async fn asset_bytes_arr(
    dir: &std::path::Path,
    assets: &Value,
    key: &str,
    faults: &mut AssetFaults,
) -> Vec<Option<Bytes>> {
    let Some(arr) = assets.get(key).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let resolved = match v.as_str() {
            Some(s) => match director_assets::resolve_media(dir, s).await {
                Ok(b) => b,
                Err(fault) => {
                    faults.note(&format!("{key}[{i}]"), fault);
                    None
                }
            },
            None => None,
        };
        out.push(resolved);
    }
    out
}

/// Resolve a timeline's inline `assets` object into the byte parts the compile expects:
/// `(opener_image, voice/mixed audio, bgm, per-shot scene images, per-shot v2v source clips)`.
/// `seg_images_b64` / `seg_v2v_b64` stay aligned to the RAW segment list (the compile re-aligns
/// through its empty-prompt filter). A `v2v_source = prior_render` shot needs NO bytes here — it
/// resolves server-side from the referenced job's banked latent.
///
/// `Err` is the finished 400 body naming everything that would not resolve, which the caller must
/// reject on: a keyframe pin or scene image that silently went missing changes what comes out (i2v
/// quietly degrades to t2v). It covers lost refs AND fields whose payload will not decode — the
/// second used to be indistinguishable from "no asset attached" and so travelled on as a `None`.
type RenderAssets = (
    Option<Bytes>,
    Option<Bytes>,
    Option<Bytes>,
    Vec<Option<Bytes>>,
    Vec<Option<Bytes>>,
);
async fn decode_assets(dir: &std::path::Path, assets: &Value) -> Result<RenderAssets, String> {
    let mut faults = AssetFaults::default();
    let image_part = asset_bytes(dir, assets, "opener_image_b64", &mut faults).await;
    let audio_part = asset_bytes(dir, assets, "audio_b64", &mut faults).await;
    let bgm_part = asset_bytes(dir, assets, "bgm_b64", &mut faults).await;
    let seg_image_parts = asset_bytes_arr(dir, assets, "seg_images_b64", &mut faults).await;
    let seg_v2v_parts = asset_bytes_arr(dir, assets, "seg_v2v_b64", &mut faults).await;
    if !faults.is_empty() {
        return Err(faults.message());
    }
    Ok((
        image_part,
        audio_part,
        bgm_part,
        seg_image_parts,
        seg_v2v_parts,
    ))
}

#[derive(Deserialize)]
struct TierQuery {
    /// `final` (default) or `proxy`.
    #[serde(default)]
    tier: Option<String>,
    /// `all` (default) re-renders the timeline and REPLACES the tier's takes. `stale` renders only
    /// what [`h3_render_plan`] says needs it and keeps every existing take.
    ///
    /// 🔴 Defaulting to `all` is what keeps this backwards compatible: `/run` has always meant a
    /// fresh full render, and every existing caller — the mobile client, the critic, a bookmark —
    /// must keep getting exactly that.
    #[serde(default)]
    scope: Option<String>,
}

/// The requested tier, or `final` when unset. `None` for an unknown tier.
fn tier_or_final(tier: Option<&str>) -> Option<&'static str> {
    normalize_tier(
        tier.map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("final"),
    )
}

// ─── Which engine a project renders on ────────────────────────────────────────

/// The Director's engines: `ltx25` is LTX-2.5 on ComfyUI and `h3` is MiniMax-H3 (joint
/// audio+video, no bank, no CFG/variants).
///
/// 🔴 [`ENGINE_LTX`] is a THIRD value that is only ever READ. It names the retired C++ `ltx-video`
/// service (LTX-2.3), and three stores still hold it with three different lifetimes: job sidecars
/// (1 h TTL), `director_projects.timeline.engine` (permanent) and `director_shot_renders.engine`
/// (permanent, `0039`). Nothing produces it as a render target any more, and nothing may map it
/// onto `ltx25`: the two services keep separate job stores, so a 2.3 job id handed to 2.5 is a 404
/// after the full queue wait. The engine-filtered render queries are what keep those rows out of a
/// resume — a project resolves as `ltx25` now, and an `'ltx'` row simply does not match.
///
/// 🔴 `ltx25` is LTX-SHAPED, and every `== ENGINE_H3` branch in this file is therefore correct
/// for it unchanged: it takes the LTX request document, the LTX prompt conventions, the LTX
/// compiled timeline, and — since kobbler `6ca0b6f2` — the three latent-bank fields
/// (`resume_job_id`, `segments[i].bank_job_id`, `retake_from`). Its retake does not splice the
/// tail back, so the job's `/media` is the rendered window alone (`LtxJob::job_is_partial`).
pub(crate) const ENGINE_LTX: &str = "ltx";
pub(crate) const ENGINE_H3: &str = "h3";
pub(crate) const ENGINE_LTX25: &str = "ltx25";

/// The engine a saved timeline renders on.
///
/// Stored TOP-LEVEL in the timeline doc, next to `model` — it is render state, not editor state,
/// and a doc that lost it would compile its shots for an engine that refuses half of them.
///
/// Absent — or the retired `ltx` — is `ltx25`, the only LTX there is. The owner confirmed on
/// 2026-08-14 that no Director project was left on LTX-2.3, so this opens such a document on 2.5
/// rather than on a service that would refuse every render. It is a reading of the DOCUMENT only:
/// it never rewrites a stored `'ltx'`, and the render rows that carry one stay unreachable from a
/// project that now resolves as `ltx25`, which is what stops a 2.3 bank id being resumed.
///
/// H3 projects live in `director_projects` like every other project, deliberately and not
/// incidentally: the asset sweep scans `director_projects.timeline::text` for the `asset:<hash>`
/// refs that keep an asset alive, so a project in a table of its own would have every one of its
/// stills and reference clips collected 7 days later.
pub(crate) fn project_engine(timeline: &Value) -> &'static str {
    match timeline
        .get("engine")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some(ENGINE_H3) => ENGINE_H3,
        _ => ENGINE_LTX25,
    }
}

/// The engine a WRITING request is for, from its optional `engine` field.
///
/// The LLM endpoints are stateless — they never load the project — so the editor states which
/// engine the prompts are being written for on each call. Anything that is not `h3` is `ltx25`:
/// LTX-2.5 is the only LTX engine, so an absent, legacy or unrecognised value has exactly one
/// service it could mean. Only H3 has to be named, and it is the naming that matters — an H3
/// prompt rendered on LTX carries an audio block and inline negatives into a model that has a
/// separate audio sheet and a negative field.
pub(crate) fn request_engine(engine: Option<&str>) -> &'static str {
    match engine.map(str::trim) {
        Some(ENGINE_H3) => ENGINE_H3,
        _ => ENGINE_LTX25,
    }
}

/// True when a writing request is for MiniMax-H3.
///
/// ⚠️ `ltx25` is deliberately NOT h3. The prompt craft, the reply shape and the compiled document
/// are LTX's — 2.5 is a checkpoint of the LTX family reached over the LTX wire, not a second
/// prompt space. Every writing route branches h3-vs-not, so this one predicate keeps LTX-2.5 on
/// the LTX arm without a second set of prompts.
pub(crate) fn is_h3(engine: Option<&str>) -> bool {
    request_engine(engine) == ENGINE_H3
}

/// The SERVER'S half of the resume/retake gate: can this engine reach a latent bank right now?
///
/// 🔴 The browser is gated on `resumeAvailable(engine, props.bank_enabled)` and gets this right.
/// NOTHING ELSE WAS. `is_configured()` only says a URL is set, so a configured LTX-2.5 with
/// `COMFY_API_LTX_BANK_DIR` unset passed straight through, queued, and 501'd after the wait — and
/// the unattended critic dispatches one of those per low-scoring shot of a dailies review, at
/// night, with nobody reading the log. A refusal that arrives before the queue is the only kind
/// worth having.
///
/// `Some(response)` is the refusal, `None` means go ahead. The answer is cached on the client
/// (`LtxVideoClient::bank_live`), so a twelve-shot review costs one probe.
pub(crate) async fn bank_gate(state: &AppState, engine: &str) -> Option<Response> {
    let client = &state.ltx25;
    if !client.is_configured() {
        return Some(err(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{engine} service not configured"),
        ));
    }
    if client.bank_live().await {
        return None;
    }
    Some(err(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!(
            "{engine} banks a latent per shot, but this deployment has no bank directory \
             (COMFY_API_LTX_BANK_DIR) — with it unset the engine answers 501 to a resume and to a \
             single-shot retake. Render the tier again instead. The same answer comes back when \
             the engine is unreachable, since one that cannot be asked cannot be resumed from."
        ),
    ))
}

/// Impose LTX-2.5's delivery limits on a request the SAVED DOCUMENT supplied. Both callers
/// (`/run` and the retake) have already sent an H3 project down its own path, so this is reached
/// only for LTX.
///
/// 🔴 The document stores what the author asked for, which is not always what the engine about to
/// render can do — and the two server paths that dispatch from a saved timeline read the ladder
/// straight out of it. `dims_are_target` makes the base pass `target ÷ the profile's factor`, and
/// LTX-2.5 declines the refine that would put it back for any shot carrying an opening frame,
/// keyframes or character references. So a High ×2 project renders 960×544 and delivers it as 1080p.
///
/// The editor's own dispatch does this in `compile.ts::compileForDelivery`; this is the same rule
/// for the callers that never see the editor — including the unattended critic's auto-retakes.
fn apply_engine_delivery_limits(req: &mut LtxVideoGenerateRequest) {
    req.profile = Some(koblem_client::types::LtxQualityProfile::Standard);
    req.upscaler_ratio = Some(koblem_client::types::LtxUpscalerRatio::None);
    req.hires_chain = None;
}

/// `DELETE /…/job?id=` against the engine that actually holds the job dir.
///
/// 🔴 The routing key is which engine BANKED the chain, never which one the project renders on now.
/// Both callers used to pass the latter, so a project that changed engine asked the new service to
/// drop dirs it had never heard of, while the engine that held them was never asked at all.
/// `director_shot_renders.engine` (`0039`) is what makes the difference sayable.
///
/// ⚠️ A chain banked as `'ltx'` names a dir in the retired C++ service, which is not deployed —
/// the call lands on `ltx25` with an id it has never held and 404s. That is the honest outcome and
/// it is left alone deliberately: cleanup is best-effort, and the alternative (treating an `'ltx'`
/// id as an `ltx25` one anywhere) is precisely the mapping that must never exist.
async fn drop_engine_job(
    state: &AppState,
    engine: &str,
    job: &str,
) -> std::result::Result<(), String> {
    match request_engine(Some(engine)) {
        ENGINE_H3 => state
            .minimax_h3
            .delete_job(job)
            .await
            .map_err(|e| e.to_string()),
        // No latent chain on ltx25 — its job dir holds staged references and the per-segment mp4s —
        // and the shim's own `JobStore` sweeps it `COMFY_API_JOB_TTL` after the job finishes.
        // Asked anyway: the dir is the project's, and tidying it early is free.
        _ => state.ltx25.delete_job(job).await.map_err(|e| e.to_string()),
    }
}

// ─── Packing the timeline into H3 generations ─────────────────────────────────
//
// 🔴 This is the load-bearing difference between the two engines and the reason the H3 paths below
// do not read like the LTX ones.
//
// **LTX's unit is a segment = one render. H3's unit is a GENERATION — up to 15.083 s containing
// 1-N shots**, where a shot boundary is `[Shot N] At MM:SS.mmm` inside ONE prompt and the model
// renders the cut itself. `<scenetrans>` audio continuity across a cut exists ONLY within a
// generation, and cross-shot identity is carried by prose ("the captain from Shot 1").
//
// ⇒ Rendering shots separately is the DEGRADATION you accept when the timeline outruns the 15 s
// budget — not the way to get a cut. Chaining for a cut is strictly worse: it loses
// `<scenetrans>`, loses the prose identity carry, and adds a seam that is never frame-exact.
//
// koblem owns the packing (and the shim explicitly does not re-pack) because koblem owns the
// timeline and the budget arithmetic. Everything downstream — delivery, takes, retakes — is then in
// GENERATION space.

/// One timeline clip, as the packer needs it.
///
/// A deliberately narrow read of the doc's `clips[]` rather than a mirror of the editor's
/// `DirectorClip`: everything the packer does not use (LoRAs, keyframes, beats, v2v, characters)
/// is not merely unused here, it has no meaning on this engine at all, and naming those fields
/// would suggest otherwise.
#[derive(Debug, Clone, Default, Deserialize)]
struct H3TimelineClip {
    /// 🔴 Read by the AUTHORED path only (see [`authored_generations`]), not by the packer: a
    /// generation's `clip_ids[]` names clips by this, and that link is the only thing that says how
    /// long an authored generation runs. The packer walks clips in order and never needs it.
    #[serde(default)]
    id: Option<String>,
    /// The shot's attached image, as an `asset:<hash>` ref. How it reaches the model depends on
    /// [`h3_frame_mode`](Self::h3_frame_mode).
    #[serde(default)]
    image_b64: Option<String>,
    /// `"keyframe"` → the image is an omni `<Picture N>` on `ref2va` (role `frame_anchor`).
    /// Anything else (`"pin"`, absent) → it wire-pins the first frame on `fl2va`.
    /// Mirrors `h3FrameMode` in `web/src/lib/director/h3-generations.ts`.
    #[serde(default)]
    h3_frame_mode: Option<String>,
    #[serde(default)]
    prompt: String,
    /// Authored length. Falls back to the project's default shot length.
    #[serde(default)]
    seconds: Option<f64>,
    /// Shown in the generation's advisory `shots[]` so the UI can label a cut without reparsing
    /// the prompt. Never reaches the model.
    #[serde(default)]
    gist: Option<String>,
    /// 🔴 Start a NEW generation at this clip, accepting the seam. Same field the editor's own
    /// packer (`web/src/lib/director/h3-generations.ts`) reads.
    ///
    /// Deliberately NOT `new_scene`. On LTX `new_scene` means "render this fresh instead of
    /// continuing the previous shot's motion" — i.e. a cut — and on H3 a cut is free INSIDE a
    /// generation and better there. Reusing it would split a generation at every cut, which is
    /// precisely the chaining-for-a-cut mistake, paid once per cut in lost `<scenetrans>`.
    #[serde(default)]
    h3_break_before: Option<bool>,
}

/// How long a generation of THIS project may be, in frames.
///
/// 🔴 A project carrying a song renders to a SHORTER ceiling, because WanGP locks an audio
/// reference at 15 s and `H3_MAX_FRAMES` (362) is 15.083 s. Past it the generation has audio for
/// only part of its length and H3 INVENTS the tail — roughly the last second is the model guessing
/// at the song. Audible, and reported as the audio "going funny at the end".
///
/// Read off the DELIVERY block, the same field `parse_h3_soundtrack` reads later, so the length a
/// generation is packed to and the song muxed over it cannot disagree. The constants are
/// koblem-client's rather than local copies — the editor clamps to the same
/// `H3_MAX_FRAMES_WITH_AUDIO`, and two numbers meaning one ceiling is how they drift apart.
fn h3_frame_ceiling(timeline: &Value) -> u32 {
    let has_song = timeline
        .pointer("/request/soundtrack/audio")
        .and_then(Value::as_str)
        .is_some_and(|a| !a.trim().is_empty());
    if has_song {
        koblem_client::types::H3_MAX_FRAMES_WITH_AUDIO
    } else {
        koblem_client::types::H3_MAX_FRAMES
    }
}

/// Build the render's generations from the AUTHORED document — `timeline.h3_generations[]` —
/// rather than by re-deriving prose from `clips[]`.
///
/// 🔴 **The prompt is the source of truth.** A generation's authored block is the six-section
/// schema the editor composed and the author then edited by hand; it is shipped VERBATIM. The
/// sections UI is a parsed, two-way view of that text, not a separate authority, so nothing here
/// re-composes, re-words or re-orders it.
///
/// This exists because the render path used to look for `request.generations[]` and fall through to
/// [`pack_clips_into_generations`] when it was absent — and it was absent on every saved project,
/// so **every H3 render shipped clip prose instead of the authored schema**, with no references, no
/// steps and no LoRAs (measured 2026-08-09: a 3553-character authored block replaced by 476
/// characters of prose; `references: []` against three attached assets; 20 steps against an
/// authored 8; the turbo LoRA dropped). The packer is now the LAST resort, for a document that has
/// no authored generations at all.
///
/// What it assembles per generation, and from where:
///
/// | field | source |
/// |---|---|
/// | `prompt` | `h3_generations[].prompt`, verbatim |
/// | `frames` | the `clip_ids[]` clips' authored seconds, summed (`build_chain` snaps to 5+17k) |
/// | `references` | `timeline.h3_references[]`, filtered by `reference_ids[]` when the doc sets one |
/// | keyframe | each named clip's `image_b64` — an omni `frame_anchor` ref, or `first_frame` when the clip wire-pins |
/// | `steps` / `loras` | `editor.h3Steps` / `editor.h3Loras`, the project defaults `compile.ts` resolves |
///
/// ⚠️ **Ordinal order is a judgement call and it decides what `<Picture 1>` names.** The clip's own
/// keyframe is emitted FIRST, before the project references, because that is what the authored
/// prompts in existing documents mean by `<Picture 1>` (they describe the shot's own scene, and the
/// cast image is a crop of it). The editor's `keyframeReferences` appends instead. If a document
/// ever reads the other way the fix is the order of the two `extend` calls below — nothing else.
fn authored_generations(timeline: &Value, clips: &[H3TimelineClip]) -> Option<H3Packing> {
    use koblem_client::types::h3_snap_frames;

    let authored = timeline.get("h3_generations")?.as_array()?;
    if authored.is_empty() {
        return None;
    }

    // Project-wide references, addressed by id. `id` is editor-only bookkeeping and is stripped:
    // the wire type has no such field and the shim forbids extras, so leaving it on turns every
    // referenced render into a 400.
    let project_refs: Vec<(String, Value)> = timeline
        .get("h3_references")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .filter_map(|r| {
                    let mut o = r.as_object()?.clone();
                    let id = o.remove("id")?.as_str()?.to_string();
                    Some((id, Value::Object(o)))
                })
                .collect()
        })
        .unwrap_or_default();

    // Project defaults. `compile.ts` resolves these into every generation that named none of its
    // own, and they are the whole point of the turbo LoRA: the LoRA and its step count have to move
    // TOGETHER or the -39% is not collected.
    let ed = |k: &str| timeline.pointer(&format!("/editor/{k}"));
    let steps = ed("h3Steps").and_then(Value::as_u64);
    let loras: Vec<&str> = ed("h3Loras")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let lora_mults: Vec<f64> = ed("h3LoraMultipliers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();

    let default_seconds = timeline
        .pointer("/editor/shotSeconds")
        .and_then(Value::as_f64)
        .filter(|s| *s > 0.0)
        .unwrap_or(5.0);

    let ceiling = h3_frame_ceiling(timeline);

    let mut generations: Vec<Value> = Vec::with_capacity(authored.len());
    let mut shot_to_generation: Vec<usize> = Vec::new();

    for g in authored {
        // An empty block is not a render. `build_chain` filters these too, but doing it here keeps
        // `shot_to_generation` in step with the list that actually ships.
        let prompt = g.get("prompt").and_then(Value::as_str).unwrap_or("").trim();
        if prompt.is_empty() {
            continue;
        }
        let gi = generations.len();

        let clip_ids: Vec<&str> = g
            .get("clip_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mine: Vec<&H3TimelineClip> = clip_ids
            .iter()
            .filter_map(|want| {
                clips
                    .iter()
                    .find(|c| c.id.as_deref().is_some_and(|id| id == *want))
            })
            .collect();

        let mut o = serde_json::Map::new();
        o.insert("prompt".into(), json!(prompt));

        // Summed UNSNAPPED — `build_chain` snaps once, per generation. Snapping per clip and
        // summing would add up to a grid step of rounding per shot, and the cut times written
        // inside the prompt would then describe a timeline the render does not have.
        let seconds: f64 = mine
            .iter()
            .map(|c| c.seconds.filter(|s| *s > 0.0).unwrap_or(default_seconds))
            .sum();
        if seconds > 0.0 {
            // 🔴 SNAPPED here, not merely clamped, because `frames` is READ before `build_chain`
            // ever snaps it. `h3_soundtrack::generation_shape` takes this number as the generation's
            // rendered length and lays every slice from it, so an off-grid value silently
            // mis-positions the song: an authored 14.0 s emitted 336, the render produced 345, and
            // the next generation's slice started 0.375 s early — per generation, accumulating,
            // which is the "starts in sync, visibly late by the last chorus" failure
            // `h3-soundtrack.ts` opens by warning about. Measured 2026-08-10: koblem planned
            // 42.375 s for a cut the editor and the engine both make 43.042 s.
            //
            // Clamped BEFORE the snap, the same way `pack_clips_into_generations` does it a few
            // hundred lines down, so the file states the rule once. `ceiling` is itself on the
            // 5+17k grid, so it survives the snap — and clamping first is what keeps an authored
            // 14.4 s (346 frames, one past the ceiling) from snapping to 362 and rendering 15.083 s
            // against a 15 s audio lock, where the model invents the last second of the song.
            let raw = (seconds * H3_FPS as f64).round() as u32;
            o.insert("frames".into(), json!(h3_snap_frames(raw.min(ceiling))));
        }

        // ── Continuation ─────────────────────────────────────────────────────
        //
        // 🔴 The editor gives every generation but the first a continuation and does NOT persist it
        // — the packing is always re-derived (`h3-generations.ts`), so an absent key means "the
        // default", not "no join". This function read it as "no join", and the result was silent
        // and total: since it started building the request, EVERY generation past the first has
        // rendered as a fresh, unanchored start. The whole `continuation.from = "prev"` resolution
        // in `h3_single_generation_chain` — bank keys, selected takes, the `dropped` report — was
        // dead code, because nothing ever handed it a continuation to resolve.
        //
        // Emitting `{"mode": "latents"}` and letting serde fill the rest reproduces the editor's
        // `H3_CONTINUATION_DEFAULT` exactly (`from: "prev"`, `k: 7`, `anchor: true`) without
        // restating the numbers here, which is what keeps the two from drifting. An authored
        // block wins outright: a hard cut is `mode: "none"`, not an absence.
        //
        // ⚠️ Inert on a generation that wire-pins a frame — `derive_task` sends it to fl2va, where
        // the pin does nothing (measured 5% / -2% / 3%). Emitted anyway, because matching the
        // editor generation-for-generation is the property worth having; suppressing it here would
        // be a third rule about seams in a third place.
        if gi > 0 {
            let authored_cont = g.get("continuation").filter(|c| !c.is_null()).cloned();
            o.insert(
                "continuation".into(),
                authored_cont
                    .unwrap_or_else(|| json!({ "mode": H3ContinuationMode::Latents.as_str() })),
            );
        }

        // ── Steps and LoRAs: the generation's own, else the project's ────────
        //
        // 🔴 The precedence MIRRORS `compile.ts` (`compileH3Timeline`), deliberately: the wire has
        // no project level — every LoRA and every step count is a per-generation field
        // (`contract.py: Generation`) — so "the whole project renders at 8 steps with the turbo
        // LoRA" has to become that value on each generation, while a generation that overrode it
        // keeps its own. Two resolvers that disagree would make the same document render
        // differently depending on which path built the request, which is the class of bug this
        // whole function exists to end.
        //
        // ⚠️ The override test is the generation's own list being NON-EMPTY. An empty list means
        // "inherit", not "no LoRAs" — clearing the set is not representable on the wire, on either
        // engine. Same rule as `effectiveLoras` on the LTX side.
        let gen_loras: Vec<&str> = g
            .get("loras")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .filter(|v: &Vec<&str>| !v.is_empty())
            .unwrap_or_else(|| loras.clone());
        // The multipliers follow whichever list won, so a generation that overrides the LoRAs can
        // never inherit the project's and apply them positionally to different adapters.
        let overrode = g
            .get("loras")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        let gen_mults: Vec<f64> = if overrode {
            g.get("lora_multipliers")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default()
        } else {
            lora_mults.iter().copied().take(gen_loras.len()).collect()
        };
        let gen_steps = g
            .get("steps")
            .and_then(Value::as_u64)
            .filter(|s| *s > 0)
            .or(steps.filter(|s| *s > 0));

        if let Some(s) = gen_steps {
            o.insert("steps".into(), json!(s));
        }
        if !gen_loras.is_empty() {
            o.insert("loras".into(), json!(gen_loras));
            if !gen_mults.is_empty() {
                o.insert("lora_multipliers".into(), json!(gen_mults));
            }
        }

        // ── References ───────────────────────────────────────────────────────
        let mut refs: Vec<Value> = Vec::new();
        for c in &mine {
            let Some(image) = c.image_b64.as_deref().filter(|s| !s.trim().is_empty()) else {
                continue;
            };
            if c.h3_frame_mode.as_deref() == Some("keyframe") {
                refs.push(json!({
                    "kind": "image", "role": "frame_anchor", "image": image
                }));
            } else {
                // Wire-pinned: `fl2va`, and `build_chain` turns this into the first_frame
                // reference itself. A generation that pins cannot also carry omni references, and
                // it is that function's job to say so by name.
                o.insert("first_frame".into(), json!(image));
            }
        }
        // The author's own reference list. `reference_ids` scopes it per generation when the
        // editor writes one; absent (which is every document today) means the project's references
        // all apply, which is what a single-generation project means by them anyway.
        let scoped: Option<Vec<&str>> = g
            .get("reference_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect());
        refs.extend(project_refs.iter().filter_map(|(id, r)| match &scoped {
            Some(ids) if !ids.contains(&id.as_str()) => None,
            _ => Some(r.clone()),
        }));
        if !refs.is_empty() {
            o.insert("references".into(), Value::Array(refs));
        }

        generations.push(Value::Object(o));
        // One entry per shot the generation holds, which is what a retake naming a SHOT resolves
        // through. An authored generation with no clips still occupies one slot.
        shot_to_generation.extend(std::iter::repeat_n(gi, mine.len().max(1)));
    }

    (!generations.is_empty()).then_some(H3Packing {
        generations,
        shot_to_generation,
    })
}

/// The result of packing: the generations, and where each rendered shot ended up.
#[derive(Debug)]
struct H3Packing {
    generations: Vec<Value>,
    /// Rendered-shot index → generation index. What turns a retake request naming a SHOT into the
    /// generation that actually has to be re-rendered.
    shot_to_generation: Vec<usize>,
}

/// Walk the timeline's clips in order and pack them into generations.
///
/// A generation closes when the next clip would push it past [`H3_MAX_FRAMES`] (362 frames =
/// 15.083 s, the model's documented ceiling and upstream's own `window_default`), or when the clip
/// asks to start one. Nothing else splits: a cut is cheaper and better inside a generation than
/// across a boundary.
///
/// ⚠️ **Prompt composition here is a FALLBACK.** The frontend owns the structured six-section
/// editor and is the only thing that can compose H3's prompt schema properly — bare prose supplies
/// no `overall_soundscape` and no `non_diegetic_music` at all, leaving the model to invent the
/// entire audio bed unguided. When the document supplies a composed per-generation prompt (i.e.
/// `request.generations[]` already exists), the packer never runs and that text is used verbatim.
/// What this builds is the best that can be done from per-clip prompts alone: their text, joined
/// with the `[Shot N] At MM:SS.mmm` markers that ARE the shot structure.
///
/// 🔴 So the fallback emits `frames` / `prompt` / `shots` and NOTHING ELSE — no `steps`, no `seed`,
/// no `loras`, and none of the six authored sections. That is not an oversight to be filled in
/// here: every one of those is per-GENERATION authored state, and a document that reached this
/// branch has no generations to have authored it against (`request.generations[]` is what the
/// editor writes when it composes, and its absence is precisely what runs this code). Matching
/// stored generation documents back onto a packing derived here would be a second copy of the
/// frontend's packer, disagreeing with it silently the first time either changed. A document that
/// wants a turbo LoRA, a step count or a written soundscape has to be COMPOSED, which is what the
/// Director does on every save.
fn pack_clips_into_generations(
    clips: &[H3TimelineClip],
    default_seconds: f64,
    // The generation ceiling in frames. `H3_MAX_FRAMES` normally; the shorter
    // `H3_MAX_FRAMES_WITH_AUDIO` when the project carries a song — WanGP locks an audio reference
    // at 15 s, and a generation past that has audio for only part of its length, so H3 invents the
    // tail. Passed in rather than read here so this function and `authored_generations` cannot
    // disagree about how long a generation may be.
    ceiling: u32,
) -> Result<H3Packing, String> {
    use koblem_client::types::{H3_MAX_FRAMES, H3_MIN_FRAMES, h3_seconds_from_frames};

    // Empty-prompt clips are filtered exactly as the LTX path filters empty segments, so the
    // rendered indices are the ones the Director's persisted rows use.
    let rendered: Vec<&H3TimelineClip> = clips
        .iter()
        .filter(|c| !c.prompt.trim().is_empty())
        .collect();
    if rendered.is_empty() {
        return Err("the timeline has no shots with prompts to render".to_string());
    }

    // Raw (un-snapped) frames per clip. Snapping happens once, per GENERATION: snapping each clip
    // and summing would add up to 16 frames of grid rounding per shot, and the cut times inside the
    // generation would then describe a timeline the render does not have.
    let mut raw_frames: Vec<u32> = Vec::with_capacity(rendered.len());
    for (i, c) in rendered.iter().enumerate() {
        let seconds = c.seconds.filter(|s| *s > 0.0).unwrap_or(default_seconds);
        let frames = (seconds * H3_FPS as f64).round().max(1.0) as u32;
        // 🔴 Refused against the HARD limit, not `ceiling`, and the two are deliberately different
        // numbers. `ceiling` is a mux concern — it trims the tail the song cannot cover — while
        // this is the engine's "there is no single render this long", which is what makes a seam
        // unavoidable. Pointing the refusal at `ceiling` turns the clamp into an error for the very
        // case it was added to handle: 14.4 s rounds to 346 against an audio ceiling of 345, so a
        // shot that only needs 0.025 s trimmed off gets rejected instead, and `flush`'s
        // `.min(ceiling)` becomes unreachable — `would_overflow` already holds every multi-clip
        // generation under `ceiling`, so a lone over-ceiling clip was its only remaining caller.
        if frames > H3_MAX_FRAMES {
            return Err(format!(
                "shot {} is {seconds:.1}s; H3 renders at most {:.3}s in one go and a shot cannot \
                 be split across generations without a seam. Shorten it, or split it into two \
                 shots in the timeline",
                i + 1,
                h3_seconds_from_frames(H3_MAX_FRAMES)
            ));
        }
        raw_frames.push(frames);
    }

    // Emit the generation accumulated in `current`. A free function rather than a closure: it has
    // to run interleaved with reads of `generations.len()` (that IS the generation index a shot is
    // filed under), which a closure holding a mutable borrow of the same vec cannot allow.
    fn flush(
        out: &mut Vec<Value>,
        current: &mut Vec<usize>,
        current_frames: &mut u32,
        rendered: &[&H3TimelineClip],
        raw_frames: &[u32],
        ceiling: u32,
    ) {
        use koblem_client::types::{h3_format_cut_time, h3_snap_frames};
        if current.is_empty() {
            return;
        }
        // Snapped UP onto `5 + 17k`, then clamped — so the render is at least as long as
        // authored, and at most one grid step longer.
        //
        // 🔴 It used to be clamped at 107 frames as well, which silently LENGTHENED any
        // generation under 4.458 s — a 2-second authored beat rendered as 4.458 s with no error
        // anywhere, and `h3-generations.ts` then made the last shot absorb the difference. The
        // floor is now the VAE's own decodable minimum (22), so the authored length survives;
        // being under MiniMax's documented 4 s is REPORTED once by `build_chain` instead.
        // 🔴 CLAMPED BEFORE THE SNAP, which rounds UP. A generation one frame over the ceiling
        // snaps to the next grid step and lands past it — an authored 14.4 s is 345.6 frames, which
        // rounds to 346 and snaps to 362, i.e. 15.083 s against a 15 s audio lock, and H3 invents
        // the last second of the song. `ceiling` is on the 5+17k grid, so it survives the snap.
        let frames = h3_snap_frames((*current_frames).min(ceiling));
        let multi = current.len() > 1;
        let mut shots: Vec<Value> = Vec::with_capacity(current.len());
        let mut prompt = String::new();
        let mut at = 0.0f64;
        for (n, &ci) in current.iter().enumerate() {
            let clip = rendered[ci];
            let mut shot = json!({ "index": n });
            if n == 0 {
                // `[Shot 1]` is untimed — the generation simply starts there. And a SINGLE-shot
                // generation carries no marker at all: that is the plain request the model was
                // trained on, and prefixing `[Shot 1]` to it changes text the model reads.
                if multi {
                    prompt.push_str("[Shot 1]\n");
                }
            } else {
                shot["at_seconds"] = json!(at);
                prompt.push_str(&format!(
                    "\n\n[Shot {}] At {}\n",
                    n + 1,
                    h3_format_cut_time(at)
                ));
            }
            if let Some(label) = clip
                .gist
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                shot["label"] = json!(label);
            }
            prompt.push_str(clip.prompt.trim());
            shots.push(shot);
            at += raw_frames[ci] as f64 / H3_FPS as f64;
        }
        out.push(json!({
            "frames": frames,
            "prompt": prompt,
            "shots": shots,
        }));
        current.clear();
        *current_frames = 0;
    }

    let mut generations: Vec<Value> = Vec::new();
    let mut shot_to_generation: Vec<usize> = vec![0; rendered.len()];
    // Indices (into `rendered`) of the clips accumulated for the generation being built.
    let mut current: Vec<usize> = Vec::new();
    let mut current_frames: u32 = 0;

    for (i, c) in rendered.iter().enumerate() {
        let starts_new = c.h3_break_before.unwrap_or(false);
        let would_overflow = current_frames + raw_frames[i] > ceiling;
        if !current.is_empty() && (starts_new || would_overflow) {
            flush(
                &mut generations,
                &mut current,
                &mut current_frames,
                &rendered,
                &raw_frames,
                ceiling,
            );
        }
        shot_to_generation[i] = generations.len();
        current.push(i);
        current_frames += raw_frames[i];
    }
    flush(
        &mut generations,
        &mut current,
        &mut current_frames,
        &rendered,
        &raw_frames,
        ceiling,
    );

    debug_assert!(!generations.is_empty());
    // Nothing may pack under the DECODABLE floor by accident: a single 0.1-second clip is a legal
    // timeline and must produce a renderable generation rather than a VAE crash. (This used to
    // assert the 107-frame guideline, which is why every short timeline came out lengthened.)
    debug_assert!(
        generations
            .iter()
            .all(|g| g["frames"].as_u64().unwrap_or(0) >= H3_MIN_FRAMES as u64)
    );
    Ok(H3Packing {
        generations,
        shot_to_generation,
    })
}

/// Steps an H3 preview renders at when neither the editor nor a recorded geometry says otherwise.
///
/// 8 against the RUNBOOK's 20, i.e. ~2x cheaper — not the ~20x that a quarter-resolution canvas
/// bought on LTX. See [`apply_h3_preview_steps`] for why that trade is the only one on offer here.
const H3_DEFAULT_PREVIEW_STEPS: u64 = 8;

/// The step count an H3 PREVIEW render uses, in precedence order.
///
/// The editor's own "Preview steps" setting first — the same `timeline.editor` block
/// [`h3_request_for`] already reads `shotSeconds` from, so the control the user can see is the
/// control that acts. Then a previously recorded `metadata.proxy_render.steps`, which is what an
/// LTX-compiled proxy stamps and what a project converted from LTX is carrying. Then the default.
fn h3_preview_steps(project: &repo::DirectorProjectRow) -> u64 {
    let from_editor = project
        .timeline
        .pointer("/editor/proxySteps")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0);
    let from_metadata = project
        .metadata
        .pointer("/proxy_render/steps")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0);
    // The engine's own bound is 1..=100 (`contract.py`'s `Generation.steps`); clamping here means a
    // typo in the editor is a cheap preview rather than a 422 from the shim.
    from_editor
        .or(from_metadata)
        .unwrap_or(H3_DEFAULT_PREVIEW_STEPS)
        .clamp(1, 100)
}

/// Bring an H3 request down to its PREVIEW tier — which changes the step count and nothing else.
///
/// 🔴 Deliberately not [`apply_proxy_geometry`], and deliberately not a smaller canvas either. LTX's
/// proxy tier is a quarter-resolution pass; on H3 that makes the preview **non-predictive of the
/// render it is previewing**, which is the one thing a preview is for:
///
/// - `pipeline.py:360-363` draws the initial noise as
///   `randn((1, 24, latent_t, latent_h, latent_w), generator=manual_seed(seed))`. At 512x288 the
///   latent grid is 36x64; at 1344x768 it is 96x168. A seeded `randn` on a different SHAPE is a
///   different draw, not a downsample of the same one — no element-wise correspondence at all, so
///   composition, camera path and subject placement are a fresh roll at the same seed.
/// - The AUDIO noise is drawn from that same generator immediately afterwards, so it starts from a
///   stream advanced by a different number of elements. Even the music bed at seed S is unrelated.
/// - RoPE does not rescue it. `packing.py::_frame_grid` normalises the positional extent by
///   `sqrt(latent_h * latent_w)`, so the extent is a function of ASPECT RATIO and is near
///   scale-invariant — which is exactly why the described CONTENT transfers while the specific roll
///   does not. Scale-invariance of the positional field is not seed reproducibility.
///
/// Holding the canvas fixed and dropping only the steps gives byte-identical initial noise: the
/// same framing, the same subject placement and the same audio bed as the committed render, merely
/// under-converged. Peak VRAM is unchanged (~11 GB) — which is the binding constraint anyway, so
/// the cheaper-canvas version was not buying headroom either.
///
/// ⚠️ It follows that the preview clip is the same file SIZE as the final one, so on H3 the proxy
/// tier no longer means "fewer bytes for a phone". It means "same render, sooner".
fn apply_h3_preview_steps(request: &mut serde_json::Map<String, Value>, steps: u64) {
    let Some(gens) = request.get_mut("generations").and_then(Value::as_array_mut) else {
        return;
    };
    for g in gens {
        if let Some(g) = g.as_object_mut() {
            g.insert("steps".to_string(), json!(steps));
        }
    }
}

/// A project's saved timeline → the H3 request JSON that will actually be submitted, plus the
/// rendered-shot → generation map.
///
/// The two ways a document can arrive here, in priority order:
///
///   1. **`request.generations[]` already exists.** The frontend composed them — including each
///      generation's prompt, which is the structured six-section schema block and the only form
///      that gives the model an `overall_soundscape` and a `non_diegetic_music` section. Used
///      VERBATIM; nothing here re-packs or re-words it.
///   2. **The authored document: `timeline.h3_generations[]`.** See [`authored_generations`]. The
///      authored block is shipped verbatim, with the project's references, steps and LoRAs.
///   3. **Last resort: pack `timeline.clips[]`.** For a document with no authored generations at
///      all. See [`pack_clips_into_generations`] — including why its prompt composition is a
///      fallback and not the intended path.
///
/// 🔴 (2) exists because (1) is absent on every saved project — measured 2026-08-09 across all five
/// — so every H3 render took (3) and shipped clip prose. Do not "simplify" this back to two arms
/// without first checking that a saved document actually carries `request.generations[]`.
async fn h3_request_for(
    state: &AppState,
    project: &repo::DirectorProjectRow,
    tier: &str,
) -> Result<
    (
        Value,
        Vec<usize>,
        director_assets::StagedParts,
        Option<f64>,
        // Head frames the slice offsets assumed the seams would drop — beside `sliced_for_seconds`
        // because they are the two halves of one question and only ever set together.
        u32,
        Vec<f64>,
    ),
    String,
> {
    let Some(req_val) = project.timeline.get("request") else {
        return Err("project timeline has no 'request' to render".to_string());
    };
    let mut req_val = req_val.clone();
    let Some(request) = req_val.as_object_mut() else {
        return Err("invalid render request in timeline".to_string());
    };

    let shot_to_generation = h3_compile_generations(project, request)?;

    if tier == "proxy" {
        apply_h3_preview_steps(request, h3_preview_steps(project));
    }
    h3_finish_request(state, project, tier, req_val, shot_to_generation).await
}

/// Fill in `request.generations[]` from whichever of the three sources the document has, and return
/// the rendered-shot → generation map.
///
/// Split out of [`h3_request_for`] because it is the only part a caller can afford to run just to
/// LOOK: it is pure document arithmetic, where the rest of `h3_request_for` cuts soundtrack slices
/// with ffmpeg and reads every reference off disk. [`h3_render_plan`] needs the compiled generations
/// to compare prompts against the takes table, and runs on a button hover — it must not pay for a
/// render's worth of I/O to answer "what would this do".
///
/// 🔴 Mutates `request` in place, exactly as the inline version did, so both callers see the same
/// document a render would submit.
fn h3_compile_generations(
    project: &repo::DirectorProjectRow,
    request: &mut serde_json::Map<String, Value>,
) -> Result<Vec<usize>, String> {
    let composed: Vec<usize> = request
        .get("generations")
        .and_then(Value::as_array)
        .map(|gens| {
            // The doc composed them. Its `shots[]` is what tells a retake which generation holds a
            // given shot — the same map the packer would have produced.
            let mut map = Vec::new();
            for (gi, g) in gens.iter().enumerate() {
                let shots = g
                    .get("shots")
                    .and_then(Value::as_array)
                    .map(|s| s.len())
                    .unwrap_or(1)
                    .max(1);
                map.extend(std::iter::repeat_n(gi, shots));
            }
            map
        })
        .filter(|m| !m.is_empty())
        .unwrap_or_default();

    let shot_to_generation = if composed.is_empty() {
        let clips: Vec<H3TimelineClip> = project
            .timeline
            .get("clips")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| format!("timeline clips are unreadable: {e}"))?
            .unwrap_or_default();
        // 🔴 AUTHORED FIRST, and BEFORE the no-clips refusal — an authored block is a renderable
        // document whether or not the timeline still has clips to measure it against. The packer
        // below only ever had clip prose, and falling through to it while an authored block sat in
        // the same document is what shipped 476 characters of prose in place of a 3553-character
        // schema, with no references, no steps and no LoRAs. See `authored_generations`.
        if let Some(authored) = authored_generations(&project.timeline, &clips) {
            request.insert("generations".into(), Value::Array(authored.generations));
            authored.shot_to_generation
        } else {
            if clips.is_empty() {
                return Err(
                    "the timeline has no authored `h3_generations` and no `clips` to pack into \
                     generations"
                        .to_string(),
                );
            }
            // The project's default shot length, from the editor's own setting. 5 s is what a new
            // clip gets, so a doc that never recorded one behaves as the editor does.
            let default_seconds = project
                .timeline
                .pointer("/editor/shotSeconds")
                .and_then(Value::as_f64)
                .filter(|s| *s > 0.0)
                .unwrap_or(5.0);
            // The same ceiling the authored path uses, from the same helper — a document that
            // falls through to the packer is still held to the soundtrack's 15 s audio lock.
            let packed = pack_clips_into_generations(
                &clips,
                default_seconds,
                h3_frame_ceiling(&project.timeline),
            )?;
            request.insert("generations".into(), Value::Array(packed.generations));
            packed.shot_to_generation
        }
    } else {
        composed
    };
    Ok(shot_to_generation)
}

/// Why one generation needs rendering again. `None` means it does not.
///
/// Ordered by how it reads to the person deciding: their own edit first, then a consequence of it,
/// then the dispatch's own constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum H3Staleness {
    /// No selected take at this index. A shot that has never rendered, or whose take was deleted.
    NeverRendered,
    /// The take exists but was rendered from different prompt text.
    PromptChanged,
    /// This generation is fine, but it ANCHORS to one that is being re-rendered — so the latents it
    /// continued from are about to stop existing, and keeping it would join it to a predecessor
    /// that no longer appears anywhere in the timeline.
    ContinuesFromStale,
    /// Neither stale nor able to be skipped: it sits between two generations that are, and a span
    /// is dispatched as one job. See [`h3_span_chain_from`].
    InsideTheSpan,
}

/// What a "continue" would do, and what a "re-render everything" would destroy.
#[derive(Debug, serde::Serialize)]
struct H3RenderPlan {
    /// One entry per compiled generation, in render order. `None` = has a current take, skipped.
    generations: Vec<Option<H3Staleness>>,
    /// The contiguous span a continue would render, inclusive. `None` when nothing is stale.
    span: Option<(usize, usize)>,
    /// Takes a full re-render deletes — every take of this tier, not merely the selected ones.
    existing_takes: usize,
}

impl H3RenderPlan {
    /// How many generations a continue actually renders.
    fn span_len(&self) -> usize {
        self.span.map(|(a, b)| b - a + 1).unwrap_or(0)
    }
}

/// Work out which generations a "continue" has to render.
///
/// # Staleness PROPAGATES, and that is the part worth getting right
///
/// The naive rule — "render the ones with no take, plus the ones whose prompt changed" — is wrong
/// on this engine, and wrong in a way that produces a plausible cut with a broken seam in it.
/// Generation `k+1` continues from generation `k`'s banked latents. Re-render `k` and those latents
/// are replaced, so `k+1` is now joined to a version of its predecessor that appears nowhere in the
/// timeline. It has to be re-rendered too, even though nothing about it changed.
///
/// So staleness runs FORWARD along the continuation chain — but only across ANCHORED seams. A
/// generation the author made a hard cut (`continuation.mode = "none"`, or no continuation at all)
/// depends on nothing before it, which is what stops one edit at the top of a long timeline from
/// condemning the whole thing.
///
/// # Prompt comparison is against the TAKE, not against the last run
///
/// Each take row stores the prompt it was rendered from (`director_shot_renders.prompt`, written by
/// the harvest from the job's own `scripts`). Comparing the compiled generation's prompt against
/// the SELECTED take's is what makes this survive retakes, take selection, and edits made between
/// runs — none of which a "last rendered at" timestamp would get right.
async fn h3_render_plan(
    state: &AppState,
    project: &repo::DirectorProjectRow,
    tier: &str,
) -> Result<H3RenderPlan, String> {
    let Some(req_val) = project.timeline.get("request") else {
        return Err("project timeline has no 'request' to render".to_string());
    };
    let mut req_val = req_val.clone();
    let Some(request) = req_val.as_object_mut() else {
        return Err("invalid render request in timeline".to_string());
    };
    h3_compile_generations(project, request)?;
    let parsed: super::minimax_h3::H3Request = serde_json::from_value(req_val.clone())
        .map_err(|e| format!("invalid render request in timeline: {e}"))?;

    let selected = repo_renders::selected_shots(&state.db, &project.id, tier)
        .await
        .map_err(|e| format!("reading this project's takes: {e}"))?;
    let all_takes = repo_renders::list_by_project(&state.db, &project.id)
        .await
        .map_err(|e| format!("reading this project's takes: {e}"))?;
    // Every take of THIS tier, including the stitched cut at -1: a full run's wipe drops all of
    // them, so that is the number the warning has to quote.
    let existing_takes = all_takes.iter().filter(|t| t.tier == tier).count();

    // `(prompt, anchored)` per generation, and the selected take's prompt per generation — the only
    // two things the rule needs, lifted out so the rule itself is testable without a database.
    //
    // `anchor` carries a serde default of TRUE, so a continuation present but silent about it
    // deserialises as anchored, which matches the shim. Reading the typed field rather than the raw
    // JSON is what gets that for free.
    let shape: Vec<(String, bool)> = parsed
        .generations
        .iter()
        .map(|g| {
            let anchored = g
                .continuation
                .as_ref()
                .map(|c| c.anchor && c.mode != H3ContinuationMode::None)
                .unwrap_or(false);
            (g.prompt.clone(), anchored)
        })
        .collect();
    // 🔴 Takes are filed in RENDERED generation space (`build_chain` drops empty-prompt
    // generations) and this walks COMPILED space. The two coincide for every document the packer
    // and the authored path produce — neither ever emits a promptless generation — and the span
    // handed to `h3_span_chain_from` has to be in compiled space, so converting here would trade a
    // theoretical mismatch for a real one. Asserted rather than assumed silently.
    debug_assert!(
        parsed
            .generations
            .iter()
            .all(|g| !g.prompt.trim().is_empty()),
        "a promptless generation puts compiled and rendered indices out of step"
    );
    let takes: Vec<Option<String>> = (0..shape.len())
        .map(|i| {
            selected
                .iter()
                .find(|t| t.shot_index == i as i32)
                .map(|t| t.prompt.clone())
        })
        .collect();

    let (generations, span) = h3_staleness(&shape, &takes);
    Ok(H3RenderPlan {
        generations,
        span,
        existing_takes,
    })
}

/// The staleness rule itself: which generations need re-rendering, and the span that covers them.
///
/// Pure, so the propagation rule — the part that is easy to get subtly wrong and impossible to
/// notice — is testable without a project, a database or an engine. See [`h3_render_plan`] for why
/// staleness travels forward across anchored seams.
///
/// `shape` is `(prompt, anchors_to_predecessor)` per generation; `takes[i]` is the prompt the
/// SELECTED take at generation `i` was rendered from, or `None` when there is no take.
fn h3_staleness(
    shape: &[(String, bool)],
    takes: &[Option<String>],
) -> (Vec<Option<H3Staleness>>, Option<(usize, usize)>) {
    let mut out: Vec<Option<H3Staleness>> = Vec::with_capacity(shape.len());
    let mut previous_is_stale = false;
    for (i, (prompt, anchored)) in shape.iter().enumerate() {
        let reason = match takes.get(i).and_then(Option::as_ref) {
            None => Some(H3Staleness::NeverRendered),
            Some(t) if t.trim() != prompt.trim() => Some(H3Staleness::PromptChanged),
            Some(_) if previous_is_stale && *anchored => Some(H3Staleness::ContinuesFromStale),
            Some(_) => None,
        };
        previous_is_stale = reason.is_some();
        out.push(reason);
    }

    // The span, and everything caught inside it. A generation between two stale ones is NOT stale —
    // it is collateral of dispatching a span as a single job — so it is labelled differently, and
    // the editor can tell the user it is being re-rendered for a reason that is not its own.
    let span = out.iter().position(Option::is_some).map(|f| {
        let l = out
            .iter()
            .rposition(Option::is_some)
            .expect("a first implies a last");
        (f, l)
    });
    if let Some((f, l)) = span {
        for slot in out.iter_mut().take(l + 1).skip(f) {
            if slot.is_none() {
                *slot = Some(H3Staleness::InsideTheSpan);
            }
        }
    }
    (out, span)
}

/// The rest of [`h3_request_for`]: cut the soundtrack, stage the references, hand back the wire
/// document. Separated only so the compile above can be reused on its own — nothing here is
/// optional for a render.
async fn h3_finish_request(
    state: &AppState,
    project: &repo::DirectorProjectRow,
    tier: &str,
    mut req_val: Value,
    shot_to_generation: Vec<usize>,
) -> Result<
    (
        Value,
        Vec<usize>,
        director_assets::StagedParts,
        Option<f64>,
        u32,
        Vec<f64>,
    ),
    String,
> {
    // Same edge as the LTX path, but STAGED rather than expanded: H3 takes its references as
    // multipart parts, so a ref becomes a `part:<name>` marker and its bytes come back beside the
    // document. Expanding to base64 here — which is what this used to do — meant `build_references`
    // immediately decoded the string back to the bytes we had just read off disk, and an
    // unresolvable ref fell through to the shim as "no such path" naming a hash.
    //
    // The walk is structural, so it covers reference images, reference video containers and
    // reference audio without naming any of them. `ref_` is H3's documented part-name convention
    // (`docker/h3-wangp/API.md` §2) and its shim keeps a legacy alias keyed on that prefix.
    // 🔴 BEFORE `stage_refs`, and that ordering is the whole integration. The soundtrack pass only
    // replaces each `soundtrack:` sentinel with the `asset:<hash>` of THIS generation's span of the
    // song; the staging walk below then treats it as any other reference asset. Nothing downstream —
    // ordinal allocation, `audio_count <= visual_count`, the wire encoding — learns that a
    // soundtrack exists, which is why none of it needed changing.
    //
    // Slices are cut for every generation, INCLUDING on the retake path: the retake narrows the
    // generation list afterwards, so generation N keeps the slice cut for N rather than for 0.
    let soundtrack = parse_h3_soundtrack(&req_val);
    // 🔴 All three outcomes are distinct and only one of them is "carry on". `prepare` below turns a
    // `None` song into "soundtrack.audio did not resolve to any bytes", which says nothing about
    // WHICH way it failed — and this is the pass that runs BEFORE any GPU time is spent, so it is
    // the cheapest possible place to say it precisely.
    let song = match soundtrack.as_ref() {
        Some(s) => {
            match director_assets::resolve_media(state.director_assets_dir.as_ref(), &s.audio).await
            {
                Ok(Some(bytes)) => Some(bytes),
                Ok(None) => {
                    return Err(
                        "the project soundtrack carries no audio — the block is set but \
                                its audio field is empty; re-attach the song"
                            .to_string(),
                    );
                }
                Err(e) => return Err(format!("reading the project soundtrack: {e}")),
            }
        }
        None => None,
    };
    let prepared = super::h3_soundtrack::prepare(
        soundtrack.as_ref(),
        song.as_ref(),
        &mut req_val,
        // The store the finished slices are written into...
        state.director_assets_dir.as_ref(),
        // ...and a scratch root for ffmpeg's working files, which are NOT assets and must not be
        // left in a content-addressed directory. `prepare` makes and removes its own subdirectory.
        &std::env::temp_dir(),
    )
    .await?;
    if let Some(prepared) = &prepared {
        for w in &prepared.warnings {
            warn!(project = %project.id, tier, "h3 soundtrack: {w}");
        }
    }
    // What the slices were CUT for. Carried through to the render, which compares it against the
    // finished clip — the only thing that would ever catch `H3_TRIM_JOIN` drifting out of step with
    // the shim's own `trim_join`. See `h3_soundtrack::drift_note`.
    let sliced_for_seconds = prepared.as_ref().map(|p| p.plan.delivered_seconds);
    // What the offsets ASSUMED the seams would remove — checked against the shim's own
    // `trimmed_head_total`, which is the same quantity measured on the file it delivered. Without
    // it the guard has only a duration, and a duration cannot tell a trim rule that MOVED from a
    // trim that CRASHED — it reported the first for the second on 2026-08-11.
    let expected_trim_frames = prepared
        .as_ref()
        .map(|p| p.expected_trim_frames)
        .unwrap_or(0);
    // Where each generation's first surviving frame lands in the song, in COMPILED generation
    // order — the same order `request.generations` is in, which is the space a retake's
    // `generation_index` is expressed in. `h3_chain_for` narrows it to rendered space; the retake
    // path indexes it directly. Empty when the project has no soundtrack, which is the signal
    // downstream that there is nothing to mux.
    //
    // 🔴 Present for EVERY generation, including ones the plan skipped conditioning for. A shot the
    // author scoped the song away from still plays under the muxed song — what it opted out of is
    // singing to it, not the soundtrack itself — so it needs its offset like any other.
    let delivery_offsets: Vec<f64> = prepared
        .as_ref()
        .map(|p| {
            p.plan
                .slices
                .iter()
                .map(|s| s.delivered_at_seconds)
                .collect()
        })
        .unwrap_or_default();

    let staged =
        director_assets::stage_refs(state.director_assets_dir.as_ref(), &mut req_val, "ref_")
            .await
            .map_err(|missing| director_assets::missing_refs_message(&missing))?;
    Ok((
        req_val,
        shot_to_generation,
        staged,
        sliced_for_seconds,
        expected_trim_frames,
        delivery_offsets,
    ))
}

/// Narrow COMPILED-space song offsets to RENDERED-generation space.
///
/// 🔴 `build_chain` drops empty-prompt generations, and the engine's partials are numbered in what
/// survives that filter — so a compiled-space offset list handed straight to the job would be off
/// by one per dropped generation, laying the wrong shot's music over everything after it. The two
/// spaces are identical for every document the packer and the authored path produce today; this
/// exists so that stays true by construction rather than by luck.
fn h3_offsets_in_rendered_space(
    generations: &[super::minimax_h3::H3Generation],
    compiled: &[f64],
) -> Vec<f64> {
    generations
        .iter()
        .zip(compiled)
        .filter(|(g, _)| !g.prompt.trim().is_empty())
        .map(|(_, offset)| *offset)
        .collect()
}

/// The project's soundtrack block, if it set one.
///
/// Read off the raw document rather than the parsed [`super::minimax_h3::H3Request`] because this
/// runs before the parse — the sentinels have to be resolved into real assets first, or the parse
/// would be validating references whose payload does not exist yet.
fn parse_h3_soundtrack(req: &Value) -> Option<koblem_client::types::H3Soundtrack> {
    serde_json::from_value(req.get("soundtrack")?.clone()).ok()
}

/// Parse + build an H3 project's saved request into the engine chain. `Err` is the 400 body.
async fn h3_chain_for(
    state: &AppState,
    project: &repo::DirectorProjectRow,
    tier: &str,
) -> Result<(super::minimax_h3::H3Chain, bool), String> {
    let (
        req_val,
        _shot_to_generation,
        staged,
        sliced_for_seconds,
        expected_trim_frames,
        delivery_offsets,
    ) = h3_request_for(state, project, tier).await?;
    let parsed: super::minimax_h3::H3Request = serde_json::from_value(req_val.clone())
        .map_err(|e| format!("invalid render request in timeline: {e}"))?;
    let save_clip = parsed.save_clip;
    let mut chain = super::minimax_h3::build_chain(&parsed, &req_val, &staged)?;
    chain.sliced_for_seconds = sliced_for_seconds;
    chain.expected_trim_frames = expected_trim_frames;
    chain.delivery_offsets = h3_offsets_in_rendered_space(&parsed.generations, &delivery_offsets);
    Ok((chain, save_clip))
}

/// `POST …/projects/{id}/run` for an H3 project. The LTX path's shape, minus everything H3 has no
/// concept of: no audio parts (it takes no audio input), no resume, no critique recursion guard on
/// a bank that does not exist.
///
/// ⚠️ The job's `segments` count and its `scripts` list are per GENERATION, not per shot — the
/// timeline's shots were packed by [`h3_request_for`] and a generation may hold several. Everything
/// downstream (partials, takes, the harvest) is in that space too.
async fn run_project_h3(
    state: AppState,
    headers: &HeaderMap,
    id: String,
    tier: &'static str,
    project: repo::DirectorProjectRow,
    stale_only: bool,
) -> Response {
    if !state.minimax_h3.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "minimax-h3 service not configured",
        );
    }
    // 🔴 `stale_only` changes THREE things together, and they only make sense together: which
    // generations are submitted, which project index the job files them under, and whether the
    // harvest replaces the tier. A continue that rendered a span but still wiped would delete
    // exactly the takes it was invoked to preserve, so `wipe` is derived here rather than passed.
    let (chain, save_clip, shot_offset) = if stale_only {
        let plan = match h3_render_plan(&state, &project, tier).await {
            Ok(p) => p,
            Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
        };
        let Some((first, last)) = plan.span else {
            // Not an error, and deliberately not a dispatch either: 200 with nothing started, so the
            // editor can say "everything is up to date" instead of showing a render that instantly
            // finishes.
            return (
                StatusCode::OK,
                Json(json!({
                    "status": "up_to_date",
                    "tier": tier,
                    "engine": ENGINE_H3,
                    "rendered": 0,
                })),
            )
                .into_response();
        };
        // The same span dispatch a retake uses, with wider bounds — see `h3_span_chain_from`.
        let (req_val, _map, staged, _sliced, _trim_frames, offsets) =
            match h3_request_for(&state, &project, tier).await {
                Ok(v) => v,
                Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
            };
        match h3_span_chain_from(
            req_val, staged, offsets, &state, &project, tier, first, last,
        )
        .await
        {
            Ok((chain, first_index, save_clip, _pinned)) => (chain, save_clip, Some(first_index)),
            Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
        }
    } else {
        match h3_chain_for(&state, &project, tier).await {
            Ok((chain, save_clip)) => (chain, save_clip, None),
            Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
        }
    };
    // Record what the preview tier was actually rendered at. Steps only — the canvas is the
    // project's own, deliberately (see [`apply_h3_preview_steps`]), so stamping a width/height here
    // would record a downscale that never happened and would be read back as one.
    if tier == "proxy"
        && let Err(e) = repo::patch_metadata(
            &state.db,
            &id,
            &json!({ "proxy_render": { "steps": h3_preview_steps(&project) } }),
        )
        .await
    {
        warn!(error = %e, project = %id, "director run: preview steps stamp failed");
    }

    let who = match super::queue::principal_of(&state, headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let prompts = chain.prompts.clone();
    let generation_count = prompts.len();
    let job_id = state.ltx_jobs.create_director_on(
        generation_count as u32,
        prompts,
        id.clone(),
        tier.into(),
        Some(ENGINE_H3.to_string()),
        // A full run's shot indices ARE the project's; a continue's are offset by the first
        // generation of its span, exactly as a retake's are by the one generation it holds.
        shot_offset,
    );
    if tier == "final"
        && let Err(e) = repo::patch_metadata(
            &state.db,
            &id,
            &json!({ "last_job_id": job_id, "last_run_at": now_rfc3339() }),
        )
        .await
    {
        warn!(error = %e, "director run: metadata stamp failed");
    }

    // 🔴 ONE ENQUEUE for the whole cut, not one per generation. `job.segments`, `generations_done`
    // and `partials[]` all live on this one record — splitting it would give the editor N renders of
    // one shot each and nothing that adds up to the cut. Retakes are their own jobs, correctly.
    //
    // A fresh full run replaces the whole persisted set FOR ITS TIER (wipe = true). A continue must
    // NOT: its whole purpose is that the takes it did not re-render survive, and it does not hold a
    // take for every shot to swap in.
    let job_id = match super::minimax_h3::enqueue_h3_render(
        &state,
        &who,
        chain,
        super::minimax_h3::H3Dispatch {
            job_id,
            save_clip,
            director_project_id: id,
            director_tier: tier.to_string(),
            director_shot_offset: shot_offset,
            director_wipe: !stale_only,
            // A `/run` IS a full run, so it always offers the project's `timeline.critique` setting
            // the chance to fire; the retake path deliberately does not, or the critic's own
            // re-rolls would recurse.
            director_critique: true,
        },
        crate::routes::queue::QueueOptions::from_headers(headers),
    )
    .await
    {
        Ok(job_id) => job_id,
        Err((code, msg)) => return err(code, &msg),
    };

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "status": "queued",
            "tier": tier,
            "engine": ENGINE_H3,
            "scope": if stale_only { "stale" } else { "all" },
            // What this job actually renders, and where the harvest files its first generation —
            // the editor needs both to say "rendering shots 2-3" rather than "rendering 2 shots".
            "rendered": generation_count,
            "shot_offset": shot_offset.unwrap_or(0),
        })),
    )
        .into_response()
}

/// `GET …/projects/{id}/render-plan?tier=final|proxy` — what a run would render, and what a full
/// one would destroy. Read-only; the editor asks before showing the Render all choice.
///
/// 🔴 Answers `{"supported": false}` for an LTX project rather than 404 or an error. LTX has its own
/// resume machinery (a banked prefix and a splice) that this does not model, and inventing a
/// staleness answer for it would be a confident wrong one. The editor falls back to today's
/// behaviour on `supported: false`, which is the correct behaviour for that engine.
async fn render_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<TierQuery>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = tier_or_final(q.tier.as_deref()) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };
    let project = match repo::get(&state.db, &id).await {
        Ok(Some(p)) => p,
        Ok(None) => return err(StatusCode::NOT_FOUND, "project not found"),
        Err(e) => {
            warn!(error = %e, "director render-plan: load failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "load failed");
        }
    };
    // H3-only: the render plan describes the PACKING of clips into generations, and LTX does not
    // pack — a shot is a render. Echo the project's own engine rather than a literal, so a client
    // can tell "not supported here" from "you asked about the wrong project".
    let engine = project_engine(&project.timeline);
    if engine != ENGINE_H3 {
        return Json(json!({ "supported": false, "engine": engine, "tier": tier })).into_response();
    }
    match h3_render_plan(&state, &project, tier).await {
        Ok(plan) => Json(json!({
            "supported": true,
            "engine": ENGINE_H3,
            "tier": tier,
            "generations": plan.generations,
            "total": plan.generations.len(),
            "span": plan.span.map(|(f, l)| json!({ "first": f, "last": l })),
            "continue_count": plan.span_len(),
            "existing_takes": plan.existing_takes,
        }))
        .into_response(),
        // A document that cannot compile cannot render either, so this is the same 400 `/run` would
        // give — surfaced here first, which is the cheaper place to find out.
        Err(msg) => err(StatusCode::BAD_REQUEST, &msg),
    }
}

/// `POST /api/v1/ltx-director/projects/{id}/run?tier=final|proxy` — compile the saved timeline to
/// a chain render and dispatch it. Returns 202 + `{job_id}`; poll the render via
/// the shared `GET /api/v1/ltx-video/jobs/{job_id}` endpoint. The started job id
/// is also stamped into the project's `metadata.last_job_id` (final tier only — the `last_*` stamps
/// describe the final chain, and pointing them at a proxy chain would make a final retake resume
/// from low-res latents).
async fn run_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<TierQuery>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = tier_or_final(q.tier.as_deref()) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };
    // 🔴 An unknown scope is a 400, not a silent fall-back to `all`. The two differ by whether the
    // tier's takes are destroyed, so a typo must never be resolved in the destructive direction.
    let stale_only = match q.scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("all") => false,
        Some("stale") => true,
        Some(other) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("unknown scope {other:?} — expected `all` or `stale`"),
            );
        }
    };

    // Load BEFORE probing an engine: which one this project renders on is a property of the saved
    // document, so checking ltx-video's health first would 503 an H3 project on an engine it never
    // intended to use.
    let project = match repo::get(&state.db, &id).await {
        Ok(Some(p)) => p,
        Ok(None) => return err(StatusCode::NOT_FOUND, "project not found"),
        Err(e) => {
            warn!(error = %e, "director run: load failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "load failed");
        }
    };
    let engine = project_engine(&project.timeline);
    if engine == ENGINE_H3 {
        return run_project_h3(state, &headers, id, tier, project, stale_only).await;
    }
    // 🔴 Refused rather than ignored on LTX. That engine reaches the same goal a different way — a
    // banked prefix and an end-pinned splice — which this models nothing of, so honouring the
    // parameter would mean guessing and ignoring it would mean silently running the destructive
    // full render the caller was explicitly trying to avoid.
    if stale_only {
        return err(
            StatusCode::BAD_REQUEST,
            "`scope=stale` is only supported on MiniMax-H3 projects — an LTX project resumes from \
             its banked prefix instead, via the retake path",
        );
    }
    // Is the LTX engine there at all. Asked after the H3 branch above, for the reason that branch
    // loads the project first: a project renders on the engine its document names, and probing the
    // other one would 503 a render that was never going to touch it.
    if !state.ltx25.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{engine} service not configured"),
        );
    }

    // The timeline must carry a compiled `request`. The editor produces it on
    // save; we deserialize it into the same wire type the /generate path uses.
    let Some(req_val) = project.timeline.get("request") else {
        return err(
            StatusCode::BAD_REQUEST,
            "project timeline has no 'request' to render",
        );
    };
    // The saved doc holds `asset:<hash>` references, not bytes — resolve them to the inline base64
    // the render path expects. Same edge as `AssetJson`, reached from storage instead of a request.
    let mut req_val = req_val.clone();
    // The saved request is always the FINAL-tier one. A proxy run is only cheap if it is brought
    // down to the proxy geometry first — otherwise "preview" silently costs a full render.
    if tier == "proxy" {
        let geometry = project
            .metadata
            .get("proxy_render")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(default_proxy_geometry);
        let Some(request) = req_val.as_object_mut() else {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid render request in timeline",
            );
        };
        apply_proxy_geometry(request, &geometry);
        // Record what this tier is actually rendered at, so a later proxy RETAKE can reproduce it
        // (same reason the /ltx-video proxy path records it). Best-effort.
        if let Err(e) =
            repo::patch_metadata(&state.db, &id, &json!({ "proxy_render": geometry })).await
        {
            warn!(error = %e, project = %id, "director run: proxy geometry stamp failed");
        }
    }
    // STAGED, not expanded: each `asset:<hash>` becomes a `part:<name>` marker and its bytes ride
    // to the engine as a binary part (§3). A saved timeline is where the biggest documents live —
    // stills, keyframe pins and character sheets across every shot — and this is the path that
    // used to inline all of them on every run.
    let staged =
        match director_assets::stage_refs(state.director_assets_dir.as_ref(), &mut req_val, "a_")
            .await
        {
            Ok(staged) => staged,
            Err(missing) => {
                warn!(project = %id, missing = ?missing, "director run: unresolved asset refs");
                return err(
                    StatusCode::BAD_REQUEST,
                    &director_assets::missing_refs_message(&missing),
                );
            }
        };
    let mut gen_req: LtxVideoGenerateRequest = match serde_json::from_value(req_val) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "director run: bad request doc");
            return err(
                StatusCode::BAD_REQUEST,
                "invalid render request in timeline",
            );
        }
    };
    apply_engine_delivery_limits(&mut gen_req);

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

    // Decode inline assets → the byte parts the compile expects. seg_images_b64
    // is aligned to the RAW segment list (the compile re-aligns through the
    // empty-prompt filter itself).
    let assets = project.timeline.get("assets").cloned().unwrap_or(json!({}));
    let (image_part, audio_part, bgm_part, seg_image_parts, seg_v2v_parts) =
        match decode_assets(state.director_assets_dir.as_ref(), &assets).await {
            Ok(parts) => parts,
            Err(faults) => {
                warn!(project = %id, faults = %faults, "director run: unusable assets");
                return err(StatusCode::BAD_REQUEST, &faults);
            }
        };

    let who = match super::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // Tag the job with this project + tier so the render's output is harvested into the owned store
    // (and re-harvested on a restart-reattach, since the tag persists in the sidecar) — and with
    // the ENGINE, which is what picks the client for the submit, the poll, the cancel and the
    // reattach, and the container the clip is written under.
    let job_id = state.ltx_jobs.create_director_on(
        prompts.len() as u32,
        prompts,
        id.clone(),
        tier.into(),
        Some(engine.to_string()),
        None,
    );

    // Stamp the job onto the project so the editor can reconnect to the render
    // after a reload (best-effort; a failed stamp doesn't block the render).
    if tier == "final"
        && let Err(e) = repo::patch_metadata(
            &state.db,
            &id,
            &json!({ "last_job_id": job_id, "last_run_at": now_rfc3339() }),
        )
        .await
    {
        warn!(error = %e, "director run: metadata stamp failed");
    }

    let save_clip = gen_req.save_clip;
    let compiled = match super::ltx_video::compile_ltx_chain(
        &state,
        gen_req,
        &staged,
        image_part,
        audio_part,
        bgm_part,
        seg_image_parts,
        seg_v2v_parts,
        // No multipart parts on this path: a saved timeline carries each shot's audio inline as
        // `segments[i].audio_b64`, which the compile decodes itself.
        Vec::new(),
        None,
    )
    .await
    {
        Ok(c) => c,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
    };

    // 🔴 ONE ENQUEUE for the whole cut. `job.segments`, `generations_done` and `partials[]` all live
    // on this one record, so a per-shot enqueue would give the editor N renders of one shot each and
    // nothing that adds up to the cut. A fresh full run replaces the whole persisted set FOR ITS
    // TIER (wipe = true).
    let job_id = match super::ltx_video::enqueue_ltx(
        &state,
        &who,
        compiled,
        super::ltx_video::LtxDispatch {
            job_id,
            director_project_id: Some(id),
            director_tier: Some(tier.to_string()),
            director_shot_offset: None,
            director_wipe: true,
            // A `/run` IS a full run, so it always offers the project's `timeline.critique` setting
            // the chance to fire; the retake path deliberately does not, or the critic's own
            // re-rolls would recurse.
            director_critique: true,
            save_clip,
        },
        crate::routes::queue::QueueOptions::from_headers(&headers),
    )
    .await
    {
        Ok(job_id) => job_id,
        Err((code, msg)) => return err(code, &msg),
    };

    (
        StatusCode::ACCEPTED,
        Json(json!({ "job_id": job_id, "status": "queued", "tier": tier })),
    )
        .into_response()
}

/// The proxy tier's fallback geometry: what the editor's own "preview this shot" uses. Only reached
/// when a project has never had a proxy render recorded — without it a client that cannot compile
/// its own request (a narrow/mobile one driving `/run?tier=proxy`) could never START a cheap
/// preview, only retake one that already existed.
fn default_proxy_geometry() -> Value {
    json!({
        "width": 512,
        "height": 288,
        "steps": 8,
        "profile": "standard",
        "upscaler_ratio": "none",
    })
}

/// Overlay a recorded proxy geometry onto a final-tier request. The upscale chain is dropped
/// outright: it is the final tier's, and it would reintroduce exactly the cost the proxy tier
/// exists to avoid.
fn apply_proxy_geometry(request: &mut serde_json::Map<String, Value>, geometry: &Value) {
    if let Some(overrides) = geometry.as_object() {
        for (key, value) in overrides {
            if !value.is_null() {
                request.insert(key.clone(), value.clone());
            }
        }
    }
    request.insert("hires_chain".into(), Value::Null);
    request.insert("dims_are_target".into(), Value::Bool(false));
}

// ─── Segment-level retake ─────────────────────────────────────────────────────

/// Body for `POST …/projects/{id}/retake`. Re-render ONE shot of a rendered timeline without
/// re-rendering the whole thing, reloading the banked prefix from the prior render's job dir.
#[derive(Deserialize)]
struct RetakePayload {
    /// RAW index into the timeline's `request.segments` array — the shot to retake (as the
    /// editor sees it, before the empty-prompt filter).
    segment: usize,
    /// New prompt for that shot. Absent ⇒ keep the saved prompt.
    #[serde(default)]
    prompt: Option<String>,
    /// New global seed (the LTX chain seed is global; overriding it re-seeds the retaken shot).
    /// Pass a fresh value (or `-1` for random) to get a different take with the same prompt.
    #[serde(default)]
    seed: Option<i64>,
    /// New scene image for the shot (bare or data-URI base64) — makes it a fresh i2v scene-cut.
    /// An empty string CLEARS any existing image (the shot reverts to a continuation). Absent ⇒
    /// leave the shot's image untouched.
    #[serde(default)]
    image_b64: Option<String>,
    /// The banked render to retake from — the ltx-video-server job id (`resume_job_id`/
    /// `server_job_id` from a job status). Absent ⇒ derived from the project's last render.
    #[serde(default)]
    resume_job_id: Option<String>,
    /// Write the prompt/seed/image overrides back into the saved timeline so retakes stack
    /// (default true). The transient `resume_job_id` is never persisted.
    #[serde(default = "default_true")]
    persist: bool,
    /// Which render stream to re-roll: `final` (default) or `proxy`, the cheap preview tier.
    #[serde(default)]
    tier: Option<String>,
}

const fn default_true() -> bool {
    true
}

/// `POST /api/v1/ltx-director/projects/{id}/retake` — re-render a single shot of a previously
/// rendered timeline, reloading the banked prefix `[0, segment)` from the prior render's artifact
/// dir instead of re-rendering it. Returns 202 + `{job_id}`; poll via the shared
/// `GET /api/v1/ltx-video/jobs/{job_id}`. The overrides (prompt/seed/image) are, by default,
/// persisted back into the saved timeline so iterative retakes accumulate.
///
/// Works for ANY shot, not just the last. LTX-2.5 restores the tail as it was and does not re-pin
/// it (`retake.splices_tail: false`), so the join after the retaken shot can drift until the next
/// hard cut and the job delivers that shot alone rather than a fresh cut.
async fn retake_segment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    AssetJson(body): AssetJson<RetakePayload>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = tier_or_final(body.tier.as_deref()) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };

    let project = match repo::get(&state.db, &id).await {
        Ok(Some(p)) => p,
        Ok(None) => return err(StatusCode::NOT_FOUND, "project not found"),
        Err(e) => {
            warn!(error = %e, "director retake: load failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "load failed");
        }
    };
    let engine = project_engine(&project.timeline);
    if engine == ENGINE_H3 {
        return retake_segment_h3(state, &headers, id, tier, body, project).await;
    }
    // ⚠️ LTX-2.5 reaches this path now — kobbler `6ca0b6f2` gave it a real per-segment bank, so
    // `resume_job_id`, `segments[i].bank_job_id` and `retake_from` are honoured rather than 501'd.
    // What it does NOT do is splice: shot `k` re-renders against the banked prefix and the shots
    // after it are restored as they were, not re-pinned to the new take (`retake.splices_tail:
    // false`). The consequence downstream is that this job's `/media` is shot `k` alone rather than
    // a fresh cut — see `LtxJob::job_is_partial`, which is what stops the harvest filing it.
    if let Some(resp) = bank_gate(&state, engine).await {
        return resp;
    }

    // Work on a mutable copy of the timeline so overrides can be applied + (optionally) persisted.
    let mut timeline = project.timeline.clone();
    let Some(request) = timeline.get_mut("request").filter(|r| r.is_object()) else {
        return err(
            StatusCode::BAD_REQUEST,
            "project timeline has no 'request' to retake",
        );
    };

    // The shot must exist and, after any override, carry a non-empty prompt (empty shots aren't
    // rendered / banked, so there's nothing to retake).
    let Some(segs) = request.get_mut("segments").and_then(Value::as_array_mut) else {
        return err(StatusCode::BAD_REQUEST, "timeline request has no segments");
    };
    if body.segment >= segs.len() {
        return err(
            StatusCode::BAD_REQUEST,
            "segment index out of range for this timeline",
        );
    }
    let seg = &mut segs[body.segment];
    if let Some(p) = &body.prompt {
        seg["prompt"] = json!(p);
    }
    let prompt_now = seg
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if prompt_now.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "the retaken shot has no prompt — set one to retake it",
        );
    }
    // Scene image override: a non-empty image makes the shot a fresh i2v scene-cut (image source =
    // upload, backed by the inline seg_images_b64 slot); an empty string clears it back to a
    // continuation. The compile resolves the bytes only when the shot's image source is `upload`.
    if let Some(img) = &body.image_b64 {
        if img.trim().is_empty() {
            seg["image"] = Value::Null;
        } else {
            seg["image"] = json!({ "kind": "upload" });
        }
    }
    // Seed is a top-level chain param, not per-segment.
    if let Some(s) = body.seed {
        request["seed"] = json!(s);
    }
    // Own the mutated request so the `&mut timeline` borrow held via `request` ends here — the
    // assets edit below needs its own mutable borrow of `timeline`. `timeline["request"]` keeps
    // the same mutations for persistence.
    let request_val = request.clone();

    // Mirror the scene image into the inline assets array (aligned to RAW segment index), growing
    // it with nulls as needed. This is what decode_assets → the compile reads for the bytes.
    if let Some(img) = &body.image_b64 {
        let assets = timeline
            .as_object_mut()
            .and_then(|o| {
                o.entry("assets")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
            })
            .expect("assets is an object");
        let arr = assets
            .entry("seg_images_b64")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("seg_images_b64 is an array");
        if arr.len() <= body.segment {
            arr.resize(body.segment + 1, Value::Null);
        }
        arr[body.segment] = if img.trim().is_empty() {
            Value::Null
        } else {
            json!(img)
        };
    }

    // Resolve the banked render to retake from: explicit body id (checked against this project +
    // tier) → that TIER's own banked latents → (final only) the project's stamped server job id.
    // Without one there's no banked prefix to reload, so a retake can't be dispatched.
    let resume_job_id = match resolve_resume_job_id_for_tier(
        &state,
        &id,
        &project.metadata,
        tier,
        engine,
        body.resume_job_id.as_deref(),
    )
    .await
    {
        Ok(resume) => resume,
        Err(message) => return err(StatusCode::BAD_REQUEST, &message),
    };

    let who = match super::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // Compile and validate BEFORE anything is written or dispatched, so a request that cannot render
    // leaves the project exactly as it was.
    let prepared = match prepare_retake(
        &state,
        RetakeDispatch {
            project_id: &id,
            timeline: &timeline,
            request_val,
            resume_job_id: &resume_job_id,
            segment: body.segment,
            tier,
            engine,
            proxy_overrides: project.metadata.get("proxy_render"),
        },
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(message) => return err(StatusCode::BAD_REQUEST, &message),
    };

    // Persist the content overrides back into the saved timeline (default) so retakes stack. This
    // happens BEFORE the dispatch and its outcome DECIDES the dispatch: the render is compiled from
    // the overrides, so a conflict means the saved timeline is not the one about to be rendered.
    // Answering 202 with the live version (which is what this did) handed the caller a valid token
    // for a document it does not have — its next autosave then passed the CAS and blind-overwrote
    // the peer, which is the exact lost update the CAS exists to prevent. Conflict is now a 409 with
    // nothing dispatched: the client reconciles and asks again.
    let mut version = project.version;
    if body.persist {
        match repo::update(
            &state.db,
            &id,
            &project.name,
            &timeline,
            &project.metadata,
            project.version,
        )
        .await
        {
            Ok(outcome) => match retake_may_dispatch(&outcome) {
                Some(current) => version = current,
                None => return update_response(outcome),
            },
            Err(e) => {
                warn!(error = %e, "director retake: persist failed");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "persist failed");
            }
        }
    }

    let job_id = match spawn_retake(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(&headers),
        prepared,
    )
    .await
    {
        Ok(job_id) => job_id,
        Err(message) => return err(StatusCode::BAD_REQUEST, &message),
    };

    // Stamp the render onto the project so a following retake can resolve its banked dir + the
    // editor can reconnect after a reload. Server-owned keys, so this goes through `patch_metadata`
    // whether or not the timeline was persisted: it must not move the version (the editor authored
    // nothing) and it must not be droppable by the next client PUT. The `last_*` stamps describe the
    // FINAL chain only — pointing them at a proxy chain would make a later final retake resume from
    // low-res latents.
    let stamp = if tier == "final" {
        json!({
            "last_job_id": job_id,
            "last_run_at": now_rfc3339(),
            "last_server_job_id": resume_job_id,
            // Which store that id lives in. Without it the stamp is an opaque string that outlives
            // the engine picker, and `resolve_resume_job_id_for_tier` would hand it to whichever
            // engine the project is on now — see `0039`.
            "last_server_engine": engine,
            "last_retake_segment": body.segment,
        })
    } else {
        json!({ "last_run_at": now_rfc3339() })
    };
    if let Err(e) = repo::patch_metadata(&state.db, &id, &stamp).await {
        warn!(error = %e, "director retake: metadata stamp failed");
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "status": "queued",
            "tier": tier,
            "retake_segment": body.segment,
            "resume_job_id": resume_job_id,
            "version": version,
        })),
    )
        .into_response()
}

/// `POST …/projects/{id}/retake` for an H3 project — re-render the GENERATION containing the
/// requested shot, and file its output as a new take of that generation.
///
/// 🔴 **This is a generation retake, not a shot retake, and a per-shot one is not expressible.** A
/// generation is the smallest thing H3 can render: the cuts inside it are `[Shot N]` markers in one
/// prompt, rendered by the model in one denoise, and `<scenetrans>` audio continuity across a cut
/// exists only within one. Pulling a single shot out would mean chaining — which loses
/// `<scenetrans>`, loses the prose identity carry between shots, and adds a seam that is never
/// frame-exact. So the caller names a shot and the generation holding it is what re-renders.
///
/// Structurally different from the LTX retake in the other direction too. LTX reloads the banked
/// prefix `[0, k)`, re-renders shot `k` and splices the banked tail back end-pinned; there is no
/// splice here, so `resume_job_id` is a category error rather than an unsupported option, and
/// nothing downstream of the retaken generation is touched or invalidated.
///
/// The job is tagged with `shot_offset = g` so the harvest files its `seg_0` as project unit `g`
/// rather than 0, and skips the stitched-cut take (a one-generation "timeline" is not the project's
/// cut). The stitched cut is re-derived from the selected takes by `…/rebuild`.
async fn retake_segment_h3(
    state: AppState,
    headers: &HeaderMap,
    id: String,
    tier: &'static str,
    body: RetakePayload,
    project: repo::DirectorProjectRow,
) -> Response {
    if !state.minimax_h3.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "minimax-h3 service not configured",
        );
    }
    // An explicit resume id is a category error here, not a no-op: there is no splice to resume
    // into, and silently ignoring it would leave the caller believing it resumed something.
    if body
        .resume_job_id
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        return err(
            StatusCode::BAD_REQUEST,
            "an H3 project has no banked prefix to splice — a retake here re-renders the whole \
             generation containing the requested shot",
        );
    }

    // Apply the overrides to a mutable copy, exactly as the LTX path does, so retakes stack.
    //
    // The overrides land on the TIMELINE CLIP, not on the compiled request: the clip is what the
    // packer reads, and a per-generation prompt is composed from the clips it packed. Writing to a
    // compiled generation instead would be overwritten by the next compile and would also mean
    // rewriting one shot's text inside a joined prompt, which is not a substring operation anyone
    // should be doing.
    let mut timeline = project.timeline.clone();
    let Some(clips) = timeline.get_mut("clips").and_then(Value::as_array_mut) else {
        return err(
            StatusCode::BAD_REQUEST,
            "project timeline has no 'clips' to retake",
        );
    };
    if body.segment >= clips.len() {
        return err(
            StatusCode::BAD_REQUEST,
            "shot index out of range for this timeline",
        );
    }
    let clip = &mut clips[body.segment];
    if let Some(p) = &body.prompt {
        clip["prompt"] = json!(p);
    }
    if clip
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "the retaken shot has no prompt — set one to retake it",
        );
    }
    // An image override is the shot's opening frame (`fl2va`'s first keyframe), stored inline on
    // the clip — there is no `assets.seg_images_b64` sidecar array to keep in step, as there is on
    // LTX. An empty string clears it back to `t2va`.
    //
    // ⚠️ On H3 this is a property of the GENERATION, not of the shot: FL2VA's `frame_index` accepts
    // only `[0]`, `[-1]` and `[0,-1]`, so an opener pins t=0 of the whole render. Setting one on a
    // shot that is not first in its generation therefore pins the generation's opening frame, not
    // that shot's — which is why the editor should only offer it on a generation's first shot.
    if let Some(img) = &body.image_b64 {
        clip["image_b64"] = if img.trim().is_empty() {
            Value::Null
        } else {
            json!(img)
        };
    }
    // Per CLIP, not per request: re-seeding the whole request would change every other generation's
    // seed for the next full run. (`< 0` = engine-random.)
    if let Some(sd) = body.seed {
        clip["seed"] = json!(sd);
    }
    // The compiled `request.generations` is now stale — it was composed from the clips as they
    // were. Dropping it makes `h3_request_for` re-pack from the edited clips, which is the whole
    // point of writing the override there. (Only when the editor's compile has not already been
    // re-run, which for a retake it never has.)
    if let Some(request) = timeline.get_mut("request").and_then(Value::as_object_mut) {
        request.remove("generations");
    }

    // Persist BEFORE dispatch, and let the CAS outcome decide whether to dispatch — same rule as the
    // LTX path: the render is compiled from these overrides, so a conflict means the saved timeline
    // is not the one about to render.
    let mut version = project.version;
    if body.persist {
        match repo::update(
            &state.db,
            &id,
            &project.name,
            &timeline,
            &project.metadata,
            project.version,
        )
        .await
        {
            Ok(outcome) => match retake_may_dispatch(&outcome) {
                Some(current) => version = current,
                None => return update_response(outcome),
            },
            Err(e) => {
                warn!(error = %e, "director retake: persist failed");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "persist failed");
            }
        }
    }

    let mut stored = project;
    stored.timeline = timeline;
    // Cut the request down to the one GENERATION before building, not after: every check in
    // `build_chain` — canvas, reference caps, the fl2va/ref2va exclusion, the shot cut times — then
    // runs against exactly what is submitted rather than against a timeline this render is not
    // going to produce.
    let (chain, generation_index, save_clip, continuation_from) = match h3_single_generation_chain(
        &state,
        &stored,
        tier,
        H3RetakeTarget::RawClip(body.segment),
    )
    .await
    {
        Ok(v) => v,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
    };

    let who = match super::queue::principal_of(&state, headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let spawned = match dispatch_h3_retake(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(headers),
        &id,
        tier,
        chain,
        generation_index,
        save_clip,
        continuation_from,
        Some(body.segment),
    )
    .await
    {
        Ok(spawned) => spawned,
        Err(message) => return err(StatusCode::BAD_REQUEST, &message),
    };
    let job_id = spawned.job_id;
    let continuation_from = spawned.continuation_from;

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "status": "queued",
            "tier": tier,
            "engine": ENGINE_H3,
            // The shot the caller ASKED for, echoed back…
            "retake_segment": body.segment,
            // …and the generation that is actually re-rendering, which is what the take is filed
            // under and what the UI has to highlight. They differ whenever the generation packed
            // more than one shot, and a client that assumed they were the same would show the
            // retake landing on the wrong row.
            "generation_index": generation_index,
            "shot_index": generation_index,
            // The latent-bank entry this retake continues from, or null when it renders as a fresh
            // start (no continuation in the doc, or nothing banked to pin to — `dropped` says
            // which). NOT a resume: the pin is conditioning, never a spliced prefix.
            "continuation_from": continuation_from,
            // Constant null, and present on purpose: an LTX-shaped client parses this response
            // unchanged, and there is no banked prefix to splice here.
            "resume_job_id": Value::Null,
            "version": version,
        })),
    )
        .into_response()
}

/// What a dispatched H3 retake produced, for the caller's response.
///
/// Deliberately NOT an echo of everything that went in: `generation_index` is the caller's own
/// argument and re-returning it would be a second copy of a number that can then disagree.
pub(crate) struct H3RetakeSpawn {
    pub(crate) job_id: String,
    /// The latent-bank entry this retake pinned to, or `None` for a fresh start. Returned rather
    /// than left to the caller because the stamp above consumes it.
    pub(crate) continuation_from: Option<String>,
}

/// Create the job, spawn the render, stamp the metadata. Shared by the retake ENDPOINT and the
/// critic's auto-retake.
///
/// 🔴 Shared deliberately, and the seam is where it is on purpose. The two callers legitimately
/// differ in how they edit the timeline first — the endpoint applies a caller's prompt/image/seed
/// overrides, the critic only re-seeds — but everything from the built chain onwards MUST NOT
/// diverge: the `shot_offset` is in generation space and the harvest must run with `wipe = false` so
/// a single take does not wipe its siblings. A second copy of that would be a second chance to get
/// `wipe` wrong, and getting it wrong deletes every other generation's takes.
#[allow(clippy::too_many_arguments)]
async fn dispatch_h3_retake(
    state: &AppState,
    who: &crate::auth::Principal,
    opts: crate::routes::queue::QueueOptions,
    id: &str,
    tier: &'static str,
    chain: super::minimax_h3::H3Chain,
    generation_index: i32,
    save_clip: bool,
    continuation_from: Option<String>,
    // The CLIP the user clicked, when there was one. `None` from the critic, which works in
    // generation space and has no clip to name — and stamping a clip index it invented would be
    // worse than omitting the key.
    retake_segment: Option<usize>,
) -> Result<H3RetakeSpawn, String> {
    let prompts = chain.prompts.clone();
    let job_id = state.ltx_jobs.create_director_on(
        prompts.len() as u32,
        prompts,
        id.to_string(),
        tier.into(),
        Some(ENGINE_H3.to_string()),
        // The offset is in GENERATION space on this engine — see `LtxJob::director_shot_offset`.
        Some(generation_index),
    );
    // wipe = false: this adds a take to ONE generation and must not disturb any other.
    let job_id = match super::minimax_h3::enqueue_h3_render(
        state,
        who,
        chain,
        super::minimax_h3::H3Dispatch {
            job_id,
            save_clip,
            director_project_id: id.to_string(),
            director_tier: tier.to_string(),
            director_shot_offset: Some(generation_index),
            director_wipe: false,
            director_critique: false,
        },
        // 🔴 Stays `default()` where `/run` reads the caller's `X-Queue-*`: this helper is shared
        // with the critic's auto-retake, which has no request and no headers to read.
        opts,
    )
    .await
    {
        Ok(job_id) => job_id,
        Err((_, message)) => {
            return Err(message);
        }
    };

    if let Err(e) = repo::patch_metadata(
        &state.db,
        id,
        &if tier == "final" {
            let mut stamp = json!({
                // Cloned: both are returned to the caller, and `json!` takes them by value.
                "last_job_id": job_id.clone(),
                "last_run_at": now_rfc3339(),
                "last_retake_generation": generation_index,
                // The bank entry this retake pinned to. Stamped because the failure it enables is
                // engine-side and asynchronous: an evicted key fails the render with a message
                // naming the KEY, and this is what turns that key back into "generation N of this
                // project, re-render it".
                "last_retake_continuation_from": continuation_from.clone(),
            });
            if let (Some(seg), Some(o)) = (retake_segment, stamp.as_object_mut()) {
                o.insert("last_retake_segment".into(), json!(seg));
            }
            stamp
        } else {
            json!({ "last_run_at": now_rfc3339() })
        },
    )
    .await
    {
        warn!(error = %e, "director retake: metadata stamp failed");
    }

    Ok(H3RetakeSpawn {
        job_id,
        continuation_from,
    })
}

/// Re-render ONE H3 generation at a fresh seed, for the vision critic's auto-retake.
///
/// 🔴 The seed is written onto the generation's FIRST CLIP and the compiled `request.generations`
/// is dropped, so the packer re-derives the request from the clips — exactly what the retake
/// endpoint does. Writing the seed into the compiled generation instead would be overwritten by the
/// editor's next compile, so the take the critic judged and the take the document produces would
/// diverge with nothing on screen saying so.
///
/// 🔴 **The document is NOT written.** The seed lands on an in-memory copy and goes out with the
/// dispatch only, matching `ltx_critic::retake_shot`'s own rule and for the same reasons: a
/// server-side write bumps the version, which disarms an open editor's autosave once per re-roll,
/// and a critic that lost the CAS would render a seed the document then disagreed with anyway. The
/// take's provenance belongs on the TAKE — the harvest records `job.seed` on the row it writes.
pub(crate) async fn critic_retake_h3(
    state: &AppState,
    project_id: &str,
    tier: &'static str,
    generation_index: usize,
) -> Option<String> {
    let project = repo::get(&state.db, project_id).await.ok().flatten()?;
    let mut timeline = project.timeline.clone();

    // Which clip opens this generation, in RAW clip space. `shots[]` on the composed request is the
    // packing the render used, so walking it is reading the answer rather than re-deriving it.
    let rendered_first: usize = timeline
        .pointer("/request/generations")
        .and_then(Value::as_array)
        .map(|gens| {
            gens.iter()
                .take(generation_index)
                .map(|g| {
                    g.get("shots")
                        .and_then(Value::as_array)
                        .map(|s| s.len())
                        .unwrap_or(1)
                        .max(1)
                })
                .sum()
        })
        .unwrap_or(generation_index);
    let clips = timeline.get_mut("clips").and_then(Value::as_array_mut)?;
    // Rendered space skips blank-prompt clips; walk back to the raw index of the Nth non-blank.
    let mut seen = 0usize;
    let mut raw = None;
    for (i, c) in clips.iter().enumerate() {
        if c.get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            continue;
        }
        if seen == rendered_first {
            raw = Some(i);
            break;
        }
        seen += 1;
    }
    let raw = raw?;
    // A fresh seed from the clock. rem_euclid, not abs(): abs() panics on i64::MIN, and a seed is
    // not worth a reachable panic.
    let seed =
        (time::OffsetDateTime::now_utc().unix_timestamp_nanos() as i64).rem_euclid(2_000_000_000);
    clips[raw]["seed"] = json!(seed);
    // Stale now — it was composed from the clips as they were. Dropping it makes the packer re-pack.
    if let Some(request) = timeline.get_mut("request").and_then(Value::as_object_mut) {
        request.remove("generations");
    }

    let mut stored = project;
    stored.timeline = timeline;
    let (chain, gi, save_clip, continuation_from) = match h3_single_generation_chain(
        state,
        &stored,
        tier,
        H3RetakeTarget::Generation(generation_index),
    )
    .await
    {
        Ok(v) => v,
        Err(msg) => {
            warn!(project = %project_id, generation = generation_index, message = %msg, "critic: H3 retake dispatch failed");
            return None;
        }
    };
    // 🔴 `EnvKey`: koblem itself, not a person. It also means the re-roll enqueues at band 0 rather
    // than behind the night's queue — the critic blocks on this job before it judges the next shot,
    // so parking it behind an unrelated cut would stall the review it is part of.
    dispatch_h3_retake(
        state,
        &crate::auth::Principal::EnvKey,
        // The critic has no request to read a pin or a band off — it IS koblem.
        crate::routes::queue::QueueOptions::default(),
        project_id,
        tier,
        chain,
        gi,
        save_clip,
        continuation_from,
        None,
    )
    .await
    .map(|spawned| spawned.job_id)
    .inspect_err(
        |message| warn!(project = %project_id, generation = generation_index, %message, "critic: H3 retake dispatch failed"),
    )
    .ok()
}

/// Which generation a retake is for, in whichever space the caller actually holds.
///
/// 🔴 The two callers genuinely do not share a coordinate system, and collapsing them was how the
/// off-by-one class of bug got in before: the editor clicks a CLIP (`clips[]` index, including
/// blank ones), while the critic reads a take row whose `shot_index` is already the GENERATION
/// index. A single `usize` parameter cannot say which it is holding, and both are plausible values.
#[derive(Clone, Copy, Debug)]
pub(crate) enum H3RetakeTarget {
    /// An index into the timeline's `clips[]`, blanks included. Converted here.
    RawClip(usize),
    /// A generation index, already in the space takes are filed under.
    Generation(usize),
}

/// The one-GENERATION chain for retaking the generation `target` names, and the generation index it
/// will be filed under.
///
/// # Scoping is now structural, and that removed a whole class of bug
///
/// Under the old contract references were request-level and narrowed to shots by an index list, so
/// a single-shot retake had to rewrite every reference's scope to `[0]` or `[]` — never dropping
/// the out-of-scope ones, because a reference's position assigns its `<Picture i>` ordinal and
/// removing an earlier one renumbers every later one, silently pointing "`<Picture 2>` wears the
/// red coat" at somebody else.
///
/// References now live ON the generation that uses them. Keeping one generation therefore keeps
/// exactly its own list, in its own order, with its own ordinals — nothing to rewrite and nothing
/// to renumber.
///
/// # The continuation is RESOLVED, not dropped
///
/// A generation past the first continues from the one before it, and the saved doc spells that
/// `continuation.from = "prev"`. `"prev"` is the SHIM's word, resolved against the previous
/// generation *of the same job* — and a retake's job holds exactly one generation, so "prev" names
/// nothing. This used to drop the continuation outright and render the generation as a fresh
/// start: the shot was right and the join back to its predecessor was not, which is a seam the
/// retake introduced and the take it replaced did not have.
///
/// So the retake substitutes the preceding generation's own latent-bank key, which
/// `harvest_director_renders` recorded per take (`0038`). Two rules govern the pick and both
/// matter:
///
///   * the key comes from that generation's **selected** take, never its newest row. Takes coexist
///     by design (`0029`); the cut plays the selected one, and pinning to a take the user rejected
///     would continue from a version of the shot that appears nowhere in the timeline.
///   * an explicit key already in the doc is forwarded **verbatim**. It is not "prev", so it is not
///     the shim's to resolve, and rewriting it would override a deliberate cross-job pin.
///
/// 🔴 A stored key can be EVICTED — the engine's bank is size-capped and reclaims oldest-first
/// (`bank.py::evict`) — and koblem has no way to ask: the shim exposes bank *stats* on `/health`
/// but no key lookup. A vanished key therefore fails the render with the shim's own message ("bank
/// key … is gone — it was evicted or never written. Re-render the generation it came from."), and
/// that is the intended outcome. Falling back to a hard cut on a miss would deliver the exact
/// silent seam this resolution exists to remove, on the one path where the user asked for
/// continuity explicitly. The resolved key is returned so the caller can name the generation in
/// its 202 and its metadata stamp, which is what turns that engine-side message into something
/// actionable.
///
/// Only a genuinely absent key drops the continuation — no take, or a take from before `0038` —
/// and that is reported through [`H3Chain::dropped`], as before.
async fn h3_single_generation_chain(
    state: &AppState,
    project: &repo::DirectorProjectRow,
    tier: &str,
    target: H3RetakeTarget,
) -> Result<(super::minimax_h3::H3Chain, i32, bool, Option<String>), String> {
    let (
        req_val,
        shot_to_generation,
        staged,
        _sliced_for_seconds,
        _expected_trim_frames,
        delivery_offsets,
    ) = h3_request_for(state, project, tier).await?;
    let generation_index = match target {
        // The editor asks in CLIP space, because that is what the user clicked.
        H3RetakeTarget::RawClip(raw) => {
            // `shot_to_generation` is indexed by RENDERED shot (empty-prompt clips filtered), and
            // `raw` is the editor's index into `clips[]`. Convert, rather than assuming they agree:
            // they only do when every clip has a prompt.
            let rendered_index = project
                .timeline
                .get("clips")
                .and_then(Value::as_array)
                .map(|clips| {
                    clips
                        .iter()
                        .take(raw)
                        .filter(|c| {
                            c.get("prompt")
                                .and_then(Value::as_str)
                                .is_some_and(|p| !p.trim().is_empty())
                        })
                        .count()
                })
                .unwrap_or(raw);
            let Some(&gi) = shot_to_generation.get(rendered_index) else {
                return Err("shot index out of range for this timeline".to_string());
            };
            gi
        }
        // 🔴 The critic asks in GENERATION space, and it has no choice: a take row's `shot_index`
        // IS the generation index on this engine (`LtxJob::director_shot_offset`), and the critic
        // only ever sees take rows. Making it convert back to a clip would mean re-deriving the
        // packing it is not holding, to hand in a number this function would immediately convert
        // forward again.
        H3RetakeTarget::Generation(gi) => gi,
    };

    h3_span_chain_from(
        req_val,
        staged,
        delivery_offsets,
        state,
        project,
        tier,
        generation_index,
        generation_index,
    )
    .await
}

/// The chain for rendering generations `first..=last` as one job, and the index it is filed from.
///
/// [`h3_single_generation_chain`]'s body, generalised from one generation to a contiguous SPAN so
/// that "continue" (render what is stale) and "retake" (render exactly this one) are the same
/// dispatch with different bounds. A retake is `first == last`, and every rule below was already
/// true for it.
///
/// # Why a span rather than a set
///
/// The stale set is computed with forward propagation (see [`h3_render_plan`]), so a generation
/// whose predecessor is being re-rendered is itself stale — it continued from latents that will no
/// longer exist. What survives that is nearly always contiguous, and rendering `min..=max` of it as
/// ONE job keeps `"prev"` meaning what it says for every generation after the first. Dispatching a
/// non-contiguous set as several jobs would mean each one resolving its pin against a take the job
/// before it has not banked yet, which is a race with a wrong answer rather than a slow one.
///
/// The cost is that a non-stale generation caught between two stale ones is re-rendered. That is
/// reported to the caller by [`h3_render_plan`] rather than hidden.
#[allow(clippy::too_many_arguments)]
async fn h3_span_chain_from(
    req_val: Value,
    staged: director_assets::StagedParts,
    delivery_offsets: Vec<f64>,
    state: &AppState,
    project: &repo::DirectorProjectRow,
    tier: &str,
    first: usize,
    last: usize,
) -> Result<(super::minimax_h3::H3Chain, i32, bool, Option<String>), String> {
    let generation_index = first;
    let mut parsed: super::minimax_h3::H3Request = serde_json::from_value(req_val.clone())
        .map_err(|e| format!("invalid render request in timeline: {e}"))?;
    if first > last || last >= parsed.generations.len() {
        return Err("shot index out of range for this timeline".to_string());
    }
    // Drained in order, not `swap_remove`d: a span longer than one generation must keep the order
    // the seams were authored in, and swapping the tail element into the hole would silently
    // reverse a pair.
    let mut span: Vec<super::minimax_h3::H3Generation> =
        parsed.generations.drain(first..=last).collect();
    // 🔴 Only the FIRST generation's `"prev"` needs resolving. Every later one in the span has its
    // real predecessor inside this same job, which is exactly what `"prev"` means to the shim —
    // rewriting those to a bank key would pin them to the PREVIOUS take of a generation this job is
    // about to replace.
    let mut generation = span.remove(0);

    // ── Resolve `continuation.from` out of "prev" and into a real bank key ────────────────────
    //
    // `dropped` is Some only when there is genuinely nothing to pin to. An evicted key is NOT that
    // case: it is a key we have, that the engine will refuse, loudly — see the 🔴 above.
    let mut pinned_to: Option<String> = None;
    let mut dropped: Option<String> = None;
    if let Some(cont) = generation.continuation.as_mut() {
        // Owned, so the `cont.from = …` rewrite below is not fighting a live borrow of the field it
        // is replacing.
        let from = cont.from.trim().to_string();
        if cont.mode == H3ContinuationMode::None {
            // The doc asked for a hard cut. Nothing to resolve, and nothing to report.
        } else if from != H3_CONTINUATION_FROM_PREV {
            // An explicit key. Not the shim's to resolve, and not ours to second-guess.
            pinned_to = Some(from);
        } else if generation_index == 0 {
            // A first generation that continues from "prev" is a malformed doc — the shim rejects
            // it at submit (`contract.py:527`). Dropping it here means the retake still renders,
            // and the report says why it could not join anything.
            dropped = Some(
                "generations[].continuation (generation 1 continues from `prev`, but nothing \
                 precedes it — the saved timeline is malformed)"
                    .to_string(),
            );
        } else {
            // Project-space shot index == generation index on this engine: the retake job is
            // tagged `shot_offset = generation_index` and the harvest files its `seg_0` there.
            let previous = generation_index as i32 - 1;
            match repo_renders::selected_bank_key(&state.db, &project.id, tier, previous, ENGINE_H3)
                .await
            {
                Ok(Some(key)) => {
                    cont.from = key.clone();
                    pinned_to = Some(key);
                }
                Ok(None) => {
                    dropped = Some(format!(
                        "generations[].continuation (generation {generation_index} has no \
                         recorded latent-bank key — it was rendered before koblem stored them, so \
                         there is nothing to continue from. Re-run the project, or retake \
                         generation {generation_index} first, to bank one)"
                    ));
                }
                // Not silently degraded to a hard cut: a lookup that failed is not evidence that
                // no pin exists, and dispatching without one would burn a 15-minute render on a
                // seam the caller did not ask for.
                Err(e) => {
                    warn!(error = %e, project = %project.id, tier, "director retake: bank key lookup failed");
                    return Err(format!(
                        "could not read generation {generation_index}'s latent-bank key, so this \
                         retake cannot be joined to it: {e}"
                    ));
                }
            }
        }
    }
    if dropped.is_some() {
        generation.continuation = None;
    }
    // The resolved head back in front of the rest of the span, in authored order.
    span.insert(0, generation);
    let span_len = span.len();
    parsed.generations = span;

    let save_clip = parsed.save_clip;
    let mut chain = super::minimax_h3::build_chain(&parsed, &req_val, &staged)?;
    // 🔴 A span renders PART of the timeline, so the finished container is that part alone — not the
    // whole cut the slices were planned against. Comparing them would report drift on every retake
    // and every continue. The guard belongs to a full render; these slice offsets were already fixed
    // when the plan was cut, and re-checking here could only produce a false alarm.
    chain.sliced_for_seconds = None;
    // Same reasoning: the retake renders ONE generation, so it has no seams of its own to account
    // for and the whole-cut expectation would be compared against a single-generation job.
    chain.expected_trim_frames = 0;
    // 🔴 Only THIS SPAN's offsets, because the job's partials are numbered from its own first
    // generation — hand it the full-length list and generation 0's offset would be laid over a clip
    // that starts partway into the song. This is what makes "render shot" and "continue" deliver the
    // song rather than H3's reconstruction of it: with no offset for an index the mux is skipped,
    // and the reconstruction is what ships as the take.
    //
    // Sliced in COMPILED space (which is what `first`/`last` are in), then narrowed to rendered
    // space against the span's own generations — see `h3_offsets_in_rendered_space`.
    chain.delivery_offsets = delivery_offsets
        .get(first..=last)
        .map(|window| h3_offsets_in_rendered_space(&parsed.generations, window))
        .unwrap_or_default();
    debug_assert!(
        chain.delivery_offsets.is_empty() || span_len >= chain.delivery_offsets.len(),
        "more offsets than generations in the span"
    );
    if let Some(note) = dropped {
        chain.dropped.push(note);
    }
    Ok((chain, generation_index as i32, save_clip, pinned_to))
}

/// The version a persisting retake goes on with, or `None` to stop it dead ([`update_response`]
/// then owes the caller the 409/404).
///
/// A conflict MUST stop the dispatch. The render is compiled from overrides that could not be saved,
/// so the version this used to hand back with its 202 was a valid token for a document the caller
/// does not have — and the caller adopts it, so its next autosave passed the CAS and blind-overwrote
/// the peer. That is precisely the lost update the CAS exists to prevent, re-created by the endpoint
/// meant to respect it.
fn retake_may_dispatch(outcome: &repo::UpdateOutcome) -> Option<i64> {
    match outcome {
        repo::UpdateOutcome::Updated(row) => Some(row.version),
        repo::UpdateOutcome::Conflict { .. } | repo::UpdateOutcome::Missing => None,
    }
}

/// Whether a caller-supplied `resume_job_id` may be used for this project + tier.
///
/// The body value used to be trusted verbatim, short-circuiting before any check: a proxy retake
/// could be pointed at the FINAL chain's bank (appending low-res latents onto high-res ones — the
/// exact failure the tier split exists to prevent) or at another project's latents entirely. An id
/// is acceptable only if this project banked it in THIS tier ON THIS ENGINE, or if it is the engine
/// chain of a job the store still holds for this project + tier + engine — which is the window
/// between a render finishing and its harvest recording the row, i.e. exactly when the editor
/// retakes the shot it just watched. The engine half is `0039`: the editor persists its resume
/// target in the saved document, so a project switched to another engine keeps quoting the previous
/// one's id until something refuses it.
fn explicit_resume_allowed(
    explicit: &str,
    banked: &[String],
    jobs: &[(String, crate::ltx_jobs::LtxJob)],
    project_id: &str,
    tier: &str,
    engine: &str,
) -> bool {
    banked.iter().any(|id| id == explicit)
        || jobs.iter().any(|(_, j)| {
            j.director_project_id.as_deref() == Some(project_id)
                && j.director_tier.as_deref() == Some(tier)
                && j.engine_name() == engine
                && j.server_job_id.as_deref() == Some(explicit)
        })
}

/// The banked render a retake reloads its prefix from: a (validated) explicit id → the chain THIS
/// TIER'S FIRST SHOT was banked out of → that tier's newest banked chain → for `final` only, the
/// project's server-stamped chain. `Err` carries the 400 the caller owes the client.
///
/// 🔴 Shot 0's chain rather than the newest one, and it is the same rule the client applies in
/// `web/src/lib/director/renders.ts::bankedResumeTarget`: the engine composes `"<job>:<segment>"`
/// and walks from segment 0, so an id whose bank starts at shot 5 — which is what a continue mints
/// — answers a 400 to the next resume. The retake path survives a wrong pick here because
/// [`restore_selected_takes`] names every shot's own bank, but a prefix shot whose row predates the
/// `latent_job_id` column has no entry there and falls through to this id.
///
/// The proxy tier is a SEPARATE chain with its own banked latents at its own resolution, so for
/// anything but `final` the authoritative source is the `latent_job_id` recorded on that tier's own
/// takes, and the `last_*` stamps (which only ever describe the final chain) are not consulted.
pub(crate) async fn resolve_resume_job_id_for_tier(
    state: &AppState,
    project_id: &str,
    metadata: &Value,
    tier: &str,
    engine: &str,
    explicit: Option<&str>,
) -> std::result::Result<String, String> {
    // Every chain THIS ENGINE banked in this tier, newest first. It is the ACCEPTANCE set for an
    // explicit id, and the fallback when shot 0 cannot answer.
    //
    // 🔴 Engine-scoped, and that is the whole of `0039`, which now carries the WHOLE of the
    // LTX-2.3 retirement with it. Rows banked as `'ltx'` name latents in a service that no longer
    // exists; a project resolves as `ltx25`, so those rows do not match here and cannot be adopted
    // as a chain. Handing one over instead is not a warning — the engine takes the id, queues, and
    // 404s after the full wait. Nothing anywhere may rewrite a stored `'ltx'` into `'ltx25'`.
    let banked: Vec<String> =
        repo_renders::latent_chains_by_project_tier(&state.db, project_id, tier)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.is(engine))
            .map(|c| c.id)
            .collect();
    if let Some(explicit) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        if explicit_resume_allowed(
            explicit,
            &banked,
            &state.ltx_jobs.list(),
            project_id,
            tier,
            engine,
        ) {
            return Ok(explicit.to_string());
        }
        return Err(format!(
            "resume_job_id is not a banked {tier}-tier {engine} render of this project"
        ));
    }
    if let Ok(Some(id)) =
        repo_renders::first_shot_latent_job_id(&state.db, project_id, tier, engine).await
    {
        return Ok(id);
    }
    // Shot 0 predates the `latent_job_id` column (or was never banked). The newest chain THIS
    // ENGINE banked is the only thing left to name — it may start past shot 0 and be refused, which
    // is a better answer than a chain from a store that is not there.
    if let Some(id) = banked.into_iter().next() {
        return Ok(id);
    }
    // The last resort: a stamp left by an earlier retake, which predates the render rows on a
    // project that has only ever retaken. It carries `last_server_engine` for the same reason the
    // rows carry `engine` — the stamp outlives the engine it was made on, and a stamp that cannot
    // name its own engine is not evidence about this one.
    if tier == "final"
        && let Some(id) = metadata
            .get("last_server_job_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .filter(|_| metadata.get("last_server_engine").and_then(Value::as_str) == Some(engine))
            .map(String::from)
            .or_else(|| {
                metadata
                    .get("last_job_id")
                    .and_then(Value::as_str)
                    .and_then(|jid| state.ltx_jobs.get(jid))
                    .filter(|j| j.engine_name() == engine)
                    .and_then(|j| j.server_job_id)
            })
    {
        return Ok(id);
    }
    Err("no banked render to retake from — run the project first".into())
}

/// Compile an (already-mutated) timeline request into the wire type + its asset bytes, validating
/// everything the render needs. Shared by the retake endpoint and the vision critic's auto-retake so
/// there is exactly one description of what a retake dispatch is.
///
/// Separate from [`spawn_retake`] on purpose: preparing is the only step that can fail, so a caller
/// can validate, then take its conditional write, then dispatch — and a rejected write leaves
/// nothing rendering and nothing half-persisted.
///
/// `resume_job_id` is per-dispatch and deliberately not written back into the saved request — a
/// later `/run` must remain a fresh full render.
///
/// `tier` decides BOTH which tier the result is harvested into and how the request is shaped: the
/// saved timeline request is always the final-tier one, so a proxy retake has to be brought back
/// down to the proxy geometry that tier was actually rendered at (recorded as `proxy_render` on the
/// project when a proxy render is submitted). Without that a "cheap preview re-roll" would silently
/// cost a full-resolution render AND land in the wrong tier.
pub(crate) struct RetakeDispatch<'a> {
    pub project_id: &'a str,
    pub timeline: &'a Value,
    pub request_val: Value,
    pub resume_job_id: &'a str,
    pub segment: usize,
    pub tier: &'a str,
    /// Which LTX engine renders it (`ltx` or `ltx25`). Recorded on the JOB, because it picks the
    /// client that submits, polls, cancels and re-attaches, and the container every file is named
    /// with. Both engines bank, and both take the same three request fields; they differ in whether
    /// the tail is re-pinned, which is a fact about the OUTPUT and lands via `job_is_partial`.
    pub engine: &'a str,
    pub proxy_overrides: Option<&'a Value>,
}

/// A retake that has passed every check and is ready to be handed to the renderer.
pub(crate) struct PreparedRetake {
    project_id: String,
    tier: String,
    segment: usize,
    engine: String,
    gen_req: LtxVideoGenerateRequest,
    /// Bytes behind the `part:<name>` markers in `gen_req` (§3).
    staged: director_assets::StagedParts,
    prompts: Vec<String>,
    parts: RenderAssets,
}

pub(crate) async fn prepare_retake(
    state: &AppState,
    d: RetakeDispatch<'_>,
) -> std::result::Result<PreparedRetake, String> {
    let RetakeDispatch {
        project_id,
        timeline,
        mut request_val,
        resume_job_id,
        segment,
        tier,
        engine,
        proxy_overrides,
    } = d;
    if tier != "final" {
        let Some(overrides) = proxy_overrides.filter(|v| v.is_object()) else {
            return Err(format!(
                "no recorded {tier}-tier render geometry for this project — render that tier once first"
            ));
        };
        let Some(request) = request_val.as_object_mut() else {
            return Err("invalid render request in timeline".into());
        };
        apply_proxy_geometry(request, overrides);
    }
    // Staged rather than expanded (§3, §4.3): part names derive from the CONTENT HASH, so a
    // retake presents byte-identical conditioning inputs to the bank by construction, and a
    // character reference used across ten shots is one upload instead of ten base64 copies.
    let staged = match director_assets::stage_refs(
        state.director_assets_dir.as_ref(),
        &mut request_val,
        "a_",
    )
    .await
    {
        Ok(staged) => staged,
        Err(missing) => {
            warn!(project = %project_id, missing = ?missing, "director retake: unresolved asset refs");
            return Err(director_assets::missing_refs_message(&missing));
        }
    };
    let mut gen_req: LtxVideoGenerateRequest = match serde_json::from_value(request_val) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "director retake: bad request doc");
            return Err("invalid render request in timeline".into());
        }
    };
    gen_req.resume_job_id = Some(resume_job_id.to_string());
    apply_engine_delivery_limits(&mut gen_req);

    restore_selected_takes(
        state,
        project_id,
        tier,
        engine,
        &mut gen_req.segments,
        Some(segment),
    )
    .await;

    let prompts: Vec<String> = gen_req
        .segments
        .iter()
        .map(|s| s.prompt.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if prompts.is_empty() {
        return Err("at least one segment prompt is required".into());
    }

    // Decode the inline assets from the mutated timeline (so a new scene image is picked up).
    let assets = timeline.get("assets").cloned().unwrap_or(json!({}));
    let parts = match decode_assets(state.director_assets_dir.as_ref(), &assets).await {
        Ok(parts) => parts,
        Err(faults) => {
            warn!(project = %project_id, faults = %faults, "director retake: unusable assets");
            return Err(faults);
        }
    };

    Ok(PreparedRetake {
        project_id: project_id.to_string(),
        tier: tier.to_string(),
        segment,
        engine: engine.to_string(),
        gen_req,
        staged,
        prompts,
        parts,
    })
}

/// Register a Director job for a prepared retake and put it on the queue. Returns the koblem job id.
///
/// A retake is its OWN job, correctly — one shot re-rendered against the banked prefix — where a
/// full run stays one enqueue for the whole cut.
pub(crate) async fn spawn_retake(
    state: &AppState,
    who: &crate::auth::Principal,
    opts: crate::routes::queue::QueueOptions,
    prepared: PreparedRetake,
) -> Result<String, String> {
    let PreparedRetake {
        project_id,
        tier,
        segment,
        engine,
        gen_req,
        staged,
        prompts,
        parts,
    } = prepared;
    let (image_part, audio_part, bgm_part, seg_image_parts, seg_v2v_parts) = parts;

    // No shot offset on LTX: a retake keeps the caller's own segment numbering, so its takes file
    // at their own index. Whether the OUTPUT is the whole timeline is the engine's to report — it
    // is not, since 2.5 does not splice the tail back — see `job_is_partial`.
    let job_id = state.ltx_jobs.create_director_on(
        prompts.len() as u32,
        prompts,
        project_id.clone(),
        tier.clone(),
        Some(engine),
        None,
    );
    let save_clip = gen_req.save_clip;
    let compiled = super::ltx_video::compile_ltx_chain(
        state,
        gen_req,
        &staged,
        image_part,
        audio_part,
        bgm_part,
        seg_image_parts,
        seg_v2v_parts,
        // No multipart parts on this path: a saved timeline carries each shot's audio inline as
        // `segments[i].audio_b64`, which the compile decodes itself.
        Vec::new(),
        Some(segment as u32),
    )
    .await?;

    // A retake adds a take to one shot + a new stitched cut — keep every other shot's persisted
    // takes (wipe = false). And no critique: the critic's own re-rolls come through here, so
    // chaining a review off one would recurse.
    super::ltx_video::enqueue_ltx(
        state,
        who,
        compiled,
        super::ltx_video::LtxDispatch {
            job_id,
            director_project_id: Some(project_id),
            director_tier: Some(tier),
            director_shot_offset: None,
            director_wipe: false,
            director_critique: false,
            save_clip,
        },
        // 🔴 Stays `default()` where `/run` reads the caller's `X-Queue-*`: this helper is shared
        // with the critic's auto-retake, which has no request and no headers to read.
        opts,
    )
    .await
    .map_err(|(_, msg)| msg)
}

/// Current time as an RFC3339 string (best-effort; used only for a UI hint).
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

// ─── Durable per-project renders ───────────────────────────────────────────────

/// A project's owned, non-swept render dir (`director-renders/<project_id>/`). Ids are
/// server-minted uuids, but this one arrives from the URL — reject a path-traversing id.
pub(crate) fn project_render_dir(state: &AppState, project_id: &str) -> Option<std::path::PathBuf> {
    if project_id.is_empty()
        || project_id.contains('/')
        || project_id.contains('\\')
        || project_id.contains("..")
    {
        return None;
    }
    Some(state.director_renders_dir.join(project_id))
}

/// The container extension a persisted take is written with, from its recorded MIME.
///
/// 🔴 Two engines, two containers: ltx-video delivers VP9-in-webm, the MiniMax-H3 shim delivers
/// **x265-in-mp4** (`H3_OUTPUT_CODEC=libx265_8`, CRF 8 — WanGP's own `libx264_8` default is CRF 23
/// and too lossy for a master). This is not a naming preference: **webm cannot carry HEVC**, so an
/// H3 take under a `.webm` name breaks `rebuild_cut`'s `ffmpeg -c copy` concat with a mux error
/// that says nothing about where the wrong name came from.
///
/// Keyed off the MIME rather than the engine so a row read back from `director_shot_renders` — long
/// after its job left the store — still knows what it is.
fn container_ext(content_type: &str) -> &'static str {
    if content_type.contains("mp4") {
        "mp4"
    } else {
        "webm"
    }
}

/// The two persisted render tiers. `proxy` = cheap preview stream (small/few-steps, with audio +
/// LoRAs); `final` = full-res. Anything else is rejected so a bad `tier` can't spawn phantom rows.
pub(crate) fn normalize_tier(tier: &str) -> Option<&'static str> {
    match tier {
        "proxy" => Some("proxy"),
        "final" => Some("final"),
        _ => None,
    }
}

/// The koblem-served URL for one persisted render (`shot_index` −1 ⇒ the stitched `final`).
fn render_url(project_id: &str, tier: &str, shot_index: i32) -> String {
    let shot = if shot_index < 0 {
        "final".to_string()
    } else {
        shot_index.to_string()
    };
    format!("/api/v1/ltx-director/projects/{project_id}/renders/{tier}/{shot}")
}

/// If `job_id` backs a Director project, harvest its (terminal) output into that project's owned
/// store. Reads the project id + tier from the job's own tag, so the restart-reattach path can
/// persist a render it finishes without re-deriving the association. `wipe=false` there (a reattach
/// can't know if the interrupted render was a full run or a partial — never drop other shots).
pub(crate) async fn maybe_harvest_director(state: AppState, job_id: String, wipe: bool) {
    let Some(job) = state.ltx_jobs.get(&job_id) else {
        return;
    };
    let (Some(project_id), Some(tier)) = (job.director_project_id, job.director_tier) else {
        return;
    };
    harvest_director_renders(state, project_id, job_id, tier, wipe).await;
}

/// What a harvest may do with one job's output. Every field is read off the JOB rather than passed
/// in, so the restart-reattach path files a render it finishes exactly where the original dispatch
/// would have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HarvestScope {
    /// Project shot index of the job's own unit 0.
    shot_offset: i32,
    /// This job's `/media` is NOT the project's cut, so no stitched-cut take is filed from it.
    partial_render: bool,
    /// Whether this harvest may drop the rows it did not write. Not the negation of
    /// `partial_render`: a full render that could not stitch is both, and the swap is what repairs
    /// it. See [`harvest_scope`].
    wipe: bool,
}

/// 🔴 Point each shot this render RESTORES at the take the USER PICKED, rather than at whatever the
/// single `resume_job_id` bank happens to hold.
///
/// This is the reported bug: "pick a take → render again → it forgot the one I picked". One
/// `resume_job_id` names one bank and that bank is the LAST roll of every shot, so selecting an
/// older take gave a correct video immediately and then silently lost it on the next retake of any
/// other shot. The engine composes the key itself from `"<job id>:<index>"` and 400s on one it
/// cannot resolve, so `by_shot` carries JOB ids and no key is ever minted here.
///
/// 🔴 TWO INDEX SPACES. `by_shot` is keyed by the PROMPT-FILTERED index (empty-prompt shots are
/// never rendered or banked), which is also the engine's own numbering; `segments` is RAW. Walking
/// both together is what keeps them aligned — indexing the raw array by a filtered index attaches a
/// take to the wrong shot on any project with a blank shot in it.
///
/// The retaken shot is left alone: it is re-rendered, so it has no take to restore. Shots whose
/// selected take predates the `latent_job_id` column — or was banked by an engine other than the one
/// now rendering (`0039`) — are absent from `by_shot` and keep the chain default rather than being
/// guessed at.
///
/// ⚠️ A RETAKE marks the prefix only, and nothing after the retaken shot. The field names the take
/// a shot this request does not render is RESTORED from; LTX-2.5 renders one shot and restores only
/// [0, k), so its tail is not restored — it is left alone. Naming a take there is a claim the engine
/// answers with a warning saying that take is "replaced", which is not what happens either. A
/// CONTINUE marks everything banked, because every one of those shots really is restored.
///
/// 🔴 `retake: None` is that continue, and it is the reason this is not retake-only. A continue
/// names one `resume_job_id`, and that id resolves only the segments THAT job banked: a chain of
/// continues leaves shots 0-4 in job A's bank and 5-6 in job B's, and no single id addresses both
/// — the bank is a flat `"<job>:<segment>"` store with no aliasing, so the second continue 400s on
/// a missing segment 0. Naming each shot's own bank makes the chain length irrelevant, and restores
/// the picked take on a continue rather than only on a retake.
fn apply_selected_takes(
    segments: &mut [koblem_client::types::LtxSegment],
    by_shot: &std::collections::HashMap<i32, String>,
    retaken_raw: Option<usize>,
) {
    let restores_tail = retaken_raw.is_none();
    let mut filtered = 0i32;
    let mut past_retake = false;
    for (raw, seg) in segments.iter_mut().enumerate() {
        if seg.prompt.trim().is_empty() {
            continue;
        }
        if Some(raw) == retaken_raw {
            past_retake = true;
        } else if (restores_tail || !past_retake)
            && let Some(bank) = by_shot.get(&filtered)
        {
            seg.bank_job_id = Some(bank.clone());
        }
        filtered += 1;
    }
}

/// Point every shot a resuming render RESTORES at the take the user picked, from the project's own
/// rows. The one entry point for both dispatch shapes — [`prepare_retake`] and the Director's
/// continue through `/ltx-video/generate` — so the two cannot drift into two answers for "which
/// banked take is this shot".
///
/// Only ever called alongside a `resume_job_id`: the engine 400s on a `bank_job_id` in a request
/// that resumes nothing, because a caller sending one has picked a specific take and rendering a
/// fresh roll instead is the exact failure the field exists to prevent.
pub(crate) async fn restore_selected_takes(
    state: &AppState,
    project_id: &str,
    tier: &str,
    engine: &str,
    segments: &mut [koblem_client::types::LtxSegment],
    retake: Option<usize>,
) {
    match repo_renders::selected_bank_ids(&state.db, project_id, tier, engine).await {
        Ok(banks) => apply_selected_takes(segments, &banks.into_iter().collect(), retake),
        Err(e) => {
            // Not fatal: without per-shot banks the render degrades to the old single-bank
            // behaviour, which is what every retake did before this. Worth a line, though —
            // it means the take the user picked may not be the one that comes back.
            warn!(error = %e, project = %project_id, tier,
                  "director resume: could not read per-shot take selection; \
                   falling back to the single resume bank");
        }
    }
}

/// 🔴 THE TWO ENGINES SAY "PARTIAL" DIFFERENTLY, AND ONLY ONE OF THEM SHIFTS.
///
/// H3's retake renders project unit `k` as its own unit 0, so an OFFSET is what makes it partial
/// there. `ltx25` does not shift — a resume or a retake keeps the caller's own segment numbering, so
/// its takes file at their own index — but `/media` is still only the shots inside the window, and
/// the engine says so in `result.job_is_partial`. Reading the offset alone would file that fragment
/// as the project's stitched cut, replacing a good full cut with the two shots a resume re-rendered.
/// [`LtxJob::delivers_fragment`] is the one place the two are put together, so the status body a
/// watching client adopts and this harvest cannot answer differently.
///
/// The same flag is set by a FULL render whose concatenation failed, where `/media` is unit 0 alone.
/// Same consequence for the CUT — those bytes are not it — and the opposite one for the SWAP.
///
/// 🔴 A windowed render must never swap: the wipe drops every row this job did not write, and such a
/// job holds a take for the window only, so a resume with `wipe` would delete precisely the shots it
/// restored from the bank. A full render whose stitch died is not that: it rendered every shot, and
/// refusing it the swap leaves the PREVIOUS render's trailing rows in place — on a timeline that got
/// shorter, `selected_shots` then rebuilds an over-long cut out of shots the author deleted. Reading
/// one flag for both questions is what produced that, and [`LtxJob::renders_subrange`] is the one
/// that answers this one. The caller passes `wipe` from the dispatch it made; the engine's own
/// account of what it rendered is what may take it away.
fn harvest_scope(job: &crate::ltx_jobs::LtxJob, requested_wipe: bool) -> HarvestScope {
    HarvestScope {
        shot_offset: job.director_shot_offset.unwrap_or(0),
        partial_render: job.delivers_fragment(),
        wipe: requested_wipe && !job.renders_subrange(),
    }
}

/// Copy a finished Director render out of the TTL'd `ltx_jobs` store into the project's owned,
/// non-swept dir and record the pointers, so the shots survive the store's 1h sweep + a koblem
/// restart. Awaited by the queue adapter once the engine is done and the card released — the job is
/// terminal and the clip + any progressive partials are already on disk. The prompt AS RENDERED per
/// shot comes
/// from the job's own `scripts` (the PROMPT-FILTERED list, aligned to `partials[].segment_index`).
/// `tier` is the render stream (`proxy`|`final`) — its rows + files are namespaced independently.
/// `wipe` REPLACES that tier's set (a fresh full run replaces everything; a retake / through-K
/// render updates in place) — the new takes are written first and the old ones dropped only once
/// they are, so a failure mid-harvest cannot leave the project with neither. Best-effort throughout.
pub(crate) async fn harvest_director_renders(
    state: AppState,
    project_id: String,
    job_id: String,
    tier: String,
    wipe: bool,
) {
    // Active until HARVESTED, not merely until the job is terminal — a client switching to the
    // durable rows on that transition would otherwise race the replace about to happen here. Marked
    // at the top of the harvest rather than at dispatch: the dispatch no longer owns a task that
    // outlives it, and a queued job is non-terminal anyway, so the store record already covers the
    // hours before this runs.
    let _pending = HarvestPending::new(&job_id);
    let Some(tier) = normalize_tier(&tier) else {
        warn!(project = %project_id, tier = %tier, "director harvest: unknown tier");
        return;
    };
    let Some(job) = state.ltx_jobs.get(&job_id) else {
        return;
    };
    let HarvestScope {
        shot_offset,
        partial_render,
        wipe,
    } = harvest_scope(&job, wipe);
    let prompts = &job.scripts;
    let Some(dir) = project_render_dir(&state, &project_id) else {
        return;
    };
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        warn!(error = %e, project = %project_id, "director harvest: mkdir failed");
        return;
    }

    // The tier's latents BEFORE this harvest. A wipe drops whichever of them the swap below leaves
    // unreferenced — read now, because the new rows land first.
    let replaced_latents = if wipe {
        repo_renders::latent_chains_by_project_tier(&state.db, &project_id, tier)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // The fallback follows the ENGINE, not a constant: the H3 shim delivers x265-in-mp4 and ltx25
    // delivers h264-in-mp4 (ComfyUI's only audio-carrying save node is `SaveVideo`, whose format
    // enum is `["auto","mp4"]`), and webm cannot carry either — so a webm fallback on one of those
    // jobs would name every take wrongly and only fail later, at the `ffmpeg -c copy` concat that
    // rebuilds the cut. One source of truth: `LtxJob::clip_ext`, the same function the job store
    // named the files with.
    let content_type = job
        .content_type
        .clone()
        .unwrap_or_else(|| format!("video/{}", job.clip_ext()));
    let ext = container_ext(&content_type);

    // The engine job id whose persist-store dir holds this render's banked seg_<n>.bin latents
    // (Director renders are submitted with persist:true). Recorded on every row so a retake can
    // reference the latents and project-delete / tier-wipe can tell the engine to drop them.
    let latent_job_id = job.server_job_id.as_deref();

    // Each progressively-delivered shot → <tier>_seg_<idx>_v<take>.webm. A retake is a NEW koblem
    // job, so it lands as a new TAKE of that shot rather than overwriting the previous one; the
    // restart-reattach path re-harvests the SAME job id, which reuses that job's existing take.
    for p in &job.partials {
        let src = state.ltx_jobs.partial_path(&job_id, p.segment_index);
        let shot_index = p.segment_index as i32 + shot_offset;
        let (variant, existing_name) = match repo_renders::next_variant(
            &state.db,
            &project_id,
            tier,
            shot_index,
            &job_id,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, project = %project_id, tier, seg = p.segment_index, "director harvest: take lookup failed");
                continue;
            }
        };
        let file_name = existing_name
            .unwrap_or_else(|| format!("{tier}_seg_{}_v{variant}.{ext}", p.segment_index));
        if let Err(e) = tokio::fs::copy(&src, dir.join(&file_name)).await {
            warn!(error = %e, project = %project_id, tier, seg = p.segment_index, "director harvest: copy partial failed");
            continue;
        }
        // Carry H3's own generated audio across with the take. It lives beside the partial in the
        // job store, which is TTL-swept, so not copying here means it is gone within a day — and it
        // is the only surviving answer to "what did the model actually make" once the song has been
        // muxed over the picture. Named off the take so the pair is obvious on disk.
        //
        // Best-effort and never `continue`: the take itself is already safely copied above, and a
        // missing diagnostic must not look like a failed harvest.
        {
            let model_src = src.with_extension(crate::routes::h3_soundtrack::MODEL_AUDIO_EXT);
            if tokio::fs::try_exists(&model_src).await.unwrap_or(false) {
                let stem = file_name
                    .rsplit_once('.')
                    .map_or(&file_name[..], |(s, _)| s);
                let dest = dir.join(format!(
                    "{stem}.{}",
                    crate::routes::h3_soundtrack::MODEL_AUDIO_EXT
                ));
                if let Err(e) = tokio::fs::copy(&model_src, dest).await {
                    warn!(error = %e, project = %project_id, tier, seg = p.segment_index, "director harvest: copy model audio failed");
                }
            }
        }
        // `scripts` is this JOB's prompt list, so it is indexed by the job's own segment index —
        // not the project shot the take is filed under.
        let prompt = prompts
            .get(p.segment_index as usize)
            .map(String::as_str)
            .unwrap_or("");
        // This unit's latent-bank key, as the poll body reported it (`generations[].bank_key` on H3,
        // `segments[].bank_key` on ltx25 → `LtxJob::bank_keys`). Matched on the JOB's own unit
        // index, the same space `segment_index` is in — `shot_offset` maps into project space and
        // must not be applied here, or an H3 retake (offset = its generation) would look up a key
        // its one-generation job never had.
        //
        // Stored so a later render can name THIS take rather than whichever was rolled last. On H3
        // it is what a retake of the next generation pins `continuation.from` to; without it the
        // retake's `"prev"` resolves against a job holding one generation, i.e. nothing (see
        // `h3_single_generation_chain`). On ltx25 the key is `"<engine job id>:<segment index>"` and
        // the shim composes it itself from `segments[i].bank_job_id` — so what a resume or a retake
        // actually sends is the JOB half, `latent_job_id`, and this row is the record that the
        // engine really did bank this shot. Absent on the C++ LTX engine, which banks by job dir.
        let bank_key = job
            .bank_keys
            .iter()
            .find(|(index, _)| *index == p.segment_index)
            .map(|(_, key)| key.as_str());
        if let Err(e) = repo_renders::upsert(
            &state.db,
            repo_renders::ShotRenderUpsert {
                project_id: &project_id,
                tier,
                shot_index,
                prompt,
                file_name: &file_name,
                content_type: &content_type,
                latent_job_id,
                bank_key,
                engine: Some(job.engine_name()),
                source_job_id: &job_id,
                variant,
                seed: job.seed,
            },
        )
        .await
        {
            warn!(error = %e, project = %project_id, tier, seg = p.segment_index, "director harvest: upsert partial failed");
        }
    }

    // The stitched cut → <tier>_final_v<take>.webm (shot_index −1). Only a completed run produces
    // one, and each run's cut is its own take: a retake yields a new stitched cut, and keeping the
    // previous one is what lets a rebuilt cut be compared against the render it came from.
    if job.state == koblem_client::types::VideoJobState::Completed && !partial_render {
        let src = state.ltx_jobs.clip_path(&job_id);
        match repo_renders::next_variant(&state.db, &project_id, tier, -1, &job_id).await {
            Ok((variant, existing_name)) => {
                let file_name =
                    existing_name.unwrap_or_else(|| format!("{tier}_final_v{variant}.{ext}"));
                if let Err(e) = tokio::fs::copy(&src, dir.join(&file_name)).await {
                    warn!(error = %e, project = %project_id, tier, "director harvest: copy final failed");
                } else if let Err(e) = repo_renders::upsert(
                    &state.db,
                    repo_renders::ShotRenderUpsert {
                        project_id: &project_id,
                        tier,
                        shot_index: -1,
                        prompt: "",
                        file_name: &file_name,
                        content_type: &content_type,
                        latent_job_id,
                        // The stitched cut is a concatenation, not a generation, so it has no
                        // entry in the engine's bank and nothing may ever continue from it.
                        bank_key: None,
                        engine: Some(job.engine_name()),
                        source_job_id: &job_id,
                        variant,
                        seed: job.seed,
                    },
                )
                .await
                {
                    warn!(error = %e, project = %project_id, tier, "director harvest: upsert final failed");
                }
            }
            Err(e) => {
                warn!(error = %e, project = %project_id, tier, "director harvest: final take lookup failed");
            }
        }
    }

    // ── Delivery problems, onto the project where the editor can see them ─────────────────────
    //
    // 🔴 Stamped BEFORE the `!wipe` early return, so a RETAKE reports too — a retake is the single
    // most likely thing to drop a generation and the most likely to be read as "I must not have
    // pressed the button".
    //
    // Written unconditionally, empty list included: the empty write is what CLEARS a previous run's
    // banner, and a stamp that only ever appeared would leave a fixed problem on screen forever.
    // An LTX job has no delivery warnings and so clears it too, which is correct — the banner
    // describes the last render of this project, whatever engine ran it.
    if let Err(e) = repo::patch_metadata(
        &state.db,
        &project_id,
        &json!({
            // By reference: `json!` takes its values by value, and moving the field out would
            // partially move `job`, which the wipe branch below still reads.
            "last_render_warnings": &job.delivery_warnings,
            "last_render_warnings_at": now_rfc3339(),
        }),
    )
    .await
    {
        warn!(error = %e, project = %project_id, "director harvest: delivery-warning stamp failed");
    }

    if !wipe {
        return;
    }
    // 🔴 A wipe is a SWAP, and there is nothing to swap IN unless the run finished.
    //
    // A cancelled or failed run harvests only the units that landed before it stopped — a strict
    // subset — so wiping there deletes the previous COMPLETE render, rows and files, and leaves a
    // truncated one in its place. On a ~16-generation MiniMax-H3 cut (one ~8-hour engine job) that
    // is hours of GPU destroyed by pressing Cancel, and the degenerate case is worse still: a run
    // cancelled in its first seconds harvests nothing at all and the delete then takes the whole
    // tier. Nothing recovers either, because the swap unlinks the files.
    //
    // So the old set stays. The new rows are already in the table beside it and win per shot index
    // on `selected`, and the next run that actually completes performs the swap that was skipped.
    // ⚠️ The cost of keeping them is that a tier can hold shots from two different chains — old
    // tail generations that the cancelled run never got to — so a cut rebuilt in that state is
    // complete but mixed at the join. Complete-and-mixed is recoverable by retaking the tail;
    // deleted is not, which is the whole trade.
    if job.state != koblem_client::types::VideoJobState::Completed {
        info!(
            project = %project_id, tier, job = %job_id, state = ?job.state,
            "director harvest: run did not complete — keeping the previous set instead of swapping"
        );
        return;
    }
    // Swap: the new set is on disk and in the table, so drop what this run replaces. Anything this
    // job wrote is kept by source id, which also keeps a re-harvest of the same job idempotent.
    let removed =
        match repo_renders::delete_by_project_tier_except(&state.db, &project_id, tier, &job_id)
            .await
        {
            Ok(removed) => removed,
            Err(e) => {
                warn!(error = %e, project = %project_id, tier, "director harvest: swap failed");
                return;
            }
        };
    for file_name in removed {
        let _ = tokio::fs::remove_file(dir.join(&file_name)).await;
    }
    // Drop the replaced chains from the engine's never-swept persist store — but only the ones no
    // surviving take still points at, and never this render's own. A wiping run is always a FRESH
    // chain (`/run` never resumes, and the editor's incremental preview passes `wipe=false` exactly
    // when it does), so its bank is never one of these.
    let kept = repo_renders::latent_chains_by_project_tier(&state.db, &project_id, tier)
        .await
        .unwrap_or_default();
    for chain in replaced_latents {
        if kept.iter().any(|k| k.id == chain.id) || latent_job_id == Some(chain.id.as_str()) {
            continue;
        }
        // Same intent on every engine, different route and different stakes. The C++ LTX engine
        // drops a banked latent chain from its never-swept persist store. On H3 and ltx25 the job
        // dir holds staged references and the per-unit clips, so this is housekeeping — neither
        // engine's LATENT bank is reachable by job id, and both bound theirs by bytes with their own
        // newest-first-keep sweep. Routed by the engine that BANKED the chain, not by the one now
        // harvesting: a wipe after an engine switch replaces the previous engine's rows, and asking
        // the new container about them is a 404 that leaves the old dirs behind.
        let owner = chain.engine.as_deref().unwrap_or_else(|| job.engine_name());
        if let Err(e) = drop_engine_job(&state, owner, &chain.id).await {
            warn!(error = %e, project = %project_id, tier, job = %chain.id, engine = owner, "director harvest: engine job cleanup failed");
        }
    }

    // Unattended dailies review: the takes it judges are the rows written above, so this is the
    // first moment it can run at all. Detached rather than awaited — the caller is the queue runner
    // finishing a job, and the critic waits on re-rolls of its own, so awaiting it here would park a
    // finished render in `collecting` for the length of a second render.
    if job.director_critique {
        tokio::spawn(super::ltx_critic::maybe_critique_after_render(
            state, job_id,
        ));
    }
}

// ─── Takes: several rolls of the same shot, kept side by side ──────────────────

/// `GET …/projects/{id}/renders/{tier}/{shot}/takes` — every take of one shot, with each take's
/// seed, its own URL, and the vision critic's verdict if it has been judged. This is what makes an
/// unattended overnight run useful: the retakes accumulate instead of overwriting, and the pick is
/// made afterwards by a human looking at them.
async fn list_takes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tier, shot)): Path<(String, String, String)>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = normalize_tier(&tier) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };
    let Some(shot_index) = parse_shot_index(&shot) else {
        return err(StatusCode::BAD_REQUEST, "bad shot index");
    };
    match repo_renders::list_takes(&state.db, &id, tier, shot_index).await {
        Ok(rows) => {
            let items: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "variant": r.variant,
                        "selected": r.selected,
                        "prompt": r.prompt,
                        "seed": r.seed,
                        "critic": r.critic,
                        "content_type": r.content_type,
                        "created_at": r.created_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
                        "url": format!("{}/takes/{}", render_url(&id, tier, shot_index), r.variant),
                        // 🔴 The render that produced this take — and the ONLY thing on the row
                        // from which "which take of the previous shot did this one continue from"
                        // can be reconstructed. A full run writes one take per shot all sharing
                        // this id and wipes the tier's other takes as it lands, so two takes with
                        // the same id came out of one submitted chain, where `continuation.from:
                        // "prev"` could only have meant each other. Nothing records a RETAKE's pin
                        // (`last_retake_continuation_from` holds one value for the whole project,
                        // overwritten by the next retake), so without this the editor cannot tell
                        // that changing a selection has left the shots after it joining footage
                        // that no longer plays.
                        "source_job_id": r.source_job_id,
                        // This take's OWN bank entry, `"<engine job id>:<generation index>"` —
                        // what the NEXT generation's `continuation.from` pins to. Surfaced so the
                        // join readout can name the key, which is what an engine-side "evicted
                        // key" failure reports back.
                        "bank_key": r.bank_key,
                    })
                })
                .collect();
            Json(json!({ "items": items })).into_response()
        }
        Err(e) => {
            warn!(error = %e, "director takes list failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "list takes failed")
        }
    }
}

/// `GET …/projects/{id}/renders/{tier}/{shot}/takes/{variant}` — stream one specific take.
async fn get_take(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tier, shot, variant)): Path<(String, String, String, i32)>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = normalize_tier(&tier) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };
    let Some(shot_index) = parse_shot_index(&shot) else {
        return err(StatusCode::BAD_REQUEST, "bad shot index");
    };
    let Some(dir) = project_render_dir(&state, &id) else {
        return err(StatusCode::BAD_REQUEST, "bad project id");
    };
    let row = match repo_renders::get_take(&state.db, &id, tier, shot_index, variant).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "take not found"),
        Err(e) => {
            warn!(error = %e, "director take get failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "get take failed");
        }
    };
    serve_render_file(&headers, &dir.join(&row.file_name), &row.content_type).await
}

#[derive(Deserialize)]
struct SelectTakePayload {
    variant: i32,
}

#[derive(Deserialize)]
struct StillPayload {
    /// Timecode within the take, in seconds. Absent → [`DEFAULT_STILL_SECONDS`].
    #[serde(default)]
    seconds: Option<f64>,
    /// Which take to grab from. Absent → the selected one (what the cut plays).
    #[serde(default)]
    variant: Option<i32>,
    /// Run the promoted frame through the reference-preparation pipeline (matte → alpha-edge
    /// validation → flatten on white → letterbox) before storing it.
    ///
    /// Default TRUE, and the default matters: the editor's own copy tells an author to "render the
    /// shot that introduces them, then promote a frame of it", so this is the RECOMMENDED way to
    /// get a reference — and a raw video frame is close to a worst case for it. It is unmatted, so
    /// it carries its own background (the measured location-hijack failure), and a subject in a
    /// rendered shot almost always touches a frame edge (the measured severed-subject failure,
    /// which produced a legless robot). Promoting one raw silently produced the bad reference the
    /// prepare endpoint exists to prevent.
    #[serde(default = "default_true")]
    prepare: bool,
    /// Render geometry to letterbox to. Absent → the engine's usual bucket.
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

/// Where to grab from when the caller doesn't say. Half a second in: past the first frame (which on
/// a continuation shot is the join, and is the frame most likely to carry motion blur) and inside
/// every shot the Director can produce.
const DEFAULT_STILL_SECONDS: f64 = 0.5;

/// `POST …/projects/{id}/renders/{tier}/{shot}/still` — freeze one frame of a rendered shot into
/// the content-addressed asset store and hand back its `asset:<hash>`.
///
/// This is the t2v half of character references. An i2v shot already owns an image the editor can
/// point a character at; a t2v shot owns nothing until it has been rendered — so the shot that
/// INTRODUCES a character becomes that character's reference, after the fact, from its own footage.
/// The engine only ever wanted an image, so nothing downstream has to know where it came from.
///
/// Server-side on purpose. The bytes are already here behind an auth-gated URL, ffmpeg keeps the
/// exact decoded pixels (a browser canvas grab of a 10-bit VP9 clip is tone-mapped to 8-bit first),
/// and the store is content-addressed, so promoting the same frame twice writes nothing.
async fn promote_still(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tier, shot)): Path<(String, String, String)>,
    Json(body): Json<StillPayload>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = normalize_tier(&tier) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };
    let Some(shot_index) = parse_shot_index(&shot) else {
        return err(StatusCode::BAD_REQUEST, "bad shot index");
    };
    let Some(dir) = project_render_dir(&state, &id) else {
        return err(StatusCode::BAD_REQUEST, "bad project id");
    };
    let row = match body.variant {
        Some(v) => repo_renders::get_take(&state.db, &id, tier, shot_index, v).await,
        None => repo_renders::get(&state.db, &id, tier, shot_index).await,
    };
    let row = match row {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "render not found"),
        Err(e) => {
            warn!(error = %e, "director still: render lookup failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "get render failed");
        }
    };
    let source = dir.join(&row.file_name);
    if !tokio::fs::try_exists(&source).await.unwrap_or(false) {
        return err(StatusCode::NOT_FOUND, "render file missing");
    }
    let seconds = body
        .seconds
        .filter(|s| s.is_finite() && *s >= 0.0)
        .unwrap_or(DEFAULT_STILL_SECONDS);
    let png = match extract_still(&source, seconds).await {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!(error = %e, shot = shot_index, "director still: frame extract failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "frame extract failed");
        }
    };
    // Run the promoted frame through the SAME preparation pipeline an upload gets. This path is
    // what the editor's own copy recommends ("render the shot that introduces them, then promote a
    // frame of it"), and a raw video frame is the worst input for it: unmatted, so it carries its
    // own background, and a subject in a rendered shot almost always touches a frame edge. Both are
    // measured failures. Promoting raw silently produced exactly the reference the prepare endpoint
    // exists to reject.
    //
    // A 422 is passed straight through: it means THIS FRAME cannot serve as a reference, and the
    // message says which edge. The caller should pick a different moment, not retry.
    let mut edge_report = serde_json::Value::Null;
    let png = if body.prepare {
        let width = body.width.unwrap_or(1280).clamp(64, 4096);
        let height = body.height.unwrap_or(704).clamp(64, 4096);
        match prepare_reference_bytes(&state, png.clone(), width, height, false).await {
            Ok((prepared, edges)) => {
                edge_report = edges;
                prepared
            }
            Err((status, payload)) => return (status, Json(payload)).into_response(),
        }
    } else {
        png
    };
    let hash = match store_prepared_reference(&state, &png).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    let (width, height) = png_dimensions(&png).unwrap_or((0, 0));
    Json(json!({
        "ref": director_assets::make_ref(&hash),
        "hash": hash,
        "bytes": png.len(),
        "content_type": "image/png",
        "width": width,
        "height": height,
        "seconds": seconds,
        "variant": row.variant,
        "prepared": body.prepare,
        "edges": edge_report,
    }))
    .into_response()
}

/// One frame of `source` at `seconds`, as PNG bytes.
///
/// `-ss` goes AFTER `-i` deliberately: input seeking lands on the nearest keyframe, and a shot is a
/// few seconds long, so the decode-and-discard cost is trivial next to handing back a frame from a
/// different moment than the one the user scrubbed to. `-frames:v 1` and `image2pipe` keep it to
/// stdout with no temp file.
async fn extract_still(source: &std::path::Path, seconds: f64) -> std::io::Result<Bytes> {
    let ss = format!("{seconds:.3}");
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(source)
        .args(["-ss", ss.as_str()])
        .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"])
        .output()
        .await?;
    if !output.status.success() || output.stdout.is_empty() {
        // A seek past the end yields success + no bytes, which is the common case worth naming.
        return Err(std::io::Error::other(format!(
            "ffmpeg produced no frame at {seconds:.3}s ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(Bytes::from(output.stdout))
}

/// A PNG's pixel dimensions, read straight out of the IHDR chunk (8-byte signature, 8-byte chunk
/// header, then two big-endian u32s). Cheap enough to be worth it — the editor needs to say whether
/// a promoted still matches the render's framing, and a second ffprobe for two numbers is not.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let ihdr = bytes.get(16..24)?;
    let w = u32::from_be_bytes(ihdr[0..4].try_into().ok()?);
    let h = u32::from_be_bytes(ihdr[4..8].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

// ─── reference-sheet ingestion ───────────────────────────────────────────────

/// Fraction of one edge that may carry subject alpha before the subject counts as SEVERED.
///
/// Not zero: a matte's antialiasing routinely puts a few pixels of partial alpha on an edge, and
/// rejecting those would reject every good reference. Two percent of a 1280-wide edge is ~26 px,
/// which the measured failure clears by a wide margin — `hit-robot-crop.png` had 164 px of alpha
/// on its last row (12.8%) and produced a legless robot in every shot that used it.
const REFERENCE_EDGE_ALPHA_FRACTION: f64 = 0.02;

/// Alpha above which a pixel counts as subject rather than matte falloff.
const REFERENCE_EDGE_ALPHA_FLOOR: u8 = 32;

#[derive(serde::Deserialize)]
struct PrepareReferenceQuery {
    /// Render width the reference will be used at. The letterbox targets this aspect so the
    /// engine's `match_target` resize does not have to crop anything.
    width: Option<u32>,
    height: Option<u32>,
    /// Skip the matte and only letterbox. For a reference that is ALREADY cut out on white.
    #[serde(default)]
    skip_matte: bool,
}

/// `POST /api/v1/ltx-director/assets/prepare-reference` — turn a snapshot into a reference the
/// memory-regime checkpoints can actually hold, and REFUSE the ones they cannot.
///
/// The target, from three measured failures, is: **matted subject, on white, evenly lit,
/// uncropped, nothing touching the frame edge.** Asking an author to produce that by hand is
/// asking them to know why it matters; every failure is invisible in a render that succeeds.
///
/// The pipeline is one matting call plus two ffmpeg passes:
///
/// 1. **Matte** to an RGBA cutout. This is also what removes the environmental light — an unmatted
///    photo leaks its own background, and lighting with no visible source in frame is read as the
///    subject's own colour and travels with it (a neon-lit cutout stayed pink on a moonlit beach).
/// 2. **Validate the alpha edges.** If the matte touches a frame edge the subject is severed, and
///    the model reinvents the missing part differently in every shot. This is the highest-value
///    check here and the one nobody can perform by eye, because the cut is in the ALPHA, not in
///    the visible image.
/// 3. **Flatten on white and letterbox** to the render aspect, in one filter graph.
///
/// Deliberately NOT done: a generative relight. flux2 is reachable, but `sd_version_is_inpaint()`
/// excludes FLUX2 and a mask sent anyway is decoded, allocated and silently ignored — so the edit
/// cannot be constrained to the background, and an unconstrained relight rewrites the subject,
/// which is precisely what an identity reference must not do. Matte-on-white already removes the
/// environmental light this was meant to fix.
async fn prepare_reference(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PrepareReferenceQuery>,
    body: Bytes,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty image");
    }
    let width = q.width.unwrap_or(1280).clamp(64, 4096);
    let height = q.height.unwrap_or(704).clamp(64, 4096);
    match prepare_reference_bytes(&state, body, width, height, q.skip_matte).await {
        Ok((flattened, edge_report)) => match store_prepared_reference(&state, &flattened).await {
            Ok(hash) => Json(json!({
                "ref": director_assets::make_ref(&hash),
                "hash": hash,
                "bytes": flattened.len(),
                "content_type": "image/png",
                "width": width,
                "height": height,
                "matted": !q.skip_matte,
                "edges": edge_report,
            }))
            .into_response(),
            Err(resp) => resp,
        },
        Err((status, payload)) => (status, Json(payload)).into_response(),
    }
}

/// Store prepared bytes as a director asset. File before row -- a file with no row is invisible and
/// swept, whereas a row with no file is a ref that resolves to nothing.
async fn store_prepared_reference(state: &AppState, bytes: &[u8]) -> Result<String, Response> {
    let hash = director_assets::hash_bytes(bytes);
    if let Err(e) =
        director_assets::write_bytes(state.director_assets_dir.as_ref(), &hash, bytes).await
    {
        warn!(error = %e, "prepared reference write failed");
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "asset write failed"));
    }
    if let Err(e) = repo_assets::upsert(&state.db, &hash, "image/png", bytes.len() as i64).await {
        warn!(error = %e, "prepared reference upsert failed");
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "asset store failed"));
    }
    Ok(hash)
}

/// The preparation pipeline itself, split out of the HTTP handler so the TAKE-PROMOTION path can
/// reuse it. Returns the prepared PNG and the edge report, or a status plus a JSON body to hand
/// straight back. A 422 here is not transient: the image is unusable AS A REFERENCE, and the
/// message names the edge and the consequence.
async fn prepare_reference_bytes(
    state: &AppState,
    body: Bytes,
    width: u32,
    height: u32,
    skip_matte: bool,
) -> Result<(Bytes, serde_json::Value), (StatusCode, serde_json::Value)> {
    // 1. Matte to RGBA. `bg_mode=alpha` is the one output that carries BOTH what we need: the
    //    cutout to flatten and the alpha to validate. Asking for the mask separately would cost a
    //    second GPU call for information already in this response.
    let cutout: Bytes = if skip_matte {
        body.clone()
    } else {
        if !state.matting.is_configured() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": "matting service not configured — supply a reference already cut out on white, or skip preparation"}),
            ));
        }
        let opts = crate::matting::RemoveOptions {
            bg_mode: Some("alpha".to_string()),
            refine: Some(true),
            ..Default::default()
        };
        // Raw PNG rather than `png_base64`: one image, and the caller wants bytes (§5a-a).
        match state
            .matting
            .remove_single_raw("reference.png", body, &opts)
            .await
        {
            Ok(cutout) if cutout.png.is_empty() => {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    json!({"error": "matting returned no image"}),
                ));
            }
            Ok(cutout) => cutout.png,
            Err(e) => {
                warn!(error = %e, "reference matting failed");
                return Err((
                    StatusCode::BAD_GATEWAY,
                    json!({"error": format!("background removal failed: {e}")}),
                ));
            }
        }
    };

    // 2. Edge validation. Skipped for an already-flat input, which has no alpha to read.
    let mut edge_report = serde_json::Value::Null;
    if !skip_matte {
        match reference_edge_alpha(&cutout).await {
            Ok(edges) => {
                let worst = edges.worst();
                edge_report = json!({
                    "top": edges.top, "bottom": edges.bottom,
                    "left": edges.left, "right": edges.right,
                    "threshold": REFERENCE_EDGE_ALPHA_FRACTION,
                });
                if worst > REFERENCE_EDGE_ALPHA_FRACTION {
                    return Err((
                        StatusCode::UNPROCESSABLE_ENTITY,
                        json!({
                            "error": format!(
                                "the subject is CROPPED — {:.0}% of the {} edge is subject, not background. \
                                 The model cannot see what was cut off, so it reinvents that part \
                                 differently in every shot (a reference cut at the ankles produced a \
                                 different pair of feet each time). Re-shoot or re-crop so nothing \
                                 touches the frame edge.",
                                worst * 100.0,
                                edges.worst_edge(),
                            ),
                            "edges": edge_report,
                        }),
                    ));
                }
            }
            // A validation we could not run must not block an otherwise fine reference; it is
            // reported as unknown so the editor can say so rather than imply a pass.
            Err(e) => warn!(error = %e, "reference edge validation could not run"),
        }
    }

    // 3. Flatten on white + letterbox to the render aspect.
    let flattened = match letterbox_on_white(&cutout, width, height).await {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!(error = %e, "reference letterbox failed");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "could not letterbox the reference"}),
            ));
        }
    };
    Ok((flattened, edge_report))
}

/// How much of each frame edge carries subject alpha, as a fraction of that edge's length.
struct ReferenceEdgeAlpha {
    top: f64,
    bottom: f64,
    left: f64,
    right: f64,
}

impl ReferenceEdgeAlpha {
    fn worst(&self) -> f64 {
        self.top.max(self.bottom).max(self.left).max(self.right)
    }
    fn worst_edge(&self) -> &'static str {
        let w = self.worst();
        if w == self.top {
            "top"
        } else if w == self.bottom {
            "bottom"
        } else if w == self.left {
            "left"
        } else {
            "right"
        }
    }
}

/// Read an RGBA PNG's alpha plane and measure each border row/column.
///
/// ffmpeg rather than a PNG-decoding crate on purpose: ffmpeg is already a hard dependency of this
/// module, `alphaextract` hands back the plane as one grayscale image, and `rawvideo` makes it a
/// flat `width * height` byte buffer with no decoder to add and no format surprises.
async fn reference_edge_alpha(png: &[u8]) -> std::io::Result<ReferenceEdgeAlpha> {
    let (width, height) =
        png_dimensions(png).ok_or_else(|| std::io::Error::other("cutout is not a readable PNG"))?;
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "-i",
            "-",
            "-vf",
            "alphaextract",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gray",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("ffmpeg stdin unavailable"))?;
        stdin.write_all(png).await?;
        stdin.shutdown().await?;
    }
    let output = child.wait_with_output().await?;
    // An ffmpeg FAILURE must not read as a clean pass. Without this, a missing `alphaextract`, a
    // truncated pipe, or matting handing back an opaque PNG because `bg_mode=alpha` was ignored all
    // produce empty stdout -- which the short-plane branch below would report as
    // `{top:0, bottom:0, left:0, right:0}`, i.e. an affirmative "nothing touches the edge" for the
    // one check nobody can perform by eye. Fail loudly so the caller takes the Err branch and the
    // editor says UNKNOWN rather than PASSED.
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "ffmpeg alphaextract failed ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let plane = output.stdout;
    let expected = width as usize * height as usize;
    if plane.is_empty() {
        // Genuinely no alpha channel -- an opaque PNG. Distinct from the failure above: there is
        // no matte, so there is no severed subject to find. That is a real pass.
        return Ok(ReferenceEdgeAlpha {
            top: 0.0,
            bottom: 0.0,
            left: 0.0,
            right: 0.0,
        });
    }
    if plane.len() < expected {
        // No alpha channel at all (an opaque PNG) yields nothing; that is a pass, not a failure —
        // there is no matte to have severed anything.
        return Ok(ReferenceEdgeAlpha {
            top: 0.0,
            bottom: 0.0,
            left: 0.0,
            right: 0.0,
        });
    }
    let w = width as usize;
    let h = height as usize;
    let opaque = |x: usize, y: usize| plane[y * w + x] > REFERENCE_EDGE_ALPHA_FLOOR;
    let count_row = |y: usize| (0..w).filter(|&x| opaque(x, y)).count() as f64 / w as f64;
    let count_col = |x: usize| (0..h).filter(|&y| opaque(x, y)).count() as f64 / h as f64;
    Ok(ReferenceEdgeAlpha {
        top: count_row(0),
        bottom: count_row(h - 1),
        left: count_col(0),
        right: count_col(w - 1),
    })
}

/// Scale to fit inside `width` x `height` and centre on a WHITE canvas, never cropping.
///
/// # 🔴 EVERY PIXEL HERE COSTS SAMPLING TIME ON EVERY STEP OF EVERY SHOT
///
/// A reference is encoded to latent cells of 32x32 pixels, and each cell is a token the DiT carries
/// through all 48 blocks on every sampling step of every shot the reference is scoped to. MEASURED
/// at 1280x704x97: ~4.5s per 880 cells. A token holding flat white costs exactly what a token
/// holding a face costs.
///
/// This function pads to the RENDER resolution, and on the first image measured the subject occupied
/// only 30.9% of that canvas — so ~69% of the reference's tokens encode flat white.
///
/// ## 🔴 DO NOT "FIX" THAT BY CROPPING THE CANVAS. It was tried and it BREAKS THE CHARACTER.
///
/// Cropping to the subject cut 880 tokens to 330 and saved 3.97s (44% of the reference's cost) —
/// and rendered a giant head on a vestigial body, reproducibly, on two seeds on `echo-full`.
///
/// The reason: cropping changes the subject's SCALE RELATIVE TO ITS REFERENCE FRAME. Wide, the
/// subject fills ~31% of the frame; cropped, it fills most of it, and the model reads that ratio as
/// information about how large the subject is. The padding is not dead space — the subject-to-frame
/// ratio is itself a signal, and BOTH checkpoints were trained on render-resolution references
/// (the engine's own `ltxav_fit_image_to_canvas` says the letterbox is "a trained convention, not a
/// presentation choice", and MSR's published examples use same-sized frames too).
///
/// So the white tokens are not waste from the model's point of view. They are the convention.
///
/// What is still open, and is a DIFFERENT operation: DOWNSCALING the whole reference. That
/// preserves the aspect, the letterbox and the subject-to-frame ratio exactly, and spends only
/// resolution — so it does not fight the convention the way cropping did. Its risk is losing fine
/// detail, not distorting proportions. Untested.
///
/// One more constraint if anyone does change the geometry: references are packed on the frame axis
/// of ONE tensor (`stable-diffusion.cpp`, "all references in one shot must share a resolution"), so
/// anything per-reference makes a two-reference shot fail at RENDER time as a generic
/// `generation_failed`. Whatever the canvas is, it must be uniform across a shot.
///
/// The padding VALUE is not a presentation choice: white is the trained convention for a
/// memory-regime reference, and `force_original_aspect_ratio=decrease` is what guarantees the
/// subject keeps every pixel it arrived with. An RGBA cutout composited over the white canvas is
/// also what flattens it, so this is one pass rather than two.
async fn letterbox_on_white(png: &[u8], width: u32, height: u32) -> std::io::Result<Bytes> {
    let filter = format!(
        "[0:v]scale={width}:{height}:force_original_aspect_ratio=decrease[fg];\
         color=c=white:s={width}x{height}[bg];\
         [bg][fg]overlay=(W-w)/2:(H-h)/2:format=auto,format=rgb24"
    );
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "-i",
            "-",
            "-filter_complex",
            filter.as_str(),
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("ffmpeg stdin unavailable"))?;
        stdin.write_all(png).await?;
        stdin.shutdown().await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(std::io::Error::other(format!(
            "ffmpeg letterbox failed ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(Bytes::from(output.stdout))
}

/// `POST …/projects/{id}/renders/{tier}/{shot}/select` — choose which take of a shot is the one.
///
/// This only moves the pointer. The stitched cut is NOT re-derived here, because the engine's
/// banked latents still hold whichever take was rendered last: call `…/{tier}/rebuild` to
/// re-concatenate the cut from the selected takes.
async fn select_take(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tier, shot)): Path<(String, String, String)>,
    Json(body): Json<SelectTakePayload>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = normalize_tier(&tier) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };
    let Some(shot_index) = parse_shot_index(&shot) else {
        return err(StatusCode::BAD_REQUEST, "bad shot index");
    };
    match repo_renders::select_take(&state.db, &id, tier, shot_index, body.variant).await {
        Ok(true) => Json(json!({ "ok": true, "variant": body.variant })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "take not found"),
        Err(e) => {
            warn!(error = %e, "director take select failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "select take failed")
        }
    }
}

/// `POST …/projects/{id}/renders/{tier}/rebuild` — re-derive the stitched cut from the SELECTED
/// take of every shot.
///
/// A concatenation is the honest operation here: the engine's own stitched output is exactly the
/// sequence of the per-shot clips it delivered (each already trimmed of its continuation overlap),
/// so joining the selected takes reproduces it.
///
/// 🔴 That "already trimmed" is a property of how the takes were FETCHED, not of the takes. On LTX
/// the engine trims each window itself. On H3 the per-generation route serves the UNTRIMMED render
/// by default — deliberately, because the latent bank and the pixel tail must see every frame — and
/// with the overlay anchor on (the locked continuation method) a continuation's frame 0 is a
/// reproduction of the previous generation's LAST frame. Stream-copying those files together
/// duplicates that frame at every seam: a one-frame stutter per join, 15 of them in a 16-generation
/// cut, and nothing here can detect it after the fact. What keeps this function correct is that
/// `minimax_h3::fetch_generation` asks for `?joinable=1`, so the bytes on disk are already
/// join-safe. Do not "optimise" that query away.
///
/// What it deliberately does NOT do is re-render —
/// picking an older take cannot rewind the engine's banked latents, and LTX is not bit-reproducible
/// from a seed anyway, so a "re-render the chosen take" button would quietly hand back a
/// different clip.
async fn rebuild_cut(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tier)): Path<(String, String)>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = normalize_tier(&tier) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };
    let Some(dir) = project_render_dir(&state, &id) else {
        return err(StatusCode::BAD_REQUEST, "bad project id");
    };
    let shots = match repo_renders::selected_shots(&state.db, &id, tier).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "director rebuild: list failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "list takes failed");
        }
    };
    if shots.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no rendered shots to join");
    }
    let content_type = shots[0].content_type.clone();
    // ffmpeg concat demuxer over the selected takes. Stream-copy: these are all the same encoder
    // settings from the same render chain, so there is nothing to re-encode and no generation loss.
    let list_path = dir.join(format!("{tier}_rebuild.txt"));
    let mut list = String::new();
    for row in &shots {
        // The concat demuxer takes a path per line; these are basenames in `dir`, which is also
        // ffmpeg's working reference for a relative path in the list file.
        list.push_str(&format!(
            "file '{}'\n",
            row.file_name.replace('\'', "'\\''")
        ));
    }
    if let Err(e) = tokio::fs::write(&list_path, list).await {
        warn!(error = %e, "director rebuild: list write failed");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "rebuild failed");
    }
    // 🔴 The output container has to match the takes'. `-c copy` streams the source codec through
    // unchanged, and an H3 take is x265 — which webm cannot carry at all, so a hard-coded `.webm`
    // here makes ffmpeg refuse the mux outright rather than producing a wrong file.
    let out_name = format!("{tier}_rebuilt.{}", container_ext(&content_type));
    let out_path = dir.join(&out_name);
    let status = tokio::process::Command::new("ffmpeg")
        .arg("-y")
        .args(["-f", "concat", "-safe", "0"])
        .arg("-i")
        .arg(&list_path)
        .args(["-c", "copy"])
        .arg(&out_path)
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    let _ = tokio::fs::remove_file(&list_path).await;
    match status {
        Ok(s) if s.success() => {}
        other => {
            warn!(project = %id, tier, status = ?other, "director rebuild: ffmpeg concat failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "ffmpeg concat failed");
        }
    }
    // The rebuilt cut becomes a take of shot -1, tagged with a synthetic source job so it never
    // collides with a rendered cut and so rebuilding twice replaces its own take rather than
    // stacking copies.
    let source_job_id = format!("rebuild:{tier}");
    let variant = match repo_renders::next_variant(&state.db, &id, tier, -1, &source_job_id).await {
        Ok((v, _)) => v,
        Err(e) => {
            warn!(error = %e, "director rebuild: take lookup failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "rebuild failed");
        }
    };
    if let Err(e) = repo_renders::upsert(
        &state.db,
        repo_renders::ShotRenderUpsert {
            project_id: &id,
            tier,
            shot_index: -1,
            prompt: "",
            file_name: &out_name,
            content_type: &content_type,
            latent_job_id: None,
            // A rebuilt cut is a concat of takes, not a render — it has no bank entry of its own,
            // and therefore no engine store for one to live in.
            bank_key: None,
            engine: None,
            source_job_id: &source_job_id,
            variant,
            seed: None,
        },
    )
    .await
    {
        warn!(error = %e, "director rebuild: upsert failed");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "rebuild failed");
    }
    Json(json!({
        "ok": true,
        "shots": shots.len(),
        "variant": variant,
        "url": format!("{}/takes/{}", render_url(&id, tier, -1), variant),
    }))
    .into_response()
}

/// Serve one persisted clip with a cache validator, so a re-read costs a 304 rather than the file.
///
/// The editor re-reads a project's clips on every load and after every render, and a `final` shot is
/// 1.4–19.5 MB through an auth-gated route. Without a validator nothing can revalidate, so every one
/// of those reads was the whole file again — the cost the proxy tier exists to avoid, reintroduced
/// on the path a phone actually uses.
///
/// The tag is (size, mtime), not the file name: `rebuild_cut` rewrites `<tier>_rebuilt.<ext>` in
/// place with new content, so a name-derived tag would pin a stale cut forever.
async fn serve_render_file(
    headers: &HeaderMap,
    path: &std::path::Path,
    content_type: &str,
) -> Response {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return err(StatusCode::NOT_FOUND, "render file missing");
    };
    let tag = render_etag(&meta);
    let cache = "private, max-age=0, must-revalidate";
    if headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|sent| etag_matches(sent, &tag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (axum::http::header::ETAG, tag.as_str()),
                (axum::http::header::CACHE_CONTROL, cache),
            ],
        )
            .into_response();
    }
    let Ok(bytes) = tokio::fs::read(path).await else {
        return err(StatusCode::NOT_FOUND, "render file missing");
    };
    let mut resp = byte_serve::serve_bytes(headers, Bytes::from(bytes), content_type);
    let out = resp.headers_mut();
    if let Ok(value) = tag.parse() {
        out.insert(axum::http::header::ETAG, value);
    }
    out.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, max-age=0, must-revalidate"),
    );
    resp
}

fn render_etag(meta: &std::fs::Metadata) -> String {
    let stamp = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\"{:x}-{stamp:x}\"", meta.len())
}

/// `If-None-Match` matching, per RFC 9110: `*`, or any member of the comma list, weak comparison
/// (a `W/` prefix on either side is not a difference for a media re-read).
fn etag_matches(sent: &str, tag: &str) -> bool {
    let strip = |v: &str| v.trim().trim_start_matches("W/").to_string();
    let tag = strip(tag);
    sent.trim() == "*" || sent.split(',').any(|candidate| strip(candidate) == tag)
}

/// `final` (the stitched cut, stored as shot −1) or a prompt-filtered shot index.
fn parse_shot_index(shot: &str) -> Option<i32> {
    if shot.eq_ignore_ascii_case("final") {
        Some(-1)
    } else {
        shot.parse().ok().filter(|n: &i32| *n >= 0)
    }
}

/// `GET /api/v1/ltx-director/projects/{id}/renders` — the project's persisted clips across BOTH
/// tiers (stitched final + per-shot), each with a koblem URL the editor fetches to rehydrate
/// playback/status on load.
async fn list_renders(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match repo_renders::list_by_project(&state.db, &id).await {
        Ok(rows) => {
            // One entry per (tier, shot): the SELECTED take, plus how many takes that shot has, so
            // the editor knows there is something to choose between without fetching every take.
            let mut counts: std::collections::HashMap<(String, i32), usize> =
                std::collections::HashMap::new();
            for r in &rows {
                *counts.entry((r.tier.clone(), r.shot_index)).or_default() += 1;
            }
            let items: Vec<Value> = rows
                .into_iter()
                .filter(|r| r.selected)
                .map(|r| {
                    let takes = counts.get(&(r.tier.clone(), r.shot_index)).copied().unwrap_or(1);
                    json!({
                        "tier": r.tier,
                        "shot_index": r.shot_index,
                        "prompt": r.prompt,
                        "content_type": r.content_type,
                        "created_at": r.created_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
                        "url": render_url(&id, &r.tier, r.shot_index),
                        "variant": r.variant,
                        "takes": takes,
                        "seed": r.seed,
                        "critic": r.critic,
                        // The render that produced this clip. A client that watched that render
                        // already holds the identical bytes, so this is what lets it re-read the
                        // durable rows without re-downloading the whole set behind them.
                        "source_job_id": r.source_job_id,
                        // The engine chain this take was banked out of. The editor adopts it as the
                        // tier's resume target, so a device that never dispatched the render can
                        // still retake one shot instead of re-rendering the whole prefix. NULL on a
                        // pre-0027 row — the client must keep whatever it already had.
                        "latent_job_id": r.latent_job_id,
                        // Which engine's store that chain lives in. The editor persists its resume
                        // target across reloads and the engine is a picker the author can move, so
                        // without this the client re-adopts a chain the current engine 404s on.
                        // NULL on a row `0039` could not place, and on a rebuilt cut.
                        "engine": r.engine,
                    })
                })
                .collect();
            Json(json!({ "items": items })).into_response()
        }
        Err(e) => {
            warn!(error = %e, "director renders list failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "list renders failed")
        }
    }
}

#[derive(Deserialize)]
struct MediaQuery {
    /// Fallback auth for a `<video src>`, which cannot set an Authorization header — it can only
    /// carry a query param. The bearer header still takes precedence.
    token: Option<String>,
}

/// Bearer header OR `?token=`, validated identically. Anything that streams bytes a media element
/// plays needs the second path; skipping validation on it would make the route an open media proxy.
async fn media_auth_ok(
    pool: &sqlx::PgPool,
    api_key_env: Option<&str>,
    headers: &HeaderMap,
    token: Option<&str>,
) -> bool {
    if require_auth(pool, api_key_env, headers).await.is_ok() {
        return true;
    }
    let Some(token) = token.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    crate::auth::validate_token(pool, api_key_env, token)
        .await
        .is_ok()
}

/// `GET /api/v1/ltx-director/projects/{id}/renders/{tier}/{shot}` — stream one persisted clip from
/// the project's owned dir. `tier` is `proxy`|`final`; `shot` is `final` (the stitched result) or a
/// prompt-filtered shot index. Accepts `?token=` so the URL can be used directly as a `<video src>`
/// instead of the bytes having to be copied somewhere a media element can reach.
async fn get_render(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tier, shot)): Path<(String, String, String)>,
    Query(q): Query<MediaQuery>,
) -> Response {
    if !media_auth_ok(
        &state.db,
        state.api_key.as_deref(),
        &headers,
        q.token.as_deref(),
    )
    .await
    {
        return err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let Some(tier) = normalize_tier(&tier) else {
        return err(StatusCode::BAD_REQUEST, "bad tier");
    };
    let Some(shot_index) = parse_shot_index(&shot) else {
        return err(StatusCode::BAD_REQUEST, "bad shot index");
    };
    let Some(dir) = project_render_dir(&state, &id) else {
        return err(StatusCode::BAD_REQUEST, "bad project id");
    };
    let row = match repo_renders::get(&state.db, &id, tier, shot_index).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "render not found"),
        Err(e) => {
            warn!(error = %e, "director render get failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "get render failed");
        }
    };
    serve_render_file(&headers, &dir.join(&row.file_name), &row.content_type).await
}

// ─── Content-addressed editor assets ──────────────────────────────────────────

/// `POST /api/v1/ltx-director/assets` — store an editor asset and return its reference.
///
/// Body is the raw bytes (Content-Type names the media type). Responds `{ ref, hash, bytes }`,
/// where `ref` is what the timeline stores in place of the base64 it used to inline.
///
/// Idempotent by content: the SHA-256 IS the identity, so re-uploading bytes already held is a
/// metadata touch and no write. That makes a retry free and de-duplicates a still reused across
/// shots down to one row and one file.
async fn upload_asset(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty asset");
    }
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let hash = director_assets::hash_bytes(&body);
    let dir = state.director_assets_dir.as_ref();
    // Write the file before the row: a file with no row is invisible and swept, whereas a row
    // with no file is a ref that resolves to nothing.
    if let Err(e) = director_assets::write_bytes(dir, &hash, &body).await {
        warn!(error = %e, "director asset write failed");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "asset write failed");
    }
    if let Err(e) = repo_assets::upsert(&state.db, &hash, &content_type, body.len() as i64).await {
        warn!(error = %e, "director asset upsert failed");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "asset store failed");
    }
    Json(json!({
        "ref": director_assets::make_ref(&hash),
        "hash": hash,
        "bytes": body.len(),
        "content_type": content_type,
    }))
    .into_response()
}

/// `GET /api/v1/ltx-director/assets/{hash}` — stream an asset back.
///
/// The response is marked immutable and cached for a year: the URL names the content, so it can
/// never go stale. That is what lets the editor stop rebuilding `data:` URIs on every render and
/// lets the browser keep them across reloads.
async fn get_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let content_type = match repo_assets::touch_content_type(&state.db, &hash).await {
        Ok(Some(ct)) => ct,
        Ok(None) => return err(StatusCode::NOT_FOUND, "asset not found"),
        Err(e) => {
            warn!(error = %e, "director asset lookup failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "asset lookup failed");
        }
    };
    let Some(bytes) = director_assets::read_bytes(state.director_assets_dir.as_ref(), &hash).await
    else {
        return err(StatusCode::NOT_FOUND, "asset bytes missing");
    };
    let mut resp = byte_serve::serve_bytes(&headers, bytes, &content_type);
    // `private` because the fetch carries an Authorization header — a shared cache must not keep
    // it, a browser cache may.
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    resp
}

/// Delete every stored asset that no saved project references and that nothing has read for
/// `stale_days`. Best-effort and idempotent; called on a timer from `main`.
pub async fn sweep_assets(state: &AppState, stale_days: i64) {
    let hashes = match repo_assets::stale_unreferenced(&state.db, stale_days).await {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "director asset sweep query failed");
            return;
        }
    };
    if hashes.is_empty() {
        return;
    }
    let dir = state.director_assets_dir.as_ref();
    for hash in &hashes {
        if let Some(path) = director_assets::path_for(dir, hash) {
            let _ = tokio::fs::remove_file(path).await;
        }
        if let Err(e) = repo_assets::delete(&state.db, hash).await {
            warn!(error = %e, hash = %hash, "director asset delete failed");
        }
    }
    tracing::info!(count = hashes.len(), "director assets swept");
}

// ─── Auto-director (LLM plan) ─────────────────────────────────────────────────

/// A lyric/spoken word with start/end seconds. Either supplied pre-aligned by the
/// caller (the editor already has these from `/transcribe`) or produced here by
/// STT from `audio_b64`. `start` is authoritative for shot-cut timing.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct LyricWord {
    start: f64,
    #[serde(default)]
    end: f64,
    #[serde(default)]
    text: String,
}

/// A reference image handed to the director. The LLM (a VLM) studies it; a shot
/// that cites it renders i2v from that exact frame.
#[derive(Debug, Clone, Deserialize)]
struct RefImage {
    /// Bare or `data:`-prefixed base64.
    image_b64: String,
    #[serde(default)]
    label: Option<String>,
}

/// `POST /api/v1/ltx-director/plan` body. Everything except `brief`/`ref_images`
/// is optional; sensible LTX defaults fill the rest.
#[derive(Debug, Deserialize)]
struct PlanRequest {
    #[serde(default)]
    brief: String,
    #[serde(default)]
    fps: Option<u32>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    /// Base DiT variant. Left to the caller; the auto-director never selects the
    /// `lipdub` variant — dialogue/singing uses native LTX joint-AV, not relip.
    #[serde(default)]
    model: Option<String>,
    /// Runtime LoRAs to apply on top of `model`, each at its own strength. Passed straight
    /// through to the compiled render spec. Empty → none. Unlike `model`, these are tunable
    /// per render, at the cost of VRAM and sampling time per adapter.
    #[serde(default)]
    loras: Vec<LtxRuntimeLora>,
    /// "none" | "voice" | "bgm" | "mixed". Drives the compiled `audio_source`.
    #[serde(default)]
    audio_kind: Option<String>,
    /// Song/voice track (bare or data-URI base64). When present and `lyrics` is
    /// empty, koblem word-aligns it via STT before planning.
    #[serde(default)]
    audio_b64: Option<String>,
    /// Pre-aligned word timings (skip STT if supplied).
    #[serde(default)]
    lyrics: Vec<LyricWord>,
    /// May the planner place Prompt Relay beats inside shots? `None`/true = yes. `false` = plan
    /// every shot as one continuous action, for a project that wants plain single-prompt shots or
    /// where the planner has been too beat-happy. Only governs what the model may SUGGEST — beats
    /// already on the timeline are never touched by planning.
    #[serde(default)]
    beats: Option<bool>,
    /// Reference images fed to the VLM director + used as i2v scene frames.
    #[serde(default)]
    ref_images: Vec<RefImage>,
    /// Overall target length used only for the no-lyrics (instrumental) path to
    /// distribute shot durations. Ignored when lyrics drive the timeline.
    #[serde(default)]
    target_seconds: Option<f64>,
    /// Hard cap on shot count (defaults to 16, clamped 1..=64). Long videos are planned in
    /// chunks so the LLM never does one oversized generation.
    #[serde(default)]
    max_shots: Option<u32>,
    /// STT language hint when aligning `audio_b64`.
    #[serde(default)]
    language: Option<String>,
    /// Project canon (backstory / characters / tone / rules) grounding every shot so the LLM
    /// doesn't hallucinate the scenario from the brief alone.
    #[serde(default)]
    story: Option<StoryContext>,
    /// The project's recurring cast, with the appearance sentence each character must carry
    /// verbatim into every shot they appear in. Empty ⇒ byte-identical to before.
    #[serde(default)]
    cast: Vec<LtxCastMember>,
    /// Which engine these shots will render on — `"ltx"` (default) or `"h3"`. Governs the PROMPT
    /// CONVENTIONS the planner is given, nothing else: H3 packs shots into GENERATIONS and cuts
    /// freely inside one, LTX composes continuations with beats. See `routes::h3_prompting`.
    #[serde(default)]
    engine: Option<String>,
    /// H3 only: the project's omni references, IN ORDER, so the planner can name the ones each shot
    /// is about. Ignored on LTX (whose identity refs cannot be addressed by name at all).
    #[serde(default)]
    h3_references: Vec<crate::routes::h3_prompting::H3RefBrief>,
}

/// One shot as emitted by the director LLM — STRUCTURE plus a light seed, never a written prompt.
/// `start_word` indexes into the lyric word list (the cut point); koblem turns word onsets into
/// exact seconds. The `seconds` field is the fallback for the no-lyrics path.
#[derive(Debug, Deserialize)]
struct LlmShot {
    #[serde(default)]
    start_word: Option<usize>,
    #[serde(default)]
    seconds: Option<f64>,
    /// What the planner writes per shot, which the per-shot Draft path later expands into the full
    /// prompt. On LTX a terse one-liner; on H3 the render's whole shooting script, which the H3
    /// schema calls `description` — see `h3_prompting::director_system_prompt` for why the
    /// one-liner was retired there.
    #[serde(default, alias = "description")]
    idea: String,
    /// H3 only: the one plain sentence the timeline shows for the render. The `idea` is now far too
    /// long to scan, so the planner writes the label alongside it.
    #[serde(default)]
    gist: Option<String>,
    /// H3 only: what this shot SOUNDS like, in a few words.
    ///
    /// H3 generates the soundtrack per shot from noise, so audio is a per-shot authoring decision
    /// with no LTX equivalent — on LTX the track is supplied once for the whole timeline and the
    /// planner has nothing to say about it. Captured at plan time (rather than left to the
    /// expansion pass) because it is a CONTINUITY decision: whether the score carries across a cut
    /// is only visible when you can see the neighbouring shots, which is the planner's view and not
    /// the per-shot writer's.
    #[serde(default)]
    audio: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    /// Index into `ref_images` this shot renders from (i2v), or null (t2v).
    #[serde(default)]
    ref_image: Option<usize>,
    /// Hard cut: this shot opens a NEW scene instead of flowing out of the previous shot's final
    /// frame. Drives the clip's `new_scene` → the segment's `scene_cut`, and tells the Draft path
    /// whether the shot BEFORE it must end on a continuation point.
    #[serde(default)]
    new_scene: Option<bool>,
    /// Prompt Relay beats INSIDE this one continuous shot — emitted for a shot that contains a
    /// distinct MOMENT rather than one continuous action. A single beat is kept and is the most
    /// reliable shape there is; a shot with none renders as one prompt on the engine's
    /// byte-identical null-mask path, which is still most shots.
    #[serde(default)]
    beats: Vec<LlmBeat>,
}

/// One beat the director placed inside a shot. `start_word` indexes the same transcript lane the
/// shot cuts use, so a beat can land on a word onset for free; `seconds` is a plain
/// offset-within-shot and is the right choice whenever there is no transcript, or the moment does
/// not correspond to anything being said. Neither is privileged — a beat is an action at a time,
/// not a caption on a word.
#[derive(Debug, Deserialize)]
struct LlmBeat {
    #[serde(default)]
    start_word: Option<usize>,
    #[serde(default)]
    seconds: Option<f64>,
    /// The beat clause — what CHANGES at this moment. Never the setting; the shot prompt owns that.
    #[serde(default)]
    idea: String,
}

/// One chunk of the director's reply: the shots for this window plus a rolling recap the next
/// window continues from (see [`run_director_llm`]).
#[derive(Debug, Deserialize)]
struct LlmChunk {
    /// `renders` on H3, where a planned entry is a whole render.
    #[serde(default, alias = "renders")]
    shots: Vec<LlmShot>,
    #[serde(default)]
    summary: Option<String>,
}

/// Convert a shot duration (seconds) to a VAE-valid LTX frame count: `8k+1`,
/// floored at 9. Mirrors `ltx_video::frames_from_seconds` (kept local to avoid a
/// cross-module pub leak). Used to derive a representative uniform
/// `segment_frames` for the compiled render (per-clip variable frames are the
/// GPU-gated follow-up; per-clip `seconds` is preserved for when it lands).
fn frames_from_seconds(seconds: f64, fps: u32) -> u32 {
    let raw = (seconds * fps as f64).round().max(1.0) as i64;
    let k = (((raw - 1) as f64) / 8.0).round().max(1.0) as i64;
    (k * 8 + 1) as u32
}

// 🔴 These used to be re-derived here — a second copy of the frame grid, the fps and the snap
// direction, in a file that also decides how long every shot is. They now come from
// `koblem-client`, which is where the shim's own constants are mirrored, because the two copies
// could drift by exactly the amount that matters: the grid gained a floor and a 362 cap with the
// WanGP contract, and a local copy without them would have gone on offering 5-frame shots the VAE
// cannot decode and 20-second ones the model cannot render. (The floor is 22, the VAE's own
// decodable minimum — it was 107 until 2026-08-07, which was MiniMax's documented 4 s duration
// guideline mistaken for a limit.)
pub(crate) use koblem_client::types::{H3_FPS, h3_frames_from_seconds};

/// The duration an H3 render will ACTUALLY be, once its frames are snapped.
///
/// The prompt writer is given this rather than the authored length, because on H3 the prompt itself
/// carries timings — a shot list in timecoded blocks, and audio cues at named seconds — so a writer
/// working from the authored number places its last cue past the end.
pub(crate) fn h3_snapped_seconds(seconds: f64) -> f64 {
    koblem_client::types::h3_seconds_from_frames(h3_frames_from_seconds(seconds))
}

use crate::routes::ltx_prompting::extract_json_object;

/// Render a slice `[start, end)` of the lyric lane as a compact transcript annotated with
/// GLOBAL word indices so cut points stay valid across chunks: `[42 @12.30s] word …`. The
/// director plans in windows over these slices, so each call only ever sees its own section.
fn lyrics_transcript_range(words: &[LyricWord], start: usize, end: usize) -> String {
    let end = end.min(words.len());
    let mut out = String::new();
    for (i, w) in words.iter().enumerate().take(end).skip(start) {
        if i > start {
            out.push(' ');
        }
        out.push_str(&format!("[{i} @{:.2}s] {}", w.start, w.text.trim()));
    }
    out
}

/// Split `len` items into `n` contiguous, roughly-equal ranges `[start, end)`. Used to give each
/// planning window its own slice of the lyric lane. `n` is clamped to `1..=len`.
fn partition_ranges(len: usize, n: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return vec![];
    }
    let n = n.clamp(1, len);
    let base = len / n;
    let rem = len % n;
    let mut ranges = Vec::with_capacity(n);
    let mut start = 0;
    for i in 0..n {
        let end = start + base + usize::from(i < rem);
        ranges.push((start, end));
        start = end;
    }
    ranges
}

/// `POST /api/v1/ltx-director/plan` — the LLM auto-director. Composes a full
/// [`DirectorTimeline`] (clips + per-shot prompts/durations/modes + compiled
/// render `request` + inline `assets`) from a brief + optional song (lyrics with
/// word timings) + reference images. Shot cuts snap to lyric onsets so the
/// visuals track what is sung/said when. The returned doc is drop-in for
/// `POST /projects` (save) → `/projects/{id}/run` (render).
///
/// Audio is native LTX joint-AV: dialogue/singing shots rely on LTX generating
/// the audio + lipsync from its joint latent — the planner never routes through
/// the `lipdub`/relip path (that was a Wan-VACE crutch for an audio-less model).
async fn plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<PlanRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }

    let brief = req.brief.trim().to_string();
    if brief.is_empty() && req.ref_images.is_empty() {
        return err(StatusCode::BAD_REQUEST, "brief or ref_images is required");
    }

    // Which engine's PROMPT CONVENTIONS this plan is written for, and which engine the emitted doc
    // says it is for. Both matter: the second is what stops auto-directing an H3 project from
    // handing back a document that reads as LTX (the editor rebuilds its whole state from this doc,
    // so an absent `engine` silently moves the project onto the other engine).
    let engine = request_engine(req.engine.as_deref());
    let h3 = engine == ENGINE_H3;
    // 24 is not H3's default, it is its only value — the frame grid, the 4x temporal VAE and the
    // 40 Hz audio latent rate are all defined at it, and the engine 400s anything else.
    let fps = if h3 {
        H3_FPS
    } else {
        req.fps.unwrap_or(24).clamp(8, 60)
    };
    let width = req.width.unwrap_or(1280);
    let height = req.height.unwrap_or(704);
    let max_shots = req.max_shots.unwrap_or(16).clamp(1, 64) as usize;
    let story = req.story.clone().unwrap_or_default();
    // 🔴 Taken from the request on BOTH engines. This used to be forced to "none" on H3, on the
    // reasoning that H3 refuses every audio INPUT field — and the force did far more than it
    // claimed. `audio_kind` is what gates the STT word-alignment below, so zeroing it here did not
    // merely decline to send a track: it deleted the LYRIC LANE, and with it every shot cut that
    // would have snapped to a word onset. An H3 music video planned this way cut on nothing.
    //
    // The premise was wrong anyway. H3 does take supplied audio — as a numbered `<Audio N>` omni
    // reference, per generation (`minimax_h3_handler.validate_generative_settings` accepts
    // `audio_prompt_type="A"`/`"AB"`). What it refuses is the LTX-shaped `audio_source` field, and
    // that is handled where it belongs: the H3 branch below replaces the compiled request wholesale
    // rather than sending a subset of the LTX one, so `audio_source` never reaches the engine.
    let audio_kind = req
        .audio_kind
        .as_deref()
        .unwrap_or("none")
        .trim()
        .to_lowercase();

    // 1. Resolve the lyric lane: caller-supplied timings win; otherwise word-align
    //    the supplied track via STT (same Parakeet path as /transcribe).
    let mut lyrics: Vec<LyricWord> = req.lyrics.clone();
    if lyrics.is_empty()
        && let Some(bytes) = req.audio_b64.as_deref().and_then(decode_b64)
    {
        let params = SttParams {
            timestamp_granularity: Some("word".to_string()),
            beam_size: None,
            language: req.language.clone(),
        };
        match state
            .stt
            .transcribe_upload("audio.wav", bytes, &params, None)
            .await
        {
            Ok((permit, resp, _)) => {
                drop(permit);
                lyrics = resp
                    .words
                    .into_iter()
                    .map(|w| LyricWord {
                        start: w.start,
                        end: w.end,
                        text: w.text,
                    })
                    .collect();
            }
            Err(e) => {
                warn!(error = %e, "auto-director STT align failed; planning without lyric onsets");
            }
        }
    }
    lyrics.sort_by(|a, b| a.start.total_cmp(&b.start));

    // 2. Ask the director LLM (gemma-3-12b-it if configured, else the main Qwen
    //    LLM) to segment the song/brief into shots.
    let shots = run_director_llm(
        &state,
        DirectorPlan {
            brief: &brief,
            story: &story,
            lyrics: &lyrics,
            ref_images: &req.ref_images,
            cast: &req.cast,
            max_shots,
            // Prompt Relay is an LTX conditioning layout; the H3 route refuses `beats` outright, so
            // a planner allowed to place them would produce a timeline that 400s at submit.
            allow_beats: !h3 && req.beats.unwrap_or(true),
            engine,
            h3_references: &req.h3_references,
        },
    )
    .await;
    if shots.is_empty() {
        return err(StatusCode::BAD_GATEWAY, "director LLM produced no shots");
    }

    // 3. Turn the LLM's cut points into exact per-shot seconds. With lyrics, each
    //    shot starts at its word's onset and runs until the next shot's onset
    //    (last shot → end of the last word); without lyrics, use the LLM's own
    //    `seconds` (or an even split of target_seconds).
    let mut durations = resolve_durations(&shots, &lyrics, req.target_seconds, fps);
    // 🔴 On H3 a planned entry is a RENDER. With no transcript, an entry that forgot its length
    // takes the full budget rather than the 4 s LTX fallback (the bug that made every render 4 s),
    // and nothing leaves here outside what one generation can render. With a transcript the length
    // is the gap between onsets, which the planner chose; it is kept, and the editor flags a
    // render that runs over.
    if h3 && lyrics.is_empty() {
        use crate::routes::h3_prompting::{MAX_GENERATION_SECONDS, MIN_GENERATION_SECONDS};
        for (d, shot) in durations.iter_mut().zip(&shots) {
            let want = shot.seconds.unwrap_or(MAX_GENERATION_SECONDS);
            *d = h3_snapped_seconds(want.clamp(MIN_GENERATION_SECONDS, MAX_GENERATION_SECONDS));
        }
    }

    // 4. Build clips + the compiled render request + inline assets.
    let mut clips: Vec<Value> = Vec::with_capacity(shots.len());
    let mut segments: Vec<LtxSegment> = Vec::with_capacity(shots.len());
    let mut seg_images_b64: Vec<Value> = Vec::with_capacity(shots.len());

    // Stage 1 output: structure + a light idea per shot. `prompt` stays EMPTY — the per-shot Draft
    // path fills it in once the user has signed off on the plan, with that shot's own length and
    // continuation context in hand.
    let mut synopsis_list = String::new();
    for (i, (shot, &seconds)) in shots.iter().zip(durations.iter()).enumerate() {
        let idea = shot.idea.trim().to_string();
        // i2v iff the shot cites a valid ref image AND the LLM didn't force t2v.
        let ref_img = shot
            .ref_image
            .and_then(|i| req.ref_images.get(i))
            .filter(|_| shot.mode.as_deref() != Some("t2v"));
        let is_i2v = ref_img.is_some();
        // A ref, not the bytes: this goes into the document the editor adopts and saves. The
        // extractor handed us base64; store-back returns the same hash the editor uploaded.
        let mut image_b64 = None;
        if let Some(r) = ref_img {
            image_b64 = Some(
                director_assets::ref_for_b64(
                    state.director_assets_dir.as_ref(),
                    &state.db,
                    &r.image_b64,
                )
                .await,
            );
        }
        // An i2v shot already renders fresh from its own frame, so the flag is only meaningful on a
        // plain t2v cut — and never on shot 0, which has nothing to continue from.
        //
        // On H3 it is unconditionally true past shot 0: shots share no latent state, so every
        // boundary IS a hard cut whatever the planner said. Recording it as anything else would
        // have the editor draw a continuation the render cannot produce.
        let new_scene = if h3 {
            i > 0
        } else {
            shot.new_scene.unwrap_or(false) && !is_i2v && i > 0
        };

        let seg_image_val = match &image_b64 {
            Some(b) => Value::String(b.clone()),
            None => Value::Null,
        };
        let mode = if is_i2v { "i2v" } else { "t2v" };

        // Prompt Relay beats. A beat's time comes from the transcript lane when the planner named a
        // word, and from its own `seconds` otherwise — there is no requirement for the shot to have
        // any transcript at all. Either way it ends up as an offset INSIDE this shot, which is what
        // both the editor and the render compiler want.
        //
        // A SINGLE beat is kept. It used to be discarded as "nearly inert, its Voronoi cell is the
        // whole shot" — true of the engine as it was, and fixed 2026-07-28 by capping the cell
        // radius. A lone beat is now the cleanest case there is: GPU-measured, one beat at 4.0s of a
        // 5s shot put its action exactly on 4.0s against a model that otherwise showed it from
        // frame zero. Dropping it here would have thrown away the strongest configuration.
        let shot_onset = shot.start_word.and_then(|w| lyrics.get(w)).map(|w| w.start);
        let mut shot_beats: Vec<Value> = Vec::new();
        for beat in shot.beats.iter().take(4) {
            let text = beat.idea.trim();
            if text.is_empty() {
                continue;
            }
            let word = beat.start_word.and_then(|w| lyrics.get(w));
            let offset = match (word.map(|w| w.start), shot_onset) {
                (Some(absolute), Some(base)) => absolute - base,
                _ => match beat.seconds {
                    Some(s) => s,
                    None => continue,
                },
            };
            if !offset.is_finite() {
                continue;
            }
            let mut value = json!({
                "seconds": offset.clamp(0.0, (seconds - 0.1).max(0.0)),
                "text": text,
            });
            if let Some(w) = word {
                value["label"] = json!(w.text.trim());
            }
            shot_beats.push(value);
        }
        shot_beats.truncate(3);
        let seg_beats: Vec<LtxBeat> = shot_beats
            .iter()
            .map(|b| LtxBeat {
                seconds: b["seconds"].as_f64().unwrap_or(0.0),
                text: b["text"].as_str().unwrap_or_default().to_string(),
                strength: None,
                window: None,
            })
            .collect();

        synopsis_list.push_str(&format!(
            "Shot {} [{mode} · {seconds:.0}s]: {idea}\n",
            i + 1
        ));
        let mut clip = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "idea": idea,
            "prompt": "",
            "mode": mode,
            "seconds": seconds,
            "image_b64": image_b64,
            "keyframes": [],
            "new_scene": new_scene,
            "beats": shot_beats,
        });
        if let Some(g) = shot
            .gist
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
        {
            clip["gist"] = json!(g);
        }
        // The shot's audio intent, carried onto the clip so the per-shot writer expands the sound
        // the planner heard rather than inventing its own. H3 only — on LTX the field has no
        // meaning and is omitted so an LTX doc stays byte-identical.
        if h3
            && let Some(a) = shot
                .audio
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
        {
            clip["h3_audio"] = json!(a);
        }
        clips.push(clip);
        segments.push(LtxSegment {
            prompt: String::new(),
            seconds: Some(seconds),
            image: is_i2v.then_some(LtxImageSource::Upload),
            keyframes: None,
            scene_cut: new_scene.then_some(true),
            beats: (!seg_beats.is_empty()).then_some(seg_beats),
            ..Default::default()
        });
        seg_images_b64.push(seg_image_val);
    }

    // Representative uniform segment_frames (median shot) — the run compiler emits
    // one global frame count today; per-clip `seconds` is retained for the
    // variable-frame follow-up.
    let mut sorted = durations.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2];
    // Each engine has its own VAE-valid grid — LTX `8k+1`, H3 `17n+5` — and the editor derives its
    // default shot length from this number, so snapping to the wrong one puts every shot's UI
    // duration a fraction out from what renders.
    let segment_frames = if h3 {
        h3_frames_from_seconds(median)
    } else {
        frames_from_seconds(median, fps)
    };

    let audio_source = match audio_kind.as_str() {
        "voice" => LtxAudioSource::VoiceUpload,
        "mixed" => LtxAudioSource::MixedUpload,
        "bgm" => LtxAudioSource::BgmUpload,
        _ => LtxAudioSource::None,
    };

    let request = LtxVideoGenerateRequest {
        segments,
        model: req.model.clone().filter(|m| !m.trim().is_empty()),
        loras: req.loras.clone(),
        // Director dims are the OUTPUT target; the profile sizes its base pass down from here.
        width: Some(width),
        height: Some(height),
        dims_are_target: Some(true),
        fps: Some(fps),
        segment_frames: Some(segment_frames),
        audio_source,
        save_clip: true,
        ..Default::default()
    };
    let mut request_val = match serde_json::to_value(&request) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "auto-director: failed to serialize compiled request");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "compile failed");
        }
    };
    // H3 takes a different request, not a subset of this one: it has no `cfg_scale`, no `model`, no
    // audio INPUT, no `fps` but 24 — and, since the WanGP contract, no `segments` either. Its unit
    // is a GENERATION holding 1-N shots.
    //
    // What this emits is deliberately EMPTY of generations. The plan's job is the timeline
    // (`clips`), and packing shots into generations is a function of their durations and the 15 s
    // budget, which `h3_request_for` does at render time from those same clips — after the user has
    // edited them, which is the only moment the packing is actually correct. Emitting a packing
    // here would be emitting one computed from durations the user is about to change, and the
    // editor would then have to know to throw it away.
    //
    // The canvas IS emitted, because it is a project setting rather than a derived one.
    if h3 {
        request_val = json!({
            "generations": [],
            "width": width,
            "height": height,
            "fps": H3_FPS,
            "save_clip": true,
        });
    }

    // Route the resolved audio into the compiled assets by role — as a ref, since a whole song
    // inlined into the emitted doc is the single biggest thing the asset store took out of it.
    let mut audio_ref = None;
    if let Some(b) = &req.audio_b64 {
        audio_ref = Some(
            director_assets::ref_for_b64(state.director_assets_dir.as_ref(), &state.db, b).await,
        );
    }
    let (audio_asset, bgm_asset) = match audio_kind.as_str() {
        "voice" | "mixed" => (audio_ref, None),
        "bgm" => (None, audio_ref),
        _ => (None, None),
    };

    // 5. Emit the DirectorTimeline doc (same shape the editor persists/saves/runs).
    // The story canon rides along in it, so its reference images get the same treatment; `story`
    // itself keeps the bytes, since that is the copy the VL calls above studied.
    let mut story_doc = story.clone();
    for r in &mut story_doc.references {
        // `take()` first: assigning back into a place still borrowed by the `if let` doesn't build.
        if let Some(b) = r.image_b64.take() {
            r.image_b64 = Some(
                director_assets::ref_for_b64(state.director_assets_dir.as_ref(), &state.db, &b)
                    .await,
            );
        }
    }
    let lyrics_val = serde_json::to_value(&lyrics).unwrap_or_else(|_| Value::Array(vec![]));
    // 🔴 NO ENGINE HAS DiT VARIANTS ANY MORE. H3 and LTX-2.5 each ship one transformer and refuse a
    // `model` that is not "" or "base", so a plan names none — and must not carry one across from a
    // document written when LTX-2.3 offered a picker. The failure is invisible from the editor:
    // 2.5's props answer `variants: []`, which HIDES the picker (`SettingsSheet` renders it only
    // when `variants.length > 1`) while the stale value stays in the saved timeline and is still
    // sent at submit, so the project 400s on every render with a field the user cannot see, let
    // alone clear. Observed 2026-08-13 on a project moved from 2.3 to 2.5 carrying
    // `model: "audioreactive"`.
    let doc_model: Option<String> = None;
    let timeline = json!({
        // v2: width/height are the OUTPUT target (see `dims_are_target`).
        "version": 2,
        // Which engine this plan was written FOR. Load-bearing, not decorative: the editor rebuilds
        // its entire state from this document, so a plan that omitted it would move an H3 project
        // onto LTX the moment the user accepted it — taking with it a shot list cut for hard cuts
        // and prompts carrying inline negatives and audio blocks.
        //
        // `h3_references` is deliberately NOT emitted here: the planner is handed reference briefs
        // (kind + label) and never the pixels, so it cannot reconstruct the roster. The editor
        // re-attaches its own on commit, exactly as it already does for `characters`.
        "engine": engine,
        "fps": fps,
        "segment_frames": segment_frames,
        "width": width,
        "height": height,
        "model": doc_model,
        "audio_kind": if audio_kind.is_empty() { "none" } else { audio_kind.as_str() },
        "clips": clips,
        "lyrics": lyrics_val,
        "request": request_val,
        "assets": {
            "opener_image_b64": Value::Null,
            "audio_b64": audio_asset,
            "bgm_b64": bgm_asset,
            "seg_images_b64": seg_images_b64,
        },
        "editor": {
            "generated_by": "auto-director",
            "story": serde_json::to_value(&story_doc).unwrap_or(Value::Null),
        },
    });

    // The end-to-end read-back of the plan the user is about to approve. Best-effort: a failed
    // synopsis costs a review aid, not the plan itself.
    let summary = write_synopsis(&state, &story, &synopsis_list, shots.len()).await;

    Json(PlanResponse { timeline, summary }).into_response()
}

#[derive(Debug, Serialize)]
struct PlanResponse {
    timeline: Value,
    /// Prose synopsis of the whole planned story, for the user to review before any prompt is
    /// drafted. Absent when the synopsis call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

/// One shot fed to the summariser — the editor's per-shot fields, no assets.
#[derive(Debug, Deserialize)]
struct SummaryShot {
    #[serde(default)]
    idea: Option<String>,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    seconds: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SummarizeRequest {
    #[serde(default)]
    shots: Vec<SummaryShot>,
    #[serde(default)]
    story: Option<StoryContext>,
    /// `"ltx"` (default) or `"h3"`.
    ///
    /// 🔴 It only decides ONE thing here — whether the per-shot line carries a `mode` tag — and that
    /// one thing was wrong on H3. The tag prints the clip's stored `mode`, which on an H3 project is
    /// a field nothing renders from: `t2v` there reads to the model as "this shot builds its scene
    /// from scratch", which is exactly backwards for a cut in the middle of a generation. See
    /// [`number_shots_for`] for the same fact from the other side.
    #[serde(default)]
    engine: Option<String>,
}

#[derive(Debug, Serialize)]
struct SummarizeResponse {
    summary: String,
}

/// `POST /api/v1/ltx-director/summarize` — read the whole shot list back to the
/// user as a short prose synopsis of the video it describes. One LLM call, no
/// images (prompts are already text); grounded in the project canon when present.
async fn summarize(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<SummarizeRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }

    let shots: Vec<&SummaryShot> = req
        .shots
        .iter()
        .filter(|s| {
            !s.prompt.trim().is_empty()
                || s.idea
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|i| !i.is_empty())
        })
        .collect();
    if shots.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no shots to summarise");
    }

    let h3 = request_engine(req.engine.as_deref()) == ENGINE_H3;
    let mut shot_list = String::new();
    for (i, s) in shots.iter().enumerate() {
        let body = if s.prompt.trim().is_empty() {
            s.idea.as_deref().unwrap_or("").trim()
        } else {
            s.prompt.trim()
        };
        // No `mode` on H3 — see `SummarizeRequest::engine`. The length still earns its place on both
        // engines: a synopsis of a 2-second shot and of a 15-second one are different paragraphs.
        let tag = match (h3, s.seconds) {
            (true, Some(v)) => format!(" [~{v:.0}s]"),
            (true, None) => String::new(),
            (false, secs) => {
                let mode = s.mode.as_deref().unwrap_or("t2v");
                let secs = secs.map(|v| format!(" (~{v:.0}s)")).unwrap_or_default();
                format!(" [{mode}{secs}]")
            }
        };
        shot_list.push_str(&format!("Shot {}{tag}: {body}\n", i + 1));
    }

    let story = req.story.unwrap_or_default();
    match write_synopsis(&state, &story, &shot_list, shots.len()).await {
        Some(summary) => Json(SummarizeResponse { summary }).into_response(),
        None => err(
            StatusCode::BAD_GATEWAY,
            "summarise failed — the LLM is unavailable",
        ),
    }
}

/// Read an ordered shot list back as a short prose synopsis of the video it describes. `/summarize`
/// and the auto-director's end-to-end review summary are the same job over the same list shape, so
/// they share the one call. `None` = the call failed or came back empty; a caller that has other
/// work to return (like `/plan`) can carry on without it.
async fn write_synopsis(
    state: &AppState,
    story: &StoryContext,
    shot_list: &str,
    n: usize,
) -> Option<String> {
    let mut system = String::from(
        "You are a film editor. Given an ordered list of shots for a short video, write a tight \
         prose synopsis (2–4 sentences) of the video the shots describe: the story/arc, the \
         setting and mood, and how it opens and closes. Describe the video as a viewer would \
         experience it — do not enumerate the shots, mention shot numbers, or discuss technical \
         fields like modes or durations. Reply with the synopsis only, no preamble.",
    );
    let canon = crate::routes::ltx_prompting::story_context_block(story);
    if !canon.is_empty() {
        system.push_str("\n\n");
        system.push_str(&canon);
    }

    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": format!("Shots ({n}):\n{shot_list}")},
        ],
        "max_tokens": 1024,
        "temperature": 0.5,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_director", &mut body).await;

    let client = state.director_llm.as_ref().unwrap_or(&state.llama);
    match client.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        Err(e) => {
            warn!(error = %e, "director synopsis: LLM call failed");
            None
        }
    }
}

/// One editor shot fed to rework/review — the per-shot text fields, no inline assets.
#[derive(Debug, Default, Deserialize)]
struct ShotIn {
    /// The clip's stable editor id. Every per-shot LLM result is keyed to this, never to a
    /// position: a small model miscounts, and multi-turn chat drifts as shots are added/removed.
    #[serde(default)]
    id: String,
    #[serde(default)]
    idea: Option<String>,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    seconds: Option<f64>,
    #[serde(default)]
    gist: Option<String>,
    /// Hard cut: this shot renders FRESH from its own prompt instead of continuing the previous
    /// shot's final frame. Drives whether the PREVIOUS shot is told to set up a continuation.
    #[serde(default)]
    new_scene: Option<bool>,
}

impl ShotIn {
    fn is_blank(&self) -> bool {
        self.prompt.trim().is_empty()
            && self.idea.as_deref().unwrap_or("").trim().is_empty()
            && self.gist.as_deref().unwrap_or("").trim().is_empty()
    }
    /// The fullest available text for this shot (prompt beats gist beats idea).
    fn body(&self) -> &str {
        let p = self.prompt.trim();
        if !p.is_empty() {
            return p;
        }
        let g = self.gist.as_deref().unwrap_or("").trim();
        if !g.is_empty() {
            return g;
        }
        self.idea.as_deref().unwrap_or("").trim()
    }
    /// True when this shot renders FRESH rather than continuing the previous shot's final frame:
    /// an explicit hard cut, or an i2v/v2v shot that starts from its own image / source clip. This
    /// mirrors the render path's scene classification (`ltx_video.rs` `seg_specs`/`scene_cut`).
    fn starts_fresh(&self) -> bool {
        self.new_scene.unwrap_or(false)
            || matches!(
                self.mode.as_deref(),
                Some("i2v" | "v2v" | "v2v_edit" | "v2v_restyle")
            )
    }
}

/// Whether the shot at `i` closes its scene — nothing continues out of its final frame. `last`
/// answers that for the final shot of the list, which the caller owns: for a whole timeline it
/// closes, but `/extend` is about to append to it, so there it continues.
fn closes_scene(shots: &[&ShotIn], i: usize, last: bool) -> bool {
    shots.get(i + 1).map_or(last, |next| next.starts_fresh())
}

/// Per-request shot ids for the LLM. The model never sees a clip's full uuid — it echoes a short
/// token it can copy without miscounting, and koblem maps that token back to the clip id. The
/// mapping is the ONLY thing trusted on the way back in: an id the model invented, or repeated, is
/// dropped rather than guessed at, because a dropped edit is recoverable and a wrong-shot edit is
/// not (that exact class of bug is why positional indexing was removed).
#[derive(Debug)]
struct ShotTokens {
    /// Token per shot, in the order the shots were listed to the model.
    order: Vec<String>,
    by_token: std::collections::HashMap<String, String>,
}

/// Reserved for "insert at the very start" in a chat `add_shot`. `@` can never appear in a token
/// (tokens keep alphanumerics only), so this can't collide with a real shot id.
const AT_START: &str = "@start";

impl ShotTokens {
    fn new(ids: &[&str]) -> Self {
        let mut by_token: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut order = Vec::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            let alnum: String = id
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .flat_map(char::to_lowercase)
                .collect();
            // 6 chars of a uuid is short enough for a small model to copy verbatim and wide enough
            // that a collision needs a deliberate duplicate id; widen, then fall back to an
            // ordinal, so a token is always unique WITHIN this request.
            let token = [6usize, 8, 12, 32]
                .into_iter()
                .map(|n| alnum.chars().take(n).collect::<String>())
                .find(|t| t.len() >= 4 && !by_token.contains_key(t))
                .unwrap_or_else(|| {
                    (i..)
                        .map(|n| format!("x{n}"))
                        .find(|t| !by_token.contains_key(t))
                        .expect("infinite range yields a free token")
                });
            by_token.insert(token.clone(), (*id).to_string());
            order.push(token);
        }
        Self { order, by_token }
    }

    fn token(&self, i: usize) -> &str {
        self.order.get(i).map(String::as_str).unwrap_or("")
    }

    /// The exact token list, for the prompt's "these are the only valid ids" line.
    fn joined(&self) -> String {
        self.order.join(", ")
    }
}

/// Resolves the ids an LLM echoed back to clip ids, at most once each per `scope`. Unknown and
/// repeated ids are DROPPED and recorded as warnings — never fuzzy-matched onto a shot.
struct IdResolver<'a> {
    tokens: &'a ShotTokens,
    /// A token space the reply may NAME but must not USE — H3 chat's cut ids, when the turn is
    /// addressing renders. See [`Self::with_foreign`].
    foreign: Option<&'a ShotTokens>,
    seen: std::collections::HashSet<(String, String)>,
    warnings: Vec<String>,
}

impl<'a> IdResolver<'a> {
    fn new(tokens: &'a ShotTokens) -> Self {
        Self {
            tokens,
            foreign: None,
            seen: std::collections::HashSet::new(),
            warnings: Vec::new(),
        }
    }

    /// Declare a SECOND token space that exists in this request but is not addressable.
    ///
    /// 🔴 **This is what makes an id from the wrong space fail LOUDLY.** On H3 a turn addressing
    /// renders is looking at a document whose cuts also have ids — the model has seen them in
    /// earlier turns, in a `/review` finding, or simply guesses one — and without this a cut id is
    /// indistinguishable from an invented one, so the user is told "the model did not echo an id it
    /// was given" for an id that was, once, exactly that. Naming the mistake is also the only way
    /// the *next* turn can correct it: the warning is what the client shows.
    ///
    /// ⚠️ It never resolves anything. A cut id is dropped either way — this only changes the
    /// sentence, which is the entire point: two token spaces, one addressable.
    fn with_foreign(mut self, foreign: &'a ShotTokens) -> Self {
        self.foreign = Some(foreign);
        self
    }

    /// Map one echoed id → its clip id, rejecting anything we can't map EXACTLY. Used directly for
    /// results a shot may legitimately receive more than one of (a review can raise two findings
    /// against the same shot); edits go through [`Self::resolve`] instead.
    fn lookup(&mut self, raw: Option<&str>) -> Option<(String, String)> {
        // Normalisation is format-only — trim, unwrap quoting, lowercase. Deliberately NOT a
        // prefix/nearest match: an id we can't map exactly is an id we must not act on.
        let key: String = raw
            .unwrap_or("")
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect();
        match self.tokens.by_token.get(&key) {
            Some(clip_id) => Some((key, clip_id.clone())),
            // A token from the foreign space is a DIFFERENT mistake from an invented one and gets
            // its own sentence — see `with_foreign`. Both are dropped; only the message differs.
            None if self.foreign.is_some_and(|f| f.by_token.contains_key(&key)) => {
                self.warnings.push(format!(
                    "dropped a result naming {:?} — that is the id of a moment INSIDE a shot, and on this engine an edit addresses the whole shot (one render, one prompt). Use the id in the shot's heading.",
                    raw.unwrap_or("").trim()
                ));
                None
            }
            None => {
                self.warnings.push(format!(
                    "dropped a result for unknown shot id {:?} — the model did not echo an id it was given",
                    raw.unwrap_or("").trim()
                ));
                None
            }
        }
    }

    /// Map one echoed id → its clip id, at most once per `scope`. Two EDITS naming the same shot
    /// are ambiguous — we can't know which the model meant to win — so the later one is dropped.
    /// `scope` separates independent edit streams that may each name a shot once (one chat turn can
    /// legitimately set_prompt AND set_gist on the same shot).
    fn resolve(&mut self, raw: Option<&str>, scope: &str) -> Option<String> {
        let (key, clip_id) = self.lookup(raw)?;
        if !self.seen.insert((scope.to_string(), key.clone())) {
            self.warnings.push(format!(
                "dropped a duplicate {scope} for shot id {key:?} — the model named the same shot twice"
            ));
            return None;
        }
        Some(clip_id)
    }
}

/// Render shots as a numbered block for the LLM: `Shot N (id t0k3n) [t2v · 6s · CONTINUES]: body`.
/// The ordinal is there so the model can reason and talk about order; the `id` is the ONLY thing it
/// may echo to name a shot. `tokens` is None for paths that return no per-shot results (they need
/// the list purely as context). `last_closes` — see [`closes_scene`].
///
/// 🔴 **Every caller passes its request's engine, and none may hard-code one.** There
/// used to be a one-line `number_shots` wrapper that did exactly that, and `/gist-shots` and
/// `/title` both reached for it because it was the shorter name — which listed every H3 script under
/// LTX's tags, silently, on a path that returns text nobody diffs. The wrapper is gone so the choice
/// has to be made at each call site.
///
/// The tag differs because the FACT differs. On LTX a shot is `carried` or fresh and either
/// CONTINUES into the next or CLOSES its scene, and the writer's job depends on which. On H3 the
/// `carried` mode does not exist — a listing that told an H3 writer a shot was `carried` would be
/// inviting it to write "she keeps walking" against a render that opens on noise.
///
/// 🔴 The H3 line used to carry a constant ` · CUT` flag, from the era when every shot was its own
/// render. Shots now live inside a generation and the cut between them is ordinary, so the flag
/// said nothing while implying an independence that is false — see
/// `h3_prompting::GENERATION_BOUNDARY_FACTS`. Only the length is left.
fn number_shots_for(
    engine: &str,
    shots: &[&ShotIn],
    tokens: Option<&ShotTokens>,
    last_closes: bool,
) -> String {
    if engine == ENGINE_H3 {
        let mut out = String::new();
        for (i, s) in shots.iter().enumerate() {
            let id = tokens
                .map(|t| format!(" (id {})", t.token(i)))
                .unwrap_or_default();
            // An unknown length yields an empty tag; print no brackets at all rather than `[]`.
            let len = crate::routes::h3_prompting::shot_length_tag(s.seconds);
            let len = if len.is_empty() {
                String::new()
            } else {
                format!(" [{len}]")
            };
            out.push_str(&format!("Shot {}{id}{len}: {}\n", i + 1, s.body()));
        }
        return out;
    }
    let mut out = String::new();
    for (i, s) in shots.iter().enumerate() {
        // A shot past the first that doesn't start fresh renders from the previous shot's final
        // frame — tag it `carried` (treat as i2v: describe the change, not the standing scene)
        // rather than its stored `t2v`, which would read as "build the whole scene from scratch".
        // The token is `carried`, not `cont`: a bare `cont` reads to the model like a "cont'd:"
        // prefix and leaks onto the front of the drafted prompt. Explained in
        // `ltx_prompting::multi_shot_pacing`.
        let mode = if i > 0 && !s.starts_fresh() {
            "carried"
        } else {
            s.mode.as_deref().unwrap_or("t2v")
        };
        let id = tokens
            .map(|t| format!(" (id {})", t.token(i)))
            .unwrap_or_default();
        let pacing = crate::routes::ltx_prompting::shot_pacing_tag(
            s.seconds,
            closes_scene(shots, i, last_closes),
        );
        out.push_str(&format!(
            "Shot {}{id} [{mode}{pacing}]: {}\n",
            i + 1,
            s.body()
        ));
    }
    out
}

/// The ids of every non-blank shot, in list order — the input to [`ShotTokens::new`].
fn shot_ids<'a>(shots: &[&'a ShotIn]) -> Vec<&'a str> {
    shots.iter().map(|s| s.id.as_str()).collect()
}

/// Which ids [`number_generations`] prints, and therefore which ids the reply may name.
///
/// 🔴 **One listing, two id spaces, because the two surfaces land their answers in different
/// places.** A `/review` finding becomes a one-click rework of one CLIP, so a finding named after a
/// render would arrive with nothing to fix. A chat EDIT has only one useful destination — the
/// render's block, which is the whole stored document — so an edit named after a cut lands on
/// `DirectorClip.prompt`, a mirror the block overwrites on the next block edit, and evaporates.
///
/// ⚠️ The two spaces must never both be printed. A listing carrying both invites the model to pick,
/// and half its answers then resolve to the wrong kind of thing.
#[derive(Debug, Clone, Copy)]
enum GenListingIds<'a> {
    /// Name the CUTS inside each render — `/review`.
    Cuts,
    /// Name the RENDER — chat. Indexed in step with `gens`, so `tokens.token(i)` is `gens[i]`'s id.
    Generations(&'a ShotTokens),
}

/// Render the H3 timeline as the reviewer has to see it: one entry per **generation** — one render
/// — carrying the assembled block that will actually be sent for it, verbatim.
///
/// 🔴 **This exists because [`number_shots_for`] cannot answer a schema question.** It prints one
/// line per clip: no sections, no `[Shot N] At MM:SS.mmm` cut tags, no reference labels, no
/// retention lines and no generation boundaries. A reviewer handed that and asked to find "a cut
/// time that is not strictly increasing" has nothing to look at, and a model asked for findings it
/// cannot derive returns invented ones. The block is the stored document
/// (`DirectorH3Generation.prompt`), so the reviewer gets the document.
///
/// 🔴 **Which ids the header carries is the CALLER's decision, and it is not cosmetic** — see
/// [`GenListingIds`]. `/review` names cuts because a finding becomes a one-click rework of one clip;
/// chat names renders because an edit's only useful destination is the render's block. Handing a
/// surface the other space produces edits that resolve to nothing, silently.
///
/// ⚠️ A generation may hold a clip with no text at all. Those were filtered out of `shots` upstream
/// and so have no token — they are left out of the header rather than given an id whose findings
/// would be dropped on the way back in.
///
/// The tail block is not decoration: a cut the packing does not place is one the reviewer would
/// otherwise never see, and silently dropping a shot from a continuity review is exactly the class
/// of miss the review exists to catch.
fn number_generations(
    gens: &[crate::routes::h3_prompting::H3GenerationBrief],
    shots: &[&ShotIn],
    tokens: &ShotTokens,
    ids: GenListingIds<'_>,
) -> String {
    // clip id → its position in the listing, which is what indexes `tokens`. First writer wins:
    // duplicate ids already collapse to one token in `ShotTokens`, so resolving them to the same
    // shot here keeps the two in step instead of inventing a second mapping.
    let mut by_id: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, s) in shots.iter().enumerate() {
        by_id.entry(s.id.as_str()).or_insert(i);
    }
    // `Cut 3 (id a1b2c3) [4.5s]: body` — the fallback line, used both inside an unwritten generation
    // and in the unplaced tail. `ordinal` is None where there is no generation to count within.
    //
    // 🔴 The `(id …)` is printed ONLY where cuts are addressable. Under generation ids a cut is
    // context and nothing else, and printing a token beside it is an invitation to name it — which
    // is the one thing the reply must not do, and which we can then only refuse.
    let cut_line = |ordinal: Option<usize>, i: usize| {
        let s = shots[i];
        let len = crate::routes::h3_prompting::shot_length_tag(s.seconds);
        let len = if len.is_empty() {
            String::new()
        } else {
            format!(" [{len}]")
        };
        let n = ordinal.map(|o| format!(" {o}")).unwrap_or_default();
        let id = match ids {
            GenListingIds::Cuts => format!(" (id {})", tokens.token(i)),
            GenListingIds::Generations(_) => String::new(),
        };
        format!("Cut{n}{id}{len}: {}\n", s.body())
    };

    let mut placed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut out = String::new();
    for (i, brief) in gens.iter().enumerate() {
        let cuts: Vec<usize> = brief
            .shots
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .collect();
        placed.extend(cuts.iter().copied());
        let count = match cuts.len() {
            1 => "1 cut".to_string(),
            n => format!("{n} cuts"),
        };
        let cut_ids = match cuts.as_slice() {
            [] => " (no cut ids — a finding about this one has to use null)".to_string(),
            [one] => format!(" (id {})", tokens.token(*one)),
            many => format!(
                " (ids {})",
                many.iter()
                    .map(|i| tokens.token(*i))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        // Two decimals, not one: cut times are written to the millisecond and a duration rounded to
        // 14.4s makes a legal 14.38s cut look out of range and an illegal 14.44s one look fine.
        let dur = brief
            .seconds
            .filter(|s| *s > 0.0)
            .map(|s| format!(" · {s:.2}s"))
            .unwrap_or_default();
        let task = brief
            .task
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| match brief.expected_field() {
                Some(field) => format!(" · {t} (body field `{field}:`)"),
                None => format!(" · {t}"),
            })
            .unwrap_or_default();
        // The header is where the id space shows itself. Under `Cuts` the render is unnamed and its
        // cuts are listed; under `Generations` the render carries the only id in the listing, plus
        // the author's own label for it so the model can recognise it without reading 7,000
        // characters of schema.
        match ids {
            GenListingIds::Cuts => {
                out.push_str(&format!("Shot {} — {count}{cut_ids}{dur}{task}", i + 1))
            }
            GenListingIds::Generations(g) => {
                // Quoted (Debug) so a label containing a `·` or a newline cannot be read as more
                // header fields — the model is being asked to tell these apart at a glance.
                let gist = brief
                    .gist()
                    .map(|label| format!(" · gist: {label:?}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "Shot {} (id {}) — {count}{dur}{task}{gist}",
                    i + 1,
                    g.token(i)
                ))
            }
        }
        match brief.block() {
            // ⚠️ A seeded block renders exactly like an authored one, so it is shown in full — but
            // it is unedited machine assembly of the cut bodies, and a reviewer that does not know
            // that will file a finding for every section the author has not filled in yet.
            Some(block) if brief.authored == Some(false) => out.push_str(&format!(
                " — NOT AUTHORED: nobody has opened this render, so this is the block koblem \
                 composes from its cuts. It is what renders until somebody writes one.\n{block}\n\n"
            )),
            Some(block) => out.push_str(&format!("\n{block}\n\n")),
            // 🔴 Never print an empty block. A generation nobody has opened has no prompt at all,
            // and rendering it as an empty one would read as an author who wrote nothing — which is
            // a finding the reviewer would then raise against every unwritten render in the script.
            None if cuts.is_empty() => out.push_str(
                " — NOT WRITTEN YET, and no cut in it carries any text: nothing has been authored \
                 for this render.\n\n",
            ),
            None => {
                out.push_str(
                    " — NOT WRITTEN YET: this render has no prompt block. What follows is what its \
                     cuts' authors typed, which is all there is:\n",
                );
                for (j, i) in cuts.iter().enumerate() {
                    out.push_str(&cut_line(Some(j + 1), *i));
                }
                out.push('\n');
            }
        }
    }

    let unplaced: Vec<usize> = (0..shots.len()).filter(|i| !placed.contains(i)).collect();
    if !unplaced.is_empty() {
        out.push_str(
            "Cuts the packing above does not place — they are in the timeline, but which render \
             they land in is not known here:\n",
        );
        for i in unplaced {
            out.push_str(&cut_line(None, i));
        }
        out.push('\n');
    }
    out
}

/// The shared "echo the id" contract line. Every per-shot endpoint says this the same way, because
/// the id discipline is the whole reason those endpoints stopped landing edits on the wrong shot.
fn echo_ids_rule(tokens: &ShotTokens) -> String {
    format!(
        " Name every shot by the exact \"id\" given in its line — copy it character for character, \
         never renumber, invent, abbreviate or reorder ids. The only valid ids are: {}. A result \
         whose id is not in that list is discarded.",
        tokens.joined()
    )
}

/// Parse a `{ "<key>": [ … ] }` LLM reply into `Vec<T>`, recovering the complete items from a
/// truncated array when the full parse fails (see [`ltx_prompting::salvage_array_elements`]). A
/// truncated batch keeps its whole prefix instead of erroring the request.
fn parse_or_salvage<T: serde::de::DeserializeOwned>(text: &str, whole: &str) -> Vec<T> {
    // `text` is the extracted `{…}`; `whole` is the raw reply we scan when the object is truncated.
    #[derive(Deserialize)]
    struct OneArray<U> {
        #[serde(alias = "shots", alias = "findings", alias = "gists", alias = "items")]
        items: Vec<U>,
    }
    if let Ok(w) = serde_json::from_str::<OneArray<T>>(text) {
        return w.items;
    }
    crate::routes::ltx_prompting::salvage_array_elements(whole)
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

#[derive(Debug, Deserialize)]
struct ReworkRequest {
    #[serde(default)]
    shots: Vec<ShotIn>,
    #[serde(default)]
    instruction: String,
    #[serde(default)]
    story: Option<StoryContext>,
    /// The project's recurring cast, with the appearance sentence each character must carry
    /// verbatim into every shot they appear in. Empty ⇒ byte-identical to before.
    #[serde(default)]
    cast: Vec<LtxCastMember>,
    /// `"ltx"` (default) or `"h3"` — which engine's prompt conventions to rewrite INTO. A rework is
    /// where an engine mismatch does the most damage: it rewrites every prompt in the project at
    /// once, so getting this wrong converts a whole script to the other engine's conventions in one
    /// call.
    #[serde(default)]
    engine: Option<String>,
    /// H3 only: the project's omni references, in order, so a rewrite can keep addressing them.
    #[serde(default)]
    h3_references: Vec<crate::routes::h3_prompting::H3RefBrief>,
    /// H3 only: where the song's voice falls, one lane per GENERATION in render order, each in that
    /// generation's OWN LOCAL seconds. See `h3_prompting::H3VocalLane`.
    ///
    /// 🔴 A rework rewrites every block in the project in one call, so it is the single fastest way
    /// to put a mimed vocal back into a generation somebody had already corrected by hand. Absent ⇒
    /// the prompt is unchanged from what it was before the lane existed.
    #[serde(default)]
    h3_vocal_lanes: Vec<crate::routes::h3_prompting::H3VocalLane>,
}

#[derive(Debug, Default, Deserialize)]
struct ReworkedShot {
    /// The shot id this rewrite targets, echoed from the input. An id we can't map exactly is a
    /// rewrite we drop.
    #[serde(default)]
    id: Option<String>,
    /// A `Value`, not a `String`, because a reply may carry either shape: H3 now returns its
    /// finished schema block as a JSON string, and a model still answering in the retired
    /// structured-draft shape returns an object. Both are read — see [`rewritten_prompt_text`].
    #[serde(default)]
    prompt: Value,
    #[serde(default)]
    gist: Option<String>,
}

/// One rewritten prompt as text: a string passes through, an object is re-serialised compactly.
///
/// On H3 the string arm is now the normal one and carries the finished schema block; the object arm
/// is the retired structured draft, which [`rewritten_prompt_draft`] also returns typed.
fn rewritten_prompt_text(v: &Value) -> String {
    crate::routes::h3_prompting::generation_text(v).unwrap_or_default()
}

/// The same rewrite, TYPED — so the structured draft crosses the wire as an object rather than as
/// JSON the client has to re-parse out of a string.
///
/// `None` for an LTX rewrite (a prose paragraph is a `Value::String`, not an object) and — the
/// normal case since the H3 writer started returning finished blocks — for any H3 rewrite, where
/// [`rewritten_prompt_text`] carries the block itself. Kept because a model still answering in the
/// retired object shape must keep working through a rollout.
fn rewritten_prompt_draft(v: &Value) -> Option<koblem_client::types::H3PromptDraft> {
    if !v.is_object() {
        return None;
    }
    serde_json::from_value::<koblem_client::types::H3PromptDraft>(v.clone())
        .ok()
        .filter(|d| !d.is_empty())
}

#[derive(Debug, Serialize)]
struct ReworkResponseShot {
    /// The clip id this rewrite applies to.
    id: String,
    /// The prompt: LTX's prose paragraph, or H3's finished schema block.
    prompt: String,
    /// **H3 only, and normally absent.** The retired structured draft, when a model answered in
    /// that shape instead of writing a block. Kept live for a rollout.
    #[serde(skip_serializing_if = "Option::is_none")]
    h3_draft: Option<koblem_client::types::H3PromptDraft>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gist: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReworkResponse {
    shots: Vec<ReworkResponseShot>,
    /// Rewrites dropped on the way back in (unknown / duplicate id). Surfaced rather than silently
    /// swallowed so the user knows a shot went untouched.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// `POST /api/v1/ltx-director/rework` — apply one director-level change across the WHOLE script,
/// rewriting every shot's prompt (+gist) in place while holding continuity. Shot count/order are
/// preserved; each rewrite comes back keyed to the clip `id` it belongs to.
async fn rework(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<ReworkRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let instruction = req.instruction.trim();
    if instruction.is_empty() {
        return err(StatusCode::BAD_REQUEST, "instruction is required");
    }
    let shots: Vec<&ShotIn> = req.shots.iter().filter(|s| !s.is_blank()).collect();
    if shots.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no shots to rework");
    }
    let tokens = ShotTokens::new(&shot_ids(&shots));

    let story = req.story.unwrap_or_default();
    let engine = request_engine(req.engine.as_deref());
    let h3 = engine == ENGINE_H3;
    // H3's checkpoint (and with it the body field name, the section set and the task-type prefix)
    // is DERIVED from the references, never chosen — see `h3_prompting::H3Inputs::resolve`.
    let h3_plan =
        crate::routes::h3_prompting::H3Inputs::from_refs(&req.h3_references, false, false)
            .resolve();
    // 🔴 The H3 arm is ONE call now. It used to be six `push_str`es here, which put the ORDER of
    // H3's prompt craft in a file that knows nothing about H3 — and the order became load-bearing
    // the moment MiniMax's own guide had to come first, before any fact that might qualify it.
    let mut system = if h3 {
        crate::routes::h3_prompting::rework_system(
            h3_plan.checkpoint,
            &req.h3_references,
            &req.cast,
            &req.h3_vocal_lanes,
        )
    } else {
        String::from(
            "You revise an existing multi-shot video script to apply a director's change while \
             keeping continuity. For EACH shot, rewrite its detailed LTX video prompt so the change \
             is reflected wherever it applies; shots the change doesn't touch should stay \
             essentially the same (adjust only for continuity). Keep the SAME number of shots in \
             the SAME order. Each prompt stays a single vivid cinematic paragraph.",
        )
    };
    if !h3 {
        system.push_str(&crate::routes::ltx_prompting::multi_shot_pacing());
    }
    let canon = crate::routes::ltx_prompting::story_context_block(&story);
    if !canon.is_empty() {
        system.push_str("\n\n");
        system.push_str(&canon);
    }
    // Identity is held by exact repetition of each character's appearance sentence, so a rewrite
    // that paraphrases it silently swaps the character out. Stated to every path that writes or
    // rewrites a prompt, including this one. H3 states it its own way — MiniMax's guide plus the
    // project's cast roster — and has already done so above.
    if !h3 {
        system.push_str(&crate::routes::ltx_prompting::cast_clause(&req.cast));
    }
    // The `prompt` value differs in SHAPE per engine: LTX rewrites a paragraph, H3 rewrites a whole
    // schema block.
    //
    // ⚠️ This endpoint is keyed by clip `id`, so its envelope has to stay JSON — which means an H3
    // block rides as a JSON STRING and its newlines have to be escaped. That is stated outright
    // rather than assumed: the single-generation and script paths avoid the problem entirely by
    // returning text with a sentinel (`h3_prompting::GENERATION_SEPARATOR`), and they avoid it
    // because escaping a multi-line block into JSON is what local models get wrong. If reworks come
    // back malformed, this is the first place to look, and the fix is a text protocol with the id
    // on its own line — not more emphasis here.
    let prompt_value = if h3 {
        "\"<the complete rewritten schema block as ONE JSON string — every newline written \\n, \
         nothing removed and no section dropped>\""
    } else {
        "\"<rewritten shot prompt>\""
    };
    system.push_str(&format!(
        "\n\nReply as JSON only: {{\"shots\": [{{\"id\": \"<the shot's id>\", \"prompt\": \
         {prompt_value}, \"gist\": \"<one plain-language sentence, no camera jargon>\"}}, \
         ...]}}. Include the \"id\" on EVERY entry and cover all {n} shots.{rule}",
        n = shots.len(),
        rule = echo_ids_rule(&tokens),
    ));

    let user = format!(
        "Change to apply across the script:\n{instruction}\n\nCurrent shots ({}):\n{}",
        shots.len(),
        number_shots_for(engine, &shots, Some(&tokens), true)
    );
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        // Rewrites one full prompt (+gist) per shot as JSON — must fit ALL shots or the JSON
        // truncates and the whole rework fails. ~340 tok/shot; ceiling sized for large scripts.
        "max_tokens": (shots.len() as u32 * 400).clamp(4096, 32000),
        "temperature": 0.6,
    });
    crate::sampling::apply_sampling(&state.db, "sampling_director", &mut body).await;

    let client = state.director_llm.as_ref().unwrap_or(&state.llama);
    // 🔴 An H3 entry is a WHOLE block, ~1,800 tokens at the cap — 400 a shot truncated the JSON
    // after the first couple of renders and the rest of the rework was lost.
    let per_shot = if h3 {
        crate::routes::h3_prompting::H3_BLOCK_ANSWER_TOKENS
    } else {
        400
    };
    crate::director_think::enable(&mut body, client, (shots.len() as u32 * per_shot).max(4096));
    let text = match client.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => {
            warn!(error = %e, "director rework: LLM call failed");
            return err(
                StatusCode::BAD_GATEWAY,
                "rework failed — the LLM is unavailable",
            );
        }
    };
    // Recover complete shots even if the JSON was truncated (a long rework can outrun max_tokens).
    let reworked: Vec<ReworkedShot> = parse_or_salvage(
        crate::routes::ltx_prompting::extract_json_object(&text),
        &text,
    );
    if reworked.is_empty() {
        warn!(reply = %text, "director rework: no usable shots in reply");
        return err(StatusCode::BAD_GATEWAY, "rework returned malformed output");
    }

    // Each rewrite lands on the clip whose id it echoed, or on no clip at all. There is deliberately
    // no positional fallback: the position a rewrite arrived at is not evidence of which shot it is
    // for, and acting on that guess is how a rework lands on the wrong clip.
    let mut ids = IdResolver::new(&tokens);
    let out: Vec<ReworkResponseShot> = reworked
        .into_iter()
        .filter_map(|s| {
            let id = ids.resolve(s.id.as_deref(), "rework")?;
            let raw = rewritten_prompt_text(&s.prompt);
            // An H3 rewrite is already a JSON object; only the LTX paragraph wants de-fencing.
            let prompt = if raw.starts_with('{') {
                raw
            } else {
                crate::routes::ltx_prompting::clean_prompt(&raw)
            };
            if prompt.trim().is_empty() {
                return None;
            }
            let h3_draft = rewritten_prompt_draft(&s.prompt);
            let gist = s
                .gist
                .map(|g| g.trim().to_string())
                .filter(|g| !g.is_empty());
            Some(ReworkResponseShot {
                id,
                prompt,
                h3_draft,
                gist,
            })
        })
        .collect();
    if out.is_empty() {
        warn!(warnings = ?ids.warnings, "director rework: every rewrite was dropped");
        return err(StatusCode::BAD_GATEWAY, "rework produced no usable shots");
    }

    Json(ReworkResponse {
        shots: out,
        warnings: ids.warnings,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct ReviewRequest {
    #[serde(default)]
    shots: Vec<ShotIn>,
    #[serde(default)]
    story: Option<StoryContext>,
    /// `"ltx"` (default) or `"h3"`. A continuity review means something different on each: LTX's
    /// reviewer looks for a shot that CONTRADICTS the frame it renders out of, H3's for a shot that
    /// DEPENDS on one that will not be there.
    #[serde(default)]
    engine: Option<String>,
    /// H3 only: the timeline packed into GENERATIONS, each carrying the assembled block that will
    /// actually be sent for it.
    ///
    /// 🔴 **Without this the review is structurally unable to do most of its job.** `shots` is one
    /// line per clip; the reviewer's schema checks (cut times, reference labels, retention lines,
    /// the body field) all live inside the block, which is the stored document. Empty ⇒ the reviewer
    /// is told it is seeing cut bodies only and is asked for nothing it cannot see — byte-identical
    /// to the pre-2026-08-08 behaviour for a client that does not send it.
    ///
    /// ⚠️ Ignored on LTX, where a shot IS a render and `shots` already carries its whole prompt.
    #[serde(default)]
    h3_generations: Vec<crate::routes::h3_prompting::H3GenerationBrief>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFinding {
    /// The shot id the issue is about, echoed from the input; explicit `null`/absent = a
    /// whole-story note.
    #[serde(default)]
    shot: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    issue: String,
}

#[derive(Debug, Deserialize)]
struct ReviewReply {
    #[serde(default)]
    findings: Vec<RawFinding>,
}

#[derive(Debug, Serialize)]
struct ReviewFinding {
    /// The clip id the issue applies to, or `null` for a story-wide note.
    #[serde(skip_serializing_if = "Option::is_none")]
    shot: Option<String>,
    severity: String,
    issue: String,
}

#[derive(Debug, Serialize)]
struct ReviewResponse {
    findings: Vec<ReviewFinding>,
    /// Findings dropped on the way back in (unknown / duplicate id).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// `POST /api/v1/ltx-director/review` — a script-supervisor pass over the shots: flags continuity
/// breaks, weak/vague shots and pacing problems as concrete, actionable findings tied to a shot
/// (or the whole story). The client turns each shot-scoped finding into a one-click rework.
///
/// Two input shapes, one id space:
///
/// | engine | what the reviewer is shown | what it may be asked for |
/// |---|---|---|
/// | LTX | one line per shot — a shot IS a render, and its line carries the whole prompt | continuity, vagueness, pacing |
/// | H3 + `h3_generations` | one entry per RENDER, with its assembled block verbatim | the above **plus** every schema check, because the schema is on screen |
/// | H3, no `h3_generations` | one line per cut | continuity, vagueness, pacing — and it is told the blocks are absent |
///
/// 🔴 In all three the model names a **clip id** or `null`. The H3 listing is per generation but its
/// ids are the cut ids inside each one, because `ReviewItem.clipId` drives a per-clip rework and a
/// finding with no clip has nothing to fix.
async fn review(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<ReviewRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let shots: Vec<&ShotIn> = req.shots.iter().filter(|s| !s.is_blank()).collect();
    if shots.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no shots to review");
    }
    let n = shots.len();
    let tokens = ShotTokens::new(&shot_ids(&shots));

    let story = req.story.unwrap_or_default();
    let engine = request_engine(req.engine.as_deref());
    // The packed generations are only meaningful on H3, and only when the client sent them. This one
    // flag decides BOTH halves of the request: what the user turn shows, and — through
    // `review_system` — which findings the reviewer is asked for at all. They must not drift apart,
    // which is why it is computed once here rather than tested twice.
    let gens: &[crate::routes::h3_prompting::H3GenerationBrief] = if engine == ENGINE_H3 {
        &req.h3_generations
    } else {
        &[]
    };
    let mut system = if engine == ENGINE_H3 {
        crate::routes::h3_prompting::review_system(!gens.is_empty())
    } else {
        String::from(
            "You are a script supervisor reviewing a multi-shot video before it is rendered. Find \
             concrete problems: continuity errors (a character, object, wardrobe, time-of-day or \
             setting that contradicts an earlier shot), shots that are vague or weak, and pacing or \
             story-clarity issues. Only report real, actionable problems — if the script is clean, \
             return an empty list. Do not invent issues to fill space.",
        )
    };
    let canon = crate::routes::ltx_prompting::story_context_block(&story);
    if !canon.is_empty() {
        system.push_str("\n\n");
        system.push_str(&canon);
    }
    system.push_str(&format!(
        "\n\nReply as JSON only: {{\"findings\": [{{\"shot\": \"<the id of the shot this applies to, \
         or null for a whole-story note>\", \"severity\": \"high\"|\"medium\"|\"low\", \"issue\": \
         \"<one concrete, actionable sentence>\"}}, ...]}}. Empty findings list if nothing is \
         wrong.{rule} Use null — never a guess — when an issue is about the story as a whole rather \
         than one shot.",
        rule = echo_ids_rule(&tokens),
    ));
    if !gens.is_empty() {
        // 🔴 The id space did not change and must not: the listing is per RENDER but the ids in it
        // are the CUT ids, because a finding is turned into a one-click rework of one clip. Say so
        // explicitly — a model shown `Shot 1 — 3 cuts` will otherwise answer `"shot": "1"`, which
        // maps to nothing and is dropped, losing the finding in silence.
        system.push_str(
            " On this engine the only ids that exist are the CUT ids listed in each shot's header: \
             name the cut the issue is about, never the shot it sits in and never an ordinal. If an \
             issue is about a whole shot rather than one cut, name the FIRST cut in that shot; if it \
             is about the story as a whole, use null.",
        );
    }

    let user = if gens.is_empty() {
        format!(
            "Shots ({n}):\n{}",
            number_shots_for(engine, &shots, Some(&tokens), true)
        )
    } else {
        format!(
            "This script is {g} shot(s) — {n} cut(s) in total. Each entry is one render, followed \
             by the prompt block that will be sent for it.\n\n{}",
            // 🔴 Cut ids, always. `ReviewItem.clipId` drives the one-click rework — see
            // `GenListingIds`.
            number_generations(gens, &shots, &tokens, GenListingIds::Cuts),
            g = gens.len(),
        )
    };
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        // Findings scale with shot count — a fixed cap truncated the JSON on long scripts.
        "max_tokens": (n as u32 * 160).clamp(2048, 16000),
        "temperature": 0.4,
    });
    crate::sampling::apply_sampling(&state.db, "sampling_director", &mut body).await;

    let client = state.director_llm.as_ref().unwrap_or(&state.llama);
    crate::director_think::enable(&mut body, client, (n as u32 * 160).clamp(2048, 16000));
    let text = match client.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => {
            warn!(error = %e, "director review: LLM call failed");
            return err(
                StatusCode::BAD_GATEWAY,
                "review failed — the LLM is unavailable",
            );
        }
    };
    // A valid empty list means "clean script", so distinguish that from a parse failure: full parse
    // first (empty is fine), and only salvage complete findings when the JSON itself was truncated.
    let raw_findings: Vec<RawFinding> = match serde_json::from_str::<ReviewReply>(
        crate::routes::ltx_prompting::extract_json_object(&text),
    ) {
        Ok(r) => r.findings,
        Err(_) => {
            let salvaged: Vec<RawFinding> =
                crate::routes::ltx_prompting::salvage_array_elements(&text)
                    .into_iter()
                    .filter_map(|v| serde_json::from_value(v).ok())
                    .collect();
            if salvaged.is_empty() {
                warn!(reply = %text, "director review: unparseable JSON");
                return err(StatusCode::BAD_GATEWAY, "review returned malformed output");
            }
            salvaged
        }
    };

    // An absent/null `shot` is a real answer — the whole-story note. An id we can't map is NOT:
    // pinning a finding on a guessed shot sends the user to rework the wrong clip, so the finding
    // is dropped instead of being demoted to a story-wide note it was never meant to be.
    let mut ids = IdResolver::new(&tokens);
    let findings: Vec<ReviewFinding> = raw_findings
        .into_iter()
        .filter_map(|f| {
            let issue = f.issue.trim().to_string();
            if issue.is_empty() {
                return None;
            }
            let named = f.shot.as_deref().map(str::trim).filter(|s| {
                !s.is_empty() && !s.eq_ignore_ascii_case("null") && !s.eq_ignore_ascii_case("none")
            });
            let shot = match named {
                Some(raw) => Some(ids.lookup(Some(raw))?.1),
                None => None,
            };
            let severity = match f.severity.as_deref().map(str::to_lowercase).as_deref() {
                Some("high") => "high",
                Some("low") => "low",
                _ => "medium",
            }
            .to_string();
            Some(ReviewFinding {
                shot,
                severity,
                issue,
            })
        })
        .collect();

    Json(ReviewResponse {
        findings,
        warnings: ids.warnings,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct GistShotsRequest {
    #[serde(default)]
    shots: Vec<ShotIn>,
    #[serde(default)]
    story: Option<StoryContext>,
    /// `"ltx"` (default) or `"h3"` — chooses which listing [`number_shots_for`] prints.
    ///
    /// 🔴 Absent, an H3 script is listed with LTX's tags, and the one that hurts is `carried`: it
    /// tells the reader the shot continues out of the previous shot's final frame. On H3 that is
    /// never true across a generation boundary and is not what the flag means inside one, so the
    /// gist comes back describing a continuation the render does not perform.
    #[serde(default)]
    engine: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawGist {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    gist: String,
}

#[derive(Debug, Serialize)]
struct GistItem {
    /// The clip id this gist belongs to.
    id: String,
    gist: String,
}

#[derive(Debug, Serialize)]
struct GistShotsResponse {
    gists: Vec<GistItem>,
    /// Gists dropped on the way back in (unknown / duplicate id).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// Map the model's gist entries onto clips by the id each entry echoed. An entry with no usable id
/// is dropped: its array position tells us nothing about which shot it describes, and a gist on the
/// wrong shot silently mislabels the timeline.
fn map_gists(raw: Vec<RawGist>, ids: &mut IdResolver) -> Vec<GistItem> {
    raw.into_iter()
        .filter_map(|g| {
            let gist = g.gist.trim().to_string();
            if gist.is_empty() {
                return None;
            }
            let id = ids.resolve(g.id.as_deref(), "gist")?;
            Some(GistItem { id, gist })
        })
        .collect()
}

/// `POST /api/v1/ltx-director/gist-shots` — batch-write a one-sentence plain-language gist for many
/// shots in a single LLM call (backfilling scannable summaries for jargon-heavy / auto-directed
/// shots). Gists come back aligned to the input order via `index`.
async fn gist_shots(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<GistShotsRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let shots: Vec<&ShotIn> = req.shots.iter().filter(|s| !s.is_blank()).collect();
    if shots.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no shots to summarise");
    }
    let n = shots.len();
    let tokens = ShotTokens::new(&shot_ids(&shots));

    let story = req.story.unwrap_or_default();
    let mut system = String::from(
        "You summarise film shots for a director. For EACH shot, write ONE short plain-language \
         sentence describing what happens — the action and subject a viewer would notice, with NO \
         camera moves, lens, lighting or technical jargon.",
    );
    let canon = crate::routes::ltx_prompting::story_context_block(&story);
    if !canon.is_empty() {
        system.push_str("\n\n");
        system.push_str(&canon);
    }
    system.push_str(&format!(
        "\n\nReply as JSON only: {{\"gists\": [{{\"id\": \"<the shot's id>\", \"gist\": \
         \"<one plain-language sentence>\"}}, ...]}}. Put the \"id\" on EVERY entry so each summary \
         is tied to the right shot; cover all {n} shots.{rule}",
        rule = echo_ids_rule(&tokens),
    ));

    let user = format!(
        "Shots ({n}):\n{}",
        number_shots_for(
            request_engine(req.engine.as_deref()),
            &shots,
            Some(&tokens),
            true,
        )
    );
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "max_tokens": (n as u32 * 120).clamp(1024, 12000),
        "temperature": 0.3,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_director", &mut body).await;

    let client = state.director_llm.as_ref().unwrap_or(&state.llama);
    let text = match client.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => {
            warn!(error = %e, "director gist-shots: LLM call failed");
            return err(
                StatusCode::BAD_GATEWAY,
                "gist failed — the LLM is unavailable",
            );
        }
    };
    // Recovers a truncated array (a long batch can outrun max_tokens) as complete {id,gist} entries.
    let raw: Vec<RawGist> = parse_or_salvage(
        crate::routes::ltx_prompting::extract_json_object(&text),
        &text,
    );
    if raw.is_empty() {
        warn!(reply = %text, "director gist-shots: no usable gists in reply");
        return err(StatusCode::BAD_GATEWAY, "gist returned malformed output");
    }

    let mut ids = IdResolver::new(&tokens);
    let gists = map_gists(raw, &mut ids);
    if gists.is_empty() {
        warn!(warnings = ?ids.warnings, "director gist-shots: every gist was dropped");
        return err(StatusCode::BAD_GATEWAY, "gist produced no summaries");
    }

    Json(GistShotsResponse {
        gists,
        warnings: ids.warnings,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct ExtendRequest {
    #[serde(default)]
    shots: Vec<ShotIn>,
    /// How many new shots to append (clamped 1..=12).
    #[serde(default)]
    count: Option<u32>,
    /// Optional steer for where the story should go next ("wrap it up", "a twist", …).
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    story: Option<StoryContext>,
    /// The project's recurring cast, with the appearance sentence each character must carry
    /// verbatim into every shot they appear in. Empty ⇒ byte-identical to before.
    #[serde(default)]
    cast: Vec<LtxCastMember>,
    /// `"ltx"` (default) or `"h3"` — which engine's prompt conventions the new shots are written in.
    #[serde(default)]
    engine: Option<String>,
    /// H3 only: the project's omni references, in order, so appended shots can address them.
    #[serde(default)]
    h3_references: Vec<crate::routes::h3_prompting::H3RefBrief>,
}

#[derive(Debug, Default, Deserialize)]
struct ExtendShot {
    #[serde(default)]
    idea: Option<String>,
    /// A `Value` for the same reason as [`ReworkedShot::prompt`]: LTX returns a paragraph, H3 a
    /// schema block as a string — or, from a model answering in the retired shape, an object.
    #[serde(default)]
    prompt: Value,
    #[serde(default)]
    gist: Option<String>,
    #[serde(default)]
    seconds: Option<f64>,
    #[serde(default)]
    new_scene: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ExtendResponseShot {
    #[serde(skip_serializing_if = "Option::is_none")]
    idea: Option<String>,
    /// The prompt: LTX's prose paragraph, or H3's finished schema block.
    prompt: String,
    /// **H3 only, and normally absent.** The retired structured draft, when a model answered in
    /// that shape instead of writing a block. Kept live for a rollout.
    #[serde(skip_serializing_if = "Option::is_none")]
    h3_draft: Option<koblem_client::types::H3PromptDraft>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seconds: Option<f64>,
    /// Hard cut: this new shot starts a fresh scene rather than continuing the previous shot's
    /// final frame. The client compiles it to the segment's `scene_cut`.
    #[serde(skip_serializing_if = "Option::is_none")]
    new_scene: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ExtendResponse {
    shots: Vec<ExtendResponseShot>,
}

/// `POST /api/v1/ltx-director/extend` — continue the story: write N NEW shots that pick up from
/// where the current timeline ends, consistent with the shots so far and the project canon.
async fn extend(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<ExtendRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let existing: Vec<&ShotIn> = req.shots.iter().filter(|s| !s.is_blank()).collect();
    if existing.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "need at least one existing shot to continue from",
        );
    }
    let count = req.count.unwrap_or(3).clamp(1, 12) as usize;
    let direction = req
        .direction
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let story = req.story.unwrap_or_default();
    let engine = request_engine(req.engine.as_deref());
    let h3 = engine == ENGINE_H3;
    // 🔴 The checkpoint is DERIVED from the references and decides WHICH GUIDE the writer is handed
    // (`h3_prompting::H3Inputs::resolve`), so it has to be resolved before the system prompt exists
    // — not appended to one, as it used to be.
    let h3_plan =
        crate::routes::h3_prompting::H3Inputs::from_refs(&req.h3_references, false, false)
            .resolve();
    let mut system = if h3 {
        crate::routes::h3_prompting::extend_system(
            h3_plan.checkpoint,
            &req.h3_references,
            &req.cast,
        )
    } else {
        String::from(
            "You are a director continuing an existing multi-shot video. Write the NEXT shots that \
             pick up exactly where the current timeline ends — same characters, world, tone and \
             continuity. Each shot is a single vivid cinematic LTX video prompt (subject, action, \
             camera, setting).",
        )
    };
    if h3 {
        use crate::routes::h3_prompting as h3p;
        // 🔴 The old text here forced `"new_scene":true` on every shot, from the era when each shot
        // was its own render. A cut INSIDE a generation is the cheap, default move now; `new_scene`
        // means "this shot starts a new generation", which is the expensive one.
        system.push_str(&format!(
            " Each new entry is a GENERATION of roughly {min:.0}-{max:.0} seconds that may hold \
             several shots: give it a \"seconds\" sized to the beats it plays, and set \
             \"new_scene\":true, because an appended generation is by definition a new render with \
             its own soundtrack. Put a sequence of cuts INSIDE one entry as several `[Shot N]` \
             blocks rather than spending an entry on each cut.",
            min = h3p::MIN_GENERATION_SECONDS,
            max = h3p::MAX_GENERATION_SECONDS,
        ));
    } else {
        system.push_str(&crate::routes::ltx_prompting::multi_shot_pacing());
        system.push_str(
            " Each NEW shot chooses its own length: give it a \"seconds\" of roughly 2-10s sized to \
             the beat it plays, and pace its prompt to that. Set \"new_scene\":true on a new shot \
             that HARD CUTS to a different place, time or subject, and false when it should flow \
             straight out of the previous shot's final frame — that flag is what decides whether \
             the shot before it ends on a continuation point or closes its scene, so apply the \
             CONTINUES/CLOSES rule above to each new shot using its OWN successor's flag. The last \
             new shot closes.",
        );
    }
    let canon = crate::routes::ltx_prompting::story_context_block(&story);
    if !canon.is_empty() {
        system.push_str("\n\n");
        system.push_str(&canon);
    }
    // Identity is held by exact repetition of each character's appearance sentence, so a rewrite
    // that paraphrases it silently swaps the character out. Stated to every path that writes or
    // rewrites a prompt, including this one. The H3 arm has already said it its own way above.
    if !h3 {
        system.push_str(&crate::routes::ltx_prompting::cast_clause(&req.cast));
    }
    // As in `rework`, the `prompt` value's SHAPE is engine-specific: a paragraph on LTX, a whole
    // schema block on H3 — and, as there, the id-keyed envelope forces the block through JSON
    // string escaping. See `rework` for why that is stated rather than assumed.
    let prompt_value = if h3 {
        "\"<the new generation's complete schema block as ONE JSON string — every newline written \
         \\n, with all of its sections>\""
    } else {
        "\"<full shot prompt>\""
    };
    system.push_str(&format!(
        "\n\nReply as JSON only: {{\"shots\": [{{\"idea\": \"<short one-liner>\", \"prompt\": \
         {prompt_value}, \"gist\": \"<one plain-language sentence, no jargon>\", \"seconds\": <number>, \
         \"new_scene\": <true|false>}}, ...]}} with exactly {count} NEW shots."
    ));

    // The existing shots are read-only context, so they carry no ids to echo. Their last shot is
    // tagged CONTINUES, not CLOSES: extending the timeline is precisely what continues out of it.
    let mut user = format!(
        "The video so far ({} shots):\n{}\n\nWrite the next {count} shots that continue from here.",
        existing.len(),
        number_shots_for(engine, &existing, None, false)
    );
    if let Some(d) = direction {
        user.push_str(&format!("\n\nWhere it should go next: {d}"));
    }
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "max_tokens": (count as u32 * 400).clamp(2048, 8192),
        "temperature": 0.8,
    });
    crate::sampling::apply_sampling(&state.db, "sampling_director", &mut body).await;

    let client = state.director_llm.as_ref().unwrap_or(&state.llama);
    let per_shot = if h3 {
        crate::routes::h3_prompting::H3_BLOCK_ANSWER_TOKENS
    } else {
        400
    };
    crate::director_think::enable(&mut body, client, (count as u32 * per_shot).max(2048));
    let text = match client.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => {
            warn!(error = %e, "director extend: LLM call failed");
            return err(
                StatusCode::BAD_GATEWAY,
                "extend failed — the LLM is unavailable",
            );
        }
    };
    let extended: Vec<ExtendShot> = parse_or_salvage(
        crate::routes::ltx_prompting::extract_json_object(&text),
        &text,
    );
    if extended.is_empty() {
        warn!(reply = %text, "director extend: no usable shots in reply");
        return err(StatusCode::BAD_GATEWAY, "extend returned malformed output");
    }

    let shots: Vec<ExtendResponseShot> = extended
        .into_iter()
        .filter_map(|s| {
            let raw = rewritten_prompt_text(&s.prompt);
            // An H3 generation is already a JSON object; only the LTX paragraph wants de-fencing.
            let prompt = if raw.starts_with('{') {
                raw
            } else {
                crate::routes::ltx_prompting::clean_prompt(&raw)
            };
            if prompt.trim().is_empty() {
                return None;
            }
            let h3_draft = rewritten_prompt_draft(&s.prompt);
            Some(ExtendResponseShot {
                idea: s
                    .idea
                    .map(|i| i.trim().to_string())
                    .filter(|i| !i.is_empty()),
                prompt,
                h3_draft,
                gist: s
                    .gist
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty()),
                seconds: s.seconds,
                new_scene: s.new_scene,
            })
        })
        .take(count)
        .collect();
    if shots.is_empty() {
        return err(StatusCode::BAD_GATEWAY, "extend produced no usable shots");
    }

    Json(ExtendResponse { shots }).into_response()
}

#[derive(Debug, Deserialize)]
struct TitleRequest {
    #[serde(default)]
    shots: Vec<ShotIn>,
    #[serde(default)]
    brief: Option<String>,
    #[serde(default)]
    story: Option<StoryContext>,
    /// `"ltx"` (default) or `"h3"` — chooses which listing [`number_shots_for`] prints. Same reason
    /// as [`GistShotsRequest::engine`]; the cost is only ever a slightly worse title, but the tags
    /// are noise the reader has to see past either way.
    #[serde(default)]
    engine: Option<String>,
}

#[derive(Debug, Serialize)]
struct TitleResponse {
    title: String,
}

/// `POST /api/v1/ltx-director/title` — name the project: a short, catchy 2–6 word title from the
/// brief + shots. Used to auto-title untitled projects.
async fn title(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<TitleRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let shots: Vec<&ShotIn> = req.shots.iter().filter(|s| !s.is_blank()).collect();
    let brief = req.brief.as_deref().map(str::trim).unwrap_or("");
    if shots.is_empty() && brief.is_empty() {
        return err(StatusCode::BAD_REQUEST, "nothing to title");
    }

    let story = req.story.unwrap_or_default();
    let mut system = String::from(
        "You name short films. Reply with ONE short, catchy, evocative title of 2–6 words — no \
         quotes, no punctuation at the end, no explanation, just the title.",
    );
    let canon = crate::routes::ltx_prompting::story_context_block(&story);
    if !canon.is_empty() {
        system.push_str("\n\n");
        system.push_str(&canon);
    }

    let mut user = String::new();
    if !brief.is_empty() {
        user.push_str(&format!("Brief: {brief}\n\n"));
    }
    if !shots.is_empty() {
        user.push_str(&format!(
            "Shots:\n{}",
            number_shots_for(request_engine(req.engine.as_deref()), &shots, None, true)
        ));
    }
    user.push_str("\nGive this a title.");

    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "max_tokens": 24,
        "temperature": 0.8,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_director", &mut body).await;

    let client = state.director_llm.as_ref().unwrap_or(&state.llama);
    let title = match client.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .trim_matches(['"', '\'', '.', '“', '”'])
            .trim()
            .to_string(),
        Err(e) => {
            warn!(error = %e, "director title: LLM call failed");
            return err(
                StatusCode::BAD_GATEWAY,
                "title failed — the LLM is unavailable",
            );
        }
    };
    if title.is_empty() {
        return err(StatusCode::BAD_GATEWAY, "title returned no text");
    }
    // Guard against a chatty model — keep it to a sane title length.
    let title = title.chars().take(80).collect::<String>();

    Json(TitleResponse { title }).into_response()
}

#[derive(Debug, Deserialize)]
struct ChatTurn {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    #[serde(default)]
    messages: Vec<ChatTurn>,
    #[serde(default)]
    shots: Vec<ShotIn>,
    #[serde(default)]
    story: Option<StoryContext>,
    /// Reference images (bare/data-URI base64) — character/mood canon for the VL model to see.
    #[serde(default)]
    reference_images: Vec<String>,
    /// `"ltx"` (default) or `"h3"` — a chat turn can rewrite a prompt, so it needs the same
    /// conventions the batch writers get.
    #[serde(default)]
    engine: Option<String>,
    /// H3 only: the project's omni references, in order, so a chat rewrite keeps addressing them.
    #[serde(default)]
    h3_references: Vec<crate::routes::h3_prompting::H3RefBrief>,
    /// The project's recurring cast, with the appearance sentence each character carries verbatim.
    ///
    /// 🔴 Chat was the LAST rewriting surface with no cast — `/rework`, `/script-from-llm` and the
    /// single-shot writer have all had it. Identity on both engines is held by exact repetition of
    /// that sentence, so a chat turn asked to "make Mia's shot colder" was rewriting her appearance
    /// from memory and swapping the character out with nothing to show for it. Empty ⇒ every byte of
    /// the prompt is what it was, so an older client is unaffected.
    #[serde(default)]
    cast: Vec<LtxCastMember>,
    /// H3 only: the packed generations with their assembled blocks — the same briefs `/review` gets.
    ///
    /// 🔴 **Without this a chat turn edits a shot it cannot see.** On H3 the render unit is the
    /// GENERATION and the artifact is its block ([`crate::routes::h3_prompting::H3GenerationBrief`]);
    /// a chat listing of one line per cut shows neither, so the model rewrote a body with no idea of
    /// the render's duration, its other shots, its cut times, its soundscape or its score — and
    /// answered questions about the video from the cut bodies alone, which is most of what the
    /// author reads out of a chat turn.
    ///
    /// Absent ⇒ the per-cut listing, exactly as before. The listing SHAPE is what this switches, so
    /// it is read as a presence test and never merged with the cut lines: two descriptions of the
    /// same shot in one prompt is how a model ends up answering about the one nobody meant.
    ///
    /// 🔴 **Send each brief's `id` and chat also EDITS the block instead of only reading it.** With
    /// ids present the whole turn moves to the generation id space
    /// ([`ChatIdSpace::Generation`]) and every action means the generation-level thing; without
    /// them the blocks are context and every action still names a cut, which is where the edits
    /// used to evaporate.
    #[serde(default)]
    h3_generations: Vec<crate::routes::h3_prompting::H3GenerationBrief>,
}

#[derive(Debug, Serialize)]
struct ChatResponseOut {
    reply: String,
    /// Edit actions the client applies to the timeline. Every `shot`/`after` has been resolved from
    /// the id the model echoed to an id the client keys on — which KIND of id is [`Self::target`];
    /// unmappable actions never get here.
    actions: Value,
    /// Actions dropped on the way back in (unknown / duplicate / wrong-space id).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    /// What every `shot`/`after` above is an id OF.
    ///
    /// 🔴 `"generation"` ⇒ `DirectorH3Generation.id`, and the actions mean the generation-level
    /// thing: `set_prompt` is that render's whole block, `set_gist` its display label, `set_seconds`
    /// its total length, `add_shot`/`delete_shot` a whole render. **Absent ⇒ `DirectorClip.id`**,
    /// which is every LTX turn and every H3 turn from a client that sent no generation ids — so an
    /// older client sees a byte-identical response and keeps working.
    ///
    /// ⚠️ A client that does not understand `"generation"` must refuse the batch, not apply it. The
    /// ids will not be found in its clip list, so applying is *currently* a silent no-op rather than
    /// a wrong write — but that is an accident of two id spaces not colliding, not a guarantee.
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'static str>,
}

/// Everything before the action list: who the assistant is, the id contract, and the reply envelope.
/// Identical on both engines — what a shot IS differs, what a well-formed reply looks like does not.
const CHAT_SPEC_HEAD: &str = r#"You are the director's assistant for a multi-shot AI video editor. You can DISCUSS the video and also CHANGE it. Every shot has a short "id" — that id, copied exactly, is the ONLY way to name a shot in an action.

Reply with EXACTLY ONE JSON object and nothing else — no markdown, no code fences, no text before or after it:
{"reply": "<your entire message to the user>", "actions": [ ...zero or more edit actions... ]}

CRITICAL: your whole message to the user lives inside the "reply" string — sentences, lists, multi-line answers, all of it. This is a JSON string, so write newlines as \n (a real line break would make the JSON invalid). Do NOT put any of your answer outside the JSON object.

Each action is exactly one of:
"#;

/// The LTX action list — and the H3 list for a client that sends no generation ids, where a chat id
/// is still a cut id and `set_prompt` still writes `DirectorClip.prompt`.
const CHAT_SPEC_ACTIONS_CLIP: &str = r#"{"type":"set_prompt","shot":"<id>","value":"<new full cinematic shot prompt>"}
{"type":"set_gist","shot":"<id>","value":"<one plain-language sentence, no camera jargon>"}
{"type":"set_idea","shot":"<id>","value":"<short one-liner>"}
{"type":"set_seconds","shot":"<id>","value":<number>}
{"type":"add_shot","after":"<id>"|"@start","idea":"<one-liner>","prompt":"<full prompt>"}
{"type":"delete_shot","shot":"<id>"}
{"type":"set_story","field":"backstory"|"tone"|"rules","value":"<text>"}
"#;

/// The shared examples and rules that follow the action list. Every sentence in it is true of both
/// id spaces — it talks about ids, ordinals and when to emit actions at all, never about what a
/// shot's prompt contains.
const CHAT_SPEC_TAIL: &str = r#"
Examples (the ids below are illustrative — only ever use ids from the shot list) —
A question that needs a list (note the list is INSIDE reply, with \n, and actions is empty):
{"reply": "Locations by shot:\n1. neon-lit back alley\n2. rain-slicked rooftop\n3. underground subway platform", "actions": []}
An edit request (short reply + the actions that make the change):
{"reply": "Done — shot 2 now takes place at night.", "actions": [{"type":"set_prompt","shot":"a3f9c1","value":"..."},{"type":"set_gist","shot":"a3f9c1","value":"..."}]}

Rules: include actions ONLY when the user asks for a change in their LATEST message; for questions, acknowledgements, or anything already handled return "actions": []. The shot list below is the CURRENT state and already reflects every edit made earlier in this conversation — NEVER re-emit an edit you already applied (doing so would double-apply it). Talk to the user in shot NUMBERS ("shot 2"), but address every action with the shot's "id". For add_shot, "after" is the id of the shot to insert after ("@start" = at the very start; omit "after" = append at the end). When you rewrite a prompt, also refresh its gist with a set_gist action."#;

/// The H3 action list for a client that DOES send generation ids: every id names a render.
///
/// 🔴 Two deliberate differences from [`CHAT_SPEC_ACTIONS_CLIP`], each of which is a destination
/// field and not a wording choice:
///
/// - `set_prompt`'s value is the whole block, because it lands on `DirectorH3Generation.prompt` —
///   the stored artifact. On the clip arm the same action lands on `DirectorClip.prompt`, one
///   `[Shot N]` body, which the block overwrites on the next block edit.
/// - **`set_idea` is gone.** `DirectorH3Generation` has no `idea` field and cannot grow a useful
///   one: an "idea" is a one-liner you write *before* the prompt exists, and here the prompt IS the
///   document — the one-line label for a render is its `gist`. Offering the action anyway would
///   produce edits with nowhere to land, which `validate_chat_actions` then drops with a warning
///   the user has to read to discover their request went nowhere.
const CHAT_SPEC_ACTIONS_GENERATION: &str = r#"{"type":"revise","shots":["<id>", ...] | "all","instruction":"<the change, written so it can be applied to one shot on its own>"}
{"type":"set_prompt","shot":"<id>","value":"<the shot's COMPLETE prompt block>"}
{"type":"set_gist","shot":"<id>","value":"<one plain-language sentence, no camera jargon>"}
{"type":"set_seconds","shot":"<id>","value":<number>}
{"type":"add_shot","after":"<id>"|"@start","idea":"<one-liner>","prompt":"<the new shot's COMPLETE prompt block>"}
{"type":"delete_shot","shot":"<id>"}
{"type":"set_story","field":"backstory"|"tone"|"rules","value":"<text>"}
"#;

/// What each action MEANS once its id names a render rather than a cut. Appended after
/// [`CHAT_SPEC_TAIL`], because it is a gloss on the vocabulary the tail has just finished
/// describing.
///
/// ⚠️ The 15.08 s ceiling is stated as a fact and NOT enforced here — `validate_chat_actions` checks
/// identity and nothing else, deliberately (see its doc). A longer `set_seconds` re-packs the shot
/// into two renders on the client, which is a visible outcome rather than a silent one, but it does
/// orphan the second half's block; the model is told so it does not propose it in the first place.
const CHAT_SPEC_GENERATION_MEANINGS: &str = r#"

🔴 ON THIS ENGINE ONE SHOT = ONE RENDER = ONE PROMPT BLOCK. That block is the shot's whole document: every section header, every `[Shot N] At MM:SS.mmm` mark inside it, the soundscape and the score. The `[Shot N]` marks INSIDE a block are moments within that single render — they are NOT shots you can name, they have no ids of their own, and changing one means rewriting the block that holds it. So:
- "revise" is how you change what a shot's block says. You give the INSTRUCTION, not the text: the editor hands it to the prompt writer once per shot you list, and each shot is rewritten in its own pass with the brief, the references and its neighbours in hand. Whenever a change applies to more than one render — "every shot", "all of them", a character or a continuity fix that runs through the video — list EVERY shot it applies to, or "all". Only say in your reply that you changed what your actions actually change.
- "set_prompt" REPLACES the entire block with text you write yourself. Use it only when the user dictates the exact block or a small literal edit to one shot; for anything else, use "revise". Repeat every section and every mark you are not changing, verbatim; anything you leave out is deleted from the render.
- "set_seconds" is how long the whole render is, in seconds. One render is at most 15.08 s — a shot that wants longer has to become two shots.
- "set_gist" is the one-line label the editor shows for the shot. It is a caption for a human and is never sent to the video model, so it is not a place to put schema text.
- "add_shot" creates a new render after the one you name, and its "prompt" is a complete block.
- "delete_shot" removes the whole render.
- There is no "set_idea" on this engine."#;

/// Which id space a chat turn's actions are in — i.e. what the client has to look the returned
/// `shot`/`after` up in.
///
/// 🔴 **This is the bug this type exists to close.** The action vocabulary is the same either way,
/// so nothing about an action's SHAPE says which kind of thing it names; the id does, and only the
/// server knows which listing it built. Sending the answer back with the actions
/// ([`ChatResponseOut::target`]) is what lets the client hard-fail on a space it does not handle
/// instead of resolving a generation id against its clip list, finding nothing, and dropping every
/// edit in silence.
#[derive(Debug, Clone, Copy)]
enum ChatIdSpace<'a> {
    /// `DirectorClip.id` — LTX always, and H3 for a client that sends no generation ids.
    Clip,
    /// `DirectorH3Generation.id` — H3 with generation ids. One id = one render = one block.
    ///
    /// ⚠️ Carries the CUT tokens it is displacing, and is the only way to construct this variant, so
    /// "addressing renders" and "knowing which ids are the wrong ones" cannot come apart. They are
    /// what [`IdResolver::with_foreign`] turns a cut id into a sentence naming the mistake instead
    /// of the generic "the model did not echo an id it was given".
    Generation { cuts: &'a ShotTokens },
}

impl ChatIdSpace<'_> {
    /// The value put on the wire, and `None` for [`Self::Clip`] so an LTX response is byte-identical
    /// to what it was before this field existed. Absent therefore means `Clip` to the client.
    fn wire(self) -> Option<&'static str> {
        match self {
            Self::Clip => None,
            Self::Generation { .. } => Some("generation"),
        }
    }
}

/// The action contract for one chat turn, assembled from the shared envelope + the id space's own
/// action list.
///
/// ⚠️ The three literals are the original single-literal spec, split at two points and not otherwise
/// touched, so [`ChatIdSpace::Clip`] reproduces the pre-2026-08-09 prompt byte for byte. That is the
/// LTX guarantee, and it is a property of the bytes rather than of a runtime check — if you edit
/// `CHAT_SPEC_HEAD`, `CHAT_SPEC_ACTIONS_CLIP` or `CHAT_SPEC_TAIL` you have changed LTX's prompt.
fn chat_actions_spec(space: ChatIdSpace<'_>) -> String {
    let mut s = String::with_capacity(4096);
    s.push_str(CHAT_SPEC_HEAD);
    s.push_str(match space {
        ChatIdSpace::Clip => CHAT_SPEC_ACTIONS_CLIP,
        ChatIdSpace::Generation { .. } => CHAT_SPEC_ACTIONS_GENERATION,
    });
    s.push_str(CHAT_SPEC_TAIL);
    if matches!(space, ChatIdSpace::Generation { .. }) {
        s.push_str(CHAT_SPEC_GENERATION_MEANINGS);
    }
    s
}

/// Rewrite each action's shot reference from the id the model echoed into the id the client keys on
/// — a `DirectorClip.id`, or a `DirectorH3Generation.id` under [`ChatIdSpace::Generation`] — and
/// drop any action whose id doesn't map EXACTLY. The client applies whatever comes back, so an
/// unresolved id must never reach it: a dropped edit is recoverable, an edit on the wrong shot is
/// not.
///
/// ⚠️ **Identity, and nothing else.** No value here is range-checked, length-checked or reshaped:
/// the rest of the action's shape is the client's, and koblem stays a thin proxy. Notably a
/// `set_seconds` past the 15.08 s one-render ceiling is passed through — the model is told the
/// ceiling in [`CHAT_SPEC_GENERATION_MEANINGS`] and the client re-packs, visibly, if it ignores it.
fn validate_chat_actions(
    raw: Vec<Value>,
    tokens: &ShotTokens,
    space: ChatIdSpace<'_>,
) -> (Vec<Value>, Vec<String>) {
    let mut ids = match space {
        ChatIdSpace::Clip => IdResolver::new(tokens),
        ChatIdSpace::Generation { cuts } => IdResolver::new(tokens).with_foreign(cuts),
    };
    let mut out: Vec<Value> = Vec::with_capacity(raw.len());
    for mut a in raw {
        let Some(kind) = a.get("type").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        // 🔴 `set_idea` has no generation-level meaning and is not in the generation contract: a
        // render's one-line label IS its `gist`, and `DirectorH3Generation` has no `idea` field for
        // the value to land in. Refuse it HERE rather than letting it through to a client that would
        // resolve the id, find a generation, and write a key nothing reads — a change the user was
        // told was applied and that no render will ever show.
        if kind == "set_idea" && matches!(space, ChatIdSpace::Generation { .. }) {
            ids.warnings.push(
                "dropped a set_idea — on this engine a shot's one-line label is its gist, and the \
                 prompt block is the only other thing stored"
                    .to_string(),
            );
            continue;
        }
        // 🔴 `revise` names MANY renders and carries an instruction, not a value: the editor runs
        // the render writer once per render. It exists because a reply that had to carry every
        // rewritten block wrote one and claimed all of them. `"all"` is expanded HERE, against the
        // listing the model was shown, so "every shot" is a fact the client can count.
        if kind == "revise" {
            if !matches!(space, ChatIdSpace::Generation { .. }) {
                ids.warnings.push(
                    "dropped a revise — it needs the render ids, which this editor did not send"
                        .to_string(),
                );
                continue;
            }
            let Some(instruction) = a
                .get("instruction")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                ids.warnings
                    .push("dropped a revise with no instruction to apply".to_string());
                continue;
            };
            let named = a.get("shots").or_else(|| a.get("shot")).cloned();
            let mut targets: Vec<String> = Vec::new();
            match named {
                Some(Value::String(v)) if v.trim().eq_ignore_ascii_case("all") => {
                    targets.extend(
                        tokens
                            .order
                            .iter()
                            .filter_map(|t| tokens.by_token.get(t).cloned()),
                    );
                }
                Some(Value::String(v)) => targets.extend(ids.lookup(Some(&v)).map(|(_, id)| id)),
                Some(Value::Array(vs)) => {
                    for v in vs.iter().filter_map(Value::as_str) {
                        if let Some((_, id)) = ids.lookup(Some(v)) {
                            targets.push(id);
                        }
                    }
                }
                _ => {}
            }
            let mut unique = std::collections::HashSet::new();
            targets.retain(|id| unique.insert(id.clone()));
            if targets.is_empty() {
                ids.warnings
                    .push("dropped a revise that named no shot on the timeline".to_string());
                continue;
            }
            out.push(json!({"type": "revise", "shots": targets, "instruction": instruction}));
            continue;
        }
        // `after` is a POSITION, not an edit target — several new shots may legitimately share one
        // anchor — so it is looked up, never deduped. Everything else names the shot being edited,
        // where a second action of the SAME type on one shot is ambiguous and the later one loses.
        let resolved: Option<Value> = match kind.as_str() {
            "set_story" => None,
            "add_shot" => match a.get("after") {
                None | Some(Value::Null) => None,
                Some(Value::String(v)) if v.trim().eq_ignore_ascii_case(AT_START) => {
                    Some(json!(AT_START))
                }
                Some(Value::String(v)) => match ids.lookup(Some(v.as_str())) {
                    Some((_, target)) => Some(json!(target)),
                    None => continue,
                },
                Some(other) => {
                    ids.warnings.push(format!(
                        "dropped an add_shot with a non-id anchor {other} — \"after\" must be a shot id or \"{AT_START}\""
                    ));
                    continue;
                }
            },
            _ => match ids.resolve(a.get("shot").and_then(Value::as_str), &kind) {
                Some(target) => Some(json!(target)),
                None => continue,
            },
        };
        match (kind.as_str(), resolved) {
            ("set_story", _) => {}
            ("add_shot", Some(v)) => a["after"] = v,
            ("add_shot", None) => {}
            (_, Some(v)) => a["shot"] = v,
            (_, None) => continue,
        }
        out.push(a);
    }
    (out, ids.warnings)
}

/// The generation briefs a chat turn may actually list: H3's, or none at all.
///
/// 🔴 **The generation listing is H3-only, and the engine decides — never the presence of the
/// field.** LTX has no generations; a `h3_generations` array on an LTX request is a client bug, and
/// honouring it would print an LTX script under H3's headings with H3's cut vocabulary. That is the
/// same class of silent mis-listing the deleted `number_shots` wrapper caused in the other
/// direction — a listing nobody diffs, describing a shape the project does not have.
///
/// Empty ⇒ the caller gets the per-cut listing, byte-identical to what it got before this field
/// existed, which is what keeps an older client unaffected.
fn chat_generation_briefs<'a>(
    engine: &str,
    gens: &'a [crate::routes::h3_prompting::H3GenerationBrief],
) -> &'a [crate::routes::h3_prompting::H3GenerationBrief] {
    if engine == ENGINE_H3 { gens } else { &[] }
}

/// The generation ids a chat turn may address — `Some` only when EVERY brief carries one.
///
/// 🔴 **All or nothing, deliberately.** A listing where some renders have an id and others do not
/// is worse than one where none do: the model cannot see why, so it either invents an id for the
/// nameless ones (dropped, and the user is told an edit went nowhere) or picks the render it CAN
/// name over the one the user meant. Falling back wholesale puts the turn on the cut-id contract,
/// which is coherent — the older-client behaviour, byte for byte.
///
/// ⚠️ It does not check that the ids are DISTINCT. [`ShotTokens`] already guarantees a unique token
/// per entry even for duplicate ids, so two generations sharing an id get two tokens that both
/// resolve to that id — the same behaviour duplicate clip ids have always had on every other path.
fn chat_generation_ids(
    gens: &[crate::routes::h3_prompting::H3GenerationBrief],
) -> Option<Vec<&str>> {
    if gens.is_empty() {
        return None;
    }
    gens.iter()
        .map(crate::routes::h3_prompting::H3GenerationBrief::id)
        .collect()
}

/// `POST /api/v1/ltx-director/chat` — a context-grounded assistant for the director editor. The full
/// shot list + story (and any reference images) are injected every turn, so the model is always
/// grounded; it answers questions AND returns edit `actions` the client applies to the timeline.
async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(req): AssetJson<ChatRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let convo: Vec<&ChatTurn> = req
        .messages
        .iter()
        .filter(|m| !m.content.trim().is_empty())
        .collect();
    if convo.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no message");
    }

    let story = req.story.unwrap_or_default();
    let shots: Vec<&ShotIn> = req.shots.iter().filter(|s| !s.is_blank()).collect();
    // The CUT tokens, always built. They are the addressable id space on LTX and on an H3 client
    // that sends no generation ids; on the H3 generation path they are the FOREIGN space, and
    // `number_generations` still needs them for the cut lines an unwritten render falls back to.
    let cuts = ShotTokens::new(&shot_ids(&shots));

    let engine = request_engine(req.engine.as_deref());
    let h3 = engine == ENGINE_H3;
    let gens = chat_generation_briefs(engine, &req.h3_generations);
    // 🔴 **The decision the whole handler turns on: what a chat action NAMES.** On H3 a shot is a
    // generation — one prompt, one render, one delivery — and its prompt block is the only stored
    // artifact, so an edit has exactly one useful destination. Naming a cut instead put `set_prompt`
    // on `DirectorClip.prompt`, a seed the compiler reads only for a generation that has no block
    // yet: the edit reached the render only for a generation nobody had opened, and silently
    // evaporated for every other one.
    //
    // ⚠️ It is derived ONCE and drives four things that must not disagree — the action contract, the
    // craft clause (what a `set_prompt` value IS), the listing's ids, and what the reply is resolved
    // against. Wiring any of them separately is how the model gets told about one id space and
    // answered in another.
    let gen_tokens = chat_generation_ids(gens).map(|ids| ShotTokens::new(&ids));
    let space = if gen_tokens.is_some() {
        ChatIdSpace::Generation { cuts: &cuts }
    } else {
        ChatIdSpace::Clip
    };
    // The tokens the model is told to echo, and the ones the reply is resolved against.
    let tokens = gen_tokens.as_ref().unwrap_or(&cuts);

    // System prompt: the action contract + the live timeline context (story + the shot list with
    // its ids). The ids are re-derived from the CURRENT document every turn, so a shot added or
    // deleted mid-conversation can't drift an earlier turn's id onto a different one.
    let mut system = chat_actions_spec(space);
    let canon = crate::routes::ltx_prompting::story_context_block(&story);
    if !canon.is_empty() {
        system.push_str("\n\n");
        system.push_str(&canon);
    }
    if shots.is_empty() {
        system.push_str("\n\nThe timeline currently has no shots.");
    } else {
        // A chat turn can rewrite prompts, so it needs the same craft rules the batch writers get —
        // its shot lines carry those tags either way.
        system.push_str("\n\n");
        if h3 {
            use crate::routes::h3_prompting as h3p;
            // The checkpoint (and with it the guide, the body field and the section set) is DERIVED
            // from the references that are actually present — never from a caller's opinion.
            let plan = h3p::H3Inputs::from_refs(&req.h3_references, false, false).resolve();
            // Same three states as `space`, and they must be read off the same facts: the craft
            // clause tells the model what a `set_prompt` value is, and the contract tells it which
            // field that value lands in. Disagreeing gives a block containing a fragment.
            let scope = match (gen_tokens.is_some(), gens.is_empty()) {
                (true, _) => h3p::H3ChatScope::Blocks,
                (false, false) => h3p::H3ChatScope::BlocksReadOnly,
                (false, true) => h3p::H3ChatScope::Cuts,
            };
            system.push_str(&h3p::chat_craft_block(
                plan.checkpoint,
                &req.h3_references,
                &req.cast,
                scope,
            ));
        } else {
            system.push_str(crate::routes::ltx_prompting::multi_shot_pacing().trim());
            // Identity is held by exact repetition of each character's appearance sentence, so a
            // rewrite that paraphrases it swaps the character out. H3 says this its own way and has
            // already done so, inside `chat_craft_block`.
            system.push_str(&crate::routes::ltx_prompting::cast_clause(&req.cast));
        }
        // Three listings. `number_generations` groups the cuts under the RENDER they are packed into
        // and prints its block — naming the render when the client gave us generation ids, the cuts
        // when it did not; `number_shots_for` prints one line per cut. Whichever it is, the ids in
        // the listing are exactly the ids `echo_ids_rule` allows and `validate_chat_actions`
        // resolves, which is the invariant that keeps the shape switchable at all.
        let (unit, listing) = match gen_tokens.as_ref() {
            _ if gens.is_empty() => (
                shots.len(),
                number_shots_for(engine, &shots, Some(&cuts), true),
            ),
            Some(g) => (
                gens.len(),
                number_generations(gens, &shots, &cuts, GenListingIds::Generations(g)),
            ),
            None => (
                gens.len(),
                number_generations(gens, &shots, &cuts, GenListingIds::Cuts),
            ),
        };
        system.push_str(&format!(
            "\n\nCurrent shots ({unit}) — this is the live state AFTER every edit so far; do not \
             redo edits already reflected here:\n{listing}{rule}",
            rule = echo_ids_rule(tokens),
        ));
    }

    let mut messages: Vec<Value> = vec![json!({"role": "system", "content": system})];

    // Reference images ride in one leading user turn so the VL model can see the established look.
    let refs: Vec<&String> = req
        .reference_images
        .iter()
        .filter(|s| !s.trim().is_empty())
        .take(6)
        .collect();
    // The H3 reference pictures, each labelled with the ordinal the blocks address it by — the
    // characters the user is talking about, rather than only their captions.
    let pictures = if h3 {
        crate::routes::h3_prompting::reference_picture_parts(&req.h3_references).1
    } else {
        Vec::new()
    };
    if !refs.is_empty() || !pictures.is_empty() {
        let mut content: Vec<Value> = vec![json!({
            "type": "text",
            "text": "Reference images for the characters / look in this project:",
        })];
        for r in refs {
            content.push(json!({
                "type": "image_url",
                "image_url": {"url": as_data_uri_local(r)},
            }));
        }
        content.extend(pictures);
        messages.push(json!({"role": "user", "content": content}));
        messages
            .push(json!({"role": "assistant", "content": "Got it — I can see the references."}));
    }

    for m in convo {
        let role = match m.role.as_str() {
            "assistant" => "assistant",
            _ => "user",
        };
        messages.push(json!({"role": role, "content": m.content.trim()}));
    }

    // A chat turn can ask to rewrite many/all shots, so scale the ceiling with the script size.
    //
    // 🔴 A generation-targeted `set_prompt` returns a COMPLETE block — up to `PROMPT_CHAR_CAP`
    // (7,000) characters, ~1,800 tokens — where the per-cut arm's 160 budgets for a paragraph. Under
    // the old figure a two-shot rewrite hit the 2,048 floor and truncated mid-section, and a
    // truncated block is not a failed edit: it parses, it stores, and it renders without whatever
    // fell off the end. 2,400/render leaves room for the reply prose alongside one block each.
    let max_tokens = match space {
        ChatIdSpace::Generation { .. } => (gens.len() as u32 * 2400).clamp(4096, 16000),
        ChatIdSpace::Clip => (shots.len() as u32 * 160).clamp(2048, 16000),
    };
    let mut body = json!({
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0.5,
    });
    crate::sampling::apply_sampling(&state.db, "sampling_director", &mut body).await;

    let client = state.director_llm.as_ref().unwrap_or(&state.llama);
    crate::director_think::enable(&mut body, client, max_tokens);
    let text = match client.chat_completion_json(&body).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => {
            warn!(error = %e, "director chat: LLM call failed");
            return err(
                StatusCode::BAD_GATEWAY,
                "chat failed — the LLM is unavailable",
            );
        }
    };

    // Parse {reply, actions} defensively — small models emit slightly-invalid JSON (raw newline in
    // a string, truncated array). Prefer a strict parse; otherwise leniently pull out the reply
    // string and salvage the actions, so the user never sees a raw `{"reply":…}` blob or a 502.
    let parsed: Option<Value> =
        serde_json::from_str(crate::routes::ltx_prompting::extract_json_object(&text)).ok();

    let actions: Vec<Value> = parsed
        .as_ref()
        .and_then(|p| p.get("actions"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_else(|| {
            text.find("\"actions\"")
                .map(|i| crate::routes::ltx_prompting::salvage_array_elements(&text[i..]))
                .unwrap_or_default()
        });

    // Reply from the object's "reply" field (strict, else lenient for slightly-invalid JSON).
    let field_reply = parsed
        .as_ref()
        .and_then(|p| p.get("reply"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if text.trim_start().starts_with('{') {
                crate::routes::ltx_prompting::lenient_json_string_field(&text, "reply")
                    .map(|r| r.trim().to_string())
                    .filter(|r| !r.is_empty())
            } else {
                None
            }
        });

    let reply = match field_reply {
        Some(mut r) => {
            // Models often close a short JSON reply then dump the real answer (e.g. a list) as
            // trailing prose past the `}` — fold that back in so it isn't lost.
            let trailing = crate::routes::ltx_prompting::text_after_json_object(&text);
            if !trailing.is_empty() && !trailing.starts_with('{') {
                r.push('\n');
                r.push_str(trailing);
            }
            r
        }
        // No JSON reply field: plain prose IS the reply; JSON-only-actions gets a stock line.
        None if actions.is_empty() => text.trim().to_string(),
        None => "Done — applied your changes.".to_string(),
    };

    let (actions, warnings) = validate_chat_actions(actions, tokens, space);
    Json(ChatResponseOut {
        reply,
        actions: Value::Array(actions),
        warnings,
        target: space.wire(),
    })
    .into_response()
}

/// Compute per-shot durations from the LLM's cut points. Lyrics present → each
/// shot spans from its `start_word` onset to the next shot's onset (last → the
/// last word's end). Lyrics absent → the LLM's `seconds` (even split of
/// `target_seconds` as a fallback). All floored to a sane min so no shot is
/// shorter than the VAE's smallest clip.
fn resolve_durations(
    shots: &[LlmShot],
    lyrics: &[LyricWord],
    target_seconds: Option<f64>,
    fps: u32,
) -> Vec<f64> {
    let min_sec = 9.0 / fps as f64; // the 9-frame VAE floor
    let n = shots.len();

    if !lyrics.is_empty() {
        // Onset per shot: the cited word's start, clamped into range; shot 0 at 0.
        let last_end = lyrics.last().map(|w| w.end.max(w.start)).unwrap_or(0.0);
        let mut onsets: Vec<f64> = shots
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if i == 0 {
                    return 0.0;
                }
                s.start_word
                    .and_then(|wi| lyrics.get(wi))
                    .map(|w| w.start)
                    .unwrap_or_else(|| {
                        // No/'bad word index → even-space across the song.
                        last_end * (i as f64) / (n as f64)
                    })
            })
            .collect();
        // Monotonic non-decreasing onsets.
        for i in 1..onsets.len() {
            if onsets[i] < onsets[i - 1] {
                onsets[i] = onsets[i - 1];
            }
        }
        return (0..n)
            .map(|i| {
                let end = if i + 1 < n { onsets[i + 1] } else { last_end };
                (end - onsets[i]).max(min_sec)
            })
            .collect();
    }

    // No lyrics: honour the LLM's per-shot seconds, else split target evenly.
    let fallback = target_seconds
        .map(|t| (t / n as f64).max(min_sec))
        .unwrap_or(4.0);
    shots
        .iter()
        .map(|s| s.seconds.unwrap_or(fallback).max(min_sec))
        .collect()
}

/// What the planner is being asked to cut into shots.
struct DirectorPlan<'a> {
    brief: &'a str,
    story: &'a StoryContext,
    lyrics: &'a [LyricWord],
    ref_images: &'a [RefImage],
    cast: &'a [LtxCastMember],
    max_shots: usize,
    allow_beats: bool,
    /// `ltx` or `h3` — which engine's prompt conventions the planner is briefed with.
    engine: &'static str,
    /// H3 only: the project's omni references, in order, so a shot's idea can name the ones it is
    /// about. Empty on LTX, whose identity refs cannot be addressed by name.
    h3_references: &'a [crate::routes::h3_prompting::H3RefBrief],
}

/// Build the director prompt + call the planning LLM (gemma if configured, else
/// the main LLM). Returns the raw reply text or an error response.
async fn run_director_llm(state: &AppState, plan: DirectorPlan<'_>) -> Vec<LlmShot> {
    let DirectorPlan {
        brief,
        story,
        lyrics,
        ref_images,
        cast,
        max_shots,
        allow_beats,
        engine,
        h3_references,
    } = plan;
    // Plan in windows of ~CHUNK_SHOTS so a long video never becomes one oversized generation
    // (the old single call OOM'd llama-server → 502). Each window sees only its lyric slice + a
    // rolling recap of the story so far, keeping input AND output bounded regardless of length.
    const CHUNK_SHOTS: usize = 10;
    let has_lyrics = !lyrics.is_empty();

    let n_chunks = max_shots.div_ceil(CHUNK_SHOTS).max(1);
    // Lyric slices per window (contiguous, roughly equal). Empty on the instrumental path.
    let word_ranges = if has_lyrics {
        partition_ranges(lyrics.len(), n_chunks)
    } else {
        Vec::new()
    };
    let n_chunks = if has_lyrics {
        word_ranges.len().max(1)
    } else {
        n_chunks
    };

    // The two engines want a structurally different plan, not a differently-worded one — H3 groups
    // shots into GENERATIONS (cuts are free inside one, the boundary is the expensive seam) where
    // LTX chains continuations — so they get two system prompts rather than one with flags. Both
    // live beside the conventions they encode: LTX's below, H3's in `routes::h3_prompting`.
    //
    // 🔴 `new_scene` means different things per engine and the client compiles it differently: on
    // LTX it is a hard cut, on H3 it is "start a NEW generation". See
    // `h3_prompting::director_system_prompt`.
    let h3 = engine == ENGINE_H3;
    let system = if h3 {
        crate::routes::h3_prompting::director_system_prompt(story, h3_references, cast)
    } else {
        director_system_prompt(story, ref_images, cast, allow_beats)
    };
    let client = state.director_llm.as_ref().unwrap_or(&state.llama);

    let mut all_shots: Vec<LlmShot> = Vec::with_capacity(max_shots);
    let mut story_so_far: Option<String> = None;

    for c in 0..n_chunks {
        let remaining = max_shots.saturating_sub(all_shots.len());
        if remaining == 0 {
            break;
        }
        let k = remaining.min(CHUNK_SHOTS);

        let user = director_user_prompt(
            h3,
            brief,
            ref_images,
            story_so_far.as_deref(),
            all_shots.last().map(|s| s.idea.trim()),
            has_lyrics,
            word_ranges.get(c).copied(),
            lyrics,
            k,
        );

        // Story canon images first (identity/mood), then the auto-director's own ref frames.
        let mut content: Vec<Value> = vec![json!({"type": "text", "text": user})];
        content.extend(crate::routes::ltx_prompting::story_context_images(story));
        for r in ref_images {
            content.push(json!({
                "type": "image_url",
                "image_url": {"url": as_data_uri_local(&r.image_b64)},
            }));
        }
        // The H3 reference pictures themselves, each labelled with its ordinal — the planner names
        // characters by ordinal in every description, so it has to be able to see who they are.
        if h3 {
            content.extend(crate::routes::h3_prompting::reference_picture_parts(h3_references).1);
        }
        // 🔴 An H3 entry is a render's whole shooting script — a paragraph a shot, ~1,000 tokens a
        // render — where an LTX entry is a one-liner. A truncated chunk drops the WHOLE window.
        let answer = if h3 {
            (k as u32 * 1200).max(2048)
        } else {
            (k as u32 * 180).max(1024)
        };

        let mut body = json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": content},
            ],
            // `answer` is sized per engine above. LTX: k structure entries of ~45 tokens plus beats,
            // with ~2.5x headroom, because a truncated chunk JSON drops the WHOLE window.
            "max_tokens": answer,
            "temperature": 0.7,
        });
        crate::sampling::apply_sampling(&state.db, "sampling_director", &mut body).await;
        crate::director_think::enable(&mut body, client, answer);

        let reply = match client.chat_completion_json(&body).await {
            Ok((_permit, resp)) => resp
                .pointer("/choices/0/message/content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            Err(e) => {
                // One flaky window shouldn't sink the whole plan; skip it and press on. If EVERY
                // window fails (e.g. llama-server down) we return empty → caller 502s.
                warn!(chunk = c, error = %e, "auto-director: chunk LLM call failed; skipping");
                continue;
            }
        };

        // Salvage the shots that arrived even if a dense chunk's JSON was truncated, rather than
        // dropping the whole window (which left gaps in long auto-directed scripts).
        let chunk: LlmChunk = match serde_json::from_str(extract_json_object(&reply)) {
            Ok(p) => p,
            Err(e) => {
                let salvaged: Vec<LlmShot> =
                    crate::routes::ltx_prompting::salvage_array_elements(&reply)
                        .into_iter()
                        .filter_map(|v| serde_json::from_value(v).ok())
                        .collect();
                if salvaged.is_empty() {
                    warn!(chunk = c, error = %e, reply = %reply, "auto-director: chunk unparseable + unsalvageable; skipping");
                    continue;
                }
                warn!(
                    chunk = c,
                    recovered = salvaged.len(),
                    "auto-director: recovered shots from truncated chunk JSON"
                );
                LlmChunk {
                    shots: salvaged,
                    summary: None,
                }
            }
        };
        if let Some(s) = chunk
            .summary
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            story_so_far = Some(s);
        }
        let before = all_shots.len();
        all_shots.extend(
            chunk
                .shots
                .into_iter()
                .filter(|s| !s.idea.trim().is_empty()),
        );
        // H3 with no transcript is told to stop once the brief is told rather than pad the video
        // out to `max_shots`. A short window is that signal — another window would invent material.
        if h3 && !has_lyrics && all_shots.len() - before < k {
            break;
        }
    }

    all_shots.truncate(max_shots);
    all_shots
}

/// The director's system prompt — identical for every planning window. Leads with the project
/// canon (so characters/scenario are honoured), then the shot-structure rules, then the chunked
/// JSON reply contract (`shots` + a rolling `summary`).
///
/// Stage 1 of two. The planner decides STRUCTURE — cut points, durations, mode, ref-image choice,
/// scene boundaries — and seeds each shot with a light `idea`. It deliberately does NOT write
/// prompts: a chunked planner asked for a dozen cinematic paragraphs at once is starved for output
/// budget and produces thin ones, whereas the per-shot Draft path writes each prompt with that
/// shot's own length, neighbours and continuation context in hand. Hence no `BASE`/`audio_clause`
/// here — prompt-writing craft belongs to the path that writes prompts.
fn director_system_prompt(
    story: &StoryContext,
    ref_images: &[RefImage],
    cast: &[LtxCastMember],
    allow_beats: bool,
) -> String {
    let mut system = String::from(
        "You are the director of an AI music-video generator for LTX-2.5. You cut the song or \
         brief into SHOTS and give each shot a one-line idea. You do NOT write the finished shot \
         prompts — a later pass expands each idea into a full prompt, so keep every idea light, \
         concrete and expandable: a seed, not a paragraph. ",
    );
    if !story.is_empty() {
        system.push_str(&crate::routes::ltx_prompting::story_context_block(story));
        system.push(' ');
    }
    system.push_str(
        "Each shot is ONE continuous take of roughly 2-10 seconds — never longer. Cut to a new shot \
         rather than letting one sprawl across unrelated moments, and give each shot enough room to \
         land exactly one beat. Each shot's visuals must track what is sung or said at that moment.",
    );
    system.push_str(
        " Set \"new_scene\":true on a shot that HARD CUTS to a different place, time or subject, \
         and false when it should flow straight out of the previous shot's final frame — the \
         renderer literally continues from that frame, so shots within one scene stay visually \
         consistent for free, while a hard cut re-invents everything. Group the video into a few \
         real scenes; do not hard-cut every shot.",
    );
    if !ref_images.is_empty() {
        system.push_str(
            " The attached reference images define the characters/look; keep them consistent. A \
             shot that should render from a reference frame sets \"ref_image\" to that image's \
             index (0-based).",
        );
    }
    if crate::routes::ltx_prompting::story_has_images(story) {
        system.push_str(
            " Reference images labelled as canon characters/mood define the established look — \
             MATCH their subjects exactly; they are not frames of any single shot.",
        );
    }
    // The planner writes IDEAS, not prompts, but it is where a character first gets named — and a
    // shot that calls the same person something else is what the expansion pass then inherits.
    system.push_str(&crate::routes::ltx_prompting::cast_clause(cast));
    system.push_str(if !allow_beats {
        " Never use beats: plan every shot as ONE continuous action and omit \"beats\" entirely. \
         A shot that would need two separate moments should be split into two shots instead."
    } else {
        " A shot where something HAPPENS AT A MOMENT rather than throughout can carry timed BEATS: \
         the shot's own idea stays the setting for the whole take, and each beat says what CHANGES \
         at one moment inside it. One, two or three beats — never four. Keep each beat to a few \
         words naming only the change (\"sets the glass down\", \"turns to the door\"), and never \
         repeat the setting inside a beat. \
         A SINGLE beat is not only allowed but the most reliable kind: it pins one clear event to \
         one moment, and is the right shape whenever a shot is mostly one continuous state with a \
         single thing happening partway through (\"walks down the corridor\" + a beat at 4s for \
         \"a cat lands on his shoulder\"). Prefer placing a beat in the BACK HALF of its shot — an \
         event asked for very early is less likely to land. \
         Time a beat EITHER with \"start_word\" (an index on the transcript lane, so the action \
         lands on that word) OR with \"seconds\" (an offset from the START of that shot). Use \
         \"seconds\" whenever there is no transcript, or when the moment you want does not \
         correspond to anything being said. A shot that is genuinely one uninterrupted action still \
         needs no beats at all."
    });
    system.push_str(
        " You plan the video in successive batches. Reply with STRICT JSON only, no prose: \
         {\"shots\":[{\"start_word\":<int>,\"idea\":\"<a terse 4-10 word one-liner naming the shot>\",\
         \"mode\":\"i2v\"|\"t2v\",\"ref_image\":<int|null>,\"new_scene\":<true|false>,\
         \"beats\":[{\"start_word\":<int>,\"seconds\":<number>,\"idea\":\"<a few words: what changes \
         here>\"}]}],\
         \"summary\":\"<1-2 sentences recapping the story THROUGH the shots you just wrote, so the \
         next batch continues seamlessly>\"}. An \"idea\" is a short human label like \"neon alley \
         chase, rain\" — never a cinematic paragraph, never camera or lighting jargon. Omit \"beats\" \
         entirely for a single-action shot. Omit code fences.",
    );
    system
}

/// The per-window user prompt: brief + ref-image legend + the rolling "story so far" recap +
/// this window's lyric slice (or the instrumental instruction), asking for up to `k` shots.
#[allow(clippy::too_many_arguments)]
fn director_user_prompt(
    h3: bool,
    brief: &str,
    ref_images: &[RefImage],
    story_so_far: Option<&str>,
    prev_shot: Option<&str>,
    has_lyrics: bool,
    word_range: Option<(usize, usize)>,
    lyrics: &[LyricWord],
    k: usize,
) -> String {
    if h3 {
        return h3_director_user_prompt(
            brief,
            story_so_far,
            prev_shot,
            word_range.filter(|_| has_lyrics),
            lyrics,
            k,
        );
    }
    let mut user = String::new();
    if !brief.is_empty() {
        user.push_str(&format!("Brief / concept:\n{brief}\n\n"));
    }
    if !ref_images.is_empty() {
        user.push_str("Reference images (by index):\n");
        for (i, r) in ref_images.iter().enumerate() {
            let label = r.label.as_deref().unwrap_or("reference");
            user.push_str(&format!("  {i}: {label}\n"));
        }
        user.push('\n');
    }
    if let Some(recap) = story_so_far {
        user.push_str(&format!(
            "STORY SO FAR (continue from here; do NOT repeat earlier shots):\n{recap}\n"
        ));
        if let Some(prev) = prev_shot.filter(|p| !p.is_empty()) {
            user.push_str(&format!("The immediately previous shot was: {prev}\n"));
        }
        user.push('\n');
    }
    match (has_lyrics, word_range) {
        (true, Some((ws, we))) => {
            user.push_str(
                "Transcript with word onsets — each token is [index @onset-seconds] word. This is \
                 whatever the audio happens to contain: song lyrics, dialogue, narration, or a mix. \
                 It is machine-transcribed and often WRONG or garbled, so treat the text as a rough \
                 hint at the mood and subject matter, NOT as words that must be illustrated or lip \
                 synced — trust the timing, not the spelling. Where the transcript falls silent the \
                 audio is instrumental or ambient, and a shot there should NOT show anyone singing \
                 or talking. Choose shot boundaries by picking the word index where each new shot \
                 begins. The shot's duration is derived from the next shot's onset, so cuts land on \
                 the audio — which means the GAP you leave between consecutive start_words IS each \
                 shot's length. Space them so every shot lands in the 2-10s range:\n",
            );
            user.push_str(&lyrics_transcript_range(lyrics, ws, we));
            let last = we.saturating_sub(1);
            let span = section_span(lyrics, ws, we);
            user.push_str(&format!(
                "\n\nThis section spans word {ws} to {last}, about {span:.0}s of audio. Produce up \
                 to {k} shots covering ONLY this section; the first shot MUST start at word {ws}, \
                 and every start_word must fall within {ws}..={last}."
            ));
        }
        _ => {
            user.push_str(&format!(
                "There are no lyric timings. Produce up to {k} shots that continue the story in \
                 order; give each shot its own \"seconds\" duration (roughly 2-10s, sized to the \
                 beat it plays) instead of \"start_word\"."
            ));
        }
    }
    user
}

/// The per-window user prompt on H3: the brief VERBATIM, where the story has got to, and either
/// this window's transcript or the instruction to size each render itself.
///
/// 🔴 The unit is the RENDER (one clip, 4-15 s) — see `h3_prompting::director_system_prompt`. The
/// LTX prompt's "2-10s" per shot is exactly the guidance that produced a timeline of 4-second
/// renders, so this arm says nothing of the kind.
fn h3_director_user_prompt(
    brief: &str,
    story_so_far: Option<&str>,
    prev_render: Option<&str>,
    word_range: Option<(usize, usize)>,
    lyrics: &[LyricWord],
    k: usize,
) -> String {
    use crate::routes::h3_prompting::{MAX_GENERATION_SECONDS, MIN_GENERATION_SECONDS};
    let mut user = String::new();
    if !brief.is_empty() {
        user.push_str(&format!(
            "The author's brief — work from ALL of it:\n<<<\n{brief}\n>>>\n\n"
        ));
    }
    if let Some(recap) = story_so_far {
        user.push_str(&format!(
            "STORY SO FAR (continue from here; do NOT repeat what is already planned):\n{recap}\n"
        ));
        if let Some(prev) = prev_render.filter(|p| !p.is_empty()) {
            user.push_str(&format!(
                "The render immediately before, as planned:\n{prev}\n"
            ));
        }
        user.push('\n');
    }
    match word_range {
        Some((ws, we)) => {
            user.push_str(&format!(
                "Transcript with word onsets — each token is [index @onset-seconds] word. It is \
                 machine-transcribed and often WRONG, so trust the timing, not the spelling. Choose \
                 each render's start by the word index it begins on: the gap between consecutive \
                 start_words IS that render's length, so space them {MIN_GENERATION_SECONDS:.0} to \
                 {MAX_GENERATION_SECONDS:.0} seconds apart, filling toward \
                 {MAX_GENERATION_SECONDS:.0}. Put the cuts INSIDE each render on the music.\n"
            ));
            user.push_str(&lyrics_transcript_range(lyrics, ws, we));
            let last = we.saturating_sub(1);
            let span = section_span(lyrics, ws, we);
            user.push_str(&format!(
                "\n\nThis section spans word {ws} to {last}, about {span:.0}s of audio. Produce up \
                 to {k} renders covering ONLY this section; the first render MUST start at word \
                 {ws}, and every start_word must fall within {ws}..={last}."
            ));
        }
        None => {
            user.push_str(&format!(
                "There are no lyric timings. Produce up to {k} renders that continue the story in \
                 order, each with its own \"seconds\" — as long as its material allows, up to \
                 {MAX_GENERATION_SECONDS:.0}. If the whole brief is told before you reach {k}, stop \
                 there: never pad the video out with renders the brief did not ask for."
            ));
        }
    }
    user
}

/// Wall-clock length of a lyric slice `[start, end)`, so a planning window can be told how much
/// song it is cutting up and size its shots to fit.
fn section_span(words: &[LyricWord], start: usize, end: usize) -> f64 {
    let end = end.min(words.len());
    if start >= end {
        return 0.0;
    }
    let first = words[start].start;
    let last = &words[end - 1];
    (last.end.max(last.start) - first).max(0.0)
}

/// Wrap a bare base64 image as a data URI for the VL model (passes `data:` through).
fn as_data_uri_local(b64: &str) -> String {
    if b64.starts_with("data:") {
        b64.to_string()
    } else {
        format!("data:image/png;base64,{b64}")
    }
}

// ─── Lyric alignment (STT) ───────────────────────────────────────────────────

/// `POST /api/v1/ltx-director/transcribe` — word-align an audio track for the
/// timeline's lyric lane. Multipart: `file` (audio), optional `language`,
/// optional `granularity` (`word` | `segment` | `char`, default `word`).
/// Returns `{ text, words, segments, language }` where each word/segment is
/// `{ start, end, text }` in seconds — the editor maps `start` to an x-position
/// so cuts/keyframes can snap to lyric onsets.
async fn transcribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }

    let mut audio: Option<(String, Bytes)> = None;
    let mut language: Option<String> = None;
    let mut granularity: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name() {
            Some("file") => {
                let filename = field
                    .file_name()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "audio.wav".to_string());
                match field.bytes().await {
                    Ok(b) => audio = Some((filename, b)),
                    Err(_) => return err(StatusCode::BAD_REQUEST, "failed to read 'file' part"),
                }
            }
            Some("language") => language = field.text().await.ok().filter(|s| !s.is_empty()),
            Some("granularity") => granularity = field.text().await.ok().filter(|s| !s.is_empty()),
            _ => {}
        }
    }

    let Some((filename, bytes)) = audio else {
        return err(StatusCode::BAD_REQUEST, "missing 'file' (audio) part");
    };

    let params = SttParams {
        timestamp_granularity: Some(granularity.unwrap_or_else(|| "word".to_string())),
        beam_size: None,
        language,
    };

    match state
        .stt
        .transcribe_upload(&filename, bytes, &params, None)
        .await
    {
        Ok((permit, resp, _wait)) => {
            drop(permit); // release the GPU permit before serialising the response
            Json(json!({
                "text": resp.text,
                "words": resp.words,
                "segments": resp.segments,
                "language": resp.language,
            }))
            .into_response()
        }
        Err(e) => {
            warn!(error = %e, "director transcribe failed");
            err(StatusCode::BAD_GATEWAY, "transcription failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltx_jobs::LtxJob;
    use koblem_client::types::{H3Continuation, VideoJobPartial, VideoJobState, h3_snap_frames};

    /// Anything that is not `h3` is LTX-2.5, the only LTX engine — including the retired `ltx`,
    /// which no longer names a service anything could dispatch to. H3 is the one that has to be
    /// named: an H3 prompt rendered on LTX carries an audio block and inline negatives into a
    /// model with a separate audio sheet and a negative-prompt field.
    #[test]
    fn anything_but_h3_reads_as_ltx25() {
        assert_eq!(request_engine(None), ENGINE_LTX25);
        assert_eq!(request_engine(Some("")), ENGINE_LTX25);
        assert_eq!(request_engine(Some("wan")), ENGINE_LTX25);
        assert_eq!(request_engine(Some("ltx")), ENGINE_LTX25);
        assert_eq!(request_engine(Some("ltx25")), ENGINE_LTX25);
        assert_eq!(request_engine(Some(" ltx25 ")), ENGINE_LTX25);
        assert_eq!(request_engine(Some(" h3 ")), ENGINE_H3);
        assert!(is_h3(Some("h3")));
        assert!(!is_h3(None));
        assert!(!is_h3(Some("ltx25")));
    }

    /// A saved document's engine survives the round trip the same way.
    ///
    /// 🔴 A document still naming `ltx` opens on 2.5 — the owner confirmed no Director project was
    /// left on LTX-2.3 — and the stored string is NOT rewritten. That matters one level down: the
    /// project resolves as `ltx25`, so `director_shot_renders` rows banked as `'ltx'` no longer
    /// match the engine-filtered resume queries and a 2.3 job id can never be adopted as a chain.
    #[test]
    fn project_engine_reads_h3_and_defaults_to_ltx25() {
        assert_eq!(project_engine(&json!({ "engine": "ltx25" })), ENGINE_LTX25);
        assert_eq!(project_engine(&json!({ "engine": "h3" })), ENGINE_H3);
        assert_eq!(project_engine(&json!({ "engine": "ltx" })), ENGINE_LTX25);
        assert_eq!(project_engine(&json!({})), ENGINE_LTX25);
        assert_eq!(project_engine(&json!({ "engine": "nope" })), ENGINE_LTX25);
        // The retired name is still a distinct VALUE — it is what banked rows and old documents
        // hold — and nothing above turns one of those into an `ltx25` bank id.
        assert_ne!(ENGINE_LTX, ENGINE_LTX25);
    }

    /// H3's grid snaps UP, and the direction is the expensive thing to get wrong: an H3 prompt
    /// carries its own timed shot list and audio cues, so a writer given a shorter number than the
    /// render actually produces places its last cue after the render has ended.
    ///
    /// The FLOOR is **22 frames (0.917 s)** — the video VAE's own decodable minimum, below which
    /// a 2-latent block decodes 0 chunks and the decoder raises. A near-zero duration therefore
    /// renders as 0.92 s, not as a 5-frame stub.
    ///
    /// 🔴 It was 107 (4.458 s) until 2026-08-07, and that silently lengthened every shorter beat.
    /// 107 is only the grid-snap of MiniMax's documented 4-second guideline, which WanGP hardened
    /// into a `raise`; nothing in the model requires it.
    #[test]
    fn h3_frames_snap_up_onto_the_grid_and_never_under_the_floor() {
        use koblem_client::types::{H3_MAX_FRAMES, H3_MIN_FRAMES};
        assert_eq!(h3_frames_from_seconds(0.01), H3_MIN_FRAMES);
        // 🔴 4.0 s is no longer a floor: it lands on the grid at 107 because 96 rounds up, which
        // is arithmetic, not a clamp. A 2-second beat now stays 2 seconds.
        assert_eq!(h3_frames_from_seconds(4.0), 107);
        assert_eq!(h3_frames_from_seconds(2.0), 56);
        // 5.0 s = 120 frames → 124, NOT 120.
        assert_eq!(h3_frames_from_seconds(5.0), 124);
        // Already on the grid: unchanged.
        assert_eq!(h3_frames_from_seconds(124.0 / 24.0), 124);
        assert!(h3_snapped_seconds(5.0) > 5.0);
        // …and the ceiling clamps rather than growing: 20 s is not renderable in one generation.
        assert_eq!(h3_frames_from_seconds(20.0), H3_MAX_FRAMES);
    }

    /// The H3 listing must not offer a writer LTX's `carried` mode: on H3 a shot's relationship to
    /// its neighbour is decided by the PACKING, not by the shot. Inside a generation a cut is a
    /// timed `[Shot N]` tag in one prompt and continuity across it is free; across a generation
    /// boundary it is a seam the writer cannot see from the listing either way. So the listing
    /// carries the number and the length and takes no position on continuity.
    #[test]
    fn h3_shot_listing_takes_no_position_on_continuity() {
        let shots = [
            ShotIn {
                id: "a".into(),
                prompt: "one".into(),
                seconds: Some(6.0),
                ..Default::default()
            },
            ShotIn {
                id: "b".into(),
                prompt: "two".into(),
                seconds: Some(6.0),
                ..Default::default()
            },
        ];
        let refs: Vec<&ShotIn> = shots.iter().collect();
        let listed = number_shots_for(ENGINE_H3, &refs, None, true);
        assert_eq!(listed, "Shot 1 [6.0s]: one\nShot 2 [6.0s]: two\n");
        assert!(!listed.contains("carried"));
        assert!(!listed.contains("CONTINUES"));
        // The LTX arm is untouched.
        let ltx = number_shots_for(ENGINE_LTX25, &refs, None, true);
        assert!(ltx.contains("carried"));
    }

    fn cut(id: &str, prompt: &str, seconds: f64) -> ShotIn {
        ShotIn {
            id: id.into(),
            prompt: prompt.into(),
            seconds: Some(seconds),
            ..Default::default()
        }
    }

    /// A brief with no generation `id` — the older-client shape, which keeps chat on the cut id
    /// space. `named_gen_brief` is the one that moves it.
    fn gen_brief(
        prompt: Option<&str>,
        seconds: f64,
        task: &str,
        shots: &[&str],
    ) -> crate::routes::h3_prompting::H3GenerationBrief {
        crate::routes::h3_prompting::H3GenerationBrief {
            id: None,
            gist: None,
            prompt: prompt.map(str::to_string),
            seconds: Some(seconds),
            task: Some(task.into()),
            shots: shots.iter().map(|s| (*s).to_string()).collect(),
            authored: None,
        }
    }

    /// The same brief, named — i.e. a render a chat action can address.
    fn named_gen_brief(
        id: &str,
        prompt: Option<&str>,
        seconds: f64,
        task: &str,
        shots: &[&str],
    ) -> crate::routes::h3_prompting::H3GenerationBrief {
        crate::routes::h3_prompting::H3GenerationBrief {
            id: Some(id.into()),
            ..gen_brief(prompt, seconds, task, shots)
        }
    }

    /// 🔴 The whole point of the listing: the reviewer sees the BLOCK that will actually be sent,
    /// byte for byte, under a header that gives it the duration its cut times must fall inside, the
    /// checkpoint (and therefore the body field it must use) and the ids of the cuts inside it.
    ///
    /// Feeding it `number_shots_for` instead — one line per clip — leaves every schema check in
    /// `review_system` unanswerable, which is not a weaker review but an invitation to invent.
    #[test]
    fn the_h3_review_listing_carries_each_generations_block_verbatim() {
        let block = "detailed_description:\n[Shot 1] A woman walks.\n\n[Shot 2] At 00:04.500, cut to \
                     the wall.";
        let shots = [cut("aaaa1111", "walks", 4.5), cut("bbbb2222", "wall", 9.9)];
        let refs: Vec<&ShotIn> = shots.iter().collect();
        let tokens = ShotTokens::new(&shot_ids(&refs));
        // 15.083 s = 362 frames — the real ceiling, and a duration whose two-decimal form is not a
        // rounding tie (14.375 is, and a test that turns on tie-breaking tests the formatter).
        let gens = [gen_brief(
            Some(block),
            15.0833,
            "ref2va",
            &["aaaa1111", "bbbb2222"],
        )];
        let listed = number_generations(&gens, &refs, &tokens, GenListingIds::Cuts);
        assert_eq!(
            listed,
            format!(
                "Shot 1 — 2 cuts (ids aaaa11, bbbb22) · 15.08s · ref2va (body field \
                 `detailed_description:`)\n{block}\n\n"
            )
        );
        // The ids in the header are the ones the reply contract accepts — the mapping back to clip
        // ids is what turns a finding into a one-click rework.
        let mut ids = IdResolver::new(&tokens);
        assert_eq!(
            ids.lookup(Some("bbbb22")).map(|(_, clip)| clip),
            Some("bbbb2222".to_string())
        );
    }

    /// 🔴 The chat surface reuses that same listing, and the gate is the ENGINE. An LTX project that
    /// somehow carries `h3_generations` must not be described to the model in generations it does
    /// not have — the failure would be a listing nobody diffs, exactly like the `number_shots`
    /// wrapper's.
    #[test]
    fn only_an_h3_chat_turn_is_shown_generations() {
        let gens = [gen_brief(
            Some("detailed_description:\nx"),
            6.0,
            "t2va",
            &[],
        )];
        assert_eq!(chat_generation_briefs(ENGINE_H3, &gens).len(), 1);
        assert!(chat_generation_briefs(ENGINE_LTX25, &gens).is_empty());
        // No briefs is the older client, and it takes the per-cut listing it always took.
        assert!(chat_generation_briefs(ENGINE_H3, &[]).is_empty());
    }

    /// 🔴 A generation nobody has opened has NO block. Presenting an empty one would read as an
    /// author who wrote nothing, so the entry says it is unwritten and falls back to the cut bodies
    /// — the same text the old listing carried, now labelled honestly.
    #[test]
    fn an_unwritten_generation_says_so_instead_of_showing_an_empty_block() {
        let shots = [cut("cccc3333", "she leaves", 6.0)];
        let refs: Vec<&ShotIn> = shots.iter().collect();
        let tokens = ShotTokens::new(&shot_ids(&refs));
        // Blank is unwritten: a stored empty string must not be mistaken for a block.
        let gens = [gen_brief(Some("  \n "), 6.0, "t2va", &["cccc3333"])];
        let listed = number_generations(&gens, &refs, &tokens, GenListingIds::Cuts);
        assert!(listed.contains("NOT WRITTEN YET"), "{listed}");
        assert!(
            listed.contains("Cut 1 (id cccc33) [6.0s]: she leaves"),
            "{listed}"
        );
        assert!(
            listed.contains("t2va (body field `integrated_multimodal_description:`)"),
            "{listed}"
        );
        // Absent is the same state as blank.
        let absent = [gen_brief(None, 6.0, "t2va", &["cccc3333"])];
        assert_eq!(
            listed,
            number_generations(&absent, &refs, &tokens, GenListingIds::Cuts)
        );
    }

    /// A seeded block is what renders, so it is shown in full — but the reviewer is told nobody
    /// wrote it, or it files a finding for every section the author has not reached yet. An absent
    /// flag reads as authored, so a client that sends only the block is unchanged.
    #[test]
    fn a_seeded_block_is_shown_but_flagged_as_nobodys_work() {
        let shots = [cut("aaaa1111", "walks", 4.5)];
        let refs: Vec<&ShotIn> = shots.iter().collect();
        let tokens = ShotTokens::new(&shot_ids(&refs));
        let mut brief = gen_brief(
            Some("detailed_description:\n[Shot 1] A woman walks."),
            4.5,
            "ref2va",
            &["aaaa1111"],
        );
        assert!(
            !number_generations(
                std::slice::from_ref(&brief),
                &refs,
                &tokens,
                GenListingIds::Cuts
            )
            .contains("NOT AUTHORED")
        );
        brief.authored = Some(false);
        let listed = number_generations(
            std::slice::from_ref(&brief),
            &refs,
            &tokens,
            GenListingIds::Cuts,
        );
        assert!(listed.contains("NOT AUTHORED"), "{listed}");
        assert!(listed.contains("[Shot 1] A woman walks."), "{listed}");
    }

    /// A cut the packing does not place still has to reach the reviewer. Dropping it silently is
    /// exactly the miss a continuity pass exists to catch, and a cut with no entry can never be
    /// named by a finding either.
    #[test]
    fn cuts_no_generation_claims_are_listed_rather_than_dropped() {
        let shots = [cut("dddd4444", "one", 4.0), cut("eeee5555", "two", 4.0)];
        let refs: Vec<&ShotIn> = shots.iter().collect();
        let tokens = ShotTokens::new(&shot_ids(&refs));
        let gens = [gen_brief(
            Some("detailed_description:\nx"),
            4.0,
            "ref2va",
            &["dddd4444"],
        )];
        let listed = number_generations(&gens, &refs, &tokens, GenListingIds::Cuts);
        assert!(listed.contains("Shot 1 — 1 cut (id dddd44)"), "{listed}");
        assert!(listed.contains("does not place"), "{listed}");
        assert!(listed.contains("Cut (id eeee55) [4.0s]: two"), "{listed}");
    }

    /// A generation whose clips all carry blank text has no id the model may echo (they were
    /// filtered out upstream), so it must not be handed one — an echoed id that maps to nothing is
    /// a finding dropped in silence.
    #[test]
    fn a_generation_with_no_nameable_cut_says_findings_on_it_need_null() {
        let shots = [cut("ffff6666", "kept", 4.0)];
        let refs: Vec<&ShotIn> = shots.iter().collect();
        let tokens = ShotTokens::new(&shot_ids(&refs));
        let gens = [
            gen_brief(
                Some("detailed_description:\nx"),
                4.0,
                "ref2va",
                &["ffff6666"],
            ),
            gen_brief(
                Some("detailed_description:\ny"),
                4.0,
                "ref2va",
                &["blank-clip"],
            ),
        ];
        let listed = number_generations(&gens, &refs, &tokens, GenListingIds::Cuts);
        assert!(listed.contains("Shot 2 — 0 cuts (no cut ids"), "{listed}");
        assert!(!listed.contains("blank-clip"), "{listed}");
    }

    // ── Chat's H3 id space: the RENDER, not the cut ───────────────────────────

    /// 🔴 The bug this whole arm closes. Chat is a project-level assistant, and on H3 the thing it
    /// can usefully change is the RENDER: one generation, one prompt block, one delivery. The
    /// listing must therefore name the render and nothing else — a cut token printed beside a body
    /// is an invitation to address a `DirectorClip.prompt` that the compiler stops reading the
    /// moment the generation has a block.
    #[test]
    fn the_chat_listing_names_the_render_and_never_its_cuts() {
        let block = "detailed_description:\n[Shot 1] A woman walks.\n\n[Shot 2] At 00:04.500, she \
                     looks up.";
        let shots = [
            cut("aaaa1111", "walks", 4.5),
            cut("bbbb2222", "looks up", 9.9),
        ];
        let refs: Vec<&ShotIn> = shots.iter().collect();
        let cuts = ShotTokens::new(&shot_ids(&refs));
        let mut brief = named_gen_brief(
            "9999aaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            Some(block),
            15.0833,
            "ref2va",
            &["aaaa1111", "bbbb2222"],
        );
        brief.gist = Some("she waits at the bar".into());
        let gens = [brief];
        let gen_tokens = ShotTokens::new(&chat_generation_ids(&gens).unwrap());
        let listed =
            number_generations(&gens, &refs, &cuts, GenListingIds::Generations(&gen_tokens));
        assert_eq!(
            listed,
            format!(
                "Shot 1 (id 9999aa) — 2 cuts · 15.08s · ref2va (body field \
                 `detailed_description:`) · gist: \"she waits at the bar\"\n{block}\n\n"
            )
        );
        // The cut tokens exist — they are the FOREIGN space — but no byte of the listing offers one.
        assert!(
            !listed.contains("aaaa11") && !listed.contains("bbbb22"),
            "{listed}"
        );
    }

    /// An unwritten render still falls back to its cut bodies, because that text is all there is —
    /// but under generation ids those lines carry no `(id …)`, or the model would name one.
    #[test]
    fn cut_bodies_are_listed_without_ids_when_the_render_is_the_target() {
        let shots = [
            cut("cccc3333", "she leaves", 6.0),
            cut("dddd4444", "orphan", 4.0),
        ];
        let refs: Vec<&ShotIn> = shots.iter().collect();
        let cuts = ShotTokens::new(&shot_ids(&refs));
        let gens = [named_gen_brief("gen-1", None, 6.0, "t2va", &["cccc3333"])];
        let gen_tokens = ShotTokens::new(&chat_generation_ids(&gens).unwrap());
        let listed =
            number_generations(&gens, &refs, &cuts, GenListingIds::Generations(&gen_tokens));
        assert!(listed.contains("Shot 1 (id gen1) — 1 cut"), "{listed}");
        assert!(listed.contains("Cut 1 [6.0s]: she leaves"), "{listed}");
        // The unplaced tail too — a cut no render claims is context, never a target.
        assert!(listed.contains("Cut [4.0s]: orphan"), "{listed}");
        assert!(
            !listed.contains("(id cccc33)") && !listed.contains("(id dddd44)"),
            "{listed}"
        );
    }

    /// 🔴 All or nothing. A listing where some renders can be named and others cannot is worse than
    /// one where none can: the model cannot see which is which, so it invents an id for the nameless
    /// ones and the user is told an edit went nowhere. One id missing ⇒ the whole turn falls back to
    /// the cut contract, which at least is coherent.
    #[test]
    fn one_unnamed_generation_drops_the_whole_turn_to_the_cut_id_space() {
        let named = named_gen_brief(
            "gen-1",
            Some("detailed_description:\nx"),
            4.0,
            "ref2va",
            &[],
        );
        let unnamed = gen_brief(Some("detailed_description:\ny"), 4.0, "ref2va", &[]);
        assert_eq!(
            chat_generation_ids(std::slice::from_ref(&named)),
            Some(vec!["gen-1"])
        );
        assert_eq!(chat_generation_ids(&[named, unnamed]), None);
        // And no briefs at all is the older client, not a nameless one.
        assert_eq!(chat_generation_ids(&[]), None);
    }

    /// The contract the model is handed has to describe the field the value lands in. Under
    /// generation ids that is `DirectorH3Generation.prompt`, so `set_prompt` is a whole block and
    /// `set_idea` — which has no generation-level home — is not offered at all.
    #[test]
    fn the_generation_contract_asks_for_a_block_and_drops_set_idea() {
        let cuts = ShotTokens::new(&["aaaa1111"]);
        let gen_spec = chat_actions_spec(ChatIdSpace::Generation { cuts: &cuts });
        let clip = chat_actions_spec(ChatIdSpace::Clip);
        assert!(gen_spec.contains("COMPLETE prompt block"), "{gen_spec}");
        assert!(!gen_spec.contains("set_idea\",\"shot\""), "{gen_spec}");
        assert!(
            gen_spec.contains("ONE SHOT = ONE RENDER = ONE PROMPT BLOCK"),
            "{gen_spec}"
        );
        assert!(gen_spec.contains("at most 15.08 s"), "{gen_spec}");
        // The clip arm is the pre-existing prompt: `set_idea` present, no generation gloss.
        assert!(clip.contains(r#"{"type":"set_idea","shot":"<id>","value":"<short one-liner>"}"#));
        assert!(!clip.contains("ONE SHOT = ONE RENDER"));
        // Both keep the shared envelope and the shared rules, which is what makes the split safe.
        for s in [&gen_spec, &clip] {
            assert!(s.starts_with("You are the director's assistant"), "{s}");
            assert!(s.contains("Reply with EXACTLY ONE JSON object"), "{s}");
            assert!(
                s.contains("NEVER re-emit an edit you already applied"),
                "{s}"
            );
        }
    }

    /// 🔴 A cut id in a generation-targeted reply must fail LOUDLY. It is dropped either way, but
    /// "the model did not echo an id it was given" is false for an id that was on screen last turn,
    /// and the warning is the only thing that tells the author (and the next turn) what went wrong.
    #[test]
    fn a_cut_id_in_a_generation_turn_is_refused_by_name() {
        let cuts = ShotTokens::new(&["aaaa1111-2222-3333-4444-555555555555"]);
        let gens = ShotTokens::new(&["9999aaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]);
        let space = ChatIdSpace::Generation { cuts: &cuts };
        let raw = vec![
            json!({"type": "set_prompt", "shot": gens.token(0), "value": "detailed_description:\nx"}),
            // The cut id — real, addressable on `/review`, and not this surface's to write.
            json!({"type": "set_prompt", "shot": cuts.token(0), "value": "she walks"}),
            // An id from neither space keeps the original wording.
            json!({"type": "set_gist", "shot": "ffffff", "value": "x"}),
            // `set_idea` has nowhere to land on a generation.
            json!({"type": "set_idea", "shot": gens.token(0), "value": "cold open"}),
        ];
        let (out, warnings) = validate_chat_actions(raw, &gens, space);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["shot"],
            json!("9999aaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert_eq!(warnings.len(), 3);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("a moment INSIDE a shot")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("the model did not echo an id it was given")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("dropped a set_idea")),
            "{warnings:?}"
        );
        // ⚠️ On the CLIP arm the same `set_idea` is a legitimate action and must survive untouched.
        let (out, _) = validate_chat_actions(
            vec![json!({"type": "set_idea", "shot": cuts.token(0), "value": "cold open"})],
            &cuts,
            ChatIdSpace::Clip,
        );
        assert_eq!(out.len(), 1);
    }

    /// 🔴 The owner's report: "tell it to fix every shot and only one shot will get changed (but it
    /// thinks it's fixing every shot)". A chat reply had to carry every rewritten block — up to
    /// 7,000 characters each — inside one JSON string, so it wrote one and said it had done them
    /// all. `revise` carries the INSTRUCTION instead, and the editor runs the render writer once
    /// per render. `"all"` is expanded here, so "every shot" cannot quietly mean one.
    #[test]
    fn a_revise_names_every_render_it_touches() {
        let cuts = ShotTokens::new(&["aaaa1111"]);
        let gens = ShotTokens::new(&["g1111111", "g2222222", "g3333333"]);
        let space = ChatIdSpace::Generation { cuts: &cuts };
        let raw = vec![
            json!({"type": "revise", "shots": "all", "instruction": "make the nurse older"}),
            json!({"type": "revise", "shots": [gens.token(2), gens.token(0)], "instruction": "rain"}),
            json!({"type": "revise", "shot": gens.token(1), "instruction": "night"}),
            json!({"type": "revise", "shots": "all", "instruction": "  "}),
            json!({"type": "revise", "shots": ["ffffff"], "instruction": "x"}),
        ];
        let (out, warnings) = validate_chat_actions(raw, &gens, space);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0]["shots"], json!(["g1111111", "g2222222", "g3333333"]));
        assert_eq!(out[0]["instruction"], json!("make the nurse older"));
        // Order as the model gave it; the editor revises in this order.
        assert_eq!(out[1]["shots"], json!(["g3333333", "g1111111"]));
        assert_eq!(out[2]["shots"], json!(["g2222222"]));
        assert!(
            warnings.iter().any(|w| w.contains("no instruction")),
            "{warnings:?}"
        );
        // On the clip arm there is no render writer to hand it to.
        let (out, warnings) = validate_chat_actions(
            vec![json!({"type": "revise", "shots": "all", "instruction": "x"})],
            &cuts,
            ChatIdSpace::Clip,
        );
        assert!(out.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    /// The generation contract offers `revise` and says when it is the only right answer.
    #[test]
    fn the_generation_contract_offers_revise_for_multi_render_changes() {
        let cuts = ShotTokens::new(&["aaaa1111"]);
        let s = chat_actions_spec(ChatIdSpace::Generation { cuts: &cuts });
        assert!(s.contains("{\"type\":\"revise\""), "{s}");
        assert!(s.contains("more than one render"), "{s}");
        assert!(!chat_actions_spec(ChatIdSpace::Clip).contains("revise"));
    }

    /// The client cannot tell a generation id from a clip id by looking at it, so the response says
    /// which it is — and says NOTHING on the clip arm, keeping an LTX response byte-identical to
    /// what it was before the field existed.
    #[test]
    fn the_response_names_its_id_space_only_when_it_is_the_new_one() {
        let cuts = ShotTokens::new(&["aaaa1111"]);
        assert_eq!(ChatIdSpace::Clip.wire(), None);
        assert_eq!(
            ChatIdSpace::Generation { cuts: &cuts }.wire(),
            Some("generation")
        );
        let clip = serde_json::to_value(ChatResponseOut {
            reply: "ok".into(),
            actions: json!([]),
            warnings: vec![],
            target: ChatIdSpace::Clip.wire(),
        })
        .unwrap();
        assert_eq!(clip, json!({"reply": "ok", "actions": []}));
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn row(version: i64) -> repo::DirectorProjectRow {
        repo::DirectorProjectRow {
            id: "p1".into(),
            name: "Shared".into(),
            timeline: json!({ "request": { "segments": [] } }),
            metadata: json!({}),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
            version,
        }
    }

    /// A save from a stale copy is a CONFLICT, not a missing row and not a server fault — and the
    /// body has to carry the live version or the client has nothing to resync against.
    #[tokio::test]
    async fn stale_update_maps_to_409_with_the_current_version() {
        let resp = update_response(repo::UpdateOutcome::Conflict { current_version: 7 });
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(resp).await["current_version"], 7);
    }

    #[tokio::test]
    async fn deleted_project_still_maps_to_404_and_success_returns_the_new_version() {
        assert_eq!(
            update_response(repo::UpdateOutcome::Missing).status(),
            StatusCode::NOT_FOUND
        );

        let resp = update_response(repo::UpdateOutcome::Updated(row(4)));
        assert_eq!(resp.status(), StatusCode::OK);
        // Every read path hands back a token; the PUT response is one of them.
        assert_eq!(body_json(resp).await["version"], 4);
    }

    /// A client old enough to predate the check must not get a free unconditional overwrite — the
    /// whole point is that the stale tab loses.
    #[test]
    fn an_unversioned_put_body_is_rejected_not_defaulted() {
        let body: ProjectUpdate =
            serde_json::from_value(json!({ "name": "x", "timeline": {} })).unwrap();
        assert!(body.version.is_none());
        let versioned: ProjectUpdate =
            serde_json::from_value(json!({ "name": "x", "timeline": {}, "version": 3 })).unwrap();
        assert_eq!(versioned.version, Some(3));
        assert_eq!(versioned.project.name, "x");
    }

    fn job(project: Option<&str>, tier: &str, state: VideoJobState) -> LtxJob {
        LtxJob {
            state,
            server_job_id: Some("srv-1".into()),
            director_critique: false,
            queue_job_id: None,
            segments: 3,
            scripts: vec!["a".into(), "b".into(), "c".into()],
            gate_wait_ms: 0,
            render_ms: 0,
            error: None,
            content_type: None,
            cancel_requested: false,
            partials: vec![VideoJobPartial {
                segment_index: 0,
                url: "/api/v1/ltx-video/jobs/kob-1/segments/0".into(),
            }],
            delivery_warnings: Vec::new(),
            director_project_id: project.map(str::to_string),
            director_tier: Some(tier.to_string()),
            engine: None,
            director_shot_offset: None,
            job_is_partial: false,
            renders_window: false,
            total_eta_seconds: None,
            progress: None,
            current_step: None,
            total_steps: None,
            phase: None,
            preview_seq: None,
            generation_index: None,
            generations_done: None,
            bank_keys: Vec::new(),
            seed: Some(7),
            created: 1_700_000_000,
            updated: 1_700_000_005,
        }
    }

    /// A retake restores the take the user picked, in FILTERED index space — and claims nothing
    /// about the shots it does not touch.
    ///
    /// The blank shot in the middle is the point: `by_shot` is keyed by the engine's own
    /// prompt-filtered numbering, so indexing the raw array with it attaches shot 2's take to shot 3.
    #[test]
    fn per_shot_takes_follow_the_filtered_index_and_stop_at_the_tail() {
        let segments = || -> Vec<koblem_client::types::LtxSegment> {
            ["a", "b", "", "c", "d"]
                .iter()
                .map(|p| serde_json::from_value(json!({ "prompt": p })).unwrap())
                .collect()
        };
        let by_shot: std::collections::HashMap<i32, String> = (0..4)
            .map(|i| (i, format!("srv-{i}")))
            .collect::<Vec<_>>()
            .into_iter()
            .collect();
        let picked = |segs: &[koblem_client::types::LtxSegment]| -> Vec<Option<String>> {
            segs.iter().map(|s| s.bank_job_id.clone()).collect()
        };

        // Raw index 3 is filtered index 2 — the blank shot is skipped. The engine renders that shot
        // alone and restores only [0, 2). Marking the tail would make a claim the engine answers
        // with a warning about a take being "replaced" when it is untouched.
        let mut retake = segments();
        apply_selected_takes(&mut retake, &by_shot, Some(3));
        assert_eq!(
            picked(&retake),
            vec![Some("srv-0".into()), Some("srv-1".into()), None, None, None]
        );

        // A PREFIX shot with no recorded take keeps the chain default rather than being guessed at.
        let mut sparse = segments();
        apply_selected_takes(
            &mut sparse,
            &std::collections::HashMap::from([(1, "srv-1".to_string())]),
            Some(3),
        );
        assert_eq!(
            picked(&sparse),
            vec![None, Some("srv-1".into()), None, None, None]
        );
    }

    /// A CONTINUE names every banked shot, because a chain of them spans several banks.
    ///
    /// 🔴 The failure: LTX-2.5's bank is keyed `"<job>:<segment>"` with no id aliasing, so the
    /// single `resume_job_id` a continue carries resolves ONLY the segments that one job banked.
    /// Render 5 shots as job A, continue two more as job B, and B's bank holds segments 5-6 and no
    /// segment 0 at all — the next continue is a hard 400. Naming each shot's own bank makes the
    /// chain length irrelevant, and the shots being APPENDED have no row, so they stay unclaimed
    /// (naming one there is a shot the engine warns is "replaced" rather than restored).
    #[test]
    fn a_continue_names_every_banked_shot_across_a_chain_of_banks() {
        let mut segs: Vec<koblem_client::types::LtxSegment> = ["a", "b", "", "c", "d", "e", "f"]
            .iter()
            .map(|p| serde_json::from_value(json!({ "prompt": p })).unwrap())
            .collect();
        // Filtered 0-1 banked by job A, 2-3 by the first continue's job B; 4-5 are the new tail.
        let by_shot: std::collections::HashMap<i32, String> = [
            (0, "srv-a".to_string()),
            (1, "srv-a".to_string()),
            (2, "srv-b".to_string()),
            (3, "srv-b".to_string()),
        ]
        .into_iter()
        .collect();
        apply_selected_takes(&mut segs, &by_shot, None);
        assert_eq!(
            segs.iter()
                .map(|s| s.bank_job_id.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("srv-a".into()),
                Some("srv-a".into()),
                None,
                Some("srv-b".into()),
                Some("srv-b".into()),
                None,
                None,
            ]
        );
    }

    /// 🔴 A partial render must not be filed as the project's cut, and must not swap the tier.
    ///
    /// The failure this pins is destructive and silent: on LTX-2.5 a resume's `/media` is the
    /// RENDERED SEGMENTS ONLY, stream-copy concatenated, and it keeps the caller's own segment
    /// numbering — so nothing about the indices looks wrong. Copying those bytes over the stitched
    /// cut replaces a five-shot cut with the two shots the resume happened to render, and a wipe
    /// alongside it then deletes the rows for the three it restored from the bank.
    #[test]
    fn a_partial_render_neither_becomes_the_cut_nor_swaps_the_tier() {
        let full = job(Some("p1"), "final", VideoJobState::Completed);
        assert_eq!(
            harvest_scope(&full, true),
            HarvestScope {
                shot_offset: 0,
                partial_render: false,
                wipe: true
            }
        );

        // ltx25 resume / retake: the engine says partial and names the window it rendered, and
        // there is NO offset — the takes still file at their own index. Reading the offset alone is
        // exactly the bug.
        let mut resumed = full.clone();
        resumed.engine = Some("ltx25".into());
        resumed.job_is_partial = true;
        resumed.renders_window = true;
        assert_eq!(
            harvest_scope(&resumed, true),
            HarvestScope {
                shot_offset: 0,
                partial_render: true,
                wipe: false
            }
        );

        // 🔴 A FULL render whose concatenation failed sets the SAME flag and is the OPPOSITE case.
        // `/media` is segment 0 alone, so it is still not the cut — but every shot rendered, so the
        // tier swap is exactly what should happen. Treating it as a window left the previous run's
        // trailing rows behind, and a timeline that had lost shots rebuilt an over-long cut out of
        // them. The engine separates the two itself: no `rendered_segments`, so no window.
        let mut broken = full.clone();
        broken.engine = Some("ltx25".into());
        broken.job_is_partial = true;
        assert!(harvest_scope(&broken, true).partial_render);
        assert!(harvest_scope(&broken, true).wipe);
        // …and it is the DISPATCH that decides there is a swap to have at all.
        assert!(!harvest_scope(&broken, false).wipe);

        // H3's retake shifts instead, and that is still what makes it partial there.
        let mut h3_retake = full.clone();
        h3_retake.engine = Some("h3".into());
        h3_retake.director_shot_offset = Some(2);
        assert_eq!(
            harvest_scope(&h3_retake, false),
            HarvestScope {
                shot_offset: 2,
                partial_render: true,
                wipe: false
            }
        );

        // A caller that never asked to wipe is not given one.
        assert!(!harvest_scope(&full, false).wipe);
    }

    /// A retake that cannot save its own overrides must not render them. Answering 202 with the
    /// LIVE version handed the caller a token for a document it never had: it adopts that token,
    /// its next autosave passes the CAS, and the peer's work is gone — the exact lost update the
    /// version column exists to stop.
    #[tokio::test]
    async fn a_retake_that_loses_the_cas_is_a_409_not_a_202_with_a_borrowed_token() {
        let conflict = repo::UpdateOutcome::Conflict {
            current_version: 12,
        };
        assert_eq!(retake_may_dispatch(&conflict), None);
        let stop = update_response(conflict);
        assert_eq!(stop.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(stop).await["current_version"], 12);

        assert_eq!(retake_may_dispatch(&repo::UpdateOutcome::Missing), None);
        assert_eq!(
            retake_may_dispatch(&repo::UpdateOutcome::Updated(row(9))),
            Some(9)
        );
    }

    /// `metadata` reaches Postgres as jsonb, where `'null'::jsonb || '{…}'::jsonb` silently
    /// evaluates to an ARRAY — one such write and every later stamp appended an element while every
    /// read returned nothing, for the life of the row. Only PATCH used to check.
    #[test]
    fn only_an_object_may_be_stored_as_metadata() {
        assert!(metadata_is_object(None));
        assert!(metadata_is_object(Some(&json!({}))));
        assert!(metadata_is_object(Some(&json!({ "notes": "x" }))));
        for bad in [Value::Null, json!([]), json!(3), json!("meta"), json!(true)] {
            assert!(!metadata_is_object(Some(&bad)), "{bad}");
        }
    }

    /// A body-supplied `resume_job_id` was forwarded to the engine verbatim, before any check: a
    /// proxy retake could be pointed at the FINAL chain's bank (low-res latents appended onto
    /// high-res — what the tier split exists to prevent) or at another project's latents entirely.
    #[test]
    fn an_explicit_resume_id_must_belong_to_this_project_and_tier() {
        // The banked list the caller passes is already scoped to the tier being retaken.
        let banked = |tier: &str| match tier {
            "proxy" => vec!["engine-proxy".to_string()],
            _ => vec!["engine-final".to_string()],
        };
        let jobs = vec![
            (
                "kob-mine".to_string(),
                job(Some("p1"), "proxy", VideoJobState::Completed),
            ),
            (
                "kob-other".to_string(),
                job(Some("p2"), "proxy", VideoJobState::Completed),
            ),
        ];
        let allowed = |id: &str, tier: &str| {
            explicit_resume_allowed(id, &banked(tier), &jobs, "p1", tier, ENGINE_LTX)
        };

        assert!(allowed("engine-proxy", "proxy"));
        assert!(allowed("engine-final", "final"));
        // Just-finished render whose harvest has not banked a row yet — the editor retaking the
        // shot it watched land is the common case, not an edge one.
        assert!(allowed("srv-1", "proxy"));

        // Cross-tier (low-res latents onto the high-res chain), cross-project, and pure invention.
        assert!(!allowed("engine-proxy", "final"));
        assert!(!allowed("engine-final", "proxy"));
        assert!(!allowed("srv-1", "final"));
        assert!(!explicit_resume_allowed(
            "srv-1",
            &[],
            &jobs,
            "p3",
            "proxy",
            ENGINE_LTX
        ));
        assert!(!allowed("../../etc/passwd", "proxy"));
        // Cross-ENGINE, which is the same claim as cross-project: the two LTX deployments keep
        // separate job stores, so `srv-1` names nothing in 2.5's. The banked list is already
        // engine-filtered by its caller; this is the in-flight half of the same rule.
        assert!(!explicit_resume_allowed(
            "srv-1",
            &[],
            &jobs,
            "p1",
            "proxy",
            ENGINE_LTX25
        ));
    }

    /// A persisted clip is re-read on every project load and after every render; without a validator
    /// each of those was the whole file again. The comparison has to accept the shapes a browser
    /// actually sends, or the 304 never happens and the header is decoration.
    #[test]
    fn a_clip_revalidates_on_the_etag_it_was_served() {
        let tag = "\"1f4-abc\"";
        assert!(etag_matches(tag, tag));
        assert!(etag_matches("*", tag));
        assert!(etag_matches("W/\"1f4-abc\"", tag));
        assert!(etag_matches("\"other\", \"1f4-abc\"", tag));

        assert!(!etag_matches("\"1f4-abd\"", tag));
        assert!(!etag_matches("", tag));
        // A rebuilt cut reuses its file NAME with new bytes, so the tag has to move with the file.
        assert_ne!(
            render_etag(&std::fs::metadata("Cargo.toml").unwrap()),
            "\"0-0\""
        );
    }

    /// This is the whole point of the endpoint: a second device asks "what is rendering for MY
    /// project" and gets only that — not another project's render, not a finished one it would
    /// re-adopt and re-watch forever, and not an untagged scratch render from the /ltx-video page.
    #[test]
    fn active_lists_only_this_projects_unfinished_renders() {
        let jobs = vec![
            (
                "kob-live".to_string(),
                job(Some("p1"), "final", VideoJobState::Rendering),
            ),
            (
                "kob-done".to_string(),
                job(Some("p1"), "final", VideoJobState::Completed),
            ),
            (
                "kob-failed".to_string(),
                job(Some("p1"), "proxy", VideoJobState::Failed),
            ),
            (
                "kob-other".to_string(),
                job(Some("p2"), "final", VideoJobState::Rendering),
            ),
            (
                "kob-scratch".to_string(),
                job(None, "final", VideoJobState::Rendering),
            ),
        ];
        let items = active_jobs_for_project("p1", &jobs, &Default::default());
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0],
            json!({
                "job_id": "kob-live",
                "project_id": "p1",
                "tier": "final",
                "status": "rendering",
                "harvesting": false,
                "segments": 3,
                "completed_segments": 1,
                "scripts": ["a", "b", "c"],
                "server_job_id": "srv-1",
                "seed": 7,
                "partials": [{
                    "segment_index": 0,
                    "url": "/api/v1/ltx-video/jobs/kob-1/segments/0",
                }],
                "engine": "ltx",
                "shot_offset": null,
                "job_is_partial": false,
                "created_at": "2023-11-14T22:13:20Z",
            })
        );
    }

    /// The signal a reloaded tab has no other way to recover: `pendingPartialRender` is PREDICTED at
    /// dispatch, and a client adopting somebody else's render never dispatched anything. Every
    /// spelling has to reach it — H3 is a fragment by its SHIFT and sets no flag, LTX-2.5 is one by
    /// the flag and keeps the caller's own numbering — or the tab adopts one generation as the
    /// project's whole cut.
    #[test]
    fn active_reports_a_fragment_however_the_job_spells_it() {
        let full = job(Some("p1"), "final", VideoJobState::Rendering);
        let mut h3_retake = full.clone();
        h3_retake.director_shot_offset = Some(4);
        let mut ltx25_resume = full.clone();
        ltx25_resume.job_is_partial = true;
        // The window on its own. `bank_report` always sets the flag beside it, so this is the term
        // `delivers_fragment` states so a producer that stopped doing so cannot make it wrong.
        let mut windowed = full.clone();
        windowed.renders_window = true;

        let of = |j: LtxJob| {
            active_jobs_for_project("p1", &[("kob-1".to_string(), j)], &Default::default())[0]
                ["job_is_partial"]
                .clone()
        };
        assert_eq!(of(full), json!(false));
        assert_eq!(of(h3_retake), json!(true));
        assert_eq!(of(ltx25_resume), json!(true));
        assert_eq!(of(windowed), json!(true));
    }

    /// A queued job has no partials and no engine id yet, but IS the answer to "is something
    /// running for this project" — it must not be filtered out or serialise to junk.
    #[test]
    fn active_includes_a_queued_job_with_nothing_rendered_yet() {
        let mut j = job(Some("p1"), "proxy", VideoJobState::Queued);
        j.server_job_id = None;
        j.partials.clear();
        let items = active_jobs_for_project("p1", &[("kob-q".to_string(), j)], &Default::default());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["status"], json!("queued"));
        assert_eq!(items[0]["tier"], json!("proxy"));
        assert_eq!(items[0]["completed_segments"], json!(0));
        assert_eq!(items[0]["server_job_id"], Value::Null);
    }

    /// The `/active` → `/renders` handoff. A job goes terminal the moment the render ends, but its
    /// clips only reach `/renders` when the harvest lands — and a full run's harvest replaces the
    /// tier's whole set on the way there. A second device that stopped polling `/active` on
    /// "terminal" and asked for the durable rows could read a half-swapped set, which is why the
    /// client had a blind 2s sleep. The server now says so.
    #[test]
    fn a_finished_render_stays_active_until_its_harvest_lands() {
        let jobs = vec![
            (
                "kob-harvesting".to_string(),
                job(Some("p1"), "proxy", VideoJobState::Completed),
            ),
            (
                "kob-settled".to_string(),
                job(Some("p1"), "proxy", VideoJobState::Completed),
            ),
        ];
        let pending = std::collections::HashSet::from(["kob-harvesting".to_string()]);

        let items = active_jobs_for_project("p1", &jobs, &pending);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["job_id"], json!("kob-harvesting"));
        // Still active, but the render itself is over — a client shows "saving", not "rendering".
        assert_eq!(items[0]["status"], json!("completed"));
        assert_eq!(items[0]["harvesting"], json!(true));

        // Once the harvest clears, the project has nothing in flight and the durable rows are whole.
        assert!(active_jobs_for_project("p1", &jobs, &Default::default()).is_empty());

        // A render still RUNNING is pending a harvest too, but its rows are not being rewritten yet
        // — a client must read that as "rendering" (and attach to it), not as "wait for the rows".
        let live = vec![(
            "kob-live".to_string(),
            job(Some("p1"), "proxy", VideoJobState::Rendering),
        )];
        let pending_live = std::collections::HashSet::from(["kob-live".to_string()]);
        assert_eq!(
            active_jobs_for_project("p1", &live, &pending_live)[0]["harvesting"],
            json!(false)
        );
    }

    fn queue_row(id: &str, state: koblem_client::types::JobState) -> koblem_client::types::Job {
        serde_json::from_value(json!({
            "id": id,
            "kind": "h3",
            "state": state,
            "band": 1,
            "queue_rank": 1.0,
            "created_by": "web",
            "created_at": "2026-08-16T17:33:48Z",
        }))
        .expect("queue row fixture")
    }

    /// 🔴 The defect that made the Director reload its own video every twelve seconds. A cancel
    /// goes through the QUEUE; `ltx_jobs` keeps saying `Queued` because the terminal state is
    /// written by a render task that, for a job cancelled before dispatch, never ran. `sweep` drops
    /// terminal jobs only — so without the overlay the dead row is in `/active` for the life of the
    /// process, and every open tab re-adopts it on every poll.
    #[test]
    fn active_drops_a_job_the_queue_cancelled_under_it() {
        let mut cancelled = job(Some("p1"), "final", VideoJobState::Queued);
        cancelled.queue_job_id = Some("q-1".into());
        let jobs = with_queue_state(
            vec![("kob-1".to_string(), cancelled)],
            &[queue_row("q-1", koblem_client::types::JobState::Cancelled)],
        );
        assert!(active_jobs_for_project("p1", &jobs, &Default::default()).is_empty());
    }

    /// The overlay must not swallow the jobs the endpoint exists for: a row genuinely waiting its
    /// turn is still in flight, and a record that reached a terminal state of its own outranks the
    /// row (the adapter files `Completed` inside its collect pass, before the runner marks `done`).
    #[test]
    fn active_keeps_a_job_the_queue_is_still_holding() {
        let mut waiting = job(Some("p1"), "final", VideoJobState::Queued);
        waiting.queue_job_id = Some("q-1".into());
        let jobs = with_queue_state(
            vec![("kob-1".to_string(), waiting)],
            &[queue_row("q-1", koblem_client::types::JobState::Queued)],
        );
        let items = active_jobs_for_project("p1", &jobs, &Default::default());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["status"], json!("queued"));

        let mut done = job(Some("p1"), "final", VideoJobState::Completed);
        done.queue_job_id = Some("q-2".into());
        let jobs = with_queue_state(
            vec![("kob-2".to_string(), done)],
            &[queue_row("q-2", koblem_client::types::JobState::Running)],
        );
        assert!(active_jobs_for_project("p1", &jobs, &Default::default()).is_empty());
    }

    /// A record with no queue row at all is untouched — the `/ltx-video` page's own renders and
    /// anything dispatched before the queue existed still report exactly what the store holds.
    #[test]
    fn a_job_with_no_queue_row_is_left_alone() {
        let jobs = with_queue_state(
            vec![(
                "kob-1".to_string(),
                job(Some("p1"), "final", VideoJobState::Rendering),
            )],
            &[queue_row(
                "q-other",
                koblem_client::types::JobState::Cancelled,
            )],
        );
        assert_eq!(
            active_jobs_for_project("p1", &jobs, &Default::default()).len(),
            1
        );
    }

    /// The guard, not a matched pair of calls: the render it covers can panic or be cancelled, and a
    /// job that never cleared would sit in `/active` forever — which reads as "the engine is busy"
    /// and blocks every client from starting another render.
    #[test]
    fn a_pending_harvest_clears_even_if_the_render_unwinds() {
        let job_id = format!("kob-guard-{}", uuid::Uuid::new_v4());
        let held = job_id.clone();
        let died = std::panic::catch_unwind(move || {
            let _pending = HarvestPending::new(&held);
            assert!(pending_harvests().contains(&held));
            panic!("render died");
        });
        assert!(died.is_err());
        assert!(!pending_harvests().contains(&job_id));
    }

    /// `/run` and `/retake` were final-only; the tier now arrives from the caller, so an unknown
    /// one has to be REJECTED rather than normalised into a render nothing can find again.
    #[test]
    fn tier_defaults_to_final_and_rejects_anything_unknown() {
        assert_eq!(tier_or_final(None), Some("final"));
        assert_eq!(tier_or_final(Some("")), Some("final"));
        assert_eq!(tier_or_final(Some("  ")), Some("final"));
        assert_eq!(tier_or_final(Some(" proxy ")), Some("proxy"));
        assert_eq!(tier_or_final(Some("final")), Some("final"));
        assert_eq!(tier_or_final(Some("Proxy")), None);
        assert_eq!(tier_or_final(Some("draft")), None);
    }

    /// A `<video src>` cannot set an Authorization header, so the persisted-render route has to
    /// take the same credential as a query param. Both paths must accept a good token and both
    /// must reject a bad one — a query fallback that skips validation is an open media proxy.
    #[tokio::test]
    async fn media_auth_accepts_header_or_query_token() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
        else {
            eprintln!("test database unreachable, skipping");
            return;
        };
        let key = Some("secret-key");
        let bearer = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert("authorization", format!("Bearer {v}").parse().unwrap());
            h
        };

        assert!(media_auth_ok(&pool, key, &bearer("secret-key"), None).await);
        assert!(media_auth_ok(&pool, key, &HeaderMap::new(), Some("secret-key")).await);
        // A stale/absent header is rescued by a good query token — the media-element path.
        assert!(media_auth_ok(&pool, key, &bearer(""), Some("secret-key")).await);

        assert!(!media_auth_ok(&pool, key, &HeaderMap::new(), None).await);
        assert!(!media_auth_ok(&pool, key, &HeaderMap::new(), Some("   ")).await);
        assert!(!media_auth_ok(&pool, key, &HeaderMap::new(), Some("wrong")).await);
        assert!(!media_auth_ok(&pool, key, &bearer("wrong"), Some("wrong")).await);
    }

    fn word(text: &str, start: f64) -> LyricWord {
        LyricWord {
            start,
            end: start + 0.3,
            text: text.to_string(),
        }
    }

    /// Four clips with realistic uuid-ish ids.
    fn tokens4() -> (ShotTokens, Vec<String>) {
        let ids: Vec<String> = (1..=4)
            .map(|i| format!("{i}1111111-2222-3333-4444-55555555555{i}"))
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        (ShotTokens::new(&refs), ids)
    }

    #[test]
    fn tokens_are_short_stable_and_unique() {
        let (t, ids) = tokens4();
        for (i, id) in ids.iter().enumerate() {
            // Short enough for a small model to copy verbatim, and derived from the id (not the
            // position) so it survives a shot being added or deleted between turns.
            assert_eq!(t.token(i).len(), 6);
            assert!(id.to_lowercase().replace('-', "").starts_with(t.token(i)));
        }
        let uniq: std::collections::HashSet<&str> = (0..4).map(|i| t.token(i)).collect();
        assert_eq!(uniq.len(), 4);
        // Duplicate ids can't collapse onto one token — each still gets its own.
        let dupes = ShotTokens::new(&["same-id", "same-id"]);
        assert_ne!(dupes.token(0), dupes.token(1));
    }

    #[test]
    fn gist_mapping_keys_on_id_not_position() {
        let (t, ids) = tokens4();
        // The model skipped shot 2 and returned shot 4 before shot 3. Under positional mapping the
        // "robot marches" gist would land on clip 2; keyed on id it lands on clip 4.
        let raw = vec![
            RawGist {
                id: Some(t.token(0).into()),
                gist: "opening".into(),
            },
            RawGist {
                id: Some(t.token(3).into()),
                gist: "robot marches".into(),
            },
            RawGist {
                id: Some(t.token(2).into()),
                gist: "she turns".into(),
            },
        ];
        let mut r = IdResolver::new(&t);
        let by_id: std::collections::HashMap<String, String> = map_gists(raw, &mut r)
            .into_iter()
            .map(|g| (g.id, g.gist))
            .collect();
        assert_eq!(by_id.get(&ids[0]).map(String::as_str), Some("opening"));
        assert_eq!(
            by_id.get(&ids[3]).map(String::as_str),
            Some("robot marches")
        );
        assert_eq!(by_id.get(&ids[2]).map(String::as_str), Some("she turns"));
        assert_eq!(by_id.get(&ids[1]), None); // skipped — left untouched, not shifted
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn unknown_and_duplicate_ids_are_dropped_never_guessed() {
        let (t, ids) = tokens4();
        let raw = vec![
            RawGist {
                id: Some(t.token(0).into()),
                gist: "keep".into(),
            },
            // An id the model invented: close to a real token, but not one. It must NOT be
            // fuzzy-matched onto shot 1 — the whole point of the id contract.
            RawGist {
                id: Some(format!("{}z", &t.token(0)[..5])),
                gist: "invented".into(),
            },
            // A second gist for shot 1: ambiguous, so the later one loses.
            RawGist {
                id: Some(t.token(0).into()),
                gist: "duplicate".into(),
            },
            // No id at all — position is not evidence of which shot this describes.
            RawGist {
                id: None,
                gist: "anonymous".into(),
            },
        ];
        let mut r = IdResolver::new(&t);
        let got = map_gists(raw, &mut r);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, ids[0]);
        assert_eq!(got[0].gist, "keep");
        assert_eq!(r.warnings.len(), 3);
        assert!(r.warnings.iter().any(|w| w.contains("unknown shot id")));
        assert!(r.warnings.iter().any(|w| w.contains("duplicate")));
    }

    #[test]
    fn id_lookup_normalises_format_only() {
        let (t, ids) = tokens4();
        let mut r = IdResolver::new(&t);
        // Quoting/case/whitespace a model wraps around the token are format, not identity.
        for raw in [
            format!(" {} ", t.token(1)),
            format!("\"{}\"", t.token(1)),
            format!("#{}", t.token(1)),
            t.token(1).to_uppercase(),
        ] {
            assert_eq!(
                r.lookup(Some(&raw)).map(|x| x.1).as_deref(),
                Some(ids[1].as_str())
            );
        }
        // A prefix of a real token is NOT that token.
        assert!(r.lookup(Some(&t.token(1)[..4])).is_none());
    }

    #[test]
    fn chat_actions_resolve_ids_and_drop_unmappable() {
        let (t, ids) = tokens4();
        let raw = vec![
            json!({"type": "set_prompt", "shot": t.token(1), "value": "night"}),
            // Same shot, different action type — both are legitimate and must survive.
            json!({"type": "set_gist", "shot": t.token(1), "value": "she waits"}),
            // Same shot AND type twice — ambiguous, later one dropped.
            json!({"type": "set_prompt", "shot": t.token(1), "value": "day"}),
            // A hallucinated id must not land anywhere.
            json!({"type": "delete_shot", "shot": "ffffff"}),
            // Story edits carry no shot id.
            json!({"type": "set_story", "field": "tone", "value": "bleak"}),
            json!({"type": "add_shot", "after": AT_START, "idea": "cold open"}),
            json!({"type": "add_shot", "after": t.token(2), "idea": "reprise"}),
            // A leftover positional anchor is not an id — drop rather than mis-place the shot.
            json!({"type": "add_shot", "after": 0, "idea": "legacy"}),
        ];
        let (out, warnings) = validate_chat_actions(raw, &t, ChatIdSpace::Clip);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0]["shot"], json!(ids[1]));
        assert_eq!(out[0]["value"], json!("night"));
        assert_eq!(out[1]["shot"], json!(ids[1]));
        assert_eq!(out[2]["type"], json!("set_story"));
        assert_eq!(out[3]["after"], json!(AT_START));
        assert_eq!(out[4]["after"], json!(ids[2])); // resolved to a clip id, not left as a token
        assert_eq!(warnings.len(), 3);
    }

    #[test]
    fn closes_scene_tracks_the_next_shot_not_this_one() {
        let shot = |mode: &str, new_scene: Option<bool>| ShotIn {
            prompt: "x".into(),
            mode: Some(mode.into()),
            new_scene,
            ..Default::default()
        };
        let owned = [
            shot("t2v", None),
            shot("t2v", Some(true)), // hard cut ⇒ shot 0 closes
            shot("i2v", None),       // fresh from its own frame ⇒ shot 1 closes
            shot("t2v", None),       // plain continuation ⇒ shot 2 closes anyway (i2v starts fresh)
        ];
        let shots: Vec<&ShotIn> = owned.iter().collect();
        assert!(closes_scene(&shots, 0, true)); // next is a hard cut
        assert!(closes_scene(&shots, 1, true)); // next is i2v — starts fresh
        assert!(!closes_scene(&shots, 2, true)); // next is a plain t2v continuation
        // The final shot's answer is the caller's: a whole timeline ends, /extend continues.
        assert!(closes_scene(&shots, 3, true));
        assert!(!closes_scene(&shots, 3, false));
    }

    #[test]
    fn number_shots_carries_ids_and_pacing() {
        let (t, _) = tokens4();
        let owned = [
            ShotIn {
                id: "a".into(),
                prompt: "alpha".into(),
                seconds: Some(6.0),
                ..Default::default()
            },
            ShotIn {
                id: "b".into(),
                prompt: "beta".into(),
                seconds: Some(3.0),
                new_scene: Some(true),
                ..Default::default()
            },
        ];
        let shots: Vec<&ShotIn> = owned.iter().collect();
        let listed = number_shots_for(ENGINE_LTX25, &shots, Some(&t), true);
        // Shot 1 is followed by a hard cut ⇒ it closes; shot 2 is last ⇒ it closes too.
        assert!(listed.contains(&format!(
            "Shot 1 (id {}) [t2v · 6s · CLOSES]: alpha",
            t.token(0)
        )));
        assert!(listed.contains(&format!(
            "Shot 2 (id {}) [t2v · 3s · CLOSES]: beta",
            t.token(1)
        )));
        // Context-only paths list no ids to echo.
        let plain = number_shots_for(ENGINE_LTX25, &shots, None, false);
        assert!(plain.contains("Shot 1 [t2v · 6s · CLOSES]: alpha"));
        assert!(plain.contains("Shot 2 [t2v · 3s · CONTINUES]: beta"));
        assert!(!plain.contains("(id"));
    }

    #[test]
    fn number_shots_tags_plain_continuation_as_carried() {
        // Shot 0 opens (t2v); shot 1 is a plain t2v continuation (renders from shot 0's final
        // frame); shot 2 cuts fresh from its own image (i2v). Only shot 1 is a start-continuation,
        // so only it is retagged `carried` — the mislabel that made the chat/rework batch tell a
        // continuation to build the scene from scratch.
        let owned = [
            ShotIn {
                id: "a".into(),
                prompt: "open".into(),
                seconds: Some(4.0),
                ..Default::default()
            },
            ShotIn {
                id: "b".into(),
                prompt: "middle".into(),
                seconds: Some(4.0),
                ..Default::default()
            },
            ShotIn {
                id: "c".into(),
                prompt: "cut".into(),
                seconds: Some(4.0),
                mode: Some("i2v".into()),
                ..Default::default()
            },
        ];
        let shots: Vec<&ShotIn> = owned.iter().collect();
        let listed = number_shots_for(ENGINE_LTX25, &shots, None, true);
        assert!(listed.contains("Shot 1 [t2v · 4s · CONTINUES]: open"));
        assert!(listed.contains("Shot 2 [carried · 4s · CLOSES]: middle"));
        assert!(listed.contains("Shot 3 [i2v · 4s · CLOSES]: cut"));
        // The old bare `cont` token leaked onto drafted prompts as a "cont'd:" prefix.
        assert!(!listed.contains("[cont "));
    }

    #[test]
    fn partition_ranges_tiles_and_balances() {
        // 10 words into 3 windows → contiguous, sizes 4/3/3, covering [0,10).
        let r = partition_ranges(10, 3);
        assert_eq!(r, vec![(0, 4), (4, 7), (7, 10)]);
        // Windows tile the whole range with no gaps/overlap.
        assert_eq!(r.first().unwrap().0, 0);
        assert_eq!(r.last().unwrap().1, 10);
        for w in r.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
        // More windows than words clamps to one window per word.
        assert_eq!(partition_ranges(2, 8), vec![(0, 1), (1, 2)]);
        assert!(partition_ranges(0, 4).is_empty());
    }

    #[test]
    fn lyrics_transcript_range_uses_global_indices() {
        let words = [
            word("I", 0.0),
            word("was", 0.4),
            word("here", 0.9),
            word("now", 1.5),
        ];
        // A mid-song window keeps GLOBAL indices so cut points stay valid across chunks.
        let slice = lyrics_transcript_range(&words, 2, 4);
        assert!(slice.contains("[2 @0.90s] here"));
        assert!(slice.contains("[3 @1.50s] now"));
        assert!(!slice.contains("[0 "));
        assert!(!slice.contains("was"));
        // Out-of-range end is clamped, not panicked.
        assert_eq!(
            lyrics_transcript_range(&words, 3, 99),
            "[3 @1.50s] now".to_string()
        );
    }

    // ─── H3 generation packing ───────────────────────────────────────────────

    fn clip(prompt: &str, seconds: f64) -> H3TimelineClip {
        H3TimelineClip {
            prompt: prompt.to_string(),
            seconds: Some(seconds),
            ..Default::default()
        }
    }

    /// The core of the reframe: shots go INTO one generation until the budget says otherwise. A
    /// cut inside a generation is free and carries `<scenetrans>` audio continuity; a boundary
    /// between generations is a seam that carries neither. So four 3-second shots are ONE render,
    /// not four.
    #[test]
    fn shots_pack_into_one_generation_until_the_budget_says_otherwise() {
        let clips = [
            clip("a", 3.0),
            clip("b", 3.0),
            clip("c", 3.0),
            clip("d", 3.0),
        ];
        let p = pack_clips_into_generations(&clips, 5.0, koblem_client::types::H3_MAX_FRAMES)
            .expect("packs");
        // 12 s fits inside 15.083 s, so it is one render with three cuts.
        assert_eq!(p.generations.len(), 1);
        assert_eq!(p.shot_to_generation, vec![0, 0, 0, 0]);
        let g = &p.generations[0];
        // 12 s = 288 raw frames → 5 + 17*17 = 294.
        assert_eq!(g["frames"], json!(294));
        let shots = g["shots"].as_array().unwrap();
        assert_eq!(shots.len(), 4);
        // `[Shot 1]` is untimed; the rest carry ABSOLUTE times from the generation's start.
        assert!(shots[0].get("at_seconds").is_none());
        assert_eq!(shots[1]["at_seconds"], json!(3.0));
        assert_eq!(shots[3]["at_seconds"], json!(9.0));
        // …and the prompt carries the markers that ARE the shot structure.
        let prompt = g["prompt"].as_str().unwrap();
        assert!(prompt.starts_with("[Shot 1]\n"), "{prompt}");
        assert!(prompt.contains("[Shot 2] At 00:03.000"), "{prompt}");
        assert!(prompt.contains("[Shot 4] At 00:09.000"), "{prompt}");
    }

    /// Overflow is the ONLY automatic split. Five 4-second shots are 20 s, which no single
    /// generation can render, so the fifth opens a new one — and its cut times restart at zero,
    /// because they are relative to the generation and not to the timeline.
    #[test]
    fn a_generation_closes_when_the_next_shot_would_overflow_the_budget() {
        let clips: Vec<H3TimelineClip> = (0..5).map(|i| clip(&format!("s{i}"), 4.0)).collect();
        let p = pack_clips_into_generations(&clips, 5.0, koblem_client::types::H3_MAX_FRAMES)
            .expect("packs");
        assert_eq!(p.generations.len(), 2);
        // 3 x 4 s = 12 s fits; the fourth would make 16 s > 15.083 s.
        assert_eq!(p.shot_to_generation, vec![0, 0, 0, 1, 1]);
        let second = &p.generations[1];
        let shots = second["shots"].as_array().unwrap();
        assert!(shots[0].get("at_seconds").is_none());
        assert_eq!(shots[1]["at_seconds"], json!(4.0));
    }

    /// 🔴 The split flag is `h3_break_before`, NOT `new_scene`. `new_scene` means "this is a
    /// cut", and a cut is free INSIDE a generation and better there — splitting on it would be the
    /// chaining-for-a-cut mistake, paid once per cut.
    #[test]
    fn only_an_explicit_new_generation_flag_splits_early() {
        let mut clips = [clip("a", 3.0), clip("b", 3.0), clip("c", 3.0)];
        clips[1].h3_break_before = Some(true);
        let p = pack_clips_into_generations(&clips, 5.0, koblem_client::types::H3_MAX_FRAMES)
            .expect("packs");
        assert_eq!(p.generations.len(), 2);
        assert_eq!(p.shot_to_generation, vec![0, 1, 1]);
        // The one that asked to split opens its generation, untimed.
        assert!(
            p.generations[1]["shots"][0].get("at_seconds").is_none(),
            "a generation's first shot is always untimed"
        );
    }

    /// A single-shot generation carries NO `[Shot 1]` marker: that is the plain request the model
    /// was trained on, and prefixing a marker changes text the model reads.
    #[test]
    fn a_single_shot_generation_carries_no_marker() {
        let clips = [clip("a lone wide shot", 6.0)];
        let p = pack_clips_into_generations(&clips, 5.0, koblem_client::types::H3_MAX_FRAMES)
            .expect("packs");
        assert_eq!(p.generations[0]["prompt"], json!("a lone wide shot"));
        assert_eq!(p.generations[0]["shots"].as_array().unwrap().len(), 1);
    }

    /// 🔴 A SHORT TIMELINE IS NO LONGER LENGTHENED. A 1-second clip packs to 1.04 s (25 frames on
    /// the `5 + 17k` grid), not to the old 4.458 s: the floor here is the VAE's decodable minimum
    /// (22), and MiniMax's documented 4 s is a guideline `build_chain` REPORTS rather than a
    /// number this rewrites the author's intent to.
    ///
    /// The only clamping left is the grid itself, which is model law.
    #[test]
    fn a_short_timeline_keeps_its_authored_length() {
        use koblem_client::types::H3_MIN_FRAMES;
        let clips = [clip("a", 1.0)];
        let p = pack_clips_into_generations(&clips, 5.0, koblem_client::types::H3_MAX_FRAMES)
            .expect("packs");
        // 24 frames snaps UP one grid step to 39; it is NOT pushed to 107.
        assert_eq!(p.generations[0]["frames"], json!(39));

        // …and below the decodable floor there genuinely is no render to offer, so 22 stands.
        let tiny = [clip("a", 0.05)];
        let p = pack_clips_into_generations(&tiny, 5.0, koblem_client::types::H3_MAX_FRAMES)
            .expect("packs");
        assert_eq!(p.generations[0]["frames"], json!(H3_MIN_FRAMES));
    }

    /// A shot longer than one generation cannot be packed at all, and splitting it across a
    /// boundary would put a seam in the middle of a continuous shot. Refused, with the two ways
    /// out named.
    #[test]
    fn a_shot_longer_than_one_generation_is_refused_rather_than_split() {
        let clips = [clip("a very long take", 20.0)];
        let e = pack_clips_into_generations(&clips, 5.0, koblem_client::types::H3_MAX_FRAMES)
            .expect_err("20 s does not fit");
        assert!(e.contains("shot 1"), "{e}");
        assert!(e.contains("15.083"), "{e}");

        // Empty-prompt clips are filtered, and nothing left is an error rather than a zero-
        // generation job the engine would refuse.
        let empty = [clip("   ", 5.0)];
        assert!(
            pack_clips_into_generations(&empty, 5.0, koblem_client::types::H3_MAX_FRAMES).is_err()
        );
    }

    /// The default shot length fills in for a clip that never recorded one, so a doc written by an
    /// older editor packs as the editor would draw it.
    /// 🔴 Staleness travels FORWARD across an anchored seam, and stops at a hard cut.
    ///
    /// Generation `k+1` continues from `k`'s banked latents, so re-rendering `k` leaves `k+1`
    /// joined to a predecessor that no longer exists anywhere in the timeline. Without propagation
    /// a continue would produce a plausible cut with a broken seam in it — the failure mode that
    /// makes "only render what changed" dangerous rather than merely incomplete.
    #[test]
    fn staleness_propagates_along_anchored_seams_only() {
        let shot = |p: &str, anchored: bool| (p.to_string(), anchored);
        let took = |p: &str| Some(p.to_string());

        // Shot 0's prompt changed. 1 anchors to it, so it goes too. 2 is a HARD CUT, so it stops.
        let (marks, span) = h3_staleness(
            &[shot("NEW words", false), shot("b", true), shot("c", false)],
            &[took("old words"), took("b"), took("c")],
        );
        assert_eq!(
            marks,
            vec![
                Some(H3Staleness::PromptChanged),
                Some(H3Staleness::ContinuesFromStale),
                None
            ]
        );
        assert_eq!(span, Some((0, 1)), "the hard cut keeps shot 2 out of it");

        // Nothing changed: no span, and a continue does nothing at all.
        let (marks, span) = h3_staleness(
            &[shot("a", false), shot("b", true)],
            &[took("a"), took("b")],
        );
        assert_eq!(marks, vec![None, None]);
        assert_eq!(span, None);

        // A shot appended to a rendered timeline is the everyday case: one generation, at the end.
        let (_, span) = h3_staleness(
            &[shot("a", false), shot("b", true), shot("c", true)],
            &[took("a"), took("b"), None],
        );
        assert_eq!(span, Some((2, 2)));

        // Whitespace is not an edit — the harvest stores the prompt verbatim and the compiler is
        // free to re-wrap it, so comparing untrimmed would re-render the whole timeline for nothing.
        let (marks, _) = h3_staleness(&[shot("  a\n", false)], &[took("a")]);
        assert_eq!(marks, vec![None]);
    }

    /// 🔴 A healthy generation BETWEEN two stale ones is re-rendered, and says so as its own reason.
    ///
    /// The span is dispatched as one job so that `"prev"` keeps meaning the generation before it;
    /// the cost is this collateral, and it must be visible rather than filed as "changed".
    #[test]
    fn a_healthy_generation_inside_the_span_is_labelled_as_collateral() {
        let shot = |p: &str| (p.to_string(), false);
        let took = |p: &str| Some(p.to_string());
        // 0 and 2 are stale; 1 is fine, and every seam is a hard cut so nothing propagates.
        let (marks, span) = h3_staleness(
            &[shot("NEW a"), shot("b"), shot("NEW c")],
            &[took("a"), took("b"), took("c")],
        );
        assert_eq!(
            marks,
            vec![
                Some(H3Staleness::PromptChanged),
                Some(H3Staleness::InsideTheSpan),
                Some(H3Staleness::PromptChanged)
            ]
        );
        assert_eq!(span, Some((0, 2)));
    }

    /// 🔴 Song offsets follow the generations that actually RENDER.
    ///
    /// `build_chain` drops empty-prompt generations and the engine numbers its partials in what
    /// survives, so a compiled-space list handed straight to the job would be off by one per
    /// dropped generation — every shot after it muxed with the wrong span of the song, silently,
    /// since a wrong offset still produces music.
    #[test]
    fn song_offsets_are_narrowed_to_the_generations_that_render() {
        let shot = |prompt: &str| crate::routes::minimax_h3::H3Generation {
            prompt: prompt.to_string(),
            ..Default::default()
        };
        let generations = [shot("shot one"), shot("   "), shot("shot three")];
        let compiled = [0.0, 14.333, 28.666];
        assert_eq!(
            h3_offsets_in_rendered_space(&generations, &compiled),
            vec![0.0, 28.666],
            "the blank generation's offset goes with it"
        );
        // No soundtrack: nothing to narrow, and the empty vec is the "do not mux" signal.
        assert!(h3_offsets_in_rendered_space(&generations, &[]).is_empty());
    }

    /// 🔴 A project with a song is held to the SHORTER ceiling on BOTH packing paths.
    ///
    /// WanGP locks an audio reference at 15 s; 362 frames is 15.083 s, so a generation packed to
    /// the bare ceiling has audio for part of its length and H3 invents the rest. An authored
    /// 14.4 s is 345.6 frames, which rounds to 346 — one frame over — and the grid snap rounds UP,
    /// so it landed on 362 and bought 0.7 s of invented song. Clamping has to happen BEFORE the
    /// snap, which is what this pins.
    #[test]
    fn a_soundtrack_holds_both_packers_to_the_audio_ceiling() {
        use koblem_client::types::{H3_MAX_FRAMES, H3_MAX_FRAMES_WITH_AUDIO};
        let song = serde_json::json!({
            "request": { "soundtrack": { "audio": "asset:deadbeef", "mode": "replace" } }
        });
        let silent = serde_json::json!({ "request": {} });
        assert_eq!(h3_frame_ceiling(&song), H3_MAX_FRAMES_WITH_AUDIO);
        assert_eq!(h3_frame_ceiling(&silent), H3_MAX_FRAMES);
        // An empty audio field is not a soundtrack.
        assert_eq!(
            h3_frame_ceiling(&serde_json::json!({"request":{"soundtrack":{"audio":"  "}}})),
            H3_MAX_FRAMES
        );

        // 14.4 s: the exact length that rounded to 346 and snapped to 362.
        let clips = vec![H3TimelineClip {
            prompt: "a shot".into(),
            seconds: Some(14.4),
            ..H3TimelineClip::default()
        }];
        let packed = pack_clips_into_generations(&clips, 5.0, H3_MAX_FRAMES_WITH_AUDIO)
            .expect("packs under the audio ceiling");
        assert_eq!(
            packed.generations[0]["frames"], 345,
            "clamped before the snap, not after: {:?}",
            packed.generations[0]
        );
        // Without a song the same clip keeps its authored length, snapped up as before.
        let bare = pack_clips_into_generations(&clips, 5.0, H3_MAX_FRAMES).expect("packs");
        assert_eq!(bare.generations[0]["frames"], 362);
    }

    #[test]
    fn a_clip_without_a_duration_takes_the_project_default() {
        let clips = [H3TimelineClip {
            prompt: "a".to_string(),
            ..Default::default()
        }];
        let p = pack_clips_into_generations(&clips, 8.0, koblem_client::types::H3_MAX_FRAMES)
            .expect("packs");
        // 8 s = 192 raw frames → 5 + 17*11 = 192 exactly.
        assert_eq!(p.generations[0]["frames"], json!(192));
    }

    /// Two engines, two containers — and webm cannot carry HEVC, so this is what stops
    /// `rebuild_cut` handing ffmpeg a mux it must refuse.
    #[test]
    fn the_take_container_follows_the_recorded_mime() {
        assert_eq!(container_ext("video/mp4"), "mp4");
        assert_eq!(container_ext("video/webm"), "webm");
        // Anything unrecognised reads as webm, which is what every take written before H3 is.
        assert_eq!(container_ext(""), "webm");
    }

    /// The shape of the project both regressions were measured on: three authored generations over
    /// clips of 14.4 / 14.0 / 14.0 s, one clip each, with a project soundtrack attached.
    fn three_shot_timeline() -> (Value, Vec<H3TimelineClip>) {
        let timeline = json!({
            // 🔴 `h3_frame_ceiling` reads the DELIVERY block, so this is what makes the ceiling
            // 345 rather than 362 — and 345 is the only reason an authored 14.4 s lands on the
            // grid at all.
            "request": { "soundtrack": { "mode": "replace", "audio": "asset:deadbeef" } },
            "h3_soundtrack_mode": "replace",
            "h3_references": [{ "id": "snd", "kind": "audio", "role": "audio_reuse",
                                "audio": "soundtrack:" }],
            "h3_generations": [
                { "id": "g0", "clip_ids": ["a"], "prompt": "block a" },
                { "id": "g1", "clip_ids": ["b"], "prompt": "block b" },
                { "id": "g2", "clip_ids": ["c"], "prompt": "block c" },
            ],
            "clips": [
                { "id": "a", "prompt": "a", "seconds": 14.4 },
                { "id": "b", "prompt": "b", "seconds": 14.0 },
                { "id": "c", "prompt": "c", "seconds": 14.0 },
            ],
        });
        let clips: Vec<H3TimelineClip> =
            serde_json::from_value(timeline["clips"].clone()).expect("clips");
        (timeline, clips)
    }

    /// 🔴 `frames` is READ as the rendered length before anything snaps it —
    /// `h3_soundtrack::generation_shape` lays every audio slice from this number. Emitting the
    /// unsnapped value put an authored 14.0 s on the wire as 336 while the render produced 345, so
    /// the next generation's slice started 0.375 s early, per generation, accumulating.
    ///
    /// Measured 2026-08-10 on `11599681-…`: koblem logged `42.38s delivered` for a cut the editor
    /// and the engine both make 43.042 s.
    #[test]
    fn authored_frames_land_on_the_grid_so_the_soundtrack_plan_matches_the_render() {
        let (timeline, clips) = three_shot_timeline();
        let packed = authored_generations(&timeline, &clips).expect("packed");
        let frames: Vec<u64> = packed
            .generations
            .iter()
            .map(|g| g["frames"].as_u64().expect("frames"))
            .collect();
        // 14.0 s rounds to 336, which is OFF the 5+17k grid and snapped up to 345 by the engine.
        // 14.4 s rounds to 346, one past the 345 audio ceiling, and clamps down onto it.
        assert_eq!(frames, vec![345, 345, 345]);
        for f in frames {
            assert_eq!(
                h3_snap_frames(f as u32),
                f as u32,
                "every emitted frame count must already be on the grid"
            );
        }
    }

    /// 🔴 The editor gives every generation but the first a continuation and never persists it, so
    /// an absent key means "the default" and not "no join". Reading it as "no join" made every
    /// generation past the first render as a fresh, unanchored start — silently, and it left the
    /// whole `continuation.from = "prev"` resolution in `h3_single_generation_chain` unreachable.
    #[test]
    fn every_generation_but_the_first_continues_from_prev() {
        let (timeline, clips) = three_shot_timeline();
        let packed = authored_generations(&timeline, &clips).expect("packed");

        assert!(
            packed.generations[0].get("continuation").is_none(),
            "nothing precedes the first generation"
        );
        for g in &packed.generations[1..] {
            let c: H3Continuation =
                serde_json::from_value(g["continuation"].clone()).expect("continuation");
            // Serde fills the rest, which is what keeps these numbers defined in ONE place
            // (koblibs) rather than restated here and in `h3-generations.ts`.
            assert_eq!(c.mode, H3ContinuationMode::Latents);
            assert_eq!(c.from, H3_CONTINUATION_FROM_PREV);
            assert_eq!(c.k, 7);
            assert!(c.anchor, "an unanchored seam reproduces no frame to trim");
        }
    }

    /// An authored block wins outright — a hard cut is `mode: "none"`, which is a decision and not
    /// an absence, and the default must not overwrite it.
    #[test]
    fn an_authored_continuation_is_forwarded_verbatim() {
        let (mut timeline, clips) = three_shot_timeline();
        timeline["h3_generations"][1]["continuation"] = json!({ "mode": "none" });
        timeline["h3_generations"][2]["continuation"] = json!({ "mode": "frame", "k": 12 });
        let packed = authored_generations(&timeline, &clips).expect("packed");

        let cut: H3Continuation =
            serde_json::from_value(packed.generations[1]["continuation"].clone()).expect("cut");
        assert_eq!(cut.mode, H3ContinuationMode::None);

        let framed: H3Continuation =
            serde_json::from_value(packed.generations[2]["continuation"].clone()).expect("framed");
        assert_eq!(framed.mode, H3ContinuationMode::Frame);
        assert_eq!(framed.k, 12);
        // Unstated fields still fall to the shared defaults rather than to zero.
        assert_eq!(framed.from, H3_CONTINUATION_FROM_PREV);
        assert!(framed.anchor);
    }

    /// The two fixes only pay off together: the slice plan reads `frames` AND the seam flag, and
    /// getting either wrong moves every later generation's audio. This is the arithmetic the
    /// editor's `planSoundtrack` performs on the same document, so agreeing here is the property.
    #[test]
    fn the_soundtrack_plan_now_agrees_with_the_editor() {
        use crate::routes::h3_soundtrack::generation_shape;
        use koblem_client::types::h3_plan_soundtrack;
        let (timeline, clips) = three_shot_timeline();
        let mut packed = authored_generations(&timeline, &clips).expect("packed");
        // The soundtrack reference is what puts a generation in scope for a slice; the render path
        // attaches it from `h3_references` before the plan runs.
        for g in &mut packed.generations {
            g.as_object_mut().unwrap().insert(
                "references".into(),
                json!([{ "kind": "audio", "role": "audio_reuse", "audio": "soundtrack:" }]),
            );
        }
        // 🔴 Planned through the REAL reader, so this asserts on what actually consumes `frames`
        // and `continuation` rather than on a restatement of it.
        let plan = h3_plan_soundtrack(&generation_shape(&packed.generations), 208.008);

        // 🔴 UPDATED 2026-08-14 with `H3_TRIM_JOIN` 1 -> 22 (comfy-api's continuation now
        // defaults to `placement="on_target"`, which trims `frames_for_latents(7)` head
        // frames instead of the single `frame0_anchor` frame). 345 + 323 + 323 delivered
        // = 41.292 s. The numbers below are the OLD trim's, kept in this comment because
        // the shape of the regression this test was written for has not changed: a wrong
        // trim moves every offset after the first and nothing fails.
        //   trim=1  -> 43.042 s, offsets 0 / 14.333 / 28.667
        //   (the original regression planned 42.375 s, offsets 0 / 14.375 / 28.375)
        assert!((plan.delivered_seconds - 41.291_666).abs() < 1e-4);
        let offsets: Vec<f64> = plan.slices.iter().map(|s| s.song_offset_seconds).collect();
        assert!((offsets[0] - 0.0).abs() < 1e-6);
        // Each offset reaches BACK by the trim: the model re-renders those frames and
        // needs their audio, even though they never reach the cut.
        assert!((offsets[1] - 13.458_333).abs() < 1e-4);
        assert!((offsets[2] - 26.916_666).abs() < 1e-4);
    }
}
