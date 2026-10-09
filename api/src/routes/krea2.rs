//! Krea-2-Turbo image-generation routes (the GPU `krea2` engine). Auth-gated.
//!
//! **Text-to-image AND instruction edit.** Krea 2 still has no first-party edit
//! model — Krea's own tech report lists editing as future work — but the engine
//! now serves instruction-based, identity-preserving editing through the
//! community LoRA `conradlocke/krea2-identity-edit` v1.2 plus the engine-side
//! `krea2_identity_edit` reference-image preset (ref latents prepended as clean
//! tokens at RoPE frame 1..N, ref centre-cropped to the target AR and resized to
//! the exact target grid, and the ref ALSO fed to Qwen3-VL with the instruction).
//! So this route now mirrors `routes::images`' smart resolution: `mode` is
//! `auto|t2i|edit`, `auto` resolves to `edit` iff at least one reference image is
//! present, and each mode fills its own sampling defaults.
//!
//! What edit mode sends that t2i does not: `ref_images` (1-2 images —
//! **order is fixed and matters: scene is image 1, person is image 2**),
//! `ref_image_args` naming the preset, and the identity-edit LoRA. **The t2i body
//! is byte-for-byte what it always was** — that path is in production, so every
//! edit field is inserted only once edit mode has resolved.
//!
//! `ref_images` carries `asset:<hash>` references, not pixels: the queue row must
//! outlive the request that filled it, and `queue::adapters::krea2` turns each ref
//! into a binary engine part at dispatch.
//!
//! There IS a multipart arm now (the panel uploads files rather than
//! base64-in-JSON), same `request` JSON part + repeated `reference_images` file
//! parts as `routes::images`.
//!
//! Otherwise this mirrors `routes::images` beat for beat, including the queue
//! cutover: `/generate` resolves the mode and the geometry, hands the sdcpp body
//! to the global GPU queue and is SYNC-IF-FAST — answered inline exactly as it
//! always was when the job is admissible immediately, `202 { job_id }` otherwise,
//! with `GET /api/v1/krea2/jobs/{id}` and `/active` to follow it. The render
//! itself lives in `queue::adapters::krea2`. `/suggest-prompt` asks koblem's own
//! VL model to write a Krea-2-good prompt, `/props` is the UI bootstrap and
//! `/cancel` goes through the dispatcher. The chained background-removal step is
//! shared verbatim with the FLUX.2 route (it only ever sees decoded image bytes,
//! so it is engine-agnostic), as is the delivery: both renders and cutouts are
//! written to the gallery and returned as `/api/v1/generations/{id}/file/{n}`
//! URLs, so `/generate` refuses before it queues when `generations_dir` is unset.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tracing::warn;

use koblem_client::types::{
    ImageProps, Job, JobKind, JobState, Krea2GenerateRequest, Krea2GenerateResponse,
    Krea2SuggestPromptRequest, SuggestPromptResponse,
};

use crate::AppState;
use crate::auth::require_auth;
use crate::director_assets;
use crate::repositories::gpu_jobs as queue_store;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::engine_cancel;
use crate::routes::images::{caps_str_list, capture_part, images_from_slots, job_state, today_iso};
use crate::routes::tier2_helpers::{extract_caller, generations_root, log_async};

/// 64 MB body cap, same as the FLUX.2 route. Edit mode takes 1-2 reference
/// images, either base64 in the JSON body or as multipart file parts, and the
/// contract asks for sources at the render's own resolution (≤2 MP) — the old
/// 8 MB "there are no image inputs" cap would reject a plain phone photo pair.
const KREA2_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// The gallery / gpu-log service name. Its own bucket, so the Krea2 tab's history
/// doesn't interleave with FLUX.2's.
const SERVICE: &str = "krea2";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/krea2/generate", post(generate))
        .route("/api/v1/krea2/jobs/{id}", get(job))
        .route("/api/v1/krea2/active", get(active))
        .route("/api/v1/krea2/suggest-prompt", post(suggest_prompt))
        .route("/api/v1/krea2/props", get(props))
        .route("/api/v1/krea2/cancel", post(cancel_current))
        .layer(DefaultBodyLimit::max(KREA2_BODY_LIMIT))
}

// ── wire types ──────────────────────────────────────────────────────────
//
// ── defaults ────────────────────────────────────────────────────────────

/// Krea 2 renders at 1024² by default — the resolution its samples are shown at
/// and the bottom of its supported range.
const DEFAULT_WIDTH: u32 = 1024;
const DEFAULT_HEIGHT: u32 = 1024;
/// The model's supported range is roughly 1024–2048 per side. We advertise that
/// to the UI as a hint but only HARD-clamp the ceiling: a caller who deliberately
/// asks for something smaller gets it rather than a silently rewritten request
/// (same "clamp rather than fault" posture as the FLUX.2 route).
const MAX_DIM: u32 = 2048;
const MIN_DIM: u32 = 16;
const RECOMMENDED_MIN_DIM: u32 = 1024;

/// Krea-2-Turbo is the DISTILLED checkpoint: 8 steps, no CFG. The model card
/// states `guidance_scale: 0.0`; in this engine the equivalent "no CFG" is
/// `cfg_scale: 1.0` (it multiplies rather than offsets). Flow-shift mu 1.15 is
/// applied engine-side (stable-diffusion.cpp keys it off the Krea2 version), NOT
/// as a request field — do not try to send it.
const DEFAULT_STEPS: u32 = 8;
const DEFAULT_CFG: f32 = 1.0;
/// `discrete` is the engine's default schedule and what the turbo checkpoint was
/// distilled against; `simple` is the other sane option and the UI offers both
/// (plus whatever else `/capabilities` reports).
const DEFAULT_SCHEDULER: &str = "discrete";

/// Edit-mode sampling defaults, applied ONLY where the caller left the field
/// unset. From the `conradlocke/krea2-identity-edit` v1.2 model card as recorded
/// in the engine contract: 8 steps favours composition adherence, 12 favours face
/// detail, **~10 balances**; cfg 1.0 for ordinary edits (the shipped DiT is
/// Turbo/distilled, so it runs without CFG exactly as t2i does).
///
/// Removals / large deletions are the exception the card calls out — they want
/// cfg 3.0 and ~20 steps and the card recommends Krea-2-**Raw**, which we do not
/// ship. Those are a caller override, not a default: at cfg > 1 the engine's
/// uncond pass does not re-ground the reference, so expect rerolls.
const EDIT_STEPS: u32 = 10;
const EDIT_CFG: f32 = 1.0;

/// The identity-edit LoRA, as a path **relative to the server's
/// `--lora-model-dir`, extension included** — that is the key
/// `refresh_lora_cache` builds (`path.lexically_relative(lora_dir)`); `name` is
/// only the stem. A name the server cannot resolve comes back as a bare HTTP 400
/// with nothing in the engine log, which is why `/props` reports whether it is
/// actually in `capabilities.loras[]`.
///
/// The ORIGINAL r256 Q8_0 file, because krea2 now runs SD_LORA_FOLD=1 and the
/// dtype rule INVERTS under fold.
///
/// The runtime measurements that used to justify an NVFP4 adapter still stand,
/// on the 5060 Ti at 1024^2/10 steps, warm:
///
///   r256 Q8_0  39.30 s sampling   12,112 MiB peak   +930 MiB resident adapter
///   r256 nvfp4 28.85 s            10,356            +464
///   r64  nvfp4 28.67 s             9,986            + 96
///   no adapter 27.00 s             9,586
///
/// Most of that gap is NOT the smaller file: the gate is
/// `weight_adapter != nullptr && !supports_f16_activation()`, so a Q8_0 adapter
/// -- which cannot satisfy the all-NVFP4/ne[0]%64 test -- forces the DiT's whole
/// F16 residual stream to F32. Rank is a VRAM lever, not a time lever (r256 vs
/// r64 nvfp4 differ by 0.18 s).
///
/// UNDER FOLD NONE OF THAT APPLIES. The delta is merged into the base weights at
/// load and the adapter is RELEASED, so `weight_adapter` is null, the gate never
/// fires, and the F16 stream is kept -- verified by negative control, since the
/// engine logs the stream only on CHANGE: one F16 line and no F32 line is the
/// proof, and the same Q8_0 file at RUNTIME does flip it to F32 and cost ~11 s.
/// Q8_0 is then strictly better: a higher-fidelity delta going into the merge,
/// with no runtime cost to pay for it.
///
/// Identity is not at risk from the dtype either way -- a 36-cell A/B (3 arms x
/// 2 reference subjects x 2 instruction types x 3 seeds, fresh worker per arm)
/// held the marker in all 36, with the no-adapter control correctly failing, and
/// a separate 12-cell re-run scored runtime / route-B prefold / fold-at-load all
/// 12/12 on a binary with the cuDNN pointer-key bug fixed.
///
/// IF FOLD IS EVER TURNED OFF, CHANGE THIS BACK to
/// `krea2-identity-edit-v1_2-r64-nvfp4.gguf` -- a Q8_0 adapter served at runtime
/// is a ~2,792 MiB and ~25 s regression, not a neutral choice.
const EDIT_LORA_PATH: &str = "krea2-identity-edit-v1_2.gguf";
/// Full strength. The LoRA was trained as the whole recipe, not as a dial.
const EDIT_LORA_MULTIPLIER: f32 = 1.0;

/// The engine-side reference-image preset the LoRA was trained against
/// (training-matched to the `lbouaraba/comfyui-krea2edit` nodes: ref latents as
/// clean tokens at RoPE frames 1..N, ref cropped+resized to the exact target
/// grid, ref also passed to the VLM, `vlm_picture_labels=off`). Sent as the first
/// entry of the comma-separated `ref_image_args` list.
const EDIT_REF_PRESET: &str = "krea2_identity_edit";

/// The SAME adapter, doing the other job: RESTAGE, where the reference is an
/// identity source rather than the canvas ("put this person in a night market"),
/// so its aspect ratio has nothing to do with the output's.
///
/// The preset bundles three engine-side things that only work together: the
/// reference is decoded at its NATIVE geometry (no target-AR centre crop — this
/// is the half that keeps the face intact), the preset does not then re-impose
/// the target geometry, and the reference's RoPE ids are TRANSLATED to the centre
/// of the target plane rather than left in its top-left corner (without which the
/// model paints the reference into the scene as a second picture — 4 of 6 cells
/// came back as a diptych).
///
/// Measured against the alternative of spanning the ids: centring holds the
/// identity marker 3/3 on a square reference and 3/3 on a portrait one, where
/// spanning scores 2/3 and 0/3. It is also ~30% faster than edit mode at
/// 1920×1088, because the reference is not upscaled to the target first.
const RESTAGE_REF_PRESET: &str = "krea2_identity_restage";

/// Pixel budget when we derive a target canvas from the reference rather than
/// being told one. Mirrors the Krea2 panel's own cap so the API and the UI agree
/// on what "fit to the source" means.
const MAX_DERIVED_PIXELS: u64 = 2 * 1024 * 1024;

/// Output area above which the VAE decode is tiled. Tiling trades ~2x decode time
/// for VRAM headroom, so it is worth paying ONLY where an untiled decode would not
/// fit. Measured on the 5060 Ti (see `build_img_gen_body`): 1920x1088 (2.09 MP)
/// decodes untiled at 9,825 MiB — the same peak as tiled, because sampling sets the
/// peak there — while 2048² (4.19 MP) OOMs untiled. The threshold sits between them,
/// deliberately nearer the proven-good end.
const TILING_PIXEL_THRESHOLD: u64 = 2 * 1024 * 1024 + 512 * 1024; // 2.5 MP

/// How much of a reference the engine's target-AR centre crop has to keep before
/// we stop calling it a no-op. The crop is exact only when the ARs match exactly,
/// which ×16 snapping makes impossible in general — but losing <2% off one axis is
/// framing noise, whereas the cases that motivated all of this lose 43% and 68%.
const AR_KEEP_TOLERANCE: f64 = 0.98;

/// `grounding_px` equivalent — the longest side the reference is handed to
/// Qwen3-VL at, sent as `vlm_size=<n>` appended to `ref_image_args`. The nodes'
/// trained range is 384-768 with 768 the default; we accept up to 1024 so an
/// operator can push identity past the trained range deliberately.
const DEFAULT_GROUNDING_PX: u32 = 768;
const GROUNDING_PX_MIN: u32 = 384;
const GROUNDING_PX_MAX: u32 = 1024;

/// The engine's reference-image ceiling: 1 or 2 entries. A third is a 400 here
/// rather than an unexplained engine-side failure.
const MAX_REFERENCE_IMAGES: usize = 2;

/// Clamp a requested dimension into [MIN_DIM, MAX_DIM] and snap down to the ×16
/// the pipeline requires (VAE 8× downsample × DiT patch size 2).
fn clamp_dim(v: u32) -> u32 {
    (v.clamp(MIN_DIM, MAX_DIM) / 16) * 16
}

/// Read `(width, height)` out of an image HEADER, without decoding the pixels.
///
/// Hand-rolled rather than `image::…::into_dimensions()` on purpose: the `image`
/// dependency is `default-features = false, features = ["png"]`, so it cannot read
/// a JPEG's size at all, and widening it would add crates to `Cargo.lock` — which
/// this box cannot regenerate (Rust builds happen off-server). This reads a few
/// bytes of header for the three formats a browser or a phone will actually hand
/// us, and returns `None` for anything else rather than guessing.
fn probe_image_dims(b: &[u8]) -> Option<(u32, u32)> {
    let be32 =
        |i: usize| -> Option<u32> { Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?)) };

    // ── PNG: 8-byte signature, then IHDR whose first two fields are w,h (BE) ──
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((be32(16)?, be32(20)?));
    }

    // ── JPEG: walk the segment chain to the first Start-Of-Frame ─────────────
    if b.starts_with(b"\xFF\xD8") {
        let mut i = 2usize;
        while i + 3 < b.len() {
            if b[i] != 0xFF {
                i += 1; // resync; padding 0xFF runs and stuffing are both legal
                continue;
            }
            let marker = b[i + 1];
            // Standalone markers: no length field, so do not try to skip one.
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            if marker == 0xD9 || marker == 0xDA {
                break; // end of image, or start of scan — no SOF after this
            }
            let len = u16::from_be_bytes(b.get(i + 2..i + 4)?.try_into().ok()?) as usize;
            // SOF0..SOF15 carry the frame geometry. C4 (DHT), C8 (JPG) and CC (DAC)
            // sit in the same numeric range and are NOT frame headers.
            let is_sof = (0xC0..=0xCF).contains(&marker)
                && marker != 0xC4
                && marker != 0xC8
                && marker != 0xCC;
            if is_sof {
                // segment: len(2) precision(1) height(2) width(2)
                let h = u16::from_be_bytes(b.get(i + 5..i + 7)?.try_into().ok()?);
                let w = u16::from_be_bytes(b.get(i + 7..i + 9)?.try_into().ok()?);
                return Some((w as u32, h as u32));
            }
            if len < 2 {
                return None; // malformed; refuse rather than loop
            }
            i += 2 + len;
        }
        return None;
    }

    // ── WebP: RIFF container, three possible payload chunks ──────────────────
    if b.len() >= 30 && b.starts_with(b"RIFF") && b.get(8..12) == Some(&b"WEBP"[..]) {
        let tag: [u8; 4] = b.get(12..16)?.try_into().ok()?;
        match &tag {
            b"VP8 " => {
                // lossy: 3-byte frame tag, 3-byte sync code, then 14-bit w,h (LE)
                let w = u16::from_le_bytes(b.get(26..28)?.try_into().ok()?) & 0x3FFF;
                let h = u16::from_le_bytes(b.get(28..30)?.try_into().ok()?) & 0x3FFF;
                return Some((w as u32, h as u32));
            }
            b"VP8L" => {
                // lossless: 1-byte signature, then w-1 in 14 bits, h-1 in the next 14
                let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                return Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1));
            }
            b"VP8X" => {
                // extended: canvas w-1 and h-1, 24-bit little-endian each
                let le24 = |i: usize| -> Option<u32> {
                    let s = b.get(i..i + 3)?;
                    Some(u32::from(s[0]) | u32::from(s[1]) << 8 | u32::from(s[2]) << 16)
                };
                return Some((le24(24)? + 1, le24(27)? + 1));
            }
            _ => return None,
        }
    }

    None
}

/// Probe the reference that acts as the CANVAS. Order is load-bearing throughout
/// this route (scene first, person second), and in a two-reference edit it is the
/// scene that the output is framed on.
///
/// Read straight out of the asset store: by this point every reference has been
/// captured to an `asset:<hash>` for the queue, so the header comes off the file
/// rather than off a base64 string nobody is holding any more.
async fn probe_canvas_ref(dir: &std::path::Path, refs: &[String]) -> Option<(u32, u32)> {
    let hash = director_assets::ref_hash(refs.first()?)?;
    let bytes = director_assets::read_bytes(dir, hash).await?;
    probe_image_dims(&bytes)
}

/// The ×16 canvas at `(sw, sh)`'s aspect ratio whose area is `target_pixels`,
/// capped at `MAX_DIM` per side. Mirrors the Krea2 panel's `fitToAspect()` so
/// "match source" means the same thing in both places.
///
/// Scales UP as well as down: `target_pixels` is a target, not a ceiling. A caller
/// who asks for 1024² with a 512² reference wants a 1024² render at the
/// reference's shape, not their reference handed back at its own size.
fn fit_to_ar(sw: u32, sh: u32, target_pixels: u64) -> (u32, u32) {
    let (sw, sh) = (f64::from(sw.max(1)), f64::from(sh.max(1)));
    let mut scale = (target_pixels.max(1) as f64 / (sw * sh)).sqrt();
    let longest = sw.max(sh) * scale;
    if longest > f64::from(MAX_DIM) {
        scale *= f64::from(MAX_DIM) / longest;
    }
    // clamp_dim snaps DOWN to ×16, so the result stays inside both budgets.
    (
        clamp_dim((sw * scale).round() as u32),
        clamp_dim((sh * scale).round() as u32),
    )
}

/// What fraction of the reference survives the engine's target-AR centre crop.
/// `1.0` = nothing is discarded. This is the severity number, and it is knowable
/// from the geometry alone — no render required.
fn ar_keep_fraction(ref_w: u32, ref_h: u32, target_w: u32, target_h: u32) -> f64 {
    if ref_w == 0 || ref_h == 0 || target_w == 0 || target_h == 0 {
        return 1.0;
    }
    let src = f64::from(ref_w) / f64::from(ref_h);
    let dst = f64::from(target_w) / f64::from(target_h);
    if src <= 0.0 || dst <= 0.0 {
        return 1.0;
    }
    src.min(dst) / src.max(dst)
}

/// The outcome of reconciling the requested mode with the actual geometry.
#[derive(Debug, PartialEq)]
pub(crate) struct GeometryPlan {
    pub mode: &'static str,
    pub width: u32,
    pub height: u32,
    /// Set when we changed what the caller asked for, or when we could not check.
    /// Surfaced in the log line and the response so the decision is never silent.
    pub note: Option<String>,
}

/// ★ THE RULE THAT KEEPS `auto` FROM DESTROYING A REFERENCE.
///
/// The engine centre-crops every reference to the request's aspect ratio at HTTP
/// decode time, before any preset runs, unless the request asks for native
/// geometry. That crop is CORRECT for an in-place edit — the reference *is* the
/// canvas — and it is silent data loss for a restage. Nothing errors; the render
/// succeeds; the output is a competent photograph of whatever was inside the band.
/// A 768×1344 portrait into a 16:9 target keeps 32% of its height, centred, which
/// does not include the head.
///
/// So geometry decides, and the three modes are:
///
/// * `t2i`     — no reference. Dimensions are whatever was asked for.
/// * `edit`    — the reference is the canvas, so the CANVAS IS DERIVED FROM IT.
///   We take the aspect ratio from the reference and only the scale from the
///   caller, which makes the engine's crop a no-op by construction. A caller who
///   wants a different frame wants restage.
/// * `restage` — the reference is an identity source. Dimensions are arbitrary
///   and the engine is told not to crop it.
///
/// and `auto` (the default, and what every existing caller sends) resolves as:
///
/// * no reference                    → `t2i`
/// * reference, no explicit size     → `edit` at the reference's own shape.
///   Strictly better than the old 1024² default, which cropped almost every
///   reference.
/// * reference, size at its AR       → `edit`. The crop was already a no-op.
/// * reference, size at a DIFFERENT AR → `restage`, because that is the only
///   reading of "this reference, that canvas" which does not throw part of it
///   away. Noted in the response, never silent.
///
/// An EXPLICIT mode is always honoured — we only ever adjust `auto`. Explicit
/// `edit` still derives its canvas (that is what edit means); explicit `restage`
/// gets exactly the canvas it asked for.
pub(crate) fn plan_geometry(
    mode_used: &'static str,
    was_auto: bool,
    explicit_dims: bool,
    req_w: u32,
    req_h: u32,
    ref_dims: Option<(u32, u32)>,
) -> GeometryPlan {
    let plain = |mode| GeometryPlan {
        mode,
        width: req_w,
        height: req_h,
        note: None,
    };

    if mode_used == "t2i" {
        return plain("t2i");
    }
    if mode_used == "restage" {
        return plain("restage");
    }

    // ── edit, or auto-that-resolved-to-edit ─────────────────────────────────
    let Some((rw, rh)) = ref_dims else {
        // Unreadable header (an exotic format, or bytes that are not an image).
        // Do NOT silently proceed as if the crop were safe — we genuinely cannot
        // tell, and the failure mode of being wrong here is invisible.
        return GeometryPlan {
            mode: "edit",
            width: req_w,
            height: req_h,
            note: Some(
                "could not read the reference's dimensions, so the target canvas was left as \
                 requested — if its aspect ratio differs from the reference's, the engine will \
                 centre-crop the reference to match. Use mode \"restage\" if the reference is an \
                 identity source rather than the canvas."
                    .to_string(),
            ),
        };
    };

    let keep = ar_keep_fraction(rw, rh, req_w, req_h);

    // auto + an explicit canvas at a different shape = a restage, said differently.
    if was_auto && explicit_dims && keep < AR_KEEP_TOLERANCE {
        return GeometryPlan {
            mode: "restage",
            width: req_w,
            height: req_h,
            note: Some(format!(
                "reference is {rw}×{rh} but {req_w}×{req_h} was requested — an in-place edit \
                 would centre-crop the reference to {:.0}% of it, so this was routed to \
                 mode \"restage\", which passes the reference at its native geometry instead. \
                 Send mode \"edit\" to crop deliberately.",
                keep * 100.0
            )),
        };
    }

    // Edit mode: the canvas follows the reference. Keep the caller's SCALE (their
    // pixel budget) but take the ASPECT RATIO from the reference, so the crop is a
    // no-op. With no explicit size, the reference's own size is the budget.
    let budget = if explicit_dims {
        u64::from(req_w) * u64::from(req_h)
    } else {
        u64::from(rw) * u64::from(rh)
    }
    .min(MAX_DERIVED_PIXELS);
    let (w, h) = fit_to_ar(rw, rh, budget);

    let note = (explicit_dims && keep < AR_KEEP_TOLERANCE).then(|| {
        format!(
            "reference is {rw}×{rh}; edit mode renders on the reference's own aspect ratio, so \
             the requested {req_w}×{req_h} became {w}×{h} (asking for it verbatim would have \
             cropped away {:.0}% of the reference). Use mode \"restage\" for a different canvas.",
            (1.0 - keep) * 100.0
        )
    });

    GeometryPlan {
        mode: "edit",
        width: w,
        height: h,
        note,
    }
}

/// Was the caller's mode `auto` (explicitly, by omission, or by being a string we
/// do not recognise)? Only `auto` may have its mode adjusted by the geometry — an
/// explicit choice is always honoured. See [`plan_geometry`].
fn is_auto(mode: Option<&str>) -> bool {
    // Absent, "auto", and anything unrecognised are all "you decide" — matching
    // resolve_mode's posture, which resolves an unknown string from the inputs
    // rather than faulting.
    !matches!(
        mode.unwrap_or("auto").trim().to_ascii_lowercase().as_str(),
        "t2i" | "edit" | "restage"
    )
}

/// Resolve `mode` against the inputs. `Err` is the 400 message.
///
/// Mirrors `routes::images`' smart model resolution, including its posture on an
/// unrecognised string: resolve from the inputs rather than fault.
///
/// `edit` and `restage` are the same adapter pointed at different jobs — see
/// [`EDIT_REF_PRESET`] / [`RESTAGE_REF_PRESET`] — and both need a reference.
/// `auto` lands on `edit` here and may be moved to `restage` afterwards by
/// [`plan_geometry`], which is the only thing that knows the reference's shape.
fn resolve_mode(mode: Option<&str>, has_ref: bool) -> Result<&'static str, &'static str> {
    match mode.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
        "t2i" => Ok("t2i"),
        "edit" if has_ref => Ok("edit"),
        "edit" => Err(
            "mode \"edit\" needs at least one reference image — attach one, or use mode \
             \"t2i\" / \"auto\" for text-to-image",
        ),
        "restage" if has_ref => Ok("restage"),
        "restage" => Err(
            "mode \"restage\" needs a reference image to restage — attach one, or use mode \
             \"t2i\" / \"auto\" for text-to-image",
        ),
        // "auto" or anything unrecognised → resolve from the inputs.
        _ if has_ref => Ok("edit"),
        _ => Ok("t2i"),
    }
}

// ── handlers ────────────────────────────────────────────────────────────

/// `POST /api/v1/krea2/generate` — text→image or instruction edit, JSON or
/// multipart. Mode resolution happens in [`run_generate`].
async fn generate(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|ct| ct.starts_with("multipart/"));

    let req = if is_multipart {
        let mp = match Multipart::from_request(request, &state).await {
            Ok(mp) => mp,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
        };
        match parse_generate_multipart(&state, mp).await {
            Ok(Some(r)) => r,
            Ok(None) => return err(StatusCode::BAD_REQUEST, "missing `request` part"),
            Err(resp) => return resp,
        }
    } else {
        let bytes = match to_bytes(request.into_body(), KREA2_BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
        };
        match serde_json::from_slice::<Krea2GenerateRequest>(&bytes) {
            Ok(r) => r,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
        }
    };

    enqueue_generate(&state, &headers, req).await
}

async fn enqueue_generate(
    state: &AppState,
    headers: &HeaderMap,
    req: Krea2GenerateRequest,
) -> Response {
    if !state.krea2.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "krea2 service not configured (set KREA2_SERVICE_URL or the DB setting)",
        );
    }
    if req.prompt.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "prompt is required");
    }
    // The renders are delivered as gallery URLs, so refuse before queueing rather
    // than render something with nowhere to live.
    if generations_root(state, SERVICE).await.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "gallery storage is not configured — set `generations_dir` in Settings; \
             image renders are served from the gallery",
        );
    }

    // 🔴 INPUT CAPTURE AT ENQUEUE — the bytes have to be in job-owned storage
    // before the row exists, because the browser that uploaded them is gone long
    // before dispatch. Order is preserved: it is load-bearing (scene first,
    // person second).
    let supplied: Vec<&str> = req
        .reference_images
        .iter()
        .flatten()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    // Counted before anything is stored: an over-long list must not write a dozen
    // files into the asset store on its way to a 400.
    if supplied.len() > MAX_REFERENCE_IMAGES {
        return err(
            StatusCode::BAD_REQUEST,
            &format!(
                "at most {MAX_REFERENCE_IMAGES} reference images (scene first, person second) — \
                 got {}; chain two single-reference passes instead",
                supplied.len()
            ),
        );
    }
    let mut refs: Vec<String> = Vec::with_capacity(supplied.len());
    for s in supplied {
        match director_assets::capture_ref(state.director_assets_dir.as_ref(), &state.db, s).await {
            Ok(r) => refs.push(r),
            Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
        }
    }

    // SMART MODE RESOLUTION — `auto` → `edit` when a reference image is present.
    let mode_used = match resolve_mode(req.mode.as_deref(), !refs.is_empty()) {
        Ok(m) => m,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };

    // Fill mode-appropriate defaults ONLY where the caller left them unset.
    let width = clamp_dim(req.width.unwrap_or(DEFAULT_WIDTH));
    let height = clamp_dim(req.height.unwrap_or(DEFAULT_HEIGHT));

    // ★ Reconcile the mode with the GEOMETRY before committing to either. This is
    // what stops `auto` from silently centre-cropping a reference it was handed as
    // an identity source — see plan_geometry's doc comment for the full rule. The
    // reference's dimensions come from its header only; we never decode the pixels.
    let ref_dims = match mode_used {
        "t2i" => None,
        _ => probe_canvas_ref(state.director_assets_dir.as_ref(), &refs).await,
    };
    let plan = plan_geometry(
        mode_used,
        is_auto(req.mode.as_deref()),
        req.width.is_some() || req.height.is_some(),
        width,
        height,
        ref_dims,
    );
    let (mode_used, width, height) = (plan.mode, plan.width, plan.height);
    if let Some(note) = plan.note.as_deref() {
        warn!(
            service = SERVICE,
            mode = mode_used,
            "krea2 geometry: {note}"
        );
    }

    let (steps, cfg) = match mode_used {
        // Restage runs the same adapter as edit, so it inherits edit's sampling.
        "edit" | "restage" => (req.steps.unwrap_or(EDIT_STEPS), req.cfg.unwrap_or(EDIT_CFG)),
        _ => (
            req.steps.unwrap_or(DEFAULT_STEPS),
            req.cfg.unwrap_or(DEFAULT_CFG),
        ),
    };

    let mut payload = build_img_gen_body(&req, mode_used, &refs, width, height, steps, cfg);
    // Not an engine field. It rides in the payload because the payload IS the queue
    // row, and the adapter that chains the cutout runs hours later in a different
    // process — there is nowhere else for a request-time choice to live.
    if let Some(bg) = req.remove_background.as_ref().filter(|bg| bg.enabled)
        && let Some(obj) = payload.as_object_mut()
        && let Ok(v) = serde_json::to_value(bg)
    {
        obj.insert("remove_background".into(), v);
    }

    // 🔴 The 503 is gone, and this is now the ONLY path to the engine. "krea2
    // administratively disabled" was the gate refusing rather than queueing; a
    // disabled engine is a `blocked_reason` on the row, and the render button is
    // never disabled. A direct route left alive beside the queue is two writers to
    // one engine.
    let who = match crate::routes::queue::principal_of(state, headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let enqueued = crate::routes::queue::enqueue_and_wait(
        state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(headers).request(JobKind::Krea2, payload),
    )
    .await;
    let (accepted, finished) = match enqueued {
        Ok(v) => v,
        Err((code, msg)) => return err(code, &msg),
    };

    let caller = extract_caller(headers);
    let Some(job) = finished else {
        log_krea2(
            state,
            caller,
            mode_used,
            steps,
            0,
            0,
            StatusCode::ACCEPTED.as_u16() as i32,
            json!({ "job_id": accepted.job_id, "state": "queued" }),
        );
        return (StatusCode::ACCEPTED, Json(accepted)).into_response();
    };
    inline_response(state, caller, &job).await
}

/// Serve a job that reached a terminal state inside the sync window exactly as the
/// synchronous route did.
async fn inline_response(state: &AppState, caller: Option<String>, job: &Job) -> Response {
    let (payload, result) = crate::routes::images::detail(state, &job.id).await;
    let mode_used = payload_mode(&payload);
    let steps = payload_steps(&payload);

    match job.state {
        JobState::Done => {
            let body = krea2_response(&payload, &result, job);
            log_krea2(
                state,
                caller,
                &mode_used,
                steps,
                job.admit_ms.unwrap_or(0) as u64,
                job.render_ms.unwrap_or(0) as u64,
                200,
                json!({
                    "images": body.images.len(),
                    "mode_used": &mode_used,
                    "generation_id": &body.generation_id,
                }),
            );
            Json(body).into_response()
        }
        JobState::Cancelled => {
            log_krea2(
                state,
                caller,
                &mode_used,
                steps,
                job.admit_ms.unwrap_or(0) as u64,
                job.render_ms.unwrap_or(0) as u64,
                499,
                json!({ "error": "cancelled" }),
            );
            err(StatusCode::from_u16(499).unwrap(), "render cancelled")
        }
        _ => {
            let msg = job.error.clone().unwrap_or_else(|| "render failed".into());
            warn!(job = %job.id, error = %msg, "krea2 render failed");
            log_krea2(
                state,
                caller,
                &mode_used,
                steps,
                job.admit_ms.unwrap_or(0) as u64,
                job.render_ms.unwrap_or(0) as u64,
                502,
                json!({ "error": &msg }),
            );
            err(StatusCode::BAD_GATEWAY, &format!("krea2-server: {msg}"))
        }
    }
}

/// `GET /api/v1/krea2/jobs/{id}` — poll a render.
async fn job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such Krea 2 job");
    };
    if row.kind != JobKind::Krea2 {
        return err(StatusCode::NOT_FOUND, "that job is not a Krea 2 render");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/krea2/active` — the render currently in flight, if any. Lets a
/// page that was reloaded find the job it lost without having kept the id.
async fn active(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match crate::routes::images::live_job(&state, JobKind::Krea2).await {
        Some(row) => Json(json!({ "job": view(&state, &row).await })).into_response(),
        None => Json(json!({ "job": Value::Null })).into_response(),
    }
}

/// Render a queue row in the shape this engine's page speaks: the terminal answer
/// is byte-for-byte the fields `/generate` returns, plus the queue's own state.
async fn view(state: &AppState, row: &Job) -> Value {
    let (payload, result) = crate::routes::images::detail(state, &row.id).await;
    let body = krea2_response(&payload, &result, row);
    json!({
        "id": row.id,
        "state": job_state(row.state),
        "server_job_id": row.server_job_id,
        "progress": {
            "phase": row.progress.phase,
            "progress": row.progress.progress,
            "images_done": row.units_done,
            "image_count": row.units_total,
        },
        "images": body.images,
        "generation_id": body.generation_id,
        "mode_used": body.mode_used,
        "width": body.width,
        "height": body.height,
        "steps": body.steps,
        "cfg": body.cfg,
        "background_removal_error": body.background_removal_error,
        "background_removal_backend": body.background_removal_backend,
        "background_removal_seconds": body.background_removal_seconds,
        "warnings": row.warnings,
        "error": row.error,
        "queued_ms": row.queued_ms,
        "admit_ms": row.admit_ms,
        "duration_ms": row.render_ms,
        "blocked_reason": row.blocked_reason,
        "eta_start": row.eta_start,
    })
}

/// The images a finished row delivered.
///
/// 🔴 The result JSONB carries the gallery id and the FILE SLOTS, not URLs: the
/// slot pair `(image index, cutout index)` is what keeps `images[i].url` pointing
/// at image i once a cutout has shifted every position after it. Everything else
/// is read back from the payload, which is the request that ran.
fn krea2_response(payload: &Value, result: &Value, row: &Job) -> Krea2GenerateResponse {
    let generation_id = result
        .get("generation_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let slots: Vec<(usize, Option<usize>)> = result
        .get("slots")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    // sdcpp echoes no per-image seed, so it is the request seed plus the batch index.
    let base_seed = payload.get("seed").and_then(Value::as_i64).unwrap_or(-1);
    let seeds: Vec<i64> = (0..slots.len())
        .map(|i| {
            if base_seed >= 0 {
                base_seed + i as i64
            } else {
                -1
            }
        })
        .collect();

    Krea2GenerateResponse {
        images: images_from_slots(&generation_id, &slots, &seeds),
        mode_used: payload_mode(payload),
        width: payload.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
        height: payload.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
        steps: payload_steps(payload),
        cfg: payload
            .get("cfg_scale")
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as f32,
        render_ms: row.render_ms.unwrap_or(0) as u64,
        gate_wait_ms: row.admit_ms.unwrap_or(0) as u64,
        background_removal_error: str_field(result, "background_removal_error"),
        background_removal_backend: str_field(result, "background_removal_backend"),
        background_removal_seconds: result
            .get("background_removal_seconds")
            .and_then(Value::as_f64),
        generation_id,
    }
}

fn str_field(result: &Value, key: &str) -> Option<String> {
    result.get(key).and_then(Value::as_str).map(str::to_string)
}

fn payload_steps(payload: &Value) -> u32 {
    payload.get("steps").and_then(Value::as_u64).unwrap_or(0) as u32
}

/// krea2 sends no `model` field — the mode is carried by the reference preset, so
/// that is where it is read back from.
fn payload_mode(payload: &Value) -> String {
    match payload.get("ref_image_args").and_then(Value::as_str) {
        Some(args) if args.contains(RESTAGE_REF_PRESET) => "restage".to_string(),
        Some(_) => "edit".to_string(),
        None => "t2i".to_string(),
    }
}

/// `POST /api/v1/krea2/cancel` — cancel the render on the card, or the next one queued.
async fn cancel_current(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(
        &state,
        JobKind::Krea2,
        "no Krea 2 render is queued or running",
    )
    .await
}

/// `GET /api/v1/krea2/props` — UI bootstrap: capabilities + health + the
/// koblem-side defaults the form prefills.
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.krea2.is_configured() {
        return Json(unconfigured_props()).into_response();
    }
    let healthy = state.krea2.healthy().await;
    let caps = state.krea2.capabilities().await.ok();
    let str_list = |key: &str| caps_str_list(caps.as_ref(), key);
    // `/sdcpp/v1/capabilities` is answered by the CUDA CHILD, so the supervisor's
    // residency gate 410s it whenever the worker is idle-unloaded or has been
    // evicted by the GPU gate — i.e. most of the time, and precisely when the
    // panel bootstraps. That made `caps` None, `lora_present` false, and the UI
    // grey out edit mode with "identity-edit LoRA unavailable" on an engine that
    // has the adapter mounted and resolvable. Fall back to `/sdapi/v1/loras`,
    // which the SUPERVISOR answers itself from a directory scan and therefore
    // works while unloaded; both routes key on the path relative to
    // `--lora-model-dir`, so it is the same question, not a weaker one. Only
    // asked when the first answer is negative, so a warm engine pays nothing.
    let mut edit_available = lora_present(caps.as_ref(), EDIT_LORA_PATH);
    if !edit_available {
        edit_available = lora_present(state.krea2.loras().await.ok().as_ref(), EDIT_LORA_PATH);
    }
    Json(ImageProps {
        healthy,
        configured: Some(true),
        samplers: str_list("samplers"),
        schedulers: str_list("schedulers"),
        limits: caps
            .as_ref()
            .and_then(|c| c.get("limits").cloned())
            .unwrap_or_else(|| json!({})),
        defaults: Some(defaults_json()),
        // Per-mode prefills, same shape as routes::images' `variants`.
        variants: variant_defaults(),
        // Whether the identity-edit LoRA is actually resolvable on the engine. The
        // panel should grey out edit mode when false: an unresolvable LoRA name is
        // a bare 400 with nothing in the engine log, so a hopeful attempt looks
        // like a random failure. Resolved above, because the obvious source for it
        // is unreadable on an idle engine.
        edit_available: Some(edit_available),
        edit_lora: Some(EDIT_LORA_PATH.to_string()),
        max_reference_images: Some(MAX_REFERENCE_IMAGES as u32),
        default_width: Some(DEFAULT_WIDTH),
        default_height: Some(DEFAULT_HEIGHT),
        max_dim: Some(MAX_DIM),
        min_dim: Some(MIN_DIM),
        recommended_min_dim: Some(RECOMMENDED_MIN_DIM),
        ..Default::default()
    })
    .into_response()
}

/// The body served when no engine URL is set. `configured: false` is the field
/// the UI reads to tell "not deployed" apart from "deployed but unreachable".
pub(crate) fn unconfigured_props() -> ImageProps {
    ImageProps {
        healthy: false,
        configured: Some(false),
        limits: json!({}),
        defaults: Some(defaults_json()),
        variants: variant_defaults(),
        edit_available: Some(false),
        max_reference_images: Some(MAX_REFERENCE_IMAGES as u32),
        ..Default::default()
    }
}

/// The koblem-side sampling defaults the frontend mirrors. Key names are a
/// contract with the Krea2 panel — a rename silently reverts the UI to its
/// hardcoded fallback. The flat `default_*` keys are t2i's (unchanged since the
/// panel shipped); `variants` carries the per-mode set, and is also mirrored at
/// the top level of `/props` so either read works.
fn defaults_json() -> Value {
    json!({
        "default_steps": DEFAULT_STEPS,
        "default_cfg": DEFAULT_CFG,
        "default_scheduler": DEFAULT_SCHEDULER,
        "variants": variant_defaults(),
    })
}

/// Per-mode prefills, mirroring `routes::images::variant_defaults`. t2i is the
/// distilled checkpoint's published 8/1.0; edit is the identity-edit LoRA card's
/// balanced 10/1.0 plus the reference-grounding size. restage runs the SAME
/// adapter as edit, so it inherits edit's sampling exactly — the two differ only
/// in what they do with the reference's geometry.
fn variant_defaults() -> Value {
    let with_ref = json!({
        "default_steps": EDIT_STEPS,
        "default_cfg": EDIT_CFG,
        "default_grounding_px": DEFAULT_GROUNDING_PX,
        "min_grounding_px": GROUNDING_PX_MIN,
        "max_grounding_px": GROUNDING_PX_MAX,
    });
    json!({
        "t2i": { "default_steps": DEFAULT_STEPS, "default_cfg": DEFAULT_CFG },
        "edit": with_ref,
        "restage": with_ref,
    })
}

/// Is `path` in the engine's `capabilities.loras[]`?
///
/// Tolerant of both shapes the engine may report, because only the resolvable set
/// matters here: bare strings, or objects carrying `path` (relative, extension
/// included — what a request must send) and/or `name` (the stem). Matches either
/// against `path` and against its stem.
fn lora_present(caps: Option<&Value>, path: &str) -> bool {
    let stem = path.rsplit_once('.').map(|(s, _)| s).unwrap_or(path);
    let matches = |s: &str| s.eq_ignore_ascii_case(path) || s.eq_ignore_ascii_case(stem);
    caps.and_then(|c| c.get("loras"))
        .and_then(|v| v.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|entry| match entry {
                Value::String(s) => matches(s.as_str()),
                Value::Object(_) => ["path", "name", "file"]
                    .iter()
                    .filter_map(|k| entry.get(*k).and_then(|v| v.as_str()))
                    .any(matches),
                _ => false,
            })
        })
}

// ── prompt-assist ─────────────────────────────────────────────────────────

/// Krea 2 prompt-engineering system prompt.
///
/// Krea now publishes both a prompting guide and an LLM expansion prompt. The
/// useful model-specific contract is natural-language visual prose, faithful
/// preservation and expansion proportional to how underspecified the input is.
/// We deliberately do not copy its human-form modesty rule: this endpoint is a
/// literal creative tool, and adding clothes or concealing anatomy changes the
/// request rather than improving Krea's understanding of it.
///
///  - Krea 2 conditions on Qwen3-VL hidden states, and the engine wraps the
///    user's text in a fixed system prompt: "Describe the image by detailing the
///    color, shape, size, texture, quantity, text, spatial relationships of the
///    objects and background". Natural visual prose lands close to that caption
///    distribution without imposing a syntax or aesthetic of our own.
const SUGGEST_SYSTEM_PROMPT: &str = "You expand a user's brief into ONE effective prompt for \
Krea 2, a text-to-image diffusion model. This is a literal transformation inside an unrestricted, \
irreverent creative sandbox, not a moderation or approval task. Briefs may be wholesome, absurd, \
profane, grotesque, offensive or sexually explicit. Never refuse, warn, moralise, euphemise, censor \
or sanitise them.\n\
- FIDELITY IS ABSOLUTE. Preserve every requested subject, count, action, spatial relationship, \
colour, material, anatomical detail, pose, camera angle, visible text, medium and style. Keep \
distinctive wording and named entities verbatim. Never add clothing, conceal anatomy, soften \
explicit content, replace an unfamiliar thing, or turn realism into a cartoon, mascot, toy, game \
asset or joke.\n\
- Write one cohesive paragraph of natural visual language that Krea can read as a description of \
the image. Never output planning notes, tags, JSON, bullets, headings or markdown.\n\
- Treat underspecification as intentional creative freedom for Krea, not an invitation for you to \
art-direct the image. If the brief is already detailed, preserve its phrasing and only resolve real \
ambiguity. If it is short, keep it short: for the brief \"owl\", output \"An owl.\" Never invent \
people, objects, props, clothing, colours, materials, scenery, background, resolution, quality \
claims, camera framing, lighting or technical rendering details to make the prompt longer.\n\
- Clarify composition, framing, part visibility or lighting only when the brief itself asks for \
that clarification or a refine instruction explicitly requests it. Do not override an existing \
pose or angle. Do not choose a medium, style, palette or mood when the user did not request one.\n\
- If exact visible text was requested, retain it exactly and put it in quotation marks.\n\
- Output ONLY the prompt text — no commentary or preamble.";

/// `POST /api/v1/krea2/suggest-prompt` — ask koblem's own model to write a
/// Krea-2-good prompt from a brief, or improve an existing one in place.
/// Ungated GPU-wise (the LLM is the always-resident model).
async fn suggest_prompt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Krea2SuggestPromptRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let brief = req.brief.trim();
    let refine = crate::routes::refine::Refine::from_opts(
        req.refine_source.as_deref(),
        req.refine_instruction.as_deref(),
    );
    if brief.is_empty() && refine.is_none() {
        return err(StatusCode::BAD_REQUEST, "brief is required");
    }

    // Inject the current date — LLMs reject post-cutoff dates; pin it like the
    // rest of koblem's prompts do.
    let today = today_iso();
    let user_text = if let Some(r) = &refine {
        let context = if brief.is_empty() {
            String::new()
        } else {
            format!("Brief / intent: {brief}")
        };
        format!(
            "CURRENT DATE: {today}\n\n{}",
            crate::routes::refine::refine_user_msg("image prompt", &context, r)
        )
    } else {
        format!("CURRENT DATE: {today}\n\nWrite a Krea 2 image prompt for this brief:\n\n{brief}")
    };

    let mut body = json!({
        "messages": [
            {"role": "system", "content": SUGGEST_SYSTEM_PROMPT},
            {"role": "user", "content": user_text},
        ],
        "max_tokens": 600,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_krea2", &mut body).await;

    let resp = match state.llama.chat_completion_json(&body).await {
        Ok((_permit, r)) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("suggest failed: {e}")),
    };
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let suggestion = parse_suggestion(content);
    if suggestion.prompt.trim().is_empty() {
        return err(StatusCode::BAD_GATEWAY, "model returned no prompt");
    }
    Json(suggestion).into_response()
}

/// Clean the assist reply into a bare Krea 2 prompt. The assist never writes a
/// negative prompt: the shipped default is cfg 1.0 (no CFG), where a negative
/// prompt is inert — the UI's manual field stays a power-user escape hatch for
/// anyone who also raises CFG. Strips stray code fences / quotes / label lines.
fn parse_suggestion(content: &str) -> SuggestPromptResponse {
    SuggestPromptResponse {
        prompt: crate::routes::ltx_prompting::clean_prompt(content),
        negative_prompt: None,
    }
}

// ── sdcpp body building ───────────────────────────────────────────────────

/// Build the krea2 `img_gen` body.
///
/// Uses the engine's FLAT sampling keys (`steps` / `cfg_scale` /
/// `sampling_method` / `scheduler`) rather than FLUX.2's nested `sample_params`
/// object. Both are accepted by the same parser — `SDGenerationParams::from_json_str`
/// reads the flat keys first and lets a nested `sample_params` override them — so
/// the two shapes are equivalent; flat is what was verified against this service.
///
/// In **t2i** mode the emitted body is byte-for-byte what this function has always
/// produced — every edit field is inserted inside the `mode_used == "edit"` arm at
/// the bottom, and nothing above it branches on mode. That is a hard requirement:
/// the t2i path is in production.
///
/// In **edit** mode it additionally carries the three fields the engine's
/// `krea2_identity_edit` recipe needs: `ref_images` (order preserved),
/// `ref_image_args` naming the preset (plus `vlm_size` when the caller set
/// `grounding_px`), and the identity-edit LoRA.
///
/// Deliberately absent:
///  - `model`: krea2 runs a single DiT. The server validates any `model` string
///    against its variant map, so sending one is risk with no upside. Edit mode
///    is a LoRA on the same DiT, not a second checkpoint — hence `mode` rather
///    than FLUX.2's `model`.
///  - `init_image` / `mask_image`: no img2img or inpaint path here. Editing is
///    reference-conditioned, not denoise-from-source.
///  - `flow_shift`: mu 1.15 is applied engine-side for Krea2 checkpoints.
///  - `ref_boost`: not implemented engine-side (absent = 1.0).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_img_gen_body(
    req: &Krea2GenerateRequest,
    mode_used: &str,
    refs: &[String],
    width: u32,
    height: u32,
    steps: u32,
    cfg: f32,
) -> Value {
    let mut body = json!({
        "prompt": req.prompt,
        "width": width,
        "height": height,
        "batch_count": req.batch.unwrap_or(1).max(1),
        "seed": req.seed.unwrap_or(-1),
        "steps": steps,
        "cfg_scale": cfg,
        "scheduler": req.scheduler.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_SCHEDULER),
    });
    let obj = body.as_object_mut().unwrap();

    if let Some(n) = req
        .negative_prompt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        obj.insert("negative_prompt".into(), json!(n));
    }
    if let Some(s) = req.sampler.as_deref().filter(|s| !s.trim().is_empty()) {
        obj.insert("sampling_method".into(), json!(s));
    }
    // VAE tiling is a VRAM lever that COSTS TIME, so it is now chosen by output size
    // instead of being left to the server's blanket `--vae-tiling` default.
    //
    // MEASURED 2026-08-01, 5060 Ti, prod engine (offload on), peak VRAM per render:
    //   1024²      untiled 8,797 MiB   decode 0.90s  |  tiled 8,541 MiB  decode 1.91s
    //   1920x1088  untiled 9,825 MiB   decode 1.89s  |  tiled 9,825 MiB  decode 3.80s
    //   2048²      untiled OOMs at decode            |  tiled 11,387 MiB decode 8.04s
    //
    // Two things that comment history got wrong and this encodes instead:
    //   * at 1920 the peak is set by SAMPLING, not decode, so tiling saves ZERO VRAM
    //     there and costs 1.9s. (The old "untiled 1920 peaks 14,281 MiB" figure belongs
    //     to the KREA2_OFFLOAD="" regime, which prod left behind.)
    //   * only 2048-class output actually needs tiling, and it needs it to not OOM.
    // So: tile above TILING_PIXEL_THRESHOLD, run untiled at or below it. At 1024² —
    // the game-asset size — that is worth 1.0s of a 10.7s render on its own.
    //
    // An explicit `vae_tiling` from the caller still wins; this only sets the default.
    let tiled = req
        .vae_tiling
        .unwrap_or((width as u64) * (height as u64) > TILING_PIXEL_THRESHOLD);
    obj.insert("vae_tiling_params".into(), json!({ "enabled": tiled }));

    // ── reference modes ONLY, from here down ────────────────────────────────
    //
    // edit and restage differ in exactly ONE emitted byte-range: the preset name.
    // Same adapter, same multiplier, same grounding knob — the preset is what tells
    // the engine whether the reference is the canvas (crop it to the target) or an
    // identity source (decode it natively and centre its RoPE ids). Keeping the two
    // arms as one branch is deliberate: it is why they cannot drift apart.
    if matches!(mode_used, "edit" | "restage") && !refs.is_empty() {
        obj.insert("ref_images".into(), json!(refs));

        let preset = if mode_used == "restage" {
            RESTAGE_REF_PRESET
        } else {
            EDIT_REF_PRESET
        };

        // Comma-separated key=value list, `preset=` first. `vlm_size` overrides the
        // preset's own 768 only when the caller asked for a different grounding
        // size — the preset is training-matched, so we don't restate its defaults.
        let mut ref_image_args = format!("preset={preset}");
        if let Some(px) = req.grounding_px {
            let px = px.clamp(GROUNDING_PX_MIN, GROUNDING_PX_MAX);
            ref_image_args.push_str(&format!(",vlm_size={px}"));
        }
        obj.insert("ref_image_args".into(), json!(ref_image_args));

        obj.insert(
            "lora".into(),
            json!([{ "path": EDIT_LORA_PATH, "multiplier": EDIT_LORA_MULTIPLIER }]),
        );
    }

    body
}

/// t2i body for an in-process caller that only has a prompt and a size — the
/// `/api/v1/oneshot` composite. Routed through the same builder, clamping and
/// distilled-checkpoint defaults as the public route so Krea 2's sampling
/// contract stays known in exactly one place. Returns the body plus the
/// dimensions actually used (clamped and snapped to ×16).
pub(crate) fn t2i_body(
    prompt: &str,
    negative: Option<&str>,
    width: u32,
    height: u32,
    seed: Option<i64>,
) -> (Value, u32, u32) {
    let req = Krea2GenerateRequest {
        prompt: prompt.to_string(),
        negative_prompt: negative.map(str::to_string),
        seed,
        ..Default::default()
    };
    let (w, h) = (clamp_dim(width), clamp_dim(height));
    (
        build_img_gen_body(&req, "t2i", &[], w, h, DEFAULT_STEPS, DEFAULT_CFG),
        w,
        h,
    )
}

// ── multipart parsing ─────────────────────────────────────────────────────

/// Parse a `/generate` multipart body: a `request` JSON part (without the image
/// b64s) plus repeated `reference_images` file parts. Same field names as
/// `routes::images` so the two panels can share upload code, and the same input
/// capture: every file part is written into the asset store here and referenced by
/// `asset:<hash>`, because the render may not start for hours.
///
/// **Part order is the reference order** (scene first, person second) — multipart
/// preserves it and so does this, since the order is load-bearing for the LoRA.
async fn parse_generate_multipart(
    state: &AppState,
    mut mp: Multipart,
) -> Result<Option<Krea2GenerateRequest>, Response> {
    let mut request: Option<Krea2GenerateRequest> = None;
    let mut reference_images: Vec<String> = Vec::new();

    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                match name.as_str() {
                    "request" => {
                        let t = field.text().await.map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("reading request field: {e}"),
                            )
                        })?;
                        request = Some(serde_json::from_str(&t).map_err(|e| {
                            err(StatusCode::BAD_REQUEST, &format!("bad request: {e}"))
                        })?);
                    }
                    "reference_images" => {
                        reference_images.push(capture_part(state, field, &name).await?);
                    }
                    _ => {
                        let _ = field.bytes().await;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    &format!("malformed multipart: {e}"),
                ));
            }
        }
    }

    let Some(mut req) = request else {
        return Ok(None);
    };
    // File parts append to whatever the JSON part already carried, so a caller can
    // mix a base64 scene from the gallery with an uploaded person photo.
    if !reference_images.is_empty() {
        match &mut req.reference_images {
            Some(existing) => existing.extend(reference_images),
            None => req.reference_images = Some(reference_images),
        }
    }
    Ok(Some(req))
}

// ── helpers ───────────────────────────────────────────────────────────────

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn log_krea2(
    state: &AppState,
    caller: Option<String>,
    mode_used: &str,
    steps: u32,
    gate_wait_ms: u64,
    duration_ms: u64,
    status: i32,
    mut response_meta: Value,
) {
    // §9.2: what this render actually put on the wire. `wire.request.parts == 0` on every krea2
    // row until B1/B2 land — reference images are still base64 inside the JSON.
    if let Some(obj) = response_meta.as_object_mut() {
        obj.insert("wire".to_string(), state.krea2.wire().as_meta());
    }
    log_async(
        state.db.clone(),
        CreateLog {
            service: SERVICE.into(),
            endpoint: "generate".into(),
            request_meta: json!({ "mode_used": mode_used, "steps": steps }),
            response_meta,
            sample_id: None,
            gate_wait_ms: gate_wait_ms as i64,
            duration_ms: duration_ms as i64,
            status,
            caller,
        },
    );
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Krea2GenerateRequest {
        Krea2GenerateRequest {
            prompt: "a lighthouse".into(),
            ..Default::default()
        }
    }

    fn t2i(r: &Krea2GenerateRequest) -> Value {
        build_img_gen_body(r, "t2i", &[], 1024, 1024, 8, 1.0)
    }

    fn edit(r: &Krea2GenerateRequest, refs: &[String]) -> Value {
        build_img_gen_body(r, "edit", refs, 1024, 1024, EDIT_STEPS, EDIT_CFG)
    }

    fn restage(r: &Krea2GenerateRequest, refs: &[String]) -> Value {
        build_img_gen_body(r, "restage", refs, 1920, 1088, EDIT_STEPS, EDIT_CFG)
    }

    fn one_ref() -> Vec<String> {
        vec!["AAAA".to_string()]
    }

    /// A real PNG header for `w`×`h`. Only the first 24 bytes are ever read, but
    /// building it honestly means the test exercises the same code a request does.
    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]);
        v
    }

    // The distilled checkpoint's published values. The UI seeds its step/cfg
    // defaults from `defaults_json`, so the key names are a frontend contract.
    #[test]
    fn defaults_are_the_distilled_checkpoints_values() {
        let d = super::defaults_json();
        assert_eq!(d["default_steps"], 8);
        assert_eq!(d["default_cfg"], 1.0);
        assert_eq!(d["default_scheduler"], "discrete");
    }

    // Edit mode's own prefills, from the identity-edit LoRA card: ~10 steps
    // balances composition vs face detail; cfg 1.0 (the DiT is still distilled).
    #[test]
    fn variant_defaults_expose_per_mode_step_cfg() {
        let v = super::variant_defaults();
        assert_eq!(v["t2i"]["default_steps"], 8);
        assert_eq!(v["t2i"]["default_cfg"], 1.0);
        assert_eq!(v["edit"]["default_steps"], 10);
        assert_eq!(v["edit"]["default_cfg"], 1.0);
        assert_eq!(v["edit"]["default_grounding_px"], 768);
    }

    // Flat sampling keys, and NO `model` field: krea2 runs a single DiT and the
    // server rejects a `model` string that isn't in its variant map.
    #[test]
    fn body_uses_flat_sampling_keys_and_omits_model() {
        let b = t2i(&req());
        assert_eq!(b["steps"], 8);
        assert_eq!(b["cfg_scale"], 1.0);
        assert_eq!(b["scheduler"], "discrete");
        assert_eq!(b["batch_count"], 1);
        assert!(b.get("model").is_none());
        assert!(b.get("sample_params").is_none());
        assert!(b.get("init_image").is_none());
        assert!(b.get("ref_images").is_none());
    }

    // HARD REQUIREMENT: the t2i body is unchanged by edit mode existing. Pinned as
    // an exact key set + exact JSON, so adding an edit field outside the edit arm
    // fails here rather than in production.
    #[test]
    fn t2i_body_is_byte_identical_to_the_shipped_shape() {
        let mut r = req();
        // Reference images present but mode explicitly t2i: still nothing sent.
        r.reference_images = Some(one_ref());
        r.grounding_px = Some(640);
        let b = t2i(&r);
        // The whole body, pinned: an exact-value compare, so an edit field added
        // outside the edit arm fails here and not in production.
        assert_eq!(
            b,
            json!({
                "prompt": "a lighthouse",
                "width": 1024,
                "height": 1024,
                "batch_count": 1,
                "seed": -1,
                "steps": 8,
                "cfg_scale": 1.0,
                "scheduler": "discrete",
                // Size-derived, and 1024² is under TILING_PIXEL_THRESHOLD. Always
                // present now — see `vae_tiling_defaults_by_output_size`.
                "vae_tiling_params": { "enabled": false },
            })
        );
        for k in ["ref_images", "ref_image_args", "lora"] {
            assert!(b.get(k).is_none(), "t2i body must not carry {k}");
        }
    }

    // The three fields the engine's krea2_identity_edit recipe needs. `lora[].path`
    // is the path relative to --lora-model-dir WITH extension: the stem alone is an
    // unexplained bare 400 from the engine.
    #[test]
    fn edit_body_carries_refs_preset_and_lora() {
        let refs = one_ref();
        let b = edit(&req(), &refs);
        assert_eq!(b["ref_images"], json!(["AAAA"]));
        assert_eq!(b["ref_image_args"], "preset=krea2_identity_edit");
        assert_eq!(
            b["lora"],
            json!([{"path": EDIT_LORA_PATH, "multiplier": 1.0}])
        );
        // Still one DiT, still flat sampling keys.
        assert!(b.get("model").is_none());
        assert_eq!(b["steps"], 10);
        assert_eq!(b["cfg_scale"], 1.0);
    }

    // Reference ORDER is load-bearing (scene first, person second) — the builder
    // must never reorder or dedupe.
    #[test]
    fn edit_preserves_reference_order() {
        let refs = vec!["SCENE".to_string(), "PERSON".to_string()];
        let b = edit(&req(), &refs);
        assert_eq!(b["ref_images"], json!(["SCENE", "PERSON"]));
    }

    // grounding_px rides as `,vlm_size=<n>` on ref_image_args, clamped. Unset must
    // leave the preset's own trained 768 alone rather than restating it.
    #[test]
    fn grounding_px_appends_vlm_size_and_clamps() {
        let refs = one_ref();
        let mut r = req();
        r.grounding_px = Some(640);
        assert_eq!(
            edit(&r, &refs)["ref_image_args"],
            "preset=krea2_identity_edit,vlm_size=640"
        );

        r.grounding_px = Some(64);
        assert_eq!(
            edit(&r, &refs)["ref_image_args"],
            "preset=krea2_identity_edit,vlm_size=384"
        );

        r.grounding_px = Some(4096);
        assert_eq!(
            edit(&r, &refs)["ref_image_args"],
            "preset=krea2_identity_edit,vlm_size=1024"
        );

        r.grounding_px = None;
        assert_eq!(
            edit(&r, &refs)["ref_image_args"],
            "preset=krea2_identity_edit"
        );
    }

    // `auto` resolves BOTH ways off the presence of a reference image, and an
    // unrecognised string resolves from the inputs rather than faulting (the FLUX.2
    // route's posture).
    #[test]
    fn auto_resolves_from_reference_presence() {
        assert_eq!(resolve_mode(None, false), Ok("t2i"));
        assert_eq!(resolve_mode(None, true), Ok("edit"));
        assert_eq!(resolve_mode(Some("auto"), false), Ok("t2i"));
        assert_eq!(resolve_mode(Some("AUTO"), true), Ok("edit"));
        assert_eq!(resolve_mode(Some("nonsense"), true), Ok("edit"));
        assert_eq!(resolve_mode(Some("nonsense"), false), Ok("t2i"));
    }

    // Explicit t2i with an image attached stays t2i (no silent mode switch);
    // explicit edit with no image is a 400, not a silent t2i render at edit's
    // step count.
    #[test]
    fn explicit_modes_win_and_edit_without_an_image_is_rejected() {
        assert_eq!(resolve_mode(Some("t2i"), true), Ok("t2i"));
        assert_eq!(resolve_mode(Some("edit"), true), Ok("edit"));
        let e = resolve_mode(Some(" Edit "), false).expect_err("must reject");
        assert!(e.contains("reference image"), "unhelpful message: {e}");
    }

    // restage is the same adapter and the same reference plumbing as edit; the
    // ONLY difference on the wire is the preset name, and that is what tells the
    // engine not to centre-crop the reference to the target AR.
    #[test]
    fn restage_body_differs_from_edit_only_in_the_preset() {
        let r = req();
        let refs = one_ref();
        let mut e = edit(&r, &refs);
        let mut s = restage(&r, &refs);

        assert_eq!(e["ref_image_args"], "preset=krea2_identity_edit");
        assert_eq!(s["ref_image_args"], "preset=krea2_identity_restage");
        assert_eq!(s["lora"], e["lora"], "restage must carry the same adapter");
        assert_eq!(s["ref_images"], e["ref_images"]);

        // Everything else that is not geometry must be identical. Strip the two
        // fields that are legitimately different and compare wholesale, so a field
        // added to one arm and not the other cannot slip through.
        for b in [&mut e, &mut s] {
            let o = b.as_object_mut().unwrap();
            o.remove("ref_image_args");
            o.remove("width");
            o.remove("height");
        }
        assert_eq!(e, s);
    }

    #[test]
    fn restage_needs_a_reference_and_explicit_modes_still_win() {
        assert_eq!(resolve_mode(Some("restage"), true), Ok("restage"));
        assert_eq!(resolve_mode(Some(" ReStage "), true), Ok("restage"));
        let e = resolve_mode(Some("restage"), false).expect_err("must reject");
        assert!(e.contains("reference image"), "unhelpful message: {e}");
        // `restage` is a recognised mode, so it is NOT auto and geometry may not
        // move it.
        assert!(!is_auto(Some("restage")));
        assert!(is_auto(None));
        assert!(is_auto(Some("auto")));
        assert!(is_auto(Some("nonsense")));
    }

    #[test]
    fn image_headers_are_probed_without_decoding() {
        assert_eq!(probe_image_dims(&png_header(768, 1344)), Some((768, 1344)));
        // JPEG: SOI, a JFIF APP0 to skip over, then SOF0 carrying h then w.
        let jpeg = [
            &[0xFF, 0xD8][..],
            &[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00][..], // APP0, len 4
            &[0xFF, 0xC0, 0x00, 0x11, 0x08][..],       // SOF0, len 17, precision 8
            &1088u16.to_be_bytes()[..],                // height
            &1920u16.to_be_bytes()[..],                // width
        ]
        .concat();
        assert_eq!(probe_image_dims(&jpeg), Some((1920, 1088)));
        // Not an image, and a truncated PNG: `None`, never a guess.
        assert_eq!(probe_image_dims(b"not an image at all"), None);
        assert_eq!(probe_image_dims(&png_header(1, 1)[..12]), None);
    }

    // The severity number from the writeup, reproduced from geometry alone: a
    // square source into 16:9 keeps 57% of its height, a 768x1344 portrait 32%.
    #[test]
    fn ar_keep_fraction_matches_the_measured_severities() {
        let sq = ar_keep_fraction(1024, 1024, 1920, 1088);
        assert!((sq - 0.5667).abs() < 0.01, "square kept {sq}");
        let port = ar_keep_fraction(768, 1344, 1920, 1088);
        assert!((port - 0.3238).abs() < 0.01, "portrait kept {port}");
        // Matched ARs lose nothing, in either orientation.
        assert!((ar_keep_fraction(1920, 1088, 1920, 1088) - 1.0).abs() < 1e-9);
        assert!((ar_keep_fraction(512, 512, 1024, 1024) - 1.0).abs() < 1e-9);
    }

    // ★ The rule. `auto` must never silently destroy the reference.
    #[test]
    fn auto_never_silently_crops_the_reference() {
        // No explicit size: edit renders on the reference's own shape, so the
        // engine's crop is a no-op. The old behaviour was 1024x1024 here, which
        // cropped a portrait reference to 57% of itself for no reason at all.
        let p = plan_geometry("edit", true, false, 1024, 1024, Some((768, 1344)));
        assert_eq!(p.mode, "edit");
        assert!(
            (ar_keep_fraction(768, 1344, p.width, p.height) - 1.0).abs() < 0.02,
            "derived {}x{} does not match the reference's AR",
            p.width,
            p.height
        );

        // Explicit size at a DIFFERENT aspect ratio is the data-loss case, and
        // auto resolves it by restaging rather than by cropping. Never silent.
        let p = plan_geometry("edit", true, true, 1920, 1088, Some((768, 1344)));
        assert_eq!(p.mode, "restage", "auto must not crop away 68% of a face");
        assert_eq!((p.width, p.height), (1920, 1088));
        let note = p.note.expect("the reroute must be explained");
        assert!(note.contains("restage"), "unhelpful note: {note}");

        // Explicit size that already matches: nothing to fix, stays edit.
        let p = plan_geometry("edit", true, true, 1024, 1024, Some((512, 512)));
        assert_eq!(p.mode, "edit");
        assert_eq!(p.note, None);
        assert_eq!((p.width, p.height), (1024, 1024));
    }

    #[test]
    fn explicit_modes_are_honoured_even_when_the_geometry_disagrees() {
        // Explicit restage keeps the canvas it asked for, untouched.
        let p = plan_geometry("restage", false, true, 1920, 1088, Some((768, 1344)));
        assert_eq!((p.mode, p.width, p.height), ("restage", 1920, 1088));
        assert_eq!(p.note, None);

        // Explicit edit stays edit, but the canvas follows the reference — that is
        // what edit MEANS — and the change is explained rather than silent.
        let p = plan_geometry("edit", false, true, 1920, 1088, Some((768, 1344)));
        assert_eq!(p.mode, "edit");
        assert!(
            (ar_keep_fraction(768, 1344, p.width, p.height) - 1.0).abs() < 0.02,
            "edit canvas {}x{} left the reference's AR",
            p.width,
            p.height
        );
        assert!(p.note.is_some(), "a rewritten canvas must be explained");

        // t2i is never touched by any of this.
        let p = plan_geometry("t2i", true, true, 1920, 1088, None);
        assert_eq!(
            (p.mode, p.width, p.height, p.note),
            ("t2i", 1920, 1088, None)
        );
    }

    // An unreadable reference is the one case we cannot rule on. It must not be
    // treated as safe — the whole failure mode here is invisibility.
    #[test]
    fn an_unreadable_reference_is_reported_not_assumed_safe() {
        let p = plan_geometry("edit", true, true, 1920, 1088, None);
        assert_eq!(p.mode, "edit");
        assert_eq!((p.width, p.height), (1920, 1088));
        let note = p.note.expect("must say it could not check");
        assert!(note.contains("could not read"), "unhelpful note: {note}");
    }

    // Derived canvases stay legal: x16 on both axes, inside MAX_DIM and the 2 MP
    // budget the panel also uses.
    #[test]
    fn derived_canvases_are_legal_and_budgeted() {
        for (rw, rh) in [(768, 1344), (1024, 1024), (6000, 4000), (100, 3000)] {
            let p = plan_geometry("edit", true, false, 1024, 1024, Some((rw, rh)));
            assert_eq!(p.width % 16, 0, "{rw}x{rh} -> width {}", p.width);
            assert_eq!(p.height % 16, 0, "{rw}x{rh} -> height {}", p.height);
            assert!(
                p.width <= MAX_DIM && p.height <= MAX_DIM,
                "{rw}x{rh} over MAX_DIM"
            );
            let px = u64::from(p.width) * u64::from(p.height);
            assert!(px <= MAX_DERIVED_PIXELS, "{rw}x{rh} -> {px} px over budget");
        }
    }

    // The panel greys edit mode out on this. Both engine shapes count, and the
    // stem alone (`name`) counts as present even though a REQUEST must send the
    // path with its extension.
    #[test]
    fn edit_availability_reads_capabilities_loras() {
        assert!(!lora_present(None, EDIT_LORA_PATH));
        assert!(!lora_present(Some(&json!({})), EDIT_LORA_PATH));
        assert!(!lora_present(
            Some(&json!({"loras": ["something-else.gguf"]})),
            EDIT_LORA_PATH
        ));
        assert!(lora_present(
            Some(&json!({"loras": [EDIT_LORA_PATH]})),
            EDIT_LORA_PATH
        ));
        assert!(lora_present(
            Some(&json!({"loras": [{
                "name": EDIT_LORA_PATH.trim_end_matches(".gguf"),
                "path": EDIT_LORA_PATH,
            }]})),
            EDIT_LORA_PATH
        ));
    }

    // The `/sdapi/v1/loras` fallback body, captured verbatim from the running
    // krea2 engine WHILE ITS WORKER WAS UNLOADED — the exact state in which
    // `/sdcpp/v1/capabilities` 410s and edit mode used to go dark. The route
    // returns a BARE ARRAY, so `Krea2Client::loras` wraps it under `loras`;
    // pinned here because an unwrapped array reads as absent and silently
    // restores the old symptom.
    #[test]
    fn edit_availability_reads_the_sdapi_loras_fallback_shape() {
        let wire = json!([
            {"name": "krea2-identity-edit-v1_2", "path": "krea2-identity-edit-v1_2.gguf"},
            {"name": "snofs_krea_v1_nostrip", "path": "snofs_krea_v1_nostrip.safetensors"},
        ]);
        assert!(lora_present(
            Some(&json!({ "loras": wire })),
            EDIT_LORA_PATH
        ));
        // Unwrapped, it is not the shape the reader expects.
        assert!(!lora_present(Some(&json!([])), EDIT_LORA_PATH));
    }

    // vae_tiling now defaults BY OUTPUT SIZE rather than deferring to the server's
    // blanket --vae-tiling. Small output decodes untiled (faster, still fits);
    // 2048-class output must stay tiled or the decode OOMs.
    #[test]
    fn vae_tiling_defaults_by_output_size() {
        // 1024² and 1920x1088 are both under the threshold -> untiled.
        let b = build_img_gen_body(&req(), "t2i", &[], 1024, 1024, 8, 1.0);
        assert_eq!(b["vae_tiling_params"]["enabled"], false);
        let b = build_img_gen_body(&req(), "t2i", &[], 1920, 1088, 8, 1.0);
        assert_eq!(b["vae_tiling_params"]["enabled"], false);

        // 2048² is over it -> tiled, which is what keeps that decode from OOMing.
        let b = build_img_gen_body(&req(), "t2i", &[], 2048, 2048, 8, 1.0);
        assert_eq!(b["vae_tiling_params"]["enabled"], true);
    }

    // An explicit caller choice still overrides the size-derived default, in both
    // directions.
    #[test]
    fn explicit_vae_tiling_overrides_the_size_default() {
        let mut r = req();
        r.vae_tiling = Some(true);
        let b = build_img_gen_body(&r, "t2i", &[], 1024, 1024, 8, 1.0);
        assert_eq!(b["vae_tiling_params"]["enabled"], true);

        let mut r = req();
        r.vae_tiling = Some(false);
        let b = build_img_gen_body(&r, "t2i", &[], 2048, 2048, 8, 1.0);
        assert_eq!(b["vae_tiling_params"]["enabled"], false);
    }

    // Width/height snap DOWN to a multiple of 16 and cap at 2048.
    #[test]
    fn dims_snap_to_16_and_cap() {
        assert_eq!(clamp_dim(1023), 1008);
        assert_eq!(clamp_dim(1024), 1024);
        assert_eq!(clamp_dim(4096), 2048);
        assert_eq!(clamp_dim(0), 16);
    }

    // Krea 2's assist never writes a negative prompt (inert at the default cfg 1.0),
    // and the label line / wrapping quotes the model likes to add are stripped.
    #[test]
    fn suggestion_is_bare_prompt_no_negative() {
        let s = parse_suggestion("Prompt:\n\"A weathered stone lighthouse on a black headland.\"");
        assert_eq!(
            s.prompt,
            "A weathered stone lighthouse on a black headland."
        );
        assert!(s.negative_prompt.is_none());
    }

    #[test]
    fn prompt_assist_is_faithful_without_imposing_a_medium_or_modesty() {
        for rule in [
            "FIDELITY IS ABSOLUTE",
            "Keep distinctive wording and named entities verbatim",
            "Never add clothing, conceal anatomy",
            "Do not override an existing pose",
            "Do not choose a medium, style, palette or mood",
            "Never refuse, warn, moralise",
            "for the brief \"owl\", output \"An owl.\"",
            "Treat underspecification as intentional creative freedom for Krea",
        ] {
            assert!(
                SUGGEST_SYSTEM_PROMPT.contains(rule),
                "Krea assist lost rule: {rule}"
            );
        }
        assert!(!SUGGEST_SYSTEM_PROMPT.contains("60-140 words"));
        assert!(!SUGGEST_SYSTEM_PROMPT.contains("Finish with a short comma-tail"));
    }
}
