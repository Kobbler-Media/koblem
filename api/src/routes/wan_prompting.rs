//! Wan 2.2 (VACE) prompt-writing guidance for koblem's Wan "draft with LLM" surfaces
//! (script-from-llm, per-shot prompt-from-llm). Encodes the Wan 2.2 prompt formula — motion-first,
//! concrete verbs, aesthetic control (light source, shot size, lens, camera angle + movement) — on
//! top of the VACE identity-continuity rule that keeps the render from drifting to a new subject.
//!
//! Wan differs from LTX (see routes/ltx_prompting.rs): it is video-only (no audio, no dialogue),
//! it wants "how things move, not what appears", and its i2v guidance is stricter — describe only
//! the motion/camera/environment and animate ONLY what is already visible in the frame. The
//! generic reply parsing / image labelling / prompt cleaning is reused from `ltx_prompting`.

/// The shared "how to write" rules for every Wan prompt — mode-neutral craft only, like
/// [`super::ltx_prompting::BASE`]. It says NOTHING about what to cover (that is mode-specific: t2v
/// paints the whole scene, i2v/continuation describe only motion) so the model is never told to
/// describe the standing scene and then told not to. Wan 2.2 is video-only — sound and dialogue
/// are never mentioned.
pub const BASE_WAN: &str = "Write ONE flowing paragraph of concrete motion in present tense. Wan \
2.2 responds to HOW things move, not a list of what appears — use strong action verbs (\"strides\", \
\"orbits\", \"sways\", \"shatters\") and give each motion an amplitude and speed (\"slowly\", \
\"violently\"), never vague words like \"moving\" or \"cinematic\". One clip is ONE continuous shot \
— no in-clip cuts or scene changes — so keep to one or two actions and one or two camera moves, \
with no contradictory directions. Never put on-screen text or logos in frame, and avoid extreme \
transformations and crowded multi-subject scenes.";

/// The load-bearing VACE clause: the continuation model drifts to a NEW subject if beats diverge,
/// so every shot must hold the exact same subject/appearance/setting.
pub const IDENTITY: &str = " Keep the exact same subject and appearance in every shot — the same \
person or character, the same clothing, colours and setting — simply continuing the action. Never \
introduce a new person, never change their clothing or identity, and never cut to a different \
scene or subject.";

/// A single worked contrast for Wan's i2v family — the single most important i2v rule is "don't
/// re-describe the frame". Appended to both the i2v and continuation clauses.
const WAN_I2V_EXAMPLE: &str = " For example — good: \"The woman turns toward the window and her \
smile fades as the camera slowly pushes in.\" Not: \"A woman in a red coat by a window in warm \
light\" — that only restates the frame.";

/// How a Wan shot treats its starting point: build from scratch, from an attached image, or as a
/// continuation of the previous shot's final frame.
#[derive(Clone, Copy)]
enum WanKind {
    T2v,
    I2v,
    Continuation,
}

/// t2v vs i2v vs continuation framing. This is the ONLY place "what to cover" is stated: t2v lists
/// the full Wan formula (subject → scene → motion → aesthetic control → style); the i2v family
/// lists only motion + camera, because the frame already carries everything else.
fn mode_clause(kind: WanKind) -> String {
    match kind {
        WanKind::T2v => " Text-to-video: build everything from the brief — there is no starting \
             frame, so detail is your only lever. Cover, in order: the subject and its motion; the \
             scene and its motion (light, atmosphere, particles); the camera move (push-in, \
             pull-out, pan, tilt, tracking, orbit, or a \"fixed camera\"); the aesthetic control — \
             light source, shot size, camera angle, lens and colour tone; and any overall style."
            .to_string(),
        WanKind::I2v => format!(
            " Image-to-video: the attached image already sets the subject, scene, lighting and \
             style, so do NOT re-describe what is in the frame. Write only the motion and the \
             camera move, and animate only what is already visible. For a still camera, say \
             \"fixed camera\".{WAN_I2V_EXAMPLE}"
        ),
        WanKind::Continuation => format!(
            " This shot continues straight out of the previous shot, whose final frame already sets \
             the subject, scene, lighting and style — do NOT re-describe it. Write only the NEW \
             motion and camera move: the next beat of the same action. For a still camera, say \
             \"fixed camera\".{WAN_I2V_EXAMPLE}"
        ),
    }
}

/// Duration scaling — a longer shot needs more sustained motion described.
fn duration_clause(seconds: Option<f64>) -> String {
    match seconds {
        Some(s) if s > 0.0 => format!(
            " This shot runs about {s:.0}s — describe enough continuous motion to fill it without \
             rushing or freezing."
        ),
        _ => " Describe a single continuous motion, richly and concretely.".to_string(),
    }
}

/// System prompt for the single-shot Wan writer (`prompt-from-llm`).
pub fn shot_system_prompt(
    mode: Option<koblem_client::types::WanVaceMode>,
    seconds: Option<f64>,
    has_image: bool,
) -> String {
    use koblem_client::types::WanVaceMode;
    // A continuation renders from the previous shot's final frame — it HAS a starting frame, so it
    // is i2v-like (describe the change), never t2v ("build the scene from scratch"). An attached
    // image wins, since there is a literal frame to animate.
    let kind = if has_image || matches!(mode, Some(WanVaceMode::I2v)) {
        WanKind::I2v
    } else if matches!(mode, Some(WanVaceMode::Continuation)) {
        WanKind::Continuation
    } else {
        WanKind::T2v
    };
    let mut s = String::from("You write a single prompt for the Wan 2.2 AI video generator. ");
    s.push_str(BASE_WAN);
    s.push_str(&mode_clause(kind));
    if matches!(kind, WanKind::Continuation) {
        s.push_str(IDENTITY);
    }
    s.push_str(&duration_clause(seconds));
    s.push_str(" Reply with ONLY the prompt paragraph — no preamble, no quotes, no commentary.");
    s
}

/// System prompt for the multi-shot Wan script writer (`script-from-llm`). Always continuation, so
/// the identity clause is mandatory. Emits a strict JSON array for robust parsing.
pub fn script_system_prompt(n: u32, seconds: Option<f64>, has_image: bool) -> String {
    let mut s = format!(
        "You write shot prompts for the Wan 2.2 (VACE) continuation video generator. Break the \
         brief into {n} shots that continue ONE unbroken action in order, one detailed prompt per \
         shot. "
    );
    s.push_str(BASE_WAN);
    if has_image {
        s.push_str(
            " The attached image is the opening frame — it sets the subject, clothing, colours, \
             style and setting. Do NOT re-describe it; describe the motion and camera that carry \
             the action forward.",
        );
    } else {
        s.push_str(&mode_clause(WanKind::T2v));
    }
    s.push_str(IDENTITY);
    // Each shot after the first continues from the previous shot's final frame — the next beat of
    // the same action, not a fresh scene.
    s.push_str(
        " After the first shot, each shot continues from the previous shot's final frame — describe \
         only the NEW motion and camera, the next beat of the same action, not the scene again.",
    );
    s.push_str(&duration_clause(seconds));
    s.push_str(
        " Reply with STRICT JSON only, no code fences and no prose: \
         {\"shots\":[\"<full prompt paragraph>\", ...]}.",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use koblem_client::types::WanVaceMode;

    #[test]
    fn i2v_and_t2v_differ() {
        let i2v = shot_system_prompt(Some(WanVaceMode::I2v), Some(3.0), true);
        let t2v = shot_system_prompt(Some(WanVaceMode::T2v), Some(3.0), false);
        // i2v: only motion + camera, never re-describe the frame. t2v: build the whole scene.
        assert!(i2v.contains("animate only what is already visible"));
        assert!(i2v.contains("do NOT re-describe"));
        assert!(i2v.contains("only restates the frame")); // the worked example
        assert!(t2v.contains("no starting frame"));
        assert!(t2v.contains("aesthetic control"));
        assert!(!t2v.contains("re-describe"));
        // Both carry the Wan motion-first craft rules.
        for p in [&i2v, &t2v] {
            assert!(p.contains("HOW things move"));
            assert!(p.contains("camera"));
        }
        // Sound lives nowhere — the mode clauses (like the whole prompt) never mention it.
        assert!(!mode_clause(WanKind::I2v).to_lowercase().contains("sound"));
    }

    #[test]
    fn continuation_is_treated_as_i2v_not_t2v() {
        // The bug this fixes: a continuation renders from the previous shot's final frame, so it
        // must NOT be told there is no starting frame / to build the scene from scratch.
        let cont = shot_system_prompt(Some(WanVaceMode::Continuation), Some(4.0), false);
        assert!(!cont.contains("no starting frame"));
        assert!(cont.contains("continues straight out of the previous shot"));
        assert!(cont.contains("do NOT re-describe"));
        assert!(cont.contains("only restates the frame")); // shares the i2v example
        // IDENTITY still rides along so the subject doesn't drift across the continuation.
        assert!(cont.contains("exact same subject"));
        assert!(cont.contains("Never introduce a new person"));
        // A plain t2v shot gets neither the continuation clause nor IDENTITY.
        let plain = shot_system_prompt(Some(WanVaceMode::T2v), Some(4.0), false);
        assert!(!plain.contains("Never introduce a new person"));
        assert!(!plain.contains("continues straight out"));
    }

    #[test]
    fn script_prompt_requests_json_and_identity() {
        let s = script_system_prompt(4, Some(3.0), true);
        assert!(s.contains("\"shots\""));
        assert!(s.contains("exact same subject"));
        assert!(s.contains("opening frame"));
        assert!(s.contains("continues from the previous shot's final frame"));
        assert!(s.contains("3s"));
    }

    #[test]
    fn no_audio_language_leaks_in() {
        // Wan 2.2 is video-only: the prompt guidance must never ask for sound/dialogue.
        for kind in [WanKind::T2v, WanKind::I2v, WanKind::Continuation] {
            let c = mode_clause(kind).to_lowercase();
            assert!(!c.contains("dialogue") && !c.contains("audio") && !c.contains("sound"));
        }
        let s = shot_system_prompt(Some(WanVaceMode::T2v), Some(4.0), false);
        assert!(!s.to_lowercase().contains("dialogue"));
        assert!(!s.to_lowercase().contains("audio"));
    }
}
