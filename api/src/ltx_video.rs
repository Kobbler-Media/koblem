//! HTTP client for the `ltx25` plugin on the `comfy-api` shim — LTX-2.5 under ComfyUI, and the
//! only LTX engine koblem has. The C++ `ltx-video` service (LTX-2.3) it was originally written
//! against is gone from the deployment; the module keeps its name because the WIRE is that
//! service's ("koblem can be repointed at this service with no client change" was a stated goal of
//! the plugin's contract).
//!
//! Like flux2, the server is an async job queue: `POST /ltx/v1/generate` (multipart:
//! a `request` JSON part + per-segment `audio_<i>` wav parts) returns a job id, the
//! worker child renders the whole chain, and the client polls `GET /sdcpp/v1/jobs/{id}`
//! to completion. `POST /sdcpp/v1/jobs/{id}/cancel` aborts a queued/in-flight render.
//!
//! koblem owns the orchestration (script, audio separate/slice, bgm mux) in
//! `routes/ltx_video.rs`; this module is the RPCs + the acquire + the poll loop.
//!
//! # The gate line, and why it is not `Ltx`
//!
//! This client acquires the shared [`GpuLock`] on the **`minimax_h3`** budget line, not one of its
//! own. `ltx25` and `h3` are two plugins on ONE comfy-api process in front of ONE ComfyUI, with a
//! single-slot queue: they cannot render at once and they hold the same resident weights. A second
//! line would let the gate admit an H3 render beside an LTX one and double-count the card.
//!
//! For the same reason this client registers **no** drain/unload URL. The one it would register is
//! the comfy-api container, which the H3 client has already registered under
//! `HeavyKind::MinimaxH3`; a second entry under `HeavyKind::Ltx` would only give the gate a second
//! name for one process to SIGKILL.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, GpuLtxGuard};
use koblem_client::types::LtxLoraInfo;
use serde_json::Value;

use crate::wire::{PartSupport, WireForm, WireMeter};

/// The WHOLE audio timeline, handed to the engine once so IT can slice and mux it.
///
/// Both fields are RIFF/WAVE (the engine's only decoder — `load_wav_16k` /
/// `load_wav_full` in `src/longcat_audio.hpp`); koblem transcodes uploads to WAV, which is a
/// FORMAT conversion, not timing arithmetic — the point of this path is that no client-side
/// number mirrors an engine internal.
///
/// * `drive` — conditions lip-sync. The engine cuts each segment's window from it using its
///   own (content-adaptive) seam trim, anchored to the true accumulated timeline.
/// * `track` — muxed verbatim as the deliverable audio, cut to the ACTUAL stitched length.
///   May be the same bytes as `drive` (voice drives AND delivers).
///
/// Empty (`default()`) = the legacy pre-sliced `audio_<i>` path.
///
/// `shot_drive` / `shot_track` are the PER-SHOT overlay (multipart `audio_full_<i>` /
/// `audio_track_<i>`), indexed by the engine's FILTERED segment index and sparse — a `None` entry
/// contributes nothing. Each clip starts at its own shot and is cut at the shot boundary, and the
/// engine composites them over `drive`/`track`, so the whole-timeline pair keeps working as a bed
/// that each shot replaces its own span of. Requires the seam trim to be declared
/// (`segment_seam_drop_frames`) — the shot grid is `visible[i] = render[i] - drop[i]`.
#[derive(Default, Clone)]
pub struct ChainAudio {
    pub drive: Option<Vec<u8>>,
    pub track: Option<Vec<u8>>,
    pub shot_drive: Vec<Option<Vec<u8>>>,
    pub shot_track: Vec<Option<Vec<u8>>>,
}

/// Connect timeout guards a dead host. No request timeout: a chain render runs for
/// minutes and we poll with our own loop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-request deadline for the METADATA calls only — capabilities, and the bank probe built on it.
///
/// 🔴 The client-wide default has to stay unset (a render is minutes), but that also meant an engine
/// which accepts the TCP connection and then never answers — a ComfyUI busy with someone else's
/// prompt — hung the caller forever. `resumable_now` asks this on every 1.5 s poll of a failed
/// LTX-2.5 job, so each hung probe opened another connection behind the last one.
const META_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an answer from [`LtxVideoClient::bank_enabled`] is trusted.
///
/// 🔴 There has to be one, in both directions. Cached forever, an operator who sets
/// `COMFY_API_LTX_BANK_DIR` and restarts ComfyUI keeps getting 503s from the resume gate until
/// koblem itself is restarted — and the same in reverse. Not cached at all, a FAILED probe is
/// re-asked on every poll, so an engine that is merely down costs a connect timeout per poll. A
/// short TTL is both answers: the miss is bounded and the recovery is automatic.
const BANK_LIVE_TTL: Duration = Duration::from_secs(60);

/// How often the poll loop checks job status. Chain renders are minutes long, so a
/// slower cadence than flux's 250ms is fine.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Transient poll failures (a dropped connection, a brief 5xx) shouldn't abandon an
/// hour-long render. Retry the poll this many consecutive times before giving up.
const MAX_POLL_RETRIES: u32 = 20;
/// Backoff between retried polls after a transient error.
const POLL_RETRY_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("comfy-api ltx25 HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("comfy-api ltx25 returned {0}: {1}")]
    Upstream(u16, String),
    #[error("ltx service not configured")]
    NotConfigured,
    #[error("comfy-api ltx25 job {0} failed: {1}")]
    JobFailed(String, String),
    #[error("comfy-api ltx25 job {0} was cancelled")]
    Cancelled(String),
    #[error("comfy-api ltx25 response missing required field: {0}")]
    MissingField(&'static str),
    #[error("comfy-api ltx25 job {0} returned undecodable media: {1}")]
    BadMedia(String, String),
    #[error("streaming media to disk failed: {0}")]
    Stream(#[from] crate::wire::StreamError),
}

/// Terminal/transient state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Queued,
    Generating,
    Completed,
    Failed,
    Cancelled,
}

/// A progressively-delivered shot handed up from the poll loop the instant the engine finishes
/// it (while the rest of the chain still renders). The render task persists the bytes under the
/// koblem job dir and records a served URL, so the Director previews shots as they land.
pub struct LtxPartialChunk {
    pub segment_index: u32,
    pub bytes: Vec<u8>,
}

/// What `generate` hands back to the route handler: the stitched movie.
pub struct LtxRenderResult {
    /// The container bytes inlined as base64, when the engine chose to inline them. `None` when it
    /// did not — see [`LtxVideoClient::media_to_file`], which is how the bytes are obtained either
    /// way.
    pub video_b64: Option<String>,
    pub mime_type: String,
    pub output_format: String,
    pub fps: i64,
    pub frame_count: i64,
    /// 🔴 `/media` ON THIS JOB IS NOT THE TIMELINE. Set by the `ltx25` plugin for **two different
    /// reasons**, and the caller's action is the same for both: never copy these bytes over a
    /// stitched cut.
    ///
    ///   * a resume or a retake — `/media` is the RENDERED SUB-RANGE, `ffmpeg -c copy` concatenated.
    ///     [`rendered_segments`](Self::rendered_segments) names which shots.
    ///   * a FULL render whose concatenation failed — `/media` is segment 0 alone, and there is **no**
    ///     `rendered_segments`. [`job_media_is`](Self::job_media_is) is the only account of it.
    ///
    /// Emitted as `true` or ABSENT, never `false`.
    pub job_is_partial: bool,
    /// The caller's own segment indices this job rendered. Only present on a resume/retake — an
    /// empty list beside `job_is_partial` is the failed-concatenation case above.
    pub rendered_segments: Vec<u32>,
    /// The engine's prose account of what `/media` actually holds. Read (rather than logged) only
    /// where there is nothing structured to read: a failed concatenation.
    pub job_media_is: Option<String>,
}

/// The engine saying its concatenation FAILED, as the sentence to show the author — or `None` when
/// this render's `/media` is a fragment for the ordinary reason.
///
/// 🔴 A window render can fail to concatenate TOO, and then it reports both: the segments it
/// rendered AND the failure, appended to `job_media_is`. Keying the note off an empty
/// [`rendered_segments`](LtxRenderResult::rendered_segments) therefore dropped the only account of a
/// resume whose stitch died — the takes were on disk and nothing anywhere said the cut was not. The
/// prose is the only signal the plugin publishes for it, so it is what this reads.
///
/// A plain resume or retake returns `None`: "segments [3, 4] ONLY" is expected on every continue,
/// and putting it in front of the author on each one is how a warning banner stops being read.
///
/// ⚠️ Sniffing English prose for "concatenat" is the fragile part, and it is fragile because the
/// plugin publishes nothing structured for this case — `rendered_segments` is empty on BOTH a failed
/// full stitch and a failed windowed one, so it cannot separate them. If the plugin ever grows a
/// real flag, read that instead. A reworded message loses the note, not the render: the takes are on
/// disk either way and `job_is_partial` still stops the fragment becoming the cut.
pub fn concat_failure_note(result: &LtxRenderResult) -> Option<String> {
    if !result.job_is_partial {
        return None;
    }
    let note = result.job_media_is.as_deref();
    let failed = note.is_some_and(|s| s.to_ascii_lowercase().contains("concatenat"));
    if !failed && !result.rendered_segments.is_empty() {
        return None;
    }
    Some(note.map(str::to_string).unwrap_or_else(|| {
        "the engine could not concatenate this render's shots — every take is on disk, but the \
         stitched cut is not. Rebuild the cut from the takes."
            .to_string()
    }))
}

/// The latent-bank facts a poll body carries, forwarded on change like H3's progress block.
///
/// Every field defaults to "said nothing", so a body that publishes neither — an engine build
/// predating the bank, or one whose bank directory is unset — reads as a plain full render rather
/// than as a partial one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LtxBankReport {
    /// `(segment index, bank key)` for every segment whose entry the engine has CONFIRMED ON DISK.
    /// A segment that has not finished (or banked nothing) is absent, never null — the key is a
    /// promise a later resume or retake can name, so advertising one early is a 400 at the moment
    /// the caller is ready to use it.
    pub bank_keys: Vec<(u32, String)>,
    /// This job rendered only a SUB-RANGE of the caller's segments.
    ///
    /// 🔴 Not the same question as [`partial`](Self::partial), and conflating them is what made a
    /// failed concatenation skip its tier swap. Both of `partial`'s causes make `/media` a fragment;
    /// only this one means shots the caller asked for were not rendered at all. The two sources are
    /// the plan's own window echo and, on completion, a non-empty `rendered_segments` — which the
    /// engine emits for a window and never for a full render that merely failed to stitch.
    pub window: bool,
    /// This job's `/media` is not the whole timeline.
    ///
    /// 🔴 TWO SOURCES, because they become true at different times and for different reasons.
    /// `plan.render_window.mode != "full"` is a resume or a retake and is echoed by EVERY poll, so
    /// the flag is persisted early enough to survive a koblem restart. `result.job_is_partial` also
    /// covers a FULL render whose concatenation failed, and exists only on a completed body. Reading
    /// both here is what lets the restart-reattach path — which recovers a render that finished
    /// while koblem was down, from one status call and no `wait` — know either way.
    ///
    /// The window also publishes `rendered`, `restored_from_bank`, `carry_from`, `resume_job_id` and
    /// (when the window stops short of the end) `not_rendered_tail` + `tail_note`. None of them has a
    /// reader here: the indices koblem files takes under come off the units themselves, and the tail
    /// note restates `retakeSplicesTail: false`, which the editor already models per engine.
    pub partial: bool,
}

#[derive(Clone)]
pub struct LtxVideoClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    /// Job id of the render currently in flight, so the `/cancel` route can reach it.
    in_flight: Arc<RwLock<Option<String>>>,
    /// Last answer to [`bank_enabled`] and WHEN it was given, so the retake gate is not a round
    /// trip per shot and is not a permanent verdict either. `None` until something has asked.
    /// See [`bank_live`].
    bank_live: Arc<RwLock<Option<(bool, Instant)>>>,
    /// What the last submit and the last media fetch actually cost on the wire. See `wire.rs`.
    wire: WireMeter,
    /// Whether this engine build understands `part:<name>` media (§9.4).
    part_support: PartSupport,
}

impl LtxVideoClient {
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
            bank_live: Arc::new(RwLock::new(None)),
            wire: WireMeter::new(),
            part_support: PartSupport::new(),
        }
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalised = base_url
            .as_ref()
            .map(|s| s.trim_end_matches('/').to_string());
        *self.base_url.write().expect("ltx base_url poisoned") = normalised;
        // A different host is a different binary: what the old one supported says nothing.
        self.part_support.reset();
        *self.bank_live.write().expect("ltx bank_live poisoned") = None;
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("ltx base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("ltx base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the GPU as this engine's HEAVY consumer. Returns `None` if the service has been
    /// administratively disabled or the budget cannot be freed for it.
    ///
    /// 🔴 The **`minimax_h3`** line, not an `Ltx` one — see the module header. Nothing here may
    /// grow its own budget line while `ltx25` and `h3` share one ComfyUI.
    pub async fn acquire(&self) -> Option<(GpuLtxGuard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_minimax_h3().await?;
        Some((guard, start.elapsed()))
    }

    /// The tag this engine's wire measurements and gpu_logs rows are filed under.
    pub fn service_tag(&self) -> &'static str {
        "ltx25"
    }

    /// `GET /sdcpp/v1/capabilities` — samplers/schedulers/limits/defaults.
    pub async fn capabilities(&self) -> Result<Value, Error> {
        let url = self.url("/sdcpp/v1/capabilities")?;
        let resp = self.http.get(url).timeout(META_TIMEOUT).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// `GET /ltx/v1/bank` — every banked segment, newest first, with what it cost and what it can be
    /// continued into.
    ///
    /// Returned verbatim. koblem interprets none of it: an entry's core keys (`key`, geometry,
    /// `frames`, `latent_t`, `tail_frames`, `has_anchor`, `size_bytes`, `created`) sit beside
    /// plugin-specific ones the plugin chooses to publish (`job`, `segment`, `template`, `seed`,
    /// `fps`, `upscale`, `video_vae`, `cont_carry_*`, `segment_seam_drop_frames`,
    /// `frames_delivered`), and re-declaring that set here would put a third copy of it between the
    /// engine and the editor for no decision this process makes.
    ///
    /// 🔴 THE BANK IS PER PLUGIN and this URL resolves against the plugin's own route prefix, so a
    /// build without one answers 404 rather than an empty listing.
    pub async fn bank(&self) -> Result<Value, Error> {
        let url = self.url("/ltx/v1/bank")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// Whether this engine build can resume and retake **right now**.
    ///
    /// 🔴 A DEPLOYMENT FACT, not a feature flag, which is why it is asked rather than compiled in.
    /// The `ltx25` plugin is fully capable of both and answers 501 to each when
    /// `COMFY_API_LTX_BANK_DIR` is unset.
    ///
    /// `bank.enabled` is the flag that tracks that directory, and it is the one this question is
    /// really about. `resume.supported` and `retake.supported` are ANDed onto it so that a build
    /// which withdraws either is honoured — not because they move with the setting; treat them as
    /// the engine's own veto rather than as two more readings of the same fact.
    ///
    /// `None` when the engine does not answer or publishes no bank block — no live fact, so the
    /// caller keeps whatever its own capability record says.
    pub async fn bank_enabled(&self) -> Option<bool> {
        let caps = self.capabilities().await.ok()?;
        let flag = |block: &str, key: &str| {
            caps.pointer(&format!("/{block}/{key}"))
                .and_then(Value::as_bool)
        };
        let bank = flag("bank", "enabled")?;
        let live = bank
            && flag("resume", "supported").unwrap_or(false)
            && flag("retake", "supported").unwrap_or(false);
        self.remember_bank_live(live);
        Some(live)
    }

    /// Is the Prompt Relay node MOUNTED here — the `beats` half of the question
    /// [`bank_enabled`](Self::bank_enabled) asks about the latent bank.
    ///
    /// 🔴 A CAPABILITY IS NOT A DEPLOYMENT. `custom_nodes/ltx25_prompt_relay` is first-party and
    /// mounted into the comfyui container, and it only appears in `/object_info` once THAT
    /// container has restarted — separately from comfy-api, which serves this endpoint. Until it
    /// does, a request carrying `beats` is refused at graph build with a 501 naming the mount, and
    /// that refusal arrives **after** the GPU gate has been acquired and waited on. The engine
    /// probes `/object_info` on every capabilities call and publishes the answer, so the editor can
    /// hide the control rather than spend a queue wait discovering it.
    ///
    /// `None` when the engine does not answer, or is old enough not to publish the key — no live
    /// fact, so the caller keeps whatever its own capability record says.
    pub async fn beats_enabled(&self) -> Option<bool> {
        self.capabilities()
            .await
            .ok()?
            .get("beats")
            .and_then(Value::as_bool)
    }

    fn remember_bank_live(&self, live: bool) {
        *self.bank_live.write().expect("ltx bank_live poisoned") = Some((live, Instant::now()));
    }

    /// The same answer, from cache where there is one — the SERVER-SIDE gate on every resume and
    /// retake dispatch.
    ///
    /// 🔴 Asked at most once per [`BANK_LIVE_TTL`] rather than per shot. The unattended critic can
    /// dispatch a retake for every low-scoring shot of a twelve-shot project in one pass, and the
    /// answer is a deployment fact — a probe per shot would be twelve round trips to learn the same
    /// thing twelve times. `set_base_url` drops the cache outright, because a different host is a
    /// different deployment; the TTL covers the SAME host changing its mind, which is what an
    /// operator setting `COMFY_API_LTX_BANK_DIR` and restarting ComfyUI does.
    ///
    /// `false` when the engine cannot be reached: nothing can be resumed from a service that is not
    /// answering, and the alternative is a job that waits out the queue to fail. That answer is
    /// cached like any other — an unreachable engine used to be re-probed on every status poll,
    /// paying a connect timeout each time.
    pub async fn bank_live(&self) -> bool {
        if let Some((known, at)) = *self.bank_live.read().expect("ltx bank_live poisoned")
            && at.elapsed() < BANK_LIVE_TTL
        {
            return known;
        }
        let live = self.bank_enabled().await.unwrap_or(false);
        // `bank_enabled` only records an ANSWER; a probe that failed has to be recorded too, or the
        // next caller re-asks a service that is not there.
        self.remember_bank_live(live);
        live
    }

    /// `GET /sdapi/v1/loras` — the adapters the engine ACTUALLY has, from a live scan of its
    /// `--lora-model-dir`, annotated with whatever `loras.json` in that directory says about each.
    ///
    /// This is the authority on which adapters exist, and asking beats maintaining a copy: a name
    /// koblem offers that the engine cannot resolve is a hard 400 at RENDER time — minutes into a
    /// job, after the user picked it — whereas the engine's own listing cannot be out of step with
    /// its directory.
    ///
    /// Entries the manifest does not describe come back with just `path`, and
    /// [`LtxLoraInfo::for_path`] fills in the koblibs catalog's defaults — so this works against an
    /// engine predating the manifest, which returns `{name, path}` and nothing else.
    pub async fn loras(&self) -> Result<Vec<LtxLoraInfo>, Error> {
        let url = self.url("/sdapi/v1/loras")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let raw: Vec<Value> = resp.json().await?;
        Ok(raw
            .iter()
            .filter_map(|e| {
                // `path` is the name relative to the LoRA dir, extension included — the exact
                // string a request must send. `name` is the bare stem and does NOT resolve.
                let path = e.get("path")?.as_str()?.trim();
                if path.is_empty() {
                    return None;
                }
                // Catalog first, then let the manifest override it field by field. That order is
                // deliberate: the manifest sits next to the weights and is editable without a
                // rebuild, so an operator who states a strength there outranks a compiled-in one,
                // while a field the manifest omits keeps the reviewed default instead of silently
                // reverting to a neutral 1.0.
                let mut info = LtxLoraInfo::for_path(path);
                if let Some(v) = e
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    info.label = v.to_string();
                }
                if let Some(v) = e
                    .get("blurb")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    info.blurb = v.to_string();
                }
                if let Some(v) = e.get("default_multiplier").and_then(Value::as_f64) {
                    info.default_multiplier = v as f32;
                }
                if let Some(v) = e
                    .get("trigger")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    info.trigger = Some(v.to_string());
                    info.trigger_required = e
                        .get("trigger_required")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                }
                // Whether the listing engine can actually USE this adapter. `h3` and `ltx25` are
                // two plugins on one ComfyUI, and comfy resolves a LoRA name against its own
                // `models/loras` only — so putting H3 on ComfyUI meant symlinking H3's turbo
                // adapter in beside LTX's, where this listing's directory scan picks it up. Absent
                // (a build predating the flag) keeps the `for_path` default of `true`.
                //
                // 🔴 Read HERE and not via serde: this whole function parses `Vec<Value>` and
                // patches `for_path`'s output field by field, so a `#[serde(default)]` on the
                // struct never runs on this path. A field added to the struct and not added here
                // is silently always its default.
                if let Some(v) = e.get("for_this_model").and_then(Value::as_bool) {
                    info.for_this_model = v;
                }
                Some(info)
            })
            .collect())
    }

    /// Liveness: true when the server's `/health` returns 2xx.
    pub async fn healthy(&self) -> bool {
        let Ok(url) = self.url("/health") else {
            return false;
        };
        matches!(self.http.get(url).send().await, Ok(resp) if resp.status().is_success())
    }

    /// `GET /health` body verbatim. Used by `/props` to surface the
    /// last_render_sec hint for the UI progress estimate.
    pub async fn health(&self) -> Option<Value> {
        let url = self.url("/health").ok()?;
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    /// `POST /ltx/v1/generate` — submit a chain render (multipart). Returns the server job
    /// id and records it as in-flight so the `/cancel` route can reach it. The background
    /// render task calls this, then [`wait`](Self::wait) to poll it to completion.
    pub async fn submit(
        &self,
        request: &Value,
        audio_slices: &[Vec<u8>],
        media_parts: &[(String, Bytes)],
    ) -> Result<String, Error> {
        self.submit_full(request, audio_slices, &ChainAudio::default(), media_parts)
            .await
    }

    /// `POST /ltx/v1/generate` — submit a chain render, handing the engine the WHOLE audio
    /// timeline (`full`) instead of pre-sliced per-segment windows. See [`ChainAudio`].
    /// Passing an empty `full` is byte-identical to [`submit`](Self::submit).
    pub async fn submit_full(
        &self,
        request: &Value,
        audio_slices: &[Vec<u8>],
        full: &ChainAudio,
        media_parts: &[(String, Bytes)],
    ) -> Result<String, Error> {
        let job_id = self
            .submit_raw(request, audio_slices, full, media_parts)
            .await?;
        *self.in_flight.write().expect("ltx in_flight poisoned") = Some(job_id.clone());
        Ok(job_id)
    }

    /// `POST /ltx/v1/generate` — submit a chain render (multipart). Returns the job id.
    async fn submit_raw(
        &self,
        request: &Value,
        audio_slices: &[Vec<u8>],
        full: &ChainAudio,
        media_parts: &[(String, Bytes)],
    ) -> Result<String, Error> {
        let url = self.url("/ltx/v1/generate")?;
        // §9.4: `part:` markers only go to an engine that says it understands them; otherwise the
        // bytes go back into the document as base64 and this is the request it has always been
        // sent. Deciding here rather than in the route means one place knows, and the route
        // stages unconditionally.
        let inlined;
        let (request, media_parts): (&Value, &[(String, Bytes)]) =
            if media_parts.is_empty() || self.accepts_part_refs().await {
                (request, media_parts)
            } else {
                let mut fallback = request.clone();
                crate::director_assets::inline_parts(&mut fallback, media_parts);
                inlined = fallback;
                (&inlined, &[])
            };
        let mut form = WireForm::new().json("request", request);
        for (name, bytes) in media_parts {
            form = form
                .bytes(
                    name,
                    format!("{name}.bin"),
                    "application/octet-stream",
                    bytes.to_vec(),
                )
                .map_err(Error::Request)?;
        }
        for (i, bytes) in audio_slices.iter().enumerate() {
            form = form
                .bytes(
                    &format!("audio_{i}"),
                    format!("aud_{i}.wav"),
                    "audio/wav",
                    bytes.clone(),
                )
                .map_err(Error::Request)?;
        }
        // Engine-owned audio: the whole timeline, once. The engine cuts each segment's drive
        // window against its OWN seam trim and muxes the deliverable itself, so nothing here
        // needs to know the overlap. When these are present the engine IGNORES `audio_<i>`.
        if let Some(drive) = &full.drive {
            form = form
                .bytes("audio_full", "full.wav", "audio/wav", drive.clone())
                .map_err(Error::Request)?;
        }
        if let Some(track) = &full.track {
            form = form
                .bytes("audio_track", "track.wav", "audio/wav", track.clone())
                .map_err(Error::Request)?;
        }
        // Per-shot clips overlaid on that bed. Sparse — only shots that carry one are sent, and the
        // engine probes every index rather than stopping at the first gap.
        for (i, bytes) in full.shot_drive.iter().enumerate() {
            let Some(bytes) = bytes else { continue };
            form = form
                .bytes(
                    &format!("audio_full_{i}"),
                    format!("full_{i}.wav"),
                    "audio/wav",
                    bytes.clone(),
                )
                .map_err(Error::Request)?;
        }
        for (i, bytes) in full.shot_track.iter().enumerate() {
            let Some(bytes) = bytes else { continue };
            form = form
                .bytes(
                    &format!("audio_track_{i}"),
                    format!("track_{i}.wav"),
                    "audio/wav",
                    bytes.clone(),
                )
                .map_err(Error::Request)?;
        }
        self.wire
            .submitted(self.service_tag(), "generate", form.size());
        let resp = self
            .http
            .post(url)
            .multipart(form.into_form())
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            // The LTX handler rejects an unusable request with a JSON `error`, but a rejection from
            // the engine's HTTP layer — or from the worker supervisor mid-respawn — carries no body
            // at all. Say which, rather than surfacing `returned 400: ` and leaving nothing to go on.
            let body = match resp.text().await {
                Ok(b) if !b.trim().is_empty() => b,
                Ok(_) => "<no body: rejected before the LTX handler ran — engine HTTP layer or a \
                          worker respawn; check the comfy-api log and /health, then retry>"
                    .to_string(),
                Err(e) => format!("<body unreadable: {e}>"),
            };
            return Err(Error::Upstream(status, body));
        }
        let body: Value = resp.json().await?;
        body.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or(Error::MissingField("id"))
    }

    /// `GET /sdcpp/v1/jobs/{id}/segments/{n}` — one progressively-delivered shot's container bytes,
    /// served by the engine while the chain is still rendering (and after it completes). Mirrors
    /// [`fetch_media_to_file`](Self::fetch_media_to_file). Non-2xx (a shot not yet exposed /
    /// gone) → `Upstream`.
    pub async fn fetch_segment(&self, server_job_id: &str, n: u32) -> Result<Vec<u8>, Error> {
        let url = self.url(&format!("/sdcpp/v1/jobs/{server_job_id}/segments/{n}"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// The finished container bytes, streamed STRAIGHT TO `dest` — never resident in koblem.
    ///
    /// §9.1: fetching into a `Vec<u8>` and then writing it holds the whole artifact twice over,
    /// which on the p99 render is ~220 MB and on the biggest one on disk is 589 MB. This is the
    /// same GET with a chunked copy instead, and it is the ONLY way this client hands over a
    /// finished render — there is deliberately no bytes-returning variant to reach for.
    ///
    /// Returns the byte count written.
    pub async fn fetch_media_to_file(
        &self,
        server_job_id: &str,
        dest: &std::path::Path,
    ) -> Result<u64, Error> {
        let url = self.url(&format!("/sdcpp/v1/jobs/{server_job_id}/media"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let n = crate::wire::stream_to_file(resp, dest).await?;
        self.wire.fetched_raw(self.service_tag(), n);
        Ok(n)
    }

    /// The finished container on disk at `dest`, however the engine chose to deliver it.
    ///
    /// The raw branch streams. The inline-base64 branch cannot: the artifact arrived inside the
    /// poll body and is already a `String` in RAM, which is exactly what `SDCPP_JOB_MEDIA_B64=0`
    /// (B4) exists to stop. So this gets cheaper the moment that flag flips, with no caller change.
    pub async fn media_to_file(
        &self,
        server_job_id: &str,
        result: &LtxRenderResult,
        dest: &std::path::Path,
    ) -> Result<u64, Error> {
        let Some(b64) = result.video_b64.as_deref() else {
            return self.fetch_media_to_file(server_job_id, dest).await;
        };
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            .map_err(|e| Error::BadMedia(server_job_id.to_string(), e.to_string()))?;
        self.wire
            .fetched_inline_b64(self.service_tag(), bytes.len() as u64);
        tokio::fs::write(dest, &bytes)
            .await
            .map_err(|e| crate::wire::StreamError::Write {
                path: dest.display().to_string(),
                source: e,
            })?;
        Ok(bytes.len() as u64)
    }

    /// Does the engine take `part:<name>` media (§9.4)? Asked once per process per service, on
    /// the supervisor's `/health`, which answers while the worker is unloaded.
    pub async fn accepts_part_refs(&self) -> bool {
        if let Some(known) = self.part_support.cached() {
            return known;
        }
        match self.health().await {
            Some(body) => self.part_support.observe(&body),
            // Unreachable /health is not evidence of anything, so nothing is cached and the safe
            // transport is used for this submit: base64 works against every engine ever deployed.
            None => false,
        }
    }

    /// What the last submit and the last media fetch cost, for the render log row (§9.2).
    pub fn wire(&self) -> &WireMeter {
        &self.wire
    }

    /// `GET /sdcpp/v1/jobs/{id}` — one poll body, for a caller outside the poll loop.
    ///
    /// The restart-reattach path's only way to learn that a still-running job is a resume or a
    /// retake: it recovers the media directly when the render finished while koblem was down, so it
    /// never reaches [`wait`](Self::wait) and never sees a `result` block.
    pub async fn status(&self, job_id: &str) -> Result<Value, Error> {
        self.poll(job_id).await
    }

    /// `GET /sdcpp/v1/jobs/{id}` — poll once, returning the parsed body.
    async fn poll(&self, job_id: &str) -> Result<Value, Error> {
        let url = self.url(&format!("/sdcpp/v1/jobs/{job_id}"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// `POST /sdcpp/v1/jobs/{id}/cancel` — cancel the in-flight render. No-op (Ok) when
    /// nothing is rendering.
    pub async fn cancel_in_flight(&self) -> Result<Value, Error> {
        let job_id = self
            .in_flight
            .read()
            .expect("ltx in_flight poisoned")
            .clone();
        let Some(job_id) = job_id else {
            return Ok(serde_json::json!({ "status": "idle" }));
        };
        self.cancel(&job_id).await
    }

    /// `POST /sdcpp/v1/jobs/{id}/cancel` for a specific job id.
    pub async fn cancel(&self, job_id: &str) -> Result<Value, Error> {
        let url = self.url(&format!("/sdcpp/v1/jobs/{job_id}/cancel"))?;
        let resp = self.http.post(url).send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(Error::Upstream(status.as_u16(), body.to_string()));
        }
        Ok(body)
    }

    /// Delete a job's artifact dir in the engine by its opaque id. koblem calls this on Director
    /// project-delete / tier-wipe so a render's banked latents are cleaned up on demand rather than
    /// waiting out `COMFY_API_JOB_TTL`. Idempotent engine-side (a missing id succeeds); callers
    /// treat cleanup as best-effort.
    pub async fn delete_job(&self, job_id: &str) -> Result<(), Error> {
        let url = self.url("/ltx/v1/job")?;
        self.http
            .delete(url)
            .query(&[("id", job_id)])
            .send()
            .await?;
        Ok(())
    }

    /// Poll a submitted render to completion, surviving transient poll failures (a dropped
    /// connection / brief 5xx) so a network blip can't abandon an hour-long render. Clears
    /// the in-flight slot on exit. The caller [`submit`](Self::submit)s first to get `job_id`.
    pub async fn wait(&self, job_id: &str) -> Result<LtxRenderResult, Error> {
        self.wait_with_partials(job_id, None, None).await
    }

    /// Like [`wait`](Self::wait), but when the render asked for progressive per-segment delivery
    /// (`emit_segments`), each poll parses the response's `partials` array and, for every NEW
    /// `segment_index`, fetches that shot's clip ([`fetch_segment`](Self::fetch_segment)) and hands
    /// `(index, bytes)` to `sink` so the render task can persist it live — without waiting for the
    /// final stitched result. Best-effort: a fetch failure is logged and the index left unseen so a
    /// later poll can retry; it never blocks or fails the render. `sink = None` → identical to
    /// [`wait`](Self::wait).
    ///
    /// `bank` receives an [`LtxBankReport`] whenever one CHANGES. It is fed from every poll,
    /// including the one that reports a FAILED job: the plugin publishes a key for each segment that
    /// banked before the run died, which is the whole point of the salvage path — a twelve-shot run
    /// that failed on shot twelve can resume from shot eleven instead of re-rendering eleven shots.
    pub async fn wait_with_partials(
        &self,
        job_id: &str,
        sink: Option<tokio::sync::mpsc::UnboundedSender<LtxPartialChunk>>,
        bank: Option<tokio::sync::mpsc::UnboundedSender<LtxBankReport>>,
    ) -> Result<LtxRenderResult, Error> {
        let _clear = InFlightClear {
            slot: self.in_flight.clone(),
        };
        let mut transient: u32 = 0;
        // segment indices already delivered to the sink, so a shot that stays in `partials` across
        // many polls is fetched + forwarded exactly once.
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        // Only forward a CHANGE: at a 500 ms cadence most polls repeat the last one, and each
        // forwarded report rewrites the job sidecar.
        let mut last_bank = LtxBankReport::default();
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
                        "ltx poll failed; retrying"
                    );
                    tokio::time::sleep(POLL_RETRY_BACKOFF).await;
                    continue;
                }
            };
            // Progressive delivery: forward each newly-finished shot to the sink. Present while
            // generating AND on the completed body, so a shot that lands on the same poll as the
            // final result is still delivered. Best-effort — never fails the render.
            if let Some(tx) = &sink
                && let Some(arr) = body.get("partials").and_then(|v| v.as_array())
            {
                for p in arr {
                    let Some(idx) = p.get("segment_index").and_then(|v| v.as_u64()) else {
                        continue;
                    };
                    let idx = idx as u32;
                    if seen.contains(&idx) {
                        continue;
                    }
                    match self.fetch_segment(job_id, idx).await {
                        Ok(bytes) => {
                            seen.insert(idx);
                            // Receiver dropped (task gone) → stop trying to forward, keep rendering.
                            let _ = tx.send(LtxPartialChunk {
                                segment_index: idx,
                                bytes,
                            });
                        }
                        Err(e) => {
                            // Leave `idx` unseen so the next poll retries; a shot that never fetches
                            // just isn't previewed.
                            tracing::warn!(
                                job = %job_id, segment = idx, error = %e,
                                "ltx partial fetch failed; will retry"
                            );
                        }
                    }
                }
            }
            if let Some(tx) = &bank {
                let report = bank_report(&body);
                if report != last_bank {
                    last_bank = report.clone();
                    let _ = tx.send(report);
                }
            }
            let status = match body.get("status").and_then(|v| v.as_str()) {
                Some("queued") => JobStatus::Queued,
                Some("generating") => JobStatus::Generating,
                Some("completed") => JobStatus::Completed,
                Some("failed") => JobStatus::Failed,
                Some("cancelled") => JobStatus::Cancelled,
                _ => JobStatus::Generating,
            };
            match status {
                JobStatus::Completed => return parse_result(&body),
                JobStatus::Failed => {
                    let msg = body
                        .pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("generation failed")
                        .to_string();
                    return Err(Error::JobFailed(job_id.to_string(), msg));
                }
                JobStatus::Cancelled => return Err(Error::Cancelled(job_id.to_string())),
                JobStatus::Queued | JobStatus::Generating => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }
}

/// Pull the video b64 out of a completed VidGen job body. Shape:
/// `{result: {output_format, mime_type, fps, frame_count, b64_json}}`.
fn parse_result(body: &Value) -> Result<LtxRenderResult, Error> {
    let result = body.get("result").ok_or(Error::MissingField("result"))?;
    let output_format = result
        .get("output_format")
        .and_then(|v| v.as_str())
        .unwrap_or("webm")
        .to_string();
    let mime_type = result
        .get("mime_type")
        .and_then(|v| v.as_str())
        .unwrap_or("video/webm")
        .to_string();
    let fps = result.get("fps").and_then(|v| v.as_i64()).unwrap_or(0);
    let frame_count = result
        .get("frame_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    // Absent `b64_json` is NOT an error. A stitched chain is 20-53 MB, so inlining it costs another
    // third again in base64 on a hop that already has a raw-bytes route for the same buffer; an
    // engine configured not to inline it says so by omitting the field, and `media_to_file` fetches.
    let video_b64 = result
        .get("b64_json")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(LtxRenderResult {
        video_b64,
        mime_type,
        output_format,
        fps,
        frame_count,
        job_is_partial: result
            .get("job_is_partial")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        rendered_segments: u32_array(result.get("rendered_segments")),
        job_media_is: result
            .get("job_media_is")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// The latent-bank facts in one poll body. See [`LtxBankReport`].
///
/// 🔴 `segments[]` (the `ltx25` unit key) is NOT positionally aligned with `plan.segments[]` on a
/// partial render: the plan echoes every shot and the units are only the rendered ones. Each unit's
/// own `index` is the caller's index, so it is read off the unit rather than from its position.
pub fn bank_report(body: &Value) -> LtxBankReport {
    // The window, from whichever half of the body has landed: the plan echoes it on every poll, and
    // the result names the segments it rendered. Neither is read from `job_is_partial`, which a full
    // render whose stitch died also sets.
    let window = body
        .pointer("/plan/render_window/mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode != "full")
        || !u32_array(body.pointer("/result/rendered_segments")).is_empty();
    LtxBankReport {
        bank_keys: body
            .get("segments")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| {
                        let index = s.get("index").and_then(Value::as_u64)? as u32;
                        let key = s.get("bank_key").and_then(Value::as_str)?;
                        (!key.is_empty()).then(|| (index, key.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        window,
        partial: window
            || body
                .pointer("/result/job_is_partial")
                .and_then(Value::as_bool)
                .unwrap_or(false),
    }
}

fn u32_array(v: Option<&Value>) -> Vec<u32> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_u64)
                .map(|n| n as u32)
                .collect()
        })
        .unwrap_or_default()
}

/// Clears the in-flight job slot on drop so a finished render's id can't be targeted by a
/// late `/cancel`.
struct InFlightClear {
    slot: Arc<RwLock<Option<String>>>,
}

impl Drop for InFlightClear {
    fn drop(&mut self) {
        if let Ok(mut g) = self.slot.write() {
            *g = None;
        }
    }
}

#[cfg(test)]
mod tests {
    /// A completed job without `b64_json` must parse. That field carries the WHOLE render inline,
    /// and an engine configured to stop inlining it signals that by omitting it — treating the
    /// omission as a malformed response would fail every render the moment that config is turned on.
    /// `media_to_file` then streams the same buffer from the raw-bytes route instead.
    #[test]
    fn a_completed_job_parses_without_inline_media() {
        let body = serde_json::json!({
            "status": "completed",
            "result": { "output_format": "webm", "mime_type": "video/webm", "fps": 24, "frame_count": 97 },
        });
        let parsed = super::parse_result(&body).expect("completed job must parse");
        assert!(parsed.video_b64.is_none());
        assert_eq!(parsed.frame_count, 97);

        // An empty string is the same statement as absence, and must not become an empty clip.
        let mut with_empty = body.clone();
        with_empty["result"]["b64_json"] = serde_json::json!("");
        assert!(
            super::parse_result(&with_empty)
                .expect("parses")
                .video_b64
                .is_none()
        );

        // The inline form still parses — it is what every deployed engine sends today.
        let mut inline = body;
        inline["result"]["b64_json"] = serde_json::json!("aGk=");
        assert_eq!(
            super::parse_result(&inline)
                .expect("parses")
                .video_b64
                .as_deref(),
            Some("aGk=")
        );
    }

    /// The three shapes `job_is_partial` appears in, and the one it does not.
    ///
    /// 🔴 The failure this pins is the whole point of the flag: a caller that copies a resume's
    /// `/media` over the project's stitched cut replaces N shots with the two it re-rendered.
    #[test]
    fn a_partial_render_says_so_and_a_full_one_does_not() {
        let full = serde_json::json!({
            "status": "completed",
            "result": { "fps": 24, "frame_count": 97 },
        });
        let parsed = super::parse_result(&full).expect("parses");
        assert!(!parsed.job_is_partial);
        assert!(parsed.rendered_segments.is_empty());

        // A resume: the window's own segments, in the caller's index space.
        let mut resume = full.clone();
        resume["result"]["job_is_partial"] = serde_json::json!(true);
        resume["result"]["rendered_segments"] = serde_json::json!([3, 4]);
        resume["result"]["job_media_is"] = serde_json::json!("segments [3, 4] ONLY");
        let parsed = super::parse_result(&resume).expect("parses");
        assert!(parsed.job_is_partial);
        assert_eq!(parsed.rendered_segments, vec![3, 4]);

        // A FULL render whose concatenation failed carries NO `rendered_segments`, so the flag alone
        // cannot be read as "a window": the prose is the only account of what the bytes are.
        let mut broken = full;
        broken["result"]["job_is_partial"] = serde_json::json!(true);
        broken["result"]["job_media_is"] =
            serde_json::json!("🔴 segment 0 ONLY — concatenating 5 segments FAILED");
        let parsed = super::parse_result(&broken).expect("parses");
        assert!(parsed.job_is_partial);
        assert!(parsed.rendered_segments.is_empty());
        assert!(parsed.job_media_is.is_some());
    }

    /// A stitch that failed has to be REPORTED, and the shape of the run it failed on is not what
    /// decides that — a resume can lose its concatenation too, and then the segment list is
    /// non-empty and the prose is the only account of the loss.
    #[test]
    fn a_failed_concatenation_is_reported_whatever_the_run_rendered() {
        let note = |body: serde_json::Value| {
            super::concat_failure_note(&super::parse_result(&body).expect("parses"))
        };
        let base = serde_json::json!({ "status": "completed", "result": { "fps": 24 } });

        // An ordinary full render has nothing to say.
        assert_eq!(note(base.clone()), None);

        // An ordinary resume: expected on every continue, so it must not raise a banner.
        let mut resume = base.clone();
        resume["result"]["job_is_partial"] = serde_json::json!(true);
        resume["result"]["rendered_segments"] = serde_json::json!([3, 4]);
        resume["result"]["job_media_is"] = serde_json::json!("segments [3, 4] ONLY");
        assert_eq!(note(resume.clone()), None);

        // The same resume, whose stitch then died. The window is still listed; the loss is only in
        // the prose, and dropping it left this render with no account at all.
        let mut broken_window = resume;
        broken_window["result"]["job_media_is"] =
            serde_json::json!("segments [3, 4] ONLY — AND THE CONCATENATION FAILED");
        assert!(
            note(broken_window)
                .expect("reported")
                .contains("CONCATENATION FAILED")
        );

        // A full render whose stitch died lists no segments, and gets koblem's own wording when the
        // engine sent none.
        let mut broken_full = base;
        broken_full["result"]["job_is_partial"] = serde_json::json!(true);
        assert!(
            note(broken_full)
                .expect("reported")
                .contains("Rebuild the cut from the takes")
        );
    }

    /// Bank keys come off each unit's OWN `index`, and the window's mode is what says "partial"
    /// before any `result` exists.
    #[test]
    fn bank_keys_are_read_by_unit_index_not_by_position() {
        // A retake of shot 4: one unit, and it is NOT `segments[0]` of the timeline.
        let body = serde_json::json!({
            "status": "generating",
            "plan": { "render_window": { "mode": "retake", "rendered": [4] } },
            "segments": [
                { "index": 4, "status": "completed", "bank_key": "ltx25_7a1:4" },
            ],
        });
        let report = super::bank_report(&body);
        assert!(report.partial);
        assert!(report.window);
        assert_eq!(report.bank_keys, vec![(4, "ltx25_7a1:4".to_string())]);

        // In flight: a segment that has not banked yet is ABSENT, and an empty key is the same
        // statement as absence — neither may become a key a later resume would name.
        let body = serde_json::json!({
            "status": "generating",
            "plan": { "render_window": { "mode": "full", "rendered": [0, 1] } },
            "segments": [
                { "index": 0, "bank_key": "ltx25_7a1:0" },
                { "index": 1 },
                { "index": 2, "bank_key": "" },
            ],
        });
        let report = super::bank_report(&body);
        assert!(!report.partial);
        assert!(!report.window);
        assert_eq!(report.bank_keys, vec![(0, "ltx25_7a1:0".to_string())]);

        // A FULL render whose concatenation failed. The plan's window still says "full", so the
        // completed body is the only place this is knowable — and the reattach path, which recovers
        // such a render from ONE status call and never calls `wait`, would otherwise file segment 0
        // as the project's cut.
        let body = serde_json::json!({
            "status": "completed",
            "plan": { "render_window": { "mode": "full", "rendered": [0, 1] } },
            "result": { "job_is_partial": true, "job_media_is": "🔴 segment 0 ONLY" },
            "segments": [{ "index": 0, "bank_key": "ltx25_7a1:0" }],
        });
        let report = super::bank_report(&body);
        assert!(report.partial);
        // 🔴 …and it is NOT a window. Every shot rendered; only the stitch died. Reading `partial`
        // for both questions made the harvest refuse this run its tier swap, so the previous
        // render's trailing rows survived and a shortened timeline rebuilt an over-long cut.
        assert!(!report.window);

        // A resume whose stitch ALSO died: both true, and the window is what decides the swap.
        let mut broken_window = body.clone();
        broken_window["result"]["rendered_segments"] = serde_json::json!([3, 4]);
        let report = super::bank_report(&broken_window);
        assert!(report.partial);
        assert!(report.window);

        // A body that publishes neither must not be mistaken for a partial render.
        assert_eq!(
            super::bank_report(&serde_json::json!({ "status": "completed" })),
            super::LtxBankReport::default()
        );
    }
}
