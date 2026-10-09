use super::Result;
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize)]
pub struct PromptTemplate {
    pub media_kind: String,
    pub slot: String,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct PromptVariable {
    pub media_kind: String,
    pub name: String,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

pub async fn get_template(
    pool: &PgPool,
    media_kind: &str,
    slot: &str,
) -> Result<Option<PromptTemplate>> {
    sqlx::query_as!(
        PromptTemplate,
        "SELECT media_kind, slot, body, updated_at \
         FROM prompt_templates WHERE media_kind = $1 AND slot = $2",
        media_kind,
        slot
    )
    .fetch_optional(pool)
    .await
}

pub async fn upsert_template(
    pool: &PgPool,
    media_kind: &str,
    slot: &str,
    body: &str,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO prompt_templates (media_kind, slot, body, updated_at) \
         VALUES ($1, $2, $3, NOW()) \
         ON CONFLICT (media_kind, slot) DO UPDATE \
         SET body = $3, updated_at = NOW()",
        media_kind,
        slot,
        body
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_template(pool: &PgPool, media_kind: &str, slot: &str) -> Result<bool> {
    let r = sqlx::query!(
        "DELETE FROM prompt_templates WHERE media_kind = $1 AND slot = $2",
        media_kind,
        slot
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn get_variable(
    pool: &PgPool,
    media_kind: &str,
    name: &str,
) -> Result<Option<PromptVariable>> {
    sqlx::query_as!(
        PromptVariable,
        "SELECT media_kind, name, body, updated_at \
         FROM prompt_variables WHERE media_kind = $1 AND name = $2",
        media_kind,
        name
    )
    .fetch_optional(pool)
    .await
}

pub async fn upsert_variable(
    pool: &PgPool,
    media_kind: &str,
    name: &str,
    body: &str,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO prompt_variables (media_kind, name, body, updated_at) \
         VALUES ($1, $2, $3, NOW()) \
         ON CONFLICT (media_kind, name) DO UPDATE \
         SET body = $3, updated_at = NOW()",
        media_kind,
        name,
        body
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_variable(pool: &PgPool, media_kind: &str, name: &str) -> Result<bool> {
    let r = sqlx::query!(
        "DELETE FROM prompt_variables WHERE media_kind = $1 AND name = $2",
        media_kind,
        name
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Resolved variable view used by the prompts UI's single-load path.
///
/// One DB round-trip returns every variable for `media_kind`: the union of
/// seeded names (passed in) plus every user-defined name from the per-kind
/// AND default rows, with each row's per-kind body, default body, and the
/// most-recent updated_at on whichever DB layer "won" (per-kind beats default
/// beats `None` when only the Rust-baked seeded value would apply).
pub struct ResolvedVariable {
    pub name: String,
    pub per_kind_body: Option<String>,
    pub default_body: Option<String>,
    pub updated_at: Option<OffsetDateTime>,
}

/// Per-kind override counts, returned as `(media_kind, template_count, variable_count)`
/// rows. Kinds with zero rows in BOTH tables are omitted; the caller fills
/// those in as zeroes.
pub async fn count_overrides_by_kind(pool: &PgPool) -> Result<Vec<(String, i64, i64)>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT media_kind, \
                SUM(CASE WHEN src = 't' THEN 1 ELSE 0 END)::bigint AS templates, \
                SUM(CASE WHEN src = 'v' THEN 1 ELSE 0 END)::bigint AS variables \
         FROM ( \
            SELECT media_kind, 't' AS src FROM prompt_templates \
            UNION ALL \
            SELECT media_kind, 'v' AS src FROM prompt_variables \
         ) x \
         GROUP BY media_kind",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_resolved_variables(
    pool: &PgPool,
    media_kind: &str,
    seeded_names: &[&str],
) -> Result<Vec<ResolvedVariable>> {
    let seeded: Vec<String> = seeded_names.iter().map(|s| s.to_string()).collect();
    sqlx::query_as!(
        ResolvedVariable,
        r#"
        WITH all_names AS (
            SELECT name FROM prompt_variables WHERE media_kind = $1
            UNION
            SELECT name FROM prompt_variables WHERE media_kind = 'default'
            UNION
            SELECT unnest($2::TEXT[]) AS name
        )
        SELECT
            n.name AS "name!",
            pk.body AS per_kind_body,
            dk.body AS default_body,
            GREATEST(pk.updated_at, dk.updated_at) AS updated_at
        FROM all_names n
        LEFT JOIN prompt_variables pk ON pk.media_kind = $1 AND pk.name = n.name
        LEFT JOIN prompt_variables dk ON dk.media_kind = 'default' AND dk.name = n.name
        ORDER BY n.name
        "#,
        media_kind,
        &seeded
    )
    .fetch_all(pool)
    .await
}
