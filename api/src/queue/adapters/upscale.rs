//! Upscale — the segmented, resumable adapter (shape B).
//!
//! One enqueued upscale is a SEQUENCE of engine jobs. A window past comfy-api's published host-RAM
//! guard is split by [`crate::upscale::plan_segments`], and koblem drives the segments itself —
//! one `POST /upscale/v1/generate` each — so every segment can be fetched, joined and reported as
//! it lands, and so a restart resumes rather than re-rendering finished GPU work.
//!
//! # The payload this adapter is enqueued with
//!
//! ```json
//! { "request": <crate::upscale::UpscaleRequest>,
//!   "plan":    <crate::upscale::SegmentPlan>,
//!   "source_label": "upload" }
//! ```
//!
//! 🔴 `request.source_path` is the CAPTURED source, not a staged one. An enqueued job can sit for
//! eight hours, so the browser that POSTed the multipart is long gone by dispatch — the route
//! captures the upload at enqueue and this adapter stages it at dispatch (see [`StagedSource`]).
//!
//! 🔴 `plan` is computed at ENQUEUE and carried, because [`JobRunner::units`] and
//! [`JobRunner::shape_key`] are synchronous and the split needs an ffprobe plus the engine's
//! `/capabilities`. It is also the thing the accept body shows the user (segments, and their
//! seams), so re-deriving a different one at dispatch would silently contradict what they agreed
//! to. The segment index is a pure function of it, which is what makes a resume safe.
//!
//! # Where the GPU is released
//!
//! After the LAST segment is fetched and before the join. The join is an ffmpeg concat over
//! gigabytes and takes minutes on a long clip; holding the comfy-api lane through it would idle
//! every other engine on that box. That is what `collecting` is for.
//!
//! # Where the output goes
//!
//! The gallery, via [`persist_generation_files`] — hard-linked and hashed by streaming, never read
//! into RAM. 🔴 This is the fix for the old policy hole: the retired per-engine store expired the
//! deliverable on a 7-day TTL because `persist_generation` would have needed the whole 4K clip in
//! memory. A generated asset lives until the user deletes it, and the gallery is where that is
//! true — `jobs::gpu_retention` sweeps this adapter's working directory and never its output.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{Job, JobKind, JobProgress, Preempt};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::verdict;
use crate::AppState;
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::tier2_helpers::{
    OutputFile, generation_file_url, link_or_copy, persist_generation_files,
};
use crate::upscale::{
    self as engine, Error as UpscaleError, MAX_SEGMENTS, MIN_SEGMENT_SECONDS, SegmentPlan,
    UpscaleCompletion, UpscaleRequest,
};

/// The gallery bucket. Its own tag, not a shared "video" one: the gallery/history query is by
/// service, so two engines in one bucket cannot be told apart — the same rule
/// `routes::ltx_video` states where it files under `service_tag()`.
const SERVICE_TAG: &str = "upscale";
const ENDPOINT_TAG: &str = "upscale-generate";

const JOINED_STEM: &str = "upscaled";

#[derive(Debug, Clone, Deserialize)]
struct UpscalePayload {
    request: UpscaleRequest,
    plan: SegmentPlan,
    #[serde(default)]
    source_label: Option<String>,
}

fn parse_payload(payload: &Value) -> Result<UpscalePayload, String> {
    serde_json::from_value::<UpscalePayload>(payload.clone())
        .map_err(|e| format!("this is not an upscale payload: {e}"))
}

// ── shape ────────────────────────────────────────────────────────────────────

/// Megapixels of the DELIVERED frame, bucketed. The engine's own cost model is
/// `SECONDS_PER_MPX_FRAME`, so megapixels is the axis; the segment count is `units_total` and the
/// frames per segment ride on it, which is why neither is in the key.
///
/// Buckets are coarse on purpose — 1080p (2.07), a 2× LTX shot (2560x1408 = 3.60) and 4K (8.29)
/// each land in their own, and nothing finer would ever accumulate samples.
fn mpx_bucket(width: u32, height: u32) -> &'static str {
    let mpx = f64::from(width) * f64::from(height) / 1_000_000.0;
    match mpx {
        m if m <= 1.5 => "1mpx",
        m if m <= 3.0 => "2mpx",
        m if m <= 6.0 => "4mpx",
        m if m <= 12.0 => "8mpx",
        _ => "huge",
    }
}

fn shape_of(plan: &SegmentPlan) -> String {
    format!(
        "upscale/{}",
        mpx_bucket(plan.output_width, plan.output_height)
    )
}

/// The window of segment `index`, as an offset into the source.
///
/// 🔴 The segments must tile the plan's window exactly once with the LAST one short. Getting this
/// wrong duplicates or drops footage and the join hides it.
fn segment_window(
    req: &UpscaleRequest,
    plan: &SegmentPlan,
    index: u32,
) -> (Option<f64>, Option<f64>) {
    let Some(each) = plan.segment_seconds else {
        return (req.start_time, req.duration);
    };
    let start = req.start_time.unwrap_or(0.0) + f64::from(index) * each;
    let remaining = plan.window_seconds - f64::from(index) * each;
    (Some(start), Some(each.min(remaining).max(0.0)))
}

/// The split's own arithmetic, re-checked at enqueue.
///
/// The route computes the plan with `plan_segments`, which enforces all of this; this is the guard
/// for a hand-built enqueue (the CLI, an external caller) carrying a plan nothing validated. 🔴 The
/// queue stretches the feedback loop from minutes to hours, and a job that cannot possibly succeed
/// must never enter it and wait until 03:00 to say so.
fn validate_payload(p: &UpscalePayload) -> Result<(), String> {
    if p.request.source_path.as_os_str().is_empty() {
        return Err("the payload names no source clip".to_string());
    }
    if p.plan.output_width == 0 || p.plan.output_height == 0 {
        return Err("the plan resolves to an empty frame".to_string());
    }
    if p.plan.window_seconds <= 0.0 {
        return Err("the plan covers an empty window".to_string());
    }
    if p.plan.segments == 0 || p.plan.segments > MAX_SEGMENTS {
        return Err(format!(
            "a {}-segment upscale is outside the engine's cap of {MAX_SEGMENTS}. Each segment is a \
             separate prompt on a single-slot queue shared with every other engine on that box — \
             upscale it in pieces.",
            p.plan.segments
        ));
    }
    if let Some(each) = p.plan.segment_seconds
        && each < MIN_SEGMENT_SECONDS
    {
        return Err(format!(
            "{each:.2} s segments are below the {MIN_SEGMENT_SECONDS:.1} s floor, where the \
             per-prompt overheads dominate and the seams multiply for nothing. Lower the scale."
        ));
    }
    Ok(())
}

fn seg_name(index: u32) -> String {
    format!("seg{index:04}.{}", engine::OUTPUT_FORMAT)
}

/// A failure message that says WHICH segment, and names the shared mount when the cause is staging.
///
/// A bare engine error on segment 5 of 6 is indistinguishable from one on segment 1, and the
/// difference decides whether re-running is cheap. A staging failure has one overwhelmingly likely
/// cause and the symptom ("works on a small file, dies on a big one") reads as a size bug until the
/// message says so.
///
/// 🔴 Also the one place a typed engine error becomes a queue message, so it is where the retry
/// verdict is taken. The three callers (submit, poll, fetch) all propagate this string unwrapped
/// out of `run`.
fn describe(e: &UpscaleError, index: u32, total: u32) -> String {
    let where_ = if total > 1 {
        format!("segment {} of {total}: ", index + 1)
    } else {
        String::new()
    };
    let msg = match e {
        UpscaleError::Staging(msg) => format!(
            "{where_}{msg}. 🔴 This is almost always the shared staging mount: koblem and comfy-api \
             must both see UPSCALE_STAGING_HOST at the SAME absolute path, or the source cannot be \
             handed over by path and a large file cannot be handed over at all."
        ),
        other => format!("{where_}{other}"),
    };
    verdict::note(msg, verdict_of(e))
}

/// A retry is unusually cheap here: the delivered segments on disk are the resume state, so a
/// second attempt re-renders only the segment that died, not the whole clip.
fn verdict_of(e: &UpscaleError) -> Failure {
    match e {
        UpscaleError::Request(e) => verdict::transport(e),
        UpscaleError::Upstream(status, _) => verdict::upstream(*status),
        // The shared mount is a deployment fact, not a wire hiccup — it is exactly the same on the
        // next attempt. `JobFailed` is the plugin's own sentence about the render and is not read.
        UpscaleError::Staging(_)
        | UpscaleError::NotConfigured
        | UpscaleError::JobFailed(..)
        | UpscaleError::Cancelled(_)
        | UpscaleError::MissingField(_) => Failure::Permanent,
    }
}

// ── the staged source ────────────────────────────────────────────────────────

/// The source, linked into the shared `UPSCALE_STAGING_HOST` mount for the life of ONE run.
///
/// 🔴 STAGED AT DISPATCH, UNLINKED ON EVERY EXIT. Staging at enqueue would park five whole videos
/// in a directory both containers mount for as long as five queued upscales take to reach the head
/// of the queue — overnight, routinely. Unlinking lives in `Drop` rather than on each return path
/// because there are eight of them (submit, poll, fetch, cancel, preempt, join, persist, panic) and
/// a leak here is a gigabyte.
struct StagedSource {
    path: PathBuf,
}

impl StagedSource {
    async fn stage(dir: &Path, source: &Path, job_id: &str) -> Result<Self, String> {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            return Err(format!(
                "the upscale staging directory {} is not writable ({e}) — this is the mount both \
                 koblem and comfy-api must see at the SAME absolute path, and without it a source \
                 clip cannot be handed over at all",
                dir.display()
            ));
        }
        if !tokio::fs::try_exists(source).await.unwrap_or(false) {
            return Err(format!(
                "the captured source {} is gone, so this upscale cannot be dispatched. It is \
                 captured when the job is enqueued and must outlive the queue wait.",
                source.display()
            ));
        }
        // A job id off the queue, filtered anyway: a hand-crafted one must not name a path outside
        // the staging directory.
        let stem: String = job_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        let stem = if stem.is_empty() {
            "job"
        } else {
            stem.as_str()
        };
        let suffix = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4")
            .to_ascii_lowercase();
        let path = dir.join(format!("q{stem}.{suffix}"));
        link_or_copy(source, &path).await.map_err(|e| {
            format!(
                "could not stage {} into {} ({e}) — this is the shared mount comfy-api reads the \
                 source by path from",
                source.display(),
                dir.display()
            )
        })?;
        Ok(StagedSource { path })
    }
}

impl Drop for StagedSource {
    fn drop(&mut self) {
        // Synchronous, because `Drop` cannot await and this must also run while unwinding. One
        // unlink is a single syscall.
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %self.path.display(), error = %e,
                  "upscale: could not unlink the staged source; it is a whole video in a shared \
                   directory nothing else cleans");
        }
    }
}

// ── the adapter ──────────────────────────────────────────────────────────────

pub struct UpscaleAdapter {
    state: AppState,
}

/// How one segment's poll ended. Cancellation is not an error: the engine confirmed it.
enum Segment {
    Delivered(Box<UpscaleCompletion>),
    Cancelled,
}

impl UpscaleAdapter {
    pub fn new(state: AppState) -> Self {
        UpscaleAdapter { state }
    }

    /// Segments, the joined output, and nothing that outlives them.
    fn work_dir(&self, job_id: &str) -> PathBuf {
        self.state.queue_files.work_dir(job_id)
    }

    /// How far the sequence actually got, read off koblem's own disk.
    ///
    /// 🔴 The FILES are the authority, not the checkpoint: a segment is renamed into place only
    /// once its fetch completed, so a present file is a whole one, and a crash between the fetch
    /// and the checkpoint write must not re-render finished GPU work. Contiguous from zero because
    /// a gap would make every later index's window wrong.
    async fn segments_on_disk(dir: &Path, total: u32) -> u32 {
        let mut n = 0;
        while n < total
            && tokio::fs::try_exists(dir.join(seg_name(n)))
                .await
                .unwrap_or(false)
        {
            n += 1;
        }
        n
    }

    /// Poll one segment to a terminal state, mirroring the engine's phase onto the row.
    ///
    /// Only `Interrupt::Cancel` is honoured here. A preempt's checkpoint is BETWEEN segments —
    /// that is what `AtCheckpoint` costs, at most one segment — and stopping mid-render would
    /// throw away the GPU already spent on it.
    async fn poll(
        &self,
        ctx: &JobCtx,
        engine_id: &str,
        index: u32,
        total: u32,
    ) -> Result<Segment, String> {
        let client = &self.state.upscale;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let waiter = client.wait(engine_id);
        tokio::pin!(waiter);
        let mut cancelling = false;
        loop {
            tokio::select! {
                done = &mut waiter => {
                    // 🔴 Once a cancel has been asked for, any terminal answer means cancelled —
                    // including a segment that finished anyway. The lane is released on the
                    // strength of this returning, so it returns only when the engine has stopped
                    // answering "generating".
                    if cancelling {
                        return Ok(Segment::Cancelled);
                    }
                    return match done {
                        Ok(c) => Ok(Segment::Delivered(Box::new(c))),
                        Err(UpscaleError::Cancelled(_)) => Ok(Segment::Cancelled),
                        Err(e) => Err(describe(&e, index, total)),
                    };
                }
                _ = ticker.tick() => {
                    if !cancelling && ctx.cancel.get() == Interrupt::Cancel {
                        cancelling = true;
                        if let Err(e) = client.cancel(engine_id).await {
                            warn!(job = %ctx.job.id, engine = %engine_id, error = %e,
                                  "upscale: the engine refused the cancel; polling until it stops");
                        }
                    }
                    let p = client.progress();
                    ctx.progress(
                        JobProgress {
                            phase: p.phase.clone(),
                            progress: p.progress,
                            current_step: p.current_step,
                            total_steps: p.total_steps,
                            queue_position: p.queue_position,
                            preview_seq: None,
                        },
                        index as i32,
                    )
                    .await;
                }
            }
        }
    }

    /// Render one segment and leave its file in `dir`. Returns the warnings the engine raised.
    async fn render_segment(
        &self,
        ctx: &JobCtx,
        p: &UpscalePayload,
        staged: &Path,
        dir: &Path,
        index: u32,
    ) -> Result<Option<Vec<String>>, String> {
        let total = p.plan.segments as u32;
        let client = &self.state.upscale;

        let mut seg_req = p.request.clone();
        seg_req.source_path = staged.to_path_buf();
        let (start, duration) = segment_window(&p.request, &p.plan, index);
        seg_req.start_time = start;
        seg_req.duration = duration;

        // `None`: koblem already sized this window, so handing the plugin a `segment_seconds` would
        // make it fan out internally and land every piece at once at the end.
        //
        // 🔴 No `gpu` field, deliberately: comfy-api does not take one. It is one process in front
        // of one ComfyUI on whatever card its container was given, which is also why
        // `gpu_engine_config` gives `minimax_h3` a single-card candidate list and why the queue
        // refuses a pin on this lane.
        let (engine_id, submit_plan) = client
            .submit(&seg_req, None)
            .await
            .map_err(|e| describe(&e, index, total))?;

        // Published the moment the engine accepts. This is the field that makes reattach possible,
        // so it is written before anything else can go wrong.
        if let Err(e) = store::set_server_job_id(&self.state.db, &ctx.job.id, &engine_id).await {
            warn!(job = %ctx.job.id, error = %e, "upscale: could not record the engine job id");
        }

        let mut warnings = submit_plan.warnings.clone();
        let completion = match self.poll(ctx, &engine_id, index, total).await? {
            Segment::Delivered(c) => c,
            Segment::Cancelled => return Ok(None),
        };
        warnings.extend(completion.warnings.iter().cloned());

        // koblem asked for exactly one window; anything else means the two sides disagree about
        // the geometry and the join would be silently short.
        let Some(delivered) = completion.segments.first() else {
            return Err(format!(
                "segment {} of {total} completed with no file",
                index + 1
            ));
        };
        if completion.segments.len() > 1 {
            warnings.push(format!(
                "segment {} came back as {} engine segments — koblem's split and the engine's RAM \
                 guard disagree. Only the first was kept, so this output is SHORT. Re-run with a \
                 smaller scale.",
                index + 1,
                completion.segments.len()
            ));
        }

        // 🔴 Fetched to a `.part` and renamed, so a present `segNNNN.mp4` is always a WHOLE one —
        // that is what lets the resume trust the disk over the checkpoint.
        let final_path = dir.join(seg_name(index));
        let part = dir.join(format!("{}.part", seg_name(index)));
        client
            .fetch_segment_to(&delivered.url, &part)
            .await
            .map_err(|e| describe(&e, index, total))?;
        tokio::fs::rename(&part, &final_path)
            .await
            .map_err(|e| format!("segment {} of {total}: {e}", index + 1))?;

        // Best-effort: comfy-api sweeps on its own TTL, so a failure here costs disk over there for
        // a while and nothing else.
        if let Err(e) = client.delete_job(&engine_id).await {
            warn!(job = %ctx.job.id, engine = %engine_id, error = %e,
                  "upscale: could not release the engine job");
        }
        Ok(Some(warnings))
    }

    /// Concatenate the delivered segments into one deliverable.
    ///
    /// 🔴 A STREAM COPY (`-c copy`). Every segment came out of the same graph with the same encoder
    /// settings, so the concat demuxer splices them without touching a pixel — instant and
    /// lossless. Re-encoding here would throw away exactly the detail the upscale was run to make.
    ///
    /// ⚠️ The CUT is still real: each segment is its own refine, which is what `plan.seams` says.
    async fn join(dir: &Path, count: u32) -> Result<PathBuf, String> {
        let output = dir.join(format!("{JOINED_STEM}.{}", engine::OUTPUT_FORMAT));
        if count == 0 {
            return Err("no segments were delivered".to_string());
        }
        if count == 1 {
            // Linked rather than renamed: `segments_on_disk` stays true afterwards, so a retry
            // that reaches here again does not re-render the only segment.
            link_or_copy(&dir.join(seg_name(0)), &output)
                .await
                .map_err(|e| format!("placing the single segment: {e}"))?;
            return Ok(output);
        }

        // The concat demuxer's list file. Paths are relative and the list lives beside them, so
        // nothing here has to think about quoting an absolute path with spaces in it.
        let mut list = String::new();
        for index in 0..count {
            list.push_str(&format!("file '{}'\n", seg_name(index)));
        }
        let list_path = dir.join("segments.txt");
        tokio::fs::write(&list_path, list)
            .await
            .map_err(|e| format!("writing the concat list: {e}"))?;
        let out = tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
            .arg(&list_path)
            .args(["-c", "copy"])
            .arg(&output)
            .output()
            .await
            .map_err(|e| format!("ffmpeg: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "ffmpeg concat failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(output)
    }

    /// File the deliverable in the gallery and drop the intermediates.
    ///
    /// 🔴 `persist_generation_files`, never `persist_generation`: it hard-links (or copies) and
    /// hashes by STREAMING, so a 4K clip is never held in RAM. This is what puts an upscale under
    /// the same retention rule as every other generated asset — it lives until the user deletes it.
    async fn collect(
        &self,
        ctx: &JobCtx,
        p: &UpscalePayload,
        dir: &Path,
        warnings: &mut Vec<String>,
    ) -> Result<(), String> {
        let count = p.plan.segments as u32;
        let output = Self::join(dir, count).await.map_err(|e| {
            format!(
                "the segments rendered but could not be joined: {e}. They are still on disk under \
                 {}.",
                dir.display()
            )
        })?;

        let meta = json!({
            "engine": "upscale",
            "job_id": ctx.job.id,
            "source": p.source_label,
            "segments": count,
            "seams": p.plan.seams,
            "plan": p.plan,
            "request": {
                "scale_mode": p.request.scale_mode,
                "multiplier": p.request.multiplier,
                "noise": p.request.noise,
                "detailer": p.request.detailer,
                "scale_method": p.request.scale_method,
                "start_time": p.request.start_time,
                "duration": p.request.duration,
            },
        });
        let persisted = persist_generation_files(
            &self.state,
            SERVICE_TAG,
            ENDPOINT_TAG,
            meta,
            &[OutputFile {
                ext: engine::OUTPUT_FORMAT.to_string(),
                mime: engine::OUTPUT_MIME.to_string(),
                kind: "video".to_string(),
                path: output.clone(),
            }],
        )
        .await;

        match persisted {
            Some(id) => {
                info!(job = %ctx.job.id, generation = %id, segments = count, "upscale complete");
                let row = json!({ "generation_id": id, "url": generation_file_url(&id, 0) });
                if let Err(e) = store::set_result(&self.state.db, &ctx.job.id, &row).await {
                    warn!(job = %ctx.job.id, error = %e, "upscale: could not record the result");
                }
                // Only now: the gallery holds the deliverable on its own link, so the segments and
                // the working copy are duplicates of bytes the user already owns.
                if let Err(e) = tokio::fs::remove_dir_all(dir).await
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(job = %ctx.job.id, path = %dir.display(), error = %e,
                          "upscale: could not clear the working directory");
                }
            }
            None => warnings.push(format!(
                "the upscale finished but could not be filed in the gallery — `generations_dir` is \
                 unset, so nothing is persisted for this service. The output is on disk at {}.",
                output.display()
            )),
        }
        Ok(())
    }

    /// Submit → poll → fetch, segment by segment, then release and collect.
    async fn drive(&self, ctx: &JobCtx, p: &UpscalePayload, staged: &Path) -> RunResult {
        let total = p.plan.segments as u32;
        let dir = self.work_dir(&ctx.job.id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("cannot create the upscale working directory: {e}"))?;

        let done = Self::segments_on_disk(&dir, total).await;
        let banked = ctx
            .resume
            .as_ref()
            .and_then(|r| r.get("segments_done"))
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        if let Some(banked) = banked
            && banked != done
        {
            warn!(job = %ctx.job.id, banked, on_disk = done,
                  "upscale: the checkpoint and the delivered segments disagree; the files win");
        }
        if done > 0 {
            info!(job = %ctx.job.id, segments_done = done, segments = total,
                  "upscale: resuming rather than re-rendering finished GPU work");
        }

        let mut warnings: Vec<String> = Vec::new();
        for index in done..total {
            match ctx.cancel.get() {
                Interrupt::Cancel => return Ok(JobOutcome::Cancelled),
                // The checkpoint is BETWEEN segments: the delivered files ARE the resume state, so
                // the cost of a preempt is at most the segment about to start.
                Interrupt::Preempt => {
                    return Ok(JobOutcome::Preempted {
                        resume: json!({ "segments_done": index }),
                    });
                }
                Interrupt::None => {}
            }
            match self.render_segment(ctx, p, staged, &dir, index).await? {
                Some(w) => {
                    for warning in w {
                        if !warnings.contains(&warning) {
                            warnings.push(warning);
                        }
                    }
                }
                None => return Ok(JobOutcome::Cancelled),
            }
            ctx.progress(
                JobProgress {
                    phase: Some("delivered".to_string()),
                    ..Default::default()
                },
                (index + 1) as i32,
            )
            .await;
        }

        // 🔴 THE ENGINE/DISK BOUNDARY. Every segment is fetched; what is left is an ffmpeg concat
        // over gigabytes, which is minutes on a long clip and no GPU at all. Past here the job is
        // not cancellable — the artifact is landing and the GPU cost is paid — so the cancel token
        // is not polled again.
        ctx.release_gpu();

        self.collect(ctx, p, &dir, &mut warnings).await?;
        Ok(JobOutcome::Done { warnings })
    }

    /// Stage, drive, unstage. The one place the staged source's lifetime is bounded.
    async fn execute(&self, ctx: JobCtx) -> RunResult {
        if !self.state.upscale.is_configured() {
            return Err(
                "the upscaler is not configured — set `upscale_server_url` in Settings".to_string(),
            );
        }
        let p = parse_payload(&ctx.payload)?;
        let staged = StagedSource::stage(
            &self.state.upscale_staging_dir,
            &p.request.source_path,
            &ctx.job.id,
        )
        .await?;
        let path = staged.path.clone();
        let outcome = self.drive(&ctx, &p, &path).await;
        drop(staged);
        outcome
    }
}

#[async_trait]
impl JobRunner for UpscaleAdapter {
    fn kind(&self) -> JobKind {
        JobKind::Upscale
    }

    /// 🔴 The comfy-api budget line, shared with h3, ltx25 and music3 because they are four plugins
    /// on ONE process. Upscale has no line of its own and must not invent one — that would price
    /// the same card twice. The mutex between them is the LANE, which `Lane::of` already gives.
    fn service(&self) -> ServiceKind {
        ServiceKind::MinimaxH3
    }

    /// At most one segment, because the delivered segments are the checkpoint.
    fn preempt(&self) -> Preempt {
        Preempt::AtCheckpoint
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        validate_payload(&parse_payload(payload)?)
    }

    fn shape_key(&self, payload: &Value) -> String {
        match parse_payload(payload) {
            Ok(p) => shape_of(&p.plan),
            Err(_) => "upscale/unknown".to_string(),
        }
    }

    fn units(&self, payload: &Value) -> Option<i32> {
        parse_payload(payload).ok().map(|p| p.plan.segments as i32)
    }

    fn label(&self, payload: &Value) -> String {
        let Ok(p) = parse_payload(payload) else {
            return "upscale".to_string();
        };
        let source = p.source_label.unwrap_or_else(|| "source".to_string());
        let size = format!("{}x{}", p.plan.output_width, p.plan.output_height);
        if p.plan.seams > 0 {
            format!(
                "upscale · {source} · {size} · {} segments, {} seams",
                p.plan.segments, p.plan.seams
            )
        } else {
            format!("upscale · {source} · {size}")
        }
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        self.execute(ctx).await
    }

    /// ⚠️ THE INTERRUPTED SEGMENT IS CANCELLED AND RE-RUN, NOT ADOPTED.
    ///
    /// Adopting it would mean re-entering a poll on an engine job whose output this process may
    /// already have fetched, with no way to know how far that fetch got — and comfy-api's records
    /// expire in TEN MINUTES, so a slow reattach finds nothing anyway. Cancelling costs at most one
    /// segment and cannot deliver a half-written file; everything already on disk is kept.
    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        if let Some(engine_id) = ctx.job.server_job_id.clone() {
            info!(job = %ctx.job.id, engine = %engine_id,
                  "upscale: cancelling the segment that was in flight when koblem restarted");
            if let Err(e) = self.state.upscale.cancel(&engine_id).await {
                warn!(job = %ctx.job.id, engine = %engine_id, error = %e,
                      "upscale: could not cancel the interrupted segment; it ages out of comfy's TTL");
            }
        }
        self.execute(ctx).await
    }

    /// 🔴 Does not return until the engine has confirmed. The caller releases the lane on the
    /// strength of it, and a lane released early means the next job submits into a busy ComfyUI.
    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(engine_id) = job.server_job_id.as_deref() else {
            // Between segments there is nothing in flight; the run loop's own checkpoint stops the
            // sequence.
            return Ok(());
        };
        match self.state.upscale.cancel(engine_id).await {
            Ok(()) => Ok(()),
            // Gone is stopped. 404/410 mean the record expired or was already released, which is
            // the confirmation this is waiting for.
            Err(UpscaleError::Upstream(404 | 410, _)) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upscale::ScaleMode;

    fn plan(w: u32, h: u32, segments: usize, each: Option<f64>, window: f64) -> SegmentPlan {
        SegmentPlan {
            segment_seconds: each,
            segments,
            output_width: w,
            output_height: h,
            window_seconds: window,
            frames_per_prompt: 148,
            seams: segments.saturating_sub(1),
        }
    }

    fn request() -> UpscaleRequest {
        UpscaleRequest {
            source_path: PathBuf::from("/var/lib/koblem/upscale-capture/src.mp4"),
            scale_mode: ScaleMode::Multiplier,
            multiplier: 2.0,
            shorter_size: None,
            longer_size: None,
            width: None,
            height: None,
            noise: None,
            detailer: false,
            scale_method: None,
            start_time: None,
            duration: None,
            segment_seconds: None,
        }
    }

    fn payload(plan: SegmentPlan) -> Value {
        json!({
            "request": request(),
            "plan": plan,
            "source_label": "upload",
        })
    }

    /// 🔴 Coarse buckets on the DELIVERED megapixels — the axis the engine's own
    /// `SECONDS_PER_MPX_FRAME` model uses. A per-resolution key would give every render its own
    /// sample of one and the estimator would never learn anything.
    #[test]
    fn nearby_output_sizes_share_a_bucket() {
        // 1080p and 1920x1088 (the DIM_MULTIPLE snap of it) are the same shape.
        assert_eq!(mpx_bucket(1920, 1080), mpx_bucket(1920, 1088));
        // A 2x LTX shot, 1080p and 4K are not.
        assert_ne!(mpx_bucket(2560, 1408), mpx_bucket(1920, 1080));
        assert_ne!(mpx_bucket(2560, 1408), mpx_bucket(3840, 2160));
        assert_eq!(
            shape_of(&plan(2560, 1408, 3, Some(6.0), 15.0)),
            "upscale/4mpx"
        );
    }

    /// 🔴 The segment count is NOT in the key: `units_total` × `unit_p50` is what lets a 6-segment
    /// upscale be estimated from 2-segment history.
    #[test]
    fn the_segment_count_stays_out_of_the_shape() {
        assert_eq!(
            shape_of(&plan(2560, 1408, 2, Some(6.0), 12.0)),
            shape_of(&plan(2560, 1408, 6, Some(6.0), 36.0)),
        );
    }

    /// The per-segment windows must tile the plan's window exactly once, with the LAST one short.
    /// Getting this wrong duplicates or drops footage and the join hides it.
    #[test]
    fn segment_windows_tile_the_plan_exactly_once() {
        let p = plan(2560, 1408, 3, Some(6.0), 15.0);
        let mut req = request();
        req.start_time = Some(2.0);
        let mut covered = 0.0;
        for index in 0..p.segments as u32 {
            let (start, duration) = segment_window(&req, &p, index);
            let (start, duration) = (
                start.expect("a split names its start"),
                duration.expect("and its length"),
            );
            assert!(duration > 0.0, "segment {index} is empty");
            assert!(
                (start - (2.0 + covered)).abs() < 1e-9,
                "segment {index} does not abut"
            );
            covered += duration;
        }
        assert!(
            (covered - p.window_seconds).abs() < 1e-9,
            "covered {covered}"
        );
    }

    /// A one-prompt upscale keeps the caller's own window rather than inventing one — the case
    /// with no seams at all is the case to aim for.
    #[test]
    fn a_single_segment_keeps_the_requested_window() {
        let mut req = request();
        req.start_time = Some(600.0);
        req.duration = Some(4.0);
        let p = plan(2560, 1408, 1, None, 4.0);
        assert_eq!(segment_window(&req, &p, 0), (Some(600.0), Some(4.0)));
    }

    /// The queue stretches the feedback loop from minutes to hours, so a job that cannot succeed
    /// must be refused at enqueue rather than at 03:00.
    #[test]
    fn an_unworkable_plan_is_refused_at_enqueue() {
        let check = |v: Value| -> Result<(), String> { validate_payload(&parse_payload(&v)?) };
        assert!(check(payload(plan(2560, 1408, 3, Some(6.0), 15.0))).is_ok());
        let too_many = check(payload(plan(
            2560,
            1408,
            MAX_SEGMENTS + 1,
            Some(6.0),
            400.0,
        )))
        .expect_err("past the engine's own cap");
        assert!(too_many.contains("upscale it in pieces"), "{too_many}");
        let too_short =
            check(payload(plan(2560, 1408, 40, Some(0.5), 20.0))).expect_err("under the floor");
        assert!(too_short.contains("Lower the scale"), "{too_short}");
        assert!(
            check(payload(plan(2560, 1408, 1, None, 0.0))).is_err(),
            "an empty window"
        );
        // A payload with no source, or no plan at all, never becomes a job.
        assert!(parse_payload(&json!({ "plan": plan(2560, 1408, 1, None, 4.0) })).is_err());
    }

    /// 🔴 The resume reads the DELIVERED FILES, not a counter, and stops at the first gap: a
    /// missing segment would make every later index's window wrong.
    #[tokio::test]
    async fn the_resume_counts_contiguous_delivered_segments() {
        let dir =
            std::env::temp_dir().join(format!("koblem-upscale-adapter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(UpscaleAdapter::segments_on_disk(&dir, 4).await, 0);
        for index in [0_u32, 1, 3] {
            std::fs::write(dir.join(seg_name(index)), b"x").expect("write");
        }
        assert_eq!(
            UpscaleAdapter::segments_on_disk(&dir, 4).await,
            2,
            "segment 3 is past a gap and cannot be trusted as progress"
        );
        // A half-fetched segment is a `.part` and is never counted.
        std::fs::write(dir.join(format!("{}.part", seg_name(2))), b"x").expect("write");
        assert_eq!(UpscaleAdapter::segments_on_disk(&dir, 4).await, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 The staged source is unlinked when the run's scope ends — including the panic and the
    /// preempt paths, which is why it is a `Drop` rather than a call on each return.
    #[test]
    fn the_staged_source_is_unlinked_when_the_run_ends() {
        let dir =
            std::env::temp_dir().join(format!("koblem-upscale-staged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("qjob1.mp4");
        std::fs::write(&path, b"pretend-video").expect("write");
        {
            let _staged = StagedSource { path: path.clone() };
            assert!(path.exists(), "still needed while the run holds it");
        }
        assert!(
            !path.exists(),
            "a queued upscale must not park a video in the shared mount"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A staging failure has one overwhelmingly likely cause and the message has to name it.
    #[test]
    fn a_staging_failure_names_the_shared_mount() {
        let msg = describe(&UpscaleError::Staging("503 from stage_path".into()), 0, 1);
        assert!(msg.contains("UPSCALE_STAGING_HOST"), "{msg}");
        assert!(msg.to_lowercase().contains("same absolute path"), "{msg}");
        // …and a multi-segment failure says where it got to: re-running from scratch and resuming
        // are very different costs.
        let deep = describe(&UpscaleError::NotConfigured, 4, 6);
        assert!(deep.starts_with("segment 5 of 6: "), "{deep}");
    }

    /// 🔴 The staging mount is the one that reads like a transient and is not: it is a deployment
    /// fact and it is identical on the next attempt.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: UpscaleError| verdict_of(&e);
        assert_eq!(
            t(UpscaleError::Upstream(429, "queue full".into())),
            Failure::Transient
        );
        assert_eq!(
            t(UpscaleError::Upstream(503, "draining".into())),
            Failure::Transient
        );
        assert_eq!(
            t(UpscaleError::Upstream(502, "gateway".into())),
            Failure::Transient
        );
        assert_eq!(
            t(UpscaleError::Upstream(400, "bad window".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(UpscaleError::Upstream(410, "record expired".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(UpscaleError::Staging("no such mount".into())),
            Failure::Permanent
        );
        assert_eq!(t(UpscaleError::NotConfigured), Failure::Permanent);
        assert_eq!(t(UpscaleError::Cancelled("e".into())), Failure::Permanent);
        assert_eq!(
            t(UpscaleError::JobFailed("e".into(), "OOM in vae".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(UpscaleError::MissingField("segments")),
            Failure::Permanent
        );
    }

    /// `describe` is the only place a typed error becomes a queue message, so the verdict has to
    /// survive it — the queue never sees anything else.
    #[test]
    fn the_verdict_survives_describe() {
        let msg = describe(
            &UpscaleError::Upstream(429, "pending queue full".into()),
            0,
            3,
        );
        assert_eq!(verdict::recall(&msg), Failure::Transient);
        let msg = describe(
            &UpscaleError::Upstream(422, "pending queue full".into()),
            0,
            3,
        );
        assert_eq!(verdict::recall(&msg), Failure::Permanent);
    }
}
