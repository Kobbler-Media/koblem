//! Shared helpers for parsing LLM review responses and computing the kobload
//! ↔ LLM diff.
//!
//! The LLM is shown a numbered candidate list and emits a blind accept-list:
//!
//! ```json
//! {
//!   "accept": [{"idx": 2, "reason": "..."}, {"idx": 7, "reason": "..."}],
//!   "notes": "optional batch-level observation"
//! }
//! ```
//!
//! `parse_review_response` produces a [`ParsedReview`]; `compute_diff` takes
//! that plus the original candidate list and emits the three buckets
//! ([`ReviewResponse::agreed`], `vetoed`, `promoted`) by intersecting the
//! LLM's accept set with kobload's accept set.

use koblem_client::types::{ReviewAccept, ReviewCandidate, ReviewDelta};
use serde_json::Value;
use tracing::warn;

/// What a successful parse of the LLM JSON yielded.
pub struct ParsedReview {
    pub accepts: Vec<ReviewAccept>,
    pub notes: Option<String>,
    /// Set when the JSON could not be parsed at all. Surfaced as a top-level
    /// `notes` warning by the caller so the playground UI shows the raw error.
    pub parse_error: Option<String>,
}

/// Pull `choices[0].finish_reason` from a chat-completion response body.
pub fn finish_reason(body: &Value) -> Option<String> {
    body.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Prepend a warning line to an existing notes string (or return the warning
/// standalone if there's nothing to prepend to). Used by call sites to layer
/// truncation/parse-error warnings on top of the LLM's own `notes` text.
pub fn prepend_note(existing: &Option<String>, warning: &str) -> String {
    match existing {
        Some(text) if !text.is_empty() => format!("{warning}\n{text}"),
        _ => warning.to_string(),
    }
}

/// If the LLM stopped because it hit `max_tokens` or context limit, return a
/// user-visible note so callers can prepend it to their review notes.
pub fn truncation_note(
    finish: &Option<String>,
    accept_count: usize,
    candidate_count: usize,
) -> Option<String> {
    match finish.as_deref() {
        Some("length") => Some(format!(
            "⚠ LLM output truncated (finish_reason=length): got {accept_count} accepts for {candidate_count} candidates"
        )),
        _ => None,
    }
}

/// Format the chat completion request body into a human-readable debug prompt.
pub fn format_debug_prompt(body: &Value) -> String {
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    let mut parts = Vec::new();
    for msg in &messages {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        parts.push(format!("[{role}]\n{content}"));
    }
    parts.join("\n\n---\n\n")
}

/// Parse the LLM's JSON response into a [`ParsedReview`].
///
/// Tolerates markdown fences, junk before/after the JSON object, and a few
/// shapes the model occasionally drifts into. On unrecoverable parse failure,
/// returns an empty accept list with `parse_error` populated so the caller
/// can surface it without crashing.
pub fn parse_review_response(content: &str) -> ParsedReview {
    // Try strict parse on a fence-stripped trim first so we can detect the
    // doubled-brace recovery and surface it in notes. If strict fails, fall
    // back to extract_json_payload which also handles prose-with-embedded-JSON
    // (model leaks JSON mid-paragraph after `</think>` — common on Qwen3
    // reasoning distills when scaffolded list output competes with a
    // prose-style answer mode).
    let trimmed = strip_fences(content.trim());
    let strict_failed = serde_json::from_str::<Value>(trimmed).is_err();
    let parsed: Value = match crate::think::extract_json_payload(content, None) {
        Some(v) => v,
        None => {
            warn!("failed to parse review response, raw: {content}");
            return ParsedReview {
                accepts: Vec::new(),
                notes: None,
                parse_error: Some("LLM response parse error: no JSON object found".to_string()),
            };
        }
    };

    let mut notes = parsed
        .get("notes")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // Surface the recovery in the response notes so the user sees that the
    // LLM mangled the output even when downstream consumers don't notice.
    // Two known triggers: (a) doubled-brace sampling glitch from llama-server,
    // (b) model leaked prose around the JSON (verbose answer-mode after
    // `</think>` on the Qwen3 reasoning distill).
    if strict_failed {
        notes = Some(prepend_note(
            &notes,
            "⚠ recovered JSON via lenient extraction (prose leak or doubled-brace)",
        ));
    }

    // Parse the accept array. Supports three shapes from smallest to
    // largest, and any mix of them in a single response:
    //
    //   1. bare integer         → [1, 2, 3]
    //   2. object with just idx → [{"idx": 1}, {"idx": 2}]
    //   3. object with reason   → [{"idx": 1, "reason": "good"}, ...]
    //
    // The bare-integer form exists for output-size compression — every pick
    // costs ~2 tokens instead of ~7 for the object wrapper, which adds up on
    // scenarios that accept 15+ candidates. Shape (3) is still useful when
    // debugging via the testbed with a reason-bearing grammar override, so
    // we keep it compatible.
    let accepts = parsed
        .get("accept")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    // Shape 1: bare integer.
                    if let Some(n) = item.as_u64() {
                        return Some(ReviewAccept {
                            idx: n as usize,
                            reason: String::new(),
                        });
                    }
                    // Shape 2/3: object with idx (and optional reason).
                    let idx = item.get("idx").and_then(|i| i.as_u64())? as usize;
                    let reason = item
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(ReviewAccept { idx, reason })
                })
                .collect()
        })
        .unwrap_or_default();

    ParsedReview {
        accepts,
        notes,
        parse_error: None,
    }
}

/// Strip markdown code fences (```json … ```) if the model wrapped its JSON.
fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_prefix("```").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

/// Result of comparing the LLM's accept set against kobload's accept set.
pub struct DiffResult {
    pub agreed: Vec<ReviewDelta>,
    pub vetoed: Vec<ReviewDelta>,
    pub promoted: Vec<ReviewDelta>,
    /// 1-based indices the LLM emitted that fell outside the candidate range.
    /// Empty in normal operation; non-empty is a sign the model is hallucinating
    /// or that the candidate list got truncated mid-prompt.
    pub dropped_indices: Vec<usize>,
}

/// Compute the kobload ↔ LLM diff: agreed / vetoed / promoted.
///
/// Indices in `accepts` are 1-based and refer to positions in `candidates`.
/// Out-of-range indices are dropped from the buckets but reported in
/// `DiffResult::dropped_indices` so the caller can surface them to users.
pub fn compute_diff(accepts: &[ReviewAccept], candidates: &[ReviewCandidate]) -> DiffResult {
    use std::collections::HashMap;

    // Map LLM accepts by idx → reason for fast lookup. Out-of-range indices
    // are stripped here and recorded in `dropped` for caller diagnostics.
    let mut llm_accepts: HashMap<usize, &str> = HashMap::new();
    let mut dropped: Vec<usize> = Vec::new();
    for a in accepts {
        if a.idx == 0 || a.idx > candidates.len() {
            warn!(
                "LLM accept idx {} is out of range (1..={}); dropping",
                a.idx,
                candidates.len()
            );
            dropped.push(a.idx);
            continue;
        }
        llm_accepts.insert(a.idx, a.reason.as_str());
    }

    let mut agreed = Vec::new();
    let mut vetoed = Vec::new();
    let mut promoted = Vec::new();

    for (i, c) in candidates.iter().enumerate() {
        let one_based = i + 1;
        let llm_picked = llm_accepts.get(&one_based).copied();
        match (c.kobload_accepted, llm_picked) {
            (true, Some(reason)) => agreed.push(ReviewDelta {
                idx: one_based,
                name: c.name.clone(),
                reason: Some(reason.to_string()),
            }),
            (true, None) => vetoed.push(ReviewDelta {
                idx: one_based,
                name: c.name.clone(),
                reason: None, // LLM was silent on rejects by design.
            }),
            (false, Some(reason)) => promoted.push(ReviewDelta {
                idx: one_based,
                name: c.name.clone(),
                reason: Some(reason.to_string()),
            }),
            (false, None) => {} // both rejected — silent agreement, not surfaced.
        }
    }

    DiffResult {
        agreed,
        vetoed,
        promoted,
        dropped_indices: dropped,
    }
}

/// Collapse duplicate candidate names down to a single entry each.
///
/// Kobload emits one `ReviewCandidate` per scored `Release` (i.e. per GUID),
/// so the same release name showing up across two indexers produces two rows
/// with identical title/scoring and slightly different `publish_date` /
/// `kobload_score` / per-variant metadata. At scale the LLM anchors on the
/// first ~40 rows and stops properly reading the rest — the testbed fixture
/// loader already applies the same dedup for the exact same reason.
///
/// Selection rule per name group:
///   1. Latest `publish_date` wins (newer variants are what the user actually
///      wants to grab; older duplicates are usually stale re-posts).
///   2. Tiebreak on higher `kobload_score` when dates match or neither parses.
///
/// Case-insensitive on name. Preserves the original relative order of the
/// kept entries so downstream sort logic (e.g. `sort_candidates_by_relevance`)
/// still sees kobload's front-loading intact.
pub fn dedup_candidates_by_name(candidates: &mut Vec<ReviewCandidate>) {
    use std::collections::{HashMap, HashSet};

    if candidates.len() < 2 {
        return;
    }

    let mut winner_by_name: HashMap<String, usize> = HashMap::new();
    for (idx, cand) in candidates.iter().enumerate() {
        let key = cand.name.to_ascii_lowercase();
        match winner_by_name.get(&key) {
            None => {
                winner_by_name.insert(key, idx);
            }
            Some(&cur_idx) => {
                if prefers_newer(cand, &candidates[cur_idx]) {
                    winner_by_name.insert(key, idx);
                }
            }
        }
    }

    let keep: HashSet<usize> = winner_by_name.values().copied().collect();
    let mut idx = 0;
    candidates.retain(|_| {
        let keep_it = keep.contains(&idx);
        idx += 1;
        keep_it
    });
}

/// Returns true when `challenger` should replace `current` in a dedup group.
/// Latest publish_date wins; falls back to higher kobload_score on ties or
/// unparseable dates.
fn prefers_newer(challenger: &ReviewCandidate, current: &ReviewCandidate) -> bool {
    match (
        parse_publish_date(challenger.publish_date.as_deref()),
        parse_publish_date(current.publish_date.as_deref()),
    ) {
        (Some(a), Some(b)) if a != b => a > b,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => challenger.kobload_score > current.kobload_score,
    }
}

/// Parse a candidate's publish_date, accepting RFC 3339, RFC 2822 (common for
/// Newznab RSS feeds), and plain `YYYY-MM-DD`.
fn parse_publish_date(s: Option<&str>) -> Option<time::OffsetDateTime> {
    let s = s?;
    if let Ok(dt) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339) {
        return Some(dt);
    }
    if let Ok(dt) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc2822) {
        return Some(dt);
    }
    let fmt = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").ok()?;
    time::Date::parse(s, &fmt)
        .ok()
        .and_then(|d| d.with_hms(0, 0, 0).ok())
        .map(|pd| pd.assume_utc())
}

/// Pull `usage.prompt_tokens` and `usage.completion_tokens` out of a
/// chat-completion response body.
pub fn token_usage(body: &Value) -> (Option<i32>, Option<i32>) {
    let usage = body.get("usage");
    let prompt = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let completion = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    (prompt, completion)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, kobload_accepted: bool) -> ReviewCandidate {
        ReviewCandidate {
            name: name.to_string(),
            kobload_accepted,
            ..Default::default()
        }
    }

    #[test]
    fn parses_clean_response() {
        let raw = r#"{"accept":[{"idx":2,"reason":"good"}],"notes":"ok"}"#;
        let p = parse_review_response(raw);
        assert!(p.parse_error.is_none());
        assert_eq!(p.accepts.len(), 1);
        assert_eq!(p.accepts[0].idx, 2);
        assert_eq!(p.accepts[0].reason, "good");
        assert_eq!(p.notes.as_deref(), Some("ok"));
    }

    #[test]
    fn parses_bare_integer_accept_list() {
        // The compressed-output shape: accept items are raw integers.
        let raw = r#"{"notes":"ok","accept":[2,5,9]}"#;
        let p = parse_review_response(raw);
        assert!(p.parse_error.is_none());
        assert_eq!(p.accepts.len(), 3);
        assert_eq!(p.accepts[0].idx, 2);
        assert_eq!(p.accepts[1].idx, 5);
        assert_eq!(p.accepts[2].idx, 9);
        assert!(p.accepts[0].reason.is_empty());
    }

    #[test]
    fn parses_mixed_accept_shapes() {
        // In case the grammar override lets old and new shapes coexist.
        let raw = r#"{"notes":null,"accept":[1,{"idx":2,"reason":"explicit"},3]}"#;
        let p = parse_review_response(raw);
        assert!(p.parse_error.is_none());
        assert_eq!(p.accepts.len(), 3);
        assert_eq!(p.accepts[0].idx, 1);
        assert_eq!(p.accepts[1].idx, 2);
        assert_eq!(p.accepts[1].reason, "explicit");
        assert_eq!(p.accepts[2].idx, 3);
    }

    #[test]
    fn parses_empty_accept_list() {
        let raw = r#"{"accept":[],"notes":"nothing matched"}"#;
        let p = parse_review_response(raw);
        assert!(p.accepts.is_empty());
        assert_eq!(p.notes.as_deref(), Some("nothing matched"));
    }

    #[test]
    fn strips_markdown_fences() {
        let raw = "```json\n{\"accept\":[],\"notes\":null}\n```";
        let p = parse_review_response(raw);
        assert!(p.parse_error.is_none());
        assert!(p.accepts.is_empty());
        assert!(p.notes.is_none());
    }

    #[test]
    fn populates_parse_error_on_garbage() {
        let p = parse_review_response("not json at all");
        assert!(p.parse_error.is_some());
        assert!(p.accepts.is_empty());
    }

    #[test]
    fn diff_buckets() {
        let candidates = vec![
            cand("a-kob-rejected", false),
            cand("b-kob-accepted", true),
            cand("c-kob-accepted", true),
            cand("d-kob-rejected", false),
        ];
        // LLM picked b (agree) and a (promote). c is vetoed. d is silent-reject.
        let accepts = vec![
            ReviewAccept {
                idx: 1,
                reason: "rescued".to_string(),
            },
            ReviewAccept {
                idx: 2,
                reason: "looks good".to_string(),
            },
        ];
        let diff = compute_diff(&accepts, &candidates);
        assert_eq!(diff.agreed.len(), 1);
        assert_eq!(diff.agreed[0].name, "b-kob-accepted");
        assert_eq!(diff.vetoed.len(), 1);
        assert_eq!(diff.vetoed[0].name, "c-kob-accepted");
        assert_eq!(diff.promoted.len(), 1);
        assert_eq!(diff.promoted[0].name, "a-kob-rejected");
        assert!(diff.dropped_indices.is_empty());
    }

    #[test]
    fn diff_drops_out_of_range_indices() {
        let candidates = vec![cand("only", true)];
        let accepts = vec![
            ReviewAccept {
                idx: 99,
                reason: "bogus".to_string(),
            },
            ReviewAccept {
                idx: 1,
                reason: "ok".to_string(),
            },
        ];
        let diff = compute_diff(&accepts, &candidates);
        assert_eq!(diff.agreed.len(), 1);
        assert!(diff.vetoed.is_empty());
        assert!(diff.promoted.is_empty());
        assert_eq!(diff.dropped_indices, vec![99]);
    }

    #[test]
    fn truncation_note_only_on_length() {
        assert!(truncation_note(&Some("stop".to_string()), 3, 10).is_none());
        assert!(truncation_note(&None, 3, 10).is_none());
        assert!(truncation_note(&Some("length".to_string()), 3, 10).is_some());
    }

    fn cand_pubdate(name: &str, publish_date: Option<&str>, score: f64) -> ReviewCandidate {
        let mut c = cand(name, false);
        c.publish_date = publish_date.map(String::from);
        c.kobload_score = score;
        c
    }

    #[test]
    fn dedup_keeps_latest_publish_date() {
        let mut cands = vec![
            cand_pubdate(
                "Euphoria.S03E02.1080p.WEB-DL",
                Some("Mon, 13 Apr 2026 03:49:47 +0200"),
                0.75,
            ),
            cand_pubdate(
                "Unique.Release.1080p.WEB",
                Some("Mon, 20 Apr 2026 03:20:16 +0200"),
                0.80,
            ),
            cand_pubdate(
                "Euphoria.S03E02.1080p.WEB-DL",
                Some("Mon, 20 Apr 2026 03:20:16 +0200"),
                0.70,
            ),
        ];
        dedup_candidates_by_name(&mut cands);
        assert_eq!(cands.len(), 2, "collapses duplicate name down to one");
        let euphoria = cands
            .iter()
            .find(|c| c.name == "Euphoria.S03E02.1080p.WEB-DL")
            .expect("euphoria kept");
        assert_eq!(
            euphoria.publish_date.as_deref(),
            Some("Mon, 20 Apr 2026 03:20:16 +0200"),
            "newer publish_date variant wins even with lower kobload_score",
        );
    }

    #[test]
    fn dedup_tiebreaks_on_kobload_score_when_dates_match() {
        let date = Some("2026-04-20");
        let mut cands = vec![
            cand_pubdate("Release.A", date, 0.60),
            cand_pubdate("Release.A", date, 0.95),
            cand_pubdate("Release.A", date, 0.80),
        ];
        dedup_candidates_by_name(&mut cands);
        assert_eq!(cands.len(), 1);
        assert!((cands[0].kobload_score - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn dedup_is_case_insensitive() {
        let mut cands = vec![
            cand_pubdate("Release.A.1080p", None, 0.5),
            cand_pubdate("RELEASE.A.1080P", None, 0.8),
        ];
        dedup_candidates_by_name(&mut cands);
        assert_eq!(cands.len(), 1);
    }

    #[test]
    fn dedup_preserves_distinct_names_and_order() {
        let mut cands = vec![
            cand_pubdate("Release.A", None, 0.5),
            cand_pubdate("Release.B", None, 0.6),
            cand_pubdate("Release.C", None, 0.7),
        ];
        dedup_candidates_by_name(&mut cands);
        assert_eq!(cands.len(), 3);
        assert_eq!(cands[0].name, "Release.A");
        assert_eq!(cands[1].name, "Release.B");
        assert_eq!(cands[2].name, "Release.C");
    }

    #[test]
    fn dedup_handles_missing_publish_date() {
        // Variant with a date should beat a variant without one.
        let mut cands = vec![
            cand_pubdate("Release.A", None, 0.9),
            cand_pubdate("Release.A", Some("2026-04-20"), 0.1),
        ];
        dedup_candidates_by_name(&mut cands);
        assert_eq!(cands.len(), 1);
        assert!(cands[0].publish_date.is_some());
    }

    #[test]
    fn dedup_no_op_on_empty_or_single() {
        let mut empty: Vec<ReviewCandidate> = vec![];
        dedup_candidates_by_name(&mut empty);
        assert!(empty.is_empty());

        let mut one = vec![cand_pubdate("Release.A", None, 0.5)];
        dedup_candidates_by_name(&mut one);
        assert_eq!(one.len(), 1);
    }
}
