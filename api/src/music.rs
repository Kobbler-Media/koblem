//! HTTP client for acestep.cpp's `ace-server` — the GPU "music" engine.
//!
//! ace-server is an async job queue: `POST /lm` and `POST /synth` return a job
//! id immediately, the single worker runs it, and the client polls `GET
//! /job?id=N` then fetches with `&result=1`. `POST /job?id=N&cancel=1` aborts a
//! job between LM/DiT steps — that's how koblem yields the GPU to TTS mid-render.
//!
//! This client acquires the shared [`GpuLock`] as the `music` consumer: TTS
//! preempts it, and it evicts STT/Vision/peer-TTS on acquire (its render needs
//! ~2.4-2.8 GB beside the always-resident LLM). Pipeline orchestration (LM →
//! synth, polling, save) lives in `routes/music.rs`; this module is the RPCs
//! plus the acquire and the TTS-aware poll loop.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use kob_gpu_gate::{GpuLock, GpuMusicGuard};
use reqwest::multipart::{Form, Part};
use serde_json::Value;

/// How often the poll loop checks job status and the TTS-preempt flag.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ace-server HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("ace-server returned {0}: {1}")]
    Upstream(u16, String),
    #[error("ace-server multipart parse failed: {0}")]
    Multipart(String),
    #[error("music service not configured")]
    NotConfigured,
    #[error("ace-server job {0} failed")]
    JobFailed(String),
    #[error("music render preempted by TTS")]
    Preempted,
}

/// Terminal/transient state of an ace-server job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

/// A decoded audio track fetched from a `/synth` result.
pub struct AudioPart {
    pub content_type: String,
    pub bytes: Bytes,
}

#[derive(Clone)]
pub struct MusicClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

impl MusicClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        // No request timeout: a synth job can take 10-12 s and we poll with our
        // own loop. Connect timeout guards against a dead host.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");
        let normalized = base_url.map(|s| s.trim_end_matches('/').to_string());
        // Self-register on the gate (like Flux) so a higher-priority
        // consumer (TTS) — or the peer songgen engine acquiring the music lane —
        // can SIGKILL-unload acestep's GPU worker via /v1/admin/unload instead of
        // waiting out its idle window.
        gate.set_music_service("acestep", normalized.clone(), "/v1/admin/unload");
        Self {
            http,
            base_url: Arc::new(RwLock::new(normalized)),
            gate,
        }
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        let normalized = base_url.map(|s| s.trim_end_matches('/').to_string());
        self.gate
            .set_music_service("acestep", normalized.clone(), "/v1/admin/unload");
        let mut guard = self.base_url.write().expect("music base_url poisoned");
        *guard = normalized;
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("music base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("music base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the GPU as the music consumer. Returns `None` if TTS has
    /// preempted (the caller should 503 and let the client retry).
    pub async fn acquire(&self) -> Option<(GpuMusicGuard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_music().await?;
        Some((guard, start.elapsed()))
    }

    /// Whether TTS has requested the GPU — polled during a render so we can
    /// cancel the ace job and drop the guard.
    pub fn is_tts_requested(&self) -> bool {
        self.gate.is_tts_requested()
    }

    /// `GET /props` — model lists + defaults. Returns the raw JSON.
    pub async fn props(&self) -> Result<Value, Error> {
        let url = self.url("/props")?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
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

    /// `POST /lm` — submit an LM (code-sketch) job. Returns the job id.
    pub async fn submit_lm(&self, body: &Value) -> Result<String, Error> {
        self.submit_json("/lm", body).await
    }

    async fn submit_json(&self, path: &str, body: &Value) -> Result<String, Error> {
        let url = self.url(path)?;
        let resp = self.http.post(url).json(body).send().await?;
        Self::parse_job_id(resp).await
    }

    /// `POST /synth`. With no audio attachments, sends the request as JSON
    /// (generate post-LM / passthrough). With a `source` (cover/repaint) and/or
    /// a `ref_audio` (timbre reference, any mode), sends multipart. `body` may be
    /// a single AceRequest or a JSON array (one per distinct song). Returns the
    /// job id.
    pub async fn submit_synth(
        &self,
        body: &Value,
        source: Option<(Bytes, String)>,
        ref_audio: Option<(Bytes, String)>,
    ) -> Result<String, Error> {
        if source.is_none() && ref_audio.is_none() {
            return self.submit_json("/synth", body).await;
        }
        let url = self.url("/synth")?;
        let mut form = Form::new().text("request", serde_json::to_string(body).unwrap_or_default());
        if let Some((bytes, name)) = source {
            form = form.part("audio", file_part(bytes, &name)?);
        }
        if let Some((bytes, name)) = ref_audio {
            form = form.part("ref_audio", file_part(bytes, &name)?);
        }
        let resp = self.http.post(url).multipart(form).send().await?;
        Self::parse_job_id(resp).await
    }

    /// `POST /understand` — submit a source song for the reverse pipeline
    /// (audio → caption + metadata + lyrics). Returns the job id.
    pub async fn submit_understand(
        &self,
        audio: Bytes,
        filename: &str,
        gpu: Option<&str>,
    ) -> Result<String, Error> {
        let url = self.url("/understand")?;
        let mut form = Form::new().part("audio", file_part(audio, filename)?);
        if let Some(g) = gpu {
            form = form.text("gpu", g.to_string());
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
        body.get("id")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .ok_or_else(|| Error::Upstream(502, format!("no job id in response: {body}")))
    }

    /// `POST /job?id=N&cancel=1`. Best-effort — logs nothing, swallows errors:
    /// it's called on the preempt/abort path where the job may already be done.
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
            _ => JobStatus::Running,
        })
    }

    /// Poll `job_id` until it's done. Between polls, check the TTS-preempt flag:
    /// when TTS wants the GPU, cancel the ace job and return [`Error::Preempted`]
    /// so the caller drops the guard and frees the write lock for TTS.
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
                JobStatus::Running => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    /// `GET /job?id=N&result=1` for an LM job — JSON array of enriched
    /// AceRequests (each carries the generated lyrics, metadata, audio_codes).
    pub async fn fetch_lm_result(&self, job_id: &str) -> Result<Vec<Value>, Error> {
        let url = self.url(&format!("/job?id={job_id}&result=1"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let body: Value = resp.json().await?;
        match body {
            Value::Array(a) => Ok(a),
            other => Ok(vec![other]),
        }
    }

    /// `GET /job?id=N&result=1` for a synth job — multipart/mixed of paired
    /// audio + latent parts. Returns just the audio parts (one per track).
    pub async fn fetch_synth_audio(&self, job_id: &str) -> Result<Vec<AudioPart>, Error> {
        let url = self.url(&format!("/job?id={job_id}&result=1"))?;
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
            .unwrap_or("")
            .to_string();
        // ace-server returns `multipart/mixed`, but `multer::parse_boundary`
        // only accepts `multipart/form-data` — extract the boundary ourselves
        // and hand it to multer, which parses the body regardless of subtype.
        let boundary = extract_boundary(&content_type).ok_or_else(|| {
            Error::Multipart(format!("no boundary in content-type {content_type:?}"))
        })?;

        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let mut mp = multer::Multipart::new(stream, boundary);
        let mut parts = Vec::new();
        while let Some(field) = mp
            .next_field()
            .await
            .map_err(|e| Error::Multipart(e.to_string()))?
        {
            let part_ct = field
                .content_type()
                .map(|m| m.to_string())
                .unwrap_or_default();
            let is_audio = part_ct.starts_with("audio/");
            let data = field
                .bytes()
                .await
                .map_err(|e| Error::Multipart(e.to_string()))?;
            // Keep audio parts; skip the paired latent (application/octet-stream).
            if is_audio {
                parts.push(AudioPart {
                    content_type: part_ct,
                    bytes: data,
                });
            }
        }
        if parts.is_empty() {
            return Err(Error::Multipart("no audio parts in synth result".into()));
        }
        Ok(parts)
    }

    /// `GET /job?id=N&result=1` for an understand job — multipart/mixed of one
    /// JSON part (the enriched AceRequest, as a 1-element array) + one latent
    /// part. Returns the parsed AceRequest JSON (caption, lyrics, bpm, …).
    pub async fn fetch_understand(&self, job_id: &str) -> Result<Value, Error> {
        let url = self.url(&format!("/job?id={job_id}&result=1"))?;
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
            .unwrap_or("")
            .to_string();
        let boundary = extract_boundary(&content_type).ok_or_else(|| {
            Error::Multipart(format!("no boundary in content-type {content_type:?}"))
        })?;
        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let mut mp = multer::Multipart::new(stream, boundary);
        while let Some(field) = mp
            .next_field()
            .await
            .map_err(|e| Error::Multipart(e.to_string()))?
        {
            let is_json = field
                .content_type()
                .map(|m| m.essence_str().starts_with("application/json"))
                .unwrap_or(false);
            let data = field
                .bytes()
                .await
                .map_err(|e| Error::Multipart(e.to_string()))?;
            if is_json {
                let v: Value = serde_json::from_slice(&data)
                    .map_err(|e| Error::Multipart(format!("bad understand JSON: {e}")))?;
                // The pipeline serialises a 1-element array `[AceRequest]`.
                return Ok(match v {
                    Value::Array(mut a) if !a.is_empty() => a.remove(0),
                    other => other,
                });
            }
        }
        Err(Error::Multipart("no json part in understand result".into()))
    }
}

/// Build a reqwest multipart file part from audio bytes + filename.
fn file_part(bytes: Bytes, filename: &str) -> Result<Part, Error> {
    Part::bytes(bytes.to_vec())
        .file_name(filename.to_string())
        .mime_str(guess_audio_mime(filename))
        .map_err(Error::Request)
}

/// Pull the `boundary=...` value out of a `multipart/*` Content-Type header.
/// Tolerates quotes and trailing parameters. Used instead of
/// `multer::parse_boundary`, which rejects anything but `multipart/form-data`.
fn extract_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        let part = part.trim();
        let rest = part
            .strip_prefix("boundary=")
            .or_else(|| part.strip_prefix("boundary ="))?;
        Some(rest.trim().trim_matches('"').to_string())
    })
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

#[cfg(test)]
mod tests {
    use super::extract_boundary;

    #[test]
    fn boundary_from_multipart_mixed() {
        // ace-server's actual response shape.
        assert_eq!(
            extract_boundary("multipart/mixed; boundary=ace-batch-boundary").as_deref(),
            Some("ace-batch-boundary")
        );
        // Quoted + form-data also work.
        assert_eq!(
            extract_boundary("multipart/form-data; boundary=\"xY z\"").as_deref(),
            Some("xY z")
        );
        // No boundary → None.
        assert_eq!(extract_boundary("application/json"), None);
    }
}
