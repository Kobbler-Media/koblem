//! Wan2.2-VACE video.
//!
//! 🔴 **Shape A, not B.** The table said "segmented", but `routes::wan_vace` does
//! not drive a segment loop: it builds ONE chain carrying `segments[]` +
//! `n_segments` and posts it as ONE engine job, which renders and stitches the
//! whole clip internally. koblem never sees a segment boundary — the status route
//! says so in as many words ("The Wan engine never emits progressive shots"), and
//! `WanRenderResult` carries one container, not N. There is therefore no
//! checkpoint to stop at and nothing to hand back, so this is `Preempt::Kill` with
//! `units() == 1` rather than an `AtCheckpoint` that would have to lie.
//!
//! The payload IS the engine chain as the route builds it, plus two koblem-side
//! keys the engine takes as named multipart parts rather than JSON: `init_image`
//! and `ref_image`, each an `asset:<hash>` reference or bare base64.

use std::time::Duration;

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{Job, JobKind, JobProgress, Preempt};
use serde_json::{Value, json};
use tracing::warn;

use super::verdict;
use crate::AppState;
use crate::director_assets::{self, PartUploads};
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::tier2_helpers::{
    OutputFile, generation_file_url, inject_gpu, persist_generation_files,
};
use crate::wan_vace::{Error as WanError, WanRenderResult};

const SERVICE: &str = "wan-vace";
const CANCELLED: &str = "cancelled";
const PREEMPTED: &str = "preempted";
/// How often the run checks the cancel token while the client's own poll loop
/// runs. Not a poll interval — the client owns that.
const TICK: Duration = Duration::from_secs(1);
/// `/cancel` answers 409 while the engine still holds the render. The lane is
/// released on the strength of a confirmed cancel, so 409 is retried.
const CANCEL_TRIES: u32 = 20;
const CANCEL_RETRY: Duration = Duration::from_millis(500);

pub struct WanVaceAdapter {
    state: AppState,
}

fn verdict_of(e: &WanError) -> Failure {
    match e {
        WanError::Request(e) => verdict::transport(e),
        WanError::Upstream(status, _) => verdict::upstream(*status),
        // `Stream` is half a body that stopped arriving and half a disk that would
        // not take it, and nothing here can tell them apart — so it does not spend a
        // second whole-clip render guessing. The rest cannot change between
        // attempts: the engine's own failure report, a cancel, undecodable media, a
        // field it never sent.
        WanError::NotConfigured
        | WanError::JobFailed(..)
        | WanError::Cancelled(_)
        | WanError::MissingField(_)
        | WanError::BadMedia(..)
        | WanError::Stream(_) => Failure::Permanent,
    }
}

impl WanVaceAdapter {
    pub fn new(state: AppState) -> Self {
        WanVaceAdapter { state }
    }

    /// The client's poll loop, wrapped so the row learns the phase and the cancel
    /// token is honoured. The client's `wait` already survives transient poll
    /// failures; this does not re-implement that.
    async fn wait(&self, ctx: &JobCtx, sid: &str) -> Result<WanRenderResult, String> {
        let waiter = self.state.wan_vace.wait(sid);
        tokio::pin!(waiter);
        let mut ticker = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                done = &mut waiter => {
                    return done.map_err(|e| match e {
                        WanError::Cancelled(_) => CANCELLED.to_string(),
                        e => verdict::note(e.to_string(), verdict_of(&e)),
                    });
                }
                _ = ticker.tick() => {
                    match ctx.cancel.get() {
                        // 🔴 The lane is released only once the engine CONFIRMS.
                        Interrupt::Cancel => {
                            self.confirm_cancel(sid).await?;
                            return Err(CANCELLED.to_string());
                        }
                        // Preempt::Kill: there is no checkpoint, so an operator
                        // preempt costs the whole render and the job is requeued.
                        Interrupt::Preempt => {
                            self.confirm_cancel(sid).await?;
                            return Err(PREEMPTED.to_string());
                        }
                        Interrupt::None => {}
                    }
                    // No numbers: no sd.cpp-fork deployment publishes a step count,
                    // and a 0/0 bar reads as finished.
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
    async fn confirm_cancel(&self, sid: &str) -> Result<(), String> {
        for attempt in 0..CANCEL_TRIES {
            match self.state.wan_vace.cancel(sid).await {
                Ok(_) => return Ok(()),
                // Gone (404) or expired (410): nothing is holding the worker.
                Err(WanError::Upstream(404 | 410, _)) => return Ok(()),
                Err(WanError::Upstream(409, _)) if attempt + 1 < CANCEL_TRIES => {
                    tokio::time::sleep(CANCEL_RETRY).await;
                }
                Err(e) => return Err(format!("cancelling wan-vace job {sid}: {e}")),
            }
        }
        Err(format!("wan-vace never confirmed the cancel of job {sid}"))
    }

    /// Where the finished container is streamed to. The wan job store's own dir,
    /// so it is the same filesystem the gallery normally shares — which is what
    /// makes `persist_generation_files` a hard link rather than a 589 MB copy.
    fn staging_path(&self, job_id: &str) -> std::path::PathBuf {
        self.state.queue_files.clip_path(job_id)
    }

    /// Link the clip into the gallery and drop the staging copy.
    ///
    /// The gallery IS the delivery for a queued render — there is no HTTP response
    /// to stream it into — so a persist that fails is a failed job, not a warning.
    async fn collect(&self, ctx: &JobCtx, clip: &std::path::Path) -> Result<(), String> {
        let saved = persist_generation_files(
            &self.state,
            SERVICE,
            "generate",
            meta(&ctx.payload),
            &[OutputFile {
                ext: "webm".to_string(),
                mime: "video/webm".to_string(),
                kind: "video".to_string(),
                path: clip.to_path_buf(),
            }],
        )
        .await;
        // The gallery holds its own link to the same inode, so removing the staging
        // name costs nothing and stops an orphan the job store's sweep would never
        // collect (it only knows ids it minted itself).
        let _ = tokio::fs::remove_file(clip).await;
        let id = saved.ok_or_else(|| {
            "render succeeded but saving to the gallery failed — check `generations_dir` is set \
             and writable"
                .to_string()
        })?;

        // The staging name is gone by here, so the gallery row is the only thing
        // left that points at the clip.
        let row = json!({ "generation_id": id, "url": generation_file_url(&id, 0) });
        if let Err(e) = store::set_result(&self.state.db, &ctx.job.id, &row).await {
            warn!(job = %ctx.job.id, error = %e, "wan-vace: could not record the result");
        }
        Ok(())
    }

    /// Submit, recording the engine's id the moment it accepts.
    async fn submit(&self, ctx: &JobCtx) -> Result<String, String> {
        let mut chain = ctx.payload.clone();
        // The two images the engine wants as NAMED parts, not as JSON. Lifted out
        // before the generic staging walk so they cannot also become `part:` markers.
        let init = self.resolve_named(&mut chain, "init_image").await?;
        let reference = self.resolve_named(&mut chain, "ref_image").await?;
        if let Some(obj) = chain.as_object_mut() {
            if init.is_some() {
                obj.insert("has_init_image".into(), json!(true));
            }
            if reference.is_some() {
                obj.insert("has_reference_image".into(), json!(true));
            }
        }

        // Whatever else the document references from the asset store.
        let staged =
            director_assets::stage_refs(&self.state.director_assets_dir, &mut chain, "ref_")
                .await
                .map_err(|missing| director_assets::missing_refs_message(&missing))?;
        let mut uploads = PartUploads::default();
        uploads.absorb(staged);
        uploads.retain_used_by(&chain);

        // 🔴 The card the dispatcher placed us on. Omitting it lands the worker on
        // its own WORKER_DEFAULT_GPU — which is how a render ends up resident on an
        // administratively disabled card.
        inject_gpu(&mut chain, ctx.gpu.as_deref());

        let sid = self
            .state
            .wan_vace
            .submit(
                &chain,
                init.as_deref(),
                reference.as_deref(),
                uploads.entries(),
            )
            .await
            .map_err(|e| verdict::note(e.to_string(), verdict_of(&e)))?;
        // The one field that makes reattach possible.
        if let Err(e) = store::set_server_job_id(&self.state.db, &ctx.job.id, &sid).await {
            warn!(job = %ctx.job.id, error = %e, "wan-vace: could not record the engine job id");
        }
        Ok(sid)
    }

    /// Take `key` out of the chain and resolve it to bytes (`asset:<hash>` or bare
    /// base64), because the engine reads these two from multipart fields.
    async fn resolve_named(&self, chain: &mut Value, key: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(Value::String(s)) = chain.as_object_mut().and_then(|obj| obj.remove(key)) else {
            return Ok(None);
        };
        if s.trim().is_empty() {
            return Ok(None);
        }
        match director_assets::resolve_media(&self.state.director_assets_dir, &s).await {
            Ok(Some(bytes)) => Ok(Some(bytes.to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("{key}: {e}")),
        }
    }
}

/// What the gallery row records. `segments[]` is prose and worth keeping; nothing
/// here is base64 because both images travel as parts.
fn meta(payload: &Value) -> Value {
    let mut meta = payload.clone();
    if let Some(obj) = meta.as_object_mut() {
        let had_images = obj.remove("init_image").is_some() | obj.remove("ref_image").is_some();
        obj.insert("had_image_inputs".into(), json!(had_images));
        obj.insert("engine".into(), json!("wan-vace"));
    }
    meta
}

fn dims(payload: &Value) -> (i64, i64) {
    (
        payload.get("width").and_then(Value::as_i64).unwrap_or(1280),
        payload.get("height").and_then(Value::as_i64).unwrap_or(720),
    )
}

fn frames(payload: &Value) -> i64 {
    payload.get("frames").and_then(Value::as_i64).unwrap_or(65)
}

fn n_segments(payload: &Value) -> i64 {
    payload
        .get("n_segments")
        .and_then(Value::as_i64)
        .or_else(|| {
            payload
                .get("segments")
                .and_then(Value::as_array)
                .map(|a| a.len() as i64)
        })
        .unwrap_or(1)
        .max(1)
}

#[async_trait]
impl JobRunner for WanVaceAdapter {
    fn kind(&self) -> JobKind {
        JobKind::WanVace
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::WanVace
    }

    /// 🔴 No checkpoint surface — see the module header. One engine job renders and
    /// stitches every segment, so an interrupt costs the whole render either way.
    fn preempt(&self) -> Preempt {
        Preempt::Kill
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        let has_prompt = payload
            .get("segments")
            .and_then(Value::as_array)
            .is_some_and(|segs| {
                segs.iter().any(|s| {
                    s.get("prompt")
                        .and_then(Value::as_str)
                        .is_some_and(|p| !p.trim().is_empty())
                })
            });
        if !has_prompt {
            return Err("at least one segment prompt is required".to_string());
        }
        let (w, h) = dims(payload);
        if w <= 0 || h <= 0 {
            return Err("width and height must be positive".to_string());
        }
        if frames(payload) <= 0 {
            return Err("frames must be positive".to_string());
        }
        // 🔴 There is deliberately no canvas-multiple or frame-grid check here,
        // because the ENGINE has none either: `routes_wan.cpp` renames `frames` to
        // `video_frames` and forwards it, and refuses only a segment/prompt count
        // mismatch, a bad part hash and a full queue. Nothing validates dimensions
        // at any layer.
        //
        // So enqueue-time validation has nothing to replicate — inventing a modulus
        // here would refuse renders the engine would happily have produced, which is
        // the one failure worse than finding out late. (Contrast LTX, which does
        // publish a law: `(frames - 1) % 8 == 0`, refused at submit.)
        Ok(())
    }

    /// Duration scales with pixels x frames. The segment count multiplies it — but
    /// koblem cannot observe segments here (see the module header), so it stays out
    /// of `units` and is left to the estimator's whole-job number.
    fn shape_key(&self, payload: &Value) -> String {
        let (w, h) = dims(payload);
        format!("wan_vace/{w}x{h}/{}f", frames(payload))
    }

    /// 🔴 One. The engine renders every segment inside a single job and publishes
    /// no per-segment progress, so any other number would draw a bar that never
    /// moves off zero.
    /// The shot count, even though the bar can only ever read 0 or N.
    ///
    /// 🔴 This is an ESTIMATOR field before it is a progress field. The engine
    /// renders and stitches every shot inside one job and never emits a segment
    /// boundary, so koblem cannot report partial progress — but an 8-shot render
    /// takes roughly eight times an 8-shot-shaped 1-shot render, and reporting 1
    /// would make `units × unit_p50` estimate them identically. A bar that jumps
    /// is a cosmetic loss; an ETA that is 8x wrong is the thing the queue exists
    /// to get right.
    fn units(&self, payload: &Value) -> Option<i32> {
        Some(n_segments(payload).max(1) as i32)
    }

    fn label(&self, payload: &Value) -> String {
        let (w, h) = dims(payload);
        let segs = n_segments(payload);
        let mode = payload
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("t2v")
            .to_string();
        let prompt: String = payload
            .pointer("/segments/0/prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();
        format!("wan-vace · {mode} · {segs} shots · {w}x{h} · {prompt}")
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        if !self.state.wan_vace.is_configured() {
            return Err("wan_vace is not configured".to_string());
        }
        let sid = self.submit(&ctx).await?;
        let clip = self.staging_path(&ctx.job.id);

        let outcome = match self.wait(&ctx, &sid).await {
            // 🔴 Fetch the container BEFORE releasing the card. The engine holds it
            // only in the CUDA child's RAM — nothing stitched reaches its volume —
            // so the next job to take the guard can unload that child and take this
            // render with it. This IS the engine/disk boundary for the fleet.
            Ok(result) => self
                .state
                .wan_vace
                .media_to_file(&sid, &result, &clip)
                .await
                .map(|_| ())
                .map_err(|e| {
                    verdict::note(format!("fetching the finished clip: {e}"), verdict_of(&e))
                }),
            Err(e) => Err(e),
        };
        ctx.release_gpu();

        match outcome {
            Ok(()) => {}
            Err(e) if e == CANCELLED => return Ok(JobOutcome::Cancelled),
            Err(e) if e == PREEMPTED => return Ok(JobOutcome::Preempted { resume: json!({}) }),
            Err(e) => return Err(e),
        }

        self.collect(&ctx, &clip).await?;
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
        let clip = self.staging_path(&ctx.job.id);
        let outcome = match self.wait(&ctx, &sid).await {
            Ok(result) => self
                .state
                .wan_vace
                .media_to_file(&sid, &result, &clip)
                .await
                .map(|_| ())
                .map_err(|e| {
                    verdict::note(format!("fetching the finished clip: {e}"), verdict_of(&e))
                }),
            Err(e) => Err(e),
        };
        ctx.release_gpu();
        match outcome {
            Ok(()) => {}
            Err(e) if e == CANCELLED => return Ok(JobOutcome::Cancelled),
            Err(e) if e == PREEMPTED => return Ok(JobOutcome::Preempted { resume: json!({}) }),
            Err(e) => return Err(e),
        }
        self.collect(&ctx, &clip).await?;
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

    fn key(payload: &Value) -> String {
        let (w, h) = dims(payload);
        format!("wan_vace/{w}x{h}/{}f", frames(payload))
    }

    /// The shot count multiplies the render but is not the shape — otherwise a
    /// 6-shot render at a resolution already measured has no history at all.
    #[test]
    fn the_shot_count_is_not_in_the_key() {
        let one = json!({"width": 1280, "height": 720, "frames": 65, "n_segments": 1});
        let six = json!({"width": 1280, "height": 720, "frames": 65, "n_segments": 6});
        assert_eq!(key(&one), key(&six));
        assert_ne!(
            key(&one),
            key(&json!({"width": 1280, "height": 720, "frames": 129}))
        );
    }

    /// `n_segments` and a bare `segments[]` are the same statement.
    #[test]
    fn the_shot_count_reads_either_spelling() {
        assert_eq!(n_segments(&json!({"n_segments": 3})), 3);
        assert_eq!(
            n_segments(&json!({"segments": [{"prompt": "a"}, {"prompt": "b"}]})),
            2
        );
        assert_eq!(n_segments(&json!({})), 1);
    }

    /// 🔴 A whole-clip render is the most expensive thing in the queue and there is
    /// no checkpoint to resume from, so only the fleet saying "not now" and a wire
    /// that broke buy a second one.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: WanError| verdict_of(&e);
        assert_eq!(
            t(WanError::Upstream(429, "wan queue full".into())),
            Failure::Transient
        );
        assert_eq!(
            t(WanError::Upstream(503, "wan draining".into())),
            Failure::Transient
        );
        assert_eq!(
            t(WanError::Upstream(500, "wan boom".into())),
            Failure::Transient
        );
        assert_eq!(
            t(WanError::Upstream(400, "wan bad chain".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(WanError::Upstream(404, "wan no job".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(WanError::Upstream(409, "wan held".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(WanError::Upstream(410, "wan expired".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(WanError::Upstream(422, "wan bad parts".into())),
            Failure::Permanent
        );
        assert_eq!(t(WanError::NotConfigured), Failure::Permanent);
        assert_eq!(t(WanError::Cancelled("wan-job".into())), Failure::Permanent);
        assert_eq!(
            t(WanError::JobFailed("wan-job".into(), "node raised".into())),
            Failure::Permanent
        );
        assert_eq!(t(WanError::MissingField("video")), Failure::Permanent);
        assert_eq!(
            t(WanError::BadMedia("wan-job".into(), "not base64".into())),
            Failure::Permanent
        );
    }

    /// The queue only ever sees the flattened sentence, so the verdict has to
    /// survive the flattening.
    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = WanError::Upstream(429, "wan pending queue is full".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Transient);
        let e = WanError::Upstream(400, "wan refused that chain".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Permanent);
    }
}
