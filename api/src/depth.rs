//! HTTP client for the depth service — Depth-Anything V2 (the default) or
//! Depth-Anything 3, wrapped in `vision-server`, the same binary that serves
//! matting's `/remove`.
//!
//! ONE process, ONE worker, ONE VRAM budget: `/depth` and `/remove` are two
//! endpoints on the same container, so this client deliberately acquires the
//! gate's EXISTING `matting` budget line rather than a `depth` line of its own.
//! A separate line would let the placer put the two families on different cards,
//! and since each submit carries the chosen card as a `gpu` multipart field —
//! which respawns the worker when it changes — alternating requests would thrash
//! one shared worker between cards. Sharing the line also refcount-shares the
//! placement, which is what "one process" actually means. DA3's delta is ~300 MiB
//! at native 504 and V2's is smaller still (measured on UMA/Vulkan), well inside
//! the peak `matting` already reserves.
//!
//! For the same reason this client does NOT register its URL on the gate:
//! `MattingClient` already registers the container so an eviction can SIGKILL the
//! worker, and re-registering under the same key would clobber that if the two
//! URLs ever diverge (`main` warns when they do).

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use kob_gpu_gate::GpuLock;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};

/// Errors from DepthClient calls.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("depth HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("depth service returned {0}: {1}")]
    Upstream(u16, String),
    #[error("depth service not configured")]
    NotConfigured,
    #[error("depth service is administratively disabled")]
    Disabled,
}

/// Which depth family serves the request. V2 is the server's default — on flat-lit
/// matted sprites it resolves 4-10x more relief than DA3-Small; DA3 stays reachable
/// for photographic callers and is the only one with a confidence map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum DepthModel {
    #[serde(rename = "depth-anything-v2", alias = "v2")]
    V2,
    #[serde(rename = "depth-anything-3", alias = "v3", alias = "3")]
    V3,
}

impl DepthModel {
    pub fn as_str(self) -> &'static str {
        match self {
            DepthModel::V2 => "depth-anything-v2",
            DepthModel::V3 => "depth-anything-3",
        }
    }
}

/// Knobs forwarded to `vision-server`'s `/depth` query string. `None` leaves the
/// server default in place.
///
/// There is deliberately no single `process_res`: the two families do not agree
/// on what an inference resolution is. V2's knob is a FLOOR on the SHORTEST side
/// (native 518, and it never downscales below the input); DA3's is a BOUND on the
/// LONGEST side (native 504). Sending the axis the chosen model does not
/// implement is a 400 upstream, which is the point — one name for both would
/// silently mean two different pictures.
#[derive(Debug, Default, Clone)]
pub struct DepthOptions {
    /// "cpu" | "gpu" (server default).
    pub backend: Option<String>,
    /// `None` takes the server default, which is V2.
    pub model: Option<DepthModel>,
    /// depth-anything-v2 only — floor on the SHORTEST side.
    pub process_res_short: Option<u32>,
    /// depth-anything-3 only — bound on the LONGEST side.
    pub process_res_long: Option<u32>,
}

impl DepthOptions {
    /// Query-string pairs for `/depth`. No `format` pair: unlike `/remove`, the
    /// reply is JSON regardless.
    fn query_pairs(&self) -> Vec<(&'static str, String)> {
        let mut q: Vec<(&'static str, String)> = Vec::new();
        if let Some(b) = self.backend.as_deref().filter(|s| !s.is_empty()) {
            q.push(("backend", b.to_string()));
        }
        if let Some(m) = self.model {
            q.push(("model", m.as_str().to_string()));
        }
        if let Some(v) = self.process_res_short {
            q.push(("process_res_short", v.to_string()));
        }
        if let Some(v) = self.process_res_long {
            q.push(("process_res_long", v.to_string()));
        }
        q
    }
}

#[derive(Clone)]
pub struct DepthClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
}

/// One result from `/depth`. Every PNG is 16-bit greyscale at the INPUT
/// resolution, min/max normalised — a raw value is
/// `min + (px / 65535) * (max - min)`, so the bounds travel with the pixels or
/// the image is meaningless.
///
/// The confidence triple is ABSENT, not defaulted, when the model has no
/// confidence head — V2 does not. A caller that needs a blend weight derives one
/// (the alpha distance field is what DA3's confidence tracked anyway) rather than
/// reading a fabricated constant as a model that was uniformly sure.
#[derive(Debug, Deserialize)]
pub struct DepthResult {
    pub width: u32,
    pub height: u32,
    pub depth_png_base64: String,
    pub depth_min: f64,
    pub depth_max: f64,
    #[serde(default)]
    pub confidence_png_base64: Option<String>,
    #[serde(default)]
    pub confidence_min: Option<f64>,
    #[serde(default)]
    pub confidence_max: Option<f64>,
}

/// `/depth` JSON response.
///
/// `depth_polarity` is load-bearing and the two models disagree on it:
/// `"disparity"` (V2 — larger is NEARER) or `"distance"` (DA3 — larger is
/// FARTHER). A consumer that ignores it reads every sprite inside out.
#[derive(Debug, Deserialize)]
pub struct DepthResponse {
    pub count: u32,
    pub results: Vec<DepthResult>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub depth_polarity: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub elapsed_seconds: Option<f64>,
}

/// `/health` response. `vision-server` serves one health doc for every family it
/// hosts, so this mirrors the matting shape.
#[derive(Debug, Default, Deserialize)]
pub struct HealthInfo {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub worker_alive: bool,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub in_flight: Option<i64>,
    /// koblem-side liveness flag — true when the probe reached the service.
    #[serde(default)]
    pub healthy: bool,
}

impl DepthClient {
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
        let mut guard = self.base_url.write().expect("depth base_url poisoned");
        *guard = normalize(base_url);
    }

    pub fn base_url(&self) -> Option<String> {
        self.base_url
            .read()
            .expect("depth base_url poisoned")
            .clone()
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("depth base_url poisoned")
            .as_deref()
            .is_some_and(|s| !s.is_empty())
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        let base = self.base_url().ok_or(Error::NotConfigured)?;
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

    /// Estimate depth for a batch of images via repeated multipart `images`
    /// fields. N images in, N results out.
    pub async fn depth_batch(
        &self,
        images: &[(String, Bytes)],
        opts: &DepthOptions,
    ) -> Result<DepthResponse, Error> {
        let url = self.url("/depth")?;

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
        for (name, bytes) in images {
            let part = Part::bytes(bytes.to_vec()).file_name(name.clone());
            form = form.part("images", part);
        }

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
        let parsed: DepthResponse = resp.json().await?;
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

    #[test]
    fn query_pairs_table() {
        let cases: &[(DepthOptions, &[(&str, &str)])] = &[
            (DepthOptions::default(), &[]),
            (
                DepthOptions {
                    backend: Some("cpu".to_string()),
                    ..Default::default()
                },
                &[("backend", "cpu")],
            ),
            (
                DepthOptions {
                    backend: Some(String::new()),
                    process_res_short: Some(1024),
                    ..Default::default()
                },
                &[("process_res_short", "1024")],
            ),
            (
                DepthOptions {
                    backend: Some("gpu".to_string()),
                    model: Some(DepthModel::V3),
                    process_res_long: Some(504),
                    ..Default::default()
                },
                &[
                    ("backend", "gpu"),
                    ("model", "depth-anything-3"),
                    ("process_res_long", "504"),
                ],
            ),
            (
                DepthOptions {
                    model: Some(DepthModel::V2),
                    process_res_short: Some(518),
                    ..Default::default()
                },
                &[("model", "depth-anything-v2"), ("process_res_short", "518")],
            ),
        ];
        for (opts, expected) in cases {
            let got = opts.query_pairs();
            let got: Vec<(&str, &str)> = got.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert_eq!(&got[..], *expected, "opts: {opts:?}");
        }
    }

    /// V2 has no confidence head, and the absence is the wire shape — not a
    /// zero, not a one. A caller that blends on it has to see that it is gone.
    #[test]
    fn a_v2_response_parses_with_no_confidence_at_all() {
        let raw = serde_json::json!({
            "count": 1,
            "results": [{
                "width": 4, "height": 4,
                "depth_png_base64": "AA==",
                "depth_min": 0.0, "depth_max": 1.0,
            }],
            "model": "depth-anything-v2",
            "depth_polarity": "disparity",
            "backend": "vulkan",
        });
        let parsed: DepthResponse = serde_json::from_value(raw).expect("v2 shape must parse");
        assert_eq!(parsed.model.as_deref(), Some("depth-anything-v2"));
        assert_eq!(parsed.depth_polarity.as_deref(), Some("disparity"));
        let r = &parsed.results[0];
        assert!(r.confidence_png_base64.is_none());
        assert!(r.confidence_min.is_none());
        assert!(r.confidence_max.is_none());
    }

    #[test]
    fn model_names_round_trip_through_the_query_string() {
        for (m, s) in [
            (DepthModel::V2, "depth-anything-v2"),
            (DepthModel::V3, "depth-anything-3"),
        ] {
            assert_eq!(m.as_str(), s);
            assert_eq!(serde_json::to_value(m).unwrap(), serde_json::json!(s));
            assert_eq!(
                serde_json::from_value::<DepthModel>(serde_json::json!(s)).unwrap(),
                m
            );
        }
        assert_eq!(
            serde_json::from_value::<DepthModel>(serde_json::json!("v2")).unwrap(),
            DepthModel::V2
        );
        assert_eq!(
            serde_json::from_value::<DepthModel>(serde_json::json!("v3")).unwrap(),
            DepthModel::V3
        );
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
