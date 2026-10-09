//! Content-addressed Director asset metadata — `director_assets`.
//!
//! Bytes live on disk under the assets dir, named by their SHA-256; this table carries the
//! content type, the size and the GC timestamps. Writes are idempotent by construction: the
//! hash IS the identity, so re-uploading the same still is a no-op that returns the same ref.
//!
//! Runtime sqlx queries (no `query!` macros), matching `director_projects.rs` /
//! `director_shot_renders.rs` — a migration touch doesn't force an offline-cache regen.

use sqlx::PgPool;

use super::Result;

/// Record an asset, or touch the existing row when these exact bytes are already stored.
/// Returns `true` when the row was newly inserted (the caller then needs to write the file).
pub async fn upsert(pool: &PgPool, hash: &str, content_type: &str, bytes: i64) -> Result<bool> {
    let row: (bool,) = sqlx::query_as(
        r#"
        INSERT INTO director_assets (hash, content_type, bytes)
        VALUES ($1, $2, $3)
        ON CONFLICT (hash)
        DO UPDATE SET last_used_at = NOW()
        RETURNING (xmax = 0) AS inserted
        "#,
    )
    .bind(hash)
    .bind(content_type)
    .bind(bytes)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// An asset's content type, bumping `last_used_at` so the sweep treats it as live.
pub async fn touch_content_type(pool: &PgPool, hash: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        r#"
        UPDATE director_assets SET last_used_at = NOW()
        WHERE hash = $1
        RETURNING content_type
        "#,
    )
    .bind(hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Hashes untouched for `stale_days` that nothing still references.
///
/// The reference test is a substring match against each holder's JSON. That is exact for this
/// data — a hash only ever appears as part of an `asset:<hash>` ref, and a 64-hex-char string has
/// no other reason to occur — and it keeps the sweep to one query regardless of how the refs are
/// nested inside the document.
///
/// Three holders now: saved Director projects, agent-chat attachments, and the queue's own
/// non-terminal rows. The second is load-bearing — an agent conversation stores only the ref, so
/// sweeping its attachment would leave the turn unreplayable and its image 404ing in the
/// transcript.
///
/// 🔴 The third is what makes the store safe to enqueue against at all. Every capture path writes
/// `asset:<hash>` into `gpu_jobs.payload`, and a job queued or held longer than `stale_days` would
/// otherwise have its inputs swept out from under it and fail at dispatch, hours later, with a
/// missing-ref error. Only a NON-TERMINAL row holds: a finished render's refs are history, and
/// counting them would pin every asset for the fortnight the queue keeps its rows.
pub async fn stale_unreferenced(pool: &PgPool, stale_days: i64) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT a.hash
        FROM director_assets a
        WHERE a.last_used_at < NOW() - ($1 || ' days')::interval
          AND NOT EXISTS (
              SELECT 1 FROM director_projects p
              WHERE p.timeline::text LIKE '%' || a.hash || '%'
          )
          AND NOT EXISTS (
              SELECT 1 FROM agent_messages m
              WHERE m.attachments::text LIKE '%' || a.hash || '%'
          )
          AND NOT EXISTS (
              SELECT 1 FROM gpu_jobs j
              WHERE j.state NOT IN ('done', 'failed', 'cancelled')
                AND j.payload::text LIKE '%' || a.hash || '%'
          )
        "#,
    )
    .bind(stale_days.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

pub async fn delete(pool: &PgPool, hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM director_assets WHERE hash = $1")
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    async fn seed_stale(pool: &PgPool, hash: &str) {
        upsert(pool, hash, "image/png", 1).await.unwrap();
        sqlx::query(
            "UPDATE director_assets SET last_used_at = NOW() - INTERVAL '30 days' WHERE hash = $1",
        )
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_job(pool: &PgPool, state: &str, hash: &str) {
        sqlx::query(
            "INSERT INTO gpu_jobs (id, kind, payload, state, queue_rank, created_by) \
             VALUES ($1, 'avatar', $2, $3, 1, 'test')",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(json!({ "image": format!("asset:{hash}"), "return": "bytes" }))
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A render enqueued on Friday and still queued a week later must not have its portrait swept
    /// out from under it — it would fail at dispatch, hours later, with a missing-ref error.
    #[tokio::test]
    async fn the_sweep_spares_a_live_queue_row_s_captured_inputs() {
        let Some((pool, _guard)) =
            crate::repositories::test_pool("director_assets, gpu_jobs").await
        else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let queued = "a".repeat(64);
        let held = "b".repeat(64);
        let finished = "c".repeat(64);
        let orphan = "d".repeat(64);
        for hash in [&queued, &held, &finished, &orphan] {
            seed_stale(&pool, hash).await;
        }
        seed_job(&pool, "queued", &queued).await;
        seed_job(&pool, "held", &held).await;
        seed_job(&pool, "done", &finished).await;

        let stale = stale_unreferenced(&pool, 7).await.unwrap();
        assert!(
            !stale.contains(&queued),
            "a queued row still needs its input"
        );
        assert!(!stale.contains(&held), "a held row still needs its input");
        assert!(
            stale.contains(&finished),
            "a finished render's refs are history, not a hold"
        );
        assert!(stale.contains(&orphan), "nothing references this one");
    }
}
