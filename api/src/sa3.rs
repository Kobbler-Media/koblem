//! HTTP client for sa3-server — the Stable Audio 3 port, a LIGHT "music" engine
//! offered alongside acestep/songgen. SA3 does text→audio (game SFX / short music)
//! plus audio editing: audio-to-audio (SDEdit), inpaint, and continuation.
//!
//! sa3-server is an ASYNC job queue (same shape as songgen-server): `POST /generate`
//! (t2a JSON) and `POST /edit` (a2a/inpaint/continuation multipart) return a
//! `{job_id}` immediately; the single worker renders it; the client polls
//! `GET /job?id=<id>` then fetches the wav with `&download=1` (streamed once).
//! `POST /job?id=<id>&cancel=1` (or `POST /unload`) SIGKILLs the forked render
//! child — that's how koblem yields the GPU to TTS mid-render and reclaims VRAM.
//!
//! Unlike songgen (HEAVY, ~7 GB), SA3 is LIGHT: all-Q8 weights ≈ 2.67 GB. It is
//! its OWN gate budget line (`ServiceKind::Sa3`, a small ~2.8 GB peak) — no longer
//! sharing acestep's slot — acquired via [`acquire_for_sa3`], so it co-resides
//! with other engines whenever its peak fits the card. It self-registers on the
//! gate's music registry ([`set_music_service`]) under the tag `sa3` so an
//! eviction or operator disable can force-unload its warm worker via `/unload`
//! (true-0 VRAM).

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, GpuMusicGuard};
use reqwest::multipart::{Form, Part};
use serde_json::Value;

/// How often the poll loop checks job status and the TTS-preempt flag.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Tag keying SA3 in the gate's music registry (peer of `acestep`).
const MUSIC_TAG: &str = "sa3";

/// Worker-kill endpoint the gate POSTs to force-unload SA3 (SIGKILLs the warm
/// render worker → true-0 VRAM).
pub const UNLOAD_PATH: &str = "/unload";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sa3-server HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("sa3-server returned {0}: {1}")]
    Upstream(u16, String),
    #[error("sa3 service not configured")]
    NotConfigured,
    #[error("sa3-server job {0} failed")]
    JobFailed(String),
    #[error("sa3 render preempted by TTS")]
    Preempted,
}

/// Terminal/transient state of a sa3-server job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone)]
pub struct Sa3Client {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

impl Sa3Client {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        // No request timeout: even a long render is bounded by our own poll loop.
        // Connect timeout guards against a dead host.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");
        let normalized = base_url.map(|s| s.trim_end_matches('/').to_string());
        // Self-register on the gate's music registry (peer of acestep) so TTS
        // preemption / an operator disable force-unloads SA3's warm worker via
        // /unload (true-0 VRAM) instead of waiting out its idle window.
        gate.set_music_service(MUSIC_TAG, normalized.clone(), UNLOAD_PATH);
        Self {
            http,
            base_url: Arc::new(RwLock::new(normalized)),
            gate,
        }
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalized = base_url.map(|s| s.trim_end_matches('/').to_string());
        self.gate
            .set_music_service(MUSIC_TAG, normalized.clone(), UNLOAD_PATH);
        let mut guard = self.base_url.write().expect("sa3 base_url poisoned");
        *guard = normalized;
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("sa3 base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("sa3 base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the GPU as the LIGHT music consumer. SA3 is its own budget line
    /// (`ServiceKind::Sa3`, a small ~2.8 GB peak) — no longer sharing acestep's
    /// slot — so it co-resides with other engines whenever its peak fits.
    /// `None` = SA3 administratively disabled (the caller should 503).
    pub async fn acquire(&self) -> Option<(GpuMusicGuard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_sa3().await?;
        Some((guard, start.elapsed()))
    }

    /// Whether TTS has requested the GPU — polled during a render so we cancel the
    /// sa3 job and drop the guard.
    pub fn is_tts_requested(&self) -> bool {
        self.gate.is_tts_requested()
    }

    /// `GET /health` — true on a 2xx with `status == "ok"`.
    pub async fn healthy(&self) -> bool {
        let Ok(url) = self.url("/health") else {
            return false;
        };
        match self.http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                body.get("status")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "ok")
                    .unwrap_or(true)
            }
            _ => false,
        }
    }

    /// `GET /health` → the build/version string, if reachable.
    pub async fn version(&self) -> Option<String> {
        let url = self.url("/health").ok()?;
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: Value = resp.json().await.ok()?;
        body.get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// `POST /generate` (text→audio, t2a). JSON body. Returns the job id.
    pub async fn submit_generate(&self, body: &Value) -> Result<String, Error> {
        let url = self.url("/generate")?;
        let resp = self.http.post(url).json(body).send().await?;
        Self::parse_job_id(resp).await
    }

    /// `POST /edit` (a2a / inpaint / continuation) — multipart with the `input`
    /// audio file and the request knobs (`mode`, `prompt`, …) as form text fields.
    /// Returns the job id.
    pub async fn submit_edit(
        &self,
        fields: &[(String, String)],
        input: (Bytes, String),
    ) -> Result<String, Error> {
        let url = self.url("/edit")?;
        let mut form = Form::new();
        for (k, v) in fields {
            form = form.text(k.clone(), v.clone());
        }
        let (bytes, name) = input;
        form = form.part("input", file_part(bytes, &name)?);
        let resp = self.http.post(url).multipart(form).send().await?;
        Self::parse_job_id(resp).await
    }

    async fn parse_job_id(resp: reqwest::Response) -> Result<String, Error> {
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let body: Value = resp.json().await?;
        body.get("job_id")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .ok_or_else(|| Error::Upstream(502, format!("no job_id in response: {body}")))
    }

    /// `POST /job?id=<id>&cancel=1`. Best-effort — swallows errors: called on the
    /// preempt/abort path where the job may already be done.
    pub async fn cancel(&self, job_id: &str) {
        if let Ok(url) = self.url(&format!("/job?id={job_id}&cancel=1")) {
            let _ = self.http.post(url).send().await;
        }
    }

    async fn poll_status(&self, job_id: &str) -> Result<JobStatus, Error> {
        let url = self.url(&format!("/job?id={job_id}"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let body: Value = resp.json().await?;
        Ok(match body.get("status").and_then(|v| v.as_str()) {
            Some("done") => JobStatus::Done,
            Some("failed") => JobStatus::Failed,
            Some("cancelled") => JobStatus::Cancelled,
            Some("pending") => JobStatus::Pending,
            _ => JobStatus::Running,
        })
    }

    /// Poll `job_id` until it's done. Between polls, check the TTS-preempt flag:
    /// when TTS wants the GPU, cancel the sa3 job and return [`Error::Preempted`]
    /// so the caller drops the guard and frees it for TTS.
    pub async fn await_job(&self, job_id: &str) -> Result<(), Error> {
        loop {
            if self.is_tts_requested() {
                self.cancel(job_id).await;
                return Err(Error::Preempted);
            }
            match self.poll_status(job_id).await? {
                JobStatus::Done => return Ok(()),
                JobStatus::Failed => return Err(Error::JobFailed(job_id.to_string())),
                JobStatus::Cancelled => return Err(Error::Preempted),
                JobStatus::Pending | JobStatus::Running => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    /// `GET /job?id=<id>&download=1` — the rendered wav, streamed exactly once
    /// after the job is `done`.
    pub async fn download(&self, job_id: &str) -> Result<(String, Bytes), Error> {
        let url = self.url(&format!("/job?id={job_id}&download=1"))?;
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
            .unwrap_or("audio/wav")
            .to_string();
        let bytes = resp.bytes().await?;
        Ok((content_type, bytes))
    }
}

/// Build a reqwest multipart file part from audio bytes + filename.
fn file_part(bytes: Bytes, filename: &str) -> Result<Part, Error> {
    Part::bytes(bytes.to_vec())
        .file_name(filename.to_string())
        .mime_str(guess_audio_mime(filename))
        .map_err(Error::Request)
}

/// MIME for an uploaded source-audio file, from its extension.
fn guess_audio_mime(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".mp3") {
        "audio/mpeg"
    } else if lower.ends_with(".flac") {
        "audio/flac"
    } else if lower.ends_with(".ogg") || lower.ends_with(".opus") {
        "audio/ogg"
    } else {
        "audio/wav"
    }
}
