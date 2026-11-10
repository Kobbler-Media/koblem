//! Beefy LLM mode: hand the whole box to Qwen3.8-27B.
//!
//! Swaps the resident gemma-4-12b for a 27B model split across BOTH cards
//! (`-sm tensor -ts 37,63`, ~25 GiB of the box's 28.6 GiB). While it is on,
//! essentially no other GPU work can run.
//!
//! ## Why this shape
//!
//! **Two always-up containers, not a model swap.** llama-server's
//! `/v1/admin/load` takes no model argument and the worker's `child_argv_` is
//! frozen at construction, so "load a different model" is not expressible. Both
//! servers therefore stay up; entering Beefy re-points `llama_server_url` at the
//! other one. The idle parent is CUDA-free and costs ~0 VRAM — it only spawns its
//! worker child on a real inference POST or an explicit `/v1/admin/load`.
//!
//! **A mode that suspends the scheduler, not a placement.** `Placement` carries
//! exactly one `card_uuid`, so a both-cards reservation cannot be written into
//! the ledger at all. Instead `GpuLock::set_exclusive_with` refuses admission to
//! every service outside `BEEFY_ALLOW`, and budgets the ones inside it against a
//! STATIC per-card reservation (`BEEFY_RESERVE_MB`, the 27B's measured peak) that
//! stands in for the LLM's ledger row. Static on purpose: the live measured-free
//! clamp is blind between the load and the first deep prefill, when the model has
//! claimed only part of what it will hold — the window the 2026-09-03 breeze
//! incident poisoned it in. Unset variables give the old whole-box lock.
//!
//! **No Docker control.** ComfyUI keeps running with its models unloaded via
//! comfy-api's admin route. Measured: it holds a 284 MiB residual after a real
//! render (a native `comfy_aimdo` arena that `/free` does not return), and Beefy
//! at ctx 131072 co-exists with that, serving images with ~924 MiB to spare.
//!
//! The manual equivalent — and the fallback when koblem itself is down — is
//! `kobbler/scripts/beefy-llm.sh {on|off|status}`.
//!
//! ## The restart bridge
//!
//! The mode's persisted halves (flag, URL, gate lock) survive a beefy container
//! restart — a runtime update — but the 21.7 GiB does not: the supervisor comes
//! back honest at `loaded:false`, and exl3 does NOT lazy-load on a bare inference
//! POST (`LAZY_SPAWN` is off by design, so a stray client can never pull 21.7 GiB
//! through the 127.0.0.1-published port). Without a bridge, every beefy update
//! left the mode `on: true, loaded: false` with every request 503ing until an
//! operator noticed.
//!
//! The bridge is [`ensure_loaded`]: one local `/health` GET per request, and a
//! single-flight `POST /v1/admin/load` only when the weights are actually gone.
//! Every path that can carry inference to the beefy URL funnels through it —
//! `LlamaClient::chat_completion` (gated on the client's both-cards flag) and
//! `beefy_proxy::admit` (gated on [`serving_url`]) — and a koblem boot spawns a
//! background warm-up instead of blocking on the load. No beefy-side change and
//! no restart of the beefy container: the supervisor already answers `/health`
//! honestly and `/v1/admin/load` idempotently, which is all the bridge needs.

use std::time::{Duration, Instant};

use kob_gpu_gate::{GpuLock, ServiceKind};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::AppState;
use crate::config::Config;
use crate::repositories::{gpu_jobs, settings};

/// Persisted "Beefy is on". Re-applied at boot — every in-memory flag resets.
const MODE_KEY: &str = "llm_beefy_mode";
/// 🔴 The URL to put back. Without it, an `exit` after a restart cannot know
/// what the normal LLM URL was, because `llama_server_url` now holds the beefy
/// one. Written BEFORE the flip so a crash in between is still recoverable.
const RESTORE_KEY: &str = "llm_beefy_restore_url";
/// The row `set_base_url` and `main.rs`'s `resolve_gpu_url` both read.
const URL_KEY: &str = "llama_server_url";
/// When the mode was entered, RFC3339. Persisted rather than in-memory for the
/// same reason as `MODE_KEY`: the state this is worth reading is "it has been on
/// since 22:14 and it is now morning", and a koblem restart in between must not
/// reset the clock to zero and hide exactly that.
const SINCE_KEY: &str = "llm_beefy_since";

#[derive(Debug)]
pub enum BeefyError {
    /// `BEEFY_LLM_SERVICE_URL` is unset — the feature is not deployed here.
    NotConfigured,
    /// GPU work is in flight. Carries the job ids so the caller can name them.
    JobsRunning(Vec<String>),
    /// ComfyUI is rendering or has work queued — possibly a prompt submitted to it
    /// directly, which no `gpu_jobs` row knows about.
    ComfyBusy,
    /// One or more services could not prove they released their VRAM. Nothing
    /// was flipped; their reservations are retained.
    EvictFailed(Vec<String>),
    /// The 27B did not come up. Already rolled back.
    LoadFailed(String),
    Db(String),
}

impl std::fmt::Display for BeefyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(
                f,
                "BEEFY_LLM_SERVICE_URL is unset — the feature is not deployed here"
            ),
            Self::JobsRunning(ids) => write!(f, "GPU work is in flight: {}", ids.join(", ")),
            Self::ComfyBusy => write!(f, "ComfyUI is rendering or has work queued"),
            Self::EvictFailed(kinds) => write!(f, "could not evict: {}", kinds.join(", ")),
            Self::LoadFailed(detail) => write!(f, "the 27B did not come up: {detail}"),
            Self::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BeefyStatus {
    /// The persisted flag.
    pub on: bool,
    /// What this PROCESS is actually talking to — not what the setting says.
    /// 🔴 These disagree if someone hand-edits `llama_server_url` while Beefy is
    /// on; `inconsistent` below is the detector.
    pub llm_url: String,
    pub beefy_url: Option<String>,
    pub restore_url: Option<String>,
    /// The beefy server reports `{"loaded": true}`.
    /// 🔴 The ONLY proof the weights are resident: the worker-isolation parent
    /// answers `{"status":"ok"}` with no model loaded, so reachability is not
    /// readiness.
    pub loaded: bool,
    /// `gpu.exclusive()`, as a wire string.
    pub exclusive_for: Option<String>,
    /// Kinds still admitted beside the 27B (`BEEFY_ALLOW`), as the gate holds them
    /// right now — empty when off, or when the box is locked whole.
    pub allow: Vec<String>,
    /// `BEEFY_ALLOW` as configured — what `allow` WILL be once the mode is on.
    /// The enter dialog is shown while off, when `allow` is still empty.
    pub configured_allow: Vec<String>,
    /// `X-Worker-Gpu` suppression is active (must be true whenever `on`).
    pub multi_gpu: bool,
    pub live_jobs: usize,
    /// The model id llama-server reports at the CURRENT url, leaf-only
    /// (`Qwen3.8-27B-UD-Q4_K_XL`). `None` when the server is unreachable.
    /// 🔴 Off `/v1/models`, which the worker-isolation parent answers itself
    /// without spawning a child — so reading it does NOT load 25 GiB of weights,
    /// and it is NOT evidence the weights are resident. `loaded` is.
    pub model: Option<String>,
    /// RFC3339 wall clock of when the mode was entered, if it is on. The UI
    /// renders elapsed from it — "on for 9h" is the whole point of showing it.
    pub since: Option<String>,
    /// 🔴 True only if `DIRECTOR_LLM_SERVICE_URL` is set — in which case Director
    /// traffic does NOT follow the flip. False in production today.
    pub director_llm_pinned: bool,
    /// comfy-api's `busy` — the union with ComfyUI's OWN queue. `None` when comfy-api
    /// is not configured or did not answer. 🔴 True while `on` means someone is
    /// rendering on ComfyUI directly (`:8189`), around every gate: koblem's own comfy
    /// work is refused during Beefy.
    pub comfy_busy: Option<bool>,
    /// comfy-api's admission is closed. Must be true whenever `on`.
    pub comfy_draining: Option<bool>,
    /// The half-state detector: the flag disagrees with the live URL, the gate, or
    /// comfy-api's drain.
    pub inconsistent: bool,
}

/// Shared, not per-call: `is_loaded` now runs on every chat request in Beefy
/// mode, and a fresh client (fresh connection pool) per call would churn
/// connections on the hot path.
fn short_http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("http client")
    })
}

/// `POST {base}{path}`, best effort. Returns whether it answered 2xx.
async fn admin_post(base: &str, path: &str, timeout: Duration) -> bool {
    let c = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("http client");
    match c
        .post(format!("{}{}", base.trim_end_matches('/'), path))
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(e) => {
            warn!(base, path, error = %e, "beefy: admin POST failed");
            false
        }
    }
}

/// Does `{base}/health` report a resident model?
async fn is_loaded(base: &str) -> bool {
    let c = short_http();
    match c
        .get(format!("{}/health", base.trim_end_matches('/')))
        .send()
        .await
    {
        Ok(r) => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("loaded").and_then(|l| l.as_bool()))
            .unwrap_or(false),
        Err(_) => false,
    }
}

async fn get_str(db: &PgPool, key: &str) -> Option<String> {
    settings::get_setting(db, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
}

async fn get_bool(db: &PgPool, key: &str) -> bool {
    settings::get_setting(db, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// comfy-api's `/health` as `(busy, draining)`; `None` when it is not configured or
/// did not answer.
///
/// 🔴 `busy` is the union with ComfyUI's OWN queue, so a render started from its web
/// UI counts. That matters because comfy-api's forced unload cancels only ITS jobs:
/// a foreign prompt survives the unload and keeps its VRAM on the 5060 Ti.
async fn comfy_state(state: &AppState) -> Option<(bool, bool)> {
    // Bounded: the H3 client has no request timeout, and a wedged comfy-api must
    // not hang the Beefy status page or the switch.
    let h = tokio::time::timeout(Duration::from_secs(5), state.minimax_h3.health())
        .await
        .ok()??;
    let busy = h.get("busy")?.as_bool()?;
    let draining = h.get("draining").and_then(Value::as_bool).unwrap_or(false);
    Some((busy, draining))
}

/// Close comfy-api's admission for the rest of the session — the same thing `enter`
/// does to gemma. The gate already refuses koblem's own comfy work; this also turns
/// away anything that talks to comfy-api directly (`:8107`).
///
/// 🔴 Must come AFTER any unload: comfy-api's unload CLEARS the drain flag, and only
/// `/v1/admin/load` clears it otherwise. The drain blocks until idle, so it is given
/// a short bound — it runs straight after an unload, when there is nothing to wait for.
async fn drain_comfy(state: &AppState) {
    if let Some(base) = state.minimax_h3.base_url()
        && !admin_post(&base, "/v1/admin/drain?timeout=5", Duration::from_secs(15)).await
    {
        warn!(url = %base, "beefy: could not close comfy-api's admission");
    }
}

/// Give the box to the 27B: `BEEFY_ALLOW` may still co-reside, inside what
/// `BEEFY_RESERVE_MB` leaves on each card. One function so `enter` and the boot
/// re-arm can never disagree about what "held" means.
fn hold_box(gpu: &GpuLock, config: &Config) {
    gpu.set_exclusive_with(
        ServiceKind::Llm,
        &config.beefy_allow,
        config.beefy_reserve_mb.clone(),
    );
}

/// The URL an external client (pi, via `routes/beefy_proxy.rs`) should be piped
/// to — `None` unless the box is actually held for the 27B.
///
/// 🔴 BOTH halves: the persisted flag AND the gate's lock. A flag that survived a
/// half-finished `exit` is not a held box, and piping a request there would load
/// 21.7 GiB onto cards the scheduler thinks are free.
pub async fn serving_url(state: &AppState) -> Option<String> {
    let url = state.config.beefy_llm_url.clone()?;
    held_and_on(&state.db, &state.gpu).await.then_some(url)
}

/// Single-flight across every call path that can see `loaded:false` at once: the
/// weights watcher, koblem's own chat (via `LlamaClient`), and the beefy proxy (pi).
/// Two of them must not both spend 40 s on the same load — the supervisor's own
/// LOCK would serialize them anyway, but the second caller should not hold a client
/// request open for nothing.
static LOAD_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Make the 27B resident at `url`, if it is not already.
///
/// The restart bridge: the persisted halves of the mode (flag, URL, gate) survive
/// a beefy container restart, but the 21.7 GiB does not — the supervisor comes
/// back honest at `loaded:false`, and until someone calls `/v1/admin/load` every
/// inference is a 503. This is that someone: re-check, load, prove.
///
/// Idempotent and cheap in steady state — one local `/health` GET when the weights
/// are already resident. The load itself is bounded by the supervisor's own
/// `LOAD_TIMEOUT_S` (300 s here, matching `finish_enter`).
pub async fn ensure_loaded_url(url: &str) -> Result<(), BeefyError> {
    if is_loaded(url).await {
        return Ok(());
    }
    let _guard = LOAD_LOCK.lock().await;
    // Somebody else (or the supervisor's own poison recycle) loaded it while we
    // queued.
    if is_loaded(url).await {
        return Ok(());
    }
    let t0 = Instant::now();
    let ok = admin_post(url, "/v1/admin/load", Duration::from_secs(300)).await;
    if !ok || !is_loaded(url).await {
        return Err(BeefyError::LoadFailed(
            "POST /v1/admin/load did not leave the model resident — the container is down, or the load failed (check the beefy container logs)".to_string(),
        ));
    }
    info!(
        url,
        load_ms = t0.elapsed().as_millis() as u64,
        "beefy: weights were not resident — loaded them"
    );
    Ok(())
}

/// [`ensure_loaded_url`], gated on the box actually being held for the 27B.
/// `None` from [`serving_url`] means "not Beefy traffic" — nothing to do, no
/// error: an accidental load is exactly what the supervisor's off-by-default
/// `LAZY_SPAWN` exists to prevent.
pub async fn ensure_loaded(state: &AppState) -> Result<(), BeefyError> {
    let Some(url) = serving_url(state).await else {
        return Ok(());
    };
    ensure_loaded_url(&url).await
}

pub async fn status(state: &AppState) -> BeefyStatus {
    let db = &state.db;
    let on = get_bool(db, MODE_KEY).await;
    let beefy_url = state.config.beefy_llm_url.clone();
    let llm_url = state.llama.base_url();
    let loaded = match beefy_url.as_deref() {
        Some(u) if on => is_loaded(u).await,
        _ => false,
    };
    let model = state.llama.model_name().await;
    let exclusive_for = state.gpu.exclusive().map(|k| k.as_str().to_string());
    let multi_gpu = state.llama.is_multi_gpu();
    let live_jobs = gpu_jobs::live(db).await.map(|v| v.len()).unwrap_or(0);
    let points_at_beefy = beefy_url
        .as_deref()
        .is_some_and(|b| llm_url.trim_end_matches('/') == b.trim_end_matches('/'));
    let comfy = comfy_state(state).await;
    // Any disagreement between the flag, the live URL, the header suppression and
    // the gate means someone changed one of them behind our back. A comfy-api that
    // lost its drain (restarted mid-session) counts too; `enter` re-drains it.
    let inconsistent = on != points_at_beefy
        || on != multi_gpu
        || on != (exclusive_for.as_deref() == Some("llm"))
        || (on && comfy.is_some_and(|(_, draining)| !draining));
    BeefyStatus {
        on,
        llm_url,
        beefy_url,
        restore_url: get_str(db, RESTORE_KEY).await,
        loaded,
        exclusive_for,
        allow: state
            .gpu
            .exclusive_allow()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect(),
        configured_allow: state
            .config
            .beefy_allow
            .iter()
            .map(|k| k.as_str().to_string())
            .collect(),
        multi_gpu,
        live_jobs,
        model,
        // Only meaningful while on. A stale stamp left behind by a failed `exit`
        // would read as "Beefy has been on since yesterday" on a box that is not
        // in Beefy at all, so the flag gates it rather than the row's presence.
        since: if on {
            get_str(db, SINCE_KEY).await
        } else {
            None
        },
        director_llm_pinned: state.director_llm.is_some(),
        comfy_busy: comfy.map(|(busy, _)| busy),
        comfy_draining: comfy.map(|(_, draining)| draining),
        inconsistent,
    }
}

/// Hand the box to the 27B.
///
/// Reproduces `beefy-llm.sh`'s sequence, including the order traps. Idempotent:
/// entering while already on re-verifies the weights and repairs a half-finished
/// enter rather than erroring.
pub async fn enter(state: &AppState, force: bool) -> Result<BeefyStatus, BeefyError> {
    let db = &state.db;
    let beefy_url = state
        .config
        .beefy_llm_url
        .clone()
        .ok_or(BeefyError::NotConfigured)?;

    // 1. Idempotence / crash repair.
    if get_bool(db, MODE_KEY).await {
        if is_loaded(&beefy_url).await {
            // The one piece a comfy-api restart can undo mid-session.
            drain_comfy(state).await;
            return Ok(status(state).await);
        }
        info!("beefy: already on but the model is not resident — repairing");
        return finish_enter(state, &beefy_url).await;
    }

    // 2. Preflight. 🔴 Refuse rather than preempt: `pixal3d_rig` and
    //    `pixal3d_motion` are `Preempt::None`, so "preempt everything" is not a
    //    contract that can be honoured, and a half-preempted queue is worse than
    //    a refusal.
    let live = gpu_jobs::live(db)
        .await
        .map_err(|e| BeefyError::Db(e.to_string()))?;
    if !live.is_empty() {
        let ids: Vec<String> = live.iter().map(|j| j.id.clone()).collect();
        if !force {
            return Err(BeefyError::JobsRunning(ids));
        }
        warn!(
            count = ids.len(),
            "beefy: FORCE — proceeding over live jobs; they will be starved"
        );
    }
    // ComfyUI's own queue, which `gpu_jobs` cannot see (a web-UI render, a script
    // on :8189). The unload below would not stop it — see `comfy_state`.
    if comfy_state(state).await.is_some_and(|(busy, _)| busy) {
        if !force {
            return Err(BeefyError::ComfyBusy);
        }
        warn!("beefy: FORCE — proceeding while ComfyUI is busy; its render keeps its VRAM");
    }

    // 3. Hold the door BEFORE evicting, or a queued job admits onto the card the
    //    eviction just freed. The allow-list's co-tenants are evicted below like
    //    everything else and re-admit against the reservation afterwards — except
    //    a standing one (the text embedder), which stays put when the reservation
    //    leaves its card room: it would only reload beside the 27B, and reloading
    //    after the 27B has claimed the card is the order that can OOM.
    hold_box(&state.gpu, &state.config);

    // 4. Record what to restore, before anything is flipped.
    let normal_url = state.llama.base_url();
    if let Err(e) = settings::set_setting(db, RESTORE_KEY, &json!(normal_url)).await {
        state.gpu.set_exclusive(None);
        return Err(BeefyError::Db(e.to_string()));
    }

    // 5. Evict every other resident, concurrently.
    let spare: Vec<ServiceKind> = state
        .config
        .beefy_allow
        .iter()
        .copied()
        .filter(|k| k.is_standing())
        .collect();
    let failed = state
        .gpu
        .evict_all_except_sparing(ServiceKind::Llm, &spare)
        .await;
    if !failed.is_empty() && !force {
        state.gpu.set_exclusive(None);
        return Err(BeefyError::EvictFailed(
            failed.iter().map(|k| k.as_str().to_string()).collect(),
        ));
    }
    // ComfyUI is not always in the ledger, so ask for it by name. `unload_if_idle`
    // fires the eviction path unconditionally; a `false` only means "nothing to
    // do" or "a render holds a guard", and step 2 ruled the latter out.
    let _ = state.gpu.unload_if_idle(ServiceKind::MinimaxH3).await;
    drain_comfy(state).await;

    // 6. Gemma. NOT covered by `evict_all_except` — Llm is the keep kind.
    //    🔴 unload THEN drain: `unload` clears the drain flag, so the reverse
    //    order re-arms nothing.
    admin_post(&normal_url, "/v1/admin/unload", Duration::from_secs(30)).await;
    admin_post(&normal_url, "/v1/admin/drain", Duration::from_secs(10)).await;

    finish_enter(state, &beefy_url).await
}

/// Steps 7–9: point at the beefy server, load it, prove it, persist.
/// Split out so the idempotent repair path can re-run exactly this much.
async fn finish_enter(state: &AppState, beefy_url: &str) -> Result<BeefyStatus, BeefyError> {
    let db = &state.db;

    // 🔴 multi_gpu FIRST, so no request can observe the beefy URL while the
    // X-Worker-Gpu header is still being sent (which would pin a two-card model
    // to one card and 503 every request).
    state.llama.set_multi_gpu(true);
    state.llama.set_base_url(Some(beefy_url.to_string()));

    let t0 = Instant::now();
    // Measured 7 s; the worker's internal health ceiling is 120 s.
    let ok = admin_post(beefy_url, "/v1/admin/load", Duration::from_secs(300)).await;
    if !ok || !is_loaded(beefy_url).await {
        warn!("beefy: the 27B did not come up — rolling back");
        let _ = exit(state, true).await;
        return Err(BeefyError::LoadFailed(
            "load returned an error, or /health still reports loaded:false".to_string(),
        ));
    }
    info!(
        load_ms = t0.elapsed().as_millis() as u64,
        "beefy: model resident"
    );

    settings::set_setting(db, URL_KEY, &json!(beefy_url))
        .await
        .map_err(|e| BeefyError::Db(e.to_string()))?;
    // 🔴 Only if absent. This function is ALSO the crash-repair path — an `enter`
    // on a box that is already on but has lost its weights re-runs exactly this
    // much — and restamping there would erase how long the mode has really been
    // held, which is the one number that catches a switch left on overnight.
    // Best effort: a clock that failed to write is not worth failing the flip.
    if get_str(db, SINCE_KEY).await.is_none()
        && let Ok(now) =
            time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)
    {
        let _ = settings::set_setting(db, SINCE_KEY, &json!(now)).await;
    }
    settings::set_setting(db, MODE_KEY, &json!(true))
        .await
        .map_err(|e| BeefyError::Db(e.to_string()))?;
    info!(url = beefy_url, "beefy: ON");
    Ok(status(state).await)
}

/// Give the box back.
///
/// Total by construction: it restores unconditionally rather than from a record
/// of what `enter` did, so it also repairs a half-finished enter, a reboot, or a
/// mode that was never entered. Mirrors `beefy-llm.sh`'s `cmd_off`.
pub async fn exit(state: &AppState, _force: bool) -> Result<BeefyStatus, BeefyError> {
    let db = &state.db;
    // 🔴 The env fallback is what makes this total.
    let restore_url = get_str(db, RESTORE_KEY)
        .await
        .unwrap_or_else(|| state.config.llama_server_url.clone());

    // Best effort: the container may be gone, which is precisely when `exit` is
    // being used as a repair tool.
    if let Some(b) = state.config.beefy_llm_url.as_deref() {
        admin_post(b, "/v1/admin/unload", Duration::from_secs(60)).await;
    }

    // 🔴 multi_gpu LAST here (mirror of enter's FIRST).
    state.llama.set_base_url(Some(restore_url.clone()));
    state.llama.set_multi_gpu(false);
    state.gpu.set_exclusive(None);
    // Idempotent belt: the standing reservation was never removed (Llm was the
    // keep kind), but a forced path or a card edit could have dropped it.
    state.gpu.seed_resident(ServiceKind::Llm);

    // 🔴 MANDATORY, not optional. `enter` left gemma DRAINING with no child, and
    // the drain flag is cleared only by `/v1/admin/unload` or `/v1/admin/load`.
    // koblem calls `/v1/admin/load` nowhere else. Skip this and every chat 503s
    // "service is draining" until the container restarts.
    if !admin_post(&restore_url, "/v1/admin/load", Duration::from_secs(300)).await {
        warn!(url = %restore_url, "beefy: could not re-arm the normal LLM — it may still be draining");
    }
    // 🔴 Equally mandatory, for the same reason: `enter` left comfy-api draining and
    // only `/v1/admin/load` reopens it. Skip this and h3, ltx25, music3 and upscale
    // all answer 503 "service is draining" until the container restarts. Best
    // effort — it answers at once and holds no weights.
    if let Some(comfy) = state.minimax_h3.base_url()
        && !admin_post(&comfy, "/v1/admin/load", Duration::from_secs(30)).await
    {
        warn!(url = %comfy, "beefy: could not reopen comfy-api — it may still be draining");
    }

    settings::set_setting(db, URL_KEY, &json!(restore_url))
        .await
        .map_err(|e| BeefyError::Db(e.to_string()))?;
    let _ = settings::set_setting(db, RESTORE_KEY, &Value::Null).await;
    let _ = settings::set_setting(db, SINCE_KEY, &Value::Null).await;
    settings::set_setting(db, MODE_KEY, &json!(false))
        .await
        .map_err(|e| BeefyError::Db(e.to_string()))?;
    info!(url = %restore_url, "beefy: OFF");
    Ok(status(state).await)
}

/// Re-arm the in-memory halves after a koblem restart.
///
/// The persisted halves look after themselves: `llama_server_url` is DB-backed,
/// so `main.rs` has already pointed the client and the gate's `HeavyKind::Llm` at
/// the beefy container before this runs. What does NOT survive a restart is every
/// in-memory flag — the gate's `exclusive` and the client's `multi_gpu`.
///
/// 🔴 Deliberately does NOT block on the weights. A boot that blocks on a GPU load
/// is wrong (the container may be down, and the worker's health poll runs to 120 s).
/// What changed since this comment was first written: "the lazy path covers it" was
/// only true of the llama.cpp runtime — exl3's supervisor answers a bare inference
/// POST with 503 `model_not_loaded` (its `LAZY_SPAWN` is off by design, so a stray
/// client can never pull 21.7 GiB). A restarted beefy container therefore left the
/// mode `on: true, loaded: false` with nothing to close the gap.
///
/// So a standing watcher ([`watch_weights`], spawned in `main.rs`) keeps the
/// weights resident for the life of the process, and every request path
/// (`LlamaClient::chat_completion`, `beefy_proxy::admit`) funnels through
/// [`ensure_loaded`]: a request that arrives before the next watcher pass loads
/// the weights itself, single-flight, instead of dying with a 503.
pub async fn apply_at_boot(
    db: &PgPool,
    gpu: &GpuLock,
    llama: &crate::llama::LlamaClient,
    config: &Config,
) {
    if !get_bool(db, MODE_KEY).await {
        return;
    }
    hold_box(gpu, config);
    llama.set_multi_gpu(true);
    warn!(
        url = %llama.base_url(),
        "beefy: mode was ON at shutdown — re-armed exclusive scheduling and the both-cards flag. GPU work stays refused until POST /api/v1/gpu/beefy {{\"on\":false}}"
    );
}

/// How often [`watch_weights`] re-checks residency while the box is held for
/// the 27B: one local /health GET per pass, and a load only ever happens when
/// the weights are actually gone (a beefy container restart).
const WEIGHTS_WATCH_INTERVAL: Duration = Duration::from_secs(30);
/// Failure backoff cap. A down beefy container is retried until
/// `restart: unless-stopped` brings it back, but not with a warn line every
/// 30 s; a successful pass resets the cadence.
const WEIGHTS_WATCH_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// The standing half of the restart bridge: for the life of the process, while
/// the box is held for the 27B and the persisted mode is on, keep the weights
/// resident. This is what makes a beefy container restart (a runtime update)
/// self-heal in the background within one interval, so the next real request
/// does not pay the ~40 s load inline; the request path (`llama.rs`,
/// `beefy_proxy.rs`) is the backstop for the request that arrives before the
/// next pass. It also covers the boot case (a box reboot restarts beefy AND
/// koblem): the first pass lands ~30 s after boot, and the backoff retries
/// until the beefy container is up.
///
/// Gated on BOTH halves, exactly like [`serving_url`]: a held gate with the
/// flag off (a half-finished `exit` that crashed before its last write) is a
/// state whose intent is unknown, and spending 21.7 GiB on a guess is exactly
/// what `LAZY_SPAWN` exists to prevent. `exit` releases the gate BEFORE it
/// clears the flag, and `enter` loads the weights BEFORE it sets the flag, so
/// the watcher can never race a mode flip.
pub async fn watch_weights(db: PgPool, gpu: GpuLock, beefy_url: Option<String>) {
    let mut backoff = WEIGHTS_WATCH_INTERVAL;
    loop {
        tokio::time::sleep(backoff).await;
        let Some(url) = beefy_url.clone() else {
            continue;
        };
        if !held_and_on(&db, &gpu).await {
            continue;
        }
        match ensure_loaded_url(&url).await {
            Ok(()) => backoff = WEIGHTS_WATCH_INTERVAL,
            Err(e) => {
                warn!(error = %e, "beefy: weights watcher: the 27B is not resident");
                backoff = (backoff * 2).min(WEIGHTS_WATCH_BACKOFF_MAX);
            }
        }
    }
}

/// The two halves [`serving_url`] checks, without an `AppState`. The gate is
/// checked first so the DB read only happens while the box is actually held
/// for the 27B.
async fn held_and_on(db: &PgPool, gpu: &GpuLock) -> bool {
    gpu.exclusive() == Some(ServiceKind::Llm) && get_bool(db, MODE_KEY).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::{
        Json, Router,
        routing::{get, post},
    };

    use super::{BeefyError, ensure_loaded_url};

    /// A fake exl3 supervisor: `/health` reports `loaded` only after
    /// `/v1/admin/load` has completed, and the counter proves how many load
    /// POSTs actually went out.
    async fn fake_supervisor() -> (String, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let loads = Arc::new(AtomicUsize::new(0));
        let loaded = Arc::new(AtomicBool::new(false));

        let health_loaded = loaded.clone();
        let load_loads = loads.clone();
        let load_loaded = loaded.clone();

        let app = Router::new()
            .route(
                "/health",
                get(move || {
                    let loaded = health_loaded.clone();
                    async move {
                        Json(serde_json::json!({
                            "status": "ok",
                            "loaded": loaded.load(Ordering::Relaxed)
                        }))
                    }
                }),
            )
            .route(
                "/v1/admin/load",
                post(move || {
                    let loads = load_loads.clone();
                    let loaded = load_loaded.clone();
                    async move {
                        loads.fetch_add(1, Ordering::Relaxed);
                        // The supervisor's own LOCK + spawn + residency poll.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        loaded.store(true, Ordering::Relaxed);
                        Json(serde_json::json!({ "status": "loaded" }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), loads, loaded)
    }

    #[tokio::test]
    async fn ensure_loaded_loads_once_under_concurrency_and_then_is_free() {
        let (base, loads, loaded) = fake_supervisor().await;

        // Five callers all see loaded:false at once: exactly ONE load POST.
        let mut handles = Vec::new();
        for _ in 0..5 {
            let base = base.clone();
            handles.push(tokio::spawn(async move { ensure_loaded_url(&base).await }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        assert!(loaded.load(Ordering::Relaxed));
        assert_eq!(loads.load(Ordering::Relaxed), 1);

        // Steady state: no further load POSTs, no error.
        ensure_loaded_url(&base).await.unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ensure_loaded_reports_a_failed_load() {
        // A supervisor that answers /health but refuses the load: the bridge
        // must surface the failure, not spin.
        let app = Router::new()
            .route(
                "/health",
                get(|| async { Json(serde_json::json!({ "status": "ok", "loaded": false })) }),
            )
            .route(
                "/v1/admin/load",
                post(|| async {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(serde_json::json!({ "error": "refusing to spawn" })),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let err = ensure_loaded_url(&format!("http://{addr}"))
            .await
            .unwrap_err();
        assert!(matches!(err, BeefyError::LoadFailed(_)));
    }
}
