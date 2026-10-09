//! HTTP client for MiniMax Music 3 — the `music3` plugin on the `comfy-api` shim.
//!
//! # What this engine is, and what it is NOT
//!
//! Music 3 is text→song: a structured CAPTION plus tagged LYRICS become a complete track (44.1 kHz
//! stereo, up to 360 s). It is a two-stage model and both stages matter to this client:
//!
//!   1. an **autoregressive** stage inside the text encoder, which emits the acoustic conditioning
//!      sequence and DECIDES THE SONG'S LENGTH, then
//!   2. a flow-matching DiT sampled by an ordinary `KSampler`.
//!
//! 🔴 **`max_duration` is a CEILING, not a target.** The AR stage may end the song early, and the
//! latent is sized from what it actually produced (`MiniMaxMusic3TextEncode`'s `seconds` output is
//! wired into `EmptyMiniMaxMusic3LatentAudio`). So the authoritative length is
//! [`Music3Track::duration_seconds`] off the completion body, never the number that was requested.
//! Showing the request's ceiling as "the length" is wrong on every song the model chose to end.
//!
//! 🔴 **Only stage 2 has step progress.** ComfyUI's websocket reports the sampler's steps and
//! nothing else, so the AR stage is a long stretch at 0/N. That is why [`Music3Progress::phase`]
//! exists and why any UI must show it: a bar pinned at zero with no label reads as a hang, and the
//! AR stage is a large share of wall clock.
//!
//! 🔴 **There is no audio input of any kind.** The DAV VAE cannot encode — ComfyUI's `sd.py`
//! installs a `_no_encode` that raises — so there is no audio-to-audio, no inpaint, no
//! continuation, no clone and no reference timbre. Everything acestep / songgen / sa3 do with a
//! source wav is structurally impossible here, and the plugin answers **501** naming the reason
//! rather than dropping the field. Nothing in this module should ever grow an upload path.
//!
//! # Gate placement: it takes the `minimax_h3` line, and that is deliberate
//!
//! `music3`, `h3` and `ltx25` are three plugins on ONE comfy-api process in front of ONE ComfyUI
//! with a **single-slot queue**. They physically cannot render at once and they contend for the
//! same resident-weights budget, so this client acquires the **`minimax_h3`** budget line — exactly
//! as the LTX-2.5 client does (see `ltx_video.rs`). Giving Music 3 a
//! line of its own would let the gate admit it beside an H3 render, double-count the card, and hold
//! a second card's worth of budget hostage for the ~700 s the first render blocks inside comfy's
//! queue — a reservation nothing is using.
//!
//! For the same reason this client **registers no drain/unload URL**. That URL is the comfy-api
//! container, already registered under `HeavyKind::MinimaxH3` by [`crate::minimax_h3`]; registering
//! it again under any other key would point the gate's SIGKILL at the wrong container.
//!
//! Consequence worth stating plainly: disabling `minimax_h3` in the GPU scheduler disables Music 3
//! too. That is correct — it is one container.
//!
//! # Render shape: a background task and a job store (it did not start that way)
//!
//! `POST /api/v1/music3/generate` returns 202 and the render runs in a task; the browser polls
//! `GET /api/v1/music3/jobs/{id}`. It shipped synchronous — the acestep/songgen/sa3 shape — with a
//! documented threshold for when that would stop being right. Real renders crossed it. The decision,
//! the measurements and what the store deliberately does NOT do are in `music3_jobs.rs`.
//!
//! ## Cost model — per second of DELIVERED audio, never per second of the cap
//!
//! ```text
//!   untiled       ~2.2x realtime     120 s cap → ~269 s wall     360 s cap → ~800 s
//!   tiled @ 256   ~1.65x realtime    120 s cap → ~203 s wall     360 s cap → ~599 s
//! ```
//!
//! 🔴 Both lines assume the ComfyUI service runs `--vram-headroom` 6 or lower, and that is a
//! LAUNCH flag on someone else's container, not anything a request can set. At headroom 8 the
//! 8758 MB autoregressive text encoder does not fit in the 5060 Ti's budget and is re-fetched
//! over PCIe once per token — 25 tokens per second of audio — which measured **2.3x wall on
//! the identical request** (a 40 s song: 71.9 s → 176.8 s). If renders here start reading ~2x
//! the published `eta_seconds`, that flag is the first thing to check; the output is unchanged
//! either way (the two arms produce bit-identical files), so nothing else will look wrong.
//!
//! 🔴 It is roughly PROPORTIONAL, and an earlier reading of these same renders that called it
//! *sublinear* was an arithmetic error worth naming, because the mistake is easy to repeat: it
//! divided wall clock by the **requested cap** on a song that ended early (a 120 s request that
//! delivered 71.2 s), which makes long renders look disproportionately cheap. Divide by delivered
//! seconds and the multiple is near-constant.
//!
//! So a per-second rate IS a sound mental model here — as long as it is per second of audio that
//! actually came out. Since nothing knows that in advance, do not estimate with it: consume
//! [`Music3SubmitPlan::eta_seconds`], which the engine fits and evaluates at the cap so it stays an
//! upper bound.
//!
//! A client that goes away is no longer the render's problem — the task owns the guard — but
//! [`InFlightClear`] still cancels the engine job whenever the poll loop ends without a verdict,
//! which is now the cancel path and the task-abort path rather than the disconnect path.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use kob_gpu_gate::{GpuLock, GpuMinimaxH3Guard};
use serde_json::Value;

/// Connect timeout guards a dead host. No request timeout: a 5-minute song is minutes of render
/// and we poll with our own loop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the poll loop checks job status.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Transient poll failures (a dropped connection, a brief 5xx) shouldn't abandon a long render.
const MAX_POLL_RETRIES: u32 = 20;
/// Backoff between retried polls after a transient error.
const POLL_RETRY_BACKOFF: Duration = Duration::from_secs(2);

// ── model law, echoed from the engine's capabilities and duplicated here ──────────────────────
//
// 🔴 These are `comfy/ldm/minimax_music/ar.py` constants. They exist here so a request can be
// rejected in milliseconds instead of after the gate wait, and they are the SECOND copy — the
// engine's `/music3/v1/capabilities` is the first and the UI reads that one. If the model's limits
// move, both move.

/// `AUDIO_FRAMES_PER_SECOND`. The AR stage emits 25 acoustic frames per second of audio.
pub const AUDIO_FRAMES_PER_SECOND: f64 = 25.0;
/// `MAX_AUDIO_FRAMES` (9000) / 25 — six minutes, the hard ceiling.
pub const MAX_DURATION_SECONDS: f64 = 360.0;
/// The node's own floor: one audio frame.
pub const MIN_DURATION_SECONDS: f64 = 0.04;
/// `MAX_PROMPT_TOKENS` — the tokenised caption+lyrics ceiling.
pub const MAX_PROMPT_TOKENS: usize = 5000;
/// A deliberately loose char-per-token heuristic for the pre-flight length check.
///
/// ⚠️ **It is a heuristic and the engine owns the real check.** Rejecting here is only about
/// turning "you waited for the GPU and then got a 400" into "you got a 400". It is set generously
/// (4 chars/token is optimistic for English prose and very optimistic for CJK) so this side never
/// refuses something the tokeniser would have accepted.
pub const CHARS_PER_TOKEN_ESTIMATE: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("music3 HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("music3 returned {0}: {1}")]
    Upstream(u16, String),
    #[error("music3 service not configured")]
    NotConfigured,
    #[error("music3 job {0} failed: {1}")]
    JobFailed(String, String),
    #[error("music3 job {0} was cancelled")]
    Cancelled(String),
    #[error("music3 response missing required field: {0}")]
    MissingField(&'static str),
}

/// Terminal/transient state of a job. The core's five, shared with the H3 plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Queued,
    Generating,
    Completed,
    Failed,
    Cancelled,
}

/// The `202` a submit came back with, minus the id.
///
/// Kept whole for the same reason H3 keeps its plan echo: it carries the numbers the caller did
/// NOT choose. A `null` seed came back drawn and echoed, a `takes: 3` became three seeded entries,
/// and `ignored` names every field the plugin accepted and discarded. Without the echo, "my
/// setting did nothing" is undiscoverable.
///
/// 🔴 **THE ACCEPT BODY IS FLAT.** `{id, status, queue_position, tracks, warnings, ignored, …}` —
/// there is no `plan` wrapper here. That body is the core's, shared verbatim with the H3 and LTX
/// plugins, so it could not be reshaped for this engine without moving two live wires. The
/// nested `plan` block exists **only on the poll body**, which is why the authoritative plan
/// arrives at completion ([`Music3Completion::plan`]) rather than at submit.
#[derive(Debug, Clone, Default)]
pub struct Music3SubmitPlan {
    /// Engine job id.
    pub id: String,
    /// Per-track echo: `{index, seed, max_duration, steps, cfg, cfg_scale, top_k, sampler,
    /// scheduler, format}`. Raw JSON on purpose — this is a REPORT, and a typed mirror would need
    /// revising every time the plugin adds a diagnostic field.
    pub tracks: Vec<Value>,
    /// Which DiT the plugin resolved (`minimax_music3_dit_int8_convrot`), when it says.
    pub model: Option<String>,
    /// The plugin's ETA for the whole job, from its `extra` block. **Populated as of 2026-08-14.**
    ///
    /// 🔴 **Consume this; never re-derive it.** The plugin fits the curve and publishes both the
    /// number and its provenance: `capabilities.eta_model` carries
    /// `by_tiled_decode: {false: {...}, true: {...}}` — a pair of fits, not one, because tiling
    /// changes the intercept AND the slope — plus the measured points and a note. A caller that
    /// wants the curve reads it from the engine, the same rule as the rest of the model law; a
    /// second copy of the coefficients in koblem would drift the moment the engine is re-fitted,
    /// and drift silently, because a wrong ETA still looks like an ETA.
    ///
    /// (Fitted on DELIVERED seconds and evaluated at the cap, so it is an upper bound by
    /// construction: `untiled 3.9 + 2.21·cap`, `tiled 5.1 + 1.65·cap` as refitted 2026-08-16
    /// on three points per arm. Recorded here as orientation only — read the live values, do
    /// not hardcode these. The previous pair, `1.1 + 2.23` / `13.0 + 1.57`, is what this drift
    /// looks like in practice: the tiled one was measured at a tile size the engine no longer
    /// ships.)
    ///
    /// ⚠️ It is an **upper** estimate, and deliberately so: the AR stage routinely ends a song
    /// early (see [`Music3Track::duration_seconds`]), so the real wall clock often lands well under
    /// it. That is exactly why nothing should drive a progress BAR from it — a bar keyed on this
    /// reaches 100% and then stalls. Show it as text.
    ///
    /// `eta_basis` says what the figure came from, so an estimate can never be mistaken for a
    /// measurement.
    pub eta_seconds: Option<f64>,
    pub eta_basis: Option<String>,
    /// Non-fatal notes: a clamped duration, a `quality` ignored on a lossless format.
    pub warnings: Vec<String>,
    /// Fields the request carried that the plugin accepted and discarded.
    pub ignored: Vec<String>,
}

impl Music3SubmitPlan {
    /// The seeds actually drawn, in track order — what a "same song again" retake needs.
    pub fn seeds(&self) -> Vec<i64> {
        self.tracks
            .iter()
            .filter_map(|t| t.get("seed").and_then(Value::as_i64))
            .collect()
    }

    /// The accept echo as JSON for the generation record / the render log row.
    ///
    /// `engine_plan` is the poll body's authoritative `plan` block when the render finished, and
    /// `None` when it did not. Kept as a separate key rather than merged over the top: the two are
    /// produced at different moments, and silently overwriting the accepted values with the final
    /// ones would erase the only evidence that the engine changed its mind mid-job.
    pub fn to_json(&self, engine_plan: Option<&Value>) -> Value {
        serde_json::json!({
            "tracks": self.tracks,
            "model": self.model,
            "eta_seconds": self.eta_seconds,
            "eta_basis": self.eta_basis,
            "warnings": self.warnings,
            "ignored": self.ignored,
            "take_count": self.tracks.len(),
            "engine_plan": engine_plan,
        })
    }
}

/// What a finished render hands back.
pub struct Music3Completion {
    pub tracks: Vec<Music3Track>,
    /// 🔴 The poll body's `plan` block — the AUTHORITATIVE plan, and the only place the full one
    /// exists. The accept body is flat (see [`Music3SubmitPlan`]), so this cannot be read at
    /// submit time; it arrives with the completion or not at all.
    pub plan: Option<Value>,
}

/// In-flight progress, parsed off every poll.
///
/// 🔴 Every field is optional and absence means UNKNOWN, never zero. A job that has not reached the
/// sampler yet reports no steps at all, and "step 0 of 0" is what a progress bar draws as
/// *finished*.
/// `Deserialize` as well as `Serialize`: this rides inside `Music3Job`, which is persisted as a
/// sidecar so a render survives a koblem restart.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Music3Progress {
    /// 0.0-1.0 over the whole job.
    pub progress: Option<f64>,
    pub current_step: Option<u32>,
    pub total_steps: Option<u32>,
    /// 🔴 **The field that stops a healthy render looking hung.** `conditioning` is the AR stage —
    /// unstepped, and a large share of wall clock, so `current_step` stays absent throughout it.
    /// `denoising` is the DiT, which does report steps. `decoding` is the DAV VAE.
    ///
    /// 🔴 Do not hardcode either word downstream. Capabilities publishes
    /// `progress.stepped_phases` / `progress.unstepped_phases`; drive the UI off those, so a new
    /// phase word cannot silently become an unlabelled bar stuck at zero. (`denoising` is the
    /// core's existing vocabulary, shared with H3 and LTX — it is not a Music-3 coinage.)
    pub phase: Option<String>,
    /// Which take is rendering, and how many there are.
    pub track_index: Option<u32>,
    pub track_count: Option<u32>,
    pub tracks_done: Option<u32>,
    /// The engine's own ETA for this job, carried over from the accept body so a page that joins
    /// mid-render can still show it.
    ///
    /// 🔴 An **upper** estimate — see [`Music3SubmitPlan::eta_seconds`]. Render it as text
    /// ("about 2m 45s"), never as the denominator of a progress bar: songs routinely finish well
    /// under it, so a bar keyed on this hits 100% and then sits there.
    pub eta_seconds: Option<f64>,
}

/// One finished track off a completion body.
///
/// 🔴 `duration_seconds` is the RESOLVED length from the AR stage and is routinely shorter than the
/// requested `max_duration`. It is the only honest number for "how long is this song".
#[derive(Debug, Clone, Default)]
pub struct Music3Track {
    pub index: u32,
    pub seed: Option<i64>,
    /// 🔴 The core's `media` field — **the only sanctioned way to reach a take's bytes.** The core
    /// hardcodes `/jobs/{id}/generations/{n}` (a `/tracks/{n}` mount also answers today, which is
    /// exactly the trap: a path this client constructed would keep working right up until the core
    /// moved it, and then fetch the wrong file rather than 404). Named `media` after the wire.
    pub media: Option<String>,
    pub content_type: String,
    pub duration_seconds: Option<f64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub bytes: Option<u64>,
}

impl Music3Track {
    /// The file extension implied by `content_type`. Used to name the saved clip, because the
    /// request's `format` is what was ASKED for and the response is what arrived.
    ///
    /// 🔴 There is no `wav` arm, and that is not an omission: **ComfyUI has no wav writer at all.**
    /// `SaveAudioAdvanced` offers flac / mp3 / opus and `SaveAudio` is flac-only, so these three
    /// are the complete set this engine can produce. An unrecognised type gets `bin` rather than a
    /// plausible-looking guess — a mislabelled container that plays anyway is how a format change
    /// goes unnoticed for months.
    pub fn extension(&self) -> &'static str {
        match self.content_type.as_str() {
            "audio/mpeg" | "audio/mp3" => "mp3",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/ogg" | "audio/opus" => "opus",
            _ => "bin",
        }
    }
}

#[derive(Clone)]
pub struct Music3Client {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    /// Job id of the render currently in flight, so the cancel route can reach it.
    in_flight: Arc<RwLock<Option<String>>>,
    /// The accepted job's ETA, held so `/progress` can report it for the whole render. It arrives
    /// on the 202 and appears in no later body, so without this a page that reloads mid-render
    /// loses it.
    eta: Arc<RwLock<Option<f64>>>,
    /// Latest progress for the in-flight render, for `GET /api/v1/music3/progress`.
    ///
    /// A shared snapshot rather than a channel because the consumer is a BROWSER POLLING while its
    /// own `POST /generate` is still open. A channel would need a subscriber alive at the moment
    /// each update is published; a snapshot answers whoever asks, whenever they ask, and survives a
    /// page refresh mid-render. It is also the whole reason a job store is not needed here: the
    /// render is one synchronous request (the acestep/songgen shape), and the only thing the page
    /// needs out-of-band is "which phase, which step".
    progress: Arc<RwLock<Music3Progress>>,
}

impl Music3Client {
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
            eta: Arc::new(RwLock::new(None)),
            progress: Arc::new(RwLock::new(Music3Progress::default())),
        }
    }

    /// The in-flight render's latest progress. Default (every field `None`) when idle — which the
    /// UI must draw as "nothing running", never as 0%.
    ///
    /// The ETA is overlaid from the accepted job rather than parsed per-poll: it is published once,
    /// on the 202, and never repeated.
    pub fn progress(&self) -> Music3Progress {
        let mut p = self
            .progress
            .read()
            .expect("music3 progress poisoned")
            .clone();
        p.eta_seconds = *self.eta.read().expect("music3 eta poisoned");
        p
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalised = base_url
            .as_ref()
            .map(|s| s.trim_end_matches('/').to_string());
        *self.base_url.write().expect("music3 base_url poisoned") = normalised;
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("music3 base_url poisoned")
            .is_some()
    }

    fn base(&self) -> Result<String, Error> {
        self.base_url
            .read()
            .expect("music3 base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        Ok(format!("{}{}", self.base()?, path))
    }

    /// Acquire the GPU on the **`minimax_h3`** budget line — the comfy-api container's one slot,
    /// shared with the `h3` and `ltx25` plugins. `None` = administratively disabled.
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

    /// `GET /music3/v1/capabilities` — the model law (duration ceiling, prompt-token cap, sampler
    /// and format lists, `audio_conditioning: false`).
    ///
    /// Read while a UI is BUILDING a request, so the editor shows the real limits without
    /// submitting anything and without touching the GPU. This is the authority; the constants in
    /// this module are the fast-reject copy.
    pub async fn capabilities(&self) -> Result<Value, Error> {
        let resp = self
            .http
            .get(self.url("/music3/v1/capabilities")?)
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

    /// `POST /music3/v1/generate` — JSON only. There is no multipart form here and there never
    /// will be: the model takes no audio (see the module header).
    pub async fn submit(&self, request: &Value) -> Result<Music3SubmitPlan, Error> {
        let resp = self
            .http
            .post(self.url("/music3/v1/generate")?)
            .json(request)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            // Every rejection from the plugin carries a JSON body. A rejection from the layer ABOVE
            // it carries none, and a bare "returned 400: " sends the reader to the wrong log.
            let body = match resp.text().await {
                Ok(b) if !b.trim().is_empty() => b,
                Ok(_) => "<no body: rejected before the music3 plugin ran — the comfy-api HTTP \
                          layer or a proxy; check the comfy-api log and /health, then retry>"
                    .to_string(),
                Err(e) => format!("<body unreadable: {e}>"),
            };
            return Err(Error::Upstream(status, body));
        }
        let body: Value = resp.json().await?;
        let plan = parse_submit(&body)?;
        *self.in_flight.write().expect("music3 in_flight poisoned") = Some(plan.id.clone());
        // Published once, here, and in no later body — so stash it for `/progress`.
        *self.eta.write().expect("music3 eta poisoned") = plan.eta_seconds;
        Ok(plan)
    }

    /// `GET /music3/v1/jobs/{id}` — poll once.
    async fn poll(&self, job_id: &str) -> Result<Value, Error> {
        let resp = self
            .http
            .get(self.url(&format!("/music3/v1/jobs/{job_id}"))?)
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

    /// Poll to completion, publishing progress into the shared snapshot as it changes.
    ///
    /// The snapshot is cleared on exit — success, failure or cancel — by [`InFlightClear`], so an
    /// idle client reports "nothing running" rather than the last render's frozen 30/30, which a
    /// page loaded after a render would otherwise draw as a job stuck at the finish line.
    pub async fn wait(&self, job_id: &str) -> Result<Music3Completion, Error> {
        let mut clear = InFlightClear {
            slot: self.in_flight.clone(),
            eta: self.eta.clone(),
            progress: self.progress.clone(),
            client: self.clone(),
            job_id: job_id.to_string(),
            settled: false,
        };
        let mut transient: u32 = 0;
        let mut last_progress = Music3Progress::default();
        loop {
            let body = match self.poll(job_id).await {
                Ok(body) => {
                    transient = 0;
                    body
                }
                Err(e) => {
                    transient += 1;
                    if transient > MAX_POLL_RETRIES {
                        return Err(e);
                    }
                    tracing::warn!(
                        job = %job_id, attempt = transient, error = %e,
                        "music3 poll failed; retrying"
                    );
                    tokio::time::sleep(POLL_RETRY_BACKOFF).await;
                    continue;
                }
            };
            let now = parse_progress(&body);
            if now != last_progress {
                *self.progress.write().expect("music3 progress poisoned") = now.clone();
                last_progress = now;
            }
            // 🔴 An UNKNOWN status maps to `generating`, not to an error. The plugin may add fields
            // freely, and a client that failed closed on a status it had not been taught would
            // abandon a live render. (The other half of that contract is the plugin's: it must not
            // invent a sixth TERMINAL status, which this would read as "still working" forever.)
            let status = match body.get("status").and_then(Value::as_str) {
                Some("queued") => JobStatus::Queued,
                Some("generating") => JobStatus::Generating,
                Some("completed" | "done") => JobStatus::Completed,
                Some("failed") => JobStatus::Failed,
                Some("cancelled") => JobStatus::Cancelled,
                _ => JobStatus::Generating,
            };
            // Every arm below leaves the loop having learned the engine's own verdict, so there is
            // nothing left to cancel. Setting this BEFORE returning is what stops the drop guard
            // firing a pointless cancel at a job that already finished.
            match status {
                JobStatus::Completed => {
                    clear.settled = true;
                    return Ok(Music3Completion {
                        tracks: parse_tracks(&body),
                        // 🔴 Read here and nowhere else: the nested `plan` exists only on the poll
                        // body, so the completion is the one chance to capture it.
                        plan: body.get("plan").filter(|v| v.is_object()).cloned(),
                    });
                }
                JobStatus::Failed => {
                    clear.settled = true;
                    let msg = body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("generation failed")
                        .to_string();
                    return Err(Error::JobFailed(job_id.to_string(), msg));
                }
                JobStatus::Cancelled => {
                    clear.settled = true;
                    return Err(Error::Cancelled(job_id.to_string()));
                }
                JobStatus::Queued | JobStatus::Generating => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// Fetch one finished track's bytes, by **following the poll body's `media` field**.
    ///
    /// 🔴 There is deliberately NO constructed-path fallback. The core owns that URL and hardcodes
    /// `/jobs/{id}/generations/{n}`; a `/tracks/{n}` mount answers today too, so a path built here
    /// would work — until the core moved, at which point it would quietly fetch some other take's
    /// bytes instead of failing. A missing `media` is therefore an error naming the field, which is
    /// loud, recoverable and impossible to mistake for a correct render.
    pub async fn fetch_track(&self, track: &Music3Track) -> Result<(String, bytes::Bytes), Error> {
        let media = track
            .media
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or(Error::MissingField("tracks[].media"))?;
        let url = if media.starts_with("http://") || media.starts_with("https://") {
            media.to_string()
        } else if let Some(rest) = media.strip_prefix('/') {
            format!("{}/{}", self.base()?, rest)
        } else {
            format!("{}/{}", self.base()?, media)
        };
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or(track.content_type.as_str())
            .to_string();
        Ok((content_type, resp.bytes().await?))
    }

    /// `POST /music3/v1/jobs/{id}/cancel`. No-op (Ok) when idle.
    pub async fn cancel_in_flight(&self) -> Result<Value, Error> {
        let job_id = self
            .in_flight
            .read()
            .expect("music3 in_flight poisoned")
            .clone();
        let Some(job_id) = job_id else {
            return Ok(serde_json::json!({ "status": "idle" }));
        };
        self.cancel(&job_id).await
    }

    /// `POST /music3/v1/jobs/{id}/cancel` for a specific job id.
    ///
    /// ⚠️ Honouring it is per-stage. Inside the AR loop the checkpoints are frequent; inside the
    /// sampler they are one step apart. Cancel frees no VRAM either — only the gate's
    /// `/v1/admin/unload` does, and that belongs to the H3 client that registered it.
    pub async fn cancel(&self, job_id: &str) -> Result<Value, Error> {
        let resp = self
            .http
            .post(self.url(&format!("/music3/v1/jobs/{job_id}/cancel"))?)
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(Error::Upstream(status.as_u16(), body.to_string()));
        }
        Ok(body)
    }

    /// `DELETE /music3/v1/job?id=<engine-job-id>` — drop the staged artifacts. Idempotent.
    pub async fn delete_job(&self, job_id: &str) -> Result<(), Error> {
        let url = self.url(&format!(
            "/music3/v1/job?id={}",
            percent_encoding::utf8_percent_encode(job_id, percent_encoding::NON_ALPHANUMERIC)
        ))?;
        let resp = self.http.delete(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(())
    }
}

/// Parse the `202`. `id` is the only hard requirement — everything else is the plugin reporting
/// back, and a missing field must not fail a render that was accepted.
///
/// 🔴 Every field is read FLAT. The core's accept body has no `plan` wrapper; reading
/// `/plan/tracks` here silently produced an empty echo — no seeds, no per-take settings — on a
/// render that had in fact reported all of them.
fn parse_submit(body: &Value) -> Result<Music3SubmitPlan, Error> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(Error::MissingField("id"))?;
    Ok(Music3SubmitPlan {
        id,
        tracks: body
            .get("tracks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        // From the plugin's `extra` block, which the core merges flat into the accept body.
        eta_seconds: body
            .get("eta_seconds")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite()),
        eta_basis: body
            .get("eta_basis")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        warnings: string_array(body.get("warnings")),
        ignored: string_array(body.get("ignored")),
    })
}

fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Pull the progress block out of a poll body. Absence is UNKNOWN, never zero.
fn parse_progress(body: &Value) -> Music3Progress {
    let u32_at = |k: &str| body.get(k).and_then(Value::as_u64).map(|v| v as u32);
    let tracks = body.get("tracks").and_then(Value::as_array);
    Music3Progress {
        progress: body.get("progress").and_then(Value::as_f64),
        current_step: u32_at("current_step"),
        total_steps: u32_at("total_steps"),
        phase: body
            .get("phase")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        track_index: u32_at("track_index"),
        track_count: u32_at("track_count").or_else(|| tracks.map(|a| a.len() as u32)),
        tracks_done: u32_at("tracks_done").or_else(|| {
            tracks.map(|a| {
                a.iter()
                    .filter(|t| {
                        matches!(
                            t.get("status").and_then(Value::as_str),
                            Some("done" | "completed")
                        )
                    })
                    .count() as u32
            })
        }),
        // Not on a poll body — the engine publishes it once, on the 202. `progress()` overlays the
        // accepted job's value on the way out.
        eta_seconds: None,
    }
}

/// Read the finished tracks off a completion body.
///
/// A unit with no `index` is skipped rather than positionally guessed: attributing a song to the
/// wrong take is silent, and a missing take is not.
fn parse_tracks(body: &Value) -> Vec<Music3Track> {
    body.get("tracks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let index = t.get("index").and_then(Value::as_u64)? as u32;
                    Some(Music3Track {
                        index,
                        seed: t.get("seed").and_then(Value::as_i64),
                        // `url` accepted as an alias only because it costs nothing; `media` is the
                        // field the core writes.
                        media: t
                            .get("media")
                            .or_else(|| t.get("url"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                        content_type: t
                            .get("content_type")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("audio/mpeg")
                            .to_string(),
                        // 🔴 The AR stage's answer, not the request's ceiling.
                        duration_seconds: t
                            .get("duration_seconds")
                            .and_then(Value::as_f64)
                            .filter(|d| d.is_finite() && *d > 0.0),
                        sample_rate: t
                            .get("sample_rate")
                            .and_then(Value::as_u64)
                            .map(|v| v as u32),
                        channels: t.get("channels").and_then(Value::as_u64).map(|v| v as u32),
                        bytes: t.get("bytes").and_then(Value::as_u64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Tears down one render's client-side state, and **cancels the engine job if the poll loop did not
/// reach a verdict of its own**.
///
/// 🔴 The cancel is the load-bearing half, and it exists because of how this engine is driven.
/// A Music 3 render is ONE SYNCHRONOUS REQUEST: `POST /api/v1/music3/generate` stays open for the
/// whole render (the acestep/songgen shape — see the module header for why, and for the threshold
/// that would change it). When the browser goes away — a reload, a navigation, a proxy idle
/// timeout — axum drops the handler future, which drops the poll loop, which drops this. Without a
/// cancel here that leaves a **live render burning the card with nobody collecting it**, until
/// comfy-api's TTL sweeper eventually deletes its output. There is a recorded incident on this box
/// of that exact shape (a chain killed by a wall clock, then its inputs swept mid-render).
///
/// 🔴 And unlike H3, this client CANNOT recover from orphaning it. H3 persists `server_job_id` in
/// the koblem job store and has `reattach_job`; Music 3's engine job id lives **only** in
/// `in_flight`, which this same guard clears — so a job orphaned here is unreachable by the
/// `/cancel` route forever after. Copying H3's `InFlightClear` verbatim into a context with no job
/// store behind it is what would have made that hole; cancelling closes it.
///
/// Cancelling on disconnect is also the RIGHT policy for this shape rather than a lesser evil:
/// there is no way to reattach to a synchronous render, so its output was already unreachable the
/// moment the client left. Killing it returns the card instead of donating it.
///
/// ⚠️ The cancel is fire-and-forget on a spawned task, because `Drop` cannot await. It is
/// best-effort by construction: a failure is logged, not retried. Note also that cancelling frees
/// no VRAM — only the gate's `/v1/admin/unload` does — so the gate's budget is released by the
/// guard drop while comfy is still winding the render down. That window is inherent to cancel on
/// this engine and is not introduced here.
struct InFlightClear {
    slot: Arc<RwLock<Option<String>>>,
    eta: Arc<RwLock<Option<f64>>>,
    progress: Arc<RwLock<Music3Progress>>,
    client: Music3Client,
    job_id: String,
    /// Set by the poll loop when the ENGINE reported a terminal state. False means we stopped
    /// watching for our own reasons — a dropped future, or poll retries exhausted — and the render
    /// may well still be running.
    settled: bool,
}

impl Drop for InFlightClear {
    fn drop(&mut self) {
        if let Ok(mut p) = self.progress.write() {
            *p = Music3Progress::default();
        }
        if let Ok(mut e) = self.eta.write() {
            *e = None;
        }
        if let Ok(mut g) = self.slot.write() {
            *g = None;
        }
        if self.settled {
            return;
        }
        // `try_current` rather than `tokio::spawn`: this can run during runtime shutdown, where a
        // bare spawn panics — and a panic in a Drop is far worse than a missed cancel.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                job = %self.job_id,
                "🔴 music3 render abandoned with no tokio runtime to cancel it — the engine job is \
                 STILL RENDERING and nothing will collect it"
            );
            return;
        };
        let client = self.client.clone();
        let job_id = self.job_id.clone();
        handle.spawn(async move {
            match client.cancel(&job_id).await {
                Ok(_) => tracing::info!(
                    job = %job_id,
                    "music3 render abandoned (client went away); cancelled the engine job"
                ),
                Err(e) => tracing::warn!(
                    job = %job_id, error = %e,
                    "🔴 music3 render abandoned AND the cancel failed — the engine job may still be \
                     rendering; check comfy-api's queue"
                ),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// 🔴 **THE ACCEPT BODY IS FLAT.** This test exists because the first version of this client
    /// read `/plan/tracks`, which the core does not emit — and the failure was silent: a render
    /// that reported two drawn seeds came back with an empty echo and nobody would have noticed
    /// until someone tried to reproduce a take. The nested `plan` belongs to the POLL body.
    #[test]
    fn a_submit_reads_the_flat_accept_body_not_a_plan_wrapper() {
        let plan = super::parse_submit(&json!({
            "id": "mm3_71ab", "status": "queued", "queue_position": 0,
            "model": "minimax_music3_dit_int8_convrot",
            "tracks": [
                {"index": 0, "seed": 8123, "max_duration": 120.0, "steps": 30,
                 "cfg": 1.7, "cfg_scale": 1.7, "top_k": 50, "format": "mp3"},
                {"index": 1, "seed": 8124, "max_duration": 120.0, "steps": 30,
                 "cfg": 1.7, "cfg_scale": 1.7, "top_k": 50, "format": "mp3"}
            ],
            "eta_seconds": 244.0, "eta_basis": "measured:120s@30steps",
            "warnings": ["quality is ignored for format=flac"],
            "ignored": ["reference_audio_strength"],
        }))
        .expect("a 202 must parse");
        assert_eq!(plan.id, "mm3_71ab");
        assert_eq!(plan.seeds(), vec![8123, 8124]);
        assert_eq!(
            plan.model.as_deref(),
            Some("minimax_music3_dit_int8_convrot")
        );
        assert_eq!(plan.eta_seconds, Some(244.0));
        assert_eq!(plan.ignored, vec!["reference_audio_strength"]);

        // A body shaped the OLD way — everything under `plan` — must yield an empty echo rather
        // than appearing to work, so a regression here is visible in this test and not in prod.
        let wrapped = super::parse_submit(&json!({
            "id": "mm3_71ab",
            "plan": {"tracks": [{"index": 0, "seed": 8123}], "model": "x"},
        }))
        .expect("parses");
        assert!(wrapped.tracks.is_empty());
        assert_eq!(wrapped.model, None);

        // The ETA is null until a render has been measured; that must read as "unknown", never 0.
        let no_eta = super::parse_submit(&json!({ "id": "m", "eta_seconds": null })).expect("ok");
        assert_eq!(no_eta.eta_seconds, None);

        // Everything but `id` is optional: an accepted render must not fail on a field the plugin
        // stopped sending.
        let bare = super::parse_submit(&json!({ "id": "mm3_71ab" })).expect("parses");
        assert!(bare.tracks.is_empty());
        assert!(bare.seeds().is_empty());
        assert_eq!(bare.to_json(None)["take_count"], json!(0));
        assert!(super::parse_submit(&json!({ "status": "queued" })).is_err());
    }

    /// 🔴 The one number this engine gets wrong if you trust the request: the song is 118.44 s, not
    /// the 240 s ceiling that was asked for. The AR stage ended it early, which is normal.
    #[test]
    fn the_resolved_duration_wins_over_the_requested_ceiling() {
        let tracks = super::parse_tracks(&json!({
            "status": "completed",
            "tracks": [{
                "index": 0, "status": "done", "seed": 8123,
                // 🔴 `media`, and the core's real path shape — `generations`, not `tracks`.
                "media": "/music3/v1/jobs/mm3_71ab/generations/0",
                "content_type": "audio/mpeg", "duration_seconds": 118.44,
                "sample_rate": 44100, "channels": 2, "bytes": 2874112,
            }],
        }));
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].duration_seconds, Some(118.44));
        assert_eq!(tracks[0].extension(), "mp3");
        assert_eq!(tracks[0].sample_rate, Some(44100));
        assert_eq!(
            tracks[0].media.as_deref(),
            Some("/music3/v1/jobs/mm3_71ab/generations/0")
        );

        // A body that says nothing about length yields None — "unknown", which the caller must
        // render as unknown rather than as zero.
        let quiet = super::parse_tracks(&json!({"tracks": [{"index": 0}]}));
        assert_eq!(quiet[0].duration_seconds, None);
        assert_eq!(quiet[0].content_type, "audio/mpeg");
        // No `media` ⇒ None, and `fetch_track` turns that into a named error rather than guessing
        // a path. There is no wav arm anywhere: ComfyUI cannot write one.
        assert_eq!(quiet[0].media, None);
        assert_eq!(quiet[0].extension(), "mp3");

        // An entry with no index is dropped: mis-attributing a take is silent, a missing one isn't.
        assert!(super::parse_tracks(&json!({"tracks": [{"seed": 1}]})).is_empty());
    }

    /// The three formats ComfyUI can actually write, and nothing else. An unknown content type is
    /// named `bin` on purpose — a mislabelled container that still plays is how a format change
    /// survives unnoticed.
    #[test]
    fn only_the_three_writable_formats_map_to_an_extension() {
        let ext = |ct: &str| {
            super::parse_tracks(&json!({"tracks": [{"index": 0, "content_type": ct}]}))[0]
                .extension()
        };
        assert_eq!(ext("audio/flac"), "flac");
        assert_eq!(ext("audio/mpeg"), "mp3");
        assert_eq!(ext("audio/opus"), "opus");
        assert_eq!(ext("audio/wav"), "bin");
    }

    /// 🔴 The AR stage reports no steps. `phase` is the only thing separating "conditioning, be
    /// patient" from "hung", so it must survive the parse even when every step field is absent.
    #[test]
    fn the_unstepped_conditioning_phase_is_still_reported() {
        let p = super::parse_progress(&json!({
            "status": "generating", "phase": "conditioning",
            "track_index": 0,
            "tracks": [{"index": 0, "status": "generating"}, {"index": 1, "status": "queued"}],
        }));
        assert_eq!(p.phase.as_deref(), Some("conditioning"));
        // Absent, NOT zero — a bar drawn from `Some(0)/Some(0)` reads as finished.
        assert_eq!(p.current_step, None);
        assert_eq!(p.total_steps, None);
        // Derived from the units when the body doesn't say it outright.
        assert_eq!(p.track_count, Some(2));
        assert_eq!(p.tracks_done, Some(0));

        // 🔴 `denoising`, the core's own word — shared with H3 and LTX. Not `sampling`.
        let denoising = super::parse_progress(&json!({
            "status": "generating", "phase": "denoising",
            "current_step": 9, "total_steps": 30, "progress": 0.3,
            "tracks": [{"index": 0, "status": "done"}, {"index": 1, "status": "generating"}],
        }));
        assert_eq!(denoising.phase.as_deref(), Some("denoising"));
        assert_eq!(denoising.current_step, Some(9));
        assert_eq!(denoising.tracks_done, Some(1));

        assert_eq!(
            super::parse_progress(&json!({"status": "queued"})),
            super::Music3Progress::default()
        );
    }
}
