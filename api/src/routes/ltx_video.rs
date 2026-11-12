//! LTX multi-segment video routes — a director script of per-segment prompts rendered as one
//! resident-DiT chain and stitched into a single clip, on the `ltx25` plugin. Auth-gated.
//!
//! 🔴 **THIS MODULE NO LONGER RENDERS.** [`compile_ltx_chain`] turns a request into the engine
//! chain and [`enqueue_ltx`] puts it on the global queue; `queue::adapters::ltx25` submits, polls
//! and collects. That is the only path to the card — a direct route left alive beside the queue is
//! two writers to one engine. What stays here is everything that is genuinely about the REQUEST:
//!
//!   1. Resolve the optional reference portrait (upload / url) → a `part:<name>` marker.
//!   2. Resolve audio per role: the voice component drives per-segment lip-sync — and is only
//!      ever a clean voice, so BGM can't move the mouths. A `mixed` upload is split via songgen
//!      `/separate` ONLY to obtain that voice, then the original full-quality mix is handed over
//!      as the deliverable track; voice-only likewise delivers the original continuous upload.
//!      A `bgm` upload replaces the generated track.
//!   3. Lay out every shot's visible / render / seam-drop window and compile the chain JSON.
//!   4. Capture every part and every transcoded wav into the content-addressed store and enqueue.
//!
//! koblem owns separation and choosing the original full-fidelity source as the deliverable, so no
//! separation artifacts / `/separate` length truncation reach a final mixed-or-bgm track, and BGM
//! never reaches the lip-sync driver. The engine owns per-segment slicing and the final mux.
//!
//! RELIP goes the same way — [`relip`] captures its two uploads and enqueues `JobKind::LtxRelip`,
//! and `queue::adapters::ltx_relip` owns the windowing, the render and the stitch.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine as _;
use bytes::Bytes;
use koblem_client::types::{
    Job as QueueJob, JobKind, JobState, LTX_REF_SHARE_CAPTURE, LTX_REF_SHARE_WARN, LtxAudioSource,
    LtxBeat, LtxCharacterRef, LtxFrameDrop, LtxFrameDropMode, LtxIdentityRegime, LtxImageSource,
    LtxKeyframe, LtxLoraInfo, LtxPromptFromLlmRequest, LtxPromptFromLlmResponse, LtxPromptImage,
    LtxQualityProfile, LtxRefResizeMode, LtxRelipRequest, LtxRuntimeLora, LtxScriptFromLlmRequest,
    LtxScriptFromLlmResponse, LtxSegmentMode, LtxShotAudioKind, LtxUpscalerRatio, LtxV2vSource,
    LtxVideoGenerateRequest, LtxVideoProps, VideoJobListResponse, VideoJobState, VideoJobStatus,
    VideoJobSubmitResponse, ltx_apply_lora_triggers, ltx_character_prompt, ltx_ref_token_share,
};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::AppState;
use crate::auth::require_auth;
use crate::director_assets::{AssetJson, PartUploads, StagedParts};
use crate::routes::engine_cancel;
use crate::routes::ltx_prompting;

/// 256 MB body cap — a long voice/mixed track + a 4K portrait, well under the
/// server's own 512 MB limit.
const LTX_BODY_LIMIT: usize = 256 * 1024 * 1024;
/// Seam headroom (frames) padded onto each continuation shot's RENDER length, and — sent as
/// `cont_seam_drop_frames` — the exact number the engine trims back off at the join.
///
/// It is `8 * K`, derived, not tuned: the K carried latents occupy one 8-frame latent group each.
/// The causal VAE decodes them to `1 + (K-1)*8` real frames and the model spends the remaining 7
/// re-rendering its way into the group before it produces genuinely new content, so `8 * K` is the
/// whole re-rendered block. (The old hardcoded 24 was this formula evaluated at the then-current
/// K=3 and left behind when K moved to 2 — the engine trimmed ~8-16 while koblem still padded 24,
/// which is what desynced the audio after each seam.)
///
/// Two invariants ride on it, which is why both sides MUST use the same number:
///   * `visible + overlap` stays a valid 8k+1 latent-frame count (8*K is a multiple of 8);
///   * produced == requested `shot_visible` EXACTLY, and each segment's drive-audio window is cut
///     against the trim that is actually applied — so picture and track are frame-exact at the seam.
const fn ltx_chain_seam_drop(cont_latent_frames: u32) -> u32 {
    8 * cont_latent_frames
}

/// Default hold strength for image pins (keyframes / scene images / opener) when the request
/// doesn't set `pin_strength`. 1.0 = the pinned image is held exactly at its frame (crispest);
/// the UI can soften it toward the generated motion.
const DEFAULT_PIN_STRENGTH: f64 = 1.0;

/// Default shot length in frames — 169 = `8*21+1` = 7.04s at 24fps (the fps every UI defaults to).
///
/// Sized off the MEASURED VRAM envelope, not a round number. The 22B DiT tops out at 241 render
/// frames (see [`LTX_MAX_RENDER_FRAMES`]); a shot also has to pay for its seam overlap, its
/// character references and the rest of the per-request budget, which the owner's arithmetic puts
/// at ~3s of the 10s ceiling. 7s is what is left, and LTX holds content coherence well past it
/// (it is specified to 20s), so drift is not the binding constraint here — VRAM is.
const DEFAULT_SEGMENT_FRAMES: u32 = 169;

/// Hard ceiling on the RENDER frame count of a single shot (visible + seam overlap) — the number
/// koblem will ask the engine to sample.
///
/// 🔴 STILL 241, and it is LTX-2.5's own limit: that engine OOMs at exactly 241 frames at
/// 1920×1088 inside `VAEDecodeTiled`. The number arrived as a VRAM measurement on the retired 2.3
/// engine (241f = 11,464 MiB against an 11,500 MiB target; 249f = 11,836 MiB; 273f = 12,030 MiB)
/// and the two agreeing is a coincidence worth nothing — what keeps it here is the 2.5 failure.
///
/// ⚠️ It is NOT a tiling threshold, and the two must not be conflated: nobody asks for a tiled
/// render by asking for a longer shot, so koblem refuses rather than letting the engine quietly
/// downgrade one.
///
/// If the envelope is ever re-measured, move this with it.
const LTX_MAX_RENDER_FRAMES: u32 = 241;

// ┌───────────────────────────────────────────────────────────────────────────────────────────┐
// │ LTX-2.3 SIGMA SCHEDULES — READ THIS BEFORE TOUCHING ANY σ VALUE                              │
// ├───────────────────────────────────────────────────────────────────────────────────────────┤
// │ These constants are CONSTANTLY confused because THREE distinct renoise `sigma0` (σ0) regimes │
// │ coexist here, each belonging to a DIFFERENT pipeline. The σ0 is the first value of a refine  │
// │ stage's schedule — the noise level that stage starts from. What matters is whether a refine  │
// │ CONTINUES the base trajectory (starts where the base ended) or SDEdit-RENOISES UP (starts     │
// │ ABOVE where the base left the latent, to re-synthesise detail at higher resolution). The σ0   │
// │ tells you which. Do not mix them up:                                                          │
// │                                                                                               │
// │ • σ0 = 0.725  — "Standard" 2-stage, ver3 DISTILLED, CONTINUE-TRAJECTORY (engine               │
// │                 LTX_HIRES_CONTINUE=1). Base = 6-step LCM `LTX_VER3_BASE_SIGMAS`, which ENDS at │
// │                 0.725; the refine `LTX_VER3_REFINE_SIGMAS` = [0.725, 0.421875, 0.0] then       │
// │                 CONTINUES from that exact 0.725 with NO SDEdit re-noise (2-step Euler). The     │
// │                 base decoded alone (ending at 0.725) is the "preview"; production CONTINUES it. │
// │                 Engine mirror: `kVer3TwoPassBase` / `kVer3SinglePass` in stable-diffusion.cpp.  │
// │                                                                                               │
// │ • σ0 = 0.85   — "High" (2-stage 8/3) and "4x Upscale" (3-stage 8/3/3). Base = 8-step           │
// │                 euler_a_cfg_pp `LTX_HIGH_BASE_SIGMAS` (runs all the way to 0.0); THEN           │
// │                 each ×2 upscale stage SDEdit-RENOISES the upscaled latent UP to 0.85 and        │
// │                 resamples down: `LTX_UPSCALE_STAGE_SIGMAS` = [0.85, 0.725, 0.421875, 0.0]       │
// │                 (3-step euler_cfg_pp), anchored by `LTX_UPSCALE_STAGE_SIGMA0` = 0.85. THIS is    │
// │                 the community "make videos" recipe (RuneXX I2V_T2V_Basic: 8-step base → σ0=0.85 │
// │                 upscale). 0.85 is a fresh renoise level, NOT a point on the base trajectory.     │
// │                                                                                               │
// │ • σ0 = 0.909375 — RELIP / seam-resume "stage-2 distilled" ONLY (a SEPARATE continuation/resume │
// │                 pipeline, NOT plain t2v/i2v, and NOT the main-render upscale renoise). It lives  │
// │                 ENGINE-SIDE as `kStage2DistilledSigmas` = {0.909375, 0.725, 0.421875, 0.0} in    │
// │                 stable-diffusion.cpp — this file NEVER emits a 0.909375 renoise σ0. The 0.909375 │
// │                 you DO see below is merely an INTERMEDIATE value inside the base schedules        │
// │                 (position 6 of the 6/8-step ladders), not a renoise anchor. In community          │
// │                 ComfyUI graphs 0.909375 shows up the same way plus as the unwired legacy           │
// │                 "ManualSigmas (LTX-2.0)" node — never as the main upscale renoise.                 │
// │                                                                                               │
// │ Quick map: Standard = 6+2, continue @0.725 · High = 8+3, SDEdit @0.85 · Upscale4x = 8+3+3,      │
// │ SDEdit @0.85 per stage · relip = engine-only, @0.909375. See resolve_quality_plan() below.      │
// └───────────────────────────────────────────────────────────────────────────────────────────┘

/// ver3 "distilled" base-pass sigma schedule (6-step LCM, cfg 1.0). Sent as
/// `sample_params.custom_sigmas`; overridable via `base_custom_sigmas`. ENDS at 0.725 (not 0.0):
/// the refine CONTINUES from there — see the "LTX-2.3 SIGMA SCHEDULES" banner above.
const LTX_VER3_BASE_SIGMAS: [f32; 7] = [1.0, 0.99375, 0.9875, 0.98125, 0.975, 0.909375, 0.725];
/// ver3 "distilled" refine (hires) sigma schedule (2-step Euler). Sent as
/// `hires.custom_sigmas`; overridable via `refine_custom_sigmas`. σ0 = 0.725 == where
/// `LTX_VER3_BASE_SIGMAS` ended: this is a trajectory CONTINUE, NOT a σ0=0.85 SDEdit renoise
/// (banner above).
const LTX_VER3_REFINE_SIGMAS: [f32; 3] = [0.725, 0.421875, 0.0];

/// "High" / "4x Upscale" base-pass sigma schedule (8-step, cfg_pp). Sent as
/// `sample_params.custom_sigmas` when those profiles are selected. Runs the full ladder to 0.0
/// (unlike the ver3 base, which stops at 0.725); the 0.909375 at position 6 is a mid-schedule
/// step, NOT a renoise σ0 — see the "LTX-2.3 SIGMA SCHEDULES" banner above.
const LTX_HIGH_BASE_SIGMAS: [f32; 9] = [
    1.0, 0.99375, 0.9875, 0.98125, 0.975, 0.909375, 0.725, 0.421875, 0.0,
];
/// Per-stage SDEdit refine sigma schedule for a ×2 `hires_chain` stage (3-step Euler cfg_pp).
/// σ0 = 0.85 RE-NOISES the upscaled latent UP (fresh noise level, above the base trajectory) —
/// this is the community "make videos" upscale recipe. Contrast the ver3 continue @0.725 (banner).
const LTX_UPSCALE_STAGE_SIGMAS: [f32; 4] = [0.85, 0.725, 0.421875, 0.0];
/// The baked final-stage renoise `sigma0` (== `LTX_UPSCALE_STAGE_SIGMAS[0]`). The "detail /
/// upscale strength" control rescales the whole final-stage schedule around this anchor.
const LTX_UPSCALE_STAGE_SIGMA0: f64 = 0.85;

/// Final upscale stage's SDEdit sigma schedule, optionally rescaled by the request's "detail /
/// upscale strength" (`upscale_detail_strength`) so its renoise `sigma0` becomes that value.
/// `None` → the exact baked [`LTX_UPSCALE_STAGE_SIGMAS`] (byte-identical). `Some(d)` → the whole
/// schedule scaled by `d / 0.85` (terminal `0.0` preserved), keeping it monotonically decreasing:
/// higher = more new detail synthesised at the final upscale, lower = more faithful to the base.
fn final_stage_sigmas(detail: Option<f64>) -> Vec<f32> {
    match detail {
        None => LTX_UPSCALE_STAGE_SIGMAS.to_vec(),
        Some(d) => {
            let factor = (d / LTX_UPSCALE_STAGE_SIGMA0) as f32;
            LTX_UPSCALE_STAGE_SIGMAS
                .iter()
                .map(|&s| if s == 0.0 { 0.0 } else { s * factor })
                .collect()
        }
    }
}
/// Default quality negative prompt applied by the High / 4x Upscale profiles (and offered as
/// a Custom starting point). Only meaningfully active with cfg>1 or a `*_cfg_pp` sampler —
/// which those profiles use (cfg_pp folds the uncond in even at cfg 1).
const LTX_DEFAULT_QUALITY_NEGATIVE: &str = "low resolution, blurry, grainy, pixelated, motion \
     blur, low detail, flat lighting, noisy image, poor texture, soft edges, muted colors, \
     overexposed, underexposed, static, no movement, still frame, low quality";

/// The resolved base-pass + hires plan for a render, materialised from the request's
/// [`LtxQualityProfile`]. `Standard` resolves to today's exact values (byte-identical request);
/// High / 4x bake the contract recipe; Custom reads the request's override fields + `hires_chain`.
struct QualityPlan {
    /// Base sampler — drives BOTH the top-level `sampling_method` and `sample_params.sample_method`
    /// (historically the same value).
    base_sample_method: String,
    /// Base-pass sigma schedule (`sample_params.custom_sigmas`).
    base_sigmas: Vec<f32>,
    /// Base-pass step count (top-level `steps`).
    base_steps: u32,
    /// Base-pass guidance (top-level `cfg_scale`).
    base_cfg: f64,
    /// Uncond conditioning. `None` → omit `negative_prompt` (Standard back-compat).
    negative_prompt: Option<String>,
    /// Multi-stage upscale+refine chain. `Some(non-empty)` → emit `hires_chain` and skip the single
    /// `hires{}` block; `None` → fall back to the single-hires path (Standard back-compat).
    hires_chain: Option<Vec<Value>>,
    /// Engine base-pass resolution (already ×32-snapped, min 32). Under the default (legacy) base
    /// semantics this is the request's `width`/`height` unchanged. Under `dims_are_target` it is the
    /// target OUTPUT (`width`/`height`) divided by this profile's total upscale factor, so every
    /// profile lands on the SAME output instead of overshooting it (a 4×-upscale render starts from
    /// a quarter-size base, not the full frame). See [`resolve_quality_plan`].
    base_width: u32,
    base_height: u32,
}

/// Build one `hires_chain` stage object in the shared engine shape (one upsample + SDEdit refine).
fn hires_stage_json(
    upscaler: &str,
    sigmas: Vec<f32>,
    sample_method: &str,
    cfg: f64,
    steps: u32,
) -> Value {
    json!({
        "upscaler": upscaler,
        "custom_sigmas": sigmas,
        "sample_method": sample_method,
        "cfg": cfg,
        "steps": steps,
    })
}

/// Total spatial ×-factor a resolved hires stage applies, inferred from its upscaler name (mirrors
/// [`LtxUpscalerRatio::scale_mult`]): the ×1.5 upscaler → 1.5, everything else (the ×2 default) →
/// 2.0. Used to size a `dims_are_target` base pass down to the requested output.
fn stage_scale_mult(upscaler: &str) -> f64 {
    if upscaler.contains("x1.5") { 1.5 } else { 2.0 }
}

/// Engine base-pass dimension that upscales to `target` after a refine chain of total factor
/// `mult`, snapped to the ×32 LTX spatial grid (min 32). `mult <= 1.0` → the target itself (no
/// refine downscale). The caller is expected to have snapped `target` to a multiple of
/// `32 × mult` so the base is a clean ×32 value and every intermediate ×2 stage stays on-grid;
/// [`round32`] still guards off-grid API callers so the engine never sees a non-multiple.
fn base_from_target(target: u32, mult: f64) -> u32 {
    if mult <= 1.0 {
        return round32(target);
    }
    round32((target as f64 / mult).round() as u32)
}

/// Resolve the request's quality profile into a concrete [`QualityPlan`]. The default
/// (`Standard`/absent) reproduces the historical single-hires ver3 request EXACTLY — the base
/// sampler/sigmas/steps/cfg fall to the same values and no `negative_prompt`/`hires_chain` is
/// materialised, so a Standard render is byte-identical to today.
///
/// For the σ0 regimes each arm emits (Standard = continue @0.725 · High/4x = SDEdit @0.85), see the
/// "LTX-2.3 SIGMA SCHEDULES" banner above the sigma constants. relip's @0.909375 is engine-only and
/// is NOT produced here.
///
/// Resolution scaling: by default `width`/`height` are the engine BASE pass (legacy semantics —
/// what every existing caller sends). When `dims_are_target` is set (the Director), `width`/`height`
/// are instead the desired OUTPUT resolution and the base pass is sized DOWN by the profile's total
/// upscale factor — so a 4×-upscale render starts from a quarter-size base and every profile lands
/// on the SAME output, rather than the multi-stage chains ballooning past the target. The Director
/// snaps the target to a multiple of `32 × factor` so the base is a clean ×32 value.
fn resolve_quality_plan(req: &LtxVideoGenerateRequest) -> QualityPlan {
    // koblem's default ×2 spatial upscaler — the stage upscaler for High/4x and the fallback for a
    // Custom stage that doesn't name one.
    let x2 = LtxUpscalerRatio::X2
        .upscaler_name()
        .unwrap_or("ltx-2.3-spatial-upscaler-x2-1.1");
    // High/4x negative: honour a caller-supplied non-empty override, else the baked recipe.
    let default_negative = || {
        req.negative_prompt
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| LTX_DEFAULT_QUALITY_NEGATIVE.to_string())
    };
    // Base-pass resolution. Under `dims_are_target` the request dims are the OUTPUT and the base is
    // that divided by the profile's total upscale `mult` (×32-snapped); otherwise the dims ARE the
    // base (legacy — byte-identical for every non-Director caller). Each arm passes its own factor.
    let dims_target = req.dims_are_target.unwrap_or(false);
    let req_w = req.width.unwrap_or(1280);
    let req_h = req.height.unwrap_or(704);
    let base_dims = |mult: f64| -> (u32, u32) {
        if dims_target {
            (base_from_target(req_w, mult), base_from_target(req_h, mult))
        } else {
            (round32(req_w), round32(req_h))
        }
    };
    match req.profile.unwrap_or_default() {
        LtxQualityProfile::Standard => {
            // One single-hires stage at the effective upscaler ratio (×2 default; `none` → the base
            // IS the output). Target-mode base = output / ratio.
            let (base_width, base_height) = base_dims(req.effective_upscaler_ratio().scale_mult());
            QualityPlan {
                base_sample_method: req
                    .sampling_method
                    .clone()
                    .unwrap_or_else(|| "lcm".to_string()),
                base_sigmas: req
                    .base_custom_sigmas
                    .clone()
                    .unwrap_or_else(|| LTX_VER3_BASE_SIGMAS.to_vec()),
                base_steps: req.steps.unwrap_or(6),
                base_cfg: req.cfg_scale.unwrap_or(1.0),
                negative_prompt: req.negative_prompt.clone(),
                hires_chain: None,
                base_width,
                base_height,
            }
        }
        LtxQualityProfile::High => {
            // One ×2 refine stage → base = output / 2.
            let (base_width, base_height) = base_dims(2.0);
            QualityPlan {
                base_sample_method: "euler_a_cfg_pp".to_string(),
                base_sigmas: LTX_HIGH_BASE_SIGMAS.to_vec(),
                base_steps: 8,
                base_cfg: 1.0,
                negative_prompt: Some(default_negative()),
                // Single ×2 refine — the final stage, so its renoise sigma0 follows the detail
                // control (None → baked schedule, byte-identical).
                hires_chain: Some(vec![hires_stage_json(
                    x2,
                    final_stage_sigmas(req.upscale_detail_strength),
                    "euler_cfg_pp",
                    1.0,
                    3,
                )]),
                base_width,
                base_height,
            }
        }
        LtxQualityProfile::Upscale4x => {
            // Two ×2 refine stages → base = output / 4.
            let (base_width, base_height) = base_dims(4.0);
            QualityPlan {
                base_sample_method: "euler_a_cfg_pp".to_string(),
                base_sigmas: LTX_HIGH_BASE_SIGMAS.to_vec(),
                base_steps: 8,
                base_cfg: 1.0,
                negative_prompt: Some(default_negative()),
                hires_chain: Some(vec![
                    // First ×2 refine: fixed schedule (the detail control only steers the LAST stage).
                    hires_stage_json(
                        x2,
                        LTX_UPSCALE_STAGE_SIGMAS.to_vec(),
                        "euler_cfg_pp",
                        1.0,
                        3,
                    ),
                    // Final ×2 refine: renoise sigma0 follows the detail control (None → baked).
                    hires_stage_json(
                        x2,
                        final_stage_sigmas(req.upscale_detail_strength),
                        "euler_cfg_pp",
                        1.0,
                        3,
                    ),
                ]),
                base_width,
                base_height,
            }
        }
        LtxQualityProfile::Custom => {
            // Custom: the request's override fields drive the base pass, and its `hires_chain` (an
            // editable stage list) drives the upscale — each stage falling to the contract defaults
            // for any field left unset. Empty list → no upscale chain (renders base only).
            // `mult` = product of the stages' ×-factors (so target-mode base = output / mult); an
            // empty chain falls back to the single-hires ratio.
            let mut mult = 1.0_f64;
            let stages: Vec<Value> = req
                .hires_chain
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|s| {
                    let sigmas = if s.custom_sigmas.is_empty() {
                        LTX_UPSCALE_STAGE_SIGMAS.to_vec()
                    } else {
                        s.custom_sigmas.clone()
                    };
                    let upscaler = s.upscaler.as_deref().unwrap_or(x2);
                    mult *= stage_scale_mult(upscaler);
                    hires_stage_json(
                        upscaler,
                        sigmas,
                        s.sample_method.as_deref().unwrap_or("euler_cfg_pp"),
                        s.cfg.unwrap_or(1.0),
                        s.steps.unwrap_or(3),
                    )
                })
                .collect();
            let (base_width, base_height) = if stages.is_empty() {
                base_dims(req.effective_upscaler_ratio().scale_mult())
            } else {
                base_dims(mult)
            };
            QualityPlan {
                base_sample_method: req
                    .sampling_method
                    .clone()
                    .unwrap_or_else(|| "euler_a_cfg_pp".to_string()),
                base_sigmas: req
                    .base_custom_sigmas
                    .clone()
                    .unwrap_or_else(|| LTX_HIGH_BASE_SIGMAS.to_vec()),
                base_steps: req.steps.unwrap_or(8),
                base_cfg: req.cfg_scale.unwrap_or(1.0),
                negative_prompt: req.negative_prompt.clone(),
                hires_chain: if stages.is_empty() {
                    None
                } else {
                    Some(stages)
                },
                base_width,
                base_height,
            }
        }
    }
}

/// Convert a shot duration (seconds) to a VAE-valid LTX frame count: `8k+1`
/// (the temporal-VAE constraint the engine expects), floored at 9. Used to send
/// per-segment `frames` for Director variable-length shots.
fn frames_from_seconds(seconds: f64, fps: u32) -> u32 {
    let raw = (seconds * fps as f64).round().max(1.0) as i64;
    let k = (((raw - 1) as f64) / 8.0).round().max(1.0) as i64;
    (k * 8 + 1) as u32
}

/// `None` when the shot fits the engine's temporal window; otherwise the message the job fails
/// with. See [`LTX_MAX_RENDER_FRAMES`] for why this is a refusal and not a clamp.
///
/// `render` is the count the DiT actually samples (visible + any continuation overlap) and is the
/// only number the window cares about; `visible` is carried in solely to state the answer in the
/// units the user is holding — a seconds slider, not a frame budget.
pub(crate) fn shot_window_error(
    index: usize,
    render: u32,
    visible: u32,
    fps: u32,
) -> Option<String> {
    if render <= LTX_MAX_RENDER_FRAMES {
        return None;
    }
    let fps = fps.max(1);
    let pad = render.saturating_sub(visible);
    let max_visible = LTX_MAX_RENDER_FRAMES.saturating_sub(pad);
    Some(format!(
        "shot {} is {render} frames ({:.2}s at {fps}fps{}), over the {}-frame VRAM budget \
         measured for this resolution. Longer shots do not fit the card, and past 248 the model \
         also starts tiling the shot instead of rendering it in one pass, so this render was \
         refused rather than risked. Longest this shot can be: {:.2}s ({max_visible} frames). \
         Split it into two shots to go longer.",
        index + 1,
        render as f64 / fps as f64,
        if pad > 0 {
            format!(", including {pad} frames of continuation overlap")
        } else {
            String::new()
        },
        LTX_MAX_RENDER_FRAMES,
        max_visible as f64 / fps as f64,
    ))
}

/// One rendered shot's resolved inputs, kept aligned through the empty-prompt filter (so an
/// uploaded image / v2v clip / keyframes still map to the right rendered segment). Built once in
/// [`compile_ltx_chain`]; replaces the earlier positional tuple so the growing v2v fields stay
/// legible.
/// `Default` is for the unit tests only — every production `SegSpec` is built field-by-field in
/// [`compile_ltx_chain`], and there is deliberately no `..Default::default()` on that path, so a
/// field
/// added later still has to be filled in explicitly there.
#[allow(dead_code)]
#[derive(Default)]
struct SegSpec {
    prompt: String,
    /// Which TAKE this shot is restored from, as the engine bank job id holding it. Only set on a
    /// retake, and only for shots that are restored rather than re-rendered. See
    /// `LtxSegment::bank_job_id` — without it every restored shot comes out of the single chain
    /// `resume_job_id`, i.e. the last roll rather than the selected one.
    bank_job_id: Option<String>,
    /// Optional per-shot DiT variant (an author's pin — the automatic routing sets `loras`
    /// instead). The engine leases this only at the segment boundary.
    model: Option<String>,
    /// Per-shot runtime LoRAs. Empty ⇒ inherit the request's top-level set; non-empty REPLACES
    /// it for this shot. Switched at the same segment boundary as `model`, but far cheaper
    /// (~1s to re-apply adapters vs a whole DiT swap).
    loras: Vec<LtxRuntimeLora>,
    image: LtxImageSource,
    image_bytes: Option<Bytes>,
    keyframes: Vec<LtxKeyframe>,
    seconds: Option<f64>,
    /// Explicit generation mode (`None` = infer t2v/i2v from image/keyframes, legacy).
    mode: Option<LtxSegmentMode>,
    v2v_strength: Option<f64>,
    v2v_source: Option<LtxV2vSource>,
    /// Foreign-clip bytes for a `v2v_source = upload` shot (multipart `seg_v2v_<raw i>`).
    v2v_upload: Option<Bytes>,
    /// Hard cut: render this shot FRESH from its prompt alone (no init image / continuation). Only
    /// meaningful for shot >= 1; the engine already treats image/keyframe/v2v shots as fresh, so
    /// this drives the scene classification (audio offsets + render padding) for a plain t2v cut.
    scene_cut: bool,
    /// Per-shot image-pin hold strength (opener / keyframes / scene image). Emitted as
    /// `segments[i].pin_strength` for EVERY shot that states one, shot 0 included, and applied by
    /// the engine to that window alone (`segment_pin_strengths[]`). `None` → the shot inherits the
    /// chain-global `strength`, which is the request's top-level `pin_strength` or the koblem
    /// default — and only that, never another shot's value.
    pin_strength: Option<f64>,
    /// This shot's own audio clip (multipart `seg_audio_<raw i>`, else the request's inline
    /// `audio_b64`), and what it is for. Overlaid on the chain-level track by the ENGINE — koblem
    /// only transcodes and forwards it, so nothing here has to know where the shot sits.
    audio_bytes: Option<Bytes>,
    audio_kind: LtxShotAudioKind,
    /// Prompt Relay beats for this shot, in VISIBLE clip seconds. Empty = one prompt for the
    /// whole shot, which is the byte-identical engine path (no cross-attention mask at all).
    beats: Vec<LtxBeat>,
    /// Per-shot sampling overrides. `None` inherits the chain value.
    seed: Option<i64>,
    steps: Option<u32>,
    cfg: Option<f64>,
    negative_prompt: Option<String>,
    /// The identity references conditioning THIS shot, each already carrying the STABLE
    /// per-character `source_id` the Director allocated at project level.
    ///
    /// This is a per-shot SELECTION, not the project cast: the Director lists every character on
    /// every shot with an all-on default and lets one be switched off, and a character switched off
    /// here is also left out of the cast sent to the LLM for this shot. Empty = an ordinary shot;
    /// the engine sees no `character_refs` for it at all.
    character_refs: Vec<LtxCharacterRef>,
    /// Reference head-frame trim for THIS shot, ALREADY resolved against the project value — the
    /// shot's own setting when it has one, the project's otherwise. `None` only for a request that
    /// predates the field, which resolves to auto downstream.
    ///
    /// Resolved at spec-build time rather than at emit time so exactly one place knows the
    /// precedence rule, the same way `pin_strength` and `model` are handled.
    frame_drop: Option<LtxFrameDrop>,
}

impl SegSpec {
    /// The engine `v2v_mode` for this shot (1 = restyle, 2 = edit), or `None` for a non-v2v shot.
    fn engine_v2v_mode(&self) -> Option<u32> {
        self.mode.and_then(LtxSegmentMode::engine_v2v_mode)
    }
}

/// One identity reference as the ENGINE wants it: `image` (not `image_b64`), the source-phase tag,
/// and the resize mode. Mirrors how a keyframe becomes `{image, frame}` — koblem's wire name is the
/// editor-facing one, the engine's is its own.
///
/// `resize_mode` falls back to the [`LtxIdentityRegime`] of the SELECTED MODEL, not to a constant:
/// a 1536×1024 sheet must stay `native_resolution` or it is squashed into the video bucket and
/// loses the per-panel face detail, while Echo's memory bank encodes its slots at render size and
/// wants `match_target`. Neither is a sane default for the other, so the model decides and a
/// per-reference value still wins.
///
/// `name` is deliberately dropped here: it exists so koblem can LOG which character got which tag,
/// and the engine has no use for it.
/// The adapter file names in scope for a shot, for [`LtxIdentityRegime::for_render`].
///
/// Mirrors the engine's inherit/replace rule exactly: a non-empty per-segment set REPLACES the
/// request's top-level one, and empty or absent inherits it. Resolving the regime against the wrong
/// one of those is silent — the render succeeds with a reference sized and tagged for the other
/// mechanism — which is the whole reason this is a named helper rather than a `.or()` at each site.
fn effective_lora_paths<'a>(
    segment: Option<&'a [LtxRuntimeLora]>,
    chain: &'a [LtxRuntimeLora],
) -> impl Iterator<Item = &'a str> {
    let set = match segment {
        Some(s) if !s.is_empty() => s,
        _ => chain,
    };
    set.iter().map(|l| l.path.as_str())
}

fn engine_character_ref(r: &LtxCharacterRef, regime: LtxIdentityRegime) -> Value {
    let mut o = json!({ "image": r.image_b64 });
    if let Some(sid) = r.source_id {
        o["source_id"] = json!(sid);
    }
    // Always stated — the difference between the modes is invisible in a successful render, so it
    // is not a thing to leave to an engine default drifting.
    o["resize_mode"] = json!(
        match r.resize_mode.unwrap_or_else(|| regime.resize_mode()) {
            LtxRefResizeMode::NativeResolution => "native_resolution",
            LtxRefResizeMode::MatchTarget => "match_target",
        }
    );
    o
}

/// Collapse per-shot identity references into the ONE chain-level array the engine takes, each
/// entry paired with the segment indices that asked for it (its `segments` scope).
///
/// Union rather than per-segment arrays because the image BYTES live here: a 1536×1024 sheet
/// repeated into twenty segment objects is tens of megabytes of base64 in a single POST, whereas one
/// entry plus a list of indices is a few bytes per shot.
///
/// Deduplication is by `(source_id, image)` precisely BECAUSE the tag is allocated once per
/// character at project level: the same character on eight shots is one entry with eight indices,
/// and two entries can only ever be two different characters. Indices are the RENDERED (filtered)
/// segment index — the same space as `segments[]` in the request and `partials[].segment_index`.
fn chain_character_refs(seg_specs: &[SegSpec]) -> Vec<(&LtxCharacterRef, Vec<u32>)> {
    let mut out: Vec<(&LtxCharacterRef, Vec<u32>)> = Vec::new();
    for (i, spec) in seg_specs.iter().enumerate() {
        for r in &spec.character_refs {
            match out
                .iter_mut()
                .find(|(e, _)| e.source_id == r.source_id && e.image_b64 == r.image_b64)
            {
                Some((_, segments)) => segments.push(i as u32),
                None => out.push((r, vec![i as u32])),
            }
        }
    }
    out
}

/// A v2v shot's resolved source, as it will be emitted into the per-segment chain object.
enum V2vResolved {
    /// `v2v_source_job_id` + `v2v_source_segment` — a banked diffusion latent (a clip we rendered),
    /// referenced by the engine's own job id. The engine resolves it to `seg_<segment>.bin` within
    /// its artifact roots; no filesystem path crosses the wire and koblem needs no fs access.
    Latent { job_id: String, segment: u32 },
    /// `control_frames` — per-frame base64 PNGs (a foreign / uploaded clip, pixel-encode path).
    Control(Vec<Value>),
}

/// Decode a foreign v2v source clip (uploaded bytes) into `control_frames` at `w×h` (the render's
/// resolution — the engine appends them as the guide/restyle source). Writes to a fresh temp dir,
/// ffmpeg-extracts every frame, and hands each one to `uploads` as a binary part, returning the
/// `part:<name>` markers in frame order (§5/B3 — a frame is ~194 KiB, and there are dozens).
async fn decode_clip_to_control_frames(
    bytes: &Bytes,
    w: u32,
    h: u32,
    uploads: &mut PartUploads,
) -> Result<Vec<Value>, String> {
    let work = std::env::temp_dir().join(format!("ltx-v2v-{}", uuid::Uuid::new_v4()));
    let frames_dir = work.join("frames");
    tokio::fs::create_dir_all(&frames_dir)
        .await
        .map_err(|e| format!("mkdir: {e}"))?;
    let src = work.join("source");
    tokio::fs::write(&src, bytes)
        .await
        .map_err(|e| format!("write source: {e}"))?;
    let n = extract_frames(&src, &frames_dir, w, h)
        .await
        .map_err(|e| format!("extract frames: {e}"))?;
    if n == 0 {
        let _ = tokio::fs::remove_dir_all(&work).await;
        return Err("source clip yielded no frames".to_string());
    }
    let mut frames = Vec::with_capacity(n as usize);
    for f in 1..=n {
        let b = tokio::fs::read(frame_path(&frames_dir, f))
            .await
            .map_err(|e| format!("read frame {f}: {e}"))?;
        frames.push(Value::String(uploads.add(Bytes::from(b))));
    }
    let _ = tokio::fs::remove_dir_all(&work).await;
    Ok(frames)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/ltx-video/generate", post(generate))
        // Lip-dub / relip: attach a drive-audio track to any source video → re-sync mouths.
        // Backs both the LTX-page lipdub tab and the standalone "dub any video" feature.
        .route("/api/v1/ltx-video/relip", post(relip))
        .route("/api/v1/ltx-video/script-from-llm", post(script_from_llm))
        .route("/api/v1/ltx-video/prompt-from-llm", post(prompt_from_llm))
        .route("/api/v1/ltx-video/props", get(props))
        // The engine's latent bank: which segments are still continuable, and what the store costs.
        // A byte-bounded sweep drops the oldest, so this is a live question about its disk.
        .route("/api/v1/ltx-video/bank", get(bank))
        .route("/api/v1/ltx-video/cancel", post(cancel_current))
        // Async job model: submit returns 202 + {job_id}; the browser polls status and
        // re-fetches the finished clip, surviving disconnect/navigation/reconnect.
        .route("/api/v1/ltx-video/jobs", get(list_jobs))
        .route("/api/v1/ltx-video/jobs/{id}", get(job_status))
        .route("/api/v1/ltx-video/jobs/{id}/media", get(job_media))
        // Progressive per-segment delivery: each finished shot, served the instant the engine
        // emits it (before the stitched clip), so the Director can preview shots live.
        .route("/api/v1/ltx-video/jobs/{id}/segments/{n}", get(job_segment))
        .layer(DefaultBodyLimit::max(LTX_BODY_LIMIT))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

/// `GET /api/v1/ltx-video/props` — UI bootstrap (health + defaults).
///
/// There is one LTX engine, so an `?engine=` the caller sends selects nothing and is ignored. The
/// LoRA list is a live scan of the ENGINE's own adapter directory rather than a compiled-in copy:
/// `contract.py` validates every name against that same listing, so a name from anywhere else is a
/// hard rejection at submit.
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let client = &state.ltx25;
    let healthy = client.is_configured() && client.healthy().await;
    let last_render_sec = client
        .health()
        .await
        .as_ref()
        .and_then(|h| h.get("last_render_sec"))
        .and_then(|v| v.as_f64());
    let p = LtxVideoProps {
        healthy,
        default_segment_frames: DEFAULT_SEGMENT_FRAMES,
        default_cont_latent_frames: 2,
        default_steps: 8,
        // 24, not the 48 this advertised for years: every UI that renders LTX hardcodes 24 and
        // prod renders at 24, so 48 made the advertised (frames, fps) pair describe a shot half
        // the length of the one it actually produces.
        default_fps: 24,
        default_resolution: "1280x704".to_string(),
        last_render_sec,
        // LTX-2.5 has exactly one DiT, and an empty list is what makes the picker disappear rather
        // than offer a checkpoint that does not exist.
        variants: Vec::new(),
        loras: resolve_offered_loras(&state).await,
        // Asked only of an engine that can answer — the probe costs a round trip and a down engine
        // has nothing to say about its bank directory.
        bank_enabled: if healthy {
            client.bank_enabled().await
        } else {
            None
        },
        // The same shape for the second deployment fact: whether the Prompt Relay custom node is
        // mounted. Two round trips rather than one shared `capabilities()` read, deliberately —
        // this is a once-per-page bootstrap against an endpoint the supervisor answers with the
        // worker unloaded, and `bank_enabled` also populates the server-side `bank_live` cache as
        // it goes, which a shared read would have to reproduce.
        beats_enabled: if healthy {
            client.beats_enabled().await
        } else {
            None
        },
    };
    Json(p).into_response()
}

/// `GET /api/v1/ltx-video/bank` — the engine's latent bank, newest first.
///
/// A thin pass-through of `GET /ltx/v1/bank`: what is banked, what it cost, and what each entry can
/// be continued into. The listing exists because a key is written when a segment COMPLETES and
/// dropped by a byte-bounded sweep — so "can I still resume that render?" is a question about the
/// engine's disk right now, which no koblem-side record can answer.
async fn bank(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let client = &state.ltx25;
    if !client.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "ltx25 service not configured",
        );
    }
    match client.bank().await {
        Ok(listing) => Json(listing).into_response(),
        // 404 travels as a 404: a build with no bank directory has no such route, and the caller
        // deserves to be told rather than shown an empty bank.
        Err(crate::ltx_video::Error::Upstream(status, body)) => err(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            &body,
        ),
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

/// What the LoRA picker offers, and at what strengths.
///
/// THE ENGINE IS THE SOURCE, and there is nothing else. `GET /sdapi/v1/loras` is a live scan of its
/// `--lora-model-dir` annotated from that directory's `loras.json`, so it cannot be out of step
/// with the files that actually exist — which a hand-maintained copy demonstrably could be, and the
/// way that failed was a render-time 400 minutes into a committed job. The engine also validates
/// every submitted name against the same listing, so anything offered from elsewhere is a rejection
/// waiting to happen.
///
/// An empty list is therefore the honest answer whenever the engine cannot be asked: the picker
/// disappears rather than offering names nothing can resolve.
async fn resolve_offered_loras(state: &AppState) -> Vec<LtxLoraInfo> {
    let client = &state.ltx25;
    // No LTX service at all — the whole tab is off, so asking is pointless and warning is noise.
    if !client.is_configured() {
        return Vec::new();
    }
    let listed = match client.loras().await {
        Ok(list) => list,
        Err(e) => {
            warn!(error = %e, "ltx: could not read the engine's LoRA listing; offering none");
            return Vec::new();
        }
    };
    // Drop the co-tenant's adapters. `h3` and `ltx25` are two plugins on ONE ComfyUI, so 2.5's
    // listing is a scan of a directory it SHARES with H3 and includes H3's LoRAs; the plugin flags
    // them `for_this_model: false` rather than hiding them, and this is where that flag is spent.
    //
    // 🔴 Filtered, not greyed out: attaching one is not an error anywhere downstream. Its keys miss
    // the LTX DiT completely, so comfy applies zero of them and renders the unmodified model at
    // full price — a render that looks like it honoured the request. (The plugin also refuses one
    // at submit now, so this is the picker being correct rather than the only guard.)
    let (offered, foreign): (Vec<_>, Vec<_>) = listed.into_iter().partition(|l| l.for_this_model);
    if !foreign.is_empty() {
        debug!(
            dropped = %foreign.iter().map(|l| l.path.as_str()).collect::<Vec<_>>().join(", "),
            offered = offered.len(),
            "ltx: hid another plugin's adapters from the LoRA picker"
        );
    }
    offered
}

/// `POST /api/v1/ltx-video/cancel` — stop the Director's live render NOW, or drop the next one
/// queued.
///
/// 🔴 Through the DISPATCHER, never straight at the engine. The dispatcher owns the run, so it is
/// the only thing that can stop it AND file the terminal state; reaching past it leaves a row
/// `running` with nothing running.
///
/// All three kinds on this engine, because the editor has ONE stop button and the caller must not
/// have to know which one its project is on. Relip is among them now that it is a queue kind: the
/// scoped fallback that used to reach `cancel_in_flight` past the dispatcher is gone with the
/// acquire it existed for, and leaving it would be a second thing cancelling one engine.
async fn cancel_current(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(row) = engine_cancel::current(&state, |j| {
        matches!(j.kind, JobKind::Ltx25 | JobKind::H3 | JobKind::LtxRelip)
    })
    .await
    else {
        return err(StatusCode::NOT_FOUND, "no LTX render is running or waiting");
    };
    engine_cancel::cancel_row(&state, &row).await
}

/// Which engine's prompt conventions a drafting request wants, plus the extra context only H3 has.
///
/// A `#[serde(flatten)]` envelope rather than new fields on the shared wire types, deliberately:
/// `LtxScriptFromLlmRequest` / `LtxPromptFromLlmRequest` live in koblibs and are shared with
/// every consumer, so widening them is a cross-repo change. The
/// engine choice is koblem's own routing concern and nothing outside this service reads it, so it
/// rides here and the shared types stay untouched.
#[derive(Debug, serde::Deserialize)]
struct H3Envelope<T> {
    /// `"ltx"` (default), `"h3"` or `"ltx25"`. Absent = LTX, which is what every existing client
    /// sends.
    ///
    /// ⚠️ `ltx25` writes with the LTX conventions and is NOT a third arm here: LTX-2.5 is a new
    /// checkpoint of the same model family reached over the same wire, so it takes the same
    /// prompts. Only `h3` has its own prompt space (`h3_prompting`).
    #[serde(default)]
    engine: Option<String>,
    /// H3 only: the references for this GENERATION, in the order the runner will add them.
    ///
    /// Order matters more than it looks: the runner assigns `<Picture i>` / `<Video k>` /
    /// `<Audio j>` ordinals per kind in pipeline add-order (images, then videos with their enabled
    /// soundtrack numbered first, then standalone audio), and the prompt addresses a reference by
    /// the exact label the conditioner emitted. A reordered list addresses the wrong asset and
    /// still renders something plausible. Scope is the generation, not the shot — one WanGP
    /// `generate` call builds one reference presentation for all of its shots.
    #[serde(default)]
    h3_references: Vec<crate::routes::h3_prompting::H3RefBrief>,
    /// H3 only: does this shot pin a CLOSING frame as well as an opener (`fl2va`'s `end_image`)?
    /// There is no LTX equivalent — LTX interpolates through mid-shot keyframes instead.
    #[serde(default)]
    h3_end_image: bool,
    /// H3 only: the duration of the whole GENERATION this shot belongs to, already snapped to the
    /// engine's frame grid.
    ///
    /// 🔴 Not a duplicate of the request's own `seconds`, and both numbers are correct for their own
    /// job. `seconds` is THIS SHOT's length — the editor's slider, what the shot occupies inside the
    /// pack, what the listing shows — and it stays exactly as it was. The prompt's arithmetic is the
    /// GENERATION's: every `MM:SS.mmm` cut time in a body is absolute from the start of the
    /// generation and the alignment line's `S.SS` is the generation's end (`GUIDE-base:86`), so a
    /// writer handed 5 seconds for a 15-second, 3-shot generation places all of its cuts inside the
    /// first third and the rest of the pack has nowhere to go.
    ///
    /// Absent ⇒ fall back to `seconds`. An older client sends nothing, and a caller that does no
    /// packing has a generation of exactly one shot, where the two numbers are the same number.
    #[serde(default)]
    h3_generation_seconds: Option<f64>,
    /// H3 only: how many `[Shot N]` bodies that generation holds.
    ///
    /// 🔴 The writer must be TOLD this rather than left to infer it. The reply's `shots[]` is mapped
    /// back onto the packed shots by POSITION, so a generation that returns two bodies for three
    /// shots does not lose the third quietly — it slides body 2 onto shot 2's neighbour and writes
    /// over the wrong one. Absent ⇒ the writer chooses its own cuts, which is right for a caller
    /// with no pack and wrong for one with a pack.
    #[serde(default)]
    h3_shot_count: Option<u32>,
    /// H3 only: 0-based index, within that generation, of the shot the user is editing.
    ///
    /// 0-based because it indexes the pack; the writer reads the 1-based `[Shot N]` tag, and
    /// `h3_prompting::cut_plan_facts` does the one conversion. Its whole purpose is to be stated
    /// ALONGSIDE the shot count: which shot the change is about, and that every other body still
    /// comes back in order.
    #[serde(default)]
    h3_shot_focus: Option<u32>,
    /// H3 only: the audio intent the planner heard for this shot, to be expanded into the prompt's
    /// audio block rather than re-invented.
    #[serde(default)]
    h3_audio: Option<String>,
    /// H3 only: the shot's aspect, as a phrase for the prompt's spec line ("16:9 landscape").
    #[serde(default)]
    aspect: Option<String>,
    /// H3 only: does anything appear on screen as text? Gates the one-line on-screen-text rule
    /// (visible copy in `"double quotes"`, verbatim, untranslated) — a shot with none must not be
    /// invited to add any.
    #[serde(default)]
    on_screen_text: bool,
    /// H3 only: where the song's voice falls, in each generation's OWN LOCAL seconds.
    ///
    /// 🔴 One entry per generation IN RENDER ORDER on the batch path (`script-from-llm`), and
    /// exactly one entry — describing this shot's own generation — on the single path
    /// (`prompt-from-llm`), where anything past the first is ignored. See
    /// `h3_prompting::H3VocalLane` for why an empty `spans` is a measurement and an absent lane is
    /// not, and for the local-clock rule the caller has to convert onto.
    ///
    /// Absent ⇒ the writer keeps the `speech_in_shot` boolean it has always had. One field for both
    /// routes because they share this envelope, and because giving the two shapes different names
    /// is how a client ends up sending the wrong one to the wrong route.
    #[serde(default)]
    h3_vocal_lanes: Vec<crate::routes::h3_prompting::H3VocalLane>,
    /// H3 only, OPT-IN: the project song's asset hash, so the writer can HEAR this generation's
    /// slice of it. See [`song_slice_for_writer`].
    ///
    /// 🔴 **The BARE 64-hex hash, deliberately not an `asset:<hash>` ref.** `AssetJson` walks the
    /// whole body and rewrites every `asset:` string into the base64 of its bytes, which for an 8 MB
    /// song means an 11 MB `String` allocated inside a request that only wants ~14 s out of the
    /// middle of it. Dropping the prefix keeps the field invisible to that walk — the same
    /// exemption `stage_refs` gives the delivery soundtrack, and for the same reason: this is not a
    /// reference the model receives, it is a file koblem reads.
    #[serde(default)]
    h3_song_asset: Option<String>,
    /// H3 only: where this generation's slice starts IN THE SONG, seconds.
    ///
    /// 🔴 `H3AudioSlice::song_offset_seconds`, never `delivered_at_seconds` — the two are one trim
    /// frame apart and koblem's own `offsets=[…]` log line prints the second one. The caller takes
    /// it from `h3_plan_soundtrack` / `planSoundtrack` rather than re-deriving it; two hand-rolled
    /// copies of that arithmetic have silently drifted before.
    #[serde(default)]
    h3_song_offset_seconds: Option<f64>,
    /// H3 only: how many seconds of song this generation is conditioned on.
    #[serde(default)]
    h3_song_slice_seconds: Option<f64>,
    /// The author's brief for the WHOLE video, verbatim — the auto-direct brief.
    ///
    /// 🔴 The per-render writer used to be handed the planner's one-liner and nothing of the brief,
    /// so whatever the planner dropped was gone for good. Named `project_brief`, not `brief`,
    /// because this envelope flattens over `LtxScriptFromLlmRequest`, which owns a `brief` of its
    /// own, and serde hands a flattened field to the OUTER struct first.
    #[serde(default)]
    project_brief: Option<String>,
    #[serde(flatten)]
    inner: T,
}

impl<T> H3Envelope<T> {
    fn is_h3(&self) -> bool {
        crate::routes::ltx_director::is_h3(self.engine.as_deref())
    }
}

/// `POST /api/v1/ltx-video/script-from-llm` — expand a brief into N director prompts.
///
/// `AssetJson`: the opening frame and any story references arrive as `asset:<hash>` refs from a
/// saved Director document.
async fn script_from_llm(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(mut body): AssetJson<H3Envelope<LtxScriptFromLlmRequest>>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let h3 = body.is_h3();
    let aspect = body.aspect.clone();
    let h3_references = body.h3_references.clone();
    let h3_vocal_lanes = std::mem::take(&mut body.h3_vocal_lanes);
    let req = body.inner;
    let n = req.n_segments.clamp(1, 24);
    let secs = req.seconds_per_segment.unwrap_or(4.0).max(1.0);
    // The length the unit will ACTUALLY render at. H3's grid is coarse (17 frames ≈ 0.71 s) and the
    // prompt carries its own `MM:SS.mmm` cut times, so a writer given the authored number places
    // cuts past the end of the generation.
    let secs = if h3 {
        crate::routes::ltx_director::h3_snapped_seconds(secs)
    } else {
        secs
    };
    let image = req
        .image
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let brief = req.brief.trim();
    if brief.is_empty() && image.is_none() {
        return err(StatusCode::BAD_REQUEST, "brief or image is required");
    }

    // Detailed LTX-2.3 prompting (see routes/ltx_prompting.rs): the i2v variant reads the attached
    // opening frame and continues from it; t2v defines each scene from scratch. Output is a strict
    // JSON array so the multi-sentence paragraphs survive parsing.
    let story = req.story.clone().unwrap_or_default();
    // Two engines, two sets of conventions, one branch. H3's live entirely in `h3_prompting`; see
    // that module's header for why LTX's could not be parameterised into covering both.
    let mut system = if h3 {
        crate::routes::h3_prompting::script_system_prompt(
            n,
            Some(secs),
            aspect.as_deref(),
            &h3_references,
            &req.cast,
            // 🔴 The one writer that decides the standing `<Audio N>` sentence for EVERY generation
            // in a single turn, and until 2026-08-14 the one writer never told whether anyone sings.
            &h3_vocal_lanes,
        )
    } else {
        ltx_prompting::script_system_prompt(
            n,
            Some(secs),
            req.audio_supplied,
            image.is_some(),
            &req.cast,
        )
    };
    if !story.is_empty() {
        system = format!("{} {system}", ltx_prompting::story_context_block(&story));
        if ltx_prompting::story_has_images(&story) {
            system.push_str(
                " The attached reference images define the established characters/look — MATCH \
                 them; they are canon.",
            );
        }
    }

    // 🔴 The H3 arm used to get the LTX sentence — "break this into N shots" — under a system
    // prompt that says N GENERATIONS, each holding several shots. Told two different units, the
    // model picks one, and it was usually the wrong one.
    let mut user = if h3 {
        format!(
            "Write the {n} generations in order, one complete prompt block each. Each runs about \
             {secs:.0}s unless the brief below gives its own length."
        )
    } else {
        format!(
            "Break this into {n} shots that tell the story in order. Each shot lasts about {secs:.0}s."
        )
    };
    if !brief.is_empty() {
        user.push_str(&format!("\nBrief: {brief}"));
    } else {
        user.push_str(
            "\nThere is no written brief — base the shots entirely on the attached opening frame.",
        );
    }

    // i2v: send the frame as an image_url content part, exactly as routes/images.rs feeds the
    // VL model. t2v with no canon images sends plain text. Story reference images (canon
    // characters/mood) ride along after the opening frame.
    let story_images = ltx_prompting::story_context_images(&story);
    let user_content = if image.is_none() && story_images.is_empty() {
        json!(user)
    } else {
        let mut parts: Vec<Value> = vec![json!({"type": "text", "text": user})];
        if let Some(b64) = image {
            parts.push(json!({"type": "image_url", "image_url": {"url": as_data_uri(b64)}}));
        }
        parts.extend(story_images);
        Value::Array(parts)
    };
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_content},
        ],
        "max_tokens": (n * 320).max(1024),
        "temperature": 0.7,
    });
    crate::sampling::apply_sampling(&state.db, "sampling_ltx_video", &mut body).await;
    // 🔴 An H3 entry is a whole block, ~1,800 tokens at the cap. The old 320 a unit was a fifth of
    // one, so every block after the first came back cut off — and a cut-off block still parses,
    // stores and renders without whatever fell off its end. The Director no longer writes H3 here
    // (it drafts one render per call); this keeps the other callers whole.
    let per_unit = if h3 {
        crate::routes::h3_prompting::H3_BLOCK_ANSWER_TOKENS
    } else {
        320
    };
    crate::director_think::enable(&mut body, &state.llama, (n * per_unit).max(1024));
    let text = match state
        .llama
        .chat_completion_json_logged(&body, &state.db, "ltx_prompt_from_llm")
        .await
    {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("llm: {e}")),
    };
    // H3's unit is a GENERATION and its reply unit is now the FINISHED SCHEMA BLOCK — the model
    // writes the artifact and `web/src/lib/director/h3-block.ts` reads it, instead of the model
    // filling in our editor's JSON shape. `parse_generations` splits the blocks on
    // `h3_prompting::GENERATION_SEPARATOR` and still accepts the retired JSON envelopes, so a
    // rollout has both live. A model that ignored the contract entirely and sent prose still comes
    // back through `parse_shots`, so the surface never returns nothing.
    let segments = if h3 {
        let gens = crate::routes::h3_prompting::parse_generations(&text, n as usize);
        if gens.is_empty() {
            ltx_prompting::parse_shots(&text, n as usize)
        } else {
            gens
        }
    } else {
        ltx_prompting::parse_shots(&text, n as usize)
    };
    // ⚠️ The typed draft carrier, kept alive but **normally empty now**. A block does not parse as
    // an `H3PromptDraft`, so the all-or-nothing gate below leaves this vector empty and the client
    // reads `segments[i]` as a block instead (`h3-llm.ts::h3DraftFrom`, carrier 4). It stays
    // because a saved project, an older koblem and a model that still answers in the retired JSON
    // shape all put something here, and dropping it would turn a graceful degradation into lost
    // work.
    //
    // ALL-OR-NOTHING: populated only when every generation parsed, so a reader never has to work
    // out whether entry 3 is missing or merely index 3 of a shorter list.
    let h3_generations: Vec<koblem_client::types::H3PromptDraft> = if h3 {
        let parsed: Vec<_> = segments
            .iter()
            .filter_map(|s| serde_json::from_str::<koblem_client::types::H3PromptDraft>(s).ok())
            .filter(|d: &koblem_client::types::H3PromptDraft| !d.is_empty())
            .collect();
        if parsed.len() == segments.len() {
            parsed
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    Json(LtxScriptFromLlmResponse {
        segments,
        h3_generations,
    })
    .into_response()
}

/// The USER turn of a single draft / rework / rewrite, per engine.
///
/// 🔴 Every string here exists twice because the two engines are being asked for two different
/// ARTEFACTS, not because the wording drifted. On LTX the unit is one shot and the answer is a
/// finished paragraph of prose. On H3 the unit is a GENERATION — one prompt block holding 1-N
/// `[Shot N]` cuts, written in the schema MiniMax's own guide describes.
///
/// So the H3 arm names neither LTX nor "a shot prompt", asks for the generation's block, and states
/// NO reply shape: `h3_prompting::block_contract` already stated it exactly, and a second, vaguer
/// statement of it in the user turn is how a writer ends up posting prose where a block belongs.
/// Until this split existed the H3 path asked for "a full LTX shot prompt" underneath a system
/// prompt of pure H3 schema craft — the two together describe an artefact that does not exist, and
/// the model resolves that by picking one.
///
/// `gist_only` is deliberately NOT here: summarising a shot in one plain sentence is the one
/// genuinely engine-neutral task on this route, and it stays inline at its call site.
fn draft_user_turn(
    h3: bool,
    instruction: Option<&str>,
    rewrite: Option<&str>,
    idea: &str,
) -> String {
    if let Some(instr) = instruction {
        // Rework: apply a directed change. With an existing prompt, revise in place; otherwise write
        // fresh toward the instruction. Either way, only touch what the change calls for.
        return match rewrite.or((!idea.is_empty()).then_some(idea)) {
            Some(base) if h3 => format!(
                "Revise this generation to apply the requested change. Keep everything the change \
                 does NOT touch — the subjects' identity and look, the setting, the shots and their \
                 cut times, and continuity with the neighbouring generations. Change only what the \
                 instruction asks.\n\nChange to apply: {instr}\n\nThe generation as it stands:\n\
                 {base}"
            ),
            Some(base) => format!(
                "Revise this LTX shot prompt to apply the requested change. Keep everything the \
                 change does NOT touch — the subject's identity and look, the setting, and \
                 continuity with the neighbouring shots. Change only what the instruction asks.\n\n\
                 Change to apply: {instr}\n\nExisting shot prompt:\n{base}"
            ),
            None if h3 => {
                format!(
                    "Write the prompt block for this generation, following this direction: {instr}"
                )
            }
            None => format!(
                "Write a full LTX shot prompt for this shot, following this direction: {instr}"
            ),
        };
    }
    if let Some(rw) = rewrite {
        return if h3 {
            format!(
                "Rewrite this generation. Keep the same scene, subjects, action and intent, and the \
                 same shots in the same order, but vary the wording, framing and detail for a fresh \
                 take:\n{rw}"
            )
        } else {
            format!(
                "Rewrite this existing LTX shot prompt. Keep the same scene, subject, action and \
                 intent, but vary the wording, framing and detail for a fresh take:\n{rw}"
            )
        };
    }
    match (h3, idea.is_empty()) {
        (true, true) => String::from("Write the prompt block for this generation."),
        (true, false) => format!("Turn this idea into this generation's prompt block:\n{idea}"),
        (false, true) => String::from("Write a full LTX shot prompt for this shot."),
        (false, false) => format!("Turn this idea into a full LTX shot prompt:\n{idea}"),
    }
}

/// `POST /api/v1/ltx-video/prompt-from-llm` — write ONE detailed LTX shot prompt for a single
/// prompt field (the per-shot "draft" buttons in the director / storyboard editors). Image-aware
/// (a scene image / keyframes ride as VL `image_url` parts with positional labels) and mode-aware
/// (t2v defines the scene; i2v describes only what happens next).
///
/// `AssetJson`: every image here comes off a shot in the editor, so it arrives as an
/// `asset:<hash>` ref. With a plain `Json` the ref was pasted into the `data:` URI verbatim and
/// llama-server answered `Failed to load image or audio file`.
async fn prompt_from_llm(
    State(state): State<AppState>,
    headers: HeaderMap,
    AssetJson(mut body): AssetJson<H3Envelope<LtxPromptFromLlmRequest>>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let h3 = body.is_h3();
    let h3_references = body.h3_references.clone();
    let h3_end_image = body.h3_end_image;
    let h3_audio = body.h3_audio.clone();
    let h3_generation_seconds = body.h3_generation_seconds;
    let h3_shot_count = body.h3_shot_count;
    let h3_shot_focus = body.h3_shot_focus;
    let aspect = body.aspect.clone();
    let on_screen_text = body.on_screen_text;
    // One generation, so one lane. Anything past the first is a caller sending the batch shape to
    // the single route; ignored rather than 400'd, because the only harm is unused information.
    let h3_vocal_lanes = std::mem::take(&mut body.h3_vocal_lanes);
    let h3_song_asset = body.h3_song_asset.clone();
    let h3_song_offset_seconds = body.h3_song_offset_seconds;
    let h3_song_slice_seconds = body.h3_song_slice_seconds;
    let project_brief = body
        .project_brief
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string);
    let mut req = body.inner;
    // The length the shot will ACTUALLY render at — H3's prompts carry their own timings, so the
    // writer must be given the snapped number and not the authored one.
    if h3 && let Some(s) = req.seconds.filter(|s| *s > 0.0) {
        req.seconds = Some(crate::routes::ltx_director::h3_snapped_seconds(s));
    }
    let has_images = req.images.iter().any(|im| !im.image_b64.trim().is_empty());
    let has_refs = req
        .references
        .iter()
        .any(|im| !im.image_b64.trim().is_empty());
    let mode = ltx_prompting::PromptMode::resolve(req.mode, has_images, req.continuation);
    let idea = req.idea.trim();
    let prev = req
        .prev_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let next = req
        .next_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let story = req.story.clone().unwrap_or_default();
    let rewrite = req
        .rewrite
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let want_gist = req.want_gist || req.gist_only;
    // A drafted beat is a suggestion for the editor, so never offer one for a shot that already
    // carries hand-placed timing, and never on the summarise-only path. Never on H3 either: Prompt
    // Relay is an LTX conditioning layout and the H3 route refuses `beats` outright, so a proposed
    // beat is a suggestion the user can accept and then not render.
    let propose_beats = !h3 && req.want_beats && !req.gist_only && req.beats.is_empty();
    let gist_only = req.gist_only;
    let instruction = req
        .instruction
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // Prior shots (earliest → latest) that set the story so far; trimmed + numbered for the prompt.
    let prior: Vec<&str> = req
        .prior_shots
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if idea.is_empty()
        && rewrite.is_none()
        && instruction.is_none()
        && !has_images
        && !has_refs
        && prev.is_none()
        && next.is_none()
        && prior.is_empty()
        && story.is_empty()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "provide an idea, an image, or neighbouring shots to draft from",
        );
    }

    // gist_only is a pure summarisation task — skip the heavy shot-writing craft rules. It is also
    // the one path here that is genuinely engine-neutral: a plain-language sentence about what
    // happens in a shot does not depend on which model renders it.
    let mut system = if gist_only {
        String::from(
            "You summarise film shots for a director. Given a shot's prompt, reply with ONE short \
             plain-language sentence describing what happens in the scene — no camera moves, lens, \
             lighting or technical jargon, just the action and subject a viewer would notice.",
        )
    } else if h3 {
        use crate::routes::h3_prompting as h3p;
        // The checkpoint, the alignment-line variant and the `summary:` task-type prefix are all
        // DERIVED from the conditioning actually present, by the decision table in `h3_prompting`
        // — exactly as the runner derives the checkpoint. An explicit value would only be checked
        // against this and 400 on a mismatch.
        let inputs = h3p::H3Inputs::from_refs(&h3_references, has_images, h3_end_image);
        // 🔴 The GENERATION's duration, never `req.seconds` when the caller knows better — see
        // `H3Envelope::h3_generation_seconds`. Snapped again on the way through because the fallback
        // already was (line ~1205): one field, one grid, whichever branch supplied it.
        let generation_seconds = h3_generation_seconds
            .filter(|s| *s > 0.0)
            .map(crate::routes::ltx_director::h3_snapped_seconds)
            .or(req.seconds);
        h3p::shot_system_prompt(h3p::H3GenerationPrompt {
            seconds: generation_seconds,
            shot_count: h3_shot_count,
            shot_focus: h3_shot_focus,
            aspect: aspect.as_deref(),
            refs: &h3_references,
            cast: &req.cast,
            speech_in_shot: req.speech_in_shot,
            // Supersedes `speech_in_shot` when present — both are read off the same transcript and
            // the boolean's "somebody sings" is the standing permission the lane exists to bound.
            vocal_lane: h3_vocal_lanes.first(),
            on_screen_text,
            want_gist,
            ..h3p::H3GenerationPrompt::new(inputs)
        })
    } else {
        ltx_prompting::shot_system_prompt(ltx_prompting::ShotPrompt {
            seconds: req.seconds,
            audio_supplied: req.audio_supplied,
            has_refs: has_images,
            closes_scene: req.closes_scene,
            beats: &req.beats,
            cast: &req.cast,
            speech_in_shot: req.speech_in_shot,
            ..ltx_prompting::ShotPrompt::new(mode)
        })
    };
    // Prepend the project canon so a lone t2v draft describes the established subject (e.g. "the
    // robot") to spec instead of inventing it.
    if !story.is_empty() {
        system = format!("{} {system}", ltx_prompting::story_context_block(&story));
    }
    let (picture_clause, picture_parts) = if h3 && !gist_only {
        crate::routes::h3_prompting::reference_picture_parts(&h3_references)
    } else {
        (String::new(), Vec::new())
    };
    system.push_str(&picture_clause);
    // When a gist is wanted alongside the prompt, switch the reply contract to JSON so we get both
    // in one call. gist_only asks for just the gist.
    if gist_only {
        system.push_str(
            "\n\nReply as a JSON object only: {\"gist\": \"<one plain-language sentence>\"}.",
        );
    } else if want_gist && !h3 {
        // H3 has already stated its own reply contract — the finished schema block, with the gist
        // on a line after `h3_prompting::GIST_SEPARATOR`. Appending this LTX JSON envelope on top
        // would give the model two contradictory reply shapes, and it would force a multi-line
        // block through JSON string escaping, which is the failure the sentinel exists to avoid.
        system.push_str(
            "\n\nReply as a JSON object only, no prose around it: {\"prompt\": \"<the full shot \
             prompt>\", \"gist\": \"<one plain-language sentence describing what happens, with NO \
             camera/lens/lighting jargon>\"}.",
        );
    }
    // Beat proposal. Only offered when the shot has none — a drafted beat is a suggestion, and
    // overwriting hand-placed timing would be the worst possible behaviour here.
    if propose_beats {
        system.push_str(&ltx_prompting::beat_proposal_clause(req.seconds));
    }

    let mut user = if gist_only {
        let src = rewrite.unwrap_or(idea);
        format!("Summarise this shot in one plain sentence:\n{src}")
    } else {
        draft_user_turn(h3, instruction, rewrite, idea)
    };
    // The planner's audio intent. Passed as DIRECTION rather than folded into the idea, so the
    // writer expands the sound the plan actually heard instead of inventing a new one. On H3 it
    // lands in the generation's two audio sections (ambience/action → `overall_soundscape`,
    // audience-only score → `non_diegetic_music`) plus any diegetic event in a shot body — there
    // is exactly one of each per generation, covering every shot in it.
    if h3
        && !gist_only
        && let Some(a) = h3_audio.as_deref().map(str::trim).filter(|a| !a.is_empty())
    {
        user.push_str(&format!(
            "\n\nThe audio intent planned here — expand it into `overall_soundscape` and \
             `non_diegetic_music`, do not replace it: {a}"
        ));
    }
    if h3
        && !gist_only
        && let Some(b) = project_brief.as_deref()
    {
        user.push_str(&format!(
            "\n\nTHE AUTHOR'S BRIEF for the whole video — this generation is one part of it. \
             Everything the brief asks for that belongs in THIS generation goes in at full \
             strength: dialogue and on-screen text word for word, every gag, prop and reaction. \
             Leave the parts that belong to other generations to them.\n<<<\n{b}\n>>>"
        ));
    }
    if !prior.is_empty() && !gist_only {
        user.push_str("\n\nThe story so far — earlier shots in order:");
        for (i, s) in prior.iter().enumerate() {
            user.push_str(&format!("\n{}. {s}", i + 1));
        }
        user.push_str(
            "\nThis shot continues from them — keep characters, setting and continuity consistent.",
        );
    }
    if let Some(p) = prev {
        user.push_str(&format!(
            "\n\nThe previous shot (immediately before) was: {p}"
        ));
    }
    if let Some(nx) = next {
        user.push_str(&format!("\nThe next shot will be: {nx}"));
    }
    // gist_only is a text-summarisation task — the shot's frames/references don't inform it, so we
    // skip the image clauses AND the images themselves (cheaper, no jargon leakage into the gist).
    let refs: Vec<&LtxPromptImage> = if gist_only {
        Vec::new()
    } else {
        req.references
            .iter()
            .filter(|im| !im.image_b64.trim().is_empty())
            .collect()
    };
    if has_images && !gist_only {
        // 🔴 LTX's keyframe rule, and LTX's ONLY. It says the opposite of what the H3 system prompt
        // riding in the same request says: `GUIDE-base` §3.1 requires the writer to establish
        // style, subjects, composition and scene anchors FROM the pinned image, and the served
        // I2VA prompt is the LONGEST in the corpus at 4,534 characters (`REQ-fl2va:13`). Appending
        // this to an H3 draft put both halves of that contradiction in one request and let the
        // model pick.
        //
        // The H3 arm appends NOTHING here, deliberately. Do not "fix" that by writing an H3 version
        // of this paragraph: MiniMax's own guide is in the system prompt verbatim, it carries the
        // rule PER VARIANT (§3.1 I2VA anchors shot 1's opening, §3.2 FL2VA supplies the motion path
        // between two frames, §3.3 L2VA infers backwards toward a final frame), and a second copy
        // in the user turn would be a variant-blind authority for a rule the model is already
        // reading. This defect is what one rule stated in two places looks like when they drift.
        if !h3 {
            let labels: Vec<(&str, Option<f64>)> = req
                .images
                .iter()
                .filter(|im| !im.image_b64.trim().is_empty())
                .map(|im| (im.role.as_str(), im.seconds))
                .collect();
            user.push_str(&format!(
                "\n\nThese images are fixed frames of this shot, already injected into the video: \
                 {}. Use them ONLY to see where the motion starts and ends — write only the action \
                 and camera that moves the subject between them. Do NOT describe how any frame \
                 looks; its subject, clothing, colours and setting are already fixed by the image.",
                ltx_prompting::label_images(&labels)
            ));
        } else {
            // The H3 arm still has to SAY THE IMAGES ARE THERE, and that is not the same thing as
            // telling the writer what to do with them. Two reasons it cannot simply be silent:
            //
            //   * the clause immediately below opens "The next N attached images…", which is a
            //     POSITIONAL claim. The pinned frames are attached first (see the content-order
            //     comment under this block), so with nothing naming them the reference clause's
            //     "next" has no antecedent and the model reads frame 1 as reference 1.
            //   * the frames a generation pins are the one kind of conditioning that is
            //     frame-exact, and a writer that mistakes one for a mood board writes toward an
            //     image the render is going to reproduce exactly.
            //
            // So this states WHAT each leading image is and stops. Everything about how to write
            // from it — anchor the opening, supply the motion path, infer backwards — is in
            // `GUIDE-base` §3, which is in the system prompt verbatim and knows the variant.
            let labels: Vec<(&str, Option<f64>)> = req
                .images
                .iter()
                .filter(|im| !im.image_b64.trim().is_empty())
                .map(|im| (im.role.as_str(), im.seconds))
                .collect();
            user.push_str(&format!(
                "\n\nAttached first, before anything else: the frame(s) this generation PINS — {}. \
                 These are conditioning the render actually receives, not references.",
                ltx_prompting::label_images(&labels)
            ));
        }
        // ⚠️ NOT gated, and it survives the split for a reason the clause states in its own words:
        // these images are writing-steer references the user likes ("They are NOT frames of this
        // shot and are never rendered"). That is true on both engines — nothing in it describes LTX
        // conditioning, and on H3 a `<Picture N>` that IS conditioning arrives through
        // `h3_references` and is addressed by ordinal from the reference roster instead.
        user.push_str(&ltx_prompting::reference_clause(refs.len()));
        if ltx_prompting::story_has_images(&story) {
            user.push_str(
                "\n\nThe remaining attached images are character/mood references that define the \
                 established look — describe the subject to MATCH them exactly. They are canon, NOT \
                 frames of this shot.",
            );
        }
    } else if !gist_only {
        user.push_str(&ltx_prompting::reference_clause(refs.len()));
        if ltx_prompting::story_has_images(&story) {
            user.push_str(
                "\n\nThe attached images are character/mood references that define the established \
                 look — describe the subject to MATCH them exactly. They are canon, NOT frames of \
                 this shot.",
            );
        }
    }

    // 🔴 OPT-IN, OFF BY DEFAULT (`KOBLEM_H3_WRITER_SONG_AUDIO`). Resolved BEFORE the content array
    // is built because the clause announcing it has to be inside the text part, and `user` is moved
    // into that part on the next line.
    //
    // 🔴 AND the loaded model has to have an audio encoder. Verified soft-fail: the `Err` arm below
    // already drafts without the slice rather than failing the request, and this predicate joins it
    // rather than replacing it — Beefy mode's Qwen3.8 mmproj is VISION-ONLY, so on that server the
    // slice would be attached, silently ignored, and the clause below would tell the writer to
    // "listen to it" and write energy off a sound it never received. That is worse than no audio.
    let want_song_audio = h3 && !gist_only && state.config.h3_writer_song_audio;
    let audio_ok = crate::routes::chat::audio_input_supported(&state);
    if want_song_audio && !audio_ok {
        tracing::warn!(
            "h3 writer: KOBLEM_H3_WRITER_SONG_AUDIO is on but the loaded LLM has no audio encoder \
             (Beefy LLM mode?) — drafting from the vocal lane and the images only"
        );
    }
    let song_audio: Option<String> = if want_song_audio && audio_ok {
        match song_slice_for_writer(
            &state,
            h3_song_asset.as_deref(),
            h3_song_offset_seconds,
            h3_song_slice_seconds,
        )
        .await
        {
            Ok(url) => url,
            // NOT a failed draft. The writer still has the vocal lane, the roster and the brief;
            // losing the audio costs it tone and nothing else, and failing a prompt request over an
            // optional attachment would make an opt-in flag a new way for drafting to break.
            Err(e) => {
                tracing::warn!(error = %e, "h3 writer song slice unavailable; drafting without it");
                None
            }
        }
    } else {
        None
    };
    if song_audio.is_some() {
        user.push_str(
            "\n\nATTACHED LAST, after the images, is the ACTUAL AUDIO this generation is \
             conditioned on — its own slice of the project song, starting at this generation's \
             frame 0. Listen to it and write the energy and where the cuts fall to what you hear. \
             Keep the BODY direction to one plain concrete sentence — moving naturally with the \
             track, never settling into a pose — and put the effort into keeping the CAMERA \
             moving instead: that is what carries the movement, and writing out full choreography \
             reads as overacting. ⚠️ Where the vocal lane above \
             states a span in seconds, THE LANE WINS: those spans were measured off the transcript, \
             and what a singer sounds like is not evidence of when their mouth is open.",
        );
    }

    // Image order must match the positional clauses above: shot frames, then per-shot references,
    // then story-canon images (which the story clause calls "the remaining attached images"), and
    // the song slice — when attached — strictly last.
    let mut content: Vec<Value> = vec![json!({"type": "text", "text": user})];
    if !gist_only {
        for im in req
            .images
            .iter()
            .filter(|im| !im.image_b64.trim().is_empty())
        {
            content.push(json!({
                "type": "image_url",
                "image_url": {"url": as_data_uri(im.image_b64.trim())},
            }));
        }
        for im in &refs {
            content.push(json!({
                "type": "image_url",
                "image_url": {"url": as_data_uri(im.image_b64.trim())},
            }));
        }
        content.extend(ltx_prompting::story_context_images(&story));
        // Labelled one by one, so the positional clauses above still count right: these are
        // character references too, and each carries its own ordinal beside it.
        content.extend(picture_parts);
        // LAST, after every image — `reference_clause` and the story clause above both make
        // POSITIONAL claims about "the next / remaining attached images", and an audio part slipped
        // in among them would break the count those clauses depend on.
        if let Some(url) = song_audio.as_deref() {
            content.push(crate::routes::chat::audio_content_part(url));
        }
    }

    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": content},
        ],
        // Budget has to cover REASONING + the answer. Gemma 4 emits `reasoning_content` before it
        // writes anything, and koblem reads `content` — so a budget that only fits the prose comes
        // back EMPTY with finish_reason "length", not merely truncated. Measured 2026-07-28 on
        // gemma-4-12B-it-qat: a bare one-line idea spent 504 completion tokens (~228 words of
        // reasoning, then a 103-word prompt) — already over the old 512 before any story canon,
        // cast or beat clauses were added. Roomy on purpose; the model still stops on its own.
        "max_tokens": if propose_beats { 2000 } else if want_gist { 1800 } else { 1500 },
        "temperature": if gist_only { 0.3 } else { 0.7 },
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_ltx_video", &mut body).await;
    // A gist is a one-line label; everything else here writes the render's document. An H3 block
    // runs to the 7,000-character serving cap (~1,800 tokens) plus its gist line.
    if !gist_only {
        let answer = if h3 {
            crate::routes::h3_prompting::H3_BLOCK_ANSWER_TOKENS
        } else {
            2000
        };
        crate::director_think::enable(&mut body, &state.llama, answer);
    }
    let text = match state
        .llama
        .chat_completion_json_logged(&body, &state.db, "h3_prompt_from_llm")
        .await
    {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("llm: {e}")),
    };

    // H3 replies with the FINISHED SCHEMA BLOCK, which rides verbatim in the `prompt` string field
    // for `web/src/lib/director/h3-block.ts` to read back into the panels.
    //
    // 🔴 Never run an H3 block through `clean_prompt`. That function drops a short leading line
    // ending in a colon (it is hunting *"Here is the prompt:"*), and the first line of a Ref2VA
    // block is `subject_definitions:` — 20 characters, ends in a colon. It would eat the header and
    // every reference definition under it, silently, leaving a prompt that still renders.
    // `h3_prompting::clean_block` strips a code fence and nothing else.
    //
    // ⚠️ The retired JSON draft is still recognised on the way IN: a model that answers in the old
    // contract shape keeps working, because `extract_json_object` is only reached when the reply
    // actually parses as one. `gist_only` is engine-neutral text summarisation and keeps the prose
    // path on both engines.
    let h3_json = |t: &str| -> Option<koblem_client::types::H3PromptDraft> {
        serde_json::from_str::<koblem_client::types::H3PromptDraft>(
            ltx_prompting::extract_json_object(t),
        )
        .ok()
        .filter(|d| !d.is_empty())
    };
    let clean = |t: &str| -> String {
        if !h3 || gist_only {
            return ltx_prompting::clean_prompt(t);
        }
        if h3_json(t).is_some() {
            return ltx_prompting::extract_json_object(t).to_string();
        }
        crate::routes::h3_prompting::split_gist(t).0
    };
    // …and the typed draft, so a reply that IS the old object crosses the wire as an object rather
    // than as JSON the client has to re-parse out of a string. `None` on LTX and — the normal case
    // now — on an H3 reply that is a block, where `prompt` carries the block itself.
    let h3_draft = |t: &str| -> Option<koblem_client::types::H3PromptDraft> {
        if !h3 || gist_only {
            return None;
        }
        h3_json(t)
    };

    if want_gist {
        // JSON reply contract: {prompt?, gist?}. Fall back gracefully if the model returned prose.
        let parsed: Value =
            serde_json::from_str(ltx_prompting::extract_json_object(&text)).unwrap_or(Value::Null);
        let gist = parsed
            .get("gist")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // 🔴 On H3 the gist rides AFTER the block behind `h3_prompting::GIST_SEPARATOR`, not inside
        // JSON, and this is now the normal path rather than the error path. Asking a local model to
        // escape a multi-line schema block into a JSON string is what the old contract did and what
        // it got wrong; a sentinel line cannot collide with a `name:` header or a `[Shot N]` tag.
        // The JSON reading above still wins when it found one, so a model answering in the retired
        // shape is unaffected.
        let gist = gist.or_else(|| {
            if h3 && !gist_only {
                crate::routes::h3_prompting::split_gist(&text).1
            } else {
                None
            }
        });
        if gist_only {
            // No prompt rewrite — echo an empty prompt (callers read only `gist`). If JSON failed,
            // treat the whole reply as the gist sentence.
            let gist = gist.or_else(|| {
                let t = ltx_prompting::clean_prompt(&text);
                (!t.trim().is_empty()).then_some(t)
            });
            return Json(LtxPromptFromLlmResponse {
                prompt: String::new(),
                h3_draft: None,
                gist,
                beats: Vec::new(),
            })
            .into_response();
        }
        let prompt = parsed
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(ltx_prompting::clean_prompt)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| clean(&text));
        let beats = if propose_beats {
            ltx_prompting::parse_proposed_beats(&parsed, req.seconds)
        } else {
            Vec::new()
        };
        return Json(LtxPromptFromLlmResponse {
            prompt,
            h3_draft: h3_draft(&text),
            gist,
            beats,
        })
        .into_response();
    }

    let prompt = clean(&text);
    Json(LtxPromptFromLlmResponse {
        h3_draft: h3_draft(&text),
        prompt,
        gist: None,
        beats: Vec::new(),
    })
    .into_response()
}

/// The longest span of song the writer is ever handed. A generation cannot exceed
/// `H3_MAX_AUDIO_REF_SECONDS` anyway, so this only ever fires on a caller sending nonsense — but it
/// is the difference between a bad request costing one truncated attachment and it costing the whole
/// track through an audio encoder.
const WRITER_SONG_MAX_SECONDS: f64 = 20.0;

/// THIS generation's slice of the project song, as an `audio/wav` data URL for the prompt writer.
///
/// # Why the slicing is server-side
///
/// The browser holds the song as an `asset:<hash>` and nothing else — the bytes live in koblem's
/// content-addressed store, and the editor streams them for the waveform. Slicing in the browser
/// would mean decoding the whole track through WebAudio and re-encoding a wav in JS on every draft;
/// slicing by sending the WHOLE song up would put ~8 MB on the wire per prompt call. Neither is
/// worth it when `h3_soundtrack::cut_span` — the exact ffmpeg invocation the renderer already uses
/// to cut conditioning slices, seek discipline included — is already here.
///
/// # What it deliberately does NOT do
///
/// It does not re-derive the offset. `song_offset_seconds` comes from `h3_plan_soundtrack` on
/// whichever side computed it, and this function is handed the number. Two hand-rolled copies of
/// that arithmetic have drifted before (`h3-soundtrack.ts`'s header records the incident), and a
/// third one whose only symptom is "the writer heard the wrong 14 seconds" would be undetectable.
///
/// `Ok(None)` means "nothing to attach" — no hash, no offsets, or a caller that asked for zero
/// seconds. `Err` means it was asked for something real and could not produce it, which the caller
/// logs and drafts without.
async fn song_slice_for_writer(
    state: &AppState,
    song_asset: Option<&str>,
    offset_seconds: Option<f64>,
    slice_seconds: Option<f64>,
) -> Result<Option<String>, String> {
    let (Some(hash), Some(offset), Some(seconds)) = (song_asset, offset_seconds, slice_seconds)
    else {
        return Ok(None);
    };
    let hash = hash.trim();
    let seconds = seconds.min(WRITER_SONG_MAX_SECONDS);
    if hash.is_empty() || seconds <= 0.0 {
        return Ok(None);
    }
    // 🔴 The BARE hash, so `AssetJson`'s structural `asset:` walk never saw this field and never
    // inlined 8 MB of base64 into a request that wants 14 seconds of it. `path_for` is also the
    // confinement guard — 64 lowercase hex or nothing, so a crafted value cannot traverse the store.
    let Some(song_path) =
        crate::director_assets::path_for(state.director_assets_dir.as_ref(), hash)
    else {
        return Err(format!("{hash:.12} is not a well-formed asset hash"));
    };
    if !tokio::fs::try_exists(&song_path).await.unwrap_or(false) {
        return Err(format!("the song asset {hash:.12} is not in the store"));
    }

    // A scratch file, removed before returning: this is ffmpeg's output, not an asset. Writing it
    // into the content-addressed store would leave one unaddressable wav per draft in a directory
    // whose whole contract is that everything in it is addressable.
    let scratch =
        std::env::temp_dir().join(format!("koblem-writer-song-{}.wav", uuid::Uuid::now_v7()));
    let cut = crate::routes::h3_soundtrack::cut_span(
        &song_path,
        &scratch,
        offset,
        seconds,
        crate::routes::h3_soundtrack::LISTENING_SPAN_RATE,
        crate::routes::h3_soundtrack::LISTENING_SPAN_CHANNELS,
    )
    .await;
    let bytes = match cut {
        Ok(()) => tokio::fs::read(&scratch)
            .await
            .map_err(|e| format!("reading the writer's song slice: {e}")),
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&scratch).await;
    let bytes = bytes?;
    if bytes.is_empty() {
        return Err("the writer's song slice came out empty".to_string());
    }
    // A `data:` URL, NOT raw base64 — `chat::audio_content_part` is the one place that strips the
    // wrapper, because `input_audio` is parsed with `accept_base64_uri = false` and a surviving
    // prefix is decoded as garbage rather than rejected.
    Ok(Some(format!(
        "data:audio/wav;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    )))
}

/// Wrap a bare base64 image as a data URI for the VL model (passes a `data:` prefix through).
fn as_data_uri(b64: &str) -> String {
    if b64.starts_with("data:") {
        b64.to_string()
    } else {
        format!("data:image/png;base64,{b64}")
    }
}

/// `POST /api/v1/ltx-video/generate` — accept a render and return 202 + `{job_id}` at once.
/// The actual chain (audio orchestration → GPU acquire → server submit/poll → audio replace →
/// persist) runs in a background task that owns the GPU guard, so the browser can disconnect,
/// navigate, or reconnect without dropping the guard or losing the result. Poll
/// `GET …/jobs/{job_id}` for status and re-fetch the clip from `…/jobs/{job_id}/media`.
/// Optional Director-persistence routing on `/generate`, carried as query params (keeps the shared
/// `LtxVideoGenerateRequest` body untouched). When `director_project_id` is present the job is tagged
/// so its output is harvested into that project's owned store on completion; `director_tier` picks
/// the stream (`proxy`|`final`, default `final`) and `director_wipe` replaces that tier's prior set
/// (a full run) vs updating in place (a through-K / retake render).
#[derive(serde::Deserialize)]
struct DirectorTag {
    #[serde(default)]
    director_project_id: Option<String>,
    #[serde(default)]
    director_tier: Option<String>,
    #[serde(default)]
    director_wipe: Option<bool>,
    /// Run the vision critic once this render finishes, honouring the project's
    /// `timeline.critique` settings. Set only by a FULL run: an explicit flag rather than something
    /// inferred here, so a retake — including one the critic itself dispatched — can never chain
    /// back into another review.
    #[serde(default)]
    director_critique: Option<bool>,
    /// Which engine renders this: `"h3"` for MiniMax-H3, anything else (including absent) for
    /// LTX-2.5, the only LTX engine.
    ///
    /// 🔴 A QUERY PARAM rather than a body field, for the reason the Director tag is one:
    /// `LtxVideoGenerateRequest` lives in koblibs and is shared with every consumer, and the body
    /// parse is STRICT (`stage_asset_json_strict` rejects unknown keys), so a new field there is a
    /// cross-repo change. Which engine serves a request is
    /// koblem's own routing concern and nothing outside this service reads it.
    #[serde(default)]
    engine: Option<String>,
}

async fn generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(dtag): Query<DirectorTag>,
    req: Request,
) -> Response {
    let who = match crate::routes::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // 🔴 Not "is it free": an unconfigured engine is a deployment mistake and there is nothing for a
    // queue to wait for. A BUSY or disabled one is now a `blocked_reason` on the row, never a 503.
    let engine = crate::routes::ltx_director::request_engine(dtag.engine.as_deref());
    if !state.ltx25.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{engine} service not configured"),
        );
    }
    // Parse JSON-or-multipart (request JSON + optional image/audio/bgm + per-shot scene / v2v parts).
    let (mut gen_req, staged, image, audio, bgm, seg_images, seg_v2v, seg_audio) =
        match parse_request(&state, req).await {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    let prompts: Vec<String> = gen_req
        .segments
        .iter()
        .map(|s| s.prompt.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if prompts.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "at least one segment prompt is required",
        );
    }

    let director_project = dtag.director_project_id.filter(|s| !s.is_empty());
    let director_wipe = dtag.director_wipe.unwrap_or(false);
    let director_critique = dtag.director_critique.unwrap_or(false);
    let director_tier = dtag
        .director_tier
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "final".to_string());

    let resuming = gen_req
        .resume_job_id
        .as_deref()
        .is_some_and(|r| !r.trim().is_empty());
    if resuming {
        // Refused before the job exists rather than after the queue wait — the same gate the
        // retake route takes, for the same reason.
        if let Some(resp) = crate::routes::ltx_director::bank_gate(&state, engine).await {
            return resp;
        }
        // 🔴 A CONTINUE RESUMES OUT OF SEVERAL BANKS, NOT ONE.
        //
        // `resume_job_id` names one job, and on LTX-2.5 that job's bank holds only the segments IT
        // rendered: five shots as job A, then a continue banking 5-6 as job B, and no single id
        // addresses both — the next continue asks B for segment 0 and gets a 400. So the resuming
        // Director dispatch names each shot's own bank, exactly as the retake path already does,
        // which makes it independent of how many continues deep the chain is.
        if let Some(pid) = &director_project {
            // 🔴 A DIRECTOR CONTINUE MAY ONLY NAME A CHAIN THIS PROJECT BANKED ON THIS ENGINE.
            //
            // The id comes off the editor's saved document, which outlives the engine picker: a
            // project rendered on LTX-2.3 and then switched to 2.5 kept quoting a 2.3 job id, and
            // the two deployments share no job store, so every shot of that render was a 404 in a
            // bank that has never held it. The retake route has always validated this; a continue
            // reached the engine unchecked. Same validator, so the two cannot answer differently.
            //
            // No metadata: the caller named an id, and an explicit id is accepted or refused on the
            // banked rows and the live job store alone — the `last_*` stamps are only ever the
            // fallback for a caller that named nothing, which this one never is.
            if let Err(message) = crate::routes::ltx_director::resolve_resume_job_id_for_tier(
                &state,
                pid,
                &Value::Null,
                &director_tier,
                engine,
                gen_req.resume_job_id.as_deref(),
            )
            .await
            {
                return err(StatusCode::BAD_REQUEST, &message);
            }
            crate::routes::ltx_director::restore_selected_takes(
                &state,
                pid,
                &director_tier,
                engine,
                &mut gen_req.segments,
                None,
            )
            .await;
        }
    }

    let job_id = match &director_project {
        Some(pid) => {
            let tier = director_tier.clone();
            // The PROXY tier's geometry is chosen client-side (low res, few steps, no upscale) and
            // the server otherwise never learns it — which left a proxy-tier RETAKE with nothing to
            // reproduce, so it would have rendered at full final-tier cost. Record the effective
            // overrides from the request that is actually being submitted, which is the only source
            // that cannot drift from what the tier really contains.
            if tier == "proxy" {
                let overrides = json!({
                    "width": gen_req.width,
                    "height": gen_req.height,
                    "steps": gen_req.steps,
                    "cfg_scale": gen_req.cfg_scale,
                    "profile": gen_req.profile,
                    "upscaler_ratio": gen_req.upscaler_ratio,
                });
                if let Err(e) = crate::repositories::director_projects::patch_metadata(
                    &state.db,
                    pid,
                    &json!({ "proxy_render": overrides }),
                )
                .await
                {
                    warn!(error = %e, project = %pid, "proxy render: could not record tier geometry");
                }
            }
            // The ENGINE is recorded on the job, not held in this task: it picks the client for
            // the poll, the cancel and the restart-reattach, and the container the clip and every
            // partial are written under.
            state.ltx_jobs.create_director_on(
                prompts.len() as u32,
                prompts,
                pid.clone(),
                tier,
                Some(engine.to_string()),
                None,
            )
        }
        None => state
            .ltx_jobs
            .create_on(prompts.len() as u32, prompts, Some(engine.to_string())),
    };

    let save_clip = gen_req.save_clip;
    let compiled = match compile_ltx_chain(
        &state, gen_req, &staged, image, audio, bgm, seg_images, seg_v2v, seg_audio, None,
    )
    .await
    {
        Ok(c) => c,
        Err(msg) => {
            job_fail(&state.ltx_jobs, &job_id, msg.clone());
            return err(StatusCode::BAD_REQUEST, &msg);
        }
    };

    match enqueue_ltx(
        &state,
        &who,
        compiled,
        LtxDispatch {
            // Both or neither: the adapter reads the pair, and a tier on a render that backs no
            // project would claim a harvest destination that does not exist.
            director_tier: director_project.as_ref().map(|_| director_tier),
            director_project_id: director_project,
            job_id,
            director_shot_offset: None,
            director_wipe,
            director_critique,
            save_clip,
        },
        crate::routes::queue::QueueOptions::from_headers(&headers),
    )
    .await
    {
        Ok(job_id) => (
            StatusCode::ACCEPTED,
            Json(VideoJobSubmitResponse {
                job_id,
                status: Some(VideoJobState::Queued),
            }),
        )
            .into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

/// What the queue row has to carry beside the chain: the store id the browser already polls, and
/// the Director tags the harvest runs off.
pub(crate) struct LtxDispatch {
    pub job_id: String,
    pub director_project_id: Option<String>,
    pub director_tier: Option<String>,
    pub director_shot_offset: Option<i32>,
    pub director_wipe: bool,
    /// Offer this render to the unattended dailies review once the harvest has filed it. True only
    /// on a FULL run — a retake that chained into a review would let the critic's own re-rolls
    /// recurse.
    pub director_critique: bool,
    pub save_clip: bool,
}

/// Put a compiled chain on the global queue and return the store id the caller should hand back.
///
/// 🔴 The bytes leave the request here. A queued render dispatches hours after the browser that
/// POSTed the multipart has gone, so every part and every transcoded wav becomes an `asset:<hash>`
/// on the way in; the adapter reads them back at dispatch. Nothing megabyte-sized rides in the
/// JSONB payload.
pub(crate) async fn enqueue_ltx(
    state: &AppState,
    who: &crate::auth::Principal,
    compiled: LtxCompiled,
    dispatch: LtxDispatch,
    opts: crate::routes::queue::QueueOptions,
) -> Result<String, (StatusCode, String)> {
    let LtxCompiled {
        chain,
        audio,
        uploads,
        seed,
    } = compiled;
    let assets = capture_parts(state, uploads.entries())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let audio = capture_audio(state, &audio)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    state.ltx_jobs.update(&dispatch.job_id, |j| {
        j.seed = Some(seed);
        j.director_critique = dispatch.director_critique;
    });

    let payload = json!({
        "chain": chain,
        "job_id": dispatch.job_id,
        "assets": assets,
        "audio": audio,
        "save_clip": dispatch.save_clip,
        "director_project_id": dispatch.director_project_id,
        "director_tier": dispatch.director_tier,
        "director_shot_offset": dispatch.director_shot_offset,
        "director_wipe": dispatch.director_wipe,
    });
    let accepted = crate::routes::queue::enqueue(state, who, opts.request(JobKind::Ltx25, payload))
        .await
        .inspect_err(|(_, msg)| job_fail(&state.ltx_jobs, &dispatch.job_id, msg.clone()))?;

    Ok(adopt_queue_row(state, &dispatch.job_id, &accepted).await)
}

/// Tie the store record to its queue row, and hand back the id the CLIENT should poll.
///
/// 🔴 They differ on a dedupe. The row that already exists names a DIFFERENT store record — the one
/// the first submit created — and answering with the record this submit minted would hand the caller
/// an id that never renders.
async fn adopt_queue_row(
    state: &AppState,
    job_id: &str,
    accepted: &koblem_client::types::EnqueueResponse,
) -> String {
    if accepted.deduplicated
        && let Ok(Some((payload, _))) =
            crate::repositories::gpu_jobs::payload(&state.db, &accepted.job_id).await
        && let Some(existing) = payload.get("job_id").and_then(Value::as_str)
        && existing != job_id
    {
        // The record this submit minted has nothing to render it. Closed rather than left `Queued`,
        // or it sits in `/jobs` for its whole TTL as a render that never starts.
        state.ltx_jobs.update(job_id, |j| {
            j.state = VideoJobState::Cancelled;
            j.error = Some(format!(
                "deduplicated onto the identical render already queued as {existing}"
            ));
        });
        return existing.to_string();
    }
    state.ltx_jobs.update(job_id, |j| {
        j.queue_job_id = Some(accepted.job_id.clone());
    });
    job_id.to_string()
}

/// `part name -> asset:<hash>` for every binary the chain names.
pub(crate) async fn capture_parts(
    state: &AppState,
    parts: &[(String, Bytes)],
) -> Result<serde_json::Map<String, Value>, String> {
    let mut out = serde_json::Map::new();
    for (name, bytes) in parts {
        let reference = crate::director_assets::ref_for_bytes(
            state.director_assets_dir.as_ref(),
            &state.db,
            bytes,
        )
        .await
        .map_err(|e| format!("part `{name}`: {e}"))?;
        out.insert(name.clone(), json!(reference));
    }
    Ok(out)
}

/// The engine-ready wavs, as refs. Already transcoded — doing it at dispatch would put an ffmpeg run
/// under the GPU guard.
async fn capture_audio(
    state: &AppState,
    audio: &crate::ltx_video::ChainAudio,
) -> Result<Value, String> {
    let mut shot_drive = Vec::with_capacity(audio.shot_drive.len());
    for w in &audio.shot_drive {
        shot_drive.push(capture_wav(state, w).await?);
    }
    let mut shot_track = Vec::with_capacity(audio.shot_track.len());
    for w in &audio.shot_track {
        shot_track.push(capture_wav(state, w).await?);
    }
    Ok(json!({
        "drive": capture_wav(state, &audio.drive).await?,
        "track": capture_wav(state, &audio.track).await?,
        "shot_drive": shot_drive,
        "shot_track": shot_track,
    }))
}

async fn capture_wav(state: &AppState, wav: &Option<Vec<u8>>) -> Result<Option<String>, String> {
    match wav {
        Some(bytes) => crate::director_assets::ref_for_bytes(
            state.director_assets_dir.as_ref(),
            &state.db,
            bytes,
        )
        .await
        .map(Some)
        .map_err(|e| format!("audio: {e}")),
        None => Ok(None),
    }
}

/// `POST /api/v1/ltx-video/relip` — lip-dub / relip: attach a drive-audio track to a source
/// video and re-render the mouths in sync. Multipart with a `request` JSON part (an
/// [`LtxRelipRequest`]), a `video` part (the source clip) and an `audio` part (the drive
/// track). Returns 202 + `{job_id}`; `queue::adapters::ltx_relip` renders it.
///
/// 🔴 The bytes leave the request HERE. A queued relip dispatches hours after the browser that
/// POSTed the multipart has gone, so the clip and the drive track become `asset:<hash>` refs on the
/// way in — a queue row is JSON, and neither of these is small.
///
/// 🔴 The old "ltx busy or disabled — retry shortly" refusal is gone with the acquire that raised
/// it. An unavailable engine is a `blocked_reason` on a row now, not a 503 at the form.
async fn relip(State(state): State<AppState>, headers: HeaderMap, req: Request) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    // 🔴 A CONSTANT, not a `?engine=` like the sibling routes. The window request this path builds
    // is LTX-2.5's relip form — a frozen in-context reference plus `upscale` — and the C++ engine's
    // form (`hires` + `relip_ref_tstride` + a cold-load VRAM ritual) is gone with it, so an engine
    // the caller picked would only pick which way the submit fails.
    let engine = crate::routes::ltx_director::ENGINE_LTX25;
    let (relip_req, video, audio) = match parse_relip_request(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(video) = video else {
        return err(StatusCode::BAD_REQUEST, "missing 'video' part");
    };
    let Some(audio) = audio else {
        return err(StatusCode::BAD_REQUEST, "missing 'audio' part");
    };
    let who = match crate::routes::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let dir = state.director_assets_dir.as_ref();
    let video_ref = match crate::director_assets::capture_bytes(dir, &state.db, &video).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("video: {e}")),
    };
    let audio_ref = match crate::director_assets::capture_bytes(dir, &state.db, &audio).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("audio: {e}")),
    };
    // Probed off the stored file rather than the request bytes: the capture already wrote it, and
    // the window count and the canvas are what the queue row is sized and bucketed by.
    let source = match crate::director_assets::ref_hash(&video_ref)
        .and_then(|h| crate::director_assets::path_for(dir, h))
    {
        Some(p) => p,
        None => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the captured clip is gone",
            );
        }
    };
    let plan = match crate::queue::adapters::ltx_relip::plan_for(&source, &relip_req).await {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };

    // A relip's unit is the WINDOW, and the store record counts them so the browser's poll can show
    // how far a long dub has got. The mouth-style prompt is its one script line.
    let script = relip_req.prompt.clone().unwrap_or_default();
    let job_id =
        state
            .ltx_jobs
            .create_on(plan.windows.max(1), vec![script], Some(engine.to_string()));

    let payload = crate::queue::adapters::ltx_relip::payload_for(
        &job_id, &video_ref, &audio_ref, &relip_req, &plan,
    );
    let accepted = crate::routes::queue::enqueue(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(&headers)
            .request(JobKind::LtxRelip, payload),
    )
    .await;
    let accepted = match accepted {
        Ok(a) => a,
        Err((code, msg)) => {
            job_fail(&state.ltx_jobs, &job_id, msg.clone());
            return err(code, &msg);
        }
    };

    (
        StatusCode::ACCEPTED,
        Json(VideoJobSubmitResponse {
            job_id: adopt_queue_row(&state, &job_id, &accepted).await,
            status: Some(VideoJobState::Queued),
        }),
    )
        .into_response()
}

/// `GET /api/v1/ltx-video/jobs/{id}` — current status of a submitted render. The browser
/// polls this; a reconnecting tab re-attaches by id. `result_url` is set once complete.
async fn job_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(mut job) = state.ltx_jobs.get(&id) else {
        return err(StatusCode::NOT_FOUND, "no such job");
    };
    if let Some(row) = queue_row_for(&state, &job).await {
        overlay_queue_state(&mut job, &row);
    }
    let resumable = resumable_now(&state, &job).await;
    Json(job_to_status(&id, &job, resumable)).into_response()
}

/// The `gpu_jobs` row backing a store record, when there is one.
pub(crate) async fn queue_row_for(
    state: &AppState,
    job: &crate::ltx_jobs::LtxJob,
) -> Option<QueueJob> {
    let id = job.queue_job_id.as_deref()?;
    crate::repositories::gpu_jobs::get(&state.db, id)
        .await
        .ok()?
}

/// Answer the poll from the QUEUE while the render has not reached the store yet.
///
/// 🔴 Both directions matter. A row still `queued` at 03:00 is a store record that has said
/// `Queued` since 22:00 and would otherwise be indistinguishable from one about to submit — and a
/// row the dispatcher FAILED at admission never reaches the adapter at all, so without this the
/// record sits `Queued` for the rest of its TTL and the editor waits on a render that will never
/// happen. Once the record is terminal it wins: the adapter files `Completed` inside its collect
/// pass, minutes before the runner marks the row `done`.
pub(crate) fn overlay_queue_state(job: &mut crate::ltx_jobs::LtxJob, row: &QueueJob) {
    if job.state.is_terminal() {
        return;
    }
    job.state = match row.state {
        JobState::Queued | JobState::Held => VideoJobState::Queued,
        JobState::Running | JobState::Collecting => VideoJobState::Rendering,
        JobState::Done => VideoJobState::Completed,
        JobState::Failed => VideoJobState::Failed,
        JobState::Cancelled => VideoJobState::Cancelled,
    };
    if job.error.is_none() {
        job.error.clone_from(&row.error);
    }
}

/// `VideoJobStatus.resumable` — the job RECORD's half and the DEPLOYMENT's, together.
///
/// 🔴 The deployment half is asked only once the record's half already says yes. A running job is
/// never resumable, and this route is polled every 1.5 s for the length of a render, so probing
/// unconditionally would put a capabilities round trip on the engine that is mid-render, on every
/// poll, to answer a question whose answer is already no.
async fn resumable_now(state: &AppState, job: &crate::ltx_jobs::LtxJob) -> bool {
    job.resumable(true) && state.ltx25.bank_live().await
}

/// `?standalone=true` on the job list.
///
/// The Director shares this store (`LtxJobStore::create_director`), and [`VideoJobStatus`] carries
/// no project id, so an unfiltered list gives a plain LTX client no way to tell a Director render
/// from its own. That client adopts the newest unfinished render as the engine's — which for a
/// Director job would show a render it cannot own and offer to cancel it.
#[derive(serde::Deserialize)]
struct ListJobsQuery {
    #[serde(default)]
    standalone: bool,
}

/// `GET /api/v1/ltx-video/jobs` — in-flight + recently-finished jobs, for reconnect.
async fn list_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListJobsQuery>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let mut jobs = state.ltx_jobs.list();
    // One read for the whole listing rather than one per job: the rows a store full of renders
    // points at are the pending + live set almost by definition, and the tail is terminal anyway.
    let rows = crate::repositories::gpu_jobs::recent(&state.db, 200)
        .await
        .unwrap_or_default();
    for (_, job) in &mut jobs {
        if let Some(id) = job.queue_job_id.as_deref()
            && let Some(row) = rows.iter().find(|r| r.id == id)
        {
            overlay_queue_state(job, row);
        }
    }
    let banks = BankLive::read(&state, &jobs).await;
    Json(VideoJobListResponse {
        jobs: job_list(&jobs, q.standalone, banks),
    })
    .into_response()
}

/// Whether the LTX engine can reach a latent bank right now, resolved ONCE for a whole listing.
///
/// The bank is a directory an operator configures, so this is a deployment fact and not something
/// a job record can know. Read up front rather than per job, which keeps [`job_list`] pure.
#[derive(Debug, Clone, Copy)]
struct BankLive(bool);

impl BankLive {
    /// 🔴 Asked only once some job's OWN record already claims to be resumable — the same rule as
    /// [`resumable_now`], for the same reason. A running job is never resumable, so a store full of
    /// live renders must not cost a capabilities round trip to the engine that is rendering them.
    /// The `false` it falls back to is not a claim about the deployment: it is only ever ANDed with
    /// a record half that is already false.
    async fn read(state: &AppState, jobs: &[(String, crate::ltx_jobs::LtxJob)]) -> Self {
        if !jobs.iter().any(|(_, j)| j.resumable(true)) {
            return Self(false);
        }
        Self(state.ltx25.bank_live().await)
    }

    fn resumable(self, job: &crate::ltx_jobs::LtxJob) -> bool {
        job.resumable(self.0)
    }
}

fn job_list(
    jobs: &[(String, crate::ltx_jobs::LtxJob)],
    standalone: bool,
    banks: BankLive,
) -> Vec<VideoJobStatus> {
    jobs.iter()
        .filter(|(_, j)| !standalone || j.director_project_id.is_none())
        .map(|(id, job)| job_to_status(id, job, banks.resumable(job)))
        .collect()
}

/// `GET /api/v1/ltx-video/jobs/{id}/media` — stream the finished clip held in the store.
async fn job_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(job) = state.ltx_jobs.get(&id) else {
        return err(StatusCode::NOT_FOUND, "no such job");
    };
    let Some(content_type) = job.content_type else {
        return err(StatusCode::NOT_FOUND, "job has no clip (not completed)");
    };
    let path = state.ltx_jobs.clip_path(&id);
    // serve_file honours Range requests so the <video> element can seek/scrub
    // without the whole clip being read into RAM per request.
    match crate::routes::byte_serve::serve_file(&headers, &path, &content_type).await {
        Ok(resp) => resp,
        Err(e) => err(StatusCode::NOT_FOUND, &format!("clip file missing: {e}")),
    }
}

/// `GET /api/v1/ltx-video/jobs/{id}/segments/{n}` — stream one progressively-delivered unit held
/// in the store (persisted before the stitched clip). Mirrors [`job_media`], but keyed by unit
/// index; 404 while it hasn't landed (or after it's swept). Best-effort — a missing partial just
/// isn't previewed.
///
/// The unit is a SHOT on LTX and a whole GENERATION on H3, and the container differs with it
/// (webm vs mp4), so both the path and the served type come from the store rather than being
/// hard-coded here.
async fn job_segment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, n)): Path<(String, u32)>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if state.ltx_jobs.get(&id).is_none() {
        return err(StatusCode::NOT_FOUND, "no such job");
    }
    let path = state.ltx_jobs.partial_path(&id, n);
    let content_type = state.ltx_jobs.partial_content_type(&id);
    match crate::routes::byte_serve::serve_file(&headers, &path, content_type).await {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "segment not available"),
    }
}

/// Status body for the status/list endpoints (omits the clip bytes).
fn job_to_status(id: &str, job: &crate::ltx_jobs::LtxJob, resumable: bool) -> VideoJobStatus {
    VideoJobStatus {
        job_id: id.to_string(),
        status: job.state,
        // What a client adopting a render started elsewhere counts elapsed time from.
        created_at: job.created_at(),
        server_job_id: job.server_job_id.clone(),
        // Pass back as `resume_job_id` to continue a failed render.
        resume_job_id: job.server_job_id.clone(),
        // 🔴 Both halves, and the CALLER owns them: `LtxJob::resumable` is the record's, and the
        // deployment's is a live fact about the running engine (`resumable_now` / [`BankLive`]).
        resumable,
        segments: job.segments,
        scripts: job.scripts.clone(),
        gate_wait_ms: job.gate_wait_ms,
        render_ms: job.render_ms,
        error: job.error.clone(),
        result_url: job
            .content_type
            .as_ref()
            .map(|_| format!("/api/v1/ltx-video/jobs/{id}/media")),
        // Progressive per-unit deliveries banked so far (empty for a non-progressive render, or one
        // that hasn't finished its first). The UI previews each before `result_url` lands. A unit
        // is a shot on LTX and a whole GENERATION on H3 — see `VideoJobPartial`.
        partials: job.partials.clone(),
        // Whether `result_url` is the timeline or a fragment of it. Latched from the plan echo on
        // the first poll and from `result.job_is_partial` on the last, so it is already true by the
        // time a watching client has bytes to adopt — and ORed with the offset through
        // `delivers_fragment`, because an H3 retake is a fragment by its shift and reports no flag.
        job_is_partial: job.delivers_fragment(),
        // H3 only, and `None` on every LTX job. Absence means "this engine does not report
        // progress", never zero.
        total_eta_seconds: job.total_eta_seconds,
        progress: job.progress,
        current_step: job.current_step,
        total_steps: job.total_steps,
        phase: job.phase.clone(),
        preview_seq: job.preview_seq,
        generation_index: job.generation_index,
        generations_done: job.generations_done,
    }
}

// Axum re-export so the handler can take a raw Request (JSON or multipart).
use axum::extract::Request;

type Parts = (
    LtxVideoGenerateRequest,
    // The bytes behind every `part:<name>` marker the parse wrote into the request (§3). Attached
    // to the engine submit as binary parts, or inlined again if the engine is too old (§9.4).
    StagedParts,
    Option<Bytes>,
    Option<Bytes>,
    Option<Bytes>,
    // Per-shot scene images (i2v), indexed by RAW segment index (multipart `seg_image_<i>`).
    // Sparse: a slot is `Some` only when that shot uploaded an image. Empty on the JSON path.
    Vec<Option<Bytes>>,
    // Per-shot v2v source clips (foreign uploads), indexed by RAW segment index (multipart
    // `seg_v2v_<i>`). Sparse: `Some` only when that shot's `v2v_source = upload`. koblem decodes
    // each to per-frame PNGs → the engine's `control_frames`. Empty on the JSON path.
    Vec<Option<Bytes>>,
    // Per-shot audio clips, indexed by RAW segment index (multipart `seg_audio_<i>`). Sparse.
    // Empty on the JSON path, where a saved project's clips ride inline as `segment.audio_b64`.
    Vec<Option<Bytes>>,
);

/// Deserialize a request body that may carry `asset:<hash>` references, STAGING each one: the
/// field is rewritten to a `part:<name>` marker and the bytes come back alongside, to ride to the
/// engine as a binary part. Same edge as the director's `AssetJson`
/// extractor — this path can't use it because the JSON arrives either as the whole body or as one
/// multipart part.
///
/// Also REJECTS any field the request type does not have, so a body built for another engine fails
/// here in milliseconds instead of rendering the wrong thing (see `crate::strict_json`). Only this
/// HTTP edge is strict: `routes/ltx_director.rs` re-parses the same type out of a STORED timeline
/// document and must keep accepting one written by an older build.
async fn parse_generate_json(
    assets_dir: &std::path::Path,
    raw: &[u8],
) -> Result<(LtxVideoGenerateRequest, StagedParts), String> {
    crate::strict_json::stage_asset_json_strict(assets_dir, raw, "ltx-video /generate").await
}

async fn parse_request(state: &AppState, req: Request) -> Result<Parts, Response> {
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.starts_with("multipart/form-data") {
        let mut multipart = Multipart::from_request(req, &())
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")))?;
        let mut gen_req: Option<(LtxVideoGenerateRequest, StagedParts)> = None;
        let mut image = None;
        let mut audio = None;
        let mut bgm = None;
        // Sparse per-shot scene images by raw segment index; grown on demand.
        let mut seg_images: Vec<Option<Bytes>> = Vec::new();
        // Sparse per-shot v2v source clips by raw segment index; grown on demand.
        let mut seg_v2v: Vec<Option<Bytes>> = Vec::new();
        // Sparse per-shot audio clips by raw segment index; grown on demand.
        let mut seg_audio: Vec<Option<Bytes>> = Vec::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("multipart field: {e}")))?
        {
            let name = field.name().unwrap_or("").to_string();
            match name.as_str() {
                "request" => {
                    let txt = field.text().await.unwrap_or_default();
                    gen_req = Some(
                        parse_generate_json(state.director_assets_dir.as_ref(), txt.as_bytes())
                            .await
                            .map_err(|e| err(StatusCode::BAD_REQUEST, &e))?,
                    );
                }
                "image" => image = field.bytes().await.ok(),
                "audio" => audio = field.bytes().await.ok(),
                "bgm" => bgm = field.bytes().await.ok(),
                // Per-shot scene image: `seg_image_<i>` (i = raw segment index).
                _ if name.starts_with("seg_image_") => {
                    if let Ok(i) = name["seg_image_".len()..].parse::<usize>()
                        && let Ok(b) = field.bytes().await
                    {
                        if seg_images.len() <= i {
                            seg_images.resize(i + 1, None);
                        }
                        seg_images[i] = Some(b);
                    }
                }
                // Per-shot v2v source clip: `seg_v2v_<i>` (i = raw segment index).
                _ if name.starts_with("seg_v2v_") => {
                    if let Ok(i) = name["seg_v2v_".len()..].parse::<usize>()
                        && let Ok(b) = field.bytes().await
                    {
                        if seg_v2v.len() <= i {
                            seg_v2v.resize(i + 1, None);
                        }
                        seg_v2v[i] = Some(b);
                    }
                }
                // Per-shot audio clip: `seg_audio_<i>` (i = raw segment index).
                _ if name.starts_with("seg_audio_") => {
                    if let Ok(i) = name["seg_audio_".len()..].parse::<usize>()
                        && let Ok(b) = field.bytes().await
                    {
                        if seg_audio.len() <= i {
                            seg_audio.resize(i + 1, None);
                        }
                        seg_audio[i] = Some(b);
                    }
                }
                _ => {}
            }
        }
        let (gen_req, staged) =
            gen_req.ok_or_else(|| err(StatusCode::BAD_REQUEST, "missing 'request' part"))?;
        Ok((
            gen_req, staged, image, audio, bgm, seg_images, seg_v2v, seg_audio,
        ))
    } else {
        let bytes = axum::body::to_bytes(req.into_body(), LTX_BODY_LIMIT)
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("body: {e}")))?;
        let (gen_req, staged) = parse_generate_json(state.director_assets_dir.as_ref(), &bytes)
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
        Ok((
            gen_req,
            staged,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
    }
}

// Needs FromRequest for Multipart::from_request.
use axum::extract::FromRequest;

type RelipParts = (LtxRelipRequest, Option<Bytes>, Option<Bytes>);

/// Parse the relip multipart: a `request` JSON part (optional — defaults apply) + a `video`
/// part + an `audio` part.
async fn parse_relip_request(req: Request) -> Result<RelipParts, Response> {
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !content_type.starts_with("multipart/form-data") {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "relip requires multipart/form-data (video + audio)",
        ));
    }
    let mut multipart = Multipart::from_request(req, &())
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")))?;
    let mut relip_req: Option<LtxRelipRequest> = None;
    let mut video = None;
    let mut audio = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, &format!("multipart field: {e}")))?
    {
        match field.name().unwrap_or("") {
            "request" => {
                let txt = field.text().await.unwrap_or_default();
                if !txt.trim().is_empty() {
                    relip_req = Some(serde_json::from_str(&txt).map_err(|e| {
                        err(StatusCode::BAD_REQUEST, &format!("bad 'request' json: {e}"))
                    })?);
                }
            }
            "video" => video = field.bytes().await.ok(),
            "audio" => audio = field.bytes().await.ok(),
            _ => {}
        }
    }
    Ok((relip_req.unwrap_or_default(), video, audio))
}

/// Round `n` DOWN to the nearest valid LTX latent-frame count (8k+1). Returns 0 for n<9 (too
/// short to render a standalone window).
pub(crate) fn round_down_8k1(n: u32) -> u32 {
    if n < 9 {
        return 0;
    }
    let k = (n - 1) / 8;
    8 * k + 1
}

/// Round a dimension to the nearest multiple of 32 (LTX's spatial patch requirement), min 32.
pub(crate) fn round32(x: u32) -> u32 {
    let r = ((x + 16) / 32) * 32;
    r.max(32)
}

/// One extracted frame's path. `n` is 1-based — ffmpeg's image2 muxer numbers from 1.
pub(crate) fn frame_path(out_dir: &std::path::Path, n: u32) -> std::path::PathBuf {
    out_dir.join(format!("f_{n:06}.{FRAME_EXT}"))
}

const FRAME_EXT: &str = "jpg";

/// Extract every source frame, scaled to `w×h`, into `out_dir` (see [`frame_path`]).
/// Returns the number of frames written.
///
/// JPEG rather than PNG because every frame is then base64'd into ONE `control_frames` JSON array —
/// a 20s relip is hundreds of full-resolution frames over loopback, and PNG made that ~3.4x larger
/// than it needs to be on real footage. 4:4:4 at the encoder's best quality keeps chroma at full
/// resolution, so the only loss is DCT quantisation, far below the 8x spatial compression the
/// engine's VAE applies to these same pixels straight afterwards.
pub(crate) async fn extract_frames(
    src: &std::path::Path,
    out_dir: &std::path::Path,
    w: u32,
    h: u32,
) -> std::io::Result<u32> {
    let pattern = out_dir.join(format!("f_%06d.{FRAME_EXT}"));
    let status = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(src)
        .args(["-vf", &format!("scale={w}:{h}:flags=lanczos")])
        .args(["-q:v", "2", "-pix_fmt", "yuvj444p"])
        .arg(&pattern)
        .args(["-loglevel", "error"])
        .status()
        .await?;
    if !status.success() {
        return Err(std::io::Error::other("ffmpeg frame extract failed"));
    }
    let mut count = 0u32;
    let mut rd = tokio::fs::read_dir(out_dir).await?;
    while let Some(ent) = rd.next_entry().await? {
        if ent.path().extension().and_then(|e| e.to_str()) == Some(FRAME_EXT) {
            count += 1;
        }
    }
    Ok(count)
}

/// Mark a job failed in the store (and log it) — for a record minted before the enqueue that would
/// have rendered it, which must not sit `Queued` for the rest of its TTL.
fn job_fail(store: &crate::ltx_jobs::LtxJobStore, id: &str, msg: String) {
    warn!(job = %id, msg = %msg, "ltx job failed");
    store.update(id, |j| {
        j.state = VideoJobState::Failed;
        j.error = Some(msg);
    });
}

/// One render's engine chain, compiled.
///
/// 🔴 Everything here is ROUTE-SIDE work and stays route-side: resolving an upload, splitting a
/// mixed track through songgen, transcoding every wav, decoding a v2v clip to control frames. It is
/// CPU at the edge, it is where a bad request can still be answered with a 400 instead of a job
/// that fails at 03:00, and doing it under the GPU guard would hold a card through an ffmpeg run.
pub(crate) struct LtxCompiled {
    pub chain: Value,
    pub audio: crate::ltx_video::ChainAudio,
    /// Named binary parts the chain references. They become `asset:<hash>` refs at enqueue — a queue
    /// row is JSON and the browser that POSTed the multipart is long gone by dispatch time.
    pub uploads: PartUploads,
    pub seed: i64,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn compile_ltx_chain(
    state: &AppState,
    mut req: LtxVideoGenerateRequest,
    // Bytes behind the `part:<name>` markers in `req`, staged at the parse (§3). Whatever the
    // built chain still references is attached to the submit; the rest is never uploaded.
    staged: &StagedParts,
    image_part: Option<Bytes>,
    audio_part: Option<Bytes>,
    bgm_part: Option<Bytes>,
    seg_image_parts: Vec<Option<Bytes>>,
    // Per-shot foreign v2v source clips (multipart `seg_v2v_<raw i>`), aligned to RAW segment
    // index. `Some` only for a `v2v_source = upload` shot; koblem decodes it → `control_frames`.
    seg_v2v_parts: Vec<Option<Bytes>>,
    // Per-shot audio clips (multipart `seg_audio_<raw i>`), aligned to RAW segment index. Sparse;
    // a saved project's clips arrive inline on the segment instead (`audio_b64`).
    seg_audio_parts: Vec<Option<Bytes>>,
    // Segment-level RETAKE: a RAW segment index to re-render from (reloading the banked prefix
    // [0, k) from the resumed job dir). `None` = a normal render/continuation. Only meaningful
    // together with `req.resume_job_id`. See the `retake_from` emission below.
    retake_from: Option<u32>,
) -> Result<LtxCompiled, String> {
    // ── ONE DiT: drop any base-variant name before anything reads it ──────────────────────────
    //
    // 🔴 Normalised HERE rather than at the `let model` below because `req.model` has eight other
    // readers — the identity regime, the head-trim rule, the per-shot variant fallback — and a
    // `model` local that disagreed with the field they read would be worse than the bug it fixed.
    //
    // A saved timeline can still carry a variant name from when LTX-2.3 offered a picker, and the
    // failure is invisible from the editor: props answer `variants: []`, which HIDES the picker
    // (`SettingsSheet` renders it only when `variants.length > 1`), so the value sits in the
    // document where the user cannot see or clear it and every render 400s on a field they never
    // chose. Dropped rather than 400'd: the field is meaningless, not wrong — there is one
    // checkpoint and it is the one being asked for. Per-shot overrides go with it, since
    // `seg.model` falls back to `req.model` and is validated by the same contract.
    req.model = None;
    for s in &mut req.segments {
        s.model = None;
    }

    // Keep prompt + optional per-shot scene image + keyframes + v2v source aligned through the
    // empty-prompt filter, so a shot's uploaded image (multipart `seg_image_<raw index>`), v2v
    // clip (`seg_v2v_<raw index>`) and inline keyframes still map to the right rendered segment.
    let seg_specs: Vec<SegSpec> = req
        .segments
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let p = s.prompt.trim().to_string();
            if p.is_empty() {
                return None;
            }
            Some(SegSpec {
                prompt: p,
                bank_job_id: s
                    .bank_job_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(str::to_string),
                model: s.model.clone(),
                loras: s.loras.clone(),
                image: s.image.clone().unwrap_or_default(),
                image_bytes: seg_image_parts.get(i).cloned().flatten(),
                keyframes: s.keyframes.clone().unwrap_or_default(),
                // Director variable-length: this shot's own duration (seconds). None ⇒ the
                // shot uses the uniform `segment_frames` (byte-identical to the old behaviour).
                seconds: s.seconds,
                mode: s.mode,
                v2v_strength: s.v2v_strength,
                v2v_source: s.v2v_source.clone(),
                v2v_upload: seg_v2v_parts.get(i).cloned().flatten(),
                scene_cut: s.scene_cut.unwrap_or(false),
                pin_strength: s.pin_strength,
                // Uploaded part wins over the persisted inline copy (same shot, fresher bytes).
                audio_bytes: seg_audio_parts.get(i).cloned().flatten().or_else(|| {
                    s.audio_b64
                        .as_deref()
                        .and_then(|b| decode_data_uri_b64(b).map(Bytes::from))
                }),
                audio_kind: s.audio_kind.unwrap_or_default(),
                beats: s.beats.clone().unwrap_or_default(),
                seed: s.seed,
                steps: s.steps,
                cfg: s.cfg,
                negative_prompt: s
                    .negative_prompt
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string),
                // Character sheets for this shot. Refs whose image never resolved (an editor bug,
                // or a legacy doc) are dropped rather than sent empty — an empty `image` is the
                // one input the engine answers with a body-less 400.
                character_refs: s
                    .character_refs
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|r| !r.image_b64.trim().is_empty())
                    .collect(),
                // Per-shot wins over the project value, `off` included — that is the whole point
                // of the per-shot control, and why this is `.or()` on the OPTION rather than a
                // comparison against a default.
                frame_drop: s.frame_drop.or(req.frame_drop),
            })
        })
        .collect();
    let prompts: Vec<String> = seg_specs.iter().map(|s| s.prompt.clone()).collect();
    let n_segments = prompts.len() as u32;

    let fps = req.fps.unwrap_or(24).max(1);
    let segment_frames = req.segment_frames.unwrap_or(DEFAULT_SEGMENT_FRAMES).max(9);
    // Default K=2 (locked 2026-07-21): ~8 fewer carried frames/segment than K=3, holds up on
    // eye-test with guide-pos + cond-noise.
    let cont_latent_frames = req.cont_latent_frames.unwrap_or(2).max(1);
    // Seam headroom padded onto each continuation shot's RENDER length, and the exact trim the
    // engine applies (we send it as `cont_seam_drop_frames`). Derived from K, so it can no longer
    // drift out of step with it: a shot defined as 4s produces exactly 4s, and the seam is A/V
    // frame-exact. Scene-cut shots render full (drop 0).
    let overlap = ltx_chain_seam_drop(cont_latent_frames);

    // 1. Reference image → inline base64 (top-level seg-0 i2v opener).
    // Images koblem resolved itself (an upload, a URL fetch) ride as PARTS from here on, named by
    // content hash — so the opener reused as shot 4's scene image is one upload, and the chain JSON
    // carries a ~20-byte marker where it used to carry a whole picture (§3, §5).
    let mut uploads = PartUploads::default();
    let init_marker = match resolve_image(&req.image_source, image_part).await {
        Ok(b) => b.map(|bytes| uploads.add(bytes)),
        Err(msg) => return Err(format!("image: {msg}")),
    };

    // 1b. Per-shot scene images (i2v mid-chain). Each shot with an image starts a NEW scene
    //     from it; audio stays continuous across the cut (the engine only resets video
    //     continuation). Shot 0's per-shot image, if any, is the opener and overrides the
    //     top-level `image_source`. Resolved to inline base64, aligned to `seg_specs`.
    let mut seg_image_markers: Vec<Option<String>> = Vec::with_capacity(seg_specs.len());
    for (idx, spec) in seg_specs.iter().enumerate() {
        match resolve_image(&spec.image, spec.image_bytes.clone()).await {
            Ok(b) => seg_image_markers.push(b.map(|by| uploads.add(by))),
            Err(msg) => return Err(format!("shot {} image: {msg}", idx + 1)),
        }
    }
    // Shot 0's own image wins as the opener; else the legacy top-level reference.
    let opener_marker = seg_image_markers.first().cloned().flatten().or(init_marker);

    // 2. Audio orchestration. `voice_track` drives per-segment lip-sync slicing — and is ONLY
    //    ever a clean voice, never BGM, so background music can't bleed into mouth movements.
    //    `final_audio` (when Some) REPLACES the clip's audio with the user's original
    //    full-quality upload. This includes voice-only: the model audio is conditioning only.
    let (voice_track, final_audio): (Option<Bytes>, Option<Bytes>) = match &req.audio_source {
        // Drive with 16 kHz slices, then remux the original continuous upload. This
        // matches run_singing_clip.sh: model/VAE segment audio is only conditioning and
        // would otherwise click or fuzz at a continuation seam.
        LtxAudioSource::VoiceUpload => (audio_part.clone(), audio_part.clone()),
        LtxAudioSource::BgmUpload => (None, bgm_part.clone().or_else(|| audio_part.clone())),
        LtxAudioSource::MixedUpload => {
            // Drive lip-sync with the SEPARATED voice only; the original mix is the final audio
            // (lossy/length-capped separated stems never reach output). If separation is
            // unavailable, render WITHOUT lip-sync rather than let BGM drive the mouths.
            let voice = match (&audio_part, state.songgen.is_configured()) {
                // 🔴 Under the heavy gate, like every other separation. Bypassing it was correct
                // while this ran INSIDE the render task, which already held a slot; the compile
                // now runs at ENQUEUE from three HTTP handlers that hold nothing, so there is no
                // deadlock left to avoid — and an ungated GPU call is invisible to the
                // dispatcher's direct-holder census, which is how the queue came to be able to
                // submit a songgen job on top of one.
                (Some(mix), true) => match state.songgen.acquire().await {
                    Some((guard, _wait)) => {
                        let outcome = state
                            .songgen
                            .separate(mix.clone(), "mix.wav", guard.chosen_gpu())
                            .await;
                        drop(guard);
                        match outcome {
                            Ok((voice, _bgm)) => Some(voice),
                            Err(e) => {
                                warn!(error = %e, "ltx: songgen /separate failed; rendering mixed upload without lip-sync (won't drive mouths with BGM)");
                                None
                            }
                        }
                    }
                    None => {
                        warn!(
                            "ltx: songgen is administratively disabled, so the mixed upload \
                             renders without lip-sync (won't drive mouths with BGM)"
                        );
                        None
                    }
                },
                _ => None,
            };
            (voice, audio_part.clone())
        }
        LtxAudioSource::Tts { .. } => {
            // TODO: render a TTS voice track from the joined scripts. For now, no lip-sync.
            (None, None)
        }
        LtxAudioSource::None => (None, None),
    };

    // Per-shot classification + frame budget (shared by the driving-audio offsets AND the segment
    // requests below). `scene(i)` MUST match the engine's fresh-shot rule: seg 0, a per-segment
    // scene image, a frame-0 keyframe, a v2v shot, or an explicit `scene_cut` shot render full length
    // (engine drop=0); every other shot is a continuation the engine trims by `overlap` at the seam.
    // A keyframe shot is FRESH only
    // if it pins frame 0 (or is shot 0); mid/late-only pins CONTINUE the prior motion.
    // NOTE: this MUST mirror the engine's `fresh_anchor` rule INCLUDING ITS PRECEDENCE
    // (stable-diffusion.cpp, generate_video_chain). The engine gives keyframes precedence over a
    // scene image and over the scene-cut flag: a seg>0 keyframe shot with no frame-0 pin is a
    // CONTINUATION (Director v2 "merged" reveal) even when it also carries an image or the flag.
    // Flat-OR-ing those, as this did, classified such a shot as fresh here and as a continuation
    // there — koblem padded 0, the engine trimmed anyway, and the shot plus everything after it slid
    // against the muxed track. (`segment_seam_drop_frames` below now makes any residual disagreement
    // non-fatal, but the two rules should still agree so the shot is TRIMMED the way it was
    // CONDITIONED.)
    let shot_scene: Vec<bool> = (0..prompts.len())
        .map(|i| {
            let kfs = seg_specs.get(i).map(|s| &s.keyframes);
            let has_kf = kfs.map(|k| !k.is_empty()).unwrap_or(false);
            let kf_frame0 = kfs
                .map(|k| {
                    k.iter().any(|p| {
                        ((p.seconds * fps as f64).round() as i64)
                            .clamp(0, segment_frames as i64 - 1)
                            == 0
                    })
                })
                .unwrap_or(false);
            let is_v2v = seg_specs
                .get(i)
                .map(|s| s.engine_v2v_mode().is_some())
                .unwrap_or(false);
            // Engine: kf_fresh = has_keyframes && (seg == 0 || kf_frame0), and keyframes suppress
            // both scene-cut forms.
            let kf_fresh = has_kf && (i == 0 || kf_frame0);
            let scene_img = i > 0
                && !is_v2v
                && !has_kf
                && seg_image_markers
                    .get(i)
                    .map(|o| o.is_some())
                    .unwrap_or(false);
            let text_cut = i > 0
                && !is_v2v
                && !has_kf
                && !scene_img
                && seg_specs.get(i).map(|s| s.scene_cut).unwrap_or(false);
            i == 0 || is_v2v || kf_fresh || scene_img || text_cut
        })
        .collect();
    // VISIBLE (produced) length each shot should yield — the director/UI's requested duration
    // (per-shot seconds → 8k+1, else the uniform segment_frames).
    let shot_visible: Vec<u32> = (0..prompts.len())
        .map(|i| {
            seg_specs
                .get(i)
                .and_then(|s| s.seconds)
                .map(|secs| frames_from_seconds(secs, fps))
                .unwrap_or(segment_frames)
        })
        .collect();
    // RENDER length sent to the engine: continuation shots are padded by `overlap` frames of seam
    // headroom, which the engine trims straight back off (we pin it via `cont_seam_drop_frames`).
    // Pad and trim are the same number, so a shot's produced length is EXACTLY `shot_visible`
    // (7s in → 7s out). visible is 8k+1 and overlap is a multiple of 8, so render stays a valid
    // 8k+1 count.
    let shot_render: Vec<u32> = (0..prompts.len())
        .map(|i| shot_visible[i] + if shot_scene[i] { 0 } else { overlap })
        .collect();
    // VRAM GUARD. Refuse before the GPU lock rather than after: above `LTX_MAX_RENDER_FRAMES` the
    // render does not fit the measured budget, and above 248 the engine additionally tiles the
    // temporal axis and returns a slower, softer render that looks like a successful one. A silent
    // quality downgrade is the worst of the three outcomes (render / refuse / degrade), so koblem
    // takes the refusal.
    //
    // Checked on the RENDER length, not the visible one, because the render length is what the DiT
    // actually samples: a continuation shot pays `overlap` frames of seam headroom on top of its
    // visible duration, so it hits the window sooner than a scene-cut shot of the same duration.
    // The message says which shot, both frame counts, and the seconds the user can actually get,
    // because "241" means nothing to someone holding a seconds slider.
    if let Some(msg) = (0..shot_render.len())
        .find_map(|i| shot_window_error(i, shot_render[i], shot_visible[i], fps))
    {
        return Err(msg);
    }
    // The trim the engine must apply to each shot, stated outright rather than re-derived there.
    // Exactly the padding we added, so produced == `shot_visible` for every shot — and it holds even
    // if the engine classifies a shot differently than `shot_scene` does (it warns and honours this).
    let shot_seam_drop: Vec<u32> = (0..prompts.len())
        .map(|i| shot_render[i] - shot_visible[i])
        .collect();

    // Driving audio → the engine, ENGINE-OWNED. Hand the engine the WHOLE clip once and let IT
    // slice each segment's drive window (anchored to the true accumulated timeline) and mux the
    // deliverable at the actual stitched length. koblem deliberately does NOT pre-slice: a
    // client-side offset grid cannot track what the engine really stitched, and being wrong there
    // desyncs every downstream seam.
    //
    // The window a segment is driven by is cut BEFORE that segment renders, so it is cut against
    // the seam trim we PINNED (`cont_seam_drop_frames` above) — which the engine then applies
    // verbatim. That equality is what makes picture and track frame-exact at each seam rather than
    // merely close: a trim chosen from the rendered pixels could only ever be measured after the
    // audio that drove them was already baked in.
    //
    // `drive` conditions lip-sync (16k mono — all the audio VAE takes); `track` is muxed verbatim as
    // the deliverable (48k stereo). Either may be absent. When `track` is present the engine owns the
    // mux and koblem's post-mux below is skipped; if the track transcode fails we fall back to it.
    let mut chain_audio = crate::ltx_video::ChainAudio::default();
    if let Some(v) = &voice_track {
        match transcode_wav(v, true).await {
            Ok(w) => chain_audio.drive = Some(w),
            Err(e) => warn!(error = %e, "ltx: drive transcode failed; rendering without lip-sync"),
        }
    }
    if let Some(a) = &final_audio {
        match transcode_wav(a, false).await {
            Ok(w) => chain_audio.track = Some(w),
            // Fall back to koblem's post-mux rather than shipping the model's conditioning audio.
            Err(e) => {
                warn!(error = %e, "ltx: track transcode failed; falling back to koblem post-mux")
            }
        }
    }
    // PER-SHOT audio, overlaid on that bed by the engine. Indexed by FILTERED segment index —
    // `seg_specs` is already the post-empty-prompt list, so this is the engine's own numbering.
    //
    // koblem transcodes every clip through the SAME `transcode_wav`, which is what satisfies the
    // engine's no-resampling rule (per-shot track clips must match the bed's rate/channel layout):
    // bed and shots alike come out 48k stereo, so they cannot mismatch. A `voice` shot drives AND
    // delivers (same bytes, two formats); a `bgm` shot delivers only.
    //
    // Only the shot's OWN span is affected, and the engine cuts at the shot boundary, so a clip
    // longer than its shot can't bleed into the next one. Per-shot audio implies a declared trim,
    // which `segment_seam_drop_frames` (emitted below) already supplies.
    let has_shot_audio = seg_specs.iter().any(|s| s.audio_bytes.is_some());
    if has_shot_audio {
        chain_audio.shot_drive = vec![None; seg_specs.len()];
        chain_audio.shot_track = vec![None; seg_specs.len()];
        for (i, spec) in seg_specs.iter().enumerate() {
            let Some(bytes) = spec.audio_bytes.as_ref() else {
                continue;
            };
            match transcode_wav(bytes, false).await {
                Ok(w) => chain_audio.shot_track[i] = Some(w),
                Err(e) => {
                    warn!(error = %e, shot = i + 1, "ltx: shot track transcode failed; shot keeps the chain bed")
                }
            }
            if spec.audio_kind == LtxShotAudioKind::Voice {
                match transcode_wav(bytes, true).await {
                    Ok(w) => chain_audio.shot_drive[i] = Some(w),
                    Err(e) => {
                        warn!(error = %e, shot = i + 1, "ltx: shot drive transcode failed; shot renders without lip-sync")
                    }
                }
            }
        }
    }

    // 2c. Resolve each v2v shot's source BEFORE building the chain (so a resolution failure fails
    //     the job cleanly rather than emitting a half-formed v2v segment). A clip WE rendered →
    //     its banked `seg_<i>.bin` latent path (no re-encode); a foreign upload → decoded
    //     `control_frames`. Non-v2v shots → `None`. Path resolution is confined to koblem's own
    //     artifact store (no arbitrary file reads).
    let vid_w = req.width.unwrap_or(1280);
    let vid_h = req.height.unwrap_or(704);
    let mut v2v_sources: Vec<Option<V2vResolved>> = Vec::with_capacity(seg_specs.len());
    for (i, spec) in seg_specs.iter().enumerate() {
        if spec.engine_v2v_mode().is_none() {
            v2v_sources.push(None);
            continue;
        }
        let shot = i + 1;
        let resolved = match spec.v2v_source.as_ref() {
            Some(LtxV2vSource::Upload) => {
                let Some(bytes) = spec.v2v_upload.as_ref() else {
                    return Err(format!("v2v shot {shot}: no uploaded source clip"));
                };
                match decode_clip_to_control_frames(bytes, vid_w, vid_h, &mut uploads).await {
                    Ok(frames) => V2vResolved::Control(frames),
                    Err(e) => return Err(format!("v2v shot {shot} source: {e}")),
                }
            }
            Some(LtxV2vSource::PriorRender {
                job_id: src_job,
                segment,
            }) => {
                // Reference the banked latent by the engine's OWN job id — the engine resolves it to
                // seg_<segment>.bin against its artifact roots (persist first, then FIFO). koblem
                // sends no path and touches no filesystem. `src_job` is PREFERABLY already the engine
                // job id (the editor stores the render's stable server-job dir, which for a Director
                // render lives in the persist store indefinitely). Fall back to translating a live
                // koblem job id via the store (a legacy ref, or an in-session retake before the source
                // job aged out). If neither yields an id, the engine 404s the ref and the job fails
                // with the engine's message — no local existence check is possible or needed.
                let engine_job_id = state
                    .ltx_jobs
                    .get(src_job)
                    .and_then(|j| j.server_job_id)
                    .unwrap_or_else(|| src_job.to_string());
                V2vResolved::Latent {
                    job_id: engine_job_id,
                    segment: *segment,
                }
            }
            None => {
                return Err(format!("v2v shot {shot}: mode is v2v but no source set"));
            }
        };
        v2v_sources.push(Some(resolved));
    }

    // 3. Build the server chain request.
    // Per-shot chain objects: prompt + optional scene image / v2v source. A shot i>=1 with an
    // image is a mid-chain scene cut (fresh i2v, no continuation from the prior shot); shot 0's
    // image is the opener, folded into the top-level `init_image` below (so the engine's seg-0
    // path is unchanged). Audio stays continuous across cuts (handled engine-side).
    // A shot's keyframes (frame-pinned images) win over a plain scene image — a keyframe shot
    // renders fresh via the engine keyframe branch. Each keyframe's `seconds` maps to a video
    // frame index within the shot, clamped to [0, segment_frames). A v2v shot instead carries a
    // `v2v_mode` + guide strength + its source (banked latent path or `control_frames`) and owns
    // its own content (no keyframes / scene image).
    let mut seg_objs: Vec<serde_json::Value> = Vec::with_capacity(prompts.len());
    // Whether any shot is v2v, and the chain-global guide strength — taken from the FIRST v2v shot.
    //
    // NO LONGER the value that decides each shot: verified 2026-08-02 against the deployed engine
    // (ef8e138), the per-segment `v2v_guide_strength` emitted below IS parsed and IS applied per
    // shot (routes_ltx.cpp segment parse -> chain.segment_v2v_strengths -> stable-diffusion.cpp,
    // where any value >= 0 overrides `strength` for that segment). This chain-global value is now
    // only the base/seg-0 default and the fallback for an engine predating that support.
    let mut any_v2v = false;
    let mut first_v2v_strength: Option<f64> = None;
    // WHICH identity mechanism this render's DiT implements — the sheet checkpoint's tagged
    // source-phase references, or Echo's untagged memory bank. It decides the prompt prefix, the
    // reference resize mode and the `tass_phase_scale` default, all of which are silent when wrong
    // (the render succeeds; identity just doesn't hold). Resolved once, here, from the same model
    // AND top-level adapter set the chain is about to be sent with — since `faceid-sheet` and
    // `msr-v2` stopped being checkpoints, the model name alone no longer determines the regime.
    let identity_regime =
        LtxIdentityRegime::for_render(req.model.as_deref(), effective_lora_paths(None, &req.loras));
    // Reference head-frame trim, resolved to the engine's `reference_head_trim` encoding
    // (-1 auto / 0 off / n frames). The project value rides at the request root and each shot
    // emits its own only where it differs, so the wire stays as quiet as the setting is uniform.
    //
    // 🔴 `auto` now resolves to OFF, and that is a statement about the checkpoint rather than about
    // the setting. Rendering a TASS reference verbatim into pixel frame 0 was measured on `echo-e50`
    // alone — a location plate made frame 0 the empty plate, a bare character sheet made it that
    // sheet's own source photo — and that variant went with the LTX-2.3 engine. Nothing deployed
    // leaks, and sending `-1` anyway would cut a good frame (and 1/fps of audio) off every bare t2v
    // shot carrying references, silently, on projects nobody has touched. An EXPLICIT value from
    // the caller is still honoured as-is, which is what covers a future checkpoint that does leak.
    let resolve_head_trim = |setting: Option<LtxFrameDrop>| -> i32 {
        match setting.unwrap_or_default() {
            LtxFrameDrop::Mode(LtxFrameDropMode::Auto) => 0,
            other => other.engine_reference_head_trim(),
        }
    };
    let chain_head_trim = resolve_head_trim(req.frame_drop);
    let seg_head_trim: Vec<i32> = seg_specs
        .iter()
        .map(|s| resolve_head_trim(s.frame_drop))
        .collect();
    for (i, p) in prompts.iter().enumerate() {
        // REGIME IS PROJECT-LEVEL, and must stay that way — a per-shot override here would be
        // worse than none. The engine has no per-segment override for the two things the regime
        // actually decides on the wire: `resize_mode` rides the TOP-LEVEL `character_refs` union
        // (per-segment copies are explicitly IGNORED — routes_ltx.cpp:730-734) and
        // `tass_phase_scale` is top-level only. So resolving the regime per shot can move the
        // prompt prefix and nothing else, which produces a shot whose prompt says `ref_t2v:` while
        // its references are still `match_target` at phase 0 — inconsistent in a way the uniform
        // project regime never was. That combination WAS briefly emitted; it is not any more.
        //
        // Safe in practice because the automatic routing never changes the regime AT ALL any more.
        // It used to move a prop-carrying shot onto the `msr-v2` CHECKPOINT — same Memory regime as
        // the Echo doses it moved from, so it happened to be safe. Now it attaches the MSR ADAPTER
        // on top of whatever the project runs, and MSR maps to Memory exactly as Echo does, so the
        // shot's regime is the project's by construction rather than by coincidence.
        //
        // A MANUAL cross-regime override is still reachable — from a pinned `model`, and now also
        // from a per-shot LoRA set that swaps the identity mechanism — and both are warned about
        // below rather than half-honoured.
        let refs = &seg_specs[i].character_refs;
        // Adapter trigger phrases, folded in per shot against the set THIS shot will actually run
        // with — the per-segment override when it has one, the project set otherwise, which is the
        // same resolution the engine does and the same `effective_lora_paths` the regime uses.
        // Per shot and not once at the top because a per-shot set is exactly the case where a
        // project-wide phrase would be wrong: it would put `zhuanchang` on shots that dropped the
        // transition adapter, and leave it off the one shot that added it.
        //
        // AFTER `ltx_character_prompt`, never before: the trigger appends, and the `ref_t2v: `
        // prefix that function may add has to stay the first thing the encoder sees.
        let prompt = ltx_apply_lora_triggers(
            &ltx_character_prompt(p, refs, identity_regime),
            effective_lora_paths(Some(seg_specs[i].loras.as_slice()), &req.loras),
        );
        let mut o = json!({ "prompt": prompt });
        // Deliberately NOT emitting per-segment `character_refs`. The engine ignores them and the
        // top-level union is authoritative; duplicating every reference's base64 into every segment
        // object cost up to ~100 MB of dead payload on a 20-shot project with two 1536x1024 sheets,
        // against koblem's 256 MB cap and the engine's 512 MB. The union exists precisely to avoid
        // that — see `chain_character_refs`.
        // Either axis can move a shot's regime: a pinned `model`, or a per-shot LoRA set that
        // replaces the project's identity adapter with the other mechanism's. Resolve the shot the
        // same way the chain was resolved and compare, so both are caught by one check — keying this
        // on `model` alone would have gone quiet the moment the mechanisms became adapters.
        let seg_model = seg_specs[i].model.as_deref().or(req.model.as_deref());
        let seg_regime = LtxIdentityRegime::for_render(
            seg_model,
            effective_lora_paths(Some(seg_specs[i].loras.as_slice()), &req.loras),
        );
        if !refs.is_empty() && seg_regime != identity_regime {
            warn!(
                shot = i,
                shot_model = %seg_model.unwrap_or("base"),
                shot_regime = ?seg_regime,
                project_model = %req.model.as_deref().unwrap_or("base"),
                project_regime = ?identity_regime,
                "shot resolves to a different identity regime than the project (pinned model or a \
                 per-shot LoRA set that swaps the identity mechanism); its references keep the \
                 PROJECT regime's resize_mode and tass_phase_scale because the engine has no \
                 per-segment override for either, so identity may not hold on this shot"
            );
        }
        if let Some(model) = &seg_specs[i].model {
            o["model"] = json!(model);
        }
        // Per-shot runtime LoRAs. Emitted alongside the per-shot `model` and switched at the
        // same place — the engine's `before_segment` chain hook. Re-applying adapters costs
        // ~1s, far less than the DiT swap next to it, so this is the cheap way to vary
        // an effect's STRENGTH across a timeline.
        //
        // A non-empty list REPLACES the top-level `lora` for this shot rather than adding to
        // it, so a shot can also drop an inherited adapter. Empty ⇒ inherit.
        //
        // FORWARD-COMPATIBLE: an engine that parses `lora` only at the request root ignores
        // this silently and renders every shot with the top-level set — it does NOT error. Do
        // not assume it took effect without checking the engine revision.
        if !seg_specs[i].loras.is_empty() {
            o["lora"] = json!(
                seg_specs[i]
                    .loras
                    .iter()
                    .map(|l| json!({ "path": l.path, "multiplier": l.multiplier }))
                    .collect::<Vec<Value>>()
            );
        }
        // Director variable-length: this shot's own frame count (rounded to a VAE-valid
        // 8k+1 from its seconds). Absent ⇒ the engine uses the uniform `segment_frames`.
        // Render count = requested visible + seam overlap for continuation shots, so the engine's
        // trim yields the requested duration (a 4s shot produces 4s). Uniform per-shot lengths now,
        // so always emitted (scene shots are unpadded = shot_visible).
        o["frames"] = json!(shot_render[i]);

        // Prompt Relay beats. Authored in VISIBLE clip time (what the viewer sees); the engine
        // wants a frame on the RENDER timeline, so add exactly this shot's seam drop — which is
        // the same number of head frames the engine will trim. A beat at t=0s therefore lands on
        // the first frame the viewer actually sees rather than inside the discarded overlap.
        if !seg_specs[i].beats.is_empty() {
            let beats: Vec<serde_json::Value> = seg_specs[i]
                .beats
                .iter()
                .map(|b| {
                    // Beats are sent on the shot's VISIBLE timeline and the ENGINE adds that
                    // shot's seam trim itself. We used to add `shot_seam_drop[i]` here, which only
                    // worked while the trim was a number koblem could predict; with the engine
                    // measuring each seam adaptively it no longer is. Sending seconds keeps this
                    // independent of fps and of the trim entirely.
                    // NB: engine >= the seam-autotrim build. Older engines do NOT shift beats, so
                    // koblem and the engine must ship together or beats land inside the overlap.
                    let visible = (b.seconds * fps as f64).round() as i64;
                    let mut o = json!({
                        "frame": visible.clamp(0, shot_visible[i] as i64 - 1),
                        "text": b.text.trim(),
                    });
                    if let Some(s) = b.strength {
                        o["strength"] = json!(s);
                    }
                    if let Some(w) = b.window {
                        o["window"] = json!(w);
                    }
                    o
                })
                .collect();
            o["beats"] = json!(beats);
        }
        // Per-shot sampling overrides. Emitted only when the shot sets one, so an ordinary render
        // produces byte-identical engine JSON.
        if let Some(seed) = seg_specs[i].seed.filter(|s| *s >= 0) {
            o["seed"] = json!(seed);
        }
        if let Some(steps) = seg_specs[i].steps.filter(|s| *s > 0) {
            o["steps"] = json!(steps);
        }
        if let Some(cfg) = seg_specs[i].cfg.filter(|c| *c >= 0.0) {
            o["cfg"] = json!(cfg);
        }
        if let Some(negative) = &seg_specs[i].negative_prompt {
            o["negative_prompt"] = json!(negative);
        }
        // Reference head-frame trim, per shot. Emitted only where the shot DIFFERS from the
        // project default (which rides at the request root below), so an untouched project keeps
        // producing the same engine JSON on every segment object. The engine treats a supplied
        // per-segment value as authoritative including a zero, which is how "off for this one
        // shot" survives a project that has the trim on.
        if seg_head_trim[i] != chain_head_trim {
            o["reference_head_trim"] = json!(seg_head_trim[i]);
        }

        // v2v shot: emit the mode + source, and skip the keyframe / scene-image branch.
        if let Some(resolved) = v2v_sources.get_mut(i).and_then(Option::take) {
            let spec = &seg_specs[i];
            let mode = spec.engine_v2v_mode().unwrap_or(2);
            o["v2v_mode"] = json!(mode);
            if let Some(s) = spec.v2v_strength {
                // Per-segment strength. LIVE, not aspirational: ef8e138 parses this and applies it
                // to this shot alone (any value >= 0 wins over the chain-global `strength`).
                o["v2v_guide_strength"] = json!(s);
            }
            match resolved {
                V2vResolved::Latent { job_id, segment } => {
                    // Latent-in guide-edit source (a clip we rendered): reference it by the engine's
                    // job id + segment; the engine resolves the banked seg_<segment>.bin itself.
                    o["v2v_source_job_id"] = json!(job_id);
                    o["v2v_source_segment"] = json!(segment);
                }
                V2vResolved::Control(frames) => {
                    // Pixel source (foreign clip): the engine VAE-encodes these.
                    o["control_frames"] = json!(frames);
                }
            }
            if !any_v2v {
                any_v2v = true;
                first_v2v_strength = spec.v2v_strength;
            }
            seg_objs.push(o);
            continue;
        }

        let kfs = seg_specs
            .get(i)
            .map(|s| &s.keyframes)
            .filter(|k| !k.is_empty());
        if let Some(kfs) = kfs {
            let pins: Vec<serde_json::Value> = kfs
                .iter()
                .map(|kf| {
                    // Pin position on the produced (visible) timeline, clamped to THIS shot's
                    // visible length. A continuation shot renders `overlap` extra head frames the
                    // engine trims, so shift the pin onto the RENDER timeline for those.
                    let frame_visible = ((kf.seconds * fps as f64).round() as i64)
                        .clamp(0, shot_visible[i] as i64 - 1);
                    let frame = frame_visible + if shot_scene[i] { 0 } else { overlap as i64 };
                    json!({ "image": kf.image_b64, "frame": frame })
                })
                .collect();
            o["keyframes"] = json!(pins);
        } else if i >= 1
            && let Some(Some(marker)) = seg_image_markers.get(i)
        {
            // Scene-cut image only when there are no keyframes for this shot.
            o["init_image"] = json!(marker);
        } else if i >= 1 && seg_specs[i].scene_cut {
            // Prompt-only hard cut: render this shot FRESH (no init image / no continuation from
            // the prior shot). Mutually exclusive with the image/keyframe branches above (those
            // shots are already fresh). Only emitted when true → absent otherwise → byte-identical
            // engine JSON for ordinary continuation shots.
            o["scene_cut"] = json!(true);
        }
        // Per-shot image-pin hold strength — ONE emission site, deliberately outside the branches
        // above so it covers every shot SHAPE those branches distinguish.
        //
        // ⚠️ NOT literally every shot: a v2v shot `continue`s further up and never reaches here, so
        // it emits no `pin_strength`. That is harmless today (a v2v window's `strength` IS its
        // SDEdit schedule, set by `v2v_guide_strength`, and the engine gates the pin strength on
        // `!has_v2v_source` for exactly that reason) — but it is not what "every shot" would mean
        // to someone reading this and adding a pin to the v2v path later.
        //
        // It used to live inside the keyframe and scene-image branches, which meant SHOT 0 could
        // never carry one: shot 0's image is the top-level opener, so its segment object takes
        // neither branch. Its pin strength was therefore only reachable through the chain-global
        // `strength` — and that global was sourced from the first shot that set ANY value, so
        // softening shot 3's pin softened shot 0's opener too. That is the bug this fixes, and
        // fixing it in the engine alone would not have been enough, because koblem was never
        // sending shot 0's value in the first place.
        //
        // Emitted whenever the shot states one, for any shot shape. A shot with nothing pinned
        // carries it harmlessly (the engine only reads `strength` on the pin and SDEdit paths,
        // and a V2V shot's `v2v_guide_strength` wins), which is cheaper than teaching this site
        // the pin taxonomy a second time and getting the branches subtly wrong again.
        if let Some(ps) = seg_specs[i].pin_strength {
            o["pin_strength"] = json!(ps);
        }
        // Per-shot take selection. Absent unless the request RESUMES — a retake asked for a
        // specific take, and since `restore_selected_takes` was extended to the continue path, so
        // did every continue. Only a render that starts a fresh chain emits none of these.
        if let Some(bank) = &seg_specs[i].bank_job_id {
            o["bank_job_id"] = json!(bank);
        }
        seg_objs.push(o);
    }
    // Selected base DiT variant. Default "base"; a change triggers a leak-free unload+reload
    // below. Explicit value also swaps a gen back off the "edit"/relip model.
    let model = req.model.as_deref().unwrap_or("base").to_string();
    // Materialise the quality profile (Standard/High/4x/Custom) into concrete base + hires values.
    // Standard resolves to today's exact defaults, so a Standard render is byte-identical to before.
    let plan = resolve_quality_plan(&req);
    let mut chain = json!({
        "segments": seg_objs,
        "n_segments": n_segments,
        "model": model,
        // Engine BASE-pass resolution, resolved by the quality profile. Legacy callers: the
        // request dims, ×32-snapped. `dims_are_target` (Director): the target output scaled down
        // by the profile's upscale factor so the multi-stage chain lands ON the target, not past
        // it. Already ×32-snapped in `resolve_quality_plan` (guards off-grid direct-API callers).
        "width": plan.base_width,
        "height": plan.base_height,
        "fps": fps,
        "segment_frames": segment_frames,
        "cont_latent_frames": cont_latent_frames,
        // We still PAD each continuation's render by `overlap` (see `ltx_chain_seam_drop`), but we
        // no longer dictate how much comes back off. `8 * K` is only ever an ESTIMATE of how much
        // of a continuation repeats the prior shot; the true figure varies per join — measured 8
        // and 14 on two seams of a single clip — and pinning it too long discards freshly rendered
        // frames, which is the picture JUMPING FORWARD at every cut.
        //
        // So: no `cont_seam_drop_frames`, and `-1` per continuation shot = "engine, measure it".
        // Fresh shots keep an explicit 0, because that classification IS ours to own: a fresh shot
        // must never be trimmed or its new scene loses its opening frames and the whole timeline
        // slides. That was the real value of this array, and it is preserved.
        //
        // Trade-off accepted: a shot's produced length is no longer guaranteed == `shot_visible`
        // (the engine may cut a few frames either side of our padding), so downstream timing must
        // read actual durations rather than assume the requested ones.
        "segment_seam_drop_frames": shot_seam_drop
            .iter()
            .map(|&drop| if drop > 0 { -1i64 } else { 0i64 })
            .collect::<Vec<i64>>(),
        "steps": plan.base_steps,
        "cfg_scale": plan.base_cfg,
        "sampling_method": &plan.base_sample_method,
        "seed": req.seed.unwrap_or(-1),
        // 🔴 No `output_format`. It said "webm" for as long as this route existed, and it was the
        // C++ engine's field — LTX-2.5 ignores it and delivers h264-in-mp4 regardless, because
        // ComfyUI's only audio-carrying save node is `SaveVideo`, whose format enum is
        // `["auto", "mp4"]`. Sending it was a request the engine could not honour and a claim the
        // next reader would believe; `LtxJob::clip_ext` is what actually names the file.
        // Progressive per-segment delivery: have the engine write + expose each finished shot so
        // the poll loop can preview it live (before the stitched result). Byte-identical to a
        // non-emit render otherwise — the engine only produces the extra per-shot artifacts.
        "emit_segments": true,
    });
    // How the join carries the previous shot. Only emitted when the caller asks, so a request that
    // says nothing stays byte-identical and the engine's own default (`pixel`) stands.
    //
    // 🔴 `latent` was a hard job failure until the engine was fixed on 2026-08-15:
    // `apply_iclora_parameters` wired `iclora_parameters` into every guide and `LTXVAddLatentGuide`
    // has no such pin, so `POST /prompt` rejected the whole graph — i.e. every latent-carry render
    // that also loaded an identity adapter, which is all of ours. Do not send this to an engine
    // older than that fix.
    if let Some(mode) = req
        .cont_carry_mode
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        chain["cont_carry_mode"] = json!(mode);
    }
    // The seed the chain went out with: a Director TAKE is only comparable against its siblings if
    // you can tell which roll produced it, and the auto-retake loop reads it back to avoid
    // re-rolling the same number twice.
    let seed = req.seed.unwrap_or(-1);
    // Base pass: the resolved base sampler + its custom sigma schedule. The engine reads the base
    // sigmas from `sample_params` (independent of the refine `hires`/`hires_chain` block). Base
    // sampler mirrors the top-level `sampling_method` above.
    chain["sample_params"] = json!({
        "sample_method": &plan.base_sample_method,
        "custom_sigmas": &plan.base_sigmas,
    });
    if let Some(s) = &req.scheduler {
        chain["scheduler"] = json!(s);
    }
    // Project-level reference head-frame trim. `off` sends nothing at all (the engine's own
    // default is off), so a project that wants the historical behaviour produces the historical
    // request byte for byte.
    if chain_head_trim != 0 {
        chain["reference_head_trim"] = json!(chain_head_trim);
    }
    // Prompt Relay shape, chain-global and only sent when some shot actually has beats — an
    // ordinary render must not carry relay fields at all.
    if seg_specs.iter().any(|s| !s.beats.is_empty()) {
        if let Some(eps) = req.relay_eps.filter(|e| *e > 0.0) {
            chain["relay_eps"] = json!(eps);
        }
        if let Some(frac) = req.relay_steps_frac.filter(|f| *f > 0.0 && *f <= 1.0) {
            chain["relay_steps_frac"] = json!(frac);
        }
    }
    // Uncond conditioning. Standard carries only an explicit request negative (unchanged); High/4x
    // default the quality negative unless overridden. Only meaningfully active with cfg>1 or a
    // `*_cfg_pp` sampler — which the cfg_pp profiles use.
    if let Some(n) = &plan.negative_prompt {
        chain["negative_prompt"] = json!(n);
    }
    if let Some(marker) = &opener_marker {
        chain["init_image"] = json!(marker);
    }
    // PROJECT-LEVEL image-pin hold strength (keyframes / scene images / opener) — the value a shot
    // inherits when it does not state its own. 1.0 = the pinned image IS that frame (crispest);
    // lower lets it flex toward the motion. Only sent when the render actually pins an image, so a
    // plain t2v/continuation render leaves the engine default alone.
    //
    // THE FIRST SHOT'S VALUE NO LONGER LEAKS IN HERE. This used to be
    // `first_pin_strength.or(req.pin_strength)`, i.e. whatever the first shot with an opinion said
    // became the value for EVERY pinned image in the render — the per-shot slider was a
    // project-wide slider wearing a disguise, and shot 0 (whose image is the top-level opener, so
    // it emits no segment override) had no way to disagree with it. Now the global is only ever
    // the request's own default, each shot sends its own `segments[i].pin_strength`, and the
    // engine's `segment_pin_strengths[]` applies them per window.
    //
    // ⚠️ SHIP WITH THE ENGINE. Against an engine that predates `segment_pin_strengths[]` the
    // per-shot values are dropped and every shot renders at this default — which is correct for
    // an untouched project (1.0 either way) but silently ignores every slider the user moved.
    // koblem >= this commit needs longcat-avatar.cpp >= the per-shot pin-strength commit.
    let pins_any_image = opener_marker.is_some()
        || seg_specs
            .iter()
            .any(|s| !s.keyframes.is_empty() || !matches!(s.image, LtxImageSource::None));
    if pins_any_image {
        chain["strength"] = json!(req.pin_strength.unwrap_or(DEFAULT_PIN_STRENGTH));
    }
    // v2v: the chain-global mode + guide strength. `v2v_mode` is the base/seg-0 value, so it is
    // only non-zero when SHOT 0 itself is v2v — otherwise seg 0 would wrongly render as v2v; the
    // per-shot `segments[i].v2v_mode` covers every other shot. `v2v_guide_strength` is the base
    // default a shot inherits when it names none of its own — the engine-side
    // `segment_v2v_strengths[]` this comment used to be waiting on SHIPPED, and is live at
    // ef8e138, so a shot that sets its own no longer has to settle for this one. Only emitted
    // when a shot is actually v2v, so plain t2v/i2v/relip renders stay byte-identical.
    if any_v2v {
        let seg0_mode = seg_specs
            .first()
            .and_then(SegSpec::engine_v2v_mode)
            .unwrap_or(0);
        chain["v2v_mode"] = json!(seg0_mode);
        if let Some(s) = first_v2v_strength {
            chain["v2v_guide_strength"] = json!(s);
        }
    }
    // Identity conditioning, chain-level.
    //
    // The engine contract puts `character_refs` on the POST BODY: one array, and each entry carries
    // an optional `segments` SCOPE naming the segment indices it applies to (absent = every segment,
    // which is what every pre-scope request meant and still means). The per-segment copies emitted
    // above say the same thing per shot and are redundant with the scope — they stay because they
    // are the record of what koblem intended, and both are derived from this one selection, so they
    // cannot disagree.
    //
    // The array itself is the UNION over shots, keyed by `source_id`, in ascending tag order. Union,
    // not per-segment arrays, because the image bytes live here: a 1536×1024 sheet repeated into
    // twenty segment objects is tens of megabytes of base64 in one POST, whereas one entry plus a
    // list of indices is a few bytes per shot.
    //
    // Deduplication is by `source_id` precisely BECAUSE the tag is allocated once per character at
    // project level: the same character attached to eight shots is one entry, and two entries can
    // only ever be two different characters. Each entry keeps the shots that asked for it, which is
    // what becomes its scope.
    let mut chain_refs = chain_character_refs(&seg_specs);
    if !chain_refs.is_empty() {
        chain_refs.sort_by_key(|(r, _)| r.source_id.unwrap_or(u32::MAX));
        // Which character got which tag AND which shots it scopes to, once per render. Identity
        // breaking because a source_id moved — or because a character was silently conditioning
        // shots it isn't in — is invisible in the output (the face is just "wrong"), so it has to
        // be in the log.
        let mapping: Vec<String> = chain_refs
            .iter()
            .map(|(r, segments)| {
                format!(
                    "{}=>{}@{}",
                    r.name.as_deref().unwrap_or("(unnamed)"),
                    r.source_id
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "auto".into()),
                    if segments.len() == seg_specs.len() {
                        "all".to_string()
                    } else {
                        segments
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join("/")
                    }
                )
            })
            .collect();
        let shots_with_refs = seg_specs
            .iter()
            .filter(|s| !s.character_refs.is_empty())
            .count();
        // What these references cost as a SHARE of the attention sequence — the number that decides
        // whether they hold the face or capture the framing (39.5% captured, 11.8% clean, measured).
        //
        // PER SHOT, and the worst one wins. Cost is paid per segment — each shot's sequence carries
        // only the references scoped to it — so the union is the wrong number now that shots can
        // differ: six characters spread one per shot is cheap, and the one shot holding four is the
        // thing worth warning about. A short shot also dilutes less, so both terms are that shot's.
        let (share, worst_shot) = seg_specs
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.character_refs.is_empty())
            .map(|(i, s)| {
                let share = ltx_ref_token_share(
                    plan.base_width,
                    plan.base_height,
                    shot_render[i],
                    s.character_refs.len(),
                    identity_regime,
                );
                (share, i)
            })
            .max_by(|(a, _), (b, _)| a.total_cmp(b))
            .unwrap_or((0.0, 0));
        let worst_refs = seg_specs[worst_shot].character_refs.len();
        if share >= LTX_REF_SHARE_CAPTURE {
            warn!(
                share = share,
                shot = worst_shot,
                refs = worst_refs,
                frames = shot_render[worst_shot],
                width = plan.base_width,
                height = plan.base_height,
                "ltx: the busiest shot's identity references take {:.0}% of its sequence — at this \
                 share the reference CAPTURES the framing (measured at 39.5%). Render longer shots, \
                 render bigger, or switch characters off on the shots they are not in.",
                share * 100.0
            );
        } else if share >= LTX_REF_SHARE_WARN {
            warn!(
                share = share,
                shot = worst_shot,
                refs = worst_refs,
                frames = shot_render[worst_shot],
                "ltx: the busiest shot's identity references take {:.0}% of its sequence — above \
                 the measured-clean band (11.8%) and heading for framing capture",
                share * 100.0
            );
        }
        tracing::info!(
            characters = %mapping.join(", "),
            shots = shots_with_refs,
            total_shots = seg_specs.len(),
            regime = ?identity_regime,
            model = %model,
            worst_ref_token_share = share,
            "ltx: identity conditioning"
        );
        if !LtxIdentityRegime::identity_is_trained(
            req.model.as_deref(),
            effective_lora_paths(None, &req.loras),
        ) {
            warn!(
                model = %model,
                "ltx: identity references attached to a render with no identity training — the \
                 engine applies them but nothing loaded has a trained response. Attach an identity \
                 LoRA: LTX-2.3-Licon-MSR-V2.gguf (characters, locations AND props as separate \
                 entities) or faceid-sheet-rt.gguf (4-panel character sheets)."
            );
        }
        chain["character_refs"] = json!(
            chain_refs
                .iter()
                .map(|(r, segments)| {
                    let mut o = engine_character_ref(r, identity_regime);
                    // Scope, only when the reference is NOT on every shot. Absent means "every
                    // segment", which is what a pre-scope engine does with the array anyway — so a
                    // project where every character is in every shot emits the identical JSON it
                    // always did, and only a real per-shot selection adds the field.
                    if segments.len() < seg_specs.len() {
                        o["segments"] = json!(segments);
                    }
                    o
                })
                .collect::<Vec<Value>>()
        );
        // Only meaningful alongside refs. An explicit request value wins — INCLUDING 0, which is
        // not "unset": 0 is Echo's UNTAGGED overlap layout, and leaving the field off instead
        // leaves the engine on 1.0 (the tagged, sheet-checkpoint layout). That distinction is the
        // whole difference between the two regimes, so the regime supplies the default and only a
        // regime with nothing to say (`Sheet`) omits the field.
        if let Some(scale) = req
            .tass_phase_scale
            .filter(|s| *s >= 0.0)
            .or_else(|| identity_regime.default_tass_phase_scale())
        {
            chain["tass_phase_scale"] = json!(scale);
        }
    }
    // Runtime LoRAs. Since the six folded effect checkpoints were deleted this is the ONLY path
    // for every effect except the two Echo doses, identity mechanisms included.
    // The engine takes these as a top-level `lora` ARRAY — NOT as a `<lora:name:mult>` tag in the
    // prompt. The LTX route resolves the request with an empty lora_model_dir, so a prompt tag is
    // never parsed AND never stripped: it would reach the text encoder as literal text and the
    // render would silently come back with no LoRA applied. `path` must carry the file extension;
    // the engine matches it against a cache keyed on the name relative to its LoRA directory, and
    // an unresolvable name is a hard 400 rather than a silent skip.
    if !req.loras.is_empty() {
        chain["lora"] = json!(
            req.loras
                .iter()
                .map(|l| json!({ "path": l.path, "multiplier": l.multiplier }))
                .collect::<Vec<Value>>()
        );
    }
    if let Some(stages) = &plan.hires_chain {
        // Multi-stage upscale+refine chain (High / 4x / Custom): an ordered list of
        // upsample+SDEdit-refine stages. REPLACES the single `hires{}` block (per the contract, a
        // present, non-empty `hires_chain` takes precedence and the engine ignores `hires`).
        chain["hires_chain"] = json!(stages);
    } else if let Some(upscaler) = req.effective_upscaler_ratio().upscaler_name() {
        // Single-hires back-compat path (Standard / Custom-with-no-stages). Spatial latent upscaler
        // on the decoded output. steps + sigmas mirror AUTOMATION-PROMPT-ltx-upscale.md (shared
        // across the ×1.5 / ×2 ratios); the server resolves the upscaler by name from its
        // --hires-upscalers-dir. (Just {enabled:true} is a no-op — the server needs a named upscaler.)
        let refine_sigmas = req
            .refine_custom_sigmas
            .clone()
            .unwrap_or_else(|| LTX_VER3_REFINE_SIGMAS.to_vec());
        chain["hires"] = json!({
            "enabled": true,
            "upscaler": upscaler,
            "steps": req.refine_steps.unwrap_or(2),
            "custom_sigmas": refine_sigmas,
            "sample_method": req.refine_sampler.as_deref().unwrap_or("euler"),
        });
    }
    // Resume a prior render from its banked segments (the server skips done segments and
    // continues into the same artifact dir). Empty/absent = a fresh full render.
    let resuming = req.resume_job_id.as_deref().filter(|s| !s.is_empty());
    if let Some(rid) = resuming {
        chain["resume_job_id"] = json!(rid);
    }
    // Audio gap-fill: generate the silent stretches of the drive clip, hold the rest. Emitted only
    // when asked for, so an ordinary render's engine JSON is byte-identical to before.
    if req.audio_fill_gaps.unwrap_or(false) {
        chain["audio_fill_gaps"] = json!(true);
    }
    // Segment-level RETAKE: re-render ONE banked shot in place. Maps the RAW segment index to the
    // engine's FILTERED order (empty-prompt shots aren't rendered / banked), so it lines up with the
    // seg_<i>.bin files. Emitted only alongside `resume_job_id` — a retake needs a banked prefix.
    // The engine (>= `ltxav: single-segment retake-splice`) honours `retake_from` as an alias of
    // `retake_segment`: it reloads the banked prefix [0, k), re-renders ONLY shot k, then splices the
    // banked tail (k+1, N) back end-pinned to k's new final frame so the seam stays seamless. This
    // works for ANY index, not just the last shot. (For full downstream propagation instead of the
    // tail-splice, the engine would need a separate `render_from` mode — not requested here.)
    if resuming.is_some()
        && let Some(raw) = retake_from
    {
        let filtered = req
            .segments
            .iter()
            .take(raw as usize)
            .filter(|s| !s.prompt.trim().is_empty())
            .count() as u32;
        chain["retake_from"] = json!(filtered);
    }

    // Everything the finished chain still points at: koblem's own uploads (opener, scene images,
    // control frames) plus the staged assets the document kept. Assets staged for shots that did
    // not make it into this request are never uploaded (§4.3 — a retake resends nothing it does
    // not use).
    uploads.extend_from_document(&chain, staged);

    Ok(LtxCompiled {
        chain,
        audio: chain_audio,
        uploads,
        seed,
    })
}

// ── input resolution ──────────────────────────────────────────────────────

async fn resolve_image(
    source: &LtxImageSource,
    multipart_part: Option<Bytes>,
) -> Result<Option<Bytes>, String> {
    match source {
        LtxImageSource::None => Ok(None),
        LtxImageSource::Upload => match multipart_part {
            Some(b) => Ok(Some(b)),
            None => Err("image_source=upload needs an `image` part".to_string()),
        },
        LtxImageSource::Url { url } => {
            let resp = reqwest::get(url)
                .await
                .map_err(|e| format!("fetch image: {e}"))?;
            let b = resp.bytes().await.map_err(|e| format!("read image: {e}"))?;
            Ok(Some(b))
        }
        LtxImageSource::LibraryAssetId { .. } => {
            Err("library_asset_id not supported yet for ltx-video — upload or url".to_string())
        }
    }
}

// ── audio helpers (ffmpeg) ──────────────────────────────────────────────────

/// Decode a persisted inline clip — raw base64 or a `data:…;base64,` URI (what the editor stores).
/// `None` on anything that isn't valid base64, so a corrupt saved project renders without that
/// shot's audio rather than failing the whole job.
fn decode_data_uri_b64(s: &str) -> Option<Vec<u8>> {
    let payload = s
        .trim()
        .rsplit_once("base64,")
        .map_or(s.trim(), |(_, tail)| tail);
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()
}

/// Transcode an upload to RIFF/WAVE — the engine's only decoder (`load_wav_16k` /
/// `load_wav_full` in `src/longcat_audio.hpp`; it has no mp3/m4a/opus parser).
///
/// `drive16k` = the lip-sync DRIVE clip, resampled to the 16 kHz the audio VAE encoder consumes.
/// Otherwise 48 kHz — the DELIVERABLE, kept high-fidelity so the engine's Opus encode is the only
/// generation loss. Both are STEREO.
///
/// The drive clip used to be forced to `-ac 1`. That was wrong and it was the ONLY thing making
/// LTX audio mono: the LTX audio VAE is a two-channel model (`audio_channels == 2` in
/// `ltx_audio_vae.hpp`, asserted at load) and its `encode()` has always accepted a stereo
/// waveform — handed mono it just duplicates channel 0. So the whole path from here to the
/// deliverable was carrying one channel twice, which is why rendered LTX audio measured L/R
/// correlation 0.993 (i.e. mono) against source material at 0.43-0.50.
///
/// The engine caps this back to 1 under `LTX_DRIVE_AUDIO_CHANNELS=1`, which is how the two are
/// A/B'd on one deploy — so sending stereo here is safe even if stereo conditioning turns out
/// not to be worth it.
///
/// Rate conversion is ffmpeg's (swresample), which is properly anti-aliased — verified directly:
/// a 20 kHz tone at 48 kHz decimated to 16 kHz comes out at -240 dBFS rather than folding down to
/// 4 kHz. This matters because the engine's OWN resampler was, until recently, an unfiltered
/// two-tap linear interpolation; koblem pre-converting here is what kept that bug off every
/// production render.
///
/// Note there is no `-ss`/`-t` here: this is a FORMAT conversion, not a cut. That's the whole
/// architectural point — the timing decisions moved to the side that owns the timing.
async fn transcode_wav(audio: &Bytes, drive16k: bool) -> std::io::Result<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("ltx-wav-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await?;
    let src = dir.join("in");
    let out = dir.join("out.wav");
    tokio::fs::write(&src, audio).await?;
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.arg("-y").arg("-i").arg(&src);
    if drive16k {
        cmd.args(["-ac", "2", "-ar", "16000"]);
    } else {
        cmd.args(["-ac", "2", "-ar", "48000"]);
    }
    cmd.args(["-c:a", "pcm_s16le"]);
    let status = cmd.arg(&out).args(["-loglevel", "error"]).status().await?;
    if !status.success() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err(std::io::Error::other("ffmpeg wav transcode failed"));
    }
    let bytes = tokio::fs::read(&out).await?;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(bytes)
}

/// Replace the rendered clip's audio with the user's original upload (the full-quality
/// source), copying the video stream untouched and re-encoding only the audio to opus at a
/// near-transparent bitrate (webm carries opus, not the source codec). `-shortest` trims to
/// whichever of clip/audio is shorter so the track stays aligned with the rendered length.
///
/// Operates on the clip WHERE IT ALREADY IS and writes the result back over it, returning the new
/// size. ffmpeg reads and writes files either way, so the old signature — bytes in, bytes out —
/// meant a whole video through koblem's heap twice for nothing (§9.1).
/// Swap the clip's soundtrack for `audio`, in place.
///
/// `ext` is the clip's OWN container and is not cosmetic: ffmpeg picks the muxer from the output
/// suffix, and this used to hardcode `out.webm` while copying the video stream. That is fine for the
/// C++ engine's webm and is an immediate failure for LTX-2.5, which delivers h264-in-mp4 — h264 is
/// not a legal webm video stream, so the mux exits non-zero and the clip keeps the model's audio
/// with only a warning in the log. It never fired because the only caller was gated behind
/// "the engine did not mux", which an LTX chain with a track never is.
pub(crate) async fn replace_audio_in_place(
    clip: &std::path::Path,
    audio: &Bytes,
    ext: &str,
) -> std::io::Result<u64> {
    let dir = std::env::temp_dir().join(format!("ltx-mux-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await?;
    let ain = dir.join("audio");
    let out = dir.join(format!("out.{ext}"));
    tokio::fs::write(&ain, audio).await?;
    // Opus is not a well-supported mp4 audio stream; AAC is. Match the container.
    let codec = if matches!(ext, "mp4" | "m4v" | "mov") {
        "aac"
    } else {
        "libopus"
    };
    let status = ffmpeg_replace_audio(clip, &ain, &out, codec).await;
    if !matches!(&status, Ok(s) if s.success()) {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        // A spawn/IO failure carries its own error; a non-zero exit does not.
        status?;
        return Err(std::io::Error::other("ffmpeg audio replace failed"));
    }
    // ffmpeg cannot write over its own input, so the mux lands in the temp dir and is copied
    // back. One pass over the bytes, entirely inside the kernel.
    let size = tokio::fs::copy(&out, clip).await?;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(size)
}

async fn ffmpeg_replace_audio(
    vin: &std::path::Path,
    ain: &std::path::Path,
    out: &std::path::Path,
    acodec: &str,
) -> std::io::Result<std::process::ExitStatus> {
    tokio::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(vin)
        .arg("-i")
        .arg(ain)
        .args([
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "copy",
            "-c:a",
            acodec,
            "-b:a",
            "192k",
            "-shortest",
            "-loglevel",
            "error",
        ])
        .arg(out)
        .status()
        .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn log_ltx(
    state: &AppState,
    // The engine this render actually went to. Passed rather than reached for through `state`:
    // each client keeps its OWN wire meter and its own service tag, and reading ltx-video's while
    // LTX-2.5 rendered would file the row under the wrong engine with somebody else's byte counts.
    client: &crate::ltx_video::LtxVideoClient,
    caller: Option<String>,
    chain: &Value,
    gate_wait_ms: u64,
    render_ms: u64,
    status: u16,
    mut response_meta: Value,
) {
    use crate::repositories::gpu_logs::CreateLog;
    use crate::routes::tier2_helpers::log_async;
    // §9.2: what this render actually put on the wire, so "the body dropped from MB to KB" is a
    // number in a row rather than a claim. Read off the client's meter here rather than threaded
    // through every caller — each engine is single-flight behind the GPU gate.
    if let Some(obj) = response_meta.as_object_mut() {
        obj.insert("wire".to_string(), client.wire().as_meta());
    }
    log_async(
        state.db.clone(),
        CreateLog {
            service: client.service_tag().to_string(),
            endpoint: "generate".to_string(),
            request_meta: chain.clone(),
            response_meta,
            sample_id: None,
            gate_wait_ms: gate_wait_ms as i64,
            duration_ms: render_ms as i64,
            status: status as i32,
            caller,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::VideoJobState;
    use crate::routes::ltx_prompting::strip_list_marker;
    use crate::strict_json::test_bodies;

    /// The default shot must be renderable BY DEFAULT — including the worst case, a continuation
    /// shot that also pays the seam overlap. A default that trips koblem's own window guard would
    /// mean every fresh project fails its first render.
    #[test]
    fn the_default_shot_length_fits_the_temporal_window() {
        let overlap = super::ltx_chain_seam_drop(2);
        let render = super::DEFAULT_SEGMENT_FRAMES + overlap;
        assert!(
            render <= super::LTX_MAX_RENDER_FRAMES,
            "default {} + overlap {overlap} = {render} exceeds the {}-frame window",
            super::DEFAULT_SEGMENT_FRAMES,
            super::LTX_MAX_RENDER_FRAMES,
        );
        // 8k+1 or the VAE rejects it outright.
        assert_eq!((super::DEFAULT_SEGMENT_FRAMES - 1) % 8, 0);
        // 7s at the 24fps every UI defaults to.
        assert_eq!(
            super::frames_from_seconds(7.0, 24),
            super::DEFAULT_SEGMENT_FRAMES
        );
    }

    /// Exactly the window is fine; one frame over is not. The engine tiles rather than failing
    /// above it, so an off-by-one here is the difference between a render and a silent downgrade.
    #[test]
    fn the_window_guard_is_inclusive_at_the_limit() {
        assert!(super::shot_window_error(0, super::LTX_MAX_RENDER_FRAMES, 225, 24).is_none());
        assert!(super::shot_window_error(0, super::LTX_MAX_RENDER_FRAMES + 8, 233, 24).is_some());
    }

    /// The message has to be actionable by someone holding a seconds slider: which shot (1-based),
    /// and the longest duration that shot may actually be. A continuation shot's budget is the
    /// window MINUS its overlap, so the two cases must not report the same ceiling.
    #[test]
    fn the_window_message_states_the_shot_and_a_usable_ceiling() {
        // Continuation shot: 249 rendered = 233 visible + 16 overlap → 225 frames = 9.38s.
        let cont = super::shot_window_error(2, 249, 233, 24).expect("249 is over the window");
        assert!(cont.contains("shot 3"), "{cont}");
        assert!(cont.contains("16 frames of continuation overlap"), "{cont}");
        assert!(cont.contains("225 frames"), "{cont}");
        assert!(cont.contains("9.38s"), "{cont}");

        // Scene cut: no overlap, so the whole window is available — 241 frames = 10.04s.
        let cut = super::shot_window_error(0, 249, 249, 24).expect("249 is over the window");
        assert!(!cut.contains("continuation overlap"), "{cut}");
        assert!(cut.contains("241 frames"), "{cut}");
    }

    #[tokio::test]
    async fn a_foreign_engine_body_is_rejected_and_names_the_field() {
        let dir = test_bodies::no_assets_dir();
        let (ok, staged) = super::parse_generate_json(dir, &test_bodies::ltx())
            .await
            .expect("ltx-video's own body must still parse");
        assert_eq!(ok.segments.len(), 1);
        assert!(staged.is_empty());

        let mut foreign = serde_json::from_slice::<serde_json::Value>(&test_bodies::ltx()).unwrap();
        foreign["init_image"] = serde_json::json!("opener.png");
        let msg = super::parse_generate_json(dir, &serde_json::to_vec(&foreign).unwrap())
            .await
            .expect_err("an image-endpoint field must not render here");
        assert!(msg.contains("ltx-video /generate"), "{msg}");
        assert!(msg.contains("unknown field `init_image`"), "{msg}");
    }

    /// The Director re-parses a STORED timeline `request` document with plain
    /// `serde_json::from_value` (routes/ltx_director.rs), which must keep accepting a document an
    /// older build wrote. Only the HTTP edge is strict.
    #[test]
    fn a_stored_timeline_request_with_a_retired_key_still_replays() {
        let mut doc = serde_json::from_slice::<serde_json::Value>(&test_bodies::ltx()).unwrap();
        doc["retired_knob_from_an_older_build"] = serde_json::json!(true);
        serde_json::from_value::<super::LtxVideoGenerateRequest>(doc)
            .expect("the internal re-parse must stay lenient");
    }

    /// The `/jobs/{id}` body is a client contract (web poller, `kob` CLI, agent
    /// chat). Pin the exact JSON.
    fn ltx_job() -> crate::ltx_jobs::LtxJob {
        crate::ltx_jobs::LtxJob {
            state: VideoJobState::Failed,
            server_job_id: Some("srv-9".to_string()),
            director_critique: false,
            queue_job_id: None,
            segments: 2,
            scripts: vec!["a".to_string()],
            gate_wait_ms: 7,
            render_ms: 8,
            error: Some("boom".to_string()),
            content_type: Some("video/webm".to_string()),
            cancel_requested: false,
            partials: vec![koblem_client::types::VideoJobPartial {
                segment_index: 0,
                url: "/seg0".to_string(),
            }],
            delivery_warnings: Vec::new(),
            director_project_id: None,
            director_tier: None,
            engine: None,
            director_shot_offset: None,
            job_is_partial: false,
            renders_window: false,
            total_eta_seconds: None,
            progress: None,
            current_step: None,
            total_steps: None,
            phase: None,
            preview_seq: None,
            generation_index: None,
            generations_done: None,
            bank_keys: Vec::new(),
            seed: None,
            created: 1,
            updated: 2,
        }
    }

    #[test]
    fn job_status_body_json() {
        let json = serde_json::to_value(super::job_to_status("kob-1", &ltx_job(), true)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "job_id": "kob-1",
                "status": "failed",
                "created_at": "1970-01-01T00:00:01Z",
                "server_job_id": "srv-9",
                "resume_job_id": "srv-9",
                "resumable": true,
                "segments": 2,
                "scripts": ["a"],
                "gate_wait_ms": 7,
                "render_ms": 8,
                "error": "boom",
                "result_url": "/api/v1/ltx-video/jobs/kob-1/media",
                "partials": [{ "segment_index": 0, "url": "/seg0" }],
            })
        );
    }

    /// A plain LTX client adopts the newest unfinished render it is shown as the engine's own, so
    /// the Director renders sharing this store must be excludable — otherwise that client attaches
    /// to a render belonging to a project it knows nothing about.
    #[test]
    fn standalone_job_list_excludes_director_renders() {
        let mut director = ltx_job();
        director.director_project_id = Some("p1".to_string());
        director.director_tier = Some("proxy".to_string());
        let jobs = vec![
            ("kob-dir".to_string(), director),
            ("kob-plain".to_string(), ltx_job()),
        ];

        let ids = |standalone| {
            super::job_list(&jobs, standalone, super::BankLive(true))
                .into_iter()
                .map(|j| j.job_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(true), vec!["kob-plain".to_string()]);
        // Unfiltered stays the full store — the CLI and agent listings are unchanged.
        assert_eq!(
            ids(false),
            vec!["kob-dir".to_string(), "kob-plain".to_string()]
        );
    }

    /// Every frame `extract_frames` writes must be JPEG, and must be reachable through the same
    /// [`frame_path`](super::frame_path) both readers use.
    ///
    /// Both matter. The format is what keeps a relip window's `control_frames` array — one base64
    /// image PER OUTPUT FRAME, in a single JSON body — from being several times larger than it has
    /// to be. The shared path is what stops the writer and the two readers (relip windows, v2v
    /// control frames) drifting onto different extensions, which fails as "read frame 1: No such
    /// file" minutes into a committed render.
    #[tokio::test]
    async fn extracted_frames_are_jpeg_and_found_by_frame_path() {
        if tokio::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .is_err()
        {
            return;
        }
        let work = std::env::temp_dir().join(format!("koblem-frames-{}", uuid::Uuid::now_v7()));
        let out = work.join("frames");
        tokio::fs::create_dir_all(&out).await.expect("mkdir");
        let src = work.join("src.mp4");
        let ok = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=64x64:rate=24")
            .args(["-t", "0.25", "-pix_fmt", "yuv420p"])
            .arg(&src)
            .status()
            .await
            .expect("ffmpeg source")
            .success();
        assert!(ok, "could not synthesise a source clip");

        let n = super::extract_frames(&src, &out, 64, 64)
            .await
            .expect("extract");
        assert!(n > 0, "no frames extracted");

        for f in 1..=n {
            let bytes = tokio::fs::read(super::frame_path(&out, f))
                .await
                .unwrap_or_else(|e| panic!("frame {f} not at frame_path: {e}"));
            assert_eq!(
                &bytes[..3],
                &[0xFF, 0xD8, 0xFF],
                "frame {f} is not JPEG (PNG would start 89 50 4E)"
            );
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
    }

    /// One shot carrying the named characters, each as `(source_id, image)`.
    fn shot(refs: &[(u32, &str)]) -> super::SegSpec {
        super::SegSpec {
            character_refs: refs
                .iter()
                .map(|(sid, img)| super::LtxCharacterRef {
                    image_b64: (*img).to_string(),
                    source_id: Some(*sid),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn chain_refs_dedupe_by_tag_and_collect_their_shots() {
        // Mia (2) is in shots 0 and 2, Sol (3) only in shot 1 — the scope is what makes that
        // difference reach the engine at all.
        let specs = vec![
            shot(&[(2, "mia")]),
            shot(&[(3, "sol")]),
            shot(&[(2, "mia")]),
        ];
        let out = super::chain_character_refs(&specs);
        assert_eq!(out.len(), 2, "one entry per character, not per shot");
        assert_eq!(out[0].0.source_id, Some(2));
        assert_eq!(out[0].1, vec![0, 2]);
        assert_eq!(out[1].0.source_id, Some(3));
        assert_eq!(out[1].1, vec![1]);
    }

    #[test]
    fn a_character_on_every_shot_scopes_to_every_shot() {
        // This is the case that must emit NO `segments` field — the caller omits the scope when it
        // covers everything, which is what keeps a whole-cast project's JSON byte-identical to the
        // pre-scope request.
        let specs = vec![shot(&[(2, "mia")]), shot(&[(2, "mia")])];
        let out = super::chain_character_refs(&specs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.len(), specs.len());
    }

    #[test]
    fn two_characters_sharing_a_tag_are_not_merged() {
        // Distinct images under one tag is a corrupt roster, but merging them would silently pick
        // one face for both — keep them separate so the engine sees (and the log shows) the clash.
        let specs = vec![shot(&[(2, "mia")]), shot(&[(2, "someone-else")])];
        let out = super::chain_character_refs(&specs);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn no_refs_yields_no_entries() {
        let specs = vec![shot(&[]), shot(&[])];
        assert!(super::chain_character_refs(&specs).is_empty());
    }

    /// 🔴 An H3 draft must never be asked for "a full LTX shot prompt". The system prompt it rides
    /// with is pure H3 schema craft — fill in named fields, a composer assembles them — so a user
    /// turn naming the other engine's artefact describes something that does not exist, and the
    /// model resolves the contradiction by picking one half. The engine's name is the cheap
    /// tripwire for the whole class; the unit word is the substantive half.
    #[test]
    fn the_h3_user_turn_never_asks_for_an_ltx_shot_prompt() {
        for turn in [
            super::draft_user_turn(true, Some("make it rain"), Some("the draft"), "an idea"),
            super::draft_user_turn(true, Some("make it rain"), None, ""),
            super::draft_user_turn(true, None, Some("the draft"), ""),
            super::draft_user_turn(true, None, None, ""),
            super::draft_user_turn(true, None, None, "a cat on a wall"),
        ] {
            assert!(!turn.contains("LTX"), "{turn}");
            assert!(!turn.contains("shot prompt"), "{turn}");
            assert!(turn.contains("generation"), "{turn}");
        }
    }

    /// …and the LTX arm is byte-identical to what it always was. The H3 arm was added BESIDE these
    /// strings, not carved out of them: every existing LTX draft, rework and rewrite must produce
    /// the same request it produced before H3 existed.
    #[test]
    fn the_ltx_user_turn_is_unchanged() {
        assert_eq!(
            super::draft_user_turn(false, None, None, "a cat on a wall"),
            "Turn this idea into a full LTX shot prompt:\na cat on a wall"
        );
        assert_eq!(
            super::draft_user_turn(false, None, None, ""),
            "Write a full LTX shot prompt for this shot."
        );
        assert_eq!(
            super::draft_user_turn(false, Some("make it rain"), None, ""),
            "Write a full LTX shot prompt for this shot, following this direction: make it rain"
        );
        assert!(
            super::draft_user_turn(false, None, Some("the draft"), "")
                .starts_with("Rewrite this existing LTX shot prompt.")
        );
        // A rework with a base revises IN PLACE, and the base is the last thing in the turn.
        let rework = super::draft_user_turn(false, Some("make it rain"), Some("the draft"), "");
        assert!(
            rework.starts_with("Revise this LTX shot prompt"),
            "{rework}"
        );
        assert!(
            rework.ends_with("Existing shot prompt:\nthe draft"),
            "{rework}"
        );
    }

    /// The rework's base comes from `rewrite` when there is one and the idea otherwise — that
    /// fallback is what makes "apply this change" work on a shot that has only ever had an idea.
    #[test]
    fn a_rework_falls_back_to_the_idea_as_its_base() {
        let h3 = super::draft_user_turn(true, Some("make it rain"), None, "a cat on a wall");
        assert!(
            h3.contains("The generation as it stands:\na cat on a wall"),
            "{h3}"
        );
        let ltx = super::draft_user_turn(false, Some("make it rain"), None, "a cat on a wall");
        assert!(
            ltx.contains("Existing shot prompt:\na cat on a wall"),
            "{ltx}"
        );
    }

    #[test]
    fn strips_enumeration_markers() {
        assert_eq!(strip_list_marker("1. A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("12) A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("- A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("* A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("• A dog runs"), "A dog runs");
        assert_eq!(strip_list_marker("  2.  A dog runs"), "A dog runs");
    }

    #[test]
    fn keeps_natural_leading_numbers() {
        assert_eq!(strip_list_marker("3 dancers spin"), "3 dancers spin");
        assert_eq!(strip_list_marker("A dog runs"), "A dog runs");
    }

    #[test]
    fn round32_snaps_to_spatial_grid() {
        use super::round32;
        // Exact multiples pass through; off-grid dims snap to nearest 32; never below 32.
        assert_eq!(round32(1280), 1280);
        assert_eq!(round32(704), 704);
        assert_eq!(round32(1281), 1280);
        assert_eq!(round32(1296), 1312); // 1296 rounds up (nearest is 1312)
        assert_eq!(round32(700), 704);
        assert_eq!(round32(0), 32);
        assert_eq!(round32(1), 32);
    }

    #[test]
    fn base_from_target_divides_and_snaps() {
        use super::base_from_target;
        // ×1 (or less) → the target itself.
        assert_eq!(base_from_target(1280, 1.0), 1280);
        assert_eq!(base_from_target(1280, 0.5), 1280);
        // Clean powers of two off a ÷(32×factor) target.
        assert_eq!(base_from_target(1920, 2.0), 960); // 1080p Standard/High
        assert_eq!(base_from_target(1088, 2.0), 544);
        assert_eq!(base_from_target(1920, 4.0), 480); // 4× upscale
        assert_eq!(base_from_target(1024, 4.0), 256); // 1088 → 1152/1024 for 4× (÷128)
        assert_eq!(base_from_target(1152, 4.0), 288);
        // ×1.5 single-hires (1080p-hi): base ×32-snapped.
        assert_eq!(base_from_target(1920, 1.5), 1280);
        assert_eq!(base_from_target(1056, 1.5), 704);
    }

    fn plan_dims(
        profile: Option<super::LtxQualityProfile>,
        w: u32,
        h: u32,
        target: bool,
    ) -> (u32, u32) {
        let req = super::LtxVideoGenerateRequest {
            profile,
            width: Some(w),
            height: Some(h),
            dims_are_target: Some(target),
            ..Default::default()
        };
        let plan = super::resolve_quality_plan(&req);
        (plan.base_width, plan.base_height)
    }

    #[test]
    fn quality_plan_scales_base_to_target() {
        use super::LtxQualityProfile::*;
        // Legacy (dims_are_target=false): dims ARE the base pass, unchanged for every profile.
        assert_eq!(plan_dims(None, 960, 544, false), (960, 544));
        assert_eq!(plan_dims(Some(Upscale4x), 960, 544, false), (960, 544));
        // Target mode: Standard (default ×2 upscaler) and High both halve the target output.
        assert_eq!(plan_dims(None, 1920, 1088, true), (960, 544));
        assert_eq!(plan_dims(Some(High), 1920, 1088, true), (960, 544));
        // 4× starts from a quarter-size base. A valid 4× target is ÷128 on each side; 1088 is only
        // ÷64 so the Director snaps it to 1024 or 1152 (base 256 / 288). round32 keeps even an
        // unsnapped 1088 engine-valid (→ base 288, i.e. a 1152 output — a sensible 64px overshoot).
        assert_eq!(plan_dims(Some(Upscale4x), 1920, 1024, true), (480, 256));
        assert_eq!(plan_dims(Some(Upscale4x), 1920, 1152, true), (480, 288));
    }
}
