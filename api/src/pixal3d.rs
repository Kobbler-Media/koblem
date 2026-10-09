//! Typed client for **Pixal3D** — the `longcat-rig` `avatar_server` on :8102.
//!
//! Photo → a rigged LOD ladder → prompted animation, as three separately re-runnable stages behind
//! one process. Overlaps `spar3d` in purpose and shares nothing with it in implementation, which is
//! why it is its own client, its own budget line and its own tab.
//!
//! # The shape that decides everything above this file
//!
//! | stage | measured | how often |
//! |---|---|---|
//! | `POST /v1/rig` | ~450 s for the first tier, ~150 s each after | once per character |
//! | `POST /v1/motion` | ~11 s warm, ~33 s cold (21 s of that is loading the DiT) | per prompt |
//! | `POST /v1/animate` | **0.22 s**, CPU-only | per take |
//!
//! ⇒ rigging is a job, animating is an interactive loop. [`Pixal3dClient::acquire`] is taken for
//! rig and motion and **deliberately NOT for animate**: the service itself refuses to put a 0.22 s
//! retarget behind a 450 s rig, and a gate wait here would undo that on koblem's side.
//!
//! # 🔴 Two properties of the service that koblem has to design around
//!
//! 1. **Job RECORDS are evicted at 512; the artifacts on disk are never swept.** But
//!    `GET /v1/jobs/{id}/result` reads the record, so once it is gone the GLB is unreachable over
//!    HTTP even though it still exists. koblem therefore **fetches every delivered tier to its own
//!    disk as soon as the job completes** ([`Pixal3dClient::fetch_result_to`]) rather than
//!    streaming it through on demand, and stores `stage_dir` / `rig_cache` / the clip's
//!    `result_path` — which are plain paths on a volume and outlive the record — as the re-run
//!    handles in preference to the job id.
//! 2. **`status: "completed"` with `gate.accepted: false` is normal.** The gates are advisory. The
//!    one hard failure is a rig with zero skin weights, which the service refuses on its own. This
//!    client never inspects a gate to decide success; it reports the verdict and hands over the
//!    asset.
//!
//! ⚠️ Every POST carries a body, including the cancels. `thirdparty/httplib.h` blocks forever in
//! its oversize guard on a POST with no `Content-Length` — measured 0.4 ms with a body against
//! still hung past ten minutes without one, holding one of the server's 64 worker threads.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use kob_gpu_gate::{Acquisition, GpuLock, ServiceKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::wire::{WireForm, WireMeter};

/// The result fetch moves a whole GLB — a hero tier is tens of MB — over loopback. Generous, but
/// bounded: without it a half-open socket would wedge a job at "collecting" forever.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// Submits and polls are a few hundred bytes each and answer immediately (the service returns 202
/// and does the work in a thread), so a short timeout here is a liveness check, not a deadline on
/// the render.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pixal3d HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("pixal3d returned {0}: {1}")]
    Upstream(u16, String),
    #[error("pixal3d is not configured")]
    NotConfigured,
    #[error("pixal3d job failed: {0}")]
    JobFailed(String),
    #[error("writing {0}: {1}")]
    Io(String, String),
    /// The scheduler reserved a card the service cannot place a worker on. Refused rather than
    /// submitted: the rig would run on whatever card the engine defaults to while the gate held
    /// ~11 GB against a different one, so BOTH cards' accounting would be wrong and the one
    /// actually used would never have been checked against the floor.
    #[error(
        "the GPU scheduler reserved {scheduled} for pixal3d, but the rig service can only run on \
         {actual}. Fix the pixal3d row in GPU Scheduler, or give the container that card in \
         docker-compose.override.yml (LONGCAT_RIG_GPUS + the device reservation)."
    )]
    GpuMismatch { scheduled: String, actual: String },
    /// The service reports no card set at all — an engine predating placement, or one whose
    /// `LONGCAT_RIG_GPUS` is a single pin. Nothing to verify against, so a scheduled card cannot be
    /// honoured and must not be assumed.
    #[error(
        "the GPU scheduler reserved {scheduled} for pixal3d, but the rig service reports no \
         placeable cards — it is pinned to whatever card its container was given. Either redeploy \
         it with LONGCAT_RIG_GPUS set, or point the pixal3d scheduler row at its pinned card."
    )]
    MissingGpuIdentity { scheduled: String },
}

// ── health ───────────────────────────────────────────────────────────────────

/// `GET /health`, which answers without touching the GPU and while the weights are unloaded — so
/// it is a real liveness probe, not a "did a render finish" probe.
///
/// 🔴 The four busy-ish states are separate on purpose and the UI must show them separately.
/// `waiting_for_gpu_lock` means **another process on this box owns the card** while this service
/// looks idle; reporting that as "queued" would send the user looking in the wrong place.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Health {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub draining: bool,
    #[serde(default)]
    pub in_flight: i64,
    #[serde(default)]
    pub queued: i64,
    #[serde(default)]
    pub jobs_queued: i64,
    #[serde(default)]
    pub jobs_running: i64,
    /// We hold our slot but another process on this host holds the card.
    #[serde(default)]
    pub waiting_for_gpu_lock: bool,
    /// Whether the motion DiT is resident. `false` ⇒ the next motion pays the ~21 s load, which is
    /// the whole difference between the warm ~11 s loop and the ~33 s first prompt of a session.
    #[serde(default)]
    pub loaded: bool,
    #[serde(default)]
    pub idle_unload_seconds: i64,
    #[serde(default)]
    pub min_free_mib: i64,
    #[serde(default)]
    pub rig_retries_default: i64,
    #[serde(default)]
    pub worker_isolation: bool,
    #[serde(default)]
    pub fatal: String,
    /// Every card the supervisor will accept in a submit's `gpu` field — the set its worker can
    /// actually load CUDA on. Checked against the gate allocation before submitting, so a bad
    /// scheduler row cannot reserve a card the service cannot reach. **Empty from an engine
    /// predating placement**, where the container pin is the whole story.
    #[serde(default)]
    pub gpus: Vec<String>,
    /// Where an un-targeted submit would run: the supervisor's own default, not a hard pin.
    #[serde(default)]
    pub gpu_uuid: Option<String>,
    /// Where the CUDA worker actually is right now, `None` when none is up. Distinct from
    /// `gpu_uuid` on purpose — "where would it go" is not evidence of "where is it".
    #[serde(default)]
    pub worker_gpu: Option<String>,
}

impl Health {
    pub fn ok(&self) -> bool {
        self.status == "ok"
    }

    /// The cards this service can run on, preferring the set a relocating supervisor publishes and
    /// falling back to the single default it replaced. Mirrors `spar3d::Health::supported_gpus`.
    pub fn supported_gpus(&self) -> Vec<&str> {
        if self.gpus.is_empty() {
            self.gpu_uuid.as_deref().into_iter().collect()
        } else {
            self.gpus.iter().map(String::as_str).collect()
        }
    }
}

// ── the job record ───────────────────────────────────────────────────────────

/// `GET /v1/jobs/{id}`.
///
/// ⚠️ **There is no progress field, and none is invented here.** `status` moves
/// queued → running → completed|failed|cancelled and that is the whole vocabulary; the stage
/// transitions the library logs never reach the HTTP surface. A progress display upstream has to
/// be built from elapsed time against the measured expectations, not from this record.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EngineJob {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub started_at: f64,
    #[serde(default)]
    pub completed_at: f64,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub result_path: Option<String>,
    #[serde(default)]
    pub result_mime: Option<String>,
    #[serde(default)]
    pub result: Value,
}

impl EngineJob {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "failed" | "cancelled" | "error"
        )
    }
    pub fn completed(&self) -> bool {
        self.status == "completed"
    }
}

/// One tier of the ladder, off `result.tiers[]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EngineTier {
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub decimate: i64,
    /// The GLB's path *inside the container*. Durable — unlike the job record.
    #[serde(default)]
    pub glb: String,
    #[serde(default)]
    pub joints: i64,
    #[serde(default)]
    pub named_core: i64,
    #[serde(default)]
    pub seconds: f64,
    /// `""` when the cleanup did not run; otherwise its one-line verdict, including the case where
    /// it found nothing to improve and kept the original skin byte-for-byte.
    #[serde(default)]
    pub skin_cleanup: String,
    #[serde(default)]
    pub gate: EngineGate,
}

/// The QC verdict for the draw this tier decoded.
///
/// 🔴 **A tier with `draws == 0` inherited the skeleton and ran no gate**, so every field here is
/// unpopulated for it — `skin_ok: false`, `named_core: 0`, an empty summary. Rendering that block
/// reports a failure that did not happen. The tier-level `joints`/`named_core` are the true,
/// shared values. [`EngineGate::ran`] is the guard.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EngineGate {
    #[serde(default)]
    pub draws: i64,
    #[serde(default)]
    pub accepted: bool,
    #[serde(default)]
    pub skin_ok: bool,
    #[serde(default)]
    pub humanoid_gate_ok: bool,
    #[serde(default)]
    pub generic_gate_ok: bool,
    #[serde(default)]
    pub named_core: i64,
    #[serde(default)]
    pub pose_gate_ran: bool,
    #[serde(default)]
    pub pose_gate_pass: bool,
    #[serde(default)]
    pub pose_gate_worst: f64,
    #[serde(default)]
    pub summary: String,
}

impl EngineGate {
    /// Whether this gate block describes work that actually happened.
    pub fn ran(&self) -> bool {
        self.draws > 0
    }
}

// ── requests ─────────────────────────────────────────────────────────────────

/// `POST /v1/rig`. Every field is optional on the wire; the service owns the defaults and koblem
/// does not keep a second copy of them.
///
/// 🔴 **A retry MUST move `rig_seed`.** The rig decode is a deterministic function of (mesh, seed),
/// so re-submitting an unchanged request against a cached geometry returns the draw the user just
/// rejected, 205 s later. `geo_seed` redraws the mesh instead and pays the full ~250 s geometry
/// stage — a genuinely different character, because the same photo produces a different mesh on
/// every process anyway (an open bug in the engine, which happens to make this a real fix).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RigSubmit {
    /// 🔴 The engine-side PATH of the source image, for a re-run that sends no file.
    ///
    /// It is not optional in the way it looks: `resolve_reruns` back-fills the image from the
    /// source job's record only when `geometry_from` was given as a **job id**. koblem passes the
    /// stage dir as a path (records are evicted at 512), so it owes the image path too — without
    /// it a re-rig is a 400 saying there is no image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `hero` | `high` | `medium` | `game`. Omitted ⇒ all four (~960 s).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rig_seed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geo_seed: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rig_retries: Option<u32>,
    /// Opt-in `--resolution`. 0/absent is the default 1024 lattice; **1536 is the only other value
    /// with any measured result and it is per-subject** — better hands on some characters, a worse
    /// rig on others. Must be a multiple of 16 in 32..=2032 or the service 400s at submit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub texsize: Option<u32>,
    /// Defaults to true engine-side, and it is what removes the "spears off the arms" defect. Only
    /// sent when the caller is turning it OFF.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skin_cleanup: Option<bool>,
    /// Re-bake **and re-rig** an existing geometry (~205 s, no diffusion). A job id or, preferred,
    /// the `stage_dir` path — which survives record eviction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometry_from: Option<String>,
    /// Keep the skeleton, re-bake only (~134 s, `draws: 0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rig_cache_from: Option<String>,
    /// 🔴 THE CARD THE SCHEDULER RESERVED, as a GPU UUID. Set from `Acquisition::chosen_gpu` at
    /// submit, never by the caller — the gate is the only party that can see what the other
    /// services have reserved. The engine relocates its CUDA worker onto this card (kill +
    /// respawn) before the rig starts, and refuses with a 400 if it cannot reach it.
    ///
    /// It rides in the JSON body rather than as its own multipart part **deliberately**: koblem
    /// sends that body through `json_file`, which carries a filename, and cpp-httplib drops a
    /// filename-less part on the floor. A bare `Form::text("gpu", …)` here would be ignored in
    /// silence and the rig would run on the default card while the gate believed otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
}

/// `POST /v1/motion`. The seed is a real RNG seed, so the same prompt with a new seed is a
/// genuinely different take — which is the whole re-roll loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionSubmit {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cfg: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The scheduler's card, as for [`RigSubmit::gpu`]. A motion is far under the rig's floor, so
    /// the engine will not MOVE a resident worker for one — this only decides where a cold worker
    /// starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
}

/// `POST /v1/animate`. CPU-only and ~0.22 s, so it takes no GPU permit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimateSubmit {
    /// A rig job id, or a GLB path **the container can see**. koblem sends a path: an edited rig
    /// lives in the shared staging directory, and a path outlives the job record.
    pub rig: String,
    /// Only meaningful when `rig` is a job id — a path has already chosen its tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Motion clip paths (preferred), motion job ids, or clip JSON.
    pub clips: Vec<String>,
    /// `fix` | `nofix` | `base` | `rot` | `rotbase`. Absent ⇒ the service's `rotbase`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smooth: Option<u32>,
    /// The `skeleton-exercise` track — swings every materially weighted joint in turn. On by
    /// default engine-side, and it is the honest "does this rig work at all" preview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exercise: Option<bool>,
}

// ── the client ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Pixal3dClient {
    http: reqwest::Client,
    fetch_http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    gate: GpuLock,
    wire: WireMeter,
}

impl Pixal3dClient {
    pub fn new(base_url: Option<String>, gate: GpuLock) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(CONTROL_TIMEOUT)
                .build()
                .expect("pixal3d control client"),
            fetch_http: reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .build()
                .expect("pixal3d fetch client"),
            base_url: Arc::new(RwLock::new(
                base_url.map(|s| s.trim_end_matches('/').to_string()),
            )),
            gate,
            wire: WireMeter::new(),
        }
    }

    pub fn set_base_url(&self, base_url: Option<String>) {
        *self.base_url.write().expect("pixal3d base_url poisoned") =
            base_url.map(|s| s.trim_end_matches('/').to_string());
    }

    pub fn is_configured(&self) -> bool {
        self.base_url
            .read()
            .expect("pixal3d base_url poisoned")
            .is_some()
    }

    fn url(&self, path: &str) -> Result<String, Error> {
        self.base_url
            .read()
            .expect("pixal3d base_url poisoned")
            .as_ref()
            .map(|base| format!("{base}{path}"))
            .ok_or(Error::NotConfigured)
    }

    /// The `pixal3d` budget line. Held for a rig or a motion, **never** for an animate.
    pub async fn acquire(&self) -> Option<(Acquisition, Duration)> {
        let started = std::time::Instant::now();
        let permit = self.gate.acquire(ServiceKind::Pixal3d).await?;
        Some((permit, started.elapsed()))
    }

    /// Turn the gate's allocation into the `gpu` a submit carries, having first confirmed the
    /// service can actually place a worker there.
    ///
    /// 🔴 THE CHECK IS "CAN IT REACH WHAT WE RESERVED", NOT "IS IT ALREADY THERE" — the supervisor
    /// relocates per request, so a worker sitting on the other card is not a problem, and a card
    /// missing from `/health`'s `gpus` is. Refusing here costs a fast error; not refusing costs a
    /// ~450 s rig on a card nobody reserved, with the 11 GB reservation held against the other one.
    ///
    /// `None` from the gate (placement disabled, or a no-op acquisition) ⇒ `None` here: the engine
    /// then uses its own `WORKER_DEFAULT_GPU`, which is the pre-placement behaviour.
    pub async fn placement(&self, permit: &Acquisition) -> Result<Option<String>, Error> {
        let Some(scheduled) = permit.chosen_gpu() else {
            return Ok(None);
        };
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
        Ok(Some(scheduled.to_string()))
    }

    pub async fn health(&self) -> Result<Health, Error> {
        let resp = self.http.get(self.url("/health")?).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(resp.json().await?)
    }

    pub async fn healthy(&self) -> bool {
        self.health().await.map(|h| h.ok()).unwrap_or(false)
    }

    // ── submits ──────────────────────────────────────────────────────────────

    /// A rig from an uploaded image. Multipart, not base64: both ends of this hop are ours and a
    /// base64 `image` field would grow the picture by a third for no reason. The parts are named
    /// `json` and `image`, which is what `parse_body`/`resolve_image` read.
    ///
    /// 🔴 **BOTH PARTS CARRY A FILENAME, AND THE JSON ONE HAS TO.** The server reads them with
    /// `req.form.has_file(...)`, and cpp-httplib does not file a filename-less part there.
    /// Measured 2026-08-15 against the deployed service: with `{"tiers":["ultra"]}` sent as a plain
    /// text part the request was **accepted** with every field defaulted; sent as a file part it
    /// was correctly rejected as an unknown tier. A rig would have quietly ignored the tier list,
    /// the seeds and the re-run handles — and a "try again" that must move `rig_seed` would have
    /// been a 205 s no-op with no way to tell from the outside. See [`WireForm::json_file`].
    pub async fn rig_from_image(
        &self,
        image: Bytes,
        filename: &str,
        req: &RigSubmit,
    ) -> Result<String, Error> {
        let body = serde_json::to_value(req).unwrap_or(Value::Null);
        let form = WireForm::new()
            .json_file("json", "request.json", &body)
            .map_err(Error::Request)?
            .bytes(
                "image",
                filename.to_string(),
                "application/octet-stream",
                image.to_vec(),
            )
            .map_err(Error::Request)?;
        self.wire.submitted("pixal3d", "rig", form.size());
        let resp = self
            .http
            .post(self.url("/v1/rig")?)
            .multipart(form.into_form())
            .send()
            .await?;
        accepted_id(resp).await
    }

    /// A rig that re-uses an earlier result — `geometry_from` (~205 s) or `rig_cache_from`
    /// (~134 s). No image part: the geometry already exists.
    pub async fn rig_from_previous(&self, req: &RigSubmit) -> Result<String, Error> {
        self.post_json("/v1/rig", &serde_json::to_value(req).unwrap_or(Value::Null))
            .await
    }

    pub async fn motion(&self, req: &MotionSubmit) -> Result<String, Error> {
        self.post_json(
            "/v1/motion",
            &serde_json::to_value(req).unwrap_or(Value::Null),
        )
        .await
    }

    pub async fn animate(&self, req: &AnimateSubmit) -> Result<String, Error> {
        self.post_json(
            "/v1/animate",
            &serde_json::to_value(req).unwrap_or(Value::Null),
        )
        .await
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<String, Error> {
        let resp = self.http.post(self.url(path)?).json(body).send().await?;
        accepted_id(resp).await
    }

    // ── polling and artifacts ────────────────────────────────────────────────

    pub async fn job(&self, id: &str) -> Result<EngineJob, Error> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/jobs/{id}"))?)
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

    /// Cancel. Returns in ≤7 s engine-side and leaves no partial artifact.
    ///
    /// ⚠️ An empty JSON object, never a bodyless POST — see the module header.
    pub async fn cancel(&self, id: &str) -> Result<(), Error> {
        let resp = self
            .http
            .post(self.url(&format!("/v1/jobs/{id}/cancel"))?)
            .json(&json!({}))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(())
    }

    /// Stream a completed job's artifact to `dest`. `tier` picks a rung of the ladder; without it
    /// the hero (or, for an animate/render job, the animated GLB) is served.
    ///
    /// 🔴 This is what makes koblem's copy the durable one. The route reads the job RECORD, and
    /// records are evicted at 512 — after which the file is still on the volume and no longer
    /// reachable over HTTP. Fetch on completion, not on demand.
    pub async fn fetch_result_to(
        &self,
        job_id: &str,
        tier: Option<&str>,
        dest: &Path,
    ) -> Result<u64, Error> {
        use tokio::io::AsyncWriteExt;

        let path = match tier {
            Some(t) => format!("/v1/jobs/{job_id}/result?tier={t}"),
            None => format!("/v1/jobs/{job_id}/result"),
        };
        let resp = self.fetch_http.get(self.url(&path)?).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Io(parent.display().to_string(), e.to_string()))?;
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| Error::Io(dest.display().to_string(), e.to_string()))?;
        let mut written = 0_u64;
        let mut resp = resp;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk)
                .await
                .map_err(|e| Error::Io(dest.display().to_string(), e.to_string()))?;
            written += chunk.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| Error::Io(dest.display().to_string(), e.to_string()))?;
        self.wire.fetched_raw("pixal3d", written);
        Ok(written)
    }
}

/// Read a job id off a 202. The service answers `{id, status, ...}`; anything else is an error the
/// caller has to see verbatim, because the useful ones (`insufficient_vram`, `queue_full`,
/// `draining`) each name a different thing to do next.
async fn accepted_id(resp: reqwest::Response) -> Result<String, Error> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| {
                let msg = v.get("error")?.as_str()?.to_string();
                Some(match v.get("error_code").and_then(Value::as_str) {
                    Some(code) => format!("{msg} [{code}]"),
                    None => msg,
                })
            })
            .unwrap_or(body);
        return Err(Error::Upstream(status.as_u16(), detail));
    }
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("id")?.as_str().map(str::to_string))
        .ok_or_else(|| Error::JobFailed(format!("accept body carried no job id: {body}")))
}

/// The tiers off a completed rig's `result`, in the order the ladder built them.
pub fn tiers_of(result: &Value) -> Vec<EngineTier> {
    result
        .get("tiers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| serde_json::from_value::<EngineTier>(t.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// `result.warnings`, which is written for a human and must be surfaced rather than logged.
pub fn warnings_of(result: &Value) -> Vec<String> {
    result
        .get("warnings")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|w| w.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn string_at(result: &Value, key: &str) -> Option<String> {
    result
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The re-run handles, preferred over the job id because they are paths on a volume and the job
/// record is evicted at 512.
pub fn rerun_handles(result: &Value) -> (Option<String>, Option<String>) {
    (
        string_at(result, "stage_dir"),
        string_at(result, "rig_cache"),
    )
}

/// A completed motion job's clip path — what `/v1/animate` should be handed instead of the job id.
pub fn clip_path(job: &EngineJob) -> Option<String> {
    job.result_path
        .as_ref()
        .filter(|p| !p.is_empty())
        .cloned()
        .or_else(|| string_at(&job.result, "clip"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🔴 The single most expensive thing a UI can get wrong here: a tier that inherited the
    /// skeleton reports an EMPTY gate, and rendering it says "this rig failed" about a rig that
    /// was never gated. `draws == 0` is the whole test.
    #[test]
    fn an_inherited_tier_reports_a_gate_that_did_not_run() {
        let result = json!({
            "tiers": [
                { "tier": "hero", "joints": 52, "named_core": 21,
                  "gate": { "draws": 2, "accepted": true, "skin_ok": true, "named_core": 21,
                            "pose_gate_ran": true, "pose_gate_pass": true, "pose_gate_worst": 2.194,
                            "summary": "skin ok gate=humanoid core=21/22 -> accept" } },
                { "tier": "game", "joints": 52, "named_core": 21,
                  "gate": { "draws": 0, "accepted": false, "skin_ok": false, "named_core": 0,
                            "summary": "" } }
            ]
        });
        let tiers = tiers_of(&result);
        assert_eq!(tiers.len(), 2);
        assert!(tiers[0].gate.ran());
        assert!(tiers[0].gate.accepted);
        // The inherited tier: no gate ran, and its own joint counts are still the true shared ones.
        assert!(!tiers[1].gate.ran());
        assert_eq!(tiers[1].joints, 52);
        assert_eq!(tiers[1].named_core, 21);
    }

    /// A rejected draw is a COMPLETED job that shipped an asset. Nothing in this client may treat
    /// `accepted: false` as a failure.
    #[test]
    fn a_rejected_draw_is_still_a_completed_job() {
        let job = EngineJob {
            status: "completed".into(),
            result: json!({
                "tiers": [{ "tier": "game", "joints": 43, "named_core": 13,
                            "gate": { "draws": 1, "accepted": false, "skin_ok": true,
                                      "named_core": 13, "summary": "... -> reject" } }],
                "warnings": ["tier game: NO draw satisfied the acceptance predicate — shipped the best-ranked draw."]
            }),
            ..Default::default()
        };
        assert!(job.completed());
        assert!(job.is_terminal());
        let tiers = tiers_of(&job.result);
        assert!(tiers[0].gate.ran() && !tiers[0].gate.accepted);
        assert_eq!(warnings_of(&job.result).len(), 1);
    }

    /// The re-run handles are PATHS, and koblem prefers them to the job id precisely because the
    /// record they came from is evicted after 512 jobs while they are not.
    #[test]
    fn the_rerun_handles_come_off_the_result_as_paths() {
        let result = json!({
            "stage_dir": "/out/jobs/6a7ed4dc0005/stage",
            "rig_cache": "/out/jobs/6a7ed4dc0005/rig_cache",
        });
        let (stage, cache) = rerun_handles(&result);
        assert_eq!(stage.as_deref(), Some("/out/jobs/6a7ed4dc0005/stage"));
        assert_eq!(cache.as_deref(), Some("/out/jobs/6a7ed4dc0005/rig_cache"));
        // An absent (or empty) handle must read as absent, not as the empty path.
        let (stage, cache) = rerun_handles(&json!({ "stage_dir": "" }));
        assert!(stage.is_none() && cache.is_none());
    }

    /// A retry that does not move the seed returns the draw the user just rejected. The request
    /// type must therefore be able to carry one, and must NOT send a default when it has none —
    /// `rig_seed: 0` is a real, different draw from "the service's default".
    #[test]
    fn a_seedless_rig_request_sends_no_seed_at_all() {
        let body = serde_json::to_value(RigSubmit {
            tiers: vec!["hero".into(), "game".into()],
            ..Default::default()
        })
        .expect("serialises");
        assert_eq!(body["tiers"], json!(["hero", "game"]));
        assert!(body.get("rig_seed").is_none(), "{body}");
        assert!(body.get("geo_seed").is_none(), "{body}");
        assert!(body.get("resolution").is_none(), "{body}");

        let retry = serde_json::to_value(RigSubmit {
            tiers: vec!["hero".into()],
            rig_seed: Some(12345),
            geometry_from: Some("/out/jobs/abc/stage".into()),
            ..Default::default()
        })
        .expect("serialises");
        assert_eq!(retry["rig_seed"], json!(12345));
        assert_eq!(retry["geometry_from"], json!("/out/jobs/abc/stage"));
    }

    /// 🔴 A REQUEST WITH NO PLACEMENT MUST SEND NO `gpu` AT ALL. An empty-string or null `gpu`
    /// reaching the engine is not the same as an absent one: absent means "use your own
    /// WORKER_DEFAULT_GPU", which is the entire backward-compatibility story for a service deployed
    /// before placement existed.
    #[test]
    fn an_unplaced_rig_request_sends_no_gpu_field() {
        let body = serde_json::to_value(RigSubmit {
            tiers: vec!["hero".into()],
            ..Default::default()
        })
        .expect("serialises");
        assert!(body.get("gpu").is_none(), "{body}");

        let placed = serde_json::to_value(RigSubmit {
            tiers: vec!["hero".into()],
            gpu: Some("GPU-00000000-0000-0000-0000-000000000002".into()),
            ..Default::default()
        })
        .expect("serialises");
        assert_eq!(
            placed["gpu"],
            json!("GPU-00000000-0000-0000-0000-000000000002")
        );
    }

    /// The set an engine publishes wins; the single default it replaced is the fallback. An engine
    /// that reports neither has no placeable cards, which the caller must treat as "cannot honour a
    /// reservation" rather than "any card is fine".
    #[test]
    fn supported_gpus_prefers_the_set_and_falls_back_to_the_default() {
        let relocating = Health {
            gpus: vec!["GPU-a".into(), "GPU-b".into()],
            gpu_uuid: Some("GPU-a".into()),
            ..Default::default()
        };
        assert_eq!(relocating.supported_gpus(), vec!["GPU-a", "GPU-b"]);

        let pinned = Health {
            gpu_uuid: Some("GPU-a".into()),
            ..Default::default()
        };
        assert_eq!(pinned.supported_gpus(), vec!["GPU-a"]);

        let ancient = Health::default();
        assert!(ancient.supported_gpus().is_empty());
    }

    /// A motion job's clip is addressed by PATH for the same durability reason as the rig handles.
    #[test]
    fn a_motion_clip_is_addressed_by_path() {
        let job = EngineJob {
            kind: "motion".into(),
            status: "completed".into(),
            result_path: Some("/out/jobs/abc/clip.json".into()),
            result: json!({ "frames": 120, "fps": 30.0 }),
            ..Default::default()
        };
        assert_eq!(clip_path(&job).as_deref(), Some("/out/jobs/abc/clip.json"));
        // An empty result_path must not become an empty clip reference the engine would 400 on.
        let job = EngineJob {
            result_path: Some(String::new()),
            ..Default::default()
        };
        assert!(clip_path(&job).is_none());
    }
}
