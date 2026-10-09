//! LTX-2.5 — the ComfyUI chain render, and one half of the overnight Director queue.
//!
//! # The payload
//!
//! An envelope, not a bare engine request. Three things ride beside the chain and none of them can
//! be derived from it:
//!
//! * `job_id` — the [`crate::ltx_jobs::LtxJobStore`] record. The Director polls
//!   `/api/v1/ltx-video/jobs/{id}` and previews `…/segments/{n}` while the render runs, and both
//!   read that store, so the partial FILES have to keep landing under the id the browser already
//!   holds rather than under the queue row's.
//! * `assets` — `part name -> asset:<hash>` for every binary the chain names. The chain carries
//!   `part:<name>` markers exactly as the route built them (marker names are derived from the
//!   document and are what makes a retake present byte-identical conditioning to the latent bank),
//!   and a queue row is JSON, so the bytes are fetched back out of the content-addressed store at
//!   dispatch instead of being carried through Postgres.
//! * `director_project_id` / `director_tier` — the harvest is ENGINE-SPECIFIC and stays here.
//!
//! # What is NOT re-derived
//!
//! The chain is submitted as the route compiled it. Rebuilding it here would be a second
//! implementation of `run_ltx_job`'s 1000-line compile, and the two would disagree about a seam
//! trim or a pin the day one of them changed.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::Bytes;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{Job, JobKind, JobProgress, Preempt, VideoJobState};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::verdict;
use crate::AppState;
use crate::director_assets;
use crate::ltx_video::{ChainAudio, Error as LtxError};
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as queue_rows;
use crate::routes::ltx_director::{harvest_director_renders, maybe_harvest_director};
use crate::routes::ltx_video::{
    replace_audio_in_place, round_down_8k1, round32, shot_window_error,
};
use crate::routes::tier2_helpers::{OutputFile, persist_generation_files};

pub struct Ltx25Adapter {
    state: AppState,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Ltx25Payload {
    chain: Value,
    job_id: String,
    #[serde(default)]
    assets: BTreeMap<String, String>,
    #[serde(default)]
    audio: AudioAssets,
    #[serde(default = "enabled")]
    save_clip: bool,
    #[serde(default)]
    director_project_id: Option<String>,
    #[serde(default)]
    director_tier: Option<String>,
    /// A full run REPLACES its tier's takes; a resume or a retake must not, or the shots it
    /// restored from the bank are deleted by the render that reused them.
    #[serde(default)]
    director_wipe: bool,
}

/// Engine-ready WAVs, as asset refs. Transcoding happens at enqueue: it is CPU work at the edge and
/// re-doing it under the GPU guard would hold a card through an ffmpeg run.
#[derive(Debug, Default, Deserialize)]
struct AudioAssets {
    #[serde(default)]
    drive: Option<String>,
    #[serde(default)]
    track: Option<String>,
    #[serde(default)]
    shot_drive: Vec<Option<String>>,
    #[serde(default)]
    shot_track: Vec<Option<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct Ltx25Resume {
    #[serde(default)]
    resume_job_id: Option<String>,
    #[serde(default)]
    retake_from: Option<u32>,
    #[serde(default)]
    segments_done: i32,
    #[serde(default)]
    bank_keys: Vec<(u32, String)>,
}

/// The engine request inside the envelope. Tolerant of a bare chain so the enqueue-time hooks
/// (`validate`, `shape_key`, `units`, `label`) can still read a payload that is only the request.
fn chain_of(payload: &Value) -> &Value {
    payload.get("chain").unwrap_or(payload)
}

fn segments_of(payload: &Value) -> &[Value] {
    const NONE: &[Value] = &[];
    chain_of(payload)
        .get("segments")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(NONE)
}

fn dims_of(payload: &Value) -> (u32, u32) {
    let chain = chain_of(payload);
    let get = |k: &str| chain.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    (get("width"), get("height"))
}

/// Frames the DiT samples for a shot: its own `frames` when the chain states one (Director always
/// does), else the chain-wide default.
fn shot_frames(chain: &Value, seg: &Value) -> u32 {
    seg.get("frames")
        .and_then(Value::as_u64)
        .or_else(|| chain.get("segment_frames").and_then(Value::as_u64))
        .unwrap_or(0) as u32
}

fn payload_of(value: &Value) -> Result<Ltx25Payload, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("unreadable ltx25 payload: {e}"))
}

/// 🔴 The SEGMENT COUNT is deliberately absent: it is `units_total`, and folding it in here would
/// give a twelve-shot render its own sample of one instead of twelve samples of a shot.
fn shape_key_of(payload: &Value) -> String {
    let (w, h) = dims_of(payload);
    let chain = chain_of(payload);
    let frames = segments_of(payload)
        .first()
        .map(|s| shot_frames(chain, s))
        .unwrap_or(0);
    format!("ltx25/{w}x{h}/{frames}f")
}

fn units_of(payload: &Value) -> Option<i32> {
    let n = segments_of(payload).len();
    (n > 0).then_some(n as i32)
}

fn validate_payload(payload: &Value) -> Result<(), String> {
    let chain = chain_of(payload);
    let segments = segments_of(payload);
    if segments.is_empty() {
        return Err("an ltx25 render needs at least one segment".to_string());
    }
    let (w, h) = dims_of(payload);
    // 🔴 Refused, never snapped. A canvas nobody asked for is a render nobody can use.
    if w == 0 || h == 0 || round32(w) != w || round32(h) != h {
        return Err(format!(
            "{w}x{h} is not a legal LTX canvas — both sides must be a multiple of 32 (the spatial \
             patch grid). Nearest legal: {}x{}",
            round32(w.max(1)),
            round32(h.max(1))
        ));
    }
    let fps = chain.get("fps").and_then(Value::as_u64).unwrap_or(24) as u32;
    for (i, seg) in segments.iter().enumerate() {
        let frames = shot_frames(chain, seg);
        // 🔴 REFUSED, never snapped. The delivered length of a shot is what the caller's audio
        // timeline is sliced against, so a snapped shot desynchronises every shot after it and
        // presents as bad lip-sync rather than as a bad number.
        if frames < 9 || round_down_8k1(frames) != frames {
            return Err(format!(
                "shot {} is {frames} frames, which is not a legal LTX length — the temporal VAE \
                 takes 8k+1 only. Nearest legal at or below: {}",
                i + 1,
                round_down_8k1(frames)
            ));
        }
        if let Some(msg) = shot_window_error(i, frames, frames, fps) {
            return Err(msg);
        }
    }
    Ok(())
}

async fn resolve_one(
    dir: &std::path::Path,
    reference: &Option<String>,
) -> Result<Option<Vec<u8>>, String> {
    let Some(reference) = reference.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    director_assets::resolve_media(dir, reference)
        .await
        .map_err(|e| format!("audio: {e}"))
        .map(|b| b.map(|b| b.to_vec()))
}

/// 🔴 A 503 on a resume is the plugin saying the bank entry it was pointed at is gone (it names the
/// node). Anything else is a real failure.
fn bank_is_gone(e: &LtxError) -> bool {
    matches!(e, LtxError::Upstream(503, _))
}

fn verdict_of(e: &LtxError) -> Failure {
    match e {
        LtxError::Request(e) => verdict::transport(e),
        // 🔴 NOT the fleet-wide drain: on this plugin 503 is "the bank entry you pointed at is
        // gone", and `submit` above already spends its one recovery re-rendering from the first
        // shot. A 503 that survived that will survive the next attempt too.
        LtxError::Upstream(503, _) => Failure::Permanent,
        LtxError::Upstream(status, _) => verdict::upstream(*status),
        // A chain the DiT choked on chokes it again, a body koblem cannot read is a schema
        // disagreement, and a stream that died is a twelve-shot re-render for a file that may
        // already be on the engine — none of that is worth another card.
        LtxError::NotConfigured
        | LtxError::JobFailed(..)
        | LtxError::Cancelled(_)
        | LtxError::MissingField(_)
        | LtxError::BadMedia(..)
        | LtxError::Stream(_) => Failure::Permanent,
    }
}

fn drop_resume(chain: &mut Value) {
    let Some(obj) = chain.as_object_mut() else {
        return;
    };
    obj.remove("resume_job_id");
    obj.remove("retake_from");
    if let Some(segs) = obj.get_mut("segments").and_then(Value::as_array_mut) {
        for s in segs {
            if let Some(o) = s.as_object_mut() {
                o.remove("bank_job_id");
            }
        }
    }
}

impl Ltx25Adapter {
    pub fn new(state: AppState) -> Self {
        Ltx25Adapter { state }
    }

    /// Point the chain at what a previous run banked. `resume_job_id` covers the whole prefix; a
    /// per-segment `bank_job_id` only goes on a shot that does not already name its own take,
    /// because the caller's choice of take is a selection and this is a recovery.
    fn apply_resume(chain: &mut Value, resume: &Ltx25Resume) {
        let Some(rid) = resume.resume_job_id.as_deref().filter(|s| !s.is_empty()) else {
            return;
        };
        let Some(obj) = chain.as_object_mut() else {
            return;
        };
        obj.insert("resume_job_id".into(), json!(rid));
        if let Some(k) = resume.retake_from {
            obj.insert("retake_from".into(), json!(k));
        }
        let Some(segs) = obj.get_mut("segments").and_then(Value::as_array_mut) else {
            return;
        };
        for (index, key) in &resume.bank_keys {
            let Some(seg) = segs.get_mut(*index as usize).and_then(Value::as_object_mut) else {
                continue;
            };
            seg.entry("bank_job_id").or_insert_with(|| json!(key));
        }
    }

    async fn resolve_parts(
        &self,
        assets: &BTreeMap<String, String>,
    ) -> Result<Vec<(String, Bytes)>, String> {
        let dir = self.state.director_assets_dir.as_ref();
        let mut out = Vec::with_capacity(assets.len());
        for (name, reference) in assets {
            let bytes = director_assets::resolve_media(dir, reference)
                .await
                .map_err(|e| format!("part `{name}`: {e}"))?
                .ok_or_else(|| format!("part `{name}` resolved to nothing"))?;
            out.push((name.clone(), bytes));
        }
        Ok(out)
    }

    async fn resolve_audio(&self, audio: &AudioAssets) -> Result<ChainAudio, String> {
        let dir = self.state.director_assets_dir.as_ref();
        let mut out = ChainAudio {
            drive: resolve_one(dir, &audio.drive).await?,
            track: resolve_one(dir, &audio.track).await?,
            ..ChainAudio::default()
        };
        for r in &audio.shot_drive {
            out.shot_drive.push(resolve_one(dir, r).await?);
        }
        for r in &audio.shot_track {
            out.shot_track.push(resolve_one(dir, r).await?);
        }
        Ok(out)
    }

    /// Everything after the engine is done with the card: the container's own account of itself,
    /// the soundtrack re-lay, the gallery row and the Director harvest.
    async fn collect(&self, p: &Ltx25Payload, audio: &ChainAudio, warnings: &[String]) {
        let jobs = self.state.ltx_jobs.clone();
        let clip_ext = jobs
            .get(&p.job_id)
            .map(|j| j.clip_ext())
            .unwrap_or("mp4")
            .to_string();
        let clip_mime = format!("video/{clip_ext}");
        let clip_path = jobs.clip_path(&p.job_id);

        // 🔴 The stitched cut takes ONE continuous soundtrack, not N spliced ones: the engine joins
        // per-shot mp4s with `-c copy`, which cannot trim AAC framing, so every seam drifts ~14 ms.
        // Per-shot overlays are the exception — that composite exists only inside the engine, and
        // laying the bed over it would drop every shot's own audio.
        let overlays = audio.shot_track.iter().any(Option::is_some);
        if !overlays
            && let Some(track) = audio.track.as_ref()
            && let Err(e) =
                replace_audio_in_place(&clip_path, &Bytes::from(track.clone()), &clip_ext).await
        {
            warn!(job = %p.job_id, error = %e, "ltx25: audio replace failed; keeping the engine's mux");
        }

        if p.save_clip {
            let chain = &p.chain;
            let (w, h) = dims_of(chain);
            let prompts: Vec<&str> = segments_of(chain)
                .iter()
                .filter_map(|s| s.get("prompt").and_then(Value::as_str))
                .collect();
            persist_generation_files(
                &self.state,
                self.state.ltx25.service_tag(),
                "generate",
                json!({
                    "engine": "ltx25",
                    "segments": segments_of(chain).len(),
                    "scripts": prompts,
                    "width": w,
                    "height": h,
                    "fps": chain.get("fps"),
                    "queued": true,
                }),
                &[OutputFile {
                    ext: clip_ext.clone(),
                    mime: clip_mime.clone(),
                    kind: "video".to_string(),
                    path: clip_path.clone(),
                }],
            )
            .await;
        }

        // Written unconditionally: an EMPTY list is what clears a previous run's banner, and the
        // Director harvest reads this off the job to stamp the project.
        jobs.update(&p.job_id, |j| {
            j.state = VideoJobState::Completed;
            j.content_type = Some(clip_mime.clone());
            j.delivery_warnings = warnings.to_vec();
        });

        match (p.director_project_id.as_deref(), p.director_tier.as_deref()) {
            (Some(project), Some(tier)) => {
                harvest_director_renders(
                    self.state.clone(),
                    project.to_string(),
                    p.job_id.clone(),
                    tier.to_string(),
                    p.director_wipe,
                )
                .await
            }
            // A job tagged in the store but not in the payload (an editor render adopted by the
            // queue) still has to be harvested.
            _ => {
                maybe_harvest_director(self.state.clone(), p.job_id.clone(), p.director_wipe).await
            }
        }
    }
}

#[async_trait]
impl JobRunner for Ltx25Adapter {
    fn kind(&self) -> JobKind {
        JobKind::Ltx25
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::Ltx
    }

    /// 🔴 The bank is what makes this cheap: the plugin writes one entry per segment as it decodes,
    /// so stopping costs the shot in flight and nothing else.
    fn preempt(&self) -> Preempt {
        Preempt::AtCheckpoint
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    fn validate(&self, payload: &Value) -> Result<(), String> {
        validate_payload(payload)
    }

    fn shape_key(&self, payload: &Value) -> String {
        shape_key_of(payload)
    }

    fn units(&self, payload: &Value) -> Option<i32> {
        units_of(payload)
    }

    fn label(&self, payload: &Value) -> String {
        let (w, h) = dims_of(payload);
        let segments = segments_of(payload);
        let first = segments
            .first()
            .and_then(|s| s.get("prompt"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(40)
            .collect::<String>();
        format!("ltx25 · {} shots · {w}x{h} · {first}", segments.len())
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.ltx25;
        if !client.is_configured() {
            return Err("ltx25 is not configured".to_string());
        }
        let p = payload_of(&ctx.payload)?;
        let jobs = self.state.ltx_jobs.clone();
        if jobs.get(&p.job_id).is_none() {
            // Without the record there is nowhere to file a partial and nothing for the Director's
            // poll to read, so the render would succeed invisibly.
            return Err(format!(
                "ltx job record `{}` is gone — the render was not started because its shots would \
                 have nowhere to land",
                p.job_id
            ));
        }

        let resume: Ltx25Resume = match ctx.resume.clone() {
            Some(v) => serde_json::from_value(v)
                .map_err(|e| format!("unreadable ltx25 resume state: {e}"))?,
            None => Ltx25Resume::default(),
        };
        let mut warnings: Vec<String> = Vec::new();
        let mut chain = p.chain.clone();
        Self::apply_resume(&mut chain, &resume);
        let resuming = chain.get("resume_job_id").is_some();

        let parts = self.resolve_parts(&p.assets).await?;
        let audio = self.resolve_audio(&p.audio).await?;

        let server_job_id = match client.submit_full(&chain, &[], &audio, &parts).await {
            Ok(id) => id,
            // 🔴 Re-rendered rather than failed: the bank is a cache, and a night that ends with a
            // finished render and a warning beats one that ends with a 503.
            Err(e) if resuming && bank_is_gone(&e) => {
                warnings.push(format!(
                    "the banked shots this render was continuing from are gone, so it was rendered \
                     from the first shot instead: {e}"
                ));
                drop_resume(&mut chain);
                client
                    .submit_full(&chain, &[], &audio, &parts)
                    .await
                    .map_err(|e| verdict::note(format!("submit: {e}"), verdict_of(&e)))?
            }
            Err(e) => return Err(verdict::note(format!("submit: {e}"), verdict_of(&e))),
        };

        if let Err(e) =
            queue_rows::set_server_job_id(&self.state.db, &ctx.job.id, &server_job_id).await
        {
            warn!(job = %ctx.job.id, error = %e, "ltx25: could not record the engine job id");
        }
        jobs.update(&p.job_id, |j| {
            j.state = VideoJobState::Rendering;
            j.server_job_id = Some(server_job_id.clone());
        });

        // Progressive per-shot delivery, on the paths the Director already previews from.
        let (partial_tx, mut partial_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::ltx_video::LtxPartialChunk>();
        let drain = {
            let jobs = jobs.clone();
            let job_id = p.job_id.clone();
            tokio::spawn(async move {
                while let Some(chunk) = partial_rx.recv().await {
                    let path = jobs.partial_path(&job_id, chunk.segment_index);
                    if let Err(e) = tokio::fs::write(&path, &chunk.bytes).await {
                        warn!(job = %job_id, segment = chunk.segment_index, error = %e, "write ltx partial failed");
                        continue;
                    }
                    let url = format!(
                        "/api/v1/ltx-video/jobs/{job_id}/segments/{}",
                        chunk.segment_index
                    );
                    jobs.push_partial(&job_id, chunk.segment_index, url);
                }
            })
        };

        // The bank keys are the checkpoint a preempt hands back, so they are persisted on the job
        // as they land rather than read once at the end.
        let (bank_tx, mut bank_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::ltx_video::LtxBankReport>();
        let bank_drain = {
            let jobs = jobs.clone();
            let job_id = p.job_id.clone();
            tokio::spawn(async move {
                while let Some(report) = bank_rx.recv().await {
                    jobs.update(&job_id, |j| {
                        if !report.bank_keys.is_empty() {
                            j.bank_keys.clone_from(&report.bank_keys);
                        }
                        j.job_is_partial |= report.partial;
                        j.renders_window |= report.window;
                    });
                }
            })
        };

        let waiter = client.wait_with_partials(&server_job_id, Some(partial_tx), Some(bank_tx));
        tokio::pin!(waiter);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut interrupt = Interrupt::None;
        let outcome = loop {
            tokio::select! {
                done = &mut waiter => break done,
                _ = ticker.tick() => {
                    // The store's partial list SURVIVES a preempt (same record, idempotent by
                    // index), so it already counts what an earlier run banked; the resume's own
                    // figure is the floor for shots the engine restored without re-emitting.
                    //
                    // 🔴 A resumed run emits `partials` ONLY for the shots it actually renders —
                    // a restored shot never becomes a unit at all, and the record starts empty
                    // because a resume is a brand-new job. So the delivered count alone would
                    // report a 42-shot cut resuming at 40 as "2 of 42" and the bar would jump
                    // backwards. The `max` against the resume figure is what makes it monotonic.
                    //
                    // (The engine also publishes `plan.render_window.restored_from_bank`, which
                    // would make this exact rather than a floor. Not read: a floor that cannot
                    // regress is the property this needs, and one number is easier to trust than
                    // two that must agree.)
                    let done_units = jobs
                        .get(&p.job_id)
                        .map(|j| j.partials.len() as i32)
                        .unwrap_or(0)
                        .max(resume.segments_done);
                    ctx.progress(
                        JobProgress {
                            phase: Some("rendering".to_string()),
                            ..JobProgress::default()
                        },
                        done_units,
                    )
                    .await;
                    if interrupt == Interrupt::None && ctx.cancel.is_stopping() {
                        interrupt = ctx.cancel.get();
                        // Both stops go through the engine's own cancel; the difference is only
                        // what koblem does with what was banked.
                        if let Err(e) = client.cancel(&server_job_id).await {
                            warn!(job = %p.job_id, error = %e, "ltx25: engine cancel failed");
                        }
                    }
                }
            }
        };
        let _ = drain.await;
        let _ = bank_drain.await;

        if interrupt == Interrupt::Preempt {
            ctx.release_gpu();
            let banked = jobs.get(&p.job_id);
            let bank_keys = banked
                .as_ref()
                .map(|j| j.bank_keys.clone())
                .unwrap_or_default();
            let segments_done = banked
                .map(|j| j.partials.len() as i32)
                .unwrap_or(0)
                .max(resume.segments_done);
            info!(job = %p.job_id, segments_done, "ltx25: preempted at a checkpoint");
            return Ok(JobOutcome::Preempted {
                resume: json!({
                    "resume_job_id": server_job_id,
                    "retake_from": resume.retake_from,
                    "segments_done": segments_done,
                    "bank_keys": bank_keys,
                }),
            });
        }

        let clip_path = jobs.clip_path(&p.job_id);
        let result = match outcome {
            // 🔴 Fetched UNDER the guard, and this IS the engine/disk boundary for ltx25: nothing
            // stitched ever reaches the engine's volume — the clip lives only in the CUDA child's
            // RAM — so whatever takes the card next can `unload()` the only copy. The seconds this
            // costs buy the render; the minutes of mux/persist/harvest below run released.
            Ok(result) => {
                let media = client
                    .media_to_file(&server_job_id, &result, &clip_path)
                    .await;
                ctx.release_gpu();
                match media {
                    Ok(_) => result,
                    Err(e) => {
                        jobs.update(&p.job_id, |j| {
                            j.state = VideoJobState::Failed;
                            j.error = Some(format!("fetch render media: {e}"));
                        });
                        return Err(verdict::note(
                            format!("fetch render media: {e}"),
                            verdict_of(&e),
                        ));
                    }
                }
            }
            Err(e) => {
                ctx.release_gpu();
                let cancelled = matches!(e, LtxError::Cancelled(_));
                jobs.update(&p.job_id, |j| {
                    // The engine job id stays put: the UI offers a resume from the banked shots.
                    j.state = if cancelled {
                        VideoJobState::Cancelled
                    } else {
                        VideoJobState::Failed
                    };
                    j.error = Some(e.to_string());
                });
                return if cancelled {
                    Ok(JobOutcome::Cancelled)
                } else {
                    Err(verdict::note(e.to_string(), verdict_of(&e)))
                };
            }
        };

        if result.job_is_partial {
            let window = !result.rendered_segments.is_empty();
            let note = crate::ltx_video::concat_failure_note(&result);
            jobs.update(&p.job_id, |j| {
                j.job_is_partial = true;
                j.renders_window |= window;
            });
            if let Some(note) = note {
                warnings.push(note);
            }
        }

        self.collect(&p, &audio, &warnings).await;
        Ok(JobOutcome::Done { warnings })
    }

    /// 🔴 comfy-api forgets a job after ten minutes, so this cannot be deferred behind other work —
    /// and the fast path exists because a render that finished while koblem was down is still in
    /// the engine's hand for the rest of that window.
    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.ltx25;
        let p = payload_of(&ctx.payload)?;
        let Some(server_job_id) = ctx.job.server_job_id.clone() else {
            return Err("reattach with no engine job id".to_string());
        };
        let jobs = self.state.ltx_jobs.clone();
        let audio = self.resolve_audio(&p.audio).await.unwrap_or_default();
        let mut warnings: Vec<String> = Vec::new();

        // Before the media is touched: this is the one path that can complete a resume or a retake
        // without ever reading a `result`, and filing its `/media` as the project's cut would
        // replace the timeline with the shots the resume happened to render.
        if let Ok(body) = client.status(&server_job_id).await {
            let report = crate::ltx_video::bank_report(&body);
            jobs.update(&p.job_id, |j| {
                if !report.bank_keys.is_empty() {
                    j.bank_keys.clone_from(&report.bank_keys);
                }
                j.job_is_partial |= report.partial;
                j.renders_window |= report.window;
            });
        }

        let clip_path = jobs.clip_path(&p.job_id);
        if client
            .fetch_media_to_file(&server_job_id, &clip_path)
            .await
            .is_ok()
        {
            ctx.release_gpu();
            self.collect(&p, &audio, &warnings).await;
            return Ok(JobOutcome::Done { warnings });
        }

        let outcome = client.wait(&server_job_id).await;
        let result = match outcome {
            Ok(result) => {
                let media = client
                    .media_to_file(&server_job_id, &result, &clip_path)
                    .await;
                ctx.release_gpu();
                if let Err(e) = media {
                    jobs.update(&p.job_id, |j| {
                        j.state = VideoJobState::Failed;
                        j.error = Some(format!("re-attach fetch media: {e}"));
                    });
                    return Err(verdict::note(
                        format!("re-attach fetch media: {e}"),
                        verdict_of(&e),
                    ));
                }
                result
            }
            Err(e) => {
                ctx.release_gpu();
                let cancelled = matches!(e, LtxError::Cancelled(_));
                jobs.update(&p.job_id, |j| {
                    j.state = if cancelled {
                        VideoJobState::Cancelled
                    } else {
                        VideoJobState::Failed
                    };
                    j.error = Some(format!("re-attach: {e}"));
                });
                return if cancelled {
                    Ok(JobOutcome::Cancelled)
                } else {
                    Err(verdict::note(format!("re-attach: {e}"), verdict_of(&e)))
                };
            }
        };
        if result.job_is_partial {
            let window = !result.rendered_segments.is_empty();
            let note = crate::ltx_video::concat_failure_note(&result);
            jobs.update(&p.job_id, |j| {
                j.job_is_partial = true;
                j.renders_window |= window;
            });
            if let Some(note) = note {
                warnings.push(note);
            }
        }
        self.collect(&p, &audio, &warnings).await;
        Ok(JobOutcome::Done { warnings })
    }

    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(sid) = job.server_job_id.as_deref() else {
            return Ok(());
        };
        match self.state.ltx25.cancel(sid).await {
            Ok(_) => Ok(()),
            // 410 = the job record already expired, so there is nothing holding the worker.
            Err(LtxError::Upstream(410, _)) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn abandon(&self, job_id: &str, _payload: &Value) {
        self.state
            .ltx_jobs
            .cancel_by_queue_id(job_id, "cancelled before the render started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(w: u32, h: u32, frames: &[u32]) -> Value {
        json!({
            "chain": {
                "width": w,
                "height": h,
                "fps": 24,
                "segment_frames": 169,
                "segments": frames
                    .iter()
                    .map(|f| json!({ "prompt": "a shot", "frames": f }))
                    .collect::<Vec<Value>>(),
            },
            "job_id": "koblem-1",
        })
    }

    /// 🔴 The whole point of the key: a 12-shot render and a 4-shot render of the same shape share
    /// a bucket, so the long one is estimated from the short one's history.
    #[test]
    fn the_segment_count_is_not_in_the_key() {
        assert_eq!(
            shape_key_of(&chain(1280, 704, &[97, 97, 97, 97])),
            shape_key_of(&chain(1280, 704, &[97])),
        );
        assert_ne!(
            shape_key_of(&chain(1280, 704, &[97])),
            shape_key_of(&chain(1280, 704, &[169])),
        );
        assert_eq!(units_of(&chain(1280, 704, &[97, 97, 97])), Some(3));
    }

    #[test]
    fn an_off_grid_length_is_refused_not_snapped() {
        assert!(validate_payload(&chain(1280, 704, &[97])).is_ok());
        let e = validate_payload(&chain(1280, 704, &[100])).expect_err("100 is not 8k+1");
        assert!(e.contains("8k+1"), "{e}");
        let e = validate_payload(&chain(1290, 704, &[97])).expect_err("1290 is off the 32 grid");
        assert!(e.contains("multiple of 32"), "{e}");
        // Over the measured VRAM window: refused at enqueue rather than hours into the night.
        assert!(validate_payload(&chain(1280, 704, &[249])).is_err());
    }

    /// The resume names the bank, and never overwrites a take the caller selected for a shot.
    #[test]
    fn a_resume_points_the_chain_at_its_bank() {
        let mut c = chain(1280, 704, &[97, 97])["chain"].clone();
        c["segments"][1]["bank_job_id"] = json!("chosen");
        Ltx25Adapter::apply_resume(
            &mut c,
            &Ltx25Resume {
                resume_job_id: Some("srv-1".into()),
                retake_from: None,
                segments_done: 1,
                bank_keys: vec![(0, "k0".into()), (1, "k1".into())],
            },
        );
        assert_eq!(c["resume_job_id"], json!("srv-1"));
        assert_eq!(c["segments"][0]["bank_job_id"], json!("k0"));
        assert_eq!(c["segments"][1]["bank_job_id"], json!("chosen"));
    }

    /// 🔴 503 is this plugin's "the bank is gone", not the fleet's drain, and `run` already spends
    /// its one recovery on it — a second level of retry here would be six renders, not three.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: LtxError| verdict_of(&e);
        assert_eq!(
            t(LtxError::Upstream(503, "bank gone".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(LtxError::Upstream(429, "queue full".into())),
            Failure::Transient
        );
        assert_eq!(
            t(LtxError::Upstream(502, "gateway".into())),
            Failure::Transient
        );
        assert_eq!(
            t(LtxError::Upstream(400, "bad chain".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(LtxError::Upstream(410, "expired".into())),
            Failure::Permanent
        );
        assert_eq!(t(LtxError::NotConfigured), Failure::Permanent);
        assert_eq!(t(LtxError::Cancelled("j".into())), Failure::Permanent);
        assert_eq!(
            t(LtxError::JobFailed("j".into(), "OOM".into())),
            Failure::Permanent
        );
        assert_eq!(t(LtxError::MissingField("media")), Failure::Permanent);
        assert_eq!(
            t(LtxError::BadMedia("j".into(), "not webm".into())),
            Failure::Permanent
        );
    }

    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = LtxError::Upstream(429, "ltx25 pending queue is full".into());
        assert_eq!(
            verdict::recall(&verdict::note(format!("submit: {e}"), verdict_of(&e))),
            Failure::Transient
        );
        let e = LtxError::Upstream(503, "ltx25 pending queue is full".into());
        assert_eq!(
            verdict::recall(&verdict::note(format!("submit: {e}"), verdict_of(&e))),
            Failure::Permanent
        );
    }
}
