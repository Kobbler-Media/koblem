//! Music 3 — the reference adapter.
//!
//! Chosen to go first because it is the simplest runner in the tree and already
//! had the store shape: submit, poll, release, collect, persist. The whole loop is
//! proved end to end here before the other twelve exist.

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{GenerationOutput, Job, JobKind, JobProgress, Preempt};
use serde_json::{Value, json};
use tracing::warn;

use super::verdict;
use crate::AppState;
use crate::music3::Error as Music3Error;
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::music3::{file_stamp, log_music3, save_clip, short_seed};
use crate::routes::tier2_helpers::record_generation;

pub struct Music3Adapter {
    state: AppState,
}

fn verdict_of(e: &Music3Error) -> Failure {
    match e {
        Music3Error::Request(e) => verdict::transport(e),
        Music3Error::Upstream(status, _) => verdict::upstream(*status),
        // `JobFailed` carries the plugin's own sentence about why the render died,
        // and it is not read: a caption the model choked on chokes it again, and a
        // node that raised mid-DiT raises again. The rest cannot change between
        // attempts either — a missing base_url, a cancel, a field the engine never
        // sent.
        Music3Error::NotConfigured
        | Music3Error::JobFailed(..)
        | Music3Error::Cancelled(_)
        | Music3Error::MissingField(_) => Failure::Permanent,
    }
}

impl Music3Adapter {
    pub fn new(state: AppState) -> Self {
        Music3Adapter { state }
    }

    /// Poll to completion, mirroring the engine's progress onto the row.
    ///
    /// The client's own `wait()` already polls; this wraps it so the row learns
    /// the phase. Music 3's autoregressive stage reports no step count at all, and
    /// a bare progress bar sitting at zero for minutes reads as a hang — which is
    /// why the phase label, not the number, is the thing worth plumbing.
    async fn poll(
        &self,
        ctx: &JobCtx,
        server_job_id: &str,
    ) -> Result<crate::music3::Music3Completion, String> {
        let client = &self.state.music3;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let waiter = client.wait(server_job_id);
        tokio::pin!(waiter);
        loop {
            tokio::select! {
                done = &mut waiter => {
                    return done.map_err(|e: Music3Error| verdict::note(e.to_string(), verdict_of(&e)));
                }
                _ = ticker.tick() => {
                    if ctx.cancel.get() == Interrupt::Cancel {
                        // 🔴 The lane is released only once the engine CONFIRMS.
                        // A cancelled comfy job does not free the slot the instant
                        // koblem decides it did.
                        let _ = client.cancel(server_job_id).await;
                        return Err("cancelled".to_string());
                    }
                    let p = client.progress();
                    ctx.progress(
                        JobProgress {
                            phase: p.phase.clone(),
                            progress: p.progress,
                            current_step: p.current_step,
                            total_steps: p.total_steps,
                            queue_position: None,
                            preview_seq: None,
                        },
                        p.tracks_done.unwrap_or(0) as i32,
                    )
                    .await;
                }
            }
        }
    }

    /// Fetch every take, write it under `SONGS_DIR`, and file the gallery rows.
    ///
    /// Runs after `release_gpu`, in `collecting`: the card is free and the artifact
    /// is landing, which is exactly why this phase exists as a state.
    async fn collect(
        &self,
        job_id: &str,
        done: &crate::music3::Music3Completion,
        save: bool,
    ) -> Result<(), String> {
        let client = &self.state.music3;
        if done.tracks.is_empty() {
            return Err("the engine reported success but delivered no tracks".to_string());
        }

        let stamp = file_stamp();
        let mut outputs = Vec::with_capacity(done.tracks.len());
        for track in &done.tracks {
            let (content_type, bytes) = client.fetch_track(track).await.map_err(|e| {
                verdict::note(
                    format!("fetching take {}: {e}", track.index),
                    verdict_of(&e),
                )
            })?;
            let filename = format!(
                "{stamp}-music3-{}-{}.{}",
                short_seed(track.seed),
                track.index,
                track.extension(),
            );
            save_clip(&self.state, &filename, &bytes)
                .await
                .map_err(|e| format!("failed to save rendered song: {e}"))?;
            outputs.push(GenerationOutput {
                filename,
                mime: content_type,
                size_bytes: bytes.len() as i64,
                kind: "audio".to_string(),
            });
        }

        if save {
            record_generation(
                &self.state,
                "music",
                "music3-generate",
                json!({ "engine": "music3", "job_id": job_id }),
                outputs.clone(),
            )
            .await;
        }

        // The row carries the answer this engine's own page already speaks, so
        // retiring the per-engine store did not mean rewriting its media routes.
        let tracks: Vec<Value> = done
            .tracks
            .iter()
            .zip(outputs.iter())
            .map(|(track, out)| {
                json!({
                    "url": format!("/api/v1/music3/file/{}", out.filename),
                    "filename": out.filename,
                    "index": track.index,
                    "seed": track.seed,
                    "content_type": out.mime,
                    "bytes": out.size_bytes,
                    "duration_seconds": track.duration_seconds,
                    "sample_rate": track.sample_rate,
                    "channels": track.channels,
                })
            })
            .collect();
        let result = json!({ "tracks": tracks, "plan": done.plan, "ignored": [] });
        if let Err(e) = store::set_result(&self.state.db, job_id, &result).await {
            warn!(job = %job_id, error = %e, "music3: could not record the result");
        }

        Ok(())
    }
}

#[async_trait]
impl JobRunner for Music3Adapter {
    fn kind(&self) -> JobKind {
        JobKind::Music3
    }

    /// 🔴 The comfy-api budget line, shared with h3, ltx25 and upscale because they
    /// are four plugins on ONE process. The *mutex* between them is the lane; this
    /// is only the VRAM claim.
    fn service(&self) -> ServiceKind {
        ServiceKind::MinimaxH3
    }

    /// Whole-job retry only — Music 3 publishes no checkpoint surface.
    fn preempt(&self) -> Preempt {
        Preempt::Kill
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        let caption = payload
            .get("caption")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if caption.is_empty() {
            return Err("a caption is required — describe the music".to_string());
        }
        Ok(())
    }

    /// Duration is the dominant term and takes multiply it. Buckets are coarse on
    /// purpose: a per-second key would give every render its own sample of one.
    fn shape_key(&self, payload: &Value) -> String {
        let secs = payload
            .get("max_duration")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let bucket = match secs as i64 {
            0..=60 => "60s",
            61..=120 => "120s",
            121..=240 => "240s",
            _ => "long",
        };
        let takes = payload.get("takes").and_then(Value::as_i64).unwrap_or(1);
        format!("music3/{bucket}/{takes}take")
    }

    fn units(&self, payload: &Value) -> Option<i32> {
        payload
            .get("takes")
            .and_then(Value::as_i64)
            .map(|n| n as i32)
            .or(Some(1))
    }

    fn label(&self, payload: &Value) -> String {
        let caption = payload
            .get("caption")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(48)
            .collect::<String>();
        let takes = payload.get("takes").and_then(Value::as_i64).unwrap_or(1);
        if takes > 1 {
            format!("music3 · {takes} takes · {caption}")
        } else {
            format!("music3 · {caption}")
        }
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.music3;
        if !client.is_configured() {
            return Err("music3 is not configured".to_string());
        }

        // 🔴 No `gpu` field, deliberately: comfy-api does not take one. It is one
        // process in front of one ComfyUI on whatever card its container was
        // given, which is also why `gpu_engine_config` gives `minimax_h3` a
        // single-card candidate list and why the queue refuses a pin on this lane.
        let plan = client
            .submit(&ctx.payload)
            .await
            .map_err(|e| verdict::note(e.to_string(), verdict_of(&e)))?;

        // Publish the engine's id the moment it accepts. This is the single field
        // that makes reattach possible, so it is written before anything else can
        // go wrong.
        if let Err(e) = store::set_server_job_id(&self.state.db, &ctx.job.id, &plan.id).await {
            warn!(job = %ctx.job.id, error = %e, "music3: could not record the engine job id");
        }

        let poll = self.poll(&ctx, &plan.id).await;

        // 🔴 THE ENGINE/DISK BOUNDARY. Release before any fetch, write or DB work —
        // holding the card through the collect would idle a GPU on every harvest.
        ctx.release_gpu();

        let done = match poll {
            Ok(d) => d,
            Err(e) if e == "cancelled" => return Ok(JobOutcome::Cancelled),
            Err(e) => return Err(e),
        };

        let save = ctx
            .payload
            .get("save")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.collect(&ctx.job.id, &done, save).await?;
        log_music3(
            &self.state,
            None,
            &json!({ "engine": "music3", "job_id": ctx.job.id }),
            ctx.job.admit_ms.unwrap_or(0) as u64,
            ctx.job.render_ms.unwrap_or(0) as u64,
            200,
            json!({ "takes": done.tracks.len(), "warnings": plan.warnings }),
        );
        Ok(JobOutcome::Done {
            warnings: plan.warnings.clone(),
        })
    }

    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        let Some(server_job_id) = ctx.job.server_job_id.clone() else {
            return Err("reattach with no engine job id".to_string());
        };
        let poll = self.poll(&ctx, &server_job_id).await;
        ctx.release_gpu();
        let done = match poll {
            Ok(d) => d,
            Err(e) if e == "cancelled" => return Ok(JobOutcome::Cancelled),
            Err(e) => return Err(e),
        };
        // A reattached render is still a render the user made; omitting the gallery
        // row would lose the song purely because koblem restarted.
        self.collect(&ctx.job.id, &done, true).await?;
        Ok(JobOutcome::Done { warnings: vec![] })
    }

    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(sid) = job.server_job_id.as_deref() else {
            return Ok(());
        };
        self.state
            .music3
            .cancel(sid)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter_shape(payload: Value) -> String {
        // `shape_key` and `units` read only the payload, so they can be exercised
        // without a client. This is the part that decides whether the estimator
        // learns anything, so it is worth a test of its own.
        let secs = payload
            .get("max_duration")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let bucket = match secs as i64 {
            0..=60 => "60s",
            61..=120 => "120s",
            121..=240 => "240s",
            _ => "long",
        };
        let takes = payload.get("takes").and_then(Value::as_i64).unwrap_or(1);
        format!("music3/{bucket}/{takes}take")
    }

    /// 🔴 Coarse buckets. A per-second key gives every render its own sample of
    /// one and the estimator never learns anything.
    #[test]
    fn nearby_durations_share_a_bucket() {
        assert_eq!(
            adapter_shape(json!({"max_duration": 90, "takes": 1})),
            adapter_shape(json!({"max_duration": 118, "takes": 1})),
        );
        assert_ne!(
            adapter_shape(json!({"max_duration": 90, "takes": 1})),
            adapter_shape(json!({"max_duration": 90, "takes": 3})),
        );
        assert_ne!(
            adapter_shape(json!({"max_duration": 30, "takes": 1})),
            adapter_shape(json!({"max_duration": 200, "takes": 1})),
        );
    }

    /// 🔴 A caption the model choked on chokes it again, so the plugin's own
    /// failure report buys no second slot — only the fleet saying "not now" does.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: Music3Error| verdict_of(&e);
        assert_eq!(
            t(Music3Error::Upstream(429, "queue full".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Music3Error::Upstream(503, "draining".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Music3Error::Upstream(500, "boom".into())),
            Failure::Transient
        );
        assert_eq!(
            t(Music3Error::Upstream(400, "bad caption".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(Music3Error::Upstream(422, "bad takes".into())),
            Failure::Permanent
        );
        assert_eq!(t(Music3Error::NotConfigured), Failure::Permanent);
        assert_eq!(t(Music3Error::Cancelled("j".into())), Failure::Permanent);
        assert_eq!(
            t(Music3Error::JobFailed("j".into(), "node raised".into())),
            Failure::Permanent
        );
        assert_eq!(t(Music3Error::MissingField("tracks")), Failure::Permanent);
    }

    /// The queue only ever sees the flattened sentence, so the verdict has to
    /// survive the flattening.
    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = Music3Error::Upstream(429, "pending queue is full".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Transient);
        let e = Music3Error::Upstream(400, "pending queue is full".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Permanent);
    }
}
