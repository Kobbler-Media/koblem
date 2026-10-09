//! Pixal3D rig — a photo (or a cached geometry) into a rigged LOD ladder.
//!
//! # 🔴 `Preempt::None`, and it is not a preference
//!
//! `kob-gpu-gate` deliberately refuses to unload this service. longcat-rig runs its **own FIFO and
//! admits work koblem cannot see**, so an unload driven from here races a render the service had
//! already accepted and kills it with nothing to resume from. The lane cap of 1 is koblem's belief
//! about the engine, not a mutex the engine honours. So this adapter is never preempted, and the
//! only checkpoint between rungs is a *cancel* check.
//!
//! # The ladder is submitted a rung at a time
//!
//! The unit the queue counts is a tier, and a tier is only a checkpoint if the job ends there. Rung
//! 0 pays for the geometry and the rig decode (~450 s); every later rung is submitted against the
//! finished job's `stage_dir` + `rig_cache` (~60–180 s) and therefore inherits the *same* skeleton
//! — which is exactly what `POST /rerig {"mode":"tiers"}` already does.
//!
//! 🔴 **BOTH handles, every rung.** The engine's re-run resolver maps `geometry_from` onto its
//! refined mesh but never sets the stage dir from `rig_cache_from`, so a rung that names only the
//! cache gets a *fresh* stage dir and re-runs geometry diffusion + DC remesh — a ~450 s rig wearing
//! a ~120 s bake's name. And the stage dir is NEW on every rung, so each one's own `stage_dir` is
//! what the next inherits; carrying the first one forward is the same bug one rung later.
//!
//! A rung that lands is fetched to koblem's disk immediately: the service's job **records** are
//! evicted at 512 while its files are never swept, so a tier not fetched now is a tier that exists
//! on `/out` and can never be served again.
//!
//! ⚠️ Two helpers ([`placement`], [`poll`]) live here rather than in a fourth module because
//! `pixal3d_motion` is the only other caller and the two are declared together.
//!
//! # Payload
//!
//! ```json
//! { "character_id": "chr-…", "tiers": ["hero","game"], "rig_seed": 1234,
//!   "image": "/out/jobs/…/input", "geometry_from": "/out/jobs/…/stage", … }
//! ```
//!
//! i.e. a [`RigSubmit`] plus the catalogue record it belongs to. A request with no `image` and no
//! re-run handle is a fresh upload, and its picture is read back off the character's own directory
//! — the route writes it there before it enqueues anything. `gpu` in the payload is ignored: the
//! card is the dispatcher's decision.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{Job, JobKind, JobProgress, Preempt};
use serde_json::{Value, json};
use tracing::warn;

use super::verdict;
use crate::AppState;
use crate::pixal3d::{self as engine, EngineJob, Pixal3dClient, RigSubmit};
use crate::pixal3d_jobs::{Character, CharacterTier, JobState as RecordState};
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::pixal3d::{
    TIERS, check_resolution, describe_engine_failure, describe_submit, resolve_tiers,
};

/// A rig publishes no progress at all, so a tighter interval buys requests and nothing else.
const RIG_POLL: Duration = Duration::from_secs(3);
/// The engine stops within ~7 s of a cancel; this is the wall before we stop waiting for it.
const CANCEL_CONFIRM: Duration = Duration::from_secs(20);
const CANCEL_POLL: Duration = Duration::from_secs(1);
/// The engine restarting loses its in-memory job records, which reads as a 404 forever — a run
/// that can no longer be observed has to end rather than hold the lane.
const MAX_POLL_ERRORS: u32 = 10;

pub struct Pixal3dRigAdapter {
    state: AppState,
}

impl Pixal3dRigAdapter {
    pub fn new(state: AppState) -> Self {
        Pixal3dRigAdapter { state }
    }
}

// ── shared with pixal3d_motion ───────────────────────────────────────────────

pub(super) enum Stopped {
    Cancelled,
    Failed(String),
}

/// Turn the dispatcher's card into the `gpu` a submit carries, having confirmed the service can
/// place a worker there.
///
/// 🔴 The check is "can it reach what we reserved", not "is it already there" — the supervisor
/// relocates per request. Refusing costs a fast error; not refusing costs a ~450 s rig on a card
/// nobody reserved while the reservation is held against another one. Mirrors
/// `Pixal3dClient::placement`, which takes an `Acquisition` this adapter never holds: the
/// dispatcher owns the permit and hands over a UUID.
pub(super) fn verdict_of(e: &engine::Error) -> Failure {
    match e {
        engine::Error::Request(e) => verdict::transport(e),
        // 503 here is the engine's own admission control — `queue_full`, or `insufficient_vram`
        // because a peer render still holds the card. Both end on their own.
        engine::Error::Upstream(status, _) => verdict::upstream(*status),
        // A card the scheduler and the service disagree about is a deployment fact, and `JobFailed`
        // is the rig's own sentence about a decode that will decode the same way again.
        engine::Error::NotConfigured
        | engine::Error::JobFailed(_)
        | engine::Error::Io(..)
        | engine::Error::GpuMismatch { .. }
        | engine::Error::MissingGpuIdentity { .. } => Failure::Permanent,
    }
}

pub(super) async fn placement(
    client: &Pixal3dClient,
    chosen: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(chosen) = chosen else {
        return Ok(None);
    };
    let health = client.health().await.map_err(|e| {
        verdict::note(
            format!(
                "the rig service did not answer /health, so the card the scheduler reserved could \
                 not be verified: {e}"
            ),
            verdict_of(&e),
        )
    })?;
    let supported = health.supported_gpus();
    if supported.is_empty() {
        return Err(engine::Error::MissingGpuIdentity {
            scheduled: chosen.to_string(),
        }
        .to_string());
    }
    if !supported.contains(&chosen) {
        return Err(engine::Error::GpuMismatch {
            scheduled: chosen.to_string(),
            actual: supported.join(", "),
        }
        .to_string());
    }
    Ok(Some(chosen.to_string()))
}

/// Poll one engine job to a terminal state, honouring the queue's cancel token.
pub(super) async fn poll(
    ctx: &JobCtx,
    client: &Pixal3dClient,
    engine_id: &str,
    interval: Duration,
) -> Result<EngineJob, Stopped> {
    let mut errors = 0_u32;
    loop {
        if ctx.cancel.get() == Interrupt::Cancel {
            // 🔴 The lane is released on the strength of this returning, so it waits for the
            // engine rather than for koblem's own opinion.
            if let Err(e) = cancel_and_confirm(client, engine_id).await {
                warn!(engine = %engine_id, error = %e, "pixal3d: cancel not confirmed");
            }
            return Err(Stopped::Cancelled);
        }
        match client.job(engine_id).await {
            Ok(job) if job.completed() => return Ok(job),
            Ok(job) if job.status == "cancelled" => return Err(Stopped::Cancelled),
            Ok(job) if job.is_terminal() => {
                let detail = job.error.unwrap_or_else(|| job.status.clone());
                let code = job.error_code.unwrap_or_default();
                return Err(Stopped::Failed(describe_engine_failure(&detail, &code)));
            }
            Ok(_) => errors = 0,
            Err(e) => {
                errors += 1;
                if errors >= MAX_POLL_ERRORS {
                    return Err(Stopped::Failed(format!(
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

/// Cancel, then wait until the engine says the job is over.
///
/// The POST only trips a `CancelToken`; the worker takes up to ~7 s to notice, and it holds the
/// card for all of it. Returning early would hand the lane to a job that then submits into a busy
/// service and reads as an engine bug.
pub(super) async fn cancel_and_confirm(
    client: &Pixal3dClient,
    engine_id: &str,
) -> Result<(), String> {
    client
        .cancel(engine_id)
        .await
        .map_err(|e| format!("the rig engine refused the cancel: {e}"))?;
    let deadline = Instant::now() + CANCEL_CONFIRM;
    while Instant::now() < deadline {
        match client.job(engine_id).await {
            Ok(job) if job.is_terminal() => return Ok(()),
            Ok(_) => {}
            // The record is gone (an engine restart, or eviction) — there is nothing left to wait
            // for and nothing left holding the card.
            Err(_) => return Ok(()),
        }
        tokio::time::sleep(CANCEL_POLL).await;
    }
    Err(format!(
        "the rig engine did not confirm the cancel of job {engine_id} within {} s",
        CANCEL_CONFIRM.as_secs()
    ))
}

// ── the payload ──────────────────────────────────────────────────────────────

struct RigPlan {
    character_id: String,
    tiers: Vec<String>,
    submit: RigSubmit,
}

fn plan(payload: &Value) -> Result<RigPlan, String> {
    let character_id = payload
        .get("character_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(
            "`character_id` is required — the catalogue record is where the tiers, the re-run \
             handles and the only durable copy of the GLBs land",
        )?
        .to_string();
    let mut submit: RigSubmit = serde_json::from_value(payload.clone())
        .map_err(|e| format!("this is not a pixal3d rig request: {e}"))?;
    // Empty ⇒ the whole ladder, which is the engine's own default for an absent `tiers`.
    let tiers = resolve_tiers(Some(submit.tiers.clone()), &TIERS)?;
    submit.tiers = tiers.clone();
    Ok(RigPlan {
        character_id,
        tiers,
        submit,
    })
}

fn ladder_of(payload: &Value) -> Vec<String> {
    plan(payload)
        .map(|p| p.tiers)
        .unwrap_or_else(|_| TIERS.iter().map(|t| (*t).to_string()).collect())
}

/// What lets rung k+1 inherit rung k's mesh and skeleton instead of re-decoding them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Handles {
    image: Option<String>,
    geometry_from: Option<String>,
    rig_cache_from: Option<String>,
}

/// The handles the NEXT rung inherits, read off the one that just finished.
///
/// 🔴 A completed rig always publishes both: the result object sets `stage_dir` and `rig_cache`
/// outside every conditional and outside the tier loop, and a job that fails publishes no result
/// at all. An absence here is therefore the engine having changed shape, not a single-rung bake
/// being quiet — and the old fallback of carrying the previous rung's handles forward would submit
/// a stale stage dir, which is the ~450 s-per-rung geometry re-run this ladder exists to avoid.
fn advance(result: &Value, tier: &str) -> Result<(String, String), String> {
    let (stage, cache) = engine::rerun_handles(result);
    match (stage, cache) {
        (Some(stage), Some(cache)) => Ok((stage, cache)),
        (stage, _) => Err(format!(
            "the `{tier}` rung completed but published no {}, which a finished rig always states. \
             The remaining tiers were not baked: submitting them against this rung's missing \
             handles would re-run the geometry stage and stop the ladder sharing one skeleton.",
            if stage.is_some() {
                "rig cache"
            } else {
                "stage directory"
            }
        )),
    }
}

/// One rung's request.
///
/// 🔴 `geometry_from` AND `rig_cache_from` travel together. The engine's re-run resolver maps
/// `geometry_from` onto its refined mesh but never sets the stage dir from the cache, so a rung
/// carrying only the cache re-runs geometry diffusion + DC remesh — ~450 s instead of ~120 s, per
/// rung, invisibly.
fn rung_submit(
    base: &RigSubmit,
    tier: &str,
    handles: &Handles,
    gpu: Option<&str>,
    inherits: bool,
) -> RigSubmit {
    let mut submit = RigSubmit {
        tiers: vec![tier.to_string()],
        image: handles.image.clone(),
        geometry_from: handles.geometry_from.clone(),
        rig_cache_from: handles.rig_cache_from.clone(),
        // 🔴 The card the scheduler reserved. A submit without it lands on the worker's own
        // WORKER_DEFAULT_GPU, which is how a worker ends up resident on an administratively
        // disabled card.
        gpu: gpu.map(str::to_string),
        ..base.clone()
    };
    if inherits {
        // A later rung must not re-run the decode: the ladder shares one skeleton, and a clip
        // authored on the hero has to keep playing on what comes out.
        submit.rig_seed = None;
        submit.geo_seed = None;
    }
    submit
}

impl Pixal3dRigAdapter {
    /// The picture a fresh upload rigs from. The route wrote it into the character's own directory
    /// before enqueueing, so the payload carries a name rather than 40 MB of base64.
    async fn source_image(&self, character: &Character) -> Result<(Bytes, String), String> {
        let Some(name) = character.source_filename.clone() else {
            return Err(
                "no image to rig from: the character kept no source image and the request names \
                 no engine-side path either"
                    .to_string(),
            );
        };
        let path = self.state.pixal3d_jobs.file_path(&character.id, &name);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("could not read the source image at {}: {e}", path.display()))?;
        Ok((Bytes::from(bytes), name))
    }

    /// One pass up the ladder. `in_flight` is the engine job a reattach re-enters at, which is
    /// always the first rung the catalogue record has not yet collected.
    async fn ladder(
        &self,
        ctx: &JobCtx,
        plan: RigPlan,
        mut in_flight: Option<String>,
    ) -> RunResult {
        let client = &self.state.pixal3d;
        let jobs = &self.state.pixal3d_jobs;
        if !client.is_configured() {
            return Err("pixal3d is not configured".to_string());
        }
        let cid = plan.character_id.clone();
        let Some(character) = jobs.character(&cid) else {
            return Err(format!(
                "no pixal3d character `{cid}` — without its record a finished rig has nowhere to land"
            ));
        };

        // A tier is on the record only once its GLB is on koblem's disk, so this is also what a
        // restart resumes from.
        let done: Vec<String> = character.tiers.iter().map(|t| t.tier.clone()).collect();
        let pending: Vec<String> = plan
            .tiers
            .iter()
            .filter(|t| !done.contains(t))
            .cloned()
            .collect();
        if pending.is_empty() {
            return Ok(JobOutcome::Done { warnings: vec![] });
        }

        let gpu = placement(client, ctx.gpu.as_deref()).await?;

        // The record wins over the payload: after a restart it holds the handles the finished rungs
        // published.
        let mut handles = Handles {
            image: character
                .engine_image
                .clone()
                .or_else(|| plan.submit.image.clone()),
            geometry_from: character
                .stage_dir
                .clone()
                .or_else(|| plan.submit.geometry_from.clone()),
            rig_cache_from: character
                .rig_cache
                .clone()
                .or_else(|| plan.submit.rig_cache_from.clone()),
        };

        let mut warnings: Vec<String> = Vec::new();
        let mut landed = done.len() as i32;
        let mut seconds = character.seconds.unwrap_or(0.0);

        jobs.update_character(&cid, |c| c.state = RecordState::Running);

        for (i, tier) in pending.iter().enumerate() {
            let inherits = i > 0 || !done.is_empty();
            let engine_id = match in_flight.take() {
                Some(id) => id,
                None => {
                    let submit =
                        rung_submit(&plan.submit, tier, &handles, gpu.as_deref(), inherits);
                    let submitted = if submit.image.is_none() && submit.geometry_from.is_none() {
                        let (bytes, filename) = self.source_image(&character).await?;
                        client.rig_from_image(bytes, &filename, &submit).await
                    } else {
                        client.rig_from_previous(&submit).await
                    };
                    match submitted {
                        Ok(id) => id,
                        Err(e) => return Err(verdict::note(describe_submit(&e), verdict_of(&e))),
                    }
                }
            };

            // The one field that makes a reattach possible, written before anything else can go
            // wrong. The catalogue copy is what the existing cancel route reads.
            if let Err(e) = store::set_server_job_id(&self.state.db, &ctx.job.id, &engine_id).await
            {
                warn!(job = %ctx.job.id, error = %e, "pixal3d_rig: could not record the engine job id");
            }
            jobs.update_character(&cid, |c| c.engine_job_id = Some(engine_id.clone()));
            ctx.progress(
                JobProgress {
                    phase: Some(format!("rigging the {tier} tier")),
                    ..Default::default()
                },
                landed,
            )
            .await;

            let polled = poll(ctx, client, &engine_id, RIG_POLL).await;
            let job = match polled {
                Ok(job) => job,
                Err(Stopped::Cancelled) => {
                    ctx.release_gpu();
                    jobs.update_character(&cid, |c| {
                        c.state = RecordState::Cancelled;
                        c.error = Some("cancelled".into());
                    });
                    return Ok(JobOutcome::Cancelled);
                }
                Err(Stopped::Failed(msg)) => {
                    ctx.release_gpu();
                    jobs.update_character(&cid, |c| {
                        c.state = RecordState::Failed;
                        c.error = Some(msg.clone());
                    });
                    return Err(msg);
                }
            };

            // 🔴 This rung's OWN handles, unconditionally: the stage dir is minted fresh per rung,
            // and `rig_cache_from` hands the source directory back aliased rather than copied, so
            // rung k+1 chaining onto exactly what rung k published is idempotent by construction.
            let (stage, cache) = match advance(&job.result, tier) {
                Ok(handles) => handles,
                Err(msg) => {
                    ctx.release_gpu();
                    jobs.update_character(&cid, |c| {
                        c.state = RecordState::Failed;
                        c.error = Some(msg.clone());
                    });
                    return Err(msg);
                }
            };
            handles.geometry_from = Some(stage);
            handles.rig_cache_from = Some(cache);
            if handles.image.is_none() {
                handles.image = job
                    .result
                    .get("image")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            warnings.extend(engine::warnings_of(&job.result));
            seconds += job.seconds;
            let peak = job.result.get("peak_vram_used_mib").and_then(Value::as_i64);

            let last = i + 1 == pending.len();
            if last {
                // 🔴 THE ENGINE/DISK BOUNDARY. Everything past here is a copy off a volume; the
                // card must not idle through it, and the job stops being cancellable.
                ctx.release_gpu();
                jobs.update_character(&cid, |c| c.state = RecordState::Collecting);
            }

            let delivered = engine::tiers_of(&job.result);
            let Some(et) = delivered
                .iter()
                .find(|t| &t.tier == tier)
                .or_else(|| delivered.first())
            else {
                warnings.push(format!(
                    "the `{tier}` rung completed but the engine listed no tier for it, so there \
                     was nothing to fetch."
                ));
                if last {
                    break;
                }
                continue;
            };

            let filename = format!("{}.glb", et.tier);
            let dest = jobs.file_path(&cid, &filename);
            match client
                .fetch_result_to(&engine_id, Some(&et.tier), &dest)
                .await
            {
                Ok(bytes) => {
                    landed += 1;
                    let tier_row = CharacterTier {
                        tier: et.tier.clone(),
                        decimate: et.decimate,
                        joints: et.joints,
                        named_core: et.named_core,
                        seconds: et.seconds,
                        skin_cleanup: et.skin_cleanup.clone(),
                        gate: et.gate.clone(),
                        filename,
                        bytes,
                        glb_path: et.glb.clone(),
                    };
                    let (stage, cache, img) = (
                        handles.geometry_from.clone(),
                        handles.rig_cache_from.clone(),
                        handles.image.clone(),
                    );
                    jobs.update_character(&cid, |c| {
                        c.tiers.retain(|t| t.tier != tier_row.tier);
                        c.tiers.push(tier_row);
                        c.stage_dir = stage;
                        c.rig_cache = cache;
                        if c.engine_image.is_none() {
                            c.engine_image = img;
                        }
                        if peak.is_some() {
                            c.peak_vram_mib = peak;
                        }
                        c.seconds = Some(seconds);
                    });
                    ctx.progress(
                        JobProgress {
                            phase: Some(format!("collected the {} tier", et.tier)),
                            ..Default::default()
                        },
                        landed,
                    )
                    .await;
                }
                Err(e) => {
                    // One rung failing to come across is not the ladder failing — but it has to be
                    // said, or the tier is silently absent from a character reporting success.
                    warn!(character = %cid, tier = %et.tier, error = %e,
                          "pixal3d_rig: could not fetch a delivered tier");
                    warnings.push(format!(
                        "the `{}` tier rigged but could not be fetched from the engine: {e}",
                        et.tier
                    ));
                }
            }

            if last {
                break;
            }
            if ctx.cancel.get() == Interrupt::Cancel {
                jobs.update_character(&cid, |c| {
                    c.state = RecordState::Cancelled;
                    c.error = Some("cancelled between tiers".into());
                });
                return Ok(JobOutcome::Cancelled);
            }
        }

        // Anything that reaches here has passed the boundary — `last` is the only way out of the
        // loop and it releases first.
        ctx.release_gpu();
        let empty = landed == 0;
        let carried = warnings.clone();
        jobs.update_character(&cid, |c| {
            c.state = if empty {
                RecordState::Failed
            } else {
                RecordState::Done
            };
            if empty {
                c.error = Some(
                    "the rig completed but no tier could be fetched; the engine's artifacts are \
                     still on /out under this character's job"
                        .into(),
                );
            }
            c.warnings.extend(carried);
        });
        if empty {
            return Err(
                "the rig completed but no tier could be fetched — its artifacts are still on the \
                 engine's /out volume"
                    .to_string(),
            );
        }

        // Read back rather than accumulated: the record is what the catalogue serves, and a rerig
        // keeps rungs this run never touched.
        let tiers = jobs
            .character(&cid)
            .map(|c| serde_json::to_value(&c.tiers).unwrap_or_else(|_| json!([])))
            .unwrap_or_else(|| json!([]));
        let row = json!({ "character_id": cid, "tiers": tiers });
        if let Err(e) = store::set_result(&self.state.db, &ctx.job.id, &row).await {
            warn!(job = %ctx.job.id, error = %e, "pixal3d-rig: could not record the result");
        }
        Ok(JobOutcome::Done { warnings })
    }
}

#[async_trait]
impl JobRunner for Pixal3dRigAdapter {
    fn kind(&self) -> JobKind {
        JobKind::Pixal3dRig
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::Pixal3d
    }

    /// 🔴 NEVER preempted, and this is the gate's own decision reflected back: longcat-rig has its
    /// own FIFO and admits work koblem cannot see, so an unload racing a job it cannot see kills a
    /// render the service had already accepted. Lane cap 1 is advisory here.
    fn preempt(&self) -> Preempt {
        Preempt::None
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        let plan = plan(payload)?;
        check_resolution(plan.submit.resolution)
    }

    fn shape_key(&self, payload: &Value) -> String {
        format!("pixal3d_rig/{}tier", ladder_of(payload).len())
    }

    /// One unit per rung: ~450 s for the first, ~60–180 s each after.
    fn units(&self, payload: &Value) -> Option<i32> {
        Some(ladder_of(payload).len() as i32)
    }

    fn label(&self, payload: &Value) -> String {
        let mode = if payload.get("rig_cache_from").is_some() {
            "more tiers"
        } else if payload.get("geo_seed").is_some() {
            "re-mesh"
        } else if payload.get("geometry_from").is_some() {
            "re-rig"
        } else {
            "rig"
        };
        format!("pixal3d {mode} · {}", ladder_of(payload).join("+"))
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let plan = plan(&ctx.payload)?;
        self.ladder(&ctx, plan, None).await
    }

    /// 🔴 RE-ATTACHES, IT DOES NOT RESTART. The engine is a separate process and kept working
    /// through koblem's restart; a ladder is up to ten minutes of GPU and re-submitting throws it
    /// away. The handle is the engine JOB ID (`server_job_id`) — the same thing the route's own
    /// reattach re-polls. The published PATHS are the re-*run* handles for a later rung, not a way
    /// back into a job already in flight.
    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        let plan = plan(&ctx.payload)?;
        let Some(sid) = ctx.job.server_job_id.clone() else {
            let cid = plan.character_id.clone();
            self.state.pixal3d_jobs.update_character(&cid, |c| {
                c.state = RecordState::Failed;
                c.error = Some("koblem restarted before the rig engine accepted this job".into());
            });
            return Err(
                "koblem restarted before the rig engine accepted this job, so there is nothing to \
                 re-attach to"
                    .to_string(),
            );
        };
        self.ladder(&ctx, plan, Some(sid)).await
    }

    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(sid) = job.server_job_id.as_deref() else {
            return Ok(());
        };
        cancel_and_confirm(&self.state.pixal3d, sid).await
    }

    /// 🔴 The catalogue record, not the queue row. A rig cancelled while it was still queued left
    /// the character sitting at `queued` on the 3D page with nothing to move it and no control to
    /// clear it — the rig route reads the record directly and has never folded the queue row over
    /// it, so unlike `/ltx-video/jobs` there was not even a reader-side rescue.
    async fn abandon(&self, _job_id: &str, payload: &Value) {
        let Some(cid) = payload.get("character_id").and_then(Value::as_str) else {
            return;
        };
        self.state
            .pixal3d_jobs
            .cancel_character(cid, "cancelled before the rig started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The key is the ladder LENGTH, never the tier names: a 2-rung ladder costs about the same
    /// whichever two rungs it is, and keying on names would give every combination its own sample
    /// of one.
    #[test]
    fn the_shape_is_the_ladder_length() {
        let two = json!({"character_id": "chr-1", "tiers": ["hero", "game"]});
        let also_two = json!({"character_id": "chr-2", "tiers": ["high", "medium"]});
        assert_eq!(ladder_of(&two).len(), ladder_of(&also_two).len());
        let four = json!({"character_id": "chr-3", "tiers": []});
        assert_eq!(
            ladder_of(&four).len(),
            4,
            "empty ⇒ the engine's full ladder"
        );
    }

    /// The ladder is kept in the engine's build order, not the caller's: rung 0 is the one that
    /// pays for the geometry, so which rung is first is not cosmetic.
    #[test]
    fn the_ladder_keeps_the_engines_build_order() {
        let p = plan(&json!({"character_id": "chr-1", "tiers": ["game", "hero"]})).expect("valid");
        assert_eq!(p.tiers, vec!["hero".to_string(), "game".to_string()]);
        assert_eq!(p.submit.tiers, p.tiers);
    }

    /// 🔴 The ~450 s-per-rung bug, as an assertion: the rung after the first must name BOTH the
    /// stage dir and the rig cache, because `rig_cache_from` alone keeps the skeleton and re-runs
    /// the whole geometry stage anyway.
    #[test]
    fn a_later_rung_names_both_handles_and_moves_no_seed() {
        let base = RigSubmit {
            tiers: vec!["hero".into(), "game".into()],
            rig_seed: Some(7),
            geo_seed: Some(3),
            ..Default::default()
        };
        let handles = Handles {
            image: Some("/out/jobs/a/input.png".into()),
            geometry_from: Some("/out/jobs/a/stage".into()),
            rig_cache_from: Some("/out/jobs/a/rig_cache".into()),
        };

        let first = rung_submit(&base, "hero", &Handles::default(), Some("GPU-1"), false);
        assert_eq!(first.tiers, vec!["hero".to_string()]);
        assert_eq!(first.rig_seed, Some(7), "rung 0 draws the skeleton");
        assert_eq!(first.gpu.as_deref(), Some("GPU-1"));

        let next = rung_submit(&base, "game", &handles, Some("GPU-1"), true);
        assert_eq!(next.tiers, vec!["game".to_string()]);
        assert_eq!(next.geometry_from.as_deref(), Some("/out/jobs/a/stage"));
        assert_eq!(
            next.rig_cache_from.as_deref(),
            Some("/out/jobs/a/rig_cache")
        );
        assert_eq!(next.image.as_deref(), Some("/out/jobs/a/input.png"));
        assert!(next.rig_seed.is_none() && next.geo_seed.is_none());
    }

    /// Each rung mints its OWN stage dir, so the ladder threads what the rung just published —
    /// carrying the first rung's forward is the same silent geometry re-run one step later.
    #[test]
    fn the_ladder_threads_each_rungs_own_stage_dir() {
        let first = advance(
            &json!({"stage_dir": "/out/jobs/a/stage", "rig_cache": "/out/jobs/a/rig_cache"}),
            "hero",
        )
        .expect("a finished rig publishes both");
        let mut handles = Handles {
            image: Some("/out/jobs/a/input.png".into()),
            geometry_from: Some(first.0),
            rig_cache_from: Some(first.1),
        };
        // Rung 1's bake: a new job dir, and a rig cache aliased back to the one it was handed.
        let (stage, cache) = advance(
            &json!({"stage_dir": "/out/jobs/b/stage", "rig_cache": "/out/jobs/a/rig_cache"}),
            "game",
        )
        .expect("a bake publishes both too");
        handles.geometry_from = Some(stage);
        handles.rig_cache_from = Some(cache);
        let next = rung_submit(&RigSubmit::default(), "medium", &handles, None, true);
        assert_eq!(next.geometry_from.as_deref(), Some("/out/jobs/b/stage"));
        assert_eq!(
            next.rig_cache_from.as_deref(),
            Some("/out/jobs/a/rig_cache")
        );
    }

    /// A completed rig publishes both handles unconditionally, so an absence is the engine having
    /// changed shape — an error, never a rung quietly submitted against stale handles.
    #[test]
    fn a_successful_rung_without_its_handles_is_an_error() {
        let missing_cache = advance(&json!({"stage_dir": "/out/jobs/a/stage"}), "hero")
            .expect_err("half the handles is not chainable");
        assert!(missing_cache.contains("rig cache"), "{missing_cache}");
        let missing_stage = advance(&json!({"rig_cache": "/out/jobs/a/rig_cache"}), "hero")
            .expect_err("half the handles is not chainable");
        assert!(missing_stage.contains("stage directory"), "{missing_stage}");
        assert!(advance(&json!({}), "hero").is_err());
    }

    /// A payload with no catalogue record cannot be run: the tiers, the re-run handles and the only
    /// durable copy of the GLBs all live on it.
    #[test]
    fn a_rig_without_a_character_is_refused_at_enqueue() {
        assert!(plan(&json!({"tiers": ["hero"]})).is_err());
        assert!(plan(&json!({"character_id": "  ", "tiers": ["hero"]})).is_err());
        assert!(plan(&json!({"character_id": "chr-1", "tiers": ["ultra"]})).is_err());
    }

    /// 🔴 `insufficient_vram` arrives as a 503 and is genuinely temporary — a peer render is
    /// holding the card the rig needs all 11 GB of, and it ends. A card the scheduler and the
    /// service disagree about does not.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: engine::Error| verdict_of(&e);
        assert_eq!(
            t(engine::Error::Upstream(503, "insufficient_vram".into())),
            Failure::Transient
        );
        assert_eq!(
            t(engine::Error::Upstream(503, "queue_full".into())),
            Failure::Transient
        );
        assert_eq!(
            t(engine::Error::Upstream(429, "too many".into())),
            Failure::Transient
        );
        assert_eq!(
            t(engine::Error::Upstream(400, "bad tier".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(engine::Error::Upstream(404, "no geometry".into())),
            Failure::Permanent
        );
        assert_eq!(t(engine::Error::NotConfigured), Failure::Permanent);
        assert_eq!(
            t(engine::Error::JobFailed("zero-skin rig".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(engine::Error::Io("out.glb".into(), "ENOSPC".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(engine::Error::GpuMismatch {
                scheduled: "0".into(),
                actual: "1".into()
            }),
            Failure::Permanent
        );
        assert_eq!(
            t(engine::Error::MissingGpuIdentity {
                scheduled: "0".into()
            }),
            Failure::Permanent
        );
    }

    #[test]
    fn the_verdict_survives_describe_submit() {
        let e = engine::Error::Upstream(503, "queue_full on the rig service".into());
        assert_eq!(
            verdict::recall(&verdict::note(describe_submit(&e), verdict_of(&e))),
            Failure::Transient
        );
        let e = engine::Error::Upstream(400, "queue_full on the rig service".into());
        assert_eq!(
            verdict::recall(&verdict::note(describe_submit(&e), verdict_of(&e))),
            Failure::Permanent
        );
    }
}
