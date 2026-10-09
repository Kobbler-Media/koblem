//! Director 2 saved projects — `director_projects` table.
//!
//! Each row is an editable, re-runnable video-shot timeline built in the
//! `/ltx-director` editor. `timeline` is the opaque editor doc (compiled render
//! request + inline assets + clip layout + lyric alignment); the routes layer
//! reads it back on `/run` and dispatches the existing chain render.
//!
//! Uses runtime sqlx queries (no `query!` macros) so the sqlx-offline cache
//! doesn't need regenerating on every migration touch — same convention as
//! `repositories/avatar_assets.rs` / `repositories/settings.rs`.

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;

use super::Result;

/// One saved director project.
#[derive(Debug, Clone, Serialize)]
pub struct DirectorProjectRow {
    pub id: String,
    pub name: String,
    pub timeline: Value,
    pub metadata: Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub version: i64,
}

/// Lightweight list entry (omits the heavy `timeline` blob so the projects list
/// stays cheap — base64 assets can make a single timeline several MB).
#[derive(Debug, Clone, Serialize)]
pub struct DirectorProjectSummary {
    pub id: String,
    pub name: String,
    pub metadata: Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub version: i64,
}

/// What a conditional [`update`] did. `Conflict` and `Missing` are different failures: the row
/// moved under us vs. it isn't there, and the route layer owes them different status codes.
#[derive(Debug)]
pub enum UpdateOutcome {
    Updated(DirectorProjectRow),
    Conflict { current_version: i64 },
    Missing,
}

type FullTuple = (
    String,
    String,
    Value,
    Value,
    OffsetDateTime,
    OffsetDateTime,
    i64,
);
type SummaryTuple = (String, String, Value, OffsetDateTime, OffsetDateTime, i64);

const FULL_COLS: &str = "id, name, timeline, metadata, created_at, updated_at, version";
const SUMMARY_COLS: &str = "id, name, metadata, created_at, updated_at, version";

/// The metadata keys the SERVER owns. [`patch_metadata`] is their only writer; a client-authored
/// write can neither set nor drop one.
///
/// A client's `metadata` is a load-time snapshot, and the server stamps these DURING a render
/// without bumping the version (by design — see [`patch_metadata`]). So a full-document PUT quoting
/// a still-valid version carries a copy that predates the stamp, and honouring it wholesale deleted
/// the stamp: a previewed project lost `proxy_render` to the next autosave and every later mobile
/// proxy retake 400'd, and `last_server_job_id` went the same way, throwing away the banked prefix a
/// final retake resumes from. Preserving them on write is what makes "the server owns these" true
/// rather than aspirational.
pub const SERVER_METADATA_KEYS: [&str; 8] = [
    "last_job_id",
    "last_run_at",
    "last_server_job_id",
    // Which engine's store `last_server_job_id` addresses. Server-owned alongside it and never
    // apart from it: a stamp whose engine a client PUT could drop is a stamp that reads as
    // "whatever the project renders on now", which is the `0039` failure.
    "last_server_engine",
    "last_retake_segment",
    "proxy_render",
    // Delivery problems from the last render — a dropped generation, a failed mux. Server-owned
    // for the same reason as the rest: the editor holds a load-time snapshot, so an autosave that
    // quoted a still-valid version would delete the very warning the render had just raised, and
    // it would do it the moment the author touched anything.
    "last_render_warnings",
    "last_render_warnings_at",
];

/// The server-owned subset of the row's CURRENT metadata, as a SQL expression. Correlated on the
/// UPDATE target, so it reads the pre-update value — the merge happens inside the one statement the
/// CAS guards, with no read-modify-write window for a concurrent stamp to fall into.
const KEEP_SERVER_KEYS: &str = "(SELECT COALESCE(jsonb_object_agg(k, metadata -> k), '{}'::jsonb) \
                                FROM unnest($6::text[]) AS k WHERE jsonb_exists(metadata, k))";

fn server_keys() -> Vec<&'static str> {
    SERVER_METADATA_KEYS.to_vec()
}

fn to_row(r: FullTuple) -> DirectorProjectRow {
    DirectorProjectRow {
        id: r.0,
        name: r.1,
        timeline: r.2,
        metadata: r.3,
        created_at: r.4,
        updated_at: r.5,
        version: r.6,
    }
}

fn to_summary(r: SummaryTuple) -> DirectorProjectSummary {
    DirectorProjectSummary {
        id: r.0,
        name: r.1,
        metadata: r.2,
        created_at: r.3,
        updated_at: r.4,
        version: r.5,
    }
}

/// Insert a new project. `id` is a caller-supplied uuid string.
pub async fn create(
    pool: &PgPool,
    id: &str,
    name: &str,
    timeline: &Value,
    metadata: &Value,
) -> Result<DirectorProjectRow> {
    let row: FullTuple = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO director_projects (id, name, timeline, metadata) \
         VALUES ($1, $2, $3, $4::jsonb - $5::text[]) RETURNING {FULL_COLS}"
    )))
    .bind(id)
    .bind(name)
    .bind(timeline)
    .bind(metadata)
    .bind(server_keys())
    .fetch_one(pool)
    .await?;
    Ok(to_row(row))
}

/// List projects, most-recently-edited first (without the `timeline` blob).
pub async fn list(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<DirectorProjectSummary>> {
    let rows: Vec<SummaryTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SUMMARY_COLS} FROM director_projects ORDER BY updated_at DESC LIMIT $1 OFFSET $2"
    )))
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(to_summary).collect())
}

/// Total project count (for list pagination).
pub async fn count(pool: &PgPool) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM director_projects")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// Fetch one full project by id.
pub async fn get(pool: &PgPool, id: &str) -> Result<Option<DirectorProjectRow>> {
    let row: Option<FullTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FULL_COLS} FROM director_projects WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(to_row))
}

/// Replace a project's name/timeline/metadata, but only if `expected_version` is still the version
/// on the row. A mismatch means another client rewrote the document since this caller read it, and
/// its copy would revert that work wholesale — including the banked resume ids.
///
/// [`SERVER_METADATA_KEYS`] are exempt from the replace: the row's own keep theirs, and the
/// caller's copies of them are discarded. The version check cannot cover them, because the writes
/// that produce them deliberately do not move the version.
pub async fn update(
    pool: &PgPool,
    id: &str,
    name: &str,
    timeline: &Value,
    metadata: &Value,
    expected_version: i64,
) -> Result<UpdateOutcome> {
    let row: Option<FullTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE director_projects \
         SET name = $2, timeline = $3, \
             metadata = ($4::jsonb - $6::text[]) || {KEEP_SERVER_KEYS}, \
             version = version + 1, updated_at = NOW() \
         WHERE id = $1 AND version = $5 RETURNING {FULL_COLS}"
    )))
    .bind(id)
    .bind(name)
    .bind(timeline)
    .bind(metadata)
    .bind(expected_version)
    .bind(server_keys())
    .fetch_optional(pool)
    .await?;
    if let Some(row) = row {
        return Ok(UpdateOutcome::Updated(to_row(row)));
    }
    let current: Option<(i64,)> =
        sqlx::query_as("SELECT version FROM director_projects WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(match current {
        Some((current_version,)) => UpdateOutcome::Conflict { current_version },
        None => UpdateOutcome::Missing,
    })
}

/// Rename and/or shallow-merge metadata without touching the timeline. `None` args mean "leave
/// alone"; `Ok(None)` means no such row. [`SERVER_METADATA_KEYS`] are dropped from the patch — a
/// client must not be able to forge a `last_server_job_id` pointing at another project's latents.
///
/// Unconditional by design: this can only ever change the name and merge metadata keys, so it
/// cannot revert anybody's timeline work, and demanding a fresh token would force the caller back
/// into the full-document round trip this path exists to remove. It DOES bump the version — a
/// rename is an authored edit, so a peer holding the older token must not blind-write its stale
/// name back over it.
pub async fn patch(
    pool: &PgPool,
    id: &str,
    name: Option<&str>,
    metadata: Option<&Value>,
) -> Result<Option<DirectorProjectSummary>> {
    let row: Option<SummaryTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE director_projects \
         SET name = COALESCE($2::text, name), \
             metadata = metadata || (COALESCE($3::jsonb, '{{}}'::jsonb) - $4::text[]), \
             version = version + 1, \
             updated_at = NOW() \
         WHERE id = $1 RETURNING {SUMMARY_COLS}"
    )))
    .bind(id)
    .bind(name)
    .bind(metadata)
    .bind(server_keys())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(to_summary))
}

/// Merge a partial JSON object into `metadata` (e.g. stamp `last_job_id` after a
/// render) without rewriting the whole timeline. Bumps `updated_at`.
///
/// Deliberately does NOT bump `version`: these are the server's own render stamps, written under a
/// client that authored nothing. Bumping would invalidate the open editor's token on every single
/// render and train the user to ignore the conflict banner.
pub async fn patch_metadata(pool: &PgPool, id: &str, patch: &Value) -> Result<bool> {
    let res = sqlx::query(
        r#"
        UPDATE director_projects
        SET metadata = metadata || $2, updated_at = NOW()
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(patch)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Delete one project by id. Returns whether a row was removed. The FK CASCADE
/// drops the project's `director_shot_renders` rows; the owned webm dir on disk
/// (`director-renders/<id>/`) is unlinked by the route layer, which holds the path.
pub async fn delete(pool: &PgPool, id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM director_projects WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    use crate::repositories::test_db_lock as lock;

    #[track_caller]
    fn expect_updated(outcome: UpdateOutcome) -> DirectorProjectRow {
        match outcome {
            UpdateOutcome::Updated(row) => row,
            other => panic!("expected an applied update, got {other:?}"),
        }
    }

    /// `None` only when there is no test database configured. Everything after that is a hard
    /// failure: swallowing setup errors turned this whole module green while it silently ran
    /// nothing (the cleanup below used to fail against the `director_shot_renders` foreign
    /// key, so every case skipped).
    async fn test_pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("DATABASE_URL is set but the test database is unreachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations failed");
        sqlx::query("DELETE FROM director_projects")
            .execute(&pool)
            .await
            .expect("clearing director_projects failed");
        Some(pool)
    }

    #[tokio::test]
    async fn create_list_get_update_delete_roundtrip() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let id = uuid::Uuid::now_v7().to_string();
        let timeline = json!({ "request": { "segments": [] }, "editor": {} });
        let meta = json!({});

        let created = create(&pool, &id, "My Music Video", &timeline, &meta)
            .await
            .unwrap();
        assert_eq!(created.name, "My Music Video");

        let listed = list(&pool, 50, 0).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(count(&pool).await.unwrap(), 1);

        let got = get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(
            got.timeline["request"]["segments"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        let updated = expect_updated(
            update(
                &pool,
                &id,
                "Renamed",
                &json!({ "request": { "segments": [{ "prompt": "hi" }] } }),
                &json!({ "notes": "wip" }),
                created.version,
            )
            .await
            .unwrap(),
        );
        assert_eq!(updated.name, "Renamed");
        assert!(updated.updated_at >= updated.created_at);
        assert_eq!(updated.version, created.version + 1);

        assert!(
            patch_metadata(&pool, &id, &json!({ "last_job_id": "abc" }))
                .await
                .unwrap()
        );
        let after = get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(after.metadata["last_job_id"], "abc");
        assert_eq!(after.metadata["notes"], "wip");

        assert!(delete(&pool, &id).await.unwrap());
        assert!(get(&pool, &id).await.unwrap().is_none());
        assert!(!delete(&pool, &id).await.unwrap());
    }

    /// The point of the PATCH path: a rename must not require (or disturb) the timeline, which
    /// carries the project's inline assets and is megabytes wide.
    #[tokio::test]
    async fn patch_leaves_timeline_alone_merges_metadata_and_bumps_version() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let id = uuid::Uuid::now_v7().to_string();
        let timeline = json!({ "request": { "segments": [{ "prompt": "hi" }] } });
        let created = create(&pool, &id, "Before", &timeline, &json!({ "notes": "wip" }))
            .await
            .unwrap();

        let renamed = patch(&pool, &id, Some("After"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.name, "After");
        assert_eq!(renamed.version, created.version + 1);
        let got = get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(got.timeline, timeline);
        assert_eq!(got.metadata["notes"], "wip");

        // Metadata-only patch: merges, leaves the name, bumps again.
        let tagged = patch(&pool, &id, None, Some(&json!({ "starred": true })))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tagged.name, "After");
        assert_eq!(tagged.metadata["notes"], "wip");
        assert_eq!(tagged.metadata["starred"], true);
        assert_eq!(tagged.version, created.version + 2);

        assert!(
            patch(&pool, "nope", Some("After"), None)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The autosave-clobber bug: a second client saving from a stale copy must be rejected, not
    /// silently revert the timeline (resume ids and all) to what that client last read.
    #[tokio::test]
    async fn stale_update_conflicts_instead_of_reverting() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let id = uuid::Uuid::now_v7().to_string();
        let created = create(&pool, &id, "Shared", &json!({ "shots": 0 }), &json!({}))
            .await
            .unwrap();
        let stale = created.version;

        // Device A saves.
        let a = expect_updated(
            update(
                &pool,
                &id,
                "Shared",
                &json!({ "shots": 1, "resume": "job-a" }),
                &json!({}),
                stale,
            )
            .await
            .unwrap(),
        );

        // Device B autosaves from the version it loaded — must not land.
        match update(
            &pool,
            &id,
            "Shared",
            &json!({ "shots": 0 }),
            &json!({}),
            stale,
        )
        .await
        .unwrap()
        {
            UpdateOutcome::Conflict { current_version } => assert_eq!(current_version, a.version),
            other => panic!("expected a conflict, got {other:?}"),
        }
        let got = get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(got.timeline["resume"], "job-a");
        assert_eq!(got.version, a.version);

        // Adopting the current version lets B through.
        let b = expect_updated(
            update(
                &pool,
                &id,
                "Shared",
                &json!({ "shots": 2 }),
                &json!({}),
                got.version,
            )
            .await
            .unwrap(),
        );
        assert_eq!(b.version, a.version + 1);
    }

    /// Rename-while-dirty: the editor PATCHes the name, then its pending timeline save lands. The
    /// PATCH bumps, so the save MUST quote the version the PATCH handed back — a client that kept
    /// its pre-rename token would conflict with a change it made itself.
    #[tokio::test]
    async fn a_rename_makes_the_pending_timeline_save_quote_the_patched_version() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let id = uuid::Uuid::now_v7().to_string();
        let created = create(&pool, &id, "Draft", &json!({ "shots": 1 }), &json!({}))
            .await
            .unwrap();

        let renamed = patch(&pool, &id, Some("Final cut"), None)
            .await
            .unwrap()
            .unwrap();

        // The token the client held before it renamed is now stale.
        assert!(matches!(
            update(
                &pool,
                &id,
                "Final cut",
                &json!({ "shots": 2 }),
                &json!({}),
                created.version
            )
            .await
            .unwrap(),
            UpdateOutcome::Conflict { .. }
        ));

        // Adopting the PATCH's version lets the queued timeline save through with the new name.
        let saved = expect_updated(
            update(
                &pool,
                &id,
                "Final cut",
                &json!({ "shots": 2 }),
                &json!({}),
                renamed.version,
            )
            .await
            .unwrap(),
        );
        assert_eq!(saved.name, "Final cut");
        assert_eq!(saved.timeline["shots"], 2);
    }

    /// A conflict and a deleted project are different failures — the route layer owes them 409 and
    /// 404 respectively, so the repository must not collapse them into one "no rows".
    #[tokio::test]
    async fn missing_row_is_not_reported_as_a_conflict() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let outcome = update(&pool, "nope", "x", &json!({}), &json!({}), 1)
            .await
            .unwrap();
        assert!(matches!(outcome, UpdateOutcome::Missing), "{outcome:?}");
    }

    /// The server's own render stamps must not invalidate the open editor's token, or every render
    /// would raise a conflict the user did nothing to cause.
    #[tokio::test]
    async fn server_metadata_stamps_do_not_bump_the_version() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let id = uuid::Uuid::now_v7().to_string();
        let created = create(&pool, &id, "Rendering", &json!({}), &json!({}))
            .await
            .unwrap();
        assert!(
            patch_metadata(&pool, &id, &json!({ "last_job_id": "abc" }))
                .await
                .unwrap()
        );
        let got = get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(got.metadata["last_job_id"], "abc");
        assert_eq!(got.version, created.version);

        // …and the token the editor was already holding still saves.
        expect_updated(
            update(
                &pool,
                &id,
                "Rendering",
                &json!({}),
                &json!({}),
                created.version,
            )
            .await
            .unwrap(),
        );
    }

    /// The regression this whole exemption exists for: the server stamps `proxy_render` mid-render
    /// WITHOUT bumping the version, so the very next autosave still passes the CAS — and used to
    /// replace `metadata` with a snapshot taken before the stamp existed. The key vanished and every
    /// later `POST /retake {tier:"proxy"}` hard-400'd on "no recorded proxy-tier render geometry".
    ///
    /// The same write must also not let the client FORGE one: `last_server_job_id` is what a final
    /// retake resumes from, and a client-supplied value points the engine at whatever it likes.
    #[tokio::test]
    async fn a_client_put_can_neither_drop_nor_forge_a_server_stamp() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let id = uuid::Uuid::now_v7().to_string();
        let created = create(
            &pool,
            &id,
            "Preview",
            &json!({}),
            &json!({ "notes": "wip" }),
        )
        .await
        .unwrap();

        // The server stamps a render. No version bump, so the editor's token stays valid.
        assert!(
            patch_metadata(
                &pool,
                &id,
                &json!({
                    "proxy_render": { "width": 512, "height": 288, "steps": 8 },
                    "last_server_job_id": "engine-real",
                }),
            )
            .await
            .unwrap()
        );

        // The editor autosaves the metadata it loaded BEFORE the stamp, and tries to author a
        // stamp of its own on the way past.
        let saved = expect_updated(
            update(
                &pool,
                &id,
                "Preview",
                &json!({ "shots": 1 }),
                &json!({ "notes": "edited", "last_server_job_id": "engine-forged" }),
                created.version,
            )
            .await
            .unwrap(),
        );
        assert_eq!(saved.metadata["proxy_render"]["width"], 512);
        assert_eq!(saved.metadata["last_server_job_id"], "engine-real");
        // Client-authored keys still replace wholesale — only the server's are exempt.
        assert_eq!(saved.metadata["notes"], "edited");

        let got = get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(got.metadata["proxy_render"]["steps"], 8);
        assert_eq!(got.metadata["last_server_job_id"], "engine-real");

        // A PATCH is a client-authored write too, and merges rather than replaces — so the only
        // thing to prove is that it cannot author a server key either.
        let patched = patch(
            &pool,
            &id,
            None,
            Some(&json!({ "starred": true, "proxy_render": { "width": 1920 } })),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(patched.metadata["starred"], true);
        assert_eq!(patched.metadata["proxy_render"]["width"], 512);

        // …and a create cannot bootstrap one (a forged banked-latent id on a brand-new project
        // would point a first retake at somebody else's chain).
        let other = uuid::Uuid::now_v7().to_string();
        let fresh = create(
            &pool,
            &other,
            "Fresh",
            &json!({}),
            &json!({ "notes": "mine", "last_server_job_id": "engine-real" }),
        )
        .await
        .unwrap();
        assert_eq!(fresh.metadata["notes"], "mine");
        assert_eq!(fresh.metadata.get("last_server_job_id"), None);
    }

    /// `metadata` used to accept any JSON value, and `'null'::jsonb || '{…}'::jsonb` evaluates to an
    /// ARRAY without erroring — so one such write left a row where every stamp appended an element
    /// and every read returned nothing, forever. The database now refuses the shape outright.
    #[tokio::test]
    async fn a_non_object_metadata_is_refused_by_the_database() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };

        let id = uuid::Uuid::now_v7().to_string();
        assert!(
            create(&pool, &id, "Bad", &json!({}), &Value::Null)
                .await
                .is_err()
        );

        let ok = create(&pool, &id, "Good", &json!({}), &json!({}))
            .await
            .unwrap();
        assert!(
            update(&pool, &id, "Good", &json!({}), &Value::Null, ok.version)
                .await
                .is_err()
        );
        assert!(patch(&pool, &id, None, Some(&json!([1, 2]))).await.is_err());
        // The row is untouched by the refusals, not half-written.
        let got = get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(got.metadata, json!({}));
        assert_eq!(got.version, ok.version);
    }
}
