//! GPU preempt-signal endpoint — polled by the parakeet STT worker between
//! chunks. **Permanent no-op now**: co-residency is budget-managed (no lanes),
//! so it always answers `{cancelled: false}`. The route is kept because parakeet
//! still polls it (a 404 would be noisier than a stable "don't cancel").
//!
//! Plus the per-service disable surface (§6.5 of GO-LIVE-PLAN) —
//! `/api/v1/gpu/services` list + toggle + disable-all / enable-all. Auth-gated.
//!
//! The cancel endpoint is no-auth: parakeet reaches koblem over the Docker
//! network; the URL is passed into each transcribe request as `cancel_url`
//! form field.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{
    GpuCardOccupancy, GpuCardsResponse, GpuEnginesResponse, GpuOccupancyResponse, GpuResident,
    GpuServiceState, GpuServicesResponse,
};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_scheduler;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/gpu/stt-preempt", get(stt_preempt_status))
        .route("/api/v1/gpu/services", get(list_services))
        .route("/api/v1/gpu/services/{name}", post(toggle_service))
        .route("/api/v1/gpu/services/disable-all", post(disable_all))
        .route("/api/v1/gpu/services/enable-all", post(enable_all))
        // Multi-GPU scheduler config + live occupancy.
        .route("/api/v1/gpu/scheduler/cards", get(list_cards))
        .route("/api/v1/gpu/scheduler/cards/{uuid}", post(toggle_card))
        .route("/api/v1/gpu/scheduler/engines", get(list_engines))
        .route(
            "/api/v1/gpu/scheduler/engines/{service}",
            put(upsert_engine),
        )
        .route("/api/v1/gpu/scheduler/occupancy", get(occupancy))
        // Beefy LLM: hand the whole box to the 27B. See `crate::beefy`.
        .route("/api/v1/gpu/beefy", get(beefy_status).post(beefy_set))
}

async fn stt_preempt_status(State(state): State<AppState>) -> impl IntoResponse {
    // Permanent no-op: the lane preempt signals are gone (co-residency is
    // budget-managed), so both predicates are stable `false`. Kept so parakeet's
    // poll gets a stable answer instead of a 404.
    let cancelled = state.gpu.is_tts_requested() || state.gpu.is_vision_requested();
    axum::Json(json!({"cancelled": cancelled}))
}

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

async fn list_services(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    // `busy` = the service is actively serving a request right now (a placement
    // with a live guard, or a server reporting a direct caller via its announce
    // endpoint — a live STT stream). It deliberately does NOT count a service that merely
    // reserves budget while idle — an idle-but-loaded worker the reconcile loop
    // observed, or the warm standing LLM seed — those do nothing and shouldn't
    // read as busy.
    let active = state.gpu.active_services();
    let services: Vec<GpuServiceState> = state
        .gpu
        .service_states()
        .into_iter()
        .map(|(k, enabled)| GpuServiceState {
            name: k.as_str().to_string(),
            enabled,
            busy: active.contains(&k),
        })
        .collect();
    Json(GpuServicesResponse { services }).into_response()
}

#[derive(Deserialize)]
struct ToggleBody {
    enabled: bool,
}

async fn toggle_service(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<ToggleBody>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(kind) = ServiceKind::parse(&name) else {
        return err(StatusCode::BAD_REQUEST, &format!("unknown service: {name}"));
    };
    state.gpu.set_service_enabled(kind, body.enabled).await;
    Json(json!({ "name": kind.as_str(), "enabled": body.enabled })).into_response()
}

#[derive(Deserialize, Default)]
struct DisableAllBody {
    #[serde(default)]
    except: Vec<String>,
}

async fn disable_all(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<DisableAllBody>>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let except: Vec<ServiceKind> = body
        .map(|Json(b)| b.except)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| ServiceKind::parse(s))
        .collect();
    for kind in ServiceKind::ALL {
        if except.contains(&kind) {
            continue;
        }
        state.gpu.set_service_enabled(kind, false).await;
    }
    Json(json!({ "ok": true })).into_response()
}

async fn enable_all(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    for kind in ServiceKind::ALL {
        state.gpu.set_service_enabled(kind, true).await;
    }
    Json(json!({ "ok": true })).into_response()
}

// ── Multi-GPU scheduler config ────────────────────────────────────────────────

/// `GET /api/v1/gpu/scheduler/cards` — the physical GPU list + enable flags.
async fn list_cards(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match gpu_scheduler::list_cards(&state.db).await {
        Ok(cards) => Json(GpuCardsResponse { cards }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

/// `POST /api/v1/gpu/scheduler/cards/{uuid}` — enable/disable a card. Disabling
/// drains the card (relocating relocatable residents, cancelling the rest) and
/// removes it from placement; refuses to disable the last enabled card.
async fn toggle_card(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(uuid): Path<String>,
    Json(body): Json<ToggleBody>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    // Drive the gate first — it owns the last-card-refusal + drain machinery.
    if let Err(e) = state.gpu.set_card_enabled(&uuid, body.enabled).await {
        return err(StatusCode::BAD_REQUEST, &e);
    }
    // Persist the flag so it survives a restart. If the DB write fails, the live
    // gate has already changed but the DB hasn't — a restart would silently revert
    // the operator's action. Compensate by rolling the gate back to match the DB.
    match gpu_scheduler::set_card_enabled(&state.db, &uuid, body.enabled).await {
        Ok(Some(row)) => Json(json!({ "card": row })).into_response(),
        Ok(None) => {
            let _ = state.gpu.set_card_enabled(&uuid, !body.enabled).await;
            err(StatusCode::NOT_FOUND, &format!("unknown card: {uuid}"))
        }
        Err(e) => {
            let _ = state.gpu.set_card_enabled(&uuid, !body.enabled).await;
            err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}"))
        }
    }
}

/// `GET /api/v1/gpu/scheduler/engines` — the per-service placement config.
async fn list_engines(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    match gpu_scheduler::list_engine_configs(&state.db).await {
        Ok(engines) => Json(GpuEnginesResponse { engines }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

#[derive(Deserialize)]
struct UpsertEngineBody {
    peak_mb: i32,
    #[serde(default)]
    home_gpu: String,
    #[serde(default)]
    candidate_gpus: Vec<String>,
    /// Named, relational eviction set: the `ServiceKind` names allowed to evict
    /// THIS service. Empty = never auto-evicted (replaces the old `priority`).
    #[serde(default)]
    evictable_by: Vec<String>,
    #[serde(default)]
    relocatable: bool,
    #[serde(default)]
    rules_json: String,
}

/// `PUT /api/v1/gpu/scheduler/engines/{service}` — validate + upsert a service's
/// placement config, then push the edited descriptor into the live gate (and
/// reseed the standing LLM) so the edit applies without a restart.
async fn upsert_engine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(service): Path<String>,
    Json(body): Json<UpsertEngineBody>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if ServiceKind::parse(&service).is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("unknown service: {service}"),
        );
    }
    // Validate the config before it touches the DB or the live gate: a bad peak
    // reserves nothing (admittable anywhere), an unknown card is un-placeable, and
    // a mistyped evictor name silently becomes "never evictable" — all of which
    // manifest later as "the scheduler doesn't do what I configured".
    if let Err(msg) = validate_engine_config(&state, &body) {
        return err(StatusCode::BAD_REQUEST, &msg);
    }
    let row = match gpu_scheduler::upsert_engine_config(
        &state.db,
        &service,
        body.peak_mb,
        &body.home_gpu,
        &body.candidate_gpus,
        &body.evictable_by,
        body.relocatable,
        &body.rules_json,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    // Push the single edited descriptor straight into the gate. `set_engine_config`
    // re-syncs any resident placement of this kind, so a live peak / eviction-rule
    // edit takes effect immediately.
    if let Some((gk, desc)) = gpu_scheduler::engine_to_gate(&row) {
        state.gpu.set_engine_config(gk, desc);
        // The LLM is always-resident: reseed so a home/candidate change re-homes
        // its standing reservation instead of stranding it on the old card.
        if matches!(gk, ServiceKind::Llm) {
            state.gpu.seed_resident(ServiceKind::Llm);
        }
    }
    Json(json!({ "engine": row })).into_response()
}

/// Validate an engine-config upsert against the live card list. Rejects a
/// non-positive peak, unknown home/candidate card UUIDs, a home not in the
/// candidate list, and unparseable `evictable_by` service names.
fn validate_engine_config(state: &AppState, body: &UpsertEngineBody) -> Result<(), String> {
    if body.peak_mb <= 0 {
        return Err(format!("peak_mb must be > 0 (got {})", body.peak_mb));
    }
    let known: Vec<String> = state.gpu.cards().into_iter().map(|c| c.uuid).collect();
    if !body.home_gpu.is_empty() {
        if !known.contains(&body.home_gpu) {
            return Err(format!("unknown home_gpu card: {}", body.home_gpu));
        }
        if !body.candidate_gpus.is_empty() && !body.candidate_gpus.contains(&body.home_gpu) {
            return Err("home_gpu must be included in candidate_gpus".to_string());
        }
    }
    for uuid in &body.candidate_gpus {
        if !known.contains(uuid) {
            return Err(format!("unknown candidate_gpus card: {uuid}"));
        }
    }
    for name in &body.evictable_by {
        if ServiceKind::parse(name).is_none() {
            return Err(format!("unknown evictable_by service: {name}"));
        }
    }
    Ok(())
}

/// `GET /api/v1/gpu/scheduler/occupancy` — live per-card occupancy: total, used,
/// free budget, and the resident services. Drives the settings page's "what's
/// where" panel and the out-of-band-visibility story.
async fn occupancy(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let cards: Vec<GpuCardOccupancy> = state
        .gpu
        .occupancy()
        .into_iter()
        .map(|c| GpuCardOccupancy {
            uuid: c.uuid,
            total_mb: c.total_mb.into(),
            used_mb: c.used_mb.into(),
            free_mb: c.free_mb.into(),
            measured_free_mb: c.measured_free_mb.map(i64::from),
            reserved_mb: c.reserved_mb.map(i64::from),
            fast: c.fast,
            enabled: c.enabled,
            residents: c
                .residents
                .into_iter()
                .map(|(k, mb)| GpuResident {
                    service: k.as_str().to_string(),
                    peak_mb: mb.into(),
                })
                .collect(),
        })
        .collect();
    Json(GpuOccupancyResponse { cards }).into_response()
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

// ── Beefy LLM ────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct BeefyBody {
    on: bool,
    /// Proceed over live GPU jobs (they will be starved) and over services whose
    /// unload could not be confirmed.
    #[serde(default)]
    force: bool,
}

/// `GET /api/v1/gpu/beefy` — is the box handed to the 27B, and is it actually up?
async fn beefy_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    Json(crate::beefy::status(&state).await).into_response()
}

/// `POST /api/v1/gpu/beefy` `{"on": true, "force": false}`
///
/// Both directions are repair tools, not just toggles: `enter` on an already-on
/// box re-verifies the weights and fixes a half-finished enter, and `exit` on a
/// box that was never entered is a full restore.
async fn beefy_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BeefyBody>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let result = if body.on {
        crate::beefy::enter(&state, body.force).await
    } else {
        crate::beefy::exit(&state, body.force).await
    };
    match result {
        Ok(st) => Json(st).into_response(),
        Err(crate::beefy::BeefyError::NotConfigured) => err(
            StatusCode::NOT_IMPLEMENTED,
            "BEEFY_LLM_SERVICE_URL is not set on koblem-api",
        ),
        Err(crate::beefy::BeefyError::JobsRunning(ids)) => err(
            StatusCode::CONFLICT,
            &format!(
                "{} GPU job(s) are running or collecting ({}). Wait, or retry with force — \
                 they WILL be starved",
                ids.len(),
                ids.join(", ")
            ),
        ),
        Err(crate::beefy::BeefyError::ComfyBusy) => err(
            StatusCode::CONFLICT,
            "ComfyUI is rendering or has work queued (possibly submitted to it directly). Wait, or \
             retry with force — its render keeps its VRAM on the 5060 Ti",
        ),
        Err(crate::beefy::BeefyError::EvictFailed(kinds)) => err(
            StatusCode::CONFLICT,
            &format!(
                "could not prove these services released their VRAM: {}. Their reservations are \
                 retained and nothing was flipped",
                kinds.join(", ")
            ),
        ),
        Err(crate::beefy::BeefyError::LoadFailed(detail)) => err(
            StatusCode::BAD_GATEWAY,
            &format!("Qwen3.8 failed to load ({detail}); rolled back to the normal LLM"),
        ),
        Err(crate::beefy::BeefyError::Db(e)) => {
            err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}"))
        }
    }
}
