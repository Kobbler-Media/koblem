//! Rolling retention sweep.
//!
//! Two independent concerns, per §decision-a (generations own their assets;
//! gpu_logs is pure telemetry):
//!
//! 1. **gpu_logs + gpu_samples** — telemetry. Pruned by the `gpu_log_retention_*`
//!    / `gpu_sample_retention_max_bytes` settings (on by default). Only unlinks
//!    tier-2 *sample* files under `samples_dir`; it no longer touches generated
//!    assets.
//! 2. **generations** — the user-facing gallery assets. Pruned by the
//!    `generations_retention_*` settings, which default to 0 (OFF): assets live
//!    until the user deletes them, unless an operator opts into auto-prune.
//!    Files are unlinked from the per-service root (avatar->avatars_dir,
//!    music->songs_dir, everything else->the `generations_dir` setting).
//! 3. **queue working files** (`queue::files`) — scaffolding, not deliverables:
//!    segment directories, engine clips on their way to the gallery, and the
//!    sources `/upscale/upload` captures at enqueue. Nothing unlinks these when a
//!    job is cancelled or never submitted, so they are swept by age against the
//!    live queue rows.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::queue::files::QueueFiles;
use crate::repositories::generations::{self, Victim};
use crate::repositories::{gpu_logs, settings as settings_repo};

const SWEEP_INTERVAL: Duration = Duration::from_secs(3600); // hourly

const DEFAULT_MAX_ENTRIES: i64 = 1000;
const DEFAULT_MAX_AGE_DAYS: i64 = 7;

/// Queue-row retention, in days. Wide enough that a fortnight of overnight batches
/// is still readable the morning after — the record is a few hundred bytes and the
/// question "what ran last night, and did any of it warn?" is worth keeping.
const DEFAULT_QUEUE_ROW_DAYS: i64 = 14;
const DEFAULT_MAX_SAMPLE_BYTES: i64 = 1024 * 1024 * 1024; // 1 GB

/// Working-file retention, in days, measured from last write.
///
/// 🔴 Deliberately SHORTER than the row horizon, and it is not the same knob: these are whole
/// videos (an 8 GB capture, a directory of upscaled segments), and once a job is terminal its
/// deliverable is in the gallery on its own link. Three days is long enough that a user who
/// uploaded a source on Friday can still re-submit it against the same token on Sunday.
const DEFAULT_QUEUE_FILES_DAYS: i64 = 3;

pub struct Roots {
    pub samples_dir: PathBuf,
    pub songs_dir: PathBuf,
    pub avatars_dir: PathBuf,
    pub queue_files: QueueFiles,
}

pub async fn run(pool: PgPool, roots: Roots) {
    // First tick immediately so stale state from previous runs gets pruned
    // before the server starts accepting requests.
    tick(&pool, &roots).await;

    loop {
        sleep(SWEEP_INTERVAL).await;
        tick(&pool, &roots).await;
    }
}

async fn tick(pool: &PgPool, roots: &Roots) {
    prune_telemetry(pool, &roots.samples_dir).await;
    prune_generations(pool, roots).await;
    prune_queue_rows(pool).await;
    prune_queue_files(pool, &roots.queue_files).await;
    sweep_orphan_blobs(pool, roots).await;
}

/// Drop terminal queue ROWS past the horizon.
///
/// 🔴 The row, never the asset. A generated asset lives until the user deletes it,
/// everywhere, no exceptions per kind — and the old per-engine TTLs were sweeping
/// the record, not the artifact.
///
/// The default has to outlive an unattended night by a wide margin: a music3 job
/// finishing at 01:00 used to have no record left by morning (30 minutes), so the
/// queue page could not show what ran. It is a separate knob from the asset for
/// exactly that reason.
async fn prune_queue_rows(pool: &PgPool) {
    let days = read_i64_setting(pool, "queue_row_retention_days", DEFAULT_QUEUE_ROW_DAYS).await;
    if days <= 0 {
        return;
    }
    match crate::repositories::gpu_jobs::prune_terminal(pool, days).await {
        Ok(n) if n > 0 => info!(rows = n, days, "retention: pruned finished queue rows"),
        Err(e) => warn!(error = %e, "retention: could not prune queue rows"),
        _ => {}
    }
}

/// Drop working files no live job needs any more.
///
/// 🔴 This is the leak the per-engine stores used to cover. A capture is written at ENQUEUE
/// because the browser that posted the multipart is gone by dispatch, so nothing in the run path
/// owns it when the job is cancelled — or never submitted at all — and it is a whole video in a
/// directory another container mounts.
async fn prune_queue_files(pool: &PgPool, files: &QueueFiles) {
    let days = read_i64_setting(pool, "queue_files_retention_days", DEFAULT_QUEUE_FILES_DAYS).await;
    if days <= 0 {
        return;
    }
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(days as u64 * 86_400))
    else {
        return;
    };
    sweep_queue_files(pool, files, cutoff).await;
}

async fn sweep_queue_files(pool: &PgPool, files: &QueueFiles, cutoff: SystemTime) {
    // 🔴 List BEFORE asking what is live, never after: a job enqueued between the two reads is
    // then guaranteed to be in the live set, so the race can only ever keep a file too long.
    let work = list_dir(files.work_root()).await;
    let clips = list_dir(files.clips_root()).await;
    let captures = list_dir(files.captures_root()).await;
    let live = match live_working_files(pool).await {
        Ok(live) => live,
        Err(e) => {
            warn!(error = %e, "retention: could not read the live queue rows, keeping every working file");
            return;
        }
    };

    let mut removed = 0usize;
    for c in work.into_iter().chain(clips) {
        if !live.ids.contains(stem(&c.name)) && remove_stale(&c, cutoff).await {
            removed += 1;
        }
    }
    for c in captures {
        if !live.captures.contains(&c.name) && remove_stale(&c, cutoff).await {
            removed += 1;
        }
    }
    if removed > 0 {
        info!(entries = removed, "retention: swept queue working files");
    }
}

struct Candidate {
    name: String,
    path: PathBuf,
    modified: SystemTime,
    is_dir: bool,
}

/// What a non-terminal queue row still needs on disk: its own id (segments, clip) and the captured
/// source its payload names.
///
/// 🔴 Age alone would not be safe. A queued job can sit for eight hours before it is dispatched,
/// and a preempted one's delivered segments ARE its whole resume state — so the sweep asks the
/// queue what is still live, and keeps everything when the question cannot be answered.
///
/// A runtime query rather than `sqlx::query!` for the same reason as the generation prunes above
/// it: retention policy lives in this file, and neither forces an offline-cache regen.
struct Live {
    ids: HashSet<String>,
    captures: HashSet<String>,
}

async fn live_working_files(pool: &PgPool) -> sqlx::Result<Live> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT id, payload->'request'->>'source_path'
        FROM gpu_jobs
        WHERE state NOT IN ('done', 'failed', 'cancelled')
        "#,
    )
    .fetch_all(pool)
    .await?;
    let mut live = Live {
        ids: HashSet::new(),
        captures: HashSet::new(),
    };
    for (id, source) in rows {
        live.ids.insert(id);
        if let Some(name) = source
            .as_deref()
            .and_then(|p| Path::new(p).file_name())
            .and_then(|n| n.to_str())
        {
            live.captures.insert(name.to_string());
        }
    }
    Ok(live)
}

async fn list_dir(root: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(root).await else {
        return out;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        out.push(Candidate {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path(),
            modified,
            is_dir: meta.is_dir(),
        });
    }
    out
}

async fn remove_stale(c: &Candidate, cutoff: SystemTime) -> bool {
    if c.modified > cutoff {
        return false;
    }
    let removed = if c.is_dir {
        tokio::fs::remove_dir_all(&c.path).await
    } else {
        tokio::fs::remove_file(&c.path).await
    };
    match removed {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!(path = %c.path.display(), error = %e,
                  "retention: could not unlink a queue working file");
            false
        }
    }
}

fn stem(name: &str) -> &str {
    name.split_once('.').map_or(name, |(stem, _)| stem)
}

/// A blob is written before its row is inserted, so a file younger than this may simply be a
/// generation still in flight. An hour is far longer than any render takes to persist and far
/// shorter than anyone would notice the disk.
const ORPHAN_GRACE: Duration = Duration::from_secs(3600);

/// Delete content-addressed gallery blobs no row references any more (§6.2).
///
/// This is the mark-and-sweep the plan chose over refcounting, and the reason it chose it:
/// refcounting has to survive a crash between the file write and the row insert, whereas a sweep
/// is crash-proof by construction — anything the DB does not name is garbage, whenever it was
/// orphaned and however the process died.
///
/// Two safety rules, both load-bearing:
/// * only files whose NAME IS A HASH are ever considered. A legacy `{uuid}-{i}.{ext}` output, a
///   `.part` file, an operator's stray copy: none of them can be swept by this.
/// * a file must be older than [`ORPHAN_GRACE`], so a blob written seconds ago by a render whose
///   row is not committed yet cannot be taken out from under it.
async fn sweep_orphan_blobs(pool: &PgPool, roots: &Roots) {
    use crate::routes::tier2_helpers::is_gallery_blob_name;

    // ONLY the gallery dir. avatar and music own their directories and serve those files by name
    // from their own routes; nothing in there is ours to delete, whatever it is named.
    let generations_dir = read_string_setting(pool, "generations_dir").await;
    let Some(gallery) = generations_dir.as_deref().map(std::path::PathBuf::from) else {
        return;
    };
    let services = match generations::distinct_services(pool).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "gpu_retention: orphan sweep could not list services");
            return;
        }
    };
    let mut swept = 0usize;
    let mut seen_roots: Vec<std::path::PathBuf> = Vec::new();
    for service in &services {
        let Some(root) = generations::resolve_root(
            service,
            &roots.avatars_dir,
            &roots.songs_dir,
            generations_dir.as_deref(),
        ) else {
            continue;
        };
        if root != gallery {
            continue;
        }
        // Several services share the gallery dir. Sweeping it once per service would compare its
        // files against ONE service's references and delete every other service's — so it is
        // swept exactly once, against the references of every service that resolves to it.
        if seen_roots.contains(&root) {
            continue;
        }
        seen_roots.push(root.clone());

        let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut ok = true;
        for peer in &services {
            let peer_root = generations::resolve_root(
                peer,
                &roots.avatars_dir,
                &roots.songs_dir,
                generations_dir.as_deref(),
            );
            if peer_root.as_ref() != Some(&root) {
                continue;
            }
            match generations::all_referenced_filenames(pool, peer).await {
                Ok(names) => referenced.extend(names),
                Err(e) => {
                    warn!(error = %e, service = %peer, "gpu_retention: orphan sweep reference read failed");
                    ok = false;
                }
            }
        }
        if !ok {
            continue;
        }

        let Ok(mut dir) = tokio::fs::read_dir(&root).await else {
            continue;
        };
        let now = std::time::SystemTime::now();
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_gallery_blob_name(&name) || referenced.contains(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let old_enough = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= ORPHAN_GRACE);
            if !old_enough {
                continue;
            }
            match tokio::fs::remove_file(entry.path()).await {
                Ok(()) => swept += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(path = %entry.path().display(), error = %e,
                                "gpu_retention: orphan blob unlink failed"),
            }
        }
    }
    if swept > 0 {
        info!(blobs = swept, "gpu_retention: orphan blob sweep done");
    }
}

/// gpu_logs + gpu_samples telemetry retention. Unlinks sample files only.
async fn prune_telemetry(pool: &PgPool, samples_dir: &Path) {
    let max_entries =
        read_i64_setting(pool, "gpu_log_retention_max_entries", DEFAULT_MAX_ENTRIES).await;
    let max_age_days =
        read_i64_setting(pool, "gpu_log_retention_max_age_days", DEFAULT_MAX_AGE_DAYS).await;
    let max_sample_bytes = read_i64_setting(
        pool,
        "gpu_sample_retention_max_bytes",
        DEFAULT_MAX_SAMPLE_BYTES,
    )
    .await;

    let mut unlink_paths: Vec<String> = Vec::new();

    if max_age_days > 0 {
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(max_age_days);
        match gpu_logs::prune_older_than(pool, cutoff).await {
            Ok(paths) => unlink_paths.extend(paths),
            Err(e) => warn!(error = %e, "gpu_retention: prune_older_than failed"),
        }
    }
    if max_entries > 0 {
        match gpu_logs::prune_to_max_entries(pool, max_entries).await {
            Ok(paths) => unlink_paths.extend(paths),
            Err(e) => warn!(error = %e, "gpu_retention: prune_to_max_entries failed"),
        }
    }
    if max_sample_bytes > 0 {
        match gpu_logs::prune_to_max_bytes(pool, max_sample_bytes).await {
            Ok(paths) => unlink_paths.extend(paths),
            Err(e) => warn!(error = %e, "gpu_retention: prune_to_max_bytes failed"),
        }
    }

    let mut removed = 0usize;
    for rel in &unlink_paths {
        let abs = samples_dir.join(rel);
        match tokio::fs::remove_file(&abs).await {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %abs.display(), error = %e, "gpu_retention: sample unlink failed")
            }
        }
    }
    if removed > 0 {
        info!(samples = removed, "gpu_retention: telemetry sweep done");
    }
}

/// Generation gallery retention. OFF by default (settings default to 0).
async fn prune_generations(pool: &PgPool, roots: &Roots) {
    let max_per_service = read_i64_setting(pool, "generations_retention_max_entries", 0).await;
    let max_age_days = read_i64_setting(pool, "generations_retention_max_age_days", 0).await;
    if max_per_service <= 0 && max_age_days <= 0 {
        return;
    }

    let generations_dir = read_string_setting(pool, "generations_dir").await;
    let mut victims: Vec<generations::Victim> = Vec::new();

    if max_age_days > 0 {
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(max_age_days);
        match prune_older_than(pool, cutoff).await {
            Ok(v) => victims.extend(v),
            Err(e) => warn!(error = %e, "gpu_retention: generations prune_older_than failed"),
        }
    }
    if max_per_service > 0 {
        match prune_to_max_per_service(pool, max_per_service).await {
            Ok(v) => victims.extend(v),
            Err(e) => warn!(error = %e, "gpu_retention: generations prune_max failed"),
        }
    }

    let mut removed = 0usize;
    for victim in &victims {
        let Some(root) = generations::resolve_root(
            &victim.service,
            &roots.avatars_dir,
            &roots.songs_dir,
            generations_dir.as_deref(),
        ) else {
            continue;
        };
        // Gallery files are content-addressed, so a pruned row's file may still belong to a row
        // that survived the prune (§6.2). Same rule, and the same failure stance, as the delete
        // route: if the check cannot be made, nothing is unlinked.
        let keep = match generations::still_referenced(pool, &victim.filenames).await {
            Ok(keep) => keep,
            Err(e) => {
                warn!(error = %e, "gpu_retention: reference check failed, keeping the files");
                continue;
            }
        };
        for name in &victim.filenames {
            if name.contains('/') || name.contains('\\') || name.contains("..") {
                warn!(filename = %name, "gpu_retention: refusing suspicious generation path");
                continue;
            }
            if keep.contains(name) {
                continue;
            }
            let abs = root.join(name);
            match tokio::fs::remove_file(&abs).await {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = %abs.display(), error = %e, "gpu_retention: generation unlink failed")
                }
            }
        }
    }
    if removed > 0 {
        info!(files = removed, "gpu_retention: generations sweep done");
    }
}

/// Which rows the sweep may take is retention POLICY, so it lives here rather
/// than in the repository primitive. The one rule the age/count settings do not
/// express: an `agent_messages.steps` document holds nothing but the generation
/// id, so sweeping a row an agent turn still renders leaves a 404 in a
/// transcript the user can open — the mirror image of the check
/// `repositories::director_assets::stale_unreferenced` does for attachment
/// hashes, and exact for this data because a uuid has no other reason to appear
/// in that document.
///
/// Runtime queries (no `query!` macros), matching `repositories/director_assets.rs`:
/// the reference check they mirror is written the same way, and neither forces
/// an offline-cache regen.
async fn prune_older_than(pool: &PgPool, cutoff: OffsetDateTime) -> sqlx::Result<Vec<Victim>> {
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        r#"
        DELETE FROM generations g
        WHERE g.created_at < $1
          AND NOT EXISTS (
              SELECT 1 FROM agent_messages m WHERE m.steps::text LIKE '%' || g.id || '%'
          )
        RETURNING g.service, g.outputs
        "#,
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(victim).collect())
}

fn victim((service, outputs): (String, serde_json::Value)) -> Victim {
    Victim {
        service,
        filenames: generations::output_filenames(&outputs),
    }
}

/// A protected row still occupies its rank: it is in the gallery, so it counts
/// against the per-service ceiling like any other.
async fn prune_to_max_per_service(
    pool: &PgPool,
    max_per_service: i64,
) -> sqlx::Result<Vec<Victim>> {
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        r#"
        DELETE FROM generations g
        WHERE g.id IN (
            SELECT id FROM (
                SELECT id, ROW_NUMBER() OVER (
                    PARTITION BY service ORDER BY created_at DESC
                ) AS rn
                FROM generations
            ) ranked WHERE rn > $1
        )
        AND NOT EXISTS (
            SELECT 1 FROM agent_messages m WHERE m.steps::text LIKE '%' || g.id || '%'
        )
        RETURNING g.service, g.outputs
        "#,
    )
    .bind(max_per_service)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(victim).collect())
}

async fn read_i64_setting(pool: &PgPool, key: &str, fallback: i64) -> i64 {
    settings_repo::get_setting(pool, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
        .unwrap_or(fallback)
}

async fn read_string_setting(pool: &PgPool, key: &str) -> Option<String> {
    settings_repo::get_setting(pool, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::OnceLock;
    use tokio::sync::Mutex as AsyncMutex;

    fn lock() -> &'static AsyncMutex<()> {
        static M: OnceLock<AsyncMutex<()>> = OnceLock::new();
        M.get_or_init(|| AsyncMutex::new(()))
    }

    /// No TRUNCATE: the rows are dated 90 days back and addressed by id, so the
    /// sweep under test cannot reach anything another module's tests just wrote.
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
        Some(pool)
    }

    async fn seed_old(pool: &PgPool, id: &str, service: &str) {
        sqlx::query(
            "INSERT INTO generations (id, service, endpoint, request_meta, outputs, created_at) \
             VALUES ($1, $2, 'make_video', '{}'::jsonb, $3, NOW() - INTERVAL '90 days')",
        )
        .bind(id)
        .bind(service)
        .bind(json!([{"filename": format!("{id}-0.webm"), "mime": "video/webm", "size_bytes": 1, "kind": "video"}]))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn exists(pool: &PgPool, id: &str) -> bool {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM generations WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
            > 0
    }

    /// The mirror image of `director_assets::stale_unreferenced`: an agent turn
    /// holds nothing but the generation id, so sweeping the row leaves the
    /// conversation's clip 404ing in a transcript the user can still see.
    #[tokio::test]
    async fn a_generation_an_agent_turn_still_shows_survives_the_sweep() {
        let _l = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let shown = uuid::Uuid::new_v4().to_string();
        let orphan = uuid::Uuid::new_v4().to_string();
        seed_old(&pool, &shown, "agent").await;
        seed_old(&pool, &orphan, "agent").await;

        let session = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO agent_sessions (id, title) VALUES ($1, 'retention test')")
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO agent_messages (id, session_id, seq, role, steps, status) \
             VALUES ($1, $2, 0, 'assistant', $3, 'complete')",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(session)
        .bind(json!([{
            "index": 0, "tool": "make_video", "args_summary": "", "state": "done", "detail": "",
            "outputs": [{
                "kind": "video",
                "url": format!("/api/v1/generations/{shown}/file/0"),
                "generation_id": shown,
                "mime": "video/webm",
            }],
        }]))
        .execute(&pool)
        .await
        .unwrap();

        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(30);
        let victims = prune_older_than(&pool, cutoff).await.unwrap();

        assert!(
            exists(&pool, &shown).await,
            "a generation an agent turn still renders was swept"
        );
        assert!(
            !exists(&pool, &orphan).await,
            "the unreferenced row survived"
        );
        assert!(
            victims
                .iter()
                .any(|v| v.filenames.contains(&format!("{orphan}-0.webm"))),
            "the sweep must report what it orphaned so the files get unlinked"
        );

        sqlx::query("DELETE FROM agent_sessions WHERE id = $1")
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM generations WHERE id = $1")
            .bind(&shown)
            .execute(&pool)
            .await
            .unwrap();
    }

    async fn seed_queue_job(pool: &PgPool, id: &str, state: &str, capture: &str) {
        sqlx::query(
            "INSERT INTO gpu_jobs (id, kind, payload, state, queue_rank, created_by) \
             VALUES ($1, 'upscale', $2, $3, 0, 'retention test')",
        )
        .bind(id)
        .bind(json!({ "request": { "source_path": format!("/var/lib/koblem/x/captures/{capture}") } }))
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }

    fn queue_files(tag: &str) -> (QueueFiles, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("koblem-retention-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (QueueFiles::open(&root, &root.join("captures")), root)
    }

    fn seed_files(files: &QueueFiles, id: &str, capture: &str) {
        std::fs::create_dir_all(files.work_dir(id)).unwrap();
        std::fs::write(files.work_dir(id).join("seg0000.mp4"), b"x").unwrap();
        std::fs::write(files.clip_path(id), b"x").unwrap();
        std::fs::write(files.capture_path(capture, "mp4"), b"x").unwrap();
    }

    /// 🔴 The rule the age sweep cannot express on its own. A queued job can wait eight hours, and
    /// its captured source is reachable from nothing but the row — an unlink here is a job that
    /// dies at dispatch with its input gone. Age is neutralised so liveness is the only variable.
    #[tokio::test]
    async fn a_live_jobs_working_files_survive_however_old_they_are() {
        let Some((pool, _g)) = crate::repositories::test_pool("gpu_jobs").await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let (files, root) = queue_files("live");
        seed_queue_job(&pool, "live", "queued", "up_live.mp4").await;
        seed_queue_job(&pool, "dead", "cancelled", "up_dead.mp4").await;
        seed_files(&files, "live", "up_live");
        seed_files(&files, "dead", "up_dead");

        sweep_queue_files(&pool, &files, SystemTime::now() + Duration::from_secs(60)).await;

        assert!(
            files.capture_path("up_live", "mp4").exists(),
            "a queued job's captured source was swept out from under it"
        );
        assert!(
            files.work_dir("live").exists(),
            "the delivered segments are a live job's whole resume state"
        );
        assert!(files.clip_path("live").exists());
        assert!(!files.capture_path("up_dead", "mp4").exists());
        assert!(!files.work_dir("dead").exists());
        assert!(!files.clip_path("dead").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The grace still covers a file no row names at all: `/upload` captures before the job that
    /// will reference it is enqueued, and an upload nobody submits is exactly the leak this sweeps.
    #[tokio::test]
    async fn a_fresh_unreferenced_capture_is_kept_until_the_grace_expires() {
        let Some((pool, _g)) = crate::repositories::test_pool("gpu_jobs").await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let (files, root) = queue_files("grace");
        let orphan = files.capture_path("up_orphan", "mp4");
        std::fs::write(&orphan, b"x").unwrap();

        sweep_queue_files(&pool, &files, SystemTime::now() - Duration::from_secs(60)).await;
        assert!(orphan.exists(), "swept between the upload and the enqueue");

        sweep_queue_files(&pool, &files, SystemTime::now() + Duration::from_secs(60)).await;
        assert!(!orphan.exists(), "an abandoned upload is the leak");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Ported from the repository primitive this policy layer replaced: the
    /// ceiling is counted per service, not across the whole gallery.
    #[tokio::test]
    async fn the_entry_ceiling_is_counted_per_service() {
        let _l = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        sqlx::query("DELETE FROM generations")
            .execute(&pool)
            .await
            .unwrap();
        for _ in 0..5 {
            seed_old(&pool, &uuid::Uuid::new_v4().to_string(), "images").await;
            seed_old(&pool, &uuid::Uuid::new_v4().to_string(), "music").await;
        }

        let victims = prune_to_max_per_service(&pool, 2).await.unwrap();
        assert_eq!(victims.len(), 6, "three evicted per service");
        for service in ["images", "music"] {
            let left: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM generations WHERE service = $1")
                    .bind(service)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(left, 2, "{service}");
        }
    }
}
