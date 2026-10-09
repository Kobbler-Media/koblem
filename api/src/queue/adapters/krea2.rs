//! Krea-2-Turbo images.
//!
//! Shape A (poll-and-collect). The payload IS the engine's `/sdcpp/v1/img_gen`
//! body as `routes::krea2::build_img_gen_body` produces it, plus one koblem-side
//! key the engine never sees: `remove_background`, lifted out before submit and
//! honoured after (dropping it silently would turn a cutout render into a plain
//! one with a 200).

use std::time::Duration;

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{BackgroundRemoval, Job, JobKind, JobProgress, Preempt};
use serde_json::{Value, json};
use tracing::warn;

use super::verdict;
use crate::AppState;
use crate::director_assets::{self, PartUploads};
use crate::krea2::{Error as Krea2Error, Krea2RenderResult};
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::images::{
    BgRemovalOutcome, engine_images, gallery_files, gallery_result, image_ext_mime,
    run_background_removal,
};
use crate::routes::tier2_helpers::{inject_gpu, persist_generation};

const SERVICE: &str = "krea2";
const CANCELLED: &str = "cancelled";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// The engine answers `/cancel` with 409 while it still holds the render. The lane
/// is released on the strength of a confirmed cancel, so 409 is retried, not
/// reported.
const CANCEL_TRIES: u32 = 20;
const CANCEL_RETRY: Duration = Duration::from_millis(500);
/// Both sides of the VAE (8x downsample) and the DiT patchify (2x). Refused rather
/// than snapped: a request whose canvas the queue rewrote at 03:00 is not the
/// request anyone typed.
const DIM_MULTIPLE: u32 = 16;

pub struct Krea2Adapter {
    state: AppState,
}

fn verdict_of(e: &Krea2Error) -> Failure {
    match e {
        Krea2Error::Request(e) => verdict::transport(e),
        Krea2Error::Upstream(status, _) => verdict::upstream(*status),
        // `JobFailed` carries the engine's own sentence about why sampling died, and
        // it is not read: a prompt or a canvas that killed the DiT kills it again.
        // The rest cannot change between attempts either — a missing base_url, a
        // cancel, a field the engine never sent, an image koblem cannot decode.
        Krea2Error::NotConfigured
        | Krea2Error::JobFailed(..)
        | Krea2Error::Cancelled(_)
        | Krea2Error::MissingField(_)
        | Krea2Error::BadImage(_) => Failure::Permanent,
    }
}

impl Krea2Adapter {
    pub fn new(state: AppState) -> Self {
        Krea2Adapter { state }
    }

    /// Poll to completion and collect the images.
    ///
    /// 🔴 The collect happens HERE, under the guard, not after `release_gpu`. A
    /// finished sdcpp image lives in the CUDA child's RAM until it is fetched —
    /// whatever takes the card next can unload that child and take the render with
    /// it — so for this fleet the engine/disk boundary is after the media fetch,
    /// which is exactly where `routes::krea2` drops its own guard.
    async fn poll(&self, ctx: &JobCtx, sid: &str) -> Result<Krea2RenderResult, String> {
        let client = &self.state.krea2;
        loop {
            if ctx.cancel.get() == Interrupt::Cancel {
                self.confirm_cancel(sid).await?;
                return Err(CANCELLED.to_string());
            }
            let body = client
                .poll(sid)
                .await
                .map_err(|e| verdict::note(e.to_string(), verdict_of(&e)))?;
            match body.get("status").and_then(Value::as_str) {
                Some("completed") => {
                    let parsed = crate::krea2::parse_result(&body).map_err(|e| e.to_string())?;
                    return client
                        .collect_images(sid, parsed)
                        .await
                        .map_err(|e| verdict::note(e.to_string(), verdict_of(&e)));
                }
                // The engine's own sentence about why the render died — foreign prose
                // with no typed value behind it, so it is reported and not read.
                Some("failed") => {
                    return Err(body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("generation failed")
                        .to_string());
                }
                Some("cancelled") => return Err(CANCELLED.to_string()),
                // 🔴 `queued`, `generating` and ANYTHING ELSE. An unknown status maps
                // to "still running" exactly as the client's own loop does; treating
                // it as terminal would make a new engine status a silent data loss.
                phase => {
                    let phase = phase.unwrap_or("generating").to_string();
                    // No numbers: no sd.cpp-fork deployment publishes a step count,
                    // and a 0/0 bar reads as finished.
                    ctx.progress(
                        JobProgress {
                            phase: Some(phase),
                            ..Default::default()
                        },
                        0,
                    )
                    .await;
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// 🔴 Does not return until the engine has confirmed. See [`CANCEL_TRIES`].
    async fn confirm_cancel(&self, sid: &str) -> Result<(), String> {
        for attempt in 0..CANCEL_TRIES {
            match self.state.krea2.cancel(sid).await {
                Ok(_) => return Ok(()),
                // The record is gone (404) or expired (410): nothing is holding the
                // worker, which is the question being asked.
                Err(Krea2Error::Upstream(404 | 410, _)) => return Ok(()),
                Err(Krea2Error::Upstream(409, _)) if attempt + 1 < CANCEL_TRIES => {
                    tokio::time::sleep(CANCEL_RETRY).await;
                }
                Err(e) => return Err(format!("cancelling krea2 job {sid}: {e}")),
            }
        }
        Err(format!("krea2 never confirmed the cancel of job {sid}"))
    }

    /// Optional chained background removal, then the gallery row. Runs in
    /// `collecting`: the krea2 card is already free (matting is its own budget
    /// line) and every byte is in hand.
    async fn collect(&self, ctx: &JobCtx, result: &Krea2RenderResult) -> Result<String, String> {
        let payload = &ctx.payload;
        let rendered = engine_images(result.images.iter().map(|i| i.bytes.clone()))
            .map_err(|msg| format!("krea2-server: {msg}"))?;

        let bg = payload
            .get("remove_background")
            .cloned()
            .and_then(|v| serde_json::from_value::<BackgroundRemoval>(v).ok())
            .filter(|bg| bg.enabled);
        let bg_outcome = match bg {
            Some(bg) => run_background_removal(&self.state, &bg, &rendered).await,
            None => BgRemovalOutcome::default(),
        };
        if let Some(e) = bg_outcome.error.as_deref() {
            warn!(service = SERVICE, error = %e, "background removal failed");
        }

        let (img_ext, img_mime) = image_ext_mime(Some(result.output_format.as_str()));
        let (files, slots) = gallery_files(&rendered, &bg_outcome.cutouts, img_ext, img_mime);
        let id = persist_generation(&self.state, SERVICE, "generate", meta(payload), &files)
            .await
            .ok_or_else(|| {
                "render succeeded but saving to the gallery failed — check `generations_dir` is \
                 set and writable"
                    .to_string()
            })?;

        let row = gallery_result(&id, &slots, &bg_outcome);
        if let Err(e) = store::set_result(&self.state.db, &ctx.job.id, &row).await {
            warn!(job = %ctx.job.id, error = %e, "krea2: could not record the result");
        }
        Ok(id)
    }
}

/// What the gallery row records. The engine body minus the reference images: those
/// are megabytes of base64 and `request_meta` is replayed into a form.
fn meta(payload: &Value) -> Value {
    let mut meta = payload.clone();
    if let Some(obj) = meta.as_object_mut() {
        let refs = obj
            .remove("ref_images")
            .and_then(|v| v.as_array().map(Vec::len))
            .unwrap_or(0);
        obj.insert("had_reference_images".into(), json!(refs));
        obj.insert("engine".into(), json!(SERVICE));
    }
    meta
}

fn longest_side(payload: &Value) -> i64 {
    let w = payload.get("width").and_then(Value::as_i64).unwrap_or(1024);
    let h = payload
        .get("height")
        .and_then(Value::as_i64)
        .unwrap_or(1024);
    w.max(h)
}

fn steps(payload: &Value) -> i64 {
    payload.get("steps").and_then(Value::as_i64).unwrap_or(8)
}

#[async_trait]
impl JobRunner for Krea2Adapter {
    fn kind(&self) -> JobKind {
        JobKind::Krea2
    }

    /// 🔴 Its own budget line, not a second flux slot: the ~7.75 GiB nvfp4 DiT
    /// cannot co-reside with FLUX.2 on the 5060 Ti.
    fn service(&self) -> ServiceKind {
        ServiceKind::Krea2
    }

    /// Seconds to a couple of minutes, and no checkpoint surface — requeue whole.
    fn preempt(&self) -> Preempt {
        Preempt::Kill
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        if payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            return Err("a prompt is required".to_string());
        }
        for side in ["width", "height"] {
            let v = payload.get(side).and_then(Value::as_u64).unwrap_or(0);
            if v == 0 {
                return Err(format!("{side} is required and must be positive"));
            }
            if !(v as u32).is_multiple_of(DIM_MULTIPLE) {
                return Err(format!(
                    "{side} {v} is not a multiple of {DIM_MULTIPLE} — the VAE downsamples 8x and \
                     the DiT patchifies 2x on top of it. Send a canvas the engine can render \
                     rather than one the queue silently rewrites."
                ));
            }
        }
        if steps(payload) < 1 {
            return Err("steps must be at least 1".to_string());
        }
        Ok(())
    }

    /// Pixels x steps is the whole cost model for a distilled image DiT. The batch
    /// multiplies it and is `units_total`, so it deliberately stays out of the key.
    fn shape_key(&self, payload: &Value) -> String {
        format!("krea2/{}/{}steps", longest_side(payload), steps(payload))
    }

    fn units(&self, payload: &Value) -> Option<i32> {
        payload
            .get("batch_count")
            .and_then(Value::as_i64)
            .map(|n| n.max(1) as i32)
            .or(Some(1))
    }

    fn label(&self, payload: &Value) -> String {
        let prompt: String = payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(48)
            .collect();
        let batch = self.units(payload).unwrap_or(1);
        let w = payload.get("width").and_then(Value::as_i64).unwrap_or(0);
        let h = payload.get("height").and_then(Value::as_i64).unwrap_or(0);
        if batch > 1 {
            format!("krea2 · {batch} images · {w}x{h} · {prompt}")
        } else {
            format!("krea2 · {w}x{h} · {prompt}")
        }
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.krea2;
        if !client.is_configured() {
            return Err("krea2 is not configured".to_string());
        }

        let mut body = ctx.payload.clone();
        // `asset:<hash>` references become `part:` markers plus their bytes, so a
        // Director still enqueued at 22:00 renders from the store at 03:00 instead
        // of carrying megabytes of base64 through the job row.
        let staged =
            director_assets::stage_refs(&self.state.director_assets_dir, &mut body, "ref_")
                .await
                .map_err(|missing| director_assets::missing_refs_message(&missing))?;
        let mut uploads = PartUploads::default();
        uploads.absorb(staged);
        uploads.retain_used_by(&body);

        // Not an engine field. Stripped from the SUBMITTED body only — `ctx.payload`
        // keeps it, and `collect` reads it from there.
        if let Some(obj) = body.as_object_mut() {
            obj.remove("remove_background");
        }

        // 🔴 The card the dispatcher placed us on. Omitting it lands the worker on
        // its own WORKER_DEFAULT_GPU — which is how a render ends up resident on an
        // administratively disabled card.
        inject_gpu(&mut body, ctx.gpu.as_deref());

        let sid = client
            .submit(&body, uploads.entries())
            .await
            .map_err(|e| verdict::note(e.to_string(), verdict_of(&e)))?;
        // The one field that makes reattach possible, written before anything else
        // can go wrong.
        if let Err(e) = store::set_server_job_id(&self.state.db, &ctx.job.id, &sid).await {
            warn!(job = %ctx.job.id, error = %e, "krea2: could not record the engine job id");
        }

        let poll = self.poll(&ctx, &sid).await;
        // 🔴 THE ENGINE/DISK BOUNDARY — the images are in koblem's hands now.
        ctx.release_gpu();

        let result = match poll {
            Ok(r) => r,
            Err(e) if e == CANCELLED => return Ok(JobOutcome::Cancelled),
            Err(e) => return Err(e),
        };
        let images = result.images.len() as i32;
        self.collect(&ctx, &result).await?;
        ctx.progress(
            JobProgress {
                phase: Some("completed".into()),
                ..Default::default()
            },
            images,
        )
        .await;
        Ok(JobOutcome::Done { warnings: vec![] })
    }

    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        let Some(sid) = ctx.job.server_job_id.clone() else {
            return Err("reattach with no engine job id".to_string());
        };
        let poll = self.poll(&ctx, &sid).await;
        ctx.release_gpu();
        let result = match poll {
            Ok(r) => r,
            Err(e) if e == CANCELLED => return Ok(JobOutcome::Cancelled),
            Err(e) => return Err(e),
        };
        let images = result.images.len() as i32;
        // A reattached render is still a render someone asked for; skipping the
        // gallery row would lose it purely because koblem restarted.
        self.collect(&ctx, &result).await?;
        ctx.progress(
            JobProgress {
                phase: Some("completed".into()),
                ..Default::default()
            },
            images,
        )
        .await;
        Ok(JobOutcome::Done { warnings: vec![] })
    }

    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(sid) = job.server_job_id.as_deref() else {
            return Ok(());
        };
        self.confirm_cancel(sid).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `shape_key` reads only the payload, so it is exercised without a client.
    fn key(payload: &Value) -> String {
        format!("krea2/{}/{}steps", longest_side(payload), steps(payload))
    }

    /// The batch multiplies the shape; it must not BE the shape, or a 4-image
    /// render can never be estimated from 1-image history.
    #[test]
    fn the_batch_is_not_in_the_key() {
        let a = json!({"width": 1024, "height": 1024, "steps": 8, "batch_count": 1});
        let b = json!({"width": 1024, "height": 1024, "steps": 8, "batch_count": 4});
        assert_eq!(key(&a), key(&b));
        assert_ne!(
            key(&a),
            key(&json!({"width": 2048, "height": 1024, "steps": 8}))
        );
        assert_ne!(
            key(&a),
            key(&json!({"width": 1024, "height": 1024, "steps": 20}))
        );
    }

    /// A portrait and a landscape of the same longest side cost the same.
    #[test]
    fn orientation_shares_a_bucket() {
        assert_eq!(
            key(&json!({"width": 1024, "height": 1536, "steps": 8})),
            key(&json!({"width": 1536, "height": 1024, "steps": 8})),
        );
    }

    /// 🔴 The sd.cpp fleet answers 429 when its pending queue is at 64 — that is
    /// back-pressure and worth waiting out. A canvas it refuses is refused again.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        for code in [429, 503, 500] {
            let e = Krea2Error::Upstream(code, "engine said so".into());
            assert_eq!(verdict_of(&e), Failure::Transient, "{code}");
        }
        for code in [400, 404, 409, 410, 422] {
            let e = Krea2Error::Upstream(code, "engine said so".into());
            assert_eq!(verdict_of(&e), Failure::Permanent, "{code}");
        }
        for e in [
            Krea2Error::NotConfigured,
            Krea2Error::Cancelled("j".into()),
            Krea2Error::JobFailed("j".into(), "sampler raised".into()),
            Krea2Error::MissingField("images"),
            Krea2Error::BadImage("truncated webp".into()),
        ] {
            assert_eq!(verdict_of(&e), Failure::Permanent, "{e}");
        }
    }

    /// The queue only ever sees the flattened sentence, so the verdict has to
    /// survive the flattening.
    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = Krea2Error::Upstream(429, "krea2 queue is full".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Transient);
        let e = Krea2Error::Upstream(400, "krea2 queue is full".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Permanent);
    }
}
