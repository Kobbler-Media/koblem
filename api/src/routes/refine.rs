//! Shared "improve this draft" plumbing for the creative-draft LLM endpoints.
//!
//! Every creative drafter (lyrics, image prompts, …) pairs a
//! feature-specific system prompt — the *rules and output format* — with a
//! from-scratch user message. Refining reuses the exact same system prompt (so
//! the same rules apply) and only swaps the user message for one that hands the
//! model the existing draft plus the user's requested change. Keeping the system
//! prompt untouched is the whole point: an "improve" is a draft that still obeys
//! the format, not a free-form rewrite.

/// A user's request to improve an existing draft in place. `source` is the
/// current draft (guaranteed non-empty); `instruction` is the free-form change
/// they asked for (may be empty → a general polish).
pub struct Refine<'a> {
    pub source: &'a str,
    pub instruction: &'a str,
}

impl<'a> Refine<'a> {
    /// Build from optional request fields. Trims both and drops blanks. Returns
    /// `None` when there is no existing draft to work from — a bare instruction
    /// with nothing to improve is not a refine, it's just a brief, so callers
    /// fall back to their from-scratch path.
    pub fn from_opts(source: Option<&'a str>, instruction: Option<&'a str>) -> Option<Refine<'a>> {
        let source = source.map(str::trim).filter(|s| !s.is_empty())?;
        let instruction = instruction.map(str::trim).unwrap_or("");
        Some(Refine {
            source,
            instruction,
        })
    }
}

/// Build the refine user message. `noun` names the draft in the singular so it
/// reads naturally (e.g. "lyric draft", "image prompt", "spoken script").
/// `context` is the same style/theme block the from-scratch message would carry
/// (may be empty). The system prompt is supplied unchanged by the caller.
pub fn refine_user_msg(noun: &str, context: &str, r: &Refine) -> String {
    let mut m = format!("Here is the current {noun}:\n\n{}\n\n", r.source);
    if r.instruction.is_empty() {
        m.push_str(
            "Improve it: tighten weak spots, smooth awkward phrasing and lift the overall \
             quality — while keeping everything that already works and the required format. \
             Change only what needs it; do not rewrite it wholesale.",
        );
    } else {
        m.push_str(&format!(
            "Revise it to apply this change, and nothing more — keep everything the change does \
             not touch (the parts that already read well and the required format):\n{}",
            r.instruction
        ));
    }
    let context = context.trim();
    if !context.is_empty() {
        m.push_str(&format!("\n\n{context}"));
    }
    m.push_str(&format!(
        "\n\nReturn the complete revised {noun} in the same format — the full text, not a diff or \
         only the changed lines."
    ));
    m
}
