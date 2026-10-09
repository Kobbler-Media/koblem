//! The dispatcher: one scan per tick, band then rank, skip-with-a-reason.
//!
//! The decision itself lives in [`super::admit`] and is pure. This module is the
//! I/O around it — claiming rows, acquiring the gate, spawning runs, and the boot
//! sequence.
//!
//! 🔴 SINGLE INSTANCE. The row claim is safe under `FOR UPDATE SKIP LOCKED`, but
//! the §7 unload planner is not: two koblem replicas would fight over unloads, and
//! a drain issued by one and abandoned by the other leaves an engine answering 503
//! to every render until the container restarts. Asserted at boot with a Postgres
//! advisory lock.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use kob_gpu_gate::{AdmitFailure, GpuLock, ServiceKind};
use koblem_client::types::{BlockedReason, Job, JobKind, JobState};
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::repositories::{gpu_job_stats, gpu_jobs as store};

use super::admit::{self, AdmitContext, GateView, HealthView, Reservation};
use super::clock::Clock;
use super::events::EventBus;
use super::health::EngineHealth;
use super::lane::{Lane, LaneCensus};
use super::planner::Planner;
use super::runner::{CancelToken, Failure, Interrupt, JobCtx, JobOutcome, JobRunner, ProgressSink};

/// Backstop for a missed wake-up. The dispatcher is edge-triggered (enqueue,
/// reorder, terminal, gate vacancy, card/service enable); this only covers a lost
/// edge, so it can be lazy.
const BACKSTOP: Duration = Duration::from_secs(3);

/// How long the head must have waited before its claim is held against backfill.
const HEAD_RESERVE_AFTER_MS: i64 = 5 * 60 * 1000;

/// First retry delay for an unreachable engine, doubling to [`BACKOFF_MAX`].
/// 🔴 A dead service must not be hammered once a second all night.
const BACKOFF_START_MS: i64 = 5_000;
const BACKOFF_MAX_MS: i64 = 5 * 60 * 1000;

/// Two koblem processes must not both plan unloads. Arbitrary but fixed.
const DISPATCHER_LOCK_KEY: i64 = 0x6b6f_625f_6771_0001;

/// 🔴 Bounded, and small. Three goes at a 40-minute render is two hours of a
/// night spent on something that was never going to work, and the queue behind it
/// pays for every one.
const MAX_ATTEMPTS: i32 = 3;

pub struct Registry {
    runners: HashMap<JobKind, Arc<dyn JobRunner>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            runners: HashMap::new(),
        }
    }

    pub fn register(&mut self, runner: Arc<dyn JobRunner>) {
        self.runners.insert(runner.kind(), runner);
    }

    pub fn get(&self, kind: JobKind) -> Option<Arc<dyn JobRunner>> {
        self.runners.get(&kind).cloned()
    }

    /// Kinds with no adapter yet. Their rows are refused at enqueue rather than
    /// accepted into a queue that can never run them.
    pub fn missing(&self) -> Vec<JobKind> {
        JobKind::ALL
            .into_iter()
            .filter(|k| !self.runners.contains_key(k))
            .collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// A live run's handle, so a cancel or preempt can reach it.
struct InFlight {
    cancel: CancelToken,
}

#[derive(Clone)]
pub struct Dispatcher {
    inner: Arc<Inner>,
}

struct Inner {
    pool: sqlx::PgPool,
    gate: GpuLock,
    clock: Clock,
    registry: Arc<Registry>,
    health: Arc<EngineHealth>,
    events: EventBus,
    planner: Planner,
    wake: Notify,
    in_flight: std::sync::Mutex<HashMap<String, InFlight>>,
    /// Per-lane `(next_attempt_ms, delay_ms, detail)`.
    backoff: std::sync::Mutex<HashMap<Lane, (i64, i64, String)>>,
    paused: std::sync::atomic::AtomicBool,
}

/// The real gate, seen through admission's narrow window.
struct LiveGate<'a>(&'a GpuLock);

impl GateView for LiveGate<'_> {
    fn can_place(
        &self,
        service: ServiceKind,
        pin: Option<&str>,
        force: bool,
    ) -> Option<AdmitFailure> {
        match pin {
            Some(card) => self.0.can_place_on(service, card, force),
            None => self.0.can_place(service),
        }
    }

    fn candidates(&self, service: ServiceKind) -> Vec<String> {
        self.0
            .engine_config(service)
            .map(|d| d.candidate_gpus)
            .unwrap_or_default()
    }
}

struct RowSink {
    pool: sqlx::PgPool,
    id: String,
    events: EventBus,
    kind: JobKind,
}

#[async_trait::async_trait]
impl ProgressSink for RowSink {
    async fn set(&self, progress: koblem_client::types::JobProgress, units_done: i32) {
        if let Err(e) = store::set_progress(&self.pool, &self.id, &progress, units_done).await {
            warn!(job = %self.id, error = %e, "queue: progress write failed");
        }
        // 🔴 Progress goes to the SSE stream and NOWHERE else. A 16-generation H3
        // job emits per-generation progress; pushing those would mean sixteen
        // notifications for one render.
        self.events
            .publish_progress(&self.id, self.kind, progress, units_done);
    }
}

impl Dispatcher {
    pub fn new(
        pool: sqlx::PgPool,
        gate: GpuLock,
        clock: Clock,
        registry: Arc<Registry>,
        health: Arc<EngineHealth>,
        events: EventBus,
    ) -> Self {
        let planner = Planner::new(gate.clone());
        Dispatcher {
            inner: Arc::new(Inner {
                pool,
                gate,
                clock,
                registry,
                health,
                events,
                planner,
                wake: Notify::new(),
                in_flight: std::sync::Mutex::new(HashMap::new()),
                backoff: std::sync::Mutex::new(HashMap::new()),
                paused: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// The adapter for a kind, so a route can validate, label and estimate a
    /// payload without a second registry.
    pub fn runner(&self, kind: JobKind) -> Option<Arc<dyn JobRunner>> {
        self.inner.registry.get(kind)
    }

    /// Announce a state transition on the event spine. See [`EventBus::publish_state`].
    pub fn announce(&self, job_id: &str, kind: JobKind, state: JobState) {
        self.inner.events.publish_state(job_id, kind, state);
    }

    /// Subscribe to the event spine (the SSE route's only input).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<koblem_client::types::JobEvent> {
        self.inner.events.subscribe()
    }

    /// Nudge the dispatcher: an enqueue, a reorder, a terminal job, a card or
    /// service toggle. Cheap and idempotent.
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    pub fn paused(&self) -> bool {
        self.inner.paused.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// "Finish the current job, then stop" — the overnight lever.
    pub fn set_paused(&self, paused: bool) {
        self.inner
            .paused
            .store(paused, std::sync::atomic::Ordering::Relaxed);
        self.wake();
    }

    /// Ask a live job to stop. A `collecting` job is deliberately unreachable:
    /// the GPU is already paid for, the artifact is landing, and killing it there
    /// loses the only copy.
    pub fn interrupt(&self, job_id: &str, what: Interrupt) -> bool {
        let guard = self.inner.in_flight.lock().unwrap();
        match guard.get(job_id) {
            Some(f) => {
                f.cancel.request(what);
                true
            }
            None => false,
        }
    }

    /// Cancel a row that has not started, take its dependents with it, and tell each one's adapter
    /// so the engine-side record it keeps is retired with it.
    ///
    /// 🔴 The write-through is the point. Cancelling a pending row used to be two DB statements and
    /// nothing else, and for the kinds that keep their own record — the two video stores and the
    /// pixal3d catalogue — that left the record saying "queued" with no task left to correct it.
    /// See [`JobRunner::abandon`].
    pub async fn abandon_pending(&self, id: &str) -> bool {
        let cancelled = store::cancel_pending(&self.inner.pool, id)
            .await
            .unwrap_or(false);
        if cancelled {
            self.abandon_record(id).await;
        }
        self.abandon_dependents(id).await;
        self.wake();
        cancelled
    }

    /// A parent that failed or was cancelled takes its dependents with it — including their
    /// records, which are cancelled for the same reason and by the same argument.
    pub async fn abandon_dependents(&self, parent: &str) {
        let ids = store::cascade_dependents(&self.inner.pool, parent)
            .await
            .unwrap_or_default();
        for id in ids {
            self.abandon_record(&id).await;
        }
    }

    /// Route one cancelled row to its own adapter. Silent when the kind has no adapter registered:
    /// a row that can never run has no engine-side record either.
    async fn abandon_record(&self, id: &str) {
        let Ok(Some(job)) = store::get(&self.inner.pool, id).await else {
            return;
        };
        let Some(runner) = self.inner.registry.get(job.kind) else {
            return;
        };
        let payload = store::payload(&self.inner.pool, id)
            .await
            .ok()
            .flatten()
            .map(|(p, _)| p)
            .unwrap_or(serde_json::Value::Null);
        runner.abandon(id, &payload).await;
    }

    /// 🔴 Reattach must fully settle BEFORE the first dispatch.
    ///
    /// On boot a re-attaching render still owns its card, but the gate only learns
    /// that from `observe()` via the reconcile loop — which is 5 s away. Dispatch
    /// before that settles and the queue admits a job onto occupied VRAM. So this
    /// re-takes the guard for every reattached job SYNCHRONOUSLY rather than
    /// waiting for a reconcile tick to notice.
    pub async fn bootstrap(&self) -> Result<(), String> {
        let g = &self.inner;

        // 🔴 A `running` row with no `server_job_id` never reached the engine, so
        // nothing is lost by putting it back in the queue. This is what the five
        // old stores each did by hand at boot.
        match store::requeue_unsubmitted(&g.pool).await {
            Ok(ids) if !ids.is_empty() => {
                info!(
                    count = ids.len(),
                    "queue: requeued jobs that never submitted"
                )
            }
            Err(e) => warn!(error = %e, "queue: could not requeue unsubmitted jobs"),
            _ => {}
        }

        let jobs = store::reattachable(&g.pool)
            .await
            .map_err(|e| format!("could not load reattachable jobs: {e}"))?;

        for job in jobs {
            let Some(runner) = g.registry.get(job.kind) else {
                warn!(job = %job.id, kind = job.kind.as_str(),
                    "queue: no adapter for a reattachable job; failing it rather than leaving it live");
                let _ = store::finish(
                    &g.pool,
                    &job.id,
                    JobState::Failed,
                    Some("no adapter is registered for this kind"),
                    &[],
                )
                .await;
                continue;
            };

            // The work is happening on the engine whether the ledger likes it or
            // not, so a refusal here is not a reason to skip the reattach — only a
            // reason to say so. The reconcile loop's `observe` catches up.
            let acq = match g.gate.try_acquire(runner.service()).await {
                Ok(a) => Some(a),
                Err(why) => {
                    warn!(job = %job.id, %why,
                        "queue: reattaching without a reservation; the ledger may briefly under-count");
                    None
                }
            };
            self.spawn(job, runner, acq, true).await;
        }
        Ok(())
    }

    /// Claim the single-dispatcher lock. `false` means another koblem already
    /// holds it and this process must not plan unloads.
    pub async fn claim_singleton(&self) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(DISPATCHER_LOCK_KEY)
            .fetch_one(&self.inner.pool)
            .await
            .unwrap_or(false)
    }

    pub async fn run(self) {
        loop {
            self.tick().await;
            tokio::select! {
                _ = self.inner.wake.notified() => {}
                _ = tokio::time::sleep(BACKSTOP) => {}
            }
        }
    }

    async fn tick(&self) {
        let g = &self.inner;
        let pending = match store::pending(&g.pool).await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "queue: could not read the pending set");
                return;
            }
        };
        let live = store::live(&g.pool).await.unwrap_or_default();

        // One snapshot per tick. 🔴 Reasons must not flap: computing them from a
        // ledger that moves under the scan gives two rows two different answers to
        // the same question in the same second.
        //
        // 🔴 `running` only. A `collecting` row has already had its `release_gpu`
        // called — the engine's poll said completed and the card is free — and it
        // is fetching, muxing and harvesting, which on an upscale is minutes. On
        // comfy-api that is four kinds sharing one lane held through every tail.
        let mut lanes = LaneCensus::default();
        for j in live.iter().filter(|j| j.state == JobState::Running) {
            lanes.occupy(Lane::of(j.kind));
        }
        for lane in g.health.lanes() {
            if g.health.engine_busy(lane) {
                lanes.mark_engine_busy(lane);
            }
        }
        // 🔴 Someone outside the queue is holding this engine RIGHT NOW.
        //
        // A handful of paths still acquire the gate directly and are meant to:
        // `/oneshot` renders krea2 and sa3 inline because paperworld needs the
        // bytes in the response, relip drives ltx25 in place, and songgen's and
        // sa3's editing modes take multipart audio a queued payload cannot hold.
        // Every one of them is a second writer to an engine the queue also owns.
        //
        // The health probe would catch it, but on a 5 s poll — long enough to
        // submit a second render into a busy server first. The gate already knows
        // the answer for free: a guard is held. Subtract the queue's own running
        // jobs and anything left is a direct holder, so close the lane.
        for kind in JobKind::ALL {
            let service = admit::service_of(kind);
            // Only the rows that still hold a guard: a `collecting` job dropped
            // its acquisition at the engine/disk boundary, so counting it here
            // would hide a real outsider behind it.
            let ours = live
                .iter()
                .filter(|j| j.state == JobState::Running && admit::service_of(j.kind) == service)
                .count() as u32;
            if g.gate.holders(service) > ours {
                lanes.mark_engine_busy(Lane::of(kind));
            }
        }

        let parents = self.parent_states(&pending).await;
        self.cascade_dead_parents(&pending, &parents).await;

        // 🔴 Tell the gate what is coming BEFORE anything else touches it. This is
        // what stops `rehome_standing` re-seeding the LLM into the gap between two
        // queued jobs, so job N+1 pays an eviction job N had already paid.
        g.gate.set_pending_demand(&self.demand(&pending));

        let reservation = self.reservation(&pending);
        let backoff = self.backoff_view();

        for job in &pending {
            let decision = admit::admit(
                job,
                &AdmitContext {
                    paused: self.paused(),
                    lanes: &lanes,
                    gate: &LiveGate(&g.gate),
                    health: g.health.as_ref(),
                    parents: &parents,
                    reservation: reservation.as_ref(),
                    backing_off: &backoff,
                },
            );

            match decision {
                Err(reason) => {
                    if let BlockedReason::EngineUnreachable { detail } = &reason {
                        self.note_unreachable(Lane::of(job.kind), detail);
                    }
                    if job.blocked_reason.as_ref() != Some(&reason) || job.blocked_since.is_none() {
                        self.inner
                            .events
                            .publish_blocked(&job.id, job.kind, &reason);
                    }
                    if let Err(e) = store::stamp_blocked(&g.pool, &job.id, &reason).await {
                        warn!(job = %job.id, error = %e, "queue: could not stamp a blocked reason");
                    }
                }
                Ok(()) => {
                    self.clear_backoff(Lane::of(job.kind));
                    if self.start(job).await {
                        lanes.occupy(Lane::of(job.kind));
                    }
                }
            }
        }

        // §7: the planner runs LAST, once the tick's starts have landed, so it
        // plans against what will actually be resident.
        g.planner.plan(&pending, &live).await;
    }

    /// Start one job: claim the row, acquire, spawn.
    async fn start(&self, job: &Job) -> bool {
        let g = &self.inner;
        let Some(runner) = g.registry.get(job.kind) else {
            warn!(job = %job.id, kind = job.kind.as_str(), "queue: no adapter registered");
            let _ = store::finish(
                &g.pool,
                &job.id,
                JobState::Failed,
                Some("no adapter is registered for this kind"),
                &[],
            )
            .await;
            return false;
        };

        // 🔴 Acquire BEFORE claiming the row is wrong (a lost claim leaks a guard)
        // and claiming before acquiring is wrong (a failed acquire leaves the row
        // `running` with nothing running). Claim first, then acquire, and put the
        // row back if the acquire fails — the claim is cheap to undo, the guard is
        // not.
        let queued_ms = self.elapsed_since(&job.created_at);
        match store::claim(&g.pool, &job.id, queued_ms).await {
            // 🔴 The transition a watching page cannot get any other way. Progress frames only
            // start once the adapter reports one, which on a render that spends minutes loading a
            // checkpoint is a long silence that reads exactly like "nothing has started".
            Ok(true) => g.events.publish_state(&job.id, job.kind, JobState::Running),
            Ok(false) => return false, // reordered or cancelled mid-tick; normal
            Err(e) => {
                warn!(job = %job.id, error = %e, "queue: claim failed");
                return false;
            }
        }

        let admit_start = g.clock.now_ms();
        let acq = match (job.target_gpu.as_deref(), job.force_pin) {
            (Some(card), force) => g.gate.try_acquire_on(runner.service(), card, force).await,
            (None, _) => g.gate.try_acquire(runner.service()).await,
        };
        let admit_ms = g.clock.since_ms(admit_start);

        let acq = match acq {
            Ok(a) => a,
            Err(why) => {
                // The world moved between `can_place` and here. Put it back and let
                // the next tick stamp a fresh reason — never a failure.
                let reason = match why {
                    AdmitFailure::ServiceDisabled => BlockedReason::ServiceDisabled {
                        service: runner.service().as_str().to_string(),
                    },
                    AdmitFailure::EvictionUnconfirmed => BlockedReason::EvictionCooldown,
                    AdmitFailure::BoxLocked { keep } => BlockedReason::BoxLocked {
                        service: keep.as_str().to_string(),
                    },
                    AdmitFailure::Place(b) => BlockedReason::NoCardFits {
                        detail: b.to_string(),
                    },
                };
                let _ = store::requeue_after_failed_admit(&g.pool, &job.id).await;
                let _ = store::stamp_blocked(&g.pool, &job.id, &reason).await;
                return false;
            }
        };

        let _ = store::set_admit_ms(&g.pool, &job.id, admit_ms).await;
        self.spawn(job.clone(), runner, Some(acq), false).await;
        true
    }

    /// Run a job to completion in its own task.
    async fn spawn(
        &self,
        job: Job,
        runner: Arc<dyn JobRunner>,
        acq: Option<kob_gpu_gate::Acquisition>,
        reattaching: bool,
    ) {
        let g = Arc::clone(&self.inner);
        let this = self.clone();
        let cancel = CancelToken::default();
        {
            let mut map = g.in_flight.lock().unwrap();
            map.insert(
                job.id.clone(),
                InFlight {
                    cancel: cancel.clone(),
                },
            );
        }

        let (payload, resume) = store::payload(&g.pool, &job.id)
            .await
            .ok()
            .flatten()
            .unwrap_or((serde_json::Value::Null, None));

        let sink = Arc::new(RowSink {
            pool: g.pool.clone(),
            id: job.id.clone(),
            events: g.events.clone(),
            kind: job.kind,
        });

        let job_id = job.id.clone();
        let kind = job.kind;
        let shape_key = job.shape_key.clone();

        // 🔴 `running -> collecting` happens at the ENGINE/DISK boundary, which
        // only the runner knows. The row moves when the guard is dropped, and
        // `render_ms` is stamped from the same instant — engine time, kept apart
        // from the queue wait and the eviction cost.
        let boundary = {
            let pool = g.pool.clone();
            let id = job.id.clone();
            let clock = g.clock.clone();
            let started = g.clock.now_ms();
            Box::new(move || {
                let pool = pool.clone();
                let id = id.clone();
                let render_ms = clock.since_ms(started);
                tokio::spawn(async move {
                    if let Err(e) = store::to_collecting(&pool, &id, render_ms).await {
                        warn!(job = %id, error = %e, "queue: could not record the collect phase");
                    }
                });
            }) as Box<dyn Fn() + Send + Sync>
        };

        let ctx = JobCtx::new(job, payload, resume, cancel, sink, acq, boundary);

        tokio::spawn(async move {
            let started = g.clock.now_ms();
            let outcome = if reattaching {
                runner.reattach(ctx).await
            } else {
                runner.run(ctx).await
            };
            let elapsed = g.clock.since_ms(started);

            let units_done = store::get(&g.pool, &job_id)
                .await
                .ok()
                .flatten()
                .map(|j| j.units_done)
                .unwrap_or(0);

            match outcome {
                Ok(JobOutcome::Done { warnings }) => {
                    let _ = store::finish(&g.pool, &job_id, JobState::Done, None, &warnings).await;
                    // Teach the estimator. A reattached job's duration is wrong
                    // (it started before the restart), so it is not recorded.
                    if !reattaching && let Some(shape) = shape_key.as_deref() {
                        let units = (units_done > 0).then_some(units_done);
                        let _ =
                            gpu_job_stats::record(&g.pool, kind.as_str(), shape, elapsed, units)
                                .await;
                    }
                    g.events
                        .publish_terminal(&job_id, kind, JobState::Done, None, &warnings);
                }
                Ok(JobOutcome::Preempted { resume }) => {
                    if let Err(e) = store::requeue_preempted(&g.pool, &job_id, &resume).await {
                        warn!(job = %job_id, error = %e, "queue: could not bank a preempted job");
                    }
                }
                Ok(JobOutcome::Cancelled) => {
                    let _ = store::finish(&g.pool, &job_id, JobState::Cancelled, None, &[]).await;
                    this.abandon_dependents(&job_id).await;
                    g.events
                        .publish_terminal(&job_id, kind, JobState::Cancelled, None, &[]);
                }
                Err(error) => {
                    // 🔴 Per-job terminal failure. Five queued, #2 fails → the queue
                    // CONTINUES. Nobody is watching the logs at 03:00 — which is
                    // also why a failure that will clear by itself gets one more
                    // attempt rather than being lost until morning.
                    if this
                        .requeue_failed(&job_id, kind, runner.as_ref(), &error)
                        .await
                    {
                        info!(job = %job_id, error = %error, "queue: retrying a transient failure");
                    } else {
                        let _ =
                            store::finish(&g.pool, &job_id, JobState::Failed, Some(&error), &[])
                                .await;
                        this.abandon_dependents(&job_id).await;
                        g.events.publish_terminal(
                            &job_id,
                            kind,
                            JobState::Failed,
                            Some(&error),
                            &[],
                        );
                    }
                }
            }

            g.in_flight.lock().unwrap().remove(&job_id);
            this.wake();
        });
    }

    /// Give a failed run another attempt, if it deserves one and has one left.
    ///
    /// 🔴 Two signals, neither of them the error text. The adapter's own typed
    /// classification, and whether the engine is down RIGHT NOW — a validation
    /// failure or an engine 4xx leaves it perfectly healthy and would fail
    /// identically on every attempt, while a transport failure or a crash is
    /// exactly what the health probe reports. The probe is re-run rather than read
    /// off a 5 s-old snapshot, because the answer decides whether a night's work
    /// survives.
    async fn requeue_failed(
        &self,
        job_id: &str,
        kind: JobKind,
        runner: &dyn JobRunner,
        error: &str,
    ) -> bool {
        let g = &self.inner;
        let lane = Lane::of(kind);
        let transient = runner.classify(error) == Failure::Transient || {
            g.health.poll_once().await;
            g.health.unreachable(lane).is_some()
        };
        if !transient {
            return false;
        }
        match store::retry(&g.pool, job_id, error, MAX_ATTEMPTS).await {
            Ok(true) => {
                // The same backoff an unreachable engine already gets, so the next
                // tick does not resubmit straight back into it. The wake is the
                // caller's, AFTER it has dropped the in-flight handle — waking here
                // would let a tick claim the row while the old handle is still
                // registered, and the new run would lose its cancel token.
                self.note_unreachable(lane, error);
                true
            }
            Ok(false) => false,
            Err(e) => {
                warn!(job = %job_id, error = %e, "queue: could not requeue a failed job");
                false
            }
        }
    }

    async fn parent_states(&self, pending: &[Job]) -> HashMap<String, JobState> {
        let mut out = HashMap::new();
        for job in pending {
            let Some(parent) = job.depends_on.as_deref() else {
                continue;
            };
            if out.contains_key(parent) {
                continue;
            }
            if let Ok(Some(p)) = store::get(&self.inner.pool, parent).await {
                out.insert(parent.to_string(), p.state);
            } else {
                // A parent pruned out from under its child is treated as done —
                // the alternative is a job that can never run and never says why.
                out.insert(parent.to_string(), JobState::Done);
            }
        }
        out
    }

    /// A parent that failed or was cancelled takes its dependents with it.
    async fn cascade_dead_parents(&self, pending: &[Job], parents: &HashMap<String, JobState>) {
        for job in pending {
            let Some(parent) = job.depends_on.as_deref() else {
                continue;
            };
            if matches!(
                parents.get(parent),
                Some(JobState::Failed) | Some(JobState::Cancelled)
            ) {
                self.abandon_dependents(parent).await;
            }
        }
    }

    /// The VRAM claims of the jobs at the head of the queue, deduped.
    fn demand(&self, pending: &[Job]) -> Vec<ServiceKind> {
        let mut out: Vec<ServiceKind> = Vec::new();
        for job in pending.iter().filter(|j| j.state == JobState::Queued) {
            let s = admit::service_of(job.kind);
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    fn reservation(&self, pending: &[Job]) -> Option<Reservation> {
        let head = pending.iter().find(|j| j.state == JobState::Queued)?;
        let blocked_ms = head
            .blocked_since
            .as_deref()
            .map(|s| self.elapsed_since(s))
            .unwrap_or(0);
        admit::reservation(
            head,
            blocked_ms,
            HEAD_RESERVE_AFTER_MS,
            &LiveGate(&self.inner.gate),
        )
    }

    fn elapsed_since(&self, rfc3339: &str) -> i64 {
        time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
            .map(|t| {
                self.inner
                    .clock
                    .since_ms((t.unix_timestamp_nanos() / 1_000_000) as i64)
            })
            .unwrap_or(0)
    }

    /// Lanes currently inside their retry backoff.
    fn backoff_view(&self) -> HashMap<Lane, String> {
        let now = self.inner.clock.now_ms();
        self.inner
            .backoff
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, (next, _, _))| *next > now)
            .map(|(lane, (_, _, detail))| (*lane, detail.clone()))
            .collect()
    }

    fn note_unreachable(&self, lane: Lane, detail: &str) {
        let now = self.inner.clock.now_ms();
        let mut map = self.inner.backoff.lock().unwrap();
        let entry = map
            .entry(lane)
            .or_insert((0, BACKOFF_START_MS, detail.to_string()));
        if entry.0 <= now {
            entry.0 = now + entry.1;
            entry.1 = (entry.1 * 2).min(BACKOFF_MAX_MS);
        }
    }

    fn clear_backoff(&self, lane: Lane) {
        self.inner.backoff.lock().unwrap().remove(&lane);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::adapters::fake::{FakeRunner, Script};
    use crate::queue::clock::TestClock;
    use crate::queue::health::LaneHealth;
    use crate::repositories::gpu_jobs::NewJob;
    use crate::repositories::test_pool;
    use kob_gpu_gate::{Card, EngineDesc};
    use serde_json::json;

    const A: &str = "GPU-a";
    const B: &str = "GPU-b";

    async fn pool() -> Option<(sqlx::PgPool, tokio::sync::MutexGuard<'static, ()>)> {
        test_pool("gpu_jobs, gpu_job_stats").await
    }

    /// A gate with two cards and enough room for everything, unless a test
    /// deliberately narrows it.
    fn roomy_gate() -> GpuLock {
        let g = GpuLock::new();
        g.set_cards(vec![
            Card {
                uuid: A.into(),
                total_mb: 48000,
                fast: true,
                enabled: true,
            },
            Card {
                uuid: B.into(),
                total_mb: 48000,
                fast: false,
                enabled: true,
            },
        ]);
        for kind in [
            ServiceKind::MinimaxH3,
            ServiceKind::Krea2,
            ServiceKind::Flux,
        ] {
            g.set_engine_config(
                kind,
                EngineDesc {
                    peak_mb: 8000,
                    home_gpu: A.into(),
                    candidate_gpus: vec![A.into(), B.into()],
                    evictable_by: vec![],
                    relocatable: false,
                    rules_json: String::new(),
                },
            );
        }
        g
    }

    struct Harness {
        d: Dispatcher,
        pool: sqlx::PgPool,
        clock: TestClock,
        runners: Vec<Arc<FakeRunner>>,
    }

    fn harness(pool: sqlx::PgPool, gate: GpuLock, scripts: &[(JobKind, Script)]) -> Harness {
        let clock = TestClock::new(1_000_000);
        let mut registry = Registry::new();
        let mut runners = Vec::new();
        for (kind, script) in scripts {
            let r = Arc::new(FakeRunner::new(*kind, script.clone()));
            runners.push(Arc::clone(&r));
            registry.register(r);
        }
        let d = Dispatcher::new(
            pool.clone(),
            gate,
            clock.clock(),
            Arc::new(registry),
            Arc::new(EngineHealth::new(vec![])),
            EventBus::new(),
        );
        Harness {
            d,
            pool,
            clock,
            runners,
        }
    }

    impl Harness {
        async fn add_after(&self, id: &str, kind: JobKind, parent: &str) {
            store::enqueue(
                &self.pool,
                &NewJob {
                    id: id.into(),
                    kind,
                    payload: json!({ "units": 4 }),
                    band: 1,
                    target_gpu: None,
                    force_pin: false,
                    depends_on: Some(parent.into()),
                    created_by: "user:t".into(),
                    idempotency_key: None,
                    units_total: Some(4),
                    shape_key: Some("fake/default".into()),
                    state: JobState::Queued,
                },
            )
            .await
            .unwrap();
        }

        async fn add(&self, id: &str, kind: JobKind, band: i16) {
            store::enqueue(
                &self.pool,
                &NewJob {
                    id: id.into(),
                    kind,
                    payload: json!({ "units": 4 }),
                    band,
                    target_gpu: None,
                    force_pin: false,
                    depends_on: None,
                    created_by: "user:t".into(),
                    idempotency_key: None,
                    units_total: Some(4),
                    shape_key: Some("fake/default".into()),
                    state: JobState::Queued,
                },
            )
            .await
            .unwrap();
        }

        /// One tick, then wait for whatever it started to settle.
        ///
        /// Yielding is not enough: a run's terminal write is real database I/O in
        /// a spawned task, and a test that asserts before it lands is a flake that
        /// only shows up on a loaded machine.
        async fn tick(&self) {
            self.d.tick().await;
            self.settle().await;
        }

        /// Wait until nothing is live, or give up. Bounded so a genuinely stuck
        /// test fails with its own assertion rather than hanging the suite.
        async fn settle(&self) {
            for _ in 0..600 {
                tokio::task::yield_now().await;
                let live = store::live(&self.pool).await.unwrap_or_default();
                if live.iter().all(|j| j.state != JobState::Collecting)
                    && self.d.inner.in_flight.lock().unwrap().is_empty()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        async fn state_of(&self, id: &str) -> JobState {
            store::get(&self.pool, id).await.unwrap().unwrap().state
        }

        /// Bounded wait for one row, for the tests where something else is
        /// deliberately left live and [`Harness::settle`] can never return.
        async fn wait_for(&self, id: &str, want: JobState) {
            for _ in 0..600 {
                if self.state_of(id).await == want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(self.state_of(id).await, want);
        }

        async fn reason_of(&self, id: &str) -> Option<BlockedReason> {
            store::get(&self.pool, id)
                .await
                .unwrap()
                .unwrap()
                .blocked_reason
        }
    }

    /// The whole loop, end to end: a queued job is claimed, run with a card in
    /// hand, and filed as done.
    #[tokio::test]
    async fn a_queued_job_runs_and_finishes() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("j1", JobKind::Music3, 1).await;
        h.tick().await;

        assert_eq!(h.state_of("j1").await, JobState::Done);
        let log = h.runners[0].log();
        assert_eq!(log.ran, vec!["j1"]);
        // 🔴 The dispatcher acquires and hands the acquisition to the runner. A job
        // is never "running" while holding nothing.
        assert_eq!(log.cards, vec![Some(A.to_string())]);
        assert_eq!(log.released, vec![true]);
    }

    /// 🔴 Skipping IS the backfill. The comfy lane is taken, so a second comfy job
    /// waits with a reason — and a krea2 job on a different engine starts anyway,
    /// which strict head-of-line ordering would not allow.
    #[tokio::test]
    async fn a_blocked_head_does_not_stop_the_queue() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(
            p,
            roomy_gate(),
            &[
                (JobKind::Music3, Script::WaitForInterrupt),
                (JobKind::Krea2, Script::Succeed),
            ],
        );
        h.add("comfy-1", JobKind::Music3, 1).await;
        h.add("comfy-2", JobKind::Music3, 1).await;
        h.add("krea", JobKind::Krea2, 1).await;
        h.tick().await;

        assert_eq!(h.state_of("comfy-1").await, JobState::Running);
        assert_eq!(h.state_of("comfy-2").await, JobState::Queued);
        assert_eq!(
            h.reason_of("comfy-2").await,
            Some(BlockedReason::LaneBusy {
                lane: "comfy-api".into()
            }),
            "and it says WHY, on the row"
        );
        assert_eq!(
            h.state_of("krea").await,
            JobState::Done,
            "a different engine backfills past the blocked one"
        );

        h.d.interrupt("comfy-1", Interrupt::Cancel);
    }

    /// Band before rank, all the way through the dispatcher — not just in the
    /// store's ORDER BY.
    #[tokio::test]
    async fn the_interactive_band_goes_first() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(
            p,
            roomy_gate(),
            &[(JobKind::Music3, Script::WaitForInterrupt)],
        );
        h.add("normal", JobKind::Music3, 1).await;
        h.add("urgent", JobKind::Music3, 0).await;
        h.tick().await;

        assert_eq!(h.state_of("urgent").await, JobState::Running);
        assert_eq!(h.state_of("normal").await, JobState::Queued);
        h.d.interrupt("urgent", Interrupt::Cancel);
    }

    /// 🔴 A job that cannot run is never FAILED. A disabled service at 22:00 must
    /// not burn the overnight batch — it waits, says so, and starts the moment the
    /// operator re-enables.
    #[tokio::test]
    async fn a_disabled_service_never_fails_the_batch() {
        let Some((p, _g)) = pool().await else { return };
        let gate = roomy_gate();
        gate.set_service_enabled(ServiceKind::MinimaxH3, false)
            .await;
        let h = harness(p, gate.clone(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("j1", JobKind::Music3, 1).await;

        h.tick().await;
        assert_eq!(
            h.state_of("j1").await,
            JobState::Queued,
            "waiting, not failed"
        );
        assert_eq!(
            h.reason_of("j1").await,
            Some(BlockedReason::ServiceDisabled {
                service: "minimax_h3".into()
            })
        );

        gate.set_service_enabled(ServiceKind::MinimaxH3, true).await;
        h.tick().await;
        assert_eq!(h.state_of("j1").await, JobState::Done);
    }

    /// Preempt and resume: the operator bumps a running job, it stops at its next
    /// checkpoint, and comes back at the head of its band carrying its bank.
    #[tokio::test]
    async fn a_preempted_job_resumes_from_its_checkpoint() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(
            p,
            roomy_gate(),
            &[(JobKind::Music3, Script::WaitForInterrupt)],
        );
        h.add("long", JobKind::Music3, 1).await;
        h.add("waiting", JobKind::Music3, 1).await;
        h.tick().await;
        assert_eq!(h.state_of("long").await, JobState::Running);

        assert!(h.d.interrupt("long", Interrupt::Preempt));
        h.settle().await;

        assert_eq!(h.state_of("long").await, JobState::Queued);
        let order: Vec<String> = store::pending(&h.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(
            order,
            vec!["long", "waiting"],
            "head of its band, not the back"
        );
        let (_, resume) = store::payload(&h.pool, "long").await.unwrap().unwrap();
        assert_eq!(resume.unwrap()["done"], 1, "carrying its checkpoint");

        // And the resume picks up where it left off rather than re-rendering.
        h.runners[0].set_script(Script::Units(4));
        h.tick().await;
        assert_eq!(h.state_of("long").await, JobState::Done);
    }

    /// 🔴 Five queued, #2 fails, the queue CONTINUES. Nobody is watching the logs
    /// at 03:00, and a failure that stops the batch is a lost night.
    #[tokio::test]
    async fn one_failure_does_not_stop_the_batch() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(
            p,
            roomy_gate(),
            &[(JobKind::Music3, Script::Fail("engine died"))],
        );
        for i in 1..=3 {
            h.add(&format!("j{i}"), JobKind::Music3, 1).await;
        }
        h.tick().await;
        h.tick().await;
        h.tick().await;

        for i in 1..=3 {
            assert_eq!(h.state_of(&format!("j{i}")).await, JobState::Failed);
        }
        let job = store::get(&h.pool, "j2").await.unwrap().unwrap();
        assert_eq!(job.error.as_deref(), Some("engine died"));
    }

    /// A dependent waits for its parent and no longer, and it does not hold a
    /// resource while it waits.
    #[tokio::test]
    async fn a_dependent_waits_for_its_parent() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(
            p,
            roomy_gate(),
            &[
                (JobKind::Music3, Script::WaitForInterrupt),
                (JobKind::Krea2, Script::Succeed),
            ],
        );
        h.add("parent", JobKind::Music3, 1).await;
        store::enqueue(
            &h.pool,
            &NewJob {
                id: "child".into(),
                kind: JobKind::Krea2,
                payload: json!({}),
                band: 1,
                target_gpu: None,
                force_pin: false,
                depends_on: Some("parent".into()),
                created_by: "user:t".into(),
                idempotency_key: None,
                units_total: Some(1),
                shape_key: Some("fake/default".into()),
                state: JobState::Queued,
            },
        )
        .await
        .unwrap();

        h.tick().await;
        assert_eq!(h.state_of("child").await, JobState::Queued);
        assert_eq!(
            h.reason_of("child").await,
            Some(BlockedReason::DependencyPending {
                parent: "parent".into()
            })
        );

        h.runners[0].set_script(Script::Succeed);
        h.d.interrupt("parent", Interrupt::Cancel);
        h.settle().await;
        // A cancelled parent cascades — the child is cancelled, never left waiting
        // on something that will never arrive.
        assert_eq!(h.state_of("child").await, JobState::Cancelled);
    }

    /// 🔴 The boot sequence. Reattach settles BEFORE the first dispatch, and it
    /// takes the guard synchronously — waiting for a reconcile tick to observe the
    /// running render would let the queue admit a job onto occupied VRAM.
    #[tokio::test]
    async fn reattach_settles_before_the_first_dispatch() {
        let Some((p, _g)) = pool().await else { return };
        let gate = roomy_gate();
        let h = harness(p, gate.clone(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("in-flight", JobKind::Music3, 1).await;
        h.add("never-sent", JobKind::Music3, 1).await;
        store::claim(&h.pool, "in-flight", 0).await.unwrap();
        store::claim(&h.pool, "never-sent", 0).await.unwrap();
        store::set_server_job_id(&h.pool, "in-flight", "engine-9")
            .await
            .unwrap();

        h.d.bootstrap().await.unwrap();
        h.settle().await;

        let log = h.runners[0].log();
        assert_eq!(log.reattached, vec!["in-flight"]);
        assert!(log.ran.is_empty(), "a reattach is not a fresh run");
        assert_eq!(
            h.state_of("never-sent").await,
            JobState::Queued,
            "a row that never reached the engine goes back in the queue"
        );
    }

    /// Pause is "finish the current job, then stop" — it stops STARTS, and the
    /// running job is left alone.
    #[tokio::test]
    async fn pause_stops_starts_and_not_the_running_job() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(
            p,
            roomy_gate(),
            &[(JobKind::Music3, Script::WaitForInterrupt)],
        );
        h.add("running", JobKind::Music3, 1).await;
        h.tick().await;
        assert_eq!(h.state_of("running").await, JobState::Running);

        h.d.set_paused(true);
        h.add("next", JobKind::Music3, 1).await;
        h.tick().await;
        assert_eq!(h.state_of("running").await, JobState::Running, "left alone");
        assert_eq!(h.reason_of("next").await, Some(BlockedReason::Paused));

        h.d.interrupt("running", Interrupt::Cancel);
    }

    /// A completed run teaches the estimator. Getting this wrong is silent: the
    /// queue works and the ETAs never improve.
    #[tokio::test]
    async fn a_finished_run_is_recorded_against_its_shape() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("j1", JobKind::Music3, 1).await;
        h.clock.advance_secs(0);
        h.tick().await;

        let stat = gpu_job_stats::get(&h.pool, "music3", "fake/default")
            .await
            .unwrap()
            .expect("the run taught the estimator");
        assert_eq!(stat.samples, 1);
    }

    /// 🔴 A direct holder closes the lane, immediately.
    ///
    /// `/oneshot` renders krea2 inline for paperworld, relip drives ltx25 in
    /// place, and songgen's editing modes take multipart audio a queued payload
    /// cannot hold. Each is a second writer to an engine the queue also owns. The
    /// health probe would notice on its next 5 s tick — long enough to submit a
    /// second render into a busy server first — so the census asks the gate, which
    /// already knows.
    #[tokio::test]
    async fn a_guard_held_outside_the_queue_closes_the_lane() {
        let Some((p, _g)) = pool().await else { return };
        let gate = roomy_gate();
        let h = harness(p, gate.clone(), &[(JobKind::Krea2, Script::Succeed)]);
        h.add("j1", JobKind::Krea2, 1).await;

        // Something outside the queue takes krea2 — exactly what `/oneshot` does.
        let outsider = gate.try_acquire(ServiceKind::Krea2).await.unwrap();
        h.tick().await;
        assert_eq!(h.state_of("j1").await, JobState::Queued, "not started");
        assert_eq!(
            h.reason_of("j1").await,
            Some(BlockedReason::LaneBusy {
                lane: "krea2".into()
            })
        );

        // It lets go → the queue proceeds on the very next tick.
        drop(outsider);
        h.tick().await;
        assert_eq!(h.state_of("j1").await, JobState::Done);
    }

    /// ...and the queue's OWN running job must not be mistaken for an outsider,
    /// or a single job would permanently close its own lane behind itself.
    #[tokio::test]
    async fn the_queues_own_guard_is_not_counted_twice() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(
            p,
            roomy_gate(),
            &[
                (JobKind::Krea2, Script::WaitForInterrupt),
                (JobKind::Flux2, Script::Succeed),
            ],
        );
        h.add("krea", JobKind::Krea2, 1).await;
        h.tick().await;
        assert_eq!(h.state_of("krea").await, JobState::Running);

        // A different engine is unaffected by krea2's own in-flight guard.
        h.add("flux", JobKind::Flux2, 1).await;
        h.tick().await;
        assert_eq!(h.state_of("flux").await, JobState::Done);

        h.d.interrupt("krea", Interrupt::Cancel);
    }

    /// 🔴 The lane is released at the engine/disk boundary, with the card. A
    /// `collecting` job has already had its poll say `completed`; holding the lane
    /// through the fetch/mux/harvest tail blocks all four comfy kinds for minutes
    /// on an upscale while the GPU sits idle.
    #[tokio::test]
    async fn a_collecting_job_does_not_hold_its_lane() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("tail", JobKind::Music3, 1).await;
        store::claim(&h.pool, "tail", 0).await.unwrap();
        store::set_server_job_id(&h.pool, "tail", "engine-1")
            .await
            .unwrap();
        store::to_collecting(&h.pool, "tail", 1_000).await.unwrap();

        h.add("next", JobKind::Music3, 1).await;
        h.d.tick().await;
        h.wait_for("next", JobState::Done).await;
        assert_eq!(
            h.state_of("tail").await,
            JobState::Collecting,
            "and the tail is still landing its artifact"
        );
    }

    /// 🔴 Job #2 of 5 fails at 02:00 because the engine went away, and nobody is
    /// watching. It gets another attempt — and the row says it is on its second,
    /// so a retried job never reads as a first try.
    #[tokio::test]
    async fn a_failure_against_a_dead_engine_is_retried() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("j", JobKind::Music3, 1).await;
        store::claim(&h.pool, "j", 0).await.unwrap();
        h.d.inner.health.record(
            Lane::ComfyApi,
            LaneHealth {
                down: Some("connection refused".into()),
                ..Default::default()
            },
        );

        let runner: &dyn JobRunner = h.runners[0].as_ref();
        assert!(
            h.d.requeue_failed("j", JobKind::Music3, runner, "submit: connection refused")
                .await
        );
        let job = store::get(&h.pool, "j").await.unwrap().unwrap();
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.attempt, 2);
    }

    /// 🔴 ...and a failure the engine is perfectly healthy for is NOT retried. A
    /// validation failure or an engine 4xx fails identically every time, and
    /// burning three attempts on it is strictly worse than failing once. Nothing
    /// here reads the error text.
    #[tokio::test]
    async fn a_deterministic_failure_burns_no_attempts() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("j", JobKind::Music3, 1).await;
        store::claim(&h.pool, "j", 0).await.unwrap();

        let runner: &dyn JobRunner = h.runners[0].as_ref();
        assert!(
            !h.d.requeue_failed("j", JobKind::Music3, runner, "steps must be at least 1")
                .await
        );
        assert_eq!(store::get(&h.pool, "j").await.unwrap().unwrap().attempt, 1);
    }

    /// The retry is bounded: a job that keeps failing against a dead engine gives
    /// up rather than owning the queue all night.
    #[tokio::test]
    async fn the_retries_run_out() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("j", JobKind::Music3, 1).await;
        h.d.inner.health.record(
            Lane::ComfyApi,
            LaneHealth {
                down: Some("connection refused".into()),
                ..Default::default()
            },
        );
        let runner: &dyn JobRunner = h.runners[0].as_ref();

        for _ in 0..MAX_ATTEMPTS {
            store::claim(&h.pool, "j", 0).await.unwrap();
            if !h
                .d
                .requeue_failed("j", JobKind::Music3, runner, "connection refused")
                .await
            {
                break;
            }
        }
        assert_eq!(
            store::get(&h.pool, "j").await.unwrap().unwrap().attempt,
            MAX_ATTEMPTS
        );
    }

    /// A kind with no adapter is failed rather than left in a queue that can never
    /// run it — a row nothing can dispatch is worse than an error.
    #[tokio::test]
    async fn a_kind_with_no_adapter_is_not_left_pending() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("orphan", JobKind::Flux2, 1).await;
        h.tick().await;
        assert_eq!(h.state_of("orphan").await, JobState::Failed);
    }

    /// 🔴 Cancelling a row that never ran has to reach the ADAPTER, not just the row. Every terminal
    /// state a kind writes into its own engine-side record is written by the run path, and a queued
    /// row never enters it — so a record left untold says "queued" for the life of the process, is
    /// never swept, and is believed by anything that reads it without folding the row over it. That
    /// is what had the Director re-adopting a dead render every twelve seconds for a day.
    ///
    /// The dependents matter as much as the row itself: they are cancelled by the same click and
    /// their records are just as stranded.
    #[tokio::test]
    async fn abandoning_a_pending_row_tells_its_adapter_and_its_dependents() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        h.add("parent", JobKind::Music3, 1).await;
        h.add_after("child", JobKind::Music3, "parent").await;

        assert!(h.d.abandon_pending("parent").await);

        assert_eq!(h.state_of("parent").await, JobState::Cancelled);
        assert_eq!(h.state_of("child").await, JobState::Cancelled);
        let mut told = h.runners[0].log().abandoned;
        told.sort();
        assert_eq!(told, vec!["child".to_string(), "parent".to_string()]);
    }

    /// A row past the point of no return is not "pending", and the funnel must not pretend
    /// otherwise: it reports nothing cancelled and tells no adapter, so a stray call cannot retire a
    /// record whose render is still on the GPU.
    #[tokio::test]
    async fn abandoning_a_running_row_does_nothing() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(
            p,
            roomy_gate(),
            &[(JobKind::Music3, Script::WaitForInterrupt)],
        );
        h.add("live", JobKind::Music3, 1).await;
        h.tick().await;
        assert_eq!(h.state_of("live").await, JobState::Running);

        assert!(!h.d.abandon_pending("live").await);
        assert_eq!(h.state_of("live").await, JobState::Running);
        assert!(h.runners[0].log().abandoned.is_empty());
    }

    /// 🔴 A page cannot learn "my render started" from progress frames: the adapter emits the first
    /// one whenever it feels like it, which on a render that spends minutes loading a checkpoint is
    /// a silence indistinguishable from nothing having started. Before this the spine carried
    /// progress, blocked and terminal only, so every page that needed the transition polled for it.
    #[tokio::test]
    async fn claiming_a_job_announces_it_on_the_spine() {
        let Some((p, _g)) = pool().await else { return };
        let h = harness(p, roomy_gate(), &[(JobKind::Music3, Script::Succeed)]);
        let mut rx = h.d.subscribe();
        h.add("j1", JobKind::Music3, 1).await;
        h.tick().await;

        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            assert_eq!(e.job_id, "j1");
            seen.push(e.state);
        }
        assert!(
            seen.contains(&JobState::Running),
            "the claim must reach the stream, got {seen:?}"
        );
        assert!(seen.contains(&JobState::Done), "got {seen:?}");
    }
}
