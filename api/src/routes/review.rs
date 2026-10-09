use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use koblem_client::types::{ReviewRequest, ReviewResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use tracing::error;
use uuid::Uuid;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::review_sessions;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/review-candidates", post(review_candidates))
        .route("/api/v1/review-sessions", get(list_sessions))
        .route("/api/v1/review-sessions/{id}", get(get_session))
        .route("/api/v1/review-sessions/{id}/replay", post(replay_session))
}

async fn review_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ReviewRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if request.candidates.is_empty() {
        return Json(empty_review_response()).into_response();
    }

    match crate::review_pipeline::run_review(&state, request, true, "review_pipeline:http").await {
        Ok(response) => Json(response).into_response(),
        Err(e) => {
            error!("review-candidates pipeline error: {e}");
            (StatusCode::BAD_GATEWAY, format!("llama-server error: {e}")).into_response()
        }
    }
}

/// Empty/default ReviewResponse used by short-circuit paths (empty candidate
/// list, replay before any LLM call, etc.).
fn empty_review_response() -> ReviewResponse {
    ReviewResponse {
        agreed: vec![],
        vetoed: vec![],
        promoted: vec![],
        accepts: vec![],
        dropped_indices: vec![],
        context_used: vec![],
        notes: None,
        debug_prompt: None,
        raw_llm_response: None,
        prompt_tokens: None,
        completion_tokens: None,
        context_assembly_ms: Some(0),
        llm_inference_ms: Some(0),
        session_id: None,
    }
}

// ── List / Get sessions ──────────────────────────────────────────────

#[derive(Deserialize)]
struct ListSessionsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Serialize)]
struct SessionSummaryResponse {
    id: String,
    wanted_summary: String,
    agreed_count: i64,
    vetoed_count: i64,
    promoted_count: i64,
    /// Non-zero only for legacy (pre-v3) sessions whose verdicts were stored
    /// as a flat array. UI falls back to showing this when the bucket counts
    /// are all zero.
    legacy_count: i64,
    context_assembly_ms: Option<i64>,
    llm_inference_ms: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Serialize)]
struct SessionDetailResponse {
    id: String,
    request: Value,
    wanted_summary: String,
    context_used: Vec<String>,
    prompt_sent: String,
    llm_response: Option<String>,
    verdicts: Option<Value>,
    notes: Option<String>,
    context_assembly_ms: Option<i64>,
    llm_inference_ms: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    /// FK to the underlying `llm_logs` row, when one exists.
    llm_log_id: Option<String>,
    /// CoT trace pulled from the joined `llm_logs.response_reasoning_content`.
    thinking: Option<String>,
}

async fn list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListSessionsQuery>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match review_sessions::list_sessions(&state.db, params.limit, params.offset).await {
        Ok(sessions) => {
            let items: Vec<_> = sessions
                .into_iter()
                .map(|s| SessionSummaryResponse {
                    id: s.id.to_string(),
                    wanted_summary: s.wanted_summary,
                    agreed_count: s.agreed_count,
                    vetoed_count: s.vetoed_count,
                    promoted_count: s.promoted_count,
                    legacy_count: s.legacy_count,
                    context_assembly_ms: s.context_assembly_ms,
                    llm_inference_ms: s.llm_inference_ms,
                    created_at: s.created_at,
                })
                .collect();
            Json(items).into_response()
        }
        Err(e) => {
            error!("failed to list review sessions: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match review_sessions::get_session(&state.db, id).await {
        Ok(Some(s)) => Json(SessionDetailResponse {
            id: s.id.to_string(),
            request: s.request,
            wanted_summary: s.wanted_summary,
            context_used: s.context_used,
            prompt_sent: s.prompt_sent,
            llm_response: s.llm_response,
            verdicts: s.verdicts,
            notes: s.notes,
            context_assembly_ms: s.context_assembly_ms,
            llm_inference_ms: s.llm_inference_ms,
            created_at: s.created_at,
            llm_log_id: s.llm_log_id.map(|id| id.to_string()),
            thinking: s.thinking,
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("failed to get review session: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

#[derive(Serialize)]
struct ReplayResponse {
    /// The stored request that was re-run (for audit / sanity checking).
    original_request: Value,
    /// The original verdicts JSON (whatever shape was stored). Returned so the
    /// UI can diff old-vs-new without making a second fetch.
    original_verdicts: Option<Value>,
    /// The original notes from the stored session (for the same reason).
    original_notes: Option<String>,
    /// Fresh review result from re-running the request through the current
    /// pipeline (current prompt, current grammar, current model).
    fresh: ReviewResponse,
}

/// Re-run a stored session through the review pipeline as it stands today.
///
/// Used by the history page "Replay" button so users can compare a frozen
/// real-world request against whatever prompt/model changes have happened
/// since. The replay deserializes `request_json` back into a `ReviewRequest`
/// and runs it through the same code path as `/api/v1/review-candidates`,
/// then returns both the original and the fresh result side-by-side.
async fn replay_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let session = match review_sessions::get_session(&state.db, id).await {
        Ok(Some(s)) => s,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("failed to load session for replay: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };

    let request: ReviewRequest = match serde_json::from_value(session.request.clone()) {
        Ok(r) => r,
        Err(e) => {
            error!("session {id} has unparseable request_json: {e}");
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("stored request is not a valid ReviewRequest: {e}"),
            )
                .into_response();
        }
    };

    // Replay does NOT persist a new session. Replays are throwaway debug
    // runs — if you want to keep them, hit /api/v1/review-candidates directly
    // with the same body.
    let fresh =
        match crate::review_pipeline::run_review(&state, request, false, "review_pipeline:rerun")
            .await
        {
            Ok(response) => response,
            Err(e) => {
                error!("llama-server error in replay: {e}");
                return (StatusCode::BAD_GATEWAY, format!("llama-server error: {e}"))
                    .into_response();
            }
        };

    Json(ReplayResponse {
        original_request: session.request,
        original_verdicts: session.verdicts,
        original_notes: session.notes,
        fresh,
    })
    .into_response()
}
