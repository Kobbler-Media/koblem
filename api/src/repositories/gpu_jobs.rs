//! The `gpu_jobs` table.
//!
//! Postgres rather than the disk sidecars the five old stores used, because the
//! queue needs ordering, atomic reorder and multiple writers (HTTP handlers, the
//! dispatcher, the reconcile loop) and JSON sidecars get all three wrong.
//!
//! Compile-time-checked `query!` macros throughout: this is a brand-new table
//! carrying jsonb payloads that must survive overnight across a deploy, and the
//! build box can run `make sqlx-prepare`.

use koblem_client::types::{BlockedReason, Job, JobKind, JobProgress, JobState};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;

use super::Result;

/// Distance between two freshly-enqueued jobs. Large enough that midpoint
/// insertion has ~50 subdivisions of headroom before it needs rebalancing, which
/// no realistic drag session reaches.
const RANK_STEP: f64 = 1024.0;

/// Below this gap between neighbours, midpoint insertion stops producing distinct
/// f64s and the band is renumbered. `f64` has ~52 bits of mantissa, so this is
/// two orders of magnitude of slack rather than a cliff.
const RANK_MIN_GAP: f64 = 1e-6;

/// What an enqueue needs. Everything the dispatcher fills in later is absent.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub id: String,
    pub kind: JobKind,
    pub payload: Value,
    pub band: i16,
    pub target_gpu: Option<String>,
    pub force_pin: bool,
    pub depends_on: Option<String>,
    pub created_by: String,
    pub idempotency_key: Option<String>,
    pub units_total: Option<i32>,
    pub shape_key: Option<String>,
    /// `held` when the queue is paused at enqueue time, else `queued`.
    pub state: JobState,
}

/// The raw row. Kept private-ish and mapped into the wire [`Job`] so the API
/// never leaks a column name.
struct Row {
    id: String,
    kind: String,
    state: String,
    band: i16,
    queue_rank: f64,
    target_gpu: Option<String>,
    force_pin: bool,
    server_job_id: Option<String>,
    progress: Value,
    depends_on: Option<String>,
    created_by: String,
    blocked_reason: Option<Value>,
    blocked_since: Option<OffsetDateTime>,
    blocked_history: Value,
    error: Option<String>,
    warnings: Value,
    queued_ms: Option<i64>,
    admit_ms: Option<i64>,
    render_ms: Option<i64>,
    units_total: Option<i32>,
    units_done: i32,
    shape_key: Option<String>,
    attempt: i32,
    retry_of: Option<String>,
    created_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

impl Row {
    /// `None` for a row whose kind this build does not run — a retired engine's
    /// historical row. It is skipped everywhere rather than read as some other
    /// kind: it never dispatches, never reattaches and never lists.
    fn into_job(self) -> Option<Job> {
        let Some(kind) = JobKind::parse(&self.kind) else {
            tracing::debug!(
                id = %self.id,
                kind = %self.kind,
                "gpu_jobs: skipping a row of an unknown kind"
            );
            return None;
        };
        Some(Job {
            kind,
            state: JobState::parse(&self.state).unwrap_or(JobState::Failed),
            id: self.id,
            band: self.band,
            queue_rank: self.queue_rank,
            target_gpu: self.target_gpu,
            force_pin: self.force_pin,
            server_job_id: self.server_job_id,
            progress: serde_json::from_value(self.progress).unwrap_or_default(),
            depends_on: self.depends_on,
            created_by: self.created_by,
            blocked_reason: self
                .blocked_reason
                .and_then(|v| serde_json::from_value::<BlockedReason>(v).ok()),
            blocked_since: self.blocked_since.map(rfc3339),
            blocked_history: serde_json::from_value(self.blocked_history).unwrap_or_default(),
            error: self.error,
            warnings: serde_json::from_value(self.warnings).unwrap_or_default(),
            queued_ms: self.queued_ms,
            admit_ms: self.admit_ms,
            render_ms: self.render_ms,
            units_total: self.units_total,
            units_done: self.units_done,
            shape_key: self.shape_key,
            attempt: self.attempt,
            retry_of: self.retry_of,
            label: String::new(),
            created_at: rfc3339(self.created_at),
            started_at: self.started_at.map(rfc3339),
            finished_at: self.finished_at.map(rfc3339),
            eta_start: None,
            eta_finish: None,
            eta_finish_p90: None,
            estimate_samples: 0,
        })
    }
}

/// Insert, or return the live job that already holds this idempotency key.
///
/// 🔴 `ON CONFLICT DO NOTHING` rather than a SELECT first: two clicks land in two
/// connections and check-then-act loses that race exactly when it matters. The
/// bool is "this was a duplicate".
pub async fn enqueue(pool: &PgPool, new: &NewJob) -> Result<(Job, bool)> {
    let rank = next_rank(pool, new.band).await?;
    let inserted = sqlx::query_as!(
        Row,
        r#"
        INSERT INTO gpu_jobs
            (id, kind, payload, state, band, queue_rank, target_gpu, force_pin,
             depends_on, created_by, idempotency_key, units_total, shape_key)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT DO NOTHING
        RETURNING id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
                  progress, depends_on, created_by, blocked_reason, blocked_since,
                  blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
                  units_total, units_done, shape_key, attempt, retry_of,
                  created_at, started_at, finished_at
        "#,
        new.id,
        new.kind.as_str(),
        new.payload,
        new.state.as_str(),
        new.band,
        rank,
        new.target_gpu,
        new.force_pin,
        new.depends_on,
        new.created_by,
        new.idempotency_key,
        new.units_total,
        new.shape_key,
    )
    .fetch_optional(pool)
    .await?;

    if let Some(row) = inserted {
        return row
            .into_job()
            .map(|j| (j, false))
            .ok_or(sqlx::Error::RowNotFound);
    }

    // The unique index is scoped to the non-terminal states, so the only row this
    // can have collided with is a live one carrying the same key.
    let Some(key) = new.idempotency_key.as_deref() else {
        // No key means the conflict was on the primary key — a caller reusing an
        // id. Surface the existing row rather than inventing a second one.
        let existing = get(pool, &new.id).await?;
        return match existing {
            Some(j) => Ok((j, true)),
            None => Err(sqlx::Error::RowNotFound),
        };
    };
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
               progress, depends_on, created_by, blocked_reason, blocked_since,
               blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
               units_total, units_done, shape_key, attempt, retry_of,
               created_at, started_at, finished_at
        FROM gpu_jobs
        WHERE idempotency_key = $1 AND state NOT IN ('done', 'failed', 'cancelled')
        "#,
        key,
    )
    .fetch_one(pool)
    .await?;
    row.into_job()
        .map(|j| (j, true))
        .ok_or(sqlx::Error::RowNotFound)
}

/// The rank a new job gets: after everything currently in its band.
async fn next_rank(pool: &PgPool, band: i16) -> Result<f64> {
    let max: Option<f64> = sqlx::query_scalar!(
        "SELECT MAX(queue_rank) FROM gpu_jobs WHERE band = $1 AND state IN ('queued', 'held')",
        band
    )
    .fetch_one(pool)
    .await?;
    Ok(max.unwrap_or(0.0) + RANK_STEP)
}

/// Pending rows in dispatch order: band, then rank.
pub async fn pending(pool: &PgPool) -> Result<Vec<Job>> {
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
               progress, depends_on, created_by, blocked_reason, blocked_since,
               blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
               units_total, units_done, shape_key, attempt, retry_of,
               created_at, started_at, finished_at
        FROM gpu_jobs
        WHERE state IN ('queued', 'held')
        ORDER BY band ASC, queue_rank ASC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(Row::into_job).collect())
}

/// Rows occupying a lane and a VRAM claim right now.
pub async fn live(pool: &PgPool) -> Result<Vec<Job>> {
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
               progress, depends_on, created_by, blocked_reason, blocked_since,
               blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
               units_total, units_done, shape_key, attempt, retry_of,
               created_at, started_at, finished_at
        FROM gpu_jobs
        WHERE state IN ('running', 'collecting')
        ORDER BY started_at ASC NULLS LAST
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(Row::into_job).collect())
}

pub async fn get(pool: &PgPool, id: &str) -> Result<Option<Job>> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
               progress, depends_on, created_by, blocked_reason, blocked_since,
               blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
               units_total, units_done, shape_key, attempt, retry_of,
               created_at, started_at, finished_at
        FROM gpu_jobs WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(Row::into_job))
}

pub async fn payload(pool: &PgPool, id: &str) -> Result<Option<(Value, Option<Value>)>> {
    let row = sqlx::query!("SELECT payload, resume FROM gpu_jobs WHERE id = $1", id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| (r.payload, r.resume)))
}

/// Newest-first page for the queue history.
pub async fn recent(pool: &PgPool, limit: i64) -> Result<Vec<Job>> {
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
               progress, depends_on, created_by, blocked_reason, blocked_since,
               blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
               units_total, units_done, shape_key, attempt, retry_of,
               created_at, started_at, finished_at
        FROM gpu_jobs
        ORDER BY created_at DESC
        LIMIT $1
        "#,
        limit
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(Row::into_job).collect())
}

/// Claim a queued job for this dispatcher.
///
/// `FOR UPDATE SKIP LOCKED` in a CTE so two writers cannot both take it. Returns
/// false when it was already claimed, cancelled or held in between — which is the
/// normal outcome of a reorder landing mid-tick, not an error.
pub async fn claim(pool: &PgPool, id: &str, queued_ms: i64) -> Result<bool> {
    let updated = sqlx::query!(
        r#"
        WITH claimed AS (
            SELECT id FROM gpu_jobs
            WHERE id = $1 AND state = 'queued'
            FOR UPDATE SKIP LOCKED
        )
        UPDATE gpu_jobs SET
            state = 'running',
            started_at = NOW(),
            queued_ms = $2,
            blocked_history =
                gpu_jobs_close_episode(blocked_history, blocked_reason, blocked_since),
            blocked_reason = NULL,
            blocked_since = NULL,
            alerted_at = NULL
        WHERE id IN (SELECT id FROM claimed)
        "#,
        id,
        queued_ms,
    )
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() > 0)
}

/// Put a claimed row back after the acquire failed.
///
/// The world moved between `can_place` and `try_acquire` — another job started, a
/// card was disabled. That is a reason to wait, never a failure, so the row goes
/// back to `queued` and the next tick stamps a fresh reason.
pub async fn requeue_after_failed_admit(pool: &PgPool, id: &str) -> Result<()> {
    sqlx::query!(
        r#"UPDATE gpu_jobs SET state = 'queued', started_at = NULL, queued_ms = NULL
           WHERE id = $1 AND state = 'running' AND server_job_id IS NULL"#,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Stamp why a job did not start.
///
/// 🔴 `blocked_since` is preserved across ticks that report the SAME reason —
/// otherwise "waiting 4h" resets to zero every two seconds and the one number
/// that tells you something is wrong never grows.
///
/// A reason CHANGE is the edge that closes an episode: that wait is over, and it
/// is the only moment its duration is known.
pub async fn stamp_blocked(pool: &PgPool, id: &str, reason: &BlockedReason) -> Result<()> {
    let json = serde_json::to_value(reason).unwrap_or(Value::Null);
    sqlx::query!(
        r#"
        UPDATE gpu_jobs SET
            blocked_history = CASE
                WHEN blocked_reason IS DISTINCT FROM $2
                THEN gpu_jobs_close_episode(blocked_history, blocked_reason, blocked_since)
                ELSE blocked_history END,
            blocked_reason = $2,
            blocked_since = CASE
                WHEN blocked_reason IS DISTINCT FROM $2 OR blocked_since IS NULL
                THEN NOW() ELSE blocked_since END,
            alerted_at = CASE
                WHEN blocked_reason IS DISTINCT FROM $2 THEN NULL ELSE alerted_at END
        WHERE id = $1
        "#,
        id,
        json,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claim the blocked-too-long alert for this row. `true` exactly once per
/// episode: `stamp_blocked` clears it whenever the reason changes, and `claim`
/// clears it when the job finally starts, so a job that is blocked, cleared and
/// blocked again alerts twice — which is right, it is a second problem.
pub async fn mark_alerted(pool: &PgPool, id: &str) -> Result<bool> {
    let n = sqlx::query!(
        "UPDATE gpu_jobs SET alerted_at = NOW() WHERE id = $1 AND alerted_at IS NULL",
        id
    )
    .execute(pool)
    .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn set_server_job_id(pool: &PgPool, id: &str, server_job_id: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE gpu_jobs SET server_job_id = $2 WHERE id = $1",
        id,
        server_job_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Record what the run produced, in the kind's own shape.
pub async fn set_result(pool: &PgPool, id: &str, result: &Value) -> Result<()> {
    sqlx::query!("UPDATE gpu_jobs SET result = $2 WHERE id = $1", id, result)
        .execute(pool)
        .await?;
    Ok(())
}

/// The payload, the checkpoint and the result in one read — everything a
/// per-engine detail route needs that the wire `Job` does not carry.
pub async fn detail(
    pool: &PgPool,
    id: &str,
) -> Result<Option<(Value, Option<Value>, Option<Value>)>> {
    let row = sqlx::query!(
        "SELECT payload, resume, result FROM gpu_jobs WHERE id = $1",
        id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.payload, r.resume, r.result)))
}

pub async fn set_admit_ms(pool: &PgPool, id: &str, admit_ms: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE gpu_jobs SET admit_ms = $2 WHERE id = $1",
        id,
        admit_ms
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_progress(
    pool: &PgPool,
    id: &str,
    progress: &JobProgress,
    units_done: i32,
) -> Result<()> {
    let json = serde_json::to_value(progress).unwrap_or(Value::Null);
    sqlx::query!(
        "UPDATE gpu_jobs SET progress = $2, units_done = $3 WHERE id = $1",
        id,
        json,
        units_done
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The engine/disk boundary: the card and the lane are free, the artifact is
/// landing, and the job is past the point where a cancel could do anything but
/// lose it.
pub async fn to_collecting(pool: &PgPool, id: &str, render_ms: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE gpu_jobs SET state = 'collecting', render_ms = $2 WHERE id = $1 AND state = 'running'",
        id,
        render_ms
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn finish(
    pool: &PgPool,
    id: &str,
    state: JobState,
    error: Option<&str>,
    warnings: &[String],
) -> Result<()> {
    let warnings = serde_json::to_value(warnings).unwrap_or(Value::Array(vec![]));
    sqlx::query!(
        r#"
        UPDATE gpu_jobs SET
            state = $2, error = $3, warnings = $4, finished_at = NOW(),
            blocked_history =
                gpu_jobs_close_episode(blocked_history, blocked_reason, blocked_since),
            blocked_reason = NULL, blocked_since = NULL
        WHERE id = $1
        "#,
        id,
        state.as_str(),
        error,
        warnings,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Put a failed run back for another attempt.
///
/// The row is reused rather than replaced: it keeps its place in its band and its
/// dependents keep pointing at it. `error` is left on the row while it waits, so
/// a retrying job says what went wrong rather than looking freshly queued, and
/// `attempt` is what stops it looking like a first try.
///
/// False means the attempts are spent and the caller must fail it terminally.
pub async fn retry(pool: &PgPool, id: &str, error: &str, max_attempts: i32) -> Result<bool> {
    let n = sqlx::query!(
        r#"
        UPDATE gpu_jobs SET
            state = 'queued', attempt = attempt + 1, error = $2,
            started_at = NULL, queued_ms = NULL, render_ms = NULL,
            server_job_id = NULL, units_done = 0, progress = '{}'::jsonb
        WHERE id = $1 AND state IN ('running', 'collecting') AND attempt < $3
        "#,
        id,
        error,
        max_attempts,
    )
    .execute(pool)
    .await?;
    Ok(n.rows_affected() > 0)
}

/// Link a manually-requeued row to the terminal one it came from, and continue
/// its attempt count — a hand-retried job is not a first attempt either.
pub async fn record_retry_of(pool: &PgPool, id: &str, parent: &str) -> Result<()> {
    sqlx::query!(
        r#"UPDATE gpu_jobs SET retry_of = $2,
               attempt = COALESCE((SELECT attempt FROM gpu_jobs WHERE id = $2), 0) + 1
           WHERE id = $1"#,
        id,
        parent,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A preempted job goes back to the HEAD of its band carrying its checkpoint.
pub async fn requeue_preempted(pool: &PgPool, id: &str, resume: &Value) -> Result<()> {
    let head: Option<f64> = sqlx::query_scalar!(
        r#"SELECT MIN(queue_rank) FROM gpu_jobs
           WHERE band = (SELECT band FROM gpu_jobs WHERE id = $1)
             AND state IN ('queued', 'held') AND id <> $1"#,
        id
    )
    .fetch_one(pool)
    .await?;
    let rank = head.unwrap_or(RANK_STEP) - RANK_STEP;
    sqlx::query!(
        r#"
        UPDATE gpu_jobs SET
            state = 'queued', queue_rank = $2, resume = $3,
            server_job_id = NULL, started_at = NULL
        WHERE id = $1
        "#,
        id,
        rank,
        resume,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Move a job within (or between) bands. One row update, which is the whole point
/// of a float rank.
///
/// `after = None` puts it at the head of the band. Returns false when the band
/// needs renumbering first — the caller runs [`rebalance`] and retries.
pub async fn reorder(pool: &PgPool, id: &str, band: i16, after: Option<&str>) -> Result<bool> {
    let (prev, next): (f64, Option<f64>) = match after {
        Some(anchor) => {
            let a: Option<f64> = sqlx::query_scalar!(
                "SELECT queue_rank FROM gpu_jobs WHERE id = $1 AND band = $2",
                anchor,
                band
            )
            .fetch_optional(pool)
            .await?;
            let Some(a) = a else { return Ok(false) };
            let n: Option<f64> = sqlx::query_scalar!(
                r#"SELECT MIN(queue_rank) FROM gpu_jobs
                   WHERE band = $1 AND state IN ('queued', 'held')
                     AND queue_rank > $2 AND id <> $3"#,
                band,
                a,
                id
            )
            .fetch_one(pool)
            .await?;
            (a, n)
        }
        None => {
            let n: Option<f64> = sqlx::query_scalar!(
                r#"SELECT MIN(queue_rank) FROM gpu_jobs
                   WHERE band = $1 AND state IN ('queued', 'held') AND id <> $2"#,
                band,
                id
            )
            .fetch_one(pool)
            .await?;
            (n.unwrap_or(RANK_STEP) - 2.0 * RANK_STEP, n)
        }
    };

    let rank = match next {
        Some(n) => {
            if n - prev < RANK_MIN_GAP {
                return Ok(false);
            }
            (prev + n) / 2.0
        }
        None => prev + RANK_STEP,
    };

    sqlx::query!(
        "UPDATE gpu_jobs SET band = $2, queue_rank = $3 WHERE id = $1",
        id,
        band,
        rank
    )
    .execute(pool)
    .await?;
    Ok(true)
}

/// Renumber a band onto whole `RANK_STEP` boundaries, preserving order. Only ever
/// needed after enough midpoint insertions to exhaust f64 precision between two
/// neighbours, which takes ~50 drags onto the same seam.
pub async fn rebalance(pool: &PgPool, band: i16) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE gpu_jobs g SET queue_rank = r.n::double precision * $2
        FROM (
            SELECT id, ROW_NUMBER() OVER (ORDER BY queue_rank ASC) AS n
            FROM gpu_jobs WHERE band = $1 AND state IN ('queued', 'held')
        ) r
        WHERE g.id = r.id
        "#,
        band,
        RANK_STEP,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Move (or clear) a queued job's card pin.
///
/// Only while it is pending: past admission the placement is already made and the
/// pin would be a lie about where it is running.
pub async fn repin(
    pool: &PgPool,
    id: &str,
    target_gpu: Option<&str>,
    force_pin: bool,
) -> Result<bool> {
    let n = sqlx::query!(
        r#"UPDATE gpu_jobs SET target_gpu = $2, force_pin = $3
           WHERE id = $1 AND state IN ('queued', 'held')"#,
        id,
        target_gpu,
        force_pin,
    )
    .execute(pool)
    .await?;
    Ok(n.rows_affected() > 0)
}

/// Straight to the head of band 0. The operator's "run this next".
pub async fn bump(pool: &PgPool, id: &str) -> Result<()> {
    reorder(pool, id, 0, None).await?;
    Ok(())
}

pub async fn set_held(pool: &PgPool, id: &str, held: bool) -> Result<bool> {
    let want = if held { "held" } else { "queued" };
    let from = if held { "queued" } else { "held" };
    let n = sqlx::query!(
        "UPDATE gpu_jobs SET state = $2 WHERE id = $1 AND state = $3",
        id,
        want,
        from
    )
    .execute(pool)
    .await?;
    Ok(n.rows_affected() > 0)
}

/// Cancel a job that has not started. Returns false when it is already live or
/// terminal — a running job is cancelled through the dispatcher (so the engine
/// hears about it), and a `collecting` one is not cancellable at all.
pub async fn cancel_pending(pool: &PgPool, id: &str) -> Result<bool> {
    let n = sqlx::query!(
        r#"UPDATE gpu_jobs SET state = 'cancelled', finished_at = NOW(),
               blocked_history =
                   gpu_jobs_close_episode(blocked_history, blocked_reason, blocked_since),
               blocked_reason = NULL, blocked_since = NULL
           WHERE id = $1 AND state IN ('queued', 'held')"#,
        id
    )
    .execute(pool)
    .await?;
    Ok(n.rows_affected() > 0)
}

/// A parent that failed or was cancelled takes its dependents with it. They are
/// cancelled, not failed: nothing was wrong with them.
pub async fn cascade_dependents(pool: &PgPool, parent: &str) -> Result<Vec<String>> {
    let ids = sqlx::query_scalar!(
        r#"UPDATE gpu_jobs SET state = 'cancelled', finished_at = NOW(),
               error = 'the job this depended on did not complete'
           WHERE depends_on = $1 AND state IN ('queued', 'held')
           RETURNING id"#,
        parent
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Boot: a `running` row with no `server_job_id` never reached the engine, so
/// nothing is lost by putting it back in the queue.
pub async fn requeue_unsubmitted(pool: &PgPool) -> Result<Vec<String>> {
    let ids = sqlx::query_scalar!(
        r#"UPDATE gpu_jobs SET state = 'queued', started_at = NULL
           WHERE state IN ('running', 'collecting') AND server_job_id IS NULL
           RETURNING id"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Boot: rows that outlived a restart WITH an engine job id, to be re-polled.
pub async fn reattachable(pool: &PgPool) -> Result<Vec<Job>> {
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
               progress, depends_on, created_by, blocked_reason, blocked_since,
               blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
               units_total, units_done, shape_key, attempt, retry_of,
               created_at, started_at, finished_at
        FROM gpu_jobs
        WHERE state IN ('running', 'collecting') AND server_job_id IS NOT NULL
        ORDER BY started_at ASC NULLS LAST
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(Row::into_job).collect())
}

/// Drop terminal rows past the retention horizon.
///
/// 🔴 The ROW only. A generated asset lives until the user deletes it — that is
/// the policy everywhere, no exceptions per kind — and the old stores' TTLs were
/// sweeping the record, not the artifact. The one place that was broken is
/// `upscale`, whose 7-day TTL deleted the output file because it never reached
/// the gallery; that is fixed separately by giving `persist_generation` a
/// streaming path.
pub async fn prune_terminal(pool: &PgPool, older_than_days: i64) -> Result<u64> {
    let n = sqlx::query!(
        r#"DELETE FROM gpu_jobs
           WHERE state IN ('done', 'failed', 'cancelled')
             AND finished_at IS NOT NULL
             AND finished_at < NOW() - make_interval(days => $1::int)"#,
        older_than_days as i32,
    )
    .execute(pool)
    .await?;
    Ok(n.rows_affected())
}

/// Jobs blocked longer than `secs` on a reason nothing will clear by itself.
///
/// 🔴 This is the overnight failure mode: waking up to five jobs that never
/// started because a service was left disabled is exactly what the queue exists
/// to prevent. Blocked is not broken — but blocked-for-hours has to be loud.
pub async fn stuck(pool: &PgPool, secs: i64) -> Result<Vec<Job>> {
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
               progress, depends_on, created_by, blocked_reason, blocked_since,
               blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
               units_total, units_done, shape_key, attempt, retry_of,
               created_at, started_at, finished_at
        FROM gpu_jobs
        WHERE state = 'queued'
          AND blocked_since IS NOT NULL
          AND blocked_since < NOW() - make_interval(secs => $1::double precision)
        ORDER BY blocked_since ASC
        "#,
        secs as f64,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(Row::into_job).collect())
}

/// An identical non-terminal job enqueued moments ago, for callers that did not
/// supply an idempotency key.
///
/// A backstop, not the mechanism: the UI and the CLI both mint a key, and the
/// unique index is what actually makes a double-click safe. This catches a
/// hand-rolled caller that does neither.
pub async fn recent_duplicate(
    pool: &PgPool,
    kind: JobKind,
    payload: &Value,
    within_secs: i64,
) -> Result<Option<Job>> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT id, kind, state, band, queue_rank, target_gpu, force_pin, server_job_id,
               progress, depends_on, created_by, blocked_reason, blocked_since,
               blocked_history, error, warnings, queued_ms, admit_ms, render_ms,
               units_total, units_done, shape_key, attempt, retry_of,
               created_at, started_at, finished_at
        FROM gpu_jobs
        WHERE kind = $1 AND payload = $2
          AND state NOT IN ('done', 'failed', 'cancelled')
          AND created_at > NOW() - make_interval(secs => $3::double precision)
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        kind.as_str(),
        payload,
        within_secs as f64,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(Row::into_job))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::test_pool;

    async fn pool() -> Option<(PgPool, tokio::sync::MutexGuard<'static, ()>)> {
        test_pool("gpu_jobs, gpu_job_stats").await
    }

    fn new(id: &str, kind: JobKind, band: i16) -> NewJob {
        NewJob {
            id: id.into(),
            kind,
            payload: serde_json::json!({ "caption": id }),
            band,
            target_gpu: None,
            force_pin: false,
            depends_on: None,
            created_by: "user:t".into(),
            idempotency_key: None,
            units_total: Some(1),
            shape_key: Some("t/shape".into()),
            state: JobState::Queued,
        }
    }

    /// Band first, then rank within the band. Three jobs enqueued in one order and
    /// dispatched in another is the whole point of having two axes.
    #[tokio::test]
    async fn the_scan_order_is_band_then_rank() {
        let Some((p, _g)) = pool().await else { return };
        for (id, band) in [("a", 1i16), ("b", 2), ("c", 0), ("d", 1)] {
            enqueue(&p, &new(id, JobKind::Music3, band)).await.unwrap();
        }
        let order: Vec<String> = pending(&p)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(order, vec!["c", "a", "d", "b"]);
    }

    /// A drag-reorder is ONE row update, and it lands the job between its new
    /// neighbours rather than renumbering the band.
    #[tokio::test]
    async fn a_reorder_is_one_row_update() {
        let Some((p, _g)) = pool().await else { return };
        for id in ["a", "b", "c"] {
            enqueue(&p, &new(id, JobKind::Music3, 1)).await.unwrap();
        }
        assert!(reorder(&p, "c", 1, Some("a")).await.unwrap());
        let order: Vec<String> = pending(&p)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(order, vec!["a", "c", "b"]);

        // To the head of the band.
        assert!(reorder(&p, "b", 1, None).await.unwrap());
        let order: Vec<String> = pending(&p)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(order, vec!["b", "a", "c"]);
    }

    /// Enough midpoint insertions onto one seam exhaust f64 precision. The
    /// rebalance renumbers the band and preserves the order — the failure it
    /// prevents is two jobs landing on the same rank and the queue order becoming
    /// arbitrary.
    #[tokio::test]
    async fn a_rebalance_preserves_the_order() {
        let Some((p, _g)) = pool().await else { return };
        for id in ["a", "b", "c", "d"] {
            enqueue(&p, &new(id, JobKind::Music3, 1)).await.unwrap();
        }
        reorder(&p, "d", 1, Some("a")).await.unwrap();
        let before: Vec<String> = pending(&p)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        rebalance(&p, 1).await.unwrap();
        let after: Vec<String> = pending(&p)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(before, after);
        // And every rank is now a clean multiple of the step.
        for job in pending(&p).await.unwrap() {
            assert_eq!(job.queue_rank % RANK_STEP, 0.0, "{}", job.id);
        }
    }

    /// 🔴 The 409 was accidentally protecting the user. An enqueue succeeds twice,
    /// and you wake up to two identical 40-minute renders.
    #[tokio::test]
    async fn a_double_click_does_not_render_twice() {
        let Some((p, _g)) = pool().await else { return };
        let mut a = new("first", JobKind::Music3, 1);
        a.idempotency_key = Some("form-abc".into());
        let mut b = new("second", JobKind::Music3, 1);
        b.idempotency_key = Some("form-abc".into());

        let (j1, dup1) = enqueue(&p, &a).await.unwrap();
        let (j2, dup2) = enqueue(&p, &b).await.unwrap();
        assert!(!dup1);
        assert!(dup2, "the second click is a duplicate");
        assert_eq!(j1.id, j2.id, "and it gets the FIRST job back");
        assert_eq!(pending(&p).await.unwrap().len(), 1);
    }

    /// ...but once it has finished, the same key is a legitimate second take. An
    /// index over every state would refuse it forever.
    #[tokio::test]
    async fn the_same_key_is_free_again_once_the_job_is_done() {
        let Some((p, _g)) = pool().await else { return };
        let mut a = new("first", JobKind::Music3, 1);
        a.idempotency_key = Some("k".into());
        enqueue(&p, &a).await.unwrap();
        finish(&p, "first", JobState::Done, None, &[])
            .await
            .unwrap();

        let mut b = new("second", JobKind::Music3, 1);
        b.idempotency_key = Some("k".into());
        let (j, dup) = enqueue(&p, &b).await.unwrap();
        assert!(!dup);
        assert_eq!(j.id, "second");
    }

    /// A preempted job goes back to the HEAD of its band carrying its checkpoint —
    /// not to the back, which would make preempting a 6-hour render a punishment.
    #[tokio::test]
    async fn a_preempted_job_returns_to_the_head_with_its_bank() {
        let Some((p, _g)) = pool().await else { return };
        for id in ["victim", "other"] {
            enqueue(&p, &new(id, JobKind::Music3, 1)).await.unwrap();
        }
        claim(&p, "victim", 0).await.unwrap();
        set_server_job_id(&p, "victim", "engine-1").await.unwrap();

        let bank = serde_json::json!({ "segments_done": 3, "bank_keys": ["k0", "k1", "k2"] });
        requeue_preempted(&p, "victim", &bank).await.unwrap();

        let order: Vec<String> = pending(&p)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(
            order,
            vec!["victim", "other"],
            "back at the head of its band"
        );

        let (_, resume) = payload(&p, "victim").await.unwrap().unwrap();
        assert_eq!(resume.unwrap()["segments_done"], 3);
        let job = get(&p, "victim").await.unwrap().unwrap();
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(
            job.server_job_id, None,
            "the old engine job is gone; a resume submits a new one"
        );
    }

    /// Restart: a row that never reached the engine goes back in the queue (nothing
    /// is lost), and one that DID is handed to reattach. Getting this backwards
    /// either loses a render or double-renders one.
    #[tokio::test]
    async fn a_restart_separates_the_unsubmitted_from_the_reattachable() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("never-sent", JobKind::Music3, 1))
            .await
            .unwrap();
        enqueue(&p, &new("in-flight", JobKind::Music3, 1))
            .await
            .unwrap();
        claim(&p, "never-sent", 0).await.unwrap();
        claim(&p, "in-flight", 0).await.unwrap();
        set_server_job_id(&p, "in-flight", "engine-7")
            .await
            .unwrap();

        let requeued = requeue_unsubmitted(&p).await.unwrap();
        assert_eq!(requeued, vec!["never-sent"]);

        let reattach: Vec<String> = reattachable(&p)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(reattach, vec!["in-flight"]);
        assert_eq!(
            get(&p, "never-sent").await.unwrap().unwrap().state,
            JobState::Queued
        );
    }

    /// A claim is a compare-and-set. A row reordered, held or cancelled between the
    /// scan and the claim is simply not claimed — the normal outcome of a reorder
    /// landing mid-tick, not an error.
    #[tokio::test]
    async fn a_claim_loses_gracefully() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("j", JobKind::Music3, 1)).await.unwrap();
        assert!(claim(&p, "j", 1234).await.unwrap());
        assert!(!claim(&p, "j", 0).await.unwrap(), "twice is refused");
        assert_eq!(get(&p, "j").await.unwrap().unwrap().queued_ms, Some(1234));
    }

    /// 🔴 `blocked_since` survives ticks that report the SAME reason. Without this
    /// "waiting 4h" resets to zero every two seconds and the one number that tells
    /// you something is wrong never grows.
    #[tokio::test]
    async fn a_stable_reason_keeps_its_clock() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("j", JobKind::Music3, 1)).await.unwrap();
        let lane = BlockedReason::LaneBusy {
            lane: "comfy-api".into(),
        };
        stamp_blocked(&p, "j", &lane).await.unwrap();
        let first = get(&p, "j").await.unwrap().unwrap().blocked_since;
        assert!(first.is_some());

        stamp_blocked(&p, "j", &lane).await.unwrap();
        assert_eq!(get(&p, "j").await.unwrap().unwrap().blocked_since, first);

        // A DIFFERENT reason restarts the clock — it is a different wait.
        stamp_blocked(&p, "j", &BlockedReason::EvictionCooldown)
            .await
            .unwrap();
        assert_ne!(get(&p, "j").await.unwrap().unwrap().blocked_since, first);
    }

    /// 🔴 `blocked_reason` is overwritten in place and cleared when the job starts,
    /// so by morning every row says it waited four hours and none says what for.
    /// One entry per EPISODE: the reason, when that wait began, how long it ran.
    #[tokio::test]
    async fn every_wait_leaves_an_episode_behind() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("j", JobKind::Music3, 1)).await.unwrap();
        let lane = BlockedReason::LaneBusy {
            lane: "comfy-api".into(),
        };
        stamp_blocked(&p, "j", &lane).await.unwrap();
        stamp_blocked(&p, "j", &lane).await.unwrap();
        assert!(
            get(&p, "j")
                .await
                .unwrap()
                .unwrap()
                .blocked_history
                .is_empty(),
            "a wait still running is not history yet"
        );

        stamp_blocked(&p, "j", &BlockedReason::EvictionCooldown)
            .await
            .unwrap();
        let history = get(&p, "j").await.unwrap().unwrap().blocked_history;
        assert_eq!(history.len(), 1, "the reason changed, so that wait is over");
        assert_eq!(history[0].reason, lane);
        assert!(history[0].ms >= 0);
        assert!(!history[0].since.is_empty());

        claim(&p, "j", 0).await.unwrap();
        let job = get(&p, "j").await.unwrap().unwrap();
        assert_eq!(job.blocked_history.len(), 2, "starting closes the last one");
        assert_eq!(
            job.blocked_history[1].reason,
            BlockedReason::EvictionCooldown
        );
        assert_eq!(job.blocked_reason, None);
    }

    /// A job parked all night on a flapping lane must not grow an array without
    /// limit. The newest episodes are the ones worth keeping.
    #[tokio::test]
    async fn the_history_is_bounded() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("j", JobKind::Music3, 1)).await.unwrap();
        for i in 0..40 {
            stamp_blocked(
                &p,
                "j",
                &BlockedReason::LaneBusy {
                    lane: format!("lane-{i}"),
                },
            )
            .await
            .unwrap();
        }
        let history = get(&p, "j").await.unwrap().unwrap().blocked_history;
        assert_eq!(history.len(), 20);
        assert_eq!(
            history[19].reason,
            BlockedReason::LaneBusy {
                lane: "lane-38".into()
            },
            "the oldest is dropped, never the newest"
        );
    }

    /// 🔴 A transient failure at 02:00 is not a lost night. The row is REUSED so
    /// it keeps its place in the band and its dependents keep pointing at it, and
    /// `attempt` is what stops it looking like a first try.
    #[tokio::test]
    async fn a_retry_reuses_the_row_and_runs_out() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("j", JobKind::Music3, 1)).await.unwrap();
        for attempt in 1..3 {
            assert!(claim(&p, "j", 0).await.unwrap());
            assert!(retry(&p, "j", "engine went away", 3).await.unwrap());
            let job = get(&p, "j").await.unwrap().unwrap();
            assert_eq!(job.state, JobState::Queued);
            assert_eq!(job.attempt, attempt + 1);
            assert_eq!(
                job.error.as_deref(),
                Some("engine went away"),
                "a job waiting on a retry still says what went wrong"
            );
        }

        assert!(claim(&p, "j", 0).await.unwrap());
        assert!(
            !retry(&p, "j", "engine went away", 3).await.unwrap(),
            "the attempts are spent; the caller fails it terminally"
        );
    }

    /// A manual retry is a FRESH row — the terminal one is the record of what
    /// happened — and it continues the attempt count rather than claiming to be a
    /// first try.
    #[tokio::test]
    async fn a_manual_retry_continues_the_chain() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("first", JobKind::Music3, 1))
            .await
            .unwrap();
        claim(&p, "first", 0).await.unwrap();
        retry(&p, "first", "boom", 3).await.unwrap();
        finish(&p, "first", JobState::Failed, Some("boom"), &[])
            .await
            .unwrap();

        enqueue(&p, &new("second", JobKind::Music3, 1))
            .await
            .unwrap();
        record_retry_of(&p, "second", "first").await.unwrap();
        let job = get(&p, "second").await.unwrap().unwrap();
        assert_eq!(job.retry_of.as_deref(), Some("first"));
        assert_eq!(job.attempt, 3);
        assert_eq!(
            get(&p, "first").await.unwrap().unwrap().state,
            JobState::Failed,
            "and the record of the failed run is untouched"
        );
    }

    /// A parent that failed or was cancelled takes its dependents with it. They are
    /// CANCELLED, not failed: nothing was wrong with them.
    #[tokio::test]
    async fn a_dead_parent_cancels_its_dependents() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("render", JobKind::Music3, 1))
            .await
            .unwrap();
        let mut child = new("upscale", JobKind::Upscale, 1);
        child.depends_on = Some("render".into());
        enqueue(&p, &child).await.unwrap();

        finish(&p, "render", JobState::Failed, Some("engine died"), &[])
            .await
            .unwrap();
        let cascaded = cascade_dependents(&p, "render").await.unwrap();
        assert_eq!(cascaded, vec!["upscale"]);
        let c = get(&p, "upscale").await.unwrap().unwrap();
        assert_eq!(c.state, JobState::Cancelled);
        assert!(c.error.is_some(), "and it says why");
    }

    /// Only a row that has not started can be cancelled outright. A `collecting`
    /// one is past the engine/disk boundary — the GPU is paid for and the artifact
    /// is landing.
    #[tokio::test]
    async fn a_collecting_job_is_not_cancellable() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("j", JobKind::Music3, 1)).await.unwrap();
        assert!(cancel_pending(&p, "j").await.unwrap());

        enqueue(&p, &new("k", JobKind::Music3, 1)).await.unwrap();
        claim(&p, "k", 0).await.unwrap();
        to_collecting(&p, "k", 5_000).await.unwrap();
        assert!(!cancel_pending(&p, "k").await.unwrap());
        let job = get(&p, "k").await.unwrap().unwrap();
        assert_eq!(job.state, JobState::Collecting);
        assert_eq!(job.render_ms, Some(5_000));
    }

    /// Retention drops the ROW, and only a terminal one past the horizon. 🔴 The
    /// asset is never touched: a generated asset lives until the user deletes it.
    #[tokio::test]
    async fn retention_only_takes_old_terminal_rows() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("live", JobKind::Music3, 1)).await.unwrap();
        enqueue(&p, &new("recent", JobKind::Music3, 1))
            .await
            .unwrap();
        enqueue(&p, &new("ancient", JobKind::Music3, 1))
            .await
            .unwrap();
        finish(&p, "recent", JobState::Done, None, &[])
            .await
            .unwrap();
        finish(&p, "ancient", JobState::Done, None, &[])
            .await
            .unwrap();
        sqlx::query(
            "UPDATE gpu_jobs SET finished_at = NOW() - INTERVAL '30 days' WHERE id = 'ancient'",
        )
        .execute(&p)
        .await
        .unwrap();

        assert_eq!(prune_terminal(&p, 14).await.unwrap(), 1);
        assert!(get(&p, "ancient").await.unwrap().is_none());
        assert!(get(&p, "recent").await.unwrap().is_some());
        assert!(get(&p, "live").await.unwrap().is_some());
    }

    /// Held is a pending state, not a terminal one: the dispatcher sees it, refuses
    /// to start it, and releasing it puts it straight back in line.
    #[tokio::test]
    async fn a_held_job_stays_in_the_queue() {
        let Some((p, _g)) = pool().await else { return };
        enqueue(&p, &new("j", JobKind::Music3, 1)).await.unwrap();
        assert!(set_held(&p, "j", true).await.unwrap());
        let listed = pending(&p).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, JobState::Held);
        assert!(!claim(&p, "j", 0).await.unwrap(), "held is not claimable");
        assert!(set_held(&p, "j", false).await.unwrap());
        assert!(claim(&p, "j", 0).await.unwrap());
    }

    /// The backstop for a caller that mints no idempotency key at all.
    #[tokio::test]
    async fn an_identical_payload_moments_later_is_a_duplicate() {
        let Some((p, _g)) = pool().await else { return };
        let job = new("j", JobKind::Music3, 1);
        enqueue(&p, &job).await.unwrap();
        let dup = recent_duplicate(&p, JobKind::Music3, &job.payload, 30)
            .await
            .unwrap();
        assert_eq!(dup.map(|j| j.id).as_deref(), Some("j"));

        // A different payload is not.
        let other = serde_json::json!({ "caption": "something else" });
        assert!(
            recent_duplicate(&p, JobKind::Music3, &other, 30)
                .await
                .unwrap()
                .is_none()
        );

        // And neither is the same one once it has finished.
        finish(&p, "j", JobState::Done, None, &[]).await.unwrap();
        assert!(
            recent_duplicate(&p, JobKind::Music3, &job.payload, 30)
                .await
                .unwrap()
                .is_none()
        );
    }
}
