//! `POST /api/v1/oneshot` — one call, one cut-out sprite and (optionally) one
//! sound effect. Built for paperworld, which summons a thing from a line of
//! player text and needs both halves back without orchestrating three requests.
//!
//! What it collapses. The equivalent hand-rolled sequence is `POST
//! /api/v1/krea2/generate` (with the chained `remove_background`), `POST
//! /api/v1/sa3/generate`, then an authenticated `GET /api/v1/sa3/file/{name}`
//! to actually get the wav bytes — three round trips, two of them carrying
//! base64 the caller immediately re-encodes. Here the two engines run
//! concurrently in-process and both payloads come back inline.
//!
//! **Concurrency is the point.** krea2 (~7.75 GiB) and sa3 (~2.8 GiB) are
//! separate budget lines on the gate, so they genuinely co-reside; running them
//! with `tokio::join!` roughly halves the wall clock versus a sequential caller.
//!
//! **The cut-out is non-negotiable.** The caller sizes each sprite from its
//! alpha bounding box, so an opaque image is not a degraded result — it renders
//! as a rectangle. This route therefore never returns krea2's raw `b64`: matting
//! must succeed and the result must contain visible pixels, or the image half
//! reports an error. That check is [`assert_renderable`].
//!
//! **Partial success is the contract.** A missing sound is a silent thing; a
//! missing image is a broken one. So a sound failure is a `200` carrying
//! `sound_error` with the image intact, and only auth, a malformed body, or an
//! unconfigured image engine are non-2xx.
//!
//! **Auth is deliberately cheap** — see [`key_ok`]. This route does NOT go
//! through `require_auth`, the session table or the api_keys table.
//!
//! `routes/summon.rs` streams the same work stage by stage, and does it by
//! calling [`render_image`] and [`render_sound`] here rather than growing a
//! second copy of them.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use koblem_client::types::Sa3GenerateRequest;

use crate::AppState;
use crate::matting::RemoveOptions;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::tier2_helpers::{extract_caller, inject_gpu, log_async};

/// Prompts only — there are no media inputs, so this is generous already.
const ONESHOT_BODY_LIMIT: usize = 256 * 1024;

/// The gpu-log service bucket, so oneshot traffic is separable from the krea2
/// and sa3 tabs' own history.
const SERVICE: &str = "oneshot";

/// Shared-secret header. Deliberately its own mechanism, not `Authorization:
/// Bearer` — see [`key_ok`].
pub(crate) const API_KEY_HEADER: &str = "x-api-key";

/// The doorstop value. Unset or blank — the default — means no doorstop is
/// configured, which closes the route rather than opening it: there is no
/// built-in key published in the source, so an operator who wants the
/// paperworld integration has to set one.
const API_KEY_ENV: &str = "ONESHOT_API_KEY";

/// Krea 2's own floor, and what its samples are shown at.
const DEFAULT_WIDTH: u32 = 1024;
const DEFAULT_HEIGHT: u32 = 1024;

/// Long enough to have a shape, short enough to fire as a one-shot at a world
/// position. sa3-server's own default is 10 s, which is a different thing.
const DEFAULT_SECONDS: f32 = 1.5;
/// sa3-server accepts up to 380 s; nothing that long belongs on this route, and
/// the clamp keeps one prompt from parking the music budget line for minutes.
const MAX_SECONDS: f32 = 30.0;
const MIN_SECONDS: f32 = 0.1;

/// Alpha at or above which a pixel is actually drawn. The caller renders sprites
/// with `AlphaMode::Mask(0.5)`, so a matte made entirely of softer pixels than
/// this is an invisible sprite, not a subtle one.
const ALPHA_VISIBLE: u8 = 128;

/// How many times to ask the matting service for a cut-out before giving up.
/// Two: the first attempt can land on a cold worker.
const MATTING_ATTEMPTS: usize = 2;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/oneshot", post(oneshot))
        .layer(DefaultBodyLimit::max(ONESHOT_BODY_LIMIT))
}

// ── wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OneshotRequest {
    pub image: ImageSpec,
    /// Omitted or null → no audio is rendered and `sound_b64` comes back null
    /// with no error. Silence is a valid answer here.
    #[serde(default)]
    pub sound: Option<SoundSpec>,
}

#[derive(Debug, Deserialize)]
pub struct ImageSpec {
    pub positive: String,
    /// Sent through, but inert at Krea-2-Turbo's cfg 1.0 — the distilled
    /// checkpoint runs without CFG, so there is no uncond pass to steer. Keep
    /// the constraints that matter in `positive`.
    #[serde(default)]
    pub negative: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub seed: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SoundSpec {
    pub prompt: String,
    #[serde(default)]
    pub seconds: Option<f32>,
    #[serde(default)]
    pub seed: Option<i64>,
}

/// Every field is always present; `null` is the "this half didn't happen"
/// signal, so the caller can branch without checking for absence.
#[derive(Debug, Serialize)]
pub struct OneshotResponse {
    /// Base64 PNG **with an alpha channel**, background removed. Null iff
    /// `image_error` is set.
    pub image_b64: Option<String>,
    pub image_error: Option<String>,
    /// Base64 wav. Null when no sound was asked for, or when `sound_error` is set.
    pub sound_b64: Option<String>,
    pub sound_error: Option<String>,
    /// Wall clock for the whole composite, gate wait included.
    pub render_ms: u64,
    /// The longer of the two gate waits — they are concurrent, so summing them
    /// would overstate the queue.
    pub gate_wait_ms: u64,
}

// ── auth ─────────────────────────────────────────────────────────────────────

/// The configured secret, resolved once. `None` means no doorstop is
/// configured — [`API_KEY_ENV`] unset or blank — which closes the route.
fn expected_key() -> Option<&'static str> {
    static KEY: OnceLock<Option<String>> = OnceLock::new();
    KEY.get_or_init(|| {
        std::env::var(API_KEY_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
    .as_deref()
}

/// The comparison `key_ok` delegates to, pulled out as a pure function of its
/// inputs so it is testable without depending on process env — every unit test
/// in this binary shares the same [`expected_key`] `OnceLock`.
fn key_ok_against(expected: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    let Some(got) = headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    use subtle::ConstantTimeEq;
    got.trim().as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Deliberately shoddy: one shared secret in its own header, with no built-in
/// default — unlike `require_auth`, a deployment that never sets
/// [`API_KEY_ENV`] gets the route closed, not open.
///
/// This is a conscious departure from `require_auth`, and it is contained here
/// on purpose. It touches neither the `auth_tokens` session table nor the
/// `api_keys` table, and it never consults `state.api_key` — so no existing
/// route is reachable through it, and no credential minted for koblem proper is
/// accepted by it. The blast radius is exactly the paperworld routes that call it —
/// `/api/v1/oneshot` and `/api/v1/character_sheet`, which share the one doorstop because they
/// share the one caller.
pub(crate) fn key_ok(headers: &HeaderMap) -> bool {
    key_ok_against(expected_key(), headers)
}

// ── handler ──────────────────────────────────────────────────────────────────

/// The body contract alone, with no auth check — split out from [`preflight`]
/// so it is testable with no doorstop configured (there is no built-in key any
/// more; see [`key_ok`]).
fn validate_body(body: &[u8]) -> Result<OneshotRequest, (StatusCode, String)> {
    let req: OneshotRequest = serde_json::from_slice(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid request JSON: {e}"),
        )
    })?;
    if req.image.positive.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "image.positive is required".to_string(),
        ));
    }
    if let Some(s) = &req.sound
        && s.prompt.trim().is_empty()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "sound.prompt is required when `sound` is present — omit `sound` for image only"
                .to_string(),
        ));
    }
    Ok(req)
}

/// "Engine not configured" is a deployment fault, not a render failure, so it is
/// loud. Only the IMAGE engines qualify: an unconfigured sa3 still returns 200
/// with a `sound_error`, because losing the image over a missing optional sound
/// engine is the one thing this contract must not do.
///
/// Shared with `/summon`, which renders its image half through the same two.
pub(crate) fn image_engines_ready(state: &AppState) -> Result<(), (StatusCode, String)> {
    if !state.krea2.is_configured() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "krea2 service not configured (set KREA2_SERVICE_URL or the DB setting)".to_string(),
        ));
    }
    if !state.matting.is_configured() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "matting service not configured (set MATTING_SERVICE_URL) — a cut-out sprite is \
             the only image these routes return, so they cannot run without it"
                .to_string(),
        ));
    }
    Ok(())
}

/// Everything that can reject a request before any engine is touched: the shared
/// secret, then the body (see [`validate_body`]).
fn preflight(headers: &HeaderMap, body: &[u8]) -> Result<OneshotRequest, (StatusCode, String)> {
    if !key_ok(headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            format!("missing or wrong {API_KEY_HEADER}"),
        ));
    }
    validate_body(body)
}

async fn oneshot(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let req = match preflight(&headers, &body) {
        Ok(r) => r,
        Err((status, msg)) => return err(status, &msg),
    };
    if let Err((status, msg)) = image_engines_ready(&state) {
        return err(status, &msg);
    }

    let caller = extract_caller(&headers);
    let started = Instant::now();

    // Concurrent by design: separate gate budget lines, so the two renders
    // overlap instead of queueing behind each other.
    let (image, sound) = tokio::join!(
        render_image(&state, &req.image),
        render_sound(&state, req.sound.as_ref()),
    );

    let render_ms = started.elapsed().as_millis() as u64;
    let gate_wait_ms = image.gate_wait_ms.max(sound.gate_wait_ms);

    log_async(
        state.db.clone(),
        CreateLog {
            service: SERVICE.into(),
            endpoint: "oneshot".into(),
            request_meta: json!({
                "prompt": req.image.positive,
                "sound_prompt": req.sound.as_ref().map(|s| &s.prompt),
                "width": image.width,
                "height": image.height,
            }),
            response_meta: json!({
                "image_ok": image.b64.is_some(),
                "image_error": image.error,
                "sound_ok": sound.b64.is_some(),
                "sound_error": sound.error,
                "matting_backend": image.matting_backend,
            }),
            sample_id: None,
            gate_wait_ms: gate_wait_ms as i64,
            duration_ms: render_ms as i64,
            // The image is what makes this call worth anything; a sound-only
            // failure is still a usable summon.
            status: if image.b64.is_some() { 200 } else { 502 },
            caller,
        },
    );

    Json(OneshotResponse {
        image_b64: image.b64,
        image_error: image.error,
        sound_b64: sound.b64,
        sound_error: sound.error,
        render_ms,
        gate_wait_ms,
    })
    .into_response()
}

// ── image half ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub(crate) struct ImageOutcome {
    pub(crate) b64: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) gate_wait_ms: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) matting_backend: Option<String>,
}

/// krea2 → matting → alpha check. Every exit that isn't a usable cut-out is an
/// `error`, never a fallback to the opaque render.
///
/// The dimensions are resolved up front and reported whatever happens, so a
/// failed render still logs what was attempted.
pub(crate) async fn render_image(state: &AppState, spec: &ImageSpec) -> ImageOutcome {
    let (body, width, height) = crate::routes::krea2::t2i_body(
        spec.positive.trim(),
        spec.negative.as_deref().filter(|s| !s.trim().is_empty()),
        spec.width.unwrap_or(DEFAULT_WIDTH),
        spec.height.unwrap_or(DEFAULT_HEIGHT),
        spec.seed,
    );
    let mut out = ImageOutcome {
        width,
        height,
        ..Default::default()
    };
    match cutout_render(state, body, &mut out).await {
        Ok((b64, backend)) => {
            out.b64 = Some(b64);
            out.matting_backend = backend;
        }
        Err(e) => out.error = Some(e),
    }
    out
}

/// The fallible half of [`render_image`]. Writes the gate wait into `out` as
/// soon as it is known so it survives an error return.
async fn cutout_render(
    state: &AppState,
    mut body: serde_json::Value,
    out: &mut ImageOutcome,
) -> Result<(String, Option<String>), String> {
    // Blocks until the budget scheduler can place it — a queued render is normal,
    // not a failure. `None` is the one non-blocking outcome: administratively
    // disabled.
    let (guard, gate_wait) = state
        .krea2
        .acquire()
        .await
        .ok_or("krea2 administratively disabled")?;
    out.gate_wait_ms = gate_wait.as_millis() as u64;

    inject_gpu(&mut body, guard.chosen_gpu());
    // Pure text-to-image: no reference media, so nothing to attach.
    let rendered = state.krea2.generate(&body, &[]).await;
    drop(guard);

    let result = rendered.map_err(|e| {
        warn!(error = %e, "oneshot krea2 render failed");
        format!("krea2: {e}")
    })?;
    let first = result
        .images
        .into_iter()
        .next()
        .ok_or("krea2 returned no images")?;

    let b64 = base64::engine::general_purpose::STANDARD;
    let png = first.bytes;
    if png.is_empty() {
        return Err("krea2 returned an empty image".to_string());
    }

    // Background removal is mandatory here, so it gets a real retry rather than
    // the chained krea2 route's non-fatal shrug.
    let (cutout, backend) = cut_out(state, png).await?;
    assert_renderable(cutout.to_vec()).await?;

    // The `/summon` reply is base64 by contract — a game client that cannot set an
    // Authorization header on an <img> reads the sprite out of the JSON. So this is the ONE
    // encode on the path, at the edge, rather than one on every internal hop (§5a-a).
    Ok((b64.encode(&cutout), backend))
}

/// Ask the matting service for an RGBA cut-out, retrying once. Returns the PNG
/// bytes and the backend the service reported.
///
/// Every knob is left unset on purpose: the service's own defaults are the alpha
/// cut-out, and `bg_mode`/`mask_only` are the two that would quietly produce
/// something unusable here (a flattened image, or a greyscale mask).
async fn cut_out(state: &AppState, rendered: Bytes) -> Result<(Bytes, Option<String>), String> {
    let opts = RemoveOptions::default();
    let mut last = String::new();
    for attempt in 1..=MATTING_ATTEMPTS {
        match state
            .matting
            .remove_single_raw("oneshot.png", rendered.clone(), &opts)
            .await
        {
            Ok(r) if r.png.is_empty() => last = "matting returned no results".to_string(),
            Ok(r) => return Ok((r.png, r.backend)),
            Err(e) => last = format!("matting: {e}"),
        }
        warn!(attempt, error = %last, "oneshot background removal failed");
        if attempt < MATTING_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
    Err(last)
}

/// Reject a cut-out that would draw nothing.
///
/// The empty-matte case is real: hand the model something the segmenter finds no
/// subject in and it returns a fully transparent PNG, which the caller crops to
/// an empty bounding box and renders as an invisible sprite — a silent failure
/// that looks like an engine bug. Better to say so here.
///
/// Threshold is the caller's own alpha-mask cutoff, not merely "any non-zero
/// alpha": a matte made entirely of softer pixels is equally invisible.
async fn assert_renderable(png: Vec<u8>) -> Result<(), String> {
    // Decoding a 1024² PNG is single-digit milliseconds of pure CPU — small, but
    // not something to run on the async worker while renders are in flight.
    tokio::task::spawn_blocking(move || {
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .map_err(|e| format!("cut-out is not a decodable PNG: {e}"))?
            .to_rgba8();
        if decoded.pixels().any(|p| p.0[3] >= ALPHA_VISIBLE) {
            Ok(())
        } else {
            Err(
                "background removal produced an empty cut-out (nothing above the \
                 alpha-mask threshold) — the segmenter found no subject"
                    .to_string(),
            )
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("cut-out check panicked: {e}")))
}

// ── sound half ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub(crate) struct SoundOutcome {
    pub(crate) b64: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) gate_wait_ms: u64,
}

impl SoundOutcome {
    fn failed(error: String, gate_wait_ms: u64) -> Self {
        Self {
            b64: None,
            error: Some(error),
            gate_wait_ms,
        }
    }
}

/// sa3 text→audio, fetched as bytes rather than left on disk.
///
/// `save: false` and no gallery row: these are throwaway one-shots keyed to a
/// transient world object, and the wav is read straight off the job with
/// `download` instead of round-tripping through `SONGS_DIR` and an
/// authenticated file GET.
pub(crate) async fn render_sound(state: &AppState, spec: Option<&SoundSpec>) -> SoundOutcome {
    // No sound asked for: null, and no error. Not a failure.
    let Some(spec) = spec else {
        return SoundOutcome::default();
    };
    if !state.sa3.is_configured() {
        return SoundOutcome::failed(
            "sa3 service not configured (set SA3_SERVER_URL)".to_string(),
            0,
        );
    }

    let req = Sa3GenerateRequest {
        prompt: spec.prompt.trim().to_string(),
        seconds: Some(
            spec.seconds
                .unwrap_or(DEFAULT_SECONDS)
                .clamp(MIN_SECONDS, MAX_SECONDS),
        ),
        seed: spec.seed,
        save: false,
        ..Default::default()
    };
    let mut body = crate::routes::sa3::build_generate_body(&req);

    let Some((guard, gate_wait)) = state.sa3.acquire().await else {
        return SoundOutcome::failed("sa3 administratively disabled".to_string(), 0);
    };
    let gate_wait_ms = gate_wait.as_millis() as u64;
    inject_gpu(&mut body, guard.chosen_gpu());

    let bytes = async {
        let job_id = state.sa3.submit_generate(&body).await?;
        state.sa3.await_job(&job_id).await?;
        let (_content_type, bytes) = state.sa3.download(&job_id).await?;
        Ok::<_, crate::sa3::Error>(bytes)
    }
    .await;
    drop(guard);

    match bytes {
        Ok(bytes) => SoundOutcome {
            b64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            error: None,
            gate_wait_ms,
        },
        Err(e) => {
            warn!(error = %e, "oneshot sa3 render failed");
            SoundOutcome::failed(format!("sa3: {e}"), gate_wait_ms)
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse(v: Value) -> Result<OneshotRequest, serde_json::Error> {
        serde_json::from_value(v)
    }

    // The shape paperworld sends. `sound` is optional at the type level, so an
    // image-only body must not be a deserialisation error.
    #[test]
    fn sound_is_optional_and_absent_means_silence() {
        let req = parse(json!({ "image": { "positive": "a goose" } })).unwrap();
        assert!(req.sound.is_none());
        assert!(req.image.negative.is_none());

        let req = parse(json!({
            "image": { "positive": "a goose", "negative": "blurry", "width": 768, "height": 768 },
            "sound": { "prompt": "an angry honk", "seconds": 1.5 },
        }))
        .unwrap();
        assert_eq!(req.image.width, Some(768));
        assert_eq!(req.sound.as_ref().unwrap().seconds, Some(1.5));

        // Explicit null is the same as absent.
        let req = parse(json!({ "image": { "positive": "a goose" }, "sound": null })).unwrap();
        assert!(req.sound.is_none());
    }

    // Null is the "didn't happen" signal, so the keys must survive serialisation
    // rather than being skipped — the caller branches on their value.
    #[test]
    fn every_response_field_is_always_present() {
        let body = serde_json::to_value(OneshotResponse {
            image_b64: None,
            image_error: Some("krea2: boom".into()),
            sound_b64: None,
            sound_error: None,
            render_ms: 11_000,
            gate_wait_ms: 0,
        })
        .unwrap();
        assert_eq!(
            body,
            json!({
                "image_b64": null,
                "image_error": "krea2: boom",
                "sound_b64": null,
                "sound_error": null,
                "render_ms": 11_000,
                "gate_wait_ms": 0,
            })
        );
    }

    // The image half must go out at Krea 2's distilled defaults and, critically,
    // must never carry an edit field — this route shares the production builder.
    #[test]
    fn image_body_is_a_plain_t2i_render_at_the_distilled_defaults() {
        let (body, w, h) =
            crate::routes::krea2::t2i_body("a goose", Some("blurry"), 1024, 1024, None);
        assert_eq!((w, h), (1024, 1024));
        assert_eq!(body["prompt"], "a goose");
        assert_eq!(body["negative_prompt"], "blurry");
        assert_eq!(body["steps"], 8);
        assert_eq!(body["cfg_scale"], 1.0);
        assert_eq!(body["seed"], -1);
        for k in [
            "ref_images",
            "ref_image_args",
            "lora",
            "model",
            "init_image",
        ] {
            assert!(body.get(k).is_none(), "oneshot t2i body must not carry {k}");
        }
    }

    // Dimensions go through the same clamp as the public route (×16, ≤2048).
    #[test]
    fn dimensions_are_clamped_by_the_shared_builder() {
        let (_, w, h) = crate::routes::krea2::t2i_body("x", None, 4096, 1023, None);
        assert_eq!((w, h), (2048, 1008));
    }

    // Absent seconds must become the short one-shot default, not sa3-server's
    // own 10 s, and an absurd request must not park the music budget line.
    #[test]
    fn sound_seconds_defaults_short_and_clamps() {
        let build = |seconds: Option<f32>| {
            let req = Sa3GenerateRequest {
                prompt: "a honk".into(),
                seconds: Some(
                    seconds
                        .unwrap_or(DEFAULT_SECONDS)
                        .clamp(MIN_SECONDS, MAX_SECONDS),
                ),
                save: false,
                ..Default::default()
            };
            crate::routes::sa3::build_generate_body(&req)
        };
        // Compared against the constants rather than literals: `seconds` is an
        // f32 on the wire, so 0.1 serialises as 0.10000000149011612 and a
        // literal 0.1 (an f64) would never match.
        assert_eq!(build(None)["seconds"], json!(DEFAULT_SECONDS));
        assert_eq!(build(Some(600.0))["seconds"], json!(MAX_SECONDS));
        assert_eq!(build(Some(0.0))["seconds"], json!(MIN_SECONDS));
        // The invariant behind the constant: a summon one-shot, not sa3's 10 s.
        assert!(build(None)["seconds"].as_f64().unwrap() < 3.0);
    }

    fn headers(key: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = key {
            h.insert(API_KEY_HEADER, v.parse().unwrap());
        }
        h
    }

    /// With no `ONESHOT_API_KEY` configured at all, the route must be closed —
    /// there is no built-in key to fall back to. `expected_key()` is memoized in
    /// a `OnceLock`, so this only exercises the pure `key_ok` contract directly
    /// rather than the process env, which every test in this binary shares.
    #[test]
    fn no_configured_key_rejects_every_caller() {
        let headers = HeaderMap::new();
        assert!(!key_ok_against(None, &headers));
        let mut with_header = HeaderMap::new();
        with_header.insert(API_KEY_HEADER, "anything".parse().unwrap());
        assert!(!key_ok_against(None, &with_header));
    }

    #[test]
    fn a_configured_key_accepts_only_an_exact_match() {
        let expected = Some("real-secret".to_string());
        assert!(key_ok_against(
            expected.as_deref(),
            &headers(Some("real-secret"))
        ));
        // Whitespace from a shell-set header is forgiven; a wrong value is not.
        assert!(key_ok_against(
            expected.as_deref(),
            &headers(Some("  real-secret  "))
        ));
        for bad in [None, Some(""), Some("real-secre"), Some("hunter2")] {
            assert!(
                !key_ok_against(expected.as_deref(), &headers(bad)),
                "accepted {bad:?}"
            );
        }
    }

    /// A bad request is a 4xx; everything else this route can hit is a 200 with
    /// an error field. Auth is checked BEFORE the body is parsed, so a malformed
    /// body from an unauthorised caller still reads as 401.
    #[test]
    fn the_body_contract_is_checked_once_auth_would_have_passed() {
        let reject = |b: serde_json::Value| {
            validate_body(b.to_string().as_bytes())
                .expect_err("must reject")
                .0
        };
        // Not JSON at all, and JSON that isn't this shape.
        assert_eq!(
            validate_body(b"{not json").expect_err("must reject").0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(reject(json!({})), StatusCode::BAD_REQUEST);
        assert_eq!(reject(json!({ "image": {} })), StatusCode::BAD_REQUEST);
        // Present but blank — the engines would render noise from it.
        assert_eq!(
            reject(json!({ "image": { "positive": "   " } })),
            StatusCode::BAD_REQUEST
        );
        // `sound` present means it must be usable; omitting it is the way to
        // ask for silence.
        assert_eq!(
            reject(json!({ "image": { "positive": "a goose" }, "sound": { "prompt": "" } })),
            StatusCode::BAD_REQUEST
        );
    }

    /// With no doorstop configured (unset in this test binary, same as CI),
    /// auth is checked before the body is parsed — a malformed body from an
    /// unauthorised caller still reads as 401, not 400.
    #[test]
    fn malformed_requests_are_rejected_before_any_engine_is_touched() {
        assert_eq!(
            preflight(&headers(None), b"{not json")
                .expect_err("must reject")
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            preflight(&headers(Some("hunter2")), b"{not json")
                .expect_err("must reject")
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    // A fully transparent matte is the silent failure this route exists to
    // catch: it would crop to an empty bbox and render as an invisible sprite.
    #[tokio::test]
    async fn an_empty_cutout_is_rejected_and_a_visible_one_is_not() {
        let png = |alpha: u8| {
            let mut buf = std::io::Cursor::new(Vec::new());
            let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, alpha]));
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            buf.into_inner()
        };
        assert!(assert_renderable(png(255)).await.is_ok());
        // Present, but entirely below the caller's alpha-mask cutoff.
        assert!(assert_renderable(png(64)).await.is_err());
        assert!(assert_renderable(png(0)).await.is_err());
        assert!(assert_renderable(b"not a png".to_vec()).await.is_err());
    }
}
