//! HTTP client for the `krea2` sd-server — the GPU Krea-2-Turbo image engine
//! (the same stable-diffusion.cpp fork as flux2, a different checkpoint).
//!
//! Wire-identical to [`crate::flux2`]: an async job queue where `POST
//! /sdcpp/v1/img_gen` returns a job id immediately, the single worker renders it,
//! and the client polls `GET /sdcpp/v1/jobs/{id}` to completion. `POST
//! /sdcpp/v1/jobs/{id}/cancel` aborts a queued or in-flight render (cooperative
//! cancel between DiT steps).
//!
//! **Pure JSON passthrough**: every request field — including the edit path's
//! `ref_images` / `ref_image_args` / `lora` — is built by
//! `routes::krea2::build_img_gen_body` and forwarded verbatim, so teaching the
//! engine a new field needs no change here.
//!
//! Differences from flux2 that matter to callers:
//!
//! - **Text-to-image AND instruction edit, but ONE checkpoint.** Krea 2 has no
//!   first-party edit model (Krea's own tech report lists editing as future
//!   work); editing is the `krea2-identity-edit` LoRA plus the engine's
//!   `krea2_identity_edit` reference-image preset applied to the resident DiT. So
//!   there IS a `ref_images` path now, but still no `base`/`edit` variant
//!   selector: the route never sends a `model` field, and mode selection is a
//!   per-request LoRA + preset rather than a checkpoint swap. No `init_image` /
//!   `mask_image` either — editing is reference-conditioned, not
//!   denoise-from-source.
//! - **Distilled checkpoint.** 8 steps at cfg 1.0 for t2i, ~10 for edits (the
//!   model card's `guidance_scale: 0.0` = "no CFG"; this engine spells that
//!   `cfg_scale: 1.0`). Flow-shift mu 1.15 is applied engine-side, not as a
//!   request field.
//! - **Renders are slow at high resolution** (~11 s at 1024², ~72 s at 2048² on
//!   the 5060 Ti), so the poll loop is the only timeout — same as flux2.
//! - **An unresolvable LoRA name is a bare HTTP 400** with nothing in the engine
//!   log, which surfaces here as [`Error::Upstream`] with an empty body. That is
//!   why the route reports LoRA availability from `/capabilities` rather than
//!   letting the UI find out by rendering.
//!
//! This client acquires the shared [`GpuLock`] as the `krea2` consumer — its OWN
//! budget line, not a second flux slot: the ~7.75 GiB nvfp4 DiT cannot co-reside
//! with FLUX.2 on the 5060 Ti, so the two image engines serialise by budget.
//!
//! Self-registers the krea2 service on the gate's drain/unload registry when
//! `base_url` is set, so the gate knows where to POST `/v1/admin/unload` (the
//! eviction + operator-disable side-effect) and `/v1/admin/drain`.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuKrea2Guard, GpuLock, HeavyKind, HeavyService};
use serde_json::Value;

use crate::wire::{PartSupport, WireForm, WireMeter, WireSize};

/// Connect timeout guards a dead host. No request timeout: a 2048² render takes
/// over a minute and we poll with our own loop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the poll loop checks job status.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("krea2-server HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("krea2-server returned {0}: {1}")]
    Upstream(u16, String),
    #[error("krea2 service not configured")]
    NotConfigured,
    #[error("krea2-server job {0} failed: {1}")]
    JobFailed(String, String),
    #[error("krea2-server job {0} was cancelled")]
    Cancelled(String),
    #[error("krea2-server response missing required field: {0}")]
    MissingField(&'static str),
    #[error("engine returned an undecodable image: {0}")]
    BadImage(String),
}

/// Terminal/transient state of an sdcpp job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Queued,
    Generating,
    Completed,
    Failed,
    Cancelled,
}

/// One generated image returned from a completed img_gen job.
pub struct RenderedImage {
    /// The encoded image (PNG/JPEG/WebP depending on output_format), RAW.
    ///
    /// It used to be the engine's base64 string, decoded by every caller a line later
    /// The engine now serves each image at
    /// `…/media?index=n`, and inlines base64 only while `SDCPP_JOB_MEDIA_B64` is on — so this
    /// is the one shape that works either way.
    pub bytes: Bytes,
}

/// What `generate` hands back to the route handler.
pub struct Krea2RenderResult {
    pub images: Vec<RenderedImage>,
    /// `output_format` echoed by the server (png/jpeg/webp).
    pub output_format: String,
}

#[derive(Clone)]
pub struct Krea2Client {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    /// What the last submit and the last result actually cost on the wire. See `wire.rs`.
    ///
    /// `request.parts == 0` with a fat `json_bytes` means the render fell back to inlining —
    /// either it carried no media, or the engine does not advertise `part:` yet (§9.4).
    wire: WireMeter,
    /// Whether this engine build understands `part:<name>` media (§9.4).
    part_support: PartSupport,
}

impl Krea2Client {
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
            wire: WireMeter::new(),
            part_support: PartSupport::new(),
        };
        // Register on the gate so an eviction / operator disable can SIGKILL a
        // render and so acquire_for_krea2 places against the right budget line.
        if let Some(url) = base_url.as_deref().filter(|s| !s.is_empty()) {
            me.gate.set_krea2_service(HeavyService {
                kind: HeavyKind::Krea2,
                url: url.trim_end_matches('/').to_string(),
                unload_path: "/v1/admin/unload".to_string(),
                drain_path: "/v1/admin/drain".to_string(),
            });
        }
        me
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalised = base_url
            .as_ref()
            .map(|s| s.trim_end_matches('/').to_string());
        *self.base_url.write().expect("krea2 base_url poisoned") = normalised.clone();
        // A different host is a different binary.
        self.part_support.reset();
        if let Some(url) = normalised {
            self.gate.set_krea2_service(HeavyService {
                kind: HeavyKind::Krea2,
                url,
                unload_path: "/v1/admin/unload".to_string(),
                drain_path: "/v1/admin/drain".to_string(),
            });
        }
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("krea2 base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("krea2 base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the GPU as the krea2 consumer. Returns `None` when krea2 has been
    /// administratively disabled (mirrors `Flux2Client::acquire`).
    pub async fn acquire(&self) -> Option<(GpuKrea2Guard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_krea2().await?;
        Some((guard, start.elapsed()))
    }

    /// `GET /sdcpp/v1/capabilities` — samplers/schedulers/limits/defaults, plus
    /// `loras[]`: the LoRAs the server can actually resolve, which is how
    /// `/props` decides whether edit mode is offered at all.
    pub async fn capabilities(&self) -> Result<Value, Error> {
        let url = self.url("/sdcpp/v1/capabilities")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// `GET /sdapi/v1/loras` — the resolvable adapter list, WITHOUT waking the
    /// worker.
    ///
    /// This exists because `/sdcpp/v1/capabilities` is answered by the CUDA
    /// child, so the supervisor's residency gate 410s it whenever the worker is
    /// idle-unloaded or has been evicted by the GPU gate — which is most of the
    /// time, and is exactly when the Krea2 panel bootstraps. `/sdapi/v1/loras`
    /// is registered ON THE SUPERVISOR (`register_worker_supervisor_endpoints`)
    /// precisely so that asking what adapters exist never cold-starts a ~16 GB
    /// worker, so it answers while unloaded.
    ///
    /// Same semantics, not an approximation: both routes are built from
    /// `scan_lora_dir()` over `--lora-model-dir`, keyed on the path relative to
    /// that dir, so "in this list" and "the engine can resolve it" are the same
    /// question. Returns the array wrapped as `{"loras": [...]}` so it drops
    /// straight into the same `loras[]` reader the capabilities body uses.
    pub async fn loras(&self) -> Result<Value, Error> {
        let url = self.url("/sdapi/v1/loras")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let loras: Value = resp.json().await?;
        Ok(serde_json::json!({ "loras": loras }))
    }

    /// Liveness: true when the server's `/health` returns 2xx.
    pub async fn healthy(&self) -> bool {
        let Ok(url) = self.url("/health") else {
            return false;
        };
        matches!(self.http.get(url).send().await, Ok(resp) if resp.status().is_success())
    }

    /// `POST /sdcpp/v1/img_gen` — submit a render. Returns the job id (202).
    pub(crate) async fn submit(
        &self,
        body: &Value,
        media_parts: &[(String, Bytes)],
    ) -> Result<String, Error> {
        let url = self.url("/sdcpp/v1/img_gen")?;
        // No parts and an engine that does not advertise `part:` both take the JSON body this
        // route has always taken (§9.4). Otherwise it is multipart: a `request` part plus one
        // binary part per image, which is what stops an edit's references being base64.
        if media_parts.is_empty() || !self.accepts_part_refs().await {
            let mut inlined = body.clone();
            crate::director_assets::inline_parts(&mut inlined, media_parts);
            self.wire.submitted(
                "krea2",
                "img_gen",
                WireSize {
                    json_bytes: serde_json::to_string(&inlined)
                        .map(|s| s.len())
                        .unwrap_or(0) as u64,
                    media_bytes: 0,
                    parts: 0,
                },
            );
            return self.submit_body(&url, &inlined).await;
        }
        let mut form = WireForm::new().json("request", body);
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
        self.wire.submitted("krea2", "img_gen", form.size());
        let resp = self
            .http
            .post(url)
            .multipart(form.into_form())
            .send()
            .await?;
        Self::submit_id(resp).await
    }

    /// The legacy JSON submit, kept as the fallback path.
    async fn submit_body(&self, url: &str, body: &Value) -> Result<String, Error> {
        let resp = self.http.post(url).json(body).send().await?;
        Self::submit_id(resp).await
    }

    /// The job id out of a 202, or the engine's own rejection.
    async fn submit_id(resp: reqwest::Response) -> Result<String, Error> {
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let body: Value = resp.json().await?;
        body.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or(Error::MissingField("id"))
    }

    /// Does the engine take `part:<name>` media (§9.4)? Asked on `/health`, which the supervisor
    /// answers without waking the worker, and cached for the life of the process.
    pub async fn accepts_part_refs(&self) -> bool {
        if let Some(known) = self.part_support.cached() {
            return known;
        }
        let Ok(url) = self.url("/health") else {
            return false;
        };
        let Ok(resp) = self.http.get(url).send().await else {
            return false;
        };
        if !resp.status().is_success() {
            return false;
        }
        match resp.json::<Value>().await {
            Ok(body) => self.part_support.observe(&body),
            Err(_) => false,
        }
    }

    /// `GET /sdcpp/v1/jobs/{id}` — poll once, returning the parsed body.
    pub(crate) async fn poll(&self, job_id: &str) -> Result<Value, Error> {
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

    /// `POST /sdcpp/v1/jobs/{id}/cancel` for a specific job id.
    pub async fn cancel(&self, job_id: &str) -> Result<Value, Error> {
        let url = self.url(&format!("/sdcpp/v1/jobs/{job_id}/cancel"))?;
        let resp = self.http.post(url).send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        // 200 (queued cancel / already terminal) and 202 (cooperative in-flight
        // cancel accepted) are both success.
        if !status.is_success() {
            return Err(Error::Upstream(status.as_u16(), body.to_string()));
        }
        Ok(body)
    }

    /// Submit + poll to completion. The queue owns every gated render now; this is
    /// what is left for the in-process `/api/v1/oneshot` composite, which renders a
    /// single t2i image under a guard it acquired itself.
    pub async fn generate(
        &self,
        body: &Value,
        media_parts: &[(String, Bytes)],
    ) -> Result<Krea2RenderResult, Error> {
        let job_id = self.submit(body, media_parts).await?;
        loop {
            let body = self.poll(&job_id).await?;
            let status = match body.get("status").and_then(|v| v.as_str()) {
                Some("queued") => JobStatus::Queued,
                Some("generating") => JobStatus::Generating,
                Some("completed") => JobStatus::Completed,
                Some("failed") => JobStatus::Failed,
                Some("cancelled") => JobStatus::Cancelled,
                _ => JobStatus::Generating,
            };
            match status {
                JobStatus::Completed => {
                    // Inlined images are already bytes here; the rest are fetched raw (B5).
                    return self.collect_images(&job_id, parse_result(&body)?).await;
                }
                JobStatus::Failed => {
                    let msg = body
                        .pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("generation failed")
                        .to_string();
                    return Err(Error::JobFailed(job_id, msg));
                }
                JobStatus::Cancelled => return Err(Error::Cancelled(job_id)),
                JobStatus::Queued | JobStatus::Generating => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }
}

/// Pull the image b64 payloads out of a completed sdcpp job body. The result
/// shape is `{result: {output_format, images: [{index, b64_json}]}}`.
pub(crate) fn parse_result(body: &Value) -> Result<ParsedImages, Error> {
    let result = body.get("result").ok_or(Error::MissingField("result"))?;
    let output_format = result
        .get("output_format")
        .and_then(|v| v.as_str())
        .unwrap_or("png")
        .to_string();
    let images_json = result
        .get("images")
        .and_then(|v| v.as_array())
        .ok_or(Error::MissingField("result.images"))?;
    // `b64_json` present = the engine inlined it. Absent = it is waiting at `…/media?index=n`,
    // which the caller fetches; `None` here means exactly that, and is not an error.
    let mut images: Vec<Option<Bytes>> = Vec::with_capacity(images_json.len());
    for img in images_json {
        match img.get("b64_json").and_then(|v| v.as_str()) {
            Some(b64) => {
                let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .map_err(|e| Error::BadImage(e.to_string()))?;
                images.push(Some(Bytes::from(bytes)));
            }
            None => images.push(None),
        }
    }
    if images.is_empty() {
        return Err(Error::MissingField("result.images (empty)"));
    }
    Ok(ParsedImages {
        images,
        output_format,
    })
}

/// A completed image job as the poll body describes it: the payloads that were inlined, and a
/// `None` for each one that has to be fetched.
pub(crate) struct ParsedImages {
    images: Vec<Option<Bytes>>,
    output_format: String,
}

impl Krea2Client {
    /// `GET /sdcpp/v1/jobs/{id}/media?index=n` — one finished image, RAW (B5).
    ///
    /// The route the engine grew so an image result stops being `result.images[].b64_json`: same
    /// buffer, a third fewer bytes on the wire, and no decode at either end.
    async fn fetch_image(&self, job_id: &str, index: usize) -> Result<Bytes, Error> {
        let url = self.url(&format!("/sdcpp/v1/jobs/{job_id}/media?index={index}"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.bytes().await?)
    }

    /// Turn a parsed poll body into images, fetching whatever the engine did not inline.
    ///
    /// Both branches end in the same place — bytes — so a caller cannot tell which transport ran,
    /// which is what lets `SDCPP_JOB_MEDIA_B64` be flipped per service with no koblem change.
    pub(crate) async fn collect_images(
        &self,
        job_id: &str,
        parsed: ParsedImages,
    ) -> Result<Krea2RenderResult, Error> {
        let mut images = Vec::with_capacity(parsed.images.len());
        let mut inline_bytes = 0u64;
        let mut fetched_bytes = 0u64;
        for (index, payload) in parsed.images.into_iter().enumerate() {
            let bytes = match payload {
                Some(bytes) => {
                    inline_bytes += bytes.len() as u64;
                    bytes
                }
                None => {
                    let bytes = self.fetch_image(job_id, index).await?;
                    fetched_bytes += bytes.len() as u64;
                    bytes
                }
            };
            images.push(RenderedImage { bytes });
        }
        if fetched_bytes > 0 {
            self.wire.fetched_raw("krea2", fetched_bytes);
        } else {
            self.wire.fetched_inline_b64("krea2", inline_bytes);
        }
        Ok(Krea2RenderResult {
            images,
            output_format: parsed.output_format,
        })
    }

    /// What the last submit and the last result cost, for the render log row (§9.2).
    pub fn wire(&self) -> &WireMeter {
        &self.wire
    }
}
