//! Durable session + turn state. Modelled on `LtxJobStore`: the background task
//! owns the WORK, the store owns the STATE, and a browser disconnect is a
//! non-event — the turn keeps running and the row keeps ticking.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::wire::{Step, StoredAttachment, TurnStatus};

pub type Result<T> = std::result::Result<T, sqlx::Error>;

pub struct SessionRow {
    pub id: Uuid,
    pub title: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub struct MessageRow {
    pub id: Uuid,
    pub seq: i32,
    pub role: String,
    pub content: String,
    pub attachments: Vec<StoredAttachment>,
    pub steps: Vec<Step>,
    pub status: String,
    pub created_at: OffsetDateTime,
}

pub async fn create_session(db: &PgPool, title: &str) -> Result<SessionRow> {
    let id = Uuid::now_v7();
    let row = sqlx::query!(
        "INSERT INTO agent_sessions (id, title) VALUES ($1, $2) \
         RETURNING id, title, created_at, updated_at",
        id,
        title,
    )
    .fetch_one(db)
    .await?;
    Ok(SessionRow {
        id: row.id,
        title: row.title,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

pub async fn list_sessions(db: &PgPool) -> Result<Vec<SessionRow>> {
    let rows = sqlx::query!(
        "SELECT id, title, created_at, updated_at FROM agent_sessions \
         ORDER BY updated_at DESC LIMIT 200"
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SessionRow {
            id: r.id,
            title: r.title,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect())
}

pub async fn get_session(db: &PgPool, id: Uuid) -> Result<Option<SessionRow>> {
    let row = sqlx::query!(
        "SELECT id, title, created_at, updated_at FROM agent_sessions WHERE id = $1",
        id
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| SessionRow {
        id: r.id,
        title: r.title,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

pub async fn delete_session(db: &PgPool, id: Uuid) -> Result<bool> {
    let r = sqlx::query!("DELETE FROM agent_sessions WHERE id = $1", id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn set_title(db: &PgPool, id: Uuid, title: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE agent_sessions SET title = $2, updated_at = NOW() WHERE id = $1",
        id,
        title
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn messages(db: &PgPool, session_id: Uuid) -> Result<Vec<MessageRow>> {
    let rows = sqlx::query!(
        "SELECT id, seq, role, content, attachments, steps, status, created_at \
         FROM agent_messages WHERE session_id = $1 ORDER BY seq ASC",
        session_id
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| MessageRow {
            id: r.id,
            seq: r.seq,
            role: r.role,
            content: r.content,
            attachments: from_json(r.attachments),
            steps: from_json(r.steps),
            status: r.status,
            created_at: r.created_at,
        })
        .collect())
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartTurn {
    Started(Uuid),
    /// A turn is already running in this session; its id, so the caller can
    /// point the browser at the one that is live.
    Busy(Uuid),
    NoSession,
}

/// Append the user turn and the assistant turn it will be answered by, in one
/// transaction so a crash between them can never leave an unanswerable message.
///
/// One turn per session at a time: the session row is locked FOR UPDATE, so two
/// concurrent sends cannot compute the same `seq` (a double-clicked Send used to
/// 500 on `UNIQUE (session_id, seq)`) and cannot run two loops over one
/// conversation, each replaying the other's empty `running` row.
pub async fn start_turn(
    db: &PgPool,
    session_id: Uuid,
    content: &str,
    attachments: &[StoredAttachment],
) -> Result<StartTurn> {
    let mut tx = db.begin().await?;
    // Runtime queries, not `query!`: these two are the only members of this
    // module that would need a fresh offline-cache entry, and the cache is
    // shared with work in flight elsewhere.
    let locked =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM agent_sessions WHERE id = $1 FOR UPDATE")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
    if locked.is_none() {
        return Ok(StartTurn::NoSession);
    }
    if let Some(live) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM agent_messages WHERE session_id = $1 AND status = 'running' \
         ORDER BY seq DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        return Ok(StartTurn::Busy(live));
    }

    let next: i32 = sqlx::query_scalar!(
        "SELECT COALESCE(MAX(seq) + 1, 0) FROM agent_messages WHERE session_id = $1",
        session_id
    )
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(0);

    sqlx::query!(
        "INSERT INTO agent_messages (id, session_id, seq, role, content, attachments, status) \
         VALUES ($1, $2, $3, 'user', $4, $5, 'complete')",
        Uuid::now_v7(),
        session_id,
        next,
        content,
        serde_json::to_value(attachments).unwrap_or_else(|_| Value::Array(vec![])),
    )
    .execute(&mut *tx)
    .await?;

    let turn_id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO agent_messages (id, session_id, seq, role, status) \
         VALUES ($1, $2, $3, 'assistant', $4)",
        turn_id,
        session_id,
        next + 1,
        TurnStatus::Running.as_str(),
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "UPDATE agent_sessions SET updated_at = NOW() WHERE id = $1",
        session_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StartTurn::Started(turn_id))
}

pub async fn put_steps(db: &PgPool, turn_id: Uuid, steps: &[Step]) -> Result<()> {
    sqlx::query!(
        "UPDATE agent_messages SET steps = $2, updated_at = NOW() WHERE id = $1",
        turn_id,
        serde_json::to_value(steps).unwrap_or_else(|_| Value::Array(vec![])),
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn finish_turn(
    db: &PgPool,
    turn_id: Uuid,
    status: TurnStatus,
    content: &str,
    steps: &[Step],
) -> Result<()> {
    sqlx::query!(
        "UPDATE agent_messages SET status = $2, content = $3, steps = $4, updated_at = NOW() \
         WHERE id = $1",
        turn_id,
        status.as_str(),
        content,
        serde_json::to_value(steps).unwrap_or_else(|_| Value::Array(vec![])),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Flag every still-running turn in a session for cooperative cancellation. The
/// loop polls [`is_cancel_requested`] at each GPU-acquire boundary, so a turn
/// still queued behind another render aborts before it ever renders.
pub async fn request_cancel(db: &PgPool, session_id: Uuid) -> Result<()> {
    sqlx::query!(
        "UPDATE agent_messages SET cancel_requested = TRUE, updated_at = NOW() \
         WHERE session_id = $1 AND status = 'running'",
        session_id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// A turn whose row has GONE is cancelled: `delete_session` requests the cancel
/// and DELETEs back to back, so the watcher usually reads the row after it has
/// already been removed. Only a DB error means "keep going".
pub async fn is_cancel_requested(db: &PgPool, turn_id: Uuid) -> bool {
    match sqlx::query_scalar!(
        "SELECT cancel_requested FROM agent_messages WHERE id = $1",
        turn_id
    )
    .fetch_optional(db)
    .await
    {
        Ok(Some(flag)) => flag,
        Ok(None) => true,
        Err(_) => false,
    }
}

/// Mark turns abandoned by a crashed process, so the UI is not left polling a
/// turn nothing is driving. Called once at startup.
pub async fn cancel_stale_running(db: &PgPool) -> Result<u64> {
    let r = sqlx::query!(
        "UPDATE agent_messages SET status = 'cancelled', updated_at = NOW() \
         WHERE status = 'running'"
    )
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

fn from_json<T: serde::de::DeserializeOwned>(v: Value) -> Vec<T> {
    serde_json::from_value(v).unwrap_or_default()
}

/// First line of the user's opening message, as the session title.
pub fn title_from(content: &str) -> String {
    let line = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("New chat");
    let trimmed: String = line.chars().take(60).collect();
    if trimmed.chars().count() < line.chars().count() {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::OnceLock;
    use tokio::sync::Mutex as AsyncMutex;

    fn lock() -> &'static AsyncMutex<()> {
        static M: OnceLock<AsyncMutex<()>> = OnceLock::new();
        M.get_or_init(|| AsyncMutex::new(()))
    }

    async fn test_pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("DATABASE_URL is set but the test database is unreachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations failed");
        // DELETE, not TRUNCATE: `lock()` only orders these five tests against
        // each other, and the ~950 others share this database four threads wide.
        // TRUNCATE ... CASCADE takes an AccessExclusiveLock on agent_sessions and
        // every table cascading off it, which deadlocks against whatever those
        // tests are reading. agent_messages is ON DELETE CASCADE and nothing here
        // is a sequence, so a plain DELETE clears the same rows under a
        // RowExclusiveLock that conflicts with nobody.
        sqlx::query("DELETE FROM agent_sessions")
            .execute(&pool)
            .await
            .expect("clearing agent_sessions failed");
        Some(pool)
    }

    fn started(t: StartTurn) -> Uuid {
        match t {
            StartTurn::Started(id) => id,
            other => panic!("expected a started turn, got {other:?}"),
        }
    }

    /// A double-clicked Send: both requests read the same `MAX(seq)+1` and the
    /// second 500s on `UNIQUE (session_id, seq)`. When they miss each other, two
    /// loops run over one conversation instead.
    #[tokio::test]
    async fn a_second_send_while_a_turn_runs_is_refused_not_raced() {
        let _l = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let session = create_session(&pool, "New chat").await.unwrap();

        let (a, b) = tokio::join!(
            start_turn(&pool, session.id, "make me a dog", &[]),
            start_turn(&pool, session.id, "make me a cat", &[]),
        );
        let (a, b) = (a.expect("first send"), b.expect("second send"));
        let outcomes = [a, b];
        let starts = outcomes
            .iter()
            .filter(|o| matches!(o, StartTurn::Started(_)))
            .count();
        let busies = outcomes
            .iter()
            .filter(|o| matches!(o, StartTurn::Busy(_)))
            .count();
        assert_eq!(starts, 1, "exactly one turn may start: {outcomes:?}");
        assert_eq!(busies, 1, "the loser must be told, not 500ed: {outcomes:?}");

        // One user turn + one assistant turn, not two of each.
        assert_eq!(messages(&pool, session.id).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_finished_turn_lets_the_next_send_through() {
        let _l = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let session = create_session(&pool, "New chat").await.unwrap();
        let first = started(start_turn(&pool, session.id, "one", &[]).await.unwrap());
        assert!(matches!(
            start_turn(&pool, session.id, "two", &[]).await.unwrap(),
            StartTurn::Busy(id) if id == first
        ));

        finish_turn(&pool, first, TurnStatus::Complete, "done", &[])
            .await
            .unwrap();
        let second = started(start_turn(&pool, session.id, "two", &[]).await.unwrap());
        assert_ne!(second, first);
        let rows = messages(&pool, session.id).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[tokio::test]
    async fn a_send_to_a_missing_session_is_reported_not_inserted() {
        let _l = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        assert_eq!(
            start_turn(&pool, Uuid::now_v7(), "hello", &[])
                .await
                .unwrap(),
            StartTurn::NoSession
        );
    }

    /// `delete_session` cancels then DELETEs back to back, so the 900 ms watcher
    /// loses the race and reads a row that is already gone. Treating a missing
    /// row as "not cancelled" left the turn rendering for a conversation nobody
    /// can see any more.
    #[tokio::test]
    async fn a_deleted_session_cancels_the_turn_it_took_with_it() {
        let _l = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let session = create_session(&pool, "New chat").await.unwrap();
        let turn = started(start_turn(&pool, session.id, "render", &[]).await.unwrap());
        assert!(!is_cancel_requested(&pool, turn).await);

        request_cancel(&pool, session.id).await.unwrap();
        assert!(is_cancel_requested(&pool, turn).await);

        let fresh = create_session(&pool, "New chat").await.unwrap();
        let live = started(start_turn(&pool, fresh.id, "render", &[]).await.unwrap());
        assert!(delete_session(&pool, fresh.id).await.unwrap());
        assert!(
            is_cancel_requested(&pool, live).await,
            "a turn whose row is gone must stop"
        );
    }

    /// Both the startup sweep and `request_cancel` select on it; without the
    /// index they seq-scan every message ever sent.
    #[tokio::test]
    async fn running_turns_are_indexed() {
        let _l = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let found: Option<String> = sqlx::query_scalar(
            "SELECT indexdef FROM pg_indexes \
             WHERE tablename = 'agent_messages' AND indexdef LIKE '%running%'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert!(found.is_some(), "no partial index for status = 'running'");
    }

    #[test]
    fn a_title_is_the_first_non_empty_line_capped() {
        assert_eq!(title_from("\n\n  make me a dog  \nmore"), "make me a dog");
        assert_eq!(title_from(""), "New chat");
        assert_eq!(title_from("   "), "New chat");
    }

    #[test]
    fn a_long_title_is_elided_at_sixty_characters() {
        let t = title_from(&"x".repeat(200));
        assert_eq!(t.chars().count(), 61);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn unreadable_stored_json_degrades_to_empty_rather_than_failing_the_read() {
        let steps: Vec<Step> = from_json(serde_json::json!({"not": "an array"}));
        assert!(steps.is_empty());
        let atts: Vec<StoredAttachment> = from_json(Value::Null);
        assert!(atts.is_empty());
    }
}
