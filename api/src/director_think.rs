//! Thinking for the Director's writing surfaces — planner, writers, rework and chat.
//!
//! 🔴 **These surfaces used to pin `enable_thinking: false`, and that was the wrong trade.** They
//! compose a 7,000-character H3 block against a 24 KB guide and a page of koblem facts, or cut a
//! whole brief into renders. With thinking off the model obeyed some of those rules and dropped the
//! rest, and the owner's verdict on the result was that local models "completely butcher the
//! prompts". Quality wins over latency here: each call is one render's document, and a bad one costs
//! a GPU render to discover.
//!
//! **Effort follows the model, not the call site.** Beefy (Qwen3.8-27B on exl3) runs at `xhigh`,
//! the owner's standing choice; the resident 12B runs at its template's default. Beefy's template
//! already defaults to `xhigh` (`template_vars_default`), but it is sent explicitly so a template
//! swap cannot quietly drop it. The 12B gets no effort key at all.
//!
//! ⚠️ **The budget is the other half and it is not optional.** At `xhigh` one reply measured ~11k
//! reasoning tokens, and a `max_tokens` sized for the answer alone returns NOTHING once the
//! reasoning eats it (the "xhigh returned nothing" result that was really a 12k cap). So the
//! answer budget the caller sizes is widened by the reasoning headroom below, never replaced.
//!
//! ⚠️ Applied AFTER `sampling::apply_sampling`, so a settings row cannot switch thinking back off
//! on these surfaces or shrink the budget below what the reasoning needs. Every other knob in the
//! row (temperature, penalties) still applies.

use serde_json::{Value, json};

use crate::llama::LlamaClient;

/// Reasoning headroom on Beefy at `xhigh`: ~11k measured per reply, ~3x kept for the long briefs.
pub const THINK_BUDGET_BEEFY: u32 = 32_768;
/// Reasoning headroom on the resident 12B at its default effort.
pub const THINK_BUDGET_DEFAULT: u32 = 16_384;

/// Turn thinking on for `body` at the loaded model's effort, and widen `max_tokens` so the
/// reasoning cannot starve the answer.
///
/// `answer_tokens` is what the REPLY needs (an H3 block is ~1,800 at the 7,000-character cap).
pub fn enable(body: &mut Value, client: &LlamaClient, answer_tokens: u32) {
    apply(body, client.is_multi_gpu(), answer_tokens);
}

/// [`enable`] with the model decided by the caller — the pure half, so it is testable without a
/// live client.
pub fn apply(body: &mut Value, beefy: bool, answer_tokens: u32) {
    if !body.get("chat_template_kwargs").is_some_and(Value::is_object) {
        body["chat_template_kwargs"] = json!({});
    }
    let kwargs = &mut body["chat_template_kwargs"];
    kwargs["enable_thinking"] = json!(true);
    if beefy {
        kwargs["reasoning_effort"] = json!("xhigh");
    } else if let Some(o) = kwargs.as_object_mut() {
        o.remove("reasoning_effort");
    }
    // `beefy` is `LlamaClient::is_multi_gpu` — "the beefy server" today; see
    // `LlamaClient::audio_input_supported` for the same derivation and its caveat.
    let budget = if beefy {
        THINK_BUDGET_BEEFY
    } else {
        THINK_BUDGET_DEFAULT
    };
    let want = u64::from(answer_tokens) + u64::from(budget);
    let have = body.get("max_tokens").and_then(Value::as_u64).unwrap_or(0);
    body["max_tokens"] = json!(want.max(have));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this module exists to close: every Director surface sent `enable_thinking: false`.
    #[test]
    fn a_pinned_off_switch_is_turned_on() {
        let mut body = json!({"max_tokens": 1500, "chat_template_kwargs": {"enable_thinking": false}});
        apply(&mut body, false, 2400);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(true));
    }

    #[test]
    fn beefy_runs_at_xhigh_and_the_12b_at_its_default() {
        let mut beefy = json!({});
        apply(&mut beefy, true, 2400);
        assert_eq!(beefy["chat_template_kwargs"]["reasoning_effort"], json!("xhigh"));

        let mut small = json!({"chat_template_kwargs": {"reasoning_effort": "xhigh"}});
        apply(&mut small, false, 2400);
        assert!(small["chat_template_kwargs"].get("reasoning_effort").is_none());
    }

    /// An answer-sized cap is how xhigh "returned nothing": the reasoning ate it.
    #[test]
    fn the_budget_is_the_answer_plus_the_reasoning() {
        let mut body = json!({"max_tokens": 1500});
        apply(&mut body, true, 2400);
        assert_eq!(body["max_tokens"], json!(2400 + THINK_BUDGET_BEEFY));
    }

    /// A caller that already asked for more keeps it.
    #[test]
    fn a_larger_existing_cap_is_kept() {
        let mut body = json!({"max_tokens": 100_000});
        apply(&mut body, false, 2400);
        assert_eq!(body["max_tokens"], json!(100_000));
    }
}
