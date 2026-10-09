//! Relip — lip-dub: a source clip plus a drive track, its mouths re-rendered in sync.
//!
//! One enqueued relip is a SEQUENCE of engine jobs. The relip pass doubles the DiT token count (the
//! source rides as a full-length frozen reference beside the generated frames) so a long source
//! OOMs single-shot; koblem windows it ([`RELIP_WIN`] / [`RELIP_OVERLAP`]) and drives the windows
//! itself — one `POST /ltx/v1/generate` each — then trim-and-concats the window clips.
//!
//! # The payload
//!
//! ```json
//! { "job_id":  "<LtxJobStore id>",
//!   "video":   "asset:<sha256>",
//!   "audio":   "asset:<sha256>",
//!   "request": <koblem_client::types::LtxRelipRequest>,
//!   "plan":    { "width": 1280, "height": 704, "fps": 24, "frames": 721, "windows": 13 } }
//! ```
//!
//! 🔴 `video` / `audio` are CONTENT-ADDRESSED REFS, never bytes. Relip's inputs arrive as multipart
//! parts and an enqueued job dispatches hours after that browser has gone, so both are captured
//! into the asset store at enqueue; a JSONB column never carries a video.
//!
//! 🔴 `plan` is computed at ENQUEUE, because [`JobRunner::units`] and [`JobRunner::shape_key`] are
//! synchronous and the windowing needs an ffprobe. Its `width`/`height`/`fps` ARE the render's —
//! resolved once so the queue row, the audio slicing and the frame extraction cannot disagree. Its
//! `frames`/`windows` are the ffprobe's estimate; the frames ffmpeg actually delivers at dispatch
//! are the authority, and a disagreement rides back as a warning rather than silently truncating.
//!
//! 🔴 `job_id` is the [`crate::ltx_jobs::LtxJobStore`] record, and the result is written under it:
//! the browser polls `/api/v1/ltx-video/jobs/{id}` and fetches `…/media` from the id the 202 handed
//! it, which is minted before the queue row exists. Same reason `ltx25` carries one.
//!
//! # Where the GPU is released
//!
//! After the LAST window's media reaches koblem's disk. 🔴 The fetch is deliberately UNDER the
//! guard: nothing stitched ever reaches the engine's volume — a window lives only in the CUDA
//! child's RAM — so whatever takes the card next can unload the only copy. The stitch (an ffmpeg
//! concat + x264 re-encode over the whole clip, minutes) and the persist run released.

use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{
    Job, JobKind, JobProgress, LTX_LORA_LIPDUB, LtxRelipRequest, Preempt, VideoJobState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::verdict;
use crate::AppState;
use crate::director_assets::{self, PartUploads};
use crate::ltx_video::{Error as LtxError, LtxRenderResult};
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as queue_rows;
use crate::routes::ltx_video::{extract_frames, frame_path, log_ltx, round_down_8k1, round32};
use crate::routes::tier2_helpers::{OutputFile, persist_generation_files};

/// Relip windowing constants. The compute buffer scales with total tokens, and the relip pass
/// doubles them, so a long source cannot be one DiT pass.
///
/// `WIN` = 65 frames is a valid 8k+1 latent-frame count and sits well inside the ltx25 plugin's own
/// 361-frame `control_frames` ceiling (one `LoadImage` + `ImageBatch` per frame, and 361f was
/// measured OOM at 1920×1088). `OVERLAP` = 8 frames = one causal-VAE latent step: consecutive
/// windows share their last/first 8 frames, and the duplicated leading 8 frames of every window
/// after the first are dropped when stitching, so every source frame is delivered exactly once.
///
/// ⚠️ The overlap does NOT hide the seam. On LTX-2.5 relip is not SDEdit: each window is sampled
/// fresh from noise with the source held frozen beside it, so two windows never reproduce the same
/// pixels for the frames they share.
const RELIP_WIN: u32 = 65;
const RELIP_OVERLAP: u32 = 8;
/// The window clips are stitched into the container the job store already named for an `ltx25` job
/// — see [`crate::ltx_jobs::LtxJob::clip_ext`], which is mp4 because ComfyUI's only audio-carrying
/// save node writes mp4. Writing the engine's mp4 bytes under a `.webm` name is what breaks the
/// first `-c copy` concat that rebuilds a cut.
const RELIP_CONTAINER: &str = "mp4";
const RELIP_MIME: &str = "video/mp4";

/// The aspect-preserving fit a source is rendered at when the caller names no size.
const RELIP_MAX_LONG: u32 = 1280;
const RELIP_MAX_SHORT: u32 = 704;

// ── the payload ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct RelipPayload {
    job_id: String,
    video: String,
    audio: String,
    #[serde(default)]
    request: LtxRelipRequest,
    plan: RelipPlan,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct RelipPlan {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u32,
    pub windows: u32,
}

fn parse_payload(payload: &Value) -> Result<RelipPayload, String> {
    serde_json::from_value::<RelipPayload>(payload.clone())
        .map_err(|e| format!("this is not a relip payload: {e}"))
}

/// A retry is cheap here: the delivered `win_NNN.mp4` files are the resume state, so a second
/// attempt re-renders only the window that died.
fn verdict_of(e: &LtxError) -> Failure {
    match e {
        LtxError::Request(e) => verdict::transport(e),
        // 🔴 Unlike `adapters::ltx25`, 503 is taken at face value here: a relip window never asks
        // the plugin to continue from a bank, so it cannot be the "that bank entry is gone" 503.
        LtxError::Upstream(status, _) => verdict::upstream(*status),
        LtxError::NotConfigured
        | LtxError::JobFailed(..)
        | LtxError::Cancelled(_)
        | LtxError::MissingField(_)
        | LtxError::BadMedia(..)
        | LtxError::Stream(_) => Failure::Permanent,
    }
}

/// One relip window: source frame range `[start, start+len)`.
struct RelipWindow {
    start: u32,
    len: u32,
}

/// Plan the windows over `total` source frames: consecutive ≤WIN windows stepping by
/// (WIN−OVERLAP), each length rounded down to a valid 8k+1 count. `total ≤ WIN` collapses to a
/// single window. Tail frames that can't fill a ≥9-frame window are dropped.
fn plan_relip_windows(total: u32) -> Vec<RelipWindow> {
    let step = RELIP_WIN - RELIP_OVERLAP;
    let mut windows = Vec::new();
    let mut start = 0u32;
    while start < total {
        let remaining = total - start;
        let len = round_down_8k1(remaining.min(RELIP_WIN));
        if len == 0 {
            break; // leftover tail too short to render on its own
        }
        windows.push(RelipWindow { start, len });
        if start + len >= total {
            break;
        }
        start += step;
    }
    windows
}

/// Pick a render resolution the LTX server accepts (each side a multiple of 32) that preserves
/// the source aspect and fits within `≤max_long × ≤max_short` on the long/short axis. Never
/// upscales.
fn target_dims(w: u32, h: u32, max_long: u32, max_short: u32) -> (u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    let landscape = w >= h;
    let (long, short) = if landscape { (w, h) } else { (h, w) };
    let scale = (max_long as f64 / long as f64)
        .min(max_short as f64 / short as f64)
        .min(1.0);
    let nl = round32((long as f64 * scale).round() as u32).min(round32(max_long));
    let ns = round32((short as f64 * scale).round() as u32).min(round32(max_short));
    if landscape { (nl, ns) } else { (ns, nl) }
}

/// Parse an ffmpeg rational like "30000/1001" or "24/1" → f64.
fn parse_rational(s: &str) -> Option<f64> {
    match s.split_once('/') {
        Some((n, d)) => {
            let n: f64 = n.trim().parse().ok()?;
            let d: f64 = d.trim().parse().ok()?;
            if d == 0.0 { None } else { Some(n / d) }
        }
        None => s.trim().parse().ok(),
    }
}

fn as_f64(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// ffprobe the first video stream → (width, height, fps, frames).
///
/// The frame count is `nb_frames` where the container states one and duration × fps otherwise —
/// counting for real means decoding the whole file, which is the work `extract_frames` does at
/// dispatch anyway. It is an ESTIMATE and only sizes the queue row; the delivered frames win.
async fn probe_source(path: &Path) -> std::io::Result<(u32, u32, f64, u32)> {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate,nb_frames,duration:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .await?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| std::io::Error::other(format!("ffprobe json: {e}")))?;
    let s = v
        .pointer("/streams/0")
        .ok_or_else(|| std::io::Error::other("no video stream"))?;
    let w = s.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
    let h = s.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
    if w == 0 || h == 0 {
        return Err(std::io::Error::other("source has no video dimensions"));
    }
    let fps = s
        .get("r_frame_rate")
        .and_then(Value::as_str)
        .and_then(parse_rational)
        .filter(|f| *f > 0.0)
        .unwrap_or(24.0);
    let frames = match as_f64(s.get("nb_frames")).filter(|n| *n >= 1.0) {
        Some(n) => n as u32,
        None => {
            let seconds = as_f64(s.get("duration"))
                .or_else(|| as_f64(v.pointer("/format/duration")))
                .unwrap_or(0.0);
            (seconds * fps).round().max(0.0) as u32
        }
    };
    Ok((w, h, fps, frames))
}

/// The plan a relip enqueue carries, from the captured source and the caller's overrides.
pub(crate) async fn plan_for(source: &Path, req: &LtxRelipRequest) -> Result<RelipPlan, String> {
    let (sw, sh, src_fps, frames) = probe_source(source)
        .await
        .map_err(|e| format!("ffprobe source: {e}"))?;
    // fps: caller override → source fps (rounded) → 24. Drives both the audio windowing and the
    // output timing.
    let fps = req
        .fps
        .unwrap_or_else(|| src_fps.round().max(1.0) as u32)
        .max(1);
    let (width, height) = match (req.width, req.height) {
        (Some(w), Some(h)) => (round32(w), round32(h)),
        _ => target_dims(sw, sh, RELIP_MAX_LONG, RELIP_MAX_SHORT),
    };
    Ok(RelipPlan {
        width,
        height,
        fps,
        frames,
        windows: plan_relip_windows(frames).len() as u32,
    })
}

pub(crate) fn payload_for(
    job_id: &str,
    video: &str,
    audio: &str,
    req: &LtxRelipRequest,
    plan: &RelipPlan,
) -> Value {
    json!({
        "job_id": job_id,
        "video": video,
        "audio": audio,
        "request": req,
        "plan": plan,
    })
}

// ── shape ────────────────────────────────────────────────────────────────────

/// Megapixels of the rendered frame, bucketed.
///
/// 🔴 The WINDOW COUNT is deliberately absent, and it is the one axis a reader expects to find
/// here: a relip's wall clock is (windows × per-window cost) and the window count is `units_total`,
/// so folding it in would give a 13-window relip its own sample of one instead of thirteen samples
/// of a window. Every window is the same [`RELIP_WIN`] frames by construction, so what is left that
/// moves the per-window cost is the canvas and the two-stage refine.
fn mpx_bucket(width: u32, height: u32) -> &'static str {
    let mpx = f64::from(width) * f64::from(height) / 1_000_000.0;
    match mpx {
        m if m <= 0.5 => "sub1mpx",
        m if m <= 1.5 => "1mpx",
        _ => "2mpx",
    }
}

fn shape_of(p: &RelipPayload) -> String {
    format!(
        "ltx_relip/{}/{}",
        mpx_bucket(p.plan.width, p.plan.height),
        if p.request.two_stage {
            "2stage"
        } else {
            "1stage"
        }
    )
}

fn validate_payload(p: &RelipPayload) -> Result<(), String> {
    for (what, reference) in [("video", &p.video), ("audio", &p.audio)] {
        if director_assets::ref_hash(reference).is_none() {
            return Err(format!(
                "the relip {what} must be an `asset:<hash>` ref — an enqueued render dispatches \
                 hours after the upload that carried it"
            ));
        }
    }
    let RelipPlan {
        width,
        height,
        fps,
        frames,
        windows,
    } = p.plan;
    // 🔴 Refused, never snapped. A canvas nobody asked for is a render nobody can use.
    if width == 0 || height == 0 || round32(width) != width || round32(height) != height {
        return Err(format!(
            "{width}x{height} is not a legal LTX canvas — both sides must be a multiple of 32 (the \
             spatial patch grid). Nearest legal: {}x{}",
            round32(width.max(1)),
            round32(height.max(1))
        ));
    }
    if fps == 0 {
        return Err(
            "a relip at 0 fps has no timeline to slice its drive audio against".to_string(),
        );
    }
    if windows == 0 {
        return Err(format!(
            "this source is too short to relip ({frames} frames) — the temporal VAE takes 8k+1 \
             only, so a window needs at least 9 frames"
        ));
    }
    Ok(())
}

// ── the scratch directory ────────────────────────────────────────────────────

/// Everything one relip needs on koblem's own disk: the extracted source frames, the 16 kHz drive
/// wav, and the delivered window clips.
///
/// 🔴 Keyed by the QUEUE JOB ID and NOT cleared between runs. That is what makes a preempt cost one
/// window: the delivered `win_NNN.mp4` files ARE the checkpoint, and re-extracting several hundred
/// full-resolution JPEGs is minutes of ffmpeg under the GPU guard.
///
/// 🔴 Keyed by queue job id under the configured queue-files root, NOT `temp_dir()`.
///
/// The pre-queue relip could use a disposable scratch because it ran to completion inside one task.
/// A preempted one parks several hundred full-resolution frames and its delivered windows here
/// until it resumes — on a tmpfs that is RAM, and across a container recreate it is a silent
/// re-render from window 0. The retention sweep keeps a work dir while its row is non-terminal, so
/// this is durable for exactly as long as the job is.
struct Scratch {
    work: PathBuf,
    frames: PathBuf,
    drive_wav: PathBuf,
    /// Resolved ONCE for the whole windowed render: the fold is paid per lora-set change, so a
    /// per-window multiplier would re-fold the 21.5 GB DiT at every window boundary.
    lipdub_strength: f64,
    prompt: String,
}

impl Scratch {
    async fn open(state: &AppState, job_id: &str, req: &LtxRelipRequest) -> Result<Self, String> {
        let work = state.queue_files.work_dir(job_id);
        let frames = work.join("frames");
        tokio::fs::create_dir_all(&frames)
            .await
            .map_err(|e| format!("mkdir workdir: {e}"))?;
        Ok(Scratch {
            drive_wav: work.join("drive16k.wav"),
            work,
            frames,
            // 1.0 is the scale the adapter was TRAINED at, which is not the same as a scale
            // measured on this checkpoint; clamped because a negative multiplier inverts the
            // adapter and past ~2 it cooks the sample, and a typo here costs minutes of GPU per
            // window.
            lipdub_strength: req.lipdub_strength.unwrap_or(1.0).clamp(-2.0, 2.0),
            // Caller prompt only. The adapter rides the request's `lora` array as an object (never
            // a `<lora:…>` prompt tag — the LTX stack neither parses NOR strips one, so it would
            // reach the text encoder as literal text and the render would silently carry no
            // adapter at all).
            prompt: req.prompt.clone().unwrap_or_default().trim().to_string(),
        })
    }

    fn window_clip(&self, index: u32) -> PathBuf {
        self.work.join(format!("win_{index:03}.{RELIP_CONTAINER}"))
    }

    /// How many windows are already delivered, contiguous from zero — a gap would put every later
    /// window's frames at the wrong offset.
    async fn windows_on_disk(&self, total: u32) -> u32 {
        let mut n = 0;
        while n < total
            && tokio::fs::try_exists(self.window_clip(n))
                .await
                .unwrap_or(false)
        {
            n += 1;
        }
        n
    }

    /// The extracted frames, extracting them if this is the first run.
    ///
    /// 🔴 The count is stamped in a sentinel written AFTER ffmpeg exits 0, so a run interrupted
    /// mid-extraction re-extracts rather than windowing a truncated source. Skipping the re-extract
    /// on a resume is the point: it is minutes of ffmpeg and it now runs under the GPU guard.
    async fn frames(&self, source: &Path, plan: &RelipPlan) -> Result<u32, String> {
        let sentinel = self.work.join("frames.done");
        if let Ok(text) = tokio::fs::read_to_string(&sentinel).await
            && let Ok(n) = text.trim().parse::<u32>()
        {
            return Ok(n);
        }
        let n = extract_frames(source, &self.frames, plan.width, plan.height)
            .await
            .map_err(|e| format!("extract frames: {e}"))?;
        tokio::fs::write(&sentinel, n.to_string())
            .await
            .map_err(|e| format!("recording the frame count: {e}"))?;
        Ok(n)
    }

    /// The drive track as a 16 kHz stereo wav — the lip-sync driver format, on disk so each window
    /// can be sliced out of it.
    ///
    /// Stereo because the LTX audio VAE is a two-channel model and forcing `-ac 1` collapses the
    /// drive signal before the engine can see both channels. `LTX_DRIVE_AUDIO_CHANNELS=1` on the
    /// engine reverts that without a koblem redeploy.
    async fn drive(&self, audio: &Path) -> Result<(), String> {
        if tokio::fs::try_exists(&self.drive_wav)
            .await
            .unwrap_or(false)
        {
            return Ok(());
        }
        let status = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(audio)
            .args(["-ac", "2", "-ar", "16000"])
            .arg(&self.drive_wav)
            .args(["-loglevel", "error"])
            .status()
            .await
            .map_err(|e| format!("audio convert: {e}"))?;
        if !status.success() {
            return Err("audio convert: ffmpeg failed".to_string());
        }
        Ok(())
    }

    /// Slice `[t0, t0+dur)` seconds out of the drive wav → wav bytes (layout preserved, so a stereo
    /// drive stays stereo — re-imposing `-ac 1` here would undo the convert above).
    async fn slice(&self, t0: f64, dur: f64, index: u32) -> Result<Vec<u8>, String> {
        let out = self.work.join(format!("aud_{index}.wav"));
        let status = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-ss", &format!("{t0}"), "-t", &format!("{dur}"), "-i"])
            .arg(&self.drive_wav)
            .args(["-ac", "2", "-ar", "16000"])
            .arg(&out)
            .args(["-loglevel", "error"])
            .status()
            .await
            .map_err(|e| format!("slice audio (window {index}): {e}"))?;
        if !status.success() {
            return Err(format!("slice audio (window {index}): ffmpeg failed"));
        }
        tokio::fs::read(&out)
            .await
            .map_err(|e| format!("slice audio (window {index}): {e}"))
    }
}

/// Trim-and-concat the window clips into `out`, dropping the leading OVERLAP frames of every window
/// after the first (those cover source frames the previous window already delivered), then mux the
/// full drive audio (aac) over the result. N=1 is a plain audio-mux.
async fn stitch_relip_windows(
    windows: &[PathBuf],
    drive_audio: &Path,
    out: &Path,
) -> std::io::Result<()> {
    if windows.is_empty() {
        return Err(std::io::Error::other("no windows to stitch"));
    }
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.arg("-y");
    for w in windows {
        cmd.arg("-i").arg(w);
    }
    cmd.arg("-i").arg(drive_audio);
    let n = windows.len();
    let audio_idx = n; // last input

    if n == 1 {
        // Single window: keep the video verbatim, just swap in the drive audio.
        cmd.args([
            "-map",
            "0:v",
            "-map",
            &format!("{audio_idx}:a"),
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-shortest",
            "-loglevel",
            "error",
        ]);
    } else {
        // Trim OVERLAP frames off windows >0, then concat the video streams; re-encode (a filter
        // graph can't stream-copy). CRF 16 because this is a master koblem may re-encode again, not
        // a delivery: the engine's own frames are the only picture there will ever be.
        let mut fc = String::new();
        let mut labels = String::new();
        for i in 0..n {
            if i == 0 {
                fc.push_str(&format!("[{i}:v]setpts=PTS-STARTPTS[v{i}];"));
            } else {
                fc.push_str(&format!(
                    "[{i}:v]trim=start_frame={RELIP_OVERLAP},setpts=PTS-STARTPTS[v{i}];"
                ));
            }
            labels.push_str(&format!("[v{i}]"));
        }
        fc.push_str(&format!("{labels}concat=n={n}:v=1:a=0[outv]"));
        cmd.args(["-filter_complex", &fc]);
        cmd.args([
            "-map",
            "[outv]",
            "-map",
            &format!("{audio_idx}:a"),
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "16",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-shortest",
            "-loglevel",
            "error",
        ]);
    }
    let status = cmd.arg(out).status().await?;
    if !status.success() {
        return Err(std::io::Error::other("ffmpeg stitch failed"));
    }
    Ok(())
}

// ── the adapter ──────────────────────────────────────────────────────────────

pub struct LtxRelipAdapter {
    state: AppState,
}

/// How one window's poll ended. Cancellation is not an error: the engine confirmed it.
enum Window {
    Delivered(Box<LtxRenderResult>),
    Cancelled,
}

impl LtxRelipAdapter {
    pub fn new(state: AppState) -> Self {
        LtxRelipAdapter { state }
    }

    async fn asset_path(&self, what: &str, reference: &str) -> Result<PathBuf, String> {
        let hash = director_assets::ref_hash(reference)
            .ok_or_else(|| format!("the relip {what} is not an asset ref"))?;
        let path = director_assets::path_for(self.state.director_assets_dir.as_ref(), hash)
            .ok_or_else(|| format!("the relip {what} names no stored asset"))?;
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Err(director_assets::missing_refs_message(&[hash.to_string()]));
        }
        Ok(path)
    }

    /// The store record's own terminal state. The queue row says `cancelled` on its own; this is
    /// what the browser polling `/jobs/{id}` reads, and it keeps the window count so a resumed
    /// render is not mistaken for one that never started.
    fn cancelled(store: &crate::ltx_jobs::LtxJobStore, job_id: &str, done: u32, total: u32) {
        store.update(job_id, |j| {
            j.state = VideoJobState::Cancelled;
            j.error = Some(format!("cancelled after {done} of {total} windows"));
        });
    }

    fn fail(&self, job_id: &str, msg: &str) {
        warn!(job = %job_id, msg = %msg, "relip failed");
        self.state.ltx_jobs.update(job_id, |j| {
            j.state = VideoJobState::Failed;
            j.error = Some(msg.to_string());
        });
    }

    /// Poll one window to a terminal state, mirroring progress onto the row.
    ///
    /// Only [`Interrupt::Cancel`] is honoured here. A preempt's checkpoint is BETWEEN windows —
    /// that is what `AtCheckpoint` costs, at most one window — and stopping mid-render would throw
    /// away the GPU already spent on it.
    async fn poll(
        &self,
        ctx: &JobCtx,
        engine_id: &str,
        index: u32,
        total: u32,
    ) -> Result<Window, String> {
        let client = &self.state.ltx25;
        let waiter = client.wait(engine_id);
        tokio::pin!(waiter);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut cancelling = false;
        loop {
            tokio::select! {
                done = &mut waiter => {
                    // 🔴 Once a cancel has been asked for, any terminal answer means cancelled —
                    // including a window that finished anyway. The lane is released on the strength
                    // of this returning, so it returns only when the engine has stopped answering
                    // "generating".
                    if cancelling {
                        return Ok(Window::Cancelled);
                    }
                    return match done {
                        Ok(r) => Ok(Window::Delivered(Box::new(r))),
                        Err(LtxError::Cancelled(_)) => Ok(Window::Cancelled),
                        Err(e) => Err(verdict::note(
                            format!("render window {index}/{total}: {e}"),
                            verdict_of(&e),
                        )),
                    };
                }
                _ = ticker.tick() => {
                    if !cancelling && ctx.cancel.get() == Interrupt::Cancel {
                        cancelling = true;
                        if let Err(e) = client.cancel(engine_id).await {
                            warn!(job = %ctx.job.id, engine = %engine_id, error = %e,
                                  "relip: the engine refused the cancel; polling until it stops");
                        }
                    }
                    ctx.progress(
                        JobProgress {
                            phase: Some("rendering".to_string()),
                            ..JobProgress::default()
                        },
                        index as i32,
                    )
                    .await;
                }
            }
        }
    }

    /// Render one window and leave its clip in the scratch directory.
    ///
    /// The window IS the shot: its length is the `control_frames` array, and `video_frames` is
    /// checked against that rather than read as a setting — a disagreement means koblem's own
    /// windowing has drifted from the audio slice.
    async fn render_window(
        &self,
        ctx: &JobCtx,
        p: &RelipPayload,
        s: &Scratch,
        w: &RelipWindow,
        index: u32,
        total: u32,
    ) -> Result<Window, String> {
        let client = &self.state.ltx25;
        // control_frames as PARTS, not base64. This is the largest single payload in the whole
        // system: ~194 KiB per frame, 65 frames per window and ~12 windows on a 30 s clip, which
        // inlined meant ~190 MB of base64 built, parsed and decoded per relip. Frames repeat across
        // the seam between windows, and a part is named by its content hash, so the overlap uploads
        // once.
        let mut uploads = PartUploads::default();
        let mut control_frames = Vec::with_capacity(w.len as usize);
        for f in w.start..w.start + w.len {
            let bytes = tokio::fs::read(frame_path(&s.frames, f + 1))
                .await
                .map_err(|e| format!("read frame {f}: {e}"))?;
            control_frames.push(Value::String(uploads.add(Bytes::from(bytes))));
        }

        let t0 = f64::from(w.start) / f64::from(p.plan.fps);
        let dur = f64::from(w.len) / f64::from(p.plan.fps);
        let wav = s.slice(t0, dur, index).await?;

        let chain = json!({
            "control_frames": control_frames,
            "video_frames": w.len,
            "relip": true,
            "n_segments": 1,
            "lora": [{ "path": LTX_LORA_LIPDUB, "multiplier": s.lipdub_strength }],
            // The caller's two-stage toggle, in the field this engine reads. `hires` would resolve
            // to the same topology, but only by inference and with a warning; an explicit `upscale`
            // is taken as the caller's own choice. The upscaler FILE is not ours to name — the
            // graph loads the 2.5 spatial x2 whatever a request says.
            //
            // ⚠️ It is an identity switch, not a quality dial: guides are cropped before
            // `LTXVLatentUpsampler`, so with it on the frozen source conditions stage 1 ONLY and the
            // stage-2 refine runs with nothing holding the face it is meant to be dubbing.
            "upscale": p.request.two_stage,
            "width": p.plan.width,
            "height": p.plan.height,
            "fps": p.plan.fps,
            "a2v_guidance": p.request.a2v_guidance.unwrap_or(1.0),
            "seed": p.request.seed.unwrap_or(-1),
            "segments": [{ "prompt": s.prompt }],
        });

        // One audio slice → the server sees it as `audio_0`.
        let engine_id = client
            .submit(&chain, &[wav], uploads.entries())
            .await
            .map_err(|e| {
                verdict::note(
                    format!("submit window {index}/{total}: {e}"),
                    verdict_of(&e),
                )
            })?;

        // Published on the QUEUE ROW the moment the engine accepts. This is the field that makes
        // `cancel` and `reattach` possible, so it is written before anything else can go wrong.
        //
        // 🔴 And on the row ONLY. The store record's `server_job_id` is served back as
        // `VideoJobStatus.resume_job_id` — a handle for continuing a chain from its latent bank —
        // and a relip window is not that: every window is sampled fresh from the frozen source,
        // there is no bank behind it, and offering the last one as a resume is a button that
        // renders something nobody asked for.
        if let Err(e) = queue_rows::set_server_job_id(&self.state.db, &ctx.job.id, &engine_id).await
        {
            warn!(job = %ctx.job.id, error = %e, "relip: could not record the engine job id");
        }

        let result = match self.poll(ctx, &engine_id, index, total).await? {
            Window::Delivered(r) => r,
            Window::Cancelled => return Ok(Window::Cancelled),
        };

        // 🔴 Fetched to a `.part` and renamed, so a present `win_NNN.mp4` is always a WHOLE one —
        // that is what lets a resume trust the disk. Streamed to the file the stitcher reads, never
        // held in RAM: a relip is a dozen windows and each is a whole clip.
        let final_path = s.window_clip(index);
        let part = s.work.join(format!("win_{index:03}.part"));
        client
            .media_to_file(&engine_id, &result, &part)
            .await
            .map_err(|e| {
                verdict::note(format!("fetch window {index} media: {e}"), verdict_of(&e))
            })?;
        tokio::fs::rename(&part, &final_path)
            .await
            .map_err(|e| format!("window {index}/{total}: {e}"))?;
        Ok(Window::Delivered(result))
    }

    /// Stitch, file the deliverable under the store id the browser holds, and drop the scratch.
    async fn collect(
        &self,
        ctx: &JobCtx,
        p: &RelipPayload,
        s: &Scratch,
        drive_audio: &Path,
        rendered: &Rendered,
    ) -> Result<(), String> {
        let clips: Vec<PathBuf> = (0..rendered.windows).map(|i| s.window_clip(i)).collect();
        let out_path = s.work.join(format!("final.{RELIP_CONTAINER}"));
        stitch_relip_windows(&clips, drive_audio, &out_path)
            .await
            .map_err(|e| format!("stitch windows: {e}"))?;

        // Both readers take the stitched file AS A FILE: it is on disk already, so nothing here
        // reads a whole clip into memory to make copies of it.
        let store = self.state.ltx_jobs.clone();
        let clip_path = store.clip_path(&p.job_id);
        tokio::fs::copy(&out_path, &clip_path)
            .await
            .map_err(|e| format!("write clip: {e}"))?;
        let clip_bytes = tokio::fs::metadata(&clip_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        if p.request.save_clip {
            // Filed under the engine's own service tag — `web/src/lib/director/engine.ts::
            // historyServiceFor` is the reader, and a bucket nothing was written to shows an empty
            // history with no error at all.
            persist_generation_files(
                &self.state,
                self.state.ltx25.service_tag(),
                "relip",
                json!({
                    "relip": true,
                    "prompt": p.request.prompt,
                    "windows": rendered.windows,
                    "frames": rendered.frames,
                    "width": p.plan.width,
                    "height": p.plan.height,
                    "fps": p.plan.fps,
                    "two_stage": p.request.two_stage,
                    "a2v_guidance": p.request.a2v_guidance,
                    "seed": p.request.seed,
                    "had_video_inputs": true,
                    "queued": true,
                }),
                &[OutputFile {
                    ext: RELIP_CONTAINER.to_string(),
                    mime: RELIP_MIME.to_string(),
                    kind: "video".to_string(),
                    path: clip_path.clone(),
                }],
            )
            .await;
        }

        // Slim request meta for the log (NOT the per-window chain — its control_frames array would
        // bloat the DB by tens of MB per render).
        log_ltx(
            &self.state,
            &self.state.ltx25,
            Some(ctx.job.created_by.clone()),
            &json!({
                "relip": true,
                "windows": rendered.windows,
                "frames": rendered.frames,
                "width": p.plan.width,
                "height": p.plan.height,
                "fps": p.plan.fps,
                "a2v_guidance": p.request.a2v_guidance.unwrap_or(1.0),
                "seed": p.request.seed.unwrap_or(-1),
            }),
            ctx.job.admit_ms.unwrap_or(0) as u64,
            rendered.render_ms,
            200,
            json!({ "bytes": clip_bytes, "relip": true, "windows": rendered.windows }),
        );

        store.update(&p.job_id, |j| {
            j.state = VideoJobState::Completed;
            j.content_type = Some(RELIP_MIME.to_string());
            j.render_ms = rendered.render_ms;
        });
        let row = json!({
            "job_id": p.job_id,
            "url": format!("/api/v1/ltx-video/jobs/{}/media", p.job_id),
        });
        if let Err(e) = queue_rows::set_result(&self.state.db, &ctx.job.id, &row).await {
            warn!(job = %ctx.job.id, error = %e, "relip: could not record the result");
        }

        // Only now: the deliverable is under the store id and in the gallery, so the frames and the
        // window clips are duplicates of bytes the user already owns.
        if let Err(e) = tokio::fs::remove_dir_all(&s.work).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!(job = %ctx.job.id, path = %s.work.display(), error = %e,
                  "relip: could not clear the scratch directory");
        }
        Ok(())
    }

    /// Window by window, then release and collect.
    async fn drive(&self, ctx: &JobCtx, p: &RelipPayload) -> RunResult {
        let store = self.state.ltx_jobs.clone();
        let source = self.asset_path("video", &p.video).await?;
        let drive_audio = self.asset_path("audio", &p.audio).await?;
        let s = Scratch::open(&self.state, &ctx.job.id, &p.request).await?;

        let frames = s.frames(&source, &p.plan).await?;
        if frames == 0 {
            return Err("source video yielded no frames".to_string());
        }
        let windows = plan_relip_windows(frames);
        if windows.is_empty() {
            return Err(format!("source too short to relip ({frames} frames < 9)"));
        }
        let total = windows.len() as u32;

        let mut warnings: Vec<String> = Vec::new();
        // The queue row's `units_total` was sized off ffprobe's frame count; ffmpeg's is the one
        // that renders. A silent difference would leave the row short of its own total forever.
        if total != p.plan.windows {
            warnings.push(format!(
                "the source delivered {frames} frames, which is {total} windows and not the {} \
                 this job was queued as",
                p.plan.windows
            ));
        }
        s.drive(&drive_audio).await?;
        store.update(&p.job_id, |j| {
            j.segments = total;
            j.state = VideoJobState::Rendering;
            j.gate_wait_ms = ctx.job.admit_ms.unwrap_or(0) as u64;
        });

        // 🔴 The FILES are the authority, not the checkpoint: a window is renamed into place only
        // once its fetch completed, so a present clip is a whole one and a crash between the fetch
        // and the checkpoint write must not re-render finished GPU work.
        let done = s.windows_on_disk(total).await;
        if let Some(banked) = ctx
            .resume
            .as_ref()
            .and_then(|r| r.get("windows_done"))
            .and_then(Value::as_u64)
            && banked as u32 != done
        {
            warn!(job = %ctx.job.id, banked, on_disk = done,
                  "relip: the checkpoint and the delivered windows disagree; the files win");
        }
        // 🔴 Said out loud rather than paid silently. `units_done` is what a previous process
        // reported delivering, so if the scratch directory is gone the GPU minutes behind those
        // windows are being spent again and the user is entitled to know why the night was long.
        if done < ctx.job.units_done as u32 {
            warnings.push(format!(
                "{} window(s) this render had already delivered were gone from its scratch \
                 directory and were rendered again",
                ctx.job.units_done as u32 - done
            ));
        }
        if done > 0 {
            info!(job = %ctx.job.id, windows_done = done, windows = total,
                  "relip: resuming rather than re-rendering finished GPU work");
        }

        let started = Instant::now();
        for index in done..total {
            match ctx.cancel.get() {
                Interrupt::Cancel => {
                    Self::cancelled(&store, &p.job_id, index, total);
                    return Ok(JobOutcome::Cancelled);
                }
                // The checkpoint is BETWEEN windows: the delivered clips ARE the resume state, and
                // every window is sampled fresh from the frozen source, so nothing after this one
                // depends on it having run.
                Interrupt::Preempt => {
                    ctx.release_gpu();
                    info!(job = %ctx.job.id, windows_done = index, "relip: preempted at a checkpoint");
                    return Ok(JobOutcome::Preempted {
                        resume: json!({ "windows_done": index }),
                    });
                }
                Interrupt::None => {}
            }
            let rendered = self
                .render_window(ctx, p, &s, &windows[index as usize], index, total)
                .await;
            match rendered {
                Ok(Window::Delivered(_)) => {}
                Ok(Window::Cancelled) => {
                    Self::cancelled(&store, &p.job_id, index, total);
                    return Ok(JobOutcome::Cancelled);
                }
                Err(e) => {
                    self.fail(&p.job_id, &e);
                    return Err(e);
                }
            }
            ctx.progress(
                JobProgress {
                    phase: Some("delivered".to_string()),
                    ..JobProgress::default()
                },
                (index + 1) as i32,
            )
            .await;
        }

        // 🔴 THE ENGINE/DISK BOUNDARY, and it is here rather than after the poll because nothing
        // stitched reaches the engine's volume: a window lives only in the CUDA child's RAM, so
        // whatever takes the card next can unload the only copy. Every window is fetched now; what
        // is left is an ffmpeg concat and an x264 pass over the whole clip, which is minutes and no
        // GPU at all. Past here the job is not cancellable, so the token is not polled again.
        ctx.release_gpu();

        let rendered = Rendered {
            windows: total,
            frames,
            render_ms: started.elapsed().as_millis() as u64,
        };
        if let Err(e) = self.collect(ctx, p, &s, &drive_audio, &rendered).await {
            self.fail(&p.job_id, &e);
            return Err(e);
        }
        Ok(JobOutcome::Done { warnings })
    }
}

/// What the render turned out to be, for the harvest and the log.
struct Rendered {
    windows: u32,
    frames: u32,
    render_ms: u64,
}

#[async_trait]
impl JobRunner for LtxRelipAdapter {
    fn kind(&self) -> JobKind {
        JobKind::LtxRelip
    }

    /// 🔴 ltx25's line, because relip renders on the ltx25 plugin. A claim of its own would price
    /// the same card twice; the mutex between them is the LANE, which `Lane::of` already gives.
    fn service(&self) -> ServiceKind {
        ServiceKind::Ltx
    }

    /// 🔴 At most one window. Each window is a SEPARATE `/ltx/v1/generate` sampled fresh from noise
    /// with the source held frozen beside it — no window continues another, and the delivered
    /// `win_NNN.mp4` files sit on koblem's own disk until the stitch at the end. Nothing is muxed in
    /// place, so stopping between windows loses only the window not yet started.
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
            Ok(p) => shape_of(&p),
            Err(_) => "ltx_relip/unknown".to_string(),
        }
    }

    fn units(&self, payload: &Value) -> Option<i32> {
        parse_payload(payload).ok().map(|p| p.plan.windows as i32)
    }

    fn label(&self, payload: &Value) -> String {
        let Ok(p) = parse_payload(payload) else {
            return "relip".to_string();
        };
        let prompt: String = p
            .request
            .prompt
            .unwrap_or_default()
            .chars()
            .take(40)
            .collect();
        format!(
            "relip · {} windows · {}x{} · {prompt}",
            p.plan.windows, p.plan.width, p.plan.height
        )
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        if !self.state.ltx25.is_configured() {
            return Err("ltx25 is not configured".to_string());
        }
        let p = parse_payload(&ctx.payload)?;
        if self.state.ltx_jobs.get(&p.job_id).is_none() {
            // Without the record there is nowhere to file the clip and nothing for the browser's
            // poll to read, so the render would succeed invisibly.
            return Err(format!(
                "ltx job record `{}` is gone — the relip was not started because its clip would \
                 have nowhere to land",
                p.job_id
            ));
        }
        self.drive(&ctx, &p).await
    }

    /// ⚠️ THE INTERRUPTED WINDOW IS CANCELLED AND RE-RUN, NOT ADOPTED.
    ///
    /// 🔴 `server_job_id` names the window that was in flight, but NOT which window it was, and the
    /// difference decides where its media belongs: adopting it would mean fetching a clip this
    /// process may already have fetched into the NEXT window's file, which the stitch cannot detect
    /// and which delivers the same seconds twice. comfy-api forgets a job after ten minutes anyway,
    /// so a slow reattach finds nothing. Cancelling costs at most one window; every window already
    /// on disk is kept, and if the scratch directory is gone `drive` says so in a warning rather
    /// than re-rendering the night quietly.
    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        if let Some(engine_id) = ctx.job.server_job_id.clone() {
            info!(job = %ctx.job.id, engine = %engine_id,
                  "relip: cancelling the window that was in flight when koblem restarted");
            if let Err(e) = self.state.ltx25.cancel(&engine_id).await {
                warn!(job = %ctx.job.id, engine = %engine_id, error = %e,
                      "relip: could not cancel the interrupted window; it ages out of comfy's TTL");
            }
        }
        self.run(ctx).await
    }

    /// 🔴 Does not return until the engine has confirmed. The caller releases the lane on the
    /// strength of it, and a lane released early means the next job submits into a busy ComfyUI.
    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(engine_id) = job.server_job_id.as_deref() else {
            // Between windows there is nothing in flight; the run loop's own checkpoint stops the
            // sequence.
            return Ok(());
        };
        match self.state.ltx25.cancel(engine_id).await {
            Ok(_) => Ok(()),
            // Gone is stopped: 410 means the record expired, so nothing is holding the worker.
            Err(LtxError::Upstream(404 | 410, _)) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn abandon(&self, job_id: &str, _payload: &Value) {
        self.state
            .ltx_jobs
            .cancel_by_queue_id(job_id, "cancelled before the render started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(width: u32, height: u32, frames: u32, two_stage: bool) -> Value {
        payload_for(
            "koblem-1",
            &format!("asset:{}", "a".repeat(64)),
            &format!("asset:{}", "b".repeat(64)),
            &LtxRelipRequest {
                prompt: Some("a talking head".to_string()),
                two_stage,
                ..LtxRelipRequest::default()
            },
            &RelipPlan {
                width,
                height,
                fps: 24,
                frames,
                windows: plan_relip_windows(frames).len() as u32,
            },
        )
    }

    /// 🔴 The whole point of the key: a 13-window relip and a 2-window one of the same shape share
    /// a bucket, so the long one is estimated from the short one's history.
    #[test]
    fn the_window_count_is_not_in_the_key() {
        let shape = |v: Value| shape_of(&parse_payload(&v).expect("payload"));
        assert_eq!(
            shape(payload(1280, 704, 721, false)),
            shape(payload(1280, 704, 120, false))
        );
        assert_ne!(
            shape(payload(1280, 704, 721, false)),
            shape(payload(1280, 704, 721, true))
        );
        assert_ne!(
            shape(payload(1280, 704, 721, false)),
            shape(payload(640, 352, 721, false))
        );
    }

    /// The windows must tile the source, overlapping by exactly OVERLAP so the stitch's trim
    /// delivers every source frame once. Getting this wrong duplicates or drops footage and the
    /// concat hides it.
    #[test]
    fn windows_tile_the_source_with_one_overlap_each() {
        let windows = plan_relip_windows(400);
        assert!(windows.len() > 1);
        let mut delivered = windows[0].len;
        for pair in windows.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert_eq!(b.start, a.start + (RELIP_WIN - RELIP_OVERLAP));
            assert!(
                b.start + RELIP_OVERLAP <= a.start + a.len,
                "the seam does not overlap"
            );
            assert_eq!(round_down_8k1(b.len), b.len, "a window must be 8k+1");
            delivered += b.len - RELIP_OVERLAP;
        }
        assert!(
            delivered <= 400,
            "delivered {delivered} of 400 source frames"
        );
        assert!(400 - delivered < 9, "the dropped tail is under one window");
    }

    /// A source shorter than one latent step cannot be windowed at all, and the queue stretches the
    /// feedback loop from minutes to hours — so it is refused at enqueue, not at 03:00.
    #[test]
    fn an_unrenderable_source_is_refused_at_enqueue() {
        let check = |v: Value| -> Result<(), String> { validate_payload(&parse_payload(&v)?) };
        assert!(check(payload(1280, 704, 721, false)).is_ok());
        assert!(plan_relip_windows(8).is_empty());
        let short = check(payload(1280, 704, 8, false)).expect_err("8 frames is under one window");
        assert!(short.contains("too short to relip"), "{short}");
        let grid = check(payload(1290, 704, 721, false)).expect_err("1290 is off the 32 grid");
        assert!(grid.contains("multiple of 32"), "{grid}");
        // 🔴 A payload carrying bytes instead of a ref never becomes a job: it would dispatch hours
        // after the browser that uploaded them had gone.
        let inline = parse_payload(&json!({
            "job_id": "koblem-1",
            "video": "AAAA",
            "audio": format!("asset:{}", "b".repeat(64)),
            "request": {},
            "plan": { "width": 1280, "height": 704, "fps": 24, "frames": 721, "windows": 13 },
        }))
        .expect("shape is fine");
        assert!(validate_payload(&inline).is_err());
    }

    /// A single-window relip keeps the whole source in one pass — the case with no seams at all is
    /// the case to aim for.
    #[test]
    fn a_short_source_is_one_window() {
        let windows = plan_relip_windows(RELIP_WIN);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].start, 0);
        assert_eq!(windows[0].len, RELIP_WIN);
    }

    /// Fits inside the window on both axes, keeps the source's orientation, and lands on the
    /// spatial patch grid — the three things the engine refuses a request for.
    #[test]
    fn the_default_canvas_fits_the_window_on_both_axes() {
        for (sw, sh) in [(1920, 1080), (1080, 1920), (3840, 2160), (720, 720)] {
            let (w, h) = target_dims(sw, sh, RELIP_MAX_LONG, RELIP_MAX_SHORT);
            assert_eq!(round32(w), w, "{sw}x{sh} -> {w}x{h}");
            assert_eq!(round32(h), h, "{sw}x{sh} -> {w}x{h}");
            assert!(w.max(h) <= RELIP_MAX_LONG, "{sw}x{sh} -> {w}x{h}");
            assert!(w.min(h) <= RELIP_MAX_SHORT, "{sw}x{sh} -> {w}x{h}");
            assert_eq!(sw >= sh, w >= h, "{sw}x{sh} -> {w}x{h} flipped orientation");
        }
        assert_eq!(
            target_dims(1920, 1080, RELIP_MAX_LONG, RELIP_MAX_SHORT),
            (1248, 704)
        );
    }

    /// 🔴 503 is transient HERE and permanent in `adapters::ltx25`: a relip window never continues
    /// from a bank, so the plugin's "that bank entry is gone" 503 cannot be what this is.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: LtxError| verdict_of(&e);
        assert_eq!(
            t(LtxError::Upstream(503, "draining".into())),
            Failure::Transient
        );
        assert_eq!(
            t(LtxError::Upstream(429, "queue full".into())),
            Failure::Transient
        );
        assert_eq!(
            t(LtxError::Upstream(500, "boom".into())),
            Failure::Transient
        );
        assert_eq!(
            t(LtxError::Upstream(400, "bad window".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(LtxError::Upstream(410, "expired".into())),
            Failure::Permanent
        );
        assert_eq!(t(LtxError::NotConfigured), Failure::Permanent);
        assert_eq!(t(LtxError::Cancelled("j".into())), Failure::Permanent);
        assert_eq!(
            t(LtxError::JobFailed("j".into(), "OOM".into())),
            Failure::Permanent
        );
        assert_eq!(t(LtxError::MissingField("media")), Failure::Permanent);
        assert_eq!(
            t(LtxError::BadMedia("j".into(), "short".into())),
            Failure::Permanent
        );
    }

    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = LtxError::Upstream(503, "relip plugin draining".into());
        let msg = verdict::note(format!("submit window 1/4: {e}"), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Transient);
        let e = LtxError::Upstream(400, "relip plugin draining".into());
        let msg = verdict::note(format!("submit window 1/4: {e}"), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Permanent);
    }
}
