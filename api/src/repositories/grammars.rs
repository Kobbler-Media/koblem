use super::Result;
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

#[derive(Debug, Serialize)]
pub struct NamedGrammar {
    pub name: String,
    pub gbnf: String,
    pub description: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

pub async fn list_grammars(pool: &PgPool) -> Result<Vec<NamedGrammar>> {
    sqlx::query_as!(
        NamedGrammar,
        "SELECT name, gbnf, description, created_at, updated_at \
         FROM grammars ORDER BY name"
    )
    .fetch_all(pool)
    .await
}

pub async fn get_grammar(pool: &PgPool, name: &str) -> Result<Option<NamedGrammar>> {
    sqlx::query_as!(
        NamedGrammar,
        "SELECT name, gbnf, description, created_at, updated_at \
         FROM grammars WHERE name = $1",
        name
    )
    .fetch_optional(pool)
    .await
}

pub async fn upsert_grammar(
    pool: &PgPool,
    name: &str,
    gbnf: &str,
    description: Option<&str>,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO grammars (name, gbnf, description, updated_at) \
         VALUES ($1, $2, $3, NOW()) \
         ON CONFLICT (name) DO UPDATE \
         SET gbnf = $2, description = $3, updated_at = NOW()",
        name,
        gbnf,
        description
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_grammar(pool: &PgPool, name: &str) -> Result<bool> {
    let result = sqlx::query!("DELETE FROM grammars WHERE name = $1", name)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
