//! `gpu_job_stats` — the measured duration table the estimator reads.
//!
//! 🔴 Deliberately not a query over `gpu_logs`. That table records exactly the
//! right things per request, and it is **pruned by default**
//! (`gpu_log_retention_max_entries` / `max_age_days`), so estimating from it means
//! the estimates silently get worse the moment an operator tightens retention.
//! This rollup is tiny, permanent and readable in one query.

use sqlx::PgPool;
use time::OffsetDateTime;

use super::Result;

/// How many raw durations to keep per shape. p50/p90 are recomputed from these
/// rather than decayed, so the numbers stay explainable — a p90 you cannot
/// reconstruct is a p90 nobody trusts.
const WINDOW: usize = 50;

#[derive(Debug, Clone)]
pub struct Stat {
    pub kind: String,
    pub shape_key: String,
    pub samples: i32,
    pub p50_ms: i64,
    pub p90_ms: i64,
    pub unit_p50_ms: i64,
    pub updated_at: OffsetDateTime,
}

/// Nearest-rank, rounding DOWN. With an even sample count either neighbour is a
/// defensible median; the lower one is chosen so a single slow outlier cannot pull
/// the p50 up, which matters because the p50 is what the ETA is built from.
fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).floor() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Fold one completed run into its bucket.
///
/// `units` is the count the job actually rendered; a job with no unit count
/// contributes to the whole-job percentiles only, which is right — inventing a
/// per-unit number from a job whose unit count is unknown would poison the one
/// field that makes "remaining" work.
pub async fn record(
    pool: &PgPool,
    kind: &str,
    shape_key: &str,
    duration_ms: i64,
    units: Option<i32>,
) -> Result<()> {
    let existing = sqlx::query!(
        "SELECT recent_ms, recent_unit_ms, samples FROM gpu_job_stats WHERE kind = $1 AND shape_key = $2",
        kind,
        shape_key
    )
    .fetch_optional(pool)
    .await?;

    let (mut recent, mut recent_unit, samples) = match existing {
        Some(r) => (r.recent_ms, r.recent_unit_ms, r.samples),
        None => (Vec::new(), Vec::new(), 0),
    };

    recent.push(duration_ms);
    if recent.len() > WINDOW {
        let drop = recent.len() - WINDOW;
        recent.drain(0..drop);
    }
    if let Some(u) = units.filter(|u| *u > 0) {
        recent_unit.push(duration_ms / u as i64);
        if recent_unit.len() > WINDOW {
            let drop = recent_unit.len() - WINDOW;
            recent_unit.drain(0..drop);
        }
    }

    let mut sorted = recent.clone();
    sorted.sort_unstable();
    let mut sorted_unit = recent_unit.clone();
    sorted_unit.sort_unstable();

    sqlx::query!(
        r#"
        INSERT INTO gpu_job_stats
            (kind, shape_key, samples, p50_ms, p90_ms, unit_p50_ms, recent_ms, recent_unit_ms, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
        ON CONFLICT (kind, shape_key) DO UPDATE SET
            samples = $3, p50_ms = $4, p90_ms = $5, unit_p50_ms = $6,
            recent_ms = $7, recent_unit_ms = $8, updated_at = NOW()
        "#,
        kind,
        shape_key,
        samples + 1,
        percentile(&sorted, 0.5),
        percentile(&sorted, 0.9),
        percentile(&sorted_unit, 0.5),
        &recent,
        &recent_unit,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// One bucket. Used by the dispatcher's tests to assert that a finished run
/// actually taught the estimator — a silent failure otherwise: the queue works
/// and the ETAs never improve.
#[cfg(test)]
pub async fn get(pool: &PgPool, kind: &str, shape_key: &str) -> Result<Option<Stat>> {
    let row = sqlx::query_as!(
        Stat,
        r#"SELECT kind, shape_key, samples, p50_ms, p90_ms, unit_p50_ms, updated_at
           FROM gpu_job_stats WHERE kind = $1 AND shape_key = $2"#,
        kind,
        shape_key
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Every bucket, for the estimator's one-query load and the settings page.
pub async fn all(pool: &PgPool) -> Result<Vec<Stat>> {
    let rows = sqlx::query_as!(
        Stat,
        r#"SELECT kind, shape_key, samples, p50_ms, p90_ms, unit_p50_ms, updated_at
           FROM gpu_job_stats ORDER BY kind, shape_key"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// p50/p90 over a known set. The index is on the SORTED window, so a run of
    /// fast jobs after one slow one must not drag the median up.
    #[test]
    fn percentiles_read_off_the_sorted_window() {
        let mut v: Vec<i64> = (1..=10).map(|n| n * 1000).collect();
        v.sort_unstable();
        assert_eq!(percentile(&v, 0.5), 5000);
        assert_eq!(percentile(&v, 0.9), 9000);
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[7], 0.9), 7);
    }
}
