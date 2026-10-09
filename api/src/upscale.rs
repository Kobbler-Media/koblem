//! HTTP client for the LTX-2.5 upscaler — the `upscale` plugin on the `comfy-api` shim.
//!
//! # What this engine is, and what it is NOT
//!
//! An mp4 goes in and a bigger mp4 comes out. It is the only engine koblem talks to whose input is
//! a video and whose output is not generated from a prompt.
//!
//! 🔴 THIS REPLACED SeedVR2 ON MEASUREMENTS, not preference. Same clip, same ground truth,
//! 2026-08-14: SeedVR2's best arm was 230 s with 1.25x the temporal flicker of a bicubic control;
//! this is 48 s at 0.84x. SeedVR2's weights and plugin are gone.
//!
//!   * **Nothing is prompted.** Both text encodes in the graph are the empty string and both CFGs
//!     are 1.0, so the negative branch is never evaluated — the picture being refined IS the
//!     conditioning. Every generative field is a 501 from the plugin naming that.
//!   * **Two knobs.** [`UpscaleRequest::noise`] is how much the refine may redraw and
//!     [`UpscaleRequest::detailer`] toggles an IC-LoRA that is measurably free. The six SeedVR2
//!     had — chunk mode, frames per chunk, overlap, colour correction, checkpoint, VAE tiles — are
//!     gone, because the artefacts they tuned around are gone with the engine.
//!   * **The upsampler is a fixed 2x**, so the target size is chosen by resizing the SOURCE to
//!     half of it. Deliverable sizes are multiples of 64 and frame counts are `8n+1`; the plugin
//!     snaps both and REPORTS what it will actually deliver.
//!   * **fps and audio are preserved by WIRING, not by a field.** An upscaler that resamples or
//!     drops the sound is broken, and the template mechanism refuses to bind over a link.
//!
//! # 🔴 The three walls, and why this client looks the way it does
//!
//! **1. HOST RAM, not VRAM.** ComfyUI holds video as fp32 `(T, H, W, 3)`, so one minute of 1080p
//! output is ~36 GB *per live copy* and the graph holds about four. The plugin publishes a
//! `ram_guard` on `/capabilities` and refuses a window past it. The fix it names is
//! `segment_seconds`, which slices the VIDEO object before it is ever decoded to frames — so this
//! client's job is to compute a split from the source geometry and the published guard, rather
//! than to hand the engine a request it will refuse. See [`plan_segments`].
//!
//! **2. The model's own context is capped at 30 LATENT frames** (~5 s at 24 fps) no matter how
//! long the clip is. So "upscale it all in one pass" would not buy global consistency even if the
//! memory existed. What stability there is across a long clip comes from the pinned seed and from
//! `noise` being low enough that the refine is not free to re-decide the picture.
//!
//! **3. The bytes must not cross HTTP.** A source clip is routinely gigabytes. The plugin's
//! `part:` route would buffer the whole body in koblem, again in comfy-api's multipart parser, and
//! again in the probe's temp copy. So this client uses the `path:` route ONLY: koblem writes (or
//! hard-links) the source into a directory both containers mount at the SAME absolute path
//! (`UPSCALE_STAGING_HOST`, `/var/lib/koblem/upscale-staging`), and sends a path. comfy-api's
//! `stage_path` then streams it into ComfyUI's input dir in 4 MiB chunks. Nothing anywhere holds a
//! whole video in memory.
//!
//! ⚠️ If that mount is missing, `stage_path` answers **503** naming it. That is the failure to look
//! for first when an upscale dies instantly on a large file and works on a small one.
//!
//! # Gate placement: it takes the `minimax_h3` line, and that is deliberate
//!
//! `upscale`, `music3`, `h3` and `ltx25` are four plugins on ONE comfy-api process in front of ONE
//! ComfyUI with a **single-slot queue**. They physically cannot render at once. This client
//! therefore acquires the **`minimax_h3`** budget line, exactly as [`crate::music3`] does and for
//! the same reason — and it **registers no drain/unload URL**, because that URL is the comfy-api
//! container, already registered under `HeavyKind::MinimaxH3` by [`crate::minimax_h3`].
//!
//! 🔴 A CONSEQUENCE WORTH STATING PLAINLY: an upscale cannot run while an H3 or LTX-2.5 render is
//! in flight, and an H3 chain is ONE prompt for the whole job. So an upscale submitted mid-render
//! does not overlap it — it queues behind it, for however long that render takes. This is why the
//! Video → Upscale tab is its own thing rather than a post-phase bolted onto a render job.
//!
//! # ⚠️ NOTHING HAS EVER BEEN RENDERED THROUGH THIS PATH
//!
//! The weights landed 2026-08-14 and the plugin was built the same day; both cards were busy.
//! Every node class, input name and enum on the far side was verified against a live
//! `/object_info`, and the plugin's own test suite holds the graph to its generator — but no frame
//! has gone through it. So:
//!
//!   * the plugin publishes `eta_seconds: null` and this client does not invent one;
//!   * [`RAM_COPIES_ASSUMED`] and the guard it feeds are somebody's arithmetic, not a measurement,
//!     and this client reads the guard from `/capabilities` at submit time so a measured value
//!     changes a number on the far side rather than a constant here;
//!   * wall clock is unknown, so the poll loop has no render timeout of its own.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use kob_gpu_gate::{GpuLock, GpuMinimaxH3Guard};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Connect timeout guards a dead host. No request timeout: an upscale runs for minutes and we poll
/// with our own loop. Same reasoning as every other engine client here.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the poll loop checks job status.
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Transient poll failures (a dropped connection, a brief 5xx) must not abandon a long render.
const MAX_POLL_RETRIES: u32 = 20;
const POLL_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// The container the plugin delivers: h264 in mp4 at CRF 12 (`SaveVideo` defaults to 23, which
/// throws away the detail the render just paid for). Not webm — `SaveVideo` writes mp4 only.
pub const OUTPUT_FORMAT: &str = "mp4";
pub const OUTPUT_MIME: &str = "video/mp4";

/// 🔴 The default scale. 2× is what upstream's own template ships and what the model was
/// demonstrated at; 4× is legal on the wire and will very often trip the RAM guard, because the
/// guard is quadratic in the scale factor.
pub const DEFAULT_MULTIPLIER: f64 = 2.0;

/// 🔴 The fallback copy of the plugin's RAM arithmetic, used ONLY when `/capabilities` cannot be
/// reached. The authority is the far side — [`plan_segments`] takes the guard as an argument
/// precisely so a measured value there changes this client's behaviour without an edit here.
///
/// `frames * width * height * 3 * 4 * copies`, and `copies` counts the full-size fp32 IMAGE
/// tensors the graph holds live at once (the resized input, the padded preprocess output, the
/// decoded output, the postprocess reference).
pub const RAM_COPIES_ASSUMED: u64 = 4;
/// 24 GiB. A property of a 62 GB box shared with a resident DiT, not of the model.
pub const RAM_GUARD_BYTES_ASSUMED: u64 = 24 * 1024 * 1024 * 1024;

/// ⚠️ A floor on how short a computed segment may be, in seconds. Below this the per-prompt
/// overheads (model load, VAE tiling, the colour pass) dominate and the seams multiply for nothing.
/// A request whose geometry demands segments shorter than this is refused with the arithmetic, so
/// the caller lowers the scale instead of getting a hundred two-second joins.
pub const MIN_SEGMENT_SECONDS: f64 = 2.0;

/// A sanity bound matching the plugin's own `max_segments`. Refused here too so the caller sees the
/// arithmetic rather than a 400 from the far side.
pub const MAX_SEGMENTS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("upscale HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("upscale returned {0}: {1}")]
    Upstream(u16, String),
    #[error("upscale service not configured")]
    NotConfigured,
    #[error("upscale job {0} failed: {1}")]
    JobFailed(String, String),
    #[error("upscale job {0} was cancelled")]
    Cancelled(String),
    #[error("upscale response missing required field: {0}")]
    MissingField(&'static str),
    /// The source could not be staged where comfy-api can read it. Its own variant because the
    /// cause is almost always one thing — the shared mount — and the message must say so.
    #[error("could not stage the source clip for the upscaler: {0}")]
    Staging(String),
}

/// Terminal/transient state of a job. The core's five, shared with every plugin on this service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Queued,
    Generating,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" | "pending" => Self::Queued,
            "generating" | "running" => Self::Generating,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" | "canceled" => Self::Cancelled,
            _ => return None,
        })
    }
}

// ── the request a caller builds ──────────────────────────────────────────────

/// How the output size is chosen. Mirrors the plugin's `scale_mode`, which is a dynamic-combo
/// option on `ResizeImageMaskNode` — 🔴 the upsampler is a FIXED 2x, so the target is reached by
/// resizing the SOURCE to half of it, which is why deliverable sizes are multiples of 64. This is
/// what decides the output resolution AND therefore the host RAM the prompt will want.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleMode {
    Multiplier,
    ShorterSide,
    LongerSide,
    Dimensions,
}

impl ScaleMode {
    fn as_wire(self) -> &'static str {
        match self {
            Self::Multiplier => "multiplier",
            Self::ShorterSide => "shorter_side",
            Self::LongerSide => "longer_side",
            Self::Dimensions => "dimensions",
        }
    }
}

/// What a caller asks for. Deliberately small: everything absent is the plugin's default, and the
/// plugin's defaults are chosen for long-clip stability (`noise` 0.4 and a FIXED seed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpscaleRequest {
    /// Absolute path to the source clip, as BOTH containers see it. See the module header: this is
    /// the only route this client uses.
    pub source_path: PathBuf,
    #[serde(default = "default_scale_mode")]
    pub scale_mode: ScaleMode,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    #[serde(default)]
    pub shorter_size: Option<u32>,
    #[serde(default)]
    pub longer_size: Option<u32>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// 🔴 THE ONE KNOB THAT MATTERS: how much of the picture the refine may redraw, as its
    /// starting sigma. 0.0 skips the refine entirely — no transformer, no text encoder, nothing
    /// that can invent. 0.4 is the default. 0.85 is where the generation workflow starts and
    /// where it grew a subject a pair of glasses. Above ~0.3 it CHOOSES detail rather than
    /// recovering it, so two noise levels disagree about what the detail IS; the seed is fixed,
    /// so one request still reproduces exactly.
    #[serde(default)]
    pub noise: Option<f64>,
    /// The detailer IC-LoRA on the transformer. Measured free (48.0 s against 48.1 s) and
    /// PSNR-neutral at `noise 0.4` while the file grows ~6.5% — it adds high-frequency content
    /// rather than moving the picture. Refused at `noise: 0`, where there is no transformer.
    #[serde(default)]
    pub detailer: bool,
    /// How the frame is resampled to the target size before the refine sees it — the plugin's
    /// `SCALE_METHODS`: `nearest-exact` | `bilinear` | `area` | `bicubic` | `lanczos`.
    ///
    /// `None` means the plugin's own `lanczos`, and that is the reason this is an `Option` rather
    /// than a defaulted string: the default belongs to the engine, and a copy here is a second
    /// definition that can drift from it.
    #[serde(default)]
    pub scale_method: Option<String>,
    /// Window into the source, in seconds. `None`/0 means the whole clip.
    #[serde(default)]
    pub start_time: Option<f64>,
    #[serde(default)]
    pub duration: Option<f64>,
    /// 🔴 Normally left `None` and COMPUTED by [`plan_segments`] from the source geometry and the
    /// engine's published RAM guard. Set explicitly only to override that arithmetic.
    #[serde(default)]
    pub segment_seconds: Option<f64>,
}

fn default_scale_mode() -> ScaleMode {
    ScaleMode::Multiplier
}
fn default_multiplier() -> f64 {
    DEFAULT_MULTIPLIER
}

impl UpscaleRequest {
    /// The JSON body `POST /upscale/v1/generate` takes.
    ///
    /// Only fields the caller actually set are emitted. Everything else is the plugin's default,
    /// and those defaults are load-bearing — see the type's field docs.
    pub fn to_wire(&self, segment_seconds: Option<f64>) -> Value {
        let mut body = json!({
            "video": { "path": self.source_path.to_string_lossy() },
            "scale_mode": self.scale_mode.as_wire(),
        });
        let map = body.as_object_mut().expect("object literal");
        match self.scale_mode {
            ScaleMode::Multiplier => {
                map.insert("multiplier".into(), json!(self.multiplier));
            }
            ScaleMode::ShorterSide => {
                if let Some(v) = self.shorter_size {
                    map.insert("shorter_size".into(), json!(v));
                }
            }
            ScaleMode::LongerSide => {
                if let Some(v) = self.longer_size {
                    map.insert("longer_size".into(), json!(v));
                }
            }
            ScaleMode::Dimensions => {
                if let Some(v) = self.width {
                    map.insert("width".into(), json!(v));
                }
                if let Some(v) = self.height {
                    map.insert("height".into(), json!(v));
                }
            }
        }
        if self.detailer {
            map.insert("detailer".into(), Value::from(true));
        }
        for (key, value) in [
            ("scale_method", self.scale_method.clone().map(Value::from)),
            ("noise", self.noise.map(Value::from)),
            ("start_time", self.start_time.map(Value::from)),
            ("duration", self.duration.map(Value::from)),
            ("segment_seconds", segment_seconds.map(Value::from)),
        ] {
            if let Some(v) = value {
                map.insert(key.into(), v);
            }
        }
        body
    }
}

// ── the segment arithmetic ───────────────────────────────────────────────────

/// What the source looks like, as ffprobe reports it. koblem probes BEFORE submitting so the split
/// can be computed and shown to the user, rather than discovered as a 400.
#[derive(Debug, Clone, Copy)]
pub struct SourceGeometry {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_seconds: f64,
}

/// The engine's published RAM guard, read from `/upscale/v1/capabilities`.
#[derive(Debug, Clone, Copy)]
pub struct RamGuard {
    pub max_estimate_bytes: u64,
    pub copies: u64,
}

impl Default for RamGuard {
    /// The fallback, used only when `/capabilities` is unreachable. See [`RAM_GUARD_BYTES_ASSUMED`].
    fn default() -> Self {
        Self {
            max_estimate_bytes: RAM_GUARD_BYTES_ASSUMED,
            copies: RAM_COPIES_ASSUMED,
        }
    }
}

impl RamGuard {
    /// Parse the `ram_guard` block off a capabilities body, falling back field by field. A missing
    /// or malformed block must not stop an upscale — it only makes the split conservative.
    pub fn from_capabilities(caps: &Value) -> Self {
        let d = Self::default();
        let g = caps.get("ram_guard");
        Self {
            max_estimate_bytes: g
                .and_then(|g| g.get("max_estimate_bytes"))
                .and_then(Value::as_u64)
                .unwrap_or(d.max_estimate_bytes),
            copies: g
                .and_then(|g| g.get("copies"))
                .and_then(Value::as_u64)
                .filter(|c| *c > 0)
                .unwrap_or(d.copies),
        }
    }

    /// How many output frames fit under the guard at this geometry. Never zero.
    pub fn frames_that_fit(&self, out_width: u32, out_height: u32) -> u64 {
        let per_frame = u64::from(out_width) * u64::from(out_height) * 3 * 4 * self.copies;
        if per_frame == 0 {
            return 1;
        }
        (self.max_estimate_bytes / per_frame).max(1)
    }
}

/// The output geometry a request will produce, mirroring the plugin's `resolved_size`.
///
/// Duplicated on purpose, exactly as the plugin duplicates ComfyUI's own chunk law: this side needs
/// it to compute the split and to SHOW the user the target before anything is submitted, and the
/// far side needs it to refuse a bad request. Two callers, one arithmetic; the plugin's plan echo
/// carries the authoritative pair and the UI displays that once the 202 lands.
pub fn resolved_size(src: SourceGeometry, req: &UpscaleRequest) -> (u32, u32) {
    let (w, h) = (src.width.max(1), src.height.max(1));
    match req.scale_mode {
        ScaleMode::Multiplier => {
            let m = req.multiplier.max(0.01);
            (
                ((f64::from(w) * m).round() as u32).max(1),
                ((f64::from(h) * m).round() as u32).max(1),
            )
        }
        ScaleMode::ShorterSide => {
            let target = f64::from(req.shorter_size.unwrap_or(0));
            let short = f64::from(w.min(h));
            let f = if short > 0.0 { target / short } else { 1.0 };
            (
                ((f64::from(w) * f).round() as u32).max(1),
                ((f64::from(h) * f).round() as u32).max(1),
            )
        }
        ScaleMode::LongerSide => {
            let target = f64::from(req.longer_size.unwrap_or(0));
            let long = f64::from(w.max(h));
            let f = if long > 0.0 { target / long } else { 1.0 };
            (
                ((f64::from(w) * f).round() as u32).max(1),
                ((f64::from(h) * f).round() as u32).max(1),
            )
        }
        ScaleMode::Dimensions => (
            req.width.unwrap_or(w).max(1),
            req.height.unwrap_or(h).max(1),
        ),
    }
}

/// What the split will be, and why — published to the caller before anything renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentPlan {
    /// `None` when the whole window fits in one prompt, which is the case with no seams at all.
    pub segment_seconds: Option<f64>,
    pub segments: usize,
    pub output_width: u32,
    pub output_height: u32,
    /// The window actually being upscaled, after `start_time`/`duration`.
    pub window_seconds: f64,
    /// Frames the guard says fit in one prompt at this geometry.
    pub frames_per_prompt: u64,
    /// 🔴 Set when the split is more than one. The seams are REAL — each segment is its own refine,
    /// so the model can choose ambiguous texture differently on either side — and a UI that does
    /// not say so is lying about what it produced.
    pub seams: usize,
}

/// Refusals that are arithmetic, not policy — so the message can show the arithmetic.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(
        "at {out_width}x{out_height} only {frames_per_prompt} frames fit in one prompt \
         ({guard_gib:.0} GiB of fp32 IMAGE tensors), which is {seconds:.2} s at {fps:.2} fps — \
         below the {min:.1} s floor. Splitting this far would be dozens of joins, each a place the \
         model can invent different texture. Lower the scale, or upscale a shorter window."
    )]
    SegmentTooShort {
        out_width: u32,
        out_height: u32,
        frames_per_prompt: u64,
        guard_gib: f64,
        seconds: f64,
        fps: f64,
        min: f64,
    },
    #[error(
        "this window is {window:.1} s and splits into {segments} segments of {each:.1} s, over the \
         {max} cap. Each segment is a separate prompt on a single-slot queue shared with every \
         other engine on that box. Upscale it in pieces."
    )]
    TooManySegments {
        window: f64,
        segments: usize,
        each: f64,
        max: usize,
    },
    #[error("the source reports {0}, which cannot be upscaled")]
    UnusableSource(String),
}

/// Compute the split from the source geometry and the engine's published guard.
///
/// 🔴 THIS IS THE WHOLE REASON THE CLIENT PROBES FIRST. The plugin will refuse a window past its
/// RAM guard with a message naming `segment_seconds`; doing the arithmetic here means the caller
/// sees the split (and its seam count) in the accept body instead of a 400, and means the UI can
/// warn before the button is pressed rather than after.
///
/// Returns a plan whose `segment_seconds` is `None` when the whole window fits — which is the only
/// case with no seams, and the case to aim for.
pub fn plan_segments(
    src: SourceGeometry,
    req: &UpscaleRequest,
    guard: RamGuard,
) -> Result<SegmentPlan, PlanError> {
    if src.fps <= 0.0 || src.duration_seconds <= 0.0 || src.width == 0 || src.height == 0 {
        return Err(PlanError::UnusableSource(format!(
            "{}x{} at {:.3} fps, {:.3} s",
            src.width, src.height, src.fps, src.duration_seconds
        )));
    }
    let (out_w, out_h) = resolved_size(src, req);
    let start = req.start_time.unwrap_or(0.0).max(0.0);
    let available = (src.duration_seconds - start).max(0.0);
    let window = match req.duration {
        Some(d) if d > 0.0 => d.min(available),
        _ => available,
    };
    if window <= 0.0 {
        return Err(PlanError::UnusableSource(format!(
            "an empty window (start {start:.3} s of a {:.3} s source)",
            src.duration_seconds
        )));
    }

    let frames_per_prompt = guard.frames_that_fit(out_w, out_h);
    let seconds_per_prompt = frames_per_prompt as f64 / src.fps;
    let total_frames = (window * src.fps).ceil() as u64;

    // The whole window fits: one prompt, no seams. The case worth aiming for.
    if total_frames <= frames_per_prompt {
        return Ok(SegmentPlan {
            segment_seconds: None,
            segments: 1,
            output_width: out_w,
            output_height: out_h,
            window_seconds: window,
            frames_per_prompt,
            seams: 0,
        });
    }

    // An explicit override skips the arithmetic but not the caps: a caller who names a number
    // still gets told when it is unworkable.
    let each = match req.segment_seconds {
        Some(s) if s > 0.0 => s,
        // A small safety margin under the computed ceiling. The guard is an estimate on the far
        // side too, and landing exactly on it is asking for a 400 on the last segment.
        _ => (seconds_per_prompt * 0.95).floor().max(MIN_SEGMENT_SECONDS),
    };
    // 🔴 The floor is tested against the GEOMETRY as well as against `each`: the `.max()` above
    // clamps a too-short computed window back up to the floor, so `each` alone can only ever catch
    // an explicit override — a geometry demanding half-second segments would otherwise be answered
    // with dozens of floor-length ones, every one past the guard the split exists to respect.
    if each < MIN_SEGMENT_SECONDS || seconds_per_prompt < MIN_SEGMENT_SECONDS {
        return Err(PlanError::SegmentTooShort {
            out_width: out_w,
            out_height: out_h,
            frames_per_prompt,
            guard_gib: guard.max_estimate_bytes as f64 / 1024.0_f64.powi(3),
            seconds: each.min(seconds_per_prompt),
            fps: src.fps,
            min: MIN_SEGMENT_SECONDS,
        });
    }
    let segments = (window / each).ceil().max(1.0) as usize;
    if segments > MAX_SEGMENTS {
        return Err(PlanError::TooManySegments {
            window,
            segments,
            each,
            max: MAX_SEGMENTS,
        });
    }
    Ok(SegmentPlan {
        segment_seconds: Some(each),
        segments,
        output_width: out_w,
        output_height: out_h,
        window_seconds: window,
        frames_per_prompt,
        seams: segments.saturating_sub(1),
    })
}

// ── what comes back ──────────────────────────────────────────────────────────

/// The `202`, minus the id. Kept whole for the same reason H3 keeps its plan echo: it carries the
/// numbers the caller did NOT choose — the resolved target geometry, the segment windows, the RAM
/// estimate per segment, and the warnings that say what the engine is about to do differently.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpscaleSubmitPlan {
    pub segments: u32,
    /// `"2560x1408"`. The engine's own resolution of the request, which is the authority — this
    /// client's [`resolved_size`] is the pre-flight copy.
    pub target: Option<String>,
    pub warnings: Vec<String>,
    pub ignored: Vec<String>,
    /// 🔴 Always `None` today, and that is the engine being honest rather than a parse failure:
    /// nothing has ever rendered through this path, so it publishes no ETA rather than a guess.
    pub eta_seconds: Option<f64>,
    pub eta_basis: Option<String>,
}

/// One delivered segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpscaleSegment {
    pub index: u32,
    /// Path on the engine, e.g. `/upscale/v1/jobs/{id}/segments/0`. Fetched, never guessed.
    pub url: String,
    pub start_time: Option<f64>,
    pub duration: Option<f64>,
    /// Probed from the delivered file by the engine, so it describes the bytes rather than the
    /// request. `None` when the probe failed.
    pub output_width: Option<u32>,
    pub output_height: Option<u32>,
    pub duration_seconds: Option<f64>,
}

/// Live progress, for the poll route.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpscaleProgress {
    /// The core's own vocabulary (`loading` / `encoding` / `denoising` / `decoding` / `saving`).
    pub phase: Option<String>,
    pub current_step: Option<u32>,
    /// 🔴 Three, when there is a refine — the schedule is three sigmas down to zero and `noise`
    /// scales them rather than adding steps. `None` on the `noise: 0` arm, where there is no
    /// sampler and a 0/3 bar would be a lie. ⚠️ Also a small share of the wall clock: the tiled VAE
    /// decode dominates and reports nothing.
    pub total_steps: Option<u32>,
    pub progress: Option<f64>,
    pub queue_position: Option<u32>,
    /// Which segment of the koblem-side split is in flight, 1-based, and how many there are. The
    /// engine knows nothing about this: each segment is its own engine job.
    pub segment: Option<u32>,
    pub segment_count: Option<u32>,
}

/// A finished engine job.
#[derive(Debug, Clone)]
pub struct UpscaleCompletion {
    pub segments: Vec<UpscaleSegment>,
    pub plan: Value,
    pub warnings: Vec<String>,
    pub elapsed_seconds: Option<f64>,
    /// 🔴 The engine NEVER concatenates. `result.joined` is always false and the join is koblem's
    /// job — see `routes::upscale`. Carried so nothing downstream can assume otherwise.
    pub joined: bool,
}

#[derive(Clone)]
pub struct UpscaleClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    /// Engine job id of the segment currently in flight, so a cancel can reach it.
    in_flight: Arc<RwLock<Option<String>>>,
    progress: Arc<RwLock<UpscaleProgress>>,
}

impl UpscaleClient {
    /// 🔴 No gate registration, deliberately — see the module header. The comfy-api URL is already
    /// registered under `HeavyKind::MinimaxH3` by the H3 client that shares the container, and a
    /// second registration would replace an entry the gate needs.
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            base_url: Arc::new(RwLock::new(
                base_url
                    .as_ref()
                    .map(|s| s.trim_end_matches('/').to_string()),
            )),
            gate,
            in_flight: Arc::new(RwLock::new(None)),
            progress: Arc::new(RwLock::new(UpscaleProgress::default())),
        }
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalised = base_url
            .as_ref()
            .map(|s| s.trim_end_matches('/').to_string());
        *self.base_url.write().expect("upscale base_url poisoned") = normalised;
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("upscale base_url poisoned")
            .is_some()
    }

    fn base(&self) -> Result<String, Error> {
        self.base_url
            .read()
            .expect("upscale base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        Ok(format!("{}{}", self.base()?, path))
    }

    pub fn progress(&self) -> UpscaleProgress {
        self.progress
            .read()
            .expect("upscale progress poisoned")
            .clone()
    }

    fn set_progress(&self, f: impl FnOnce(&mut UpscaleProgress)) {
        f(&mut self.progress.write().expect("upscale progress poisoned"));
    }

    /// Acquire the GPU on the **`minimax_h3`** budget line — the comfy-api container's one slot,
    /// shared with the `h3`, `ltx25` and `music3` plugins. `None` = administratively disabled.
    pub async fn acquire(&self) -> Option<(GpuMinimaxH3Guard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_minimax_h3().await?;
        Some((guard, start.elapsed()))
    }

    /// Liveness: the shared comfy-api `/health`.
    pub async fn healthy(&self) -> bool {
        let Ok(url) = self.url("/health") else {
            return false;
        };
        matches!(self.http.get(url).send().await, Ok(resp) if resp.status().is_success())
    }

    /// `GET /upscale/v1/capabilities` — the model law, the checkpoint list, and the RAM guard this
    /// client plans its split against.
    ///
    /// Read while a UI is BUILDING a request, so the editor shows the real limits without
    /// submitting anything and without touching the GPU.
    pub async fn capabilities(&self) -> Result<Value, Error> {
        let resp = self
            .http
            .get(self.url("/upscale/v1/capabilities")?)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// The RAM guard to plan against. Falls back to this module's copy when capabilities cannot be
    /// read — a split computed from a stale-but-conservative guard beats refusing to plan at all.
    pub async fn ram_guard(&self) -> RamGuard {
        match self.capabilities().await {
            Ok(caps) => RamGuard::from_capabilities(&caps),
            Err(e) => {
                tracing::warn!(error = %e, "upscale capabilities unreadable; planning the segment \
                                            split against this build's fallback RAM guard");
                RamGuard::default()
            }
        }
    }

    /// `POST /upscale/v1/generate` — JSON only, and the `video` is a PATH.
    ///
    /// 🔴 There is no multipart form here and there should never be one: the source is a video, and
    /// the `part:` route buffers it whole on both sides. See the module header.
    pub async fn submit(
        &self,
        req: &UpscaleRequest,
        segment_seconds: Option<f64>,
    ) -> Result<(String, UpscaleSubmitPlan), Error> {
        let body = req.to_wire(segment_seconds);
        let resp = self
            .http
            .post(self.url("/upscale/v1/generate")?)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            // Every rejection from the plugin carries a JSON problem body whose message is the
            // useful part; a rejection from the layer above may not.
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or(text);
            return Err(Error::Upstream(status, message));
        }
        let body: Value = resp.json().await?;
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or(Error::MissingField("id"))?
            .to_string();
        let plan = UpscaleSubmitPlan {
            segments: body.get("segments").and_then(Value::as_u64).unwrap_or(1) as u32,
            target: body
                .get("target")
                .and_then(Value::as_str)
                .map(str::to_owned),
            warnings: string_list(body.get("warnings")),
            ignored: string_list(body.get("ignored")),
            eta_seconds: body.get("eta_seconds").and_then(Value::as_f64),
            eta_basis: body
                .get("eta_basis")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        *self.in_flight.write().expect("upscale in_flight poisoned") = Some(id.clone());
        Ok((id, plan))
    }

    /// Poll one engine job to a terminal state.
    ///
    /// ⚠️ NO RENDER TIMEOUT, and that is not an oversight: nothing has ever rendered through this
    /// path, so any number here would be a guess with the power to kill a healthy job. The caller's
    /// cooperative cancel is the way out. `MAX_POLL_RETRIES` covers a dropped connection, which is
    /// a different thing from a slow render.
    pub async fn wait(&self, job_id: &str) -> Result<UpscaleCompletion, Error> {
        let mut retries = 0_u32;
        loop {
            let resp = match self
                .http
                .get(self.url(&format!("/upscale/v1/jobs/{job_id}"))?)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    retries += 1;
                    if retries > MAX_POLL_RETRIES {
                        return Err(Error::Request(e));
                    }
                    tokio::time::sleep(POLL_RETRY_BACKOFF).await;
                    continue;
                }
            };
            if !resp.status().is_success() {
                retries += 1;
                if retries > MAX_POLL_RETRIES {
                    return Err(Error::Upstream(
                        resp.status().as_u16(),
                        resp.text().await.unwrap_or_default(),
                    ));
                }
                tokio::time::sleep(POLL_RETRY_BACKOFF).await;
                continue;
            }
            retries = 0;
            let body: Value = resp.json().await?;

            self.set_progress(|p| {
                p.phase = body.get("phase").and_then(Value::as_str).map(str::to_owned);
                p.current_step = body
                    .get("current_step")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                p.total_steps = body
                    .get("total_steps")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                p.progress = body.get("progress").and_then(Value::as_f64);
                p.queue_position = body
                    .get("queue_position")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
            });

            let status = body
                .get("status")
                .and_then(Value::as_str)
                .and_then(JobStatus::parse);
            match status {
                Some(JobStatus::Completed) => {
                    *self.in_flight.write().expect("upscale in_flight poisoned") = None;
                    return Ok(UpscaleCompletion {
                        segments: parse_segments(&body),
                        plan: body.get("plan").cloned().unwrap_or(Value::Null),
                        warnings: string_list(
                            body.pointer("/plan/warnings").or(body.get("warnings")),
                        ),
                        elapsed_seconds: body.get("elapsed_seconds").and_then(Value::as_f64),
                        // 🔴 Read, never assumed. The engine states it explicitly and it is always
                        // false today; if that ever changes, this client must not have hardcoded
                        // the old answer.
                        joined: body
                            .pointer("/result/joined")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
                Some(JobStatus::Failed) => {
                    *self.in_flight.write().expect("upscale in_flight poisoned") = None;
                    // 🔴 NESTED. Every engine client here reads the message with
                    // `/error/message`; a bare string was the old shape and is not what this
                    // service sends.
                    let why = body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("no reason given")
                        .to_string();
                    return Err(Error::JobFailed(job_id.to_string(), why));
                }
                Some(JobStatus::Cancelled) => {
                    *self.in_flight.write().expect("upscale in_flight poisoned") = None;
                    return Err(Error::Cancelled(job_id.to_string()));
                }
                _ => {}
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Fetch one delivered segment. Streamed to `dest` rather than buffered — the output is LARGER
    /// than the input by the square of the scale factor, which is the whole point of the feature
    /// and exactly why it must not be held in memory.
    pub async fn fetch_segment_to(&self, url_path: &str, dest: &Path) -> Result<u64, Error> {
        use tokio::io::AsyncWriteExt;

        let resp = self.http.get(self.url(url_path)?).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| Error::Staging(format!("cannot create {}: {e}", dest.display())))?;
        let mut written = 0_u64;
        let mut stream = resp;
        while let Some(chunk) = stream.chunk().await? {
            file.write_all(&chunk)
                .await
                .map_err(|e| Error::Staging(format!("writing {}: {e}", dest.display())))?;
            written += chunk.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| Error::Staging(format!("flushing {}: {e}", dest.display())))?;
        Ok(written)
    }

    /// Cooperative cancel of whatever segment is in flight.
    pub async fn cancel_in_flight(&self) -> Result<(), Error> {
        let id = self
            .in_flight
            .read()
            .expect("upscale in_flight poisoned")
            .clone();
        match id {
            Some(id) => self.cancel(&id).await,
            None => Ok(()),
        }
    }

    pub async fn cancel(&self, job_id: &str) -> Result<(), Error> {
        let resp = self
            .http
            .post(self.url(&format!("/upscale/v1/jobs/{job_id}/cancel"))?)
            // 🔴 An explicit empty object, not a bodyless POST: a body-less POST hangs cpp-httplib
            // on the sd.cpp fleet and the habit is cheap to keep everywhere.
            .json(&json!({}))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(())
    }

    /// Release the engine's copy of a finished job. Best-effort: comfy-api sweeps on its own TTL,
    /// so a failure here costs disk for a while and nothing else.
    pub async fn delete_job(&self, job_id: &str) -> Result<(), Error> {
        let resp = self
            .http
            .delete(self.url(&format!("/upscale/v1/job?id={job_id}"))?)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(())
    }
}

// ── parsing helpers ──────────────────────────────────────────────────────────

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The poll body's `segments[]`. 🔴 The unit key is `segments` because the plugin sets
/// `unit_key = "segments"`; `partials[]` carries the same set and is what H3/LTX read. Either
/// works, and this reads the typed one.
fn parse_segments(body: &Value) -> Vec<UpscaleSegment> {
    body.get("segments")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(UpscaleSegment {
                        index: row.get("index").and_then(Value::as_u64)? as u32,
                        url: row.get("media").and_then(Value::as_str)?.to_string(),
                        start_time: row.get("start_time").and_then(Value::as_f64),
                        duration: row.get("duration").and_then(Value::as_f64),
                        output_width: row
                            .get("output_width")
                            .and_then(Value::as_u64)
                            .map(|v| v as u32),
                        output_height: row
                            .get("output_height")
                            .and_then(Value::as_u64)
                            .map(|v| v as u32),
                        duration_seconds: row.get("duration_seconds").and_then(Value::as_f64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(w: u32, h: u32, fps: f64, secs: f64) -> SourceGeometry {
        SourceGeometry {
            width: w,
            height: h,
            fps,
            duration_seconds: secs,
        }
    }

    fn req() -> UpscaleRequest {
        UpscaleRequest {
            source_path: PathBuf::from("/var/lib/koblem/upscale-staging/x.mp4"),
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

    /// 🔴 The whole point of the pre-flight: a short clip must render as ONE prompt, because that
    /// is the only case with no seams.
    #[test]
    fn a_short_clip_is_one_prompt_with_no_seams() {
        // An LTX-2.5 shot: 65 frames at 1280x704, 2x -> 2560x1408.
        let plan =
            plan_segments(src(1280, 704, 25.0, 2.6), &req(), RamGuard::default()).expect("fits");
        assert_eq!(plan.segments, 1);
        assert_eq!(plan.seams, 0);
        assert!(plan.segment_seconds.is_none(), "no split, so no field");
        assert_eq!((plan.output_width, plan.output_height), (2560, 1408));
    }

    /// A full-length H3 generation at 2x does NOT fit, and the split has to be computed rather than
    /// discovered as a 400 from the engine.
    #[test]
    fn a_long_h3_generation_splits_and_reports_its_seams() {
        // 15 s at 24 fps = 360 frames at 1280x704 -> 2560x1408.
        let plan =
            plan_segments(src(1280, 704, 24.0, 15.0), &req(), RamGuard::default()).expect("splits");
        assert!(plan.segments > 1, "{plan:?}");
        assert_eq!(plan.seams, plan.segments - 1);
        let each = plan.segment_seconds.expect("a split names its window");
        assert!(each >= MIN_SEGMENT_SECONDS);
        // Every segment must fit under the guard, which is the property the split exists for.
        let frames_each = (each * 24.0).ceil() as u64;
        assert!(frames_each <= plan.frames_per_prompt, "{plan:?}");
        // And the segments must cover the window.
        assert!(each * plan.segments as f64 >= plan.window_seconds - 0.001);
    }

    /// 4x is quadratic in the guard, so it refuses with the arithmetic rather than splitting into
    /// dozens of joins. The message is the deliverable here — it has to say what to do instead.
    #[test]
    fn an_impossible_scale_refuses_with_the_arithmetic() {
        let mut r = req();
        r.multiplier = 4.0;
        let err = plan_segments(src(1920, 1088, 24.0, 60.0), &r, RamGuard::default())
            .expect_err("7680x4352 cannot be segmented sanely");
        let msg = err.to_string();
        assert!(msg.contains("7680x4352"), "{msg}");
        assert!(msg.contains("Lower the scale"), "{msg}");
    }

    /// The guard comes off the wire so a MEASURED value on the engine changes the split here
    /// without an edit. A generous guard collapses a split back to one prompt.
    #[test]
    fn a_measured_guard_from_capabilities_changes_the_split() {
        let caps = json!({ "ram_guard": { "max_estimate_bytes": 96_u64 * 1024 * 1024 * 1024,
                                          "copies": 2 } });
        let guard = RamGuard::from_capabilities(&caps);
        assert_eq!(guard.copies, 2);
        let plan = plan_segments(src(1280, 704, 24.0, 15.0), &req(), guard).expect("fits now");
        assert_eq!(plan.segments, 1, "a bigger guard means no seams: {plan:?}");
    }

    /// A malformed or absent block must not stop an upscale — it only makes the split conservative.
    #[test]
    fn a_missing_ram_guard_falls_back_rather_than_failing() {
        let guard = RamGuard::from_capabilities(&json!({}));
        assert_eq!(guard.max_estimate_bytes, RAM_GUARD_BYTES_ASSUMED);
        assert_eq!(guard.copies, RAM_COPIES_ASSUMED);
        // A zero `copies` would divide the arithmetic by nothing; it is filtered, not trusted.
        let zero = RamGuard::from_capabilities(&json!({ "ram_guard": { "copies": 0 } }));
        assert_eq!(zero.copies, RAM_COPIES_ASSUMED);
    }

    /// `start_time`/`duration` narrow the window, and the split is computed against the WINDOW
    /// rather than the source — upscaling 10 s out of an hour must not read as an hour.
    #[test]
    fn the_split_is_computed_against_the_window_not_the_source() {
        let mut r = req();
        r.start_time = Some(600.0);
        r.duration = Some(4.0);
        let plan = plan_segments(src(1280, 704, 24.0, 3600.0), &r, RamGuard::default())
            .expect("a 4 s window of an hour-long source");
        assert_eq!(plan.segments, 1);
        assert!((plan.window_seconds - 4.0).abs() < 1e-6);
    }

    /// A duration past the end truncates to what exists rather than planning against a number the
    /// source cannot supply.
    #[test]
    fn a_window_past_the_end_is_clamped() {
        let mut r = req();
        r.start_time = Some(8.0);
        r.duration = Some(600.0);
        let plan =
            plan_segments(src(640, 360, 24.0, 10.0), &r, RamGuard::default()).expect("clamped");
        assert!((plan.window_seconds - 2.0).abs() < 1e-6, "{plan:?}");
    }

    /// 🔴 The wire uses the PATH route and nothing else. A `part` here would buffer a whole video
    /// in three places; the test exists so that never quietly reappears.
    #[test]
    fn the_wire_names_a_path_and_never_a_part() {
        let body = req().to_wire(Some(6.0));
        assert_eq!(
            body.pointer("/video/path").and_then(Value::as_str),
            Some("/var/lib/koblem/upscale-staging/x.mp4")
        );
        assert!(body.pointer("/video/part").is_none());
        assert_eq!(
            body.get("segment_seconds").and_then(Value::as_f64),
            Some(6.0)
        );
        assert_eq!(body.get("multiplier").and_then(Value::as_f64), Some(2.0));
    }

    /// Only the fields a caller SET are sent. Everything else is the plugin's default, and those
    /// defaults (overlap 2, colour `lab`, a fixed seed) are chosen for long-clip stability —
    /// echoing our own copy of them would make this client the place they drift.
    #[test]
    fn unset_options_are_absent_rather_than_defaulted_here() {
        let body = req().to_wire(None);
        for absent in ["segment_seconds", "noise", "detailer"] {
            assert!(body.get(absent).is_none(), "{absent} should not be sent");
        }
    }

    /// A dimensions request sends its own pair and NOT `multiplier` — the far side's `resize_type`
    /// is a dynamic combo whose options contribute different inputs, and a stale sibling key is
    /// dropped silently there rather than rejected.
    #[test]
    fn each_scale_mode_sends_only_its_own_keys() {
        let mut r = req();
        r.scale_mode = ScaleMode::Dimensions;
        r.width = Some(1920);
        r.height = Some(1088);
        let body = r.to_wire(None);
        assert_eq!(
            body.get("scale_mode").and_then(Value::as_str),
            Some("dimensions")
        );
        assert_eq!(body.get("width").and_then(Value::as_u64), Some(1920));
        assert!(body.get("multiplier").is_none());

        r.scale_mode = ScaleMode::ShorterSide;
        r.shorter_size = Some(1080);
        let body = r.to_wire(None);
        assert_eq!(body.get("shorter_size").and_then(Value::as_u64), Some(1080));
        assert!(body.get("width").is_none());
    }

    /// The completion parser reads `joined` off the body rather than assuming today's answer.
    #[test]
    fn segments_and_the_join_flag_come_off_the_body() {
        let body = json!({
            "status": "completed",
            "segments": [
                {"index": 0, "media": "/upscale/v1/jobs/j/generations/0",
                 "start_time": 0.0, "duration": 6.0,
                 "output_width": 2560, "output_height": 1408, "duration_seconds": 6.01},
                {"index": 1, "media": "/upscale/v1/jobs/j/generations/1",
                 "start_time": 6.0, "duration": 4.0}
            ],
            "result": {"joined": false}
        });
        let segs = parse_segments(&body);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].output_width, Some(2560));
        assert_eq!(segs[1].start_time, Some(6.0));
        assert!(
            segs[1].output_width.is_none(),
            "an unprobed segment stays null"
        );
        assert_eq!(
            body.pointer("/result/joined").and_then(Value::as_bool),
            Some(false)
        );
    }
}
