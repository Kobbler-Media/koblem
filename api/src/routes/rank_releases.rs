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
pub struct RankRequest {
    pub wanted: Value,
    #[serde(default)]
    pub quality_preferences: Value,
    pub releases: Vec<Value>,
    /// Per-request sampling override. Merged on top of the DB default
    /// (`sampling_rank_releases`) and the `Deterministic` baseline.
    #[serde(default)]
    pub sampling: Option<SamplingConfig>,
}

#[derive(Serialize)]
pub struct RankResponse {
    pub ranked: Vec<RankedRelease>,
}

#[derive(Serialize)]
pub struct RankedRelease {
    pub release: String,
    pub score: u32,
    pub reason: String,
}

pub async fn rank_releases(
    State(state): State<AppState>,
    Json(input): Json<RankRequest>,
) -> impl IntoResponse {
    // Baseline pins `enable_thinking: false` only — no sampling preset.
    // See review_pipeline for rationale (Qwen distill loops on temp=0).
    let baseline = SamplingConfig {
        enable_thinking: Some(false),
        ..SamplingConfig::default()
    };
    let db_default = crate::sampling::resolve_sampling(&state.db, "sampling_rank_releases").await;
    let request_override = input.sampling.clone().unwrap_or_default();
    let sampling = baseline.overlay(&db_default).overlay(&request_override);
    let mut body = prompts::rank_releases_messages(
        &input.wanted,
        &input.quality_preferences,
        &input.releases,
        &sampling,
    );
    prompts::apply_grammar_to_body(&state.db, &mut body, &sampling).await;

    let mut log_seed = CreateLog::new(
        Some("rank_releases".to_string()),
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
            tracing::warn!("failed to start rank_releases llm log: {e}");
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
            error!("llama-server error in rank-releases: {e}");
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

    match parse_rank_response(content) {
        Ok(result) => Json(result).into_response(),
        Err(e) => {
            error!("failed to parse rank response: {e}, raw: {content}");
            Json(RankResponse { ranked: vec![] }).into_response()
        }
    }
}

fn parse_rank_response(content: &str) -> Result<RankResponse, String> {
    // extract_json_payload handles leading fences, embedded fences in prose,
    // doubled braces, and reasoning-content fallback in one helper. Raw
    // response stays in llm_logs.response_content for diagnosis.
    let parsed: Value = crate::think::extract_json_payload(content, None)
        .ok_or_else(|| "JSON parse error: no JSON object found in response".to_string())?;

    let ranked = parsed
        .get("ranked")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|item| RankedRelease {
                    release: item
                        .get("release")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    score: item.get("score").and_then(|s| s.as_u64()).unwrap_or(0) as u32,
                    reason: item
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(RankResponse { ranked })
}
