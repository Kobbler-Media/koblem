//! `/api/v1/queue` — the queue's own surface.
//!
//! The render routes keep their own paths and their own bodies; this is the index
//! above them. [`enqueue`] is the shared entry every ported render route calls.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use koblem_client::types::{
    EnqueueRequest, EnqueueResponse, Job, JobKind, JobState, QueueSnapshot, ReorderRequest,
    SubmitMode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::AppState;
use crate::auth::{Principal, require_auth};
use crate::queue::estimate::{Estimator, replay};
use crate::queue::lane::Lane;
use crate::queue::runner::Interrupt;
use crate::repositories::{gpu_job_stats, gpu_jobs as store};

/// How long a job blocked on an operator-clearable reason waits before the queue
/// calls it stuck. Long enough that a busy evening never trips it, short enough
/// that a night is not lost.
const STUCK_AFTER_SECS: i64 = 30 * 60;

/// Backstop dedupe window for callers that mint no idempotency key. The UI and the
/// CLI both do, and the unique index is what actually makes a double-click safe;
/// this only catches a hand-rolled caller.
const DEDUPE_WINDOW_SECS: i64 = 30;

/// How many jobs the ETA replay assumes can run at once. Derived from the enabled
/// card count: one card means one render at a time however many lanes are free.
fn concurrency(state: &AppState) -> usize {
    state
        .gpu
        .occupancy()
        .iter()
        .filter(|c| c.enabled)
        .count()
        .max(1)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/queue", get(snapshot).post(enqueue_route))
        .route("/api/v1/queue/events", get(events))
        .route("/api/v1/queue/stats", get(stats))
        .route("/api/v1/queue/pause", post(pause))
        .route("/api/v1/queue/jobs/{id}", get(one).delete(cancel))
        .route("/api/v1/queue/jobs/{id}/reorder", post(reorder))
        .route("/api/v1/queue/jobs/{id}/hold", post(hold))
        .route("/api/v1/queue/jobs/{id}/preempt", post(preempt))
        .route("/api/v1/queue/jobs/{id}/retry", post(retry))
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// The caller, or the response that refuses them. Public so a ported render route
/// can reuse exactly this gate before enqueueing.
pub(crate) async fn principal_of(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Principal, Response> {
    principal(state, headers).await
}

async fn principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .map_err(|c| c.into_response())
}

/// Decorate rows with everything the API promises but the table does not store:
/// the human label, and the simulated start/finish.
async fn decorate(state: &AppState, mut jobs: Vec<Job>, live: &[Job]) -> Vec<Job> {
    for job in &mut jobs {
        job.label = label_for(state, job).await;
    }
    schedule(state, &mut jobs, live).await;
    jobs
}

/// Stamp the simulated schedule onto rows.
///
/// 🔴 Replay the dispatcher rather than summing durations: parallelism across
/// cards and the comfy-api single slot are exactly what decide when the batch
/// finishes.
async fn schedule(state: &AppState, jobs: &mut [Job], live: &[Job]) {
    let stats = gpu_job_stats::all(&state.db).await.unwrap_or_default();
    let estimator = Estimator::new(stats);
    for job in jobs.iter_mut() {
        job.estimate_samples = estimator.whole(job).samples;
    }

    let now = time::OffsetDateTime::now_utc();
    let live_with_elapsed: Vec<(Job, i64)> = live
        .iter()
        .map(|j| {
            let started = j
                .started_at
                .as_deref()
                .and_then(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .ok()
                })
                .map(|t| (now - t).whole_milliseconds() as i64)
                .unwrap_or(0);
            (j.clone(), started.max(0))
        })
        .collect();

    let slots = replay(&estimator, &live_with_elapsed, jobs, concurrency(state));
    for slot in slots {
        if let Some(job) = jobs.iter_mut().find(|j| j.id == slot.job_id) {
            // An estimate built from nothing is not published as a time. Handing
            // back "starts ~02:10" off zero samples is precisely the confident-
            // looking guess this design refuses to make.
            if slot.samples == 0 {
                continue;
            }
            job.eta_start = slot.start_ms.and_then(|ms| stamp(now, ms));
            job.eta_finish = stamp(now, slot.finish_ms);
            job.eta_finish_p90 = stamp(now, slot.finish_p90_ms);
        }
    }
}

fn stamp(now: time::OffsetDateTime, offset_ms: i64) -> Option<String> {
    (now + time::Duration::milliseconds(offset_ms))
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

async fn label_for(state: &AppState, job: &Job) -> String {
    let Some(dispatcher) = state.queue.get() else {
        return job.kind.as_str().to_string();
    };
    let Some(runner) = dispatcher.runner(job.kind) else {
        return job.kind.as_str().to_string();
    };
    match store::payload(&state.db, &job.id).await {
        Ok(Some((payload, _))) => runner.label(&payload),
        _ => job.kind.as_str().to_string(),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    /// Include terminal rows (the history tail). Default: pending + live only.
    #[serde(default)]
    history: bool,
    #[serde(default)]
    limit: Option<i64>,
}

async fn snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    let live = store::live(&state.db).await.unwrap_or_default();
    let pending = store::pending(&state.db).await.unwrap_or_default();

    let mut jobs = if q.history {
        store::recent(&state.db, q.limit.unwrap_or(200).clamp(1, 500))
            .await
            .unwrap_or_default()
    } else {
        let mut v = live.clone();
        v.extend(pending.clone());
        v
    };
    jobs = decorate(&state, jobs, &live).await;

    let stuck = store::stuck(&state.db, STUCK_AFTER_SECS)
        .await
        .unwrap_or_default()
        .into_iter()
        // Only what an operator can actually do something about. A busy comfy lane
        // at 03:00 is the queue working correctly; a service left disabled at 22:00
        // is a lost night.
        .filter(|j| {
            j.blocked_reason
                .as_ref()
                .is_some_and(|r| r.is_operator_clearable())
        })
        .map(|j| j.id)
        .collect();

    let eta_finish = jobs.iter().filter_map(|j| j.eta_finish.clone()).max();

    Json(QueueSnapshot {
        paused: state.queue.get().is_some_and(|d| d.paused()),
        pending: pending.len() as i64,
        running: live.len() as i64,
        eta_finish,
        stuck,
        jobs,
        // 🔴 A mutex read, nothing more — deliberately cheap enough to ride the
        // 10 s snapshot. Beefy mode has to be visible from EVERY page (a switch
        // left on overnight is the same lost night as a service left disabled),
        // and every page already holds this stream open for the topbar
        // indicator. Carrying it here buys the whole-app banner for zero new
        // requests; a `/api/v1/gpu/beefy` poll would have been a second timer on
        // every screen for a bit that changes twice a day.
        box_locked: state.gpu.exclusive().map(|k| k.as_str().to_string()),
    })
    .into_response()
}

async fn one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    match store::get(&state.db, &id).await {
        Ok(Some(job)) => {
            // 🔴 The replay runs over the WHOLE queue, not this row alone: when a
            // job starts is decided by everything in front of it, and a one-row
            // replay would answer "right now" for every job in the band.
            let live = store::live(&state.db).await.unwrap_or_default();
            let mut all = live.clone();
            all.extend(store::pending(&state.db).await.unwrap_or_default());
            if !all.iter().any(|j| j.id == id) {
                all.push(job);
            }
            schedule(&state, &mut all, &live).await;
            match all.into_iter().find(|j| j.id == id) {
                Some(mut j) => {
                    j.label = label_for(&state, &j).await;
                    Json(j).into_response()
                }
                None => err(StatusCode::NOT_FOUND, "no such job"),
            }
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "no such job"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn enqueue_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EnqueueRequest>,
) -> Response {
    let who = match principal(&state, &headers).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    match enqueue(&state, &who, req).await {
        Ok(resp) => (StatusCode::ACCEPTED, Json(resp)).into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

/// The queue knobs a caller can set on ANY render route, read off headers.
///
/// 🔴 Headers, not body fields, for two reasons. Half these routes take multipart
/// bodies, where a JSON field means touching the request type; and every JSON one
/// parses through `strict_json::from_value_strict`, which **rejects unknown
/// fields** — so a client sending `"mode": "now"` in the body would get a 400
/// naming it. One mechanism that works on all twelve.
///
/// Everything here is optional and absence means "the default for this caller",
/// which is what `enqueue` computes from the principal.
#[derive(Debug, Clone, Default)]
pub(crate) struct QueueOptions {
    pub mode: Option<SubmitMode>,
    pub band: Option<i16>,
    pub target_gpu: Option<String>,
    pub force_pin: bool,
    pub depends_on: Option<String>,
    pub idempotency_key: Option<String>,
}

impl QueueOptions {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        QueueOptions {
            // 🔴 An explicit mode ALWAYS wins over the principal default. The CLI
            // authenticates with an API key, whose default is `now` — the opposite
            // of what a CLI is for — so without this override "queue five renders
            // overnight" cannot be expressed at all.
            mode: get("x-queue-mode").and_then(|v| match v.as_str() {
                "queue" => Some(SubmitMode::Queue),
                "now" => Some(SubmitMode::Now),
                _ => None,
            }),
            band: get("x-queue-band").and_then(|v| v.parse().ok()),
            target_gpu: get("x-queue-gpu"),
            force_pin: get("x-queue-force").is_some_and(|v| v == "1" || v == "true"),
            depends_on: get("x-queue-after"),
            idempotency_key: get("idempotency-key"),
        }
    }

    /// Fold into an enqueue for `kind` with this payload.
    pub(crate) fn request(self, kind: JobKind, payload: Value) -> EnqueueRequest {
        EnqueueRequest {
            kind,
            payload,
            mode: self.mode,
            band: self.band,
            target_gpu: self.target_gpu,
            force_pin: self.force_pin,
            depends_on: self.depends_on,
            idempotency_key: self.idempotency_key,
        }
    }
}

/// The shared enqueue every ported render route calls.
///
/// 🔴 Validation happens HERE, before the row exists. The queue stretches the
/// feedback loop from minutes to hours: a request enqueued at 22:00 that 400s at
/// 03:00 is a night of nothing, and the user was looking at the form when it could
/// have been caught. A job that cannot possibly succeed must never enter the queue.
pub(crate) async fn enqueue(
    state: &AppState,
    who: &Principal,
    req: EnqueueRequest,
) -> Result<EnqueueResponse, (StatusCode, String)> {
    let Some(dispatcher) = state.queue.get() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the queue dispatcher is not running on this process".to_string(),
        ));
    };
    let Some(runner) = dispatcher.runner(req.kind) else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            format!("{} has no adapter yet", req.kind.as_str()),
        ));
    };
    runner
        .validate(&req.payload)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    // 🔴 Refused, not ignored. comfy-api takes no per-request `gpu` — it is one
    // process in front of one ComfyUI on whatever card the container was given —
    // so honouring a pin here would move the gate's RESERVATION to the pinned card
    // while the render carried on where it always does, and the occupancy ledger
    // would then be describing a card that is not the busy one.
    if req.target_gpu.is_some() && !Lane::of(req.kind).steers_by_request() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "{} runs on comfy-api, which takes no per-request card — it renders on whatever \
                 GPU its container was given, so a pin here could only make the scheduler's \
                 occupancy wrong. Move the container instead.",
                req.kind.as_str()
            ),
        ));
    }

    if let Some(parent) = req.depends_on.as_deref()
        && store::get(&state.db, parent).await.ok().flatten().is_none()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("no such job to depend on: {parent}"),
        ));
    }

    // Default by principal, override by request. The CLI authenticates with an API
    // key and would otherwise be forced into `now` — the opposite of what it is for.
    let mode = req.mode.unwrap_or(if who.queues_by_default() {
        SubmitMode::Queue
    } else {
        SubmitMode::Now
    });
    // 🔴 An immediate job jumps the ORDERING, not the SCHEDULER. It still gets a
    // row and still goes through admission — bypassing the dispatcher would take
    // VRAM the dispatcher believed it had. And it does NOT preempt: it waits for
    // the running job exactly as the gate would make it wait today.
    let band = req.band.unwrap_or(match mode {
        SubmitMode::Now => 0,
        SubmitMode::Queue => 1,
    });

    if req.idempotency_key.is_none()
        && let Ok(Some(dup)) =
            store::recent_duplicate(&state.db, req.kind, &req.payload, DEDUPE_WINDOW_SECS).await
    {
        let (position, eta_start) = receipt(state, &dup.id).await;
        return Ok(EnqueueResponse {
            position,
            job_id: dup.id,
            state: dup.state,
            eta_start,
            blocked_reason: dup.blocked_reason,
            deduplicated: true,
        });
    }

    let new = store::NewJob {
        id: job_id(req.kind, &req.payload),
        kind: req.kind,
        band,
        target_gpu: req.target_gpu,
        force_pin: req.force_pin,
        depends_on: req.depends_on,
        created_by: who.label(),
        idempotency_key: req.idempotency_key,
        units_total: runner.units(&req.payload),
        shape_key: Some(runner.shape_key(&req.payload)),
        state: JobState::Queued,
        payload: req.payload,
    };

    let (job, deduplicated) = store::enqueue(&state.db, &new)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 🔴 Only a row that is actually new. A dedupe returns the job already queued, and announcing
    // it again would tell every watching page that something started when nothing did.
    if !deduplicated {
        state.queue.announce(&job.id, job.kind, job.state);
    }
    state.queue.wake();

    let (position, eta_start) = receipt(state, &job.id).await;
    Ok(EnqueueResponse {
        position,
        job_id: job.id,
        state: job.state,
        eta_start,
        blocked_reason: job.blocked_reason,
        deduplicated,
    })
}

/// How long a sync-if-fast caller waits inline before it is handed a job id.
///
/// 🔴 This is what stops "everything is a job" from regressing the one-quick-image
/// loop. `POST /images` and krea2 t2i become: create the row, and if it is
/// admissible immediately, wait inline and return the image exactly as today —
/// otherwise 202. One implementation, both behaviours, and the record exists
/// either way so a reload never loses a render.
const SYNC_WAIT: Duration = Duration::from_secs(45);

/// Poll interval while waiting inline. Short: the whole point is that a fast
/// render answers like a synchronous one.
const SYNC_POLL: Duration = Duration::from_millis(250);

/// Enqueue, then wait inline for a fast job.
///
/// Returns `Ok(Some(job))` when the job reached a terminal state within the
/// window — the caller then serves its result as it always did. `Ok(None)` means
/// it is still going and the caller should answer 202 with the id.
///
/// The wait is bounded and the row is never touched by it: a caller that hangs up
/// changes nothing, which is the difference between this and the old synchronous
/// routes.
pub(crate) async fn enqueue_and_wait(
    state: &AppState,
    who: &Principal,
    req: EnqueueRequest,
) -> Result<(EnqueueResponse, Option<Job>), (StatusCode, String)> {
    let accepted = enqueue(state, who, req).await?;
    let deadline = tokio::time::Instant::now() + SYNC_WAIT;
    loop {
        let Ok(Some(job)) = store::get(&state.db, &accepted.job_id).await else {
            return Ok((accepted, None));
        };
        if job.state.is_terminal() {
            return Ok((accepted, Some(job)));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok((accepted, None));
        }
        tokio::time::sleep(SYNC_POLL).await;
    }
}

/// What the caller is told about a job it just submitted: its 1-based position in
/// the pending set, and when the replay says it starts.
///
/// 🔴 "queued at #3" without a time is the receipt saying nothing useful. The
/// estimator already knows, and this is the one moment the user is looking.
async fn receipt(state: &AppState, id: &str) -> (Option<i64>, Option<String>) {
    let Ok(mut pending) = store::pending(&state.db).await else {
        return (None, None);
    };
    let position = pending
        .iter()
        .position(|j| j.id == id)
        .map(|i| i as i64 + 1);
    let live = store::live(&state.db).await.unwrap_or_default();
    schedule(state, &mut pending, &live).await;
    let eta = pending
        .into_iter()
        .find(|j| j.id == id)
        .and_then(|j| j.eta_start);
    (position, eta)
}

fn job_id(kind: JobKind, payload: &Value) -> String {
    let mut h = Sha256::new();
    h.update(uuid::Uuid::now_v7().as_bytes());
    h.update(payload.to_string().as_bytes());
    let digest = crate::media_sign::hex(&h.finalize());
    format!("{}_{}", kind.as_str(), &digest[..16])
}

async fn reorder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ReorderRequest>,
) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    let Ok(Some(job)) = store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such job");
    };
    if !job.state.is_pending() {
        return err(
            StatusCode::CONFLICT,
            "only a job that has not started can be reordered",
        );
    }

    // 🔴 A pin can need moving after the fact: a card gets disabled while a job is
    // already queued behind it, and then the pin is exactly the thing that is
    // wrong. Applied before the ordering change so one request can do both.
    if let Some(target) = req.target_gpu.clone() {
        let force = req.force_pin.unwrap_or(job.force_pin);
        match store::repin(&state.db, &id, target.as_deref(), force).await {
            Ok(true) => {}
            Ok(false) => {
                return err(
                    StatusCode::CONFLICT,
                    "only a job that has not started can be repinned",
                );
            }
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
        state.queue.wake();
    } else if let Some(force) = req.force_pin
        && let Err(e) = store::repin(&state.db, &id, job.target_gpu.as_deref(), force).await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }

    if req.bump {
        if let Err(e) = store::bump(&state.db, &id).await {
            return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
        state.queue.wake();
        return StatusCode::NO_CONTENT.into_response();
    }

    let band = req.band.unwrap_or(job.band);
    // A float rank runs out of precision after enough midpoint insertions onto one
    // seam; renumber and retry rather than silently landing two jobs on one rank.
    match store::reorder(&state.db, &id, band, req.after.as_deref()).await {
        Ok(true) => {}
        Ok(false) => {
            if let Err(e) = store::rebalance(&state.db, band).await {
                return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
            }
            if let Err(e) = store::reorder(&state.db, &id, band, req.after.as_deref()).await {
                return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
            }
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
    state.queue.wake();
    StatusCode::NO_CONTENT.into_response()
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    let Ok(Some(job)) = store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such job");
    };

    match job.state {
        // Cancelling a queued job is just deleting its claim on the future.
        JobState::Queued | JobState::Held => {
            state.queue.abandon_pending(&state.db, &id).await;
            StatusCode::NO_CONTENT.into_response()
        }
        JobState::Running => {
            match state.queue.get() {
                Some(d) if d.interrupt(&id, Interrupt::Cancel) => {
                    StatusCode::ACCEPTED.into_response()
                }
                // Running with no live task: the dispatcher is on another process,
                // or the task has just finished. Nothing to interrupt.
                _ => err(
                    StatusCode::CONFLICT,
                    "this job is running but not on this process",
                ),
            }
        }
        // 🔴 Past the engine/disk boundary. The GPU cost is already paid and the
        // artifact is landing; killing it here loses the only copy.
        JobState::Collecting => err(
            StatusCode::CONFLICT,
            "this job has finished on the GPU and is saving its output — it cannot be cancelled now",
        ),
        _ => err(StatusCode::CONFLICT, "this job has already finished"),
    }
}

#[derive(Deserialize)]
struct HoldBody {
    #[serde(default)]
    held: bool,
}

async fn hold(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<HoldBody>,
) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    match store::set_held(&state.db, &id, body.held).await {
        Ok(true) => {
            state.queue.wake();
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => err(
            StatusCode::CONFLICT,
            "only a job that has not started can be held or released",
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Operator preempt: stop the running job at its next checkpoint and give it back
/// its place at the head of its band.
///
/// 🔴 Operator-only, never automatic from band. Automatic preemption plus a
/// trickle of small jobs is a 6-hour render that never finishes.
async fn preempt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    let Ok(Some(job)) = store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such job");
    };
    if job.state != JobState::Running {
        return err(StatusCode::CONFLICT, "only a running job can be preempted");
    }
    let Some(dispatcher) = state.queue.get() else {
        return err(
            StatusCode::CONFLICT,
            "the dispatcher is not on this process",
        );
    };
    match dispatcher.runner(job.kind).map(|r| r.preempt()) {
        Some(koblem_client::types::Preempt::None) | None => err(
            StatusCode::CONFLICT,
            "this engine cannot be interrupted — it has queued work koblem cannot see",
        ),
        Some(_) if dispatcher.interrupt(&id, Interrupt::Preempt) => {
            StatusCode::ACCEPTED.into_response()
        }
        Some(_) => err(
            StatusCode::CONFLICT,
            "this job is not running on this process",
        ),
    }
}

/// Run a finished job again.
///
/// 🔴 A FRESH ROW, never a resurrection. The terminal row is the record of what
/// happened — `finished_at`, `render_ms` and the whole `blocked_history` describe
/// *that* run — and rewinding it to `queued` destroys exactly the history the
/// queue keeps. The new row carries `retry_of` and continues the attempt count, so
/// a retried job never reads as a first try.
///
/// It goes through the shared enqueue, so it is validated, deduped, banded and
/// placed identically to any other submit.
async fn retry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let who = match principal(&state, &headers).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Ok(Some(job)) = store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such job");
    };
    if !job.state.is_terminal() {
        return err(
            StatusCode::CONFLICT,
            "this job has not finished — hold or cancel it instead of retrying it",
        );
    }
    let Ok(Some((payload, _))) = store::payload(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "this job's payload is gone");
    };

    // A parent pruned out from under the retry would make the enqueue 400 on a
    // dependency that no longer means anything.
    let depends_on = match job.depends_on.as_deref() {
        Some(p) if store::get(&state.db, p).await.ok().flatten().is_some() => job.depends_on,
        _ => None,
    };

    let req = EnqueueRequest {
        kind: job.kind,
        payload,
        mode: None,
        band: Some(job.band),
        target_gpu: job.target_gpu,
        force_pin: job.force_pin,
        depends_on,
        idempotency_key: None,
    };
    match enqueue(&state, &who, req).await {
        Ok(resp) => {
            if !resp.deduplicated {
                let _ = store::record_retry_of(&state.db, &resp.job_id, &id).await;
            }
            (StatusCode::ACCEPTED, Json(resp)).into_response()
        }
        Err((code, msg)) => err(code, &msg),
    }
}

#[derive(Deserialize)]
struct PauseBody {
    #[serde(default)]
    paused: bool,
}

async fn pause(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PauseBody>,
) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    match state.queue.get() {
        Some(d) => {
            d.set_paused(body.paused);
            Json(json!({ "paused": body.paused })).into_response()
        }
        None => err(
            StatusCode::CONFLICT,
            "the dispatcher is not on this process",
        ),
    }
}

async fn stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    match gpu_job_stats::all(&state.db).await {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|s| {
                    json!({
                        "kind": s.kind,
                        "shape_key": s.shape_key,
                        "samples": s.samples,
                        "p50_ms": s.p50_ms,
                        "p90_ms": s.p90_ms,
                        "unit_p50_ms": s.unit_p50_ms,
                        "updated_at": s
                            .updated_at
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                        // 🔴 Published so a caller can tell a measurement from a
                        // guess. Below this the number is a prior.
                        "confident": s.samples >= crate::queue::estimate::CONFIDENT_SAMPLES,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The SSE stream. Replaces a pile of per-engine polling the current UIs do.
async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, Response> {
    principal(&state, &headers).await?;
    let Some(dispatcher) = state.queue.get() else {
        return Err(err(
            StatusCode::CONFLICT,
            "the dispatcher is not on this process",
        ));
    };
    let rx = dispatcher.subscribe();
    let stream = async_stream::stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    yield Ok(Event::default().event("job").data(data));
                }
                // A slow client lagged. Tell it so it re-reads the snapshot rather
                // than silently missing a transition.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    yield Ok(Event::default().event("lagged").data(n.to_string()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    /// 🔴 The one that matters. The CLI authenticates with an API key, whose
    /// principal default is `now` — the opposite of what a shell loop wants — so
    /// without a working override "queue five renders overnight" cannot be
    /// expressed. Every render route reads these off the request.
    #[test]
    fn an_explicit_mode_is_carried_off_the_headers() {
        let opts = QueueOptions::from_headers(&headers(&[("x-queue-mode", "queue")]));
        assert_eq!(opts.mode, Some(SubmitMode::Queue));
        let opts = QueueOptions::from_headers(&headers(&[("x-queue-mode", "now")]));
        assert_eq!(opts.mode, Some(SubmitMode::Now));
    }

    /// Absent means "the default for this caller", which `enqueue` computes from
    /// the principal. It must NOT collapse to a hardcoded mode.
    #[test]
    fn absence_defers_to_the_principal() {
        let opts = QueueOptions::from_headers(&HeaderMap::new());
        assert_eq!(opts.mode, None);
        assert_eq!(opts.band, None);
        assert_eq!(opts.target_gpu, None);
        assert!(!opts.force_pin);
        assert_eq!(opts.depends_on, None);
        assert_eq!(opts.idempotency_key, None);
    }

    /// An unparseable value is absence, not a 400. These are operator conveniences
    /// on a render submit; refusing the whole render because a band was misspelled
    /// would be a worse trade than ignoring it.
    #[test]
    fn a_bad_value_falls_back_rather_than_refusing() {
        let opts = QueueOptions::from_headers(&headers(&[
            ("x-queue-mode", "sometime"),
            ("x-queue-band", "not-a-number"),
            ("idempotency-key", "   "),
        ]));
        assert_eq!(opts.mode, None);
        assert_eq!(opts.band, None);
        assert_eq!(opts.idempotency_key, None, "whitespace is not a key");
    }

    /// The pin, the force flag and the dependency all reach the request.
    #[test]
    fn the_pin_and_the_dependency_reach_the_enqueue() {
        let opts = QueueOptions::from_headers(&headers(&[
            ("x-queue-gpu", "GPU-00000000"),
            ("x-queue-force", "1"),
            ("x-queue-after", "music3_abc"),
            ("x-queue-band", "2"),
        ]));
        let req = opts.request(JobKind::Upscale, json!({"a": 1}));
        assert_eq!(req.target_gpu.as_deref(), Some("GPU-00000000"));
        assert!(req.force_pin);
        assert_eq!(req.depends_on.as_deref(), Some("music3_abc"));
        assert_eq!(req.band, Some(2));
        assert_eq!(req.kind, JobKind::Upscale);
    }
}
