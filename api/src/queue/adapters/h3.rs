//! MiniMax-H3 — the joint audio+video chain, and the other half of the overnight Director queue.
//!
//! # The payload
//!
//! The same envelope shape as [`super::ltx25`], for the same reasons: the engine request exactly as
//! `routes::minimax_h3::build_chain` compiled it, plus the [`crate::ltx_jobs::LtxJobStore`] id the
//! browser is already polling, the `part name -> asset:<hash>` map that lets the reference bytes be
//! read back out of the content-addressed store at dispatch, and the Director tag.
//!
//! # The unit is a GENERATION
//!
//! 🔴 Not a shot. The cuts inside a generation are prompt text and `<scenetrans>` audio continuity
//! exists only within one, so a per-shot H3 retake is not expressible — which is why a preempt here
//! costs up to one whole generation (~14 min at prod shape) and why the resume carries per-
//! generation bank keys rather than a segment index.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{
    H3_FRAME_BASE, H3_FRAME_PERIOD, H3_LATENT_CYCLE, H3_MAX_FRAMES, H3_MIN_FRAMES, H3ReferenceKind,
    H3Soundtrack, H3SoundtrackMode, Job, JobKind, JobProgress, Preempt, VideoJobState,
    h3_carry_is_on_phase, h3_latents_for_frames, h3_legal_carry_depths,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::verdict;
use crate::AppState;
use crate::director_assets;
use crate::minimax_h3::{Error as H3Error, H3_MIME_TYPE, H3_OUTPUT_FORMAT, H3RefPart};
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as queue_rows;
use crate::routes::h3_soundtrack;
use crate::routes::ltx_director::{harvest_director_renders, maybe_harvest_director};
use crate::routes::minimax_h3::{check_canvas, part_media};
use crate::routes::tier2_helpers::{OutputFile, persist_generation_files};

pub struct H3Adapter {
    state: AppState,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct H3Payload {
    chain: Value,
    job_id: String,
    #[serde(default)]
    assets: BTreeMap<String, String>,
    /// koblem's own copy of the song. The engine's audio is a RECONSTRUCTION of it, so the supplied
    /// track is laid over each generation as it lands and over the finished container.
    #[serde(default)]
    soundtrack: Option<H3Soundtrack>,
    /// Where each generation starts in the song, in seconds. Computed at enqueue from the packing.
    #[serde(default)]
    delivery_offsets: Vec<f64>,
    /// What the soundtrack plan sliced the song for, and how many frames koblem's
    /// mirrored `H3_TRIM_JOIN` expects the shim to eat at each join. The seam
    /// guard compares both against what actually landed — see the call site.
    #[serde(default)]
    sliced_for_seconds: Option<f64>,
    #[serde(default)]
    expected_trim_frames: u32,
    #[serde(default = "enabled")]
    save_clip: bool,
    #[serde(default)]
    director_project_id: Option<String>,
    #[serde(default)]
    director_tier: Option<String>,
    #[serde(default)]
    director_wipe: bool,
}

#[derive(Debug, Default, Deserialize)]
struct H3Resume {
    #[serde(default)]
    generations_done: u32,
    /// `(generation index, bank key)`. What the first surviving generation's `continuation.from`
    /// has to point at: `"prev"` resolves against the previous generation IN THIS JOB, and after a
    /// preempt there is not one.
    #[serde(default)]
    bank_keys: Vec<(u32, String)>,
}

/// koblem's copy of the project soundtrack, written beside the job's clip for the life of ONE run.
///
/// 🔴 STAGED AT DISPATCH, UNLINKED ON EVERY EXIT. Unlinking lives in `Drop` rather than on each
/// return path because there are eight of them (preempt, engine error, fetch failure, cancel, mux,
/// persist, success, panic) and it used to sit on the success path alone — a night of preempted
/// music-video renders parked a whole soundtrack per render in a directory whose sweep only knows
/// the ids it minted itself.
struct StagedSong {
    path: PathBuf,
}

impl Drop for StagedSong {
    fn drop(&mut self) {
        // Synchronous, because `Drop` cannot await and this must also run while unwinding.
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %self.path.display(), error = %e,
                  "h3: could not unlink the staged soundtrack; it is a whole song nothing else \
                   cleans");
        }
    }
}

fn chain_of(payload: &Value) -> &Value {
    payload.get("chain").unwrap_or(payload)
}

fn generations_of(payload: &Value) -> &[Value] {
    const NONE: &[Value] = &[];
    chain_of(payload)
        .get("generations")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(NONE)
}

/// The canvas a generation renders at: its own override, else the job's.
fn canvas_of(chain: &Value, generation: Option<&Value>) -> (u32, u32) {
    let get = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).map(|n| n as u32);
    let w = generation
        .and_then(|g| get(g, "width"))
        .or_else(|| get(chain, "width"))
        .unwrap_or(0);
    let h = generation
        .and_then(|g| get(g, "height"))
        .or_else(|| get(chain, "height"))
        .unwrap_or(0);
    (w, h)
}

fn frames_of(generation: &Value) -> u32 {
    generation
        .get("frames")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

fn payload_of(value: &Value) -> Result<H3Payload, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("unreadable h3 payload: {e}"))
}

fn verdict_of(e: &H3Error) -> Failure {
    match e {
        H3Error::Request(e) => verdict::transport(e),
        H3Error::Upstream(status, _) => verdict::upstream(*status),
        // 🔴 A generation here is ~14 minutes and the bank the resume needs lives on the ENGINE,
        // which a re-run does not inherit — so anything that is not the fleet saying "not now" is
        // worth a manual retry rather than three automatic full re-renders. `JobFailed` is the
        // node's own sentence and is never read.
        H3Error::NotConfigured
        | H3Error::JobFailed(..)
        | H3Error::Cancelled(_)
        | H3Error::MissingField(_)
        | H3Error::BadMedia(..)
        | H3Error::Stream(_) => Failure::Permanent,
    }
}

/// 🔴 `{ref|noref}` and not the generation count: `units_total` carries the length, so a
/// sixteen-generation cut is estimated from four-generation history.
fn shape_key_of(payload: &Value) -> String {
    let chain = chain_of(payload);
    let (w, h) = canvas_of(chain, generations_of(payload).first());
    let refs = generations_of(payload).iter().any(|g| {
        g.get("references")
            .and_then(Value::as_array)
            .is_some_and(|r| !r.is_empty())
    });
    format!("h3/{w}x{h}/{}", if refs { "ref" } else { "noref" })
}

fn units_of(payload: &Value) -> Option<i32> {
    let n = generations_of(payload).len();
    (n > 0).then_some(n as i32)
}

/// Pure arithmetic only, and every rule here would otherwise surface MINUTES into a render — after
/// the DiT and the text encoder have loaded — which under a queue is hours after the person who
/// typed it went to bed.
fn validate_payload(payload: &Value) -> Result<(), String> {
    let chain = chain_of(payload);
    let generations = generations_of(payload);
    if generations.is_empty() {
        return Err(
            "an h3 render needs at least one generation — there is no unconditional path"
                .to_string(),
        );
    }
    if let (Some(w), Some(h)) = (
        chain.get("width").and_then(Value::as_u64),
        chain.get("height").and_then(Value::as_u64),
    ) {
        check_canvas(w as u32, h as u32, None)?;
    }
    for (gi, g) in generations.iter().enumerate() {
        if g.get("width").is_some() || g.get("height").is_some() {
            let (w, h) = canvas_of(chain, Some(g));
            check_canvas(w, h, Some(gi))?;
        }
        let frames = frames_of(g);
        if !(H3_MIN_FRAMES..=H3_MAX_FRAMES).contains(&frames) {
            return Err(format!(
                "generation {} is {frames} frames, outside H3's {H3_MIN_FRAMES}-{H3_MAX_FRAMES} \
                 range",
                gi + 1
            ));
        }
        if !(frames - H3_FRAME_BASE).is_multiple_of(H3_FRAME_PERIOD) {
            return Err(format!(
                "generation {} is {frames} frames, which is off H3's frame grid — a legal length \
                 is {H3_FRAME_BASE} + {H3_FRAME_PERIOD}n",
                gi + 1
            ));
        }
        // 🔴 The phase rule, and it is FATAL. The carry is taken from the previous generation's
        // tail, so `(T_prev - k)` has to land on the latent grid or the node raises — minutes in,
        // with the whole chain's checkpoints already loaded.
        //
        // ⚠️ `T_prev` is LATENT TOKENS. This read the pixel frame count, which is a different
        // number that happens to divide by the same 5, so the check ran and lied: it refused the
        // default `k=7` off a 345-frame shot and offered `k=5`, which the backend then refuses.
        // 16 of H3's 20 legal lengths were unchainable at the default depth.
        let Some(k) = g.pointer("/continuation/k").and_then(Value::as_u64) else {
            continue;
        };
        let from = g
            .pointer("/continuation/from")
            .and_then(Value::as_str)
            .unwrap_or("");
        // A bank key names a generation this job does not contain, so its length is not knowable
        // here; only an in-job carry can be checked.
        if from != koblem_client::types::H3_CONTINUATION_FROM_PREV {
            continue;
        }
        let Some(prev) = generations.get(gi.wrapping_sub(1)) else {
            continue;
        };
        // Named `prev_frames`, never `t_prev`: `T_prev` is the LATENT count in every rule that
        // mentions it, and calling the frame count that is how the two got mixed up here.
        let prev_frames = frames_of(prev);
        let k = k as u32;
        if !h3_carry_is_on_phase(prev_frames, k) {
            return Err(format!(
                "generation {} carries k={k} latent tokens from a {prev_frames}-frame generation \
                 (T_prev={} tokens), but the engine needs (T_prev - k) to divide by \
                 {H3_LATENT_CYCLE}. Legal depths here: {:?}",
                gi + 1,
                h3_latents_for_frames(prev_frames),
                h3_legal_carry_depths(prev_frames),
            ));
        }
    }
    Ok(())
}

/// Where a resume has to restart: the first generation with no delivered take.
///
/// 🔴 A COUNT would be wrong. The partial list survives a preempt and a generation that arrived
/// unplayable is deliberately never published, so `len()` on a list missing index 3 would resume at
/// 4 and leave a permanent hole in the timeline.
fn first_gap(partials: &[koblem_client::types::VideoJobPartial]) -> u32 {
    let mut next = 0;
    while partials.iter().any(|p| p.segment_index == next) {
        next += 1;
    }
    next
}

fn kind_of(kind: Option<&str>) -> H3ReferenceKind {
    match kind {
        Some("video") => H3ReferenceKind::Video,
        Some("audio") => H3ReferenceKind::Audio,
        _ => H3ReferenceKind::Image,
    }
}

impl H3Adapter {
    pub fn new(state: AppState) -> Self {
        H3Adapter { state }
    }

    /// Trim the generations a previous run already banked and re-point the survivor's carry at the
    /// bank key that replaces `"prev"`. Returns how many generations were dropped — the offset every
    /// partial this run delivers has to be filed under, or take 1 of the resume overwrites take 1 of
    /// the timeline.
    fn apply_resume(chain: &mut Value, resume: &H3Resume) -> Result<u32, String> {
        let done = resume.generations_done as usize;
        if done == 0 {
            return Ok(0);
        }
        let Some(generations) = chain.get_mut("generations").and_then(Value::as_array_mut) else {
            return Ok(0);
        };
        if done >= generations.len() {
            return Err(format!(
                "the resume says {done} generations are already banked, but the chain only has {}",
                generations.len()
            ));
        }
        let carry_from = generations.get(done - 1).map(frames_of).unwrap_or_default();
        let tail = generations.split_off(done);
        let banked = std::mem::replace(generations, tail);
        let Some(first) = generations.first_mut() else {
            return Ok(done as u32);
        };
        if let Some(c) = first.get_mut("continuation").and_then(Value::as_object_mut)
            && c.get("from").and_then(Value::as_str)
                == Some(koblem_client::types::H3_CONTINUATION_FROM_PREV)
        {
            let key = resume
                .bank_keys
                .iter()
                .find(|(i, _)| *i as usize == done - 1)
                .map(|(_, k)| k.clone())
                .ok_or_else(|| {
                    format!(
                        "generation {} continues from the one before it, but the resume banked no \
                         key for generation {done} — continuing would re-render from the top",
                        done + 1
                    )
                })?;
            let k = c.get("k").and_then(Value::as_u64).unwrap_or(0) as u32;
            // Same phase rule, same unit: LATENT TOKENS, not frames. A bank carry is checked
            // against the banked generation's own `latent_t` backend-side, but the length it was
            // rendered at is the same number this chain declares.
            if !h3_carry_is_on_phase(carry_from, k) {
                return Err(format!(
                    "resuming at generation {} would carry k={k} from a {carry_from}-frame \
                     generation (T_prev={} tokens), which is off the {H3_LATENT_CYCLE}-token \
                     phase grid. Legal depths here: {:?}",
                    done + 1,
                    h3_latents_for_frames(carry_from),
                    h3_legal_carry_depths(carry_from),
                ));
            }
            c.insert("from".into(), json!(key));
        }
        let _ = banked;
        Ok(done as u32)
    }

    /// The reference bytes, back out of the content-addressed store. The `part` names in the chain
    /// are the ones `stage_refs` minted at enqueue, so the shim's own `part` back-pointers still
    /// resolve.
    async fn resolve_parts(&self, p: &H3Payload) -> Result<Vec<H3RefPart>, String> {
        let dir = self.state.director_assets_dir.as_ref();
        let mut kinds: BTreeMap<String, H3ReferenceKind> = BTreeMap::new();
        for g in generations_of(&p.chain) {
            let Some(refs) = g.get("references").and_then(Value::as_array) else {
                continue;
            };
            for r in refs {
                if let Some(name) = r.get("part").and_then(Value::as_str) {
                    kinds.insert(
                        name.to_string(),
                        kind_of(r.get("kind").and_then(Value::as_str)),
                    );
                }
            }
        }
        let mut out = Vec::with_capacity(p.assets.len());
        for (name, reference) in &p.assets {
            let bytes = director_assets::resolve_media(dir, reference)
                .await
                .map_err(|e| format!("reference part `{name}`: {e}"))?
                .ok_or_else(|| format!("reference part `{name}` resolved to nothing"))?;
            // 🔴 The extension is load-bearing: WanGP picks a demuxer by suffix, so a webm named
            // `.mp4` fails to decode two layers below anything that can name the reference.
            let (ext, mime) = part_media(
                kinds.get(name).copied().unwrap_or(H3ReferenceKind::Image),
                &bytes,
            );
            out.push(H3RefPart {
                file_name: format!("{name}.{ext}"),
                mime: mime.to_string(),
                bytes: bytes.to_vec(),
                name: name.clone(),
            });
        }
        Ok(out)
    }

    /// koblem's copy of the song, on disk BEFORE the first generation lands: each partial is muxed
    /// as it arrives, because the partials are what the Director files as takes.
    async fn stage_song(&self, p: &H3Payload, warnings: &mut Vec<String>) -> Option<StagedSong> {
        let st = p.soundtrack.as_ref()?;
        if st.mode == H3SoundtrackMode::Keep {
            return None;
        }
        let dir = self.state.director_assets_dir.as_ref();
        match director_assets::resolve_media(dir, &st.audio).await {
            Ok(Some(bytes)) => {
                let path = self
                    .state
                    .ltx_jobs
                    .clip_path(&p.job_id)
                    .with_extension("soundtrack.src");
                match tokio::fs::write(&path, &bytes).await {
                    Ok(()) => Some(StagedSong { path }),
                    Err(e) => {
                        h3_soundtrack::note_mux_failure(
                            &p.job_id,
                            &format!("writing the soundtrack to disk: {e}"),
                            warnings,
                        );
                        None
                    }
                }
            }
            Ok(None) => {
                h3_soundtrack::note_mux_failure(
                    &p.job_id,
                    "the project soundtrack carries no audio — the block is set but its audio \
                     field is empty",
                    warnings,
                );
                None
            }
            Err(e) => {
                h3_soundtrack::note_mux_failure(
                    &p.job_id,
                    &format!("reading the project soundtrack: {e}"),
                    warnings,
                );
                None
            }
        }
    }

    async fn collect(&self, p: &H3Payload, warnings: &[String]) {
        let jobs = self.state.ltx_jobs.clone();
        let clip_path = jobs.clip_path(&p.job_id);

        if p.save_clip {
            let prompts: Vec<&str> = generations_of(&p.chain)
                .iter()
                .filter_map(|g| g.get("prompt").and_then(Value::as_str))
                .collect();
            let (w, h) = canvas_of(chain_of(&p.chain), generations_of(&p.chain).first());
            persist_generation_files(
                &self.state,
                "minimax-h3",
                "generate",
                json!({
                    "engine": "h3",
                    "scripts": prompts,
                    "generations": generations_of(&p.chain).len(),
                    "width": w,
                    "height": h,
                    "delivery_warnings": warnings,
                    "queued": true,
                }),
                &[OutputFile {
                    ext: H3_OUTPUT_FORMAT.to_string(),
                    mime: H3_MIME_TYPE.to_string(),
                    kind: "video".to_string(),
                    path: clip_path,
                }],
            )
            .await;
        }

        // Written unconditionally — an empty list is what clears a previous run's banner, and a
        // `warn!` is not a report: this service's docker logs rotate within hours.
        jobs.update(&p.job_id, |j| {
            j.state = VideoJobState::Completed;
            j.content_type = Some(H3_MIME_TYPE.to_string());
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
            _ => {
                maybe_harvest_director(self.state.clone(), p.job_id.clone(), p.director_wipe).await
            }
        }
    }
}

#[async_trait]
impl JobRunner for H3Adapter {
    fn kind(&self) -> JobKind {
        JobKind::H3
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::MinimaxH3
    }

    /// 🔴 Costs up to one whole generation, which at prod shape is ~14 minutes — and it is still
    /// the cheapest stop there is, because the bank keys let the rest resume.
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
        let generations = generations_of(payload);
        let (w, h) = canvas_of(chain_of(payload), generations.first());
        let first = generations
            .first()
            .and_then(|g| g.get("prompt"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(40)
            .collect::<String>();
        format!("h3 · {} generations · {w}x{h} · {first}", generations.len())
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.minimax_h3;
        if !client.is_configured() {
            return Err("minimax-h3 is not configured".to_string());
        }
        let p = payload_of(&ctx.payload)?;
        let jobs = self.state.ltx_jobs.clone();
        if jobs.get(&p.job_id).is_none() {
            return Err(format!(
                "ltx job record `{}` is gone — the render was not started because its generations \
                 would have nowhere to land",
                p.job_id
            ));
        }

        let resume: H3Resume = match ctx.resume.clone() {
            Some(v) => {
                serde_json::from_value(v).map_err(|e| format!("unreadable h3 resume state: {e}"))?
            }
            None => H3Resume::default(),
        };
        let mut warnings: Vec<String> = Vec::new();
        let mut chain = chain_of(&p.chain).clone();
        let offset = Self::apply_resume(&mut chain, &resume)?;
        if offset > 0 {
            // 🔴 What this job's `/media` holds is now a FRAGMENT of the timeline. Filing it as the
            // project's stitched cut would replace the whole cut with its tail.
            jobs.update(&p.job_id, |j| j.job_is_partial = true);
        }

        let ref_parts = self.resolve_parts(&p).await?;
        // Advisory, echoed into the shim's logs: correlating an engine line with a koblem job
        // otherwise means matching timestamps.
        if let Some(o) = chain.as_object_mut() {
            o.insert("client_job_id".into(), json!(p.job_id.clone()));
        }

        let plan = client
            .submit(&chain, &ref_parts)
            .await
            .map_err(|e| verdict::note(format!("submit: {e}"), verdict_of(&e)))?;
        let server_job_id = plan.id.clone();
        // 🔴 SUBMIT-TIME notes are NOT delivery warnings, and must not ride the delivery banner.
        //
        // Everything the engine knew at the 202 — a frame count snapped onto the 5+17k grid, "this
        // chain is fused into one prompt so partials[] stays empty", a field it discarded — is
        // already stamped onto the project as the PLAN ECHO (`h3_plan`) and drawn by the plan
        // panel, which keeps `warnings` / `ignored` / `dropped` apart and labels each for what it
        // is. Copying them into `warnings` gave them a SECOND home under a red "N things did not
        // deliver" headline, and that banner is read HOURS after submit — after the only moment any
        // of it could have been acted on. The fused-chain note therefore fired on every chained
        // render, always, describing a trade-off koblem made on purpose.
        //
        // That is the expensive kind of wrong. The banner exists for the one failure that is
        // INDISTINGUISHABLE from never having rendered (a generation refused at the door), so a
        // permanent false positive on it trains the author to dismiss it unread — which is exactly
        // what the duration guard's false positive did before `clip_seconds` fixed it.
        //
        // Logged, not dropped: the plan echo is the surface, this is the forensic copy.
        for w in &plan.warnings {
            info!(job = %p.job_id, "h3 plan note: {w}");
        }
        if !plan.ignored.is_empty() {
            info!(
                job = %p.job_id,
                ignored = %plan.ignored.join(", "),
                "h3: fields the engine accepted and discarded"
            );
        }
        // What the engine had ALREADY said before rendering, so the completion copy below can be
        // reduced to what assembly ADDED. See the note there.
        let submit_notes: HashSet<String> = plan.warnings.iter().cloned().collect();
        if let Err(e) =
            queue_rows::set_server_job_id(&self.state.db, &ctx.job.id, &server_job_id).await
        {
            warn!(job = %ctx.job.id, error = %e, "h3: could not record the engine job id");
        }
        let seed = plan
            .generation_plans
            .first()
            .and_then(|g| g.get("seed"))
            .and_then(Value::as_i64);
        jobs.update(&p.job_id, |j| {
            j.state = VideoJobState::Rendering;
            j.server_job_id = Some(server_job_id.clone());
            j.seed = seed;
            j.total_eta_seconds = plan.total_eta_seconds;
        });

        let staged_song = self.stage_song(&p, &mut warnings).await;
        let song_path = staged_song.as_ref().map(|s| s.path.clone());

        let (partial_tx, mut partial_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::minimax_h3::H3PartialChunk>();
        let drain = {
            let jobs = jobs.clone();
            let job_id = p.job_id.clone();
            let soundtrack = p.soundtrack.clone();
            let song_path = song_path.clone();
            let offsets = p.delivery_offsets.clone();
            tokio::spawn(async move {
                let mut notes: Vec<String> = Vec::new();
                while let Some(chunk) = partial_rx.recv().await {
                    // 🔴 The job's unit 0 is the project's unit `offset` on a resume. Filing it as 0
                    // would overwrite an unrelated take.
                    let index = chunk.segment_index + offset;
                    let path = jobs.partial_path(&job_id, index);
                    if let Err(e) = tokio::fs::write(&path, &chunk.bytes).await {
                        warn!(job = %job_id, generation = index, error = %e, "write h3 partial failed");
                        continue;
                    }
                    // 🔴 A generation once arrived as 344 packets of zeros behind a valid mp4 index
                    // and was published, harvested and marked selected. An unpublished generation is
                    // a gap the author can retake; a published black one is indistinguishable from a
                    // render that genuinely came out black.
                    if let Ok(probe) = h3_soundtrack::probe_media(&path).await
                        && let Some(reason) = probe.rejection()
                    {
                        warn!(job = %job_id, generation = index, %reason, "h3 partial REFUSED");
                        notes.push(format!(
                            "generation {} arrived unplayable and was DROPPED rather than delivered \
                             as a take — {reason}. Retake this shot to get its own file back.",
                            index + 1
                        ));
                        continue;
                    }
                    // 🔴 The song goes on BEFORE the URL is published: `push_partial` is what makes
                    // the file reachable to the preview and the harvest, so publishing first races
                    // the mux and a harvest in that window files the unmuxed bytes permanently.
                    if let (Some(st), Some(song)) = (soundtrack.as_ref(), song_path.as_ref()) {
                        match offsets.get(index as usize) {
                            Some(&at) => {
                                let model_audio =
                                    path.with_extension(h3_soundtrack::MODEL_AUDIO_EXT);
                                if let Err(e) =
                                    h3_soundtrack::extract_model_audio(&path, &model_audio).await
                                {
                                    warn!(job = %job_id, generation = index, error = %e, "h3: could not keep the model's own audio");
                                }
                                let muxed = path.with_extension("muxed.mp4");
                                match h3_soundtrack::mux_soundtrack(&path, song, st, at, &muxed)
                                    .await
                                {
                                    Ok(true) => {
                                        if let Err(e) = tokio::fs::rename(&muxed, &path).await {
                                            h3_soundtrack::note_mux_failure(
                                                &job_id,
                                                &format!(
                                                    "swapping the muxed generation {} into place: {e}",
                                                    index + 1
                                                ),
                                                &mut notes,
                                            );
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(e) => h3_soundtrack::note_mux_failure(
                                        &job_id,
                                        &format!("generation {}: {e}", index + 1),
                                        &mut notes,
                                    ),
                                }
                            }
                            None => h3_soundtrack::note_mux_failure(
                                &job_id,
                                &format!(
                                    "generation {} has no planned song offset ({} planned), so it \
                                     ships with the model's own audio",
                                    index + 1,
                                    offsets.len()
                                ),
                                &mut notes,
                            ),
                        }
                    }
                    // The LTX job route deliberately: an H3 job lives in the same store and the
                    // Director's preview must not need to know which engine produced it.
                    let url = format!("/api/v1/ltx-video/jobs/{job_id}/segments/{index}");
                    jobs.push_partial(&job_id, index, url);
                }
                notes
            })
        };

        // Progress and partials arrive at completely different rates — a generation every ~14
        // minutes, a step every ~45 s — so they are separate channels.
        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::minimax_h3::H3Progress>();
        let progress_drain = {
            let jobs = jobs.clone();
            let job_id = p.job_id.clone();
            tokio::spawn(async move {
                while let Some(pr) = progress_rx.recv().await {
                    jobs.update(&job_id, |j| {
                        j.progress = pr.progress;
                        j.current_step = pr.current_step;
                        j.total_steps = pr.total_steps;
                        j.phase.clone_from(&pr.phase);
                        j.preview_seq = pr.preview_seq;
                        j.generation_index = pr.generation_index.map(|i| i + offset);
                        j.generations_done = pr.generations_done;
                        // The checkpoint a preempt hands back, persisted as it lands.
                        if !pr.bank_keys.is_empty() {
                            j.bank_keys = pr
                                .bank_keys
                                .iter()
                                .map(|(i, k)| (*i + offset, k.clone()))
                                .collect();
                        }
                    });
                }
            })
        };

        let waiter = client.wait_with_partials(&server_job_id, Some(partial_tx), Some(progress_tx));
        tokio::pin!(waiter);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut interrupt = Interrupt::None;
        let outcome = loop {
            tokio::select! {
                done = &mut waiter => break done,
                _ = ticker.tick() => {
                    if let Some(j) = jobs.get(&p.job_id) {
                        // Delivered takes, absolute: the list survives a preempt, so it already
                        // counts what an earlier run banked.
                        let done_units = j.partials.len() as i32;
                        ctx.progress(
                            JobProgress {
                                phase: j.phase.clone(),
                                progress: j.progress,
                                current_step: j.current_step,
                                total_steps: j.total_steps,
                                preview_seq: j.preview_seq,
                                queue_position: None,
                            },
                            done_units,
                        )
                        .await;
                    }
                    if interrupt == Interrupt::None && ctx.cancel.is_stopping() {
                        interrupt = ctx.cancel.get();
                        // 🔴 A forced stop on H3 is not a SIGKILL — it cancels a render and waits
                        // out a diffusion step — so this is awaited rather than fired and forgotten.
                        if let Err(e) = client.cancel(&server_job_id).await {
                            warn!(job = %p.job_id, error = %e, "h3: engine cancel failed");
                        }
                    }
                }
            }
        };
        warnings.extend(drain.await.unwrap_or_default());
        let _ = progress_drain.await;

        if interrupt == Interrupt::Preempt {
            ctx.release_gpu();
            let banked = jobs.get(&p.job_id);
            let bank_keys = banked
                .as_ref()
                .map(|j| j.bank_keys.clone())
                .unwrap_or_default();
            let generations_done = banked.map(|j| first_gap(&j.partials)).unwrap_or(offset);
            // 🔴 Cancelling an H3 job does NOT sweep the bank keys already written. Each finished
            // generation is banked by a ComfyUI output node, so a killed chain keeps every segment it
            // completed; cancel only flips flags, and `bank.sweep()` runs from `assemble()`, which is
            // reached only after every prompt SUCCEEDED. Eviction is capacity-only with no TTL and
            // oldest-first, so a preempted job's entries are the newest and go last.
            //
            // The job RECORD still expires in ten minutes, which is why the keys are read off the
            // terminal poll rather than looked up later.
            info!(job = %p.job_id, generations_done, "h3: preempted at a generation boundary");
            return Ok(JobOutcome::Preempted {
                resume: json!({
                    "generations_done": generations_done,
                    "bank_keys": bank_keys,
                }),
            });
        }

        let clip_path = jobs.clip_path(&p.job_id);
        let (result, trim_report) = match outcome {
            // 🔴 Fetched under the guard: the shim holds the container under its own ten-minute TTL
            // and the next render to take the card can trigger an unload, so waiting would be racing
            // an eviction for the only copy. Everything after this — the mux, the gallery row, the
            // harvest — runs with the card released.
            Ok((result, trim)) => {
                let media = client
                    .media_to_file(&server_job_id, &result, &clip_path)
                    .await;
                ctx.release_gpu();
                // 🔴 Read from the COMPLETION body, not the submit response: the join-trim notes are
                // appended to the plan DURING assembly, so the copy logged at submit is necessarily
                // silent about anything that went wrong delivering.
                //
                // ⚠️ And only the DELTA against that copy. `/plan/warnings` at completion is a
                // SUPERSET — it still carries every parse-time advisory the shim raised before a
                // single frame was rendered. Pushing the whole list put those advisories in the
                // delivery banner by the back door, which is the same bug the submit site above
                // just stopped committing. What `_assemble` appended is the part that is about
                // delivering, and it is the only part worth waking somebody up for.
                for w in trim
                    .engine_warnings
                    .iter()
                    .filter(|w| !submit_notes.contains(*w))
                {
                    warnings.push(format!("the engine reported: {w}"));
                }
                match media {
                    Ok(_) => (result, trim),
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
                let cancelled = matches!(e, H3Error::Cancelled(_));
                jobs.update(&p.job_id, |j| {
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
            jobs.update(&p.job_id, |j| j.job_is_partial = true);
            warnings.push(result.job_media_is.clone().unwrap_or_else(|| {
                "the shim could not concatenate this render's generations — every take is on disk, \
                 but the stitched cut is not. Rebuild the cut from the takes."
                    .to_string()
            }));
        }

        // 🔴 The song over the whole-job container, at the FIRST generation's offset rather than
        // zero: a resumed job holds the tail of the cut, and taking zero there would lay the opening
        // bars over the middle of the song.
        if let (Some(st), Some(song)) = (p.soundtrack.as_ref(), song_path.as_ref()) {
            let at = p
                .delivery_offsets
                .get(offset as usize)
                .copied()
                .unwrap_or(0.0);
            let muxed = clip_path.with_extension("muxed.mp4");
            match h3_soundtrack::mux_soundtrack(&clip_path, song, st, at, &muxed).await {
                Ok(true) => {
                    if let Err(e) = tokio::fs::rename(&muxed, &clip_path).await {
                        h3_soundtrack::note_mux_failure(
                            &p.job_id,
                            &format!("swapping the muxed clip into place: {e}"),
                            &mut warnings,
                        );
                    }
                }
                Ok(false) => {}
                Err(e) => h3_soundtrack::note_mux_failure(&p.job_id, &e, &mut warnings),
            }
        }
        // 🔴 The seam guard, and the only check that catches koblem's mirrored
        // `H3_TRIM_JOIN` drifting out of step with the shim's real `trim_join` —
        // a drift where the render and the mux both succeed and the song merely
        // slides a frame per seam. It has to run here because it needs three
        // things that only meet at this point: the completion body's trim
        // accounting, the slice the soundtrack was planned against, and the
        // duration of the file that actually landed.
        if let Some(sliced) = p.sliced_for_seconds {
            let trim = &trim_report;
            match h3_soundtrack::clip_seconds(&clip_path).await {
                Ok(delivered) => warnings.extend(h3_soundtrack::delivery_notes(
                    p.expected_trim_frames,
                    trim,
                    sliced,
                    delivered,
                )),
                // A duration we cannot read is not evidence of a drift. Say
                // nothing rather than accuse a constant that may be correct.
                Err(e) => {
                    tracing::debug!(job = %p.job_id, error = %e, "h3: could not measure the delivered cut")
                }
            }
        }

        self.collect(&p, &warnings).await;
        Ok(JobOutcome::Done { warnings })
    }

    /// 🔴 No resumable branch, unlike ltx25: continuity across a generation boundary is a pin, not a
    /// bank splice, so a job whose engine record is gone is gone — offering a "continue" would
    /// silently re-render everything and call it a resume.
    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.minimax_h3;
        let p = payload_of(&ctx.payload)?;
        let Some(server_job_id) = ctx.job.server_job_id.clone() else {
            return Err("reattach with no engine job id".to_string());
        };
        let jobs = self.state.ltx_jobs.clone();
        let mut warnings: Vec<String> = Vec::new();

        // Before the media is touched: the fast path recovers a render that finished while koblem
        // was down without ever waiting, so it is the only place this can be learned — and filing
        // generation 0 as the project's cut replaces the whole cut with its opening shot.
        if let Ok(body) = client.status(&server_job_id).await
            && crate::minimax_h3::result_is_partial(&body)
        {
            jobs.update(&p.job_id, |j| j.job_is_partial = true);
        }

        let clip_path = jobs.clip_path(&p.job_id);
        // 🔴 Ten minutes: `COMFY_API_JOB_TTL`. A 410 here is expected, not alarming.
        if client
            .fetch_media_to_file(&server_job_id, &clip_path)
            .await
            .is_ok_and(|n| n > 0)
        {
            ctx.release_gpu();
            self.collect(&p, &warnings).await;
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
                let cancelled = matches!(e, H3Error::Cancelled(_));
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
            jobs.update(&p.job_id, |j| j.job_is_partial = true);
            warnings.push(result.job_media_is.clone().unwrap_or_else(|| {
                "the shim could not concatenate this render's generations — every take is on disk, \
                 but the stitched cut is not. Rebuild the cut from the takes."
                    .to_string()
            }));
        }
        // The reattach path deliberately does NOT re-mux the song: the koblem-side plan that says
        // where each generation sits in it did not survive the restart, and a guessed offset is
        // silently wrong where no mux is loudly wrong.
        self.collect(&p, &warnings).await;
        Ok(JobOutcome::Done { warnings })
    }

    async fn cancel(&self, job: &Job) -> Result<(), String> {
        let Some(sid) = job.server_job_id.as_deref() else {
            return Ok(());
        };
        match self.state.minimax_h3.cancel(sid).await {
            Ok(_) => Ok(()),
            // 410 = the record already expired, so nothing is holding the worker.
            Err(H3Error::Upstream(410, _)) => Ok(()),
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

    /// The preempt path is the overnight path, and it returns long before the success-path
    /// cleanup this used to be — hence a `Drop` rather than a call on each return.
    #[test]
    fn the_staged_soundtrack_is_unlinked_when_the_run_ends() {
        let dir = std::env::temp_dir().join(format!("koblem-h3-song-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("koblem-1.soundtrack.src");
        std::fs::write(&path, b"pretend-song").expect("write");
        {
            let _staged = StagedSong { path: path.clone() };
            assert!(path.exists(), "still needed while the run holds it");
        }
        assert!(
            !path.exists(),
            "a preempted music video must not park its soundtrack in the job store"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn payload(w: u32, h: u32, gens: &[(u32, Option<u32>)]) -> Value {
        json!({
            "job_id": "koblem-1",
            "chain": {
                "width": w,
                "height": h,
                "fps": 24,
                "generations": gens
                    .iter()
                    .enumerate()
                    .map(|(i, (frames, k))| {
                        let mut g = json!({ "prompt": "a shot", "frames": frames });
                        if let Some(k) = k {
                            g["continuation"] = json!({ "mode": "latents", "from": "prev", "k": k });
                        }
                        let _ = i;
                        g
                    })
                    .collect::<Vec<Value>>(),
            },
        })
    }

    /// 🔴 The generation count is `units_total`, not part of the key — that is what lets a
    /// sixteen-generation cut be estimated from four-generation history.
    #[test]
    fn the_generation_count_is_not_in_the_key() {
        assert_eq!(
            shape_key_of(&payload(1344, 768, &[(345, None)])),
            shape_key_of(&payload(1344, 768, &[(345, None), (345, Some(7))])),
        );
        assert_eq!(
            shape_key_of(&payload(1344, 768, &[(345, None)])),
            "h3/1344x768/noref"
        );
        assert_eq!(
            units_of(&payload(1344, 768, &[(345, None), (345, Some(7))])),
            Some(2)
        );
    }

    /// 🔴 THIS TEST WAS INVERTED, and it pinned the bug in place: it asserted `k=5` off a
    /// 345-frame shot passes and the default `k=7` does not. Both are backwards. `T_prev` is
    /// LATENT TOKENS — 345 frames is 102 tokens — so `102 - 7 = 95` is on phase and `102 - 5 =
    /// 97` is not. Because `latents_for_frames` is `≡ 2 (mod 5)` at every legal length, the legal
    /// depths are `{2, 7, 12, …}` for every shot, and the old check passed only the four lengths
    /// (22, 107, 192, 277) whose PIXEL count happens to be `≡ 2 (mod 5)` as well.
    #[test]
    fn the_frame_grid_and_the_phase_rule_are_fatal() {
        // The default depth chains off the longest legal shot, which is the whole complaint.
        assert!(validate_payload(&payload(1344, 768, &[(345, None), (345, Some(7))])).is_ok());
        assert!(validate_payload(&payload(1344, 768, &[(345, None), (345, Some(12))])).is_ok());
        // …and off the shortest, and off a length the old rule accepted, unchanged.
        assert!(validate_payload(&payload(1344, 768, &[(22, None), (345, Some(2))])).is_ok());
        assert!(validate_payload(&payload(1344, 768, &[(277, None), (345, Some(7))])).is_ok());
        // 340 is not 5 + 17n.
        let e = validate_payload(&payload(1344, 768, &[(340, None)])).expect_err("off grid");
        assert!(e.contains("frame grid"), "{e}");
        // (102 - 5) % 5 != 0: the node raises MINUTES into the render, which under a queue is
        // hours — and this is the depth the old message TOLD people to use.
        let e = validate_payload(&payload(1344, 768, &[(345, None), (345, Some(5))]))
            .expect_err("off phase");
        assert!(e.contains("T_prev=102"), "{e}");
        assert!(
            e.contains("7"),
            "the legal depths belong in the message: {e}"
        );
        // A carry deeper than the previous shot's own latents is refused, not silently clamped.
        let e = validate_payload(&payload(1344, 768, &[(22, None), (345, Some(12))]))
            .expect_err("deeper than T_prev");
        assert!(e.contains("T_prev=7"), "{e}");
        let e = validate_payload(&payload(1000, 768, &[(345, None)])).expect_err("off the 32 grid");
        assert!(e.contains("multiple of 32"), "{e}");
    }

    /// 🔴 A generation is ~14 minutes and the bank a resume needs is the engine's, not koblem's, so
    /// only the fleet's own back-pressure buys another card here.
    #[test]
    fn only_back_pressure_and_breakage_are_retried() {
        let t = |e: H3Error| verdict_of(&e);
        assert_eq!(
            t(H3Error::Upstream(429, "queue full".into())),
            Failure::Transient
        );
        assert_eq!(
            t(H3Error::Upstream(503, "draining".into())),
            Failure::Transient
        );
        assert_eq!(t(H3Error::Upstream(500, "boom".into())), Failure::Transient);
        assert_eq!(
            t(H3Error::Upstream(400, "bad phase".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(H3Error::Upstream(410, "ttl expired".into())),
            Failure::Permanent
        );
        assert_eq!(t(H3Error::NotConfigured), Failure::Permanent);
        assert_eq!(t(H3Error::Cancelled("j".into())), Failure::Permanent);
        assert_eq!(
            t(H3Error::JobFailed("j".into(), "node raised".into())),
            Failure::Permanent
        );
        assert_eq!(t(H3Error::MissingField("media")), Failure::Permanent);
        assert_eq!(
            t(H3Error::BadMedia("j".into(), "not mp4".into())),
            Failure::Permanent
        );
    }

    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = H3Error::Upstream(429, "h3 pending queue is full".into());
        assert_eq!(
            verdict::recall(&verdict::note(format!("submit: {e}"), verdict_of(&e))),
            Failure::Transient
        );
        let e = H3Error::Upstream(422, "h3 pending queue is full".into());
        assert_eq!(
            verdict::recall(&verdict::note(format!("submit: {e}"), verdict_of(&e))),
            Failure::Permanent
        );
    }
}
