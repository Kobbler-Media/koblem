use serde_json::Value;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use super::Result;
use crate::context::ReviewContext;

/// Full review session for insert.
pub struct NewReviewSession {
    pub request: Value,
    pub wanted_summary: String,
    pub context_used: Vec<String>,
    pub prompt_sent: String,
    pub llm_response: Option<String>,
    pub verdicts: Option<Value>,
    pub notes: Option<String>,
    pub context_assembly_ms: Option<i64>,
    pub llm_inference_ms: Option<i64>,
    /// FK to the `llm_logs` row that produced this review. None when the path
    /// didn't open a log row (legacy data, playground non-streaming flows).
    pub llm_log_id: Option<Uuid>,
}

#[derive(FromRow)]
pub struct ReviewSessionSummary {
    pub id: Uuid,
    pub wanted_summary: String,
    pub agreed_count: i64,
    pub vetoed_count: i64,
    pub promoted_count: i64,
    pub legacy_count: i64,
    pub context_assembly_ms: Option<i64>,
    pub llm_inference_ms: Option<i64>,
    pub created_at: OffsetDateTime,
}

#[derive(FromRow)]
pub struct ReviewSessionDetail {
    pub id: Uuid,
    pub request: Value,
    pub wanted_summary: String,
    pub context_used: Vec<String>,
    pub prompt_sent: String,
    pub llm_response: Option<String>,
    pub verdicts: Option<Value>,
    pub notes: Option<String>,
    pub context_assembly_ms: Option<i64>,
    pub llm_inference_ms: Option<i64>,
    pub created_at: OffsetDateTime,
    pub llm_log_id: Option<Uuid>,
    pub thinking: Option<String>,
}

pub async fn create_session(pool: &PgPool, session: &NewReviewSession) -> Result<Uuid> {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO review_sessions \
         (request, wanted_summary, context_used, prompt_sent, llm_response, verdicts, notes, \
          context_assembly_ms, llm_inference_ms, llm_log_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         RETURNING id",
    )
    .bind(&session.request)
    .bind(&session.wanted_summary)
    .bind(&session.context_used)
    .bind(&session.prompt_sent)
    .bind(&session.llm_response)
    .bind(&session.verdicts)
    .bind(&session.notes)
    .bind(session.context_assembly_ms)
    .bind(session.llm_inference_ms)
    .bind(session.llm_log_id)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

pub async fn list_sessions(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<Vec<ReviewSessionSummary>> {
    let rows: Vec<ReviewSessionSummary> = sqlx::query_as(
        "SELECT id, wanted_summary, \
         CASE WHEN jsonb_typeof(verdicts) = 'object' \
              THEN COALESCE(jsonb_array_length(verdicts->'agreed'), 0)::bigint \
              ELSE 0::bigint END AS agreed_count, \
         CASE WHEN jsonb_typeof(verdicts) = 'object' \
              THEN COALESCE(jsonb_array_length(verdicts->'vetoed'), 0)::bigint \
              ELSE 0::bigint END AS vetoed_count, \
         CASE WHEN jsonb_typeof(verdicts) = 'object' \
              THEN COALESCE(jsonb_array_length(verdicts->'promoted'), 0)::bigint \
              ELSE 0::bigint END AS promoted_count, \
         CASE WHEN jsonb_typeof(verdicts) = 'array' \
              THEN COALESCE(jsonb_array_length(verdicts), 0)::bigint \
              ELSE 0::bigint END AS legacy_count, \
         context_assembly_ms, llm_inference_ms, created_at \
         FROM review_sessions \
         ORDER BY created_at DESC \
         LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_session(pool: &PgPool, id: Uuid) -> Result<Option<ReviewSessionDetail>> {
    let row: Option<ReviewSessionDetail> = sqlx::query_as(
        "SELECT rs.id, rs.request, rs.wanted_summary, rs.context_used, rs.prompt_sent, \
                rs.llm_response, rs.verdicts, rs.notes, rs.context_assembly_ms, \
                rs.llm_inference_ms, rs.created_at, rs.llm_log_id, \
                ll.response_reasoning_content AS thinking \
         FROM review_sessions rs \
         LEFT JOIN llm_logs ll ON ll.id = rs.llm_log_id \
         WHERE rs.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub struct StoreReviewParams<'a> {
    pub request_json: Value,
    pub ctx: &'a ReviewContext,
    pub prompt_sent: &'a str,
    pub llm_response: Option<&'a str>,
    pub verdicts_json: Option<Value>,
    pub notes: Option<String>,
    pub context_assembly_ms: u64,
    pub llm_inference_ms: u64,
    pub llm_log_id: Option<Uuid>,
}

pub async fn store_review(pool: &PgPool, params: StoreReviewParams<'_>) -> Option<String> {
    let session = NewReviewSession {
        request: params.request_json,
        wanted_summary: params.ctx.wanted_summary.clone(),
        context_used: params.ctx.context_used.clone(),
        prompt_sent: params.prompt_sent.to_string(),
        llm_response: params.llm_response.map(|s| s.to_string()),
        verdicts: params.verdicts_json,
        notes: params.notes,
        context_assembly_ms: Some(params.context_assembly_ms as i64),
        llm_inference_ms: Some(params.llm_inference_ms as i64),
        llm_log_id: params.llm_log_id,
    };

    match create_session(pool, &session).await {
        Ok(id) => Some(id.to_string()),
        Err(e) => {
            warn!("failed to store review session: {e}");
            None
        }
    }
}
