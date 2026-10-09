use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use koblem_client::types::SamplingConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, warn};
use uuid::Uuid;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::llm_logs::{self as llm_logs_repo, CreateLog};
use crate::routes::chat::{grammar_tag, preset_tag};
use crate::think::extract_reasoning_content;

#[derive(Deserialize)]
pub struct ListLogsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    caller: Option<String>,
}

fn default_limit() -> i64 {
    50
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/llm-logs", get(list_logs))
        .route("/api/v1/llm-logs/{id}", get(get_log))
        .route("/api/v1/llm-logs/{id}/rerun", post(rerun_log))
}

async fn list_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListLogsQuery>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match llm_logs_repo::list_logs(
        &state.db,
        query.limit,
        query.offset,
        query.caller.as_deref(),
    )
    .await
    {
        Ok(logs) => Json(logs).into_response(),
        Err(err) => {
            warn!(error = %err, "failed to list llm logs");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn get_log(
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

    match llm_logs_repo::get_log(&state.db, id).await {
        Ok(Some(log)) => Json(log).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "log not found").into_response(),
        Err(err) => {
            warn!(error = %err, "failed to get llm log");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

#[derive(Serialize)]
struct RerunResponse {
    /// New log id so the UI can open it directly.
    log_id: Uuid,
    /// Verbatim llama-server response body.
    response: Value,
}

/// Replay a past `/v1/chat/completions`-shaped request with an override
/// `SamplingConfig`. Loads the original log, reconstructs a request body
/// from its `request_messages` + `request_tools`, applies the override
/// sampling, forwards to llama-server, and writes a new `llm_logs` row.
///
/// The override body is a bare `SamplingConfig` (preset + the six knobs).
/// Sending `{}` replays with the original request's sampling, because
/// `apply_to` will only inject fields that are `Some` after resolution —
/// but rows reconstructed from the DB don't carry the original's sampling
/// *keys* on the body, so in practice an empty override means "whatever
/// llama-server's server-side flags decide".
async fn rerun_log(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(override_cfg): Json<SamplingConfig>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let orig = match llm_logs_repo::get_log(&state.db, id).await {
        Ok(Some(log)) => log,
        Ok(None) => return (StatusCode::NOT_FOUND, "log not found").into_response(),
        Err(err) => {
            warn!(error = %err, "failed to get llm log for rerun");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };

    // Rebuild a chat-completions body from the stored messages + tools.
    let mut body = json!({
        "model": "local",
        "messages": orig.request_messages.clone(),
    });
    if let Some(ref tools) = orig.request_tools {
        body["tools"] = tools.clone();
    }
    override_cfg.apply_to(&mut body);
    crate::prompts::apply_grammar_to_body(&state.db, &mut body, &override_cfg).await;

    let start = std::time::Instant::now();
    let (_permit, resp_body) = match state.llama.chat_completion_json(&body).await {
        Ok(r) => r,
        Err(e) => {
            error!("llama-server error on rerun: {e}");
            return (StatusCode::BAD_GATEWAY, format!("llama-server error: {e}")).into_response();
        }
    };
    let duration_ms = start.elapsed().as_millis() as i64;

    let content = resp_body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    let prompt_tokens = resp_body
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let completion_tokens = resp_body
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let finish = resp_body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut log = CreateLog::new(Some(format!("rerun:{}", orig.id)), orig.request_messages);
    log.request_tools = orig.request_tools;
    log.set_sampling_from_body(
        &body,
        preset_tag(&override_cfg),
        grammar_tag(&override_cfg).as_deref(),
    );
    log.response_content = content;
    log.response_reasoning_content = extract_reasoning_content(&resp_body);
    log.finish_reason = finish;
    log.duration_ms = Some(duration_ms);
    log.prompt_tokens = prompt_tokens;
    log.completion_tokens = completion_tokens;

    let new_id = match llm_logs_repo::create_log(&state.db, &log).await {
        Ok(id) => id,
        Err(err) => {
            warn!(error = %err, "failed to store rerun log");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };

    Json(RerunResponse {
        log_id: new_id,
        response: resp_body,
    })
    .into_response()
}
