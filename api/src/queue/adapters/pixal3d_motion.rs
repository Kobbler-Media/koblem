//! Pixal3D motion — a prompt into a retargetable clip. ~11 s warm, ~33 s cold.
//!
//! Shape A: submit, poll, release, record. One unit, because the engine renders one clip per
//! request and publishes no progress inside it.
//!
//! 🔴 `Preempt::None`, for the same reason as `pixal3d_rig`: the gate deliberately refuses to
//! unload longcat-rig, which runs its own FIFO and admits work koblem cannot see. Preempting an
//! ~11 s render would be pointless even if it were safe.
//!
//! 🔴 **The clip is kept as a PATH.** `/v1/animate` accepts a motion job id, but that id lives in a
//! 512-slot map while `/out` keeps the file forever — a clip generated this morning would stop
//! animating this afternoon. Nothing is fetched to koblem's disk here: the clip is only ever handed
//! back to the same engine.
//!
//! # Payload
//!
//! ```json
//! { "motion_id": "mot-…", "prompt": "a person walks forward", "seconds": 4.0, "seed": 7 }
//! ```
//!
//! i.e. a [`MotionSubmit`] plus the library record it belongs to. `gpu` in the payload is ignored:
//! the card is the dispatcher's decision.

use std::time::Duration;

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{Job, JobKind, JobProgress, Preempt};
use serde_json::{Value, json};
use tracing::warn;

use super::verdict;
use crate::AppState;
use crate::pixal3d::{self as engine, MotionSubmit};
use crate::pixal3d_jobs::JobState as RecordState;
use crate::queue::adapters::pixal3d_rig::{
    Stopped, cancel_and_confirm, placement, poll, verdict_of,
};
use crate::queue::runner::{Failure, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::pixal3d::describe_submit;

/// A motion is over in ~11 s, so the poll is tight enough that the answer is not mostly latency.
const MOTION_POLL: Duration = Duration::from_millis(500);

pub struct Pixal3dMotionAdapter {
    state: AppState,
}

impl Pixal3dMotionAdapter {
    pub fn new(state: AppState) -> Self {
        Pixal3dMotionAdapter { state }
    }

    /// Close out the library record from a completed engine job. Shared by `run` and `reattach`,
    /// because a clip rendered before a restart is still a clip the user asked for.
    async fn collect(&self, ctx: &JobCtx, motion_id: &str, job: &engine::EngineJob) -> RunResult {
        let jobs = &self.state.pixal3d_jobs;
        let Some(clip) = engine::clip_path(job) else {
            jobs.update_motion(motion_id, |m| {
                m.state = RecordState::Failed;
                m.error = Some("the motion completed but published no clip path".into());
            });
            return Err("the motion completed but published no clip path".to_string());
        };
        let frames = job
            .result
            .get("frames")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let fps = job.result.get("fps").and_then(Value::as_f64).unwrap_or(0.0);
        let resident = job
            .result
            .get("dit_was_resident")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        jobs.update_motion(motion_id, |m| {
            m.state = RecordState::Done;
            m.clip_path = Some(clip.clone());
            m.frames = frames;
            m.fps = fps;
            m.dit_was_resident = resident;
        });
        let row = json!({
            "motion_id": motion_id,
            "clip_path": clip,
            "frames": frames,
            "fps": fps,
            "dit_was_resident": resident,
        });
        if let Err(e) = store::set_result(&self.state.db, &ctx.job.id, &row).await {
            warn!(job = %ctx.job.id, error = %e, "pixal3d-motion: could not record the result");
        }
        ctx.progress(
            JobProgress {
                phase: Some("clip stored".into()),
                ..Default::default()
            },
            1,
        )
        .await;
        Ok(JobOutcome::Done { warnings: vec![] })
    }

    async fn finish(&self, ctx: &JobCtx, motion_id: &str, engine_id: &str) -> RunResult {
        let polled = poll(ctx, &self.state.pixal3d, engine_id, MOTION_POLL).await;
        // 🔴 THE ENGINE/DISK BOUNDARY: the clip stays on the engine's volume, so everything past
        // here is a record update — the card must not idle through it.
        ctx.release_gpu();
        match polled {
            Ok(job) => self.collect(ctx, motion_id, &job).await,
            Err(Stopped::Cancelled) => {
                self.state.pixal3d_jobs.update_motion(motion_id, |m| {
                    m.state = RecordState::Cancelled;
                    m.error = Some("cancelled".into());
                });
                Ok(JobOutcome::Cancelled)
            }
            Err(Stopped::Failed(msg)) => {
                self.state.pixal3d_jobs.update_motion(motion_id, |m| {
                    m.state = RecordState::Failed;
                    m.error = Some(msg.clone());
                });
                Err(msg)
            }
        }
    }
}

struct MotionPlan {
    motion_id: String,
    submit: MotionSubmit,
}

fn plan(payload: &Value) -> Result<MotionPlan, String> {
    let motion_id = payload
        .get("motion_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(
            "`motion_id` is required — the library record is the only thing that keeps the clip's \
             path, and the path is what `/v1/animate` is handed",
        )?
        .to_string();
    let submit: MotionSubmit = serde_json::from_value(payload.clone())
        .map_err(|e| format!("this is not a pixal3d motion request: {e}"))?;
    if submit.prompt.trim().is_empty() {
        return Err("`prompt` is required".to_string());
    }
    if let Some(s) = submit.seconds
        && !(0.1..=60.0).contains(&s)
    {
        return Err("`seconds` must be between 0.1 and 60".to_string());
    }
    Ok(MotionPlan { motion_id, submit })
}

/// Coarse on purpose: the render is near-constant and duration is the only term that moves it, so a
/// per-second key would give every prompt its own sample of one.
fn bucket(payload: &Value) -> &'static str {
    match payload.get("seconds").and_then(Value::as_f64) {
        None => "default",
        Some(s) if s <= 2.0 => "2s",
        Some(s) if s <= 5.0 => "5s",
        Some(s) if s <= 10.0 => "10s",
        Some(_) => "long",
    }
}

#[async_trait]
impl JobRunner for Pixal3dMotionAdapter {
    fn kind(&self) -> JobKind {
        JobKind::Pixal3dMotion
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::Pixal3d
    }

    /// 🔴 Never preempted — see the module header and `pixal3d_rig`.
    fn preempt(&self) -> Preempt {
        Preempt::None
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        plan(payload).map(|_| ())
    }

    fn shape_key(&self, payload: &Value) -> String {
        format!("pixal3d_motion/{}", bucket(payload))
    }

    fn units(&self, _payload: &Value) -> Option<i32> {
        Some(1)
    }

    fn label(&self, payload: &Value) -> String {
        let prompt = payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(48)
            .collect::<String>();
        format!("pixal3d motion · {prompt}")
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let plan = plan(&ctx.payload)?;
        let client = &self.state.pixal3d;
        if !client.is_configured() {
            return Err("pixal3d is not configured".to_string());
        }
        let mut submit = plan.submit;
        // 🔴 The card the scheduler reserved. Without it the submit lands on the worker's own
        // WORKER_DEFAULT_GPU — which is how a worker ends up resident on a disabled card.
        submit.gpu = placement(client, ctx.gpu.as_deref()).await?;

        let engine_id = client
            .motion(&submit)
            .await
            .map_err(|e| verdict::note(describe_submit(&e), verdict_of(&e)))?;
        // The field that makes reattach possible, written before anything else can fail.
        if let Err(e) = store::set_server_job_id(&self.state.db, &ctx.job.id, &engine_id).await {
            warn!(job = %ctx.job.id, error = %e, "pixal3d_motion: could not record the engine job id");
        }
        self.state.pixal3d_jobs.update_motion(&plan.motion_id, |m| {
            m.state = RecordState::Running;
            m.engine_job_id = Some(engine_id.clone());
        });
        ctx.progress(
            JobProgress {
                phase: Some("generating the clip".into()),
                ..Default::default()
            },
            0,
        )
        .await;

        self.finish(&ctx, &plan.motion_id, &engine_id).await
    }

    /// The handle is the engine JOB ID, exactly as the route's own reattach uses it: the submit
    /// already happened and an ~11 s clip is very likely finished. The clip PATH is what a later
    /// animate is handed, not a way back into a job in flight.
    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        let plan = plan(&ctx.payload)?;
        let Some(sid) = ctx.job.server_job_id.clone() else {
            self.state.pixal3d_jobs.update_motion(&plan.motion_id, |m| {
                m.state = RecordState::Failed;
                m.error = Some("koblem restarted before the engine accepted this motion".into());
            });
            return Err(
                "koblem restarted before the engine accepted this motion, so there is nothing to \
                 re-attach to"
                    .to_string(),
            );
        };
        self.finish(&ctx, &plan.motion_id, &sid).await
    }

    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(sid) = job.server_job_id.as_deref() else {
            return Ok(());
        };
        cancel_and_confirm(&self.state.pixal3d, sid).await
    }

    /// The library record a queued motion would have landed in — see the rig adapter's `abandon`.
    async fn abandon(&self, _job_id: &str, payload: &Value) {
        let Some(mid) = payload.get("motion_id").and_then(Value::as_str) else {
            return;
        };
        self.state
            .pixal3d_jobs
            .cancel_motion(mid, "cancelled before the motion started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Nearby durations share a bucket; an absent duration is its own, because the engine's default
    /// is a real length and guessing it would poison the measured ones.
    #[test]
    fn durations_bucket_coarsely() {
        assert_eq!(
            bucket(&json!({"seconds": 3.0})),
            bucket(&json!({"seconds": 4.9}))
        );
        assert_ne!(
            bucket(&json!({"seconds": 3.0})),
            bucket(&json!({"seconds": 30.0}))
        );
        assert_eq!(bucket(&json!({"prompt": "walk"})), "default");
    }

    /// A clip with no library record is unreachable the moment the engine evicts its job record, so
    /// it is refused at enqueue rather than rendered into nothing.
    #[test]
    fn a_motion_without_a_record_is_refused_at_enqueue() {
        assert!(plan(&json!({"prompt": "a person walks forward"})).is_err());
        assert!(plan(&json!({"motion_id": "mot-1", "prompt": "  "})).is_err());
        assert!(plan(&json!({"motion_id": "mot-1", "prompt": "walk", "seconds": 90.0})).is_err());
        assert!(plan(&json!({"motion_id": "mot-1", "prompt": "walk", "seconds": 4.0})).is_ok());
    }

    /// The verdict is `pixal3d_rig`'s — one service, one error type — and it has to survive
    /// `describe_submit`, which is the only thing the queue ever sees.
    #[test]
    fn the_verdict_survives_describe_submit() {
        let e = engine::Error::Upstream(503, "motion queue_full".into());
        assert_eq!(
            verdict::recall(&verdict::note(describe_submit(&e), verdict_of(&e))),
            Failure::Transient
        );
        let e = engine::Error::Upstream(422, "motion queue_full".into());
        assert_eq!(
            verdict::recall(&verdict::note(describe_submit(&e), verdict_of(&e))),
            Failure::Permanent
        );
    }
}
