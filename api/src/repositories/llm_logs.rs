use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::Result;

pub struct CreateLog {
    pub caller: Option<String>,
    pub request_messages: Value,
    pub request_tools: Option<Value>,
    pub request_temperature: Option<f64>,
    pub request_max_tokens: Option<i32>,
    pub request_top_p: Option<f64>,
    pub request_top_k: Option<i32>,
    pub request_min_p: Option<f64>,
    pub request_presence_penalty: Option<f64>,
    pub request_repetition_penalty: Option<f64>,
    pub request_enable_thinking: Option<bool>,
    /// Named preset in effect for this request, if any (e.g. `"deterministic"`,
    /// `"qwen_instruct_general"`). Stored as the snake_case tag that
    /// [`koblem_client::types::SamplingPreset`] serializes to.
    pub sampling_preset: Option<String>,
    /// Grammar mode tag (`"off"`, `"review"`, or a user-defined name).
    /// `None` when the caller didn't pick a mode (legacy / pure pass-through).
    pub request_grammar: Option<String>,
    pub streaming: bool,
    pub response_content: Option<String>,
    pub response_reasoning_content: Option<String>,
    pub response_tool_calls: Option<Value>,
    pub finish_reason: Option<String>,
    pub tool_rewrite_applied: bool,
    pub duration_ms: Option<i64>,
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
}

impl CreateLog {
    pub fn new(caller: Option<String>, request_messages: Value) -> Self {
        Self {
            caller,
            request_messages,
            request_tools: None,
            request_temperature: None,
            request_max_tokens: None,
            request_top_p: None,
            request_top_k: None,
            request_min_p: None,
            request_presence_penalty: None,
            request_repetition_penalty: None,
            request_enable_thinking: None,
            sampling_preset: None,
            request_grammar: None,
            streaming: false,
            response_content: None,
            response_reasoning_content: None,
            response_tool_calls: None,
            finish_reason: None,
            tool_rewrite_applied: false,
            duration_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
        }
    }

    pub fn set_sampling_from_body(
        &mut self,
        body: &Value,
        preset: Option<&str>,
        grammar: Option<&str>,
    ) {
        let get_f64 = |k| body.get(k).and_then(Value::as_f64);
        let get_i32 = |k| body.get(k).and_then(Value::as_i64).map(|v| v as i32);
        self.request_temperature = get_f64("temperature");
        self.request_top_p = get_f64("top_p");
        self.request_top_k = get_i32("top_k");
        self.request_min_p = get_f64("min_p");
        self.request_presence_penalty = get_f64("presence_penalty");
        self.request_repetition_penalty = get_f64("repetition_penalty");
        self.request_max_tokens = get_i32("max_tokens");
        self.sampling_preset = preset.map(str::to_string);
        self.request_grammar = grammar.map(str::to_string);
        // OpenAI-compatible callers carry thinking via the nested
        // chat_template_kwargs.enable_thinking. Capture both shapes.
        self.request_enable_thinking = body
            .get("enable_thinking")
            .and_then(Value::as_bool)
            .or_else(|| {
                body.get("chat_template_kwargs")
                    .and_then(|k| k.get("enable_thinking"))
                    .and_then(Value::as_bool)
            });
    }
}

#[derive(Serialize)]
pub struct LogSummary {
    pub id: Uuid,
    pub caller: Option<String>,
    pub message_preview: Option<String>,
    pub streaming: bool,
    pub duration_ms: Option<i64>,
    pub status: String,
    pub completion_tokens: Option<i32>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Serialize)]
pub struct LogDetail {
    pub id: Uuid,
    pub caller: Option<String>,
    pub request_messages: Value,
    pub request_tools: Option<Value>,
    pub request_temperature: Option<f64>,
    pub request_max_tokens: Option<i32>,
    pub request_top_p: Option<f64>,
    pub request_top_k: Option<i32>,
    pub request_min_p: Option<f64>,
    pub request_presence_penalty: Option<f64>,
    pub request_repetition_penalty: Option<f64>,
    pub request_enable_thinking: Option<bool>,
    pub sampling_preset: Option<String>,
    pub request_grammar: Option<String>,
    pub streaming: bool,
    pub response_content: Option<String>,
    pub response_reasoning_content: Option<String>,
    pub response_tool_calls: Option<Value>,
    pub finish_reason: Option<String>,
    pub tool_rewrite_applied: bool,
    pub duration_ms: Option<i64>,
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub status: String,
    pub error_message: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Single-shot insert for endpoints that don't stream. Uses status='complete'
/// since the caller already has the final response. Kept for the proxy +
/// raw chat paths; the four review-pipeline endpoints use [`start_log`] +
/// [`finalize_log_success`] instead so they get live progress UPDATEs while
/// llama-server churns.
pub async fn create_log(pool: &PgPool, log: &CreateLog) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO llm_logs (
            id, caller, request_messages, request_tools, request_temperature,
            request_max_tokens, request_top_p, request_top_k, request_min_p,
            request_presence_penalty, request_repetition_penalty,
            request_enable_thinking, sampling_preset, request_grammar,
            streaming, response_content, response_reasoning_content,
            response_tool_calls, finish_reason, tool_rewrite_applied,
            duration_ms, prompt_tokens, completion_tokens, status
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, 'complete')
        "#,
        id,
        log.caller,
        log.request_messages,
        log.request_tools,
        log.request_temperature,
        log.request_max_tokens,
        log.request_top_p,
        log.request_top_k,
        log.request_min_p,
        log.request_presence_penalty,
        log.request_repetition_penalty,
        log.request_enable_thinking,
        log.sampling_preset,
        log.request_grammar,
        log.streaming,
        log.response_content,
        log.response_reasoning_content,
        log.response_tool_calls,
        log.finish_reason,
        log.tool_rewrite_applied,
        log.duration_ms,
        log.prompt_tokens,
        log.completion_tokens,
    )
    .execute(pool)
    .await?;
    Ok(id)
}

/// Insert a log row in `running` state at request start. The caller must
/// later call [`update_log_progress`] / [`finalize_log_success`] /
/// [`finalize_log_error`] keyed by the returned id.
pub async fn start_log(pool: &PgPool, log: &CreateLog) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO llm_logs (
            id, caller, request_messages, request_tools, request_temperature,
            request_max_tokens, request_top_p, request_top_k, request_min_p,
            request_presence_penalty, request_repetition_penalty,
            request_enable_thinking, sampling_preset, request_grammar,
            streaming, tool_rewrite_applied, status
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, 'running')
        "#,
        id,
        log.caller,
        log.request_messages,
        log.request_tools,
        log.request_temperature,
        log.request_max_tokens,
        log.request_top_p,
        log.request_top_k,
        log.request_min_p,
        log.request_presence_penalty,
        log.request_repetition_penalty,
        log.request_enable_thinking,
        log.sampling_preset,
        log.request_grammar,
        log.streaming,
        log.tool_rewrite_applied,
    )
    .execute(pool)
    .await?;
    Ok(id)
}

/// Update accumulated streaming state on a `running` row. Guards on
/// `status='running'` so a finalize that races ahead can't be clobbered.
pub async fn update_log_progress(
    pool: &PgPool,
    id: Uuid,
    response_content: Option<&str>,
    response_reasoning_content: Option<&str>,
    completion_tokens: Option<i32>,
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE llm_logs
        SET response_content = $2,
            response_reasoning_content = $3,
            completion_tokens = COALESCE($4, completion_tokens),
            updated_at = NOW()
        WHERE id = $1 AND status = 'running'
        "#,
        id,
        response_content,
        response_reasoning_content,
        completion_tokens,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn finalize_log_success(
    pool: &PgPool,
    id: Uuid,
    response_content: Option<&str>,
    response_reasoning_content: Option<&str>,
    response_tool_calls: Option<&Value>,
    finish_reason: Option<&str>,
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
    duration_ms: i64,
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE llm_logs
        SET status = 'complete',
            response_content = $2,
            response_reasoning_content = $3,
            response_tool_calls = $4,
            finish_reason = $5,
            prompt_tokens = $6,
            completion_tokens = $7,
            duration_ms = $8,
            updated_at = NOW()
        WHERE id = $1
        "#,
        id,
        response_content,
        response_reasoning_content,
        response_tool_calls,
        finish_reason,
        prompt_tokens,
        completion_tokens,
        duration_ms,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn finalize_log_error(
    pool: &PgPool,
    id: Uuid,
    error_message: &str,
    duration_ms: i64,
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE llm_logs
        SET status = 'error',
            error_message = $2,
            duration_ms = $3,
            updated_at = NOW()
        WHERE id = $1 AND status != 'complete'
        "#,
        id,
        error_message,
        duration_ms,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Sweep any `running` rows left over from a previous koblem process —
/// called once at startup. If the previous process crashed mid-request the
/// row would otherwise tick "running" forever in the UI.
pub async fn cancel_stale_running(pool: &PgPool) -> Result<u64> {
    let result = sqlx::query!(
        r#"
        UPDATE llm_logs
        SET status = 'cancelled', updated_at = NOW()
        WHERE status = 'running'
        "#
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Mark a `running` row as `cancelled` — used by the streaming helper's drop
/// guard when the caller's future is dropped before completion (e.g. HTTP
/// client disconnect mid-request).
pub async fn finalize_log_cancelled(pool: &PgPool, id: Uuid, duration_ms: i64) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE llm_logs
        SET status = 'cancelled',
            duration_ms = $2,
            updated_at = NOW()
        WHERE id = $1 AND status = 'running'
        "#,
        id,
        duration_ms,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_logs(
    pool: &PgPool,
    limit: i64,
    offset: i64,
    caller: Option<&str>,
) -> Result<Vec<LogSummary>> {
    let rows = if let Some(caller) = caller {
        sqlx::query_as!(
            LogSummary,
            r#"
            SELECT id, caller, LEFT(response_content, 120) AS message_preview,
                   streaming, duration_ms, status, completion_tokens,
                   created_at, updated_at
            FROM llm_logs
            WHERE caller = $1
            ORDER BY (status = 'running') DESC, created_at DESC
            LIMIT $2 OFFSET $3
            "#,
            caller,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as!(
            LogSummary,
            r#"
            SELECT id, caller, LEFT(response_content, 120) AS message_preview,
                   streaming, duration_ms, status, completion_tokens,
                   created_at, updated_at
            FROM llm_logs
            ORDER BY (status = 'running') DESC, created_at DESC
            LIMIT $1 OFFSET $2
            "#,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await?
    };
    Ok(rows)
}

pub async fn get_log(pool: &PgPool, id: Uuid) -> Result<Option<LogDetail>> {
    let row = sqlx::query_as!(
        LogDetail,
        r#"
        SELECT id, caller, request_messages, request_tools, request_temperature,
               request_max_tokens, request_top_p, request_top_k, request_min_p,
               request_presence_penalty, request_repetition_penalty,
               request_enable_thinking, sampling_preset, request_grammar,
               streaming, response_content, response_reasoning_content,
               response_tool_calls, finish_reason, tool_rewrite_applied,
               duration_ms, prompt_tokens, completion_tokens,
               status, error_message, created_at, updated_at
        FROM llm_logs WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}
