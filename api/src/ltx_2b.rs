//! HTTP client for the ltx-2b video-server — the GPU "ltx_2b" LTX-Video 0.9.8 2B engine
//! (the fast, video-only "fun toy").
//!
//! This is a SEPARATE container running the IDENTICAL ltx-video-server binary + async
//! `/ltx/v1/generate` job API as the LTX-2.3 `ltx_video` service, reached internally at
//! `http://ltx-2b:8080`. It reuses the LTX director request shape (segments, per-shot scene
//! i2v, keyframes, multi-scene, per-request frames/width/height/fps/steps/seed/…) but is a
//! SINGLE model: NO variants, NO relip/edit, NO audio-vae lip-sync, NO hires upscaler.
//!
//! Like ltx_video, the server is an async job queue: `POST /ltx/v1/generate` (multipart:
//! a `request` JSON part) returns a job id, the worker child renders the whole chain, and
//! the client polls `GET /sdcpp/v1/jobs/{id}` to completion. `POST /sdcpp/v1/jobs/{id}/cancel`
//! aborts a queued/in-flight render.
//!
//! This client acquires the shared [`GpuLock`] as the `ltx_2b` HEAVY consumer — same shape
//! as the ltx/avatar/flux/songgen slots: on acquire it drains the LLM and serialises against
//! the other heavies by VRAM budget (its ~10.5 GB peak can't co-reside with the LTX-2.3 DiT on
//! the 16 GB 5060 Ti, so the two serialise). koblem owns the orchestration (image resolve,
//! request build, persist) in `routes/ltx_2b.rs`; this module is the RPCs + acquire + poll loop.
//!
//! Self-registers the ltx_2b service on the gate's heavy-slot registry when `base_url` is
//! set, so the gate knows where to POST `/unload` and `/drain`.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, GpuLtx2bGuard, HeavyKind, HeavyService};
use serde_json::Value;

use crate::wire::{PartSupport, WireForm, WireMeter};

/// Connect timeout guards a dead host. No request timeout: a chain render runs for
/// minutes and we poll with our own loop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the poll loop checks job status.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Transient poll failures (a dropped connection, a brief 5xx) shouldn't abandon a long
/// render. Retry the poll this many consecutive times before giving up.
const MAX_POLL_RETRIES: u32 = 20;
/// Backoff between retried polls after a transient error.
const POLL_RETRY_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ltx-2b-server HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("ltx-2b-server returned {0}: {1}")]
    Upstream(u16, String),
    #[error("ltx_2b service not configured")]
    NotConfigured,
    #[error("ltx-2b-server job {0} failed: {1}")]
    JobFailed(String, String),
    #[error("ltx-2b-server job {0} was cancelled")]
    Cancelled(String),
    #[error("ltx-2b-server response missing required field: {0}")]
    MissingField(&'static str),
    #[error("ltx-2b-server job {0} returned undecodable media: {1}")]
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

/// What `generate` hands back to the route handler: the stitched movie.
pub struct Ltx2bRenderResult {
    /// The container bytes inlined as base64 (webm by default), when the engine chose to inline
    /// them. `None` when it did not — see `media_to_file`, which is how the bytes are obtained
    /// either way.
    pub video_b64: Option<String>,
    pub mime_type: String,
    pub output_format: String,
    pub fps: i64,
    pub frame_count: i64,
}

#[derive(Clone)]
pub struct Ltx2bClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    /// Job id of the render currently in flight, so the `/cancel` route can reach it.
    in_flight: Arc<RwLock<Option<String>>>,
    /// What the last submit and the last media fetch actually cost on the wire. See `wire.rs`.
    wire: WireMeter,
    /// Whether this engine build understands `part:<name>` media (§9.4).
    part_support: PartSupport,
}

impl Ltx2bClient {
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
            part_support: PartSupport::new(),
        };
        if let Some(url) = base_url.as_deref().filter(|s| !s.is_empty()) {
            me.gate.set_ltx2b_service(HeavyService {
                kind: HeavyKind::Ltx2b,
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
        *self.base_url.write().expect("ltx_2b base_url poisoned") = normalised.clone();
        // A different host is a different binary.
        self.part_support.reset();
        if let Some(url) = normalised {
            self.gate.set_ltx2b_service(HeavyService {
                kind: HeavyKind::Ltx2b,
                url,
                unload_path: "/v1/admin/unload".to_string(),
                drain_path: "/v1/admin/drain".to_string(),
            });
        }
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("ltx_2b base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("ltx_2b base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the GPU as the ltx_2b HEAVY consumer. Returns `None` if Ltx2b has been
    /// administratively disabled or another heavy currently holds the slot.
    pub async fn acquire(&self) -> Option<(GpuLtx2bGuard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_ltx2b().await?;
        Some((guard, start.elapsed()))
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
    ///
    /// The whole response body rides along because a resume's `resume_from` is published
    /// HERE and nowhere else — it is the only statement of how many banked shots survived,
    /// and the queue adapter's checkpoint and truncated-bank recovery both turn on it.
    pub async fn submit(
        &self,
        request: &Value,
        media_parts: &[(String, Bytes)],
    ) -> Result<(String, Value), Error> {
        let body = self.submit_raw(request, media_parts).await?;
        let job_id = body
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or(Error::MissingField("id"))?;
        *self.in_flight.write().expect("ltx_2b in_flight poisoned") = Some(job_id.clone());
        Ok((job_id, body))
    }

    /// `POST /ltx/v1/generate` — submit a chain render (multipart). Returns the response body.
    /// The 2B is video-only, so there are no per-segment audio parts — just the `request` JSON.
    async fn submit_raw(
        &self,
        request: &Value,
        media_parts: &[(String, Bytes)],
    ) -> Result<Value, Error> {
        let url = self.url("/ltx/v1/generate")?;
        // §9.4: markers only for an engine that advertises them; otherwise the bytes go back into
        // the document and this is the request every deployed engine has always taken.
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
        self.wire.submitted("ltx-2b", "generate", form.size());
        let resp = self
            .http
            .post(url)
            .multipart(form.into_form())
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
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
        self.wire.fetched_raw("ltx-2b", n);
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
        result: &Ltx2bRenderResult,
        dest: &std::path::Path,
    ) -> Result<u64, Error> {
        let Some(b64) = result.video_b64.as_deref() else {
            return self.fetch_media_to_file(server_job_id, dest).await;
        };
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            .map_err(|e| Error::BadMedia(server_job_id.to_string(), e.to_string()))?;
        self.wire.fetched_inline_b64("ltx-2b", bytes.len() as u64);
        tokio::fs::write(dest, &bytes)
            .await
            .map_err(|e| crate::wire::StreamError::Write {
                path: dest.display().to_string(),
                source: e,
            })?;
        Ok(bytes.len() as u64)
    }

    /// Does the engine take `part:<name>` media (§9.4)? See `LtxVideoClient::accepts_part_refs`.
    pub async fn accepts_part_refs(&self) -> bool {
        if let Some(known) = self.part_support.cached() {
            return known;
        }
        match self.health().await {
            Some(body) => self.part_support.observe(&body),
            None => false,
        }
    }

    /// What the last submit and the last media fetch cost, for the render log row (§9.2).
    pub fn wire(&self) -> &WireMeter {
        &self.wire
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
            .expect("ltx_2b in_flight poisoned")
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

    /// `POST /v1/admin/unload` — drop the resident DiT so the NEXT render loads FRESH.
    /// Best-effort: on failure we just proceed. Kept for parity with ltx_video's drain/unload
    /// paths (the 2B has a single model, so there is no variant swap to force a reload for).
    pub async fn unload(&self) -> Result<(), Error> {
        let url = self.url("/v1/admin/unload")?;
        self.http.post(url).send().await?;
        Ok(())
    }

    /// Poll a submitted render to completion, surviving transient poll failures (a dropped
    /// connection / brief 5xx). Clears the in-flight slot on exit. The caller
    /// [`submit`](Self::submit)s first to get `job_id`.
    pub async fn wait(&self, job_id: &str) -> Result<Ltx2bRenderResult, Error> {
        let _clear = InFlightClear {
            slot: self.in_flight.clone(),
        };
        let mut transient: u32 = 0;
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
                        "ltx_2b poll failed; retrying"
                    );
                    tokio::time::sleep(POLL_RETRY_BACKOFF).await;
                    continue;
                }
            };
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
fn parse_result(body: &Value) -> Result<Ltx2bRenderResult, Error> {
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
    // Absent `b64_json` is NOT an error. Inlining the render costs another third again in
    // base64 on a hop that already has a raw-bytes route for the same buffer (a stitched chain is 20-53 MB);
    // an engine configured not to inline it says so by omitting the field, and `media_to_file` fetches.
    let video_b64 = result
        .get("b64_json")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(Ltx2bRenderResult {
        video_b64,
        mime_type,
        output_format,
        fps,
        frame_count,
    })
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
}
