//! **3D → Pixal3D** — photo in, a rigged character out, then prompts against it forever.
//!
//! # The timings are lopsided, and they are the whole design
//!
//! ```text
//!   rig      ~450 s first tier, ~150 s each after   once per character
//!   motion   ~11 s warm / ~33 s cold                per prompt
//!   animate  0.22 s, CPU-only                       per take
//! ```
//!
//! So this is not "upload, wait, get a thing". It is: pay ~10 minutes ONCE for a character, then
//! live in a ~12 s loop typing prompts at it. Everything here follows from that —
//!
//! * **the prompt is not on the upload form.** The rig is the artefact; prompts are what you do to
//!   it afterwards, repeatedly. `/v1/render` (the one-shot chain) is deliberately not exposed.
//! * **animate does not take the GPU gate, and is not a queued job.** The engine refuses to park a
//!   0.22 s retarget behind a 450 s rig; queueing it here would undo that on koblem's side. A rig
//!   and a motion ARE queued jobs — this module builds their payloads and hands them to the global
//!   queue, and `queue::adapters::pixal3d_{rig,motion}` run them.
//! * **takes are kept, not replaced.** Nothing scores an animation — there is no quality signal for
//!   motion at all — so comparing takes is the user's job and the store's only job is to make it
//!   cheap.
//!
//! # 🔴 Three engine behaviours a client gets wrong by default
//!
//! 1. **`status: "completed"` with `gate.accepted: false` is NORMAL.** The QC gates are advisory:
//!    the owner judged a rig failing the pose gate at 25.9 "decent". This module never withholds an
//!    asset over a verdict. The one thing that fails closed is a zero-skin rig, and the engine
//!    refuses that itself.
//! 2. **A "try again" MUST move a seed.** The rig decode is deterministic in (mesh, seed), so a
//!    re-run of an unchanged request returns the draw the user just rejected — 205 s later.
//!    [`rerig`] refuses a re-roll that would be a no-op rather than charging for one.
//! 3. **A tier that inherited the skeleton reports an EMPTY gate** (`draws: 0`). It is not a
//!    failure and must not be rendered as one; `EngineGate::ran()` is the guard.
//!
//! # Artifacts: fetched, not proxied
//!
//! The engine's job RECORDS are evicted at 512 and `GET /v1/jobs/{id}/result` reads the record, so
//! an artifact outlives its only HTTP handle. Every delivered tier is therefore pulled onto
//! koblem's disk the moment the job completes, and served from there. The re-run handles kept are
//! the engine's published PATHS (`stage_dir`, `rig_cache`, `result.image`, a clip's
//! `result_path`), which are files on a volume and outlive the record too.
//!
//! # The staging mount, and what breaks without it
//!
//! `/v1/animate` takes a rig as a job id **or a path the container can see**. A rig the user
//! repaired in the browser is neither — so koblem writes it into `PIXAL3D_STAGING_DIR`, which the
//! engine container mounts at the SAME absolute path. ⚠️ Without that mount an edit is still
//! saved, previewed and downloadable; it just cannot be animated, and the API says so in those
//! words rather than failing at submit.

use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use bytes::Bytes;
use koblem_client::types::{Job as QueueJob, JobKind, JobState as QueueState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::pixal3d::{
    self as engine, AnimateSubmit, Error as EngineError, MotionSubmit, RigSubmit,
};
use crate::pixal3d_jobs::{Animation, Character, JobState, Motion, RigEdit, RigSource};
use crate::repositories::gpu_jobs as queue_store;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::tier2_helpers::{extract_caller, log_async};

/// The `gpu_logs.service` these rows are filed under.
const SERVICE: &str = "pixal3d";

/// The LOD ladder, in the order the engine builds it. Tier 0 is the only one that pays for
/// geometry and for the rig decode; every later tier resumes from its stage dir.
pub(crate) const TIERS: [&str; 4] = ["hero", "high", "medium", "game"];

/// What a fresh upload asks for unless told otherwise: one tier to judge and one to ship, on one
/// shared skeleton, for ~600 s instead of the full ladder's ~960.
const DEFAULT_TIERS: [&str; 2] = ["hero", "game"];

/// The `base_tier` a corrected animation carries. Not a rung of the ladder — a take is output, and
/// an edit of one is a sibling asset rather than a repaired rig.
///
/// 🔴 An edit with this base must never be offered as an animate target: it already has tracks
/// baked in, and retargeting onto it would layer a second clip over the first.
const TAKE_BASE: &str = "take";

const IMAGE_LIMIT: usize = 64 * 1024 * 1024;
/// A hero-tier GLB with a baked normal map is tens of MB, and an edit is one of those coming back.
const GLB_LIMIT: usize = 512 * 1024 * 1024;
const JSON_LIMIT: usize = 256 * 1024;

/// How often a running animate is re-polled. It is over in ~0.22 s, so this is barely a poll.
const FAST_POLL: Duration = Duration::from_millis(500);

/// Consecutive poll failures tolerated before a take is failed. The engine restarting loses the
/// job record (they live in memory), which reads as a 404 forever — so a run that can no longer be
/// observed has to end rather than sit at `running`.
const MAX_POLL_ERRORS: u32 = 10;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/pixal3d/props", get(props))
        .route("/api/v1/pixal3d/active", get(active))
        .route(
            "/api/v1/pixal3d/characters",
            get(list_characters)
                .post(create_character)
                .layer(DefaultBodyLimit::max(IMAGE_LIMIT)),
        )
        .route(
            "/api/v1/pixal3d/characters/{id}",
            get(get_character).delete(remove_character),
        )
        .route(
            "/api/v1/pixal3d/characters/{id}/rename",
            post(rename_character).layer(DefaultBodyLimit::max(JSON_LIMIT)),
        )
        .route(
            "/api/v1/pixal3d/characters/{id}/rerig",
            post(rerig).layer(DefaultBodyLimit::max(JSON_LIMIT)),
        )
        .route("/api/v1/pixal3d/characters/{id}/cancel", post(cancel_job))
        .route("/api/v1/pixal3d/characters/{id}/model", get(serve_model))
        .route("/api/v1/pixal3d/characters/{id}/source", get(serve_source))
        .route("/api/v1/pixal3d/characters/{id}/bundle", get(serve_bundle))
        .route(
            "/api/v1/pixal3d/characters/{id}/edits",
            post(create_edit).layer(DefaultBodyLimit::max(GLB_LIMIT)),
        )
        .route(
            "/api/v1/pixal3d/characters/{id}/edits/{edit_id}/model",
            get(serve_edit),
        )
        .route(
            "/api/v1/pixal3d/characters/{id}/edits/{edit_id}",
            delete(remove_edit),
        )
        .route(
            "/api/v1/pixal3d/characters/{id}/animate",
            post(animate).layer(DefaultBodyLimit::max(JSON_LIMIT)),
        )
        .route(
            "/api/v1/pixal3d/characters/{id}/animations/{anim_id}/model",
            get(serve_animation),
        )
        .route(
            "/api/v1/pixal3d/characters/{id}/animations/{anim_id}",
            delete(remove_animation),
        )
        .route(
            "/api/v1/pixal3d/motions",
            get(list_motions)
                .post(create_motion)
                .layer(DefaultBodyLimit::max(JSON_LIMIT)),
        )
        .route(
            "/api/v1/pixal3d/motions/{id}",
            get(get_motion).delete(remove_motion),
        )
        .route("/api/v1/pixal3d/motions/{id}/cancel", post(cancel_job))
}

// ── the wire ─────────────────────────────────────────────────────────────────

/// What a client needs to draw the form without submitting anything.
#[derive(Debug, Serialize)]
struct Props {
    configured: bool,
    healthy: bool,
    health: Option<engine::Health>,
    tiers: Vec<&'static str>,
    default_tiers: Vec<&'static str>,
    /// `fix` | `nofix` | `base` | `rot` | `rotbase` — the retarget variants the engine validates
    /// against, listed here so a UI does not keep its own copy that can drift out of range.
    variants: Vec<&'static str>,
    /// Whether the shared directory an edited rig has to be written into actually exists. False ⇒
    /// edits are still saved and downloadable but cannot be animated.
    staging_ready: bool,
    staging_dir: String,
    /// The measured wall clocks, so the UI can set an honest expectation instead of a spinner.
    /// There is no progress on the engine's job record and none is invented.
    timings: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RigCreateRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tiers: Option<Vec<String>>,
    #[serde(default)]
    rig_seed: Option<u32>,
    #[serde(default)]
    rig_retries: Option<u32>,
    /// Opt-in geometry lattice. 1536 is the only value besides the default with any measured
    /// result, and it is **per-subject**: better hands on some characters, a worse rig on others.
    #[serde(default)]
    resolution: Option<u32>,
    #[serde(default)]
    texsize: Option<u32>,
    /// Only ever sent to the engine when turning the automatic skin-weight cleanup OFF.
    #[serde(default)]
    skin_cleanup: Option<bool>,
}

/// A re-roll. Which knob moves decides both the cost and what actually changes.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum RerigRequest {
    /// Re-decode the skeleton on the cached geometry. ~205 s, same body, different rig.
    /// 🔴 The seed MUST differ from the one that produced the parent or this is a paid no-op.
    RigSeed {
        rig_seed: u32,
        #[serde(default)]
        tiers: Option<Vec<String>>,
        #[serde(default)]
        rig_retries: Option<u32>,
    },
    /// Redraw the MESH. ~450 s, and a genuinely different character — the same photo produces a
    /// different mesh on every run anyway (an open engine bug that happens to make this a real fix
    /// rather than a re-roll of the same dice).
    GeoSeed {
        geo_seed: i32,
        #[serde(default)]
        rig_seed: Option<u32>,
        #[serde(default)]
        tiers: Option<Vec<String>>,
        #[serde(default)]
        resolution: Option<u32>,
    },
    /// Bake more rungs of the ladder onto the SAME skeleton. ~134 s a tier, `draws: 0` — no decode
    /// at all, so a clip authored on the hero still plays on what comes out.
    Tiers { tiers: Vec<String> },
}

#[derive(Debug, Clone, Deserialize)]
struct MotionRequest {
    prompt: String,
    #[serde(default)]
    seconds: Option<f32>,
    #[serde(default)]
    seed: Option<u32>,
    #[serde(default)]
    cfg: Option<f32>,
    #[serde(default)]
    steps: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct AnimateRequest {
    /// The clips to bake, by [`Motion::id`]. Order is kept: they become separate animation tracks.
    motion_ids: Vec<String>,
    /// Which GLB to retarget onto — a tier, or a rig the user repaired in the browser.
    #[serde(default = "hero_source")]
    rig_source: RigSource,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    smooth: Option<u32>,
    /// The `skeleton-exercise` track, which swings every materially weighted joint in turn. On by
    /// default: it is the honest "does this rig work at all" preview.
    #[serde(default = "yes")]
    exercise: bool,
}

fn hero_source() -> RigSource {
    RigSource::Tier {
        tier: "hero".to_string(),
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
struct EditMeta {
    #[serde(default)]
    label: Option<String>,
    base_tier: String,
    /// Whatever the browser editor reports it changed. Free-form: the editor owns that vocabulary,
    /// and pinning it here would mean a server deploy every time it learns a new repair.
    #[serde(default)]
    changes: Value,
}

#[derive(Debug, Default, Deserialize)]
struct TierQuery {
    tier: Option<String>,
    exp: Option<i64>,
    sig: Option<String>,
    token: Option<String>,
}

// ── read routes ──────────────────────────────────────────────────────────────

async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let health = if state.pixal3d.is_configured() {
        state.pixal3d.health().await.ok()
    } else {
        None
    };
    let staging = &*state.pixal3d_staging_dir;
    Json(Props {
        configured: state.pixal3d.is_configured(),
        healthy: health.as_ref().map(engine::Health::ok).unwrap_or(false),
        health,
        tiers: TIERS.to_vec(),
        default_tiers: DEFAULT_TIERS.to_vec(),
        variants: vec!["rotbase", "rot", "base", "fix", "nofix"],
        staging_ready: tokio::fs::try_exists(staging).await.unwrap_or(false),
        staging_dir: staging.display().to_string(),
        // Measured on the 3060, 2026-08-14. Published rather than hard-coded in the UI so the two
        // cannot drift, and stated as ranges because they are samples, not bounds.
        timings: json!({
            "rig_first_tier_seconds": [409, 636],
            "rig_extra_tier_seconds": [147, 168],
            "rerig_from_geometry_seconds": 205,
            "rebake_from_rig_cache_seconds": 134,
            "motion_warm_seconds": 11,
            "motion_cold_seconds": 33,
            "animate_seconds": 0.22,
            "note": "The engine publishes no progress on a job record — only queued/running/\
                     completed. These are what to expect, not what is happening."
        }),
    })
    .into_response()
}

/// The pixal3d job the queue is running or holding, if any. Animations are not consulted: the
/// retarget is CPU-only and never enters the queue.
async fn active(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let live = queue_store::live(&state.db).await.unwrap_or_default();
    let pending = queue_store::pending(&state.db).await.unwrap_or_default();
    let found = live.into_iter().chain(pending).find(is_pixal3d);
    let Some(row) = found else {
        return Json(json!({ "active": Value::Null })).into_response();
    };
    let Some((kind, id, label)) = catalogue_ref(&state, &row).await else {
        return Json(json!({ "active": Value::Null })).into_response();
    };
    Json(json!({
        "active": {
            "kind": kind,
            "id": id,
            "label": label,
            "job_id": row.id,
            "state": row.state,
            "eta_start": row.eta_start,
        }
    }))
    .into_response()
}

fn is_pixal3d(row: &QueueJob) -> bool {
    matches!(row.kind, JobKind::Pixal3dRig | JobKind::Pixal3dMotion)
}

/// The catalogue record a queue row belongs to, as `(kind, id, label)`. The id is in the payload
/// because that is the only handle that survives the engine evicting its own job record.
async fn catalogue_ref(state: &AppState, row: &QueueJob) -> Option<(&'static str, String, String)> {
    let (payload, _) = queue_store::payload(&state.db, &row.id)
        .await
        .ok()
        .flatten()?;
    match row.kind {
        JobKind::Pixal3dRig => {
            let id = payload.get("character_id")?.as_str()?.to_string();
            let label = state
                .pixal3d_jobs
                .character(&id)
                .map(|c| c.name)
                .unwrap_or_else(|| id.clone());
            Some(("rig", id, label))
        }
        JobKind::Pixal3dMotion => {
            let id = payload.get("motion_id")?.as_str()?.to_string();
            let label = state
                .pixal3d_jobs
                .motion(&id)
                .map(|m| m.prompt.chars().take(60).collect())
                .unwrap_or_else(|| id.clone());
            Some(("motion", id, label))
        }
        _ => None,
    }
}

/// The queue row a catalogue id belongs to, if it is still queued or running.
async fn job_for(state: &AppState, id: &str) -> Option<QueueJob> {
    let live = queue_store::live(&state.db).await.unwrap_or_default();
    let pending = queue_store::pending(&state.db).await.unwrap_or_default();
    for row in live.into_iter().chain(pending).filter(is_pixal3d) {
        if let Some((_, record_id, _)) = catalogue_ref(state, &row).await
            && record_id == id
        {
            return Some(row);
        }
    }
    None
}

fn rig_payload(character_id: &str, submit: &RigSubmit) -> Value {
    let mut payload = serde_json::to_value(submit).unwrap_or_else(|_| json!({}));
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("character_id".into(), json!(character_id));
    }
    payload
}

fn motion_payload(motion_id: &str, submit: &MotionSubmit) -> Value {
    let mut payload = serde_json::to_value(submit).unwrap_or_else(|_| json!({}));
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("motion_id".into(), json!(motion_id));
    }
    payload
}

/// Hand a built payload to the queue. The refusal is returned as a message, not a response, so the
/// catalogue record can be closed out carrying the reason the user is about to read.
async fn enqueue(
    state: &AppState,
    headers: &HeaderMap,
    kind: JobKind,
    payload: Value,
) -> Result<koblem_client::types::EnqueueResponse, (StatusCode, String)> {
    let who = crate::routes::queue::principal_of(state, headers)
        .await
        .map_err(|_| (StatusCode::UNAUTHORIZED, "unauthorized".to_string()))?;
    crate::routes::queue::enqueue(
        state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(headers).request(kind, payload),
    )
    .await
}

async fn list_characters(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    Json(json!({ "characters": state.pixal3d_jobs.list_characters() })).into_response()
}

async fn get_character(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match state.pixal3d_jobs.character(&id) {
        Some(c) => Json(c).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such character"),
    }
}

async fn list_motions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    Json(json!({ "motions": state.pixal3d_jobs.list_motions() })).into_response()
}

async fn get_motion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match state.pixal3d_jobs.motion(&id) {
        Some(m) => Json(m).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such motion"),
    }
}

// ── creating a character ─────────────────────────────────────────────────────

/// `POST /api/v1/pixal3d/characters` — multipart `image` + `json`. 202 and a character id.
async fn create_character(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.pixal3d.is_configured() {
        return not_configured();
    }
    let (req, filename, image) = match parse_image_multipart(multipart).await {
        Ok(v) => v,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
    };
    let tiers = match resolve_tiers(req.tiers.clone(), &DEFAULT_TIERS) {
        Ok(t) => t,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
    };
    if let Err(msg) = check_resolution(req.resolution) {
        return err(StatusCode::BAD_REQUEST, &msg);
    }
    let id = new_id("chr");
    let name = req
        .name
        .clone()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| pretty_name(&filename));
    // 🔴 The record exists before the enqueue, not after it: the adapter reads the source image
    // back off this directory and has nowhere to put a finished tier without it.
    state.pixal3d_jobs.create_character(id.clone(), name);

    // The source image lands on koblem's disk before anything is submitted: it is what the UI
    // shows beside the result, what the bundle carries, and — for a subject that rigs badly — the
    // thing to compare a dark texture against before "fixing" it.
    let ext = extension_of(&filename, "png");
    let source_name = format!("source.{ext}");
    let source_path = state.pixal3d_jobs.file_path(&id, &source_name);
    if let Err(e) = tokio::fs::write(&source_path, &image).await {
        state.pixal3d_jobs.update_character(&id, |c| {
            c.state = JobState::Failed;
            c.error = Some(format!("could not save the source image: {e}"));
        });
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not save the source image: {e}"),
        );
    }
    let submit = RigSubmit {
        // None, unlike every re-run: this request carries the picture as a multipart part, so
        // there is no engine-side path to name yet.
        image: None,
        tiers: tiers.clone(),
        rig_seed: req.rig_seed,
        geo_seed: None,
        rig_retries: req.rig_retries,
        resolution: req.resolution.filter(|r| *r > 0),
        texsize: req.texsize,
        // Only ever sent when turning it off — the engine's default is on, and it is what removes
        // the "spears off the arms" defect before a human ever sees it.
        skin_cleanup: req.skin_cleanup.filter(|on| !*on),
        geometry_from: None,
        rig_cache_from: None,
        // Filled in at submit from the gate's allocation, not here — the card is the scheduler's
        // decision and it is not made until the permit is in hand.
        gpu: None,
    };
    state.pixal3d_jobs.update_character(&id, |c| {
        c.source_filename = Some(source_name);
        c.source_bytes = image.len() as u64;
        c.tiers_requested = tiers;
        c.rig_seed = submit.rig_seed;
        c.origin = "upload".into();
        c.request = serde_json::to_value(&submit).unwrap_or(Value::Null);
    });

    let enqueued = match enqueue(
        &state,
        &headers,
        JobKind::Pixal3dRig,
        rig_payload(&id, &submit),
    )
    .await
    {
        Ok(e) => e,
        Err((code, msg)) => {
            state.pixal3d_jobs.update_character(&id, |c| {
                c.state = JobState::Failed;
                c.error = Some(msg.clone());
            });
            return err(code, &msg);
        }
    };

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "id": id,
            "job_id": enqueued.job_id,
            "position": enqueued.position,
            "status": "queued",
            "eta_seconds_first_tier": [409, 636],
            "eta_basis": "measured on the 3060, 2026-08-14. The engine publishes no progress; this \
                          is what to expect, not what is happening.",
        })),
    )
        .into_response()
}

/// `POST /api/v1/pixal3d/characters/{id}/rerig` — a re-roll, as a NEW character with a lineage.
///
/// 🔴 A re-roll is never an overwrite. The take it came from stays exactly as delivered, because
/// the whole point of re-rolling a draw lottery is being able to go back to the one you had.
async fn rerig(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<RerigRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.pixal3d.is_configured() {
        return not_configured();
    }
    let Some(parent) = state.pixal3d_jobs.character(&id) else {
        return err(StatusCode::NOT_FOUND, "no such character");
    };
    if parent.state != JobState::Done {
        return err(
            StatusCode::CONFLICT,
            "this character has not finished rigging, so there is nothing to re-run against",
        );
    }
    // The engine resolves `geometry_from`/`rig_cache_from` given a PATH without consulting a job
    // record — but it only back-fills the image from a record. So the image path has to be sent
    // explicitly, and a character that kept none cannot be re-run cheaply.
    let Some(image_path) = engine_image_path(&parent) else {
        return err(
            StatusCode::CONFLICT,
            "this character did not record the engine-side path of its source image, so a re-run \
             has nothing to feed the geometry stage. Upload the image again as a new character.",
        );
    };

    let (mut submit, origin, tiers) = match &req {
        RerigRequest::RigSeed {
            rig_seed,
            tiers,
            rig_retries,
        } => {
            // 🔴 The refusal that saves 205 s of GPU and a confused user: the rig decode is
            // deterministic in (mesh, seed), so the same seed against the same geometry returns
            // the draw that was just rejected.
            if parent.rig_seed.unwrap_or(0) == *rig_seed {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "rig_seed {rig_seed} is the seed this rig already used. The decode is \
                         deterministic in (mesh, seed), so re-running it returns the identical \
                         draw after ~205 s. Send a different seed, or re-roll the geometry instead."
                    ),
                );
            }
            let Some(stage_dir) = parent.stage_dir.clone() else {
                return err(
                    StatusCode::CONFLICT,
                    "this character kept no stage directory, so its geometry cannot be re-rigged",
                );
            };
            let tiers = match resolve_tiers(tiers.clone(), &[]) {
                Ok(t) if !t.is_empty() => t,
                Ok(_) => parent.tiers_requested.clone(),
                Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
            };
            (
                RigSubmit {
                    tiers: tiers.clone(),
                    rig_seed: Some(*rig_seed),
                    rig_retries: *rig_retries,
                    geometry_from: Some(stage_dir),
                    ..Default::default()
                },
                "rig_seed",
                tiers,
            )
        }
        RerigRequest::GeoSeed {
            geo_seed,
            rig_seed,
            tiers,
            resolution,
        } => {
            if let Err(msg) = check_resolution(*resolution) {
                return err(StatusCode::BAD_REQUEST, &msg);
            }
            let tiers = match resolve_tiers(tiers.clone(), &[]) {
                Ok(t) if !t.is_empty() => t,
                Ok(_) => parent.tiers_requested.clone(),
                Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
            };
            (
                RigSubmit {
                    tiers: tiers.clone(),
                    geo_seed: Some(*geo_seed),
                    rig_seed: *rig_seed,
                    resolution: resolution.filter(|r| *r > 0),
                    ..Default::default()
                },
                "geo_seed",
                tiers,
            )
        }
        RerigRequest::Tiers { tiers } => {
            let tiers = match resolve_tiers(Some(tiers.clone()), &[]) {
                Ok(t) if !t.is_empty() => t,
                Ok(_) => return err(StatusCode::BAD_REQUEST, "name at least one tier to bake"),
                Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
            };
            let (Some(stage_dir), Some(rig_cache)) =
                (parent.stage_dir.clone(), parent.rig_cache.clone())
            else {
                return err(
                    StatusCode::CONFLICT,
                    "this character kept no stage directory and rig cache, so more tiers cannot be \
                     baked onto its skeleton",
                );
            };
            // 🔴 BOTH handles. `rig_cache_from` alone keeps the skeleton but leaves the stage dir
            // defaulting to the new job's own — which would re-run the whole geometry stage and
            // quietly turn a 134 s bake into a 450 s rig.
            (
                RigSubmit {
                    tiers: tiers.clone(),
                    geometry_from: Some(stage_dir),
                    rig_cache_from: Some(rig_cache),
                    ..Default::default()
                },
                "tiers",
                tiers,
            )
        }
    };

    // Every re-run reaches the engine without a file part, so it has to name the picture the
    // parent already left on the volume — see `RigSubmit::image`.
    submit.image = Some(image_path.clone());

    let new_id = new_id("chr");
    let label = match origin {
        "rig_seed" => format!("{} · re-rig", parent.name),
        "geo_seed" => format!("{} · re-mesh", parent.name),
        _ => format!("{} · more tiers", parent.name),
    };
    state.pixal3d_jobs.create_character(new_id.clone(), label);

    // The source image is copied across so the new take is self-describing, and so deleting the
    // parent does not leave a character with no picture of what it was made from.
    let source_name = copy_source(&state, &parent, &new_id).await;
    state.pixal3d_jobs.update_character(&new_id, |c| {
        c.parent_id = Some(parent.id.clone());
        c.origin = origin.into();
        c.source_filename = source_name;
        c.source_bytes = parent.source_bytes;
        c.tiers_requested = tiers;
        c.rig_seed = submit.rig_seed;
        c.geo_seed = submit.geo_seed;
        c.request = serde_json::to_value(&submit).unwrap_or(Value::Null);
        // Carried so THIS character can be re-run in turn without walking back up the chain.
        c.engine_image = Some(image_path.clone());
    });

    let enqueued = match enqueue(
        &state,
        &headers,
        JobKind::Pixal3dRig,
        rig_payload(&new_id, &submit),
    )
    .await
    {
        Ok(e) => e,
        Err((code, msg)) => {
            state.pixal3d_jobs.update_character(&new_id, |c| {
                c.state = JobState::Failed;
                c.error = Some(msg.clone());
            });
            return err(code, &msg);
        }
    };

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "id": new_id,
            "job_id": enqueued.job_id,
            "position": enqueued.position,
            "status": "queued",
            "parent_id": parent.id,
            "origin": origin,
        })),
    )
        .into_response()
}

async fn rename_character(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(name) = name else {
        return err(
            StatusCode::BAD_REQUEST,
            "`name` is required and cannot be empty",
        );
    };
    if state.pixal3d_jobs.character(&id).is_none() {
        return err(StatusCode::NOT_FOUND, "no such character");
    }
    let name = name.chars().take(120).collect::<String>();
    state
        .pixal3d_jobs
        .update_character(&id, |c| c.name = name.clone());
    StatusCode::NO_CONTENT.into_response()
}

async fn remove_character(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match state.pixal3d_jobs.character(&id) {
        Some(c) if !c.state.is_terminal() => err(
            StatusCode::CONFLICT,
            "cancel this rig before deleting it — the engine is still working on it",
        ),
        Some(_) => {
            state.pixal3d_jobs.delete_character(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        None => err(StatusCode::NOT_FOUND, "no such character"),
    }
}

/// Cancel a rig or a motion, named by its catalogue id.
///
/// 🔴 Through the dispatcher, not straight at the engine. The dispatcher owns the run, so it is the
/// only thing that can stop it AND file the terminal state; reaching past it would leave a row
/// `running` with nothing running.
async fn cancel_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(row) = job_for(&state, &id).await else {
        return err(
            StatusCode::NOT_FOUND,
            "no queued or running pixal3d job for that id",
        );
    };
    match row.state {
        QueueState::Queued | QueueState::Held => {
            // Closing the catalogue record out is the adapter's `abandon`, reached through the
            // dispatcher — the same path the generic `DELETE /queue/jobs/{id}` takes. It used to be
            // repeated here, which is why cancelling the identical job from the queue page left the
            // character sitting at `queued` and cancelling it from the 3D page did not.
            state.queue.abandon_pending(&state.db, &row.id).await;
            Json(json!({ "cancelled": id, "job_id": row.id })).into_response()
        }
        QueueState::Running => match state.queue.get() {
            Some(d) if d.interrupt(&row.id, crate::queue::runner::Interrupt::Cancel) => (
                StatusCode::ACCEPTED,
                Json(json!({ "cancelled": id, "job_id": row.id })),
            )
                .into_response(),
            _ => err(
                StatusCode::CONFLICT,
                "that job is not running on this process, or has already finished on the engine",
            ),
        },
        // 🔴 Past the engine/disk boundary: the GPU cost is paid and the GLBs are being pulled
        // onto koblem's disk. Killing it here loses the only copy.
        QueueState::Collecting => err(
            StatusCode::CONFLICT,
            "this job has finished on the GPU and is collecting its artifacts — it cannot be \
             cancelled now",
        ),
        _ => err(StatusCode::CONFLICT, "this job has already finished"),
    }
}

// ── motions ──────────────────────────────────────────────────────────────────

async fn create_motion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MotionRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.pixal3d.is_configured() {
        return not_configured();
    }
    let prompt = req.prompt.trim().to_string();
    if prompt.is_empty() {
        return err(StatusCode::BAD_REQUEST, "`prompt` is required");
    }
    if let Some(s) = req.seconds
        && !(0.1..=60.0).contains(&s)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "`seconds` must be between 0.1 and 60",
        );
    }
    let id = new_id("mot");
    state.pixal3d_jobs.create_motion(id.clone(), prompt.clone());
    let submit = MotionSubmit {
        prompt,
        seconds: req.seconds,
        seed: req.seed,
        cfg: req.cfg,
        steps: req.steps,
        name: None,
        // As for RigSubmit: set at submit from the permit, never by the caller.
        gpu: None,
    };
    state.pixal3d_jobs.update_motion(&id, |m| {
        m.seed = submit.seed;
        m.seconds = submit.seconds;
        m.cfg = submit.cfg;
        m.steps = submit.steps;
    });

    // Whether the DiT is resident right now IS the difference between the ~11 s loop and a ~33 s
    // first prompt, and it is knowable before submitting — so it is reported on the accept rather
    // than left to feel like the engine got slower.
    let warm = state
        .pixal3d
        .health()
        .await
        .map(|h| h.loaded)
        .unwrap_or(false);

    let enqueued = match enqueue(
        &state,
        &headers,
        JobKind::Pixal3dMotion,
        motion_payload(&id, &submit),
    )
    .await
    {
        Ok(e) => e,
        Err((code, msg)) => {
            state.pixal3d_jobs.update_motion(&id, |m| {
                m.state = JobState::Failed;
                m.error = Some(msg.clone());
            });
            return err(code, &msg);
        }
    };

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "id": id,
            "job_id": enqueued.job_id,
            "position": enqueued.position,
            "status": "queued",
            "dit_resident": warm,
            "eta_seconds": if warm { 11 } else { 33 },
        })),
    )
        .into_response()
}

async fn remove_motion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match state.pixal3d_jobs.motion(&id) {
        Some(m) if !m.state.is_terminal() => err(
            StatusCode::CONFLICT,
            "cancel this motion before deleting it",
        ),
        Some(_) => {
            state.pixal3d_jobs.delete_motion(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        None => err(StatusCode::NOT_FOUND, "no such motion"),
    }
}

// ── animate ──────────────────────────────────────────────────────────────────

/// `POST /api/v1/pixal3d/characters/{id}/animate` — retarget clips onto a rig. ~0.22 s.
///
/// 🔴 NO GPU PERMIT. The engine runs this without a session precisely so it does not queue behind
/// a rig; acquiring here would reintroduce the wait it exists to avoid.
async fn animate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<AnimateRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.pixal3d.is_configured() {
        return not_configured();
    }
    let Some(character) = state.pixal3d_jobs.character(&id) else {
        return err(StatusCode::NOT_FOUND, "no such character");
    };
    if req.motion_ids.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "name at least one motion to bake — generate one first, they are ~11 s each",
        );
    }
    if let Some(v) = req.variant.as_deref()
        && !["fix", "nofix", "base", "rot", "rotbase"].contains(&v)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "`variant` must be one of fix | nofix | base | rot | rotbase",
        );
    }
    let mut clips = Vec::with_capacity(req.motion_ids.len());
    for motion_id in &req.motion_ids {
        let Some(motion) = state.pixal3d_jobs.motion(motion_id) else {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("no such motion {motion_id}"),
            );
        };
        match motion.clip_path {
            // By PATH, not by the engine's job id: records are evicted at 512 and a clip generated
            // yesterday would then resolve to nothing.
            Some(p) if motion.state == JobState::Done => clips.push(p),
            _ => {
                return err(
                    StatusCode::CONFLICT,
                    &format!("motion {motion_id} has not finished generating"),
                );
            }
        }
    }
    let Some(rig) = character.rig_reference(&req.rig_source) else {
        return match &req.rig_source {
            RigSource::Tier { tier } => err(
                StatusCode::BAD_REQUEST,
                &format!("this character has no `{tier}` tier — bake it first"),
            ),
            RigSource::Edit { .. } => err(
                StatusCode::CONFLICT,
                &format!(
                    "that edited rig was never written to the shared staging directory ({}), so \
                     the engine container cannot open it. The edit is still downloadable; \
                     animating one needs koblem and longcat-rig to mount that directory at the \
                     SAME absolute path.",
                    state.pixal3d_staging_dir.display()
                ),
            ),
        };
    };

    let anim_id = new_id("anm");
    let label = req
        .label
        .clone()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| {
            state
                .pixal3d_jobs
                .motion(&req.motion_ids[0])
                .map(|m| m.prompt.chars().take(60).collect())
                .unwrap_or_else(|| "take".to_string())
        });
    let tier_label = match &req.rig_source {
        RigSource::Tier { tier } => tier.clone(),
        RigSource::Edit { edit_id } => format!("edit:{edit_id}"),
    };
    let animation = Animation {
        id: anim_id.clone(),
        state: JobState::Queued,
        engine_job_id: None,
        label,
        motion_ids: req.motion_ids.clone(),
        rig_source: req.rig_source.clone(),
        variant: req.variant.clone(),
        smooth: req.smooth,
        exercise: req.exercise,
        animations: 0,
        mapped: 0,
        map_kind: String::new(),
        exercise_joints: 0,
        seconds: 0.0,
        filename: None,
        bytes: 0,
        error: None,
        created_unix: time::OffsetDateTime::now_utc().unix_timestamp(),
    };
    state
        .pixal3d_jobs
        .update_character(&id, |c| c.animations.insert(0, animation));

    let submit = AnimateSubmit {
        rig,
        // Already applied: `rig` is a resolved GLB path, and passing a tier beside it would ask the
        // engine to re-choose one off a job record that may no longer exist.
        tier: None,
        clips,
        variant: req.variant,
        smooth: req.smooth,
        exercise: Some(req.exercise),
    };
    let bg = state.clone();
    let character_id = id.clone();
    let a_id = anim_id.clone();
    let caller = extract_caller(&headers);
    tokio::spawn(async move { run_animate(bg, character_id, a_id, submit, caller).await });

    (
        StatusCode::ACCEPTED,
        Json(json!({ "id": anim_id, "status": "queued", "tier": tier_label })),
    )
        .into_response()
}

async fn remove_animation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, anim_id)): Path<(String, String)>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(character) = state.pixal3d_jobs.character(&id) else {
        return err(StatusCode::NOT_FOUND, "no such character");
    };
    let Some(animation) = character.animation(&anim_id).cloned() else {
        return err(StatusCode::NOT_FOUND, "no such animation");
    };
    if let Some(name) = &animation.filename {
        let _ = tokio::fs::remove_file(state.pixal3d_jobs.file_path(&id, name)).await;
    }
    state
        .pixal3d_jobs
        .update_character(&id, |c| c.animations.retain(|a| a.id != anim_id));
    StatusCode::NO_CONTENT.into_response()
}

// ── browser edits ────────────────────────────────────────────────────────────

/// `POST /api/v1/pixal3d/characters/{id}/edits` — multipart `glb` + `json`.
///
/// The browser repaired a rig (pruned a skin-weight leak, renamed joints to the Mixamo core) and
/// re-exported it. koblem stores the file and, when the shared mount exists, also writes it where
/// the engine container can read it so `/v1/animate` can retarget onto it.
///
/// ⚠️ The engine ALREADY runs a skin-weight cleanup on every rig, and keeps the original skin
/// byte-for-byte when it cannot prove an improvement against the pose gate. This route is the
/// manual override for what that pass declined to change, not a replacement for it.
async fn create_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    multipart: Multipart,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(character) = state.pixal3d_jobs.character(&id) else {
        return err(StatusCode::NOT_FOUND, "no such character");
    };
    let (meta, glb) = match parse_glb_multipart(multipart).await {
        Ok(v) => v,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
    };
    // `base_tier` names the rung the edit was made from — except for a corrected animation, where
    // the base is a delivered TAKE rather than a tier. Both are stored as edits because both are
    // "a GLB this character now also owns"; only the origin differs.
    if meta.base_tier == TAKE_BASE {
        if character.animations.iter().all(|a| a.filename.is_none()) {
            return err(
                StatusCode::BAD_REQUEST,
                "this character has no delivered take to have corrected",
            );
        }
    } else if character.tier(&meta.base_tier).is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("this character has no `{}` tier", meta.base_tier),
        );
    }
    // A GLB starts `glTF` + a little-endian version. Checked because everything downstream — the
    // viewer, the bundle, the retarget — assumes it, and the failure otherwise lands minutes later
    // inside the engine.
    if glb.len() < 12 || &glb[..4] != b"glTF" {
        return err(
            StatusCode::BAD_REQUEST,
            "that is not a binary glTF (GLB) file — it does not start with the `glTF` magic",
        );
    }

    let edit_id = new_id("edt");
    let filename = format!("edit-{edit_id}.glb");
    let dest = state.pixal3d_jobs.file_path(&id, &filename);
    if let Err(e) = tokio::fs::write(&dest, &glb).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not save the edited rig: {e}"),
        );
    }

    // The shared copy. Best-effort on purpose: a missing mount costs the ability to ANIMATE this
    // edit and nothing else, and refusing to save it would throw away the user's work over a
    // deployment detail.
    let staged = stage_edit(&state, &id, &edit_id, &glb).await;
    if staged.is_none() {
        warn!(character = %id, edit = %edit_id, dir = %state.pixal3d_staging_dir.display(),
              "the pixal3d staging directory is not writable — this edit cannot be animated");
    }

    let edit = RigEdit {
        id: edit_id.clone(),
        label: meta
            .label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| format!("{} · repaired", meta.base_tier)),
        base_tier: meta.base_tier,
        filename,
        bytes: glb.len() as u64,
        staged_path: staged.clone(),
        changes: meta.changes,
        created_unix: time::OffsetDateTime::now_utc().unix_timestamp(),
    };
    state
        .pixal3d_jobs
        .update_character(&id, |c| c.edits.insert(0, edit.clone()));
    (
        StatusCode::CREATED,
        Json(json!({
            "id": edit_id,
            "animatable": staged.is_some(),
            "note": if staged.is_some() { Value::Null } else { json!(format!(
                "saved, but not written to {} — this edit can be previewed and downloaded, not \
                 animated", state.pixal3d_staging_dir.display())) },
        })),
    )
        .into_response()
}

async fn remove_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, edit_id)): Path<(String, String)>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(character) = state.pixal3d_jobs.character(&id) else {
        return err(StatusCode::NOT_FOUND, "no such character");
    };
    let Some(edit) = character.edit(&edit_id).cloned() else {
        return err(StatusCode::NOT_FOUND, "no such edit");
    };
    let _ = tokio::fs::remove_file(state.pixal3d_jobs.file_path(&id, &edit.filename)).await;
    if let Some(staged) = &edit.staged_path {
        let _ = tokio::fs::remove_file(staged).await;
    }
    // The takes that were retargeted onto it go too: their rig no longer exists, so re-running one
    // would silently fall back to a different body.
    state.pixal3d_jobs.update_character(&id, |c| {
        c.edits.retain(|e| e.id != edit_id);
        c.animations
            .retain(|a| !matches!(&a.rig_source, RigSource::Edit { edit_id: e } if *e == edit_id));
    });
    StatusCode::NO_CONTENT.into_response()
}

// ── serving artifacts ────────────────────────────────────────────────────────

const GLB_MIME: &str = "model/gltf-binary";

async fn serve_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<TierQuery>,
) -> Response {
    let route = format!("/api/v1/pixal3d/characters/{id}/model");
    if let Some(resp) = media_gate(&state, &headers, &q, &route).await {
        return resp;
    }
    let Some(character) = state.pixal3d_jobs.character(&id) else {
        return err(StatusCode::NOT_FOUND, "no such character");
    };
    // Without `?tier=` the first delivered rung — the ladder is built best-first, so that is the
    // hero unless the hero was never asked for.
    let tier = match q.tier.as_deref() {
        Some(t) => match character.tier(t) {
            Some(t) => t,
            None => return err(StatusCode::NOT_FOUND, "this character has no such tier"),
        },
        None => match character.tiers.first() {
            Some(t) => t,
            None => {
                return err(
                    StatusCode::CONFLICT,
                    "this character has no delivered tier yet — poll it until it reports `done`",
                );
            }
        },
    };
    serve_from_character(&state, &headers, &id, &tier.filename, GLB_MIME).await
}

async fn serve_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, edit_id)): Path<(String, String)>,
    Query(q): Query<TierQuery>,
) -> Response {
    let route = format!("/api/v1/pixal3d/characters/{id}/edits/{edit_id}/model");
    if let Some(resp) = media_gate(&state, &headers, &q, &route).await {
        return resp;
    }
    let Some(edit) = state
        .pixal3d_jobs
        .character(&id)
        .and_then(|c| c.edit(&edit_id).cloned())
    else {
        return err(StatusCode::NOT_FOUND, "no such edit");
    };
    serve_from_character(&state, &headers, &id, &edit.filename, GLB_MIME).await
}

async fn serve_animation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, anim_id)): Path<(String, String)>,
    Query(q): Query<TierQuery>,
) -> Response {
    let route = format!("/api/v1/pixal3d/characters/{id}/animations/{anim_id}/model");
    if let Some(resp) = media_gate(&state, &headers, &q, &route).await {
        return resp;
    }
    let Some(animation) = state
        .pixal3d_jobs
        .character(&id)
        .and_then(|c| c.animation(&anim_id).cloned())
    else {
        return err(StatusCode::NOT_FOUND, "no such animation");
    };
    let Some(name) = animation.filename else {
        return err(
            StatusCode::CONFLICT,
            "this take has not been delivered yet — poll the character until it reports `done`",
        );
    };
    serve_from_character(&state, &headers, &id, &name, GLB_MIME).await
}

async fn serve_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<TierQuery>,
) -> Response {
    let route = format!("/api/v1/pixal3d/characters/{id}/source");
    if let Some(resp) = media_gate(&state, &headers, &q, &route).await {
        return resp;
    }
    let Some(name) = state
        .pixal3d_jobs
        .character(&id)
        .and_then(|c| c.source_filename)
    else {
        return err(StatusCode::NOT_FOUND, "this character kept no source image");
    };
    let mime = match extension_of(&name, "png").as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    };
    serve_from_character(&state, &headers, &id, &name, mime).await
}

async fn serve_from_character(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    filename: &str,
    mime: &str,
) -> Response {
    let path = state.pixal3d_jobs.file_path(id, filename);
    match crate::routes::byte_serve::serve_file(headers, &path, mime).await {
        Ok(resp) => resp,
        Err(_) => err(
            StatusCode::NOT_FOUND,
            "that file is missing from this character's directory",
        ),
    }
}

/// `GET /api/v1/pixal3d/characters/{id}/bundle` — every tier, every edit, every take, the source
/// image, and a manifest, as one zip.
///
/// 🔴 **STORED, not deflated, for the GLBs.** A GLB carries an already-compressed PNG normal map
/// and baked texture; deflating tens of MB of that spends CPU to save low single-digit percent.
/// The small text members are deflated, where it actually pays.
///
/// Built to a temp file and streamed, never assembled in memory: a four-tier ladder plus takes is
/// comfortably past the point where a `Vec<u8>` is a way to OOM koblem.
async fn serve_bundle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<TierQuery>,
) -> Response {
    let route = format!("/api/v1/pixal3d/characters/{id}/bundle");
    if let Some(resp) = media_gate(&state, &headers, &q, &route).await {
        return resp;
    }
    let Some(character) = state.pixal3d_jobs.character(&id) else {
        return err(StatusCode::NOT_FOUND, "no such character");
    };
    if character.tiers.is_empty() {
        return err(
            StatusCode::CONFLICT,
            "this character has nothing delivered yet",
        );
    }
    let dir = state.pixal3d_jobs.character_dir(&id);
    // 🔴 A UNIQUE NAME PER BUILD, NOT A FIXED `bundle.zip`. Two downloads of the same character
    // would otherwise write the same path concurrently and hand at least one caller a torn zip —
    // and a fixed name also leaves a permanent second copy of every tier on disk. Stale builds are
    // swept below rather than deleted after serving, because the response streams from the file
    // after this handler returns.
    let out = dir.join(format!("bundle-{}.zip", new_id("b")));
    sweep_old_bundles(&dir).await;
    let motions: Vec<Motion> = state.pixal3d_jobs.list_motions();
    let built = tokio::task::spawn_blocking({
        let out = out.clone();
        move || build_bundle(&dir, &out, &character, &motions)
    })
    .await;
    match built {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("bundle: {e}")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("bundle: {e}")),
    }
    match crate::routes::byte_serve::serve_file(&headers, &out, "application/zip").await {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "the bundle vanished"),
    }
}

fn build_bundle(
    dir: &FsPath,
    out: &FsPath,
    character: &Character,
    motions: &[Motion],
) -> Result<(), String> {
    use std::io::{Read, Write};

    let file = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let text = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // 8 MiB at a time — the whole reason this is a file and not a `Vec<u8>`.
    let copy_in = |zip: &mut zip::ZipWriter<std::io::BufWriter<std::fs::File>>,
                   name: &str,
                   from: &FsPath|
     -> Result<(), String> {
        let mut src =
            std::fs::File::open(from).map_err(|e| format!("opening {}: {e}", from.display()))?;
        zip.start_file(name, stored).map_err(|e| e.to_string())?;
        let mut buf = vec![0_u8; 8 * 1024 * 1024];
        loop {
            let n = src.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        }
        Ok(())
    };

    for tier in &character.tiers {
        let path = dir.join(&tier.filename);
        if path.is_file() {
            copy_in(&mut zip, &format!("model.{}.glb", tier.tier), &path)?;
        }
    }
    for edit in &character.edits {
        let path = dir.join(&edit.filename);
        if path.is_file() {
            copy_in(
                &mut zip,
                &format!("edits/{}.glb", safe_name(&edit.label)),
                &path,
            )?;
        }
    }
    for animation in &character.animations {
        let Some(name) = &animation.filename else {
            continue;
        };
        let path = dir.join(name);
        if path.is_file() {
            copy_in(
                &mut zip,
                &format!("animations/{}.glb", safe_name(&animation.label)),
                &path,
            )?;
        }
    }
    if let Some(source) = &character.source_filename {
        let path = dir.join(source);
        if path.is_file() {
            copy_in(&mut zip, &format!("source/{source}"), &path)?;
        }
    }

    let manifest = manifest(character, motions);
    zip.start_file("manifest.json", text)
        .map_err(|e| e.to_string())?;
    zip.write_all(
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    zip.start_file("README.md", text)
        .map_err(|e| e.to_string())?;
    zip.write_all(readme(character).as_bytes())
        .map_err(|e| e.to_string())?;
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// What the bundle says about itself. The QC verdicts are in here rather than filtered out: a rig
/// that shipped with a failing pose gate is still a deliverable, and whoever opens this zip in six
/// months needs to know which one it was.
fn manifest(character: &Character, motions: &[Motion]) -> Value {
    let tiers: Vec<Value> = character
        .tiers
        .iter()
        .map(|t| {
            json!({
                "tier": t.tier,
                "file": format!("model.{}.glb", t.tier),
                "decimate": t.decimate,
                "joints": t.joints,
                "named_core": t.named_core,
                "seconds": t.seconds,
                "skin_cleanup": t.skin_cleanup,
                // Only where a gate actually ran. `draws: 0` means this tier inherited the
                // skeleton and decoded nothing; publishing its empty verdict would read as a
                // failure that did not happen.
                "gate": if t.gate.ran() { serde_json::to_value(&t.gate).unwrap_or(Value::Null) } else { Value::Null },
            })
        })
        .collect();
    let animations: Vec<Value> = character
        .animations
        .iter()
        .filter(|a| a.filename.is_some())
        .map(|a| {
            json!({
                "file": format!("animations/{}.glb", safe_name(&a.label)),
                "label": a.label,
                "tracks": a.animations,
                "joints_mapped": a.mapped,
                "map_kind": a.map_kind,
                "prompts": a.motion_ids.iter()
                    .filter_map(|id| motions.iter().find(|m| &m.id == id))
                    .map(|m| json!({ "prompt": m.prompt, "seed": m.seed, "frames": m.frames, "fps": m.fps }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "format": "pixal3d-character-v1",
        "name": character.name,
        "created": character.created_unix,
        "engine": "longcat-rig avatar_server (hbd-longcat-avatar.cpp)",
        "lineage": { "parent": character.parent_id, "origin": character.origin,
                     "rig_seed": character.rig_seed, "geo_seed": character.geo_seed },
        "tiers": tiers,
        "one_skeleton": "All tiers share ONE skeleton (--rig-cache), so a clip authored on the hero \
                         plays on the game tier unchanged.",
        "edits": character.edits.iter().map(|e| json!({
            "file": format!("edits/{}.glb", safe_name(&e.label)),
            "label": e.label, "base_tier": e.base_tier, "changes": e.changes,
        })).collect::<Vec<_>>(),
        "animations": animations,
        "warnings": character.warnings,
        "textures": "Raw baseColor plus a baked normal map. If it looks dark, compare it against \
                     the source image before adjusting it — that is the reference, not preference.",
    })
}

fn readme(character: &Character) -> String {
    let tiers = character
        .tiers
        .iter()
        .map(|t| {
            format!(
                "- `model.{}.glb` — {} joints, {} of 22 core names",
                t.tier, t.joints, t.named_core
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# {name}\n\nA rigged character from Pixal3D (longcat-rig).\n\n## The models\n\n{tiers}\n\n\
         Every tier shares ONE skeleton, so an animation authored on any of them plays on all of \
         them. Each is a self-contained GLB: mesh, baseColor texture, baked normal map, skin \
         weights and skeleton. Nothing here needs the source image at runtime.\n\n\
         ## Animations\n\nAnimations are baked glTF animation tracks INSIDE the GLBs under \
         `animations/`, not sidecar files. `skeleton-exercise` swings every materially weighted \
         joint in turn — it is the honest test of whether the rig deforms, and it is not authored \
         motion.\n\n## What the manifest's `gate` block means\n\nAdvisory QC verdicts, recorded at \
         rig time and not recoverable by reading the GLB back. `accepted: false` means no draw \
         cleared the acceptance predicate and the best-ranked one shipped anyway — a normal, \
         usable outcome, not a corrupt file. A tier with no gate block inherited its skeleton and \
         decoded nothing.\n\n## Front-facing yaw\n\nNot canonical. Some characters face away by \
         default; frame the model rather than assuming a front.\n",
        name = character.name,
        tiers = if tiers.is_empty() {
            "- (none delivered)".into()
        } else {
            tiers
        },
    )
}

// ── telemetry ────────────────────────────────────────────────────────────────
//
// The retarget's row only. A rig and a motion are queue jobs now, and the queue's own table is
// where their timings live — a `gpu_logs` row cut at the 202 would record the acceptance and
// nothing anybody wants to measure.
//
// 🔴 **No prompt text in any row here.** A prompt is user content; the GPU log is a debugging
// surface that is listed, kept and swept on its own schedule.

/// The HTTP-shaped verdict a terminal record maps to.
///
/// ⚠️ A cancel is 499, not 500. It is neither a success nor an engine failure, and counting it as
/// one makes every failure rate computed off this table wrong.
fn status_of(state: JobState) -> i32 {
    match state {
        JobState::Done => 200,
        JobState::Cancelled => 499,
        _ => 500,
    }
}

/// A retarget's row.
///
/// ⚠️ **`gate_wait_ms` is 0 by construction, and that is not a missing value.** The retarget takes
/// no GPU permit on purpose — that is what keeps a 0.22 s job from queueing behind a 450 s rig — so
/// there is no wait to report and never will be. Do not "fix" this by wiring a gate figure in.
fn log_animate(
    state: &AppState,
    character_id: &str,
    anim_id: &str,
    caller: Option<String>,
    wall: Duration,
) {
    let Some(a) = state
        .pixal3d_jobs
        .character(character_id)
        .and_then(|c| c.animation(anim_id).cloned())
    else {
        return;
    };
    log_async(
        state.db.clone(),
        CreateLog {
            service: SERVICE.to_string(),
            endpoint: "animate".to_string(),
            request_meta: json!({
                "clips": a.motion_ids.len(),
                "variant": a.variant,
                "exercise": a.exercise,
                // Which GLB it was aimed at: a rung of the ladder, or a rig repaired in the
                // browser. A low `mapped` below means something different for each.
                "target": match &a.rig_source {
                    RigSource::Tier { tier } => tier.clone(),
                    RigSource::Edit { edit_id } => format!("edit:{edit_id}"),
                },
            }),
            response_meta: json!({
                "character_id": character_id,
                "animation_id": a.id,
                "animations": a.animations,
                "mapped": a.mapped,
                "map_kind": a.map_kind,
                "exercise_joints": a.exercise_joints,
                "error": a.error,
            }),
            sample_id: None,
            gate_wait_ms: 0,
            duration_ms: if a.seconds > 0.0 {
                (a.seconds * 1000.0) as i64
            } else {
                wall.as_millis() as i64
            },
            status: status_of(a.state),
            caller,
        },
    );
}

// ── the retarget ─────────────────────────────────────────────────────────────
//
// The one thing here that is not a queued job: `/v1/animate` is CPU-only, ~0.22 s, and takes no
// GPU permit at all — the engine runs it alongside a rig on purpose.

/// The retarget. No permit, and no slot: it is CPU-only and runs alongside a rig by design.
async fn run_animate(
    state: AppState,
    character_id: String,
    anim_id: String,
    submit: AnimateSubmit,
    caller: Option<String>,
) {
    let started = Instant::now();
    let set = |f: &dyn Fn(&mut Animation)| {
        state.pixal3d_jobs.update_character(&character_id, |c| {
            if let Some(a) = c.animations.iter_mut().find(|a| a.id == anim_id) {
                f(a);
            }
        });
    };
    let engine_id = match state.pixal3d.animate(&submit).await {
        Ok(id) => id,
        Err(e) => {
            let msg = describe_submit(&e);
            set(&|a| {
                a.state = JobState::Failed;
                a.error = Some(msg.clone());
            });
            log_animate(&state, &character_id, &anim_id, caller, started.elapsed());
            return;
        }
    };
    set(&|a| {
        a.state = JobState::Running;
        a.engine_job_id = Some(engine_id.clone());
    });

    let job = match poll(&state, &engine_id, FAST_POLL).await {
        Ok(job) => job,
        Err(PollOutcome::Cancelled) => {
            set(&|a| {
                a.state = JobState::Cancelled;
                a.error = Some("cancelled".into());
            });
            log_animate(&state, &character_id, &anim_id, caller, started.elapsed());
            return;
        }
        Err(PollOutcome::Failed(msg)) => {
            set(&|a| {
                a.state = JobState::Failed;
                a.error = Some(msg.clone());
            });
            log_animate(&state, &character_id, &anim_id, caller, started.elapsed());
            return;
        }
    };

    let filename = format!("anim-{anim_id}.glb");
    let dest = state.pixal3d_jobs.file_path(&character_id, &filename);
    let bytes = match state.pixal3d.fetch_result_to(&engine_id, None, &dest).await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("the take was baked but could not be fetched: {e}");
            set(&|a| {
                a.state = JobState::Failed;
                a.error = Some(msg.clone());
            });
            log_animate(&state, &character_id, &anim_id, caller, started.elapsed());
            return;
        }
    };
    let r = job.result.get("animate").unwrap_or(&job.result).clone();
    set(&|a| {
        a.state = JobState::Done;
        a.filename = Some(filename.clone());
        a.bytes = bytes;
        a.animations = r.get("animations").and_then(Value::as_i64).unwrap_or(0);
        a.mapped = r.get("mapped").and_then(Value::as_i64).unwrap_or(0);
        a.map_kind = r
            .get("map_kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        a.exercise_joints = r
            .get("exercise_joints")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        a.seconds = r.get("seconds").and_then(Value::as_f64).unwrap_or(0.0);
    });
    log_animate(&state, &character_id, &anim_id, caller, started.elapsed());
}

enum PollOutcome {
    Cancelled,
    Failed(String),
}

/// Poll one animate job to a terminal state. It has no cancel path of its own: it is over in
/// ~0.22 s, and there is nothing to interrupt.
async fn poll(
    state: &AppState,
    engine_id: &str,
    interval: Duration,
) -> Result<engine::EngineJob, PollOutcome> {
    let mut errors = 0_u32;
    loop {
        match state.pixal3d.job(engine_id).await {
            Ok(job) if job.completed() => return Ok(job),
            Ok(job) if job.status == "cancelled" => return Err(PollOutcome::Cancelled),
            Ok(job) if job.is_terminal() => {
                let detail = job.error.unwrap_or_else(|| job.status.clone());
                let code = job.error_code.unwrap_or_default();
                return Err(PollOutcome::Failed(describe_engine_failure(&detail, &code)));
            }
            Ok(_) => errors = 0,
            Err(e) => {
                errors += 1;
                if errors >= MAX_POLL_ERRORS {
                    return Err(PollOutcome::Failed(format!(
                        "lost contact with the rig engine after {errors} consecutive failures \
                         ({e}). If it restarted, its job records went with it — the artifacts are \
                         still on /out under job {engine_id}."
                    )));
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn engine_image_path(character: &Character) -> Option<String> {
    character.engine_image.clone().filter(|p| !p.is_empty())
}

/// Copy a parent's source image into a re-roll's own directory, so a take is self-describing.
async fn copy_source(state: &AppState, parent: &Character, new_id: &str) -> Option<String> {
    let name = parent.source_filename.clone()?;
    let from = state.pixal3d_jobs.file_path(&parent.id, &name);
    let to = state.pixal3d_jobs.file_path(new_id, &name);
    match tokio::fs::copy(&from, &to).await {
        Ok(_) => Some(name),
        Err(e) => {
            warn!(from = %from.display(), error = %e, "could not copy the source image to a re-roll");
            None
        }
    }
}

/// Drop bundles from earlier downloads. A zip is a full second copy of every tier this character
/// owns, so leaving them would roughly double the store for anything anyone downloaded twice.
///
/// Age-based rather than delete-after-serving: `serve_file` streams the response AFTER the handler
/// returns, so a build cannot safely remove its own file. Fifteen minutes is far longer than any
/// download of a few hundred MB over loopback and short enough that the copies do not accumulate.
async fn sweep_old_bundles(dir: &FsPath) {
    const KEEP: Duration = Duration::from_secs(15 * 60);
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("bundle-") || !name.ends_with(".zip") {
            continue;
        }
        let stale = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > KEEP);
        if stale {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Write an edited rig where the engine container can read it. `None` when the shared mount is
/// missing — which costs the ability to animate this edit and nothing else.
async fn stage_edit(
    state: &AppState,
    character_id: &str,
    edit_id: &str,
    glb: &[u8],
) -> Option<PathBuf> {
    let dir = &*state.pixal3d_staging_dir;
    tokio::fs::create_dir_all(dir).await.ok()?;
    let dest = dir.join(format!("{character_id}-{edit_id}.glb"));
    tokio::fs::write(&dest, glb).await.ok()?;
    Some(dest)
}

/// Validate a tier list against the ladder, keeping the engine's own build order rather than the
/// caller's — tier 0 is the one that pays for geometry, so which rung is first is not cosmetic.
pub(crate) fn resolve_tiers(
    requested: Option<Vec<String>>,
    fallback: &[&str],
) -> Result<Vec<String>, String> {
    let Some(requested) = requested else {
        return Ok(fallback.iter().map(|t| t.to_string()).collect());
    };
    if requested.is_empty() {
        return Ok(fallback.iter().map(|t| t.to_string()).collect());
    }
    for t in &requested {
        if !TIERS.contains(&t.as_str()) {
            return Err(format!(
                "unknown tier `{t}` — the ladder is hero | high | medium | game"
            ));
        }
    }
    Ok(TIERS
        .iter()
        .filter(|t| requested.iter().any(|r| r == *t))
        .map(|t| t.to_string())
        .collect())
}

/// The engine's own rule, checked here so a typo costs a 400 instead of a GPU stage that dies in
/// the DC remesh minutes later.
pub(crate) fn check_resolution(resolution: Option<u32>) -> Result<(), String> {
    match resolution {
        None | Some(0) => Ok(()),
        Some(r) if (32..=2032).contains(&r) && r % 16 == 0 => Ok(()),
        Some(r) => Err(format!(
            "`resolution` is {r}; expected 0 (the default 1024 lattice) or a multiple of 16 from \
             32 to 2032. 1536 is the only other value with any measured result and it is \
             per-subject: better geometry on some characters, a worse rig on others."
        )),
    }
}

pub(crate) fn describe_submit(e: &EngineError) -> String {
    match e {
        EngineError::Upstream(503, detail) if detail.contains("insufficient_vram") => format!(
            "{detail}\n\n🔴 The rig stage needs ~11 GB free and dies rather than degrading, so it \
             refuses rather than starting. Since it can run on either card, this means NEITHER had \
             room: the 5060 Ti is carrying a render (video takes 11-12 GB of its 16) and the 3060 \
             holds the resident LLM (~9 GB of its 12). Wait for the render, or free a card in GPU \
             Scheduler, and try again — the detail above names what each card had."
        ),
        EngineError::Upstream(503, detail) if detail.contains("queue_full") => {
            format!("{detail} — the engine is already holding as many jobs as it will admit.")
        }
        other => other.to_string(),
    }
}

pub(crate) fn describe_engine_failure(detail: &str, code: &str) -> String {
    match code {
        "insufficient_vram" => format!(
            "{detail}\n\n🔴 The rig stage needs ~11 GB free and will not degrade to fit. It can \
             use either card, so this means neither had room when the job reached the front of the \
             queue. Free one and re-run — a re-run from this character's geometry is ~205 s, not \
             the full ~450."
        ),
        "rig_failed" if detail.contains("skin") => format!(
            "{detail}\n\n🔴 A zero-skin rig is the ONE thing the engine refuses to ship: it \
             deforms nothing and would poison the shared skeleton cache for every tier. Re-roll \
             the rig with a different seed — the decode is a draw lottery and the next seed is a \
             genuinely different draw."
        ),
        _ => detail.to_string(),
    }
}

fn not_configured() -> Response {
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        "Pixal3D is not configured — set `pixal3d_server_url` in Settings (the longcat-rig \
         service, normally http://longcat-rig:8102).",
    )
}

async fn parse_image_multipart(
    mut mp: Multipart,
) -> Result<(RigCreateRequest, String, Bytes), String> {
    let mut req = RigCreateRequest::default();
    let mut image: Option<(String, Bytes)> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
        match field.name() {
            Some("json") | Some("request") => {
                let bytes = field.bytes().await.map_err(|e| e.to_string())?;
                req = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("invalid request json: {e}"))?;
            }
            Some("image") => {
                let filename = field.file_name().unwrap_or("source.png").to_string();
                image = Some((filename, field.bytes().await.map_err(|e| e.to_string())?));
            }
            _ => {}
        }
    }
    let (filename, bytes) = image.ok_or_else(|| "missing `image` part".to_string())?;
    if bytes.is_empty() {
        return Err("the image is empty".into());
    }
    Ok((req, filename, bytes))
}

async fn parse_glb_multipart(mut mp: Multipart) -> Result<(EditMeta, Bytes), String> {
    let mut meta: Option<EditMeta> = None;
    let mut glb: Option<Bytes> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
        match field.name() {
            Some("json") | Some("request") => {
                let bytes = field.bytes().await.map_err(|e| e.to_string())?;
                meta = Some(
                    serde_json::from_slice(&bytes)
                        .map_err(|e| format!("invalid edit metadata: {e}"))?,
                );
            }
            Some("glb") | Some("model") => {
                glb = Some(field.bytes().await.map_err(|e| e.to_string())?);
            }
            _ => {}
        }
    }
    let meta = meta.ok_or_else(|| "missing `json` part naming the base tier".to_string())?;
    let glb = glb.ok_or_else(|| "missing `glb` part".to_string())?;
    if glb.is_empty() {
        return Err("the edited GLB is empty".into());
    }
    Ok((meta, glb))
}

/// A filename, reduced to something safe to put in a zip entry.
fn safe_name(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "take".to_string()
    } else {
        trimmed.chars().take(60).collect()
    }
}

fn extension_of(filename: &str, fallback: &str) -> String {
    FsPath::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()) && !e.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn pretty_name(filename: &str) -> String {
    let stem = FsPath::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("character");
    let cleaned: String = stem
        .chars()
        .map(|c| if c == '_' || c == '-' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "character".to_string()
    } else {
        trimmed.chars().take(120).collect()
    }
}

fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

/// Auth for a media route: a signature issued for THIS path, a token, or a real credential. Same
/// rule as the gallery's, because a `<model-viewer src>` cannot set a header either.
async fn media_gate(
    state: &AppState,
    headers: &HeaderMap,
    q: &TierQuery,
    path: &str,
) -> Option<Response> {
    if let (Some(exp), Some(sig)) = (q.exp, q.sig.as_deref())
        && state.media_signer.verify(path, exp, sig)
    {
        return None;
    }
    if let Some(token) = q.token.as_deref().map(str::trim).filter(|s| !s.is_empty())
        && crate::auth::validate_token(&state.db, state.api_key.as_deref(), token)
            .await
            .is_ok()
    {
        return None;
    }
    auth_gate(state, headers).await
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder's build order is the engine's, not the caller's: tier 0 is the only one that pays
    /// for geometry and the rig decode, so a request listing `game` before `hero` must not make the
    /// 15k tier the expensive one.
    #[test]
    fn tiers_are_normalised_into_the_engines_build_order() {
        let got = resolve_tiers(Some(vec!["game".into(), "hero".into()]), &DEFAULT_TIERS)
            .expect("valid tiers");
        assert_eq!(got, vec!["hero".to_string(), "game".to_string()]);
        // Absent and empty both mean "the default pair", not "all four".
        assert_eq!(
            resolve_tiers(None, &DEFAULT_TIERS).expect("default"),
            vec!["hero".to_string(), "game".to_string()],
        );
        assert_eq!(
            resolve_tiers(Some(vec![]), &DEFAULT_TIERS).expect("default"),
            vec!["hero".to_string(), "game".to_string()],
        );
        // Duplicates collapse rather than baking a tier twice.
        assert_eq!(
            resolve_tiers(Some(vec!["hero".into(), "hero".into()]), &[]).expect("valid"),
            vec!["hero".to_string()],
        );
        assert!(resolve_tiers(Some(vec!["ultra".into()]), &[]).is_err());
    }

    /// The engine's own admissible set, enforced at submit so a typo cannot cost a diffusion stage
    /// that dies in the DC remesh several minutes later.
    #[test]
    fn the_resolution_rule_is_the_dcs_admissible_set() {
        assert!(check_resolution(None).is_ok());
        assert!(check_resolution(Some(0)).is_ok());
        assert!(check_resolution(Some(1536)).is_ok());
        assert!(check_resolution(Some(32)).is_ok());
        assert!(check_resolution(Some(2032)).is_ok());
        assert!(check_resolution(Some(777)).is_err());
        assert!(check_resolution(Some(16)).is_err());
        assert!(check_resolution(Some(2048)).is_err());
        // And the refusal has to say what 1536 actually costs, because it is the one value anybody
        // will reach for and it is not a free upgrade.
        let msg = check_resolution(Some(777)).unwrap_err();
        assert!(msg.contains("per-subject"), "{msg}");
    }

    /// 🔴 A re-roll body must be able to express the three genuinely different actions, and
    /// `rig_seed` — the one that is a paid no-op if it does not move — must carry a seed by
    /// construction rather than by hope.
    #[test]
    fn the_three_rerolls_are_distinct_and_the_seed_is_mandatory() {
        let by_seed: RerigRequest =
            serde_json::from_value(json!({ "mode": "rig_seed", "rig_seed": 12345 }))
                .expect("parses");
        assert!(matches!(
            by_seed,
            RerigRequest::RigSeed {
                rig_seed: 12345,
                ..
            }
        ));
        // A rig_seed re-roll with no seed does not parse at all.
        assert!(serde_json::from_value::<RerigRequest>(json!({ "mode": "rig_seed" })).is_err());

        let by_mesh: RerigRequest =
            serde_json::from_value(json!({ "mode": "geo_seed", "geo_seed": 7 })).expect("parses");
        assert!(matches!(by_mesh, RerigRequest::GeoSeed { geo_seed: 7, .. }));

        let more: RerigRequest =
            serde_json::from_value(json!({ "mode": "tiers", "tiers": ["medium"] }))
                .expect("parses");
        assert!(matches!(more, RerigRequest::Tiers { .. }));
    }

    /// Zip entry names come from user-supplied labels. A take called `../../etc/passwd` has to
    /// become an ordinary filename, not a path.
    #[test]
    fn a_take_label_cannot_become_a_path_in_the_bundle() {
        assert_eq!(safe_name("../../etc/passwd"), "etc-passwd");
        assert_eq!(
            safe_name("a person walks forward"),
            "a-person-walks-forward"
        );
        assert_eq!(safe_name(""), "take");
        assert_eq!(safe_name("///"), "take");
        assert!(!safe_name("a/b\\c").contains('/'));
    }

    /// An uploaded filename becomes a default character name and an extension. Neither may carry a
    /// path separator into the character's directory.
    #[test]
    fn an_upload_filename_yields_a_plain_name_and_extension() {
        assert_eq!(pretty_name("gilly_front-view.png"), "gilly front view");
        assert_eq!(pretty_name(""), "character");
        assert_eq!(extension_of("photo.JPEG", "png"), "jpeg");
        assert_eq!(extension_of("noextension", "png"), "png");
        // A hostile extension falls back rather than becoming part of a filename.
        assert_eq!(extension_of("x.pn/g", "png"), "png");
    }

    /// The animate default is the hero tier and the exercise track — the one that proves the rig
    /// deforms at all.
    #[test]
    fn animate_defaults_to_the_hero_tier_with_the_exercise_track() {
        let req: AnimateRequest =
            serde_json::from_value(json!({ "motion_ids": ["mot_1"] })).expect("parses");
        assert!(matches!(req.rig_source, RigSource::Tier { ref tier } if tier == "hero"));
        assert!(req.exercise);
        // And an edit can be named instead, which is the whole point of the browser repair path.
        let req: AnimateRequest = serde_json::from_value(json!({
            "motion_ids": ["mot_1"],
            "rig_source": { "kind": "edit", "edit_id": "edt_1" }
        }))
        .expect("parses");
        assert!(matches!(req.rig_source, RigSource::Edit { ref edit_id } if edit_id == "edt_1"));
    }

    /// An `insufficient_vram` refusal is the single most likely failure on a 12 GB card that also
    /// hosts the LLM, and a bare relay of the engine's sentence does not say what to do about it.
    #[test]
    fn a_vram_refusal_names_the_card_and_the_way_out() {
        let msg = describe_submit(&EngineError::Upstream(
            503,
            "insufficient_vram: 10349 MiB free, need 11000".into(),
        ));
        assert!(msg.contains("10349"), "{msg}");
        assert!(msg.to_lowercase().contains("gpu scheduler"), "{msg}");
    }
}
