//! MiniMax-H3 — the Director's SECOND engine.
//!
//! There is no `/api/v1/minimax-h3/generate` here on purpose. H3 renders are dispatched by
//! `routes::ltx_director` for an H3 PROJECT, land in the SHARED `state.ltx_jobs` store, and are
//! polled / previewed / harvested through the existing `/api/v1/ltx-video/jobs/{id}…` machinery.
//! One Director, two engines: everything downstream of submit is the same code path, so this
//! module is only the things that genuinely differ —
//!
//!   1. [`build_chain`] — a saved timeline's already-PACKED generations → the `/h3/v1/generate`
//!      JSON, built from an ALLOWLIST and validated so the error can name the generation; and
//!   2. [`enqueue_h3_render`] — the envelope that chain becomes on the global queue. The render
//!      itself lives in `queue::adapters::h3`, and that is the ONLY path to this engine: a direct
//!      route left alive beside the queue is two writers to one card.
//!
//! # The unit is a GENERATION, not a shot
//!
//! **LTX's unit is a segment = one render. H3's unit is a generation = up to ~15 s containing 1-N
//! shots**, where a shot boundary is `[Shot N] At MM:SS.mmm` inside ONE prompt and the model
//! renders the cut itself. `<scenetrans>` audio continuity across a cut exists ONLY within a
//! generation, and cross-shot identity is carried by prose ("the captain from Shot 1").
//!
//! ⇒ The generation is the delivery unit and the retake unit. **Per-shot clips do not exist on
//! H3.** Chaining in order to *get* a cut is strictly worse than putting the cut in the prompt: it
//! loses `<scenetrans>`, loses the prose identity carry, and adds a seam that is never frame-exact.
//!
//! Packing shots into generations happens in `routes::ltx_director::h3_chain_for`, because that is
//! where the timeline and the 15 s budget arithmetic live. By the time a request reaches
//! [`build_chain`] the generations already exist; this module's job is to VALIDATE them and shape
//! them for the wire. Each generation's `shots[]` is advisory — the authority is the markers inside
//! `prompt`.
//!
//! # Why an allowlist and not a filter
//!
//! Everything H3 lacks (`negative_prompt`, `cfg`, model variants, the hires chain, `resume_job_id`,
//! `retake_segment`, `pin_strength`, mid-shot `keyframes`, `v2v_*`, the LTX audio sheet) does not
//! merely fail — it does not exist, and the shim reports anything unrecognised in `ignored[]`
//! rather than rejecting it. A subtractive filter is only correct until the next LTX field is
//! added; an allowlist is correct by construction, so a legacy doc, an engine-switched project or
//! an over-eager client can never build a request that renders the wrong thing. What the doc
//! carried and this dropped is reported as [`H3Chain::dropped`], and what the ENGINE ignored comes
//! back in the 202 — both are surfaced, because a setting that did nothing is only a bug when it is
//! invisible.
//!
//! 🔴 **LoRAs are NOT on that list any more.** The shim gained per-generation `loras` +
//! `lora_multipliers` on 2026-08-07 (`contract.py:346`; `/h3/v1/capabilities` reports
//! `"loras": true`) and [`build_chain`] forwards them, validated. The one that matters is the turbo
//! LoRA — `h3-turbo-4step-ema-pruned.safetensors` at 8 steps is 465.4 s against the 20-step
//! control's 762.8 s, −39% wall for ZERO extra VRAM — and it does not set `steps` itself, so the
//! caller must lower them or the LoRA is pure overhead. What is still missing is the koblem-side UI
//! for CHOOSING one; that is being added separately, so today the only way into this field is a
//! saved document. The LTX-shaped REQUEST-level `loras` (`[{path, multiplier}]`) stays dead and
//! stays reported: H3's LoRAs are per generation and named by bare filename.
//!
//! # Two rules the request builder enforces rather than leaving to the engine
//!
//!   * **Order is semantic.** A reference's position assigns its `<Picture i>` / `<Video k>` /
//!     `<Audio j>` ordinal AND advances a shared rotary clock, so the same references in a
//!     different order are a different request. Nothing may sort, group by kind or de-duplicate.
//!   * **`ref2va` and `fl2va` are different CHECKPOINTS** (`minimax_h3_handler.py:25-28`), not a
//!     flag — so ONE GENERATION cannot carry both omni references and first/last-frame
//!     conditioning. Checked here so the editor's guard has a server-side twin, and so the error
//!     can name the generation; the shim's own 400 cannot.
//!
//! A reference does NOT bind the canvas and is NOT pinned (it is encoded at its own resolution and
//! conditions the denoise; a keyframe latent is re-injected every step and never denoised). So
//! "use this reference as the first frame" is not offered anywhere in this stack — it produces
//! something *similar*, never *exact*, which is the failure a user only notices at the cut.

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::{debug, warn};

use crate::AppState;
use crate::auth::require_auth;
use crate::director_assets;
use crate::minimax_h3::H3RefPart;
use crate::routes::engine_cancel;
use koblem_client::types::VideoJobState;

// The model's own constants + the frame-grid arithmetic. Shared from `koblem-client` rather than
// re-derived, because the SNAP DIRECTION is the easy thing to get wrong: the engine rounds UP, and
// a client that rounds the other way disagrees with it about the duration of every render.
use koblem_client::types::{
    H3_ADVISORY_MAX_PIXELS, H3_CONTINUATION_FROM_PREV, H3_DIM_MULTIPLE, H3_DOCUMENTED_MIN_FRAMES,
    H3_FPS, H3_FRAME_BASE, H3_FRAME_PERIOD, H3_MAX_AUDIO_REFS, H3_MAX_FRAMES, H3_MAX_IMAGE_REFS,
    H3_MAX_PROMPT_CHARS, H3_MAX_TOTAL_REFS, H3_MAX_VIDEO_REFS, H3_MIN_FRAMES,
    H3_MIN_REF_VIDEO_FRAMES, H3_REF_RELATIVE_SIZE_MAX, H3_REF_RELATIVE_SIZE_MIN, H3Continuation,
    H3ContinuationMode, H3ReferenceKind, H3ReferenceRole, H3Task, h3_cap_u64_or,
    h3_capability_drift, h3_legal_tails, h3_seconds_from_frames, h3_snap_frames,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        // What the editor needs to BUILD an H3 request: koblem's own copy of the limits plus the
        // shim's `/h3/v1/capabilities` document, and (with ?width=&height=&frames=) the exact
        // geometry those values would render at. Answered WITHOUT loading a model, so a UI can show
        // the snapped duration without evicting the LLM to ask.
        .route("/api/v1/minimax-h3/props", get(props))
        .route("/api/v1/minimax-h3/cancel", post(cancel_current))
        // The in-flight preview PNG. Opt-in per request (`preview: true`) because
        // `generate_preview` decodes latents through the RGB-factor path per emission and the cost
        // is unmeasured; a 404 here means the render did not ask for one, not that it failed.
        .route("/api/v1/minimax-h3/jobs/{id}/preview", get(job_preview))
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

#[derive(Deserialize)]
struct PropsQuery {
    width: Option<u32>,
    height: Option<u32>,
    frames: Option<u32>,
}

/// `GET /api/v1/minimax-h3/props[?width=&height=&frames=]` — is the engine there, and what would
/// these values render at.
///
/// Mirrors `/api/v1/ltx-video/props` so the Director can ask one question per engine. `healthy`
/// false is not an error: the editor stays usable and only the render buttons go quiet.
///
/// The limits are stated here even when the engine is down, because an editor that cannot say
/// "that is a third video reference and there are only two slots" until the GPU answers is an
/// editor that lets a user build a request and discover the refusal an hour later.
async fn props(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PropsQuery>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.minimax_h3.is_configured() {
        return Json(json!({
            "configured": false,
            "healthy": false,
            "fps": H3_FPS,
        }))
        .into_response();
    }
    let healthy = state.minimax_h3.healthy().await;
    let query = match (q.width, q.height, q.frames) {
        (None, None, None) => None,
        (w, h, f) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(w) = w {
                parts.push(format!("width={w}"));
            }
            if let Some(h) = h {
                parts.push(format!("height={h}"));
            }
            if let Some(f) = f {
                parts.push(format!("frames={f}"));
            }
            Some(parts.join("&"))
        }
    };
    // The shim's own capability document. Richer than koblem's copy — it reports the currently
    // REACHABLE frame ceiling (which reachability work may raise without a koblem change), the
    // loaded checkpoint, and the ETA model — so a UI that wants the live answer reads this and a UI
    // that only wants the contract reads the flat fields above it.
    let capabilities = if healthy {
        state
            .minimax_h3
            .capabilities(query.as_deref())
            .await
            .map_err(|e| warn!(error = %e, "minimax-h3 capabilities failed"))
            .ok()
    } else {
        None
    };
    // 🔴 Folded into props rather than given its own route, deliberately. The editor already asks
    // this endpoint one question per engine before it wants a GPU, the body is a directory listing
    // of a handful of filenames, and a second round trip would only exist so a second component
    // could forget to make it. The whole document rides through verbatim — `dir`, `exists`, `count`,
    // `loras[]`, and the shim's own `note`/`hint` when the directory is empty — because the
    // interesting case is NOT the happy list: it is `count: 0`, which means the volume is not
    // mounted and every `loras[]` request will be refused however well-formed it is.
    let loras = if healthy {
        state
            .minimax_h3
            .loras()
            .await
            .map_err(|e| warn!(error = %e, "minimax-h3 lora listing failed"))
            .ok()
    } else {
        None
    };
    let health = state.minimax_h3.health().await;

    // 🔴 THE ENGINE IS THE AUTHORITY; THE CONSTANTS ARE THE FALLBACK. Everything below used to be
    // a hand-maintained literal sitting BESIDE the live `capabilities` document, and the editor
    // reads the literals — so the two could disagree indefinitely and only the wrong one was ever
    // consulted. They did: `max_video_refs`/`max_audio_refs` published 2 for the nine days after
    // ComfyUI replaced WanGP on 2026-08-12, hiding a third reference slot the engine had been
    // accepting the whole time.
    //
    // The constants stay because they are the answer when the engine is DOWN (the Director still
    // has to draw a form) and because request validation runs at submit with nothing fetched. They
    // are no longer what a healthy engine's UI is told.
    let caps = capabilities.as_ref();
    let cap = |ptr: &str, fallback: u64| h3_cap_u64_or(caps, ptr, fallback);
    let min_frames = cap("/frames/min", H3_MIN_FRAMES as u64) as u32;
    let max_frames = cap("/frames/max", H3_MAX_FRAMES as u64) as u32;
    let documented_min_frames =
        cap("/frames/documented_min", H3_DOCUMENTED_MIN_FRAMES as u64) as u32;

    // 🔴 AND WHEN THEY DISAGREE, SAY SO — LOUDLY. Falling back silently is what let the 2 survive:
    // a client cap BELOW the engine's is not an error anywhere, because the request never reaches
    // the engine to be accepted. `mismatched` is a bug in one of the two trees and somebody has to
    // move; `unpublished` is normally just koblibs landing ahead of the engine image.
    let drift = caps.map(h3_capability_drift).unwrap_or_default();
    if !drift.mismatched.is_empty() {
        warn!(
            mismatched = ?drift.mismatched,
            "minimax-h3: koblem's compiled-in H3 constants disagree with the live \
             engine — the engine is right, move koblem-client::types"
        );
    }
    if !drift.unpublished.is_empty() {
        debug!(
            unpublished = ?drift.unpublished,
            "minimax-h3: the engine publishes no value for some H3 constants, so they \
             are unverifiable against what is really rendering"
        );
    }

    Json(json!({
        "configured": true,
        "healthy": healthy,
        // `null` when the engine is down or too old to serve the route — which is NOT the same as
        // an empty list, and the editor must keep them apart: unknown means "offer the text box",
        // empty means "there is nothing to pick and a LoRA will 400".
        "loras": loras,
        // Not negotiable, and worth stating even when the engine is down: the editor's duration
        // arithmetic is only right at 24.
        "fps": cap("/fps", H3_FPS as u64),
        "frame_period": cap("/frames/period", H3_FRAME_PERIOD as u64),
        "frame_base": cap("/frames/base", H3_FRAME_BASE as u64),
        // The generation budget. `min_frames` is the VAE's own DECODABLE floor (22 = 0.917 s; a
        // 2-latent block decodes 0 chunks and raises, which is why 5 is still refused).
        // `max_frames` is the documented 15 s ceiling and what makes a timeline need packing.
        "min_frames": min_frames,
        "max_frames": max_frames,
        "min_seconds": h3_seconds_from_frames(min_frames),
        "max_seconds": h3_seconds_from_frames(max_frames),
        // The grid-snap of MiniMax's documented 4 s duration guideline. ADVISORY: a shorter
        // generation renders, it is simply outside the range they document, and the editor says
        // so once instead of lengthening it. (It was enforced as a hard floor until 2026-08-07,
        // which silently turned every 2-second beat into a 4.458-second one.)
        "documented_min_frames": documented_min_frames,
        "documented_min_seconds": h3_seconds_from_frames(documented_min_frames),
        // 🔴 Hard: both sides must be a multiple of 32 (VAE 16x, then the DiT's 2x spatial patch).
        // 1280x720 and 1920x1080 are NOT legal H3 canvases.
        "dim_multiple": cap("/canvas/dim_multiple", H3_DIM_MULTIPLE as u64),
        // 🔴 ADVISORY, and named so. `diffusers` calls this exact number a "soft area cap" and
        // ComfyUI does not enforce it; there is no max-resolution key in the weights tree. Over
        // budget is a warning about VRAM, not a refusal — enforcing it meant true 16:9 at a 768
        // short edge (1376x768) was unrenderable for being 2.4% over.
        "max_pixels": cap("/canvas/max_pixels", H3_ADVISORY_MAX_PIXELS as u64),
        "max_pixels_advisory": true,
        "max_prompt_chars": cap("/prompt/max_chars", H3_MAX_PROMPT_CHARS as u64),
        "max_image_refs": cap("/references/max_image", H3_MAX_IMAGE_REFS as u64),
        // 🔴 3 EACH SINCE 2026-08-21, AND THE 2 BEFORE IT WAS A WanGP FACT THAT OUTLIVED
        // WanGP. WanGP held exactly two named slots per kind (`video_guide`/`video_guide2`,
        // `audio_guide`/`audio_guide2`) and refused a third in its own name, so a UI offering one
        // was offering a slot that failed at validation. ComfyUI replaced it on 2026-08-12 and its
        // `ref_videos`/`ref_audios` are COMFY_AUTOGROW_V3 templates that DECLARE `max: 3` (images
        // declare 9) — so 3 is the node's own maximum, matching MiniMax's documented figure, and
        // the engine publishes `references.limited_by.max_video = "model"`.
        "max_video_refs": cap("/references/max_video", H3_MAX_VIDEO_REFS as u64),
        "max_audio_refs": cap("/references/max_audio", H3_MAX_AUDIO_REFS as u64),
        "max_total_refs": cap("/references/max_total", H3_MAX_TOTAL_REFS as u64),
        // 🔴 STILL TRUE: there is NO size requirement on an uploaded reference to surface
        // here, because the engine discards the upload's dimensions entirely. Every still is
        // canvased to the generation's own area, so a 4K photo and a 220px crop cost identical DiT
        // rows on every step of every shot they are scoped to. A UI that offers "shrink your
        // reference to make it cheaper" is offering nothing.
        //
        // 🔴 NO LONGER TRUE: that this slider is what moves the cost. The percentage was WanGP's
        // lever. ComfyUI has a two-valued `ref_image_size` combo (`match` / `max`) instead, and the
        // shim VALIDATES anything in 50-400, maps it to `match` (i.e. 100) and warns. The live
        // lever is `capabilities.references.ref_image_size`, which is what a cost-conscious editor
        // should offer; this block is kept so an existing caller's slider still round-trips.
        "image_refs_relative_size": {
            "min": cap(
                "/references/image_refs_relative_size/min",
                H3_REF_RELATIVE_SIZE_MIN as u64,
            ),
            "max": cap(
                "/references/image_refs_relative_size/max",
                H3_REF_RELATIVE_SIZE_MAX as u64,
            ),
            "default": 100,
            // Accepted, not honoured. Named here so a UI can grey the slider rather than lie.
            "honoured": caps
                .and_then(|c| c.pointer("/references/image_refs_relative_size/honoured"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        },
        // 🔴 `ref_image_aspect_min`/`max` ARE GONE (2026-08-21), NOT REPLACED. They published
        // 0.25/4.0 out of WanGP's `pipeline.py:63`, and there is no aspect check anywhere in the
        // ComfyUI node. What it does instead (`nodes_minimax_h3.py:219-231`) is scale every
        // reference aspect-PRESERVING and DOWN-ONLY — to the generation's pixel area under
        // `ref_image_size="match"`, or to a 2048px short edge under `"max"` — rounded to the
        // canvas multiple, with `crop="disabled"`. Any aspect is accepted and nothing is cropped.
        //
        // So the honest fact, and the one an editor can act on, is the RESIZE, not a bound: a
        // reference is fitted to the generation's area, which is why its own dimensions do not
        // change its cost. That is `capabilities.references.ref_image_size` and it is already
        // published by the engine. A UI wanting to show "this 4K still will be used at ~1344x768"
        // computes it from the generation's canvas, not from a limit.
        "ref_image_aspect_unrestricted": true,
        "ref_image_downscale_only": true,
        // 🔴 THE REFERENCE-VIDEO RULE, AND IT IS NOT THE 2-15 s EVERYONE QUOTED. 48/360 came
        // from WanGP's `validate_generative_settings:274-275`. The node has a HARD floor of 5
        // frames (`raise ValueError`), NO fixed ceiling — a longer reference is truncated to the
        // generation's own length — and it snaps off-grid lengths DOWN to 5+17k, silently.
        //
        // Only the floor is a refusal, and the engine enforces it at submit by probing the staged
        // container. koblem cannot: it sees a container and cannot count frames. Publishing the
        // floor is what lets the EDITOR flag a too-short clip at upload time, where the browser
        // knows the duration and the user can still choose a different one.
        "min_ref_video_frames": cap(
            "/references/ref_video_frames/min",
            H3_MIN_REF_VIDEO_FRAMES as u64,
        ),
        // Null, and deliberately so — the ceiling is per-request. A UI wanting it for a specific
        // shot uses that shot's own frame count.
        "max_ref_video_frames": Value::Null,
        "ref_video_max_rule": "the generation's own frame count; longer is truncated, not refused",
        "ref_video_grid": "5+17k",
        // Reference-video / continuation tails must land on the 5+17k grid, or
        // `_add_video_reference` silently shortens them and the model is told a lie about time.
        "legal_tails": h3_legal_tails().collect::<Vec<_>>(),
        // Audio-only conditioning is invalid: a referenced generation needs at least one image or
        // video, and `audio_count <= visual_count`.
        "audio_only_refs": false,
        "capabilities": capabilities,
        // 🔴 What the compiled-in constants and the live engine disagree about, published so
        // the answer is visible in the editor and not only in a log line nobody tails. Empty is
        // the healthy state. A non-empty `mismatched` means one of the two trees has to move and
        // the ENGINE is the authority — the fields above already prefer it, so a UI reading this
        // document is already correct; what is stale is koblem's validation, which will refuse a
        // request the engine would have taken.
        "capability_drift": drift,
        "health": health,
    }))
    .into_response()
}

/// `POST /api/v1/minimax-h3/cancel` — stop the H3 render NOW, or drop the next one queued.
///
/// 🔴 Through the DISPATCHER, never straight at the engine. The dispatcher owns the run, so it is
/// the only thing that can stop it AND file the terminal state; reaching past it leaves a row
/// `running` with nothing running.
///
/// 🔴 The engine cancel lands in ~10 ms but is only HONOURED at a step boundary, and H3's steps are
/// 43-50 s at prod shape. A UI that treats the 202 as "stopped" will show a stopped render still
/// holding the GPU for the better part of a minute.
async fn cancel_current(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(
        &state,
        koblem_client::types::JobKind::H3,
        "no H3 render is queued or running",
    )
    .await
}

/// `GET /api/v1/minimax-h3/jobs/{id}/preview` — the latest in-flight preview PNG for a koblem job.
///
/// Keyed by the KOBLEM job id, not the engine's, so a client that is already polling
/// `/api/v1/ltx-video/jobs/{id}` needs no second identifier. `preview_seq` on that poll body is the
/// signal to re-fetch; the URL never changes.
async fn job_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(job) = state.ltx_jobs.get(&id).filter(|j| j.is_h3()) else {
        return err(StatusCode::NOT_FOUND, "no such H3 job");
    };
    // 🔴 ANSWER LOCALLY WHEN THERE IS DEMONSTRABLY NOTHING TO FETCH. `preview_seq`
    // is `None` until the engine reports a preview, and it never will for a render
    // that did not ask for one (`preview: true` is opt-in — `generate_preview`
    // decodes latents through the RGB-factor path per emission).
    //
    // The UI polls this route at 2 Hz for the lifetime of a job regardless. Against
    // an engine that does not implement the upstream route at all, that produced
    // **43,935 log lines** on comfy-api on 2026-08-17 and rotated away the request
    // body of a job that had OOMed hours earlier — the forensic window was gone
    // before anyone looked. Every one of those requests was for a render with
    // previews off, i.e. knowably pointless from right here.
    //
    // ⚠️ Not a behaviour change for the caller: this is the same 404 it already got,
    // just without a round trip. The contract is unchanged — `preview_seq` on the
    // poll body is the signal to fetch, and this only refuses when that signal has
    // never fired.
    if job.preview_seq.is_none() {
        return err(
            StatusCode::NOT_FOUND,
            "no preview: this render did not request one (preview_seq is unset)",
        );
    }
    let Some(server_job_id) = job.server_job_id else {
        return err(StatusCode::NOT_FOUND, "no such H3 job");
    };
    match state.minimax_h3.fetch_preview(&server_job_id).await {
        Ok(bytes) => {
            crate::routes::byte_serve::serve_bytes(&headers, Bytes::from(bytes), "image/png")
        }
        // A render that did not ask for previews has none — not an error worth a 502.
        Err(e) => err(StatusCode::NOT_FOUND, &format!("no preview: {e}")),
    }
}

// ─── The request the Director saves ────────────────────────────────────────────
//
// The shapes themselves live in `koblem-client` alongside `LtxVideoGenerateRequest`, per the
// project convention for wire types, and are re-exported here so this module reads as one piece.
// Everything absent from them is absent from the wire request too: serde drops unknown keys
// silently, which is the point — an H3 project whose doc still carries `beats` / `pin_strength` /
// `character_refs` (because it was an LTX project yesterday) compiles to a request the engine
// accepts instead of one that renders the wrong thing.

pub use koblem_client::types::{H3GenerateRequest as H3Request, H3Generation, H3Reference};

/// The engine request, plus what koblem itself dropped on the way.
#[derive(Debug)]
pub struct H3Chain {
    /// The `/h3/v1/generate` JSON body.
    pub chain: Value,
    /// The project's soundtrack, if it set one — carried so the render can remux the ORIGINAL
    /// file over the finished cut. The slices the engine was conditioned on are already ordinary
    /// references inside `chain` by this point; this is the delivery half, and it is not optional
    /// extra polish: `fully_copy` is a prompt marker, so what the engine returns is a
    /// reconstruction of the song rather than the song.
    pub soundtrack: Option<koblem_client::types::H3Soundtrack>,
    /// What the soundtrack slices were CUT for, in seconds of finished cut.
    ///
    /// Set by the soundtrack pre-pass, not by `build_chain` — it is the plan's own arithmetic and
    /// this struct only carries it to the render, which compares it against the clip that came
    /// back. That comparison is the ONLY thing that would catch koblem's mirrored `H3_TRIM_JOIN`
    /// drifting out of step with the shim's real `trim_join`: both the render and the mux succeed
    /// either way, and the song simply slides a frame per seam.
    pub sliced_for_seconds: Option<f64>,
    /// Head frames the slice offsets ASSUMED the seams would drop.
    ///
    /// Carried beside [`Self::sliced_for_seconds`] and set by the same pre-pass, because it answers
    /// the question that one cannot: a cut that came back the wrong length says only THAT the seams
    /// disagreed with the plan, and this is the number the shim's own `trimmed_head_total` is
    /// compared against to say HOW. 0 when the project has no soundtrack, where nothing checks it.
    pub expected_trim_frames: u32,
    /// Where each RENDERED generation of THIS job starts in the song, seconds.
    ///
    /// Indexed by the job's own generation index — the same space the engine's partials arrive in
    /// — so a retake holding project generation 3 carries a one-element vector containing
    /// generation 3's offset, not three entries.
    ///
    /// 🔴 Empty when the project has no soundtrack, which is the signal that there is nothing to
    /// mux; do not read `first()` as "0.0 is fine".
    ///
    /// Set by the caller, from [`koblem_client::types::H3AudioSlice::delivered_at_seconds`] — the
    /// plan owns this arithmetic and `build_chain` has no access to it.
    pub delivery_offsets: Vec<f64>,
    /// Reference bytes moved out of the JSON into multipart `ref_<n>` parts.
    pub ref_parts: Vec<H3RefPart>,
    /// One prompt per RENDERED GENERATION (not per shot — there is no per-shot render here), for
    /// the job store and the harvest's prompt column.
    pub prompts: Vec<String>,
    /// Keys the saved doc carried that this build did not forward. Distinct from the engine's own
    /// `ignored`: these never reached it. Reported so "I set that and nothing happened" is
    /// answerable without reading the wire.
    pub dropped: Vec<String>,
    /// Things this build accepted but wants said out loud — currently an over-budget canvas.
    ///
    /// Distinct from `dropped` (which is about fields that did nothing) and from the engine's own
    /// `plan.warnings` (which is about what the SHIM noticed): these are koblem's, raised before
    /// the request went anywhere, and they exist so that relaxing a rule from a 400 to guidance
    /// does not turn it into silence.
    pub warnings: Vec<String>,
}

/// The LTX-only keys whose presence in a saved doc is worth REPORTING when an H3 project renders.
///
/// Not a filter — the allowlist already dropped them — but a legacy LTX project switched to H3
/// still carries them, and "your negative prompt / request-level LoRA list / beats did nothing" is
/// the one thing the user cannot see from the output.
///
/// The second block is new with the WanGP contract: these were real fields on the C++ engine and a
/// doc compiled against it still carries them. `sample_method` / `scheduler` are gone because the
/// solver is chosen by the shim's env (`H3_SOLVER`, read only by `h3_sampler.py`); the two sigma
/// shifts are gone because flow shift is fixed at 12.0; `adapt_canvas`, `output_format` and
/// `seconds` are gone because the shim owns the canvas, the codec (x265/mp4) and the frame grid.
const REPORTABLE_LTX_KEYS: [&str; 23] = [
    "negative_prompt",
    "cfg_scale",
    "cfg",
    // 🔴 The REQUEST-level list only. An LTX top-level `loras` is `[{path, multiplier}]` and is
    // still dead here: H3's LoRAs are per GENERATION and are named by bare filename, so there is
    // nothing to translate a request-wide list of engine paths into. The per-generation `loras` is
    // a REAL forwarded field and is deliberately NOT in the item list below.
    "loras",
    "lora",
    "model",
    "hires_chain",
    "upscaler_ratio",
    "profile",
    "resume_job_id",
    "cont_latent_frames",
    "cont_carry_mode",
    "audio_source",
    "frame_drop",
    "tass_phase_scale",
    // ── dead with the WanGP contract ──
    "sample_method",
    "scheduler",
    "sigma_shift_video",
    "sigma_shift_audio",
    "adapt_canvas",
    "output_format",
    "seconds",
    // A doc compiled against the C++ engine still holds the per-SHOT list. Reported by name
    // because the packer works from the timeline's clips, so a stale `segments` is inert rather
    // than merely reshaped — and silently ignoring it would look like the shot list was honoured.
    "segments",
];

/// The same, per item of the doc's shot/generation array.
///
/// 🔴 `loras` is NOT here. It was, while H3 had no LoRA path at all; the shim gained per-generation
/// `loras` + `lora_multipliers` on 2026-08-07 and [`build_chain`] forwards them, so reporting the
/// field as dropped would now be a lie — and a lie in exactly the list a user consults to find out
/// why a setting did nothing.
const REPORTABLE_LTX_ITEM_KEYS: [&str; 10] = [
    "negative_prompt",
    "cfg",
    "model",
    // Prompt Relay is an LTX conditioning layout. On H3 timing lives in the prompt text
    // (`[Shot N] At MM:SS.mmm`), which is a different mechanism and not a translation of this one.
    "beats",
    // Mid-shot keyframes. H3 pins only t=0 and t=duration (FL2VA's `frame_index` accepts `[0]`,
    // `[-1]`, `[0,-1]` and nothing else), so there is no slot for one.
    "keyframes",
    "pin_strength",
    // LTX rotary-tagged latent identities. An H3 reference is text-encoder conditioning the prompt
    // addresses by ordinal — different mechanisms, so translating would look like it worked.
    "character_refs",
    // A cut is free INSIDE a generation and is expressed as prompt text, so a boolean cut flag has
    // nothing to set.
    "scene_cut",
    "init_image",
    "end_image",
];

/// Reported as a group: every `v2v_*` field. H3 has no video-to-video path at all.
const REPORTABLE_LTX_ITEM_PREFIXES: [&str; 1] = ["v2v_"];

/// Build the `/h3/v1/generate` body from a saved H3 timeline request whose `generations` are
/// already packed (see `routes::ltx_director::h3_chain_for`).
///
/// `raw` is the doc's `request` object after staging — every `asset:<hash>` already rewritten to a
/// `part:<name>` marker by the caller, as the LTX path will also do. It is read only to report LTX
/// leftovers; the request itself is built from `req`.
///
/// `staged` carries the bytes those markers stand for. It is a LOOKUP, not the upload list: a
/// document can hold an asset in a field H3 does not read, and only the references this function
/// actually emits get attached.
///
/// `Err` is a 400 the user can act on. Every check here is one the shim would also make: the point
/// of duplicating them is that koblem knows which GENERATION a problem is on and can say what to do
/// about it, where the shim's message can only describe the shape it received.
pub fn build_chain(
    req: &H3Request,
    raw: &Value,
    staged: &director_assets::StagedParts,
) -> Result<H3Chain, String> {
    // Empty-prompt generations are filtered exactly as LTX filters empty segments, so the rendered
    // indices are the ones the Director's persisted render rows use.
    let rendered: Vec<&H3Generation> = req
        .generations
        .iter()
        .filter(|g| !g.prompt.trim().is_empty())
        .collect();
    if rendered.is_empty() {
        return Err(
            "at least one generation prompt is required — H3 has no unconditional path".to_string(),
        );
    }

    // ── Canvas ───────────────────────────────────────────────────────────────
    // Checked once for the job and again for any per-generation override, because a generation that
    // overrides only `width` would otherwise slip past a job-level check that saw a legal pair.
    // The `?` is the 32-grid (model law); the `Some` arm is the area budget (VRAM guidance).
    let mut warnings: Vec<String> = Vec::new();
    if let (Some(w), Some(h)) = (req.width, req.height) {
        warnings.extend(check_canvas(w, h, None)?);
    }

    let mut generations: Vec<Value> = Vec::with_capacity(rendered.len());
    let mut prompts: Vec<String> = Vec::with_capacity(rendered.len());
    let mut ref_parts: Vec<H3RefPart> = Vec::new();

    for (gi, g) in rendered.iter().enumerate() {
        let prompt = g.prompt.trim().to_string();
        // 🔴 REJECTED, never truncated. The six-section schema puts `non_diegetic_music` LAST, so
        // truncating silently drops the entire music direction and the render merely sounds wrong —
        // which is not a failure anyone traces back to a length cap.
        if prompt.chars().count() > H3_MAX_PROMPT_CHARS {
            return Err(format!(
                "generation {} has a {}-character prompt; MiniMax's serving cap is \
                 {H3_MAX_PROMPT_CHARS}. It is refused rather than truncated — truncation would \
                 drop non_diegetic_music, the last section, and the render would just sound wrong",
                gi + 1,
                prompt.chars().count()
            ));
        }

        let mut o = Map::new();
        o.insert("prompt".into(), json!(prompt));
        prompts.push(prompt);

        // Always emitted, never left to the engine's default: the plan echo reports what it
        // snapped to, and a caller that sent nothing cannot tell a 15-second default from its own
        // intent. `H3_MAX_FRAMES` is the contract's documented default (API.md §2).
        let frames = h3_snap_frames(g.frames.unwrap_or(H3_MAX_FRAMES));
        o.insert("frames".into(), json!(frames));
        let duration = h3_seconds_from_frames(frames);
        // Said once, clearly, instead of being rewritten. Until 2026-08-07 anything under 107
        // frames was silently LENGTHENED to 4.458 s, so a 2-second authored beat became a
        // 4.458-second render with no error anywhere. 107 is only the grid-snap of MiniMax's
        // documented 4 s duration guideline; the mechanical floor is 22 (`H3_MIN_FRAMES`).
        if frames < H3_DOCUMENTED_MIN_FRAMES {
            warnings.push(format!(
                "generation {} is {frames} frames ({duration:.2}s), under the 4.0s MiniMax \
                 document as their shortest output. It renders at the length asked for; quality \
                 outside the documented range is unvalidated. (The engine also needs \
                 `H3_SHORT_CLIPS=1` to accept it — without that the shim raises it to \
                 {H3_DOCUMENTED_MIN_FRAMES} and says so in the plan.)",
                gi + 1
            ));
        }

        if let (Some(w), Some(h)) = (g.width, g.height) {
            warnings.extend(check_canvas(w, h, Some(gi))?);
            o.insert("width".into(), json!(w));
            o.insert("height".into(), json!(h));
        } else if g.width.is_some() || g.height.is_some() {
            return Err(format!(
                "generation {} overrides only one of width/height; H3 needs both or neither",
                gi + 1
            ));
        }

        // `< 0` is the engine-random sentinel and the shim understands it, so it is forwarded
        // rather than resolved here — resolving would turn "random" into a pinned value and stop a
        // retake from rolling a different take.
        if let Some(seed) = g.seed {
            o.insert("seed".into(), json!(seed));
        }
        if let Some(steps) = g.steps.filter(|n| *n > 0) {
            o.insert("steps".into(), json!(steps));
        }

        // ── Reference image budget ───────────────────────────────────────────
        //
        // Range-checked HERE and not only at the engine, for the reason every other check in
        // this function is duplicated: the message can name the generation. Refused rather than
        // clamped — 401 and 40 are far more likely typos for 400 and 400 than deliberate
        // choices, and clamping would render at a budget nobody asked for and say nothing.
        //
        // 🔴 What it costs is NOT what the file weighs. `_add_image_reference`
        // (`pipeline.py:210`) throws the upload's dimensions away and canvases to
        // `width * height * this / 100`, so a 4K photo and a thumbnail cost identical DiT rows.
        // This is the only knob that moves them.
        if let Some(rel) = g.image_refs_relative_size {
            if !(H3_REF_RELATIVE_SIZE_MIN..=H3_REF_RELATIVE_SIZE_MAX).contains(&rel) {
                return Err(format!(
                    "generation {} has image_refs_relative_size {rel} — H3 takes \
                     {H3_REF_RELATIVE_SIZE_MIN}-{H3_REF_RELATIVE_SIZE_MAX} \
                     (minimax_h3_handler.py:233)",
                    gi + 1
                ));
            }
            o.insert("image_refs_relative_size".into(), json!(rel));
        }

        // ── LoRAs ────────────────────────────────────────────────────────────
        //
        // A real forwarded field since 2026-08-07, and the reason it is worth having: the turbo
        // LoRA `h3-turbo-4step-ema-pruned.safetensors` at 8 steps is 465.4 s against the 20-step
        // control's 762.8 s — −39% wall for ZERO extra VRAM (6528 MiB both). It does NOT change
        // `steps`; whoever selects it must also lower them, or the render is merely ~5%/step slower
        // for nothing.
        check_loras(g, gi)?;
        if !g.loras.is_empty() {
            // Trimmed on the way out, and the validator above deliberately trims before judging
            // emptiness for the same reason: a name typed into an editor arrives with whatever
            // whitespace came with it, both sides' "is this a bare filename" checks pass it, and
            // the failure lands two layers down as a file that does not exist — after the GPU has
            // been taken.
            let names: Vec<&str> = g.loras.iter().map(|s| s.trim()).collect();
            o.insert("loras".into(), json!(names));
            // Emitted only when the doc actually set them — an absent list means "1.0 for each",
            // which the engine pads, and sending a synthesised all-1.0 list here would make a
            // default indistinguishable from a deliberate choice in the request log.
            if !g.lora_multipliers.is_empty() {
                o.insert("lora_multipliers".into(), json!(g.lora_multipliers));
            }
        }

        // ── Shots (advisory) ─────────────────────────────────────────────────
        o.insert("shots".into(), Value::Array(build_shots(g, gi, duration)?));

        // ── References ───────────────────────────────────────────────────────
        //
        // The generation's inline `first_frame`/`last_frame` become reference entries with the
        // FL2VA keyframe roles, which is how the wire carries them (API.md §2). Translated here
        // rather than authored that way, because the editor holds them as a property of the render
        // and putting them in a user-reorderable array alongside omni references would let a drag
        // produce a generation that needs two checkpoints at once.
        //
        // Prepended, not appended: order assigns ordinals, and the keyframes are the earliest
        // conditioning the generation has. (In practice this never renumbers anything — a
        // generation with keyframes cannot carry omni references at all, which is checked below.)
        let mut refs_in = Vec::with_capacity(g.references.len() + 2);
        for (role, image) in [
            (H3ReferenceRole::FirstFrame, g.first_frame.as_deref()),
            (H3ReferenceRole::LastFrame, g.last_frame.as_deref()),
        ] {
            if let Some(image) = non_empty(image) {
                refs_in.push(H3Reference {
                    kind: H3ReferenceKind::Image,
                    role: Some(role),
                    image: Some(image.to_string()),
                    ..Default::default()
                });
            }
        }
        refs_in.extend(g.references.iter().cloned());

        let (refs, kinds) = build_references(&refs_in, gi, &mut ref_parts, staged)?;
        if !refs.is_empty() {
            o.insert("references".into(), Value::Array(refs));
        }

        // ── Continuation ─────────────────────────────────────────────────────
        // A hard cut is no continuation at all — the shim parses `none` to absent — and sending it
        // made a one-generation retake of a hard-cut render fail the generation-0 `prev` check.
        if let Some(c) = &g.continuation
            && c.mode != H3ContinuationMode::None
        {
            o.insert("continuation".into(), build_continuation(c, gi)?);
        }

        // ── Task ─────────────────────────────────────────────────────────────
        let task = derive_task(g, gi, &kinds)?;
        o.insert("task".into(), json!(task.as_str()));

        generations.push(Value::Object(o));
    }

    let mut chain = Map::new();
    chain.insert("generations".into(), Value::Array(generations));
    if let (Some(w), Some(h)) = (req.width, req.height) {
        chain.insert("width".into(), json!(w));
        chain.insert("height".into(), json!(h));
    }
    // Sent explicitly rather than left to the engine's default: it is the ONE legal value, and
    // stating it means a request that somehow carried another rate fails at submit with the shim's
    // own message instead of rendering at a rate the timeline's arithmetic disagrees with.
    chain.insert("fps".into(), json!(H3_FPS));
    // Off unless the doc asked. `generate_preview` decodes latents through the RGB-factor path per
    // emission and the cost is unmeasured, so it must not silently tax a production render.
    if req.preview.unwrap_or(false) {
        chain.insert("preview".into(), json!(true));
    }

    Ok(H3Chain {
        chain: Value::Object(chain),
        soundtrack: req.soundtrack.clone(),
        // Filled in by the caller, which is where the slice plan lives.
        sliced_for_seconds: None,
        expected_trim_frames: 0,
        delivery_offsets: Vec::new(),
        ref_parts,
        prompts,
        dropped: dropped_ltx_keys(raw),
        warnings,
    })
}

/// Canvas grid (**hard**) + area budget (**advisory**). `gi` names the generation when the failure
/// is an override.
///
/// `Err` refuses the render; `Ok(Some(_))` renders it and reports the string. The two rules are
/// deliberately different in kind:
///
/// - **Multiple of 32 is model law.** The VAE downsamples 16x spatially and the DiT patchifies 2x
///   on top of it, so an off-grid side has no whole-number latent width. Refused, and never
///   snapped — snapping renders a canvas nobody asked for.
/// - **The area number is guidance about VRAM.** `diffusers`' `packing.py:53-54` calls this exact
///   value a *"soft area cap"*, ComfyUI does not enforce it, and no config in the MiniMax weights
///   tree carries a max resolution. We answered 400 on it until 2026-08-07, and the price was
///   that true 16:9 at a 768 short edge (1376x768 = 1,056,768 px, 2.4% over) could not be
///   rendered at all.
pub(crate) fn check_canvas(w: u32, h: u32, gi: Option<usize>) -> Result<Option<String>, String> {
    let where_ = match gi {
        Some(i) => format!("generation {}", i + 1),
        None => "the job canvas".to_string(),
    };
    if !w.is_multiple_of(H3_DIM_MULTIPLE) || !h.is_multiple_of(H3_DIM_MULTIPLE) {
        // Name the offending side and offer the neighbours: the numbers people type are 720 and
        // 1080, and "must be a multiple of 32" alone does not tell them 704/736 and 1088.
        let bad = match (
            w.is_multiple_of(H3_DIM_MULTIPLE),
            h.is_multiple_of(H3_DIM_MULTIPLE),
        ) {
            (false, true) => format!("{w} is not"),
            (true, false) => format!("{h} is not"),
            _ => format!("neither {w} nor {h} is"),
        };
        let down = |n: u32| n / H3_DIM_MULTIPLE * H3_DIM_MULTIPLE;
        // A side that is already on the grid is left alone in BOTH neighbours. Rounding it up too
        // would offer a canvas that moves a dimension the caller got right — for `1000x768` the up
        // neighbour is `1024x768`, not `1024x800`.
        let up = |n: u32| {
            if n.is_multiple_of(H3_DIM_MULTIPLE) {
                n
            } else {
                down(n) + H3_DIM_MULTIPLE
            }
        };
        return Err(format!(
            "{where_} is {w}x{h}, which is not a legal H3 canvas — {bad} a multiple of \
             {H3_DIM_MULTIPLE}. H3's VAE downsamples 16x and the DiT patchifies 2x on top of it, \
             so both sides have to divide by {H3_DIM_MULTIPLE} exactly. Nearest legal sizes: \
             {}x{} or {}x{}",
            down(w),
            down(h),
            up(w),
            up(h)
        ));
    }
    if w.saturating_mul(h) > H3_ADVISORY_MAX_PIXELS {
        return Ok(Some(format!(
            "{where_} is {w}x{h} = {} pixels, over the {H3_ADVISORY_MAX_PIXELS}-pixel budget \
             1344x768 was measured at. This is ADVISORY, not a limit — the model has no maximum \
             resolution — but a much larger canvas may not fit the 16 GB card, and every timing \
             and VRAM number we have is for 1344x768. Rendering anyway",
            w as u64 * h as u64
        )));
    }
    Ok(None)
}

/// The engine's own cap on either list (`contract.py:346`, `max_length=8`). Cost scales with the
/// NUMBER of LoRAs — runtime adapter cost is activation volume, not rank — so this is a real
/// ceiling and not a round number.
const H3_MAX_LORAS: usize = 8;

/// The per-generation LoRA list, checked exactly as `contract.py::Generation::_loras_coherent`
/// checks it.
///
/// Duplicated rather than left to the shim for the usual reason: its 400 can only describe the
/// shape it received, and every one of these failures is a property of ONE generation in a chain of
/// up to sixteen. None of them is cosmetic — each is a shape that would otherwise half-apply or
/// apply twice, and a LoRA that misbehaves on H3 presents as "the render looks 20-step" rather than
/// as an error.
fn check_loras(g: &H3Generation, gi: usize) -> Result<(), String> {
    let n = gi + 1;
    if g.loras.len() > H3_MAX_LORAS {
        return Err(format!(
            "generation {n} lists {} LoRAs; the engine takes at most {H3_MAX_LORAS}",
            g.loras.len()
        ));
    }
    if g.lora_multipliers.len() > H3_MAX_LORAS {
        return Err(format!(
            "generation {n} lists {} lora_multipliers; the engine takes at most {H3_MAX_LORAS}",
            g.lora_multipliers.len()
        ));
    }
    if !g.lora_multipliers.is_empty() && g.loras.is_empty() {
        return Err(format!(
            "generation {n} sets lora_multipliers but no loras — there is nothing for them to \
             multiply, so the render would silently be the one you did not ask for"
        ));
    }
    if g.lora_multipliers.len() > g.loras.len() {
        return Err(format!(
            "generation {n} has {} lora_multipliers for {} loras — they are matched POSITIONALLY, \
             so an extra multiplier means the list you meant is not the list you sent. (A SHORTER \
             list is fine: the missing ones are 1.0.)",
            g.lora_multipliers.len(),
            g.loras.len()
        ));
    }
    for (i, raw) in g.loras.iter().enumerate() {
        // Judged on the TRIMMED name, and sent trimmed too (`build_chain`): the checks and the
        // wire have to agree about what the name is, or a stray space passes every rule here and
        // fails as a missing file inside the engine.
        let name = raw.trim();
        // A bare filename inside the engine's mounted loras dir, nothing else: a path would
        // resolve against WanGP's loras root and could escape it.
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
            return Err(format!(
                "generation {n} lora {i} is {raw:?}; it must be a bare filename inside the \
                 engine's loras dir (e.g. `h3-turbo-4step-ema-pruned.safetensors`), never a path"
            ));
        }
        if g.loras[..i].iter().any(|earlier| earlier.trim() == name) {
            return Err(format!(
                "generation {n} lists lora {name:?} twice — its effect would be applied twice, and \
                 it costs a second adapter's worth of time to do it"
            ));
        }
    }
    Ok(())
}

/// The advisory `shots[]`, validated against the generation's real duration.
///
/// These rules are the model's, not ours: `[Shot 1]` is untimed, and every later shot carries an
/// ABSOLUTE cut time that is strictly increasing and inside the duration. A cut past the end is the
/// interesting failure — the model renders a generation that simply never reaches the second shot,
/// and the output looks like the prompt was ignored rather than like a timing error.
fn build_shots(g: &H3Generation, gi: usize, duration: f64) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(g.shots.len());
    let mut last_at = f64::NEG_INFINITY;
    for (n, s) in g.shots.iter().enumerate() {
        let mut o = Map::new();
        o.insert("index".into(), json!(s.index));
        if n == 0 {
            // Position, not `index`: a doc could number its shots from 1. What matters is that the
            // FIRST shot of the generation carries no cut time, because there is nothing to cut
            // from.
            if s.at_seconds.is_some_and(|v| v != 0.0) {
                return Err(format!(
                    "generation {}: its first shot is untimed — `[Shot 1] At …` is not a thing the \
                     model parses, the generation simply starts there",
                    gi + 1
                ));
            }
        } else {
            let Some(at) = s.at_seconds else {
                return Err(format!(
                    "generation {}: shot {} needs an absolute cut time (MM:SS.mmm from the START \
                     of the generation, not of the timeline)",
                    gi + 1,
                    n + 1
                ));
            };
            if at <= last_at {
                return Err(format!(
                    "generation {}: shot {} cuts at {at:.3}s, at or before shot {} — cut times must \
                     be strictly increasing",
                    gi + 1,
                    n + 1,
                    n
                ));
            }
            if at >= duration {
                return Err(format!(
                    "generation {}: shot {} cuts at {at:.3}s, outside its {duration:.3}s duration — \
                     that shot would never render. Shorten the earlier shots or split the \
                     generation",
                    gi + 1,
                    n + 1
                ));
            }
            last_at = at;
            o.insert("at_seconds".into(), json!(at));
        }
        if let Some(label) = non_empty(s.label.as_deref()) {
            o.insert("label".into(), json!(label));
        }
        out.push(Value::Object(o));
    }
    Ok(out)
}

/// The per-kind counts a generation's references came to, so the task derivation and the
/// audio/visual rule can both read them without walking the list again.
#[derive(Default, Clone, Copy)]
struct RefKinds {
    images: usize,
    videos: usize,
    audios: usize,
    keyframes: usize,
    first_frames: usize,
    last_frames: usize,
    //
    // 🔴 THERE IS NO `soundtracked_videos` HERE ANY MORE. It existed to count reference videos
    // with `with_audio` so `build_references` could refuse them alongside standalone audio — a
    // WanGP rule (`audio_prompt_type` was one letter set and the two modes were exclusive). The
    // engine wires `ref_video_audios` and `ref_audios` as independent autogrow groups now, so
    // there is nothing to count and nothing to refuse. Removed with the check, 2026-08-21.
}

impl RefKinds {
    fn total(self) -> usize {
        self.images + self.videos + self.audios
    }
    fn visual(self) -> usize {
        self.images + self.videos
    }
    /// Omni (Ref2VA) references: everything that is not an FL2VA keyframe.
    fn omni(self) -> usize {
        self.total() - self.keyframes
    }
}

/// The reference array as the shim wants it, plus every reference's bytes lifted into a multipart
/// part.
///
/// ORDER IS PRESERVED EXACTLY. A reference's position assigns its ordinal and advances a shared
/// rotary clock, so sorting, de-duplicating or grouping by kind would silently change the render.
/// The only thing this does is validate and re-shape each entry in place.
///
/// `ref_parts` is appended to across ALL generations: multipart field names are flat, so a part name
/// is unique in the whole request and each reference carries its own `"part"` back-pointer. It is
/// keyed by name, not by position, so two references standing for the same asset share one upload.
///
/// `staged` holds the bytes behind every `part:<name>` marker `stage_refs` wrote into the document.
fn build_references(
    refs: &[H3Reference],
    gi: usize,
    ref_parts: &mut Vec<H3RefPart>,
    staged: &director_assets::StagedParts,
) -> Result<(Vec<Value>, RefKinds), String> {
    if refs.len() > H3_MAX_TOTAL_REFS {
        return Err(format!(
            "generation {} has {} references — H3 takes at most {H3_MAX_TOTAL_REFS} total",
            gi + 1,
            refs.len()
        ));
    }

    let mut kinds = RefKinds::default();
    let mut out: Vec<Value> = Vec::with_capacity(refs.len());

    for (i, r) in refs.iter().enumerate() {
        let ordinal = i + 1;
        let named = format!("generation {} reference {ordinal}", gi + 1);
        let mut o = Map::new();
        o.insert("kind".into(), json!(kind_str(r.kind)));
        // 🔴 MAPPED DOWN, not forwarded. koblem's role vocabulary is richer than the engine's
        // (eleven values against five) because the extra ones are prompt-craft distinctions — an
        // `edit_source` and a `structure_source` produce different `summary:` task types and
        // different `retention_analysis` parentheticals, but the engine has no different SLOT to
        // put them in. Widening `app/contract.py::RefRole` to match would add five values it would
        // collapse anyway, on the one surface where every extra value is another thing
        // `validate_generation` can disagree about.
        //
        // `effective_role()` and not `role` directly: an absent role defaults per KIND, and
        // reading the absence differently here than the prompt path does would make the same
        // reference mean two things in one render.
        let role = r.effective_role();
        o.insert("role".into(), json!(role.engine_role()));

        if role.is_keyframe() {
            kinds.keyframes += 1;
            match role {
                H3ReferenceRole::FirstFrame => kinds.first_frames += 1,
                H3ReferenceRole::LastFrame => kinds.last_frames += 1,
                _ => {}
            }
        }

        // 🔴 The C++ engine took a decoded FRAME ARRAY; WanGP's `_add_video_reference` takes a
        // file. There is no frame-array path on the wire, so a doc still holding one is refused by
        // name rather than having its array silently dropped and its reference conditioned on
        // nothing.
        //
        // Ahead of the source count, and that is the whole point: `frames` is not a source, so a
        // legacy doc scores zero and would otherwise get the generic "carries 0 sources" — the
        // uninformative answer this message exists to replace.
        if r.kind == H3ReferenceKind::Video && r.video.is_none() && r.frames.is_some() {
            return Err(format!(
                "{named} is a video supplied as a FRAME ARRAY. The engine takes a container \
                 now (`video`: a webm/mp4 or an asset ref) — re-add the clip"
            ));
        }

        // Exactly one of the payload / `path` / `bank`. Zero is a reference that conditions on
        // nothing; two is a request the shim rejects, and choosing one for the caller would be
        // choosing which of two things they meant.
        let sources = r.source_count();
        if sources != 1 {
            return Err(format!(
                "{named} carries {sources} sources — each reference needs exactly one of its own \
                 bytes, a `path` the engine can see, or a `bank` key"
            ));
        }

        match r.kind {
            H3ReferenceKind::Image => {
                kinds.images += 1;
                // Only a VIDEO reference may carry a soundtrack; an image reference that does fails
                // the whole generation engine-side, so it is caught here where the ordinal is known.
                if r.audio.is_some() {
                    return Err(format!(
                        "{named} is an image carrying a soundtrack — only a video reference may. \
                         Add it as its own audio reference instead"
                    ));
                }
            }
            H3ReferenceKind::Video => {
                kinds.videos += 1;
            }
            H3ReferenceKind::Audio => {
                kinds.audios += 1;
            }
        }

        // `with_audio` and `tail_frames` are video-only, and both are silent when misapplied: an
        // image with `with_audio` would shift every later `<Audio n>` ordinal by one if the shim
        // honoured it, and a prompt addressing `<Audio 2>` would then be describing something else.
        if r.kind != H3ReferenceKind::Video {
            if r.with_audio {
                return Err(format!(
                    "{named}: `with_audio` is meaningful on video references only"
                ));
            }
            if r.tail_frames.is_some() {
                return Err(format!(
                    "{named}: `tail_frames` is meaningful on video references only"
                ));
            }
        }
        if r.with_audio {
            o.insert("with_audio".into(), json!(true));
        }
        if let Some(tail) = r.tail_frames {
            // 🔴 ACCEPTED, NOT POLICED — RELAXED 2026-08-21. This used to answer 400 to any
            // `tail_frames` off the `5+17k` grid or outside `[56, 345]`, because WanGP's
            // `_add_video_reference` truncated to the grid keeping the FIRST frames and the model
            // was then told a lie about time.
            //
            // `tail_frames` reaches nothing on the ComfyUI path. The engine validates only that it
            // is on a VIDEO reference, uses it in the ETA row estimate, and applies it to no node
            // at all — it publishes `references.tail_frames.applied = false` saying so. The node
            // decides a reference's real length from the FILE: truncate to the generation, then
            // snap down to 5+17k. So the refusal was rejecting requests over a field that changes
            // no pixels.
            //
            // The grid is still published as `legal_tails` for the editor's picker, because a tail
            // ON the grid is still the one whose ETA is right and whose meaning survives if the
            // field is ever wired again.
            o.insert("tail_frames".into(), json!(tail));
        }
        if let Some(label) = non_empty(r.label.as_deref()) {
            o.insert("label".into(), json!(label));
        }

        // The bytes ride a multipart part rather than the JSON: a reference video inlined as base64
        // costs a third again on a request that may carry a dozen of them. A `path` or a `bank`
        // passes through untouched — neither is bytes.
        if let Some(path) = non_empty(r.path.as_deref()) {
            o.insert("path".into(), json!(path));
        } else if let Some(bank) = non_empty(r.bank.as_deref()) {
            o.insert("bank".into(), json!(bank));
        } else {
            let payload = match r.kind {
                H3ReferenceKind::Image => r.image.as_deref(),
                H3ReferenceKind::Video => r.video.as_deref(),
                H3ReferenceKind::Audio => r.audio.as_deref(),
            };
            let Some(payload) = non_empty(payload) else {
                return Err(format!("{named} has no attached {}", kind_str(r.kind)));
            };
            // A `part:<name>` marker is the normal path: `stage_refs` already read the asset off
            // disk and holds its bytes, so the reference just names the part. Attaching it once per
            // NAME is what makes a character reference used in five generations one upload instead
            // of five — the shim resolves a reference's bytes with a dict lookup
            // (`docker/h3-wangp/app/main.py:388`), so several references may name the same part.
            if let Some(name) = director_assets::part_name(payload) {
                let Some(bytes) = staged.bytes(name) else {
                    // Only reachable if a document hand-wrote a `part:` marker: `stage_refs` names
                    // the parts it creates, so one it doesn't hold was never staged.
                    return Err(format!(
                        "{named} names multipart part `{name}`, which is not attached to this \
                         request"
                    ));
                };
                if !ref_parts.iter().any(|p| p.name == name) {
                    let (ext, mime) = part_media(r.kind, bytes);
                    ref_parts.push(H3RefPart {
                        file_name: format!("{name}.{ext}"),
                        mime: mime.to_string(),
                        bytes: bytes.to_vec(),
                        name: name.to_string(),
                    });
                }
                o.insert("part".into(), json!(name));
            } else {
                match decode_b64(payload) {
                    // A legacy document still holding literal base64. Staging never saw it — it is
                    // not a ref — so the bytes arrive here and get their own part, named beyond the
                    // staged range so it cannot collide with one.
                    Some(bytes) => {
                        let name = format!("ref_b{}", ref_parts.len());
                        let (ext, mime) = part_media(r.kind, &bytes);
                        ref_parts.push(H3RefPart {
                            file_name: format!("{name}.{ext}"),
                            mime: mime.to_string(),
                            bytes: bytes.to_vec(),
                            name: name.clone(),
                        });
                        o.insert("part".into(), json!(name));
                    }
                    // Not a marker and not base64 — treat it as the absolute path the shim also
                    // accepts, and let it say whether the path exists. An unresolvable
                    // `asset:<hash>` no longer lands here: `stage_refs` fails the whole request
                    // with `missing_refs_message`, which names the asset instead of asking the
                    // shim to report a hash as a missing filename.
                    None => {
                        o.insert("path".into(), json!(payload));
                    }
                }
            }
        }

        // A video reference carrying its own soundtrack as a SEPARATE payload is not expressible:
        // the container carries it, and `with_audio` decides whether it becomes an `<Audio n>`.
        if r.kind == H3ReferenceKind::Video
            && r.video.is_some()
            && non_empty(r.audio.as_deref()).is_some()
        {
            return Err(format!(
                "{named} is a video with a separate soundtrack attached. The container's own audio \
                 is what the engine reads — set `with_audio` to give it an `<Audio n>` ordinal, or \
                 add the track as its own audio reference"
            ));
        }

        out.push(Value::Object(o));
    }

    if kinds.images > H3_MAX_IMAGE_REFS {
        return Err(format!(
            "generation {} has {} image references — H3 takes at most {H3_MAX_IMAGE_REFS}",
            gi + 1,
            kinds.images
        ));
    }
    // 🔴 3, not 2 — restored 2026-08-21. The 2 was WanGP's
    // `validate_generative_settings:260-319`, which held two named slots per kind and refused a
    // third in its own name. ComfyUI replaced WanGP on 2026-08-12 and its `ref_videos` /
    // `ref_audios` are autogrow inputs declaring `max: 3`, which is also MiniMax's documented
    // figure and what the engine's own contract enforces.
    //
    // ⚠️ THIS IS THE HALF THAT CANNOT READ THE ENGINE. `props` above resolves its answer from the
    // live capabilities document; validation runs at submit with nothing fetched, so it is the
    // compiled-in constant or nothing. That asymmetry is exactly why `capability_drift` is warned
    // about: when the two disagree, the UI is already right and THIS is what will wrongly refuse.
    if kinds.videos > H3_MAX_VIDEO_REFS {
        return Err(format!(
            "generation {} has {} video references — H3 takes at most {H3_MAX_VIDEO_REFS}",
            gi + 1,
            kinds.videos
        ));
    }
    if kinds.audios > H3_MAX_AUDIO_REFS {
        return Err(format!(
            "generation {} has {} audio references — H3 takes at most {H3_MAX_AUDIO_REFS}",
            gi + 1,
            kinds.audios
        ));
    }
    // 🔴 THE LAST BAKED WanGP LIMITATION, LIFTED 2026-08-21. There used to be a refusal here.
    //
    // On WanGP a reference video's OWN soundtrack and a standalone audio reference were two MODES,
    // not two sources: `audio_prompt_type` was one letter set, `"K"` (soundtracks) excluded
    // `"A"`/`"AB"` (files), and the shim picked `"K"` the moment any video set `with_audio` — then
    // never wrote `audio_guide` at all, silently dropping every standalone track and leaving the
    // prompt addressing an `<Audio n>` the conditioner never packed. Refusing was right.
    //
    // It is not the engine's behaviour any more. `MiniMaxH3ReferenceToVideo` declares four
    // INDEPENDENT autogrow groups — `ref_images`, `ref_videos`, `ref_video_audios`, `ref_audios` —
    // and `execute` walks all of them, encoding a soundtrack and a standalone file through the
    // same `_encode_ref_audio`. `graph_build.apply_references` wires both. The engine's own
    // `contract._parse_references` is the authority on what may be sent, and its ONLY rule here is
    // MiniMax's "audio cannot be the sole input", checked below. There is nothing else to mirror.
    //
    // ⚠️ ORPHAN SLOTS ARE STRUCTURALLY IMPOSSIBLE, which is what made this safe to delete. The
    // node pairs a soundtrack to its video BY INDEX (`ref_video_audios.get("ref_video_audio_" +
    // name.rsplit("_", 1)[-1])`), and `with_audio` is a flag ON a video reference — there is no
    // way to express a bare soundtrack, so `ref_video_audio_N` can only ever exist alongside the
    // `ref_video_N` that produced it.
    //
    // The ORDINALS were the other blocker and they are fixed: `<Audio j>` is one counter over the
    // node's emission order (soundtracks in video order, then standalone files), which the shim
    // now derives from a single `contract.audio_ordinal_plan`.
    // 🔴 MiniMax's own rule (`README:60`) and nothing stricter: audio "cannot be the sole
    // input". The engine refuses exactly this and no more.
    //
    // 🔴 THIS USED TO DEMAND `audios <= visuals`, which was WanGP's
    // `validate_generative_settings` and is not a rule anywhere any more. It refused, in koblem,
    // a generation the engine would have taken — one identity plate plus two audio references
    // being the obvious shape — and like every other cap of that kind the user was told a limit
    // that did not exist. Relaxed 2026-08-21 to the engine's actual rule.
    if kinds.audios > 0 && kinds.visual() == 0 {
        return Err(format!(
            "generation {} has {} audio reference(s) and no image or video — MiniMax require audio \
             not be the sole input. Add an image or video reference, or use task=t2va with no \
             references at all",
            gi + 1,
            kinds.audios
        ));
    }
    if kinds.first_frames > 1 || kinds.last_frames > 1 {
        return Err(format!(
            "generation {} has more than one first_frame or last_frame — fl2va pins exactly t=0 \
             and t=duration ({} first, {} last)",
            gi + 1,
            kinds.first_frames,
            kinds.last_frames
        ));
    }
    Ok((out, kinds))
}

/// Which checkpoint this generation runs on, derived from the conditioning actually present.
///
/// 🔴 **`fl2va` and `ref2va` are different CHECKPOINTS, not a flag** — separate weights
/// (`minimax_h3_handler.py:25-28`). One generation cannot carry both keyframes and omni references,
/// and switching between them inside a job costs a full DiT + text-encoder reload
/// (`wgp.py:6614-6617` is never incremental), which the plan echo reports as `reload_required`.
///
/// A stated `task` is treated as an ASSERTION and checked, not obeyed: obeying it would let a
/// hand-edited or older document render on the other checkpoint with its conditioning silently
/// unused, which produces a plausible clip and no error.
fn derive_task(g: &H3Generation, gi: usize, kinds: &RefKinds) -> Result<H3Task, String> {
    if kinds.keyframes > 0 && kinds.omni() > 0 {
        return Err(format!(
            "generation {} has both a first/last frame and an omni reference. H3 renders \
             references (ref2va) and keyframes (fl2va) on DIFFERENT CHECKPOINTS, not on one model \
             with a flag — so a generation can use one or the other. Drop the frame pin, or split \
             it into two generations",
            gi + 1
        ));
    }
    if kinds.keyframes > 0 && kinds.images < kinds.keyframes {
        return Err(format!(
            "generation {}: first_frame/last_frame must be IMAGES — a keyframe latent is \
             re-injected every step and never denoised, which only a still can be",
            gi + 1
        ));
    }

    // A continuation is a Ref2VA mechanism. On fl2va/t2va it is inert (measured: 5% / -2% / 3%,
    // indistinguishable from no pin), so a generation that asks for one is asking for ref2va.
    let continues = g
        .continuation
        .as_ref()
        .is_some_and(|c| c.mode != H3ContinuationMode::None);

    let derived = if kinds.keyframes > 0 {
        H3Task::Fl2va
    } else if kinds.omni() > 0 || continues {
        H3Task::Ref2va
    } else {
        H3Task::T2va
    };

    // ref2va with references but nothing visual: audio-only conditioning is invalid. The shim would
    // also refuse it, but only after taking the GPU.
    if derived == H3Task::Ref2va && kinds.total() > 0 && kinds.visual() == 0 {
        return Err(format!(
            "generation {} has only audio references. H3 reference conditioning needs at least one \
             image or video — audio alone has nothing to attach to",
            gi + 1
        ));
    }

    if let Some(stated) = g.task
        && stated != derived
    {
        return Err(format!(
            "generation {} states task `{}` but its conditioning derives `{}`. The task follows \
             the references, not the other way round — a mismatch would render on the wrong \
             checkpoint and silently ignore the conditioning",
            gi + 1,
            stated.as_str(),
            derived.as_str()
        ));
    }
    Ok(derived)
}

/// The cross-generation continuation block.
///
/// Only meaningful ACROSS a boundary — inside a generation continuity is free and a cut is a
/// `[Shot N]` marker. Neither mode is frame-exact: MiniMax say Ref2VA *"treats the input video as
/// reference material, not as a pixel-aligned edit source"* and expose no denoising-strength
/// control, so our latent pin is an out-of-distribution patch of our own and the best known path —
/// not the model's contract. Nothing in this stack may promise LTX-style exactness.
fn build_continuation(c: &H3Continuation, gi: usize) -> Result<Value, String> {
    // `"prev"` is resolved by the shim at RUN time, which is what makes it survive a retake that
    // re-renders an earlier generation. On generation 0 there is nothing for it to resolve to.
    if gi == 0 && c.from.trim() == H3_CONTINUATION_FROM_PREV {
        return Err(
            "generation 1 continues from `prev`, but it is the first generation of the job. Point \
             it at a bank key, or drop the continuation"
                .to_string(),
        );
    }
    if c.from.trim().is_empty() {
        return Err(format!(
            "generation {}: continuation.from is empty — use `prev` or a bank key",
            gi + 1
        ));
    }
    // K=7 (22 pixel frames) is the saturation point: K=2 scores 6%, K=7 91%, K=12 91%, K=17 93% —
    // and K=17 costs 2.4x K=7's conditioning rows. The cliff is between 2 and 7, so the range is
    // about what the shim accepts rather than about what is worth using.
    if c.k == 0 || c.k > 21 {
        return Err(format!(
            "generation {}: continuation k={} is outside 1-21 (K=7 is the saturation point; K=17 \
             costs 2.4x its conditioning rows for 2% more)",
            gi + 1,
            c.k
        ));
    }
    // 🔴 FOUR FIELDS, NOT SEVEN. `tail_frames`, `drop_guide` and `posterior_noise` were WanGP
    // continuation fields and every one of them was emitted here UNCONDITIONALLY, from a struct
    // default the author had never touched — so the shim answered every continued shot with
    // "tail_frames=56 is a WanGP field" and "drop_guide has no ComfyUI analogue", and koblem
    // showed both to the human as if the render had gone wrong. The carry's depth is `k` and
    // only `k`; there is no video guide to drop; the noise knob there is a MIX FACTOR with
    // different units. Nothing was ever lost by not sending them.
    Ok(json!({
        "mode": c.mode.as_str(),
        "from": c.from.trim(),
        "k": c.k,
        "anchor": c.anchor,
    }))
}

fn kind_str(k: H3ReferenceKind) -> &'static str {
    match k {
        H3ReferenceKind::Image => "image",
        H3ReferenceKind::Video => "video",
        H3ReferenceKind::Audio => "audio",
    }
}

/// Extension + MIME for a reference part, from the bytes' MAGIC NUMBER.
///
/// The extension is load-bearing rather than cosmetic: WanGP hands reference files to decoders that
/// pick a demuxer by suffix, so a webm named `.mp4` is a decode failure two layers below anything
/// that could name the reference. Sniffing rather than trusting the doc, because the payload
/// reached us as base64 with no type attached — and falling back to the kind's most likely format
/// rather than to `application/octet-stream`, which nothing downstream can open at all.
pub(crate) fn part_media(k: H3ReferenceKind, bytes: &[u8]) -> (&'static str, &'static str) {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => ("png", "image/png"),
        [0xFF, 0xD8, 0xFF, ..] => ("jpg", "image/jpeg"),
        [b'R', b'I', b'F', b'F', ..] if bytes.get(8..12) == Some(&b"WEBP"[..]) => {
            ("webp", "image/webp")
        }
        [b'R', b'I', b'F', b'F', ..] if bytes.get(8..12) == Some(&b"WAVE"[..]) => {
            ("wav", "audio/wav")
        }
        [b'I', b'D', b'3', ..] | [0xFF, 0xFB, ..] => ("mp3", "audio/mpeg"),
        [b'O', b'g', b'g', b'S', ..] => ("ogg", "audio/ogg"),
        [b'f', b'L', b'a', b'C', ..] => ("flac", "audio/flac"),
        [0x1A, 0x45, 0xDF, 0xA3, ..] => ("webm", "video/webm"),
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => ("mp4", "video/mp4"),
        _ => match k {
            H3ReferenceKind::Image => ("png", "image/png"),
            H3ReferenceKind::Video => ("mp4", "video/mp4"),
            H3ReferenceKind::Audio => ("wav", "audio/wav"),
        },
    }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|v| !v.is_empty())
}

fn decode_b64(s: &str) -> Option<Bytes> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let payload = s.rsplit_once(";base64,").map(|(_, b)| b).unwrap_or(s);
    base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        payload.trim().as_bytes(),
    )
    .ok()
    .map(Bytes::from)
}

/// LTX-only (and now C++-engine-only) keys the saved doc carried, in document order. Used for the
/// "this did nothing" report; the allowlist above is what actually keeps them off the wire.
///
/// Scans BOTH `generations[]` and the legacy `segments[]`, because a doc compiled against the C++
/// engine has the latter and its per-shot leftovers are exactly the ones worth naming.
fn dropped_ltx_keys(raw: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(map) = raw.as_object() else {
        return out;
    };
    for key in REPORTABLE_LTX_KEYS {
        if map.get(key).is_some_and(carries_a_value) {
            out.push(key.to_string());
        }
    }
    for array_name in ["generations", "segments"] {
        let Some(items) = map.get(array_name).and_then(Value::as_array) else {
            continue;
        };
        for key in REPORTABLE_LTX_ITEM_KEYS {
            if items
                .iter()
                .any(|s| s.get(key).is_some_and(carries_a_value))
            {
                out.push(format!("{array_name}[].{key}"));
            }
        }
        for prefix in REPORTABLE_LTX_ITEM_PREFIXES {
            if items.iter().any(|s| {
                s.as_object().is_some_and(|o| {
                    o.iter()
                        .any(|(k, v)| k.starts_with(prefix) && carries_a_value(v))
                })
            }) {
                out.push(format!("{array_name}[].{prefix}*"));
            }
        }
    }
    out
}

/// Did the doc actually SET this, or is it a null / empty default? Reporting `"negative_prompt": ""`
/// as dropped would train the user to ignore the list.
fn carries_a_value(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Bool(b) => *b,
        Value::Number(_) => true,
    }
}

// ─── The dispatch ──────────────────────────────────────────────────────────────

/// What the queue row has to carry beside the chain.
pub(crate) struct H3Dispatch {
    /// The [`crate::ltx_jobs::LtxJobStore`] record. A DIFFERENT id space from the queue row's: every
    /// generation and the finished clip land under this one, so the URL the Director is already
    /// polling never moves.
    pub job_id: String,
    pub save_clip: bool,
    pub director_project_id: String,
    pub director_tier: String,
    pub director_shot_offset: Option<i32>,
    pub director_wipe: bool,
    /// Offer this render to the unattended dailies review once the harvest has filed it. True only
    /// on a FULL run — a retake that chained into a review would let the critic's own re-rolls
    /// recurse.
    pub director_critique: bool,
}

/// Put a built H3 chain on the global queue and return the store id the caller should hand back.
///
/// 🔴 The reference bytes leave the request here. A cut dispatched at 03:00 has no browser behind it,
/// so every `ref_<n>` part and the project's own copy of the song become `asset:<hash>` on the way
/// in and the adapter reads them back at dispatch.
///
/// `sliced_for_seconds` and `expected_trim_frames` ride in the envelope for the seam guard, which
/// runs in `queue::adapters::h3` because it needs the completion body's `H3TrimReport` as well.
///
/// The plan echo `stamp_h3_plan` used to write onto the project does NOT ride along: it only exists
/// after submit, which is now hours later and in another task.
pub(crate) async fn enqueue_h3_render(
    state: &AppState,
    who: &crate::auth::Principal,
    chain: H3Chain,
    dispatch: H3Dispatch,
    opts: crate::routes::queue::QueueOptions,
) -> Result<String, (axum::http::StatusCode, String)> {
    let H3Chain {
        chain,
        soundtrack,
        sliced_for_seconds,
        expected_trim_frames,
        delivery_offsets,
        ref_parts,
        prompts: _,
        dropped,
        warnings,
    } = chain;
    if !dropped.is_empty() {
        // Not a failure: the render is valid, these fields simply have no meaning on this engine.
        warn!(job = %dispatch.job_id, dropped = ?dropped, "h3 render: fields in the saved doc were not sent");
    }
    if !warnings.is_empty() {
        // Rules that used to be 400s and are now guidance — an over-budget canvas, a sub-4s
        // generation. Relaxing a rule must not make it silent.
        warn!(job = %dispatch.job_id, warnings = ?warnings, "h3 render: accepted with warnings");
    }

    let parts: Vec<(String, Bytes)> = ref_parts
        .into_iter()
        .map(|p| (p.name, Bytes::from(p.bytes)))
        .collect();
    let assets = super::ltx_video::capture_parts(state, &parts)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // 🔴 The song is CAPTURED, not carried. A legacy document holds it as literal base64, which for
    // an 8 MB track is 11 MB of string in a JSONB column that has to survive until 03:00.
    let soundtrack = match soundtrack {
        Some(mut st) => {
            st.audio = director_assets::capture_ref(
                state.director_assets_dir.as_ref(),
                &state.db,
                &st.audio,
            )
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("soundtrack: {e}")))?;
            Some(st)
        }
        None => None,
    };

    let payload = json!({
        "chain": chain,
        "job_id": dispatch.job_id,
        "assets": assets,
        "soundtrack": soundtrack,
        "delivery_offsets": delivery_offsets,
        "sliced_for_seconds": sliced_for_seconds,
        "expected_trim_frames": expected_trim_frames,
        "save_clip": dispatch.save_clip,
        "director_project_id": dispatch.director_project_id,
        "director_tier": dispatch.director_tier,
        "director_shot_offset": dispatch.director_shot_offset,
        "director_wipe": dispatch.director_wipe,
    });
    let accepted = crate::routes::queue::enqueue(
        state,
        who,
        opts.request(koblem_client::types::JobKind::H3, payload),
    )
    .await
    .inspect_err(|(_, msg)| {
        state.ltx_jobs.update(&dispatch.job_id, |j| {
            j.state = VideoJobState::Failed;
            j.error = Some(msg.clone());
        });
    })?;
    state.ltx_jobs.update(&dispatch.job_id, |j| {
        j.queue_job_id = Some(accepted.job_id.clone());
        j.director_critique = dispatch.director_critique;
    });
    Ok(dispatch.job_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(v: Value) -> H3Request {
        serde_json::from_value(v).expect("request parses")
    }

    /// These cases feed documents holding literal base64, never `part:` markers — they are about
    /// the SHAPE of a chain, and staging is exercised where it lives (`director_assets`). Shadowing
    /// the real `build_chain` with an empty staging table keeps every case reading as it did, and
    /// still proves the legacy-base64 branch survives the change: an empty table means every
    /// reference in here must find its bytes the old way.
    fn build_chain(req: &H3Request, raw: &Value) -> Result<H3Chain, String> {
        super::build_chain(req, raw, &director_assets::StagedParts::default())
    }

    /// One legal 15 s generation, as a base for the tests that vary one thing.
    fn one_gen(extra: Value) -> Value {
        let mut o = json!({ "prompt": "a wide establishing shot", "frames": 362 });
        if let (Some(dst), Some(src)) = (o.as_object_mut(), extra.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        json!({ "generations": [o] })
    }

    /// The engine snaps UP and clamps at both ends. The GRID is model law (`5 + 17k`, from
    /// `vae_clip_length`); the floor is the VAE's own decodable minimum, **22**, not the 107 this
    /// used to assert.
    ///
    /// 🔴 107 was the grid-snap of MiniMax's documented 4-second DURATION guideline, which WanGP
    /// hardened into a `raise`. Enforcing it here silently LENGTHENED every sub-4s beat.
    #[test]
    fn frames_snap_up_onto_the_grid_and_clamp_to_the_generation_budget() {
        // 22 frames = 7 latents = the first block the video VAE decodes at all. Below it a
        // 2-latent block yields 0 chunks and the decoder raises, so there IS no render to offer.
        assert_eq!(H3_MIN_FRAMES, 22);
        assert_eq!(h3_snap_frames(0), H3_MIN_FRAMES);
        assert_eq!(h3_snap_frames(5), H3_MIN_FRAMES);
        // …and nothing above the floor is rewritten any more: a 2-second beat stays 2 seconds.
        assert_eq!(h3_snap_frames(48), 56);
        assert_eq!(h3_snap_frames(96), 107);
        assert_eq!(h3_snap_frames(107), 107);
        assert_eq!(h3_snap_frames(108), 124);
        assert_eq!(h3_snap_frames(124), 124);
        // …and over the cap it clamps rather than growing; the packer is what stops this happening.
        assert_eq!(h3_snap_frames(363), H3_MAX_FRAMES);
        assert_eq!(h3_snap_frames(10_000), H3_MAX_FRAMES);
        // Every result is on the grid and inside the budget.
        for n in 0..500u32 {
            let s = h3_snap_frames(n);
            assert!((H3_MIN_FRAMES..=H3_MAX_FRAMES).contains(&s), "{n} -> {s}");
            assert_eq!(
                (s - H3_FRAME_BASE) % H3_FRAME_PERIOD,
                0,
                "{s} is off the grid"
            );
        }
        // 15.083 s is the cap, 0.917 s the mechanical floor, and 4.458 s the DOCUMENTED one.
        assert!((h3_seconds_from_frames(H3_MAX_FRAMES) - 15.0833).abs() < 0.001);
        assert!((h3_seconds_from_frames(H3_MIN_FRAMES) - 0.9167).abs() < 0.001);
        assert!((h3_seconds_from_frames(H3_DOCUMENTED_MIN_FRAMES) - 4.4583).abs() < 0.001);
    }

    /// The whole point of the allowlist: a project that was LTX yesterday (or was compiled against
    /// the C++ H3 engine last month) still carries a negative prompt, beats, keyframes, a resume id
    /// and two sigma shifts. None may reach the wire — and all must be REPORTED, because "my
    /// setting did nothing" is invisible in the output.
    ///
    /// The request-level `loras` here is the LTX `[{path, multiplier}]` shape and stays dead: H3's
    /// LoRAs are per GENERATION and named by bare filename. The per-generation field is a real one
    /// now — see `a_legal_lora_list_reaches_the_wire_with_its_multipliers`.
    #[test]
    fn dead_fields_never_reach_the_wire_and_are_reported() {
        let raw = json!({
            "generations": [
                { "prompt": "she turns to the window", "frames": 124,
                  "negative_prompt": "blurry", "cfg": 3.5, "pin_strength": 0.7,
                  "beats": [{ "seconds": 1.0, "text": "cut" }],
                  "keyframes": [{ "frame": 12 }],
                  "scene_cut": true,
                  "v2v_strength": 0.6,
                  "character_refs": [{ "image_b64": "aGk=", "source_id": 2 }] }
            ],
            "width": 1344, "height": 768,
            "negative_prompt": "worst quality",
            "cfg_scale": 3.5,
            "loras": [{ "path": "/loras/x.gguf", "multiplier": 1.0 }],
            "model": "echo-full",
            "hires_chain": [{ "upscaler": "x2" }],
            "resume_job_id": "job_ltx",
            "audio_source": { "kind": "voice_upload" },
            "sample_method": "euler",
            "scheduler": "karras",
            "sigma_shift_video": 12.0,
            "sigma_shift_audio": 3.0,
            "adapt_canvas": true,
            "output_format": "webm",
            "seconds": 5.0,
        });
        let chain = build_chain(&req(raw.clone()), &raw).expect("builds");

        let wire = serde_json::to_string(&chain.chain).unwrap();
        for forbidden in [
            "negative_prompt",
            "cfg",
            "lora",
            "hires_chain",
            "resume_job_id",
            "audio_source",
            "beats",
            "pin_strength",
            "character_refs",
            "sigma_shift",
            "adapt_canvas",
            "output_format",
            "sample_method",
            "scheduler",
            "v2v_",
        ] {
            assert!(
                !wire.contains(forbidden),
                "{forbidden} reached the H3 wire: {wire}"
            );
        }
        for reported in [
            "negative_prompt",
            "cfg_scale",
            "loras",
            "model",
            "hires_chain",
            "resume_job_id",
            "audio_source",
            "sample_method",
            "scheduler",
            "sigma_shift_video",
            "sigma_shift_audio",
            "adapt_canvas",
            "output_format",
            "seconds",
            "generations[].beats",
            "generations[].pin_strength",
            "generations[].character_refs",
            "generations[].keyframes",
            "generations[].scene_cut",
            "generations[].v2v_*",
        ] {
            assert!(
                chain.dropped.iter().any(|d| d == reported),
                "{reported} was dropped silently; dropped = {:?}",
                chain.dropped
            );
        }
        // …and the render itself is intact.
        assert_eq!(chain.prompts, vec!["she turns to the window"]);
        assert_eq!(chain.chain["generations"][0]["frames"], json!(124));
        assert_eq!(chain.chain["generations"][0]["task"], json!("t2va"));
        assert_eq!(chain.chain["fps"], json!(24));
    }

    /// H3 DOES have LoRAs — per generation, by bare filename — and the turbo LoRA is why anyone
    /// cares: 8 steps at 465.4 s against the 20-step control's 762.8 s, for zero extra VRAM. So the
    /// pair has to survive `build_chain` intact, and must no longer be reported as dropped.
    #[test]
    fn a_legal_lora_list_reaches_the_wire_with_its_multipliers() {
        let raw = json!({ "generations": [{
            "prompt": "she turns to the window", "frames": 124, "steps": 8,
            "loras": ["h3-turbo-4step-ema-pruned.safetensors", "grain.safetensors"],
            "lora_multipliers": [1.0, 0.4],
        }]});
        let chain = build_chain(&req(raw.clone()), &raw).expect("builds");
        let g = &chain.chain["generations"][0];
        assert_eq!(
            g["loras"],
            json!(["h3-turbo-4step-ema-pruned.safetensors", "grain.safetensors"])
        );
        assert_eq!(g["lora_multipliers"], json!([1.0, 0.4]));
        // 🔴 The LoRA does not choose the step count — the caller does, and forgetting to is the
        // entire −39% left on the table (at 20 steps a turbo LoRA is just ~5%/step slower).
        assert_eq!(g["steps"], json!(8));
        // …and it is no longer reported as a dropped LTX leftover, which would be a lie in exactly
        // the list a user reads to find out why a setting did nothing.
        assert!(
            !chain.dropped.iter().any(|d| d == "generations[].loras"),
            "dropped = {:?}",
            chain.dropped
        );

        // A SHORTER multiplier list is legal — the engine pads the rest with 1.0.
        let short = one_gen(json!({
            "loras": ["a.safetensors", "b.safetensors"], "lora_multipliers": [0.8],
        }));
        let chain = build_chain(&req(short.clone()), &short).expect("a shorter list is padded");
        assert_eq!(
            chain.chain["generations"][0]["lora_multipliers"],
            json!([0.8])
        );

        // An absent one emits no key at all rather than a synthesised all-1.0 list, so the request
        // log keeps "I did not choose" distinguishable from "I chose 1.0".
        let bare = one_gen(json!({ "loras": ["a.safetensors"] }));
        let chain = build_chain(&req(bare.clone()), &bare).expect("builds");
        let g = &chain.chain["generations"][0];
        assert_eq!(g["loras"], json!(["a.safetensors"]));
        assert!(g.get("lora_multipliers").is_none(), "{g}");

        // No LoRAs at all is the common case and emits neither key.
        let none = one_gen(json!({}));
        let chain = build_chain(&req(none.clone()), &none).expect("builds");
        let wire = serde_json::to_string(&chain.chain).unwrap();
        assert!(!wire.contains("lora"), "{wire}");
    }

    /// Each of these is a shape that would otherwise half-apply, apply twice, or resolve outside the
    /// engine's loras dir — and none of them shows up as an error in the output, only as a render
    /// that looks 20-step. They mirror `contract.py::Generation::_loras_coherent`; the reason to
    /// duplicate them is the part the shim's 400 cannot say — WHICH generation of the chain.
    #[test]
    fn incoherent_lora_lists_are_refused_and_the_message_names_the_generation() {
        // The offending generation is always the SECOND, so "generation 2" is a real assertion
        // rather than one that would pass on any message.
        let refuse = |bad: Value| -> String {
            let mut second = json!({ "prompt": "b", "frames": 124 });
            for (k, v) in bad.as_object().expect("object") {
                second
                    .as_object_mut()
                    .expect("object")
                    .insert(k.clone(), v.clone());
            }
            let raw = json!({ "generations": [{ "prompt": "a", "frames": 124 }, second] });
            build_chain(&req(raw.clone()), &raw).expect_err("must refuse")
        };

        let e = refuse(json!({ "lora_multipliers": [0.5] }));
        assert!(e.contains("generation 2"), "{e}");
        assert!(e.contains("no loras"), "{e}");

        // Positional matching is the reason an EXTRA multiplier is an error and not a truncation.
        let e = refuse(json!({ "loras": ["a.safetensors"], "lora_multipliers": [1.0, 0.5] }));
        assert!(e.contains("generation 2"), "{e}");
        assert!(e.contains("POSITIONALLY"), "{e}");

        // A path would resolve against WanGP's loras root and could escape it.
        for name in [
            "../../etc/passwd.safetensors",
            "sub/dir.safetensors",
            "sub\\dir.safetensors",
            ".hidden.safetensors",
            "   ",
        ] {
            let e = refuse(json!({ "loras": [name] }));
            assert!(e.contains("bare filename"), "{name}: {e}");
            assert!(e.contains("generation 2"), "{name}: {e}");
        }

        let e = refuse(json!({ "loras": ["turbo.safetensors", "turbo.safetensors"] }));
        assert!(e.contains("generation 2"), "{e}");
        assert!(e.contains("twice"), "{e}");

        // Nine of either list is past the engine's own `max_length=8`. Worth refusing here rather
        // than at the shim because cost scales with the NUMBER of LoRAs, so the ninth is not free.
        let nine: Vec<String> = (0..9).map(|i| format!("l{i}.safetensors")).collect();
        let e = refuse(json!({ "loras": nine }));
        assert!(e.contains("at most 8"), "{e}");
        let e = refuse(json!({
            "loras": ["a.safetensors"], "lora_multipliers": vec![1.0f64; 9],
        }));
        assert!(e.contains("at most 8"), "{e}");
    }

    /// A doc compiled against the C++ engine still carries `segments[]`. It is inert — the packer
    /// works from the timeline's clips — so it must be REPORTED rather than silently reshaped,
    /// which would look like the shot list was honoured.
    #[test]
    fn a_legacy_segments_array_is_reported_not_reshaped() {
        let raw = json!({
            "generations": [{ "prompt": "a", "frames": 124 }],
            "segments": [{ "prompt": "old shot 1", "beats": [{ "seconds": 1.0 }] }],
        });
        let chain = build_chain(&req(raw.clone()), &raw).expect("builds");
        assert!(chain.dropped.iter().any(|d| d == "segments"));
        assert!(chain.dropped.iter().any(|d| d == "segments[].beats"));
        assert_eq!(chain.chain["generations"].as_array().unwrap().len(), 1);
        assert!(
            !serde_json::to_string(&chain.chain)
                .unwrap()
                .contains("old shot 1")
        );
    }

    /// `ref2va` and `fl2va` are different CHECKPOINTS, so one generation cannot have both. Rejected
    /// HERE rather than at the shim, whose 400 cannot say which generation — and the message has to
    /// name the two ways out, or the only affordance the user finds is deleting the reference.
    #[test]
    fn a_generation_cannot_have_both_a_keyframe_and_an_omni_reference() {
        let both = one_gen(json!({
            "references": [
                { "kind": "image", "role": "first_frame", "image": "aGk=" },
                { "kind": "image", "role": "reference", "image": "aGk=" }
            ],
        }));
        let e = build_chain(&req(both.clone()), &both).expect_err("must refuse");
        assert!(e.contains("DIFFERENT CHECKPOINTS"), "{e}");
        assert!(e.contains("two generations"), "{e}");

        // Splitting them into two generations is the way out, and must work.
        let split = json!({ "generations": [
            { "prompt": "a", "frames": 124, "references": [
                { "kind": "image", "role": "first_frame", "image": "aGk=" }] },
            { "prompt": "b", "frames": 124, "references": [
                { "kind": "image", "role": "reference", "image": "aGk=" }] },
        ]});
        let chain = build_chain(&req(split.clone()), &split).expect("split is legal");
        assert_eq!(chain.chain["generations"][0]["task"], json!("fl2va"));
        assert_eq!(chain.chain["generations"][1]["task"], json!("ref2va"));
        // Part names are flat across the WHOLE request, not per generation. `ref_b<n>` is the
        // legacy-base64 naming — a staged asset would be `ref_<n>`, and the two ranges are kept
        // apart so a document mixing both can't have one reference resolve to the other's bytes.
        assert_eq!(chain.ref_parts.len(), 2);
        assert_eq!(chain.ref_parts[0].name, "ref_b0");
        assert_eq!(chain.ref_parts[1].name, "ref_b1");
        assert_eq!(
            chain.chain["generations"][1]["references"][0]["part"],
            json!("ref_b1")
        );
    }

    /// The editor authors keyframes as a property of the generation (`first_frame` / `last_frame`);
    /// the wire carries them as references with the FL2VA roles. That translation happens here, and
    /// it is what makes the checkpoint derivation see them at all.
    #[test]
    fn inline_keyframes_become_role_tagged_references() {
        let raw = one_gen(json!({ "first_frame": "b3Blbg==", "last_frame": "Y2xvc2U=" }));
        let chain = build_chain(&req(raw.clone()), &raw).expect("builds");
        let g = &chain.chain["generations"][0];
        assert_eq!(g["task"], json!("fl2va"));
        let refs = g["references"].as_array().expect("array");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0]["role"], json!("first_frame"));
        assert_eq!(refs[0]["kind"], json!("image"));
        assert_eq!(refs[1]["role"], json!("last_frame"));
        // …and they ship as parts like anything else.
        assert_eq!(refs[0]["part"], json!("ref_b0"));
        assert_eq!(chain.ref_parts.len(), 2);

        // A generation with only an opener is still fl2va.
        let opener = one_gen(json!({ "first_frame": "b3Blbg==" }));
        let chain = build_chain(&req(opener.clone()), &opener).expect("builds");
        assert_eq!(chain.chain["generations"][0]["task"], json!("fl2va"));

        // 🔴 And an inline keyframe still collides with an omni reference: different checkpoints.
        let both = one_gen(json!({
            "first_frame": "b3Blbg==",
            "references": [{ "kind": "image", "image": "aGk=" }],
        }));
        let e = build_chain(&req(both.clone()), &both).expect_err("must refuse");
        assert!(e.contains("DIFFERENT CHECKPOINTS"), "{e}");
    }

    /// koblem's role vocabulary is richer than the engine's, so every role maps DOWN to one of the
    /// five `app/contract.py` knows. The distinction the engine loses is carried in the prompt,
    /// which is the only place the model can read it anyway.
    #[test]
    fn roles_map_down_to_the_engines_five_and_legacy_values_still_parse() {
        let rich = json!({ "generations": [{ "prompt": "a", "frames": 124, "references": [
            { "kind": "image", "role": "storyboard", "image": "aQ==" },
            { "kind": "video", "role": "edit_source", "video": "dg==" },
            { "kind": "video", "role": "continuation_source", "video": "dg==" },
            { "kind": "audio", "role": "audio_reuse", "audio": "YQ==" },
            { "kind": "audio", "role": "audio_continuity", "audio": "YQ==" },
        ]}]});
        let chain = build_chain(&req(rich.clone()), &rich).expect("builds");
        let refs = chain.chain["generations"][0]["references"]
            .as_array()
            .unwrap();
        // Only `continuation_source` and `audio_reuse` have engine slots of their own.
        assert_eq!(refs[0]["role"], json!("reference"), "storyboard");
        assert_eq!(refs[1]["role"], json!("reference"), "edit_source");
        assert_eq!(refs[2]["role"], json!("continuation_source"));
        assert_eq!(refs[3]["role"], json!("reuse"), "audio_reuse");
        assert_eq!(refs[4]["role"], json!("reference"), "audio_continuity");

        // 🔴 An absent role is NOT an error and NOT one flat default: it resolves per KIND, to the
        // conservative claim — an image is an identity source, a video is a structure source (NOT
        // an edit source, which would claim `video editing` for a clip nobody is editing), audio
        // is a timbre reference (NOT a reuse, which is the stronger claim).
        let bare = one_gen(json!({ "references": [
            { "kind": "image", "image": "aQ==" },
            { "kind": "video", "video": "dg==" },
            { "kind": "audio", "audio": "YQ==" },
        ]}));
        let chain = build_chain(&req(bare.clone()), &bare).expect("builds");
        for r in chain.chain["generations"][0]["references"]
            .as_array()
            .unwrap()
        {
            assert_eq!(r["role"], json!("reference"), "{r}");
        }

        // …and a document carrying the ENGINE's old vocabulary still opens rather than failing to
        // deserialise, which would make the project unopenable over a summary-prefix nicety.
        let legacy = one_gen(json!({ "references": [
            { "kind": "audio", "role": "reuse", "audio": "YQ==" },
            { "kind": "image", "role": "reference", "image": "aQ==" },
            { "kind": "image", "role": "something_a_newer_client_invented", "image": "aQ==" },
        ]}));
        let chain = build_chain(&req(legacy.clone()), &legacy).expect("legacy roles parse");
        let refs = chain.chain["generations"][0]["references"]
            .as_array()
            .unwrap();
        assert_eq!(refs[0]["role"], json!("reuse"), "`reuse` has one meaning");
        assert_eq!(refs[1]["role"], json!("reference"));
        assert_eq!(refs[2]["role"], json!("reference"), "unknown falls back");
    }

    /// A stated task is an ASSERTION, not an instruction. Obeying a wrong one would render on the
    /// other checkpoint with the conditioning silently unused — a plausible clip and no error.
    #[test]
    fn a_stated_task_that_contradicts_the_conditioning_is_refused() {
        let lying = one_gen(json!({
            "task": "t2va",
            "references": [{ "kind": "image", "image": "aGk=" }],
        }));
        let e = build_chain(&req(lying.clone()), &lying).expect_err("must refuse");
        assert!(e.contains("states task `t2va`"), "{e}");
        assert!(e.contains("derives `ref2va`"), "{e}");

        // Agreeing is fine.
        let honest = one_gen(json!({
            "task": "ref2va",
            "references": [{ "kind": "image", "image": "aGk=" }],
        }));
        let chain = build_chain(&req(honest.clone()), &honest).expect("agreement is legal");
        assert_eq!(chain.chain["generations"][0]["task"], json!("ref2va"));
    }

    /// The reason staging exists: a character reference used in several generations is ONE upload.
    /// It used to be one base64 copy per use, encoded at the edge and decoded straight back here.
    ///
    /// Deduplicating the PARTS is not the same as deduplicating the REFERENCES — the reference list
    /// keeps every entry, in order, because order assigns the ordinals (see the test below). Only
    /// the bytes are shared.
    #[tokio::test]
    async fn one_staged_asset_used_twice_is_one_upload() {
        // What the document looks like after `stage_refs`: both uses carry the same marker.
        let raw = json!({ "generations": [
            { "prompt": "a", "frames": 124, "references": [
                { "kind": "image", "role": "reference", "image": "part:ref_0" }] },
            { "prompt": "b", "frames": 124, "references": [
                { "kind": "image", "role": "reference", "image": "part:ref_0" },
                { "kind": "image", "role": "reference", "image": "part:ref_1" }] },
        ]});
        let mut doc = json!({
            "a": director_assets::make_ref(&director_assets::hash_bytes(b"portrait-bytes")),
            "b": director_assets::make_ref(&director_assets::hash_bytes(b"other-bytes")),
        });
        // Stage against a real store so the table holds the two assets under ref_0 / ref_1.
        let dir = std::env::temp_dir().join(format!("koblem-h3-{}", uuid::Uuid::now_v7()));
        for bytes in [&b"portrait-bytes"[..], &b"other-bytes"[..]] {
            let h = director_assets::hash_bytes(bytes);
            director_assets::write_bytes(&dir, &h, bytes)
                .await
                .expect("store");
        }
        let staged = director_assets::stage_refs(&dir, &mut doc, "ref_")
            .await
            .expect("both present");

        let chain = super::build_chain(&req(raw.clone()), &raw, &staged).expect("builds");

        // Three references across two generations, but only two uploads.
        assert_eq!(
            chain.ref_parts.len(),
            2,
            "the repeated reference must not be uploaded twice"
        );
        assert_eq!(chain.ref_parts[0].name, "ref_0");
        assert_eq!(chain.ref_parts[0].bytes, b"portrait-bytes".to_vec());
        assert_eq!(chain.ref_parts[1].name, "ref_1");
        // Both uses still point at their part, and the reference lists are untouched in length.
        assert_eq!(
            chain.chain["generations"][0]["references"][0]["part"],
            json!("ref_0")
        );
        assert_eq!(
            chain.chain["generations"][1]["references"][0]["part"],
            json!("ref_0")
        );
        assert_eq!(
            chain.chain["generations"][1]["references"][1]["part"],
            json!("ref_1")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A `part:` marker with nothing behind it is a hand-written document, not a lost asset — a lost
    /// asset fails earlier, in `stage_refs`, with a message naming the hash. Either way it must not
    /// reach the shim, which can only report it as a missing filename.
    #[test]
    fn an_unbacked_part_marker_is_refused_here() {
        let raw = one_gen(json!({
            "references": [{ "kind": "image", "role": "reference", "image": "part:ref_9" }],
        }));
        let e = super::build_chain(
            &req(raw.clone()),
            &raw,
            &director_assets::StagedParts::default(),
        )
        .expect_err("must refuse");
        assert!(e.contains("ref_9"), "{e}");
        assert!(e.contains("not attached"), "{e}");
    }

    /// Order assigns the ordinals AND advances a shared rotary clock, so the same references in a
    /// different order are a different request. Nothing may sort, group or de-duplicate them.
    #[test]
    fn reference_order_survives_verbatim() {
        let raw = one_gen(json!({
            "references": [
                { "kind": "audio", "audio": "bm90IHdhdg==" },
                { "kind": "image", "image": "aW1n" },
                // No `with_audio`: a clip soundtrack cannot coexist with the standalone audio
                // above, and the standalone one is what makes this ordering worth pinning.
                { "kind": "video", "video": "dmlk", "tail_frames": 56 }
            ],
        }));
        let chain = build_chain(&req(raw.clone()), &raw).expect("builds");
        let refs = chain.chain["generations"][0]["references"]
            .as_array()
            .expect("array");
        assert_eq!(refs[0]["kind"], json!("audio"));
        assert_eq!(refs[1]["kind"], json!("image"));
        assert_eq!(refs[2]["kind"], json!("video"));
        // Every kind rides a multipart part now, not just audio — and each carries its own
        // back-pointer, so the shim never has to guess which part is which reference.
        assert_eq!(chain.ref_parts.len(), 3);
        assert_eq!(refs[0]["part"], json!("ref_b0"));
        assert_eq!(refs[2]["part"], json!("ref_b2"));
        assert_eq!(refs[2]["tail_frames"], json!(56));
        assert!(refs[0].get("audio").is_none());
    }

    /// 🔴 3 video / 3 audio — the `max` the node's autogrow groups declare, and MiniMax's own
    /// documented figure. The 2 was WanGP's `validate_generative_settings:260-319`, which held two
    /// named slots per kind; so was `audio_count <= visual_count`, which is not a rule anywhere any
    /// more. What survives is the total and MiniMax's sole-input rule, tested below.
    #[test]
    fn the_reference_caps_are_three_video_three_audio_and_twelve_total() {
        let three_video = one_gen(json!({
            "references": [
                { "kind": "video", "video": "dg==" },
                { "kind": "video", "video": "dg==" },
                { "kind": "video", "video": "dg==" }
            ],
        }));
        build_chain(&req(three_video.clone()), &three_video).expect("3 videos is the node's max");

        let four_video = one_gen(json!({
            "references": [
                { "kind": "video", "video": "dg==" },
                { "kind": "video", "video": "dg==" },
                { "kind": "video", "video": "dg==" },
                { "kind": "video", "video": "dg==" }
            ],
        }));
        let e = build_chain(&req(four_video.clone()), &four_video).expect_err("4 videos");
        assert!(e.contains("at most 3"), "{e}");

        // Three audio needs something visual to attach to — the sole-input rule, not a ratio.
        let three_audio = one_gen(json!({
            "references": [
                { "kind": "image", "image": "aQ==" },
                { "kind": "audio", "audio": "YQ==" },
                { "kind": "audio", "audio": "YQ==" },
                { "kind": "audio", "audio": "YQ==" }
            ],
        }));
        build_chain(&req(three_audio.clone()), &three_audio).expect("3 audio beside a still");

        let four_audio = one_gen(json!({
            "references": [
                { "kind": "image", "image": "aQ==" },
                { "kind": "audio", "audio": "YQ==" },
                { "kind": "audio", "audio": "YQ==" },
                { "kind": "audio", "audio": "YQ==" },
                { "kind": "audio", "audio": "YQ==" }
            ],
        }));
        let e = build_chain(&req(four_audio.clone()), &four_audio).expect_err("4 audio");
        assert!(e.contains("at most 3"), "{e}");

        // 🔴 Two audio against one visual — one identity plate and a pair of tracks, the obvious
        // shape the old `audios <= visuals` refusal took away. Legal.
        let lopsided = one_gen(json!({
            "references": [
                { "kind": "image", "image": "aQ==" },
                { "kind": "audio", "audio": "YQ==" },
                { "kind": "audio", "audio": "YQ==" }
            ],
        }));
        build_chain(&req(lopsided.clone()), &lopsided).expect("2 audio against 1 visual");

        // 13 references trips the total cap before any per-kind one.
        let mut references = Vec::new();
        for _ in 0..9 {
            references.push(json!({ "kind": "image", "image": "aQ==" }));
        }
        for _ in 0..3 {
            references.push(json!({ "kind": "video", "video": "dg==" }));
        }
        references.push(json!({ "kind": "audio", "audio": "YQ==" }));
        let over = one_gen(json!({ "references": references }));
        let e = build_chain(&req(over.clone()), &over).expect_err("13 references");
        assert!(e.contains("at most 12 total"), "{e}");
    }

    /// Audio-only conditioning is invalid — there is nothing for the audio to attach to. This is
    /// MiniMax's own rule (`README:60`), the one audio rule the engine still enforces, and it is
    /// all that is left of the ratio the old validator demanded.
    #[test]
    fn audio_only_reference_conditioning_is_refused() {
        let raw = one_gen(json!({
            "references": [{ "kind": "audio", "audio": "d2F2" }],
        }));
        let e = build_chain(&req(raw.clone()), &raw).expect_err("audio-only");
        assert!(e.contains("no image or video"), "{e}");
        assert!(e.contains("sole input"), "{e}");
    }

    /// Cut times: `[Shot 1]` is untimed, later shots are strictly increasing and INSIDE the
    /// duration. A cut past the end is the interesting one — the model renders a generation that
    /// never reaches the second shot, and the output reads as "the prompt was ignored".
    #[test]
    fn shot_cut_times_must_increase_and_stay_inside_the_duration() {
        // 124 frames = 5.167 s.
        let ok = one_gen(json!({
            "frames": 124,
            "shots": [
                { "index": 0 },
                { "index": 1, "at_seconds": 2.5 },
                { "index": 2, "at_seconds": 4.0 }
            ],
        }));
        let chain = build_chain(&req(ok.clone()), &ok).expect("builds");
        let shots = chain.chain["generations"][0]["shots"].as_array().unwrap();
        assert_eq!(shots.len(), 3);
        assert!(shots[0].get("at_seconds").is_none(), "[Shot 1] is untimed");
        assert_eq!(shots[2]["at_seconds"], json!(4.0));

        let timed_first = one_gen(json!({
            "shots": [{ "index": 0, "at_seconds": 1.0 }],
        }));
        let e = build_chain(&req(timed_first.clone()), &timed_first).expect_err("timed [Shot 1]");
        assert!(e.contains("untimed"), "{e}");

        let backwards = one_gen(json!({
            "frames": 124,
            "shots": [
                { "index": 0 },
                { "index": 1, "at_seconds": 3.0 },
                { "index": 2, "at_seconds": 3.0 }
            ],
        }));
        let e = build_chain(&req(backwards.clone()), &backwards).expect_err("equal cut times");
        assert!(e.contains("strictly increasing"), "{e}");

        let past_the_end = one_gen(json!({
            "frames": 124,
            "shots": [{ "index": 0 }, { "index": 1, "at_seconds": 9.0 }],
        }));
        let e = build_chain(&req(past_the_end.clone()), &past_the_end).expect_err("cut past end");
        assert!(e.contains("would never render"), "{e}");

        let untimed_second = one_gen(json!({
            "shots": [{ "index": 0 }, { "index": 1 }],
        }));
        let e =
            build_chain(&req(untimed_second.clone()), &untimed_second).expect_err("untimed cut");
        assert!(e.contains("absolute cut time"), "{e}");
    }

    /// 7000 chars is MiniMax's serving cap. REFUSED, never truncated: the schema puts
    /// `non_diegetic_music` last, so a truncation silently drops the whole music direction and the
    /// render merely sounds wrong.
    #[test]
    fn an_over_length_prompt_is_refused_and_never_truncated() {
        let long = "x".repeat(H3_MAX_PROMPT_CHARS + 1);
        let raw = json!({ "generations": [{ "prompt": long, "frames": 124 }] });
        let e = build_chain(&req(raw.clone()), &raw).expect_err("over the cap");
        assert!(e.contains("7000"), "{e}");
        assert!(e.contains("non_diegetic_music"), "{e}");

        let at_cap = "x".repeat(H3_MAX_PROMPT_CHARS);
        let ok = json!({ "generations": [{ "prompt": at_cap, "frames": 124 }] });
        assert!(
            build_chain(&req(ok.clone()), &ok).is_ok(),
            "the cap itself is legal"
        );
    }

    /// Continuation is only meaningful across a boundary. `"prev"` is resolved by the shim at RUN
    /// time (which is what makes it survive a retake), so on the first generation it resolves to
    /// nothing and must be caught before the GPU is taken.
    /// A hard cut (`mode: none`) is not a continuation, and a single-generation retake of one hits
    /// the generation-0 `prev` check: the retake path resolves `prev` only for joining modes, so the
    /// unresolved `prev` survived and every retake of a hard-cut render was refused. The shim reads
    /// `none` as absent anyway, so nothing is sent.
    #[test]
    fn a_hard_cut_is_not_sent_as_a_continuation() {
        let cut = one_gen(json!({ "continuation": { "mode": "none" } }));
        let chain = build_chain(&req(cut.clone()), &cut).expect("a hard cut needs nothing before it");
        assert_eq!(chain.chain["generations"][0].get("continuation"), None);
    }

    #[test]
    fn a_continuation_needs_something_to_continue_from() {
        let first = one_gen(json!({ "continuation": { "mode": "latents" } }));
        let e = build_chain(&req(first.clone()), &first).expect_err("nothing to continue from");
        assert!(e.contains("first generation"), "{e}");

        let ok = json!({ "generations": [
            { "prompt": "a", "frames": 124 },
            { "prompt": "b", "frames": 124, "continuation": { "mode": "latents" } },
        ]});
        let chain = build_chain(&req(ok.clone()), &ok).expect("builds");
        let c = &chain.chain["generations"][1]["continuation"];
        // Defaults come from the measured saturation point, not from the largest value that works.
        assert_eq!(c["from"], json!("prev"));
        assert_eq!(c["k"], json!(7));
        assert_eq!(c["anchor"], json!(true));
        // 🔴 The WanGP fields are GONE from the wire, not defaulted onto it. Sending them made
        // the shim answer every continued shot with two warnings the human then had to read.
        for dead in ["tail_frames", "drop_guide", "posterior_noise"] {
            assert_eq!(c.get(dead), None, "{dead} still on the wire");
        }
        // A continuation implies ref2va: on fl2va/t2va the pin is inert (measured 5% / -2% / 3%).
        assert_eq!(chain.chain["generations"][1]["task"], json!("ref2va"));

        // A bank key on generation 0 is legal — that is how a retake continues.
        let banked = one_gen(json!({
            "continuation": { "mode": "frame", "from": "h3_abc:0" },
        }));
        let chain = build_chain(&req(banked.clone()), &banked).expect("a bank key is legal");
        assert_eq!(
            chain.chain["generations"][0]["continuation"]["from"],
            json!("h3_abc:0")
        );

        // A document saved while `tail_frames` was still a continuation field still LOADS — serde
        // ignores it — and it no longer reaches the engine or trips a grid check that had nothing
        // left to protect.
        let legacy = json!({ "generations": [
            { "prompt": "a", "frames": 124 },
            { "prompt": "b", "frames": 124, "continuation": { "tail_frames": 50 } },
        ]});
        let chain = build_chain(&req(legacy.clone()), &legacy).expect("a stale field is not fatal");
        assert_eq!(
            chain.chain["generations"][1]["continuation"].get("tail_frames"),
            None
        );
    }

    /// 🔴 An off-grid tail used to be refused, on a floor of 56 and the 5+17k grid — both WanGP's,
    /// where `_add_video_reference` silently kept the FIRST frames and the model believed it was
    /// looking at the end of a clip. On the ComfyUI path `tail_frames` reaches no node at all; the
    /// engine spends it on an ETA row estimate and publishes `tail_frames.applied = false`. So it
    /// is carried through untouched, and `legal_tails` stays an editor picker rather than a gate.
    #[test]
    fn an_off_grid_reference_tail_is_carried_not_refused() {
        let off_grid = one_gen(json!({
            "references": [{ "kind": "video", "video": "dg==", "tail_frames": 39 }],
        }));
        let chain = build_chain(&req(off_grid.clone()), &off_grid).expect("39 is not a gate");
        assert_eq!(
            chain.chain["generations"][0]["references"][0]["tail_frames"],
            json!(39)
        );

        let on_grid = one_gen(json!({
            "references": [{ "kind": "video", "video": "dg==", "tail_frames": 73 }],
        }));
        let chain = build_chain(&req(on_grid.clone()), &on_grid).expect("on-grid is unchanged");
        assert_eq!(
            chain.chain["generations"][0]["references"][0]["tail_frames"],
            json!(73)
        );
    }

    /// The C++ engine took a decoded frame array; WanGP takes a container. There is no frame-array
    /// path on the wire, so a doc still holding one is refused by NAME rather than having its
    /// reference silently conditioned on nothing.
    #[test]
    fn a_legacy_frame_array_video_reference_is_refused_by_name() {
        let raw = one_gen(json!({
            "references": [{ "kind": "video", "frames": ["1", "2", "3", "4", "5"] }],
        }));
        let e = build_chain(&req(raw.clone()), &raw).expect_err("frame arrays are gone");
        assert!(e.contains("FRAME ARRAY"), "{e}");
        assert!(e.contains("container"), "{e}");
    }

    /// The 32-grid is checked for the job AND for any per-generation override — a generation that
    /// overrode only `width` would otherwise slip past a job-level check that saw a legal pair.
    ///
    /// 🔴 The AREA budget is deliberately NOT here: it is guidance, not a rule. See
    /// `an_over_budget_canvas_warns_and_renders`.
    #[test]
    fn an_off_grid_canvas_is_refused_at_both_levels() {
        let bad = json!({ "generations": [{ "prompt": "a", "frames": 124 }], "width": 1000, "height": 768 });
        let e = build_chain(&req(bad.clone()), &bad).expect_err("off the 32 grid");
        assert!(e.contains("multiple of 32"), "{e}");
        // The message has to say WHICH side and offer the neighbours, because the numbers people
        // type are 720 and 1080 and both are illegal.
        assert!(e.contains("1000 is not"), "{e}");
        assert!(e.contains("992x768"), "{e}");
        assert!(e.contains("1024x768"), "{e}");

        // 720 and 1080 are the two that catch everyone: 720/32 = 22.5, 1080/32 = 33.75.
        let hd = json!({ "generations": [{ "prompt": "a", "frames": 124 }], "width": 1280, "height": 720 });
        let e = build_chain(&req(hd.clone()), &hd).expect_err("720 is off the grid");
        assert!(e.contains("720 is not"), "{e}");
        assert!(e.contains("1280x704"), "{e}");

        let override_bad = json!({ "generations": [
            { "prompt": "a", "frames": 124, "width": 1000, "height": 768 }
        ]});
        let e = build_chain(&req(override_bad.clone()), &override_bad).expect_err("bad override");
        assert!(e.contains("generation 1"), "{e}");

        let half = json!({ "generations": [{ "prompt": "a", "frames": 124, "width": 1344 }] });
        let e = build_chain(&req(half.clone()), &half).expect_err("half an override");
        assert!(e.contains("both or neither"), "{e}");
    }

    /// 🔴 The area number is guidance about VRAM, not a limit the model has. `diffusers` calls it
    /// a *"soft area cap"*, ComfyUI does not enforce it, and no config in the weights tree carries
    /// a max resolution. Enforcing it meant **true 16:9 at a 768 short edge could not be
    /// rendered** — 1376x768 is 1,056,768 px, 2.4% over — while MiniMax's own 1344x768 is 7:4.
    #[test]
    fn an_over_budget_canvas_warns_and_renders() {
        // The exact canvas the old 400 made unreachable.
        let sixteen_nine = json!({ "generations": [{ "prompt": "a", "frames": 124 }], "width": 1376, "height": 768 });
        let chain = build_chain(&req(sixteen_nine.clone()), &sixteen_nine)
            .expect("true 16:9 at 768p must render");
        assert_eq!(chain.chain["width"], json!(1376));
        assert_eq!(chain.warnings.len(), 1, "{:?}", chain.warnings);
        assert!(
            chain.warnings[0].contains("ADVISORY"),
            "{:?}",
            chain.warnings
        );

        // …and so does something far bigger, with the VRAM reality stated rather than a refusal.
        let huge = json!({ "generations": [{ "prompt": "a", "frames": 124 }], "width": 1920, "height": 1088 });
        let chain = build_chain(&req(huge.clone()), &huge).expect("over budget is not illegal");
        assert!(
            chain.warnings[0].contains("may not fit"),
            "{:?}",
            chain.warnings
        );

        // Inside the budget says nothing at all.
        let ok = json!({ "generations": [{ "prompt": "a", "frames": 124 }], "width": 1344, "height": 768 });
        let chain = build_chain(&req(ok.clone()), &ok).expect("the shipped default");
        assert!(chain.warnings.is_empty(), "{:?}", chain.warnings);
    }

    /// A sub-4s generation RENDERS at the length asked for, and is reported once. It used to be
    /// silently lengthened to 4.458 s, which is how a 2-second authored beat became a
    /// 4.458-second one with no error anywhere.
    #[test]
    fn a_generation_under_the_documented_floor_renders_and_is_reported() {
        let short = json!({ "generations": [{ "prompt": "a", "frames": 56 }] });
        let chain = build_chain(&req(short.clone()), &short).expect("2.33s is renderable");
        assert_eq!(chain.chain["generations"][0]["frames"], json!(56));
        assert_eq!(chain.warnings.len(), 1, "{:?}", chain.warnings);
        assert!(chain.warnings[0].contains("4.0s"), "{:?}", chain.warnings);
        assert!(
            chain.warnings[0].contains("H3_SHORT_CLIPS"),
            "{:?}",
            chain.warnings
        );

        // At and above the documented floor, nothing is said.
        let normal = json!({ "generations": [{ "prompt": "a", "frames": 107 }] });
        let chain = build_chain(&req(normal.clone()), &normal).expect("on the documented floor");
        assert!(chain.warnings.is_empty(), "{:?}", chain.warnings);
    }

    /// Empty-prompt generations are filtered exactly as LTX filters empty segments, and a request
    /// with nothing left is an error rather than a zero-generation job the shim would refuse.
    #[test]
    fn empty_prompt_generations_are_filtered() {
        let raw = json!({ "generations": [
            { "prompt": "a", "frames": 124 },
            { "prompt": "   ", "frames": 124 },
            { "prompt": "c", "frames": 124 },
        ]});
        let chain = build_chain(&req(raw.clone()), &raw).expect("builds");
        assert_eq!(chain.prompts, vec!["a", "c"]);
        assert_eq!(chain.chain["generations"].as_array().unwrap().len(), 2);

        let none = json!({ "generations": [{ "prompt": "  " }] });
        assert!(build_chain(&req(none.clone()), &none).is_err());
    }

    /// Exactly one source per reference. Zero conditions on nothing; two is a request the shim
    /// rejects, and picking one for the caller would be picking which of two things they meant.
    #[test]
    fn a_reference_needs_exactly_one_source() {
        let none = one_gen(json!({ "references": [{ "kind": "image", "label": "Ines" }] }));
        let e = build_chain(&req(none.clone()), &none).expect_err("no source");
        assert!(e.contains("carries 0 sources"), "{e}");

        let two = one_gen(json!({
            "references": [{ "kind": "image", "image": "aQ==", "bank": "h3_abc:0" }],
        }));
        let e = build_chain(&req(two.clone()), &two).expect_err("two sources");
        assert!(e.contains("carries 2 sources"), "{e}");

        // A bank key alone is a legal single source — a previous generation's tail without a round
        // trip through pixels.
        let banked = one_gen(json!({
            "references": [{ "kind": "video", "role": "continuation_source", "bank": "h3_abc:0" }],
        }));
        let chain = build_chain(&req(banked.clone()), &banked).expect("bank-only is legal");
        assert_eq!(
            chain.chain["generations"][0]["references"][0]["bank"],
            json!("h3_abc:0")
        );
        assert!(chain.ref_parts.is_empty(), "a bank key ships no bytes");
    }

    /// `with_audio` / `tail_frames` on a non-video reference are silent when honoured: an image
    /// with `with_audio` would shift every later `<Audio n>` by one, and a prompt addressing
    /// `<Audio 2>` would start describing something else.
    #[test]
    fn video_only_reference_fields_are_refused_elsewhere() {
        let raw = one_gen(json!({
            "references": [{ "kind": "image", "image": "aQ==", "with_audio": true }],
        }));
        let e = build_chain(&req(raw.clone()), &raw).expect_err("with_audio on an image");
        assert!(e.contains("video references only"), "{e}");

        let sound_on_a_still = one_gen(json!({
            "references": [{ "kind": "image", "image": "aQ==", "audio": "YQ==" }],
        }));
        let e = build_chain(&req(sound_on_a_still.clone()), &sound_on_a_still)
            .expect_err("image with a soundtrack");
        assert!(e.contains("only a video reference may"), "{e}");
    }

    /// 🔴 A clip's own soundtrack beside a standalone audio file used to be refused: on WanGP the
    /// two were MODES, not sources — `engine.py:640-648` picked `audio_prompt_type: "K"` the moment
    /// any video set `with_audio` and then never wrote `audio_guide`, dropping the standalone file
    /// while the prompt still addressed an `<Audio 2>`.
    ///
    /// ComfyUI's `MiniMaxH3ReferenceToVideo` declares four INDEPENDENT autogrow groups and
    /// `execute` walks all of them, encoding a soundtrack and a standalone file through the same
    /// `_encode_ref_audio`. Both are sent, in order, because order is what assigns the ordinals.
    #[test]
    fn a_clip_soundtrack_and_a_standalone_audio_reference_may_both_be_asked_for() {
        let both = one_gen(json!({
            "references": [
                { "kind": "video", "video": "GkXfo0==", "with_audio": true },
                { "kind": "audio", "audio": "YQ==" }
            ],
        }));
        let chain = build_chain(&req(both.clone()), &both).expect("a soundtrack and a track");
        let refs = chain.chain["generations"][0]["references"]
            .as_array()
            .expect("array");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0]["kind"], json!("video"));
        assert_eq!(refs[0]["with_audio"], json!(true));
        assert_eq!(refs[1]["kind"], json!("audio"));

        // …and each on its own is still perfectly legal. The video needs a visual to sit beside the
        // audio one, which it is itself.
        let soundtrack = one_gen(json!({
            "references": [{ "kind": "video", "video": "GkXfo0==", "with_audio": true }],
        }));
        let chain =
            build_chain(&req(soundtrack.clone()), &soundtrack).expect("a clip soundtrack alone");
        // …and it reaches the shim: the flag is what makes the engine pick `audio_prompt_type: "K"`.
        assert_eq!(
            chain.chain["generations"][0]["references"][0]["with_audio"],
            json!(true)
        );

        let standalone = one_gen(json!({
            "references": [
                { "kind": "video", "video": "GkXfo0==" },
                { "kind": "audio", "audio": "YQ==" }
            ],
        }));
        build_chain(&req(standalone.clone()), &standalone).expect("a standalone track alone");
    }
}
