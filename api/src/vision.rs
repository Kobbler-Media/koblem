//! HTTP client for the SigLIP2 vision service. Acquires the shared GPU lock
//! (vision priority) before sending, returns the guard alongside the response
//! so callers can hold it through body consumption.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, GpuVisionGuard};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

/// Errors from VisionClient calls.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vision HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("vision service returned {0}: {1}")]
    Upstream(u16, String),
    #[error("vision service not configured")]
    NotConfigured,
    #[error("TTS has preempted the GPU; vision request rejected")]
    Preempted,
}

#[derive(Clone)]
pub struct VisionClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

#[derive(Debug, Deserialize)]
pub struct EmbedResponse {
    pub embeddings: Vec<Vec<f32>>,
    #[serde(default)]
    pub last_hidden_state: Option<Vec<Vec<Vec<f32>>>>,
}

#[derive(Debug, Deserialize)]
pub struct ClassifyResponse {
    pub scores: Vec<Vec<f32>>,
    #[serde(default)]
    pub logits: Option<Vec<Vec<f32>>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct HealthInfo {
    pub healthy: bool,
    pub model: Option<String>,
}

impl VisionClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        // No timeout: GPU-gated requests can queue for many minutes behind
        // TTS preempts + STT cooldowns. Matches LlamaClient's approach.
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            base_url: Arc::new(RwLock::new(
                base_url.map(|s| s.trim_end_matches('/').to_string()),
            )),
            gate,
        }
    }

    /// Hot-swap the service URL (called from the settings handler when the
    /// user updates `vision_service_url`). Stored behind an RwLock so all
    /// cloned clients see the new value without restart.
    pub fn set_base_url(&self, base_url: Option<String>) {
        let mut guard = self.base_url.write().expect("vision base_url poisoned");
        *guard = base_url.map(|s| s.trim_end_matches('/').to_string());
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("vision base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the vision guard, returning Err(Preempted) if TTS is signalling.
    /// Measures acquire wait time for logging.
    async fn acquire(&self) -> Result<(GpuVisionGuard, Duration), Error> {
        let start = Instant::now();
        let guard = self
            .gate
            .acquire_for_vision()
            .await
            .ok_or(Error::Preempted)?;
        Ok((guard, start.elapsed()))
    }

    /// Low-bandwidth health probe — no gate acquisition. Returns healthy+model.
    pub async fn health_info(&self) -> Result<HealthInfo, Error> {
        let url = self.url("/health")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Ok(HealthInfo::default());
        }
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            model: Option<String>,
        }
        let raw: Raw = resp.json().await.unwrap_or(Raw { model: None });
        Ok(HealthInfo {
            healthy: true,
            model: raw.model,
        })
    }

    /// Legacy bool probe.
    pub async fn health(&self) -> Result<bool, Error> {
        Ok(self.health_info().await?.healthy)
    }

    /// Embed a batch of images. Returns the permit alongside the response plus
    /// gate_wait (measured strictly around guard acquisition).
    pub async fn embed(
        &self,
        images: &[(String, Bytes)],
        max_num_patches: Option<u32>,
        pooling: Option<&str>,
        return_last_hidden: bool,
    ) -> Result<(GpuVisionGuard, EmbedResponse, Duration), Error> {
        let (permit, gate_wait) = self.acquire().await?;
        let url = self.url("/v1/embeddings")?;
        let mut form = Form::new();
        // Multi-GPU placement: send the gate-chosen card as a `gpu` multipart
        // field (siglip2 reads it via collect_field per the handoff §7b gotcha).
        if let Some(g) = permit.chosen_gpu() {
            form = form.text("gpu", g.to_string());
        }
        for (name, bytes) in images {
            let part = Part::bytes(bytes.to_vec()).file_name(name.clone());
            form = form.part("images", part);
        }
        if let Some(p) = max_num_patches {
            form = form.text("max_num_patches", p.to_string());
        }
        if let Some(p) = pooling {
            form = form.text("pooling", p.to_string());
        }
        if return_last_hidden {
            form = form.text("return_last_hidden", "true".to_string());
        }

        let resp = self.http.post(url).multipart(form).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let parsed: EmbedResponse = resp.json().await?;
        Ok((permit, parsed, gate_wait))
    }

    pub async fn classify(
        &self,
        images: &[(String, Bytes)],
        prompts: &[String],
        max_num_patches: Option<u32>,
        return_logits: bool,
    ) -> Result<(GpuVisionGuard, ClassifyResponse, Duration), Error> {
        let (permit, gate_wait) = self.acquire().await?;
        let url = self.url("/v1/classify")?;
        let mut form = Form::new();
        if let Some(g) = permit.chosen_gpu() {
            form = form.text("gpu", g.to_string());
        }
        for (name, bytes) in images {
            let part = Part::bytes(bytes.to_vec()).file_name(name.clone());
            form = form.part("images", part);
        }
        for p in prompts {
            form = form.text("prompts", p.clone());
        }
        if let Some(p) = max_num_patches {
            form = form.text("max_num_patches", p.to_string());
        }
        if return_logits {
            form = form.text("return_logits", "true".to_string());
        }

        let resp = self.http.post(url).multipart(form).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let parsed: ClassifyResponse = resp.json().await?;
        Ok((permit, parsed, gate_wait))
    }

    /// Encode a batch of text prompts into the SigLIP shared embedding space.
    /// Calls upstream `POST /v1/text_embeddings` (form-encoded `prompts` repeats).
    pub async fn text_embeddings(
        &self,
        prompts: &[String],
    ) -> Result<(GpuVisionGuard, EmbedResponse, Duration), Error> {
        let (permit, gate_wait) = self.acquire().await?;
        let url = self.url("/v1/text_embeddings")?;
        let mut form = Form::new();
        if let Some(g) = permit.chosen_gpu() {
            form = form.text("gpu", g.to_string());
        }
        for p in prompts {
            form = form.text("prompts", p.clone());
        }
        let resp = self.http.post(url).multipart(form).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let parsed: EmbedResponse = resp.json().await?;
        Ok((permit, parsed, gate_wait))
    }

    /// Classify pre-computed image embeddings against text prompts. Reproduces
    /// the full SigLIP forward server-side so output matches `/v1/classify`
    /// for the same images within fp16 noise. Calls upstream `POST /v1/classify_from_embeddings`
    /// (JSON body, no multipart needed since the payload is just floats).
    pub async fn classify_from_embeddings(
        &self,
        image_embeddings: &[Vec<f32>],
        prompts: &[String],
        return_logits: bool,
    ) -> Result<(GpuVisionGuard, ClassifyResponse, Duration), Error> {
        let (permit, gate_wait) = self.acquire().await?;
        let url = self.url("/v1/classify_from_embeddings")?;
        let mut body = serde_json::json!({
            "image_embeddings": image_embeddings,
            "prompts": prompts,
            "return_logits": return_logits,
        });
        if let Some(g) = permit.chosen_gpu()
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("gpu".into(), serde_json::Value::String(g.to_string()));
        }
        let resp = self.http.post(url).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let parsed: ClassifyResponse = resp.json().await?;
        Ok((permit, parsed, gate_wait))
    }
}
