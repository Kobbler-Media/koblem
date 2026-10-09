//! Typed client for the process-isolated SPAR3D + RigAnything image-to-rig
//! service. The supervisor owns its single-worker queue; Koblem owns admission
//! to the fleet-wide VRAM scheduler and durable gallery storage.

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use kob_gpu_gate::{GpuLock, ServiceKind};
use koblem_client::types::{ImageToRigRequest, RigSemanticPart, RigTextureMode};
use serde::{Deserialize, Serialize};

use crate::wire::{WireForm, WireMeter};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("SPAR3D HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("SPAR3D service returned {0}: {1}")]
    Upstream(u16, String),
    #[error("SPAR3D service not configured")]
    NotConfigured,
    #[error("SPAR3D service is administratively disabled")]
    Disabled,
    #[error("SPAR3D job failed: {0}")]
    JobFailed(String),
    #[error(
        "SPAR3D scheduler/service GPU mismatch: scheduler reserved {scheduled}, but the supervisor can only run on {actual}"
    )]
    GpuMismatch { scheduled: String, actual: String },
    #[error("SPAR3D scheduler reserved {scheduled}, but supervisor health named no GPU at all")]
    MissingGpuIdentity { scheduled: String },
}

#[derive(Clone)]
pub struct Spar3dClient {
    http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    /// What the last submit and the last result actually cost on the wire. See `wire.rs`.
    wire: WireMeter,
}

#[derive(Debug, Default, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub status: String,
    /// Where an un-targeted submit would run — the supervisor's own default,
    /// not a hard pin.
    #[serde(default)]
    pub gpu_uuid: Option<String>,
    /// Every card the supervisor will accept in `Submit::gpu`, which is the
    /// set its one-shot workers can actually load CUDA on.  Checked against the
    /// gate allocation before submitting, so a bad scheduler row cannot reserve
    /// a card the service cannot reach.  Empty from a supervisor predating
    /// relocation, where `gpu_uuid` alone is the whole set.
    #[serde(default)]
    pub gpus: Vec<String>,
}

impl Health {
    /// The cards this service can run on, preferring the field a relocating
    /// supervisor sends and falling back to the single-pin one it replaced.
    fn supported_gpus(&self) -> Vec<&str> {
        if self.gpus.is_empty() {
            self.gpu_uuid.as_deref().into_iter().collect()
        } else {
            self.gpus.iter().map(String::as_str).collect()
        }
    }
}

#[derive(Debug, Deserialize)]
struct Job {
    status: String,
    #[serde(default)]
    result_url: Option<String>,
    /// Independent LOD GLBs emitted only when requested.  The supervisor
    /// deliberately serves these as ordinary GLBs rather than a glTF
    /// extension, so Koblem can persist and bundle them unchanged.
    #[serde(default)]
    lod_result_urls: Vec<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    worker_wall_seconds: Option<f64>,
    #[serde(default)]
    metadata: serde_json::Value,
}

/// The knobs, without the picture.
///
/// §5a-a: the image travels as a multipart `image` part. It used to be an `image_base64` field
/// right here, on a hop where the route above had *already* received the bytes as a part and the
/// supervisor below writes them straight to a file — a base64 encode and decode that existed only
/// to move bytes between two of our own processes.
#[derive(Serialize)]
struct Submit<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    texture_mode: RigTextureMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_texture_resolution: Option<u32>,
    rig: bool,
    generate_lods: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_parts: Option<&'a [RigSemanticPart]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gpu: Option<&'a str>,
}

impl Spar3dClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        Self {
            http: reqwest::Client::builder()
                .build()
                .expect("SPAR3D HTTP client"),
            base_url: Arc::new(RwLock::new(
                base_url.map(|s| s.trim_end_matches('/').to_string()),
            )),
            gate,
            wire: WireMeter::new(),
        }
    }

    /// What the last submit and the last result cost, for instrumentation (§9.2).
    pub fn wire(&self) -> &WireMeter {
        &self.wire
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        *self.base_url.write().expect("spar3d base_url poisoned") =
            base_url.map(|s| s.trim_end_matches('/').to_string());
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("spar3d base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        self.base_url
            .read()
            .expect("spar3d base_url poisoned")
            .as_ref()
            .map(|base| format!("{base}{path}"))
            .ok_or(Error::NotConfigured)
    }

    pub async fn health(&self) -> Result<Health, Error> {
        let resp = self.http.get(self.url("/v1/health")?).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    /// Submit and wait. Holding the scheduler acquisition for the supervisor's
    /// queue wait is intentional: it prevents another Koblem workload from
    /// being admitted onto the 12 GB card while this job is about to load.
    pub async fn image_to_rig(
        &self,
        image: Bytes,
        req: &ImageToRigRequest,
    ) -> Result<(Bytes, Vec<Bytes>, Option<f64>, serde_json::Value), Error> {
        let permit = self
            .gate
            .acquire(ServiceKind::Spar3d)
            .await
            .ok_or(Error::Disabled)?;
        let gpu = permit.chosen_gpu();
        if let Some(scheduled) = gpu {
            // The supervisor now takes the card per request, so the check is
            // "can it reach what we reserved", not "is it already sitting
            // there".  `Submit::gpu` below carries the answer and the worker
            // narrows its own CUDA visibility to it before the first CUDA call.
            let health = self.health().await?;
            let supported = health.supported_gpus();
            if supported.is_empty() {
                return Err(Error::MissingGpuIdentity {
                    scheduled: scheduled.to_string(),
                });
            }
            if !supported.contains(&scheduled) {
                return Err(Error::GpuMismatch {
                    scheduled: scheduled.to_string(),
                    actual: supported.join(", "),
                });
            }
        }
        let body = Submit {
            seed: req.seed,
            texture_mode: req.texture_mode,
            source_texture_resolution: req.source_texture_resolution,
            rig: req.rig,
            generate_lods: req.generate_lods,
            semantic_parts: req.semantic_parts.as_deref(),
            gpu,
        };
        let form = WireForm::new()
            .json(
                "request",
                &serde_json::to_value(&body).unwrap_or(serde_json::Value::Null),
            )
            .bytes(
                "image",
                "input.png",
                "application/octet-stream",
                image.to_vec(),
            )
            .map_err(Error::Request)?;
        self.wire.submitted("spar3d", "image-to-rig", form.size());
        let resp = self
            .http
            .post(self.url("/v1/image-to-rig?wait=1")?)
            .multipart(form.into_form())
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let job: Job = resp.json().await?;
        if job.status != "completed" {
            return Err(Error::JobFailed(job.error.unwrap_or(job.status)));
        }
        let result_url = job
            .result_url
            .ok_or_else(|| Error::JobFailed("completed job omitted result_url".into()))?;
        if req.generate_lods && job.lod_result_urls.len() != 2 {
            return Err(Error::JobFailed(format!(
                "completed LOD job returned {} sidecars (expected 2)",
                job.lod_result_urls.len()
            )));
        }
        let resp = self.http.get(self.url(&result_url)?).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let glb = resp.bytes().await?;
        let mut lods = Vec::with_capacity(job.lod_result_urls.len());
        for lod_url in job.lod_result_urls {
            let resp = self.http.get(self.url(&lod_url)?).send().await?;
            if !resp.status().is_success() {
                return Err(Error::Upstream(
                    resp.status().as_u16(),
                    resp.text().await.unwrap_or_default(),
                ));
            }
            lods.push(resp.bytes().await?);
        }
        let returned: u64 = glb.len() as u64 + lods.iter().map(|l| l.len() as u64).sum::<u64>();
        self.wire.fetched_raw("spar3d", returned);
        Ok((glb, lods, job.worker_wall_seconds, job.metadata))
    }
}
