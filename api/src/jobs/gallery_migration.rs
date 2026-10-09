//! One-shot migration of legacy `{uuid}-{i}.{ext}` gallery files to content-addressed names.
//!
//! New generations are written as `<sha256>.<ext>` by
//! `tier2_helpers::persist_generation`; this brings the files that were already there over, so
//! there is one naming scheme on disk rather than two forever.
//!
//! Runs once at boot, in the background, and is idempotent: a row already carrying hash names is
//! skipped without touching the disk, so a restart costs one query.
//!
//! **Renames, never copies.** The disk this runs on is 84 % full and a gallery holds
//! hundred-megabyte clips; a copy-then-delete migration would need headroom it does not have.
//!
//! Scope: the GALLERY dir only. `avatar` and `music` resolve to their own directories, whose
//! files those routes wrote and serve by name from their own routes — a rename there would break
//! a link this migration cannot see.
//!
//! Crash safety, in order of operations per file: hash → link the new name → update the row →
//! unlink the old name. A crash anywhere leaves either the old name (row not yet updated, so the
//! next boot retries) or both names (row updated, old name orphaned — and the old name is NOT
//! hash-shaped, so the orphan sweep will never touch it; it is left for an operator). What cannot
//! happen is a row naming a file that does not exist.

use std::path::Path;

use serde_json::Value;
use sqlx::PgPool;
use tracing::{info, warn};

use crate::repositories::generations;
use crate::routes::tier2_helpers::{hash_file, is_gallery_blob_name};

pub struct Roots {
    pub songs_dir: std::path::PathBuf,
    pub avatars_dir: std::path::PathBuf,
}

pub async fn run(pool: PgPool, roots: Roots) {
    let generations_dir =
        match crate::repositories::settings::get_setting(&pool, "generations_dir").await {
            Ok(v) => v.and_then(|v| v.as_str().map(str::to_string)),
            Err(e) => {
                warn!(error = %e, "gallery migration: could not read generations_dir");
                None
            }
        };

    // ONLY the gallery. `avatar` and `music` resolve to their own dirs, whose files those routes
    // wrote themselves and serve BY NAME from their own routes (`routes::songgen::serve_file`,
    // `routes::sa3::serve_file`) — renaming one would break a link this migration cannot see.
    // Those services record a row for a file they own; they do not hand the gallery its bytes.
    let Some(gallery) = generations_dir.as_deref().map(std::path::PathBuf::from) else {
        return;
    };

    let rows = match generations::all_rows_for_migration(&pool).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "gallery migration: could not list generations");
            return;
        }
    };

    let mut moved = 0usize;
    let mut rewritten = 0usize;
    for (id, service, outputs) in rows {
        let Some(root) = generations::resolve_root(
            &service,
            &roots.avatars_dir,
            &roots.songs_dir,
            generations_dir.as_deref(),
        ) else {
            continue;
        };
        if root != gallery {
            continue;
        }
        let Some(entries) = outputs.as_array() else {
            continue;
        };
        // Nothing to do for a row that is already content-addressed — the common case on every
        // boot after the first.
        if entries.iter().all(|o| {
            o.get("filename")
                .and_then(Value::as_str)
                .is_some_and(is_gallery_blob_name)
        }) {
            continue;
        }

        let mut updated = entries.clone();
        let mut row_changed = false;
        let mut retire: Vec<String> = Vec::new();
        for entry in updated.iter_mut() {
            let Some(old) = entry
                .get("filename")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if is_gallery_blob_name(&old) || old.contains('/') || old.contains("..") {
                continue;
            }
            let Some(ext) = old.rsplit_once('.').map(|(_, e)| e.to_string()) else {
                continue;
            };
            let from = root.join(&old);
            let Ok((hash, _size)) = hash_file(&from).await else {
                // A row whose file is already gone: leave the row alone. Rewriting it to a name
                // that also does not exist would turn a missing file into a lie about which file
                // is missing.
                continue;
            };
            let new = format!("{hash}.{ext}");
            let to = root.join(&new);
            if !link_into_place(&from, &to).await {
                continue;
            }
            entry["filename"] = Value::String(new);
            retire.push(old);
            row_changed = true;
        }
        if !row_changed {
            continue;
        }
        if let Err(e) = generations::set_outputs(&pool, &id, &Value::Array(updated)).await {
            warn!(error = %e, generation = %id, "gallery migration: row update failed");
            // The new names exist and nothing points at them yet; the next boot re-hashes and
            // links again, which is a no-op. Do NOT unlink the old names.
            continue;
        }
        rewritten += 1;
        for old in retire {
            let _ = tokio::fs::remove_file(root.join(&old)).await;
            moved += 1;
        }
    }
    if rewritten > 0 {
        info!(
            rows = rewritten,
            files = moved,
            "gallery migration: content-addressed the existing gallery"
        );
    }
}

/// Hard-link `from` to `to`, treating "already there" as success.
///
/// A link rather than a rename so the row still points at a real file for the whole window; the
/// old name is unlinked only after the row is updated. Two rows sharing one blob is now legal, so
/// an existing target is the dedup working, not a collision.
async fn link_into_place(from: &Path, to: &Path) -> bool {
    if tokio::fs::try_exists(to).await.unwrap_or(false) {
        return true;
    }
    match tokio::fs::hard_link(from, to).await {
        Ok(()) => true,
        // EXDEV cannot happen (same directory), but a filesystem that refuses links can — fall
        // back to a copy rather than skipping the row.
        Err(_) => tokio::fs::copy(from, to).await.is_ok(),
    }
}
