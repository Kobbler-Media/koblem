//! HTTP client for flux2.cpp's sd-server — the GPU "flux" image-generation
//! engine (FLUX.2-Klein via the stable-diffusion.cpp fork).
//!
//! flux2's sdcpp API is an async job queue: `POST /sdcpp/v1/img_gen` returns a
//! job id immediately, the single worker renders it, and the client polls `GET
//! /sdcpp/v1/jobs/{id}` to completion. `POST /sdcpp/v1/jobs/{id}/cancel` aborts a
//! queued or in-flight render (cooperative cancel between DiT steps).
//!
//! This module is the RPCs only: submit, poll, cancel, collect. The GPU slot, the
//! poll loop and the collect belong to `queue::adapters::flux2` now — the queue is
//! the single writer to this engine.
//!
//! Self-registers the flux service on the gate's heavy-slot registry when
//! `base_url` is set, so the gate knows where to POST `/unload` (the
//! operator-disable side-effect) and `/drain`.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, HeavyKind, HeavyService};
use serde_json::Value;

use crate::wire::{PartSupport, WireForm, WireMeter, WireSize};

/// Connect timeout guards a dead host. No request timeout: a render can take
/// tens of seconds and the adapter polls with its own loop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("flux2-server HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("flux2-server returned {0}: {1}")]
    Upstream(u16, String),
    #[error("flux service not configured")]
    NotConfigured,
    #[error("flux2-server response missing required field: {0}")]
    MissingField(&'static str),
    #[error("engine returned an undecodable image: {0}")]
    BadImage(String),
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

/// What a completed render hands back to the adapter.
pub struct FluxRenderResult {
    pub images: Vec<RenderedImage>,
    /// `output_format` echoed by the server (png/jpeg/webp).
    pub output_format: String,
}

#[derive(Clone)]
pub struct Flux2Client {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    /// What the last submit and the last result actually cost on the wire. See `wire.rs`.
    wire: WireMeter,
    /// Whether this engine build understands `part:<name>` media (§9.4).
    part_support: PartSupport,
}

impl Flux2Client {
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
        // Register on the gate so Flux's heavy-slot disable can SIGKILL a render
        // and so acquire_for_flux drains the LLM + evicts light peers correctly.
        if let Some(url) = base_url.as_deref().filter(|s| !s.is_empty()) {
            me.gate.set_flux_service(HeavyService {
                kind: HeavyKind::Flux,
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
        *self.base_url.write().expect("flux base_url poisoned") = normalised.clone();
        // A different host is a different binary.
        self.part_support.reset();
        if let Some(url) = normalised {
            self.gate.set_flux_service(HeavyService {
                kind: HeavyKind::Flux,
                url,
                unload_path: "/v1/admin/unload".to_string(),
                drain_path: "/v1/admin/drain".to_string(),
            });
        }
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("flux base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("flux base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// `GET /sdcpp/v1/capabilities` — samplers/schedulers/limits/defaults +
    /// the per-variant (base/edit) info the props endpoint surfaces.
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

    /// Liveness: true when the server's `/health` returns 2xx (added alongside
    /// the dual-DiT admin endpoints). Falls back to nothing else — a down server
    /// reports unhealthy.
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
                "flux2",
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
        self.wire.submitted("flux2", "img_gen", form.size());
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
}

impl Flux2Client {
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
    ) -> Result<FluxRenderResult, Error> {
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
            self.wire.fetched_raw("flux2", fetched_bytes);
        } else {
            self.wire.fetched_inline_b64("flux2", inline_bytes);
        }
        Ok(FluxRenderResult {
            images,
            output_format: parsed.output_format,
        })
    }

    /// What the last submit and the last result cost, for the render log row (§9.2).
    pub fn wire(&self) -> &WireMeter {
        &self.wire
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
