//! The engine adapter contract.
//!
//! The dispatcher is engine-agnostic; each kind implements this. It is not a
//! rewrite of anything — every existing runner already has exactly this shape
//! (submit, poll, collect, persist), and boot already calls five separate
//! reattach functions by hand.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use async_trait::async_trait;
use kob_gpu_gate::{Acquisition, ServiceKind};
use koblem_client::types::{Job, JobKind, JobProgress, Preempt};
use serde_json::Value;

use super::lane::Lane;

pub type RunResult = Result<JobOutcome, String>;

/// Whether a failed run is worth another attempt.
///
/// 🔴 Default-deny, and NEVER inferred from the message text. A validation
/// failure or an engine 4xx fails identically every time, so burning three
/// attempts on it is strictly worse than failing once — and a substring match on
/// an error sentence is exactly how a permanent failure gets retried all night.
/// An adapter opts a failure in only where it still holds the typed error (an
/// upstream status code, a transport error) that can say so honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Permanent,
    Transient,
}

/// How a run ended. A plain `Err(String)` is a terminal failure and goes into the
/// row's `error` column.
#[derive(Debug, Clone)]
pub enum JobOutcome {
    /// 🔴 `warnings` rides along to the notification. A render that "succeeded"
    /// having silently dropped a generation looks exactly like "I forgot to press
    /// render", and nobody is reading docker logs at 3am.
    Done {
        warnings: Vec<String>,
    },
    /// Stopped at a checkpoint by operator preempt. Goes back to the HEAD of its
    /// band carrying this state.
    Preempted {
        resume: Value,
    },
    Cancelled,
}

/// What the dispatcher is asking a running job to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    None,
    /// Stop as soon as possible and lose the partial.
    Cancel,
    /// Stop at the next checkpoint and hand back resume state.
    Preempt,
}

/// A cooperative stop flag, polled by the runner at its checkpoints.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicU8>,
}

impl CancelToken {
    pub fn request(&self, what: Interrupt) {
        let v = match what {
            Interrupt::None => 0,
            Interrupt::Cancel => 1,
            Interrupt::Preempt => 2,
        };
        self.flag.store(v, Ordering::Relaxed);
    }

    pub fn get(&self) -> Interrupt {
        match self.flag.load(Ordering::Relaxed) {
            1 => Interrupt::Cancel,
            2 => Interrupt::Preempt,
            _ => Interrupt::None,
        }
    }

    pub fn is_stopping(&self) -> bool {
        self.get() != Interrupt::None
    }
}

/// Writes progress back to the job row.
#[async_trait]
pub trait ProgressSink: Send + Sync {
    async fn set(&self, progress: JobProgress, units_done: i32);
}

/// Everything a run needs, and the two release levers it must pull.
pub struct JobCtx {
    pub job: Job,
    pub payload: Value,
    /// Checkpoint state from a previous preempt. `None` = start from the top.
    pub resume: Option<Value>,
    pub cancel: CancelToken,
    pub sink: Arc<dyn ProgressSink>,
    /// The card the dispatcher placed this on — threaded into the engine's
    /// per-request `gpu` field.
    ///
    /// 🔴 A submit that omits it lands on the worker's own `WORKER_DEFAULT_GPU`,
    /// which is how a worker ends up resident on an administratively DISABLED
    /// card. Every gated submit must carry it.
    pub gpu: Option<String>,
    /// Held here so the runner cannot forget to release it, and taken by
    /// [`JobCtx::release_gpu`].
    acquisition: std::sync::Mutex<Option<Acquisition>>,
    released: Arc<AtomicBool>,
    /// Fired exactly once, when the guard is dropped. The dispatcher uses it to
    /// move the row `running -> collecting` and stamp `render_ms`, which is the
    /// only place that transition can honestly be made: nobody but the runner
    /// knows where the engine/disk boundary is.
    on_release: Box<dyn Fn() + Send + Sync>,
}

impl JobCtx {
    pub fn new(
        job: Job,
        payload: Value,
        resume: Option<Value>,
        cancel: CancelToken,
        sink: Arc<dyn ProgressSink>,
        acquisition: Option<Acquisition>,
        on_release: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        let gpu = acquisition
            .as_ref()
            .and_then(|a| a.chosen_gpu().map(str::to_string));
        JobCtx {
            job,
            payload,
            resume,
            cancel,
            sink,
            gpu,
            acquisition: std::sync::Mutex::new(acquisition),
            released: Arc::new(AtomicBool::new(false)),
            on_release,
        }
    }

    /// 🔴 Call this at the ENGINE/DISK BOUNDARY, not at the end of `run`.
    ///
    /// Every runner today drops the guard by hand the moment engine work finishes
    /// and before the disk/DB work, and that is why `collecting` exists as a state
    /// at all: the card is already free while koblem fetches artifacts,
    /// concatenates, muxes and harvests. On an upscale that tail is minutes. If
    /// the dispatcher held the guard until `run` returned, the queue would idle a
    /// card through every harvest.
    ///
    /// It releases the LANE with it, because the engine really is finished with
    /// this job — its poll said `completed` — which is why the dispatcher's lane
    /// census counts `running` rows and not `collecting` ones. The cancel path is
    /// the exception: a cancelled comfy job does not free the comfy slot the
    /// instant koblem decides it did, so there the lane is released only once
    /// [`JobRunner::cancel`] has confirmed with the engine — and a cancel can only
    /// arrive while the job is still `running`, before this is ever called.
    ///
    /// Past this point the job is in `collecting` and is **no longer cancellable**
    /// — the artifact is landing, the GPU cost is already paid, and killing it
    /// there loses the only copy.
    pub fn release_gpu(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        drop(self.acquisition.lock().unwrap().take());
        (self.on_release)();
    }

    pub fn gpu_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }

    pub async fn progress(&self, progress: JobProgress, units_done: i32) {
        self.sink.set(progress, units_done).await;
    }
}

#[async_trait]
pub trait JobRunner: Send + Sync {
    fn kind(&self) -> JobKind;
    /// The VRAM claim. The gate prices it from `gpu_engine_config.peak_mb`, which
    /// is DB-editable, so this never encodes a number.
    fn service(&self) -> ServiceKind;
    fn lane(&self) -> Lane {
        Lane::of(self.kind())
    }
    fn preempt(&self) -> Preempt;

    /// How the dispatcher should treat a failed run. See [`Failure`].
    ///
    /// Override it only from a typed upstream error the adapter still has in
    /// hand; the default refuses to retry anything, which is the safe answer.
    fn classify(&self, _error: &str) -> Failure {
        Failure::Permanent
    }

    /// Cheap, stable arithmetic validation, run at ENQUEUE.
    ///
    /// 🔴 The queue stretches the feedback loop from minutes to hours. comfy-api
    /// validates the H3 frame-phase rule at submit *specifically because* the
    /// node's own raise "arrives minutes into a render, after the DiT and the text
    /// encoder have loaded" — and a request enqueued at 22:00 that 400s at 03:00
    /// is a night of nothing, when the user was sitting in front of the form.
    ///
    /// Only the rules that are pure arithmetic and cannot drift (frame grids,
    /// phase rules, canvas multiples, dimension caps) — never the whole contract.
    /// A job that cannot possibly succeed must never enter the queue.
    fn validate(&self, _payload: &Value) -> Result<(), String> {
        Ok(())
    }

    /// The estimator bucket. 🔴 A shape, not a service: an LTX render's duration
    /// scales with segments × frames × resolution and an image's with steps ×
    /// pixels, so a per-service average is useless. Coarse buckets on purpose.
    fn shape_key(&self, payload: &Value) -> String;

    /// How many units (segments / generations / tracks) this payload will render.
    /// `None` when the payload does not say — the estimator then has only the
    /// whole-job number.
    fn units(&self, payload: &Value) -> Option<i32>;

    /// A short label for the queue row, built server-side so the page never has to
    /// know how to read thirteen payload shapes.
    fn label(&self, payload: &Value) -> String;

    /// Submit → poll → collect → persist. Receives the acquisition on the ctx and
    /// never acquires; calls [`JobCtx::release_gpu`] at the engine/disk boundary.
    async fn run(&self, ctx: JobCtx) -> RunResult;

    /// Re-poll an engine job that outlived a koblem restart.
    ///
    /// 🔴 comfy-api job records expire in TEN MINUTES, so a reattach that takes
    /// longer than that finds nothing. Artifact collection cannot be deferred
    /// behind other work.
    async fn reattach(&self, ctx: JobCtx) -> RunResult;

    /// Stop the engine-side job. 🔴 Must not return until the engine has
    /// CONFIRMED, because the caller releases the lane on the strength of it: a
    /// forced unload on H3 is not a SIGKILL — it cancels a render and waits out a
    /// diffusion step — and a lane released early means the next job submits into
    /// a busy ComfyUI and the failure reads as an engine bug.
    async fn cancel(&self, job: &Job) -> Result<(), String>;

    /// Retire the engine-side record for a row the queue killed BEFORE it ever ran.
    ///
    /// 🔴 Not [`cancel`](Self::cancel), which stops a render that is on the engine and must wait
    /// for the engine to confirm. Nothing is on the engine here: the row was `queued` or `held`, so
    /// [`run`](Self::run) — where every terminal state a kind writes into its own record is
    /// written — is never entered. A kind that keeps a record therefore leaves it saying "queued"
    /// forever, with no task left to correct it.
    ///
    /// That is not theoretical. One such record kept the Director re-adopting a dead render every
    /// twelve seconds for a day, tearing the stage down and re-downloading the project's footage
    /// each time, because `/active` believed the record over the queue.
    ///
    /// Default no-op: most kinds keep nothing beyond the queue row itself, and for them the row's
    /// own `cancelled` IS the whole truth.
    async fn abandon(&self, _job_id: &str, _payload: &Value) {}
}
