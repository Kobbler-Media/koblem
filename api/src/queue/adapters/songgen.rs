//! songgen (LeVo2) text→song.
//!
//! Shape A (poll-and-collect). The payload IS the songgen-server `POST /generate`
//! body as `routes::songgen::build_generate_body` produces it.
//!
//! 🔴 Not the sd.cpp job model. songgen is its own server: `POST /generate` →
//! `{job_id}`, `GET /job?id=` → `pending | running | done | failed | cancelled`,
//! `POST /job?id=&cancel=1`, `GET /job?id=&download=1`. Five statuses, different
//! words, a different cancel verb, and no 410 — so none of the sd.cpp status
//! handling applies here.
//!
//! Only the `generate` mode is queueable. `/clone` and `/continue` take their
//! vocal / bgm / full-mix stems as multipart audio parts, and a queue payload is
//! JSON that may sit for hours — there is nowhere for those bytes to live.

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
use crate::routes::songgen::ext_for_content_type;
use crate::routes::tier2_helpers::{inject_gpu, record_generation};
use crate::songgen::Error as SonggenError;

/// The shared music gallery bucket, so a queued song sits with every other engine's.
const SERVICE: &str = "music";
const ENDPOINT: &str = "songgen-generate";
const CANCELLED: &str = "cancelled";
const PREEMPTED: &str = "preempted";
/// How often the run checks the cancel token while the client's own poll loop runs.
const TICK: Duration = Duration::from_secs(1);
/// The client's `cancel` is fire-and-forget, so the confirmation is a poll: wait
/// for the job to leave `running`. 🔴 The caller releases the lane on this.
const CANCEL_CONFIRM: Duration = Duration::from_secs(60);

pub struct SonggenAdapter {
    state: AppState,
}

fn verdict_of(e: &SonggenError) -> Failure {
    match e {
        SonggenError::Request(e) => verdict::transport(e),
        SonggenError::Upstream(status, _) => verdict::upstream(*status),
        // `Preempted` never reaches `classify`: `wait` turns it into the PREEMPTED
        // sentinel and the run answers `JobOutcome::Preempted`, which is not a
        // failure at all. `JobFailed` is the engine's own report of a render that
        // died, and a lyric the model broke on breaks it again.
        SonggenError::Preempted | SonggenError::NotConfigured | SonggenError::JobFailed(_) => {
            Failure::Permanent
        }
    }
}

impl SonggenAdapter {
    pub fn new(state: AppState) -> Self {
        SonggenAdapter { state }
    }

    /// Poll to completion, honouring the cancel token. The client's `await_job`
    /// owns the polling (including its own TTS yield); this only wraps it.
    async fn wait(&self, ctx: &JobCtx, sid: &str) -> Result<(), String> {
        let waiter = self.state.songgen.await_job(sid);
        tokio::pin!(waiter);
        let mut ticker = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                done = &mut waiter => {
                    return done.map_err(|e| match e {
                        // The engine yielded to TTS mid-render and cancelled itself.
                        // That is a preempt by a consumer the queue does not schedule,
                        // and the job goes back to the head of its band.
                        SonggenError::Preempted => PREEMPTED.to_string(),
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
                    // songgen's autoregressive stage reports no step count, and a bar
                    // sitting at zero for minutes reads as a hang — the phase label is
                    // the thing worth plumbing.
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
        self.state.songgen.cancel(sid).await;
        match tokio::time::timeout(CANCEL_CONFIRM, self.state.songgen.await_job(sid)).await {
            Ok(_) => Ok(()),
            Err(_) => Err(format!(
                "songgen did not confirm the cancel of job {sid} within {}s",
                CANCEL_CONFIRM.as_secs()
            )),
        }
    }

    /// Write the take under `SONGS_DIR` and file the gallery row. Runs in
    /// `collecting`: the card is free and every byte is in hand.
    async fn collect(
        &self,
        ctx: &JobCtx,
        content_type: &str,
        audio: &bytes::Bytes,
    ) -> Result<(), String> {
        let payload = &ctx.payload;
        let seed = payload.get("seed").and_then(Value::as_i64);
        let filename = format!(
            "{}-songgen-{}.{}",
            file_stamp(),
            short_seed(seed),
            ext_for_content_type(content_type),
        );
        save_clip(&self.state, &filename, audio)
            .await
            .map_err(|e| format!("failed to save rendered song: {e}"))?;

        record_generation(
            &self.state,
            SERVICE,
            ENDPOINT,
            meta(payload),
            vec![GenerationOutput {
                filename: filename.clone(),
                mime: content_type.to_string(),
                size_bytes: audio.len() as i64,
                kind: "audio".to_string(),
            }],
        )
        .await;

        let row = json!({
            "tracks": [{
                "url": format!("/api/v1/songgen/file/{filename}"),
                "filename": filename,
                "seed": seed.unwrap_or(-1),
                "content_type": content_type,
                "bytes": audio.len(),
            }],
        });
        if let Err(e) = store::set_result(&self.state.db, &ctx.job.id, &row).await {
            warn!(job = %ctx.job.id, error = %e, "songgen: could not record the result");
        }
        Ok(())
    }
}

fn meta(payload: &Value) -> Value {
    let mut meta = payload.clone();
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("engine".into(), json!("songgen"));
        obj.insert("mode".into(), json!(ENDPOINT));
    }
    meta
}

/// Coarse on purpose: a per-second key gives every render its own sample of one.
/// `auto` is its own bucket rather than folded into "long" because it is a
/// different question — the model runs to EOS and the length is lyric-driven.
fn duration_bucket(payload: &Value) -> &'static str {
    match payload.get("duration").and_then(Value::as_f64) {
        None => "auto",
        Some(d) => match d as i64 {
            i64::MIN..=60 => "60s",
            61..=120 => "120s",
            121..=240 => "240s",
            _ => "long",
        },
    }
}

#[async_trait]
impl JobRunner for SonggenAdapter {
    fn kind(&self) -> JobKind {
        JobKind::Songgen
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::Songgen
    }

    /// Whole-job retry only — songgen publishes no checkpoint surface, and its own
    /// TTS yield already throws the partial away.
    fn preempt(&self) -> Preempt {
        Preempt::Kill
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        let has = |k: &str| {
            payload
                .get(k)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
        };
        if !has("description") && !has("lyric") {
            return Err(
                "a description or a lyric is required — songgen has nothing to render from \
                 otherwise"
                    .to_string(),
            );
        }
        if let Some(d) = payload.get("duration").and_then(Value::as_f64)
            && d <= 0.0
        {
            return Err("duration must be positive, or omitted to render to EOS".to_string());
        }
        Ok(())
    }

    /// Duration is the dominant term. Takes would multiply it — songgen renders one
    /// per job, so `units_total` is 1 and the key carries only the shape.
    fn shape_key(&self, payload: &Value) -> String {
        format!("songgen/{}", duration_bucket(payload))
    }

    /// 🔴 One. songgen-server returns one track per job; the route's response
    /// carries a single-element `tracks`. There is no batch knob to read.
    fn units(&self, _payload: &Value) -> Option<i32> {
        Some(1)
    }

    fn label(&self, payload: &Value) -> String {
        let desc: String = payload
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| payload.get("lyric").and_then(Value::as_str))
            .unwrap_or("")
            .chars()
            .take(48)
            .collect();
        match payload.get("duration").and_then(Value::as_f64) {
            Some(d) => format!("songgen · {:.0}s · {desc}", d),
            None => format!("songgen · {desc}"),
        }
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.songgen;
        if !client.is_configured() {
            return Err("songgen is not configured".to_string());
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
            warn!(job = %ctx.job.id, error = %e, "songgen: could not record the engine job id");
        }

        // The download stays under the guard, as `routes::songgen` has it: the
        // engine hands the wav over exactly once, and the gate can unload the
        // render child the moment the slot is free.
        let outcome = match self.wait(&ctx, &sid).await {
            Ok(()) => client.download(&sid).await.map_err(|e| {
                verdict::note(
                    format!("downloading the finished song: {e}"),
                    verdict_of(&e),
                )
            }),
            Err(e) => Err(e),
        };
        ctx.release_gpu();

        let (content_type, audio) = match outcome {
            Ok(v) => v,
            Err(e) if e == CANCELLED => return Ok(JobOutcome::Cancelled),
            Err(e) if e == PREEMPTED => return Ok(JobOutcome::Preempted { resume: json!({}) }),
            Err(e) => return Err(e),
        };
        self.collect(&ctx, &content_type, &audio).await?;
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
            Ok(()) => self.state.songgen.download(&sid).await.map_err(|e| {
                verdict::note(
                    format!("downloading the finished song: {e}"),
                    verdict_of(&e),
                )
            }),
            Err(e) => Err(e),
        };
        ctx.release_gpu();
        let (content_type, audio) = match outcome {
            Ok(v) => v,
            Err(e) if e == CANCELLED => return Ok(JobOutcome::Cancelled),
            Err(e) if e == PREEMPTED => return Ok(JobOutcome::Preempted { resume: json!({}) }),
            Err(e) => return Err(e),
        };
        // A reattached render is still a render someone asked for; skipping the
        // gallery row would lose the song purely because koblem restarted.
        self.collect(&ctx, &content_type, &audio).await?;
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

    /// Coarse buckets. A per-second key gives every render its own sample of one
    /// and the estimator never learns anything.
    #[test]
    fn nearby_durations_share_a_bucket() {
        assert_eq!(
            duration_bucket(&json!({"duration": 90.0})),
            duration_bucket(&json!({"duration": 118.0})),
        );
        assert_ne!(
            duration_bucket(&json!({"duration": 30.0})),
            duration_bucket(&json!({"duration": 200.0})),
        );
    }

    /// An omitted duration is a DIFFERENT shape — the model runs to EOS — not a
    /// short one. Folding it into `60s` would poison the cheapest bucket with the
    /// longest renders.
    #[test]
    fn an_omitted_duration_is_its_own_bucket() {
        assert_eq!(duration_bucket(&json!({})), "auto");
        assert_ne!(
            duration_bucket(&json!({})),
            duration_bucket(&json!({"duration": 30.0}))
        );
    }

    /// 🔴 The fleet saying "not now" is worth another slot; the engine saying the
    /// render died is not, and neither is a request it refused as wrong.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: SonggenError| verdict_of(&e);
        assert_eq!(
            t(SonggenError::Upstream(429, "songgen busy".into())),
            Failure::Transient
        );
        assert_eq!(
            t(SonggenError::Upstream(503, "songgen draining".into())),
            Failure::Transient
        );
        assert_eq!(
            t(SonggenError::Upstream(500, "songgen boom".into())),
            Failure::Transient
        );
        assert_eq!(
            t(SonggenError::Upstream(400, "songgen bad lyric".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(SonggenError::Upstream(404, "songgen no job".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(SonggenError::Upstream(409, "songgen held".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(SonggenError::Upstream(410, "songgen expired".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(SonggenError::Upstream(422, "songgen bad dur".into())),
            Failure::Permanent
        );
        assert_eq!(t(SonggenError::NotConfigured), Failure::Permanent);
        assert_eq!(
            t(SonggenError::JobFailed("songgen-job".into())),
            Failure::Permanent
        );
        assert_eq!(t(SonggenError::Preempted), Failure::Permanent);
    }

    /// The queue only ever sees the flattened sentence, so the verdict has to
    /// survive the flattening.
    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = SonggenError::Upstream(503, "songgen is draining for TTS".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Transient);
        let e = SonggenError::Upstream(400, "songgen refused that lyric".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Permanent);
    }
}
