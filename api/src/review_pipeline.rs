//! Shared review pipeline logic used by both the HTTP endpoint and the NATS consumer.

use std::time::Duration;

use koblem_client::types::{GrammarMode, ReviewRequest, ReviewResponse, SamplingConfig};

use crate::AppState;
use crate::context::assemble_context;
use crate::prompts::review_candidates_messages;
use crate::repositories::llm_logs::{self, CreateLog};
use crate::repositories::review_sessions::{self, StoreReviewParams};
use crate::review_helpers::{
    compute_diff, dedup_candidates_by_name, finish_reason, format_debug_prompt,
    parse_review_response, prepend_note, token_usage, truncation_note,
};
use crate::routes::chat::{grammar_tag, preset_tag};

const PROGRESS_FLUSH: Duration = Duration::from_millis(1500);

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("llama-server error: {0}")]
    Llama(#[from] crate::llama::Error),
    #[error("prompt engine error: {0}")]
    Engine(#[from] crate::prompt_engine::EngineError),
}

/// Run the full review pipeline: assemble context → sort → prompt → LLM → parse → diff → store.
///
/// Shared by the HTTP handler and the NATS consumer. The `store_session` flag
/// controls whether the result is persisted to the review_sessions table (HTTP
/// always stores; NATS stores too for audit trail).
pub async fn run_review(
    state: &AppState,
    mut request: ReviewRequest,
    store_session: bool,
    caller: &str,
) -> Result<ReviewResponse, ReviewError> {
    // 0. Collapse duplicate release names before anything else touches the
    //    candidate list. Kobload emits one row per GUID so the same title from
    //    two indexers shows up twice — at scale that drowns the LLM in repeats
    //    and anchors it on the first block of dupes.
    let before_dedup = request.candidates.len();
    dedup_candidates_by_name(&mut request.candidates);
    let after_dedup = request.candidates.len();
    if before_dedup != after_dedup {
        tracing::info!(
            before = before_dedup,
            after = after_dedup,
            dropped = before_dedup - after_dedup,
            "Deduplicated review candidates by name"
        );
    }

    // 1. Assemble context (before LLM semaphore)
    let ctx_start = std::time::Instant::now();
    let library_provider = state
        .library_provider
        .as_ref()
        .map(|p| p.as_ref() as &dyn kob_providers::LibraryProvider);
    let ctx = assemble_context(&request, library_provider, &state.db).await;
    let context_assembly_ms = ctx_start.elapsed().as_millis() as u64;

    // 2. Sort candidates by relevance then build prompt
    let sort_tokens = crate::prompts::build_sort_token_bag(&request);
    crate::prompts::sort_candidates_by_relevance(&mut request.candidates, &sort_tokens);
    // Baseline pins `enable_thinking: false` so the chat template doesn't
    // auto-prepend `<think>` (callers can flip per-request). Grammar
    // baselines to the built-in `review` GBNF — historically the only
    // grammar this pipeline shipped. Crucially does NOT pin a sampling
    // preset — greedy decoding (temp=0) loops on the Qwen reasoning distill,
    // so leave temp/top_p/etc. to the DB default and ultimately
    // llama-server's flag defaults (--temp 0.7 etc.).
    let baseline = SamplingConfig {
        enable_thinking: Some(false),
        grammar: Some(GrammarMode::Review),
        ..SamplingConfig::default()
    };
    let db_default =
        crate::sampling::resolve_sampling(&state.db, "sampling_review_candidates").await;
    let request_override = request.sampling.clone().unwrap_or_default();
    let sampling = baseline.overlay(&db_default).overlay(&request_override);
    let mut body =
        review_candidates_messages(&state.db, &ctx, &request, &request.candidates, &sampling)
            .await
            .map_err(ReviewError::Engine)?;
    crate::prompts::apply_grammar_to_body(&state.db, &mut body, &sampling).await;
    let debug_prompt = format_debug_prompt(&body);

    // 3. Open an `llm_logs` row in `running` state so the UI shows the
    //    request the moment llama-server starts churning, not only when it
    //    finishes (which can be many minutes for big candidate lists).
    let mut log_seed = CreateLog::new(
        Some(caller.to_string()),
        body.get("messages").cloned().unwrap_or_default(),
    );
    log_seed.set_sampling_from_body(
        &body,
        preset_tag(&sampling),
        grammar_tag(&sampling).as_deref(),
    );
    // start_log failure shouldn't sink the review — proceed with a nil UUID
    // and the streaming helper's UPDATEs will silently no-op (every UPDATE
    // gates on `WHERE id = $1 AND status = 'running'` so a non-existent id
    // matches zero rows).
    let log_id = match llm_logs::start_log(&state.db, &log_seed).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("failed to start review_pipeline llm log: {e}");
            uuid::Uuid::nil()
        }
    };

    // 4. Acquire LLM semaphore, stream response with live progress UPDATEs.
    let llm_start = std::time::Instant::now();
    let (_permit, response_body) = state
        .llama
        .chat_completion_streaming_with_log(&body, state.db.clone(), log_id, PROGRESS_FLUSH)
        .await?;
    let llm_inference_ms = llm_start.elapsed().as_millis() as u64;

    let content = response_body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("{}");
    let finish = finish_reason(&response_body);

    let parsed = parse_review_response(content);
    let diff = compute_diff(&parsed.accepts, &request.candidates);
    let (prompt_tokens, completion_tokens) = token_usage(&response_body);

    let mut notes = parsed.notes;
    if let Some(err) = parsed.parse_error {
        notes = Some(prepend_note(&notes, &err));
    }
    if let Some(warning) = truncation_note(&finish, parsed.accepts.len(), request.candidates.len())
    {
        notes = Some(prepend_note(&notes, &warning));
    }
    if !diff.dropped_indices.is_empty() {
        notes = Some(prepend_note(
            &notes,
            &format!(
                "⚠ LLM emitted {} out-of-range index/indices: {:?}",
                diff.dropped_indices.len(),
                diff.dropped_indices
            ),
        ));
    }

    // Store session for audit trail
    let session_id = if store_session {
        let verdicts_json = serde_json::json!({
            "accepts": parsed.accepts,
            "agreed": diff.agreed,
            "vetoed": diff.vetoed,
            "promoted": diff.promoted,
            "dropped_indices": diff.dropped_indices,
        });
        review_sessions::store_review(
            &state.db,
            StoreReviewParams {
                request_json: serde_json::to_value(&request).unwrap_or_default(),
                ctx: &ctx,
                prompt_sent: &debug_prompt,
                llm_response: Some(content),
                verdicts_json: Some(verdicts_json),
                notes: notes.clone(),
                context_assembly_ms,
                llm_inference_ms,
                // Skip the FK when start_log fell back to nil so we don't
                // store a dangling reference (FK is nullable + ON DELETE SET
                // NULL, but a real nil UUID would never resolve on join).
                llm_log_id: (!log_id.is_nil()).then_some(log_id),
            },
        )
        .await
    } else {
        None
    };

    Ok(ReviewResponse {
        agreed: diff.agreed,
        vetoed: diff.vetoed,
        promoted: diff.promoted,
        accepts: parsed.accepts,
        dropped_indices: diff.dropped_indices,
        context_used: ctx.context_used,
        notes,
        debug_prompt: Some(debug_prompt),
        raw_llm_response: Some(content.to_string()),
        prompt_tokens,
        completion_tokens,
        context_assembly_ms: Some(context_assembly_ms),
        llm_inference_ms: Some(llm_inference_ms),
        session_id,
    })
}
