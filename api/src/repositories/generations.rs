//! Generation gallery repository — the user-facing per-service asset record.
//! Mirrors gpu_logs.rs conventions. Generations own their on-disk files; the
//! caller resolves the file root by service and unlinks the filenames returned
//! by delete/prune.

use std::path::{Path, PathBuf};

use koblem_client::types::{GenerationOutput, GenerationRow};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::Result;

/// Resolve the on-disk root for a service's generation files — the single
/// source of truth shared by the persist path and the retention sweeper.
///
/// avatar/music always persist to their own dirs. Every other service persists
/// ONLY when `generations_dir` is a non-empty path; an unset/blank dir returns
/// None, which the persist path treats as "do not write" — the invariant that
/// keeps a fresh deploy from writing into a volume before it's configured.
pub fn resolve_root(
    service: &str,
    avatars_dir: &Path,
    songs_dir: &Path,
    generations_dir: Option<&str>,
) -> Option<PathBuf> {
    match service {
        "avatar" => Some(avatars_dir.to_path_buf()),
        "music" => Some(songs_dir.to_path_buf()),
        _ => generations_dir
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    }
}

/// Input for creating a generation row. `id` is caller-supplied (uuid).
pub struct CreateGeneration {
    pub id: String,
    pub service: String,
    pub endpoint: String,
    pub request_meta: Value,
    pub outputs: Vec<GenerationOutput>,
}

const FALLBACK_MIME: &str = "application/octet-stream";

/// Field-by-field so one odd entry cannot empty the row: `outputs` is
/// schema-free JSONB, and a whole-array deserialize turning into `[]` makes
/// `/generations/{id}/file/{index}` 404 for files that are still on disk.
/// `filename` is the only field with no sane default — without it there is
/// nothing to serve. The route relies on `mime` coming back non-empty.
fn parse_outputs(id: &str, outputs: &Value) -> Vec<GenerationOutput> {
    let Some(arr) = outputs.as_array() else {
        if !outputs.is_null() {
            tracing::warn!(generation_id = %id, "generation outputs is not an array; ignoring");
        }
        return Vec::new();
    };
    arr.iter()
        .filter_map(|o| {
            let Some(filename) = o.get("filename").and_then(Value::as_str) else {
                tracing::warn!(generation_id = %id, "generation output has no filename; dropped");
                return None;
            };
            Some(GenerationOutput {
                filename: filename.to_string(),
                mime: o
                    .get("mime")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(FALLBACK_MIME)
                    .to_string(),
                size_bytes: o.get("size_bytes").and_then(Value::as_i64).unwrap_or(0),
                kind: o
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn row_from_parts(
    id: String,
    service: String,
    endpoint: String,
    request_meta: Value,
    outputs: Value,
    title: Option<String>,
    created_at: OffsetDateTime,
) -> GenerationRow {
    let outputs = parse_outputs(&id, &outputs);
    GenerationRow {
        id,
        service,
        endpoint,
        request_meta,
        outputs,
        title,
        created_at: created_at.format(&Rfc3339).unwrap_or_default(),
    }
}

/// A victim of delete/prune — enough to unlink the files from disk.
pub struct Victim {
    pub service: String,
    pub filenames: Vec<String>,
}

fn outputs_to_value(outputs: &[GenerationOutput]) -> Value {
    serde_json::to_value(outputs).unwrap_or_else(|_| Value::Array(Vec::new()))
}

/// Pull the `filename` of each output entry out of an `outputs` JSONB value.
pub fn output_filenames(outputs: &Value) -> Vec<String> {
    outputs
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|o| o.get("filename").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub async fn create(pool: &PgPool, g: &CreateGeneration) -> Result<()> {
    let outputs = outputs_to_value(&g.outputs);
    sqlx::query!(
        r#"
        INSERT INTO generations (id, service, endpoint, request_meta, outputs)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        g.id,
        g.service,
        g.endpoint,
        g.request_meta,
        outputs,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list(
    pool: &PgPool,
    service: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<GenerationRow>> {
    let rows = if let Some(svc) = service {
        sqlx::query!(
            r#"
            SELECT id, service, endpoint, request_meta, outputs, title, created_at
            FROM generations
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
        .into_iter()
        .map(|r| {
            row_from_parts(
                r.id,
                r.service,
                r.endpoint,
                r.request_meta,
                r.outputs,
                r.title,
                r.created_at,
            )
        })
        .collect()
    } else {
        sqlx::query!(
            r#"
            SELECT id, service, endpoint, request_meta, outputs, title, created_at
            FROM generations
            ORDER BY created_at DESC
            LIMIT $1 OFFSET $2
            "#,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| {
            row_from_parts(
                r.id,
                r.service,
                r.endpoint,
                r.request_meta,
                r.outputs,
                r.title,
                r.created_at,
            )
        })
        .collect()
    };
    Ok(rows)
}

pub async fn count(pool: &PgPool, service: Option<&str>) -> Result<i64> {
    let n = if let Some(svc) = service {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) FROM generations WHERE service = $1"#,
            svc
        )
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query_scalar!(r#"SELECT COUNT(*) FROM generations"#)
            .fetch_one(pool)
            .await?
    };
    Ok(n.unwrap_or(0))
}

pub async fn get(pool: &PgPool, id: &str) -> Result<Option<GenerationRow>> {
    let row = sqlx::query!(
        r#"
        SELECT id, service, endpoint, request_meta, outputs, title, created_at
        FROM generations
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        row_from_parts(
            r.id,
            r.service,
            r.endpoint,
            r.request_meta,
            r.outputs,
            r.title,
            r.created_at,
        )
    }))
}

/// Set the async-generated title for a generation. Best-effort — the title path
/// swallows the error, so a failed update just leaves `title` NULL.
pub async fn set_title(pool: &PgPool, id: &str, title: &str) -> Result<()> {
    sqlx::query!(
        r#"UPDATE generations SET title = $1 WHERE id = $2"#,
        title,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete one generation, returning its service + output filenames so the caller
/// can unlink the files from the service-resolved root.
/// Which of `filenames` are STILL referenced by some surviving generation row (§6.2).
///
/// Content addressing means two rows can point at one file, so unlinking a deleted row's
/// filenames would pull the bytes out from under whatever else uses them — the one real cost the
/// plan called out. Call this AFTER the rows are gone: what it returns is what must be kept.
///
/// The lateral join is over `outputs`, which is a JSONB array of objects carrying `filename`;
/// there are ~100 rows with a handful of outputs each, so this is a scan of nothing.
pub async fn still_referenced(pool: &PgPool, filenames: &[String]) -> Result<Vec<String>> {
    if filenames.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT o->>'filename' AS "filename!"
        FROM generations, LATERAL jsonb_array_elements(outputs) AS o
        WHERE o->>'filename' = ANY($1)
        "#,
        filenames,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Every filename the gallery currently references, for the orphan sweep (§6.2).
pub async fn all_referenced_filenames(pool: &PgPool, service: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT o->>'filename' AS "filename!"
        FROM generations, LATERAL jsonb_array_elements(outputs) AS o
        WHERE service = $1 AND o->>'filename' IS NOT NULL
        "#,
        service,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Every distinct service that has rows — the sweep needs one root per service.
pub async fn distinct_services(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar!(r#"SELECT DISTINCT service AS "service!" FROM generations"#)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Rewrite one row's `outputs`, for the content-addressing migration.
pub async fn set_outputs(pool: &PgPool, id: &str, outputs: &Value) -> Result<()> {
    sqlx::query!(
        r#"UPDATE generations SET outputs = $2 WHERE id = $1"#,
        id,
        outputs,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Every row's id + service + outputs, for the migration walk. Small table by construction.
pub async fn all_rows_for_migration(pool: &PgPool) -> Result<Vec<(String, String, Value)>> {
    let rows = sqlx::query!(r#"SELECT id, service, outputs FROM generations"#)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.id, r.service, r.outputs))
        .collect())
}

pub async fn delete(pool: &PgPool, id: &str) -> Result<Option<Victim>> {
    let row = sqlx::query!(
        r#"DELETE FROM generations WHERE id = $1 RETURNING service, outputs"#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Victim {
        service: r.service,
        filenames: output_filenames(&r.outputs),
    }))
}

/// Delete every generation for one service. Returns victims (filenames) to unlink.
pub async fn delete_all_for_service(pool: &PgPool, service: &str) -> Result<Vec<Victim>> {
    let rows = sqlx::query!(
        r#"DELETE FROM generations WHERE service = $1 RETURNING service, outputs"#,
        service,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Victim {
            service: r.service,
            filenames: output_filenames(&r.outputs),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::repositories::test_pool as shared_pool;

    async fn test_pool() -> Option<(PgPool, tokio::sync::MutexGuard<'static, ()>)> {
        shared_pool("generations").await
    }

    async fn seed(pool: &PgPool, service: &str, filename: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        create(
            pool,
            &CreateGeneration {
                id: id.clone(),
                service: service.to_string(),
                endpoint: "test".to_string(),
                request_meta: json!({"test": true}),
                outputs: vec![GenerationOutput {
                    filename: filename.to_string(),
                    mime: "image/png".to_string(),
                    size_bytes: 1,
                    kind: "image".to_string(),
                }],
            },
        )
        .await
        .unwrap();
        id
    }

    #[test]
    fn resolve_root_enforces_unconfigured_invariant() {
        let avatars = Path::new("/var/avatars");
        let songs = Path::new("/var/songs");

        // avatar/music always persist to their own dirs, regardless of setting.
        assert_eq!(
            resolve_root("avatar", avatars, songs, None).as_deref(),
            Some(avatars)
        );
        assert_eq!(
            resolve_root("music", avatars, songs, None).as_deref(),
            Some(songs)
        );

        // The four base64/optional services persist NOTHING until generations_dir
        // is a real path — unset and blank both mean "do not write".
        for svc in ["images", "matting", "tts", "stt"] {
            assert_eq!(resolve_root(svc, avatars, songs, None), None, "{svc} unset");
            assert_eq!(
                resolve_root(svc, avatars, songs, Some("   ")),
                None,
                "{svc} blank"
            );
            assert_eq!(
                resolve_root(svc, avatars, songs, Some("/var/gen")).as_deref(),
                Some(Path::new("/var/gen")),
                "{svc} configured"
            );
        }
    }

    fn parse(outputs: Value) -> Vec<GenerationOutput> {
        row_from_parts(
            "gid".to_string(),
            "images".to_string(),
            "test".to_string(),
            json!({}),
            outputs,
            None,
            OffsetDateTime::now_utc(),
        )
        .outputs
    }

    /// The stored JSONB is not schema-enforced, so the read path must survive
    /// every shape a writer (past or foreign) could have left behind. Losing an
    /// entry here 404s `/generations/{id}/file/{index}` for a file that is on disk.
    #[test]
    fn malformed_outputs_degrade_per_entry_instead_of_emptying_the_row() {
        // A missing `mime` used to be tolerated by the route; it must not take
        // the whole array down.
        let got = parse(json!([{ "filename": "a.png", "size_bytes": 5, "kind": "image" }]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].mime, "application/octet-stream");
        assert_eq!(got[0].size_bytes, 5);

        // Wrong-typed / blank fields fall back rather than failing the entry.
        let got = parse(json!([{ "filename": "a.png", "mime": 7, "size_bytes": "5" }]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].mime, "application/octet-stream");
        assert_eq!(got[0].size_bytes, 0);
        assert_eq!(got[0].kind, "");
        let got = parse(json!([{ "filename": "a.png", "mime": "   " }]));
        assert_eq!(got[0].mime, "application/octet-stream");

        // Unknown keys are ignored, and one junk entry does not evict its siblings —
        // the index of every entry after it would otherwise shift.
        let got = parse(json!([
            { "filename": "a.png", "mime": "image/png", "size_bytes": 1, "kind": "image", "extra": 1 },
            { "no_filename": true },
            { "filename": "b.png", "mime": "image/png", "size_bytes": 2, "kind": "image" },
        ]));
        assert_eq!(
            got.iter().map(|o| o.filename.as_str()).collect::<Vec<_>>(),
            vec!["a.png", "b.png"]
        );

        // Not-an-array is empty, not a panic.
        assert!(parse(json!({ "filename": "a.png" })).is_empty());
        assert!(parse(Value::Null).is_empty());
    }

    /// Same tolerance, but through real JSONB — Postgres round-trips the value,
    /// so this is the shape `get`/`list` actually hand the file route.
    #[tokio::test]
    async fn malformed_stored_outputs_survive_get_and_list() {
        let Some((pool, _guard)) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let id = uuid::Uuid::new_v4().to_string();
        let outputs = json!([
            { "filename": "no-mime.png", "size_bytes": 9, "kind": "image", "legacy_url": "/x" },
            { "filename": "b.wav", "mime": "audio/wav", "size_bytes": 2, "kind": "audio" },
        ]);
        sqlx::query(
            "INSERT INTO generations (id, service, endpoint, request_meta, outputs)
             VALUES ($1, 'images', 'test', '{}'::jsonb, $2)",
        )
        .bind(&id)
        .bind(&outputs)
        .execute(&pool)
        .await
        .unwrap();

        let row = get(&pool, &id).await.unwrap().expect("row exists");
        assert_eq!(row.outputs.len(), 2, "a bad entry must not empty the row");
        assert_eq!(row.outputs[0].filename, "no-mime.png");
        assert_eq!(row.outputs[0].mime, "application/octet-stream");
        assert_eq!(row.outputs[0].size_bytes, 9);
        assert_eq!(row.outputs[1].mime, "audio/wav");

        let listed = list(&pool, Some("images"), 50, 0).await.unwrap();
        assert_eq!(listed[0].outputs.len(), 2);

        // The unlink path reads the raw value and must agree on the same entries.
        let victim = delete(&pool, &id).await.unwrap().expect("row existed");
        assert_eq!(victim.filenames, vec!["no-mime.png", "b.wav"]);
    }

    #[tokio::test]
    async fn create_list_delete_roundtrip() {
        let Some((pool, _guard)) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let id = seed(&pool, "images", "a.png").await;
        let rows = list(&pool, Some("images"), 50, 0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]
                .outputs
                .iter()
                .map(|o| o.filename.as_str())
                .collect::<Vec<_>>(),
            vec!["a.png"]
        );

        let victim = delete(&pool, &id).await.unwrap().expect("row existed");
        assert_eq!(victim.service, "images");
        assert_eq!(victim.filenames, vec!["a.png"]);
        assert_eq!(count(&pool, Some("images")).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn set_title_roundtrips_through_list_and_get() {
        let Some((pool, _guard)) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let id = seed(&pool, "images", "a.png").await;
        // Fresh rows have no title.
        assert!(get(&pool, &id).await.unwrap().unwrap().title.is_none());

        set_title(&pool, &id, "Snowy cabin at dusk").await.unwrap();

        assert_eq!(
            get(&pool, &id).await.unwrap().unwrap().title.as_deref(),
            Some("Snowy cabin at dusk")
        );
        let rows = list(&pool, Some("images"), 50, 0).await.unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Snowy cabin at dusk"));
    }
}
