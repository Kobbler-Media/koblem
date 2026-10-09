//! koblem's catalogue of Pixal3D characters, motion clips and animation takes.
//!
//! # Why koblem stores anything at all, when the service already does
//!
//! `avatar_server` keeps every artifact on `/out` forever and never sweeps — but its job
//! **records** are capped at 512 and evicted oldest-first, and `GET /v1/jobs/{id}/result` reads
//! the record. So an artifact outlives the only HTTP handle to it. Everything durable in this
//! store follows from that one fact:
//!
//! * every delivered tier is **fetched to koblem's own disk on completion**, not proxied on
//!   demand — [`CharacterTier::filename`];
//! * the re-run handles kept are the **paths** the service publishes (`stage_dir`, `rig_cache`,
//!   a clip's `result_path`), never the job id, because a path is a file on a volume and a job id
//!   is an entry in a 512-slot map;
//! * a character's own `glb` path is kept per tier, so `/v1/animate` can be handed a rig by path
//!   long after the record that produced it is gone.
//!
//! # 🔴 NO TTL, and that is the difference from `upscale_jobs`
//!
//! An upscale's output expires because it is a big file that can be remade from a source the user
//! still has. A rigged character is ~10 minutes of GPU and the *only* copy — the thing the user
//! came here to make. It is deleted when they delete it. The service's own `/out` volume is
//! managed the same way and for the same reason.
//!
//! # The three lifetimes, which are not the same
//!
//! | | cost | lives in |
//! |---|---|---|
//! | **character** (a rig) | ~450–960 s of GPU | its own directory; permanent |
//! | **motion** (a clip) | ~11 s warm | a global library — a clip is retargeted BY BONE NAME, so it is not owned by the character it was first tried on |
//! | **animation** (a take) | 0.22 s, CPU | on the character; cheap enough to keep many and compare |
//!
//! ⚠️ **Nothing scores whether an animation is "right".** There is no quality signal for motion at
//! all, so the store's job is to make takes easy to keep and compare, not to rank them.
//!
//! # 🔴 This is a CATALOGUE, not a scheduler
//!
//! It once claimed a single GPU slot and refused a second rig while one was live. The global job
//! queue owns admission now, so a record here is only ever a durable home for what a run produces:
//! the tiers, the re-run handles, the edits, the takes and the clips. Creating one takes nothing
//! and refuses nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pixal3d::EngineGate;

// ── states ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    /// The engine holds the card.
    Running,
    /// The engine is done and koblem is pulling the artifacts onto its own disk. Its own state
    /// because the GPU is already free — a UI still saying "rigging" here is telling the user to
    /// wait for something that finished.
    Collecting,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

// ── the character ────────────────────────────────────────────────────────────

/// One rung of the LOD ladder, as koblem holds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterTier {
    pub tier: String,
    #[serde(default)]
    pub decimate: i64,
    #[serde(default)]
    pub joints: i64,
    /// How many of the 22 Mixamo core slots the namer filled. **This is the number that decides
    /// whether a humanoid clip will retarget**, and it is shared across the ladder.
    #[serde(default)]
    pub named_core: i64,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub skin_cleanup: String,
    /// 🔴 Only meaningful when `gate.draws > 0`. A tier that inherited the skeleton ran no gate and
    /// reports an empty block that reads exactly like a failure.
    #[serde(default)]
    pub gate: EngineGate,
    /// The cached GLB's name under the character's directory.
    pub filename: String,
    #[serde(default)]
    pub bytes: u64,
    /// The GLB's path **inside the engine container**, for `/v1/animate {rig: <path>}`. Outlives
    /// the job record.
    #[serde(default)]
    pub glb_path: String,
}

/// A rig the user repaired in the browser: skin-weight influences pruned, joints renamed, or both.
///
/// 🔴 It is a **new file**, never an overwrite of the tier it came from. The original rig is the
/// engine's output and stays exactly as delivered; an edit that turns out worse is discarded by
/// deleting it, not by re-running 450 s of GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RigEdit {
    pub id: String,
    pub label: String,
    /// Which tier's GLB was edited.
    pub base_tier: String,
    /// Under the character's directory.
    pub filename: String,
    #[serde(default)]
    pub bytes: u64,
    /// The copy in the SHARED staging directory, at the absolute path the engine container also
    /// mounts. `None` when the mount is missing — the edit is still downloadable, it just cannot
    /// be animated. See `routes::pixal3d`.
    #[serde(default)]
    pub staged_path: Option<PathBuf>,
    /// What the editor changed, as it reported it: influences pruned, vertices touched, joints
    /// renamed. Free-form because the editor owns the vocabulary.
    #[serde(default)]
    pub changes: Value,
    pub created_unix: i64,
}

/// Which GLB an animation was retargeted onto.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RigSource {
    /// A tier of the delivered ladder.
    Tier { tier: String },
    /// A browser-edited GLB, by its [`RigEdit::id`].
    Edit { edit_id: String },
}

/// One retarget take. Cheap (0.22 s), so keeping many is the point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Animation {
    pub id: String,
    pub state: JobState,
    #[serde(default)]
    pub engine_job_id: Option<String>,
    pub label: String,
    /// The motions baked into this take, by [`Motion::id`].
    #[serde(default)]
    pub motion_ids: Vec<String>,
    pub rig_source: RigSource,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub smooth: Option<u32>,
    #[serde(default)]
    pub exercise: bool,
    /// How many glTF animation tracks the delivered GLB carries.
    #[serde(default)]
    pub animations: i64,
    /// How many joints the retarget actually mapped, and how it matched them (`names` is the
    /// humanoid path). A low `mapped` on a humanoid is the visible consequence of a low
    /// `named_core`.
    #[serde(default)]
    pub mapped: i64,
    #[serde(default)]
    pub map_kind: String,
    #[serde(default)]
    pub exercise_joints: i64,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub error: Option<String>,
    pub created_unix: i64,
}

/// A rig, and everything that hangs off it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Character {
    pub id: String,
    pub name: String,
    pub state: JobState,
    #[serde(default)]
    pub engine_job_id: Option<String>,
    /// The submitted image, under the character's directory.
    #[serde(default)]
    pub source_filename: Option<String>,
    #[serde(default)]
    pub source_bytes: u64,
    /// 🔴 The same picture's path **inside the engine container** (`result.image`, normally
    /// `/out/jobs/<id>/input`). Every re-run needs it: the engine back-fills the image from the
    /// source job's record only when the re-run handle was a job **id**, and koblem sends paths
    /// because records are evicted at 512. A character without this cannot be re-rolled cheaply.
    #[serde(default)]
    pub engine_image: Option<String>,
    #[serde(default)]
    pub tiers: Vec<CharacterTier>,
    /// Written for a human by the engine — surfaced, never logged and dropped.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// 🔴 The re-run handles, as PATHS. `stage_dir` feeds `geometry_from` (~205 s, re-decodes the
    /// skeleton); `rig_cache` feeds `rig_cache_from` (~134 s, keeps it).
    #[serde(default)]
    pub stage_dir: Option<String>,
    #[serde(default)]
    pub rig_cache: Option<String>,
    /// The request as submitted, so a re-roll can be built from it rather than from the UI's
    /// current state.
    #[serde(default)]
    pub request: Value,
    #[serde(default)]
    pub tiers_requested: Vec<String>,
    #[serde(default)]
    pub rig_seed: Option<u32>,
    #[serde(default)]
    pub geo_seed: Option<i32>,
    /// The character this one is a re-roll of, so the UI can show them as takes of one subject
    /// rather than as unrelated uploads.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// `rig_seed` | `geo_seed` | `tiers` | `upload`.
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub peak_vram_mib: Option<i64>,
    #[serde(default)]
    pub seconds: Option<f64>,
    #[serde(default)]
    pub gate_wait_ms: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub edits: Vec<RigEdit>,
    #[serde(default)]
    pub animations: Vec<Animation>,
    pub created_unix: i64,
    #[serde(default)]
    pub finished_unix: Option<i64>,
}

impl Character {
    fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            state: JobState::Queued,
            engine_job_id: None,
            source_filename: None,
            source_bytes: 0,
            engine_image: None,
            tiers: Vec::new(),
            warnings: Vec::new(),
            stage_dir: None,
            rig_cache: None,
            request: Value::Null,
            tiers_requested: Vec::new(),
            rig_seed: None,
            geo_seed: None,
            parent_id: None,
            origin: "upload".into(),
            peak_vram_mib: None,
            seconds: None,
            gate_wait_ms: None,
            duration_ms: None,
            error: None,
            edits: Vec::new(),
            animations: Vec::new(),
            created_unix: now_unix(),
            finished_unix: None,
        }
    }

    pub fn tier(&self, name: &str) -> Option<&CharacterTier> {
        self.tiers.iter().find(|t| t.tier == name)
    }

    pub fn edit(&self, id: &str) -> Option<&RigEdit> {
        self.edits.iter().find(|e| e.id == id)
    }

    pub fn animation(&self, id: &str) -> Option<&Animation> {
        self.animations.iter().find(|a| a.id == id)
    }

    /// The GLB an animate request should name, and the tier hint that goes with it.
    pub fn rig_reference(&self, source: &RigSource) -> Option<String> {
        match source {
            RigSource::Tier { tier } => self
                .tier(tier)
                .map(|t| t.glb_path.clone())
                .filter(|p| !p.is_empty()),
            RigSource::Edit { edit_id } => self
                .edit(edit_id)
                .and_then(|e| e.staged_path.as_ref())
                .map(|p| p.display().to_string()),
        }
    }
}

// ── the motion library ───────────────────────────────────────────────────────

/// A generated clip. Global rather than per-character: the retarget matches joints **by name**, so
/// one clip plays on every humanoid rig here. Owning it to a character would hide that.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Motion {
    pub id: String,
    pub state: JobState,
    #[serde(default)]
    pub engine_job_id: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub seed: Option<u32>,
    #[serde(default)]
    pub seconds: Option<f32>,
    #[serde(default)]
    pub cfg: Option<f32>,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub frames: i64,
    #[serde(default)]
    pub fps: f64,
    /// The clip's path inside the engine container — what `/v1/animate` is handed.
    #[serde(default)]
    pub clip_path: Option<String>,
    /// False ⇒ this generation paid the ~21 s DiT load. Kept because it is the entire explanation
    /// for a first prompt that felt three times slower than the rest.
    #[serde(default)]
    pub dit_was_resident: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub gate_wait_ms: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    pub created_unix: i64,
}

impl Motion {
    fn new(id: String, prompt: String) -> Self {
        Self {
            id,
            state: JobState::Queued,
            engine_job_id: None,
            prompt,
            seed: None,
            seconds: None,
            cfg: None,
            steps: None,
            frames: 0,
            fps: 0.0,
            clip_path: None,
            dit_was_resident: false,
            error: None,
            gate_wait_ms: None,
            duration_ms: None,
            created_unix: now_unix(),
        }
    }
}

// ── the store ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Pixal3dStore {
    characters: Arc<RwLock<HashMap<String, Character>>>,
    motions: Arc<RwLock<HashMap<String, Motion>>>,
    dir: Arc<PathBuf>,
}

impl Pixal3dStore {
    pub fn open(dir: PathBuf) -> Self {
        for sub in ["characters", "motions"] {
            if let Err(e) = std::fs::create_dir_all(dir.join(sub)) {
                tracing::warn!(path = %dir.join(sub).display(), error = %e,
                               "failed to create a pixal3d store directory");
            }
        }
        let characters: HashMap<String, Character> =
            load_dir(&dir.join("characters"), |c: &Character| c.id.clone());
        let motions: HashMap<String, Motion> =
            load_dir(&dir.join("motions"), |m: &Motion| m.id.clone());
        tracing::info!(
            characters = characters.len(), motions = motions.len(), dir = %dir.display(),
            "loaded the pixal3d catalogue",
        );
        Self {
            characters: Arc::new(RwLock::new(characters)),
            motions: Arc::new(RwLock::new(motions)),
            dir: Arc::new(dir),
        }
    }

    /// Where a character's own files live: the source image, every cached tier GLB, the browser
    /// edits and the animation takes.
    pub fn character_dir(&self, id: &str) -> PathBuf {
        self.dir.join("characters").join(id)
    }

    /// An absolute path inside a character's directory.
    ///
    /// 🔴 Only the final component of `filename` is used. The names come off a sidecar, which is a
    /// file on disk — a hand-edited `../../etc/passwd` has to resolve to `passwd` inside the
    /// character's own directory rather than escaping it.
    pub fn file_path(&self, id: &str, filename: &str) -> PathBuf {
        let leaf = Path::new(filename)
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("file.bin"));
        self.character_dir(id).join(leaf)
    }

    // ── characters ───────────────────────────────────────────────────────────

    pub fn list_characters(&self) -> Vec<Character> {
        let mut all: Vec<Character> = self
            .characters
            .read()
            .expect("pixal3d characters poisoned")
            .values()
            .cloned()
            .collect();
        all.sort_by_key(|c| std::cmp::Reverse(c.created_unix));
        all
    }

    pub fn character(&self, id: &str) -> Option<Character> {
        self.characters
            .read()
            .expect("pixal3d characters poisoned")
            .get(id)
            .cloned()
    }

    /// The record a queued rig will land in, and the directory its source image is written to
    /// before anything is enqueued.
    pub fn create_character(&self, id: String, name: String) -> Character {
        let mut map = self
            .characters
            .write()
            .expect("pixal3d characters poisoned");
        if let Err(e) = std::fs::create_dir_all(self.character_dir(&id)) {
            tracing::warn!(character = %id, error = %e, "failed to create the character directory");
        }
        let character = Character::new(id.clone(), name);
        self.persist_character(&character);
        map.insert(id, character.clone());
        character
    }

    pub fn update_character(&self, id: &str, f: impl FnOnce(&mut Character)) {
        let mut map = self
            .characters
            .write()
            .expect("pixal3d characters poisoned");
        if let Some(c) = map.get_mut(id) {
            f(c);
            if c.state.is_terminal() && c.finished_unix.is_none() {
                c.finished_unix = Some(now_unix());
            }
            self.persist_character(c);
        }
    }

    /// Remove a character, its sidecar and every file it owns.
    ///
    /// ⚠️ This deletes koblem's copy only. The engine's `/out/jobs/<id>/` — the stage dir and rig
    /// cache a re-roll would have resumed from — is not koblem's to delete and is left alone.
    pub fn delete_character(&self, id: &str) -> bool {
        let mut map = self
            .characters
            .write()
            .expect("pixal3d characters poisoned");
        if map.remove(id).is_none() {
            return false;
        }
        let _ = std::fs::remove_file(self.dir.join("characters").join(format!("{id}.json")));
        let _ = std::fs::remove_dir_all(self.character_dir(id));
        true
    }

    // ── motions ──────────────────────────────────────────────────────────────

    pub fn list_motions(&self) -> Vec<Motion> {
        let mut all: Vec<Motion> = self
            .motions
            .read()
            .expect("pixal3d motions poisoned")
            .values()
            .cloned()
            .collect();
        all.sort_by_key(|c| std::cmp::Reverse(c.created_unix));
        all
    }

    pub fn motion(&self, id: &str) -> Option<Motion> {
        self.motions
            .read()
            .expect("pixal3d motions poisoned")
            .get(id)
            .cloned()
    }

    pub fn create_motion(&self, id: String, prompt: String) -> Motion {
        let mut map = self.motions.write().expect("pixal3d motions poisoned");
        let motion = Motion::new(id.clone(), prompt);
        self.persist_motion(&motion);
        map.insert(id, motion.clone());
        motion
    }

    pub fn update_motion(&self, id: &str, f: impl FnOnce(&mut Motion)) {
        let mut map = self.motions.write().expect("pixal3d motions poisoned");
        if let Some(m) = map.get_mut(id) {
            f(m);
            self.persist_motion(m);
        }
    }

    /// Retire the catalogue record behind a queue row that was cancelled before it ever ran.
    /// Returns whether it moved.
    ///
    /// 🔴 A rig or motion cancelled while it was still QUEUED never enters the adapter's run path,
    /// which is where every terminal state a record reaches is written — so left alone the record
    /// sits at `queued` on the 3D page with nothing to move it and no control to clear it. Unlike
    /// the video store there is not even a reader-side rescue: the pixal3d routes read these
    /// records directly and have never folded the queue row over them.
    ///
    /// A record that already reached a terminal state of its own is untouched, and keeps its own
    /// account of why: an interrupted rig arrives here too, and "cancelled before it started" would
    /// be a worse sentence than the one the run path wrote.
    pub fn cancel_character(&self, id: &str, error: &str) -> bool {
        let mut moved = false;
        self.update_character(id, |c| {
            if c.state.is_terminal() {
                return;
            }
            c.state = JobState::Cancelled;
            if c.error.is_none() {
                c.error = Some(error.to_string());
            }
            moved = true;
        });
        moved
    }

    /// [`cancel_character`](Self::cancel_character) for a motion, and for the same reason.
    pub fn cancel_motion(&self, id: &str, error: &str) -> bool {
        let mut moved = false;
        self.update_motion(id, |m| {
            if m.state.is_terminal() {
                return;
            }
            m.state = JobState::Cancelled;
            if m.error.is_none() {
                m.error = Some(error.to_string());
            }
            moved = true;
        });
        moved
    }

    pub fn delete_motion(&self, id: &str) -> bool {
        let mut map = self.motions.write().expect("pixal3d motions poisoned");
        if map.remove(id).is_none() {
            return false;
        }
        let _ = std::fs::remove_file(self.dir.join("motions").join(format!("{id}.json")));
        true
    }

    // ── persistence ──────────────────────────────────────────────────────────

    fn persist_character(&self, c: &Character) {
        persist(&self.dir.join("characters"), &c.id, c);
    }

    fn persist_motion(&self, m: &Motion) {
        persist(&self.dir.join("motions"), &m.id, m);
    }
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn load_dir<T: for<'de> Deserialize<'de>>(
    dir: &Path,
    key: impl Fn(&T) -> String,
) -> HashMap<String, T> {
    let mut map = HashMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return map;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<T>(&b).ok())
        {
            Some(v) => {
                map.insert(key(&v), v);
            }
            None => tracing::warn!(path = %p.display(), "skipping an unreadable pixal3d sidecar"),
        }
    }
    map
}

/// Temp + rename, so a crash mid-write leaves the previous record rather than a truncated one.
fn persist<T: Serialize>(dir: &Path, id: &str, value: &T) {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };
    let tmp = dir.join(format!("{id}.json.tmp"));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(format!("{id}.json")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (Pixal3dStore, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("koblem-pixal3d-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Pixal3dStore::open(dir.clone()), dir)
    }

    fn tier(name: &str, glb: &str) -> CharacterTier {
        CharacterTier {
            tier: name.into(),
            decimate: 220_000,
            joints: 52,
            named_core: 21,
            seconds: 408.9,
            skin_cleanup: "rung 3".into(),
            gate: EngineGate::default(),
            filename: format!("{name}.glb"),
            bytes: 1234,
            glb_path: glb.into(),
        }
    }

    /// 🔴 Concurrent creates all land. The store stopped being a scheduler when the global queue
    /// took admission over, so a second rig is a second record, not a refusal.
    #[test]
    fn creating_a_second_rig_is_never_refused() {
        let (store, dir) = temp_store("slot");
        store.create_character("c1".into(), "gilly".into());
        store.create_character("c2".into(), "second".into());
        assert!(store.character("c1").is_some());
        assert!(store.character("c2").is_some());
        store.create_motion("m1".into(), "walk".into());
        assert!(store.motion("m1").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The catalogue has to survive a restart with the handles a re-roll and an animate need — and
    /// those are PATHS, because the engine evicts job records at 512 while the files stay.
    #[test]
    fn a_restart_recovers_the_paths_a_rerun_needs() {
        let (store, dir) = temp_store("reload");
        store.create_character("c1".into(), "gilly".into());
        store.update_character("c1", |c| {
            c.state = JobState::Done;
            c.engine_job_id = Some("6a7ed4dc0005".into());
            c.stage_dir = Some("/out/jobs/6a7ed4dc0005/stage".into());
            c.rig_cache = Some("/out/jobs/6a7ed4dc0005/rig_cache".into());
            c.tiers = vec![tier("hero", "/out/jobs/6a7ed4dc0005/hero.glb")];
        });
        store.create_motion("m1".into(), "a person walks forward".into());
        store.update_motion("m1", |m| {
            m.state = JobState::Done;
            m.clip_path = Some("/out/jobs/abc/clip.json".into());
            m.frames = 120;
        });

        let reopened = Pixal3dStore::open(dir.clone());
        let c = reopened.character("c1").expect("character reloads");
        assert_eq!(c.stage_dir.as_deref(), Some("/out/jobs/6a7ed4dc0005/stage"));
        assert_eq!(
            c.rig_reference(&RigSource::Tier {
                tier: "hero".into()
            })
            .as_deref(),
            Some("/out/jobs/6a7ed4dc0005/hero.glb"),
        );
        let m = reopened.motion("m1").expect("motion reloads");
        assert_eq!(m.clip_path.as_deref(), Some("/out/jobs/abc/clip.json"));
        assert_eq!(m.frames, 120);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An edited rig is animated by the path it was STAGED at, not by the copy in the character
    /// directory — the engine container can only see the shared mount.
    #[test]
    fn an_edit_animates_by_its_staged_path_and_not_at_all_without_one() {
        let (store, dir) = temp_store("edit");
        store.create_character("c1".into(), "gilly".into());
        store.update_character("c1", |c| {
            c.edits.push(RigEdit {
                id: "e1".into(),
                label: "spears removed".into(),
                base_tier: "hero".into(),
                filename: "edit-e1.glb".into(),
                bytes: 10,
                staged_path: Some(PathBuf::from("/var/lib/koblem/pixal3d-staging/c1-e1.glb")),
                changes: Value::Null,
                created_unix: 0,
            });
            c.edits.push(RigEdit {
                id: "e2".into(),
                label: "unstaged".into(),
                base_tier: "hero".into(),
                filename: "edit-e2.glb".into(),
                bytes: 10,
                staged_path: None,
                changes: Value::Null,
                created_unix: 0,
            });
        });
        let c = store.character("c1").expect("present");
        assert_eq!(
            c.rig_reference(&RigSource::Edit {
                edit_id: "e1".into()
            })
            .as_deref(),
            Some("/var/lib/koblem/pixal3d-staging/c1-e1.glb"),
        );
        // No staged copy ⇒ no rig reference at all, rather than a path the container cannot open.
        assert!(
            c.rig_reference(&RigSource::Edit {
                edit_id: "e2".into()
            })
            .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 A filename off a sidecar cannot escape the character's directory.
    #[test]
    fn a_traversal_filename_stays_inside_the_character_dir() {
        let (store, dir) = temp_store("traversal");
        let p = store.file_path("c1", "../../etc/passwd");
        assert_eq!(p, store.character_dir("c1").join("passwd"));
        assert!(p.starts_with(store.character_dir("c1")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deleting a character takes its files with it — these are the largest things koblem stores
    /// on this path, and a sidecar-only delete would leak a whole LOD ladder per character.
    #[test]
    fn deleting_a_character_removes_its_files_too() {
        let (store, dir) = temp_store("delete");
        store.create_character("c1".into(), "gilly".into());
        let glb = store.file_path("c1", "hero.glb");
        std::fs::write(&glb, b"GLB\0").expect("write");
        assert!(glb.exists());
        assert!(store.delete_character("c1"));
        assert!(!glb.exists());
        assert!(!store.character_dir("c1").exists());
        assert!(store.character("c1").is_none());
        assert!(!store.delete_character("c1"), "a second delete is a no-op");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 The queue can cancel a rig that never started, and nothing else will ever close the
    /// record out — the run path is where every terminal state is written and it is never entered.
    /// A record left at `queued` is a character stuck "rigging" on the 3D page forever.
    #[test]
    fn a_rig_or_motion_cancelled_before_it_started_is_closed_out() {
        let (store, dir) = temp_store("abandon");
        store.create_character("c1".into(), "gilly".into());
        assert!(store.cancel_character("c1", "cancelled before the rig started"));
        let c = store.character("c1").expect("record");
        assert_eq!(c.state, JobState::Cancelled);
        assert_eq!(c.error.as_deref(), Some("cancelled before the rig started"));
        assert!(c.finished_unix.is_some(), "a terminal record is stamped");

        // Already told, by something that knows more than we do.
        store.create_character("c2".into(), "second".into());
        store.update_character("c2", |c| {
            c.state = JobState::Failed;
            c.error = Some("the engine rejected the mesh".into());
        });
        assert!(!store.cancel_character("c2", "cancelled before the rig started"));
        let c = store.character("c2").expect("record");
        assert_eq!(c.state, JobState::Failed);
        assert_eq!(c.error.as_deref(), Some("the engine rejected the mesh"));

        // A record nobody minted is not an error, just nothing to do.
        assert!(!store.cancel_character("gone", "cancelled"));

        // The motion half is a second entry point to the same defect, not a variation on it: a
        // queued motion is cancelled by the same click and stranded the same way.
        store.create_motion("m1".into(), "a person walks forward".into());
        assert!(store.cancel_motion("m1", "cancelled before the motion started"));
        let m = store.motion("m1").expect("record");
        assert_eq!(m.state, JobState::Cancelled);
        assert_eq!(
            m.error.as_deref(),
            Some("cancelled before the motion started")
        );
        assert!(!store.cancel_motion("m1", "cancelled again"));

        let _ = std::fs::remove_dir_all(dir);
    }
}
