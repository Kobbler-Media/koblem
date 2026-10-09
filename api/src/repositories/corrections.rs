use super::Result;
use serde::Serialize;
use sqlx::{FromRow, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Serialize, FromRow)]
pub struct GenericRule {
    pub id: Uuid,
    pub scope: String,
    pub rule: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Serialize, FromRow)]
pub struct EntityRule {
    pub id: Uuid,
    pub entity_id: String,
    pub entity_type: String,
    pub entity_name: Option<String>,
    pub rule: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Serialize, FromRow)]
pub struct DecisionCorrection {
    pub id: Uuid,
    pub scope: String,
    pub release_name: String,
    pub entity_id: Option<String>,
    pub model_decision: String,
    pub correct_decision: String,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Unified correction entry for listing all corrections.
#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct CorrectionEntry {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub correction_type: String,
    pub scope: Option<String>,
    pub entity_id: Option<String>,
    pub entity_type: Option<String>,
    pub entity_name: Option<String>,
    pub rule: Option<String>,
    pub release_name: Option<String>,
    pub model_decision: Option<String>,
    pub correct_decision: Option<String>,
    pub reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

pub async fn create_generic_rule(pool: &PgPool, scope: &str, rule: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO generic_rules (id, scope, rule) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(scope)
        .bind(rule)
        .execute(pool)
        .await?;
    Ok(id)
}

pub async fn create_entity_rule(
    pool: &PgPool,
    entity_id: &str,
    entity_type: &str,
    entity_name: Option<&str>,
    rule: &str,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entity_rules (id, entity_id, entity_type, entity_name, rule) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(entity_id)
    .bind(entity_type)
    .bind(entity_name)
    .bind(rule)
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn create_decision_correction(
    pool: &PgPool,
    scope: &str,
    release_name: &str,
    entity_id: Option<&str>,
    model_decision: &str,
    correct_decision: &str,
    reason: &str,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO decision_corrections (id, scope, release_name, entity_id, model_decision, correct_decision, reason) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(scope)
    .bind(release_name)
    .bind(entity_id)
    .bind(model_decision)
    .bind(correct_decision)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn list_generic_rules(pool: &PgPool, scope: Option<&str>) -> Result<Vec<GenericRule>> {
    if let Some(scope) = scope {
        sqlx::query_as(
            "SELECT id, scope, rule, created_at FROM generic_rules WHERE scope = $1 ORDER BY created_at DESC",
        )
        .bind(scope)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as(
            "SELECT id, scope, rule, created_at FROM generic_rules ORDER BY created_at DESC",
        )
        .fetch_all(pool)
        .await
    }
}

pub async fn list_entity_rules(pool: &PgPool, entity_id: Option<&str>) -> Result<Vec<EntityRule>> {
    if let Some(entity_id) = entity_id {
        sqlx::query_as(
            "SELECT id, entity_id, entity_type, entity_name, rule, created_at \
             FROM entity_rules WHERE entity_id = $1 ORDER BY created_at DESC",
        )
        .bind(entity_id)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as(
            "SELECT id, entity_id, entity_type, entity_name, rule, created_at \
             FROM entity_rules ORDER BY created_at DESC",
        )
        .fetch_all(pool)
        .await
    }
}

pub async fn list_recent_corrections(
    pool: &PgPool,
    entity_id: Option<&str>,
    limit: i64,
) -> Result<Vec<DecisionCorrection>> {
    if let Some(entity_id) = entity_id {
        sqlx::query_as(
            "SELECT id, scope, release_name, entity_id, model_decision, correct_decision, reason, created_at \
             FROM decision_corrections WHERE entity_id = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(entity_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as(
            "SELECT id, scope, release_name, entity_id, model_decision, correct_decision, reason, created_at \
             FROM decision_corrections ORDER BY created_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
    }
}

#[allow(dead_code)]
pub async fn list_all_corrections(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<Vec<CorrectionEntry>> {
    let rows = sqlx::query(
        r#"
        SELECT id, 'generic_rule' as correction_type,
               scope, NULL as entity_id, NULL as entity_type,
               NULL as entity_name, rule,
               NULL as release_name, NULL as model_decision,
               NULL as correct_decision, NULL as reason, created_at
        FROM generic_rules
        UNION ALL
        SELECT id, 'entity_rule' as correction_type,
               NULL as scope, entity_id, entity_type,
               entity_name, rule,
               NULL as release_name, NULL as model_decision,
               NULL as correct_decision, NULL as reason, created_at
        FROM entity_rules
        UNION ALL
        SELECT id, 'decision_correction' as correction_type,
               scope, entity_id, NULL as entity_type,
               NULL as entity_name, NULL as rule,
               release_name, model_decision,
               correct_decision, reason, created_at
        FROM decision_corrections
        ORDER BY created_at DESC
        LIMIT $1 OFFSET $2
        "#,
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| CorrectionEntry {
            id: r.get("id"),
            correction_type: r.get("correction_type"),
            scope: r.get("scope"),
            entity_id: r.get("entity_id"),
            entity_type: r.get("entity_type"),
            entity_name: r.get("entity_name"),
            rule: r.get("rule"),
            release_name: r.get("release_name"),
            model_decision: r.get("model_decision"),
            correct_decision: r.get("correct_decision"),
            reason: r.get("reason"),
            created_at: r.get("created_at"),
        })
        .collect())
}

pub async fn delete_generic_rule(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM generic_rules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_entity_rule(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM entity_rules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_decision_correction(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM decision_corrections WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
