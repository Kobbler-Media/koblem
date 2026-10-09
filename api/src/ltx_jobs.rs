//! koblem-side job store for multi-segment renders — disk-backed so tracking and the finished
//! clip survive a koblem restart, not just a browser disconnect.
//!
//! 🔴 **NOT A SCHEDULER ANY MORE.** The global queue (`gpu_jobs`) decides what runs and when; this
//! is the MEDIA record beside it. The browser submits a render, gets a koblem `job_id` back
//! immediately (HTTP 202) and polls it here, and the queue adapter writes every partial and the
//! finished clip under that id precisely so the URL the browser already holds never moves. The
//! queue row lives in a different id space — [`LtxJob::queue_job_id`] is the link.
//!
//! Persistence: each job's metadata is a `<dir>/<id>.json` sidecar (rewritten on every state
//! change, atomically); the finished clip is `<dir>/<id>.<ext>`, where the extension follows the
//! ENGINE — see [`LtxJob::clip_ext`]. On startup the store reloads the sidecars; re-attaching a
//! render left in flight is the queue runner's job, off the `gpu_jobs` row. Finished jobs past
//! [`JOB_TTL_SECS`] are swept.
//!
//! Despite the name this store backs BOTH Director engines — the ComfyUI LTX-2.5 plugin and
//! MiniMax-H3. They share it deliberately: one poll route, one TTL sweep, one restart-reattach for
//! both. Where they differ the difference is a field (`engine`, the H3 progress block) rather than
//! a second store — and the unit a `partials` entry describes differs with it: a SHOT on LTX, a
//! whole GENERATION on H3.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use koblem_client::types::{VideoJobPartial, VideoJobState};

use crate::routes::ltx_director::{ENGINE_H3, ENGINE_LTX, ENGINE_LTX25};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LtxJob {
    pub state: VideoJobState,
    /// The engine's own job id (its artifact-dir key). Surfaced so a failed or cancelled render
    /// can be resumed (`resume_job_id`) from its banked segments, and so a koblem restart can
    /// re-attach to the still-running server job.
    pub server_job_id: Option<String>,
    pub segments: u32,
    pub scripts: Vec<String>,
    pub gate_wait_ms: u64,
    pub render_ms: u64,
    pub error: Option<String>,
    /// Set once the clip file (`<dir>/<id>.<ext>`) exists. `None` until completed.
    pub content_type: Option<String>,
    /// Cooperative-cancel flag. A `/cancel` sets it; the background render task polls it at the
    /// GPU-acquire boundary so a job still WAITING on the heavy guard (not yet submitted to the
    /// engine — the case the engine `/cancel` can't reach) aborts before it ever renders. An
    /// in-flight engine render is stopped directly via the engine's cooperative cancel.
    /// `#[serde(default)]` so sidecars written before this field still load.
    #[serde(default)]
    pub cancel_requested: bool,
    /// Progressive per-segment deliveries banked so far (empty until the engine emits any, and for
    /// renders that don't request progressive delivery). `#[serde(default)]` so sidecars written
    /// before this field still load.
    #[serde(default)]
    pub partials: Vec<VideoJobPartial>,
    /// Things that went wrong DELIVERING a render that otherwise succeeded — a generation whose
    /// bytes would not decode and was dropped, a soundtrack mux that failed.
    ///
    /// 🔴 Carried on the job so the Director harvest can stamp it onto the project, because a
    /// `warn!` is not a report: this service's docker logs rotate (50m x 3) and the detail is gone
    /// within hours. The failure these exist for looks EXACTLY like "I forgot to press render" from
    /// the editor — a shot with no take and no explanation — which is the worst possible way for a
    /// dropped generation to present. `#[serde(default)]` so sidecars written before this field
    /// still load.
    ///
    /// ⚠️ DELIVERY-TIME ONLY. Nothing the engine knew at submit belongs here — a snapped frame
    /// count, a fused continuation chain, a discarded field are all already on the project as the
    /// plan echo, and the panel that draws it labels each for what it is. They were copied in here
    /// too until 2026-08-18, which put a permanent advisory under a red "did not deliver" headline
    /// on every chained render. A banner that fires when nothing is wrong is a banner nobody reads,
    /// and this one guards the failure that looks exactly like not having rendered.
    #[serde(default)]
    pub delivery_warnings: Vec<String>,
    /// Set when this render backs a Director project: the project id + tier (`proxy`|`final`) its
    /// output is persisted into on completion. Drives the harvest into the owned `director-renders`
    /// store (survives the job store's TTL sweep). `None` for a plain (non-Director) render.
    /// `#[serde(default)]` so sidecars written before these fields still load — and so the tag
    /// survives a koblem restart, letting the reattach path harvest the render it finishes.
    #[serde(default)]
    pub director_project_id: Option<String>,
    #[serde(default)]
    pub director_tier: Option<String>,
    /// Which engine is rendering this job: `"ltx25"` = the ComfyUI LTX-2.5 plugin, `"h3"` =
    /// minimax-h3. Always written now; `None` or `"ltx"` is a sidecar left by the retired C++
    /// `ltx-video` service, which nothing can re-poll — see [`engine_name`](Self::engine_name).
    ///
    /// Load-bearing across a RESTART, which is why it is persisted rather than derived: the
    /// reattach path has only the sidecar to go on, and re-polling an H3 job against the LTX
    /// client would ask the wrong service for a job id it has never heard of — a 404 that reads
    /// exactly like "the engine restarted too", so the render would be written off as lost. It
    /// also picks which client a `/cancel` reaches and which service the job-dir cleanup is sent
    /// to — `DELETE /ltx/v1/job` with another engine's id is a call to the wrong service.
    #[serde(default)]
    pub engine: Option<String>,
    /// Set when this render covers only PART of the timeline, starting at this unit index: the
    /// harvest adds it to every partial's `segment_index` to get the project row the take belongs
    /// to, and skips the stitched-cut take entirely.
    ///
    /// 🔴 **The unit is per-engine.** On LTX it is a shot. On H3 it is a GENERATION — an H3 retake
    /// re-renders the whole generation containing the requested shot, because a generation is the
    /// smallest thing H3 can render: the cuts inside it are prompt text, and `<scenetrans>` audio
    /// continuity across a cut exists only within one. A per-shot H3 retake is not expressible.
    ///
    /// Either way the job's `seg_0` is project unit `k`. Filing it as 0 would overwrite an
    /// unrelated take, and harvesting its one-unit "stitched" output as the project's cut would
    /// replace the whole timeline with a fragment. `None` = a full run whose indices are already
    /// the project's.
    ///
    /// `ltx25` leaves this `None` even for a retake: it keeps the caller's own segment numbering,
    /// so its takes file at their own index. What its `/media` holds is a separate question — see
    /// [`job_is_partial`](Self::job_is_partial).
    #[serde(default)]
    pub director_shot_offset: Option<i32>,
    /// The ENGINE's own statement that this job's `/media` is not the whole timeline.
    ///
    /// 🔴 Not a second spelling of [`director_shot_offset`](Self::director_shot_offset). That one is
    /// a SHIFT — an H3 retake renders project generation `k` as its own unit 0 — and it happens to
    /// double as the partial flag because a shifted job is always a partial one. `ltx25` needs the
    /// flag WITHOUT the shift: a resume or a retake there keeps the caller's own segment numbering,
    /// so its takes file at their own index while its `/media` is still only the shots it rendered.
    /// Filing that fragment as the project's stitched cut replaces a good full cut with a fragment,
    /// which is the failure this exists to stop.
    ///
    /// It is also true on a FULL render whose concatenation failed, where `/media` is segment 0
    /// alone — a different cause with the same consequence.
    ///
    /// Set from `plan.render_window.mode` on the first poll (so it survives a koblem restart and is
    /// there for the reattach path, which recovers a finished render without ever seeing a `result`
    /// block) and again from `result.job_is_partial` on completion. `#[serde(default)]` so sidecars
    /// written before this field still load — as `false`, which every one of them is.
    #[serde(default)]
    pub job_is_partial: bool,
    /// The engine's own statement that this job rendered only a SUB-RANGE of the caller's segments.
    ///
    /// 🔴 The half of [`job_is_partial`](Self::job_is_partial) that is about SHOTS rather than about
    /// bytes. That flag has two causes with one consequence for `/media` and two opposite ones for
    /// the harvest: a windowed render holds takes for the window only, so replacing the tier would
    /// delete the shots it restored from the bank — while a FULL render whose concatenation failed
    /// rendered every shot, and refusing it the swap leaves the previous render's trailing rows
    /// behind, so a timeline that got shorter rebuilds an over-long cut. `ltx25` distinguishes them
    /// itself (`plan.render_window.mode`, and `rendered_segments`, which is empty on the failed
    /// stitch); this is where that distinction is kept.
    ///
    /// H3 never sets it: its window is a SHIFT ([`director_shot_offset`](Self::director_shot_offset))
    /// and its `job_is_partial` means only "the concatenation failed".
    /// `#[serde(default)]` so sidecars written before this field still load — as `false`, which is
    /// what every one of them is, and which is safe regardless: a wipe needs the DISPATCH to have
    /// asked for one too, and a resume or retake dispatch never does.
    #[serde(default)]
    pub renders_window: bool,
    // ── H3 progress (see `minimax_h3::H3Progress`) ──────────────────────────────────────────
    //
    // The LTX stack has no numeric progress at all — state transitions plus `created_at` and
    // `render_ms` are its whole progress signal — so these are `None` on every LTX job and always
    // will be. They are here rather than in a parallel H3-only store because the Director polls ONE
    // job route for both engines, and a second store would mean a second poll, a second TTL sweep
    // and a second restart-reattach path.
    //
    // Plumbed before the UI that draws them exists, deliberately: the alternative is discovering at
    // UI time that the poll loop threw the numbers away, which is a change to the render path
    // rather than to a component.
    /// The engine's own ETA for the WHOLE job at submit time, including any checkpoint reload a
    /// mixed fl2va/ref2va chain forces. The one number worth showing before an hour-long render.
    #[serde(default)]
    pub total_eta_seconds: Option<f64>,
    /// 0.0-1.0 across the whole job.
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default)]
    pub current_step: Option<u32>,
    #[serde(default)]
    pub total_steps: Option<u32>,
    /// `denoising` | `decoding` | `loading` | `encoding`. `loading` is the checkpoint reload a
    /// mixed-task chain pays between generations, and is the difference between "stuck" and
    /// "swapping a 20 GB DiT".
    #[serde(default)]
    pub phase: Option<String>,
    /// Bumps when a new preview PNG is available at `/api/v1/minimax-h3/jobs/{id}/preview`. A
    /// sequence rather than a URL because the URL never changes — a client watches the number.
    #[serde(default)]
    pub preview_seq: Option<u64>,
    /// Which generation is rendering. `segments` holds how many there are.
    #[serde(default)]
    pub generation_index: Option<u32>,
    #[serde(default)]
    pub generations_done: Option<u32>,
    /// `(generation index, latent-bank key)` for each FINISHED generation.
    ///
    /// What a retake's `continuation.from` must point at: a retake renders one generation, so
    /// `"prev"` — which the shim resolves against the *previous generation in the same job* —
    /// resolves to nothing. The bank outlives the job TTL and a process restart precisely so this
    /// stays usable an hour later.
    #[serde(default)]
    pub bank_keys: Vec<(u32, String)>,
    /// Whether the harvest should offer this render to the unattended dailies review.
    ///
    /// 🔴 On the RECORD rather than inferred, and set only by a FULL run. The critic's own auto-
    /// retakes come back through the same harvest, so a harvest that decided for itself would
    /// recurse — and the dispatch that knows which kind of run this is finished hours before the
    /// harvest runs.
    #[serde(default)]
    pub director_critique: bool,
    /// The `gpu_jobs` row this render is scheduled on.
    ///
    /// 🔴 A DIFFERENT ID SPACE from this record's own. The queue mints its own id, and the adapters
    /// keep writing partials and the clip under THIS one so the URL the browser is already polling
    /// never moves — which leaves nothing else able to answer "where is it in the queue" or "did it
    /// fail before it ever reached the engine".
    #[serde(default)]
    pub queue_job_id: Option<String>,
    /// The chain seed this render was submitted with (`-1` = engine-random). Recorded as per-take
    /// provenance: a Director take is worth nothing to compare against if you cannot tell which
    /// roll produced it. `#[serde(default)]` so sidecars written before this field still load.
    #[serde(default)]
    pub seed: Option<i64>,
    pub created: i64,
    pub updated: i64,
}

impl LtxJob {
    /// True once the server job id is known and the job failed/cancelled — i.e. there are
    /// banked segments to continue from.
    ///
    /// 🔴 Gated on the BANK the resume would actually reach. The failure this prevents is not a
    /// 501: it is offering a "resume" that would silently re-render the whole timeline at full
    /// cost and report it as a resume.
    ///
    /// 🔴 `bank_live` is the DEPLOYMENT's half and the caller owns it
    /// (`LtxVideoClient::bank_live`), because a job record cannot know it: LTX-2.5 banks latents in
    /// a directory an operator configures, and with `COMFY_API_LTX_BANK_DIR` unset the same build
    /// answers 501 to the resume this flag invites. It rides the wire as `VideoJobStatus.resumable`
    /// and the `kob` CLI prints "resume with --resume-job-id" off it, so an unqualified `true` here
    /// is a command line that cannot work.
    pub fn resumable(&self, bank_live: bool) -> bool {
        self.banks_latents()
            && bank_live
            && self.server_job_id.is_some()
            && matches!(self.state, VideoJobState::Failed | VideoJobState::Cancelled)
    }

    /// Whether this job's engine banks per-segment latents — the thing a resume, a `bank_job_id`
    /// and a retake all continue FROM.
    ///
    /// 🔴 Everything but H3, and the untagged case is NOT dead: `ltx_2b_jobs` is a second
    /// [`LtxJobStore`] whose jobs carry no engine tag, and the 2B engine banks into the job dir it
    /// always writes. So this cannot narrow to `ltx25` without taking LTX-2B's resume with it.
    ///
    /// 🔴 TRUE ON `ltx25` SINCE kobbler `6ca0b6f2` ("a latent bank, resume, retake, and partials
    /// inside one prompt"). A ComfyUI custom node writes one entry per segment the moment it
    /// decodes — including for a run that later failed, which is what makes a twelve-shot chain that
    /// died on shot twelve resumable from shot eleven. What it does NOT do is re-pin the shots after
    /// a retake, which the editor models per engine as `retakeSplicesTail: false`.
    ///
    /// ⚠️ Deployment-conditional at the far end: with `COMFY_API_LTX_BANK_DIR` unset the plugin
    /// answers 501 to `resume_job_id` / `retake_from` and says so in its capabilities. That is what
    /// `LtxVideoProps::bank_enabled` reports, and it is a live fact about the running service rather
    /// than something a job's own record could know.
    ///
    /// H3's own `bank_key`s name a content-addressed store that a generation's `continuation.from`
    /// pins to, which is a different mechanism with the same word.
    pub fn banks_latents(&self) -> bool {
        !self.is_h3()
    }

    /// Is this job's `/media` a FRAGMENT of the project's timeline rather than the timeline?
    ///
    /// 🔴 The two fields are one question with two spellings, and every reader has to ask it the
    /// same way or the answer depends on who is asking. The harvest read both and the status body
    /// read only [`job_is_partial`](Self::job_is_partial), so an H3 single-generation retake — which
    /// is partial by its OFFSET and reports no flag — came back `job_is_partial: false` to any
    /// client that did not dispatch it. That client then adopted one generation as the whole cut.
    ///
    /// [`renders_window`](Self::renders_window) is ORed in for completeness rather than for a live
    /// case: `bank_report` sets `partial = window || result.job_is_partial`, so a windowed render
    /// already arrives with the flag. Stating it here makes "rendered a sub-range but claimed the
    /// whole cut" impossible by construction instead of true by way of a producer's convention.
    pub fn delivers_fragment(&self) -> bool {
        self.director_shot_offset.is_some() || self.job_is_partial || self.renders_window
    }

    /// Did this job render only PART of the caller's timeline?
    ///
    /// 🔴 The other half of [`delivers_fragment`](Self::delivers_fragment), and the two must not be
    /// asked in each other's place. That one is about the BYTES at `/media` and answers "may this be
    /// filed as the project's stitched cut". This one is about the SHOTS and answers "may this
    /// harvest drop the rows it did not write" — which a full render whose concatenation failed may,
    /// because it rendered every one of them.
    pub fn renders_subrange(&self) -> bool {
        self.director_shot_offset.is_some() || self.renders_window
    }

    /// Which engine rendered this job. A stored value wins; absent reads as `ltx`, which covers
    /// two untagged populations and nothing else: every job of the SECOND store (`ltx_2b_jobs`,
    /// whose renders are all the one engine, so they are created without a tag), and any sidecar
    /// still on disk from the retired C++ `ltx-video` service. Nothing writes `"ltx"`; it is only
    /// ever read — which still matters to [`clip_ext`](Self::clip_ext), since both of those
    /// populations are VP9-in-webm.
    pub fn engine_name(&self) -> &str {
        match self.engine.as_deref() {
            Some(e) if !e.is_empty() => e,
            _ => ENGINE_LTX,
        }
    }

    pub fn is_h3(&self) -> bool {
        self.engine_name() == ENGINE_H3
    }

    /// True for the ComfyUI LTX-2.5 plugin — the only LTX engine, and the only one of the two
    /// live engines that banks latents ([`Self::banks_latents`]).
    pub fn is_comfy_ltx(&self) -> bool {
        self.engine_name() == ENGINE_LTX25
    }

    /// The container extension this job's clip and partials are written with.
    ///
    /// 🔴 Not cosmetic. Both live engines deliver **mp4**: H3 as x265 (`H3_OUTPUT_CODEC=libx265_8`,
    /// CRF 8 — WanGP's own `libx264_8` default is CRF 23 and too lossy for a master koblem may
    /// re-encode), and `ltx25` because ComfyUI v0.32.0's only audio-carrying save node is
    /// `SaveVideo`, whose format enum is `["auto", "mp4"]` (`SaveWEBM` is VP9/AV1 but takes
    /// **images only** — there is no audio input, so it cannot deliver a muxed clip at all).
    /// **webm cannot carry HEVC at all**, so an mp4 payload in a `.webm` name is not a naming
    /// wart: the first `ffmpeg -c copy` concat that rebuilds a cut fails, at a point with nothing
    /// to say about where the wrong name came from.
    ///
    /// 🔴 The `webm` arm is the UNTAGGED one — LTX-2B's whole store, plus any sidecar the retired
    /// C++ `ltx-video` engine left behind for the rest of the TTL. Both are the sd.cpp fork, and
    /// both already have `.webm` files on disk that this is what finds.
    pub fn clip_ext(&self) -> &'static str {
        if self.engine_name() == ENGINE_LTX {
            "webm"
        } else {
            "mp4"
        }
    }

    /// `created` as a wire timestamp. `None` only for a stored value outside the
    /// representable range, which a client reads as "start time unknown".
    pub fn created_at(&self) -> Option<time::OffsetDateTime> {
        time::OffsetDateTime::from_unix_timestamp(self.created).ok()
    }
}

/// Keep finished jobs (and their clip files) ~an hour for reconnect/re-fetch.
const JOB_TTL_SECS: i64 = 3600;
/// Hard cap on retained jobs.
const MAX_JOBS: usize = 64;

#[derive(Clone)]
pub struct LtxJobStore {
    inner: Arc<RwLock<HashMap<String, LtxJob>>>,
    dir: Arc<PathBuf>,
}

impl LtxJobStore {
    /// Open (and create) the persistence dir, reloading any sidecars from a prior process.
    pub fn open(dir: PathBuf) -> Self {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(path = %dir.display(), error = %e, "failed to create ltx jobs dir");
        }
        let mut map = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Some(id) = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
                else {
                    continue;
                };
                match std::fs::read(&p)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<LtxJob>(&b).ok())
                {
                    Some(job) => {
                        map.insert(id, job);
                    }
                    None => {
                        tracing::warn!(path = %p.display(), "skipping unreadable ltx job sidecar")
                    }
                }
            }
        }
        tracing::info!(jobs = map.len(), dir = %dir.display(), "loaded persisted LTX jobs");
        let store = Self {
            inner: Arc::new(RwLock::new(map)),
            dir: Arc::new(dir),
        };
        {
            let mut g = store.inner.write().expect("ltx job store poisoned");
            Self::sweep(&store.dir, &mut g);
        }
        store
    }

    fn now() -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp()
    }

    /// On-disk path for the finished clip (`<dir>/<id>.<ext>`).
    ///
    /// Engine-aware via [`LtxJob::clip_ext`] — an H3 job's clip is `.mp4`. The extension is looked
    /// up from the STORE rather than passed in by each caller, so there is exactly one place that
    /// knows and no way for the writer and the reader to disagree about a file name. A job that is
    /// not in the store (swept, or a bad id) reads as webm, which is what every pre-H3 job is.
    pub fn clip_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.{}", self.ext_for(id)))
    }
    /// On-disk path for a progressively-delivered unit (`<dir>/<id>_seg_<n>.<ext>`) — a shot on
    /// LTX, a whole GENERATION on H3.
    pub fn partial_path(&self, id: &str, segment_index: u32) -> PathBuf {
        self.dir
            .join(format!("{id}_seg_{segment_index}.{}", self.ext_for(id)))
    }
    fn ext_for(&self, id: &str) -> &'static str {
        self.inner
            .read()
            .expect("ltx job store poisoned")
            .get(id)
            .map(LtxJob::clip_ext)
            .unwrap_or("webm")
    }
    /// The MIME a partial is served with. Same reasoning as [`clip_path`](Self::clip_path): the
    /// finished clip records its own `content_type`, but a partial is served before the job has
    /// one, so the type has to be derived from the engine.
    pub fn partial_content_type(&self, id: &str) -> &'static str {
        if self.ext_for(id) == "mp4" {
            "video/mp4"
        } else {
            "video/webm"
        }
    }
    fn sidecar_path(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.json"))
    }

    /// Write the sidecar atomically (temp + rename) so a crash mid-write can't corrupt it.
    fn persist(dir: &Path, id: &str, job: &LtxJob) {
        let Ok(bytes) = serde_json::to_vec(job) else {
            return;
        };
        let tmp = dir.join(format!("{id}.json.tmp"));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, Self::sidecar_path(dir, id));
        }
    }

    /// Register a freshly-accepted job and return its koblem id.
    pub fn create(&self, segments: u32, scripts: Vec<String>) -> String {
        self.create_inner(segments, scripts, None, None, None, None)
    }

    /// [`create`](Self::create) for a render whose engine is known — `"ltx25"` or `"h3"`.
    ///
    /// The tag has to be recorded at CREATE time rather than derived later: it picks which client
    /// polls, cancels and re-attaches the job, and — via [`LtxJob::clip_ext`] — the container its
    /// clip and every partial are written under. A job that learned its engine only once the render
    /// task started would have already had its file names chosen for it.
    pub fn create_on(&self, segments: u32, scripts: Vec<String>, engine: Option<String>) -> String {
        self.create_inner(segments, scripts, None, None, engine, None)
    }

    /// Like [`create`](Self::create) but tags the job as backing a Director project + tier, so the
    /// render's output is harvested into the owned `director-renders` store on completion (and again
    /// on restart-reattach, since the tag is persisted in the sidecar).
    pub fn create_director(
        &self,
        segments: u32,
        scripts: Vec<String>,
        project_id: String,
        tier: String,
    ) -> String {
        self.create_inner(segments, scripts, Some(project_id), Some(tier), None, None)
    }

    /// [`create_director`](Self::create_director) with the project's engine named: `"h3"` for
    /// minimax-h3, `"ltx25"` for the ComfyUI LTX-2.5 plugin. See [`LtxJob::engine`] for why this
    /// has to be recorded rather than re-derived from the project on reattach.
    ///
    /// `shot_offset` marks a PARTIAL render — see [`LtxJob::director_shot_offset`].
    pub fn create_director_on(
        &self,
        segments: u32,
        scripts: Vec<String>,
        project_id: String,
        tier: String,
        engine: Option<String>,
        shot_offset: Option<i32>,
    ) -> String {
        self.create_inner(
            segments,
            scripts,
            Some(project_id),
            Some(tier),
            engine,
            shot_offset,
        )
    }

    fn create_inner(
        &self,
        segments: u32,
        scripts: Vec<String>,
        director_project_id: Option<String>,
        director_tier: Option<String>,
        engine: Option<String>,
        director_shot_offset: Option<i32>,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Self::now();
        let job = LtxJob {
            state: VideoJobState::Queued,
            server_job_id: None,
            segments,
            scripts,
            gate_wait_ms: 0,
            render_ms: 0,
            error: None,
            content_type: None,
            cancel_requested: false,
            partials: Vec::new(),
            delivery_warnings: Vec::new(),
            director_project_id,
            director_tier,
            engine: engine.filter(|e| !e.is_empty()),
            director_shot_offset,
            // The engine's answer, not a guess: filled in from the first poll that echoes a plan.
            job_is_partial: false,
            renders_window: false,
            // Filled in at enqueue, once the chain request has resolved its seed.
            seed: None,
            // H3 only, and unknown until the engine has accepted the job / started stepping.
            // `None` reads as "unknown", never as zero — a progress bar drawn from step 0 of 0
            // would show a queued render as finished.
            total_eta_seconds: None,
            progress: None,
            current_step: None,
            total_steps: None,
            phase: None,
            preview_seq: None,
            generation_index: None,
            generations_done: None,
            bank_keys: Vec::new(),
            director_critique: false,
            queue_job_id: None,
            created: now,
            updated: now,
        };
        let mut g = self.inner.write().expect("ltx job store poisoned");
        Self::persist(&self.dir, &id, &job);
        g.insert(id.clone(), job);
        Self::sweep(&self.dir, &mut g);
        id
    }

    pub fn get(&self, id: &str) -> Option<LtxJob> {
        self.inner
            .read()
            .expect("ltx job store poisoned")
            .get(id)
            .cloned()
    }

    /// (id, job) snapshots, newest-first. Used by the list endpoint + startup reattach.
    pub fn list(&self) -> Vec<(String, LtxJob)> {
        let mut v: Vec<(String, LtxJob)> = self
            .inner
            .read()
            .expect("ltx job store poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.1.created));
        v
    }

    pub fn update<F: FnOnce(&mut LtxJob)>(&self, id: &str, f: F) {
        let mut g = self.inner.write().expect("ltx job store poisoned");
        if let Some(j) = g.get_mut(id) {
            f(j);
            j.updated = Self::now();
            Self::persist(&self.dir, id, j);
        }
    }

    /// Record a progressively-delivered shot (idempotent by `segment_index`) and persist. The
    /// background render task calls this once it has written the shot's webm under the job dir; the
    /// status serializer then surfaces the growing `partials` list so the UI can preview each shot
    /// before the stitched result lands.
    pub fn push_partial(&self, id: &str, segment_index: u32, url: String) {
        let mut g = self.inner.write().expect("ltx job store poisoned");
        if let Some(j) = g.get_mut(id) {
            if j.partials.iter().any(|p| p.segment_index == segment_index) {
                return;
            }
            j.partials.push(VideoJobPartial { segment_index, url });
            j.partials.sort_by_key(|p| p.segment_index);
            j.updated = Self::now();
            Self::persist(&self.dir, id, j);
        }
    }

    /// Flag a non-terminal job for cooperative cancellation. No-op for a job that's already
    /// finished. The background render task polls [`is_cancel_requested`](Self::is_cancel_requested)
    /// at the GPU-acquire boundary so a still-queued job aborts before it renders.
    pub fn request_cancel(&self, id: &str) {
        let mut g = self.inner.write().expect("ltx job store poisoned");
        if let Some(j) = g.get_mut(id)
            && !j.state.is_terminal()
            && !j.cancel_requested
        {
            j.cancel_requested = true;
            j.updated = Self::now();
            Self::persist(&self.dir, id, j);
        }
    }

    pub fn is_cancel_requested(&self, id: &str) -> bool {
        self.inner
            .read()
            .expect("ltx job store poisoned")
            .get(id)
            .map(|j| j.cancel_requested)
            .unwrap_or(false)
    }

    /// Retire the record behind a queue row that was cancelled before it ever ran. Returns the
    /// record's own id, or `None` when there is nothing to retire.
    ///
    /// 🔴 Every terminal state a render reaches is written by the adapter's run path — and a row
    /// cancelled while it is still QUEUED never enters that path. Nothing else was telling the
    /// record, so it said `queued` for the life of the process: [`sweep`](Self::sweep) drops
    /// terminal records only, so it was never reclaimed either, and every reader had to fold the
    /// queue row over it to avoid believing it. One such record had the Director tearing down and
    /// re-downloading its own footage every twelve seconds for a day.
    ///
    /// Keyed on [`LtxJob::queue_job_id`] rather than the map key: the record keeps its own koblem
    /// id (that is what `/jobs/{id}` is polled by) and merely points AT the row.
    pub fn cancel_by_queue_id(&self, queue_job_id: &str, error: &str) -> Option<String> {
        let id = {
            let g = self.inner.read().expect("ltx job store poisoned");
            g.iter()
                .find(|(_, j)| {
                    j.queue_job_id.as_deref() == Some(queue_job_id) && !j.state.is_terminal()
                })
                .map(|(k, _)| k.clone())
        }?;
        self.update(&id, |j| {
            j.state = VideoJobState::Cancelled;
            // Never over an account the engine already gave: a job interrupted mid-render reaches
            // here too, and "cancelled before it started" would be a worse and wronger sentence.
            if j.error.is_none() {
                j.error = Some(error.to_string());
            }
        });
        Some(id)
    }

    fn sweep(dir: &Path, g: &mut HashMap<String, LtxJob>) {
        let now = Self::now();
        // (id, partial segment indices) for every job we drop, so its `_seg_<n>.webm` files are
        // cleaned alongside the sidecar + stitched clip.
        // The extension travels WITH the id: by the time the files are removed the job is out of
        // the map, so `ext_for` would answer "webm" for an H3 job and leak every `.mp4` it wrote.
        let mut rm: Vec<(String, Vec<u32>, &'static str)> = Vec::new();
        g.retain(|id, j| {
            let drop = j.state.is_terminal() && now - j.updated > JOB_TTL_SECS;
            if drop {
                rm.push((
                    id.clone(),
                    j.partials.iter().map(|p| p.segment_index).collect(),
                    j.clip_ext(),
                ));
            }
            !drop
        });
        if g.len() > MAX_JOBS {
            let mut finished: Vec<(String, i64)> = g
                .iter()
                .filter(|(_, j)| j.state.is_terminal())
                .map(|(k, j)| (k.clone(), j.updated))
                .collect();
            finished.sort_by_key(|(_, u)| *u);
            let drop_n = g.len() - MAX_JOBS;
            for (k, _) in finished.into_iter().take(drop_n) {
                let (segs, ext) = g
                    .get(&k)
                    .map(|j| {
                        (
                            j.partials.iter().map(|p| p.segment_index).collect(),
                            j.clip_ext(),
                        )
                    })
                    // Not `unwrap_or_default()`: `&str`'s default is `""`, which would build
                    // `<id>.` and delete nothing.
                    .unwrap_or((Vec::new(), "webm"));
                g.remove(&k);
                rm.push((k, segs, ext));
            }
        }
        for (id, segs, ext) in rm {
            let _ = std::fs::remove_file(Self::sidecar_path(dir, &id));
            let _ = std::fs::remove_file(dir.join(format!("{id}.{ext}")));
            for n in segs {
                let _ = std::fs::remove_file(dir.join(format!("{id}_seg_{n}.{ext}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (LtxJobStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "koblem-ltxjobs-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        (LtxJobStore::open(dir.clone()), dir)
    }

    /// 🔴 The whole reason this method exists. A row cancelled while it was still QUEUED never
    /// enters the adapter's run path, which is the only thing that writes a terminal state onto the
    /// record — so without this the record says `queued` for the life of the process, is never
    /// swept (both eviction paths filter on `is_terminal`), and every reader has to fold the queue
    /// row over it. One of these had the Director tearing down and re-downloading its own footage
    /// every twelve seconds for a day.
    #[test]
    fn a_queue_cancel_retires_the_record_it_points_at() {
        let (jobs, dir) = store();
        let id = jobs.create(3, vec!["a".into()]);
        jobs.update(&id, |j| j.queue_job_id = Some("q-1".into()));

        assert_eq!(
            jobs.cancel_by_queue_id("q-1", "cancelled").as_deref(),
            Some(id.as_str())
        );
        let j = jobs.get(&id).expect("record");
        assert_eq!(j.state, VideoJobState::Cancelled);
        assert_eq!(j.error.as_deref(), Some("cancelled"));
        assert!(j.state.is_terminal(), "or sweep will never reclaim it");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Keyed on the ROW, not the record: the record keeps its own koblem id (that is what
    /// `/jobs/{id}` is polled by) and only points at the queue row. An id that names neither, and a
    /// record already closed by the engine's own account, are both left alone — a job interrupted
    /// mid-render reaches the same path, and "cancelled before it started" would be a wronger
    /// sentence than the one the adapter already wrote.
    #[test]
    fn nothing_else_is_touched_and_a_told_record_keeps_its_own_account() {
        let (jobs, dir) = store();
        let untouched = jobs.create(1, vec!["a".into()]);
        assert_eq!(jobs.cancel_by_queue_id("q-nothing", "cancelled"), None);
        assert_eq!(
            jobs.get(&untouched).expect("record").state,
            VideoJobState::Queued
        );

        let told = jobs.create(1, vec!["a".into()]);
        jobs.update(&told, |j| {
            j.queue_job_id = Some("q-2".into());
            j.state = VideoJobState::Failed;
            j.error = Some("the engine refused the chain".into());
        });
        assert_eq!(jobs.cancel_by_queue_id("q-2", "cancelled"), None);
        let j = jobs.get(&told).expect("record");
        assert_eq!(j.state, VideoJobState::Failed);
        assert_eq!(j.error.as_deref(), Some("the engine refused the chain"));

        let _ = std::fs::remove_dir_all(dir);
    }
}
