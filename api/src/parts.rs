//! HTTP client for `/parts` — SAM 3 promptable concept segmentation, a THIRD
//! endpoint on the SAME `vision-server` binary that serves matting's `/remove`
//! and `/depth`.
//!
//! ONE process, ONE worker, ONE VRAM budget: exactly as in `depth.rs`, this
//! client acquires the gate's EXISTING `matting` budget line rather than a
//! `parts` line of its own. A separate line would let the placer put the families
//! on different cards, and since each submit carries the chosen card as a `gpu`
//! multipart field — which respawns the worker when it changes — alternating
//! requests would thrash one shared worker between cards. For the same reason it
//! does NOT register its URL on the gate: `MattingClient` already registers the
//! container so an eviction can SIGKILL the worker, and re-registering under the
//! same key would clobber that if the two URLs ever diverged.
//!
//! MANY PHRASES, ONE REQUEST. SAM 3 encodes the image once (~4.2 s on the local
//! Vulkan box) and then runs a cheap prompt-conditioned decode per phrase
//! (~1.0 s), so the one call takes a SLICE of phrases and there is deliberately
//! no single-prompt shape here or upstream: splitting the phrases across requests
//! pays the whole encode again each time.

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use kob_gpu_gate::GpuLock;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

pub use crate::depth::HealthInfo;

/// Sent explicitly rather than left to SAM 3's own 0.5, because a part inside an
/// object scores lower the less it stands out: measured on the 14-sprite corpus a
/// bear's forelimb is 0.67, a robot's side-bar arm 0.48 and a wizard's arm inside
/// a robe sleeve 0.41 — all three located exactly right, and the last two empty at
/// 0.5. `max_instances` set to the count the character sheet asked for is the
/// filter that works, not a score cut.
pub const DEFAULT_THRESHOLD: f32 = 0.35;
pub const DEFAULT_NMS: f32 = 0.1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parts HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("parts service returned {0}: {1}")]
    Upstream(u16, String),
    #[error("parts service not configured")]
    NotConfigured,
    #[error("parts service is administratively disabled")]
    Disabled,
}

#[derive(Debug, Default, Clone)]
pub struct PartsOptions {
    /// "cpu" | "gpu" (server default).
    pub backend: Option<String>,
    pub threshold: Option<f32>,
    pub nms: Option<f32>,
    /// Per-phrase cap; `Some(0)` and `None` both leave it uncapped.
    pub max_instances: Option<u32>,
}

impl PartsOptions {
    /// `backend` is the only knob upstream reads from the query string — the rest
    /// go through a helper that accepts either, so they ride as multipart text.
    fn query_pairs(&self) -> Vec<(&'static str, String)> {
        let mut q: Vec<(&'static str, String)> = Vec::new();
        if let Some(b) = self.backend.as_deref().filter(|s| !s.is_empty()) {
            q.push(("backend", b.to_string()));
        }
        q
    }

    fn text_fields(&self) -> Vec<(&'static str, String)> {
        let mut f = vec![
            (
                "threshold",
                self.threshold.unwrap_or(DEFAULT_THRESHOLD).to_string(),
            ),
            ("nms", self.nms.unwrap_or(DEFAULT_NMS).to_string()),
        ];
        if let Some(m) = self.max_instances {
            f.push(("max_instances", m.to_string()));
        }
        f
    }
}

/// One located instance. The mask is an 8-bit greyscale PNG (0 or 255) at the
/// INPUT resolution, and `name` is the phrase that found it.
#[derive(Debug, Deserialize)]
pub struct PartMask {
    pub name: String,
    pub score: f64,
    #[serde(rename = "box")]
    pub bbox: [f64; 4],
    pub area: u64,
    pub mask_png_base64: String,
}

#[derive(Debug, Deserialize)]
pub struct PartsResponse {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub prompts: Vec<String>,
    pub count: u32,
    pub parts: Vec<PartMask>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub encode_seconds: Option<f64>,
    /// One per prompt, in prompt order.
    #[serde(default)]
    pub decode_seconds: Vec<f64>,
    #[serde(default)]
    pub elapsed_seconds: Option<f64>,
    #[serde(default)]
    pub backend: Option<String>,
}

#[derive(Clone)]
pub struct PartsClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

impl PartsClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        // No timeout: the worker may cold-start and the gate may queue behind a
        // render. Matches the other GPU clients.
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            base_url: Arc::new(RwLock::new(normalize(base_url))),
            gate,
        }
    }

    /// Hot-swap the service URL (settings handler).
    pub fn set_base_url(&self, base_url: Option<String>) {
        let mut guard = self.base_url.write().expect("parts base_url poisoned");
        *guard = normalize(base_url);
    }

    pub fn base_url(&self) -> Option<String> {
        self.base_url
            .read()
            .expect("parts base_url poisoned")
            .clone()
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("parts base_url poisoned")
            .as_deref()
            .is_some_and(|s| !s.is_empty())
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self.base_url().ok_or(Error::NotConfigured)?;
        Ok(format!("{}{}", base, path))
    }

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

    /// One image, EVERY phrase. See the module doc: one encode, N cheap decodes.
    pub async fn parts(
        &self,
        image: (String, Bytes),
        prompts: &[String],
        opts: &PartsOptions,
    ) -> Result<PartsResponse, Error> {
        let url = self.url("/parts")?;

        // Held until the response is parsed so the reserved VRAM isn't freed
        // mid-inference. `None` = the shared vision-server line is
        // administratively disabled.
        let permit = self
            .gate
            .acquire_for_matting()
            .await
            .ok_or(Error::Disabled)?;

        let mut form = Form::new();
        // Multi-GPU placement: the gate-chosen card rides in as a `gpu` field.
        // Without it a disabled card is silently ignored.
        if let Some(g) = permit.chosen_gpu() {
            form = form.text("gpu", g.to_string());
        }
        form = form.text(
            "prompts",
            serde_json::to_string(prompts).unwrap_or_default(),
        );
        for (k, v) in opts.text_fields() {
            form = form.text(k, v);
        }
        let (name, bytes) = image;
        form = form.part("image", Part::bytes(bytes.to_vec()).file_name(name));

        let resp = self
            .http
            .post(url)
            .query(&opts.query_pairs())
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Upstream(status, body));
        }
        let parsed: PartsResponse = resp.json().await?;
        drop(permit);
        Ok(parsed)
    }
}

fn normalize(base_url: Option<String>) -> Option<String> {
    base_url
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `backend` is the only pair upstream reads from the query string; every
    /// other knob is a multipart field, and the threshold is SENT rather than
    /// left to SAM 3's own 0.5.
    #[test]
    fn the_knobs_split_between_the_query_string_and_the_form() {
        type Case<'a> = (
            PartsOptions,
            &'a [(&'a str, &'a str)],
            &'a [(&'a str, &'a str)],
        );
        let cases: &[Case] = &[
            (
                PartsOptions::default(),
                &[],
                &[("threshold", "0.35"), ("nms", "0.1")],
            ),
            (
                PartsOptions {
                    backend: Some("cpu".to_string()),
                    ..Default::default()
                },
                &[("backend", "cpu")],
                &[("threshold", "0.35"), ("nms", "0.1")],
            ),
            (
                PartsOptions {
                    backend: Some(String::new()),
                    threshold: Some(0.5),
                    nms: Some(0.2),
                    max_instances: Some(2),
                },
                &[],
                &[("threshold", "0.5"), ("nms", "0.2"), ("max_instances", "2")],
            ),
            (
                PartsOptions {
                    backend: Some("gpu".to_string()),
                    max_instances: Some(0),
                    ..Default::default()
                },
                &[("backend", "gpu")],
                &[
                    ("threshold", "0.35"),
                    ("nms", "0.1"),
                    ("max_instances", "0"),
                ],
            ),
        ];
        for (opts, query, fields) in cases {
            let got = opts.query_pairs();
            let got: Vec<(&str, &str)> = got.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert_eq!(&got[..], *query, "query for {opts:?}");
            let got = opts.text_fields();
            let got: Vec<(&str, &str)> = got.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert_eq!(&got[..], *fields, "fields for {opts:?}");
        }
    }

    /// Pins the upstream `/parts` contract this client deserialises.
    #[test]
    fn an_upstream_response_parses() {
        let raw = serde_json::json!({
            "width": 1024,
            "height": 1024,
            "prompts": ["arm", "leg"],
            "count": 2,
            "parts": [
                {"name": "arm", "score": 0.6, "box": [10.0, 20.0, 110.0, 220.0],
                 "area": 4096, "mask_png_base64": "iVBORw0="},
                {"name": "leg", "score": 0.79, "box": [1.0, 2.0, 3.0, 4.0],
                 "area": 8192, "mask_png_base64": "iVBORw1="},
            ],
            "model": "sam3",
            "encode_seconds": 4.2,
            "decode_seconds": [1.01, 0.98],
            "elapsed_seconds": 6.3,
            "backend": "vulkan",
        });
        let parsed: PartsResponse = serde_json::from_value(raw).expect("upstream shape must parse");
        assert_eq!(parsed.count, 2);
        assert_eq!(parsed.prompts, vec!["arm".to_string(), "leg".to_string()]);
        assert_eq!(parsed.parts[0].name, "arm");
        assert_eq!(parsed.parts[0].bbox, [10.0, 20.0, 110.0, 220.0]);
        assert_eq!(parsed.parts[1].area, 8192);
        assert_eq!(parsed.parts[1].mask_png_base64, "iVBORw1=");
        assert_eq!(parsed.decode_seconds, vec![1.01, 0.98]);
        assert_eq!(parsed.model.as_deref(), Some("sam3"));
    }

    /// Nothing located is an empty list, not an error — the phrases were still
    /// asked and answered.
    #[test]
    fn a_response_with_no_located_parts_parses() {
        let raw = serde_json::json!({
            "width": 8, "height": 8, "prompts": ["wing"], "count": 0, "parts": [],
            "model": "sam3", "encode_seconds": 4.2, "decode_seconds": [1.0],
            "elapsed_seconds": 5.2, "backend": "cpu",
        });
        let parsed: PartsResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.count, 0);
        assert!(parsed.parts.is_empty());
    }

    #[test]
    fn normalize_table() {
        let cases: &[(Option<&str>, Option<&str>)] = &[
            (None, None),
            (Some(""), None),
            (Some("   "), None),
            (
                Some("http://vision-server:8898/"),
                Some("http://vision-server:8898"),
            ),
            (Some(" http://h:1//"), Some("http://h:1")),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize(input.map(str::to_string)).as_deref(),
                *expected,
                "input: {input:?}"
            );
        }
    }
}
