//! Multi-GPU scheduler config — `gpu_cards` + `gpu_engine_config` tables.
//!
//! These two tables hold the load-bearing placement config the koblem layer
//! pushes into `kob-gpu-gate`'s `Placer` at startup and on every edit (so the
//! peak/priority table is a settings-page form field, not a recompile — see
//! HANDOFF-multi-gpu-scheduler.md §8b).
//!
//! Uses runtime sqlx queries (no `query!` macros) so the sqlx-offline cache
//! doesn't need regenerating — same convention as `settings.rs` /
//! `avatar_assets.rs`.

use kob_gpu_gate::{Card, EngineDesc, ServiceKind};
use koblem_client::types::{GpuCard, GpuEngineConfig};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::Result;

// ── gpu_cards ───────────────────────────────────────────────────────────────

/// List all cards, in display order.
pub async fn list_cards(pool: &PgPool) -> Result<Vec<GpuCard>> {
    let rows: Vec<(String, String, i32, bool, bool, i32, OffsetDateTime)> = sqlx::query_as(
        r#"
        SELECT uuid, label, total_mb, fast, enabled, sort_order, updated_at
        FROM gpu_cards
        ORDER BY sort_order, uuid
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(card_from_tuple).collect())
}

/// Set a card's enabled flag (the operator GPU on/off feature). Returns the
/// updated row, or `Ok(None)` if the uuid is unknown.
pub async fn set_card_enabled(pool: &PgPool, uuid: &str, enabled: bool) -> Result<Option<GpuCard>> {
    let row: Option<(String, String, i32, bool, bool, i32, OffsetDateTime)> = sqlx::query_as(
        r#"
        UPDATE gpu_cards SET enabled = $2, updated_at = NOW()
        WHERE uuid = $1
        RETURNING uuid, label, total_mb, fast, enabled, sort_order, updated_at
        "#,
    )
    .bind(uuid)
    .bind(enabled)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(card_from_tuple))
}

fn card_from_tuple(r: (String, String, i32, bool, bool, i32, OffsetDateTime)) -> GpuCard {
    GpuCard {
        uuid: r.0,
        label: r.1,
        total_mb: r.2,
        fast: r.3,
        enabled: r.4,
        sort_order: r.5,
        updated_at: r.6.format(&Rfc3339).ok(),
    }
}

/// Convert a card row into the gate's `Card`.
pub fn card_to_gate(r: &GpuCard) -> Card {
    Card {
        uuid: r.uuid.clone(),
        total_mb: r.total_mb.max(0) as u32,
        fast: r.fast,
        enabled: r.enabled,
    }
}

// ── gpu_engine_config ─────────────────────────────────────────────────────────

/// `(service, peak_mb, home_gpu, candidate_gpus, evictable_by, relocatable, rules_json, updated_at)`
/// as returned by the config queries.
type EngineConfigTuple = (
    String,
    i32,
    String,
    Value,
    Value,
    bool,
    String,
    OffsetDateTime,
);

/// List all per-service configs.
pub async fn list_engine_configs(pool: &PgPool) -> Result<Vec<GpuEngineConfig>> {
    let rows: Vec<EngineConfigTuple> = sqlx::query_as(
        r#"
        SELECT service, peak_mb, home_gpu, candidate_gpus, evictable_by, relocatable,
               rules_json, updated_at
        FROM gpu_engine_config
        ORDER BY service
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(engine_from_tuple).collect())
}

/// Upsert one service's config. `service` must be a valid `ServiceKind` name.
/// `evictable_by` is the named eviction set (`ServiceKind` name strings). The
/// numeric `priority`/`pinned` columns are left at their DB defaults on insert
/// and untouched on update — they no longer drive eviction.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_engine_config(
    pool: &PgPool,
    service: &str,
    peak_mb: i32,
    home_gpu: &str,
    candidate_gpus: &[String],
    evictable_by: &[String],
    relocatable: bool,
    rules_json: &str,
) -> Result<GpuEngineConfig> {
    let cands = Value::Array(
        candidate_gpus
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect(),
    );
    let evicts = Value::Array(
        evictable_by
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect(),
    );
    let row: EngineConfigTuple = sqlx::query_as(
        r#"
        INSERT INTO gpu_engine_config
            (service, peak_mb, home_gpu, candidate_gpus, evictable_by, relocatable, rules_json, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
        ON CONFLICT (service) DO UPDATE SET
            peak_mb = $2, home_gpu = $3, candidate_gpus = $4,
            evictable_by = $5, relocatable = $6, rules_json = $7, updated_at = NOW()
        RETURNING service, peak_mb, home_gpu, candidate_gpus, evictable_by, relocatable,
                  rules_json, updated_at
        "#,
        )
        .bind(service)
        .bind(peak_mb)
        .bind(home_gpu)
        .bind(&cands)
        .bind(&evicts)
        .bind(relocatable)
        .bind(rules_json)
        .fetch_one(pool)
        .await?;
    Ok(engine_from_tuple(row))
}

fn json_str_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn engine_from_tuple(r: EngineConfigTuple) -> GpuEngineConfig {
    GpuEngineConfig {
        service: r.0,
        peak_mb: r.1,
        home_gpu: r.2,
        candidate_gpus: json_str_array(&r.3),
        evictable_by: json_str_array(&r.4),
        relocatable: r.5,
        rules_json: r.6,
        updated_at: r.7.format(&Rfc3339).ok(),
    }
}

/// Convert a config row into the gate's `(ServiceKind, EngineDesc)`. Returns
/// `None` if the stored `service` string isn't a known `ServiceKind`.
pub fn engine_to_gate(r: &GpuEngineConfig) -> Option<(ServiceKind, EngineDesc)> {
    let kind = ServiceKind::parse(&r.service)?;
    // Map the named eviction set; silently drop any unknown service name.
    let evictable_by = r
        .evictable_by
        .iter()
        .filter_map(|s| ServiceKind::parse(s))
        .collect();
    Some((
        kind,
        EngineDesc {
            peak_mb: r.peak_mb.max(0) as u32,
            home_gpu: r.home_gpu.clone(),
            candidate_gpus: r.candidate_gpus.clone(),
            evictable_by,
            relocatable: r.relocatable,
            rules_json: r.rules_json.clone(),
        },
    ))
}

/// Load both tables and push them into the gate's placement layer. Called at
/// startup and after any edit so the live gate always reflects the DB.
pub async fn load_into_gate(pool: &PgPool, gpu: &kob_gpu_gate::GpuLock) -> Result<()> {
    let cards = list_cards(pool).await?;
    gpu.set_cards(cards.iter().map(card_to_gate).collect());
    let configs = list_engine_configs(pool).await?;
    for cfg in &configs {
        if let Some((kind, desc)) = engine_to_gate(cfg) {
            gpu.set_engine_config(kind, desc);
        }
    }
    // The LLM and the text embedder are always-resident: seed a standing placement
    // so each peak counts against its home card's budget (else a spilling heavy
    // would admit on top of it and OOM). Relocatable services that aren't actually
    // up don't need this.
    for kind in ServiceKind::ALL.into_iter().filter(|k| k.is_standing()) {
        gpu.seed_resident(kind);
    }
    Ok(())
}
