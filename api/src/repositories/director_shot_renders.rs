//! Durable per-project Director renders, in two quality tiers — `director_shot_renders`.
//!
//! Rows point at webm files copied OUT of the TTL'd LTX job store into the owned,
//! non-swept `director-renders/<project_id>/` dir, so a saved project's rendered
//! shots survive the job store's 1h sweep and a koblem restart. `tier` is `proxy`
//! (cheap preview stream) or `final`; `shot_index = -1` is the stitched final, `>= 0`
//! is the prompt-filtered shot index. The two tiers are independent chains.
//!
//! Runtime sqlx queries (no `query!` macros) so the sqlx-offline cache doesn't
//! need regenerating on a migration touch — same convention as
//! `repositories/director_projects.rs`.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

use super::Result;

/// One persisted render clip for a project + tier — i.e. one TAKE of one shot.
#[derive(Debug, Clone, Serialize)]
pub struct ShotRenderRow {
    pub tier: String,
    pub shot_index: i32,
    pub prompt: String,
    pub file_name: String,
    pub content_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Display ordinal of this take within its shot (0 = the first take).
    pub variant: i32,
    /// True for the take the editor plays and a rebuilt cut concatenates. Exactly one per shot.
    pub selected: bool,
    /// The koblem job that produced this take. Empty on rows written before takes existed.
    pub source_job_id: String,
    /// The seed this take was rendered with, when the caller knew it.
    pub seed: Option<i64>,
    /// The vision critic's verdict on this take, if it has been judged.
    pub critic: Option<serde_json::Value>,
    /// The engine chain whose banked latents this take came out of — the resume target a retake of
    /// this shot must reference. NULL on a pre-`0027` row.
    pub latent_job_id: Option<String>,
    /// This take's own entry in the engine's latent bank, `"<engine job id>:<unit index>"`.
    ///
    /// 🔴 Not a second spelling of [`latent_job_id`](Self::latent_job_id) — see `0038` for why the
    /// two must not be conflated. That one names a JOB whose banked prefix a retake RESUMES, and is
    /// the same value on every row of a tier. This one names ONE unit's entry, differs per row, and
    /// is the record that the engine really banked this take rather than merely delivering it.
    ///
    /// Written on H3 (a generation's entry, which the next generation's `continuation.from` pins to)
    /// and on `ltx25` (a segment's entry, which a resume or a retake reaches through the JOB half —
    /// the shim composes the key itself and 400s on one it cannot resolve, so a key is never minted
    /// client-side). NULL for a C++ ltx-video take, for the stitched cut, and for anything written
    /// before `0038`.
    pub bank_key: Option<String>,
    /// Which engine's store [`latent_job_id`](Self::latent_job_id) and [`bank_key`](Self::bank_key)
    /// address. `None` on a row this migration could not place — see `0039`, and note that nothing
    /// may name a chain it cannot place.
    pub engine: Option<String>,
}

type RowTuple = (
    String,
    i32,
    String,
    String,
    String,
    OffsetDateTime,
    i32,
    bool,
    String,
    Option<i64>,
    Option<serde_json::Value>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The column list every read below shares, in `RowTuple` order.
const COLS: &str = "tier, shot_index, prompt, file_name, content_type, created_at, \
                    variant, selected, source_job_id, seed, critic, latent_job_id, bank_key, engine";

pub struct ShotRenderUpsert<'a> {
    pub project_id: &'a str,
    pub tier: &'a str,
    pub shot_index: i32,
    pub prompt: &'a str,
    pub file_name: &'a str,
    pub content_type: &'a str,
    pub latent_job_id: Option<&'a str>,
    /// The bank key the poll body reported for the unit this take IS (H3 and `ltx25`). `None` on
    /// every C++ ltx-video write and on the stitched cut — and `None` on a re-harvest that no longer
    /// has the key, which is why [`upsert`] COALESCEs rather than overwriting.
    pub bank_key: Option<&'a str>,
    /// The engine that produced this take, and therefore the store its two bank ids address.
    /// `None` where no store is addressed at all — a rebuilt cut is an `ffmpeg` concat of takes,
    /// not a render, and names neither a chain nor a bank entry (`0039`).
    pub engine: Option<&'a str>,
    /// The koblem job producing this take. Re-harvesting the same job updates that take in place;
    /// a different job (i.e. a retake) becomes a NEW take of the same shot.
    pub source_job_id: &'a str,
    /// Take ordinal. Callers get this from [`next_variant`] before naming the file.
    pub variant: i32,
    pub seed: Option<i64>,
}

fn to_row(r: RowTuple) -> ShotRenderRow {
    ShotRenderRow {
        tier: r.0,
        shot_index: r.1,
        prompt: r.2,
        file_name: r.3,
        content_type: r.4,
        created_at: r.5,
        variant: r.6,
        selected: r.7,
        source_job_id: r.8,
        seed: r.9,
        critic: r.10,
        latent_job_id: r.11,
        bank_key: r.12,
        engine: r.13,
    }
}

/// The take ordinal a new take of this shot should get, and whether this producing job ALREADY has
/// a take here (in which case that take's variant + file name are reused rather than a new take
/// being minted — the restart-reattach path re-harvests the same job and must stay idempotent).
pub async fn next_variant(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    shot_index: i32,
    source_job_id: &str,
) -> Result<(i32, Option<String>)> {
    let existing: Option<(i32, String)> = sqlx::query_as(
        r#"
        SELECT variant, file_name
        FROM director_shot_renders
        WHERE project_id = $1 AND tier = $2 AND shot_index = $3 AND source_job_id = $4
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .bind(source_job_id)
    .fetch_optional(pool)
    .await?;
    if let Some((variant, file_name)) = existing {
        return Ok((variant, Some(file_name)));
    }
    let next: Option<(Option<i32>,)> = sqlx::query_as(
        r#"
        SELECT MAX(variant) + 1
        FROM director_shot_renders
        WHERE project_id = $1 AND tier = $2 AND shot_index = $3
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .fetch_optional(pool)
    .await?;
    Ok((next.and_then(|r| r.0).unwrap_or(0), None))
}

/// Record one take and make it the selected one, keyed by its producing job so a re-harvest of the
/// same job updates in place. Older takes of the same shot are kept — that is the whole point —
/// but deselected, since at most one take per shot may be selected.
pub async fn upsert(pool: &PgPool, r: ShotRenderUpsert<'_>) -> Result<()> {
    let ShotRenderUpsert {
        project_id,
        tier,
        shot_index,
        prompt,
        file_name,
        content_type,
        latent_job_id,
        bank_key,
        engine,
        source_job_id,
        variant,
        seed,
    } = r;
    let mut tx = pool.begin().await?;
    // Clear first: the "one selected take per shot" unique index would otherwise reject the insert.
    sqlx::query(
        r#"
        UPDATE director_shot_renders SET selected = FALSE
        WHERE project_id = $1 AND tier = $2 AND shot_index = $3 AND selected
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO director_shot_renders
            (project_id, tier, shot_index, prompt, file_name, content_type, latent_job_id,
             source_job_id, variant, seed, bank_key, engine, selected)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, TRUE)
        ON CONFLICT (project_id, tier, shot_index, source_job_id)
        DO UPDATE SET prompt = EXCLUDED.prompt,
                      file_name = EXCLUDED.file_name,
                      content_type = EXCLUDED.content_type,
                      latent_job_id = EXCLUDED.latent_job_id,
                      engine = EXCLUDED.engine,
                      seed = COALESCE(EXCLUDED.seed, director_shot_renders.seed),
                      -- COALESCE, not overwrite: the restart-reattach path re-harvests the same
                      -- job off its sidecar, and the poll body that carried the bank keys is long
                      -- gone by then. Overwriting would blank the pin a later retake needs and
                      -- turn a recoverable seam into a permanent one.
                      bank_key = COALESCE(EXCLUDED.bank_key, director_shot_renders.bank_key),
                      selected = TRUE,
                      created_at = NOW()
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .bind(prompt)
    .bind(file_name)
    .bind(content_type)
    .bind(latent_job_id)
    .bind(source_job_id)
    .bind(variant)
    .bind(seed)
    .bind(bank_key)
    .bind(engine)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Every take of one shot, oldest first.
pub async fn list_takes(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    shot_index: i32,
) -> Result<Vec<ShotRenderRow>> {
    let rows: Vec<RowTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM director_shot_renders \
         WHERE project_id = $1 AND tier = $2 AND shot_index = $3 ORDER BY variant ASC"
    )))
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(to_row).collect())
}

/// Fetch one specific take.
pub async fn get_take(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    shot_index: i32,
    variant: i32,
) -> Result<Option<ShotRenderRow>> {
    let row: Option<RowTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM director_shot_renders \
         WHERE project_id = $1 AND tier = $2 AND shot_index = $3 AND variant = $4"
    )))
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .bind(variant)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(to_row))
}

/// Make one take the selected one. Returns false when that take does not exist, so a caller can
/// 404 rather than silently deselecting the shot.
pub async fn select_take(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    shot_index: i32,
    variant: i32,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        UPDATE director_shot_renders SET selected = FALSE
        WHERE project_id = $1 AND tier = $2 AND shot_index = $3 AND selected AND variant <> $4
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .bind(variant)
    .execute(&mut *tx)
    .await?;
    let res = sqlx::query(
        r#"
        UPDATE director_shot_renders SET selected = TRUE
        WHERE project_id = $1 AND tier = $2 AND shot_index = $3 AND variant = $4
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .bind(variant)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(res.rows_affected() > 0)
}

/// Attach the vision critic's verdict to one take.
pub async fn set_critic(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    shot_index: i32,
    variant: i32,
    critic: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE director_shot_renders SET critic = $5
        WHERE project_id = $1 AND tier = $2 AND shot_index = $3 AND variant = $4
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .bind(variant)
    .bind(critic)
    .execute(pool)
    .await?;
    Ok(())
}

/// Each shot's SELECTED take's engine bank, as `(shot_index, latent_job_id)` ordered by shot.
///
/// This is what "render again from the takes I picked" needs. A retake used to send ONE
/// `resume_job_id` for the whole chain, so every shot it did not re-render came out of that single
/// bank — which is the bank of whichever take was rolled LAST, not the one the user selected. Shots
/// whose selected take predates the `latent_job_id` column (a pre-`0027` row) are omitted, and the
/// caller leaves those on the chain default rather than guessing.
///
/// 🔴 `engine` is the RENDERING engine, not the project's engine at some other time: a chain banked
/// under a different one is a 404 at the far end and is dropped here rather than named (`0039`).
pub async fn selected_bank_ids(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    engine: &str,
) -> Result<Vec<(i32, String)>> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        r#"
        SELECT shot_index, latent_job_id
        FROM director_shot_renders
        WHERE project_id = $1 AND tier = $2 AND shot_index >= 0
          AND selected AND latent_job_id IS NOT NULL AND engine = $3
        ORDER BY shot_index ASC
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(engine)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The MiniMax-H3 latent-bank key of one generation's SELECTED take — what a retake of the NEXT
/// generation points `continuation.from` at.
///
/// 🔴 Selected, never newest. `variant` exists so takes coexist (`0029`), and the cut plays the
/// selected one; resolving "the latest row" would pin the retake to a take the user rejected, so
/// the new generation would continue from a version of the shot that appears nowhere in the
/// timeline — visibly wrong, and wrong in a way that looks like a model failure rather than a
/// lookup failure. The partial unique index makes "selected" single-valued, so this is a `WHERE`
/// and not an `ORDER BY` preference — a preference would quietly hand back an unselected take as
/// soon as another filter (the engine, `0039`) removed the selected one from the candidates.
///
/// `None` means there is genuinely nothing to pin to — no take, a take from before `0038` / from a
/// run whose poll body never reported a key, or a take banked by ANOTHER engine (`0039`: both LTX-2.5
/// and H3 publish a `"<job>:<index>"` key, and each one is a hard failure in the other's bank). The
/// caller drops the continuation and SAYS SO; it must not invent one.
pub async fn selected_bank_key(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    shot_index: i32,
    engine: &str,
) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        r#"
        SELECT bank_key
        FROM director_shot_renders
        WHERE project_id = $1 AND tier = $2 AND shot_index = $3 AND engine = $4 AND selected
        LIMIT 1
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .bind(engine)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .and_then(|r| r.0)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty()))
}

/// One banked engine chain a project's takes point at: the store it lives in, and its id there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatentChain {
    /// Which engine holds it. `None` on a row `0039` could not place — a chain nothing may NAME,
    /// though a delete may still guess at it, since asking the wrong service is a 404 and asking
    /// none of them leaks a directory forever.
    pub engine: Option<String>,
    pub id: String,
}

impl LatentChain {
    /// Is this the same engine's store the caller is about to talk to?
    pub fn is(&self, engine: &str) -> bool {
        self.engine.as_deref() == Some(engine)
    }
}

/// Distinct banked chains backing a project (both tiers) — the persist-store dirs to ask the engine
/// to delete when the project is removed. NULL ids excluded.
pub async fn latent_chains_by_project(pool: &PgPool, project_id: &str) -> Result<Vec<LatentChain>> {
    let rows: Vec<(Option<String>, String)> = sqlx::query_as(
        r#"
        SELECT DISTINCT engine, latent_job_id
        FROM director_shot_renders
        WHERE project_id = $1 AND latent_job_id IS NOT NULL
        "#,
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(engine, id)| LatentChain { engine, id })
        .collect())
}

/// Distinct banked chains backing one tier of a project, NEWEST FIRST. NULL ids excluded.
///
/// Two callers with opposite needs share this, which is why it carries the engine rather than
/// filtering on one: a tier wipe deletes EVERY chain (order irrelevant, and it must reach the ones a
/// previous engine banked or they are orphaned in a store nothing sweeps), while
/// [`resolve_resume_job_id_for_tier`](crate::routes::ltx_director::resolve_resume_job_id_for_tier)
/// takes the first chain OF THE RENDERING ENGINE as the one to resume. Without the explicit ordering
/// that pick was whatever Postgres returned last.
pub async fn latent_chains_by_project_tier(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
) -> Result<Vec<LatentChain>> {
    let rows: Vec<(Option<String>, String)> = sqlx::query_as(
        r#"
        SELECT engine, latent_job_id
        FROM director_shot_renders
        WHERE project_id = $1 AND tier = $2 AND latent_job_id IS NOT NULL
        GROUP BY engine, latent_job_id
        ORDER BY MAX(created_at) DESC
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(engine, id)| LatentChain { engine, id })
        .collect())
}

/// The chain the tier's FIRST shot was banked out of — the only id a resume may name.
///
/// 🔴 NOT the newest one [`latent_job_ids_by_project_tier`] hands back. The engine resolves a
/// `resume_job_id` by composing `"<job>:<segment>"` and walking forward from segment 0, and a
/// continue banks ONLY the shots it rendered under a fresh id: render five shots as job A, continue
/// two more as job B, adopt B, and the next continue asks B for segment 0 and gets a hard 400.
/// LTX-2.3 masked it by aliasing each fresh id onto the bank it continued; `ltx25`'s bank is a flat
/// store with no aliases. The web client picks shot 0's row for exactly this reason
/// (`renders.ts::bankedResumeTarget`) — this is the server half of the same rule.
///
/// 🔴 WHICH TAKE: shot 0's SELECTED one, and only if THIS engine banked it. Stated as a `WHERE`
/// rather than as an `ORDER BY` preference so it is the same rule the client applies —
/// `renders.ts::bankedResumeTarget` reads the single row `GET …/renders` returns per (tier, shot),
/// which IS the selected take, and the two are documented as one rule. Ordering by `selected DESC`
/// let an unselected take win as soon as the engine filter narrowed the candidates, which is the
/// "it forgot the take I picked" failure `0029` exists to prevent.
///
/// `None` where shot 0 predates the `latent_job_id` column, was banked by another engine (`0039`),
/// or has no selected take at all (a hand-edited database) — and the caller then falls back to the
/// newest-first chain list, which is filtered by engine the same way.
pub async fn first_shot_latent_job_id(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    engine: &str,
) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT latent_job_id
        FROM director_shot_renders
        WHERE project_id = $1 AND tier = $2 AND shot_index = 0 AND latent_job_id IS NOT NULL
          AND engine = $3 AND selected
        LIMIT 1
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(engine)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Every persisted take for a project across BOTH tiers, ordered tier, shot, take.
pub async fn list_by_project(pool: &PgPool, project_id: &str) -> Result<Vec<ShotRenderRow>> {
    let rows: Vec<RowTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM director_shot_renders \
         WHERE project_id = $1 ORDER BY tier ASC, shot_index ASC, variant ASC"
    )))
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(to_row).collect())
}

/// The SELECTED take of one shot — what the editor plays and what a rebuilt cut uses. Falls back to
/// the newest take if nothing is selected (rows written before takes existed are all selected, so
/// this only matters for a hand-edited database).
pub async fn get(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    shot_index: i32,
) -> Result<Option<ShotRenderRow>> {
    let row: Option<RowTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM director_shot_renders \
         WHERE project_id = $1 AND tier = $2 AND shot_index = $3 \
         ORDER BY selected DESC, variant DESC LIMIT 1"
    )))
    .bind(project_id)
    .bind(tier)
    .bind(shot_index)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(to_row))
}

/// The selected take of every rendered shot of one tier, in shot order. `shot_index = -1` (the
/// stitched cut) is excluded — this is the per-shot list a rebuilt cut concatenates.
pub async fn selected_shots(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
) -> Result<Vec<ShotRenderRow>> {
    let rows: Vec<RowTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM director_shot_renders \
         WHERE project_id = $1 AND tier = $2 AND shot_index >= 0 AND selected \
         ORDER BY shot_index ASC"
    )))
    .bind(project_id)
    .bind(tier)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(to_row).collect())
}

/// Drop every take of one tier EXCEPT those produced by `keep_source_job_id`, returning the file
/// names that are now unreferenced (the caller unlinks them).
///
/// This is the SWAP half of a full run's replace. The wipe used to run first — files unlinked,
/// engine latents deleted, rows dropped — and only then did the copies start, each `continue`ing on
/// error. A failure anywhere in the middle left the project with no renders at all and the previous
/// latents already gone from the engine's never-swept store, i.e. nothing to fall back to and
/// nothing to resume from. Writing the new set first and dropping the old only once it is in place
/// keeps a mid-harvest failure to "some new takes alongside the old ones".
pub async fn delete_by_project_tier_except(
    pool: &PgPool,
    project_id: &str,
    tier: &str,
    keep_source_job_id: &str,
) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        DELETE FROM director_shot_renders
        WHERE project_id = $1 AND tier = $2 AND source_job_id <> $3
        RETURNING file_name
        "#,
    )
    .bind(project_id)
    .bind(tier)
    .bind(keep_source_job_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::director_projects;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use time::Duration;

    use crate::repositories::test_db_lock as lock;

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

    async fn seed_project(pool: &PgPool) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        director_projects::create(pool, &id, "t", &json!({}), &json!({}))
            .await
            .unwrap();
        id
    }

    /// The tier's chain ids, newest first — what a resume walks before it looks at shot 0.
    async fn chain_ids(pool: &PgPool, project_id: &str, tier: &str) -> Vec<String> {
        latent_chains_by_project_tier(pool, project_id, tier)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    /// One take to bank. `engine` defaults to `ltx` because that is what every ordering test here
    /// is about; the cross-engine tests set it.
    struct Banked<'a> {
        tier: &'a str,
        shot_index: i32,
        job: &'a str,
        latent: Option<&'a str>,
        bank_key: Option<&'a str>,
        engine: &'a str,
        age_secs: i64,
    }

    /// Bank one take and back-date it, so ordering assertions don't depend on insertion speed.
    async fn bank(
        pool: &PgPool,
        project_id: &str,
        tier: &str,
        shot_index: i32,
        job: &str,
        latent: Option<&str>,
        age_secs: i64,
    ) {
        bank_take(
            pool, project_id, tier, shot_index, job, latent, None, age_secs,
        )
        .await
    }

    /// The same, carrying an H3 latent-bank key — the pin a retake of the NEXT generation resolves.
    #[allow(clippy::too_many_arguments)]
    async fn bank_take(
        pool: &PgPool,
        project_id: &str,
        tier: &str,
        shot_index: i32,
        job: &str,
        latent: Option<&str>,
        bank_key: Option<&str>,
        age_secs: i64,
    ) {
        bank_row(
            pool,
            project_id,
            Banked {
                tier,
                shot_index,
                job,
                latent,
                bank_key,
                engine: "ltx",
                age_secs,
            },
        )
        .await
    }

    async fn bank_row(pool: &PgPool, project_id: &str, b: Banked<'_>) {
        let Banked {
            tier,
            shot_index,
            job,
            latent,
            bank_key,
            engine,
            age_secs,
        } = b;
        let (variant, _) = next_variant(pool, project_id, tier, shot_index, job)
            .await
            .unwrap();
        upsert(
            pool,
            ShotRenderUpsert {
                project_id,
                tier,
                shot_index,
                prompt: "p",
                file_name: "f.webm",
                content_type: "video/webm",
                latent_job_id: latent,
                bank_key,
                engine: Some(engine),
                source_job_id: job,
                variant,
                seed: None,
            },
        )
        .await
        .unwrap();
        let at = OffsetDateTime::now_utc() - Duration::seconds(age_secs);
        sqlx::query(
            "UPDATE director_shot_renders SET created_at = $4
             WHERE project_id = $1 AND tier = $2 AND source_job_id = $3",
        )
        .bind(project_id)
        .bind(tier)
        .bind(job)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
    }

    /// `resolve_resume_job_id_for_tier` takes the FIRST id this returns as the chain to resume, so
    /// "which id" must be newest-wins and not whatever Postgres happened to hand back last.
    #[tokio::test]
    async fn latent_job_ids_by_tier_are_newest_first() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let project = seed_project(&pool).await;

        bank(&pool, &project, "final", 0, "job-old", Some("lat-old"), 900).await;
        bank(&pool, &project, "final", 1, "job-mid", Some("lat-mid"), 600).await;
        bank(&pool, &project, "final", 2, "job-new", Some("lat-new"), 5).await;
        // Same tier, no latents: must not displace a real id or appear as an empty entry.
        bank(&pool, &project, "final", 3, "job-null", None, 1).await;
        // The other tier is an independent chain.
        bank(&pool, &project, "proxy", 0, "job-px", Some("lat-px"), 30).await;

        assert_eq!(
            chain_ids(&pool, &project, "final").await,
            vec![
                "lat-new".to_string(),
                "lat-mid".to_string(),
                "lat-old".to_string()
            ]
        );
        assert_eq!(
            chain_ids(&pool, &project, "proxy").await,
            vec!["lat-px".to_string()]
        );
    }

    /// A continue mints a FRESH chain id and banks only the shots it rendered, so the newest id
    /// cannot resolve segment 0 and a resume that names it is a hard 400. Shot 0's own chain is the
    /// answer, and it is the oldest one on a continued project — the opposite of newest-wins.
    #[tokio::test]
    async fn the_first_shot_names_the_chain_a_resume_can_use() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let project = seed_project(&pool).await;

        // Job A rendered shots 0-1; job B continued with 2, under its own bank.
        bank(&pool, &project, "final", 0, "job-a", Some("lat-a"), 900).await;
        bank(&pool, &project, "final", 1, "job-a", Some("lat-a"), 900).await;
        bank(&pool, &project, "final", 2, "job-b", Some("lat-b"), 30).await;

        assert_eq!(
            chain_ids(&pool, &project, "final").await.first().cloned(),
            Some("lat-b".to_string()),
            "newest-first still reports the continue's own chain"
        );
        assert_eq!(
            first_shot_latent_job_id(&pool, &project, "final", "ltx")
                .await
                .unwrap(),
            Some("lat-a".to_string())
        );
        // The other tier is an independent chain and must not borrow this one's answer.
        assert_eq!(
            first_shot_latent_job_id(&pool, &project, "proxy", "ltx")
                .await
                .unwrap(),
            None
        );
    }

    /// A full run REPLACES its tier, and the replace has to happen after the new set is safely in
    /// place — the old order (unlink files, delete the engine latents, drop the rows, then start
    /// copying) left a mid-harvest failure with no renders at all and the previous latents already
    /// gone from a store that is never swept. So the swap keeps exactly what the new run wrote and
    /// hands back the files the caller now owes an unlink.
    #[tokio::test]
    async fn the_swap_keeps_the_new_run_and_hands_back_what_it_replaced() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let project = seed_project(&pool).await;

        bank(&pool, &project, "final", 0, "job-old", Some("lat-old"), 600).await;
        bank(&pool, &project, "final", 1, "job-old", Some("lat-old"), 600).await;
        bank(
            &pool,
            &project,
            "proxy",
            0,
            "job-proxy",
            Some("lat-px"),
            300,
        )
        .await;
        // The new run: a fresh take of shot 0, alongside the old one until the swap.
        bank(&pool, &project, "final", 0, "job-new", Some("lat-new"), 5).await;
        assert_eq!(
            list_takes(&pool, &project, "final", 0).await.unwrap().len(),
            2
        );

        let removed = delete_by_project_tier_except(&pool, &project, "final", "job-new")
            .await
            .unwrap();
        assert_eq!(removed.len(), 2, "both of the old run's takes");

        let kept = list_by_project(&pool, &project).await.unwrap();
        let finals: Vec<_> = kept.iter().filter(|r| r.tier == "final").collect();
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0].source_job_id, "job-new");
        assert!(
            finals[0].selected,
            "the surviving take is the one that plays"
        );
        // The other tier is an independent stream and must not be touched by a final-tier run.
        assert_eq!(kept.iter().filter(|r| r.tier == "proxy").count(), 1);
        // Only the replaced chain is now unreferenced — the caller deletes exactly that from the
        // engine, and never the bank the surviving rows point at.
        assert_eq!(
            chain_ids(&pool, &project, "final").await,
            vec!["lat-new".to_string()]
        );
    }

    /// The editor rehydrates its resume target from these rows, so `latent_job_id` has to survive
    /// the read — including as NULL, which is what a pre-0027 project looks like and which the
    /// client must read as "keep the value you already have", not "resume nothing".
    #[tokio::test]
    async fn rows_carry_latent_job_id_including_null() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let project = seed_project(&pool).await;

        bank(&pool, &project, "final", 0, "job-a", Some("lat-a"), 60).await;
        bank(&pool, &project, "final", 1, "job-b", None, 30).await;

        let rows = list_by_project(&pool, &project).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].latent_job_id.as_deref(), Some("lat-a"));
        assert_eq!(rows[1].latent_job_id, None);
    }

    /// 🔴 The `project_ltx_retake_destroyed_takes` lesson, applied to the H3 bank pin.
    ///
    /// A retake must ADD a take carrying its own key, never overwrite the one it replaced: the
    /// replaced take is still playable, still selectable, and — once reselected — still the thing
    /// the next generation continues from. If a retake clobbered the key in place, reselecting the
    /// earlier take would leave the timeline playing take 0 while every later generation pinned to
    /// take 1's latents, which is a mismatch nothing in the UI can show.
    #[tokio::test]
    async fn a_retake_adds_a_take_with_its_own_bank_key_and_destroys_nothing() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let project = seed_project(&pool).await;

        bank_take(
            &pool,
            &project,
            "final",
            1,
            "job-a",
            None,
            Some("h3_a:1"),
            600,
        )
        .await;
        // The retake: a NEW koblem job, so a new take of the same generation.
        bank_take(
            &pool,
            &project,
            "final",
            1,
            "job-b",
            None,
            Some("h3_b:1"),
            5,
        )
        .await;

        let takes = list_takes(&pool, &project, "final", 1).await.unwrap();
        assert_eq!(
            takes.len(),
            2,
            "the replaced take is still on disk and in the table"
        );
        assert_eq!(takes[0].variant, 0);
        assert_eq!(takes[0].bank_key.as_deref(), Some("h3_a:1"));
        assert_eq!(takes[1].variant, 1);
        assert_eq!(takes[1].bank_key.as_deref(), Some("h3_b:1"));
        assert!(!takes[0].selected && takes[1].selected);
    }

    /// A retake of generation N+1 pins to the take of generation N that the CUT PLAYS — the
    /// selected one — and follows the selection when it moves. Resolving "the newest row" instead
    /// would continue from a take the user rejected.
    #[tokio::test]
    async fn the_resolved_bank_key_follows_the_selected_take_not_the_newest() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let project = seed_project(&pool).await;

        bank_take(
            &pool,
            &project,
            "final",
            0,
            "job-a",
            None,
            Some("h3_a:0"),
            600,
        )
        .await;
        bank_take(
            &pool,
            &project,
            "final",
            0,
            "job-b",
            None,
            Some("h3_b:0"),
            5,
        )
        .await;
        // The newest take is selected by default, so that is the pin.
        assert_eq!(
            selected_bank_key(&pool, &project, "final", 0, "ltx")
                .await
                .unwrap(),
            Some("h3_b:0".to_string())
        );

        // The editor picks the FIRST take back. The pin must move with it.
        assert!(select_take(&pool, &project, "final", 0, 0).await.unwrap());
        assert_eq!(
            selected_bank_key(&pool, &project, "final", 0, "ltx")
                .await
                .unwrap(),
            Some("h3_a:0".to_string())
        );

        // Tiers are independent chains: a proxy pin must never be handed to a final retake, whose
        // generations were rendered at a different resolution (the shim refuses a cross-resolution
        // continuation outright, but only after the render has already been dispatched).
        assert_eq!(
            selected_bank_key(&pool, &project, "proxy", 0, "ltx")
                .await
                .unwrap(),
            None
        );
        // Nothing rendered = nothing to pin to. The caller drops the continuation and reports it.
        assert_eq!(
            selected_bank_key(&pool, &project, "final", 7, "ltx")
                .await
                .unwrap(),
            None
        );
    }

    /// 🔴 A PROJECT THAT CHANGED ENGINE MUST NOT BE ABLE TO NAME THE PREVIOUS ONE'S BANK.
    ///
    /// The two LTX deployments have separate job stores, so a 2.3 chain id sent to 2.5 is a 404 on
    /// every shot — and the editor's engine picker sits above a project that has already rendered,
    /// so the switch is one click. Nothing filtered these reads by engine, and all three of them
    /// feed a dispatch: `selected_bank_ids` stamps every shot's `bank_job_id`,
    /// `first_shot_latent_job_id` names the chain a resume reloads, and the chain list is the
    /// acceptance set for an explicit id.
    #[tokio::test]
    async fn a_chain_is_only_nameable_by_the_engine_that_banked_it() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let project = seed_project(&pool).await;

        for shot in 0..2 {
            bank_row(
                &pool,
                &project,
                Banked {
                    tier: "final",
                    shot_index: shot,
                    job: "job-23",
                    latent: Some("lat-23"),
                    bank_key: None,
                    engine: "ltx",
                    age_secs: 600,
                },
            )
            .await;
        }

        // On the engine that rendered them, every read answers.
        assert_eq!(
            selected_bank_ids(&pool, &project, "final", "ltx")
                .await
                .unwrap(),
            vec![(0, "lat-23".to_string()), (1, "lat-23".to_string())]
        );
        assert_eq!(
            first_shot_latent_job_id(&pool, &project, "final", "ltx")
                .await
                .unwrap(),
            Some("lat-23".to_string())
        );

        // Switch the project to LTX-2.5 and nothing may be resolved out of the 2.3 rows.
        assert!(
            selected_bank_ids(&pool, &project, "final", "ltx25")
                .await
                .unwrap()
                .is_empty(),
            "a 2.3 chain must not be stamped onto a 2.5 segment"
        );
        assert_eq!(
            first_shot_latent_job_id(&pool, &project, "final", "ltx25")
                .await
                .unwrap(),
            None
        );
        let chains = latent_chains_by_project_tier(&pool, &project, "final")
            .await
            .unwrap();
        assert!(!chains.iter().any(|c| c.is("ltx25")));
        // The wipe still sees it, though — the previous engine's persist dir is never auto-swept,
        // so a cleanup that could not name it would leak it forever.
        assert_eq!(
            chains,
            vec![LatentChain {
                engine: Some("ltx".to_string()),
                id: "lat-23".to_string()
            }]
        );

        // Render on 2.5 and its OWN chain is nameable — and only on 2.5.
        bank_row(
            &pool,
            &project,
            Banked {
                tier: "final",
                shot_index: 0,
                job: "job-25",
                latent: Some("lat-25"),
                bank_key: Some("lat-25:0"),
                engine: "ltx25",
                age_secs: 5,
            },
        )
        .await;
        assert_eq!(
            first_shot_latent_job_id(&pool, &project, "final", "ltx25")
                .await
                .unwrap(),
            Some("lat-25".to_string())
        );
        // The 2.5 take is the one the cut now plays, so 2.3 has no SELECTED take to reload — the
        // chain is still there for the wipe and for the newest-first fallback, but this answer is
        // about which take the timeline shows, not about which chains exist.
        assert_eq!(
            first_shot_latent_job_id(&pool, &project, "final", "ltx")
                .await
                .unwrap(),
            None
        );
        assert!(
            latent_chains_by_project_tier(&pool, &project, "final")
                .await
                .unwrap()
                .iter()
                .any(|c| c.id == "lat-23")
        );
        // Both engines publish a `"<job>:<index>"` bank key, so the H3 continuation lookup is the
        // same hazard read from the other side.
        assert_eq!(
            selected_bank_key(&pool, &project, "final", 0, "h3")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            selected_bank_key(&pool, &project, "final", 0, "ltx25")
                .await
                .unwrap(),
            Some("lat-25:0".to_string())
        );

        // Pick the 2.3 take back and the two swap over — nothing was deleted either way.
        assert!(select_take(&pool, &project, "final", 0, 0).await.unwrap());
        assert_eq!(
            first_shot_latent_job_id(&pool, &project, "final", "ltx")
                .await
                .unwrap(),
            Some("lat-23".to_string())
        );
        assert_eq!(
            first_shot_latent_job_id(&pool, &project, "final", "ltx25")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            selected_bank_key(&pool, &project, "final", 0, "ltx25")
                .await
                .unwrap(),
            None,
            "the pin follows the selection, and the selection is now a take 2.5 did not make"
        );
    }

    /// The restart-reattach path re-harvests a job off its sidecar, long after the poll body that
    /// carried the bank keys is gone. That re-harvest must not blank the key — a blanked key is
    /// indistinguishable from "this generation was never banked", and the retake would silently
    /// drop its continuation.
    #[tokio::test]
    async fn a_reharvest_without_the_keys_does_not_blank_them() {
        let _lock = lock().lock().await;
        let Some(pool) = test_pool().await else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let project = seed_project(&pool).await;

        bank_take(
            &pool,
            &project,
            "final",
            2,
            "job-a",
            None,
            Some("h3_a:2"),
            600,
        )
        .await;
        // Same producing job ⇒ the same take, updated in place (the idempotency key), this time
        // with nothing to say about the bank.
        bank_take(&pool, &project, "final", 2, "job-a", None, None, 5).await;

        let takes = list_takes(&pool, &project, "final", 2).await.unwrap();
        assert_eq!(
            takes.len(),
            1,
            "a re-harvest updates, it does not mint a take"
        );
        assert_eq!(takes[0].bank_key.as_deref(), Some("h3_a:2"));
    }
}
