//! HTTP client for longcat-avatar's `avatar_server.py` — the GPU "avatar"
//! engine (LongCat-Video-Avatar 1.5 via stable-diffusion.cpp fork).
//!
//! Unlike music (async job queue), the avatar supervisor renders synchronously
//! per request: `POST /generate` blocks for minutes and returns the WebM bytes
//! (or a path). Concurrent `/generate` → HTTP 429. The supervisor's
//! subprocess-per-render keeps idle VRAM ~9 MiB, but each request still pays a
//! ~3-5s model-load tax — that's the cost of the v0 supervisor; §3.4.a of the
//! GO-LIVE-PLAN replaces it with a warm-weights native server.
//!
//! koblem owns the orchestration in `routes/avatar.rs`: optional LLM script
//! drafting → TTS render → `acquire_for_avatar` (drain LLM + evict light
//! peers via `kob-gpu-gate`) → drive the supervisor → save → release.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kob_gpu_gate::{GpuAvatarGuard, GpuLock, HeavyKind, HeavyService};
use serde_json::Value;

/// `/generate` can take minutes (chained 10s clip ≈ 2 min @ lap 31.1 / 53f /
/// 8 steps on the 3060). No request timeout — koblem owns the wall via the
/// `acquire_for_avatar` cancel token. Connect timeout still guards a dead host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("avatar-server HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("avatar-server returned {0}: {1}")]
    Upstream(u16, String),
    #[error("avatar service not configured")]
    NotConfigured,
    #[error("avatar render preempted by TTS")]
    Preempted,
    #[error("avatar server response missing required field: {0}")]
    MissingField(&'static str),
}

/// Auto-registers the avatar service on the gate's heavy-slot registry when
/// `base_url` is set, so `acquire_for_avatar` knows where to send `/unload`
/// (the operator-disable side-effect, §6.5).
#[derive(Clone)]
pub struct AvatarClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

impl AvatarClient {
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
        };
        // Register on the gate so Avatar's heavy-slot disable can clear it
        // and so future heavies (Flux.2 Klein, §10.1) plug into the same shape.
        if let Some(url) = base_url.as_deref().filter(|s| !s.is_empty()) {
            me.gate.set_avatar_service(HeavyService {
                kind: HeavyKind::Avatar,
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
        *self.base_url.write().expect("avatar base_url poisoned") = normalised.clone();
        if let Some(url) = normalised {
            self.gate.set_avatar_service(HeavyService {
                kind: HeavyKind::Avatar,
                url,
                unload_path: "/v1/admin/unload".to_string(),
                drain_path: "/v1/admin/drain".to_string(),
            });
        }
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("avatar base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self
            .base_url
            .read()
            .expect("avatar base_url poisoned")
            .clone()
            .ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

    /// Acquire the GPU as the avatar consumer. Returns `None` if TTS has
    /// preempted or Avatar has been administratively disabled (§6.5).
    pub async fn acquire(&self) -> Option<(GpuAvatarGuard, Duration)> {
        let start = Instant::now();
        let guard = self.gate.acquire_for_avatar().await?;
        Some((guard, start.elapsed()))
    }

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

    /// `POST /v1/admin/unload {"force": true}` — SIGKILL any in-flight render.
    /// Idempotent: idle returns the supervisor's `{status: "idle"}`. Used by
    /// the `/api/v1/avatar/cancel` route (§10.2 of GO-LIVE-PLAN).
    pub async fn cancel_in_flight(&self) -> Result<Value, Error> {
        let url = self.url("/v1/admin/unload")?;
        let resp = self
            .http
            .post(url)
            .json(&serde_json::json!({"force": true}))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(Error::Upstream(status.as_u16(), body.to_string()));
        }
        Ok(body)
    }

    /// `POST /generate` — block until the render finishes; returns the WebM
    /// bytes plus parsed metadata. Sends `return: "bytes"` so the supervisor
    /// streams the body back instead of holding it in `/tmp/avatar-out`.
    pub async fn generate(&self, body: &Value) -> Result<AvatarRenderResult, Error> {
        let url = self.url("/generate")?;
        let mut body = body.clone();
        if let Value::Object(obj) = &mut body {
            obj.insert("return".to_string(), Value::String("bytes".to_string()));
        }
        let resp = self.http.post(url).json(&body).send().await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // The supervisor signals "draining" / "busy" as 429/503 — surface as
            // `Preempted` for symmetry with the music path.
            if status.as_u16() == 503 && body.contains("draining") {
                return Err(Error::Preempted);
            }
            return Err(Error::Upstream(status.as_u16(), body));
        }
        let bytes = resp.bytes().await?;
        // The supervisor stamps `X-Avatar-*` headers with the meta the JSON
        // path would return. Pull the bits the route handler needs.
        let take_header = |k: &str| -> Option<String> {
            headers
                .get(k)
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string())
        };
        let segments_rendered = take_header("X-Avatar-segments")
            .or_else(|| take_header("x-avatar-segments"))
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);
        let render_sec = take_header("X-Avatar-render_sec")
            .or_else(|| take_header("x-avatar-render_sec"))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let offload_used = take_header("X-Avatar-offload")
            .or_else(|| take_header("x-avatar-offload"))
            .map(|s| s.eq_ignore_ascii_case("true") || s == "True")
            .unwrap_or(true);
        Ok(AvatarRenderResult {
            bytes,
            segments_rendered,
            render_ms: (render_sec * 1000.0) as u64,
            offload_used,
        })
    }
}

/// What `generate` hands back to the route handler.
pub struct AvatarRenderResult {
    pub bytes: Bytes,
    pub segments_rendered: u32,
    pub render_ms: u64,
    pub offload_used: bool,
}
