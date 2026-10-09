//! LTX-Video 0.9.8 2B — the fast, video-only engine.
//!
//! 🔴 **Shape A, not B.** Like [`super::wan_vace`], `routes::ltx_2b` builds ONE
//! chain carrying `segments[]` + `n_segments` and posts it as ONE engine job; the
//! engine renders every shot and stitches them. koblem sees a submit and a
//! completion, never a segment boundary, and the poll body carries no per-segment
//! progress.
//!
//! # The bank is what makes stopping cheap
//!
//! The render writes `seg_<n>.bin` itself, the moment window *n* finishes sampling,
//! onto a named volume. No cancel path deletes it (running, queued and the gate's
//! forced cancel all only flip status; job expiry drops the record and never
//! touches disk), and the gate's unload cannot reach it — that is SIGKILL +
//! waitpid on the CUDA child, and the bank is not in the child. A resume counts
//! those files to decide where to restart, so a preempt costs the shot in flight
//! and nothing else. Hence `AtCheckpoint`, and four consequences that are all
//! load-bearing:
//!
//! * 🔴 A resume FORKS the bank into a directory named after the NEW job id,
//!   hard-linking the shots it keeps. Every preempt→resume cycle therefore mints a
//!   fresh handle, and the checkpoint has to carry the LATEST one.
//! * 🔴 `resume_from == 0` answers **404** — killed before shot 0 landed. That is
//!   "render the whole thing", not a failure.
//! * 🔴 A resumed submit that carries audio is a hard **400**: the engine
//!   rediscovers the track from the bank.
//! * 🔴 `seg_<n>.bin` is written straight (`ofstream` truncate, no temp+rename), so
//!   the SIGKILL this queue's own preempt fires can leave a truncated file that the
//!   existence scan still counts. The engine refuses to render past it rather than
//!   rendering garbage; the recovery is one retry that re-renders that last shot.

use std::time::Duration;

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{Job, JobKind, JobProgress, Preempt};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use super::verdict;
use crate::AppState;
use crate::director_assets::{self, PartUploads};
use crate::ltx_2b::{Error as Ltx2bError, Ltx2bRenderResult};
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::tier2_helpers::{
    OutputFile, generation_file_url, inject_gpu, persist_generation_files,
};

const SERVICE: &str = "ltx-2b";
/// How often the run checks the cancel token while the client's own poll loop
/// runs. Not a poll interval — the client owns that.
const TICK: Duration = Duration::from_secs(1);
/// `/cancel` answers 409 while the engine still holds the render. The lane is
/// released on the strength of a confirmed cancel, so 409 is retried.
const CANCEL_TRIES: u32 = 20;
const CANCEL_RETRY: Duration = Duration::from_millis(500);

/// How a render stopped short.
enum Stop {
    Cancelled,
    Preempted,
    /// A shot the bank scan counted but the loader could not read — the truncated
    /// tail a SIGKILL leaves behind.
    BankLoad(String),
    Failed(String),
}

enum SubmitStop {
    /// The 404 a resume gets when the bank holds nothing to continue from.
    NoBank(String),
    Failed(String),
}

fn verdict_of(e: &Ltx2bError) -> Failure {
    match e {
        Ltx2bError::Request(e) => verdict::transport(e),
        Ltx2bError::Upstream(status, _) => verdict::upstream(*status),
        // 🔴 `JobFailed` includes the truncated-bank tail, and `run` already spends ONE retry on
        // that itself — calling it transient here would turn one recovery into six renders. The
        // rest cannot change between attempts.
        Ltx2bError::NotConfigured
        | Ltx2bError::JobFailed(..)
        | Ltx2bError::Cancelled(_)
        | Ltx2bError::MissingField(_)
        | Ltx2bError::BadMedia(..)
        | Ltx2bError::Stream(_) => Failure::Permanent,
    }
}

/// Where a run picks up. Reached from the queue's checkpoint and from the caller's
/// own `resume_job_id` alike — they are the same engine mechanism.
#[derive(Debug, Clone, Default, Deserialize)]
struct Resume {
    #[serde(default)]
    resume_job_id: String,
    /// The floor the progress bar starts from. Only a floor: the next submit
    /// re-derives the true count from the bank on disk.
    #[serde(default)]
    segments_done: i32,
    #[serde(default)]
    retake_segment: Option<u32>,
}

struct Submitted {
    sid: String,
    /// Shots the engine restored from the bank, straight off the submit response.
    restored: u32,
}

pub struct Ltx2bAdapter {
    state: AppState,
}

impl Ltx2bAdapter {
    pub fn new(state: AppState) -> Self {
        Ltx2bAdapter { state }
    }

    /// The client's poll loop, wrapped so the row learns the phase and the cancel
    /// token is honoured. The client's `wait` already survives transient poll
    /// failures; this does not re-implement that.
    ///
    /// `banked` is what a resume restored, and is the floor the bar reads from —
    /// the engine publishes nothing per shot, so the count cannot rise until the
    /// job ends.
    async fn wait(&self, ctx: &JobCtx, sid: &str, banked: i32) -> Result<Ltx2bRenderResult, Stop> {
        let waiter = self.state.ltx_2b.wait(sid);
        tokio::pin!(waiter);
        let mut ticker = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                done = &mut waiter => {
                    return done.map_err(|e| match e {
                        Ltx2bError::Cancelled(_) => Stop::Cancelled,
                        Ltx2bError::JobFailed(_, ref msg) if bank_load_failed(msg) => {
                            Stop::BankLoad(e.to_string())
                        }
                        e => Stop::Failed(verdict::note(e.to_string(), verdict_of(&e))),
                    });
                }
                _ = ticker.tick() => {
                    let interrupt = ctx.cancel.get();
                    if interrupt != Interrupt::None {
                        // 🔴 The lane is released only once the engine CONFIRMS.
                        self.confirm_cancel(sid).await.map_err(Stop::Failed)?;
                        return Err(match interrupt {
                            Interrupt::Preempt => Stop::Preempted,
                            _ => Stop::Cancelled,
                        });
                    }
                    // No numbers: no sd.cpp-fork deployment publishes a step count,
                    // and a 0/0 bar reads as finished.
                    ctx.progress(
                        JobProgress {
                            phase: Some("rendering".into()),
                            ..Default::default()
                        },
                        banked,
                    )
                    .await;
                }
            }
        }
    }

    /// 🔴 Does not return until the engine has confirmed.
    async fn confirm_cancel(&self, sid: &str) -> Result<(), String> {
        for attempt in 0..CANCEL_TRIES {
            match self.state.ltx_2b.cancel(sid).await {
                Ok(_) => return Ok(()),
                // Gone (404) or expired (410): nothing is holding the worker.
                Err(Ltx2bError::Upstream(404 | 410, _)) => return Ok(()),
                Err(Ltx2bError::Upstream(409, _)) if attempt + 1 < CANCEL_TRIES => {
                    tokio::time::sleep(CANCEL_RETRY).await;
                }
                Err(e) => return Err(format!("cancelling ltx-2b job {sid}: {e}")),
            }
        }
        Err(format!("ltx-2b never confirmed the cancel of job {sid}"))
    }

    /// Where the finished container is streamed to. The ltx-2b job store's own dir,
    /// so it is the same filesystem the gallery normally shares — which is what
    /// makes `persist_generation_files` a hard link rather than a 589 MB copy.
    fn staging_path(&self, job_id: &str) -> std::path::PathBuf {
        self.state.queue_files.clip_path(job_id)
    }

    /// Link the clip into the gallery and drop the staging copy. The gallery IS the
    /// delivery for a queued render, so a persist that fails is a failed job.
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
            warn!(job = %ctx.job.id, error = %e, "ltx-2b: could not record the result");
        }
        Ok(())
    }

    async fn submit(&self, ctx: &JobCtx, resume: Option<&Resume>) -> Result<Submitted, SubmitStop> {
        let mut chain = ctx.payload.clone();
        // The caller's own continuation and the queue's checkpoint are one field, so
        // the payload's copy is cleared and re-stated rather than merged with.
        drop_resume(&mut chain);
        if let Some(r) = resume {
            apply_resume(&mut chain, r);
            // 🔴 A resumed submit carrying audio is a hard 400 — the engine
            // rediscovers the track from the bank. Stripped before the refs are
            // staged, so those bytes are never read off disk either.
            strip_audio(&mut chain);
        }
        // Every `asset:<hash>` anywhere in the chain — the opener, per-shot scene
        // images, keyframe pins — becomes a `part:` marker plus its bytes, which is
        // exactly what the route does and what keeps base64 out of the job row.
        let staged =
            director_assets::stage_refs(&self.state.director_assets_dir, &mut chain, "ref_")
                .await
                .map_err(|missing| {
                    SubmitStop::Failed(director_assets::missing_refs_message(&missing))
                })?;
        let mut uploads = PartUploads::default();
        uploads.absorb(staged);
        uploads.retain_used_by(&chain);

        // 🔴 The card the dispatcher placed us on. Omitting it lands the worker on
        // its own WORKER_DEFAULT_GPU — which is how a render ends up resident on an
        // administratively disabled card.
        inject_gpu(&mut chain, ctx.gpu.as_deref());

        let (sid, body) = match self.state.ltx_2b.submit(&chain, uploads.entries()).await {
            Ok(accepted) => accepted,
            Err(Ltx2bError::Upstream(404, msg)) if resume.is_some() => {
                return Err(SubmitStop::NoBank(msg));
            }
            Err(e) => {
                return Err(SubmitStop::Failed(verdict::note(
                    e.to_string(),
                    verdict_of(&e),
                )));
            }
        };
        // The one field that makes reattach — and the engine's own bank resume —
        // possible.
        if let Err(e) = store::set_server_job_id(&self.state.db, &ctx.job.id, &sid).await {
            warn!(job = %ctx.job.id, error = %e, "ltx-2b: could not record the engine job id");
        }
        Ok(Submitted {
            sid,
            restored: restored_of(&body),
        })
    }
}

/// What a resume actually recovered, off the submit response.
fn restored_of(body: &Value) -> u32 {
    body.get("resume_from")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}

/// 🔴 The engine fails the render rather than sampling on top of a half-written
/// latent, and names the bank when it does.
fn bank_load_failed(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("bank") || m.contains("latent")
}

/// The checkpoint a preempt hands back.
///
/// 🔴 The id is the one we just submitted UNDER, never the one we resumed from: a
/// resume forks the bank into a directory named after the new job, so the older
/// handle names a bank that stops where this run started.
fn checkpoint(submitted: &Submitted) -> Value {
    json!({
        "resume_job_id": submitted.sid,
        "segments_done": submitted.restored,
    })
}

/// The second attempt after a bank the engine could not load: re-render the last
/// shot it counted. `None` when there is no shot to name — nothing survived, so the
/// render starts over.
fn recover(previous: &Resume, restored: u32) -> Option<Resume> {
    Some(Resume {
        retake_segment: Some(restored.checked_sub(1)?),
        ..previous.clone()
    })
}

/// Where this run picks up. The queue's checkpoint wins over the payload's own
/// `resume_job_id` — it is the newer statement of the same thing.
fn resume_state(ctx: &JobCtx) -> Result<Option<Resume>, String> {
    if let Some(v) = ctx.resume.as_ref() {
        let r: Resume = serde_json::from_value(v.clone())
            .map_err(|e| format!("unreadable ltx-2b resume state: {e}"))?;
        if !r.resume_job_id.is_empty() {
            return Ok(Some(r));
        }
    }
    Ok(ctx
        .payload
        .get("resume_job_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|id| Resume {
            resume_job_id: id.to_string(),
            ..Resume::default()
        }))
}

fn apply_resume(chain: &mut Value, resume: &Resume) {
    let Some(obj) = chain.as_object_mut() else {
        return;
    };
    obj.insert("resume_job_id".into(), json!(resume.resume_job_id));
    if let Some(k) = resume.retake_segment {
        obj.insert("retake_segment".into(), json!(k));
    }
}

fn drop_resume(chain: &mut Value) {
    let Some(obj) = chain.as_object_mut() else {
        return;
    };
    obj.remove("resume_job_id");
    obj.remove("retake_segment");
}

fn strip_audio(chain: &mut Value) {
    let Some(obj) = chain.as_object_mut() else {
        return;
    };
    obj.retain(|k, _| !k.starts_with("audio"));
    let Some(segments) = obj.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    for seg in segments {
        if let Some(o) = seg.as_object_mut() {
            o.retain(|k, _| !k.starts_with("audio"));
        }
    }
}

/// What the gallery row records. Image inputs travelled as parts, so the chain is
/// already free of base64 by the time it gets here.
fn meta(payload: &Value) -> Value {
    let mut meta = payload.clone();
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("engine".into(), json!("ltx-2b"));
    }
    meta
}

fn dims(payload: &Value) -> (i64, i64) {
    (
        payload.get("width").and_then(Value::as_i64).unwrap_or(1216),
        payload.get("height").and_then(Value::as_i64).unwrap_or(704),
    )
}

fn frames(payload: &Value) -> i64 {
    payload
        .get("segment_frames")
        .and_then(Value::as_i64)
        .unwrap_or(97)
}

/// The first frame count in this payload that is not on the `8k+1` grid, checking
/// each segment's own override before the job-wide default.
fn bad_frame_grid(payload: &Value) -> Option<i64> {
    let per_segment = payload
        .get("segments")
        .and_then(Value::as_array)
        .map(|segs| {
            segs.iter()
                .filter_map(|s| s.get("frames").and_then(Value::as_i64))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let counts = if per_segment.is_empty() {
        vec![frames(payload)]
    } else {
        per_segment
    };
    counts.into_iter().find(|f| *f > 0 && (f - 1) % 8 != 0)
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
impl JobRunner for Ltx2bAdapter {
    fn kind(&self) -> JobKind {
        JobKind::Ltx2b
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::Ltx2b
    }

    /// 🔴 The bank is what makes this cheap: the render writes each shot as it
    /// finishes sampling, no cancel path deletes it and the gate's unload cannot
    /// reach it, so stopping costs the shot in flight and nothing else.
    fn preempt(&self) -> Preempt {
        Preempt::AtCheckpoint
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
        // The route's own floor, and the only frame rule koblem states anywhere for
        // the 2B (`req.segment_frames.unwrap_or(97).max(9)`).
        if frames(payload) < 9 {
            return Err("segment_frames must be at least 9".to_string());
        }
        // 🔴 The frame grid, refused here rather than at dispatch. The engine states
        // it: `routes_ltx.cpp` answers "each LTX segment frames value must be 8k+1"
        // on `(frames - 1) % 8 != 0`. Under a queue that 400 would otherwise arrive
        // hours after the form was open.
        //
        // Refused, never snapped — the same rule as LTX-2.5's, and for the same
        // reason: the delivered length of a shot is what the caller's audio timeline
        // is sliced against, so a snapped shot desynchronises every shot after it and
        // presents as bad lip-sync rather than as a bad number.
        //
        // There is deliberately no canvas check: the engine has none. Nothing in
        // `routes_ltx.cpp` or `routes_wan.cpp` validates width or height at any layer.
        if let Some(bad) = bad_frame_grid(payload) {
            let nearest = bad - (bad - 1) % 8;
            return Err(format!(
                "frames must be 8k+1 — {bad} is not, and the engine refuses it rather than \
                 snapping it, because the delivered length is what your audio timeline is sliced \
                 against. The nearest legal count below is {nearest}."
            ));
        }
        Ok(())
    }

    /// Duration scales with pixels x frames. The shot count multiplies it, and is
    /// `units_total` — folding it in here would give a twelve-shot render its own
    /// sample of one instead of twelve samples of a shot.
    fn shape_key(&self, payload: &Value) -> String {
        let (w, h) = dims(payload);
        format!("ltx_2b/{w}x{h}/{}f", frames(payload))
    }

    /// The shot count — both halves of what `units` is for.
    ///
    /// 🔴 As an ESTIMATOR field: an 8-shot render takes roughly eight times an
    /// 8-shot-shaped 1-shot render, and reporting 1 would make `units × unit_p50`
    /// estimate them identically.
    ///
    /// 🔴 As a PROGRESS field: the engine still publishes no shot boundary mid-job,
    /// but a resumed render starts at what the bank restored, so the bar reads that
    /// out of N rather than zero.
    fn units(&self, payload: &Value) -> Option<i32> {
        Some(n_segments(payload).max(1) as i32)
    }

    fn label(&self, payload: &Value) -> String {
        let (w, h) = dims(payload);
        let segs = n_segments(payload);
        let prompt: String = payload
            .pointer("/segments/0/prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();
        let resumed = payload
            .get("resume_job_id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if resumed {
            format!("ltx-2b · resume · {segs} shots · {w}x{h} · {prompt}")
        } else {
            format!("ltx-2b · {segs} shots · {w}x{h} · {prompt}")
        }
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        if !self.state.ltx_2b.is_configured() {
            return Err("ltx_2b is not configured".to_string());
        }
        let clip = self.staging_path(&ctx.job.id);
        let mut resume = resume_state(&ctx)?;
        let mut warnings: Vec<String> = Vec::new();
        let mut recovered = false;

        loop {
            let submitted = match self.submit(&ctx, resume.as_ref()).await {
                Ok(s) => s,
                // 🔴 `resume_from == 0`: the kill landed before shot 0 finished, so
                // there is nothing to continue from. A full render is the answer, not
                // an error — and the card is already ours.
                Err(SubmitStop::NoBank(msg)) => {
                    warnings.push(format!(
                        "the shots this render was continuing from are gone ({msg}), so it was \
                         rendered from the first shot instead"
                    ));
                    resume = None;
                    continue;
                }
                Err(SubmitStop::Failed(e)) => return Err(e),
            };
            let banked = submitted.restored as i32;

            let outcome = match self.wait(&ctx, &submitted.sid, banked).await {
                // 🔴 Fetch the container BEFORE releasing the card. The engine holds
                // it only in the CUDA child's RAM, so whatever takes the guard next —
                // including the gate's own eviction of this worker — can unload that
                // child and take the render with it.
                Ok(result) => self
                    .state
                    .ltx_2b
                    .media_to_file(&submitted.sid, &result, &clip)
                    .await
                    .map(|_| ())
                    .map_err(|e| {
                        Stop::Failed(verdict::note(
                            format!("fetching the finished clip: {e}"),
                            verdict_of(&e),
                        ))
                    }),
                Err(stop) => Err(stop),
            };

            // 🔴 The truncated tail this queue's own preempt can leave behind. One
            // retry, re-rendering the last shot the bank counted; the card has not
            // been released, so it costs that shot and no scheduling.
            if let Err(Stop::BankLoad(msg)) = &outcome
                && !recovered
                && let Some(previous) = resume.as_ref()
            {
                recovered = true;
                let next = recover(previous, submitted.restored);
                warnings.push(match next.as_ref().and_then(|n| n.retake_segment) {
                    Some(k) => format!(
                        "shot {} came back from the bank unreadable ({msg}), so it was re-rendered",
                        k + 1
                    ),
                    None => format!(
                        "the banked shots were unreadable ({msg}), so the render started over"
                    ),
                });
                resume = next;
                continue;
            }

            // WARM-KEEP, as `routes::ltx_2b` does it: on success the 2B worker is
            // loaded and reap is disabled, so the placement is converted to a warm
            // Standing BEFORE the guard drops. It stays evictable — the renderers can
            // still take the card — but an idle 2B is not re-forked cold for the next
            // shot. Not seeded on failure: a broken worker must not hold budget.
            if outcome.is_ok() {
                self.state.gpu.seed_resident(ServiceKind::Ltx2b);
            }
            ctx.release_gpu();

            match outcome {
                Ok(()) => break,
                Err(Stop::Cancelled) => return Ok(JobOutcome::Cancelled),
                Err(Stop::Preempted) => {
                    return Ok(JobOutcome::Preempted {
                        resume: checkpoint(&submitted),
                    });
                }
                Err(Stop::BankLoad(e) | Stop::Failed(e)) => return Err(e),
            }
        }

        self.collect(&ctx, &clip).await?;
        ctx.progress(
            JobProgress {
                phase: Some("completed".into()),
                ..Default::default()
            },
            n_segments(&ctx.payload) as i32,
        )
        .await;
        Ok(JobOutcome::Done { warnings })
    }

    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        let Some(sid) = ctx.job.server_job_id.clone() else {
            return Err("reattach with no engine job id".to_string());
        };
        let clip = self.staging_path(&ctx.job.id);
        // Whatever the run that submitted this restored. The engine job is already in
        // flight, so there is nothing left to re-derive it from.
        let banked = resume_state(&ctx)?.map(|r| r.segments_done).unwrap_or(0);
        let outcome = match self.wait(&ctx, &sid, banked).await {
            Ok(result) => self
                .state
                .ltx_2b
                .media_to_file(&sid, &result, &clip)
                .await
                .map(|_| ())
                .map_err(|e| {
                    Stop::Failed(verdict::note(
                        format!("fetching the finished clip: {e}"),
                        verdict_of(&e),
                    ))
                }),
            Err(stop) => Err(stop),
        };
        if outcome.is_ok() {
            self.state.gpu.seed_resident(ServiceKind::Ltx2b);
        }
        ctx.release_gpu();
        match outcome {
            Ok(()) => {}
            Err(Stop::Cancelled) => return Ok(JobOutcome::Cancelled),
            // The id this reattached to is the one the bank is named after, forked or
            // not, so the checkpoint is the same statement here as in `run`.
            Err(Stop::Preempted) => {
                return Ok(JobOutcome::Preempted {
                    resume: checkpoint(&Submitted {
                        sid,
                        restored: banked.max(0) as u32,
                    }),
                });
            }
            Err(Stop::BankLoad(e) | Stop::Failed(e)) => return Err(e),
        }
        self.collect(&ctx, &clip).await?;
        ctx.progress(
            JobProgress {
                phase: Some("completed".into()),
                ..Default::default()
            },
            n_segments(&ctx.payload) as i32,
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
        format!("ltx_2b/{w}x{h}/{}f", frames(payload))
    }

    /// The shot count multiplies the render but is not the shape.
    #[test]
    fn the_shot_count_is_not_in_the_key() {
        let one = json!({"width": 1216, "height": 704, "segment_frames": 97, "n_segments": 1});
        let eight = json!({"width": 1216, "height": 704, "segment_frames": 97, "n_segments": 8});
        assert_eq!(key(&one), key(&eight));
        assert_ne!(
            key(&one),
            key(&json!({"width": 1216, "height": 704, "segment_frames": 193}))
        );
    }

    /// The chain's own defaults, so an omitted field lands in the bucket the route
    /// would actually have rendered rather than one of its own.
    #[test]
    fn the_defaults_match_the_route() {
        assert_eq!(key(&json!({})), "ltx_2b/1216x704/97f");
    }

    /// 🔴 The whole preempt→resume→preempt cycle in one assertion: a resume forks the
    /// bank into a directory named after the NEW job, so a second preempt that handed
    /// back the id it resumed FROM would point the third run at a bank that stops
    /// where the second one started.
    #[test]
    fn every_preempt_carries_the_newest_bank_id() {
        let first = checkpoint(&Submitted {
            sid: "srv-1".into(),
            restored: 0,
        });
        assert_eq!(first["resume_job_id"], json!("srv-1"));

        // The run that resumes `srv-1` is submitted as `srv-2` and restores 3 shots.
        let second = checkpoint(&Submitted {
            sid: "srv-2".into(),
            restored: 3,
        });
        assert_eq!(second["resume_job_id"], json!("srv-2"));
        assert_eq!(second["segments_done"], json!(3));
    }

    /// A resumed submit names its bank and carries no audio — the engine rediscovers
    /// the track from the bank and 400s a request that re-uploads it.
    #[test]
    fn a_resumed_chain_names_its_bank_and_drops_its_audio() {
        let mut chain = json!({
            "segments": [{"prompt": "a shot", "audio_b64": "…"}, {"prompt": "another"}],
            "audio_track": "asset:beef",
            "audio_fill_gaps": true,
            "width": 1216,
        });
        apply_resume(
            &mut chain,
            &Resume {
                resume_job_id: "srv-2".into(),
                segments_done: 3,
                retake_segment: Some(2),
            },
        );
        strip_audio(&mut chain);
        assert_eq!(chain["resume_job_id"], json!("srv-2"));
        assert_eq!(chain["retake_segment"], json!(2));
        assert_eq!(chain["width"], json!(1216));
        assert!(chain.get("audio_track").is_none(), "{chain}");
        assert!(chain.get("audio_fill_gaps").is_none(), "{chain}");
        assert!(chain["segments"][0].get("audio_b64").is_none(), "{chain}");
        assert_eq!(chain["segments"][0]["prompt"], json!("a shot"));
    }

    /// The 404 fallback has to be a genuinely full render, so the caller's own
    /// `resume_job_id` — the same field, forwarded verbatim by the route — goes with
    /// it. Otherwise the re-submit 404s again for the same reason.
    #[test]
    fn the_fresh_fallback_drops_a_callers_own_resume() {
        let mut chain = json!({
            "segments": [{"prompt": "a shot"}],
            "resume_job_id": "srv-gone",
            "retake_segment": 4,
        });
        drop_resume(&mut chain);
        assert!(chain.get("resume_job_id").is_none(), "{chain}");
        assert!(chain.get("retake_segment").is_none(), "{chain}");
    }

    /// The truncated `seg_<n>.bin` recovery: re-render the LAST shot the bank
    /// counted, keeping the bank; with nothing counted there is no shot to name and
    /// the render starts over.
    #[test]
    fn the_bank_retry_re_renders_the_last_counted_shot() {
        let previous = Resume {
            resume_job_id: "srv-2".into(),
            segments_done: 3,
            retake_segment: None,
        };
        let next = recover(&previous, 3).expect("3 restored shots leave one to retake");
        assert_eq!(next.retake_segment, Some(2));
        assert_eq!(next.resume_job_id, "srv-2");
        assert!(recover(&previous, 0).is_none());
    }

    /// The submit response is the only statement of how much of the bank survived.
    #[test]
    fn the_restored_count_comes_off_the_submit_response() {
        assert_eq!(restored_of(&json!({"id": "srv-2", "resume_from": 3})), 3);
        assert_eq!(restored_of(&json!({"id": "srv-1"})), 0);
    }

    /// A bank the engine refused to load is the one failure worth another submit;
    /// anything else is terminal.
    #[test]
    fn only_a_bank_failure_earns_the_retry() {
        assert!(bank_load_failed(
            "failed to load latent bank entry seg_2.bin"
        ));
        assert!(!bank_load_failed("CUDA out of memory"));
    }
    /// 🔴 The engine states this grid and REFUSES rather than snapping: a snapped
    /// shot desynchronises every shot after it against the caller's audio timeline
    /// and presents as bad lip-sync rather than as a bad number. Under a queue the
    /// engine's own 400 would land hours after the form was open, so it is checked
    /// at enqueue.
    #[test]
    fn frames_off_the_8k1_grid_are_caught() {
        let job = |f: i64| json!({"width": 1216, "height": 704, "segment_frames": f});
        assert_eq!(bad_frame_grid(&job(97)), None, "97 = 8*12+1");
        assert_eq!(bad_frame_grid(&job(9)), None);
        assert_eq!(bad_frame_grid(&job(193)), None);
        assert_eq!(bad_frame_grid(&job(100)), Some(100));
        assert_eq!(bad_frame_grid(&job(96)), Some(96));
        // The message names the nearest legal count BELOW, which is the one the
        // caller can safely drop to without lengthening the shot.
        let bad = 100;
        assert_eq!(bad - (bad - 1) % 8, 97);
    }

    /// A per-segment override is checked too — the engine validates every segment,
    /// not just the job-wide default.
    #[test]
    fn a_single_bad_segment_is_enough() {
        let p = json!({
            "width": 1216, "height": 704, "segment_frames": 97,
            "segments": [{"prompt": "one", "frames": 97}, {"prompt": "two", "frames": 96}],
        });
        assert_eq!(bad_frame_grid(&p), Some(96));
    }

    /// An omitted count falls back to the chain's own default, which is legal —
    /// this must not refuse a payload that simply did not say.
    #[test]
    fn an_unstated_frame_count_is_the_legal_default() {
        assert_eq!(bad_frame_grid(&json!({"width": 1216, "height": 704})), None);
    }

    /// 🔴 `JobFailed` covers the truncated-bank tail `run` already retries once by itself. Calling
    /// it transient would stack the two levels and spend six renders where one recovery was meant.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: Ltx2bError| verdict_of(&e);
        assert_eq!(
            t(Ltx2bError::Upstream(429, "at 64".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Ltx2bError::Upstream(503, "draining".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Ltx2bError::Upstream(500, "boom".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Ltx2bError::Upstream(400, "bad grid".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(Ltx2bError::Upstream(404, "no bank".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(Ltx2bError::Upstream(409, "busy".into())),
            Failure::Permanent
        );
        assert_eq!(t(Ltx2bError::NotConfigured), Failure::Permanent);
        assert_eq!(t(Ltx2bError::Cancelled("j".into())), Failure::Permanent);
        assert_eq!(
            t(Ltx2bError::JobFailed(
                "j".into(),
                "failed to load latent bank".into()
            )),
            Failure::Permanent
        );
        assert_eq!(t(Ltx2bError::MissingField("media")), Failure::Permanent);
        assert_eq!(
            t(Ltx2bError::BadMedia("j".into(), "short".into())),
            Failure::Permanent
        );
    }

    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = Ltx2bError::Upstream(429, "ltx-2b fleet is at 64".into());
        assert_eq!(
            verdict::recall(&verdict::note(e.to_string(), verdict_of(&e))),
            Failure::Transient
        );
        let e = Ltx2bError::Upstream(404, "ltx-2b fleet is at 64".into());
        assert_eq!(
            verdict::recall(&verdict::note(e.to_string(), verdict_of(&e))),
            Failure::Permanent
        );
    }
}
