//! HTTP client for the NVIDIA Parakeet-TDT STT service. Acquires the shared
//! GPU lock (STT background priority, yields to TTS/vision). Passes a
//! `cancel_url` header so the Python worker polls koblem between chunks
//! and stops early when a higher-priority job is waiting.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, GpuSttGuard};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("STT HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("STT service returned {0}: {1}")]
    Upstream(u16, String),
    #[error("STT service not configured")]
    NotConfigured,
    #[error("TTS or vision preempted the GPU; STT request rejected")]
    Preempted,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SttParams {
    /// "segment" | "word" | "char"
    pub timestamp_granularity: Option<String>,
    pub beam_size: Option<u32>,
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SttSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    #[serde(default)]
    pub id: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TranscribeResponse {
    pub text: String,
    #[serde(default)]
    pub segments: Vec<SttSegment>,
    #[serde(default)]
    pub words: Vec<SttSegment>,
    #[serde(default)]
    pub chars: Vec<SttSegment>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub preempted: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct HealthInfo {
    pub healthy: bool,
    pub model: Option<String>,
}

#[derive(Clone)]
pub struct SttClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

impl SttClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
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

    /// Hot-swap the service URL; see VisionClient::set_base_url.
    pub fn set_base_url(&self, base_url: Option<String>) {
        let mut guard = self.base_url.write().expect("stt base_url poisoned");
        *guard = base_url.map(|s| s.trim_end_matches('/').to_string());
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("stt base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    async fn acquire(&self) -> Result<(GpuSttGuard, Duration), Error> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_stt().await.ok_or(Error::Preempted)?;
        Ok((guard, start.elapsed()))
    }

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
        // Parakeet /health historically omits "model" — fall back to the
        // known Docker default so the UI has something to display.
        Ok(HealthInfo {
            healthy: true,
            model: raw
                .model
                .or_else(|| Some("parakeet-tdt-0.6b-v3".to_string())),
        })
    }

    pub async fn health(&self) -> Result<bool, Error> {
        Ok(self.health_info().await?.healthy)
    }

    fn apply_params(mut form: Form, params: &SttParams, cancel_url: Option<&str>) -> Form {
        if let Some(g) = &params.timestamp_granularity {
            form = form.text("timestamp_granularity", g.clone());
        }
        if let Some(b) = params.beam_size {
            form = form.text("beam_size", b.to_string());
        }
        if let Some(l) = &params.language {
            form = form.text("language", l.clone());
        }
        if let Some(c) = cancel_url {
            form = form.text("cancel_url", c.to_string());
        }
        form
    }

    /// Transcribe an uploaded audio blob.
    pub async fn transcribe_upload(
        &self,
        filename: &str,
        bytes: Bytes,
        params: &SttParams,
        cancel_url: Option<&str>,
    ) -> Result<(GpuSttGuard, TranscribeResponse, Duration), Error> {
        let (permit, gate_wait) = self.acquire().await?;
        let url = self.url("/v1/audio/transcriptions")?;
        let part = Part::bytes(bytes.to_vec()).file_name(filename.to_string());
        let mut form = Self::apply_params(Form::new().part("file", part), params, cancel_url);
        if let Some(g) = permit.chosen_gpu() {
            form = form.text("gpu", g.to_string());
        }
        let resp = self.http.post(url).multipart(form).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let parsed: TranscribeResponse = resp.json().await?;
        Ok((permit, parsed, gate_wait))
    }

    /// Transcribe a server-side file (bind-mounted into the parakeet container).
    pub async fn transcribe_file_path(
        &self,
        file_path: &str,
        params: &SttParams,
        cancel_url: Option<&str>,
    ) -> Result<(GpuSttGuard, TranscribeResponse, Duration), Error> {
        let (permit, gate_wait) = self.acquire().await?;
        let url = self.url("/v1/audio/transcriptions")?;
        let mut form = Self::apply_params(
            Form::new().text("file_path", file_path.to_string()),
            params,
            cancel_url,
        );
        if let Some(g) = permit.chosen_gpu() {
            form = form.text("gpu", g.to_string());
        }
        let resp = self.http.post(url).multipart(form).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let parsed: TranscribeResponse = resp.json().await?;
        Ok((permit, parsed, gate_wait))
    }
}
