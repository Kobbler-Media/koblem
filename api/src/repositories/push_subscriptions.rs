//! `push_subscriptions` — the browser endpoints a queue notification is fanned out to.

use sqlx::PgPool;

use super::Result;

/// Only the material a send needs. `created_by`, `failures` and the timestamps are written for
/// operator forensics and read with psql, not carried through a fan-out that runs per notification.
#[derive(Debug, Clone)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// Idempotent on `endpoint`: a browser that re-subscribes hands back the endpoint it already has,
/// and the keys rotate with it.
pub async fn upsert(
    pool: &PgPool,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    created_by: &str,
) -> Result<()> {
    sqlx::query!(
        r#"INSERT INTO push_subscriptions (endpoint, p256dh, auth, created_by)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (endpoint) DO UPDATE
             SET p256dh = $2, auth = $3, created_by = $4, failures = 0"#,
        endpoint,
        p256dh,
        auth,
        created_by
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn all(pool: &PgPool) -> Result<Vec<PushSubscription>> {
    let rows = sqlx::query_as!(
        PushSubscription,
        "SELECT endpoint, p256dh, auth FROM push_subscriptions ORDER BY created_at"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn remove(pool: &PgPool, endpoint: &str) -> Result<bool> {
    let done = sqlx::query!(
        "DELETE FROM push_subscriptions WHERE endpoint = $1",
        endpoint
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

pub async fn mark_delivered(pool: &PgPool, endpoint: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE push_subscriptions SET last_used_at = NOW(), failures = 0 WHERE endpoint = $1",
        endpoint
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A transient send failure. Counted rather than acted on: the row only ever disappears on the
/// push service saying the subscription is gone.
pub async fn mark_failed(pool: &PgPool, endpoint: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE push_subscriptions SET failures = failures + 1 WHERE endpoint = $1",
        endpoint
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn count(pool: &PgPool) -> Result<i64> {
    let row = sqlx::query!(r#"SELECT COUNT(*) as "n!" FROM push_subscriptions"#)
        .fetch_one(pool)
        .await?;
    Ok(row.n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::test_pool;

    struct Row {
        failures: i32,
        used: bool,
    }

    async fn row(pool: &PgPool, endpoint: &str) -> Row {
        let r = sqlx::query!(
            "SELECT failures, last_used_at FROM push_subscriptions WHERE endpoint = $1",
            endpoint
        )
        .fetch_one(pool)
        .await
        .unwrap();
        Row {
            failures: r.failures,
            used: r.last_used_at.is_some(),
        }
    }

    #[tokio::test]
    async fn a_resubscribe_rotates_the_keys_rather_than_adding_a_row() {
        let Some((pool, _guard)) = test_pool("push_subscriptions").await else {
            return;
        };
        upsert(&pool, "https://push.example/a", "old", "auth1", "user:me")
            .await
            .unwrap();
        upsert(&pool, "https://push.example/a", "new", "auth2", "user:me")
            .await
            .unwrap();

        let rows = all(&pool).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].p256dh, "new");
        assert_eq!(rows[0].auth, "auth2");
    }

    #[tokio::test]
    async fn a_transient_failure_counts_and_a_delivery_clears_it() {
        let Some((pool, _guard)) = test_pool("push_subscriptions").await else {
            return;
        };
        upsert(&pool, "https://push.example/b", "k", "a", "key:cli")
            .await
            .unwrap();
        mark_failed(&pool, "https://push.example/b").await.unwrap();
        mark_failed(&pool, "https://push.example/b").await.unwrap();
        assert_eq!(row(&pool, "https://push.example/b").await.failures, 2);

        mark_delivered(&pool, "https://push.example/b")
            .await
            .unwrap();
        let after = row(&pool, "https://push.example/b").await;
        assert_eq!(after.failures, 0);
        assert!(after.used);
    }

    #[tokio::test]
    async fn removing_an_endpoint_twice_reports_the_second_as_a_no_op() {
        let Some((pool, _guard)) = test_pool("push_subscriptions").await else {
            return;
        };
        upsert(&pool, "https://push.example/c", "k", "a", "key:env")
            .await
            .unwrap();
        assert!(remove(&pool, "https://push.example/c").await.unwrap());
        assert!(!remove(&pool, "https://push.example/c").await.unwrap());
        assert_eq!(count(&pool).await.unwrap(), 0);
    }
}
