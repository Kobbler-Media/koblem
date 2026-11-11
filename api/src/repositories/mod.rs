pub mod auth;
pub mod corrections;
pub mod director_assets;
pub mod director_projects;
pub mod director_shot_renders;
pub mod generations;
pub mod gpu_job_stats;
pub mod gpu_jobs;
pub mod gpu_logs;
pub mod gpu_scheduler;
pub mod grammars;
pub mod llm_logs;
pub mod prompt_templates;
pub mod push_subscriptions;
pub mod review_sessions;
pub mod settings;

pub type Result<T> = std::result::Result<T, sqlx::Error>;

/// One lock for every DB-backed test in this crate. Per-module locks are not
/// enough: clearing `director_projects` cascades through the FK into
/// `director_shot_renders` while that module's own tests are mid-run.
#[cfg(test)]
pub fn test_db_lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The one DB-test harness: take the crate-wide lock, connect, migrate, clear
/// `tables`, and hand back the guard so a caller cannot forget to hold it.
///
/// `None` ONLY when no test database is configured. Everything past that point is
/// a hard failure — swallowing setup errors is what let whole modules report green
/// while executing nothing.
#[cfg(test)]
pub async fn test_pool(
    tables: &str,
) -> Option<(sqlx::PgPool, tokio::sync::MutexGuard<'static, ()>)> {
    let guard = test_db_lock().lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("DATABASE_URL is set but the test database is unreachable");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("test database migrations failed");
    // DELETE, not TRUNCATE: the lock above orders the tests that call this
    // against each other, but the rest of the suite shares this database four
    // threads wide. TRUNCATE takes an AccessExclusiveLock on every table named
    // and everything cascading off them, against whatever those tests are
    // mid-read on. Every child of these tables is ON DELETE CASCADE or SET
    // NULL, and none of them carry a sequence for RESTART IDENTITY to reset,
    // so row-by-row deletes clear exactly the same state.
    for table in tables.split(',') {
        let table = table.trim();
        sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("clearing {table} failed: {e}"));
    }
    Some((pool, guard))
}
