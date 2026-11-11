//! Per-surface sampling defaults: one settings row per LLM *surface*, applied
//! by one helper.
//!
//! Every koblem call site that composes a prompt in Rust used to hardcode its
//! sampling inline — a raw `"chat_template_kwargs": {"enable_thinking": false}`
//! literal plus, sometimes, a temperature. That left `presence_penalty` (and
//! everything else) unset on essentially every structured call, so whatever the
//! llama-server CLI was started with won. Fine under gemma; wrong under Qwen3.8,
//! whose non-thinking recipe wants `presence_penalty 1.5`.
//!
//! The fix is not another per-surface copy of the same eight lines: it is
//! [`apply_sampling`], which every surface calls with its own settings key.
//!
//! ## Layering — lowest priority first
//!
//! 1. **The call site's own body.** Whatever the `json!` literal already set
//!    (`max_tokens`, a hand-picked `temperature`, the `enable_thinking: false`
//!    baseline) is left alone unless a layer above names that same field. This
//!    is what makes landing the system a no-op.
//! 2. **[`baseline_for`]** — a hardcoded starting point for the handful of
//!    surfaces that had a forced value before this existed (music's creative
//!    numbers, the three prompt-assist surfaces). Empty for everything else.
//! 3. **Inherited settings rows** ([`inherited_keys`]) — the legacy
//!    `*_llm_sampling` rows an operator may already have tuned, and the shared
//!    `sampling_music` knob that music3/songgen deliberately ride on.
//! 4. **The surface's own settings row**, written by the Settings page.
//!
//! ## Inherit means OMIT
//!
//! A field left null in the settings row is not substituted with anything: it is
//! simply never written to the body, so the call site's own value (or failing
//! that llama-server's flag default) stands. That is [`SamplingConfig::apply_to`]'s
//! existing contract and this module does not weaken it.

use koblem_client::types::SamplingConfig;
use serde_json::Value;
use sqlx::PgPool;

/// Private marker key [`apply_sampling`] stamps onto the body naming the surface
/// the call came from, so `llm_logs` can attribute a bad row to a knob.
///
/// 🔴 **Never reaches llama-server** — [`crate::llama::LlamaClient::chat_completion`]
/// strips it immediately before the POST. Anything else that serialises a koblem
/// request body onto the wire must strip it too.
pub const SAMPLING_KEY_FIELD: &str = "koblem_sampling_key";

/// Every sampling settings key koblem reads, in the order the Settings page
/// renders them. **Must stay in sync with `SAMPLING_KEYS` in
/// `web/src/routes/settings/+page.svelte`** — that array carries the labels and
/// this one carries the truth about which keys are actually consulted.
///
/// Test-only: the surfaces name their own key at the call site, so nothing in
/// the binary walks this list — it exists so the coverage test below fails when
/// a new surface lands without a baseline decision.
#[cfg(test)]
pub const SAMPLING_KEYS: &[&str] = &[
    // Release pipeline (these four resolve their own rows directly, not via
    // `apply_sampling`, because they also take a per-request override).
    "sampling_review_candidates",
    "sampling_filter_releases",
    "sampling_validate_release",
    "sampling_rank_releases",
    // Conversational
    "sampling_chat_default",
    "sampling_completions_default",
    "sampling_agent",
    // Video writers
    "sampling_director",
    "sampling_ltx_video",
    "sampling_ltx_critic",
    // Music writers
    "sampling_music",
    "sampling_music3",
    "sampling_songgen",
    // Image / character prompt assists
    "sampling_images",
    "sampling_krea2",
    "sampling_character_sheet",
    // Housekeeping
    "sampling_title",
];

/// The hardcoded starting point for a surface, before any settings row.
///
/// 🔴 **Almost every surface returns an empty config — that is the point.** An
/// empty baseline writes nothing to the body, so adding a key changes nothing
/// until an operator touches a knob. The exceptions below are surfaces that were
/// *already* forcing values from Rust before this module existed; their numbers
/// moved here so the forcing is visible in one place and overridable from the UI,
/// not so that new surfaces would acquire opinions.
pub fn baseline_for(key: &str) -> SamplingConfig {
    match key {
        // 🔴 The music family — the one place that is creative ON PURPOSE. It writes
        // captions and lyrics — it is supposed to be creative, and a
        // presence penalty on a chorus is actively wrong (a chorus repeats).
        // These are Qwen's *thinking-mode* sampling numbers used with thinking
        // OFF: high temperature, no presence penalty. They replace the
        // `unwrap_or(0.9)` / `unwrap_or(0.95)` that `apply_music_sampling`
        // used to force, which they are deliberately close to.
        //
        // 🔴 music3 and songgen carry the SAME baseline, not an empty one: they
        // route through the music writer today (`apply_music_sampling`), so an
        // empty baseline here would silently drop their creative sampling. Their
        // own rows, and the shared `sampling_music` row, layer on top — see
        // `inherited_keys`.
        "sampling_music" | "sampling_music3" | "sampling_songgen" => SamplingConfig {
            temperature: Some(1.0),
            top_p: Some(0.95),
            top_k: Some(20),
            min_p: Some(0.0),
            presence_penalty: Some(0.0),
            ..SamplingConfig::default()
        },
        // Prompt assists: preserved verbatim from the `*_llm_sampling` helpers
        // these replaced, so the conversion is behaviour-neutral. Krea 2's
        // assist is deliberately the faithful/low-variance one — it rewrites a
        // user's prompt rather than inventing, and 0.85 wandered off the brief.
        "sampling_krea2" => SamplingConfig {
            temperature: Some(0.4),
            top_p: Some(0.9),
            ..SamplingConfig::default()
        },
        "sampling_images" => SamplingConfig {
            temperature: Some(0.85),
            top_p: Some(0.95),
            ..SamplingConfig::default()
        },
        _ => SamplingConfig::default(),
    }
}

/// Lower-priority settings rows a key inherits from, lowest first.
///
/// Two jobs:
/// - **Legacy keys.** The four `*_llm_sampling` rows predate this module and
///   store the same field names (`{temperature, top_p, top_k}`), so an operator
///   who tuned one keeps that tuning; the new key overrides it when set.
/// - **`sampling_music` as the shared music knob.** music3 and songgen route
///   their writers through the music surface on purpose — "all four engines'
///   writers behave alike and an operator has one knob, not four". Setting
///   `sampling_music` still moves all three; their own rows exist only for when
///   one of them needs to differ.
fn inherited_keys(key: &str) -> &'static [&'static str] {
    match key {
        "sampling_music" => &["music_llm_sampling"],
        "sampling_music3" | "sampling_songgen" => &["music_llm_sampling", "sampling_music"],
        "sampling_krea2" => &["krea2_llm_sampling"],
        "sampling_images" => &["flux_llm_sampling"],
        _ => &[],
    }
}

/// Resolve the effective config for a surface without touching a body.
/// Exposed for callers that need the [`SamplingConfig`] itself (logging tags,
/// or a surface that also accepts a per-request override).
pub async fn resolve_sampling(db: &PgPool, key: &str) -> SamplingConfig {
    let mut cfg = baseline_for(key);
    for inherited in inherited_keys(key) {
        cfg = cfg.overlay(&crate::prompts::load_sampling_config(db, inherited).await);
    }
    cfg.overlay(&crate::prompts::load_sampling_config(db, key).await)
}

/// Apply a surface's configured sampling to a llama-server request body.
///
/// Call it **after** building the `json!` body: fields the settings row names
/// overwrite what the literal set (that is what makes the knob a knob), and
/// fields it leaves null are not written at all, so the literal's own values
/// survive untouched.
///
/// Also resolves the grammar mode (inert unless one is configured) and stamps
/// [`SAMPLING_KEY_FIELD`] for `llm_logs` attribution.
pub async fn apply_sampling(db: &PgPool, key: &str, body: &mut Value) {
    let cfg = resolve_sampling(db, key).await;
    cfg.apply_to(body);
    crate::prompts::apply_grammar_to_body(db, body, &cfg).await;
    if let Some(obj) = body.as_object_mut() {
        obj.insert(SAMPLING_KEY_FIELD.into(), Value::String(key.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_surface_but_the_documented_few_inherits() {
        // 🔴 The safe-rollout property: a key with an empty baseline writes
        // NOTHING, so landing a new surface cannot change a single request.
        for key in SAMPLING_KEYS {
            let has_values = {
                let v = baseline_for(key).resolve();
                v.temperature.is_some()
                    || v.top_p.is_some()
                    || v.top_k.is_some()
                    || v.min_p.is_some()
                    || v.presence_penalty.is_some()
                    || v.repetition_penalty.is_some()
                    || v.max_tokens.is_some()
            };
            let expected = matches!(
                *key,
                "sampling_music"
                    | "sampling_music3"
                    | "sampling_songgen"
                    | "sampling_krea2"
                    | "sampling_images"
            );
            assert_eq!(has_values, expected, "{key} baseline");
        }
    }

    #[test]
    fn an_empty_baseline_leaves_the_call_sites_body_alone() {
        let mut body = json!({
            "messages": [],
            "temperature": 0.7,
            "chat_template_kwargs": {"enable_thinking": false},
        });
        baseline_for("sampling_director").apply_to(&mut body);
        assert_eq!(body["temperature"], json!(0.7));
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            json!(false)
        );
        assert!(body.get("presence_penalty").is_none());
    }

    #[test]
    fn music_is_creative_and_carries_no_presence_penalty() {
        let v = baseline_for("sampling_music").resolve();
        assert_eq!(v.temperature, Some(1.0));
        assert_eq!(v.top_p, Some(0.95));
        assert_eq!(v.top_k, Some(20));
        assert_eq!(v.min_p, Some(0.0));
        // A chorus repeats on purpose.
        assert_eq!(v.presence_penalty, Some(0.0));
    }

    #[test]
    fn the_settings_row_wins_over_the_baseline() {
        let cfg = baseline_for("sampling_music").overlay(&SamplingConfig {
            temperature: Some(0.2),
            ..SamplingConfig::default()
        });
        let v = cfg.resolve();
        assert_eq!(v.temperature, Some(0.2), "operator override");
        assert_eq!(v.top_p, Some(0.95), "untouched field keeps the baseline");
    }

    #[test]
    fn music3_and_songgen_keep_the_creative_baseline() {
        // They render through the music writer today; an empty baseline would
        // quietly hand them llama-server's flag defaults instead.
        for key in ["sampling_music3", "sampling_songgen"] {
            assert_eq!(
                baseline_for(key).resolve().temperature,
                Some(1.0),
                "{key} lost the creative baseline"
            );
        }
    }

    #[test]
    fn music3_and_songgen_ride_the_music_knob() {
        assert!(inherited_keys("sampling_music3").contains(&"sampling_music"));
        assert!(inherited_keys("sampling_songgen").contains(&"sampling_music"));
        // Legacy rows stay honoured so existing tuning is not silently dropped.
        assert_eq!(inherited_keys("sampling_krea2"), &["krea2_llm_sampling"]);
    }
}
