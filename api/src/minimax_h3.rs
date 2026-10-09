//! HTTP client for the MiniMax-H3 video server — the GPU "minimax_h3" joint audio+video engine.
//!
//! The backend is `kobbler-h3-wangp`: a WanGP-backed shim reached internally at
//! `http://h3-wangp:8080`, speaking the contract in `kobbler/docker/h3-wangp/API.md` (Python half:
//! `app/contract.py`). It replaced a hand-written C++ port that was never used outside dev work,
//! which is why this module owes that shape nothing.
//!
//! # The one thing to understand before reading any of this
//!
//! **LTX's unit is a segment = one render. H3's unit is a GENERATION = up to ~15 s containing 1-N
//! shots**, where a shot boundary is a cut written as `[Shot N] At MM:SS.mmm` inside ONE prompt and
//! rendered by the model itself. `<scenetrans>` audio continuity across a cut exists ONLY within a
//! generation, and cross-shot identity is carried by prose ("the captain from Shot 1").
//!
//! ⇒ The **generation** is the delivery unit and the retake unit. Per-shot clips do not exist here.
//! What the poll body still calls `partials[]` is one entry per finished GENERATION — that field
//! name is kept deliberately (API.md §3) so progressive delivery reaches the Director through
//! `harvest_director_renders` without a fork.
//!
//! What is NOT the same as LTX, and is the whole reason there is a second engine:
//!
//!   * H3 renders video AND its synchronised stereo soundtrack in ONE denoise. There is no audio
//!     INPUT sheet at all.
//!   * H3 is guidance-distilled: no `negative_prompt`, no CFG != 1, no model variants, no hires
//!     chain. These are not fields the engine 400s on — they do not exist, and anything
//!     unrecognised comes back in `ignored[]`.
//!   * LoRAs, by contrast, ARE supported — per GENERATION, as `loras` + `lora_multipliers`
//!     (`contract.py:346`, `/h3/v1/capabilities` reports `"loras": true`). The one that matters is
//!     the turbo LoRA: `h3-turbo-4step-ema-pruned.safetensors` at 8 steps is 465.4 s against the
//!     20-step control's 762.8 s — **−39% wall for ZERO extra VRAM** (6528 MiB both). Per-step cost
//!     rises ~5%, so the win is entirely the step COUNT, and the field does not touch `steps`: the
//!     caller sets those itself.
//!   * `frames` snaps UP to `17n+5` with a **22** floor and a 362 cap; fps is FIXED at 24. The
//!     grid and the cap are model law; the floor is the video VAE's own decodable minimum (a
//!     2-latent block decodes 0 chunks and raises, so 5 frames is still refused). 107 — the
//!     grid-snap of MiniMax's documented 4 s guideline — was enforced as a hard floor until
//!     2026-08-07 and silently lengthened every shorter beat; it is now advisory and REPORTED.
//!   * Continuity across a generation boundary is a pin or an anchor frame and is **never**
//!     frame-exact. There is no `resume_job_id` and no retake splice.
//!
//! 🔴 `ignored[]` is load-bearing rather than decorative: `api_cli.py:121` filters the task dict
//! against `inspect.signature(wgp.generate_media).parameters` and drops unknown keys **silently**,
//! so a mistyped setting renders the default and nobody finds out. koblem builds the request from
//! an allowlist (`routes::minimax_h3`) so a bad key can rarely reach the engine, and surfaces
//! `ignored` so one that did is visible instead of discovered a week later.
//!
//! Acquires the shared [`GpuLock`] as the `minimax_h3` consumer — its OWN budget line, not a second
//! LTX slot: a ~20 GB DiT plus a ~14.5 GB text encoder cannot co-reside with the LTX-2.3 DiT, so
//! the two serialise. Self-registers on the gate's drain/unload registry when `base_url` is set.
//!
//! 🔴 `/v1/admin/drain` **blocks** until idle (`SD_DRAIN_WAIT_SECONDS`, 1800). The gate treats a
//! successful drain as permission to SIGKILL, and an early return already cost a live render 92 s
//! in — so nothing here may add a request timeout to that path.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use kob_gpu_gate::{GpuLock, GpuMinimaxH3Guard, HeavyKind, HeavyService};
use serde_json::Value;

pub use crate::ltx_video::{LtxPartialChunk as H3PartialChunk, LtxRenderResult as H3RenderResult};
use crate::wire::{WireForm, WireMeter};

/// Connect timeout guards a dead host. No request timeout: a generation runs for minutes, a chain
/// of them for the better part of an hour, and we poll with our own loop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the poll loop checks job status.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Transient poll failures (a dropped connection, a brief 5xx) shouldn't abandon a long render.
const MAX_POLL_RETRIES: u32 = 20;
/// Backoff between retried polls after a transient error.
const POLL_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// The container the shim delivers. x265 in MP4 at CRF 8 (`H3_OUTPUT_CODEC=libx265_8`) — WanGP's
/// own default is `libx264_8`, i.e. CRF 23, which is too lossy for a master koblem may re-encode
/// downstream. Not webm: webm cannot carry HEVC at all, so every `.webm` assumption on the H3 path
/// is a real bug and not a cosmetic one.
pub const H3_OUTPUT_FORMAT: &str = "mp4";
pub const H3_MIME_TYPE: &str = "video/mp4";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("minimax-h3 HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("minimax-h3 returned {0}: {1}")]
    Upstream(u16, String),
    #[error("minimax_h3 service not configured")]
    NotConfigured,
    #[error("minimax-h3 job {0} failed: {1}")]
    JobFailed(String, String),
    #[error("minimax-h3 job {0} was cancelled")]
    Cancelled(String),
    #[error("minimax-h3 response missing required field: {0}")]
    MissingField(&'static str),
    #[error("minimax-h3 job {0} returned undecodable media: {1}")]
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

/// The `202` a submit came back with, minus the ids.
///
/// Kept because the acceptance carries the render GEOMETRY THE CALLER DID NOT CHOOSE: `frames` was
/// snapped UP onto the model's `5+17k` grid, each generation's checkpoint was DERIVED from the
/// conditioning actually supplied, a `fl2va` -> `ref2va` transition inserted a full DiT +
/// text-encoder reload into the ETA, and `ignored` names every setting the engine discarded.
/// Dropping any of it would leave "my setting did nothing" undiscoverable, which is the exact
/// failure the plan echo exists to prevent.
#[derive(Debug, Clone, Default)]
pub struct H3SubmitPlan {
    /// Engine job id.
    pub id: String,
    /// Per-generation geometry: `{index, task, frames, seconds, width, height, steps, seed,
    /// checkpoint, reload_required, shots, references, continuation, eta_seconds, prompt_chars}`.
    /// Held as raw JSON on purpose — this is a REPORT, and a strongly-typed mirror of it would
    /// have to be revised (and koblibs re-published) every time the shim adds a diagnostic field.
    pub generation_plans: Vec<Value>,
    /// Sum of the per-generation ETAs, INCLUDING any checkpoint reload the chain forces. The one
    /// number worth showing a user before an hour-long job starts.
    pub total_eta_seconds: Option<f64>,
    /// `{width, height, fps}` as the job will actually render. Echoed rather than assumed: a
    /// per-generation override or an engine-side clamp is otherwise invisible.
    pub canvas: Option<Value>,
    /// Non-fatal notes from the shim: a frame snap, a multi-generation seam warning, an inert
    /// continuation on a non-Ref2VA task. Distinct from `ignored` — a warning is about something
    /// that WILL happen, `ignored` is about something that will not.
    pub warnings: Vec<String>,
    /// Fields this request carried that the engine accepted and discarded. See the module note on
    /// `api_cli.py:121`; this is the only channel through which a silently-dropped setting becomes
    /// visible, so it is parsed verbatim and never filtered.
    pub ignored: Vec<String>,

    // ── MEASURED, not planned. Filled in by the queue adapter when the render ends. ────────
    //
    // 🔴 Everything above is what the engine said BEFORE rendering. These two are what it cost, and
    // they are on the same struct for one reason: the plan echo is the only H3 artifact that
    // survives the job store's TTL (it is stamped into the project row), so it is the only place a
    // finished render's real timing can still be read an hour later. Without them the editor's only
    // number is its own session wall clock — which is how a 24.9-minute render was reported as
    // "~1 hr": the wall included a failed submit, a 54.2 s gate wait and a retry.
    /// How long this render sat waiting for the GPU before it started. NOT render time, and the
    /// whole point of keeping it apart: a queue is somebody else's render, and folding it into the
    /// duration makes the engine look slow for being busy.
    pub gate_wait_ms: Option<u64>,
    /// Guard-acquired to the last poll, in milliseconds — the same figure the job store records.
    /// (It stops before the media fetch, deliberately: downloading the container is koblem's wire
    /// cost, not the engine's render.) `None` until the render ends.
    pub render_ms: Option<u64>,
}

impl H3SubmitPlan {
    /// The plan as the JSON the Director stamps into a project / surfaces on `/active`. Omits the
    /// engine job id — the koblem job already records that as `server_job_id`.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "generation_plans": self.generation_plans,
            "total_eta_seconds": self.total_eta_seconds,
            "canvas": self.canvas,
            "warnings": self.warnings,
            "ignored": self.ignored,
            "generations": self.generation_plans.len(),
            "frames": self.total_frames(),
            "duration_seconds": self.total_seconds(),
            "width": self.width(),
            "height": self.height(),
            // `null` while a render is in flight and on a plan captured at the 202 — absence means
            // "not measured", which is not the same as zero and must not be drawn as a fast render.
            "gate_wait_ms": self.gate_wait_ms,
            "render_ms": self.render_ms,
        })
    }

    /// Fold what the render actually COST into the plan, so the stamp carries both halves.
    ///
    /// 🔴 `per_generation_seconds` comes off the poll body's `generations[n].render_seconds`, which
    /// the shim writes when each generation finishes. It is merged INTO the existing plan entries
    /// rather than published as a parallel array, because the two are already aligned by `index`
    /// and a second list keyed by position is a mis-alignment waiting to happen — the plan is the
    /// only thing that knows which generation was `ref2va` and which one paid the reload.
    ///
    /// ⚠️ A generation the engine never reported is left alone, not zeroed. A render cancelled
    /// half-way has real numbers for the generations that finished and nothing for the rest, and
    /// "0 s" against the one that was interrupted would be the only wrong number on the page.
    pub fn record_timing(
        &mut self,
        gate_wait_ms: u64,
        render_ms: u64,
        per_generation_seconds: &[(u32, f64)],
    ) {
        self.gate_wait_ms = Some(gate_wait_ms);
        self.render_ms = Some(render_ms);
        for (index, seconds) in per_generation_seconds {
            let want = u64::from(*index);
            let entry = self
                .generation_plans
                .iter_mut()
                .find(|g| g.get("index").and_then(Value::as_u64) == Some(want));
            if let Some(o) = entry.and_then(Value::as_object_mut) {
                o.insert("render_seconds".into(), serde_json::json!(seconds));
            }
        }
    }

    /// Total frames across the job, summed from the per-generation plans rather than read off a
    /// top-level field: there is no longer a top-level one, because with per-generation canvas
    /// overrides a single number was already a half-truth.
    pub fn total_frames(&self) -> Option<i64> {
        self.sum_i64("frames")
    }

    pub fn total_seconds(&self) -> Option<f64> {
        let mut any = false;
        let mut total = 0.0;
        for g in &self.generation_plans {
            if let Some(v) = g.get("seconds").and_then(Value::as_f64) {
                any = true;
                total += v;
            }
        }
        any.then_some(total)
    }

    fn sum_i64(&self, key: &str) -> Option<i64> {
        let mut any = false;
        let mut total = 0i64;
        for g in &self.generation_plans {
            if let Some(v) = g.get(key).and_then(Value::as_i64) {
                any = true;
                total += v;
            }
        }
        any.then_some(total)
    }

    pub fn width(&self) -> Option<i64> {
        self.canvas.as_ref()?.get("width")?.as_i64()
    }

    pub fn height(&self) -> Option<i64> {
        self.canvas.as_ref()?.get("height")?.as_i64()
    }

    /// Does this chain switch checkpoints part-way? A `fl2va` <-> `ref2va` transition is a full
    /// `release_model()` + reload (`wgp.py:6614-6617` is never incremental), so it is worth naming
    /// in a log line rather than leaving as an unexplained pause between generations.
    pub fn reloads(&self) -> usize {
        self.generation_plans
            .iter()
            .filter(|g| {
                g.get("reload_required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .count()
    }
}

/// In-flight progress, parsed off every poll.
///
/// H3 renders take 830 s at prod shape and the LTX stack has no numeric progress at all, so this is
/// a capability the Director gains by moving engines. Plumbed through the job store NOW even though
/// the UI that draws it lands later: the alternative is discovering at UI time that the poll loop
/// threw the numbers away, which is a change to the render path rather than to a component.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct H3Progress {
    /// 0.0-1.0 over the WHOLE job, not the current generation.
    pub progress: Option<f64>,
    pub current_step: Option<u32>,
    pub total_steps: Option<u32>,
    /// `denoising` | `decoding` | `loading` | `encoding`. `loading` is the checkpoint reload a
    /// mixed-task chain pays between generations, and is the difference between "stuck" and
    /// "swapping a 20 GB DiT".
    pub phase: Option<String>,
    /// Bumps when a new preview PNG is available at `/h3/v1/jobs/{id}/preview`. A sequence rather
    /// than a URL because the URL never changes — a client polls the number to know to re-fetch.
    pub preview_seq: Option<u64>,
    /// Which generation is rendering, and how many there are.
    pub generation_index: Option<u32>,
    pub generation_count: Option<u32>,
    pub generations_done: Option<u32>,
    /// `(generation index, bank key)` for each FINISHED generation. The bank key is what a later
    /// generation's `continuation.from` points at when it must not say `"prev"` — i.e. a retake,
    /// which has no previous generation inside its own job.
    pub bank_keys: Vec<(u32, String)>,
    /// `(generation index, render_seconds)` for each generation the shim has finished.
    ///
    /// 🔴 This is the ONLY honest per-generation timing there is. koblem's own numbers are the
    /// whole job (`render_ms`) and the wait for the card (`gate_wait_ms`); the editor's is a
    /// session wall clock that counts a failed submit and a retry as render time, which is how a
    /// 24.9-minute render got reported as "~1 hr". Read off the same `generations[]` array as
    /// `bank_keys` and forwarded on change like everything else here — a generation finishing is
    /// exactly a change, so the last one lands before the poll loop returns the completed body.
    pub render_seconds: Vec<(u32, f64)>,
}

/// One reference's bytes, uploaded as multipart part `name` (`ref_0`, `ref_1`, …).
///
/// 🔴 The name indexes the part, NOT the reference's position inside any one generation: parts are
/// numbered across the whole request because multipart field names are flat, and each reference's
/// JSON carries its own `"part": "ref_<n>"` back-pointer. The old contract's `ref_audio_<i>` was
/// audio-only and indexed per-request; the shim still accepts that spelling as a legacy alias, and
/// koblem never sends it.
#[derive(Debug, Clone)]
pub struct H3RefPart {
    /// Multipart field name — must equal the `part` written into the reference's JSON.
    pub name: String,
    pub file_name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone)]
pub struct MinimaxH3Client {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    /// Job id of the render currently in flight, so the `/cancel` route can reach it.
    in_flight: Arc<RwLock<Option<String>>>,
    /// What the last submit and the last media fetch actually cost on the wire. See `wire.rs`.
    wire: WireMeter,
}

impl MinimaxH3Client {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("failed to build HTTP client");
        let me = Self {
            http,
            base_url: Arc::new(RwLock::new(
                base_url
                    .as_ref()
                    .map(|s| s.trim_end_matches('/').to_string()),
            )),
            gate,
            in_flight: Arc::new(RwLock::new(None)),
            wire: WireMeter::new(),
        };
        if let Some(url) = base_url.as_deref().filter(|s| !s.is_empty()) {
            me.register(url.trim_end_matches('/').to_string());
        }
        me
    }

    /// Register on the gate as a heavy peer. Unchanged across the engine swap on purpose: the shim
    /// answers the same two admin paths, so the gate's eviction machinery needed no edit at all.
    fn register(&self, url: String) {
        self.gate.set_minimax_h3_service(HeavyService {
            kind: HeavyKind::MinimaxH3,
            url,
            unload_path: "/v1/admin/unload".to_string(),
            drain_path: "/v1/admin/drain".to_string(),
        });
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalised = base_url
            .as_ref()
            .map(|s| s.trim_end_matches('/').to_string());
        *self.base_url.write().expect("minimax_h3 base_url poisoned") = normalised.clone();
        if let Some(url) = normalised {
            self.register(url);
        }
    }

    /// The comfy-api base URL, if configured — Beefy drains and re-arms its admission directly.
    pub fn base_url(&self) -> Option<String> {
        self.base_url
            .read()
            .expect("minimax_h3 base_url poisoned")
            .clone()
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("minimax_h3 base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("minimax_h3 base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the GPU as the minimax_h3 consumer. `None` when the service has been administratively
    /// disabled.
    pub async fn acquire(&self) -> Option<(GpuMinimaxH3Guard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_minimax_h3().await?;
        Some((guard, start.elapsed()))
    }

    /// Liveness: true when the server's `/health` returns 2xx.
    pub async fn healthy(&self) -> bool {
        let Ok(url) = self.url("/health") else {
            return false;
        };
        matches!(self.http.get(url).send().await, Ok(resp) if resp.status().is_success())
    }

    /// `GET /health` body verbatim.
    ///
    /// Worth surfacing whole rather than reducing to a boolean, because it is the PATCH HONESTY
    /// HARNESS: four runtime patches (`solattn`, `h3_sampler`, `h3_ref_canvas`, `h3_continuity`)
    /// each report `applied`, dispatch counters, and a tri-state `healthy`.
    ///
    /// 🔴 `applied: true` proves nothing — four patches in one session logged success while doing
    /// nothing (one mutated a dict nothing read, one was overwritten by a later assignment, one
    /// dispatched 0 of 1095 calls, one had its effect silently rebuilt by the pipeline). `healthy`
    /// is `true` only when counters prove it ran; `null` + a `reason` means no render has exercised
    /// it yet, which is NOT a pass.
    pub async fn health(&self) -> Option<Value> {
        let url = self.url("/health").ok()?;
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    /// `GET /h3/v1/capabilities[?width=&height=&frames=]` — the request contract as JSON, plus
    /// (with the query) the exact geometry a request with those values would render at.
    ///
    /// Read while a UI is BUILDING a request, i.e. before it wants a GPU at all, so the editor can
    /// show the snapped frame count / duration without submitting anything and without evicting the
    /// LLM to ask.
    pub async fn capabilities(&self, query: Option<&str>) -> Result<Value, Error> {
        let path = match query.map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) => format!("/h3/v1/capabilities?{q}"),
            None => "/h3/v1/capabilities".to_string(),
        };
        let resp = self.http.get(self.url(&path)?).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// `GET /h3/v1/loras` — the adapters actually on disk in the engine's `minimax_h3/` LoRA
    /// directory, so a name stops being an act of faith.
    ///
    /// 🔴 A LoRA is a BARE FILENAME on this wire and nothing else — there is no id, no hash and no
    /// path. Before this route existed the editor could only offer a text box, and its two failure
    /// modes were indistinguishable from one another: the file is missing so nothing attaches, or
    /// the file loaded and the adapter attached to nothing. Both render, both log `(N/N) applied`,
    /// and the second renders 20-step quality in 8 steps' time — which reads as BROKEN rather than
    /// as a missing speedup.
    ///
    /// Answered from a live directory scan with no GPU involved, and the same scan backs the shim's
    /// request-time refusal, so what the editor offers and what the engine accepts cannot drift.
    ///
    /// ⚠️ `addressable: false` marks a file in a SUBDIRECTORY. The engine's own enumerator globs
    /// `**`, so it is genuinely there, but a wire name containing a separator is refused (it could
    /// escape the root) — so it is listed and not requestable, which beats omitting it and leaving
    /// somebody hunting for a file they can see on disk.
    pub async fn loras(&self) -> Result<Value, Error> {
        let resp = self.http.get(self.url("/h3/v1/loras")?).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// `POST /h3/v1/generate` — submit a generation list. Multipart: a `request` JSON text part plus
    /// one `ref_<n>` binary part per reference that ships bytes. Records the job as in-flight so
    /// `/cancel` can reach it, and returns the whole [`H3SubmitPlan`], not just the id.
    pub async fn submit(
        &self,
        request: &Value,
        ref_parts: &[H3RefPart],
    ) -> Result<H3SubmitPlan, Error> {
        let plan = self.submit_raw(request, ref_parts).await?;
        *self
            .in_flight
            .write()
            .expect("minimax_h3 in_flight poisoned") = Some(plan.id.clone());
        Ok(plan)
    }

    async fn submit_raw(
        &self,
        request: &Value,
        ref_parts: &[H3RefPart],
    ) -> Result<H3SubmitPlan, Error> {
        let url = self.url("/h3/v1/generate")?;
        let mut form = WireForm::new().json("request", request);
        for p in ref_parts {
            form = form
                .bytes(&p.name, p.file_name.clone(), &p.mime, p.bytes.clone())
                .map_err(Error::Request)?;
        }
        self.wire.submitted("minimax-h3", "generate", form.size());
        let resp = self
            .http
            .post(url)
            .multipart(form.into_form())
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            // Every rejection from the H3 handler carries a JSON body. A rejection from the layer
            // ABOVE it — the ASGI server, or a proxy — carries none at all, and a bare
            // "returned 400: " sends the reader to the wrong logs. Say which.
            let body = match resp.text().await {
                Ok(b) if !b.trim().is_empty() => b,
                Ok(_) => {
                    "<no body: rejected before the H3 handler ran — the shim's HTTP layer or a \
                          proxy; check the h3-wangp log and /health, then retry>"
                        .to_string()
                }
                Err(e) => format!("<body unreadable: {e}>"),
            };
            return Err(Error::Upstream(status, body));
        }
        let body: Value = resp.json().await?;
        parse_submit(&body)
    }

    /// `GET /h3/v1/jobs/{id}/generations/{n}?joinable=1` — one finished GENERATION's mp4, carrying
    /// its own generated soundtrack, HEAD-TRIMMED so the caller can concatenate it.
    ///
    /// 🔴 `joinable=1` is not a nicety here, it is the only correct request THIS caller can make.
    /// The shim serves the UNTRIMMED render by default and that is deliberate — the latent bank,
    /// the pixel tail and the latent carry must all see every frame or the next generation's carry
    /// is a frame short. But with the overlay anchor on (the locked continuation method) a
    /// continuation's frame 0 is a REPRODUCTION of the previous generation's last frame, and every
    /// generation fetched here is written as a Director take that
    /// `routes::ltx_director::rebuild_cut` later stream-copy concatenates (`ffmpeg -f concat -c
    /// copy`). That path never decodes, so it cannot trim: these bytes have to arrive join-safe or
    /// every seam in a rebuilt cut carries a duplicated frame — a one-frame stutter per join, 15 of
    /// them in a 16-generation cut. (`rebuild_cut`'s own doc comment still describes the LTX case,
    /// where the engine trimmed the overlap before delivery; on H3 the trim is THIS query.)
    ///
    /// Safe to send unconditionally: on generation 0, on a hard cut, or wherever `trimmed_head` is
    /// 0 the shim returns the same bytes as the default (`main.py:510`).
    ///
    /// ⚠️ The retry exists because the shim fails CLOSED: a trim that goes wrong is a 500, never a
    /// silent untrimmed file (`main.py:515`). Fail-closed is right for the shim and wrong for us —
    /// losing the take entirely is worse than a stutter — so a non-2xx is retried ONCE without the
    /// query. Never silently: the artefact is one frame, an hour of render away from the trim that
    /// failed, and nobody would ever trace it back from the output alone.
    ///
    /// The shim also serves `/sdcpp/v1/jobs/{id}/segments/{n}` as a legacy alias so a pre-rewrite
    /// koblem can poll a running shim; nothing here uses it.
    pub async fn fetch_generation(&self, server_job_id: &str, n: u32) -> Result<Vec<u8>, Error> {
        let path = format!("/h3/v1/jobs/{server_job_id}/generations/{n}");
        let resp = self
            .http
            .get(self.url(&format!("{path}?joinable=1"))?)
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(resp.bytes().await?.to_vec());
        }
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();

        let untrimmed = self.http.get(self.url(&path)?).send().await?;
        if !untrimmed.status().is_success() {
            // Both failed, so this is not about the trim at all — a 409 (still generating) or a 410
            // (staging TTL expired) answers both paths identically. Report the joinable request's
            // status, which is the one describing what we actually asked for.
            return Err(Error::Upstream(status, body));
        }
        tracing::warn!(
            job = %server_job_id, generation = n, status, body = %body,
            "🔴 minimax-h3 refused the JOINABLE generation and the untrimmed file was delivered \
             instead: a rebuilt cut will carry a DUPLICATED FRAME at this seam (a one-frame \
             stutter). The take is kept because losing it is worse, but the cut is not clean"
        );
        Ok(untrimmed.bytes().await?.to_vec())
    }

    /// Does this look like a container the engine had not finished writing?
    ///
    /// 🔴 Compressed HEVC is dense: every healthy generation measured here runs **99.56-99.60%**
    /// non-zero. The two hollow ones — both the LAST generation of their job — came back at
    /// **1.57%** and **4.67%**, a correctly-indexed mp4 over a zero-filled payload whose residue was
    /// just its intact AAC track. There is no middle ground to worry about and darkness is not a
    /// factor: entropy here is a property of the codec, not of the picture, so a legitimately black
    /// shot is still dense.
    ///
    /// Sampled rather than swept — this runs on every partial, and the question is "hole or not",
    /// which a few windows across the file answer as well as reading 40 MB would.
    fn looks_hollow(bytes: &[u8]) -> bool {
        const WINDOWS: usize = 32;
        const WINDOW: usize = 4096;
        // 🔴 A container far too small to be a generation is unwritten too, and the byte histogram
        // cannot see it — a 44-byte file is 70% non-zero, which is "dense". That is exactly what
        // arrived once: `ftyp` + `free` + an empty `mdat`, ffmpeg's header and nothing after it,
        // and because `looks_hollow` bailed on anything under one window it was never re-read. The
        // shortest legal generation is 22 frames of x265 at CRF 8; 64 KiB is orders of magnitude
        // below that and orders of magnitude above any header.
        const MIN_PLAUSIBLE: usize = 64 * 1024;
        if bytes.len() < MIN_PLAUSIBLE {
            return true;
        }
        let (mut sampled, mut nonzero) = (0usize, 0usize);
        for i in 0..WINDOWS {
            let start = (bytes.len() - WINDOW) * i / WINDOWS;
            let end = start + WINDOW;
            sampled += WINDOW;
            nonzero += bytes[start..end].iter().filter(|b| **b != 0).count();
        }
        // Half. Far below any real file and far above any hole — deliberately nowhere near either,
        // because the cost of a false positive is one wasted re-fetch and the cost of a false
        // negative is an hour of render delivered as a black clip.
        sampled > 0 && (nonzero as f64 / sampled as f64) < 0.5
    }

    /// [`fetch_generation`](Self::fetch_generation), re-fetching while the bytes look unwritten.
    ///
    /// 🔴 **The last generation of a job is the one that breaks**, twice in a row and never any
    /// other. Its partial is fetched at the moment the job completes — the earlier ones have a whole
    /// generation of render time to settle — and it arrived as a valid index over a zero payload
    /// both times. The poll loop returns as soon as it sees `completed`, so without this there is no
    /// second chance: the generation is simply lost, and the author is told to retake an hour of
    /// GPU time.
    ///
    /// Re-reading is the entire fix if the file was mid-write, and it costs one HTTP round trip
    /// against a wrong answer that costs a shot.
    async fn fetch_generation_retrying(
        &self,
        server_job_id: &str,
        n: u32,
    ) -> Result<Vec<u8>, Error> {
        const ATTEMPTS: usize = 4;
        const BACKOFF_MS: u64 = 1_500;
        let mut last = self.fetch_generation(server_job_id, n).await?;
        for attempt in 1..ATTEMPTS {
            if !Self::looks_hollow(&last) {
                return Ok(last);
            }
            tracing::warn!(
                job = %server_job_id, generation = n, attempt, bytes = last.len(),
                "🔴 minimax-h3 generation arrived with a zero-filled payload — the engine was \
                 probably still writing it; re-fetching"
            );
            tokio::time::sleep(std::time::Duration::from_millis(
                BACKOFF_MS * attempt as u64,
            ))
            .await;
            last = self.fetch_generation(server_job_id, n).await?;
        }
        // Handed back even when it still looks hollow: the drain probes it properly (with a
        // decoder, not a byte histogram) and refuses it there with a report. Two verdicts, and this
        // one is only ever allowed to ask for a re-read.
        Ok(last)
    }

    /// `GET /h3/v1/jobs/{id}/preview` — the latest in-flight preview PNG.
    ///
    /// Only produced when the request asked for it (`preview: true`): `generate_preview` decodes
    /// latents through the RGB-factor path per emission and the cost is unmeasured, so it is opt-in
    /// and must not silently tax a production render. Poll `H3Progress::preview_seq` to know when
    /// there is a new one rather than re-fetching a URL that never changes.
    pub async fn fetch_preview(&self, server_job_id: &str) -> Result<Vec<u8>, Error> {
        let url = self.url(&format!("/h3/v1/jobs/{server_job_id}/preview"))?;
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
        let url = self.url(&format!("/h3/v1/jobs/{server_job_id}/media"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let n = crate::wire::stream_to_file(resp, dest).await?;
        self.wire.fetched_raw("minimax-h3", n);
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
        result: &H3RenderResult,
        dest: &std::path::Path,
    ) -> Result<u64, Error> {
        let Some(b64) = result.video_b64.as_deref() else {
            return self.fetch_media_to_file(server_job_id, dest).await;
        };
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            .map_err(|e| Error::BadMedia(server_job_id.to_string(), e.to_string()))?;
        self.wire
            .fetched_inline_b64("minimax-h3", bytes.len() as u64);
        tokio::fs::write(dest, &bytes)
            .await
            .map_err(|e| crate::wire::StreamError::Write {
                path: dest.display().to_string(),
                source: e,
            })?;
        Ok(bytes.len() as u64)
    }

    /// What the last submit and the last media fetch cost, for the render log row (§9.2).
    pub fn wire(&self) -> &WireMeter {
        &self.wire
    }

    /// One status read, for a caller that is not waiting on the render.
    ///
    /// The restart-reattach path recovers a job that finished while koblem was down WITHOUT ever
    /// waiting, so this is its only chance to learn what the shim says the media is — see
    /// [`result_is_partial`].
    pub async fn status(&self, job_id: &str) -> Result<Value, Error> {
        self.poll(job_id).await
    }

    /// `GET /h3/v1/jobs/{id}` — poll once, returning the parsed body.
    async fn poll(&self, job_id: &str) -> Result<Value, Error> {
        let url = self.url(&format!("/h3/v1/jobs/{job_id}"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// `POST /h3/v1/jobs/{id}/cancel` — cancel the in-flight render. No-op (Ok) when idle.
    ///
    /// 🔴 The request lands in ~10 ms but HONOURING it is per-model, and H3's checkpoints are
    /// coarse: budget up to one step, which is 43-50 s at prod shape. And cancel frees no VRAM —
    /// only the gate's `/v1/admin/unload` does, which is why that is a separate call in a separate
    /// order (cancel, THEN `release_model()`).
    pub async fn cancel_in_flight(&self) -> Result<Value, Error> {
        let job_id = self
            .in_flight
            .read()
            .expect("minimax_h3 in_flight poisoned")
            .clone();
        let Some(job_id) = job_id else {
            return Ok(serde_json::json!({ "status": "idle" }));
        };
        self.cancel(&job_id).await
    }

    /// `POST /h3/v1/jobs/{id}/cancel` for a specific job id.
    pub async fn cancel(&self, job_id: &str) -> Result<Value, Error> {
        let url = self.url(&format!("/h3/v1/jobs/{job_id}/cancel"))?;
        let resp = self.http.post(url).send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(Error::Upstream(status.as_u16(), body.to_string()));
        }
        Ok(body)
    }

    // NB: no `unload()` here, deliberately. ltx_video needs one because a DiT VARIANT swap leaks
    // VRAM unless the engine reloads fresh; the shim owns its own checkpoint switching, so the
    // gate's eviction path (`/v1/admin/unload`, registered above) is the only caller that ever
    // needs to reach it.

    /// `DELETE /h3/v1/job?id=<engine-job-id>` — remove a job's staged directory (reference bytes,
    /// per-generation mp4s). Idempotent, and answerable **while the model is unloaded**, so it
    /// stays safe to call after an eviction.
    ///
    /// This does NOT drop the latent bank: the bank lives under `H3_BANK_DIR` with its own byte cap
    /// and survives both the job TTL and a process restart, precisely so a retake can still
    /// continue from a generation rendered an hour ago.
    pub async fn delete_job(&self, job_id: &str) -> Result<(), Error> {
        let url = self.url(&format!(
            "/h3/v1/job?id={}",
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

    /// Poll a submitted render to completion, forwarding each newly-finished GENERATION's bytes to
    /// `sink` and every progress change to `progress`.
    ///
    /// The poll body's array is still called `partials[]` and its entries are still keyed
    /// `segment_index` — one entry per finished generation. That naming is deliberate (API.md §3):
    /// it is what lets progressive delivery reach the Director through the shared job store and
    /// `harvest_director_renders` without either of them learning a second vocabulary.
    pub async fn wait_with_partials(
        &self,
        job_id: &str,
        sink: Option<tokio::sync::mpsc::UnboundedSender<H3PartialChunk>>,
        progress: Option<tokio::sync::mpsc::UnboundedSender<H3Progress>>,
    ) -> Result<(H3RenderResult, H3TrimReport), Error> {
        let _clear = InFlightClear {
            slot: self.in_flight.clone(),
        };
        let mut transient: u32 = 0;
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        // Only forward a CHANGE. At a 500 ms poll and 43-50 s per step, most polls say exactly what
        // the last one did, and re-writing the job sidecar for each would be a disk write every
        // half-second for the better part of an hour.
        let mut last_progress = H3Progress::default();
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
                        "minimax-h3 poll failed; retrying"
                    );
                    tokio::time::sleep(POLL_RETRY_BACKOFF).await;
                    continue;
                }
            };
            if let Some(tx) = &progress {
                let now = parse_progress(&body);
                if now != last_progress {
                    let _ = tx.send(now.clone());
                    last_progress = now;
                }
            }
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
                    match self.fetch_generation_retrying(job_id, idx).await {
                        Ok(bytes) => {
                            seen.insert(idx);
                            let _ = tx.send(H3PartialChunk {
                                segment_index: idx,
                                bytes,
                            });
                        }
                        Err(e) => {
                            // Leave `idx` unseen so the next poll retries; a generation that never
                            // fetches just isn't previewed, and still arrives in the stitched job.
                            tracing::warn!(
                                job = %job_id, generation = idx, error = %e,
                                "minimax-h3 generation fetch failed; will retry"
                            );
                        }
                    }
                }
            }
            // 🔴 An UNKNOWN status maps to `generating`, not to an error: the shim may add fields
            // freely, and a client that failed closed on a status it had not been taught would
            // abandon a live render. The other half of that contract is the shim's — API.md §3
            // forbids a sixth status, because this mapping would read it as "still working"
            // forever.
            let status = match body.get("status").and_then(|v| v.as_str()) {
                Some("queued") => JobStatus::Queued,
                Some("generating") => JobStatus::Generating,
                Some("completed") => JobStatus::Completed,
                Some("failed") => JobStatus::Failed,
                Some("cancelled") => JobStatus::Cancelled,
                _ => JobStatus::Generating,
            };
            match status {
                // 🔴 The trim report is read from THIS body — the completion poll — and nowhere
                // else. `plan.warnings` is appended to during `_assemble`, which runs after the
                // last generation finishes, so the copy koblem logged at submit time can never
                // carry a join-trim failure. Re-polling later would work too, right up until the
                // shim's TTL sweeps the job out from under it.
                JobStatus::Completed => {
                    return parse_result(&body).map(|r| (r, parse_trim_report(&body)));
                }
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

    /// Poll to completion without progressive delivery (the restart-reattach path).
    pub async fn wait(&self, job_id: &str) -> Result<H3RenderResult, Error> {
        // The reattach path has no soundtrack plan to check the trim against — it is picking up a
        // render whose koblem-side state did not survive — so it drops the report rather than
        // carrying a number nothing can interpret.
        self.wait_with_partials(job_id, None, None)
            .await
            .map(|(result, _trim)| result)
    }
}

/// Parse the `202`. `id` is the only hard requirement — everything else is geometry the engine
/// reports back, and a missing field must not fail a render that was accepted.
fn parse_submit(body: &Value) -> Result<H3SubmitPlan, Error> {
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or(Error::MissingField("id"))?;
    Ok(H3SubmitPlan {
        id,
        generation_plans: body
            .get("generations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        total_eta_seconds: body.get("total_eta_seconds").and_then(Value::as_f64),
        canvas: body.get("canvas").filter(|v| v.is_object()).cloned(),
        warnings: string_array(body.get("warnings")),
        ignored: string_array(body.get("ignored")),
        // Measured, so unknown at the 202 by definition. `record_timing` fills them in.
        gate_wait_ms: None,
        render_ms: None,
    })
}

fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Pull the progress block out of a poll body. Every field is optional: a shim that has not started
/// the first step yet reports none of them, and the absence is "unknown", never zero.
fn parse_progress(body: &Value) -> H3Progress {
    let u32_at = |k: &str| body.get(k).and_then(Value::as_u64).map(|v| v as u32);
    H3Progress {
        progress: body.get("progress").and_then(Value::as_f64),
        current_step: u32_at("current_step"),
        total_steps: u32_at("total_steps"),
        phase: body
            .get("phase")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        preview_seq: body.get("preview_seq").and_then(Value::as_u64),
        generation_index: u32_at("generation_index"),
        generation_count: u32_at("generation_count"),
        generations_done: u32_at("generations_done"),
        bank_keys: body
            .get("generations")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|g| {
                        let idx = g.get("index").and_then(Value::as_u64)? as u32;
                        let key = g.get("bank_key").and_then(Value::as_str)?;
                        (!key.is_empty()).then(|| (idx, key.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        // Same array, same shape as `bank_keys`. The shim only emits the key once a generation has
        // finished, so a partial list is the normal in-flight state and not a gap to fill in.
        render_seconds: body
            .get("generations")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|g| {
                        let idx = g.get("index").and_then(Value::as_u64)? as u32;
                        let secs = g.get("render_seconds").and_then(Value::as_f64)?;
                        secs.is_finite().then_some((idx, secs))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Does this status body say `/media` is one generation rather than the cut?
///
/// Read straight off the body rather than through [`parse_result`], which refuses a body that has
/// no `result` yet — the reattach path asks this of a job that may still be running, and "not
/// finished" has to read as "nothing to add" rather than as an error.
pub fn result_is_partial(body: &Value) -> bool {
    body.get("result")
        .and_then(|r| r.get("job_is_partial"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Pull the media description out of a completed job body.
///
/// The defaults are **mp4 / video/mp4**, not webm: the shim encodes x265 at CRF 8 and webm cannot
/// carry HEVC at all. Keeping the old webm fallbacks would have written an mp4 payload into a
/// `.webm` name and then failed at the first `ffmpeg -c copy` concat rather than at the write.
///
/// An absent `b64_json` is NOT an error — an engine configured not to inline the render says so by
/// omitting the field, and [`media_to_file`](MinimaxH3Client::media_to_file) streams it raw.
fn parse_result(body: &Value) -> Result<H3RenderResult, Error> {
    let result = body.get("result").ok_or(Error::MissingField("result"))?;
    let output_format = result
        .get("output_format")
        .and_then(|v| v.as_str())
        .unwrap_or(H3_OUTPUT_FORMAT)
        .to_string();
    let mime_type = result
        .get("mime_type")
        .and_then(|v| v.as_str())
        .unwrap_or(H3_MIME_TYPE)
        .to_string();
    let fps = result.get("fps").and_then(|v| v.as_i64()).unwrap_or(0);
    let frame_count = result
        .get("frame_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let video_b64 = result
        .get("b64_json")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(H3RenderResult {
        video_b64,
        mime_type,
        output_format,
        fps,
        frame_count,
        // The shim has exactly one way to say this — a concatenation that FAILED, leaving `/media`
        // as generation 0 alone. There is no window to name, so `rendered_segments` is always empty
        // here and the prose is the whole account. Same field, same meaning, same consequence as on
        // ltx25: these bytes must not become the project's stitched cut.
        job_is_partial: result
            .get("job_is_partial")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        rendered_segments: Vec::new(),
        job_media_is: result
            .get("job_media_is")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// What the shim says it actually removed at the seams, read off the completion body.
///
/// 🔴 **This exists because koblem was guessing.** The seam trim is the shim's business —
/// `H3_TRIM_JOIN` is a MIRROR of a default koblem never sends — so the only honest way to know what
/// a finished cut lost is to read what the shim reports. Until this struct existed koblem inferred
/// it from the clip's DURATION, which cannot tell "the trim rule moved" from "the trim crashed",
/// and on 2026-08-11 it confidently reported the first when the truth was the second: an ffmpeg
/// muxer failure had disabled trimming entirely, and the drift banner sent the author hunting a
/// constant that was correct.
///
/// Every field is optional-by-absence rather than an error: an older shim that does not publish
/// them yields zeros and an empty warning list, which reads as "nothing to say" and lets the
/// duration check stay the backstop it always was.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct H3TrimReport {
    /// `result.trimmed_head_total` — head frames the shim removed across ALL seams, EXCLUDING any
    /// generation whose trim failed. So this is what the delivered file actually lost, not what the
    /// plan intended it to lose.
    pub trimmed_head_total: i64,
    /// `result.frame_count_untrimmed` — the sum of the per-generation renders.
    pub frame_count_untrimmed: i64,
    /// Generation indices the shim flagged `trim_failed`: their duplicate anchor frame is still in
    /// the delivered cut and the seam stutters.
    pub trim_failed_generations: Vec<u32>,
    /// `plan.warnings` as of COMPLETION. Distinct from the copy koblem logs at submit time, which
    /// cannot contain anything `_assemble` appended — including the join-trim failure notes, which
    /// is exactly why the submit-time log never mentioned them.
    pub engine_warnings: Vec<String>,
}

impl H3TrimReport {
    /// Did the engine fail to trim a seam it meant to trim?
    pub fn any_trim_failed(&self) -> bool {
        !self.trim_failed_generations.is_empty()
    }
}

/// Read [`H3TrimReport`] off a completion body. Never fails: absence means "said nothing".
fn parse_trim_report(body: &Value) -> H3TrimReport {
    let result = body.get("result");
    let i64_at = |key: &str| -> i64 {
        result
            .and_then(|r| r.get(key))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    let trim_failed_generations = body
        .get("generations")
        .and_then(|v| v.as_array())
        .map(|gens| {
            gens.iter()
                .filter(|g| g.get("trim_failed").and_then(|v| v.as_bool()) == Some(true))
                .filter_map(|g| g.get("index").and_then(|v| v.as_u64()).map(|n| n as u32))
                .collect()
        })
        .unwrap_or_default();
    let engine_warnings = body
        .pointer("/plan/warnings")
        .and_then(|v| v.as_array())
        .map(|ws| {
            ws.iter()
                .filter_map(|w| w.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    H3TrimReport {
        trimmed_head_total: i64_at("trimmed_head_total"),
        frame_count_untrimmed: i64_at("frame_count_untrimmed"),
        trim_failed_generations,
        engine_warnings,
    }
}

/// Clears the in-flight job slot on drop so a finished render's id can't be targeted by a late
/// `/cancel`.
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
    use serde_json::{Value, json};

    /// What the plan echo is FOR: the geometry the caller did not choose. `frames` came back
    /// snapped, the checkpoint was derived, and generation 1 costs a full DiT + text-encoder reload
    /// because it switched checkpoint family — which is why the ETA jumps and is not a hang.
    #[test]
    fn a_submit_keeps_the_geometry_the_caller_did_not_choose() {
        let plan = super::parse_submit(&json!({
            "id": "h3_9f2c", "status": "queued", "queue_position": 0,
            "generations": [
                {"index": 0, "task": "t2va", "frames": 362, "seconds": 15.083,
                 "width": 1344, "height": 768, "steps": 20, "seed": -1,
                 "checkpoint": "minimax_h3_fl2va_pruned_local", "reload_required": false,
                 "eta_seconds": 830.0, "prompt_chars": 3480},
                {"index": 1, "task": "ref2va", "frames": 209, "seconds": 8.708,
                 "width": 1344, "height": 768, "steps": 20, "seed": -1,
                 "checkpoint": "minimax_h3_ref2va_pruned_local", "reload_required": true,
                 "eta_seconds": 520.0, "prompt_chars": 2900}
            ],
            "total_eta_seconds": 1350.0,
            "canvas": {"width": 1344, "height": 768, "fps": 24},
            "warnings": ["generations[0].frames 350 snapped UP to 362"],
            "ignored": ["sigma_shift_video", "pin_strength"],
        }))
        .expect("a 202 must parse");
        assert_eq!(plan.id, "h3_9f2c");
        assert_eq!(plan.generation_plans.len(), 2);
        // 350 was asked for; 362 is what renders. Reading back what we SENT is the bug.
        assert_eq!(plan.generation_plans[0]["frames"], json!(362));
        assert_eq!(plan.total_eta_seconds, Some(1350.0));
        assert_eq!(plan.width(), Some(1344));
        assert_eq!(plan.height(), Some(768));
        // Totals are summed from the plans; there is no top-level number to trust.
        assert_eq!(plan.total_frames(), Some(571));
        assert_eq!(plan.total_seconds(), Some(15.083 + 8.708));
        // The checkpoint switch is visible, so the pause between generations is explainable.
        assert_eq!(plan.reloads(), 1);
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.ignored, vec!["sigma_shift_video", "pin_strength"]);

        // Everything but `id` is optional: an accepted render must not fail on a field the shim
        // stopped sending.
        let bare = super::parse_submit(&json!({ "id": "h3_9f2c" })).expect("parses");
        assert!(bare.generation_plans.is_empty());
        assert!(bare.ignored.is_empty());
        assert_eq!(bare.total_eta_seconds, None);
        assert_eq!(bare.reloads(), 0);

        assert!(super::parse_submit(&json!({ "status": "queued" })).is_err());
    }

    /// mp4, not webm. The shim encodes x265 at CRF 8 and webm cannot carry HEVC — a webm default
    /// here would name the file wrongly and only fail later, at the concat that rebuilds a cut.
    #[test]
    fn a_completed_job_defaults_to_mp4_and_parses_without_inline_media() {
        let body = json!({
            "status": "completed",
            "result": { "output_format": "mp4", "mime_type": "video/mp4", "fps": 24, "frame_count": 724 },
        });
        let parsed = super::parse_result(&body).expect("completed job must parse");
        assert!(parsed.video_b64.is_none());
        assert_eq!(parsed.frame_count, 724);
        assert_eq!(parsed.fps, 24);
        assert_eq!(parsed.output_format, "mp4");
        assert_eq!(parsed.mime_type, "video/mp4");

        // …and a body that omits the descriptors entirely still says mp4.
        let bare = super::parse_result(&json!({ "status": "completed", "result": {} }))
            .expect("bare result parses");
        assert_eq!(bare.output_format, super::H3_OUTPUT_FORMAT);
        assert_eq!(bare.mime_type, super::H3_MIME_TYPE);

        let mut with_empty = body.clone();
        with_empty["result"]["b64_json"] = json!("");
        assert!(
            super::parse_result(&with_empty)
                .expect("parses")
                .video_b64
                .is_none()
        );
    }

    /// The progress block is what makes an 830-second render legible. Every field is optional and
    /// absence means UNKNOWN — a shim that has not started stepping yet must not report step 0 of
    /// 0, which a progress bar would draw as "finished".
    #[test]
    fn progress_is_parsed_and_absence_is_unknown_not_zero() {
        let p = super::parse_progress(&json!({
            "status": "generating",
            "generation_index": 1, "generation_count": 2, "generations_done": 1,
            "progress": 0.42, "current_step": 9, "total_steps": 40,
            "phase": "denoising", "preview_seq": 12,
            "generations": [
                {"index": 0, "status": "completed", "bank_key": "h3_9f2c:0",
                 "render_seconds": 967.5},
                {"index": 1, "status": "generating"}
            ],
        }));
        assert_eq!(p.progress, Some(0.42));
        assert_eq!(p.current_step, Some(9));
        assert_eq!(p.total_steps, Some(40));
        assert_eq!(p.phase.as_deref(), Some("denoising"));
        assert_eq!(p.preview_seq, Some(12));
        assert_eq!(p.generation_index, Some(1));
        assert_eq!(p.generations_done, Some(1));
        // Only FINISHED generations have a bank key, and it is what a retake's
        // `continuation.from` points at — "prev" is meaningless in a one-generation job.
        assert_eq!(p.bank_keys, vec![(0, "h3_9f2c:0".to_string())]);
        // Only the FINISHED generation reports a time. The one still rendering is absent, not 0 —
        // the whole point of surfacing this is that "how long did it take" stops being the
        // editor's session wall clock, and a zero would be worse than the wall clock.
        assert_eq!(p.render_seconds, vec![(0, 967.5)]);

        let empty = super::parse_progress(&json!({ "status": "queued" }));
        assert_eq!(empty, super::H3Progress::default());
        assert_eq!(empty.total_steps, None);
    }

    /// The measured half of the plan echo. Merged BY `index`, never by position: the plan is the
    /// only thing that knows which generation was `ref2va` and which paid the reload, and pairing a
    /// duration with the wrong one would make the reload look free.
    #[test]
    fn record_timing_merges_by_index_and_leaves_unreported_generations_alone() {
        let mut plan = super::parse_submit(&json!({
            "id": "h3_9f2c",
            "generations": [
                {"index": 0, "task": "t2va", "frames": 362, "reload_required": false},
                {"index": 1, "task": "ref2va", "frames": 209, "reload_required": true},
                {"index": 2, "task": "ref2va", "frames": 209, "reload_required": false}
            ],
        }))
        .expect("plan parses");

        // Out of document order on purpose, and generation 2 never finished — a cancelled render
        // is the normal way to get here.
        plan.record_timing(54_200, 1_494_000, &[(1, 520.4), (0, 967.5)]);

        assert_eq!(plan.gate_wait_ms, Some(54_200));
        assert_eq!(plan.render_ms, Some(1_494_000));
        let secs = |i: usize| {
            plan.generation_plans[i]
                .get("render_seconds")
                .and_then(Value::as_f64)
        };
        assert_eq!(secs(0), Some(967.5));
        assert_eq!(secs(1), Some(520.4));
        // 🔴 Absence is "not measured". A `0.0` against the generation that was interrupted would
        // be the only wrong number on the page, and the one a reader would trust most.
        assert_eq!(secs(2), None);

        // The waiting half stays SEPARATE from the render half in the stamp too. Folding a 54.2 s
        // queue into the duration is exactly how the engine gets blamed for being busy.
        let stamped = plan.to_json();
        assert_eq!(stamped["gate_wait_ms"], json!(54_200));
        assert_eq!(stamped["render_ms"], json!(1_494_000));
        // Untouched by a render that never got measured — null, never zero.
        let fresh = super::parse_submit(&json!({ "id": "h3_1", "generations": [] }))
            .expect("plan parses")
            .to_json();
        assert!(fresh["gate_wait_ms"].is_null());
        assert!(fresh["render_ms"].is_null());
    }
}
