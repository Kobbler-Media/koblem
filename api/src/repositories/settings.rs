use super::Result;
use serde_json::Value;
use sqlx::PgPool;

/// Get a single setting by key.
pub async fn get_setting(pool: &PgPool, key: &str) -> Result<Option<Value>> {
    let row: Option<(Value,)> = sqlx::query_as("SELECT value FROM settings WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
}

/// Set a single setting (upsert).
pub async fn set_setting(pool: &PgPool, key: &str, value: &Value) -> Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, NOW()) \
         ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = NOW()",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get all settings.
pub async fn get_all_settings(pool: &PgPool) -> Result<Vec<(String, Value)>> {
    let rows: Vec<(String, Value)> = sqlx::query_as("SELECT key, value FROM settings ORDER BY key")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}
