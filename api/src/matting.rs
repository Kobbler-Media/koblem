//! HTTP client for the matting (background-removal) service — Acly/vision.cpp
//! BiRefNet / RMBG-2.0 wrapped in `matting-server`. Gated under the shared budget
//! scheduler as the `matting` service (RMBG-2.0 peaks ~3.5 GB at `process_res`
//! 2048; it must be clamped server-side, but we still reserve its budget so a
//! co-resident card doesn't OOM). The gate-chosen card UUID rides in as a `gpu`
//! multipart field (matting's `/remove` is multipart — see handoff §7b).

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use kob_gpu_gate::GpuLock;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

/// Errors from MattingClient calls.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("matting HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("matting service returned {0}: {1}")]
    Upstream(u16, String),
    #[error("matting service not configured")]
    NotConfigured,
    #[error("matting service is administratively disabled")]
    Disabled,
}

/// Knobs forwarded to `matting-server`'s `/remove` query string. `None` leaves
/// the server default in place.
#[derive(Debug, Default, Clone)]
pub struct RemoveOptions {
    /// "cpu" | "gpu" (server default gpu).
    pub backend: Option<String>,
    /// Inference resolution, 256..2048 (model native ~1024 when unset).
    pub process_res: Option<u32>,
    /// Mask strength / threshold, 0..1.
    pub sensitivity: Option<f32>,
    /// Box-blur radius on the mask, px.
    pub mask_blur: Option<i32>,
    /// Dilate (+) / erode (-) the mask, px.
    pub mask_offset: Option<i32>,
    /// Edge decontamination.
    pub refine: Option<bool>,
    /// Invert the mask.
    pub invert: Option<bool>,
    /// "alpha" (RGBA cutout) | "color" (flatten onto bg_color).
    pub bg_mode: Option<String>,
    /// "#rrggbb" background for bg_mode=color.
    pub bg_color: Option<String>,
    /// Return the grayscale mask instead of the cutout.
    pub mask_only: Option<bool>,
}

impl RemoveOptions {
    /// Build the query-string pairs for the `/remove` request.
    ///
    /// `force_json` picks the response shape. The batch route needs it — several images can only
    /// come back as a list — but a single image is served as raw `image/png` without it, which is
    /// what [`MattingClient::remove_single_raw`] uses (§5a-a: this hop is ours at both ends and had
    /// no business base64ing a cutout).
    fn query_pairs(&self, force_json: bool) -> Vec<(&'static str, String)> {
        let mut q: Vec<(&'static str, String)> = if force_json {
            vec![("format", "json".to_string())]
        } else {
            Vec::new()
        };
        if let Some(b) = self.backend.as_deref().filter(|s| !s.is_empty()) {
            q.push(("backend", b.to_string()));
        }
        if let Some(v) = self.process_res {
            q.push(("process_res", v.to_string()));
        }
        if let Some(v) = self.sensitivity {
            q.push(("sensitivity", v.to_string()));
        }
        if let Some(v) = self.mask_blur {
            q.push(("mask_blur", v.to_string()));
        }
        if let Some(v) = self.mask_offset {
            q.push(("mask_offset", v.to_string()));
        }
        if let Some(v) = self.refine {
            q.push(("refine", if v { "1" } else { "0" }.to_string()));
        }
        if let Some(v) = self.invert {
            q.push(("invert", if v { "1" } else { "0" }.to_string()));
        }
        if let Some(m) = self.bg_mode.as_deref().filter(|s| !s.is_empty()) {
            q.push(("bg_mode", m.to_string()));
        }
        if let Some(c) = self.bg_color.as_deref().filter(|s| !s.is_empty()) {
            q.push(("bg_color", c.to_string()));
        }
        if let Some(v) = self.mask_only {
            q.push(("mask_only", if v { "1" } else { "0" }.to_string()));
        }
        q
    }
}

#[derive(Clone)]
pub struct MattingClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

/// A single cutout, raw. The two headers are how the PNG path reports what the JSON path put in
/// the body — the engine sets them on both.
pub struct RemoveRaw {
    pub png: Bytes,
    pub elapsed_seconds: Option<f64>,
    pub backend: Option<String>,
}

/// One result from `/remove` — a base64-encoded PNG.
#[derive(Debug, Deserialize)]
pub struct RemoveResult {
    pub png_base64: String,
}

/// `/remove` JSON response (`?format=json`).
#[derive(Debug, Deserialize)]
pub struct RemoveResponse {
    pub count: u32,
    pub results: Vec<RemoveResult>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub elapsed_seconds: Option<f64>,
}

/// `/health` response. Mirrors the matting-server health shape.
#[derive(Debug, Default, Deserialize)]
pub struct HealthInfo {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub worker_alive: bool,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub native_size: Option<i64>,
    #[serde(default)]
    pub in_flight: Option<i64>,
    /// koblem-side liveness flag — true when the probe reached the service.
    #[serde(default)]
    pub healthy: bool,
}

impl MattingClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        // No timeout: an inference can take several seconds (CPU) and the GPU
        // worker may cold-start. Matches the other GPU clients.
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        let normalized = base_url.map(|s| s.trim_end_matches('/').to_string());
        // Register the URL so the gate can SIGKILL the worker when matting is
        // evicted / relocated / disabled.
        gate.set_matting_url(normalized.clone());
        Self {
            http,
            base_url: Arc::new(RwLock::new(normalized)),
            gate,
        }
    }

    /// Hot-swap the service URL (settings handler).
    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalized = base_url.map(|s| s.trim_end_matches('/').to_string());
        self.gate.set_matting_url(normalized.clone());
        let mut guard = self.base_url.write().expect("matting base_url poisoned");
        *guard = normalized;
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("matting base_url poisoned")
            .as_deref()
            .is_some_and(|s| !s.is_empty())
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("matting base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Health probe. Returns a default (unhealthy) struct on any error so the UI
    /// can render "engine unreachable" without surfacing the error.
    pub async fn health(&self) -> Result<HealthInfo, Error> {
        let url = self.url("/health")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Ok(HealthInfo::default());
        }
        let mut info: HealthInfo = resp.json().await.unwrap_or_default();
        info.healthy = true;
        Ok(info)
    }

    /// Remove the background from a single image, taking the cutout as RAW PNG BYTES.
    ///
    /// Prefer this over [`remove_single`](Self::remove_single) wherever the caller wants bytes:
    /// the engine serves a lone image as `image/png` unless `format=json` is asked for, so the
    /// base64 in the JSON shape was pure overhead on a hop where both ends are ours — encode in
    /// C++, decode in Rust, ~33 % more bytes, for nothing (§5a-a).
    ///
    /// `elapsed_seconds` / `backend` come from the `X-BG-*` headers the engine sets on BOTH
    /// shapes, so nothing is lost by not asking for the JSON.
    pub async fn remove_single_raw(
        &self,
        name: &str,
        bytes: Bytes,
        opts: &RemoveOptions,
    ) -> Result<RemoveRaw, Error> {
        let url = self.url("/remove")?;
        let permit = self
            .gate
            .acquire_for_matting()
            .await
            .ok_or(Error::Disabled)?;
        let mut form = Form::new();
        if let Some(g) = permit.chosen_gpu() {
            form = form.text("gpu", g.to_string());
        }
        form = form.part(
            "images",
            Part::bytes(bytes.to_vec()).file_name(name.to_string()),
        );
        let resp = self
            .http
            .post(url)
            .query(&opts.query_pairs(false))
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let elapsed_seconds = header("x-bg-elapsed-seconds").and_then(|v| v.parse().ok());
        let backend = header("x-bg-backend");
        let png = resp.bytes().await?;
        drop(permit);
        Ok(RemoveRaw {
            png,
            elapsed_seconds,
            backend,
        })
    }

    /// Remove the background from a single image. Returns the resulting PNG
    /// base64 strings (usually one).
    pub async fn remove_single(
        &self,
        name: &str,
        bytes: Bytes,
        opts: &RemoveOptions,
    ) -> Result<RemoveResponse, Error> {
        self.remove_batch(&[(name.to_string(), bytes)], opts).await
    }

    /// Remove the background from a batch of images via repeated multipart
    /// `images` fields. The server returns `{count, results:[{png_base64}], …}`.
    pub async fn remove_batch(
        &self,
        images: &[(String, Bytes)],
        opts: &RemoveOptions,
    ) -> Result<RemoveResponse, Error> {
        let url = self.url("/remove")?;

        // Acquire a card under the budget scheduler. Held until the response is
        // parsed so the reserved VRAM isn't freed mid-inference. `None` = matting
        // is administratively disabled.
        let permit = self
            .gate
            .acquire_for_matting()
            .await
            .ok_or(Error::Disabled)?;

        let mut form = Form::new();
        // Multi-GPU placement: send the chosen card as a `gpu` multipart field.
        if let Some(g) = permit.chosen_gpu() {
            form = form.text("gpu", g.to_string());
        }
        for (name, bytes) in images {
            let part = Part::bytes(bytes.to_vec()).file_name(name.clone());
            form = form.part("images", part);
        }

        // reqwest's `.query()` URL-encodes the pairs for us (forces format=json).
        let resp = self
            .http
            .post(url)
            .query(&opts.query_pairs(true))
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let parsed: RemoveResponse = resp.json().await?;
        // `permit` is held to here — the reserved budget frees on return.
        drop(permit);
        Ok(parsed)
    }
}
