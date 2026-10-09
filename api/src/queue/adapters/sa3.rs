//! Stable Audio 3 text→audio.
//!
//! Shape A (poll-and-collect). The payload IS the sa3-server `POST /generate` body
//! as `routes::sa3::build_generate_body` produces it.
//!
//! 🔴 Not the sd.cpp job model, despite sharing the fleet's name. sa3-server is
//! its own service: `POST /generate` → `{job_id}`, `GET /job?id=` →
//! `pending | running | done | failed | cancelled`, `POST /job?id=&cancel=1`,
//! `GET /job?id=&download=1`. No 410, no `/sdcpp/v1` prefix, a different cancel
//! verb — none of the sd.cpp status handling applies.
//!
//! Only the `generate` mode is queueable. `/edit` (a2a, inpaint, continuation)
//! takes its source audio as a multipart part, and a queue payload is JSON that
//! may sit for hours — there is nowhere for those bytes to live.

use std::time::Duration;

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{GenerationOutput, Job, JobKind, JobProgress, Preempt};
use serde_json::{Value, json};
use tracing::warn;

use super::verdict;
use crate::AppState;
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::music3::{file_stamp, save_clip, short_seed};
use crate::routes::tier2_helpers::{inject_gpu, record_generation};
use crate::sa3::Error as Sa3Error;

/// The shared music gallery bucket, so a queued clip sits with every other
/// engine's.
const SERVICE: &str = "music";
const ENDPOINT: &str = "sa3-generate";
const CANCELLED: &str = "cancelled";
const PREEMPTED: &str = "preempted";
/// How often the run checks the cancel token while the client's own poll loop runs.
const TICK: Duration = Duration::from_secs(1);
/// The client's `cancel` is fire-and-forget, so the confirmation is a poll: wait
/// for the job to leave `running`. 🔴 The caller releases the lane on this.
const CANCEL_CONFIRM: Duration = Duration::from_secs(60);

pub struct Sa3Adapter {
    state: AppState,
}

fn verdict_of(e: &Sa3Error) -> Failure {
    match e {
        Sa3Error::Request(e) => verdict::transport(e),
        Sa3Error::Upstream(status, _) => verdict::upstream(*status),
        // `Preempted` never reaches `classify`: `wait` turns it into the PREEMPTED
        // sentinel and the run answers `JobOutcome::Preempted`, which is not a
        // failure at all. `JobFailed` is the engine's own report of a render that
        // died, and a prompt the model broke on breaks it again.
        Sa3Error::Preempted | Sa3Error::NotConfigured | Sa3Error::JobFailed(_) => {
            Failure::Permanent
        }
    }
}

impl Sa3Adapter {
    pub fn new(state: AppState) -> Self {
        Sa3Adapter { state }
    }

    /// Poll to completion, honouring the cancel token. The client's `await_job`
    /// owns the polling (including its own TTS yield); this only wraps it.
    async fn wait(&self, ctx: &JobCtx, sid: &str) -> Result<(), String> {
        let waiter = self.state.sa3.await_job(sid);
        tokio::pin!(waiter);
        let mut ticker = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                done = &mut waiter => {
                    return done.map_err(|e| match e {
                        // The engine yielded to TTS mid-render and cancelled itself.
                        // That is a preempt by a consumer the queue does not schedule,
                        // and the job goes back to the head of its band.
                        Sa3Error::Preempted => PREEMPTED.to_string(),
                        e => verdict::note(e.to_string(), verdict_of(&e)),
                    });
                }
                _ = ticker.tick() => {
                    match ctx.cancel.get() {
                        Interrupt::Cancel => {
                            self.confirm_cancel(sid).await?;
                            return Err(CANCELLED.to_string());
                        }
                        // Preempt::Kill: no checkpoint, so the whole take is lost and
                        // the job is requeued.
                        Interrupt::Preempt => {
                            self.confirm_cancel(sid).await?;
                            return Err(PREEMPTED.to_string());
                        }
                        Interrupt::None => {}
                    }
                    // sa3 publishes no step count; the phase label is the thing worth
                    // plumbing, and a 0/0 bar would read as finished.
                    ctx.progress(
                        JobProgress {
                            phase: Some("rendering".into()),
                            ..Default::default()
                        },
                        0,
                    )
                    .await;
                }
            }
        }
    }

    /// 🔴 Does not return until the engine has confirmed.
    ///
    /// The client's `cancel` swallows its response, so the confirmation is
    /// `await_job` returning: done, failed, cancelled or a record that no longer
    /// answers all mean the worker has let go, and that is the question the lane
    /// release depends on.
    async fn confirm_cancel(&self, sid: &str) -> Result<(), String> {
        self.state.sa3.cancel(sid).await;
        match tokio::time::timeout(CANCEL_CONFIRM, self.state.sa3.await_job(sid)).await {
            Ok(_) => Ok(()),
            Err(_) => Err(format!(
                "sa3 did not confirm the cancel of job {sid} within {}s",
                CANCEL_CONFIRM.as_secs()
            )),
        }
    }

    /// Write the take under `SONGS_DIR` and file the gallery row. Runs in
    /// `collecting`: the card is free and every byte is in hand.
    async fn collect(&self, ctx: &JobCtx, audio: &bytes::Bytes) -> Result<(), String> {
        // SA3 always emits wav — `routes::sa3` discards the engine's content type
        // for the same reason.
        let payload = &ctx.payload;
        let seed = payload.get("seed").and_then(Value::as_i64);
        let filename = format!("{}-sa3-{}.wav", file_stamp(), short_seed(seed));
        save_clip(&self.state, &filename, audio)
            .await
            .map_err(|e| format!("failed to save rendered clip: {e}"))?;

        record_generation(
            &self.state,
            SERVICE,
            ENDPOINT,
            meta(payload),
            vec![GenerationOutput {
                filename: filename.clone(),
                mime: "audio/wav".to_string(),
                size_bytes: audio.len() as i64,
                kind: "audio".to_string(),
            }],
        )
        .await;

        let row = json!({
            "tracks": [{
                "url": format!("/api/v1/sa3/file/{filename}"),
                "filename": filename,
                "seed": seed.unwrap_or(-1),
                "content_type": "audio/wav",
                "bytes": audio.len(),
            }],
        });
        if let Err(e) = store::set_result(&self.state.db, &ctx.job.id, &row).await {
            warn!(job = %ctx.job.id, error = %e, "sa3: could not record the result");
        }
        Ok(())
    }
}

fn meta(payload: &Value) -> Value {
    let mut meta = payload.clone();
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("engine".into(), json!("sa3"));
        obj.insert("mode".into(), json!("generate"));
        obj.insert("had_audio_inputs".into(), json!(false));
    }
    meta
}

/// Coarse on purpose: SA3 is the SHORT engine (game SFX, stings, short music), so
/// the buckets sit where its renders actually land rather than mirroring songgen's.
/// `auto` is its own bucket — an omitted length is the server's default, not a
/// short clip.
fn duration_bucket(payload: &Value) -> &'static str {
    match payload.get("seconds").and_then(Value::as_f64) {
        None => "auto",
        Some(s) => match s as i64 {
            i64::MIN..=15 => "15s",
            16..=45 => "45s",
            46..=90 => "90s",
            _ => "long",
        },
    }
}

#[async_trait]
impl JobRunner for Sa3Adapter {
    fn kind(&self) -> JobKind {
        JobKind::Sa3
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::Sa3
    }

    /// Whole-job retry only — sa3 publishes no checkpoint surface, and its own TTS
    /// yield already throws the partial away.
    fn preempt(&self) -> Preempt {
        Preempt::Kill
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        if payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            return Err("a prompt is required — describe the sound".to_string());
        }
        if let Some(s) = payload.get("seconds").and_then(Value::as_f64)
            && s <= 0.0
        {
            return Err(
                "seconds must be positive, or omitted to take the engine's default".to_string(),
            );
        }
        if let Some(st) = payload.get("steps").and_then(Value::as_i64)
            && st < 1
        {
            return Err("steps must be at least 1".to_string());
        }
        Ok(())
    }

    /// Length is the dominant term. Takes would multiply it — sa3 renders one per
    /// job, so `units_total` is 1 and the key carries only the shape.
    fn shape_key(&self, payload: &Value) -> String {
        format!("sa3/{}", duration_bucket(payload))
    }

    /// 🔴 One. sa3-server returns one clip per job; the route's response carries a
    /// single-element `tracks`. There is no batch knob to read.
    fn units(&self, _payload: &Value) -> Option<i32> {
        Some(1)
    }

    fn label(&self, payload: &Value) -> String {
        let prompt: String = payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(48)
            .collect();
        match payload.get("seconds").and_then(Value::as_f64) {
            Some(s) => format!("sa3 · {:.0}s · {prompt}", s),
            None => format!("sa3 · {prompt}"),
        }
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.sa3;
        if !client.is_configured() {
            return Err("sa3 is not configured".to_string());
        }

        let mut body = ctx.payload.clone();
        // 🔴 The card the dispatcher placed us on. Omitting it lands the render on
        // the worker's own WORKER_DEFAULT_GPU — which is how a worker ends up
        // resident on an administratively disabled card.
        inject_gpu(&mut body, ctx.gpu.as_deref());

        let sid = client
            .submit_generate(&body)
            .await
            .map_err(|e| verdict::note(e.to_string(), verdict_of(&e)))?;
        // The one field that makes reattach possible.
        if let Err(e) = store::set_server_job_id(&self.state.db, &ctx.job.id, &sid).await {
            warn!(job = %ctx.job.id, error = %e, "sa3: could not record the engine job id");
        }

        // The download stays under the guard, as `routes::sa3` has it: the engine
        // hands the wav over exactly once, and the gate can unload the render child
        // the moment the slot is free.
        let outcome = match self.wait(&ctx, &sid).await {
            Ok(()) => client
                .download(&sid)
                .await
                .map(|(_ct, bytes)| bytes)
                .map_err(|e| {
                    verdict::note(
                        format!("downloading the finished clip: {e}"),
                        verdict_of(&e),
                    )
                }),
            Err(e) => Err(e),
        };
        ctx.release_gpu();

        let audio = match outcome {
            Ok(v) => v,
            Err(e) if e == CANCELLED => return Ok(JobOutcome::Cancelled),
            Err(e) if e == PREEMPTED => return Ok(JobOutcome::Preempted { resume: json!({}) }),
            Err(e) => return Err(e),
        };
        self.collect(&ctx, &audio).await?;
        ctx.progress(
            JobProgress {
                phase: Some("completed".into()),
                ..Default::default()
            },
            1,
        )
        .await;
        Ok(JobOutcome::Done { warnings: vec![] })
    }

    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        let Some(sid) = ctx.job.server_job_id.clone() else {
            return Err("reattach with no engine job id".to_string());
        };
        let outcome = match self.wait(&ctx, &sid).await {
            Ok(()) => self
                .state
                .sa3
                .download(&sid)
                .await
                .map(|(_ct, bytes)| bytes)
                .map_err(|e| {
                    verdict::note(
                        format!("downloading the finished clip: {e}"),
                        verdict_of(&e),
                    )
                }),
            Err(e) => Err(e),
        };
        ctx.release_gpu();
        let audio = match outcome {
            Ok(v) => v,
            Err(e) if e == CANCELLED => return Ok(JobOutcome::Cancelled),
            Err(e) if e == PREEMPTED => return Ok(JobOutcome::Preempted { resume: json!({}) }),
            Err(e) => return Err(e),
        };
        // A reattached render is still a render someone asked for; skipping the
        // gallery row would lose the clip purely because koblem restarted.
        self.collect(&ctx, &audio).await?;
        ctx.progress(
            JobProgress {
                phase: Some("completed".into()),
                ..Default::default()
            },
            1,
        )
        .await;
        Ok(JobOutcome::Done { warnings: vec![] })
    }

    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(sid) = job.server_job_id.as_deref() else {
            return Ok(());
        };
        self.confirm_cancel(sid).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Coarse buckets, sized for the SHORT engine. A per-second key gives every
    /// render its own sample of one.
    #[test]
    fn nearby_lengths_share_a_bucket() {
        assert_eq!(
            duration_bucket(&json!({"seconds": 20.0})),
            duration_bucket(&json!({"seconds": 44.0})),
        );
        assert_ne!(
            duration_bucket(&json!({"seconds": 8.0})),
            duration_bucket(&json!({"seconds": 60.0})),
        );
    }

    /// An omitted length is the engine's own default, not a short clip.
    #[test]
    fn an_omitted_length_is_its_own_bucket() {
        assert_eq!(duration_bucket(&json!({})), "auto");
        assert_ne!(
            duration_bucket(&json!({})),
            duration_bucket(&json!({"seconds": 10.0}))
        );
    }

    /// 🔴 The fleet saying "not now" is worth another slot; the engine saying the
    /// render died is not, and neither is a request it refused as wrong.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: Sa3Error| verdict_of(&e);
        assert_eq!(
            t(Sa3Error::Upstream(429, "sa3 busy".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Sa3Error::Upstream(503, "sa3 draining".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Sa3Error::Upstream(500, "sa3 boom".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Sa3Error::Upstream(400, "sa3 bad steps".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(Sa3Error::Upstream(404, "sa3 no job".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(Sa3Error::Upstream(409, "sa3 held".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(Sa3Error::Upstream(410, "sa3 expired".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(Sa3Error::Upstream(422, "sa3 bad seconds".into())),
            Failure::Permanent
        );
        assert_eq!(t(Sa3Error::NotConfigured), Failure::Permanent);
        assert_eq!(t(Sa3Error::JobFailed("sa3-job".into())), Failure::Permanent);
        assert_eq!(t(Sa3Error::Preempted), Failure::Permanent);
    }

    /// The queue only ever sees the flattened sentence, so the verdict has to
    /// survive the flattening.
    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = Sa3Error::Upstream(503, "sa3 is draining for TTS".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Transient);
        let e = Sa3Error::Upstream(400, "sa3 refused those steps".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Permanent);
    }
}
