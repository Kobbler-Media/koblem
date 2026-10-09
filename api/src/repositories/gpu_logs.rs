//! GPU logs + sample retention repository.

use serde::Serialize;
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;

use super::Result;

/// Input for creating a new GPU log entry. Matches the gpu_logs table.
pub struct CreateLog {
    pub service: String,
    pub endpoint: String,
    pub request_meta: Value,
    pub response_meta: Value,
    pub sample_id: Option<i64>,
    pub gate_wait_ms: i64,
    pub duration_ms: i64,
    pub status: i32,
    pub caller: Option<String>,
}

/// Input for saving a sample file on disk.
pub struct CreateSample {
    pub kind: String,
    pub mime: String,
    pub path: String,
    pub size_bytes: i64,
}

/// Full log row for the UI and replay.
#[derive(Serialize, FromRow)]
pub struct GpuLogRow {
    pub id: i64,
    pub service: String,
    pub endpoint: String,
    pub request_meta: Value,
    pub response_meta: Value,
    pub sample_id: Option<i64>,
    pub gate_wait_ms: i64,
    pub duration_ms: i64,
    pub status: i32,
    pub caller: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Sample row.
#[derive(Serialize, FromRow)]
pub struct GpuSampleRow {
    pub id: i64,
    pub kind: String,
    pub mime: String,
    pub path: String,
    pub size_bytes: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Insert a sample row and return its id.
pub async fn create_sample(pool: &PgPool, sample: &CreateSample) -> Result<i64> {
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO gpu_samples (kind, mime, path, size_bytes)
        VALUES ($1, $2, $3, $4)
        RETURNING id
        "#,
        sample.kind,
        sample.mime,
        sample.path,
        sample.size_bytes,
    )
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Insert a log row and return its id. Fire-and-forget from the handler.
pub async fn create_log(pool: &PgPool, log: &CreateLog) -> Result<i64> {
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO gpu_logs (
            service, endpoint, request_meta, response_meta,
            sample_id, gate_wait_ms, duration_ms, status, caller
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id
        "#,
        log.service,
        log.endpoint,
        log.request_meta,
        log.response_meta,
        log.sample_id,
        log.gate_wait_ms,
        log.duration_ms,
        log.status,
        log.caller,
    )
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Paginated listing, newest first. Optional service filter.
pub async fn list(
    pool: &PgPool,
    service: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<GpuLogRow>> {
    let rows = if let Some(svc) = service {
        sqlx::query_as!(
            GpuLogRow,
            r#"
            SELECT id, service, endpoint, request_meta, response_meta,
                   sample_id, gate_wait_ms, duration_ms, status, caller, created_at
            FROM gpu_logs
            WHERE service = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
            svc,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as!(
            GpuLogRow,
            r#"
            SELECT id, service, endpoint, request_meta, response_meta,
                   sample_id, gate_wait_ms, duration_ms, status, caller, created_at
            FROM gpu_logs
            ORDER BY created_at DESC
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

pub async fn count(pool: &PgPool, service: Option<&str>) -> Result<i64> {
    let n = if let Some(svc) = service {
        sqlx::query_scalar!(r#"SELECT COUNT(*) FROM gpu_logs WHERE service = $1"#, svc)
            .fetch_one(pool)
            .await?
    } else {
        sqlx::query_scalar!(r#"SELECT COUNT(*) FROM gpu_logs"#)
            .fetch_one(pool)
            .await?
    };
    Ok(n.unwrap_or(0))
}

pub async fn get_log(pool: &PgPool, id: i64) -> Result<Option<GpuLogRow>> {
    let row = sqlx::query_as!(
        GpuLogRow,
        r#"
        SELECT id, service, endpoint, request_meta, response_meta,
               sample_id, gate_wait_ms, duration_ms, status, caller, created_at
        FROM gpu_logs
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_sample(pool: &PgPool, id: i64) -> Result<Option<GpuSampleRow>> {
    let row = sqlx::query_as!(
        GpuSampleRow,
        r#"
        SELECT id, kind, mime, path, size_bytes, created_at
        FROM gpu_samples
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Delete one log (and associated sample row+file if present).
/// Returns the sample path if the caller needs to unlink from disk.
pub async fn delete_log(pool: &PgPool, id: i64) -> Result<Option<String>> {
    let row = sqlx::query!(
        r#"
        DELETE FROM gpu_logs
        WHERE id = $1
        RETURNING sample_id
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else { return Ok(None) };
    let Some(sample_id) = row.sample_id else {
        return Ok(None);
    };

    let path = sqlx::query_scalar!(
        r#"DELETE FROM gpu_samples WHERE id = $1 RETURNING path"#,
        sample_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(path)
}

/// Delete all logs and samples. Returns all sample paths to unlink.
pub async fn delete_all(pool: &PgPool) -> Result<Vec<String>> {
    sqlx::query!(r#"DELETE FROM gpu_logs"#)
        .execute(pool)
        .await?;
    let paths = sqlx::query_scalar!(r#"DELETE FROM gpu_samples RETURNING path"#)
        .fetch_all(pool)
        .await?;
    Ok(paths)
}

/// Summary stats for the retention UI.
pub struct Stats {
    pub total_entries: i64,
    pub total_samples: i64,
    pub total_sample_bytes: i64,
    pub oldest_created_at: Option<OffsetDateTime>,
    pub newest_created_at: Option<OffsetDateTime>,
}

pub async fn stats(pool: &PgPool) -> Result<Stats> {
    let row = sqlx::query!(
        r#"
        SELECT
            (SELECT COUNT(*) FROM gpu_logs) AS "total_entries!: i64",
            (SELECT COUNT(*) FROM gpu_samples) AS "total_samples!: i64",
            (SELECT COALESCE(SUM(size_bytes), 0) FROM gpu_samples) AS "total_sample_bytes!: i64",
            (SELECT MIN(created_at) FROM gpu_logs) AS oldest_created_at,
            (SELECT MAX(created_at) FROM gpu_logs) AS newest_created_at
        "#,
    )
    .fetch_one(pool)
    .await?;

    Ok(Stats {
        total_entries: row.total_entries,
        total_samples: row.total_samples,
        total_sample_bytes: row.total_sample_bytes,
        oldest_created_at: row.oldest_created_at,
        newest_created_at: row.newest_created_at,
    })
}

/// Sweeper helpers — called from the retention background task.
pub async fn prune_older_than(pool: &PgPool, cutoff: OffsetDateTime) -> Result<Vec<String>> {
    let sample_paths = sqlx::query_scalar!(
        r#"
        DELETE FROM gpu_samples
        WHERE id IN (
            SELECT sample_id FROM gpu_logs
            WHERE created_at < $1 AND sample_id IS NOT NULL
        )
        RETURNING path
        "#,
        cutoff,
    )
    .fetch_all(pool)
    .await?;

    sqlx::query!(r#"DELETE FROM gpu_logs WHERE created_at < $1"#, cutoff,)
        .execute(pool)
        .await?;

    Ok(sample_paths)
}

/// Delete oldest logs until the total count is at/below max_entries.
/// Returns the set of sample paths to unlink.
pub async fn prune_to_max_entries(pool: &PgPool, max_entries: i64) -> Result<Vec<String>> {
    let sample_paths = sqlx::query_scalar!(
        r#"
        WITH victims AS (
            SELECT id, sample_id FROM gpu_logs
            ORDER BY created_at DESC
            OFFSET $1
        ),
        log_delete AS (
            DELETE FROM gpu_logs WHERE id IN (SELECT id FROM victims)
        )
        DELETE FROM gpu_samples
        WHERE id IN (SELECT sample_id FROM victims WHERE sample_id IS NOT NULL)
        RETURNING path
        "#,
        max_entries,
    )
    .fetch_all(pool)
    .await?;

    Ok(sample_paths)
}

/// Count samples on disk. Used by tests and the retention sweeper.
#[allow(dead_code)]
pub async fn count_samples(pool: &PgPool) -> Result<i64> {
    let n = sqlx::query_scalar!(r#"SELECT COUNT(*) FROM gpu_samples"#)
        .fetch_one(pool)
        .await?;
    Ok(n.unwrap_or(0))
}

/// Delete oldest samples (and their associated logs) until total size is
/// at/below the byte budget. Returns paths to unlink.
pub async fn prune_to_max_bytes(pool: &PgPool, max_bytes: i64) -> Result<Vec<String>> {
    let mut running: i64 = 0;
    let mut victim_ids: Vec<i64> = Vec::new();
    let rows = sqlx::query!(
        r#"
        SELECT id, size_bytes FROM gpu_samples
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        if running + row.size_bytes <= max_bytes {
            running += row.size_bytes;
        } else {
            victim_ids.push(row.id);
        }
    }
    if victim_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Clear FK references before dropping samples
    sqlx::query!(
        r#"UPDATE gpu_logs SET sample_id = NULL WHERE sample_id = ANY($1)"#,
        &victim_ids,
    )
    .execute(pool)
    .await?;

    let paths = sqlx::query_scalar!(
        r#"DELETE FROM gpu_samples WHERE id = ANY($1) RETURNING path"#,
        &victim_ids,
    )
    .fetch_all(pool)
    .await?;

    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::repositories::test_pool as shared_pool;

    async fn test_pool() -> Option<(PgPool, tokio::sync::MutexGuard<'static, ()>)> {
        shared_pool("gpu_logs, gpu_samples").await
    }

    async fn seed_log(pool: &PgPool, sample_id: Option<i64>) -> i64 {
        create_log(
            pool,
            &CreateLog {
                service: "tts".to_string(),
                endpoint: "/api/v1/tts/generate".to_string(),
                request_meta: json!({"test": true}),
                response_meta: json!({"byte_size": 0}),
                sample_id,
                gate_wait_ms: 0,
                duration_ms: 0,
                status: 200,
                caller: None,
            },
        )
        .await
        .unwrap()
    }

    async fn seed_sample(pool: &PgPool, size_bytes: i64) -> i64 {
        create_sample(
            pool,
            &CreateSample {
                kind: "audio_out".to_string(),
                mime: "audio/wav".to_string(),
                path: format!("test-{}.wav", uuid::Uuid::new_v4()),
                size_bytes,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn prune_max_entries_keeps_newest() {
        let Some((pool, _guard)) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        for _ in 0..50 {
            seed_log(&pool, None).await;
        }
        let deleted_paths = prune_to_max_entries(&pool, 30).await.unwrap();
        assert!(deleted_paths.is_empty(), "no samples, no paths to delete");
        let remaining = count(&pool, None).await.unwrap();
        assert_eq!(remaining, 30, "should have kept newest 30 of 50");
    }

    #[tokio::test]
    async fn prune_max_bytes_cascades_to_logs() {
        let Some((pool, _guard)) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        // 10 samples @ 100 MB each = 1 GB total.
        let mut sample_ids = Vec::new();
        for _ in 0..10 {
            sample_ids.push(seed_sample(&pool, 100 * 1024 * 1024).await);
        }
        // Link each to a log
        for sid in &sample_ids {
            seed_log(&pool, Some(*sid)).await;
        }
        // Budget = 300 MB → should keep 3 newest, delete 7
        let paths = prune_to_max_bytes(&pool, 300 * 1024 * 1024).await.unwrap();
        assert_eq!(paths.len(), 7, "7 samples should have been evicted");
        let remaining_samples = count_samples(&pool).await.unwrap();
        assert_eq!(remaining_samples, 3);
        // All logs should survive — their sample_id was nulled, not deleted.
        let remaining_logs = count(&pool, None).await.unwrap();
        assert_eq!(remaining_logs, 10);
    }

    #[tokio::test]
    async fn prune_older_than_cleans_samples_too() {
        let Some((pool, _guard)) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let sid = seed_sample(&pool, 1024).await;
        seed_log(&pool, Some(sid)).await;
        // Age the log — set created_at into the past
        sqlx::query!("UPDATE gpu_logs SET created_at = NOW() - INTERVAL '30 days'")
            .execute(&pool)
            .await
            .unwrap();

        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(7);
        let paths = prune_older_than(&pool, cutoff).await.unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(count(&pool, None).await.unwrap(), 0);
        assert_eq!(count_samples(&pool).await.unwrap(), 0);
    }
}
