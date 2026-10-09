//! HTTP client for songgen.cpp's `songgen-server` — the heavier "music" engine
//! (LeVo2 SongGeneration), a co-equal alternative to acestep. The UI offers both
//! as tabs on the music page (acestep = lighter/quicker, songgen = LeVo2, longer,
//! with clone/continue from an audio prompt).
//!
//! songgen-server is an ASYNC job queue: `POST /generate|/clone|/continue` return
//! a `{job_id}` immediately, the single worker renders it, and the client polls
//! `GET /job?id=<id>` then fetches the wav with `&download=1` (streamed once).
//! `POST /job?id=<id>&cancel=1` (or `POST /unload`) SIGKILLs the forked render
//! child between steps — that's how koblem yields the GPU to TTS mid-render and
//! reclaims VRAM to true-0.
//!
//! Unlike acestep (the light `music` lane), songgen peaks ~7 GB, so it's an
//! on-demand HEAVY consumer like Avatar/Flux: [`acquire_for_songgen`] drains +
//! unloads the always-resident LLM and serialises against the other heavies on
//! the heavy mutex (it would not fit beside the resident LLM in the light lane).
//! It self-registers on the gate's heavy registry (`set_songgen_service`) so a
//! peer-heavy acquire or an operator disable force-unloads it via `/unload`.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, GpuSonggenGuard, HeavyKind, HeavyService};
use reqwest::multipart::{Form, Part};
use serde_json::Value;

/// How often the poll loop checks job status and the TTS-preempt flag.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Worker-kill endpoint the gate POSTs to force-unload songgen (SIGKILLs the
/// in-flight render child → true-0 VRAM).
pub const UNLOAD_PATH: &str = "/unload";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("songgen-server HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("songgen-server returned {0}: {1}")]
    Upstream(u16, String),
    #[error("songgen service not configured")]
    NotConfigured,
    #[error("songgen-server job {0} failed")]
    JobFailed(String),
    #[error("songgen render preempted by TTS")]
    Preempted,
}

/// Terminal/transient state of a songgen-server job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone)]
pub struct SonggenClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

impl SonggenClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        // No request timeout: a render is 140-280 s and we poll with our own
        // loop. Connect timeout guards against a dead host.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");
        let normalized = base_url.map(|s| s.trim_end_matches('/').to_string());
        // Self-register on the gate's heavy registry (like Avatar/Flux) so a
        // peer-heavy acquire or an operator disable force-unloads songgen's
        // render child via /unload (true-0 VRAM).
        register_songgen(&gate, normalized.as_deref());
        Self {
            http,
            base_url: Arc::new(RwLock::new(normalized)),
            gate,
        }
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalized = base_url.map(|s| s.trim_end_matches('/').to_string());
        register_songgen(&self.gate, normalized.as_deref());
        let mut guard = self.base_url.write().expect("songgen base_url poisoned");
        *guard = normalized;
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("songgen base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("songgen base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the GPU as the songgen HEAVY consumer: drains + unloads the LLM,
    /// evicts light peers, and serialises against Avatar/Flux. `None` = songgen is
    /// administratively disabled (the caller should 503).
    pub async fn acquire(&self) -> Option<(GpuSonggenGuard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_songgen().await?;
        Some((guard, start.elapsed()))
    }

    /// Whether TTS has requested the GPU — polled during a render so we cancel the
    /// songgen job and drop the guard.
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

    /// `POST /generate` (text→song). JSON body. Returns the job id.
    pub async fn submit_generate(&self, body: &Value) -> Result<String, Error> {
        let url = self.url("/generate")?;
        let resp = self.http.post(url).json(body).send().await?;
        Self::parse_job_id(resp).await
    }

    /// `POST /clone` or `/continue` — multipart with `vocal_stem` (+ `bgm_stem`,
    /// optional `full_mix`) audio files and the request fields as form text.
    /// Returns the job id.
    pub async fn submit_stems(
        &self,
        path: &str,
        fields: &[(String, String)],
        vocal_stem: Option<(Bytes, String)>,
        bgm_stem: Option<(Bytes, String)>,
        full_mix: Option<(Bytes, String)>,
    ) -> Result<String, Error> {
        let url = self.url(path)?;
        let mut form = Form::new();
        for (k, v) in fields {
            form = form.text(k.clone(), v.clone());
        }
        if let Some((bytes, name)) = vocal_stem {
            form = form.part("vocal_stem", file_part(bytes, &name)?);
        }
        if let Some((bytes, name)) = bgm_stem {
            form = form.part("bgm_stem", file_part(bytes, &name)?);
        }
        if let Some((bytes, name)) = full_mix {
            form = form.part("full_mix", file_part(bytes, &name)?);
        }
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
    /// when TTS wants the GPU, cancel the songgen job and return
    /// [`Error::Preempted`] so the caller drops the guard and frees it for TTS.
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

    /// `POST /separate` — htdemucs split of a full mix into vocal + bgm stems.
    /// SYNCHRONOUS (not a job): the server returns BOTH stems base64-encoded in a
    /// single JSON body `{vocal, bgm}`. This is the SAME deterministic separation
    /// a `/clone` of the same audio runs internally, exposed for debugging.
    /// Returns `(vocal_wav, bgm_wav)` as raw bytes.
    pub async fn separate(
        &self,
        mix: Bytes,
        name: &str,
        gpu: Option<&str>,
    ) -> Result<(Bytes, Bytes), Error> {
        use base64::Engine as _;
        let url = self.url("/separate")?;
        let mut form = Form::new().part("mix", file_part(mix, name)?);
        if let Some(g) = gpu {
            form = form.text("gpu", g.to_string());
        }
        let resp = self.http.post(url).multipart(form).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let body: Value = resp.json().await?;
        let decode = |key: &str| -> Result<Bytes, Error> {
            let s = body.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
                Error::Upstream(502, format!("missing `{key}` in /separate response"))
            })?;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| Error::Upstream(502, format!("bad base64 `{key}`: {e}")))?;
            Ok(Bytes::from(raw))
        };
        Ok((decode("vocal")?, decode("bgm")?))
    }
}

/// (Re)register songgen on the gate's heavy registry, or leave it unregistered
/// when no URL is set. songgen has no separate drain endpoint (only the LLM is
/// drained); the gate force-unloads it via `/unload` (SIGKILL the render child).
fn register_songgen(gate: &GpuLock, url: Option<&str>) {
    if let Some(url) = url.filter(|s| !s.is_empty()) {
        gate.set_songgen_service(HeavyService {
            kind: HeavyKind::Songgen,
            url: url.trim_end_matches('/').to_string(),
            unload_path: UNLOAD_PATH.to_string(),
            drain_path: UNLOAD_PATH.to_string(),
        });
    }
}

/// Build a reqwest multipart file part from audio bytes + filename.
fn file_part(bytes: Bytes, filename: &str) -> Result<Part, Error> {
    Part::bytes(bytes.to_vec())
        .file_name(filename.to_string())
        .mime_str(guess_audio_mime(filename))
        .map_err(Error::Request)
}

/// MIME for an uploaded stem file, from its extension.
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
