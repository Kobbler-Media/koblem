use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use koblem_client::types::SamplingConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::error;

use crate::AppState;
use crate::prompts;
use crate::repositories::llm_logs::{self, CreateLog};
use crate::routes::chat::{grammar_tag, preset_tag};

const PROGRESS_FLUSH: Duration = Duration::from_millis(1500);

#[derive(Deserialize)]
pub struct ValidateRequest {
    pub release_name: String,
    pub expected_show: String,
    #[serde(default)]
    pub expected_season_episode: String,
    #[serde(default)]
    pub expected_title: String,
    /// Per-request sampling override. Merged on top of the DB default
    /// (`sampling_validate_release`) and the `Deterministic` baseline.
    #[serde(default)]
    pub sampling: Option<SamplingConfig>,
}

#[derive(Serialize)]
pub struct ValidateResponse {
    pub valid: bool,
    pub issues: Vec<String>,
}

pub async fn validate_release(
    State(state): State<AppState>,
    Json(input): Json<ValidateRequest>,
) -> impl IntoResponse {
    // Baseline pins `enable_thinking: false` only — no sampling preset.
    // See review_pipeline for rationale (Qwen distill loops on temp=0).
    let baseline = SamplingConfig {
        enable_thinking: Some(false),
        ..SamplingConfig::default()
    };
    let db_default =
        crate::sampling::resolve_sampling(&state.db, "sampling_validate_release").await;
    let request_override = input.sampling.clone().unwrap_or_default();
    let sampling = baseline.overlay(&db_default).overlay(&request_override);
    let mut body = prompts::validate_release_messages(
        &input.release_name,
        &input.expected_show,
        &input.expected_season_episode,
        &input.expected_title,
        &sampling,
    );
    prompts::apply_grammar_to_body(&state.db, &mut body, &sampling).await;

    let mut log_seed = CreateLog::new(
        Some("validate_release".to_string()),
        body.get("messages").cloned().unwrap_or_default(),
    );
    log_seed.set_sampling_from_body(
        &body,
        preset_tag(&sampling),
        grammar_tag(&sampling).as_deref(),
    );
    // start_log failure shouldn't sink the request — proceed with a nil UUID
    // and the streaming helper's UPDATEs will silently no-op.
    let log_id = match llm_logs::start_log(&state.db, &log_seed).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("failed to start validate_release llm log: {e}");
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
            error!("llama-server error in validate-release: {e}");
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

    match parse_validate_response(content) {
        Ok(result) => Json(result).into_response(),
        Err(e) => {
            error!("failed to parse validate response: {e}, raw: {content}");
            Json(ValidateResponse {
                valid: false,
                issues: vec![format!("parse error: {e}")],
            })
            .into_response()
        }
    }
}

fn parse_validate_response(content: &str) -> Result<ValidateResponse, String> {
    // extract_json_payload handles leading fences, embedded fences in prose,
    // doubled braces, and reasoning-content fallback in one helper. Raw
    // response stays in llm_logs.response_content for diagnosis. Strict-parse
    // probe up-front so we can surface "had to recover" in the validation
    // issues list when the model emitted anything non-canonical.
    let strict_failed = serde_json::from_str::<Value>(content.trim()).is_err();
    let parsed: Value = crate::think::extract_json_payload(content, None)
        .ok_or_else(|| "JSON parse error: no JSON object found in response".to_string())?;

    let valid = parsed
        .get("valid")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut issues: Vec<String> = parsed
        .get("issues")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if strict_failed {
        issues.insert(
            0,
            "⚠ recovered JSON via lenient extraction (prose leak or doubled-brace)".into(),
        );
    }

    Ok(ValidateResponse { valid, issues })
}
