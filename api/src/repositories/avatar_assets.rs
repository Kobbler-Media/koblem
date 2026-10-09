//! Saved-character library — `avatar_assets` table.
//!
//! Each row is a reference portrait the kid uploaded, keyed by uuid. The
//! actual file lives under `${AVATARS_DIR}/_assets/<id>.<ext>`. Routes layer
//! reads/writes through here; retention is unrelated to gpu_logs (these are
//! user-owned assets, not generated samples).
//!
//! Uses runtime sqlx queries (no `query!` macros) so the sqlx-offline cache
//! doesn't need regenerating on every migration touch — same convention as
//! `repositories/settings.rs`.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

use super::Result;

/// Avatar asset row — one saved character portrait.
#[derive(Debug, Clone, Serialize)]
pub struct AvatarAssetRow {
    pub id: String,
    pub original_filename: String,
    pub path: String,
    pub content_type: String,
    pub size_bytes: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Input for inserting a new asset. `path` is relative to AVATARS_DIR.
pub struct NewAvatarAsset<'a> {
    pub id: &'a str,
    pub original_filename: &'a str,
    pub path: &'a str,
    pub content_type: &'a str,
    pub size_bytes: i64,
}

/// Insert a new asset row.
pub async fn create(pool: &PgPool, asset: &NewAvatarAsset<'_>) -> Result<AvatarAssetRow> {
    let row: (String, String, String, String, i64, OffsetDateTime) = sqlx::query_as(
        r#"
        INSERT INTO avatar_assets (id, original_filename, path, content_type, size_bytes)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, original_filename, path, content_type, size_bytes, created_at
        "#,
    )
    .bind(asset.id)
    .bind(asset.original_filename)
    .bind(asset.path)
    .bind(asset.content_type)
    .bind(asset.size_bytes)
    .fetch_one(pool)
    .await?;
    Ok(AvatarAssetRow {
        id: row.0,
        original_filename: row.1,
        path: row.2,
        content_type: row.3,
        size_bytes: row.4,
        created_at: row.5,
    })
}

/// List all assets, newest first.
pub async fn list(pool: &PgPool) -> Result<Vec<AvatarAssetRow>> {
    let rows: Vec<(String, String, String, String, i64, OffsetDateTime)> = sqlx::query_as(
        r#"
        SELECT id, original_filename, path, content_type, size_bytes, created_at
        FROM avatar_assets
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AvatarAssetRow {
            id: r.0,
            original_filename: r.1,
            path: r.2,
            content_type: r.3,
            size_bytes: r.4,
            created_at: r.5,
        })
        .collect())
}

/// Fetch one asset by id.
pub async fn get_by_id(pool: &PgPool, id: &str) -> Result<Option<AvatarAssetRow>> {
    let row: Option<(String, String, String, String, i64, OffsetDateTime)> = sqlx::query_as(
        r#"
        SELECT id, original_filename, path, content_type, size_bytes, created_at
        FROM avatar_assets
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AvatarAssetRow {
        id: r.0,
        original_filename: r.1,
        path: r.2,
        content_type: r.3,
        size_bytes: r.4,
        created_at: r.5,
    }))
}

/// Delete one asset by id. Returns the row (so the caller can unlink the
/// file on disk). `Ok(None)` means the row didn't exist.
pub async fn delete(pool: &PgPool, id: &str) -> Result<Option<AvatarAssetRow>> {
    let row: Option<(String, String, String, String, i64, OffsetDateTime)> = sqlx::query_as(
        r#"
        DELETE FROM avatar_assets
        WHERE id = $1
        RETURNING id, original_filename, path, content_type, size_bytes, created_at
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AvatarAssetRow {
        id: r.0,
        original_filename: r.1,
        path: r.2,
        content_type: r.3,
        size_bytes: r.4,
        created_at: r.5,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::repositories::test_pool as shared_pool;

    async fn test_pool() -> Option<(PgPool, tokio::sync::MutexGuard<'static, ()>)> {
        shared_pool("avatar_assets").await
    }

    #[tokio::test]
    async fn create_list_get_delete_roundtrip() {
        let Some((pool, _guard)) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let id_a = uuid::Uuid::now_v7().to_string();
        let id_b = uuid::Uuid::now_v7().to_string();
        let path_a = format!("_assets/{id_a}.png");
        let path_b = format!("_assets/{id_b}.jpg");

        // INSERT
        let a = create(
            &pool,
            &NewAvatarAsset {
                id: &id_a,
                original_filename: "alice.png",
                path: &path_a,
                content_type: "image/png",
                size_bytes: 1024,
            },
        )
        .await
        .unwrap();
        assert_eq!(a.id, id_a);
        assert_eq!(a.original_filename, "alice.png");
        assert_eq!(a.path, path_a);
        assert_eq!(a.content_type, "image/png");
        assert_eq!(a.size_bytes, 1024);

        let _b = create(
            &pool,
            &NewAvatarAsset {
                id: &id_b,
                original_filename: "bob.jpg",
                path: &path_b,
                content_type: "image/jpeg",
                size_bytes: 2048,
            },
        )
        .await
        .unwrap();

        // LIST — should have both, newest (b) first.
        let all = list(&pool).await.unwrap();
        assert_eq!(all.len(), 2);
        // Most recently inserted comes first.
        assert_eq!(all[0].id, id_b);
        assert_eq!(all[1].id, id_a);

        // GET by id
        let got = get_by_id(&pool, &id_a).await.unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().original_filename, "alice.png");

        let missing = get_by_id(&pool, "no-such-id").await.unwrap();
        assert!(missing.is_none());

        // DELETE
        let removed = delete(&pool, &id_a).await.unwrap();
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().path, path_a);

        let remaining = list(&pool).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, id_b);

        // Deleting a missing row is a no-op
        let again = delete(&pool, &id_a).await.unwrap();
        assert!(again.is_none());
    }
}
