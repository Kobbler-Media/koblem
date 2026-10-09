//! Shared LTX prompt-writing guidance + LLM-reply parsing for koblem's "draft with LLM"
//! surfaces (script-from-llm, per-shot prompt-from-llm, the auto-director). Encodes the official
//! prompt guide so every drafted prompt is a long, detailed, single flowing paragraph in the
//! cinematic style LTX rewards — instead of the one-line sketches the old prompts produced.
//!
//! ⚠️ The guide it encodes is LTX-**2.3**'s, and that is provenance rather than staleness: 2.5 is a
//! new checkpoint of the same model family reached over the same wire, not a second prompt space,
//! which is why every writing route branches h3-vs-not and never 2.3-vs-2.5. What DID have to
//! change is the version named to the model below — a system prompt that introduces a retired
//! engine invites the model to reason about a checkpoint nobody is rendering on.
//!
//! Pure functions, unit-tested below. Callers assemble a system prompt from `shot_system_prompt`
//! / `script_system_prompt`, attach any images (labelled via [`label_images`]), and parse the reply
//! with [`parse_shots`] (multi-shot) or read the single paragraph directly.

use koblem_client::types::{LtxCastMember, LtxSegmentMode, StoryContext};
use serde_json::{Value, json};

/// How a drafted prompt treats its visual starting point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    /// Text-to-video: the model invents the whole scene.
    T2v,
    /// Image-to-video: an attached image already defines the look; describe only what happens next.
    I2v,
    /// Continuation: no image is attached, but the shot renders from the PREVIOUS shot's final
    /// frame — mechanically i2v, anchored on the previous shot's prompt rather than an image.
    Continuation,
    /// Video-to-video edit/restyle: a source clip already exists; describe the change to apply.
    V2v,
}

impl PromptMode {
    /// Resolve from an explicit segment mode, whether images are attached, and whether the shot
    /// continues out of the previous shot's final frame. A plain `t2v`/auto shot that continues is
    /// really i2v — it inherits the prior frame — so it resolves to [`PromptMode::Continuation`],
    /// which tells the model to describe the change, not re-describe the standing scene.
    pub fn resolve(mode: Option<LtxSegmentMode>, has_images: bool, continuation: bool) -> Self {
        match mode {
            Some(LtxSegmentMode::I2v) => PromptMode::I2v,
            Some(LtxSegmentMode::V2vEdit) | Some(LtxSegmentMode::V2vRestyle) => PromptMode::V2v,
            Some(LtxSegmentMode::T2v) if continuation => PromptMode::Continuation,
            Some(LtxSegmentMode::T2v) => PromptMode::T2v,
            _ if has_images => PromptMode::I2v,
            _ if continuation => PromptMode::Continuation,
            _ => PromptMode::T2v,
        }
    }
}

/// The shared "how to write" rules every drafted LTX prompt follows — mode-neutral craft only. It
/// deliberately says NOTHING about what to cover (that is mode-specific — see [`mode_clause`] —
/// because t2v must paint the whole scene while the i2v family must not) and NOTHING about sound
/// (that is [`audio_clause`]'s sole job). Single-sourcing each concern stops the local model
/// resolving a contradiction — "cover the lighting" vs "don't describe the scene", or "cover the
/// audio" vs "don't narrate sound" — by following the wrong half.
pub const BASE: &str = "Write ONE flowing paragraph in present tense, like a cinematographer's \
shot note. Describe the action literally, in the order it happens, and keep to the one or two \
actions that fit the shot's length — don't cram. Show feeling through physical cues — a clenched \
jaw, darting eyes — never labels like \"sad\" or \"tense\". Use plain cinematic language, not \
numbers or lists. Never put on-screen text or logos in frame, and never contradictory lighting \
or motion.";

/// Duration scaling: longer shots need longer prompts or LTX rushes the action.
fn duration_clause(seconds: Option<f64>) -> String {
    match seconds {
        Some(s) if s > 0.0 => format!(
            " This shot runs about {s:.0}s — write enough vivid detail to fill the whole \
             duration so the action never feels rushed; longer shots need longer prompts."
        ),
        _ => " Write a detailed prompt — short prompts leave the shot underfilled.".to_string(),
    }
}

/// Cross-shot visual consistency. Unless a shot closes its scene, the renderer continues the next
/// shot straight out of this one's FINAL frame — so anything that frame hides has to be invented
/// again downstream, and it will be invented differently. A shot CLOSES its scene when it is the
/// last shot, or when the next shot renders fresh (a hard cut — `LtxSegment::scene_cut` /
/// `DirectorClip::new_scene` — or an i2v/v2v shot starting from its own image/source clip).
pub fn continuation_clause(closes_scene: bool) -> &'static str {
    if closes_scene {
        " This shot ends its scene — nothing continues out of it, so close on whatever framing \
         lands the moment."
    } else {
        " The next shot continues from this shot's final frame, and that frame is the ONLY thing \
         it can see — so end wide enough that the subject, setting and surroundings all stay in \
         view. Don't end tight on a face or a detail, on a whip-pan, an empty frame or a fade, or \
         the next shot loses the scene."
    }
}

/// The pacing + scene-flow rules for a path that writes MANY shots in one call. Each shot has its
/// own length and its own answer to "does anything continue out of me?", which a single global
/// system prompt can't state — so each shot's line carries a [`shot_pacing_tag`] and this explains
/// how to read it. Composed from [`continuation_clause`] so the batch and single-shot paths can
/// never drift apart.
pub fn multi_shot_pacing() -> String {
    format!(
        " Each shot below is tagged with its own length in seconds, whether it starts fresh or is \
         `carried` (it continues out of the PREVIOUS shot's final frame), and whether it CONTINUES \
         into the next shot or CLOSES its scene. Pace each prompt to its OWN shot's length — a \
         longer shot needs more written detail or the action runs out before the shot does, and a \
         short shot must not be crammed. A `carried` shot already has its opening frame — the \
         previous shot's final frame — so describe what CHANGES, not the standing scene it shows. \
         CONTINUES:{cont} CLOSES:{closes}",
        cont = continuation_clause(false),
        closes = continuation_clause(true),
    )
}

/// The per-line pacing annotation for a batch shot listing, e.g. ` · 6s · CONTINUES`. The CONTINUES
/// / CLOSES vocabulary must match [`multi_shot_pacing`], so both live here.
pub fn shot_pacing_tag(seconds: Option<f64>, closes_scene: bool) -> String {
    let secs = seconds
        .filter(|s| *s > 0.0)
        .map(|s| format!(" · {s:.0}s"))
        .unwrap_or_default();
    let flow = if closes_scene { "CLOSES" } else { "CONTINUES" };
    format!("{secs} · {flow}")
}

/// A single worked contrast for the i2v family — the exact failure the local model keeps making
/// (restating the frame instead of moving it). Appended to both the i2v and continuation clauses,
/// which is where it matters; t2v (which must describe the scene) never sees it.
const I2V_EXAMPLE: &str = " For example — good: \"She tightens her grip on the railing and leans \
forward as the camera slowly pushes in.\" Not: \"A woman in a red coat on a balcony at golden \
hour\" — that only restates the frame.";

/// t2v vs i2v vs continuation vs v2v framing — the core distinction the guide draws. This is the
/// ONLY place "what to cover" is stated: t2v lists the whole scene, the i2v family lists only
/// motion. Sound is never mentioned here — [`audio_clause`] owns it, so a supplied-audio shot is
/// never told to think about sound in two places.
fn mode_clause(mode: PromptMode) -> String {
    match mode {
        PromptMode::T2v => " This is text-to-video: you build the whole scene, so detail is your \
             only lever. Cover the framing and style, the lighting and mood, the action from start \
             to finish, each character's age, hair and clothing, and how the camera moves."
            .to_string(),
        PromptMode::I2v => format!(
            " This is image-to-video: the opening frame is already fixed. Do NOT describe how \
             anything looks — not appearance, clothing, colour, setting or lighting. Describe ONLY \
             what changes from that frame on: how the subject moves and gestures, and how the \
             camera moves. If several frames are attached, describe just the motion that carries \
             the subject from the first frame to the last.{I2V_EXAMPLE}"
        ),
        PromptMode::Continuation => format!(
            " This shot continues straight out of the previous shot, whose final frame is already \
             on screen as your opening frame. Do NOT re-describe that scene, its characters or its \
             setting — describe ONLY what happens next: the movement, the gesture, and how the \
             camera moves.{I2V_EXAMPLE}"
        ),
        PromptMode::V2v => " This is video-to-video: a source clip already defines the scene. \
             Describe only the change to apply and the motion, not the existing scene from scratch."
            .to_string(),
    }
}

/// Audio framing. Native-AV (nothing supplied) ⇒ describe sound + quoted, beat-broken dialogue.
/// Supplied audio ⇒ describe only the visuals, never narrate sound. This is the SOLE place sound
/// is mentioned to the model (the mode clauses deliberately say nothing about it).
pub fn audio_clause(audio_supplied: bool) -> &'static str {
    if audio_supplied {
        " The soundtrack is supplied separately — describe only the visuals, never the sound."
    } else {
        " The model makes its own audio: describe ambient sound and music, and put any speech or \
         singing in quotation marks, split into short phrases with an acting direction between \
         beats, noting the voice and accent."
    }
}

/// Whether anyone is audibly speaking or singing DURING this shot, when we can tell from the
/// transcript lane.
///
/// Without this the writer reaches for "singing" or "talking" as generic liveliness and the render
/// comes back with a mouth flapping over an instrumental break or a silence between lines. The
/// transcript is machine-heard and often wrong about WORDS, but its timing is reliable enough to
/// answer "is anyone making mouth-sounds here at all", which is the only thing being asked.
///
/// `None` = unknown (no transcript at all): say nothing rather than guess, so a shot with no
/// transcript lane drafts exactly as it always did.
pub fn speech_clause(speech_in_shot: Option<bool>) -> &'static str {
    match speech_in_shot {
        Some(true) => {
            " Someone IS audibly speaking or singing across this shot, so a character's mouth \
             moving is correct here."
        }
        Some(false) => {
            " NOBODY is speaking or singing during this shot — it falls in a gap between lines, or \
             over instrumental or ambient audio. Do not write anyone singing, talking, mouthing \
             words or lip syncing; give them a closed, still mouth and let the shot carry itself on \
             action and expression."
        }
        None => "",
    }
}

/// Render a project's [`StoryContext`] as a system-prompt block the LLM must treat as canon.
/// Returned empty when the context is empty (caller skips injection). Prepend this ahead of the
/// craft rules so the established world/characters lead. The paired reference images (from
/// [`story_context_images`]) carry the visual identity.
pub fn story_context_block(ctx: &StoryContext) -> String {
    if ctx.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "PROJECT CANON — treat every detail below (and in any attached reference images labelled \
         as references) as established fact. Describe characters, objects and settings exactly as \
         specified; never invent or contradict their identity or appearance. Expand this canon \
         into vivid cinematic detail, but do not fabricate facts that aren't given.",
    );
    let backstory = ctx.backstory.trim();
    if !backstory.is_empty() {
        s.push_str("\nBackstory / scenario: ");
        s.push_str(backstory);
    }
    let tone = ctx.tone.trim();
    if !tone.is_empty() {
        s.push_str("\nTone / visual style: ");
        s.push_str(tone);
    }
    let rules = ctx.rules.trim();
    if !rules.is_empty() {
        s.push_str("\nRules (always honour): ");
        s.push_str(rules);
    }
    let named: Vec<String> = ctx
        .references
        .iter()
        .filter(|r| !r.label.trim().is_empty() || !r.description.trim().is_empty())
        .map(|r| {
            let label = r.label.trim();
            let desc = r.description.trim();
            let img = if r.image_b64.is_some() {
                " (reference image attached)"
            } else {
                ""
            };
            match (label.is_empty(), desc.is_empty()) {
                (false, false) => format!("- {label}: {desc}{img}"),
                (false, true) => format!("- {label}{img}"),
                (true, false) => format!("- {desc}{img}"),
                (true, true) => String::new(),
            }
        })
        .filter(|l| !l.is_empty())
        .collect();
    if !named.is_empty() {
        s.push_str("\nCharacters & references:\n");
        s.push_str(&named.join("\n"));
    }
    s
}

/// The VL `image_url` content parts for a [`StoryContext`]'s reference images. These are
/// IDENTITY images (describe the subject to match them) — the caller must label them distinctly
/// from a shot's own motion-anchor frames.
pub fn story_context_images(ctx: &StoryContext) -> Vec<Value> {
    ctx.references
        .iter()
        .filter_map(|r| r.image_b64.as_deref())
        .map(|b64| b64.trim())
        .filter(|b64| !b64.is_empty())
        .map(|b64| json!({"type": "image_url", "image_url": {"url": story_data_uri(b64)}}))
        .collect()
}

/// True when any reference carries an image — used to add the "reference images define the look"
/// clause to a system prompt.
pub fn story_has_images(ctx: &StoryContext) -> bool {
    ctx.references
        .iter()
        .any(|r| r.image_b64.as_deref().is_some_and(|b| !b.trim().is_empty()))
}

fn story_data_uri(b64: &str) -> String {
    if b64.starts_with("data:") {
        b64.to_string()
    } else {
        format!("data:image/png;base64,{b64}")
    }
}

/// Prompt Relay framing for a shot that carries beats: the prompt must be the SETTING ONLY.
///
/// The beats are conditioned separately and pinned to their own moments, and the prompt is the
/// zero-penalty global anchor that conditions every frame. Narrating the beats in the prompt as well
/// re-averages precisely what the beats were added to separate — so a beat-unaware writer actively
/// undoes the feature. Empty when the shot has no beats, which keeps every ordinary draft
/// byte-identical.
pub fn relay_clause(beats: &[koblem_client::types::LtxBeat]) -> String {
    if beats.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        " This shot's ACTIONS are supplied separately as timed beats, listed below, and are \
         conditioned at their own moments. Write ONLY the SETTING that holds for the whole shot — \
         who is in frame, where, the light, the lens, the camera behaviour. Do NOT narrate the \
         beats, do not sequence them, and do not use words like \"then\", \"before\" or \"after\": \
         anything you write applies to EVERY frame equally, so a described action fights the beat \
         that owns it. Timed beats (for context — do not restate them):",
    );
    for beat in beats {
        s.push_str(&format!(" [{:.2}s] {};", beat.seconds, beat.text.trim()));
    }
    s
}

/// Read a `beats` array out of a drafted JSON reply.
///
/// Everything here is defensive: a local model will occasionally emit a beat past the end of the
/// shot, a beat with no text, or four when it was asked for three. A malformed beat is dropped
/// rather than clamped into something the director never asked for, and the whole list is capped at
/// the three the editor can show. Times are clamped inside the shot because a beat at or past the
/// final frame has no frames left to act on.
pub fn parse_proposed_beats(
    parsed: &Value,
    seconds: Option<f64>,
) -> Vec<koblem_client::types::LtxBeat> {
    let span = seconds.filter(|s| *s > 0.0).unwrap_or(5.0);
    let Some(arr) = parsed.get("beats").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<koblem_client::types::LtxBeat> = arr
        .iter()
        .filter_map(|b| {
            let text = b.get("text").and_then(Value::as_str)?.trim();
            if text.is_empty() {
                return None;
            }
            let secs = b.get("seconds").and_then(Value::as_f64)?;
            if !secs.is_finite() || secs < 0.0 {
                return None;
            }
            Some(koblem_client::types::LtxBeat {
                seconds: secs.clamp(0.0, (span - 0.1).max(0.0)),
                text: text.to_string(),
                strength: None,
                window: None,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a.seconds
            .partial_cmp(&b.seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(3);
    out
}

/// Ask the writer to PROPOSE beats alongside the prompt, for a shot that has none yet.
///
/// The two halves of authoring have to move together: [`relay_clause`] tells a writer that already
/// HAS beats to write the setting only, but nothing was ever asking for beats in the first place,
/// so a shot drafted from a one-line idea came back as a single flat prompt with every action
/// crammed into it — the exact averaging Prompt Relay exists to undo.
///
/// Times are shot-relative seconds. The back-half preference is measured, not taste: an event asked
/// for in the first second or two of a shot lands maybe half the time, while one in the back half
/// has landed every time.
pub fn beat_proposal_clause(seconds: Option<f64>) -> String {
    let span = seconds.filter(|s| *s > 0.0).unwrap_or(5.0);
    format!(
        "\n\nIf — and ONLY if — this shot contains a distinct moment WHEN something changes, rather \
         than one continuous action, also return \"beats\": a list of up to three \
         {{\"seconds\": <number>, \"text\": \"<a few words naming only the change>\"}}. Seconds are \
         measured from the START of this shot, which runs about {span:.0}s. Each beat is conditioned \
         separately at its own moment, so a beat must name ONLY what changes (\"the cat lands on his \
         shoulder\", \"she sets the glass down\") and must never repeat the setting — and the prompt \
         itself must then describe the SETTING ONLY, with none of the beats narrated in it. One beat \
         is perfectly good and often the best answer. Place a beat in the BACK HALF of the shot \
         where the story allows; an action asked for in the first second or two is much less likely \
         to land. Return \"beats\": [] for a shot that is one uninterrupted action — most shots are."
    )
}

/// Cast discipline: the PROMPT-side half of character identity, and the half that has nothing to do
/// with reference images.
///
/// The reference conditions the picture; the prompt is what tells the model WHICH anchor a shot is
/// about. In the Echo memory regime there is no per-character token at all — every pinned reference
/// is in context for every shot and routing happens purely through the appearance wording — so a
/// paraphrase between shots is not a stylistic variation, it is a different character. Upstream's
/// own shot writer therefore fixes one base identity sentence per character and repeats it
/// character-for-character in every shot, varying only a separate expression sentence. This is that
/// rule, stated to our writer.
///
/// Empty for an empty cast, so every existing draft is byte-identical.
pub fn cast_clause(cast: &[LtxCastMember]) -> String {
    let live: Vec<&LtxCastMember> = cast.iter().filter(|c| !c.is_empty()).collect();
    if live.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        " RECURRING CAST — identity is held by exact repetition, so treat the sentences below as \
         fixed text, not as a description to re-express. For each character listed: call them by \
         the SAME name every time, and reproduce their appearance sentence CHARACTER FOR CHARACTER \
         — same words, same order, same punctuation — in every shot they appear in. Do not \
         paraphrase it, shorten it, merge it into another sentence, or swap in a synonym; a \
         reworded appearance is read as a DIFFERENT person and identity breaks at that shot. The \
         appearance sentence carries only stable looks (age, build, hair, face, clothing) and never \
         mood: write expression, gaze, posture and emotion as a SEPARATE sentence after it, and \
         vary that one freely. Never give a character a second name or an alternative description. \
         A character not listed here is not in this shot.",
    );
    for c in live {
        let name = c.name.trim();
        let appearance = c.appearance.trim();
        let name = if name.is_empty() { "(unnamed)" } else { name };
        if appearance.is_empty() {
            // No sentence to hold yet — the name is still worth fixing so shots stop inventing
            // synonyms for the same person.
            s.push_str(&format!(
                " {name}: use this name; no fixed appearance sentence yet."
            ));
        } else {
            s.push_str(&format!(
                " {name} — appearance sentence (verbatim): \"{appearance}\""
            ));
        }
    }
    s
}

/// Everything about one shot that shapes its system prompt. `new` gives the neutral shot — no
/// audio, no references, no beats, no cast, nothing known about continuation or speech — so a
/// caller (and every test) states only what it actually varies.
pub struct ShotPrompt<'a> {
    pub mode: PromptMode,
    pub seconds: Option<f64>,
    pub audio_supplied: bool,
    pub has_refs: bool,
    pub closes_scene: Option<bool>,
    pub beats: &'a [koblem_client::types::LtxBeat],
    pub cast: &'a [LtxCastMember],
    pub speech_in_shot: Option<bool>,
}

impl<'a> ShotPrompt<'a> {
    pub fn new(mode: PromptMode) -> Self {
        Self {
            mode,
            seconds: None,
            audio_supplied: false,
            has_refs: false,
            closes_scene: None,
            beats: &[],
            cast: &[],
            speech_in_shot: None,
        }
    }
}

/// System prompt for the single-shot writer (`prompt-from-llm`).
pub fn shot_system_prompt(p: ShotPrompt<'_>) -> String {
    let ShotPrompt {
        mode,
        seconds,
        audio_supplied,
        has_refs,
        closes_scene,
        beats,
        cast,
        speech_in_shot,
    } = p;
    let mut s = String::from("You write a single prompt for the LTX-2.5 AI video generator. ");
    s.push_str(BASE);
    s.push_str(&mode_clause(mode));
    s.push_str(audio_clause(audio_supplied));
    s.push_str(speech_clause(speech_in_shot));
    s.push_str(&duration_clause(seconds));
    if let Some(closes) = closes_scene {
        s.push_str(continuation_clause(closes));
    }
    s.push_str(&relay_clause(beats));
    s.push_str(&cast_clause(cast));
    if has_refs {
        s.push_str(
            " Reference images are attached; keep the drafted prompt consistent with what they show.",
        );
    }
    s.push_str(" Reply with ONLY the prompt paragraph — no preamble, no quotes, no commentary.");
    s
}

/// System prompt for the multi-shot script writer (`script-from-llm`). Emits a strict JSON array so
/// multi-sentence paragraphs survive parsing (line-splitting would shatter them).
pub fn script_system_prompt(
    n: u32,
    seconds: Option<f64>,
    audio_supplied: bool,
    has_opening_image: bool,
    cast: &[LtxCastMember],
) -> String {
    let mut s = format!(
        "You write shot prompts for the LTX-2.5 AI video generator. Break the brief into {n} \
         shots that tell the story in order, one detailed prompt per shot. "
    );
    s.push_str(BASE);
    if has_opening_image {
        s.push_str(
            " The attached image is the opening frame: the first shot continues directly from it. \
             Do not re-describe the static image — describe what happens and how the camera moves.",
        );
    } else {
        s.push_str(&mode_clause(PromptMode::T2v));
    }
    // The shots form a continuation chain: each one after the first (bar a hard cut) renders from
    // the previous shot's final frame, so it should describe the change, not the standing scene,
    // and must end on a frame still showing the whole scene for the next shot to continue from.
    s.push_str(
        " After the first shot, each shot continues from the previous shot's final frame — describe \
         what changes rather than the standing scene, and end each shot on a frame that still shows \
         the whole scene so the next one can continue from it.",
    );
    s.push_str(audio_clause(audio_supplied));
    s.push_str(&duration_clause(seconds));
    s.push_str(&cast_clause(cast));
    s.push_str(
        " Reply with STRICT JSON only, no code fences and no prose: \
         {\"shots\":[\"<full prompt paragraph>\", ...]}.",
    );
    s
}

/// Format attached images into positional context, e.g.
/// `Image 1 = opening frame; Image 2 = keyframe at 2.0s; Image 3 = final frame`.
pub fn label_images(images: &[(&str, Option<f64>)]) -> String {
    let mut out = String::new();
    for (i, (role, secs)) in images.iter().enumerate() {
        if i > 0 {
            out.push_str("; ");
        }
        let name = match role.trim().to_ascii_lowercase().as_str() {
            "opening" | "scene" | "" => "opening frame".to_string(),
            "keyframe" | "pin" => "keyframe".to_string(),
            "end" | "final" => "final frame".to_string(),
            other => other.to_string(),
        };
        out.push_str(&format!("Image {} = {name}", i + 1));
        if let Some(s) = secs {
            out.push_str(&format!(" at {s:.1}s"));
        }
    }
    out
}

/// User-prompt clause for per-shot visual references ("a shot like this"). Returned empty for
/// `count == 0` (caller skips it). These images follow the shot's own frames in the content list
/// (and precede any story-canon images), so the wording tells the model they are style/composition
/// targets, NOT frames of the shot.
pub fn reference_clause(count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let noun = if count == 1 {
        "image is a visual reference"
    } else {
        "images are visual references"
    };
    format!(
        "\n\nThe next {count} attached {noun} the user likes for this shot — match their framing, \
         composition, lighting and mood. They are NOT frames of this shot and are never rendered; \
         do not copy their exact subjects, and keep everything consistent with the established canon."
    )
}

/// Parse an LLM reply into up to `n` shot prompts. Prefers a JSON `{"shots":[...]}` object (or a
/// bare `[...]` array; elements may be plain strings or `{"prompt":...}` objects); falls back to
/// one shot per non-empty line (stripping list markers + any "Here are the shots:" preamble).
pub fn parse_shots(text: &str, n: usize) -> Vec<String> {
    if let Some(shots) = parse_shots_json(text) {
        return shots
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .take(n)
            .collect();
    }
    text.lines()
        .map(|l| strip_list_marker(l.trim()).to_string())
        .filter(|l| !l.is_empty() && !l.ends_with(':'))
        .take(n)
        .collect()
}

fn parse_shots_json(text: &str) -> Option<Vec<String>> {
    let t = text.trim();
    let val: Value = if t.starts_with('[') {
        serde_json::from_str(t).ok()?
    } else {
        serde_json::from_str(extract_json_object(t)).ok()?
    };
    let arr = match val {
        Value::Array(a) => a,
        Value::Object(ref o) => o.get("shots")?.as_array()?.clone(),
        _ => return None,
    };
    let out: Vec<String> = arr.iter().filter_map(shot_string).collect();
    if out.is_empty() { None } else { Some(out) }
}

/// A shot list element may be a plain string or an object carrying a `prompt` field.
fn shot_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("prompt").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Clean a single-paragraph prompt reply: strip code fences, surrounding quotes, and a leading
/// "Prompt:"-style label line the model sometimes adds despite instructions.
pub fn clean_prompt(text: &str) -> String {
    let mut t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // Drop the opening fence (and an optional language tag on its line) + the closing fence.
        let rest = rest.split_once('\n').map(|x| x.1).unwrap_or(rest);
        t = rest.trim_end_matches("```").trim();
    }
    // Drop a short leading label line like "Prompt:" / "Here is the prompt:".
    if let Some((first, rest)) = t.split_once('\n') {
        let f = first.trim();
        if f.ends_with(':') && f.len() <= 40 {
            t = rest.trim();
        }
    }
    t.trim().trim_matches('"').trim().to_string()
}

/// Extract the first balanced `{ … }` JSON object from a reply that may wrap it in ```json fences
/// or prose. Returns the slice, or the trimmed whole string when no object is found.
pub fn extract_json_object(s: &str) -> &str {
    let bytes = s.as_bytes();
    let Some(start) = s.find('{') else {
        return s.trim();
    };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &s[start..=i];
                }
            }
            _ => {}
        }
    }
    &s[start..]
}

/// Recover every COMPLETE top-level element of the first JSON array in `s`, even when the array was
/// truncated mid-stream (an LLM reply cut off by `max_tokens`, or trailing prose). Walks the array
/// body tracking string/brace/bracket state, parses each element between depth-0 commas, and drops
/// the final partial element. Handles object, string and number elements. Returns `[]` if no array.
///
/// Lets batch handlers degrade a truncated `{"items":[{…},{…},{…` into "keep the N complete items"
/// instead of failing the whole request.
pub fn salvage_array_elements(s: &str) -> Vec<serde_json::Value> {
    let Some(open) = s.find('[') else {
        return Vec::new();
    };
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut i = open + 1;
    let mut elem_start: Option<usize> = None;
    let mut depth = 0i32; // nesting of {}/[] INSIDE the array
    let mut in_str = false;
    let mut esc = false;

    let flush = |from: usize, to: usize, out: &mut Vec<serde_json::Value>| {
        let piece = s[from..to].trim();
        if !piece.is_empty()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(piece)
        {
            out.push(v);
        }
    };

    while i < n {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                if elem_start.is_none() {
                    elem_start = Some(i);
                }
                in_str = true;
            }
            b'{' | b'[' => {
                if elem_start.is_none() {
                    elem_start = Some(i);
                }
                depth += 1;
            }
            b']' if depth == 0 => {
                if let Some(st) = elem_start.take() {
                    flush(st, i, &mut out);
                }
                return out; // clean end of array
            }
            b'}' | b']' => depth -= 1,
            b',' if depth == 0 => {
                if let Some(st) = elem_start.take() {
                    flush(st, i, &mut out);
                }
            }
            b' ' | b'\n' | b'\t' | b'\r' => {}
            _ => {
                if elem_start.is_none() {
                    elem_start = Some(i);
                }
            }
        }
        i += 1;
    }
    // Truncated (no closing ']') — flush the trailing element only if it happens to parse whole.
    if let Some(st) = elem_start.take() {
        flush(st, n, &mut out);
    }
    out
}

/// The text that trails the first balanced `{…}` object in `s` (trimmed). Empty when the object
/// runs to the end, or there is no object. Lets the chat parser recover an answer a model dumped
/// AFTER closing a short JSON reply (e.g. `{"reply":"Here are the locations:"}` then the actual list).
pub fn text_after_json_object(s: &str) -> &str {
    if !s.contains('{') {
        return "";
    }
    let obj = extract_json_object(s);
    // `obj` is a subslice of `s`, so its start pointer sits inside `s`'s buffer — the offset is exact.
    let end = (obj.as_ptr() as usize - s.as_ptr() as usize) + obj.len();
    if end >= s.len() {
        return "";
    }
    s[end..].trim()
}

/// Leniently pull a JSON string field's value out of text that may not be valid JSON — e.g. an LLM
/// reply whose string contains a raw newline or unescaped quote (which `serde_json` rejects), so the
/// user never sees a raw `{"reply":"…"}` blob. Finds `"<key>"`, skips `:` and whitespace, then reads
/// the quoted value, decoding `\n \t \r \" \\` and tolerating a truncated (unterminated) string.
pub fn lenient_json_string_field(s: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = s.find(&needle)? + needle.len();
    let after = s[idx..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    let inner = after.strip_prefix('"')?;
    let mut out = String::new();
    let mut esc = false;
    for c in inner.chars() {
        if esc {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    Some(out) // unterminated (truncated) — return what we have
}

/// Strip a single leading enumeration marker (`1.`, `2)`, `-`, `*`, `•`) without eating a natural
/// leading number like "3 dancers spin…".
pub fn strip_list_marker(s: &str) -> &str {
    let s = s.trim_start();
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0
        && let Some(rest) = s[digits..]
            .strip_prefix('.')
            .or_else(|| s[digits..].strip_prefix(')'))
    {
        return rest.trim_start();
    }
    s.strip_prefix(['-', '*', '•'])
        .map(|r| r.trim_start())
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koblem_client::types::StoryReference;

    #[test]
    fn story_context_block_empty_is_empty() {
        assert!(story_context_block(&StoryContext::default()).is_empty());
        assert!(!story_has_images(&StoryContext::default()));
        assert!(story_context_images(&StoryContext::default()).is_empty());
    }

    #[test]
    fn salvage_full_array_objects() {
        let s = r#"{"shots":[{"prompt":"a"},{"prompt":"b"},{"prompt":"c"}]}"#;
        let items = salvage_array_elements(s);
        assert_eq!(items.len(), 3);
        assert_eq!(items[2]["prompt"], "c");
    }

    #[test]
    fn salvage_truncated_array_keeps_complete_prefix() {
        // JSON cut off mid-object (hit max_tokens): keep the two complete entries, drop the partial.
        let s = r#"{"shots":[{"prompt":"one"},{"prompt":"two"},{"prompt":"thr"#;
        let items = salvage_array_elements(s);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["prompt"], "one");
        assert_eq!(items[1]["prompt"], "two");
    }

    #[test]
    fn salvage_string_array_and_brace_in_string() {
        // String elements + a brace/bracket inside a string must not confuse the walker.
        let s = r#"{"gists":["he says {hi}","she [waves]","cut"]}"#;
        let items = salvage_array_elements(s);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], "he says {hi}");
        assert_eq!(items[1], "she [waves]");
    }

    #[test]
    fn salvage_no_array_is_empty() {
        assert!(salvage_array_elements("sorry, I can't do that").is_empty());
    }

    #[test]
    fn lenient_reply_extracts_from_invalid_json() {
        // Raw newline in the value makes this invalid JSON (serde rejects it) — must still recover.
        let s = "{\"reply\": \"Sure!\nDarkened shot 3.\", \"actions\": []}";
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(
            lenient_json_string_field(s, "reply").as_deref(),
            Some("Sure!\nDarkened shot 3.")
        );
    }

    #[test]
    fn text_after_json_object_recovers_trailing_list() {
        // The exact failure: model closes a short reply, then dumps the real answer after the brace.
        let s = "{\"reply\": \"Here are the locations:\"}\n1. desert\n2. city rooftop";
        assert_eq!(text_after_json_object(s), "1. desert\n2. city rooftop");
        // Object runs to the end → no trailing.
        assert_eq!(text_after_json_object(r#"{"reply":"hi","actions":[]}"#), "");
        // No object at all → no trailing.
        assert_eq!(text_after_json_object("just prose"), "");
    }

    #[test]
    fn lenient_reply_handles_escapes_and_truncation() {
        assert_eq!(
            lenient_json_string_field(r#"{"reply":"a \"quote\" ok"}"#, "reply").as_deref(),
            Some("a \"quote\" ok")
        );
        // Truncated mid-string: return the partial rather than nothing.
        assert_eq!(
            lenient_json_string_field("{\"reply\":\"cut off here", "reply").as_deref(),
            Some("cut off here")
        );
        assert_eq!(
            lenient_json_string_field(r#"{"actions":[]}"#, "reply"),
            None
        );
    }

    #[test]
    fn story_context_block_populated_carries_canon_and_refs() {
        let ctx = StoryContext {
            backstory: "A derelict Mars colony run by one lonely android.".into(),
            tone: "melancholy, rust-orange, Super-8 grain".into(),
            rules: "never show human faces".into(),
            references: vec![
                StoryReference {
                    label: "ARIA-7".into(),
                    description: "a brass android with one cracked green optic".into(),
                    image_b64: Some("Zm9v".into()),
                },
                StoryReference::default(), // ignored (all-empty)
            ],
        };
        let block = story_context_block(&ctx);
        assert!(block.contains("PROJECT CANON"));
        assert!(block.contains("never invent or contradict"));
        assert!(block.contains("derelict Mars colony"));
        assert!(block.contains("Super-8 grain"));
        assert!(block.contains("never show human faces"));
        assert!(block.contains("ARIA-7"));
        assert!(block.contains("cracked green optic"));
        assert!(story_has_images(&ctx));
        assert_eq!(story_context_images(&ctx).len(), 1);
    }

    #[test]
    fn reference_clause_present_only_when_refs_supplied() {
        assert!(reference_clause(0).is_empty());
        let one = reference_clause(1);
        assert!(one.contains("next 1 attached image is a visual reference"));
        assert!(one.contains("NOT frames of this shot"));
        let many = reference_clause(3);
        assert!(many.contains("next 3 attached images are visual references"));
    }

    #[test]
    fn cast_clause_is_empty_without_a_cast() {
        // Every existing draft goes through here — an empty roster must add nothing at all.
        assert!(cast_clause(&[]).is_empty());
        assert!(
            cast_clause(&[LtxCastMember::default()]).is_empty(),
            "a member with neither a name nor an appearance is nothing to hold"
        );
        assert!(
            !shot_system_prompt(ShotPrompt {
                seconds: Some(5.0),
                ..ShotPrompt::new(PromptMode::T2v)
            })
            .contains("RECURRING CAST")
        );
    }

    #[test]
    fn cast_clause_states_the_verbatim_rule_and_lists_each_member() {
        let cast = vec![
            LtxCastMember {
                name: "Mia".into(),
                appearance: "Mia is a woman in her twenties with dark curly hair.".into(),
                source_id: Some(2),
            },
            LtxCastMember {
                name: "Sol".into(),
                appearance: String::new(),
                source_id: Some(3),
            },
        ];
        let s = cast_clause(&cast);
        // The rule the whole mechanism rests on, and the separation that lets a shot still act.
        assert!(s.contains("CHARACTER FOR CHARACTER"));
        assert!(s.contains("paraphrase"));
        assert!(s.contains("SEPARATE sentence"));
        assert!(s.contains("Mia is a woman in her twenties with dark curly hair."));
        // A member with a name but no sentence yet still fixes the name.
        assert!(s.contains("Sol: use this name"));
        // And it reaches the writers.
        assert!(
            shot_system_prompt(ShotPrompt {
                seconds: Some(5.0),
                cast: &cast,
                ..ShotPrompt::new(PromptMode::T2v)
            })
            .contains("RECURRING CAST")
        );
        assert!(script_system_prompt(4, Some(4.0), false, false, &cast).contains("RECURRING CAST"));
    }

    #[test]
    fn t2v_and_i2v_guidance_differ() {
        let t2v = shot_system_prompt(ShotPrompt {
            seconds: Some(6.0),
            ..ShotPrompt::new(PromptMode::T2v)
        });
        let i2v = shot_system_prompt(ShotPrompt {
            seconds: Some(6.0),
            ..ShotPrompt::new(PromptMode::I2v)
        });
        let cont = shot_system_prompt(ShotPrompt {
            seconds: Some(6.0),
            ..ShotPrompt::new(PromptMode::Continuation)
        });
        // t2v paints the whole scene; the i2v family must NOT re-describe the fixed frame.
        assert!(t2v.contains("text-to-video"));
        assert!(t2v.contains("build the whole scene"));
        assert!(
            !t2v.contains("what happens next") && !t2v.contains("what changes from that frame")
        );
        assert!(i2v.contains("image-to-video"));
        assert!(i2v.contains("Do NOT describe how anything looks"));
        assert!(i2v.contains("what changes from that frame on"));
        // Multi-frame (start + end pin) case: motion-only between the frames.
        assert!(i2v.contains("from the first frame to the last"));
        // Continuation is i2v anchored on the previous shot, not an attached image.
        assert!(cont.contains("continues straight out of the previous shot"));
        assert!(cont.contains("Do NOT re-describe that scene"));
        // The one worked example rides with both i2v-family clauses, never with t2v.
        assert!(
            i2v.contains("only restates the frame") && cont.contains("only restates the frame")
        );
        assert!(!t2v.contains("only restates the frame"));
        // Sound lives ONLY in the audio clause — the mode clauses never mention it (no double-billing
        // that a supplied-audio shot would have to reconcile). Tested on the mode clause in isolation,
        // since the full prompt always appends the audio clause.
        assert!(!mode_clause(PromptMode::I2v).contains("sound"));
        assert!(!mode_clause(PromptMode::Continuation).contains("sound"));
        let t = mode_clause(PromptMode::T2v);
        assert!(!t.contains("audio") && !t.contains("sound"));
        // All carry the shared base rules (single paragraph + physical cues).
        for p in [&t2v, &i2v, &cont] {
            assert!(p.contains("ONE flowing paragraph"));
            assert!(p.contains("physical cues"));
        }
    }

    #[test]
    fn relay_clause_only_appears_with_beats() {
        let plain = shot_system_prompt(ShotPrompt {
            seconds: Some(5.0),
            ..ShotPrompt::new(PromptMode::T2v)
        });
        assert!(!plain.contains("timed beats"));
        assert!(!plain.contains("SETTING"));

        let beats = vec![
            koblem_client::types::LtxBeat {
                seconds: 0.5,
                text: "she raises a glass and drinks".into(),
                ..Default::default()
            },
            koblem_client::types::LtxBeat {
                seconds: 2.5,
                text: "she turns to the door".into(),
                ..Default::default()
            },
        ];
        let relay = shot_system_prompt(ShotPrompt {
            seconds: Some(5.0),
            beats: &beats,
            ..ShotPrompt::new(PromptMode::T2v)
        });
        // The writer must be told to hold the setting and NOT to sequence the actions — a prompt
        // that narrates the beats re-averages exactly what the beats separated.
        assert!(relay.contains("ONLY the SETTING"));
        assert!(relay.contains("do not sequence them"));
        // The beats themselves ride along as context, with their times.
        assert!(relay.contains("[0.50s] she raises a glass and drinks"));
        assert!(relay.contains("[2.50s] she turns to the door"));
    }

    #[test]
    fn audio_clause_toggles() {
        let native = shot_system_prompt(ShotPrompt::new(PromptMode::T2v));
        let supplied = shot_system_prompt(ShotPrompt {
            audio_supplied: true,
            ..ShotPrompt::new(PromptMode::T2v)
        });
        assert!(native.contains("quotation marks"));
        assert!(native.contains("makes its own audio"));
        assert!(supplied.contains("never the sound"));
        assert!(!supplied.contains("quotation marks"));
    }

    #[test]
    fn detail_scales_to_duration() {
        let long = shot_system_prompt(ShotPrompt {
            seconds: Some(8.0),
            ..ShotPrompt::new(PromptMode::T2v)
        });
        assert!(long.contains("8s"));
        assert!(long.contains("fill the whole"));
        let none = shot_system_prompt(ShotPrompt::new(PromptMode::T2v));
        assert!(none.contains("underfilled"));
    }

    #[test]
    fn continuation_clause_flips_on_scene_close() {
        let cont = continuation_clause(false);
        let closes = continuation_clause(true);
        // A continuing shot must keep the scene in the last frame (the next shot's only view); a
        // closing shot must NOT be told to.
        assert!(cont.contains("continues from this shot's final frame"));
        assert!(cont.contains("the ONLY thing"));
        assert!(cont.contains("stay in view"));
        assert!(closes.contains("ends its scene"));
        assert!(closes.contains("lands the moment"));
        assert!(!closes.contains("stay in view"));
    }

    #[test]
    fn multi_shot_pacing_carries_both_branches_and_matches_tags() {
        let p = multi_shot_pacing();
        assert!(p.contains("CONTINUES:"));
        assert!(p.contains("CLOSES:"));
        assert!(p.contains("Pace each prompt to its OWN shot's length"));
        // The rules are composed from the single-shot clause — no drifting second copy.
        assert!(p.contains(continuation_clause(false)));
        assert!(p.contains(continuation_clause(true)));
        // The tag vocabulary the rules describe must be the vocabulary the lines actually carry.
        // `carried` (not the prefix-prone `cont`) is the continuation token; keep the two in sync.
        assert!(p.contains("`carried`"));
        assert!(!p.contains("`cont`"));
        assert_eq!(shot_pacing_tag(Some(6.0), false), " · 6s · CONTINUES");
        assert_eq!(shot_pacing_tag(Some(2.4), true), " · 2s · CLOSES");
        assert_eq!(shot_pacing_tag(None, false), " · CONTINUES");
        assert_eq!(shot_pacing_tag(Some(0.0), true), " · CLOSES");
    }

    #[test]
    fn script_prompt_requests_json_and_opening_frame() {
        let with_img = script_system_prompt(4, Some(4.0), false, true, &[]);
        assert!(with_img.contains("\"shots\""));
        assert!(with_img.contains("opening frame"));
        assert!(with_img.contains("re-describe the static image"));
        // The batch tells the shots they form a continuation chain, single-sourced from the clause.
        assert!(with_img.contains("each shot continues from the previous shot's final frame"));
        let no_img = script_system_prompt(4, Some(4.0), true, false, &[]);
        assert!(no_img.contains("build the whole scene"));
        assert!(no_img.contains("never the sound"));
    }

    #[test]
    fn mode_resolves_from_mode_images_and_continuation() {
        use PromptMode::*;
        // Auto/None infers from images, then from continuation, else t2v.
        assert_eq!(PromptMode::resolve(None, true, false), I2v);
        assert_eq!(PromptMode::resolve(None, false, false), T2v);
        assert_eq!(PromptMode::resolve(None, false, true), Continuation);
        assert_eq!(
            PromptMode::resolve(Some(LtxSegmentMode::Auto), true, false),
            I2v
        );
        // A plain t2v shot that CONTINUES is really i2v — it inherits the prior frame.
        assert_eq!(
            PromptMode::resolve(Some(LtxSegmentMode::T2v), false, true),
            Continuation
        );
        assert_eq!(
            PromptMode::resolve(Some(LtxSegmentMode::T2v), false, false),
            T2v
        );
        // An explicit i2v/v2v shot keeps its mode regardless of the continuation flag.
        assert_eq!(
            PromptMode::resolve(Some(LtxSegmentMode::I2v), false, true),
            I2v
        );
        assert_eq!(
            PromptMode::resolve(Some(LtxSegmentMode::V2vEdit), false, true),
            V2v
        );
    }

    #[test]
    fn label_images_formats_roles_and_positions() {
        let s = label_images(&[
            ("opening", None),
            ("keyframe", Some(2.0)),
            ("end", Some(4.0)),
        ]);
        assert_eq!(
            s,
            "Image 1 = opening frame; Image 2 = keyframe at 2.0s; Image 3 = final frame at 4.0s"
        );
    }

    #[test]
    fn parse_shots_json_object() {
        let text = r#"{"shots":["A wide shot of a city.","A close-up of a face."]}"#;
        let got = parse_shots(text, 8);
        assert_eq!(got, vec!["A wide shot of a city.", "A close-up of a face."]);
    }

    #[test]
    fn parse_shots_bare_array_and_prompt_objects() {
        assert_eq!(parse_shots(r#"["one","two"]"#, 8), vec!["one", "two"]);
        assert_eq!(
            parse_shots(r#"{"shots":[{"prompt":"alpha"},{"prompt":"beta"}]}"#, 8),
            vec!["alpha", "beta"]
        );
    }

    #[test]
    fn parse_shots_fenced_json() {
        let text = "```json\n{\"shots\":[\"first\",\"second\"]}\n```";
        assert_eq!(parse_shots(text, 8), vec!["first", "second"]);
    }

    #[test]
    fn parse_shots_line_fallback_and_cap() {
        let text = "Here are the shots:\n1. First shot here\n2) Second shot\n- Third shot";
        // Preamble line ending in ':' is dropped; list markers stripped; capped to n.
        assert_eq!(parse_shots(text, 2), vec!["First shot here", "Second shot"]);
    }

    #[test]
    fn clean_prompt_strips_fences_and_quotes() {
        assert_eq!(clean_prompt("\"a tidy prompt\""), "a tidy prompt");
        assert_eq!(
            clean_prompt("```\nfenced prompt body\n```"),
            "fenced prompt body"
        );
        assert_eq!(
            clean_prompt("Prompt:\nthe real prompt paragraph"),
            "the real prompt paragraph"
        );
    }
}
