//! `/filter-releases` — RSS prefilter pass.
//!
//! Used as the cheap "does this even belong to my wanted item" gate in front
//! of the more expensive `/api/v1/review-candidates` pipeline. Caller passes
//! a wanted item (just title + free-text context) and a flat list of release
//! filenames. The LLM returns a blind accept-list (1-based indices into the
//! input list) of releases that semantically match the wanted item.
//!
//! Quality judgment is intentionally NOT done here — that's review's job.
//! This pass is purely "is this the right show/event/round at all?".

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use koblem_client::types::{GrammarMode, SamplingConfig};
use serde::{Deserialize, Serialize};
use tracing::error;

use std::time::Duration;

use crate::AppState;
use crate::prompts;
use crate::repositories::llm_logs::{self, CreateLog};
use crate::review_helpers::{finish_reason, parse_review_response, prepend_note, truncation_note};
use crate::routes::chat::{grammar_tag, preset_tag};

const PROGRESS_FLUSH: Duration = Duration::from_millis(1500);

#[derive(Deserialize)]
pub struct FilterRequest {
    /// The wanted item's primary identifier — usually the show or movie title.
    pub wanted_title: String,
    /// Free-text context describing the wanted item more precisely. Examples:
    /// `"Round 22 Valencia GP, Race session, 2025-11-16"`,
    /// `"S02E07 'Chikhai Bardo', aired 2025-02-21"`,
    /// `"Original 2026 release, not the 1997 film"`.
    /// The LLM uses this to disambiguate releases — keep it short and dense.
    #[serde(default)]
    pub wanted_context: String,
    pub releases: Vec<String>,
    /// Per-request sampling override. Merged on top of the DB default
    /// (`sampling_filter_releases`) and the hardcoded `Deterministic`
    /// baseline.
    #[serde(default)]
    pub sampling: Option<SamplingConfig>,
}

#[derive(Serialize)]
pub struct FilterMatch {
    /// 1-based index into the original `releases` array.
    pub idx: usize,
    /// Original release filename, echoed for caller convenience.
    pub name: String,
    /// LLM's one-sentence reason for the match.
    pub reason: String,
}

#[derive(Serialize)]
pub struct FilterResponse {
    pub matches: Vec<FilterMatch>,
    /// Batch-level note from the LLM (e.g. "rejected several Sprint sessions"),
    /// plus any truncation/parse warnings prepended by the server.
    pub notes: Option<String>,
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub duration_ms: u64,
}

pub async fn filter_releases(
    State(state): State<AppState>,
    Json(input): Json<FilterRequest>,
) -> impl IntoResponse {
    if input.releases.is_empty() {
        return Json(FilterResponse {
            matches: vec![],
            notes: None,
            prompt_tokens: None,
            completion_tokens: None,
            duration_ms: 0,
        })
        .into_response();
    }

    let start = std::time::Instant::now();
    // Baseline pins `enable_thinking: false` and uses the built-in `review`
    // grammar — historically the same accept-list shape used by the review
    // pipeline. No sampling preset baselined here, since greedy decoding
    // (temp=0) loops on the Qwen reasoning distill. Sampling falls through
    // to the DB default → llama-server flag defaults.
    let baseline = SamplingConfig {
        enable_thinking: Some(false),
        grammar: Some(GrammarMode::Review),
        ..SamplingConfig::default()
    };
    let db_default = crate::sampling::resolve_sampling(&state.db, "sampling_filter_releases").await;
    let request_override = input.sampling.clone().unwrap_or_default();
    let sampling = baseline.overlay(&db_default).overlay(&request_override);
    let mut body = prompts::filter_releases_messages(
        &input.wanted_title,
        &input.wanted_context,
        &input.releases,
        &sampling,
    );
    prompts::apply_grammar_to_body(&state.db, &mut body, &sampling).await;

    // Pre-insert a `running` log row so the row is visible in the UI for the
    // entire duration of the request. The streaming helper below UPDATEs this
    // row every PROGRESS_FLUSH with accumulated content + reasoning_content,
    // and finalizes it on success/error.
    let mut log_seed = CreateLog::new(
        Some("filter_releases".to_string()),
        body.get("messages").cloned().unwrap_or_default(),
    );
    log_seed.set_sampling_from_body(
        &body,
        preset_tag(&sampling),
        grammar_tag(&sampling).as_deref(),
    );
    // start_log failure shouldn't sink the request — proceed with a nil UUID
    // and the streaming helper's UPDATEs will silently no-op (every UPDATE
    // gates on `WHERE id = $1 AND status = 'running'`).
    let log_id = match llm_logs::start_log(&state.db, &log_seed).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("failed to start filter_releases llm log: {e}");
            uuid::Uuid::nil()
        }
    };

    let (_permit, response_body) = match state
        .llama
        .chat_completion_streaming_with_log(&body, state.db.clone(), log_id, PROGRESS_FLUSH)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("llama-server error in filter-releases: {e}");
            return (StatusCode::BAD_GATEWAY, format!("llama-server error: {e}")).into_response();
        }
    };

    let content = response_body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("{}");
    let finish = finish_reason(&response_body);

    let parsed = parse_review_response(content);

    // Resolve indices back to release names. Out-of-range indices are dropped
    // (the parser logs them; the user-visible signal is fewer matches than
    // the LLM thought it was emitting).
    let matches: Vec<FilterMatch> = parsed
        .accepts
        .iter()
        .filter_map(|a| {
            if a.idx == 0 || a.idx > input.releases.len() {
                None
            } else {
                Some(FilterMatch {
                    idx: a.idx,
                    name: input.releases[a.idx - 1].clone(),
                    reason: a.reason.clone(),
                })
            }
        })
        .collect();

    let mut notes = parsed.notes;
    if let Some(err) = parsed.parse_error {
        notes = Some(prepend_note(&notes, &err));
    }
    if let Some(warning) = truncation_note(&finish, parsed.accepts.len(), input.releases.len()) {
        notes = Some(prepend_note(&notes, &warning));
    }

    let prompt_tokens = response_body
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let completion_tokens = response_body
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let duration_ms = start.elapsed().as_millis() as u64;

    Json(FilterResponse {
        matches,
        notes,
        prompt_tokens,
        completion_tokens,
        duration_ms,
    })
    .into_response()
}
