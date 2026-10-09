//! MiniMax-H3 prompt writing — **the model's own guides are the authority. This module states only
//! the facts they cannot know.**
//!
//! # What this module stopped doing, and why 142 KB of Rust went away
//!
//! It used to reconstruct MiniMax's two prompt-writing guides in Rust clause-builders — the section
//! list, the per-section length envelopes, the retention-marker vocabularies, the five cut verbs,
//! the twelve camera motions, the dialogue grammar, the keyframe strategies, the task-type table —
//! and then ask the writing LLM to fill in **our editor's JSON shape** rather than write a prompt.
//! That was 141,892 bytes of paraphrase standing in front of 39,326 bytes of markdown that had been
//! sitting on disk the whole time. Handing the guides to the writer verbatim already produces a
//! clean, correct block (owner-verified 2026-08-08).
//!
//! So the guides are **vendored** into `api/assets/minimax-h3/` and `include_str!`d, and every H3
//! system prompt is assembled in one order:
//!
//! 1. **The guide, verbatim**, introduced as authoritative. Ref2VA gets `…_ref_en.md`; the FL2VA
//!    family gets `…_base_en.md`. [`H3Checkpoint`] chooses, and it is derived from the conditioning
//!    that is actually present — never from a caller's opinion.
//! 2. **A preamble of FACTS, not rules** — the things the guide cannot know: this generation's frame
//!    count and snapped duration, its canvas, the reference roster with the ordinals the conditioner
//!    will emit and the author's own names for them, the cast, how many cuts were authored and which
//!    one the author is editing, the serving limit, today's date.
//! 3. **How to read the author's text** — it is INTENT, in whatever shape it arrived.
//! 4. **The output contract: the finished BLOCK**, not JSON.
//!
//! 🔴 **Nothing here may restate a rule the guide states.** A rule written in two places gets
//! resolved by following the wrong half, and the half that loses is always the one nobody re-read.
//! If you find yourself about to write "the five legal cut verbs are…", stop: `GUIDE-base:92` is in
//! the prompt already, three thousand words above your sentence, and it disagrees with you the
//! moment MiniMax edits it. The test for whether a clause belongs here is simple — **could the guide
//! possibly know this?** The duration of *this* render, the label the conditioner assigned to *this*
//! asset, the number of shots *this* pack holds: no. Everything else: yes, and it is already said.
//!
//! ⚠️ 🔴 **Do not read the guides from a staging directory on disk.** That is a path on one
//! machine and it does not exist on a build box; `include_str!` is what makes the guides part of the
//! binary. Re-vendor from `MiniMax-H3/docs/` when MiniMax ships an edit, and re-vendor *whole* — a
//! partial copy is a paraphrase with extra steps.
//!
//! # What is still here, and why each thing survived
//!
//! | kept | why the guide cannot supply it |
//! | --- | --- |
//! | [`assign_ordinals`] | arithmetic over WanGP's pipeline add-order (`pipeline.py:311-340`) |
//! | [`reference_roster_clause`] | which assets exist, what they are called, what job they do |
//! | [`H3Inputs`] / [`H3Plan`] | which CHECKPOINT serves the request, i.e. which guide to send |
//! | [`render_facts`] | the snapped frame count, duration and canvas of this render |
//! | [`cut_plan_facts`] | how many cuts the author authored, and which one they are editing |
//! | [`cast_roster_clause`] | the project's recurring characters and their agreed features |
//! | [`generation_boundary_facts`] | koblem's generation/cut split — a product concept, not MiniMax's |
//! | [`shot_and_beat_facts`] | a MEASUREMENT of undocumented behaviour — see its own note |
//! | [`current_date_clause`] | today |
//! | [`block_contract`] | that we want text back, under a serving limit, with a gist beside it |
//! | [`clean_block`] / [`parse_generations`] | reading the reply |
//!
//! # Terminology, because two vocabularies collide here
//!
//! koblem's editor calls one clip a "shot". H3's `[Shot N]` is a **cut inside one generation**, and
//! the generation is what renders, delivers and retakes. This module speaks H3's vocabulary in
//! anything the model reads and koblem's in its own signatures — so `shot_count` is the number of
//! `[Shot N]` blocks inside one generation, which is also the number of editor clips packed into it.

use koblem_client::types::{LtxCastMember, StoryContext};
use serde::Deserialize;
use serde_json::Value;

// ─── The guides, vendored ─────────────────────────────────────────────────────

/// `MiniMax-H3/docs/VIDEO_PROMPT_WRITING_GUIDE_base_en.md` — 15,773 B, byte-for-byte.
///
/// The FL2VA family (t2va / i2va / fl2va / l2va): three sections keyed on
/// `integrated_multimodal_description`, the alignment instruction line, and the shot / camera /
/// speaker / dialogue grammar that the Ref2VA guide then incorporates by reference.
const GUIDE_BASE: &str =
    include_str!("../../assets/minimax-h3/VIDEO_PROMPT_WRITING_GUIDE_base_en.md");

/// `MiniMax-H3/docs/VIDEO_PROMPT_WRITING_GUIDE_ref_en.md` — 23,553 B, byte-for-byte.
///
/// Ref2VA: six sections keyed on `detailed_description`, the four label kinds, the task-type table,
/// and the two retention-marker vocabularies.
const GUIDE_REF: &str =
    include_str!("../../assets/minimax-h3/VIDEO_PROMPT_WRITING_GUIDE_ref_en.md");

/// The guide this checkpoint is served by, and the filename it is served under.
///
/// 🔴 The checkpoint is derived, never chosen — see [`H3Inputs::resolve`]. Sending the wrong guide
/// does not merely mislead: it names the wrong main body field, and the main field name is a literal
/// token sequence the model was trained to see at that position (`GUIDE-ref:229`). A Ref2VA request
/// carrying `integrated_multimodal_description:` has an unrecognised header where the trained one
/// belongs, and the four label sections that carry every reference semantics go with it.
pub fn guide(checkpoint: H3Checkpoint) -> (&'static str, &'static str) {
    match checkpoint {
        H3Checkpoint::Ref2va => ("VIDEO_PROMPT_WRITING_GUIDE_ref_en.md", GUIDE_REF),
        H3Checkpoint::Fl2vaFamily => ("VIDEO_PROMPT_WRITING_GUIDE_base_en.md", GUIDE_BASE),
    }
}

/// The guide, fenced and introduced as gospel. **The first thing in every H3 writer prompt.**
///
/// The introduction is load-bearing and is quoted from the plan: everything after it is either a
/// fact or a request, and where anything after it appears to disagree with the guide, the guide
/// wins. The fences exist so the model can tell the borrowed document from our own sentences — a
/// markdown heading dropped raw into a system prompt reads as *our* heading.
pub fn guide_block(checkpoint: H3Checkpoint) -> String {
    let (name, text) = guide(checkpoint);
    format!(
        "This is the model's own prompt-writing guide. It is authoritative. Where anything below \
         disagrees with it, the guide wins.\n\n===== BEGIN {name} =====\n{text}\n===== END {name} \
         =====\n\nEverything after this point is either a FACT about this particular render that \
         the guide could not know, or a statement of what to reply with. None of it overrides the \
         guide.\n"
    )
}

// ─── Facts the guide cannot know ──────────────────────────────────────────────

/// The four label words, exactly as `text_encoder.py:137,140,142` emits them.
///
/// 🔴 The angle brackets belong to the PROMPT. The conditioner emits `"<Picture 1>: "` ahead of each
/// reference's vision block and then appends the prompt verbatim (`text_encoder.py:153`), so the
/// label in the body is the *same string* that introduced the asset. `Picture 1` unbracketed does
/// not match it, and the mismatch is silent — the render simply ignores the reference.
pub const PICTURE_WORD: &str = "Picture";
pub const VIDEO_WORD: &str = "Video";
pub const AUDIO_WORD: &str = "Audio";

/// MiniMax's serving limit on the prompt string, from the vLLM recipe page ("Prompts cap at 7,000
/// characters").
///
/// 🔴 A **reject**, not a truncate. Truncation would silently drop `non_diegetic_music`, which is
/// the last section — so the failure mode of getting this wrong is a render with no score and no
/// error. The guide says nothing about it because it is a serving-stack limit, not a craft rule.
///
/// UNVERIFIED: not stated anywhere in the on-disk MiniMax repo; `grep -rn "7000"` over the weights
/// tree returns nothing. Authoritative-but-external. A 400 carrying a length complaint settles it.
pub const PROMPT_CHAR_CAP: usize = 7000;

/// One generation's DOCUMENTED output duration: `README:48`, *"Output duration | 4-15 seconds"*.
///
/// 🔴 Guidance handed to a planner, never a clamp — nothing here enforces it and nothing should. The
/// real mechanical floor is 22 frames (0.917 s), the video VAE's decodable minimum
/// (`koblem_client::types::H3_MIN_FRAMES`); 4 s is simply where the range MiniMax validated starts,
/// which is the right thing to tell someone planning a shot list.
pub const MIN_GENERATION_SECONDS: f64 = 4.0;
pub const MAX_GENERATION_SECONDS: f64 = 15.0;

// ─── References ───────────────────────────────────────────────────────────────

// The reference vocabulary — WHAT an asset is and WHAT JOB it does — is
// [`koblem_client::types::H3ReferenceKind`] / [`H3ReferenceRole`], and this module owns no copy of
// it.
//
// 🔴 It used to. Two enums with the same eleven variants, kept in step by a comment on each side.
// That drift is silent and asymmetric: `build_chain` decides what conditioning is SENT (via
// `H3ReferenceRole::engine_role`), this module decides what the roster CLAIMS was sent, and a role
// meaning "continuation source" on one side and "reference" on the other yields a prompt whose task
// type contradicts the request — with a render that merely looks mediocre.
pub use koblem_client::types::{H3ReferenceKind, H3ReferenceRole};

/// The `retention_analysis` parenthetical a role implies, in the corpus's own phrasing
/// (`GUIDE-ref:173, 179, 185`).
///
/// ⚠️ **Kept even though the guide states the LINE FORMAT**, because what it maps is *our* role
/// vocabulary, which the guide has never heard of. The guide says a picture entry reads
/// `<Picture 2> ([Shot 1] first frame): marker - …`; only koblem knows that *this* asset was
/// declared a first frame rather than a storyboard. The mapping is the fact; the line shape around
/// it is the guide's and is not repeated.
///
/// A free function rather than a method on [`H3ReferenceRole`]: koblem-client is a WIRE crate that
/// kobbler and kobload also depend on, and English belongs on this side of that line.
pub fn retention_scope(role: H3ReferenceRole) -> &'static str {
    match role {
        H3ReferenceRole::FirstFrame => "[Shot 1] first frame",
        H3ReferenceRole::LastFrame => "[Shot N] last frame",
        // 🔴 These two used to claim "[Shot 1]", and koblem has no idea whether that is true —
        // where a subject appears is the WRITER's decision, not a property of the reference. The
        // claim was copied by the writer onto subjects that enter later (observed 2026-08-09: a cat
        // declared to appear in [Shot 1] while the shot list had it entering at 00:06, and the
        // render produced two cats). First/last frame keep their tags because those ARE structural:
        // a first frame is t=0 and a last frame is the end, whatever the shot list does in between.
        H3ReferenceRole::IdentitySource => "appears in the video",
        H3ReferenceRole::FrameAnchor => "a concrete frame inside the video",
        H3ReferenceRole::Storyboard => "viewpoint, subject placement, and shot order",
        H3ReferenceRole::EditSource => "source video editing",
        H3ReferenceRole::ContinuationSource => "continuation starting point",
        H3ReferenceRole::StructureSource => "cut and pacing structure",
        // Audio lines carry NO parenthetical at all (`GUIDE-ref:200-204`) — these exist only so a UI
        // can label the row.
        H3ReferenceRole::VoiceTimbre => "voice timbre",
        H3ReferenceRole::AudioReuse => "reused audio",
        H3ReferenceRole::AudioContinuity => "audio continuity",
    }
}

/// One reference as the WRITER needs to see it. Deliberately carries no pixels — the writer is being
/// told which ordinal to address and what job it does, not being asked to look at it. (The VL model
/// still sees the project's story-canon images through the ordinary image path.)
#[derive(Debug, Clone, Deserialize)]
pub struct H3RefBrief {
    pub kind: H3ReferenceKind,
    /// 🔴 **"Its soundtrack is being passed as a reference"**, not "the file has sound".
    /// `GUIDE-ref:112`: *"An ordinary reference video does not create `<Audio N>` merely because the
    /// file contains sound."* In WanGP this is the `"K" in audio_prompt_type` gate
    /// (`pipeline.py:329`) — the emitter cannot infer it and must be told.
    #[serde(default)]
    pub has_audio: bool,
    /// What this reference is FOR. Absent ⇒ [`H3ReferenceRole::default_for`] the kind.
    ///
    /// 🔴 The LENIENT deserialiser, and not the derived one. A saved timeline may carry the engine's
    /// legacy catch-all `"reference"` (or a value a newer editor invented); the derived impl rejects
    /// both and takes the whole enclosing request down with it, which on this path means a Director
    /// project that will not open. Falling back to the kind's default costs one reference a slightly
    /// weaker claim in the roster.
    #[serde(default, deserialize_with = "koblem_client::types::h3_role_lenient")]
    pub role: Option<H3ReferenceRole>,
    /// Editor-only label — the author's own name for this asset ("Mia").
    ///
    /// Never sent to the engine: the model addresses a reference by ORDINAL. It is in the roster
    /// anyway because the author's brief says "Mia", and without the bridge the writer either drops
    /// her or invents a label for her.
    #[serde(default)]
    pub label: Option<String>,
    /// **Can the characters in the scene HEAR this audio?** `AudioReuse` only.
    ///
    /// 🔴 Without this the writer files a reused song under `non_diegetic_music` EVERY TIME, and it
    /// is right to: `GUIDE-ref:296` says "audience-only score belongs in `non_diegetic_music`" and
    /// its worked example is literally `<Audio 2> is directly reused as the complete audience-only
    /// score.` Nothing in the roster contradicted it, so the model copied the guide.
    ///
    /// That is fatal on a music video. `non_diegetic_music` is *defined* as music the characters
    /// **cannot** hear, so declaring the song there tells the model the performer cannot hear the
    /// track it is being asked to sing — a self-contradiction with every sync line in the same
    /// block, in the section that defines the audible layer. Observed 2026-08-10: the singing
    /// stopped locking to the vocal at all.
    ///
    /// `None` keeps the old behaviour (the guide decides), so an older client is unchanged.
    #[serde(default)]
    pub diegetic: Option<bool>,
    /// What the project does with this soundtrack at DELIVERY. `AudioReuse` only.
    ///
    /// The timeline has carried `h3_soundtrack_mode` since H3 took a soundtrack at all, and it had
    /// never once reached the writer — `grep soundtrack_mode` over `api/src` matched a single test
    /// fixture. So the writer could not know that under `Replace` the song IS the finished audio,
    /// and spent `overall_soundscape` on footsteps and room tone that the mux discards.
    #[serde(default)]
    pub soundtrack_mode: Option<H3SoundtrackMode>,
}

/// What the project does with the supplied soundtrack when the render is delivered.
///
/// Mirrors the editor's `h3SoundtrackMode` and the timeline's `h3_soundtrack_mode`. Only `Replace`
/// changes what the writer should say, but all three are carried so an unknown value is a legible
/// deserialise failure rather than a silent `Replace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum H3SoundtrackMode {
    /// The supplied song is muxed over the model's own audio: it is the entire deliverable track.
    Replace,
    /// The model's generated audio is kept as delivered.
    Keep,
    /// Both are mixed together.
    Mix,
}

impl H3RefBrief {
    pub fn role(&self) -> H3ReferenceRole {
        self.role
            .unwrap_or_else(|| H3ReferenceRole::default_for(self.kind))
    }
}

/// One packed generation as a READER of the finished script needs to see it: the assembled block
/// that will actually be sent, plus the arithmetic wrapped around it.
///
/// 🔴 **This exists because a listing of clip bodies cannot answer a schema question.** The reviewer
/// is asked to find a cut time that is not strictly increasing, a reference label used but never
/// defined, a retention line that is missing — and every one of those lives inside the assembled
/// block, which is the stored document (`DirectorH3Generation.prompt`). Handed one line per clip
/// instead, the model has no sections, no `[Shot N] At MM:SS.mmm` tags, no labels and no generation
/// boundaries to look at, so the only way it can answer is to invent. Send the document.
///
/// ⚠️ Deliberately NOT a full `DirectorH3Generation`: the per-section fields are a lens on the block
/// and re-sending them would give the reader two copies that can disagree about what the prompt says.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct H3GenerationBrief {
    /// The generation's own stable editor id (`DirectorH3Generation.id`).
    ///
    /// 🔴 **This is the id space the CHAT surface addresses, and the reason it exists.** A chat
    /// action's target on H3 is the RENDER — one generation, one prompt, one delivery — because the
    /// only field an edit can usefully land in is this generation's block. Without an id here chat
    /// had nothing to name a render by, so it named a cut, and `set_prompt` wrote `DirectorClip
    /// .prompt`: a SEED that is read only when a generation has no block yet (`h3SeedInput`), and
    /// never read again once one exists. The edit reached the render only for a generation nobody
    /// had opened, and evaporated for every other one.
    ///
    /// ⚠️ Absent ⇒ the caller is an older client, and chat falls back to the cut id space with the
    /// cut-shaped action contract, byte-identical to before. It is all-or-nothing per request (see
    /// `ltx_director::chat_generation_ids`): a listing where some renders can be named and others
    /// cannot is worse than one where none can, because the model cannot see which is which.
    ///
    /// ⚠️ `/review` ignores it — a finding is a one-click rework of one CLIP, so that path keeps
    /// naming cuts. Two surfaces, two id spaces, one brief.
    #[serde(default)]
    pub id: Option<String>,
    /// The author's one-line label for this render (`DirectorH3Generation.gist`).
    ///
    /// ⚠️ **Display only — it is NEVER compiled into the request**, on either side of the wire. It is
    /// here so a chat turn can (a) recognise a render in a list of eight without reading 7,000
    /// characters of schema, and (b) rewrite it, which is what `set_gist` now targets on H3. Do not
    /// let it leak into a block: the schema's own `summary:` section is a different thing that DOES
    /// render, and lives inside `prompt`.
    #[serde(default)]
    pub gist: Option<String>,
    /// The assembled block, verbatim. Absent or blank ⇒ nobody has written this generation yet —
    /// which the caller must SAY rather than print an empty block, or a never-opened generation
    /// reads as an author who wrote nothing (see [`Self::has_block`]).
    #[serde(default)]
    pub prompt: Option<String>,
    /// What this generation RENDERS for, after the frame-grid snap — the number every cut time in
    /// the block has to fall inside. The authored sum is not it: a cut at 14.9 s is legal in a
    /// generation that snapped up to 15.083 s and illegal in one that did not.
    #[serde(default)]
    pub seconds: Option<f64>,
    /// The wire `task` koblem derived for it: `"ref2va"`, `"fl2va"` or `"t2va"`.
    #[serde(default)]
    pub task: Option<String>,
    /// Did a human (or the writer) actually produce this block, or is it the one koblem composes
    /// from the cut bodies for a generation nobody has opened?
    ///
    /// ⚠️ Both RENDER — `compileH3Timeline` falls back to `seedH3Prompt` exactly as this does — so
    /// the distinction is not "real vs placeholder", it is how hard to press. A seeded block is
    /// unedited machine assembly and a pile of findings against it is noise; an authored one is
    /// somebody's work and worth the full pass.
    ///
    /// Absent ⇒ treated as authored, so a client that only sends `prompt` reads as it always did.
    #[serde(default)]
    pub authored: Option<bool>,
    /// The clip ids packed into this generation, in cut order.
    ///
    /// 🔴 **Clip ids, not a generation id.** A finding is turned into a one-click rework of ONE clip
    /// (`review.ts::ReviewItem.clipId`), so the reader must still be able to name a cut; a finding
    /// attributed to a generation would arrive with nothing to fix.
    #[serde(default)]
    pub shots: Vec<String>,
}

impl H3GenerationBrief {
    /// The assembled block, trimmed — `None` when this generation has never been written.
    pub fn block(&self) -> Option<&str> {
        self.prompt
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
    }

    /// The generation id, trimmed — `None` when this render cannot be named. Blank is the same
    /// state as absent: a client that sends `"id": ""` has no id to offer either, and treating the
    /// empty string as a name would mint a token every render could answer to.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// The author's label, trimmed — `None` when nobody has written one.
    pub fn gist(&self) -> Option<&str> {
        self.gist
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// The body-section header this generation's checkpoint requires, when we know the checkpoint.
    ///
    /// The two checkpoints are disjoint weight sets with disjoint schemas, so this is a hard fact
    /// about the request koblem will send, not a preference — a block written to the other schema
    /// renders, and renders the wrong shape of thing. Printing it in the listing turns "judge each
    /// generation against its own schema" from a guess into a comparison.
    pub fn expected_field(&self) -> Option<&'static str> {
        match self.task.as_deref() {
            Some("ref2va") => Some(H3Checkpoint::Ref2va.main_field()),
            Some("fl2va" | "t2va") => Some(H3Checkpoint::Fl2vaFamily.main_field()),
            _ => None,
        }
    }
}

/// One reference's resolved address: `("<Picture 2>", …)`. Brackets included — see [`PICTURE_WORD`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H3RefAddress {
    /// e.g. `<Picture 2>`, written into the prompt exactly like this.
    pub name: String,
    /// The extra `<Audio j>` a video reference whose soundtrack is enabled also answers to.
    pub audio_name: Option<String>,
    pub kind: H3ReferenceKind,
    pub role: H3ReferenceRole,
    pub label: Option<String>,
    /// Carried through from the brief so the roster can state it. See [`H3RefBrief::diegetic`].
    pub diegetic: Option<bool>,
    /// Carried through from the brief. See [`H3RefBrief::soundtrack_mode`].
    pub soundtrack_mode: Option<H3SoundtrackMode>,
}

/// Assign each reference its ordinal, **exactly as the conditioner will**.
///
/// The prompt's labels only resolve if they match the labels the conditioner emitted, so this models
/// WanGP's pipeline add-order (`pipeline.py:311-340`) rather than the order the editor happens to
/// list things in:
///
/// 1. every reference **image**, in the order given → `<Picture n>` (`pipeline.py:311-312`)
/// 2. every reference **video**, in the order given; a video whose soundtrack is enabled claims its
///    `<Audio k>` **immediately before** its own `<Video k>` (`pipeline.py:250` pushes the audio,
///    `:253` pushes the video) — corroborated by the served prompt, where the video's track is
///    `<Audio 1>` and the standalone clip is `<Audio 2>` (`REQ-ref2va:13`)
/// 3. standalone **audio** refs → `<Audio j>` (`pipeline.py:334-337`)
///
/// Ordinals are **per kind**, each counting from 1 in its own sequence (`GUIDE-ref:110`), so the
/// third entry in the list can legitimately be `<Picture 1>`.
///
/// The returned vector is in the CALLER'S order, not add-order, so it can be zipped with `refs`.
///
/// 🔴 **This is arithmetic the guide does not specify and cannot** — it is a property of the runner
/// we serve, not of the model. It is also the single most silent thing in the module: a wrong
/// ordinal addresses a different asset and renders something entirely plausible.
///
/// 🔴 **Scope is the GENERATION, not the shot.** WanGP builds one reference presentation per
/// `generate` call and a generation holds several `[Shot N]` blocks, so a per-shot rescoping is
/// wrong on this engine.
///
/// 🔴 If a continuation tail is added through the sliding-prefix path it becomes the **last**
/// `<Video N>` (`pipeline.py:338-340`), after every user reference video. Never hard-code
/// `<Video 1>` for a tail; pass it in `refs` in its real position instead.
pub fn assign_ordinals(refs: &[H3RefBrief]) -> Vec<H3RefAddress> {
    let mut out: Vec<Option<H3RefAddress>> = vec![None; refs.len()];
    let (mut image, mut video, mut audio) = (0u32, 0u32, 0u32);

    let addr = |name: String, audio_name: Option<String>, r: &H3RefBrief| H3RefAddress {
        name,
        audio_name,
        kind: r.kind,
        role: r.role(),
        label: r
            .label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        diegetic: r.diegetic,
        soundtrack_mode: r.soundtrack_mode,
    };

    for (i, r) in refs.iter().enumerate() {
        if r.kind == H3ReferenceKind::Image {
            image += 1;
            out[i] = Some(addr(format!("<{PICTURE_WORD} {image}>"), None, r));
        }
    }
    for (i, r) in refs.iter().enumerate() {
        if r.kind == H3ReferenceKind::Video {
            let audio_name = r.has_audio.then(|| {
                audio += 1;
                format!("<{AUDIO_WORD} {audio}>")
            });
            video += 1;
            out[i] = Some(addr(format!("<{VIDEO_WORD} {video}>"), audio_name, r));
        }
    }
    for (i, r) in refs.iter().enumerate() {
        if r.kind == H3ReferenceKind::Audio {
            audio += 1;
            out[i] = Some(addr(format!("<{AUDIO_WORD} {audio}>"), None, r));
        }
    }
    out.into_iter().flatten().collect()
}

/// The reference roster, handed to the writer as **facts**: which assets are in scope, the exact
/// label the conditioner introduced each one under, the author's own name for it, and what job it
/// was declared to do.
///
/// This is the one clause the guide provably cannot write. The guide explains what `<Picture N>` and
/// `<Audio N>` MEAN; only koblem knows that this request carries a storyboard image at `<Picture 1>`
/// that the author calls "the diner sketch" and a song slice at `<Audio 1>`.
///
/// Empty when there are no references — which is also what the engine sees, so prompt and
/// conditioning agree rather than the prompt describing conditioning that is not there.
///
/// The FL2VA family gets the labels but no section machinery: it has no `subject_definitions` and no
/// `retention_analysis` to file them under, and its pinned image is addressed straight from the body
/// (`REQ-fl2va:13` writes `<Picture 1>` in prose).
pub fn reference_roster_clause(refs: &[H3RefBrief], checkpoint: H3Checkpoint) -> String {
    let addresses = assign_ordinals(refs);
    if addresses.is_empty() {
        return String::new();
    }
    let ref2va = checkpoint == H3Checkpoint::Ref2va;
    let mut s = String::from(
        "\n\nREFERENCE ROSTER — facts, not choices. The conditioner introduces each asset to the \
         model under exactly the label below, so write the label exactly as given, angle brackets \
         and all. Never invent a label, never renumber one, never address a reference by the \
         author's name for it, and never write a label for an asset that is not on this list — \
         these are the only references this render receives. In scope for this generation:",
    );
    for a in &addresses {
        s.push_str(&format!("\n  - {} — {}", a.name, role_phrase(a.role)));
        if let Some(audio) = &a.audio_name {
            s.push_str(&format!("; its soundtrack is passed separately as {audio}"));
        }
        // 🔴 The song. An `audio` reference in the reuse role is a SLICE of the project's one
        // soundtrack, cut to this generation's exact length — not a separate piece of music and not
        // a style hint. A writer that does not know this describes a score it should be describing
        // the *continuation* of, and the seam is audible at every generation boundary.
        if a.kind == H3ReferenceKind::Audio && a.role == H3ReferenceRole::AudioReuse {
            s.push_str(
                "; this is the slice of the project's own soundtrack that covers exactly this \
                 generation — the same song continues in the generations either side of it",
            );
            // 🔴 Both of these override the guide, so both say so. The guide's rule
            // (`GUIDE-ref:296`) is right for a film score and wrong for a music video, and the
            // writer has no way to tell which it is holding.
            match a.diegetic {
                Some(true) => s.push_str(
                    "; the characters CAN HEAR this music — it is playing out loud in the scene \
                     around them and they react and move in time to it. It is therefore DIEGETIC, \
                     so `non_diegetic_music:` must be exactly `N/A` and must NOT describe this \
                     audio: that section means music the characters cannot hear, and filing the \
                     song there tells the model the performer cannot hear the track it is singing. \
                     Diegetic music has no section of its own — say that it plays out loud and that \
                     the subject hears it and performs to it in the `<Audio N>` line of \
                     `subject_definitions` (which is outside the 350-500 word budget), and put the \
                     per-shot synchronisation statements in `detailed_description`",
                ),
                Some(false) => s.push_str(
                    "; the characters CANNOT hear this music — it is audience-only, so it belongs \
                     in `non_diegetic_music:` and the characters must not be written as reacting to \
                     it",
                ),
                // 🔴 An older client sends no flag, and the guide's default (`GUIDE-ref:296`) then
                // files the song as audience-only — which is right for a score and fatal for a music
                // video. There is nothing to look it up in, but the writer's OWN PROSE settles it:
                // if it has written a subject performing to this audio, it has written diegetic
                // music. So make the check internal rather than guessing a default.
                None => s.push_str(
                    "; nobody has said whether the characters can hear this music, so decide it \
                     from what you yourself write and then keep the two sections consistent with \
                     that. If any subject in this video hears this audio, sings it, moves in time \
                     with it or reacts to it in any way, then it is DIEGETIC: `non_diegetic_music:` \
                     must be exactly `N/A` and must not describe this audio, because that section \
                     means music the characters cannot hear and filing the song there tells the \
                     model the performer cannot hear the track it is performing. It is only \
                     audience-only — and only then does it belong in `non_diegetic_music:` — if \
                     nothing in the scene reacts to it at all. A block that says a subject sings to \
                     this audio AND files it as an audience-only score contradicts itself, and the \
                     performance is what loses",
                ),
            }
            // 🔴 Measured 2026-08-13 across 18 renders — every prompt variant, both canvases and
            // four seeds. Correlating each clip's motion against the song's 20-180 Hz onset
            // strength and its 3-12 kHz onset strength, the HIGH band won 2-7x in all 18
            // (r 0.21-0.51 vs 0.03-0.31) at a tight +0.04-0.29 s lag, while the kick correlation
            // was near zero with incoherent lag. Two consequences the writer cannot guess: naming a
            // kick the model cannot lock to is wasted, and a loud strum nobody claimed will pull
            // motion onto itself.
            s.push_str(
                "; what the model actually LOCKS TO in this audio is the bright, percussive attack \
                 of a sound — a pick or stick hitting something, a cymbal, a consonant — and NOT \
                 the low boom of a kick drum, which it barely reacts to. So an action written to \
                 land on a deep bass hit that has no bright attack on top of it will drift, and any \
                 loud bright transient you leave UNCLAIMED will pull movement onto itself whether \
                 you wanted it or not. Give the busy stretches between your timed moments something \
                 to be doing — for a character moving to music that is MORE movement carrying \
                 through, not a held pose; a pose is the right answer only when the character is \
                 meant to be still — rather than leaving them unwritten. \
                 🔴 AND AN UNCLAIMED TRANSIENT DOES NOT ONLY STEAL MOVEMENT — IT STEALS THE NEXT \
                 EVENT YOU STAGED, and by seconds. Measured on a 14-second shot whose rift was \
                 written to arrive on a named word at 00:11.508: three hard unclaimed hits sat at \
                 8.0, 8.7 and 8.9 s inside a stretch that said only \"keeps singing\", and the rift \
                 landed on THEM instead — 2.3 s early with the beat isolated, and 8.2 s early in \
                 the version that also crowded the span. Writing one definite action for those \
                 three hits, and changing nothing else, moved the rift to within 0.6 s of its mark \
                 on both seeds. So before any timed moment that matters, look at what the track \
                 does in the seconds LEADING UP to it: if there is a loud bright hit in there with \
                 no instruction on it, give it one, or it will take your event",
            );
            if a.soundtrack_mode == Some(H3SoundtrackMode::Replace) {
                s.push_str(
                    "; this audio REPLACES the model's own generated audio at delivery, so it is \
                     the entire audible content of the finished video. `overall_soundscape:` must \
                     say that, and must not describe an ambience that implies the music is absent — \
                     any physical sound it names is heard BENEATH the music, never instead of it",
                );
            }
        }
        if let Some(label) = &a.label {
            s.push_str(&format!("; the author calls this \"{label}\""));
        }
        // Visible content takes a free-text parenthetical on its retention line; audio takes none at
        // all. The parenthetical wording is OUR role vocabulary in English — see `retention_scope`.
        //
        // 🔴 …but WHICH label wears it depends on the role, and this clause used to say "its
        // retention line" for every one of them. An identity image has no retention line, because it
        // has no entry: `GUIDE-ref:68` says an image used only to define a character, scene, costume
        // or style is cited INSIDE the `<Subject N>` it establishes and gets no standalone
        // `<Picture N>`. So the parenthetical `retention_scope` supplies for that role,
        // `(appears in [Shot 1])`, is the SUBJECT'S — which is exactly the shape of the guide's own
        // subject entry, and was being offered for a picture entry that must not exist. Mapping our
        // roles onto the guide's entry types is ours to state; the entry FORMATS stay the guide's.
        if ref2va && a.kind != H3ReferenceKind::Audio {
            if defines_a_subject(a.role) {
                s.push_str(&format!(
                    "; it gets no entry of its own — cite it inside the `<Subject N>` it \
                     establishes, and it is that `<Subject N>`, not this label, that takes the \
                     `retention_analysis` line, with the parenthetical \"({})\"",
                    retention_scope(a.role)
                ));
            } else {
                s.push_str(&format!(
                    "; it takes an entry of its own, and on that entry's `retention_analysis` line \
                     the parenthetical reads \"({})\"",
                    retention_scope(a.role)
                ));
            }
        }
        // 🔴 A frame anchor is a frame the video PASSES THROUGH, and the only thing that says
        // WHICH moment it passes through is the prose. Saying it "takes an entry of its own" was
        // not enough: measured 2026-08-09 against the live endpoint, the writer returned four
        // correctly numbered beats, zero uses of `<Picture 1>` anywhere in the block, and no
        // retention entry for it at all — so a picture the author uploaded, that koblem staged and
        // that costs DiT rows on every step of the render, went unaddressed. An unaddressed
        // reference conditions far more weakly, and silently.
        if a.role == H3ReferenceRole::FrameAnchor {
            s.push_str(&format!(
                "; 🔴 NAME IT IN THE PROSE. The beat it anchors must open by saying the picture is \
                 where that moment starts — \"[Shot 1] begins from {}, …\" — because nothing else \
                 in the block says which moment the frame belongs to, and a frame the text never \
                 names is a frame the render has no reason to pass through",
                a.name
            ));
        }
    }
    s
}

/// Does a reference in this role get **no `subject_definitions` entry of its own**, being cited
/// inside the `<Subject N>` it defines instead? Ref2VA only.
///
/// The mapping from koblem's role vocabulary onto the guide's four label kinds, which is a thing
/// only koblem can do — the guide states the rule for each kind (`GUIDE-ref:60-89`), and only koblem
/// knows that *this* image was declared an identity source rather than a first frame.
///
/// Everything except `IdentitySource` earns a standalone entry: a first/last frame, a frame anchor
/// and a storyboard are all "the image itself serves as a target frame or planning anchor"
/// (`GUIDE-ref:62,72`); a video is a whole-video relationship (`GUIDE-ref:76`); audio is always its
/// own line (`GUIDE-ref:90`). Reused visible content lifted OUT of a video is additionally a
/// `<Subject N>` (`GUIDE-ref:88`) — that does not remove the `<Video N>` entry, so it is not this
/// function's business.
fn defines_a_subject(role: H3ReferenceRole) -> bool {
    matches!(role, H3ReferenceRole::IdentitySource)
}

/// What a reference was declared to be FOR, in English. Our role vocabulary, not the guide's.
fn role_phrase(role: H3ReferenceRole) -> &'static str {
    match role {
        H3ReferenceRole::FirstFrame => "the exact opening frame of the video, at 0.00 seconds",
        H3ReferenceRole::LastFrame => "the exact final frame of the video",
        H3ReferenceRole::IdentitySource => "defines a character, scene, costume or style",
        H3ReferenceRole::FrameAnchor => "a concrete frame anchor, declared in prose",
        H3ReferenceRole::Storyboard => "a storyboard: viewpoint, subject placement and shot order",
        H3ReferenceRole::EditSource => "the source video being edited",
        H3ReferenceRole::ContinuationSource => "the source video this generation continues from",
        H3ReferenceRole::StructureSource => "supplies camera movement, cuts and rhythm only",
        H3ReferenceRole::VoiceTimbre => "a voice-timbre reference for one speaker",
        H3ReferenceRole::AudioReuse => "audio whose actual signal is reused in the target",
        H3ReferenceRole::AudioContinuity => "referenced for beat, rhythm or audio continuity",
    }
}

// ─── Checkpoint, variant, and the task-type derivation ────────────────────────

/// Which weight set serves the request, and therefore **which guide the writer is handed**.
///
/// The two are disjoint and served on separate ports with disjoint task lists (`REQ-fl2va:8` →
/// `:30010`, `REQ-ref2va:8` → `:30011`), so a single request can never be both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H3Checkpoint {
    /// H3-Base-FL2VA: zero, one or two input images (`README:57`). Three sections.
    Fl2vaFamily,
    /// H3-Base-Ref2VA: mixed image / video / audio references. Six sections.
    Ref2va,
}

impl H3Checkpoint {
    /// The section header the body lives under. Callers that print the field name (the chat surface,
    /// a rework contract) use this rather than spelling it out.
    pub fn main_field(self) -> &'static str {
        match self {
            H3Checkpoint::Fl2vaFamily => "integrated_multimodal_description",
            H3Checkpoint::Ref2va => "detailed_description",
        }
    }
}

/// Which of the base guide's four task shapes this request is — a *prompt-writing* variant, not a
/// wire value.
///
/// 🔴 **There is no `i2va` or `l2va` task at the API.** `README:57` lists two checkpoints, and the
/// script whose filename says i2va sends `"task": "fl2va"` with a single image at `frame_index: 0`
/// (`full-2k-i2va-h3-base.sh:9`). All three ride `fl2va` and differ only in the attached keyframes —
/// which is exactly the fact the writer needs, because it decides which of `GUIDE-base` §2.1's
/// instruction lines the block opens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fl2vaVariant {
    /// No keyframes. No alignment line at all (`GUIDE-base:14`).
    T2va,
    /// First frame only.
    I2va,
    /// First and last frame.
    Fl2va,
    /// Last frame only.
    L2va,
}

impl Fl2vaVariant {
    /// The guide's own name for this shape, so the facts preamble can point at a section of a
    /// document the model is already reading instead of restating it.
    pub fn as_str(self) -> &'static str {
        match self {
            Fl2vaVariant::T2va => "T2VA",
            Fl2vaVariant::I2va => "I2VA",
            Fl2vaVariant::Fl2va => "FL2VA",
            Fl2vaVariant::L2va => "L2VA",
        }
    }
}

/// How a source video is being used. `GUIDE-ref:143` is emphatic that the presence of a video does
/// not by itself create `video editing` or `video continuation` — so koblem states the USE, and the
/// guide's own table turns it into a task type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H3SourceUse {
    Edit,
    Continue,
    /// Camera movement / cuts / rhythm only ⇒ `reference generation`.
    Structure,
}

/// The six task types of `GUIDE-ref:132-139`, which compose into the `summary:` bracket prefix.
/// Ref2VA only — the FL2VA family has no `summary` section to prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H3TaskType {
    VideoEditing,
    VideoContinuation,
    KeyframeCompletion,
    ReferenceGeneration,
    AudioReuse,
    AudioReference,
}

impl H3TaskType {
    pub fn as_str(self) -> &'static str {
        match self {
            H3TaskType::VideoEditing => "video editing",
            H3TaskType::VideoContinuation => "video continuation",
            H3TaskType::KeyframeCompletion => "keyframe completion",
            H3TaskType::ReferenceGeneration => "reference generation",
            H3TaskType::AudioReuse => "audio reuse",
            H3TaskType::AudioReference => "audio reference",
        }
    }
}

/// The six inputs the derivation branches on.
#[derive(Debug, Clone, Copy, Default)]
pub struct H3Inputs {
    /// FF — an image pinned as the first frame, or (on Ref2VA) declared as a frame anchor.
    pub first_frame: bool,
    /// LF — an image pinned as the last frame.
    pub last_frame: bool,
    /// SRC — a source video being continued from, edited, or mined for structure.
    pub source_video: Option<H3SourceUse>,
    /// ID — one or more identity / style / storyboard images that are not frame anchors.
    pub identity_images: bool,
    /// ACOPY — an audio signal whose actual samples are reused, in full or part.
    pub audio_copy: bool,
    /// AREF — an audio signal only referenced (timbre / style / beat / continuity).
    pub audio_reference: bool,
}

/// The resolved plan: which checkpoint (⇒ which guide), which alignment variant, what goes on the
/// wire, and the task-type prefix the roles imply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H3Plan {
    pub checkpoint: H3Checkpoint,
    pub variant: Fl2vaVariant,
    /// The `task` field value: `"t2va"`, `"fl2va"` or `"ref2va"` (`REQ-*:12`).
    pub wire_task: &'static str,
    /// Empty on the FL2VA family, which has no `summary` section.
    pub task_types: Vec<H3TaskType>,
}

impl H3Plan {
    /// The `summary:` bracket prefix, e.g. `[video editing + audio reuse]`. Empty on the FL2VA
    /// family. Separator is space-plus-space and the brackets carry no inner padding
    /// (`GUIDE-ref:141`).
    pub fn summary_prefix(&self) -> String {
        if self.task_types.is_empty() {
            return String::new();
        }
        let joined: Vec<&str> = self.task_types.iter().map(|t| t.as_str()).collect();
        format!("[{}]", joined.join(" + "))
    }
}

impl H3Inputs {
    /// Read the six inputs off a reference roster plus the two frame pins the caller knows about.
    ///
    /// `first_frame` / `last_frame` are the caller's FL2VA pins (an attached opening image, an
    /// `end_image`). A Ref2VA `FrameAnchor` image folds into `first_frame`, because on that
    /// checkpoint the anchor is declared in prose rather than by `frame_index` and produces the same
    /// `keyframe completion` task type.
    ///
    /// A soundtracked video contributes its audio task type from the VIDEO'S role, per
    /// `GUIDE-ref:145`: editing a source video whose original audio stays audible is `audio reuse`;
    /// continuing one without copying the signal is `audio reference`.
    pub fn from_refs(refs: &[H3RefBrief], first_frame: bool, last_frame: bool) -> Self {
        let mut out = H3Inputs {
            first_frame,
            last_frame,
            ..H3Inputs::default()
        };
        for r in refs {
            match (r.kind, r.role()) {
                (H3ReferenceKind::Image, H3ReferenceRole::FirstFrame) => out.first_frame = true,
                (H3ReferenceKind::Image, H3ReferenceRole::LastFrame) => out.last_frame = true,
                (H3ReferenceKind::Image, H3ReferenceRole::FrameAnchor) => out.first_frame = true,
                (H3ReferenceKind::Image, _) => out.identity_images = true,
                (H3ReferenceKind::Video, role) => {
                    let use_ = match role {
                        H3ReferenceRole::EditSource => H3SourceUse::Edit,
                        H3ReferenceRole::ContinuationSource => H3SourceUse::Continue,
                        _ => H3SourceUse::Structure,
                    };
                    // Several videos: the strongest relationship wins, because a request that both
                    // edits and continues is described by the edit.
                    out.source_video = Some(match (out.source_video, use_) {
                        (Some(H3SourceUse::Edit), _) | (_, H3SourceUse::Edit) => H3SourceUse::Edit,
                        (Some(H3SourceUse::Continue), _) | (_, H3SourceUse::Continue) => {
                            H3SourceUse::Continue
                        }
                        _ => H3SourceUse::Structure,
                    });
                    if r.has_audio {
                        match use_ {
                            H3SourceUse::Edit => out.audio_copy = true,
                            _ => out.audio_reference = true,
                        }
                    }
                }
                (H3ReferenceKind::Audio, H3ReferenceRole::AudioReuse) => out.audio_copy = true,
                (H3ReferenceKind::Audio, _) => out.audio_reference = true,
            }
        }
        out
    }

    /// **Checkpoint selection, and the task types the declared roles imply.**
    ///
    /// Checkpoint first, because it decides which guide the writer is handed:
    ///
    /// ```text
    /// if any identity image, any reference video, or any reference audio  → Ref2VA  (task "ref2va")
    /// else if a first or last frame is pinned                             → FL2VA   (task "fl2va")
    /// else                                                                → FL2VA   (task "t2va")
    /// ```
    ///
    /// ⚠️ The task types are a *derivation from our role vocabulary*, offered to the writer as the
    /// reading koblem's own metadata supports. `GUIDE-ref` §3 is the authority on what each type
    /// means and when it applies, it is in the prompt, and it wins.
    ///
    /// UNVERIFIED: **the ordering of ` + `-joined types is our invention.** No rule is stated and the
    /// corpus is not self-consistent — `GUIDE-ref:127` puts `audio reuse` after
    /// `reference generation`, while the served `REQ-ref2va:13` writes
    /// `[video editing + audio reference + audio reuse]`. We emit visual-structural first, then
    /// audio, most-specific first. Settled by an official statement of the ordering.
    pub fn resolve(&self) -> H3Plan {
        let has_reference_asset = self.identity_images
            || self.source_video.is_some()
            || self.audio_copy
            || self.audio_reference;
        let checkpoint = if has_reference_asset {
            H3Checkpoint::Ref2va
        } else {
            H3Checkpoint::Fl2vaFamily
        };
        let variant = match (self.first_frame, self.last_frame) {
            (false, false) => Fl2vaVariant::T2va,
            (true, false) => Fl2vaVariant::I2va,
            (false, true) => Fl2vaVariant::L2va,
            (true, true) => Fl2vaVariant::Fl2va,
        };
        let wire_task = match (checkpoint, variant) {
            (H3Checkpoint::Ref2va, _) => "ref2va",
            (H3Checkpoint::Fl2vaFamily, Fl2vaVariant::T2va) => "t2va",
            (H3Checkpoint::Fl2vaFamily, _) => "fl2va",
        };
        let mut task_types = Vec::new();
        if checkpoint == H3Checkpoint::Ref2va {
            match self.source_video {
                Some(H3SourceUse::Edit) => task_types.push(H3TaskType::VideoEditing),
                Some(H3SourceUse::Continue) => task_types.push(H3TaskType::VideoContinuation),
                _ => {}
            }
            if self.first_frame || self.last_frame {
                task_types.push(H3TaskType::KeyframeCompletion);
            }
            if self.identity_images || self.source_video == Some(H3SourceUse::Structure) {
                task_types.push(H3TaskType::ReferenceGeneration);
            }
            if self.audio_copy {
                task_types.push(H3TaskType::AudioReuse);
            }
            if self.audio_reference {
                task_types.push(H3TaskType::AudioReference);
            }
        }
        H3Plan {
            checkpoint,
            variant,
            wire_task,
            task_types,
        }
    }
}

// ─── The render's own numbers ─────────────────────────────────────────────────

/// What this generation actually renders: frames, snapped seconds, canvas.
///
/// 🔴 **The single most expensive fact to get wrong**, and the reason it is stated rather than left
/// to the guide. Every `MM:SS.mmm` cut time in the body is absolute from the start of the
/// generation, and the FL2VA alignment line's `S.SS` is its end — so a writer handed the *authored*
/// duration instead of the *snapped* one writes cut times the render is already past, and a writer
/// handed one shot's length for a three-shot pack crams the whole story into the first third.
///
/// 🔴 The seconds are re-derived FROM the frame count rather than echoed, so the two numbers can
/// never contradict each other. The grid is 17n+5 at 24 fps, so 10.0 authored seconds is 243 frames
/// is 10.125 s — a caller that forgot to snap gets corrected here instead of handing the writer a
/// duration and a frame count that disagree by a quarter of a second. Stating frames as well as
/// seconds is deliberate: 10.13 s is not a number anyone would keep, and `243 frames at 24 fps` is
/// what stops the writer rounding it back to 10.
pub fn render_facts(seconds: Option<f64>, aspect: Option<&str>) -> String {
    let canvas = match aspect.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!(" The canvas is {a}."),
        None => String::new(),
    };
    let Some(authored) = seconds.filter(|s| *s > 0.0) else {
        return format!(
            "\n\nTHIS RENDER — one generation of {MIN_GENERATION_SECONDS:.0}-\
             {MAX_GENERATION_SECONDS:.0} seconds; the exact length is not settled yet.{canvas} \
             Never write a cut time you cannot place inside it."
        );
    };
    let frames = koblem_client::types::h3_frames_from_seconds(authored);
    let s = koblem_client::types::h3_seconds_from_frames(frames);
    format!(
        "\n\nTHIS RENDER — exactly {frames} frames at 24 fps, which is {s:.2} seconds. That is the \
         snapped number the engine will use, not a round one anybody would have chosen, and every \
         time you write is measured against it.{canvas} A `[Shot N]` time is absolute from the START \
         of this generation, strictly increasing, and strictly inside {s:.2} seconds with room left \
         for the final beat to play."
    )
}

/// How many cuts the author authored, and which one they are editing.
///
/// 🔴 **The shot count is a REQUIREMENT, not a hint.** The Director packs several editor clips into
/// one generation *before* this prompt is written, and the reply's shots are mapped back onto those
/// clips **by position**. A writer that returns two bodies for a three-clip pack does not lose the
/// third quietly — it slides body 2 onto clip 2's neighbour and overwrites the wrong one. Nothing in
/// the guide could know this; it is a property of our editor.
///
/// 🔴 **It counts `[Shot N]` BLOCKS, not CUTS**, and saying so is the whole affordance a packed
/// generation has for timing a moment. The author divided the timeline into N pieces; whether the
/// picture actually changes at each division is decided by the FIRST WORDS of the body
/// ([`shot_and_beat_facts`]), and a division that comes back unverbed is a beat — the camera runs
/// through it and the moment lands on its time, at no cost in cuts and with the count unchanged.
/// Without this sentence a writer reads "the author has already placed the cuts" as an instruction
/// to cut N times, which is how a packed generation ends up chopped into N framings the author never
/// asked for. The count fact lives here because the count is this clause's business; what an
/// unverbed tag DOES is stated once, below, and is not repeated here.
///
/// 🔴 The focus is the other half of the same sentence and may never be said without it. The draft
/// button lives on ONE clip and the reply covers the WHOLE generation: say only "the author is
/// editing Shot 2" and the writer helpfully returns one body, which lands on Shot 1.
///
/// `shot_focus` is 0-BASED (it indexes the pack); the `[Shot N]` tag is 1-based. This is the one
/// place the two numberings meet. A focus outside a known count is dropped rather than clamped — the
/// caller that sent it has a bug and pointing the writer at a shot that is not there is worse than
/// silence.
pub fn cut_plan_facts(shot_count: Option<u32>, shot_focus: Option<u32>) -> String {
    let count = shot_count.filter(|n| *n > 0);
    let mut s = String::new();
    match count {
        Some(n) => s.push_str(&format!(
            "\n\nTHE CUTS — the author has already divided this generation on the timeline into {n} \
             {divisions}. Write a `[Shot N]` opening on a TRANSITION for each of them, in order and \
             with no gaps, so the render cuts where the author cut. 🔴 {n} is a floor, NOT a \
             ceiling ON CUTS ONLY IF THE AUTHOR ADDS MORE: it counts the author's divisions, and \
             every `[Shot N]` you open is a cut. Time other moments with `At MM:SS.mmm,` clauses \
             INSIDE a shot, as the fact below explains, rather than by opening new shots. The \
             first {n} tags are read \
             back onto those divisions BY POSITION, so keep them in the author's order; anything \
             you add beyond them the timeline reports and can absorb.",
            divisions = if n == 1 { "division" } else { "divisions" },
        )),
        None => s.push_str(
            "\n\nTHE CUTS — the author has not placed any, so you choose how many there are. \
             Every `[Shot N]` is a cut; a moment that merely happens at a time is a timed clause \
             inside a shot. Let each cut earn itself.",
        ),
    }
    if let Some(focus) = shot_focus
        && count.is_none_or(|n| focus < n)
    {
        let tag = focus + 1;
        // 🔴 NOT "of the {n} in this generation". That phrasing counted `[Shot N]` tags, and since
        // `count` is the author's DIVISION count it announced a total the writer was then unwilling
        // to exceed — directly contradicting `cut_plan_facts`, which says in the same prompt that
        // the number is a floor and not a ceiling. Measured 2026-08-09 on the local 12B: the two
        // clauses together sent it into a reasoning loop that emitted no prompt at all in 14k
        // tokens, where the same prompt without this phrase wrote four beats.
        let of = match count {
            Some(n) if n > 1 => {
                format!(" — the {n} divisions are the author's, and this is one of them")
            }
            _ => String::new(),
        };
        s.push_str(&format!(
            " WHAT THE AUTHOR IS LOOKING AT is [Shot {tag}]{of} — the idea, the instruction and \
             any change you are given are about THAT one, and that is where the new writing goes. \
             It is not the only body you return: the reply is the whole generation, so every other \
             `[Shot N]` comes back too, in order, re-stated as it already stands unless the change \
             genuinely reaches it. Restated is not dropped — a reply carrying only [Shot {tag}] is \
             read back by position and overwrites the wrong one."
        ));
    }
    s
}

/// The project's recurring cast, and the features the author agreed for each.
///
/// The guide knows how identity is carried (`<Subject N>` definitions on Ref2VA, prose
/// back-references on the FL2VA family, `GUIDE-ref:246`). It cannot know that this project's woman
/// in the blue cardigan is called Mia and has a thin silver necklace. Only the roster is stated;
/// how to spend it is the guide's.
///
/// Empty for an empty cast, so a generation with no roster is byte-identical.
pub fn cast_roster_clause(cast: &[LtxCastMember]) -> String {
    let live: Vec<&LtxCastMember> = cast.iter().filter(|c| !c.is_empty()).collect();
    if live.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n\nRECURRING CAST — the author's own roster for this project. Call each character by the \
         SAME name every time and keep the features below exactly as written; vary expression, gaze \
         and posture freely. A character not listed here is not in this project.",
    );
    for c in live {
        let name = c.name.trim();
        let appearance = c.appearance.trim();
        let name = if name.is_empty() { "(unnamed)" } else { name };
        if appearance.is_empty() {
            s.push_str(&format!("\n  - {name} — no agreed features yet."));
        } else {
            s.push_str(&format!("\n  - {name} — {appearance}"));
        }
    }
    s
}

// ─── The vocal lane ───────────────────────────────────────────────────────────

/// `MM:SS.mmm` — the exact spelling a `[Shot N] At …` mark takes, so a span in the brief is
/// paste-ready into the block being written.
///
/// Mirrors `web/src/lib/director/h3-soundtrack.ts::h3Timecode`. Milliseconds are not decoration: at
/// 88 BPM a beat is 0.68 s and H3 places a marked action to roughly ±0.5 s, so a mark rounded to the
/// nearest second can miss by more than a beat before the model's own tolerance is counted.
pub fn h3_timecode(seconds: f64) -> String {
    let s = seconds.max(0.0);
    let mm = (s / 60.0).floor();
    format!("{:02}:{:06.3}", mm as u64, s - mm * 60.0)
}

/// One stretch of THIS generation's slice of the song where a voice is audible.
///
/// 🔴 **`start`/`end` are LOCAL seconds** — measured from the generation's own frame 0, which is
/// where its slice of the song begins. That is the same clock every `[Shot N] At MM:SS.mmm` mark
/// runs on, and it is NOT the song clock: `local = song_time - song_offset_seconds`, with the offset
/// coming from `koblem_client::types::h3_plan_soundtrack`. See `h3_soundtrack.rs`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct H3VocalSpan {
    pub start: f64,
    pub end: f64,
    /// A few of the transcript's own words at this span's ENTRY, or `None`.
    ///
    /// 🔴 **Approximate, and the clause says so out loud.** The lane comes from Parakeet run on a
    /// SINGING voice, which mishears freely ("Idol Hitler" for "Adolf Hitler" on the project this
    /// was built for). They are carried anyway because MiniMax's guide §5.4 documents the one idiom
    /// that makes a performer mime a reused soundtrack — *"When `<Audio 1>` reaches the phrase
    /// `<d>[English] …</d>`, `<Subject 1>` performs …"* — and that idiom needs a phrase. The writer
    /// is told to use a garbled one as a POSITION and not as a quotation.
    #[serde(default)]
    pub words: Option<String>,
}

/// Where the song's voice falls inside ONE generation.
///
/// # Why this is a struct and not `Vec<(f64, f64)>`
///
/// 🔴 Because the empty case is the whole point. A bare `Vec` of spans cannot distinguish
/// **"nobody measured"** from **"measured, and this generation is entirely instrumental"** — and it
/// is exactly the second one that produced the defect this type exists to kill: on the 15-generation
/// project reviewed 2026-08-14, three generations whose slice of the song has no vocal in it at all
/// were written with *"its mouth moves in perfect synchronization with the male vocals"* as a
/// standing fact, i.e. ~40 s of a character's mouth flapping at nothing. Wrapping the spans means
/// "no lane supplied" is the ABSENCE of an `H3VocalLane` and "no vocal here" is a present lane with
/// `spans: []`, and the two emit opposite instructions.
///
/// # Why it hangs off the GENERATION and not off `H3RefBrief`
///
/// The reference roster is project-wide and shared — `h3RefBriefs` builds one list for the whole
/// document. A vocal lane is per generation by construction (each generation is conditioned on its
/// own ~14 s slice), so putting it on the soundtrack reference would give every generation the same
/// answer, which is the answer that was already wrong.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct H3VocalLane {
    /// 1-based generation number, for the paths that describe MANY generations at once (the batch
    /// script writer, a rework). `None` on the single-generation path, where "this generation" is
    /// unambiguous and numbering it would invite the writer to address a neighbour.
    #[serde(default)]
    pub generation: Option<u32>,
    /// This generation's rendered length, seconds — the lane's own denominator, so the clause can
    /// name the stretches NOT covered by a span. Absent ⇒ the length is not printed and the TAIL
    /// gap (after the last span) cannot be stated, because nothing says where the render ends; the
    /// standing rule about "every other second" still applies and is stated unconditionally.
    #[serde(default)]
    pub seconds: Option<f64>,
    /// The vocal spans, in order, LOCAL seconds. Empty ⇒ this generation is entirely instrumental,
    /// which is a measurement and not a silence.
    #[serde(default)]
    pub spans: Vec<H3VocalSpan>,
    /// 🔴 **Seconds of RUN-UP at the head of this render that never reach the cut.**
    ///
    /// With `placement="on_target"` the engine overlays the previous generation's tail onto this
    /// one's first `frames_for_latents(k)` frames — 22 at K=7, 0.917 s — so the model renders them,
    /// HEARS their audio, and then they are trimmed. The spans above are CLIPPED past this point,
    /// not reported across it: `vocalLaneFor` (`web/src/lib/director/prompt-context.ts`) starts
    /// every span at `songOffsetSeconds + run_up`, so no span in this lane addresses a frame the
    /// cut never shows.
    ///
    /// What this number must NOT do is invite a cue there. A `[Shot N] At MM:SS.mmm` at or below it
    /// lands in the trimmed head and is discarded in silence — measured, not theorised: one authored
    /// mark sat at 0.592 s against a 0.917 s run-up and had to be rewritten as an opening state.
    /// So the clause spends this on ONE instruction: a phrase starting inside the run-up is already
    /// sounding when the shot appears and is described as an opening state, never timed.
    ///
    /// `0` on the first generation and wherever the seam is unanchored. Absent ⇒ an older client;
    /// the run-up sentence is simply not emitted.
    #[serde(default)]
    pub run_up_seconds: Option<f64>,
}

impl H3VocalLane {
    fn is_instrumental(&self) -> bool {
        self.spans.is_empty()
    }

    /// The gaps between the spans, LOCAL seconds — including the head before the first span and the
    /// tail after the last one when [`Self::seconds`] is known.
    ///
    /// Gaps under a beat are dropped: at 88 BPM a beat is 0.68 s, a 0.3 s gap between two sung words
    /// is not a moment where anyone shuts their mouth, and listing it would turn a readable lane
    /// into noise the writer skims.
    fn instrumental_gaps(&self) -> Vec<(f64, f64)> {
        const MIN_GAP: f64 = 0.7;
        let mut out = Vec::new();
        let mut cursor = 0.0_f64;
        for span in &self.spans {
            if span.start - cursor >= MIN_GAP {
                out.push((cursor, span.start));
            }
            cursor = cursor.max(span.end);
        }
        if let Some(total) = self.seconds
            && total - cursor >= MIN_GAP
        {
            out.push((cursor, total));
        }
        out
    }
}

/// Render one lane's measured facts as bullet lines. No instructions — those are stated once, below
/// the whole list, so a batch of fifteen generations does not repeat them fifteen times.
fn vocal_lane_lines(lane: &H3VocalLane) -> String {
    let who = match lane.generation {
        Some(n) => format!("Generation {n}"),
        None => "This generation".to_string(),
    };
    let length = lane
        .seconds
        .map(|s| format!(" ({s:.2}s long)"))
        .unwrap_or_default();
    if lane.is_instrumental() {
        return format!(
            "\n  - {who}{length}: **NO VOCAL AT ALL.** Its whole slice of the song is instrumental."
        );
    }
    let mut s = format!("\n  - {who}{length}: vocal at");
    for (i, span) in lane.spans.iter().enumerate() {
        if i > 0 {
            s.push(';');
        }
        s.push_str(&format!(
            " {}-{}",
            h3_timecode(span.start),
            h3_timecode(span.end)
        ));
        if let Some(w) = span
            .words
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty())
        {
            s.push_str(&format!(" (heard as \"{w}\")"));
        }
    }
    s.push('.');
    let gaps = lane.instrumental_gaps();
    if gaps.is_empty() {
        s.push_str(" No instrumental gap long enough to matter.");
    } else {
        s.push_str(" NO VOICE at");
        for (i, (a, b)) in gaps.iter().enumerate() {
            if i > 0 {
                s.push(';');
            }
            s.push_str(&format!(" {}-{}", h3_timecode(*a), h3_timecode(*b)));
        }
        s.push('.');
    }
    // 🔴 A FLOOR, NOT AN EXPLANATION. The head of an on-target render is a run-up: the model
    // renders and hears it, then it is trimmed and never reaches the video. The lane's spans are
    // already clipped past it, so the only thing left to say is where marks become legal — the
    // writer does not need the mechanism, and a paragraph about frames it cannot influence is
    // noise. Measured why it matters: one authored mark sat at 0.592s against a 0.917s run-up and
    // was discarded in silence until it was rewritten as an opening state.
    if let Some(run_up) = lane.run_up_seconds.filter(|r| *r > 0.0) {
        s.push_str(&format!(
            " ⚠️ The earliest markable time in this generation is {}; anything before that is \
             trimmed off and never reaches the video, so no `[Shot N] At MM:SS.mmm` may be earlier. \
             A vocal already sounding at that point belongs to the shot's OPENING STATE — describe \
             it as already mid-phrase and give it no timestamp.",
            h3_timecode(run_up)
        ));
    }
    s
}

/// 🔴 **A song's vocal is not continuous, and one boolean cannot say so.**
///
/// The writer's entire audio knowledge used to be `speech_in_shot: Option<bool>`, and `Some(true)`
/// emitted *"a `<d>` block and a moving mouth are correct"* — as a STANDING fact, over a whole
/// generation. On a real 15-generation music video (reviewed 2026-08-14) that produced three
/// generations of a character miming a vocal that had ended 37 s earlier, plus several more that
/// sang through their own instrumental breaks, because half a second of vocal and a ten-second
/// guitar solo are the same `Some(true)`.
///
/// This states the SPANS instead. Everything it asks for follows from one property of the engine:
/// the `<Audio N>` definition in `subject_definitions` is a standing fact about the whole render and
/// outranks anything a shot body says, so the spans have to be stated THERE or a per-shot correction
/// simply loses to them.
///
/// Empty for an empty lane list — see [`audio_and_text_facts`] for why absence must stay silent.
pub fn vocal_lane_facts(lanes: &[H3VocalLane]) -> String {
    if lanes.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n\nTHE VOCAL LANE — WHICH SECONDS ACTUALLY CARRY THE SONG'S VOICE. Every time below is on \
         the GENERATION's OWN LOCAL CLOCK, the same clock a `[Shot N] At MM:SS.mmm` mark runs on: \
         each generation is conditioned on its own slice of the song, so that slice starts at \
         00:00.000 here however far into the track it sits. Do not convert these to song time and \
         do not carry a mark from one generation into another.",
    );
    for lane in lanes {
        s.push_str(&vocal_lane_lines(lane));
    }
    s.push_str(
        "\n\n🔴 PUT THE SPANS IN THE `<Audio N>` DEFINITION, in `subject_definitions`. That \
         definition is a STANDING FACT about the whole generation and it outranks every shot body, \
         so \"its mouth moves in synchronisation with the vocals\" written there sings for the \
         entire render no matter what a later `[Shot N]` says. Name the seconds the voice is in, \
         and in the same breath say what the mouth and jaw DO across every other second — stated \
         positively, as something visible (\"its jaw stays shut and its neon teeth are set in one \
         fixed line\"), never as an absence. Then give each vocal ENTRY above its own \
         `[Shot N] At MM:SS.mmm` so the performance starts where the voice does.\n\n\
         🔴 A GENERATION LISTED ABOVE AS HAVING NO VOCAL HAS NO VOICE IN IT. Write no `<d>` block \
         for it, and do not write \"sings\", \"mouths\", \"lip-syncs\", \"mimes\" or \"in \
         synchronisation with the vocals\" anywhere in it. The music is still playing and the \
         characters may still move to it — what they may not do is perform a vocal that is not \
         there. (An ordinary SPOKEN line the author explicitly asked for is a different thing: that \
         is a real speaker and takes its own `(Sx)`. Do not invent one to fill the gap.)\n\n\
         The song is REUSED audio, so a performer miming it is NOT a separate vocal source. Use the \
         guide's §5.4 form — `When <Audio N> reaches <d>[English] …</d>, <Subject M> performs that \
         vocal` — and do NOT assign the song's voice an `(Sx)`.\n\n\
         ⚠️ ANY QUOTED WORDS ABOVE ARE A MACHINE TRANSCRIPT OF SINGING AND ARE APPROXIMATE. The \
         recogniser mishears sung consonants badly and has turned real lyrics into nonsense. Use \
         them to place the entry, and quote inside `<d>` only a phrase that plainly reads as \
         ordinary language; where a phrase looks garbled, cue the moment by its time instead, or \
         write `[unclear]` as the guide's §5.4 allows. Never repair a lyric by guessing at it.",
    );
    s
}

/// The facts about this generation's sound that the guide cannot see.
///
/// `speech_in_shot` is the author's own switch on the clip, and it is worth stating because the
/// failure it prevents is invisible: a generation with nobody speaking that nevertheless gets a
/// `<d>` block renders a character mouthing words the author never wrote.
///
/// `on_screen_text` likewise — the guide's rule for visible text (`GUIDE-base:146`) is one line and
/// is already in the prompt; what koblem knows is whether there is any.
///
/// 🔴 **A lane REPLACES the `speech_in_shot` sentence rather than joining it.** Both are read off
/// the same transcript, so they can only ever agree — and the boolean's `Some(true)` wording is
/// itself the defect ([`vocal_lane_facts`]): it licenses a moving mouth for the whole generation,
/// which is precisely what the lane exists to bound. Saying both would leave the writer holding a
/// standing permission and a set of limits on it, and the standing permission is the one that gets
/// followed.
///
/// ⚠️ An empty `lanes` keeps the pre-lane behaviour byte for byte, so a client that does not send
/// one is unchanged.
pub fn audio_and_text_facts(
    speech_in_shot: Option<bool>,
    on_screen_text: bool,
    lanes: &[H3VocalLane],
) -> String {
    let mut s = String::new();
    if lanes.is_empty() {
        match speech_in_shot {
            Some(true) => s.push_str(
                "\n\nSOMEBODY SPEAKS OR SINGS in this generation — the author has said so, so a \
                 `<d>` block and a moving mouth are correct.",
            ),
            Some(false) => s.push_str(
                "\n\nNOBODY SPEAKS OR SINGS in this generation — the author has said so. Write no \
                 `<d>` block at all and put no moving mouth on anyone; carry the shot on action, \
                 ambience and score.",
            ),
            None => {}
        }
    } else {
        s.push_str(&vocal_lane_facts(lanes));
    }
    if on_screen_text {
        s.push_str(
            "\n\nSOMETHING APPEARS ON SCREEN AS TEXT in this generation — the author has said so, \
             so quote the exact copy where the guide says it goes.",
        );
    }
    s
}

/// The current date, pinned. koblem's convention (see `routes/images.rs`, `routes/krea2.rs`): models
/// reject or mangle post-cutoff dates, and a writer asked for "a modern city" or handed a brief that
/// says "this week" otherwise guesses at the year.
pub fn current_date_clause() -> String {
    format!(
        "\n\nCURRENT DATE: {}. Treat it as today when the brief refers to the present, and never \
         contradict it.",
        crate::routes::images::today_iso()
    )
}

// ─── koblem's own unit: generations, and the boundary between them ────────────

/// 🔴 **The generation/cut split is koblem's product concept, and the guide has never heard of it.**
///
/// The guide describes ONE render. koblem cuts a video into many, packs several editor clips into
/// each, and renders them one call at a time. That makes the `[Shot N]` cut cheap — inside one
/// generation the shots share the sequence, the soundtrack and the score, identity carries in prose
/// (*"the captain from Shot 1"*, `REQ-t2va:13`), and a line of dialogue crosses a cut with
/// `<scenetrans>` — and makes the GENERATION BOUNDARY expensive: a separate render, a soundtrack
/// from noise, no prose carry, no frame-exact join.
///
/// That asymmetry is the whole reason the Director packs, and nothing outside koblem can tell the
/// writer about it.
///
/// 🔴 **The second paragraph is not a style note — it is a correctness rule, and it was learned from
/// a document where the drift had already happened.** Reviewed 2026-08-13 across the 15 generations
/// of "Gawdwin — Sealed by Fate": five of them opened *"The scene continues with…"* or *"The scene
/// opens on a continuation of the previous shot: …"*, and two of those then went on to RE-STAGE the
/// previous generation's action before starting their own. Generation 12 re-ran a creature carrying
/// the head through snow — an event that generation 10 had already ended, two generations earlier —
/// and generation 13 re-ran it again, so the ship "scooped the head up off the ground" that
/// generation 11 had already strapped it off. The narration was not decorative; the writer treated
/// the recap as content and rendered it. A render starting from noise cannot show "what happened
/// before", so every word spent claiming continuity is spent staging it again.
pub const GENERATION_BOUNDARY_FACTS: &str = "\n\nONE GENERATION, MANY CUTS — koblem renders a video \
as a sequence of GENERATIONS, and this prompt is one of them. Everything the guide calls a shot \
(`[Shot N]`) is a cut INSIDE this one generation: those shots share the sequence, the soundtrack and \
the score, a character carried across a cut by plain prose, and a line of dialogue that runs across \
a cut with `<scenetrans>`. So cut freely, wherever the cut earns itself. What you cannot reach \
across is the GENERATION boundary: the next generation is a separate render with its own soundtrack \
from noise, no prose carry and a join that is never frame-exact. If a sequence fits inside this \
generation's duration, keep it here as several timed `At MM:SS.mmm,` clauses inside the shot \
 rather than letting it spill.\n\n\
🔴 SO NEVER WRITE A SENTENCE ABOUT A PREVIOUS GENERATION. The model rendering this block has not \
seen one and has no memory of one, so \"the scene continues from the previous shot\", \"as before\", \
\"the creature from the previous scene\" and \"it resumes what it was doing\" are not continuity — \
they are an instruction to STAGE that other thing here, and it gets staged. Measured on a real \
project: two generations opened by recapping an event that had ended two generations earlier, and \
both rendered the recap, undoing what the timeline had already moved past. Write the opening STATE \
instead, as something plainly visible in this render's own first second — \"the scene opens on the \
robot already mid-march, seen from the front at a fixed tracking distance\" — and define a returning \
character in `subject_definitions` by what it LOOKS like, never as \"the one from the previous \
generation\". The join to what came before is koblem's job, not the prose's.";

/// 🔴 Earned the hard way: five distinct defects on ONE 14-second shot over five renders, all the
/// same shape, none of them the model misbehaving. Each was a span carrying no instruction, filled
/// with the strongest available prior. Written up in
/// `~/handoffs/koblem/h3-director/` and worth restating here because a writer that knows it
/// produces the right prompt first time instead of after five renders:
///
/// | symptom | the actual gap |
/// | --- | --- |
/// | mouth flapped for 8 s before any vocal | nothing said what the mouth was doing while running |
/// | it sang from frame 0 regardless | `<Audio 1>`'s definition said *"performs its vocal"* with no time bound |
/// | mouth opened on a hard skid | a violent physical beat with no mouth state |
/// | its face appeared on the BACK of its head | *"does not turn its body"* + *"keeps its face towards the lens"* |
/// | it returned to the reference photo's alley | the closing beat named no setting at all |
///
/// Two more of the same shape, both found 2026-08-13 reviewing all 15 generations of a music video:
///
/// | symptom | the actual gap |
/// | --- | --- |
/// | the mouth mimed a vocal that is not in the slice | *"in perfect synchronisation with the male vocals"* as a STANDING fact, on three generations whose slice of the song is instrumental — the song's vocal ends 37 s before the video does |
/// | the same fixed dance ran under every mood the track has | a named step (*"headbanging"*, *"an aggressive heavy-metal dance"*) running the whole generation, with nothing saying how the energy changes under it |
///
/// And the correction to that one, which cost more than the defect did (2026-08-15). The fix
/// applied above was "write the movement QUALITY and bind it to the audio" — *"holds a still,
/// weighted, mechanical pose between the accents and moves on them"*. It went onto 15 of 16
/// generations of the same music video and produced a robot that **bopped like an awkward teenager
/// at a dance party**. The abstract half ("takes its movement from the tone") does not resolve into
/// a body at all; the concrete half ("still between the accents") resolves perfectly, into a twitch.
///
/// 🔴 **And then the correction to THAT one, which is the version to keep.** "Concrete body actions
/// with amplitude" was the obvious next move, and it is ALSO wrong — measured, not argued, on
/// **39 clips: six prompt arms × three seeds** over one 8-second window, plus a five-rung reduction
/// ladder, ranked by the author on the pictures. Results, in the author's words: the fully specified
/// choreography arm moved the most of anything and was *"probably overexaggerated"*; the arm cut to
/// a single plain sentence moved measurably less and was *"not a statue, just suited to the music"*.
///
/// | arm | silhouette pose change | verdict |
/// |---|---|---|
/// | the bop clause above | 0.10 | the original defect |
/// | bare prose, camera LOCKED | 0.12 | still the defect |
/// | one plain sentence, camera FREE | 0.20–0.25 | ✅ what shipped |
/// | full choreography, camera FREE | 0.30–0.37 | ❌ overacted |
///
/// So the usable range has a **ceiling as well as a floor**, and the three real levers are, in
/// order: (1) the camera — the same one-sentence body direction nearly DOUBLED with a moving
/// camera, and it also placed timed events far more tightly; (2) the body needs one plain concrete
/// sentence, no more; (3) timed `[Shot N]` marks contribute NOTHING to amplitude — deleting all of
/// them measured identically — so they are for story only.
///
/// ⚠️ Lever (1) was written as *"never lock the camera"* and that is **too strong**, corrected
/// 2026-08-16 against 345-frame renders at the project's own canvas. Camera motion trades directly
/// against picture stability — background churn 1.7 locked / 3.3 dolly / 7.6 slow arc / 8.7 busier
/// — and an arc costs most because it must render scenery the prompt never described. Worse, the
/// choice cannot be delegated: told nothing, given explicit permission, or told to move *with the
/// music*, the engine returned a dead-still camera on every seed. **It does not self-direct**, so
/// naming a move is a per-shot decision rather than a default. And a shot must never END on a
/// framing instruction — across a chain each shot's final framing becomes the next one's opening,
/// so "comes to rest, centred, clean for continuation" compounds into a locked close-up by the
/// last shot. That clause was on nearly every generation of a real 16-shot video and is exactly
/// what produced "camera fixed on the robot up close the whole time".
/// ⚠️ The amplitude metric behind those numbers is a FLOOR DETECTOR: it separates a statue from a
/// performance and then stops tracking quality and starts working against it. Never rank on it.
/// Harness, clips and the full ladder: `~/handoffs/docker/comfyui/h3-dance/`.
///
/// The guide cannot supply any of this: it documents the SCHEMA, and this is behaviour.
///
/// ⚠️ Deliberately says nothing about section order, word counts or the retention vocabulary.
/// The guide owns those, and a second copy here would be the paraphrase this module exists to
/// avoid.
pub fn unspecified_span_facts() -> String {
    String::from(
        "\n\nWHAT HAPPENS TO A SPAN YOU SAY NOTHING ABOUT — it does not stay neutral, it gets \
         filled. H3 denoises picture and sound together, so every second of the render needs a \
         value for the things that can move, and anything you leave unstated is supplied by the \
         model from whatever it has: the reference image's background, the previous generation's \
         last frames, or — most often — a performance nobody asked for. Measured repeatedly on \
         this engine: a robot told only to RUN spent the run mouthing words, because its mouth had \
         no stated behaviour and a face beside a soundtrack gets one.\n\nSo audit for SILENCE, not \
         for mistakes. Walk the generation second by second and ask which seconds carry no \
         instruction for the FACE, the SETTING and the CAMERA — those three go wrong most — then \
         say what IS true there. \"Its jaw stays shut, teeth clenched with the strain of the run\" \
         is an instruction; leaving the mouth unmentioned is not.\n\n🔴 A `subject_definitions` \
         entry is a STANDING FACT for the whole generation, and it OUTRANKS any shot body that \
         argues with it. `<Audio 1> … which performs its vocal` — with no time bound — kept a \
         performer singing from the first frame through four separate shot-level instructions \
         saying its jaw was shut, and only stopped when the DEFINITION carried the bound: \
         `… which performs its vocal from 00:08.467 onward`. Anything true for only part of the \
         generation must say so where it is defined, not only where it happens.\n\n🔴 WRITE THE \
         END STATE POSITIVELY. A negation is the weak form — the model has to represent a thing in \
         order to be told it is absent, and saying \"no voice\" three times says \"voice\" three \
         times. \"Jaw locked shut, no lip movement of any kind\" failed three renders running; \
         \"its jaw stays shut, teeth clenched together with the strain of the run, bared in one \
         fixed grimace\" worked. Give the part an ACTION to be doing, and when a state must persist \
         say so as something visibly true — \"the dome holds only its glowing neon brain, and it \
         stays that way through the final frame\" — rather than as something that does not \
         happen.\n\n\
         🔴 KNOW THE LIMIT OF THAT RULE: A DEFINITION GOVERNS A STANDING STATE, NOT AN ARRIVAL TIME \
         AND NOT WHETHER A BACKGROUND HOLDS STILL. Putting a fact in `subject_definitions` works \
         when it is something ongoing the model would otherwise get wrong throughout — a mouth \
         singing, lights burning, a shell staying clean. It does NOT reliably constrain WHEN a \
         thing appears or WHETHER a place stays put. Both were tried and both failed, measured, two \
         seeds each: writing \"this rift is ABSENT from the picture for the first 11.5 seconds, the \
         air behind the subject is plain empty night sky\" into the rift's own definition moved its \
         arrival by nothing; writing \"this city is ONE single unchanging place, the same buildings \
         stay in the same places from the first frame to the last\" into the setting's definition \
         changed its stability by nothing. What fixed each was STRUCTURAL — giving the arrival its \
         own uncrowded beat with the transients before it claimed, and changing the camera move so \
         the shot stopped demanding scenery it had never described. ⇒ When the defect is about \
         TIMING, or about the picture HOLDING TOGETHER, more prose is the wrong tool; change the \
         beat structure or the camera. Save the definition for what is continuously true.\n\n\
         🔴 CHECK EVERY PAIR OF CONSTRAINTS FOR AN ABSURD SOLUTION. Two instructions \
         that can only both be satisfied by something ridiculous will be satisfied ridiculously. \
         \"stays standing in one spot and does not turn its body\" plus \"keeps its face towards \
         the lens for the whole move\", over a camera arcing 90 degrees, has exactly one solution: \
         the face moves to the back of the head. It rendered that, faithfully, more than once.\n\n\
         🔴 ONLY THE CAMERA CAN CHANGE WHAT IS VISIBLE. A subject turning, standing up or pointing \
         does not reveal anything — the lens decides what is in frame, so every reveal, every \
         change of setting and every new background has to be carried by a described camera move. \
         \"It turns ninety degrees and the rotation reveals the city behind it\" is not something \
         that can happen, and the model satisfies it the only way left: it SWAPS the background, \
         which reads as a hard cut in the middle of a shot. Write the move instead — the camera \
         arcs around the subject in one direction, and as it does the alley mouth drifts to the \
         edge and out of frame while the city comes round behind — and say what is behind the \
         subject at each stage, because a background nobody has named is free to be replaced. If \
         two places appear in one generation, say how the camera TRAVELS from one to the other.\n\n\
         🔴 A SONG'S VOCAL IS NOT CONTINUOUS, AND THE MOUTH IS ON FOR EXACTLY AS LONG AS THE VOCAL \
         IS. Any real track has intros, instrumental breaks, solos and an outro, and one generation \
         is a fifteen-second slice that can land entirely inside one of them. \"its mouth moves in \
         perfect synchronisation with the male vocals\" written as a standing fact over a slice with \
         no vocal in it does not produce stillness — it produces a mouth flapping at nothing for the \
         whole render, which is the single most reported defect on this engine. So state the SPANS: \
         say in the `<Audio N>` definition which seconds of THIS generation carry the vocal, name \
         what the mouth does across every other second (\"its jaw is closed and its neon teeth are \
         set in one fixed line\"), and time each sing-cue with an `At MM:SS.mmm,` clause at the \
         moment the vocal actually enters. If the brief does not tell you where the vocal falls, do \
         NOT default to \"throughout\" — write the singing against the one moment the author names \
         and say plainly what the mouth is doing on either side of it.\n\n\
         🔴 A DANCE IS A VOCABULARY OF MOVES, NOT ONE STEP — AND NOT A HELD POSE. There are two \
         ways to lose this, and the second was created by over-correcting the first. Naming a \
         single move (\"headbanging\", \"an aggressive heavy-metal dance\") runs that one move for \
         the whole generation regardless of what the track is doing under it. But replacing it \
         with pure movement QUALITY is worse, and this is the measured part: \"it takes its \
         movement from the tone of the audio at each moment rather than from any fixed step; it \
         holds a still, weighted pose between the accents and moves on them\" is abstract prose a \
         text encoder cannot resolve into a body, and the one concrete thing in it — STILL between \
         the accents, MOVE on them — renders exactly as written, as a small awkward twitch per hit. \
         That clause, on 15 of 16 generations of a real music video, produced a robot that bopped \
         like an awkward teenager instead of dancing.\n\n\
         🔴 BUT THE ANSWER IS NOT MORE SPECIFICATION — IT IS AN UNLOCKED CAMERA AND ONE PLAIN \
         SENTENCE. This was settled by rendering it: 39 clips, six prompt arms, three seeds each, \
         one 8-second window of a real song, ranked by the author on the pictures. Fully specified \
         full-body choreography (\"leaning hard in towards the lens, its torso swinging right \
         through from the hips, its shoulders working, one arm or both thrown wide and up on the \
         big accents\") measured the MOST movement of any arm and was judged OVERACTED. The arm \
         that won said only that the subject sings it like it means it, throws its whole body into \
         it, and never settles into a pose — one sentence — and it read as natural. So the \
         useful range has a CEILING as well as a floor, and writing past it buys overacting.\n\n\
         The other half is the CAMERA, and it has a REAL TRADE in it, so do not reach for a move \
         reflexively. Measured: the same bare one-sentence body direction read as nearly DOUBLE the \
         movement with a moving camera than a locked one, and a moving camera also placed timed \
         events far more tightly (seed spread 0.08 s against 2.5 s with no camera direction). But \
         every bit of camera motion costs the picture its stability — background churn rose \
         monotonically with it, 1.7 locked, 3.3 on a dolly, 7.6 on a slow arc, 8.7 on a busier one \
         — and an arc costs most because it has to render scenery the prompt never described. \
         🔴 What is NOT available is asking the model to choose: told nothing about the lens, given \
         explicit permission to move it, or told to move it WITH THE MUSIC, this engine returned a \
         dead-still camera every time, on every seed. It does not self-direct. So a camera move \
         happens only if it is named — which makes naming one a DECISION, not a default. Name it \
         when the shot needs it (a reveal, following a moving subject, making room for something) \
         and leave it unwritten when it does not. \
         🔴 And never write a shot's ending as a framing — \"comes to rest on a clean centred \
         framing so the next shot can carry on\" is a lock, and across a chain of continued \
         generations it compounds: every shot ends centred on the subject, which becomes the next \
         shot's opening, until the whole video is one locked close-up. Say what the SUBJECT is \
         doing at the end and let the lens alone.\n\n\
         So: give the body ONE plain sentence, decide the camera per shot, and spend the words you \
         saved on the story. Reserve fully-specified choreography for a dance the author has \
         actually asked for by name.\n\n\
         Two more things that fall out of the same renders. A timed `[Shot N] At MM:SS.mmm` mark \
         adds NOTHING to how much a body moves — deleting every mark and folding the whole \
         generation into one shot measured identically — so spend marks on story events and \
         never on trying to choreograph. And unless the character is meant to be rigid, do not \
         reach for \"heavy\", \"weighted\", \"deliberate\", \"every move controlled\", \"feet \
         planted\" or \"head level\" — each of those is a brake, and together they are a \
         specification for standing still.\n\n\
         Finally, do not borrow a subject's ANATOMY as a metaphor for scenery. \"the open mouth of \
         the alley\" sat two clauses from a robot whose mouth had to stay shut, and the model reads \
         one vocabulary.",
    )
}

/// What `[Shot N]` costs, and how to time a moment WITHOUT spending one.
///
/// The timing-RESOLUTION guidance below — how finely the model can place an event — is a working
/// rule rather than a claim about cut semantics, and has not been re-measured on ComfyUI.
pub fn shot_and_beat_facts(fixed_count: bool) -> String {
    let mut s = String::from(
        "\n\nHOW TO TIME A MOMENT WITHOUT SPENDING A CUT. `[Shot N]` is a CUT — the guide above \
         defines the tag and its timestamp as the moment the picture CHANGES, and this engine \
         renders it that way whatever words follow it. So open a `[Shot N]` only where you \
         genuinely want the framing to change.\n\nA moment that merely HAPPENS at a time is not a \
         shot. Write it as a clause in the running prose of the shot it happens in, opening with \
         its time:\n  `At 00:05.979, the robot fires a bolt from each eye.`\nThe camera keeps \
         rolling, the action lands on that time, and no cut is spent. A 15-second generation where \
         something lands at 5 s and something else at 10 s is ONE `[Shot 1]` carrying two such \
         clauses — not three shots. Write the moment as something ARRIVED AT rather than something \
         that starts: the approach occupies the window before the time, so it reads as landing ON \
         it rather than beginning there.\n\n🔴 TIMING CANNOT RESOLVE FINER THAN ABOUT A SECOND, so \
         do not spend a clause trying. Two moments less than about a second apart are ONE moment — \
         one timed clause at the time of the FIRST, with the rest carried as prose inside it: \
         \"fires a bolt to the left, then instantly a second to the right, one on each of the two \
         hammer hits\". A fast run of events — a drum fill, a burst of shots, a stumble — is ONE \
         clause describing the run, never one per event. This matters most against music, where \
         the interesting hits are often a fraction of a second apart.\n\n🔴 THE OPPOSITE MISTAKE \
         COSTS MORE: DO NOT CROWD ONE TIMED CLAUSE WITH A WHOLE SEQUENCE. A clause that has to \
         stage many separate things does not run them in order — it smears them, and the one you \
         cared about is the casualty. Measured: a mark at 00:11.508 carrying eight events in the \
         2.87 s of shot left put the first of them EIGHT SECONDS early and turned its arrival into \
         a slow fade; giving the arrival its own mark made it land hard. So: two things closer than \
         a second are one clause, and a clause with more in it than its span can hold must be split \
         or thinned. Count the seconds you have left before writing one near the end of a shot.\n\n\
         PUT EACH `[Shot N]` ON ITS OWN LINE, and start a new line at each timed clause too. This \
         is for the AUTHOR: the prompt is the document they edit by hand, and a wall of \
         run-together tags is where a mistimed one hides.\n\nNUMBER SHOTS CONSECUTIVELY, AND \
         NEVER REPEAT A NUMBER. The number is a POSITION, not a label: the first is `[Shot 1]`, the \
         one after it `[Shot 2]`, and so on, with no gaps and no repeats. 🔴 Writing `[Shot 1]` \
         again for a later moment is silent and fatal: a repeated number reads as the same shot, so \
         everything after the first collapses into it.",
    );
    if fixed_count {
        s.push_str(
            " 🔴 THE AUTHOR'S DIVISIONS ARE THE CUTS, AND THEY ARE THE ONLY ONES. THE CUTS above \
             says where the author wants the FRAMING to change: honour those in order, so the \
             render cuts where they cut, and open a `[Shot N]` for each. Do NOT add shots beyond \
             them to time things — every extra `[Shot N]` is an extra cut the author did not ask \
             for. Time the rest with clauses inside the shot they belong to.",
        );
    } else {
        s.push_str(
            " The author has placed no divisions, so you choose how many cuts there are. Let each \
             one earn itself: if the framing does not need to change, do not open a new shot.",
        );
    }
    s
}

/// The batch form of [`GENERATION_BOUNDARY_FACTS`], for a path writing MANY generations in one call
/// (script / rework / extend / chat). Composed from it so the two can never drift.
pub fn multi_generation_pacing() -> String {
    format!(
        "{GENERATION_BOUNDARY_FACTS} Each entry you write is one GENERATION with its own length, and \
         gets its own complete prompt — its own soundscape, its own score, its own shot list. Pace \
         each one to its OWN length. Two consecutive generations do not share a soundtrack, so a bed \
         that should feel continuous has to be described in the SAME words in both, and no line of \
         dialogue may be left to finish in the next one."
    )
}

/// The per-line annotation for a batch listing, e.g. `6.2s`. Empty for an unknown length so the
/// caller can omit the brackets entirely rather than print `[]`.
pub fn shot_length_tag(seconds: Option<f64>) -> String {
    match seconds.filter(|s| *s > 0.0) {
        Some(s) => format!("{s:.1}s"),
        None => String::new(),
    }
}

// ─── How to read the author ───────────────────────────────────────────────────

/// 🔴 **The clause this whole stage exists for.**
///
/// The previous design had exactly one legal way in — our editor's form — so every other producer of
/// a prompt had to be squeezed through it or rejected, and the writer was asked to fill in a JSON
/// shape rather than write. An author who typed one line got a schema violation; an author who typed
/// a paragraph got it echoed back as the prompt.
///
/// The author's text is INTENT. It arrives in whatever form they wrote it and the writer's job is to
/// read it for what they WANT and say that in the guide's schema.
pub const AUTHOR_INTENT_CLAUSE: &str = "\n\nHOW TO READ THE AUTHOR'S TEXT — everything in the \
request below is INTENT, in whatever form the author happened to write it: a one-line idea, a \
paragraph of prose, notes, or a half-filled schema they started and abandoned. Read it for what they \
WANT and express that in the guide's schema. Do not echo it back as the prompt, do not quote it, and \
never refuse it or complain about its shape — an unstructured brief is the normal case, not an \
error. Where the author is explicit, the author decides; where the author is silent, the guide's \
craft decides and you fill the gap with something concrete rather than leaving it vague. Do not \
invent a reference that is not on the roster, and do not drop one that is.";

// ─── The output contract ──────────────────────────────────────────────────────

/// The sentinel that separates the block from an optional one-line gist.
///
/// 🔴 A line of its own, and deliberately not JSON. Asking a local model to escape a multi-line block
/// inside a JSON string is the single most reliable way to get a mangled prompt back — the old
/// contract did exactly that and the escaping is what it got wrong. A sentinel line cannot collide
/// with a section header (`name:`), a shot tag (`[Shot N]`) or an alignment line.
pub const GIST_SEPARATOR: &str = "===GIST===";

/// The sentinel between two generations' blocks on a batch path. Same reasoning as
/// [`GIST_SEPARATOR`]: text out, text back.
pub const GENERATION_SEPARATOR: &str = "===GENERATION===";

/// **Reply with the block itself.** One generation.
///
/// 🔴 This replaces the old JSON reply contract, and it is the point of the reversal: the model now
/// writes the artifact, and koblem parses it (`web/src/lib/director/h3-block.ts`) instead of
/// assembling it out of fields the model was made to invent. The guide's own worked examples — every
/// one of which is a finished block — are the shape being asked for, so there is nothing left to
/// describe.
///
/// The section *layout* is restated in one sentence per checkpoint, and only because it is the one
/// place the two schemas differ mechanically rather than in craft: Ref2VA writes `name:` then a
/// newline, the FL2VA family writes `name: ` inline. Both are shown in the guide's examples; the
/// sentence is a pointer, not a second authority.
pub fn block_contract(checkpoint: H3Checkpoint, want_gist: bool) -> String {
    let layout = match checkpoint {
        H3Checkpoint::Ref2va => {
            "the six sections of the guide's §1 table, in that order — `subject_definitions:`, \
             `summary:`, `retention_analysis:`, `detailed_description:`, `overall_soundscape:`, \
             `non_diegetic_music:` — each header on a line of its own with its body starting on the \
             next line, one blank line between sections, exactly as the guide's complete example in \
             §7 lays it out"
        }
        H3Checkpoint::Fl2vaFamily => {
            "the three core fields of the guide's §2.2, in that order — \
             `integrated_multimodal_description:`, `overall_soundscape:`, `non_diegetic_music:` — \
             each written `name: ` with its body on the SAME line, one blank line between sections, \
             exactly as the guide's cases in §5 lay them out. When an instruction line is required \
             (§2.1) it is the FIRST line of the reply with one blank line after it"
        }
    };
    let mut s = format!(
        "\n\nWHAT TO REPLY WITH — the finished prompt itself and NOTHING else. Not JSON, not a \
         code fence, no preamble, no explanation, no commentary after it. Reply with {layout}.\n\n\
         You write the `[Shot N]` tags and their `At MM:SS.mmm,` times yourself; nothing \
         downstream adds them, and `[Shot 1]` carries no timestamp. Keep the whole prompt under \
         {PROMPT_CHAR_CAP} characters — that is a hard serving limit and an over-long prompt is \
         REJECTED, not truncated."
    );
    if want_gist {
        s.push_str(&format!(
            "\n\nAfter the prompt, on a line of its own, write exactly `{GIST_SEPARATOR}`, and on the \
             line after that ONE plain-language sentence saying what happens — no camera, lens or \
             lighting jargon. That sentence labels the shot in the author's timeline and is not part \
             of the prompt. Nothing follows it."
        ));
    }
    s
}

/// **Reply with N blocks.** The batch form, for the script writer.
///
/// The blocks are separated by [`GENERATION_SEPARATOR`] on its own line rather than carried in a
/// JSON array — see that constant for why. `parse_generations` still accepts the JSON carriers a
/// koblem of the previous generation asked for, so a rollout has both live.
pub fn block_contract_many(checkpoint: H3Checkpoint, n: u32) -> String {
    let contract = block_contract(checkpoint, false);
    format!(
        "{contract}\n\nThere are {n} generations to write, so reply with {n} complete prompts in \
         story order, separated by a line containing exactly `{GENERATION_SEPARATOR}` and nothing \
         else. Each prompt is complete on its own — its own sections, its own soundscape, its \
         own score. No numbering, no titles, no text outside the prompts and the separator lines."
    )
}

// ─── Reading the reply ────────────────────────────────────────────────────────

/// Strip a code fence and trim. **Nothing else.**
///
/// 🔴 Do NOT route an H3 block through `ltx_prompting::clean_prompt`. That function drops a short
/// leading line ending in a colon (it is looking for *"Here is the prompt:"*), and the first line of
/// a Ref2VA block is `subject_definitions:` — 20 characters, ends in a colon. It would eat the
/// section header and everything the block says about its references, silently, leaving a prompt
/// that still renders.
pub fn clean_block(text: &str) -> String {
    let mut t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // Drop the opening fence (and an optional language tag on its line) + the closing fence.
        let rest = rest.split_once('\n').map(|x| x.1).unwrap_or(rest);
        t = rest.trim_end_matches("```").trim();
    }
    t.to_string()
}

/// Does this text carry one of the two main body fields, i.e. is it a block at all?
///
/// The same test `h3-block.ts::looksLikeH3Block` makes on the client, and for the same reason: the
/// section set decides the schema, so the main field's presence decides whether there is a block
/// here or merely prose.
pub fn looks_like_block(text: &str) -> bool {
    text.contains("detailed_description:") || text.contains("integrated_multimodal_description:")
}

/// Split a reply into the block and the optional gist that follows [`GIST_SEPARATOR`].
///
/// Returns the whole (cleaned) text as the block when there is no sentinel, so a model that ignored
/// the gist request still returns a usable prompt.
pub fn split_gist(text: &str) -> (String, Option<String>) {
    let cleaned = clean_block(text);
    let Some(pos) = cleaned.find(GIST_SEPARATOR) else {
        return (cleaned, None);
    };
    let block = cleaned[..pos].trim_end().to_string();
    let gist = cleaned[pos + GIST_SEPARATOR.len()..].trim();
    let gist = gist
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string);
    (block, gist)
}

/// Pull N generations out of a batch reply, in order.
///
/// Three carriers, tried in this order, and **all three stay live** — a rollout has a new koblem
/// talking to a model prompted by an old one and vice versa, and the fallbacks are the difference
/// between degrading and losing the whole reply:
///
/// 1. **blocks separated by [`GENERATION_SEPARATOR`]** — what this module now asks for;
/// 2. `{"generations":[…]}` / `{"shots":[…]}` — the previous JSON envelope, whose entries may be
///    strings (blocks) or objects (the retired structured draft);
/// 3. a bare JSON array of the same.
pub fn parse_generations(text: &str, n: usize) -> Vec<String> {
    let cleaned = clean_block(text);
    // (1) The text carrier. Gated on the reply actually being a block and not a JSON document that
    // happens to quote a section name inside a string.
    if !cleaned.starts_with('{') && !cleaned.starts_with('[') && looks_like_block(&cleaned) {
        return cleaned
            .split(GENERATION_SEPARATOR)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .take(n)
            .map(str::to_string)
            .collect();
    }
    // (2)/(3) The JSON carriers. A bare array has to be tried FIRST: `extract_json_object` looks for
    // the first `{`, which in `[{…},{…}]` is the first element, so routing an array through it would
    // silently return one generation out of many.
    let val = if cleaned.starts_with('[') {
        serde_json::from_str::<Value>(&cleaned).ok()
    } else {
        serde_json::from_str::<Value>(crate::routes::ltx_prompting::extract_json_object(text)).ok()
    };
    let Some(val) = val else {
        return Vec::new();
    };
    let arr = match &val {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o
            .get("generations")
            .or_else(|| o.get("shots"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    arr.iter().filter_map(generation_text).take(n).collect()
}

/// One generation as text: a string passes through, an object is re-serialised compactly.
///
/// The object arm is the retired structured draft, kept because a saved Director document and an
/// older koblem both still produce it and the client still parses it
/// (`h3-llm.ts::h3DraftFrom` carrier 2).
pub fn generation_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Value::Object(_) => serde_json::to_string(v).ok(),
        _ => None,
    }
}

// ─── Assembled system prompts ─────────────────────────────────────────────────

/// Everything about one GENERATION that shapes its system prompt.
///
/// `new` gives the neutral generation, so a caller (and every test) states only what it varies.
pub struct H3GenerationPrompt<'a> {
    pub inputs: H3Inputs,
    /// The SNAPPED duration this GENERATION renders at — not the authored one, and not one of its
    /// clips' lengths either. See [`render_facts`] for what each mistake costs.
    pub seconds: Option<f64>,
    /// How many `[Shot N]` blocks this generation holds, when the caller has already packed it.
    /// `None` ⇒ the writer chooses the cuts.
    pub shot_count: Option<u32>,
    /// 0-based index, within this generation, of the clip the author is editing.
    pub shot_focus: Option<u32>,
    /// e.g. `16:9 landscape`. Formatted by the caller from the request geometry.
    pub aspect: Option<&'a str>,
    /// Every reference for this generation, in the order the runner will add them.
    pub refs: &'a [H3RefBrief],
    pub cast: &'a [LtxCastMember],
    pub speech_in_shot: Option<bool>,
    /// Where the song's voice falls inside THIS generation, in LOCAL seconds.
    ///
    /// 🔴 Strictly better information than `speech_in_shot`, and it supersedes it — see
    /// [`audio_and_text_facts`]. `None` keeps the boolean's behaviour, so a client that predates
    /// the lane is unchanged.
    pub vocal_lane: Option<&'a H3VocalLane>,
    /// Does anything appear on screen as text (titles, credits, UI, a sign the design calls for)?
    pub on_screen_text: bool,
    /// Ask for a one-line gist after the block. See [`GIST_SEPARATOR`].
    pub want_gist: bool,
}

impl<'a> H3GenerationPrompt<'a> {
    pub fn new(inputs: H3Inputs) -> Self {
        Self {
            inputs,
            seconds: None,
            shot_count: None,
            shot_focus: None,
            aspect: None,
            refs: &[],
            cast: &[],
            speech_in_shot: None,
            vocal_lane: None,
            on_screen_text: false,
            want_gist: false,
        }
    }
}

/// The one-line job description that opens every writer prompt, before the guide.
fn writer_preamble() -> &'static str {
    "You write prompts for the MiniMax-H3 video model, which renders picture and its synchronised \
     stereo soundtrack together in one pass.\n\n"
}

/// Which checkpoint, which shape, and (on Ref2VA) which task types the roles imply.
///
/// A fact block, not a rule block: the guide explains what each of these means and this says which
/// one this request is.
fn request_shape_facts(plan: &H3Plan) -> String {
    let mut s = format!(
        "\n\nTHIS REQUEST is `task: \"{task}\"`, served by the {name} checkpoint — the guide above \
         is that checkpoint's guide and its body field is `{field}:`.",
        task = plan.wire_task,
        name = match plan.checkpoint {
            H3Checkpoint::Ref2va => "Ref2VA",
            H3Checkpoint::Fl2vaFamily => "FL2VA",
        },
        field = plan.checkpoint.main_field(),
    );
    match plan.checkpoint {
        H3Checkpoint::Ref2va => {
            if !plan.task_types.is_empty() {
                s.push_str(&format!(
                    " The roles declared in the roster below read, under the guide's §3 table, as \
                     {prefix} — use that as the `summary:` prefix unless the table plainly says \
                     otherwise.",
                    prefix = plan.summary_prefix(),
                ));
            }
        }
        H3Checkpoint::Fl2vaFamily => {
            s.push_str(&format!(
                " In the guide's terms this is the {variant} shape, so follow that variant's \
                 instruction line and body strategy.",
                variant = plan.variant.as_str(),
            ));
        }
    }
    s
}

/// System prompt for the single-generation writer, H3 arm of `ltx-video/prompt-from-llm`.
///
/// Order is: the job, the guide, the facts, how to read the author, what to reply with. The reply
/// contract is last because the model follows a system prompt's shape — putting it first is how a
/// writer ends up leading with the contract instead of the prompt.
pub fn shot_system_prompt(p: H3GenerationPrompt<'_>) -> String {
    let plan = p.inputs.resolve();
    let mut s = String::from(writer_preamble());
    s.push_str(&guide_block(plan.checkpoint));
    s.push_str(&request_shape_facts(&plan));
    s.push_str(&render_facts(p.seconds, p.aspect));
    s.push_str(&reference_roster_clause(p.refs, plan.checkpoint));
    s.push_str(&cast_roster_clause(p.cast));
    s.push_str(&cut_plan_facts(p.shot_count, p.shot_focus));
    // `Option<&T>` → a 0-or-1 slice, so the lane's emitter has ONE input shape across all three
    // writer paths and the batch/single wordings cannot drift apart.
    s.push_str(&audio_and_text_facts(
        p.speech_in_shot,
        p.on_screen_text,
        p.vocal_lane.map(std::slice::from_ref).unwrap_or(&[]),
    ));
    s.push_str(GENERATION_BOUNDARY_FACTS);
    // 🔴 The one surface whose reply is mapped back onto the author's clips BY POSITION, so the one
    // surface that must not be offered an EXTRA block. It is still offered the beat — spent on a
    // division the author already placed, which changes no count. See `shot_and_beat_facts`.
    //
    // The gate is driven by the packed count rather than a caller flag, so it cannot be set without
    // the pack that makes it necessary. `Some(0)` is not a pack: it reaches `cut_plan_facts` as "the
    // author placed none", and the two must agree or the writer is told to return exactly zero
    // blocks and to spend one of them.
    s.push_str(&shot_and_beat_facts(p.shot_count.is_some_and(|n| n > 0)));
    s.push_str(&unspecified_span_facts());
    s.push_str(&current_date_clause());
    s.push_str(AUTHOR_INTENT_CLAUSE);
    s.push_str(&block_contract(plan.checkpoint, p.want_gist));
    s
}

/// System prompt for the multi-generation script writer, H3 arm of `ltx-video/script-from-llm`.
///
/// The unit is the GENERATION: a three-cut sequence that fits in 15 seconds is ONE entry here, not
/// three. No shot count is passed, and none is invented — this writer is deciding where the cuts
/// fall as it breaks the brief up.
///
/// 🔴 **`lanes` is why this signature grew.** Until 2026-08-14 this path called
/// [`audio_and_text_facts`] not at all — the writer that lays out a WHOLE timeline of generations
/// was the one writer never told whether anyone sings, and it is the one that decides the standing
/// `<Audio N>` sentence for every generation at once. One lane per generation, in render order.
pub fn script_system_prompt(
    n: u32,
    seconds: Option<f64>,
    aspect: Option<&str>,
    refs: &[H3RefBrief],
    cast: &[LtxCastMember],
    lanes: &[H3VocalLane],
) -> String {
    let plan = H3Inputs::from_refs(refs, false, false).resolve();
    let mut s = String::from(writer_preamble());
    s.push_str(&guide_block(plan.checkpoint));
    s.push_str(&request_shape_facts(&plan));
    s.push_str(&format!(
        "\n\nTHE JOB — break the brief into {n} GENERATIONS that tell the story in order. A \
         generation is one render of {MIN_GENERATION_SECONDS:.0}-{MAX_GENERATION_SECONDS:.0} \
         seconds and may hold SEVERAL `[Shot N]` shots with cuts between them, so a sequence that \
         fits inside one generation's duration is one entry with several shots — never several \
         entries."
    ));
    s.push_str(&render_facts(seconds, aspect));
    s.push_str(&reference_roster_clause(refs, plan.checkpoint));
    s.push_str(&cast_roster_clause(cast));
    s.push_str(&multi_generation_pacing());
    // No `speech_in_shot` here and there never was one: this path writes N generations and the flag
    // is per generation, so a single boolean could only ever have been wrong for most of them. The
    // lane is per generation by construction, which is what makes it sayable at all here.
    s.push_str(&audio_and_text_facts(None, false, lanes));
    // This writer is deciding where the cuts fall as it breaks the brief up, so it owns the cut list
    // in every generation it writes and beats are free to it.
    s.push_str(&shot_and_beat_facts(false));
    s.push_str(&unspecified_span_facts());
    s.push_str(&current_date_clause());
    s.push_str(AUTHOR_INTENT_CLAUSE);
    s.push_str(&block_contract_many(plan.checkpoint, n));
    s
}

/// The whole-script rework system prompt, H3 arm — the complete prompt, guide included.
///
/// ⚠️ Assembled here rather than at the call site. It used to be six `push_str` calls in
/// `ltx_director.rs`, which meant the ORDER of the H3 craft lived in a file that knows nothing about
/// H3 — and the order is load-bearing now that the guide has to come first.
///
/// 🔴 `lanes` (one per generation, render order) for the same reason [`script_system_prompt`] takes
/// them: a rework rewrites EVERY block in the project in one call, so it is the fastest way to
/// reintroduce a mimed vocal into a generation somebody had already fixed by hand.
pub fn rework_system(
    checkpoint: H3Checkpoint,
    refs: &[H3RefBrief],
    cast: &[LtxCastMember],
    lanes: &[H3VocalLane],
) -> String {
    let mut s = String::from(
        "You revise an existing multi-generation MiniMax-H3 video script to apply a director's \
         change while keeping it consistent.\n\n",
    );
    s.push_str(&guide_block(checkpoint));
    s.push_str(&format!(
        "\n\nTHIS SCRIPT is served by the {name} checkpoint, so every generation's body field is \
         `{field}:`.",
        name = match checkpoint {
            H3Checkpoint::Ref2va => "Ref2VA",
            H3Checkpoint::Fl2vaFamily => "FL2VA",
        },
        field = checkpoint.main_field(),
    ));
    s.push_str(&reference_roster_clause(refs, checkpoint));
    s.push_str(&cast_roster_clause(cast));
    s.push_str(&multi_generation_pacing());
    s.push_str(&audio_and_text_facts(None, false, lanes));
    // A rework returns each generation's WHOLE block keyed by id, so its shot list is not remapped
    // by position and a beat is available — but the clause immediately below tells it to keep the
    // shot count unless the change is about pacing, which is the constraint that actually binds here.
    s.push_str(&shot_and_beat_facts(false));
    s.push_str(&unspecified_span_facts());
    s.push_str(&current_date_clause());
    s.push_str(
        "\n\nFor EACH generation, rewrite its block so the change is reflected wherever it applies; \
         generations the change does not touch come back essentially as they were. Keep the SAME \
         number of generations in the SAME order, and keep each one's shot count and cut times \
         unless the change is about pacing. Every generation stays a COMPLETE block — never lose a \
         section, and never lose its audio direction, in a rewrite.",
    );
    s
}

/// The extend system prompt, H3 arm — the complete prompt, guide included.
pub fn extend_system(
    checkpoint: H3Checkpoint,
    refs: &[H3RefBrief],
    cast: &[LtxCastMember],
) -> String {
    let mut s = String::from(
        "You are a director continuing an existing multi-generation MiniMax-H3 video.\n\n",
    );
    s.push_str(&guide_block(checkpoint));
    s.push_str(&format!(
        "\n\nTHIS SCRIPT is served by the {name} checkpoint, so every generation's body field is \
         `{field}:`.",
        name = match checkpoint {
            H3Checkpoint::Ref2va => "Ref2VA",
            H3Checkpoint::Fl2vaFamily => "FL2VA",
        },
        field = checkpoint.main_field(),
    ));
    s.push_str(&reference_roster_clause(refs, checkpoint));
    s.push_str(&cast_roster_clause(cast));
    s.push_str(&multi_generation_pacing());
    // New generations, so this writer chooses every shot in them.
    s.push_str(&shot_and_beat_facts(false));
    s.push_str(&unspecified_span_facts());
    s.push_str(&current_date_clause());
    s.push_str(
        "\n\nWrite the NEXT generations, in the same world, tone and cast. Each one renders on its \
         own and starts from noise, so it must not open by finishing a sentence, a musical phrase \
         or a gesture that only the previous generation showed. Inside a generation you may cut as \
         often as the story earns it, and there a later timed clause MAY react to what an earlier \
         one showed.",
    );
    s
}

/// What a chat turn is looking at, and therefore what a `set_prompt` value IS.
///
/// 🔴 **The three states are not a style knob — they are three different destination FIELDS**, and
/// the whole history of this clause is what happens when the instruction and the destination
/// disagree. They are an enum rather than two booleans because the fourth combination (address a
/// render you were never shown) does not exist and must not be constructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H3ChatScope {
    /// No generation briefs at all — an older client. One line per cut, and every id is a cut id.
    Cuts,
    /// Briefs, but none carrying a generation id. The blocks are on screen to READ, and every id is
    /// still a cut id, so a `set_prompt` value is still one `[Shot N]` body.
    BlocksReadOnly,
    /// Briefs with generation ids. The id space IS the render, and a `set_prompt` value is that
    /// render's COMPLETE block.
    ///
    /// ⚠️ Both of the block-carrying arms answer [`review_system`]'s `shows_blocks` question with
    /// yes — they differ on what may be WRITTEN, not on what is shown.
    Blocks,
}

/// The H3 craft the chat surface needs, for a turn that may rewrite a prompt.
///
/// 🔴 **What a `set_prompt` value is depends entirely on the field it lands in**, and that is
/// [`H3ChatScope`]. This clause was retracted once (2026-08-08) for asserting a complete block on a
/// path whose action wrote `DirectorClip.prompt` — one `[Shot N]` body, which two independent places
/// define as such: `composeH3Prompt` nests it inside a `[Shot N]` tag when it seeds a block, and
/// `h3-llm.ts::isAssembledH3Block` exists **solely** to flag a whole block that has landed there.
/// The result was a seeded prompt with `summary:` and `overall_soundscape:` buried in the middle of
/// a shot description — a prompt that renders, and renders badly.
///
/// ⚠️ The retraction carried a pairing note: *if the client ever gains an action that targets the
/// GENERATION's block (it would need a generation id space), this clause has to change with it in
/// the same commit.* **This is that commit.** [`H3ChatScope::Blocks`] is that action, so under it
/// the clause flips back to a complete block — and the other two arms keep the retracted wording,
/// because their destination field did not move. Getting this backwards in either direction
/// produces the same shape of damage: a model told to write one shot's prose while the action
/// stores a whole block writes a block containing a fragment.
pub fn chat_craft_block(
    checkpoint: H3Checkpoint,
    refs: &[H3RefBrief],
    cast: &[LtxCastMember],
    scope: H3ChatScope,
) -> String {
    // The opening sentence has to name the UNIT correctly, because everything after it is read in
    // those terms. Under `Blocks` a shot IS the render and its prompt is the whole block; under the
    // other two a shot is one `[Shot N]` inside somebody else's block.
    let mut s = String::from(match scope {
        H3ChatScope::Blocks => {
            "Some turns rewrite a shot's prompt. On this engine ONE SHOT IS ONE RENDER, and its \
             prompt is one complete MiniMax-H3 schema block — so this is the model's own guide to \
             the exact artifact you are editing. Read it for the whole block, sections and all.\n\n"
        }
        H3ChatScope::Cuts | H3ChatScope::BlocksReadOnly => {
            "Some turns rewrite a shot's prompt. On this engine a prompt is a MiniMax-H3 schema \
             block and a shot is one `[Shot N]` inside it, so this is the model's own guide to both \
             — read it for the craft of a shot description, not only for the shape of a whole \
             block.\n\n"
        }
    });
    s.push_str(&guide_block(checkpoint));
    s.push_str(&reference_roster_clause(refs, checkpoint));
    // 🔴 Chat had no cast at all until 2026-08-08, so a turn asked to "make Mia's shot colder"
    // rewrote her out of her own appearance sentence — identity on this engine is carried by
    // repeating that sentence, and a paraphrase swaps the character out silently.
    s.push_str(&cast_roster_clause(cast));
    s.push_str(&multi_generation_pacing());
    // Chat READS blocks and — under `Blocks` — WRITES them, and both halves need this: it has the
    // guide's §4.2 in front of it, where every `[Shot N]` carries a transition expression, so an
    // unverbed one in the listing looks like an author who forgot a verb rather than a deliberate
    // beat.
    //
    // 🔴 `false` on every arm, and that is a judgement rather than an oversight. `fixed_count` says
    // "you were handed N divisions, spend those and never volunteer one", which is true of a BATCH
    // writer being given a cut plan and false here: chat is answering a user who may well be asking
    // for a moment to be added, and the block is the document, so adding a mark is a legitimate
    // edit. The count discipline chat does need is narrower — keep the marks unless the user asked
    // — and is stated in the `set_prompt` clause below, where the destination field is named.
    s.push_str(&shot_and_beat_facts(false));
    s.push_str(&unspecified_span_facts());
    match scope {
        // The listing carries blocks but the ids are still per cut, so the blocks are strictly
        // context: there is nowhere for a block the model writes to land.
        H3ChatScope::BlocksReadOnly => s.push_str(
            "\n\nWHAT THE PROMPT BLOCKS IN THE LISTING ARE — each `Shot N` heading is ONE RENDER, \
             and under it is the complete block that render will be sent, verbatim: every section, \
             every `[Shot N] At MM:SS.mmm` tag, every reference label. The `[Shot N]` tags INSIDE a \
             block are the cuts and beats of that render, and the heading lists their ids in order. \
             Read the blocks to answer questions about the video, and to keep any rewrite \
             consistent with the render it sits in — its duration, the shots either side of it, its \
             soundscape and its score. They are CONTEXT: nothing you reply with replaces a block.",
        ),
        // The same listing, but the heading's id names the RENDER and `set_prompt` lands on that
        // render's block. The blocks stop being context and become the thing being edited.
        H3ChatScope::Blocks => s.push_str(
            "\n\nWHAT THE PROMPT BLOCKS IN THE LISTING ARE — each `Shot N` heading is ONE RENDER \
             and its `id` names that render, and under the heading is the complete block that \
             render will be sent, verbatim: every section, every `[Shot N] At MM:SS.mmm` mark, \
             every reference label. That block is the WHOLE document for the shot — nothing else is \
             stored, and everything the editor shows about the shot is read back out of it. The \
             `[Shot N]` marks INSIDE a block are moments within that one render, not shots you can \
             name: they have no ids, and changing one means rewriting the block that holds it.",
        ),
        H3ChatScope::Cuts => {}
    }
    match scope {
        H3ChatScope::Cuts | H3ChatScope::BlocksReadOnly => s.push_str(&format!(
            "\n\n🔴 A `set_prompt` value on this engine is the prose of ONE `[Shot N]` — that \
             shot's description and nothing else. Not a whole block: no `{field}:` or any other \
             `name:` header, no `[Shot N]` tag, no `At MM:SS.mmm` time, no soundscape and no score. \
             Those belong to the render as a whole and are not this action's to write; the tag and \
             the time are added around your text. Write it in the guide's own style for a shot \
             description, addressing any reference by the exact label from the roster, and keep it \
             in proportion to the {cap}-character limit the whole block has to fit inside.",
            field = checkpoint.main_field(),
            cap = PROMPT_CHAR_CAP,
        )),
        // 🔴 The flip. `set_prompt` writes `DirectorH3Generation.prompt`, so a value that is only a
        // paragraph REPLACES the block with that paragraph — every section the author wrote, the
        // soundscape, the score and every other mark gone, silently, on a prompt that still renders.
        // "Repeat what you are not changing" is therefore the load-bearing sentence in this clause.
        H3ChatScope::Blocks => s.push_str(&format!(
            "\n\n🔴 A `set_prompt` value on this engine is the shot's COMPLETE block, exactly as it \
             would be sent: it REPLACES the whole prompt. Write the full section set this \
             checkpoint requires — its body field is `{field}:` — with every `name:` header, every \
             `[Shot N] At MM:SS.mmm` mark, the soundscape and the score, and repeat VERBATIM every \
             part you are not changing. Anything you leave out is deleted from the render. Keep \
             the marks the block already has unless the user asked for a moment to be added or \
             removed, because the timeline's cut line-up is derived from them. Address every \
             reference by the exact label from the roster, and \
             keep the whole block under {cap} characters — that is a hard serving limit.",
            field = checkpoint.main_field(),
            cap = PROMPT_CHAR_CAP,
        )),
    }
    s
}

/// The script-supervisor (review) system prompt, H3 arm.
///
/// ⚠️ Gets **both** guides, because a review request carries no references and therefore cannot know
/// which checkpoint the script is written for — and a reviewer handed only the six-section guide
/// would report every legal three-section generation as missing four sections. Roughly 10k tokens on
/// a call that already carries every shot; the alternative is a paraphrased checklist, which is the
/// thing this module stopped doing.
///
/// 🔴 **`shows_blocks` is not a style switch — it decides which questions are ANSWERABLE.** When the
/// caller sends the packed generations ([`H3GenerationBrief`]) the reviewer is looking at the
/// assembled blocks, so cut times, reference labels, retention lines and the body field are all in
/// front of it and worth hunting for. When it does not — a client that predates the field — the
/// reviewer sees one line per cut and NONE of those things exist in its input;
/// asking for them anyway is asking a model to report on text it was never shown, and the only way
/// it can comply is to invent. That was the live defect this parameter closes: the checklist was
/// unconditional and the listing never carried a schema.
pub fn review_system(shows_blocks: bool) -> String {
    // What the reviewer is actually looking at, and therefore what it may report on. Both arms name
    // the input shape exactly, because "find schema violations" against an input with no schema in
    // it is the invitation to confabulate.
    let input_shape = if shows_blocks {
        "\n\nWHAT YOU ARE SHOWN — the script as it will actually be sent. Each entry is headed \
         `Shot N`: that is ONE GENERATION, one render, and it is the word the editor shows its \
         author. The header gives the render's duration in seconds, the checkpoint it lands on, and \
         the ids of the CUTS inside it; under it is the complete prompt block for that render, \
         verbatim — every section, every `[Shot N] At MM:SS.mmm` cut tag, every reference label. The \
         `[Shot N]` tags INSIDE a block are those cuts, which is what the guides above call a shot; \
         this listing says \"cut\" wherever the two could be confused. Each header NAMES the body \
         field its checkpoint requires, so a block whose body field is the OTHER one is a real \
         finding. An entry marked NOT AUTHORED still shows the block that will render, but nobody \
         has written it — it is koblem's own assembly of the cut bodies, so judge its STORY and \
         leave its unfilled sections alone. An entry marked NOT WRITTEN YET has no block at all: \
         its author has written nothing, which is worth at most one finding saying so and never a \
         list of missing sections."
    } else {
        "\n\nWHAT YOU ARE SHOWN — one line per CUT: its id, its length, and whatever text its author \
         has written so far. 🔴 You are NOT shown the assembled prompt blocks, the generation \
         boundaries, the cut times, the reference roster or the retention lines. So report a schema \
         problem ONLY where you can see the text that has it, and never report a section, a cut \
         time, a label or a retention line as missing from something you were not shown. What you \
         can judge is the writing in front of you: vague or weak cuts, pacing, story clarity, and a \
         cut that depends on a frame or a sound the next render will not have."
    };
    // The schema checks. Every one of them reads the block, so they are only asked for when a block
    // is on screen.
    let checklist = if shows_blocks {
        "a cut time that is not strictly increasing, or that falls outside its render's duration \
         given in the header, or that leaves no room for the final cut; a reference label used that \
         was never defined; a defined label with no retention line; a retention marker taken from \
         the wrong vocabulary; a body field that disagrees with the checkpoint in its header; a line \
         of dialogue left to finish in the NEXT generation, which is the one place nothing carries; \
         and the ordinary problems — vague or weak cuts, pacing, story clarity."
    } else {
        "a cut that is vague or weak, pacing, story clarity, and a cut written as if it continued \
         something the previous render showed."
    };
    format!(
        "You are a script supervisor reviewing a multi-shot MiniMax-H3 video before it is rendered. \
         Below are MiniMax's own two prompt-writing guides. They are authoritative: a finding is \
         worth making when the script violates one of them, or one of the facts stated after \
         them.\n\n===== BEGIN VIDEO_PROMPT_WRITING_GUIDE_base_en.md =====\n{GUIDE_BASE}\n===== END \
         =====\n\n===== BEGIN VIDEO_PROMPT_WRITING_GUIDE_ref_en.md =====\n{GUIDE_REF}\n===== END \
         =====\n\nA generation is written for ONE of the two checkpoints and you can tell which from \
         its body field: `detailed_description:` is the six-section Ref2VA schema, \
         `integrated_multimodal_description:` the three-section FL2VA one. Judge each generation \
         against its own schema — a three-section generation is not missing four sections.\
         {boundary}{beats}{gaps}{date}{input_shape}\n\nFind concrete problems, and weight them to what \
         actually breaks: {checklist} Only report real, actionable problems. If the script is clean, \
         return an empty list; do not invent issues to fill space. 🔴 A `[Shot N]` that opens with \
         an action instead of a transition expression is a BEAT and is deliberate — never report a \
         missing cut verb, and never suggest turning a beat into a cut unless the FRAMING genuinely \
         has to change there.",
        boundary = GENERATION_BOUNDARY_FACTS,
        // 🔴 Without this the reviewer files a finding on every beat in the script. It is reading
        // MiniMax's §4.2, which shows a transition expression at every tag, so a verbless one looks
        // to it exactly like an author who forgot one — a wrong finding that is then one click from
        // rewriting a working shot.
        beats = shot_and_beat_facts(false),
        gaps = unspecified_span_facts(),
        date = current_date_clause(),
    )
}

/// The auto-director's system prompt — H3 arm of `ltx_director::director_system_prompt`.
///
/// 🔴 **No guide here, deliberately.** The planner writes one-line IDEAS and never a block, so
/// handing it 24 KB of prompt-writing craft would teach it to write finished prompts in a field
/// sized for four words. What it needs is koblem's own arithmetic: what a generation costs, what a
/// cut costs, and which references exist.
///
/// ⚠️ [`super::ltx_prompting::story_context_block`] is borrowed and stays borrowed: it is project
/// canon (backstory, tone, rules, named characters) with no prompt-shape craft in it at all, and
/// duplicating it here would give the project two canons to drift apart.
pub fn director_system_prompt(
    story: &StoryContext,
    refs: &[H3RefBrief],
    cast: &[LtxCastMember],
) -> String {
    let plan = H3Inputs::from_refs(refs, false, false).resolve();
    let mut system = String::from(
        "You are the director of an AI video generator driving MiniMax-H3. You cut the brief (and \
         the song, if there is one) into SHOTS and give each shot a one-line idea. You do NOT write \
         the finished prompts — a later pass expands each idea against the model's own writing \
         guide — so keep every idea light, concrete and expandable: a seed, not a paragraph.",
    );
    system.push_str(&current_date_clause());
    if !story.is_empty() {
        system.push_str("\n\n");
        system.push_str(&crate::routes::ltx_prompting::story_context_block(story));
    }
    system.push_str(&format!(
        "\n\nMiniMax-H3's unit is a GENERATION: one render of {MIN_GENERATION_SECONDS:.0} to \
         {MAX_GENERATION_SECONDS:.0} seconds that can contain SEVERAL shots with explicit cuts \
         between them, which the model renders itself. Inside one generation the shots share \
         everything — the same soundtrack, the same score, and a character carried across a cut by \
         plain prose — so a cut inside a generation is CHEAP. What is expensive is the GENERATION \
         BOUNDARY: a new generation restarts the soundtrack from noise, cannot carry a line of \
         dialogue across, and never joins frame-exactly. So group consecutive shots into as few \
         generations as the {MAX_GENERATION_SECONDS:.0}-second budget allows, and only start a new \
         one when the budget runs out or the story genuinely breaks to a new world, time or piece \
         of music."
    ));
    system.push_str(
        "\n\nGive every shot a short AUDIO INTENT alongside its idea — one phrase saying what it \
         sounds like (\"rain on the awning, no music\", \"low sub-bass pulse and one hit on the \
         title\", \"she says the line, close and dry\"). The expansion pass merges the intents of \
         all the shots in one generation into that generation's single soundscape and single score, \
         so shots inside one generation should not ask for contradictory beds.",
    );
    if !refs.is_empty() {
        let addresses = assign_ordinals(refs);
        system.push_str(&format!(
            "\n\nThe project has references the prompts will address by ordinal — these labels are \
             facts. This is a `{}` request. Say in each shot's idea which of them the shot is \
             ABOUT, using the names below:",
            plan.wire_task
        ));
        for a in &addresses {
            system.push_str(&format!("\n  - {} — {}", a.name, role_phrase(a.role)));
            if let Some(audio) = &a.audio_name {
                system.push_str(&format!(" (its soundtrack is {audio})"));
            }
            if let Some(label) = &a.label {
                system.push_str(&format!(" — \"{label}\""));
            }
        }
    }
    system.push_str(&cast_roster_clause(cast));
    // 🔴 This paragraph used to read "Never use beats: MiniMax-H3 has no Prompt Relay layout" and
    // then push a two-moment idea into two shots. The first half is still true — the JSON schema
    // below has no `beats` field and H3 has no Prompt Relay conditioning — but the conclusion drawn
    // from it was wrong, and it was the expensive half: a `[Shot N]` written without a transition
    // expression does not cut (measured; see `shot_and_beat_facts`), so a timed moment inside a
    // running shot IS available and a planner that splits every one of them off spends a cut on each.
    system.push_str(
        "\n\nDo not emit a \"beats\" field — the schema below has none. That is a limit on this \
         REPLY, not on the video: the writing pass can place a timed moment inside a shot with no \
         cut at all, so an idea naming two moments (\"she reads the note, then looks up\") is ONE \
         shot and needs no second one. Split into two shots only where the FRAMING should change.",
    );
    system.push_str(
        "\n\nYou plan the video in successive batches. Reply with STRICT JSON only, no prose: \
         {\"shots\":[{\"start_word\":<int>,\"idea\":\"<a terse 4-10 word one-liner naming the \
         shot>\",\"audio\":\"<a few words: what this shot sounds like>\",\"mode\":\"t2v\",\
         \"ref_image\":<int|null>,\"new_scene\":<bool>}],\
         \"summary\":\"<1-2 sentences recapping the story THROUGH the shots you just wrote, so the \
         next batch continues seamlessly>\"}. \"new_scene\" means THIS SHOT STARTS A NEW \
         GENERATION: set it true only when the previous shots have used up the 15-second budget or \
         the story breaks to a new world, time or piece of music, and false when the shot is a cut \
         inside the generation already in progress — which is the common case and the cheaper one. \
         An \"idea\" is a short human label like \"neon alley chase, rain\" — never a cinematic \
         paragraph, never camera or lighting jargon. Omit code fences.",
    );
    system
}

#[cfg(test)]
mod tests {

    /// 🔴 Pinned to the five defects it was written from, because the value is entirely in the
    /// specifics: a paraphrase of "be thorough" would survive any edit and teach nothing.
    #[test]
    fn the_gap_facts_name_what_actually_went_wrong() {
        let s = unspecified_span_facts();
        // The audit question, which is the whole idea.
        assert!(s.contains("audit for SILENCE"), "{s}");
        // A definition outranks a shot body — the one that cost four renders.
        assert!(s.contains("STANDING FACT"), "{s}");
        assert!(s.contains("from 00:08.467 onward"), "{s}");
        // Positive phrasing over negation, with the pair that was actually tried.
        assert!(s.contains("WRITE THE END STATE POSITIVELY"), "{s}");
        // The contradiction that put a face on the back of a head.
        assert!(s.contains("ABSURD SOLUTION"), "{s}");
        // The anatomy/scenery collision.
        assert!(s.contains("open mouth of the alley"), "{s}");
    }

    /// ⚠️ This module's rule is that nothing here restates a rule the guide states. These are the
    /// guide's own topics, and a copy of any of them here is the drift this test exists to catch.
    #[test]
    fn the_gap_facts_do_not_paraphrase_the_guide() {
        let s = unspecified_span_facts().to_lowercase();
        for owned_by_the_guide in [
            "350",
            "500 english words",
            "fully_preserved",
            "retention_analysis",
            "non_diegetic_music",
            "subject_definitions:",
        ] {
            assert!(
                !s.contains(owned_by_the_guide),
                "restates the guide: {owned_by_the_guide}"
            );
        }
    }

    use super::*;

    fn img(role: Option<H3ReferenceRole>, label: &str) -> H3RefBrief {
        H3RefBrief {
            kind: H3ReferenceKind::Image,
            has_audio: false,
            role,
            label: Some(label.into()),
            diegetic: None,
            soundtrack_mode: None,
        }
    }
    fn vid(has_audio: bool, role: Option<H3ReferenceRole>) -> H3RefBrief {
        H3RefBrief {
            kind: H3ReferenceKind::Video,
            has_audio,
            role,
            label: None,
            diegetic: None,
            soundtrack_mode: None,
        }
    }
    fn aud(role: Option<H3ReferenceRole>) -> H3RefBrief {
        H3RefBrief {
            kind: H3ReferenceKind::Audio,
            has_audio: true,
            role,
            label: None,
            diegetic: None,
            soundtrack_mode: None,
        }
    }

    /// The reused song, with the two facts the timeline knows and the guide does not.
    fn song(diegetic: bool, mode: H3SoundtrackMode) -> H3RefBrief {
        H3RefBrief {
            diegetic: Some(diegetic),
            soundtrack_mode: Some(mode),
            ..aud(Some(H3ReferenceRole::AudioReuse))
        }
    }

    /// 🔴 The regression this whole field exists for. Without the flag the writer files a reused
    /// song under `non_diegetic_music` — music the characters CANNOT hear — on a project whose
    /// character sings it, because that is what `GUIDE-ref:296` tells it to do and nothing said
    /// otherwise. The roster has to override the guide out loud, so assert the words that do it.
    #[test]
    fn diegetic_song_overrides_the_guides_default_section() {
        let on = reference_roster_clause(
            &[song(true, H3SoundtrackMode::Replace)],
            H3Checkpoint::Ref2va,
        );
        assert!(on.contains("CAN HEAR"), "{on}");
        assert!(
            on.contains("`non_diegetic_music:` must be exactly `N/A`"),
            "{on}"
        );
        assert!(on.contains("entire audible content"), "{on}");

        let off =
            reference_roster_clause(&[song(false, H3SoundtrackMode::Keep)], H3Checkpoint::Ref2va);
        assert!(off.contains("CANNOT hear"), "{off}");
        assert!(!off.contains("must be exactly `N/A`"), "{off}");
        assert!(!off.contains("entire audible content"), "{off}");

        // 🔴 Absent ⇒ NOT silent any more. An older client sends no flag and the guide's own
        // default then files the song as audience-only, so the writer is asked the question it can
        // actually answer: does anything in the scene react to this audio? It must not be handed
        // either flag's wording, because it has not been told either.
        let quiet = reference_roster_clause(
            &[aud(Some(H3ReferenceRole::AudioReuse))],
            H3Checkpoint::Ref2va,
        );
        assert!(!quiet.contains("CAN HEAR"), "{quiet}");
        assert!(!quiet.contains("CANNOT hear"), "{quiet}");
        assert!(!quiet.contains("entire audible content"), "{quiet}");
        assert!(
            quiet.contains("decide it from what you yourself write"),
            "{quiet}"
        );
        assert!(quiet.contains("then it is DIEGETIC"), "{quiet}");
        assert!(quiet.contains("contradicts itself"), "{quiet}");
    }

    /// 🔴 The recap defect: five of one project's fifteen generations opened by narrating the one
    /// before, and two of them RENDERED the recap, re-staging an event the timeline had passed.
    #[test]
    fn a_generation_is_told_never_to_narrate_the_one_before_it() {
        let s = GENERATION_BOUNDARY_FACTS;
        assert!(
            s.contains("NEVER WRITE A SENTENCE ABOUT A PREVIOUS GENERATION"),
            "{s}"
        );
        assert!(
            s.contains("the scene continues from the previous shot"),
            "{s}"
        );
        assert!(s.contains("Write the opening STATE instead"), "{s}");
        // The batch form carries it too, or the writer that lays out a whole script never sees it.
        assert!(multi_generation_pacing().contains("PREVIOUS GENERATION"));
    }

    /// A slice of a real song is often instrumental, and a standing "it sings the vocals" over one
    /// renders as a mouth flapping at nothing — measured as the most common defect on this engine.
    #[test]
    fn the_writer_is_told_a_vocal_is_not_continuous() {
        let s = unspecified_span_facts();
        assert!(s.contains("A SONG'S VOCAL IS NOT CONTINUOUS"), "{s}");
        assert!(s.contains("do NOT default to \"throughout\""), "{s}");
    }

    /// 🔴 This rule has been wrong TWICE in opposite directions — first too abstract (a held pose
    /// between accents, which rendered as a twitch), then too specified (full choreography, which
    /// rendered as overacting). Both failures are cheap to reintroduce by deleting half the clause,
    /// so all three parts are pinned: the floor, the CEILING, and the camera being the real lever.
    #[test]
    fn the_writer_is_told_a_dance_has_a_ceiling_and_the_camera_is_the_lever() {
        let s = unspecified_span_facts();
        // The floor: not one named step, not abstract quality prose.
        assert!(s.contains("A DANCE IS A VOCABULARY OF MOVES"), "{s}");
        assert!(s.contains("headbanging"), "{s}");
        assert!(s.contains("bopped"), "{s}");
        // The ceiling, which is the half that gets forgotten.
        assert!(s.contains("NOT MORE SPECIFICATION"), "{s}");
        assert!(s.contains("OVERACTED"), "{s}");
        assert!(s.contains("ONE plain sentence"), "{s}");
        // The camera. 🔴 This half was REWRITTEN 2026-08-16 and the old needles ("keep the camera
        // moving", "Static Shot") are deliberately gone: the engine will not choose a move for
        // itself, so naming one is a decision with a stability cost, not a default to reach for.
        assert!(s.contains("It does not self-direct"), "{s}");
        assert!(s.contains("naming one a DECISION"), "{s}");
        // The chain trap: a shot that ENDS on a framing locks the shot after it.
        assert!(
            s.contains("never write a shot's ending as a framing"),
            "{s}"
        );
        // Marks are for story, not choreography.
        assert!(s.contains("adds NOTHING to how much a body moves"), "{s}");
        // The brake words. If these stop being named the clause loses its teeth.
        for brake in ["heavy", "weighted", "feet planted", "head level"] {
            assert!(s.contains(brake), "{brake} missing: {s}");
        }
    }

    // ── The vendored guides ───────────────────────────────────────────────────

    /// 🔴 The guides are the authority, so their PRESENCE is the thing to pin. A build that silently
    /// shipped an empty asset would produce prompts that are pure facts with no craft at all, and
    /// every one of them would still render something.
    #[test]
    fn the_guides_are_vendored_and_whole() {
        // Vendored 2026-08-08 at 15,773 B / 23,553 B. The bound is loose so an upstream edit is not
        // a test failure, and tight enough that a truncated or empty asset is.
        assert!(GUIDE_BASE.len() > 15_000, "base guide looks truncated");
        assert!(GUIDE_REF.len() > 23_000, "ref guide looks truncated");
        // The section names the whole schema keys on, each in its own guide and not the other.
        assert!(GUIDE_BASE.contains("integrated_multimodal_description"));
        assert!(GUIDE_REF.contains("detailed_description"));
        assert!(GUIDE_REF.contains("retention_analysis"));
    }

    /// The checkpoint chooses the guide, and choosing wrong names the wrong body field.
    #[test]
    fn the_checkpoint_chooses_the_guide() {
        let (ref_name, ref_text) = guide(H3Checkpoint::Ref2va);
        assert!(ref_name.contains("ref_en"));
        assert_eq!(ref_text, GUIDE_REF);
        let (base_name, base_text) = guide(H3Checkpoint::Fl2vaFamily);
        assert!(base_name.contains("base_en"));
        assert_eq!(base_text, GUIDE_BASE);
    }

    /// The introduction is load-bearing: it is what makes every fact after it subordinate.
    #[test]
    fn the_guide_is_introduced_as_authoritative() {
        let b = guide_block(H3Checkpoint::Ref2va);
        assert!(b.contains("It is authoritative"));
        assert!(b.contains("the guide wins"));
        assert!(b.contains(GUIDE_REF));
    }

    // ── Ordinals ──────────────────────────────────────────────────────────────

    /// 🔴 The brackets are part of the prompt (`text_encoder.py:137`, `REQ-fl2va:13`). An earlier
    /// version emitted `Picture 1`, which matches nothing the conditioner said.
    #[test]
    fn ordinals_carry_the_angle_brackets() {
        let refs = [img(None, "hero")];
        assert_eq!(assign_ordinals(&refs)[0].name, "<Picture 1>");
    }

    /// Ordinals count PER KIND (`GUIDE-ref:110`), and the pipeline adds images FIRST regardless of
    /// the order the editor listed them in (`pipeline.py:311-340`) — so the image is `<Picture 1>`
    /// even though it is third in the list.
    #[test]
    fn ordinals_follow_pipeline_add_order_not_list_order() {
        let refs = [vid(false, None), aud(None), img(None, "hero")];
        let out = assign_ordinals(&refs);
        assert_eq!(out[0].name, "<Video 1>");
        // The video claims no audio ordinal (its soundtrack is not passed), so the standalone clip
        // is `<Audio 1>` and not `<Audio 2>`.
        assert_eq!(out[1].name, "<Audio 1>");
        assert_eq!(out[2].name, "<Picture 1>");
    }

    /// A video whose soundtrack is enabled answers to two labels, and its `<Audio k>` is allocated
    /// BEFORE its own `<Video k>` (`pipeline.py:250` then `:253`) — which is why the served prompt
    /// reads `<Audio 1> is the synchronized audio track of <Video 1>` with the standalone clip at
    /// `<Audio 2>` (`REQ-ref2va:13`).
    #[test]
    fn a_soundtracked_video_takes_its_audio_ordinal_first() {
        let refs = [vid(true, Some(H3ReferenceRole::EditSource)), aud(None)];
        let out = assign_ordinals(&refs);
        assert_eq!(out[0].name, "<Video 1>");
        assert_eq!(out[0].audio_name.as_deref(), Some("<Audio 1>"));
        assert_eq!(out[1].name, "<Audio 2>");
        assert_eq!(out[1].audio_name, None);
    }

    /// An ordinary reference video does NOT create an `<Audio N>` just because the file has sound
    /// (`GUIDE-ref:112`) — `has_audio` means "its soundtrack is being passed".
    #[test]
    fn a_video_without_an_enabled_soundtrack_takes_no_audio_ordinal() {
        let refs = [vid(false, None), aud(None)];
        let out = assign_ordinals(&refs);
        assert_eq!(out[0].audio_name, None);
        assert_eq!(out[1].name, "<Audio 1>");
    }

    // ── The roster ────────────────────────────────────────────────────────────

    /// The roster states the ordinal, the role and the author's own name, and forbids inventing a
    /// label — the three things the guide cannot know.
    #[test]
    fn the_roster_states_the_facts_only_koblem_has() {
        let refs = [
            img(Some(H3ReferenceRole::IdentitySource), "Mia"),
            aud(Some(H3ReferenceRole::VoiceTimbre)),
        ];
        let s = reference_roster_clause(&refs, H3Checkpoint::Ref2va);
        assert!(s.contains("<Picture 1>"), "{s}");
        assert!(s.contains("the author calls this \"Mia\""), "{s}");
        assert!(s.contains("<Audio 1>"), "{s}");
        assert!(s.contains("Never invent a label"), "{s}");
        // The retention parenthetical is our role vocabulary, not the guide's line format.
        // 🔴 It must NOT name a shot: koblem does not know where the subject appears.
        assert!(s.contains("(appears in the video)"), "{s}");
        assert!(!s.contains("appears in [Shot 1]"), "{s}");
    }

    /// 🔴 The song is a SLICE of one project track, not a separate piece of music — the one audio
    /// fact that changes how the score is written and the one nothing else can supply.
    #[test]
    fn the_soundtrack_slice_says_it_is_a_slice() {
        let s = reference_roster_clause(
            &[aud(Some(H3ReferenceRole::AudioReuse))],
            H3Checkpoint::Ref2va,
        );
        assert!(s.contains("slice of the project's own soundtrack"), "{s}");
        // A voice-timbre clip is NOT the song and must not claim to be.
        let v = reference_roster_clause(
            &[aud(Some(H3ReferenceRole::VoiceTimbre))],
            H3Checkpoint::Ref2va,
        );
        assert!(!v.contains("slice of the project's own soundtrack"), "{v}");
    }

    /// 🔴 An identity image has NO retention line of its own, because it has no entry of its own
    /// (`GUIDE-ref:68`) — it is cited inside the `<Subject N>` it establishes, and the SUBJECT is
    /// what `retention_analysis` records. The roster used to offer `(appears in [Shot 1])` for a
    /// `<Picture N>` line that must not exist.
    #[test]
    fn an_identity_image_is_defined_inside_a_subject_not_as_its_own_entry() {
        let id = reference_roster_clause(
            &[img(Some(H3ReferenceRole::IdentitySource), "Mia")],
            H3Checkpoint::Ref2va,
        );
        assert!(id.contains("it gets no entry of its own"), "{id}");
        assert!(id.contains("that `<Subject N>`, not this label"), "{id}");
        assert!(id.contains("(appears in the video)"), "{id}");

        // A frame pin, a storyboard and a video are all standalone entries and DO carry the line.
        for role in [
            H3ReferenceRole::FirstFrame,
            H3ReferenceRole::Storyboard,
            H3ReferenceRole::FrameAnchor,
        ] {
            let s = reference_roster_clause(&[img(Some(role), "x")], H3Checkpoint::Ref2va);
            assert!(s.contains("it takes an entry of its own"), "{role:?}: {s}");
            assert!(!s.contains("no entry of its own"), "{role:?}: {s}");
        }
        let v = reference_roster_clause(
            &[vid(false, Some(H3ReferenceRole::StructureSource))],
            H3Checkpoint::Ref2va,
        );
        assert!(v.contains("(cut and pacing structure)"), "{v}");

        // Audio lines carry no parenthetical at all (`GUIDE-ref:200-204`), so neither arm fires.
        let a = reference_roster_clause(
            &[aud(Some(H3ReferenceRole::VoiceTimbre))],
            H3Checkpoint::Ref2va,
        );
        assert!(!a.contains("entry of its own"), "{a}");

        // ⚠️ The FL2VA family has no `subject_definitions` and no `retention_analysis`, so it must
        // be told none of this — it would be describing sections its schema does not have.
        let base = reference_roster_clause(
            &[img(Some(H3ReferenceRole::IdentitySource), "Mia")],
            H3Checkpoint::Fl2vaFamily,
        );
        assert!(!base.contains("entry of its own"), "{base}");
        assert!(base.contains("<Picture 1>"), "{base}");
    }

    /// A generation with no references must not be told to address any — the engine sees none
    /// either, so the two agree.
    #[test]
    fn no_refs_means_no_roster_clause() {
        assert!(reference_roster_clause(&[], H3Checkpoint::Ref2va).is_empty());
    }

    // ── Checkpoint selection and the task types ───────────────────────────────

    fn prefix(i: H3Inputs) -> String {
        i.resolve().summary_prefix()
    }

    #[test]
    fn checkpoint_selection_follows_the_assets() {
        // Nothing at all → t2va on the FL2VA checkpoint, and no summary section to prefix.
        let t = H3Inputs::default().resolve();
        assert_eq!(t.checkpoint, H3Checkpoint::Fl2vaFamily);
        assert_eq!(t.wire_task, "t2va");
        assert_eq!(t.variant, Fl2vaVariant::T2va);
        assert!(t.task_types.is_empty());
        assert!(t.summary_prefix().is_empty());

        // A frame pin alone stays on the FL2VA checkpoint — there is no `i2va` on the wire.
        let i = H3Inputs {
            first_frame: true,
            ..H3Inputs::default()
        }
        .resolve();
        assert_eq!(i.wire_task, "fl2va");
        assert_eq!(i.variant, Fl2vaVariant::I2va);

        let l = H3Inputs {
            last_frame: true,
            ..H3Inputs::default()
        }
        .resolve();
        assert_eq!(l.wire_task, "fl2va");
        assert_eq!(l.variant, Fl2vaVariant::L2va);

        // Any reference asset switches checkpoint, and with it the guide and the body field name.
        let r = H3Inputs {
            identity_images: true,
            ..H3Inputs::default()
        }
        .resolve();
        assert_eq!(r.checkpoint, H3Checkpoint::Ref2va);
        assert_eq!(r.wire_task, "ref2va");
        assert_eq!(r.checkpoint.main_field(), "detailed_description");
        assert_eq!(
            t.checkpoint.main_field(),
            "integrated_multimodal_description"
        );
    }

    /// The role→task-type derivation, which is what koblem contributes to the guide's §3 table.
    #[test]
    fn task_types_derive_from_the_declared_roles() {
        assert_eq!(
            prefix(H3Inputs {
                identity_images: true,
                ..H3Inputs::default()
            }),
            "[reference generation]"
        );
        assert_eq!(
            prefix(H3Inputs {
                identity_images: true,
                audio_reference: true,
                ..H3Inputs::default()
            }),
            "[reference generation + audio reference]"
        );
        assert_eq!(
            prefix(H3Inputs {
                first_frame: true,
                identity_images: true,
                ..H3Inputs::default()
            }),
            "[keyframe completion + reference generation]"
        );
        // `GUIDE-ref:141`'s own two examples.
        assert_eq!(
            prefix(H3Inputs {
                last_frame: true,
                source_video: Some(H3SourceUse::Continue),
                ..H3Inputs::default()
            }),
            "[video continuation + keyframe completion]"
        );
        assert_eq!(
            prefix(H3Inputs {
                source_video: Some(H3SourceUse::Edit),
                audio_copy: true,
                ..H3Inputs::default()
            }),
            "[video editing + audio reuse]"
        );
        // `GUIDE-ref:143` — a video that supplies only structure is `reference generation`.
        assert_eq!(
            prefix(H3Inputs {
                source_video: Some(H3SourceUse::Structure),
                ..H3Inputs::default()
            }),
            "[reference generation]"
        );
    }

    /// The roles a reference carries are what drive the derivation, and `GUIDE-ref:145` decides
    /// which audio type a soundtracked source video contributes.
    #[test]
    fn from_refs_reads_the_inputs_off_the_roster() {
        let editing = [vid(true, Some(H3ReferenceRole::EditSource))];
        assert_eq!(
            prefix(H3Inputs::from_refs(&editing, false, false)),
            "[video editing + audio reuse]"
        );

        let continuing = [vid(true, Some(H3ReferenceRole::ContinuationSource))];
        assert_eq!(
            prefix(H3Inputs::from_refs(&continuing, false, false)),
            "[video continuation + audio reference]"
        );

        // A Ref2VA frame anchor is declared in prose, not by frame_index, but produces the same
        // `keyframe completion` type.
        let anchored = [
            img(Some(H3ReferenceRole::FrameAnchor), "opener"),
            img(Some(H3ReferenceRole::IdentitySource), "hero"),
        ];
        assert_eq!(
            prefix(H3Inputs::from_refs(&anchored, false, false)),
            "[keyframe completion + reference generation]"
        );
    }

    /// 🔴 The defaults are conservative on purpose: an unroled roster can only ever claim
    /// `reference generation`, never `video editing` or `audio reuse`.
    #[test]
    fn unroled_references_default_to_the_weakest_claim() {
        let refs = [img(None, "a"), vid(false, None), aud(None)];
        assert_eq!(
            prefix(H3Inputs::from_refs(&refs, false, false)),
            "[reference generation + audio reference]"
        );
    }

    // ── The facts ─────────────────────────────────────────────────────────────

    /// 🔴 Frames AND seconds, both derived from the grid so they cannot contradict each other.
    /// 8.0 s is exactly on the 17n+5 grid (192 frames); 10.0 s is NOT, and snapping it to 243
    /// frames is the whole reason the writer is told frames at all.
    #[test]
    fn render_facts_state_the_snapped_numbers() {
        let s = render_facts(Some(8.0), Some("16:9 landscape"));
        assert!(s.contains("192 frames at 24 fps"), "{s}");
        assert!(s.contains("8.00 seconds"), "{s}");
        assert!(s.contains("The canvas is 16:9 landscape."), "{s}");
        // An authored length off the grid is CORRECTED, never echoed alongside a frame count it
        // disagrees with.
        let off = render_facts(Some(10.0), None);
        assert!(off.contains("243 frames at 24 fps"), "{off}");
        assert!(!off.contains("10.00 seconds"), "{off}");
        // An unknown duration says so rather than inventing one.
        let u = render_facts(None, None);
        assert!(u.contains("4-15 seconds"), "{u}");
        assert!(!u.contains("frames at 24 fps"), "{u}");
    }

    /// 🔴 A known cut count is a FLOOR, not a ceiling: the divisions say where the FRAMING changes
    /// and are read back by position, so the reply may not be capped at their number. What it must
    /// not do is buy extra tags cheaply — each one is another cut — so the clause sends timing
    /// inside the shot instead.
    #[test]
    fn a_known_cut_count_is_a_floor_not_a_ceiling() {
        let c = cut_plan_facts(Some(3), None);
        assert!(c.contains("into 3 divisions"), "{c}");
        assert!(c.contains("floor, NOT a"), "{c}");
        assert!(c.contains("BY POSITION"), "{c}");
        // 🔴 The retired wording capped the reply at the division count. It must not come back.
        assert!(!c.contains("exactly 3 `[Shot N]`"), "{c}");
        assert!(cut_plan_facts(Some(1), None).contains("into 1 division"));
        // No count, no cut list to honour.
        assert!(
            !cut_plan_facts(None, None).contains("floor"),
            "no count, no floor"
        );
    }

    /// 🔴 `shot_and_beat_facts` owns what an unverbed tag does; this clause points at it and must
    /// not grow a second copy. Two statements of the same rule drift apart, and the pair the writer
    /// ends up holding contradict each other on whether a `[Shot N]` has to cut.
    #[test]
    fn the_cut_clause_defers_the_cut_fact_rather_than_restating_it() {
        let c = cut_plan_facts(Some(3), None);
        // The pointer itself, so deleting the whole clause fails here rather than passing vacuously.
        assert!(c.contains("as the fact below explains"), "{c}");
        assert!(!c.contains("TIMEFRAME"), "{c}");
        assert!(!c.contains("BEAT"), "{c}");
    }

    /// 🔴 Naming the focused shot must never read as permission to return only that shot.
    #[test]
    fn the_focused_shot_is_named_without_excusing_the_others() {
        let c = cut_plan_facts(Some(3), Some(1));
        // 0-based in, 1-based out — the pack's index and the body's tag are different numberings
        // and this is the one place they meet.
        assert!(c.contains("[Shot 2]"), "{c}");
        assert!(c.contains("the 3 divisions are the author's"), "{c}");
        // 🔴 It must NOT announce a total number of `[Shot N]`. `count` is the author's DIVISIONS,
        // and phrasing it as "of the 3 in this generation" told the writer three tags was the whole
        // budget — the same cap `cut_plan_facts` spends a paragraph saying is only a floor.
        assert!(!c.contains("of the 3 in this generation"), "{c}");
        // A single division names the focused tag and says nothing about a total at all. Anchored
        // on the tag and the phrase itself: a bare "of them" also matches "for each of them" in the
        // CUTS sentence every count gets, so it can never hold.
        let one = cut_plan_facts(Some(1), Some(0));
        assert!(one.contains("[Shot 1] — the idea"), "{one}");
        assert!(!one.contains("divisions are the author's"), "{one}");
        assert!(
            c.contains("every other `[Shot N]` comes back too, in order"),
            "{c}"
        );
        assert!(c.contains("overwrites the wrong one"), "{c}");
        // A focus past the end of a known pack is the caller's bug; pointing the writer at a shot
        // that is not there would be worse than silence.
        assert!(!cut_plan_facts(Some(3), Some(3)).contains("WHAT THE AUTHOR IS LOOKING AT"));
        // An unknown count still names the shot — it just cannot say "of N".
        let bare = cut_plan_facts(None, Some(0));
        assert!(bare.contains("[Shot 1]"), "{bare}");
        assert!(!bare.contains(" of the "), "{bare}");
    }

    // ── `[Shot N]` is a cut ───────────────────────────────────────────────────

    /// 🔴 The fact itself, and the measurement that carries it. The numbers are asserted on because
    /// they are what makes the clause read as a finding rather than as one more style preference —
    /// a writer that has just been handed §4.2, which shows a transition expression at every single
    /// tag, needs a reason to believe the unverbed case exists at all.
    #[test]
    fn the_cut_clause_states_the_measurement_not_a_preference() {
        let s = shot_and_beat_facts(false);
        assert!(s.contains("`[Shot N]` is a CUT"), "{s}");
        assert!(s.contains("00:11.508"), "{s}");
        assert!(s.contains("EIGHT SECONDS early"), "{s}");
        // The resolution floor, which is what stops a writer timing every transient.
        assert!(
            s.contains("CANNOT RESOLVE FINER THAN ABOUT A SECOND"),
            "{s}"
        );
    }

    /// 🔴 Only the packed path caps the cuts. With divisions the author has said where the framing
    /// changes and every extra tag is a cut they did not ask for; without them the writer chooses.
    /// Both paths carry the cut rule itself, and the retracted timeframe claim must not return by
    /// either route — it is what made a music video cut on its own timing marks.
    #[test]
    fn the_divisions_cap_the_cuts_only_on_the_packed_path() {
        let free = shot_and_beat_facts(false);
        let fixed = shot_and_beat_facts(true);
        for s in [&free, &fixed] {
            assert!(s.contains("`[Shot N]` is a CUT"), "{s}");
            // The retracted claim, by name.
            assert!(!s.contains("TIMEFRAME"), "{s}");
            assert!(!s.contains("WANTS ITS OWN BEAT"), "{s}");
        }
        assert!(
            fixed.contains("THE AUTHOR'S DIVISIONS ARE THE CUTS"),
            "{fixed}"
        );
        assert!(
            !free.contains("THE AUTHOR'S DIVISIONS ARE THE CUTS"),
            "{free}"
        );
        assert!(free.contains("you choose how many cuts"), "{free}");
    }

    /// 🔴 A frame anchor the prose never names is a picture the render has no reason to pass
    /// through. Measured 2026-08-09 on the live endpoint: told only that it "takes an entry of its
    /// own", the writer used `<Picture 1>` ZERO times and gave it no retention entry; with this
    /// sentence added to the same prompt it opened the beat from it in 3 of 3 samples.
    #[test]
    fn a_frame_anchor_must_be_named_in_the_prose() {
        let s = reference_roster_clause(
            &[img(
                Some(H3ReferenceRole::FrameAnchor),
                "[Shot 1] first frame",
            )],
            H3Checkpoint::Ref2va,
        );
        assert!(s.contains("NAME IT IN THE PROSE"), "{s}");
        // The ORDINAL, not a placeholder — the writer copies what it is shown.
        assert!(s.contains("begins from <Picture 1>"), "{s}");
        // An identity image is cited inside its `<Subject N>` and must NOT get this imperative.
        let id = reference_roster_clause(
            &[img(Some(H3ReferenceRole::IdentitySource), "Mia")],
            H3Checkpoint::Ref2va,
        );
        assert!(!id.contains("NAME IT IN THE PROSE"), "{id}");
    }

    /// 🔴 Measured 2026-08-09 against the live endpoint: with a story and cast attached, the writer
    /// opened five tags and numbered every one of them `[Shot 1]` — so the editor, the packer and
    /// the engine all read one shot, the four later timestamps collapsed into it, and the render
    /// lost every timed moment. Nothing in the prompt had ever said the number increments; the
    /// count clauses repeat "[Shot 1]" and it anchored on that.
    #[test]
    fn the_writer_is_told_the_tag_number_increments() {
        for fixed in [false, true] {
            let s = shot_and_beat_facts(fixed);
            assert!(s.contains("NUMBER SHOTS CONSECUTIVELY"), "{s}");
            assert!(s.contains("NEVER REPEAT A NUMBER"), "{s}");
            // The collapse a repeat causes is named, since it is silent otherwise.
            assert!(s.contains("collapses into it"), "{s}");
        }
    }

    #[test]
    fn a_packed_generation_times_its_moments_without_extra_cuts() {
        let fixed = shot_and_beat_facts(true);
        assert!(
            fixed.contains("THE AUTHOR'S DIVISIONS ARE THE CUTS"),
            "{fixed}"
        );
        assert!(
            fixed.contains("is an extra cut the author did not ask"),
            "{fixed}"
        );
        assert!(fixed.contains("Time the rest with clauses"), "{fixed}");
    }

    /// The gate is driven by the packed count, not by a caller's flag — so the one prompt that maps
    /// its reply by position is the one prompt that withholds the extra block.
    #[test]
    fn every_path_gets_the_cut_fact() {
        let packed = shot_system_prompt(H3GenerationPrompt {
            shot_count: Some(3),
            ..H3GenerationPrompt::new(H3Inputs::default())
        });
        assert!(packed.contains("`[Shot N]` is a CUT"), "{packed}");
        // ...and that its three divisions are the cuts, so timing costs no extra one.
        assert!(
            packed.contains("THE AUTHOR'S DIVISIONS ARE THE CUTS"),
            "{packed}"
        );
        let free = shot_system_prompt(H3GenerationPrompt::new(H3Inputs::default()));
        assert!(free.contains("`[Shot N]` is a CUT"), "{free}");
        assert!(free.contains("you choose how many cuts"), "{free}");
        // A zero count is not a count. It reaches `cut_plan_facts` as "the author placed none", and
        // the two clauses must agree — a writer told to return exactly zero blocks and to spend one
        // of them has been given a contradiction, not a constraint.
        let zero = shot_system_prompt(H3GenerationPrompt {
            shot_count: Some(0),
            ..H3GenerationPrompt::new(H3Inputs::default())
        });
        assert!(
            !zero.contains("THE AUTHOR'S DIVISIONS ARE THE CUTS"),
            "{zero}"
        );
        assert!(!zero.contains("count of cuts"), "{zero}");
    }

    /// 🔴 Every surface that WRITES or JUDGES a block needs the fact. The reviewer most of all: it
    /// is reading §4.2 too, so an unverbed tag looks to it like an author who forgot a cut verb —
    /// and its findings are one click from rewriting the shot.
    #[test]
    fn every_block_surface_carries_the_cut_fact() {
        let surfaces = [
            shot_system_prompt(H3GenerationPrompt::new(H3Inputs::default())),
            script_system_prompt(3, Some(10.0), None, &[], &[], &[]),
            rework_system(H3Checkpoint::Ref2va, &[], &[], &[]),
            extend_system(H3Checkpoint::Ref2va, &[], &[]),
            chat_craft_block(H3Checkpoint::Fl2vaFamily, &[], &[], H3ChatScope::Cuts),
            chat_craft_block(H3Checkpoint::Ref2va, &[], &[], H3ChatScope::BlocksReadOnly),
            chat_craft_block(H3Checkpoint::Ref2va, &[], &[], H3ChatScope::Blocks),
            review_system(true),
            review_system(false),
        ];
        for s in &surfaces {
            assert!(s.contains("`[Shot N]` is a CUT"), "{s}");
        }
    }

    /// 🔴 The planner's retracted paragraph. It used to say H3 had no beats and push a two-moment
    /// idea into two shots — a cut spent on every timed moment. The `beats` FIELD is still refused
    /// (the reply schema has none); the conclusion that was drawn from that is gone.
    #[test]
    fn the_planner_refuses_the_beats_field_without_refusing_beats() {
        let s = director_system_prompt(&StoryContext::default(), &[], &[]);
        assert!(s.contains("Do not emit a \"beats\" field"), "{s}");
        assert!(s.contains("no cut at all"), "{s}");
        // The retracted claim must not survive anywhere in the planner's prompt.
        assert!(!s.contains("Never use beats"), "{s}");
        assert!(!s.contains("no Prompt Relay layout"), "{s}");
        // Still no guide — the planner writes IDEAS, and this fact did not change that.
        assert!(!s.contains(GUIDE_BASE));
        assert!(!s.contains(GUIDE_REF));
    }

    /// The cast roster is the author's, and an empty one adds nothing.
    #[test]
    fn the_cast_roster_lists_only_what_the_author_agreed() {
        let cast = [LtxCastMember {
            name: "Mia".into(),
            appearance: "long dark hair, blue cardigan".into(),
            ..LtxCastMember::default()
        }];
        let s = cast_roster_clause(&cast);
        assert!(s.contains("Mia — long dark hair, blue cardigan"), "{s}");
        assert!(cast_roster_clause(&[]).is_empty());
    }

    /// A generation the author marked silent must not be given a `<d>` block to fill.
    #[test]
    fn the_speech_switch_is_stated_both_ways() {
        assert!(audio_and_text_facts(Some(false), false, &[]).contains("NOBODY SPEAKS"));
        assert!(audio_and_text_facts(Some(true), false, &[]).contains("SOMEBODY SPEAKS"));
        assert!(audio_and_text_facts(None, false, &[]).is_empty());
        assert!(audio_and_text_facts(None, true, &[]).contains("APPEARS ON SCREEN AS TEXT"));
    }

    // ── The vocal lane ────────────────────────────────────────────────────────

    fn span(start: f64, end: f64) -> H3VocalSpan {
        H3VocalSpan {
            start,
            end,
            words: None,
        }
    }

    fn lane(generation: Option<u32>, seconds: f64, spans: Vec<H3VocalSpan>) -> H3VocalLane {
        H3VocalLane {
            generation,
            seconds: Some(seconds),
            spans,
            run_up_seconds: None,
        }
    }

    /// 🔴 A cue placed inside the run-up is TRIMMED AWAY IN SILENCE. Measured on the real project:
    /// one authored mark sat at 0.592s against a 0.917s run-up and had to be rewritten by hand as
    /// an opening state. The writer cannot see that failure, so the lane has to say it.
    #[test]
    fn a_run_up_publishes_a_floor_and_never_a_span_inside_it() {
        let mut l = lane(
            None,
            14.375,
            //: 0.917, NOT 0.592 — `vocalLaneFor` clips spans past the run-up before they ever
            //: reach here, because that audio belongs to the previous shot from the viewer's side.
            vec![H3VocalSpan {
                start: 0.917,
                end: 6.592,
                words: None,
            }],
        );
        l.run_up_seconds = Some(22.0 / 24.0);
        let s = vocal_lane_facts(std::slice::from_ref(&l));
        assert!(s.contains("earliest markable time"), "{s}");
        assert!(s.contains("00:00.917"), "{s}");
        assert!(s.contains("OPENING STATE"), "{s}");
        //: The lane's own spans are clipped past the run-up upstream, so nothing here should
        //: report a vocal beginning inside it — that audio belongs to the previous shot.
        assert!(
            !s.contains("00:00.592"),
            "a span inside the run-up leaked into the lane: {s}"
        );

        // 0 and absent both stay silent — the first generation has no run-up, and an older client
        // sends none. A sentence about a trim that is not happening is noise on the healthy path.
        l.run_up_seconds = Some(0.0);
        assert!(!vocal_lane_facts(std::slice::from_ref(&l)).contains("earliest markable"));
        l.run_up_seconds = None;
        assert!(!vocal_lane_facts(std::slice::from_ref(&l)).contains("earliest markable"));
    }

    /// `MM:SS.mmm`, and the milliseconds are not decoration — a beat is 0.68 s at this project's
    /// tempo, so a mark rounded to the second can miss by more than one.
    #[test]
    fn the_lane_spells_its_times_the_way_a_shot_mark_does() {
        assert_eq!(h3_timecode(0.0), "00:00.000");
        assert_eq!(h3_timecode(8.467), "00:08.467");
        assert_eq!(h3_timecode(74.5), "01:14.500");
        // A negative can only come from bad arithmetic upstream; it must not print "00:-1.000".
        assert_eq!(h3_timecode(-3.0), "00:00.000");
    }

    /// 🔴 THE DEFECT, as a test. Generation 13 of "Gawdwin — Sealed by Fate": the song's vocal ends
    /// at song 171.44 s and this generation's slice starts at 186.33 s, so there is no voice in it
    /// at all — and it was written with *"its mouth moves in perfect synchronization with the male
    /// vocals"* as a standing fact, i.e. 14 s of a mouth flapping at nothing.
    ///
    /// An empty `spans` is a MEASUREMENT, not an absence, which is the whole reason the lane is a
    /// struct: `vec![]` alone cannot say "we looked, and there is none".
    #[test]
    fn a_fully_instrumental_generation_is_told_it_has_no_voice() {
        let s = vocal_lane_facts(&[lane(None, 14.375, vec![])]);
        assert!(s.contains("NO VOCAL AT ALL"), "{s}");
        assert!(s.contains("whole slice of the song is instrumental"), "{s}");
        assert!(s.contains("HAS NO VOICE IN IT"), "{s}");
        assert!(s.contains("Write no `<d>` block for it"), "{s}");
        // 🔴 The exact standing permission that produced the flapping mouth must be nowhere near it.
        assert!(!s.contains("a moving mouth are correct"), "{s}");
        assert!(!s.contains("SOMEBODY SPEAKS"), "{s}");
        // …and the same must hold through the assembled prompt, where the boolean also lives.
        let lanes = [lane(None, 14.375, vec![])];
        let prompt = shot_system_prompt(H3GenerationPrompt {
            seconds: Some(14.375),
            // Deliberately contradicted: an older editor that still sends `true` must not be able
            // to reinstate the flapping mouth alongside a lane that says the slice is instrumental.
            speech_in_shot: Some(true),
            vocal_lane: Some(&lanes[0]),
            ..H3GenerationPrompt::new(H3Inputs::default())
        });
        assert!(prompt.contains("NO VOCAL AT ALL"), "{prompt}");
        assert!(!prompt.contains("a moving mouth are correct"), "{prompt}");
        assert!(!prompt.contains("SOMEBODY SPEAKS OR SINGS"), "{prompt}");
    }

    /// The generation-1 row of the measured table (`~/handoffs/koblem/h3-director/
    /// RESULTS-sealed-by-fate-clock-2026-08-14.md`): song offset 14.3333 s, vocal at LOCAL
    /// 8.467-13.187 and 13.827-14.375. The 0-8.467 head is instrumental and has to be named, or the
    /// standing sentence covers it.
    #[test]
    fn a_partly_sung_generation_states_both_the_vocal_and_the_gaps() {
        let s = vocal_lane_facts(&[lane(
            None,
            14.375,
            vec![span(8.467, 13.187), span(13.827, 14.375)],
        )]);
        assert!(s.contains("00:08.467-00:13.187"), "{s}");
        assert!(s.contains("00:13.827-00:14.375"), "{s}");
        // The head gap is 8.467 s and must be listed; the 0.64 s hole between the two spans is under
        // a beat and must NOT be, or the lane becomes noise the writer skims.
        assert!(s.contains("NO VOICE at 00:00.000-00:08.467"), "{s}");
        assert!(!s.contains("00:13.187-00:13.827"), "{s}");
        // The standing-fact rule, the entry marks, and the §5.4 idiom for a reused soundtrack.
        assert!(
            s.contains("PUT THE SPANS IN THE `<Audio N>` DEFINITION"),
            "{s}"
        );
        assert!(s.contains("`[Shot N] At MM:SS.mmm`"), "{s}");
        assert!(s.contains("<Subject M> performs that vocal"), "{s}");
        assert!(
            s.contains("do NOT assign the song's voice an `(Sx)`"),
            "{s}"
        );
    }

    /// 🔴 The clock. `local = song_time - song_offset_seconds`, and `song_offset` is
    /// `h3_plan_soundtrack`'s — `(345 - H3_TRIM_JOIN) / 24` x N on the measured project, NOT
    /// 14.375 x N (that is `delivered_at_seconds`, the whole generation, which is what koblem's
    /// `offsets=[…]` log line prints). Getting the two confused slides every mark by the trim per
    /// seam, cumulatively.
    ///
    /// ⚠️ The expected value is DERIVED from `H3_TRIM_JOIN`, not written out. It was hard-coded as
    /// `14.333_333_333` — `(345 - 1) / 24` — and went stale the moment the trim became 22 on
    /// 2026-08-14, failing on a constant change that is not this test's subject.
    ///
    /// This pins the conversion against the real planner rather than against a second copy of the
    /// arithmetic: 15 generations of 345 frames, the first unanchored and the rest anchored.
    #[test]
    fn local_seconds_are_song_seconds_minus_the_planners_own_offset() {
        use koblem_client::types::{H3_FPS, H3_TRIM_JOIN, h3_plan_soundtrack};
        let gens: Vec<(u32, bool, bool)> = (0..15).map(|i| (345, i > 0, true)).collect();
        let plan = h3_plan_soundtrack(&gens, 208.008);

        // The measured table's own offsets.
        assert!((plan.slices[0].song_offset_seconds - 0.0).abs() < 1e-6);
        let advance = f64::from(345 - H3_TRIM_JOIN) / f64::from(H3_FPS);
        for i in 1..15 {
            let want = advance * i as f64;
            assert!(
                (plan.slices[i].song_offset_seconds - want).abs() < 1e-6,
                "generation {i}: {} != {want}",
                plan.slices[i].song_offset_seconds
            );
        }
        // …and NOT the delivered clock, which is the whole 345 frames and so runs `H3_TRIM_JOIN`
        // frames past where the next generation starts.
        assert!((plan.slices[1].delivered_at_seconds - 14.375).abs() < 1e-6);

        // Generation 1's first sung word is at song 22.80 s. The writer is handed it on the LOCAL
        // clock, which is the number a `[Shot N] At …` carries.
        let local = 22.80 - plan.slices[1].song_offset_seconds;
        assert!((local - (22.80 - advance)).abs() < 1e-9, "{local}");
        assert_eq!(h3_timecode(local), "00:09.342");

        // The song's vocal ends at 171.44 s; the first generation starting after it has nothing
        // left to hand the writer. WHICH generation that is moves with the trim, so derive it.
        let past_vocal = (171.44 / advance).floor() as usize + 1;
        assert!(past_vocal < plan.slices.len(), "{past_vocal}");
        assert!(plan.slices[past_vocal].song_offset_seconds > 171.44);
    }

    /// The batch paths describe MANY generations at once, so each lane has to say which one it is.
    /// A `None` generation is the single-generation wording and must not number itself.
    #[test]
    fn a_batch_lane_numbers_its_generations_and_a_single_one_does_not() {
        let many = vocal_lane_facts(&[
            lane(Some(1), 14.375, vec![]),
            lane(Some(2), 14.375, vec![span(0.0, 14.375)]),
        ]);
        assert!(many.contains("Generation 1"), "{many}");
        assert!(many.contains("Generation 2"), "{many}");
        // Stated once for the whole list, not once per generation.
        assert_eq!(many.matches("PUT THE SPANS IN").count(), 1, "{many}");

        let one = vocal_lane_facts(&[lane(None, 14.375, vec![span(0.0, 5.0)])]);
        assert!(one.contains("This generation"), "{one}");
        assert!(!one.contains("Generation 1"), "{one}");
    }

    /// Quoted words earn the §5.4 idiom a phrase to hang on — and must arrive with the warning that
    /// they came out of a recogniser pointed at SINGING. On the project this was built for it heard
    /// "Idol Hitler" for "Adolf Hitler"; a writer that quotes that verbatim inside `<d>` has put a
    /// hallucinated lyric into the render.
    #[test]
    fn quoted_transcript_words_arrive_marked_approximate() {
        let s = vocal_lane_facts(&[lane(
            None,
            14.375,
            vec![H3VocalSpan {
                start: 2.0,
                end: 6.0,
                words: Some("sealed by fate".into()),
            }],
        )]);
        assert!(s.contains("heard as \"sealed by fate\""), "{s}");
        assert!(s.contains("APPROXIMATE"), "{s}");
        assert!(s.contains("[unclear]"), "{s}");
        assert!(s.contains("Never repair a lyric by guessing"), "{s}");
    }

    /// 🔴 Gap #1: the two writers that lay out a WHOLE timeline never called
    /// `audio_and_text_facts` at all, so the batch writer — the one that decides the standing
    /// `<Audio N>` sentence for every generation in one turn — was never told whether anyone sings.
    #[test]
    fn all_three_writer_paths_carry_the_lane() {
        let lanes = [
            lane(Some(1), 14.375, vec![span(1.0, 4.0)]),
            lane(Some(2), 14.375, vec![]),
        ];
        let single = shot_system_prompt(H3GenerationPrompt {
            vocal_lane: Some(&lanes[0]),
            ..H3GenerationPrompt::new(H3Inputs::default())
        });
        let script = script_system_prompt(2, Some(14.375), None, &[], &[], &lanes);
        let rework = rework_system(H3Checkpoint::Ref2va, &[], &[], &lanes);
        for s in [&single, &script, &rework] {
            assert!(s.contains("THE VOCAL LANE"), "{s}");
            assert!(
                s.contains("PUT THE SPANS IN THE `<Audio N>` DEFINITION"),
                "{s}"
            );
        }
        // The batch paths carry the instrumental generation's verdict too — that is the one the
        // single-generation path cannot see.
        assert!(script.contains("NO VOCAL AT ALL"), "{script}");
        assert!(rework.contains("NO VOCAL AT ALL"), "{rework}");
    }

    /// ⚠️ ABSENT ⇒ UNCHANGED. A client that sends no lane must get the prompt it was written
    /// against, byte for byte — the lane is new information, not a new default.
    #[test]
    fn no_lane_leaves_every_path_byte_identical() {
        assert!(vocal_lane_facts(&[]).is_empty());
        // The exact pre-lane strings, spelled out rather than compared against themselves.
        assert_eq!(
            audio_and_text_facts(Some(true), false, &[]),
            "\n\nSOMEBODY SPEAKS OR SINGS in this generation — the author has said so, so a `<d>` \
             block and a moving mouth are correct.",
        );
        assert_eq!(
            audio_and_text_facts(Some(false), false, &[]),
            "\n\nNOBODY SPEAKS OR SINGS in this generation — the author has said so. Write no `<d>` \
             block at all and put no moving mouth on anyone; carry the shot on action, ambience and \
             score.",
        );
        assert!(audio_and_text_facts(None, false, &[]).is_empty());
        // …and no path leaks a word of the new clause when nobody supplied a lane, which is what
        // "an older client is unchanged" means operationally.
        let bare = shot_system_prompt(H3GenerationPrompt {
            speech_in_shot: Some(true),
            ..H3GenerationPrompt::new(H3Inputs::default())
        });
        assert!(bare.contains("SOMEBODY SPEAKS OR SINGS"), "{bare}");
        assert!(!bare.contains("THE VOCAL LANE"), "{bare}");
        assert!(!script_system_prompt(2, Some(14.375), None, &[], &[], &[]).contains("VOCAL LANE"));
        assert!(!rework_system(H3Checkpoint::Ref2va, &[], &[], &[]).contains("VOCAL LANE"));
    }

    // ── The assembled prompts ─────────────────────────────────────────────────

    /// A Ref2VA generation is handed the REF guide, its roster, its facts, and a contract asking for
    /// the block. It must NOT be handed the other checkpoint's guide.
    #[test]
    fn a_ref2va_prompt_carries_the_ref_guide_and_the_facts() {
        let refs = [
            img(Some(H3ReferenceRole::IdentitySource), "hero"),
            aud(Some(H3ReferenceRole::VoiceTimbre)),
        ];
        let s = shot_system_prompt(H3GenerationPrompt {
            seconds: Some(10.0),
            aspect: Some("16:9 landscape"),
            refs: &refs,
            ..H3GenerationPrompt::new(H3Inputs::from_refs(&refs, false, false))
        });
        assert!(s.contains(GUIDE_REF), "the ref guide must be verbatim");
        assert!(
            !s.contains(GUIDE_BASE),
            "the wrong guide must not ride along"
        );
        assert!(
            s.contains("[reference generation + audio reference]"),
            "{s}"
        );
        assert!(s.contains("<Picture 1>"));
        assert!(s.contains("243 frames at 24 fps"));
        assert!(s.contains("the finished prompt itself"));
    }

    /// A T2VA generation gets the BASE guide and is told which of its four shapes this is.
    #[test]
    fn a_t2va_prompt_carries_the_base_guide() {
        let s = shot_system_prompt(H3GenerationPrompt {
            seconds: Some(10.0),
            ..H3GenerationPrompt::new(H3Inputs::default())
        });
        assert!(s.contains(GUIDE_BASE));
        assert!(!s.contains(GUIDE_REF));
        assert!(s.contains("the T2VA shape"), "{s}");
        assert!(s.contains("integrated_multimodal_description"));
    }

    /// 🔴 The reversal, in one assertion: we ask for the BLOCK and never for our own JSON shape.
    #[test]
    fn the_contract_asks_for_the_block_not_for_json() {
        let s = shot_system_prompt(H3GenerationPrompt::new(H3Inputs::default()));
        assert!(s.contains("Not JSON"), "{s}");
        assert!(s.contains("You write the `[Shot N]` tags"), "{s}");
        assert!(s.contains("7000 characters"), "{s}");
        // The retired contract's own field names must not reappear anywhere.
        assert!(!s.contains("\"at_seconds\""), "{s}");
        assert!(!s.contains("\"style_opening\""), "{s}");
        assert!(!s.contains("task_types\":"), "{s}");
    }

    /// 🔴 The chat retraction of 2026-08-08, which still holds for the two CUT-id arms.
    /// `set_prompt`'s value lands in `DirectorClip.prompt`, which holds ONE `[Shot N]` body — so
    /// asking for a whole block there produced a seeded prompt with `summary:` and
    /// `overall_soundscape:` nested inside a shot description. The assertions name the headers
    /// explicitly because "a complete block" is the phrase that has to stay gone from THESE arms,
    /// and a reworded version of it would pass a looser test.
    #[test]
    fn a_cut_targeted_chat_set_prompt_is_one_shot_body_and_never_a_block() {
        for scope in [H3ChatScope::Cuts, H3ChatScope::BlocksReadOnly] {
            let s = chat_craft_block(H3Checkpoint::Ref2va, &[], &[], scope);
            assert!(s.contains("the prose of ONE `[Shot N]`"), "{s}");
            assert!(s.contains("Not a whole block"), "{s}");
            // The three things it must be told NOT to write, by name.
            assert!(s.contains("`detailed_description:`"), "{s}");
            assert!(s.contains("no `[Shot N]` tag"), "{s}");
            assert!(s.contains("no soundscape and no score"), "{s}");
            // ⚠️ The guide is still in there and says the opposite about a BLOCK, which is correct —
            // the guide describes the artifact, this clause describes one action's value.
            assert!(s.contains(GUIDE_REF), "the guide is still the authority");
        }
    }

    /// 🔴 The other half of the pairing note, and the whole reason this parameter is an enum: under
    /// [`H3ChatScope::Blocks`] the action writes `DirectorH3Generation.prompt`, so the value IS the
    /// whole block and the retracted wording would now be the bug — a model told to write one
    /// shot's prose while the action stores a block produces a block containing a fragment.
    #[test]
    fn a_generation_targeted_chat_set_prompt_is_the_whole_block() {
        let s = chat_craft_block(H3Checkpoint::Ref2va, &[], &[], H3ChatScope::Blocks);
        assert!(s.contains("COMPLETE block"), "{s}");
        assert!(s.contains("REPLACES the whole prompt"), "{s}");
        // The load-bearing sentence: an omitted section is a DELETED section, not an unchanged one.
        assert!(
            s.contains("repeat VERBATIM every part you are not changing"),
            "{s}"
        );
        assert!(s.contains("Anything you leave out is deleted"), "{s}");
        // The cut-arm wording must be gone, not merely outnumbered.
        assert!(!s.contains("the prose of ONE `[Shot N]`"), "{s}");
        assert!(!s.contains("Not a whole block"), "{s}");
        // And the unit is named right from the first sentence, because everything after is read in
        // those terms.
        assert!(s.contains("ONE SHOT IS ONE RENDER"), "{s}");
    }

    /// The blocks are explained exactly when they are on screen. A block in the listing with nothing
    /// said about it is an invitation to answer with one; an explanation of blocks that are not
    /// there is a description of input the model was never given, which is the same confabulation
    /// invitation `review_system`'s `shows_blocks` closes.
    #[test]
    fn chat_reads_the_blocks_only_when_it_is_shown_them() {
        let read_only =
            chat_craft_block(H3Checkpoint::Ref2va, &[], &[], H3ChatScope::BlocksReadOnly);
        let blocks = chat_craft_block(H3Checkpoint::Ref2va, &[], &[], H3ChatScope::Blocks);
        let without = chat_craft_block(H3Checkpoint::Ref2va, &[], &[], H3ChatScope::Cuts);
        for s in [&read_only, &blocks] {
            assert!(
                s.contains("WHAT THE PROMPT BLOCKS IN THE LISTING ARE"),
                "{s}"
            );
        }
        assert!(!without.contains("WHAT THE PROMPT BLOCKS IN THE LISTING ARE"));
        // ⚠️ Only the addressable arm says the blocks can be REWRITTEN. Telling a cut-id turn that
        // would be inviting a block it has nowhere to put.
        assert!(
            read_only.contains("nothing you reply with replaces a block"),
            "{read_only}"
        );
        assert!(
            !blocks.contains("nothing you reply with replaces a block"),
            "{blocks}"
        );
    }

    /// 🔴 Chat was the last rewriting surface with no cast. Identity is carried by repeating the
    /// appearance sentence verbatim, so a turn that rewrites Mia without it swaps her out.
    #[test]
    fn a_chat_turn_carries_the_projects_cast() {
        let cast = [LtxCastMember {
            name: "Mia".into(),
            appearance: "long dark hair, blue cardigan".into(),
            ..LtxCastMember::default()
        }];
        let s = chat_craft_block(H3Checkpoint::Ref2va, &[], &cast, H3ChatScope::Cuts);
        assert!(s.contains("Mia — long dark hair, blue cardigan"), "{s}");
        // An empty cast adds nothing, so an older client's prompt is byte-identical.
        assert!(
            !chat_craft_block(H3Checkpoint::Ref2va, &[], &[], H3ChatScope::Cuts)
                .contains("RECURRING CAST")
        );
    }

    /// The author's text is intent, whatever shape it arrived in.
    #[test]
    fn the_prompt_says_the_authors_text_is_intent() {
        let s = shot_system_prompt(H3GenerationPrompt::new(H3Inputs::default()));
        assert!(s.contains("is INTENT"), "{s}");
        assert!(
            s.contains("an unstructured brief is the normal case"),
            "{s}"
        );
    }

    /// The date is pinned in every writer prompt — koblem's convention for post-cutoff dates.
    #[test]
    fn the_current_date_is_injected() {
        assert!(
            shot_system_prompt(H3GenerationPrompt::new(H3Inputs::default()))
                .contains("CURRENT DATE: 20")
        );
        assert!(review_system(true).contains("CURRENT DATE: 20"));
        assert!(review_system(false).contains("CURRENT DATE: 20"));
        assert!(extend_system(H3Checkpoint::Ref2va, &[], &[]).contains("CURRENT DATE: 20"));
    }

    /// The script writer's unit is the GENERATION, and it is told how to separate them.
    #[test]
    fn the_script_prompt_asks_for_n_blocks_separated_by_the_sentinel() {
        let s = script_system_prompt(3, Some(10.0), Some("16:9 landscape"), &[], &[], &[]);
        assert!(s.contains("break the brief into 3 GENERATIONS"), "{s}");
        assert!(s.contains("never several entries"), "{s}");
        assert!(s.contains(GENERATION_SEPARATOR), "{s}");
    }

    /// The planner writes IDEAS, so it must NOT be handed a prompt-writing guide.
    #[test]
    fn the_director_prompt_carries_no_guide() {
        let s = director_system_prompt(&StoryContext::default(), &[], &[]);
        assert!(!s.contains(GUIDE_BASE));
        assert!(!s.contains(GUIDE_REF));
        assert!(s.contains("THIS SHOT STARTS A NEW GENERATION"));
        assert!(s.contains("as few generations as the"));
    }

    /// The reviewer cannot know the checkpoint, so it gets both guides and is told how to tell.
    #[test]
    fn the_reviewer_gets_both_guides_and_a_rule_for_telling_them_apart() {
        for shows_blocks in [true, false] {
            let s = review_system(shows_blocks);
            assert!(s.contains(GUIDE_BASE));
            assert!(s.contains(GUIDE_REF));
            assert!(s.contains("is not missing four sections"), "{s}");
        }
    }

    /// 🔴 The defect this parameter exists for: the schema checklist is only asked for when the
    /// blocks are actually in the input. Shown one line per cut, the reviewer cannot see a cut time,
    /// a label or a retention line, so being told to hunt for them can only produce invented
    /// findings — the no-blocks arm must instead say they are absent.
    #[test]
    fn the_schema_checklist_is_only_asked_for_when_the_blocks_are_shown() {
        // ⚠️ The negative assertions have to name phrases of OURS. "strictly increasing" is in
        // `GUIDE_BASE:86`, which both arms carry, so asserting on it would pass either way.
        let with = review_system(true);
        assert!(with.contains("leaves no room for the final cut"), "{with}");
        assert!(
            with.contains("a defined label with no retention line"),
            "{with}"
        );
        assert!(
            with.contains("disagrees with the checkpoint in its header"),
            "{with}"
        );
        assert!(
            with.contains("the complete prompt block for that render"),
            "{with}"
        );
        assert!(with.contains("NOT WRITTEN YET"), "{with}");

        let without = review_system(false);
        assert!(
            !without.contains("leaves no room for the final cut"),
            "{without}"
        );
        assert!(
            !without.contains("disagrees with the checkpoint in its header"),
            "{without}"
        );
        assert!(
            !without.contains("a defined label with no retention line"),
            "{without}"
        );
        assert!(
            without.contains("You are NOT shown the assembled prompt blocks"),
            "{without}"
        );
        assert!(without.contains("never report a section"), "{without}");
    }

    /// The header's checkpoint has to name the body field, or "judge it against its own schema" is
    /// a guess. `t2va` and `fl2va` share the FL2VA schema; only `ref2va` is the six-section one.
    #[test]
    fn a_generation_brief_knows_which_body_field_its_checkpoint_requires() {
        let g = |task: &str| H3GenerationBrief {
            task: Some(task.into()),
            ..Default::default()
        };
        assert_eq!(g("ref2va").expected_field(), Some("detailed_description"));
        assert_eq!(
            g("fl2va").expected_field(),
            Some("integrated_multimodal_description")
        );
        assert_eq!(
            g("t2va").expected_field(),
            Some("integrated_multimodal_description")
        );
        // An unknown or absent task claims nothing rather than guessing a schema for it.
        assert_eq!(g("i2va").expected_field(), None);
        assert_eq!(H3GenerationBrief::default().expected_field(), None);
    }

    /// Blank is not written. A never-opened generation has no block, and treating its empty string
    /// as one would present it to the reviewer as an author who wrote nothing.
    #[test]
    fn a_blank_prompt_is_not_a_block() {
        assert_eq!(H3GenerationBrief::default().block(), None);
        assert_eq!(
            H3GenerationBrief {
                prompt: Some("   \n ".into()),
                ..Default::default()
            }
            .block(),
            None
        );
        let g = H3GenerationBrief {
            prompt: Some("  detailed_description:\n[Shot 1] x  ".into()),
            ..Default::default()
        };
        assert_eq!(g.block(), Some("detailed_description:\n[Shot 1] x"));
    }

    /// 🔴 A blank `id` is NOT an id. Chat's whole generation arm is gated on every brief having one
    /// (`ltx_director::chat_generation_ids`), and an empty string that counted would mint a token
    /// every unnamed render answers to — the id contract exists precisely to stop an edit landing on
    /// a shot nobody named.
    #[test]
    fn a_blank_generation_id_or_gist_is_absent() {
        assert_eq!(H3GenerationBrief::default().id(), None);
        assert_eq!(H3GenerationBrief::default().gist(), None);
        let blank = H3GenerationBrief {
            id: Some("  ".into()),
            gist: Some("\n".into()),
            ..Default::default()
        };
        assert_eq!(blank.id(), None);
        assert_eq!(blank.gist(), None);
        let g = H3GenerationBrief {
            id: Some(" gen-1 ".into()),
            gist: Some(" she leaves the bar ".into()),
            ..Default::default()
        };
        assert_eq!(g.id(), Some("gen-1"));
        assert_eq!(g.gist(), Some("she leaves the bar"));
    }

    // ── Reading the reply ─────────────────────────────────────────────────────

    /// 🔴 The trap that made this function necessary: `clean_prompt` eats a short leading line that
    /// ends in a colon, and `subject_definitions:` is exactly that.
    #[test]
    fn clean_block_keeps_the_first_section_header() {
        let block =
            "subject_definitions:\n<Subject 1> is a woman.\n\nsummary:\n[reference generation] x";
        assert_eq!(clean_block(block), block);
        assert_eq!(clean_block(&format!("```\n{block}\n```")), block);
        assert_eq!(clean_block(&format!("```text\n{block}\n```")), block);
    }

    /// The gist rides after a sentinel line, never inside JSON — a local model escaping a multi-line
    /// block into a JSON string is how the previous contract lost prompts.
    #[test]
    fn the_gist_splits_off_the_block() {
        let reply = format!("detailed_description:\n[Shot 1] x\n{GIST_SEPARATOR}\nShe leaves.");
        let (block, gist) = split_gist(&reply);
        assert_eq!(block, "detailed_description:\n[Shot 1] x");
        assert_eq!(gist.as_deref(), Some("She leaves."));
        // No sentinel: the whole reply is the block and nothing is invented.
        let (b, g) = split_gist("detailed_description:\n[Shot 1] x");
        assert_eq!(b, "detailed_description:\n[Shot 1] x");
        assert_eq!(g, None);
    }

    /// The text carrier: N blocks, one sentinel line between them.
    #[test]
    fn generations_split_on_the_sentinel() {
        let reply = format!(
            "integrated_multimodal_description: [Shot 1] a\n\noverall_soundscape: s\n\
             \n{GENERATION_SEPARATOR}\n\
             integrated_multimodal_description: [Shot 1] b\n\noverall_soundscape: t"
        );
        let out = parse_generations(&reply, 4);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].contains("[Shot 1] a"));
        assert!(out[1].contains("[Shot 1] b"));
        assert_eq!(parse_generations(&reply, 1).len(), 1);
        // A single block with no sentinel is one generation, not zero.
        assert_eq!(
            parse_generations("integrated_multimodal_description: [Shot 1] a", 3).len(),
            1
        );
    }

    /// 🔴 The JSON carriers stay live. A rollout has a koblem of each generation talking to the
    /// other, and the fallback is the difference between degrading and losing the reply.
    #[test]
    fn the_json_carriers_still_parse() {
        let envelope = r#"```json
        {"generations":[{"shots":[{"index":0,"at_seconds":null,"body":"a"}],"overall_soundscape":"s"},
                        {"shots":[{"index":0,"at_seconds":null,"body":"b"}],"overall_soundscape":"t"}]}
        ```"#;
        let out = parse_generations(envelope, 4);
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("\"body\":\"a\""));
        // Blocks carried as JSON strings work too.
        let strings = r#"{"generations":["detailed_description:\n[Shot 1] a","detailed_description:\n[Shot 1] b"]}"#;
        let out = parse_generations(strings, 4);
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("[Shot 1] b"));
        assert!(parse_generations("not json and not a block", 3).is_empty());
    }

    // ── One role enum, shared with the render path ────────────────────────────

    /// A brief and a wire reference must read the SAME role vocabulary — that is the whole reason
    /// there is no second enum here. Two copies that drifted would let a reference mean
    /// "continuation source" to `build_chain` (which decides what conditioning is sent) and
    /// "reference" to this module (which decides what the roster claims was sent).
    #[test]
    fn a_brief_and_a_wire_reference_resolve_the_same_role() {
        let wire: koblem_client::types::H3Reference = serde_json::from_value(
            serde_json::json!({"kind": "video", "role": "edit_source", "video": "x"}),
        )
        .unwrap();
        let brief: H3RefBrief =
            serde_json::from_value(serde_json::json!({"kind": "video", "role": "edit_source"}))
                .unwrap();
        assert_eq!(brief.role(), wire.effective_role());
        assert_eq!(brief.role(), H3ReferenceRole::EditSource);
        // And the same absence resolves the same way on both sides.
        let wire_bare: koblem_client::types::H3Reference =
            serde_json::from_value(serde_json::json!({"kind": "video", "video": "x"})).unwrap();
        let brief_bare: H3RefBrief =
            serde_json::from_value(serde_json::json!({"kind": "video"})).unwrap();
        assert_eq!(brief_bare.role(), wire_bare.effective_role());
        assert_eq!(brief_bare.role(), H3ReferenceRole::StructureSource);
    }

    /// 🔴 A role the emitter has not been taught must not take the request down with it. The derived
    /// deserialiser rejects `"reference"` — the engine's own catch-all — so the lenient one is
    /// load-bearing, not defensive: a 400 here fails a whole rework/extend call over one stale
    /// reference.
    #[test]
    fn an_unknown_role_on_a_brief_is_read_as_absent_not_as_an_error() {
        for role in ["reference", "time_travel", ""] {
            let brief: H3RefBrief =
                serde_json::from_value(serde_json::json!({"kind": "image", "role": role})).unwrap();
            assert_eq!(brief.role, None, "{role:?} must parse as 'not stated'");
            assert_eq!(brief.role(), H3ReferenceRole::IdentitySource);
        }
        // `reuse` only ever meant audio, so it has exactly one reading and keeps it.
        let brief: H3RefBrief =
            serde_json::from_value(serde_json::json!({"kind": "audio", "role": "reuse"})).unwrap();
        assert_eq!(brief.role(), H3ReferenceRole::AudioReuse);
        assert!(H3Inputs::from_refs(&[brief], false, false).audio_copy);
    }

    /// Every role has both an English phrase for the roster and a `retention_analysis`
    /// parenthetical. A new variant added in koblibs makes both matches non-exhaustive, which is the
    /// compiler doing the job cross-referencing comments used to attempt.
    #[test]
    fn every_role_has_prompt_english() {
        for role in [
            H3ReferenceRole::FirstFrame,
            H3ReferenceRole::LastFrame,
            H3ReferenceRole::IdentitySource,
            H3ReferenceRole::FrameAnchor,
            H3ReferenceRole::Storyboard,
            H3ReferenceRole::EditSource,
            H3ReferenceRole::ContinuationSource,
            H3ReferenceRole::StructureSource,
            H3ReferenceRole::VoiceTimbre,
            H3ReferenceRole::AudioReuse,
            H3ReferenceRole::AudioContinuity,
        ] {
            assert!(!role_phrase(role).is_empty(), "{role:?}");
            assert!(!retention_scope(role).is_empty(), "{role:?}");
        }
    }
}
