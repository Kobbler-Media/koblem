use super::Result;
use sqlx::{FromRow, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// User credentials for login validation.
pub struct UserCredentials {
    pub id: Uuid,
    pub password_hash: String,
}

/// Get user credentials by email for login validation.
pub async fn get_user_by_email(pool: &PgPool, email: &str) -> Result<Option<UserCredentials>> {
    let row = sqlx::query("SELECT id, password_hash FROM users WHERE email = $1")
        .bind(email)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| UserCredentials {
        id: r.get("id"),
        password_hash: r.get("password_hash"),
    }))
}

/// Create a new user.
pub async fn create_user(pool: &PgPool, email: &str, password_hash: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(email)
        .bind(password_hash)
        .execute(pool)
        .await?;
    Ok(id)
}

/// Get the total count of users.
pub async fn user_count(pool: &PgPool) -> Result<i64> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// Create an auth token for a user session.
/// Token hash stored; NULL expires_at = never expires.
pub async fn create_auth_token(pool: &PgPool, user_id: Uuid, token: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let token_hash = crate::auth::hash_api_key(token);
    sqlx::query(
        "INSERT INTO auth_tokens (id, user_id, token_hash, expires_at) VALUES ($1, $2, $3, NULL)",
    )
    .bind(id)
    .bind(user_id)
    .bind(token_hash)
    .execute(pool)
    .await?;
    Ok(id)
}

/// The user behind a live login session, if the token names one.
///
/// Replaces an `EXISTS` probe: the queue needs to know WHO enqueued a job, and a
/// boolean cannot say. `None` covers unknown, revoked and expired alike — the
/// caller 401s on all three identically, and telling them apart would be an
/// account-enumeration oracle.
pub async fn session_user(pool: &PgPool, token: &str) -> Result<Option<(Uuid, String)>> {
    let token_hash = crate::auth::hash_api_key(token);
    let row: Option<(Uuid, String)> = sqlx::query_as(
        r#"SELECT u.id, u.email
           FROM auth_tokens t
           JOIN users u ON u.id = t.user_id
           WHERE t.token_hash = $1
             AND t.revoked = FALSE
             AND (t.expires_at IS NULL OR t.expires_at > NOW())"#,
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// API key summary for listing.
#[derive(FromRow)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub name: String,
    pub key: Option<String>,
    pub last_used_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub revoked: bool,
}

/// List all API keys.
pub async fn list_api_keys(pool: &PgPool) -> Result<Vec<ApiKeyRow>> {
    let rows: Vec<ApiKeyRow> = sqlx::query_as(
        r#"SELECT id, name, key, last_used_at, created_at,
               (revoked_at IS NOT NULL) as "revoked"
        FROM api_keys
        ORDER BY created_at DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Create a new API key.
pub async fn create_api_key(pool: &PgPool, name: &str, key_hash: &str, key: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, key, created_at) VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind(id)
    .bind(name)
    .bind(key_hash)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Revoke an API key.
pub async fn revoke_api_key(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result =
        sqlx::query("UPDATE api_keys SET revoked_at = NOW() WHERE id = $1 AND revoked_at IS NULL")
            .bind(id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// The name of a live named API key, by hash.
///
/// `api_keys` has no `user_id` — a key principal can only ever carry its own id
/// and name, which is why the principal is a sum type rather than a user id.
pub async fn api_key_name_by_hash(pool: &PgPool, key_hash: &str) -> Result<Option<(Uuid, String)>> {
    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL")
            .bind(key_hash)
            .fetch_optional(pool)
            .await?;
    Ok(row)
}

/// Update API key last_used_at timestamp.
pub async fn touch_api_key(pool: &PgPool, key_hash: &str) -> Result<()> {
    sqlx::query("UPDATE api_keys SET last_used_at = NOW() WHERE key_hash = $1")
        .bind(key_hash)
        .execute(pool)
        .await?;
    Ok(())
}
