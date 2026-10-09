//! The agent's system prompt: fixed rules plus an engine catalogue generated
//! from the live registry, so the chooser text can never drift from what is
//! actually dispatchable.
//!
//! **Byte stability matters.** llama-server keeps a prefix KV cache; a system
//! prompt that changes between turns busts it and re-prefills the whole
//! conversation (`web/src/routes/chat/+page.svelte:119-124` already takes this
//! care). Nothing here is time- or turn-dependent — the prompt only changes when
//! the set of live engines changes, which is exactly when it must.
//!
//! Both halves are overridable through the existing prompt infrastructure
//! (`migrations/0016_prompt_templates_and_variables.sql`): the frame is the
//! `agent_system` template slot on media kind `default`, and each engine's
//! chooser line is the `agent_engine_<id>` variable.

use sqlx::PgPool;

use crate::repositories::prompt_templates;

use super::tools::LiveTool;

/// Replaced by the generated engine catalogue. An override that drops the
/// marker gets the catalogue appended instead — a prompt with no engine list at
/// all would leave the model guessing engine names.
pub const ENGINES_MARKER: &str = "{{ engines }}";

pub const SYSTEM_SLOT: &str = "agent_system";
pub const MEDIA_KIND: &str = "default";

/// The post-tool sentence. **Mandatory and load-bearing**, measured at
/// production sampling (temp 1.0 / top_p 0.95 / top_k 64, 5 samples per
/// variant) against an image-generation tool, which is the domain where this
/// model's ReAct/DALL·E training contamination bites:
///
/// | system prompt                                                | clean narration |
/// |--------------------------------------------------------------|-----------------|
/// | none                                                           | 0/5             |
/// | "You are a helpful assistant."                                 | 0/5             |
/// | "After a tool returns, reply to the user in plain prose."      | 0/5             |
/// | long "…never emit JSON describing an action…"                  | 1/5             |
/// | this sentence                                                  | 5/5             |
///
/// Note the fourth row: explicitly forbidding JSON PRIMES the failure. Keep this
/// terse and POSITIVE — do not grow it into a paragraph of prohibitions.
pub const POST_TOOL_SENTENCE: &str =
    "After a tool returns, tell the user what happened in one sentence.";

/// The default frame. Terse and affirmative on purpose: this model gets worse,
/// not better, as rules pile up (see [`POST_TOOL_SENTENCE`], and koblem's
/// standing prompt philosophy of cutting contradictory clauses).
///
/// The clarifying-question clause is the headline behaviour and needs no
/// machinery: given the engine catalogue below it, the model reliably answers
/// "sharp subject-focused (flux2) or dense photographic (krea2)?" instead of
/// guessing.
pub const DEFAULT_SYSTEM: &str = "You are Koblem, an assistant for a self-hosted media system. You \
make and change images, video, music, speech, transcripts and cutouts by calling tools, and you can \
look things up in the saved video projects.\n\
\n\
After a tool returns, tell the user what happened in one sentence.\n\
\n\
Write each prompt yourself, in the style the chosen engine wants. Each tool lists its engines with \
the best default first — use that one unless the request calls for another. Ask a short clarifying \
question instead of calling a tool when the answer would materially change the result and you \
cannot infer it. Renders are expensive and run one at a time, so treat a finished render as done \
and move on.\n\
\n\
The engines you have:\n\
{{ engines }}";

/// The full system prompt for this turn.
pub async fn system_prompt(db: &PgPool, tools: &[LiveTool]) -> String {
    let frame = prompt_templates::get_template(db, MEDIA_KIND, SYSTEM_SLOT)
        .await
        .ok()
        .flatten()
        .map(|t| t.body)
        .unwrap_or_else(|| DEFAULT_SYSTEM.to_string());
    if !frame.contains(POST_TOOL_SENTENCE) {
        tracing::warn!(
            slot = SYSTEM_SLOT,
            "agent system prompt override drops the post-tool sentence — expect ReAct blobs \
             instead of narration after every media tool call"
        );
    }
    let catalogue = engine_catalogue(db, tools).await;
    splice(&frame, &catalogue)
}

pub fn splice(frame: &str, catalogue: &str) -> String {
    if frame.contains(ENGINES_MARKER) {
        frame.replace(ENGINES_MARKER, catalogue)
    } else {
        format!("{}\n\nThe engines you have:\n{catalogue}", frame.trim_end())
    }
}

async fn engine_catalogue(db: &PgPool, tools: &[LiveTool]) -> String {
    let mut out = String::new();
    for live in tools {
        // A tool with one nameless engine (matting, STT) has nothing to choose
        // between — its description already says what it does.
        if live.engines.len() < 2 && live.engines.iter().all(|e| e.blurb.is_empty()) {
            continue;
        }
        out.push_str(&format!("\n{}:\n", live.tool.name()));
        for e in &live.engines {
            let blurb = prompt_templates::get_variable(db, MEDIA_KIND, &variable_name(e.id))
                .await
                .ok()
                .flatten()
                .map(|v| v.body)
                .unwrap_or_else(|| e.blurb.to_string());
            out.push_str(&format!("  - {}\n", blurb.trim()));
        }
    }
    out.trim_end().to_string()
}

pub fn variable_name(engine_id: &str) -> String {
    format!("agent_engine_{engine_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::{Tool, def};

    fn live(tool: Tool) -> LiveTool {
        LiveTool {
            tool,
            engines: def(tool).engines.iter().collect(),
        }
    }

    #[test]
    fn the_marker_is_replaced_not_appended() {
        let out = splice("head\n{{ engines }}\ntail", "ENGINES");
        assert_eq!(out, "head\nENGINES\ntail");
    }

    #[test]
    fn an_override_without_the_marker_still_gets_the_catalogue() {
        let out = splice("custom prompt", "ENGINES");
        assert!(out.starts_with("custom prompt"));
        assert!(out.ends_with("ENGINES"));
    }

    #[test]
    fn the_default_frame_carries_the_marker() {
        assert!(DEFAULT_SYSTEM.contains(ENGINES_MARKER));
    }

    #[test]
    fn the_post_tool_sentence_is_present_verbatim() {
        // 5/5 clean narrations with it, 0/5 without. Anything that drops or
        // rewords it regresses every media tool call.
        assert!(
            DEFAULT_SYSTEM.contains(POST_TOOL_SENTENCE),
            "the post-tool sentence is mandatory — see POST_TOOL_SENTENCE"
        );
    }

    #[test]
    fn the_frame_stays_terse_and_positive() {
        // Forbidding JSON PRIMES the ReAct blob on this model (1/5 vs 5/5), so
        // this is a guard against a well-meaning "improvement".
        let lowered = DEFAULT_SYSTEM.to_lowercase();
        for banned in ["never ", "json", "action_input", "do not output"] {
            assert!(
                !lowered.contains(banned),
                "the system prompt must not contain prohibitions ({banned})"
            );
        }
        assert!(DEFAULT_SYSTEM.len() < 900, "keep the frame terse");
    }

    #[test]
    fn the_frame_asks_for_clarification() {
        assert!(DEFAULT_SYSTEM.contains("clarifying question"));
    }

    #[test]
    fn engine_variable_names_are_namespaced() {
        assert_eq!(variable_name("krea2"), "agent_engine_krea2");
    }

    #[test]
    fn the_prompt_is_byte_stable_for_a_fixed_engine_set() {
        let tools = vec![live(Tool::MakeImage), live(Tool::MakeVideo)];
        let a = splice(DEFAULT_SYSTEM, &static_catalogue(&tools));
        let b = splice(DEFAULT_SYSTEM, &static_catalogue(&tools));
        assert_eq!(a, b, "the system prompt must not vary between turns");
    }

    #[test]
    fn the_prompt_changes_when_an_engine_goes_away() {
        let all = vec![live(Tool::MakeImage)];
        let one = vec![LiveTool {
            tool: Tool::MakeImage,
            engines: def(Tool::MakeImage)
                .engines
                .iter()
                .filter(|e| e.id == "krea2")
                .collect(),
        }];
        assert_ne!(static_catalogue(&all), static_catalogue(&one));
        assert!(!static_catalogue(&one).contains("FLUX.2"));
    }

    /// The DB-free half of `engine_catalogue`, so the shape can be asserted
    /// without a Postgres fixture.
    fn static_catalogue(tools: &[LiveTool]) -> String {
        let mut out = String::new();
        for live in tools {
            if live.engines.len() < 2 && live.engines.iter().all(|e| e.blurb.is_empty()) {
                continue;
            }
            out.push_str(&format!("\n{}:\n", live.tool.name()));
            for e in &live.engines {
                out.push_str(&format!("  - {}\n", e.blurb.trim()));
            }
        }
        out.trim_end().to_string()
    }
}
