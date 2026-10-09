//! CRUD over `prompt_templates` and `prompt_variables`, plus a preview
//! endpoint that renders a draft template against a sample / pasted payload.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use koblem_client::types::ReviewRequest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::context::ReviewContext;
use crate::fixtures;
use crate::prompt_engine;
use crate::repositories::prompt_templates as repo;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/settings/prompt-templates",
            get(list_templates_overview),
        )
        .route(
            "/api/v1/settings/prompt-templates/{media_kind}/{slot}",
            get(get_template)
                .put(upsert_template)
                .delete(delete_template),
        )
        .route(
            "/api/v1/settings/prompt-variables/{media_kind}",
            get(list_variables_for_kind),
        )
        .route(
            "/api/v1/settings/prompt-variables/{media_kind}/{name}",
            get(get_variable)
                .put(upsert_variable)
                .delete(delete_variable),
        )
        .route(
            "/api/v1/settings/prompt-templates/preview",
            post(preview_render),
        )
        .route(
            "/api/v1/settings/prompt-templates/fixtures",
            get(list_fixtures),
        )
        .route(
            "/api/v1/settings/prompt-templates/context-schema",
            get(get_context_schema),
        )
}

// ── Overview ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct OverviewResponse {
    media_kinds: Vec<MediaKindOverview>,
}

#[derive(Serialize)]
struct MediaKindOverview {
    media_kind: &'static str,
    label: &'static str,
    /// Per-kind override row counts (system + user slots, plus per-name
    /// variables). Lets the UI badge each kind as "customised" or "default".
    template_overrides: usize,
    variable_overrides: usize,
}

const KIND_LABELS: &[(&str, &str)] = &[
    (
        "default",
        "Default (applies to every kind unless overridden)",
    ),
    ("tv_episode", "TV episode"),
    ("tv_season", "TV season"),
    ("movie", "Movie"),
    ("sport_event", "Sport event"),
    ("other", "Other"),
];

async fn list_templates_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // One SQL round-trip returns counts grouped by kind; the loop fills in
    // labels + zeroes for kinds with no rows.
    let counts = match repo::count_overrides_by_kind(&state.db).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "count_overrides_by_kind failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };
    let by_kind: std::collections::HashMap<&str, (i64, i64)> = counts
        .iter()
        .map(|(k, t, v)| (k.as_str(), (*t, *v)))
        .collect();

    let media_kinds: Vec<MediaKindOverview> = KIND_LABELS
        .iter()
        .map(|(kind, label)| {
            let (t, v) = by_kind.get(*kind).copied().unwrap_or((0, 0));
            MediaKindOverview {
                media_kind: kind,
                label,
                template_overrides: t as usize,
                variable_overrides: v as usize,
            }
        })
        .collect();
    Json(OverviewResponse { media_kinds }).into_response()
}

// ── Template CRUD ───────────────────────────────────────────────────

#[derive(Serialize)]
struct TemplateBody {
    media_kind: String,
    slot: String,
    /// Resolved body the engine would use right now (per-kind override →
    /// default-kind override → Rust fallback).
    body: String,
    /// True if the request's `media_kind` has its own row.
    is_overridden: bool,
    /// True if any DB layer (per-kind or default) has a row. False = pure
    /// Rust fallback.
    has_db_row: bool,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    updated_at: Option<OffsetDateTime>,
    /// The Rust-baked default body, surfaced so the UI can show "what does
    /// the factory default look like" without a second round-trip.
    default_body: String,
}

#[derive(Deserialize)]
struct UpsertTemplateBody {
    body: String,
}

async fn get_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((media_kind, slot)): Path<(String, String)>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !is_known_kind(&media_kind) {
        return (StatusCode::BAD_REQUEST, "unknown media_kind").into_response();
    }
    if !is_known_slot(&slot) {
        return (StatusCode::BAD_REQUEST, "slot must be 'system' or 'user'").into_response();
    }

    // Two queries instead of three: per-kind + default in parallel, then
    // resolve from the same data without re-fetching.
    let (per_kind_res, default_res) =
        tokio::join!(repo::get_template(&state.db, &media_kind, &slot), async {
            if media_kind != prompt_engine::MEDIA_KIND_DEFAULT {
                repo::get_template(&state.db, prompt_engine::MEDIA_KIND_DEFAULT, &slot).await
            } else {
                Ok(None)
            }
        });
    let per_kind = match per_kind_res {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, %media_kind, %slot, "get_template per-kind failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };
    let default_row = match default_res {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, %slot, "get_template default failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };

    let is_overridden = per_kind.is_some() && media_kind != prompt_engine::MEDIA_KIND_DEFAULT;
    let (body, updated_at, has_db_row) = match (&per_kind, &default_row) {
        (Some(t), _) => (t.body.clone(), Some(t.updated_at), true),
        (None, Some(t)) => (t.body.clone(), Some(t.updated_at), true),
        (None, None) => (
            prompt_engine::default_template(&slot).to_string(),
            None,
            false,
        ),
    };

    Json(TemplateBody {
        media_kind,
        slot: slot.clone(),
        body,
        is_overridden,
        has_db_row,
        updated_at,
        default_body: prompt_engine::default_template(&slot).to_string(),
    })
    .into_response()
}

async fn upsert_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((media_kind, slot)): Path<(String, String)>,
    Json(payload): Json<UpsertTemplateBody>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !is_known_kind(&media_kind) {
        return (StatusCode::BAD_REQUEST, "unknown media_kind").into_response();
    }
    if !is_known_slot(&slot) {
        return (StatusCode::BAD_REQUEST, "slot must be 'system' or 'user'").into_response();
    }
    if let Err(e) = repo::upsert_template(&state.db, &media_kind, &slot, &payload.body).await {
        warn!(error = %e, %media_kind, %slot, "failed to upsert prompt template");
        return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((media_kind, slot)): Path<(String, String)>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match repo::delete_template(&state.db, &media_kind, &slot).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            warn!(error = %e, %media_kind, %slot, "failed to delete prompt template");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

// ── Variable CRUD ───────────────────────────────────────────────────

#[derive(Serialize)]
struct VariableSummary {
    media_kind: String,
    name: String,
    body: String,
    is_overridden: bool,
    has_db_row: bool,
    is_seeded: bool,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    updated_at: Option<OffsetDateTime>,
    default_body: String,
}

/// Single-shot per-kind variable list. One SQL round-trip returns every
/// variable's resolved body + flags + the seeded Rust default — the UI uses
/// this directly to populate its edit drafts without a second fetch per row.
#[derive(Serialize)]
struct VariableListItem {
    name: String,
    /// Resolved body the engine would use right now: per-kind row → default
    /// row → Rust seeded constant (`""` for unknown names).
    body: String,
    is_seeded: bool,
    /// True when the per-kind row exists AND we're not viewing the default
    /// kind itself (where per_kind == default_kind and the distinction is
    /// meaningless).
    has_per_kind_override: bool,
    has_default_override: bool,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    updated_at: Option<OffsetDateTime>,
    /// Rust-baked seed body so the UI's "reset to factory" target is a one-
    /// shot read. Empty string for non-seeded user-defined names.
    default_body: String,
}

async fn list_variables_for_kind(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(media_kind): Path<String>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !is_known_kind(&media_kind) {
        return (StatusCode::BAD_REQUEST, "unknown media_kind").into_response();
    }

    let resolved = match repo::list_resolved_variables(
        &state.db,
        &media_kind,
        prompt_engine::SEEDED_VARIABLES,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, %media_kind, "list_resolved_variables failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };

    let is_default_view = media_kind == prompt_engine::MEDIA_KIND_DEFAULT;
    let items: Vec<VariableListItem> = resolved
        .into_iter()
        .map(|v| {
            let is_seeded = prompt_engine::SEEDED_VARIABLES.contains(&v.name.as_str());
            let body = v
                .per_kind_body
                .clone()
                .or_else(|| v.default_body.clone())
                .unwrap_or_else(|| prompt_engine::default_variable(&v.name).to_string());
            VariableListItem {
                has_per_kind_override: v.per_kind_body.is_some() && !is_default_view,
                has_default_override: v.default_body.is_some(),
                default_body: prompt_engine::default_variable(&v.name).to_string(),
                updated_at: v.updated_at,
                is_seeded,
                body,
                name: v.name,
            }
        })
        .collect();
    Json(items).into_response()
}

async fn get_variable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((media_kind, name)): Path<(String, String)>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !is_known_kind(&media_kind) {
        return (StatusCode::BAD_REQUEST, "unknown media_kind").into_response();
    }
    if name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name required").into_response();
    }

    // Two queries (per-kind + default) instead of the previous three; result
    // computes both `is_overridden` and `body` from the same data without a
    // re-fetch.
    let per_kind = match repo::get_variable(&state.db, &media_kind, &name).await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, %media_kind, %name, "get_variable per-kind lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };
    let default_row = if media_kind != prompt_engine::MEDIA_KIND_DEFAULT {
        match repo::get_variable(&state.db, prompt_engine::MEDIA_KIND_DEFAULT, &name).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, %name, "get_variable default lookup failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
            }
        }
    } else {
        None
    };

    let is_overridden = per_kind.is_some() && media_kind != prompt_engine::MEDIA_KIND_DEFAULT;
    let (body, updated_at, has_db_row) = match (&per_kind, &default_row) {
        (Some(v), _) => (v.body.clone(), Some(v.updated_at), true),
        (None, Some(v)) => (v.body.clone(), Some(v.updated_at), true),
        (None, None) => (
            prompt_engine::default_variable(&name).to_string(),
            None,
            false,
        ),
    };

    Json(VariableSummary {
        media_kind,
        name: name.clone(),
        body,
        is_overridden,
        has_db_row,
        is_seeded: prompt_engine::SEEDED_VARIABLES.contains(&name.as_str()),
        updated_at,
        default_body: prompt_engine::default_variable(&name).to_string(),
    })
    .into_response()
}

async fn upsert_variable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((media_kind, name)): Path<(String, String)>,
    Json(payload): Json<UpsertTemplateBody>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !is_known_kind(&media_kind) {
        return (StatusCode::BAD_REQUEST, "unknown media_kind").into_response();
    }
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name required").into_response();
    }
    if !trimmed_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            "name must be ASCII alphanumeric + underscore only",
        )
            .into_response();
    }
    if let Err(e) = repo::upsert_variable(&state.db, &media_kind, trimmed_name, &payload.body).await
    {
        warn!(error = %e, %media_kind, %name, "failed to upsert prompt variable");
        return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_variable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((media_kind, name)): Path<(String, String)>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Seeded variables can have their per-kind row deleted to fall back to
    // default, but their default-kind row deletion is a "reset to factory"
    // move that's also fine — the engine still returns the Rust constant.
    match repo::delete_variable(&state.db, &media_kind, &name).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            warn!(error = %e, %media_kind, %name, "failed to delete prompt variable");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

// ── Preview ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PreviewRequest {
    media_kind: String,
    /// Draft template bodies to evaluate. Either or both can be set; absent
    /// fields fall through to the saved DB row (or Rust default).
    #[serde(default)]
    draft_system: Option<String>,
    #[serde(default)]
    draft_user: Option<String>,
    /// Per-variable draft bodies (name → body). Each one overrides the
    /// per-kind / default DB row for that variable in this single render.
    #[serde(default)]
    draft_variables: HashMap<String, String>,
    /// Pick one of the shipped fixtures by id, OR pass an explicit
    /// `fixture_payload` (a full ReviewRequest JSON). If both are set, the
    /// pasted payload wins.
    #[serde(default)]
    fixture_id: Option<String>,
    #[serde(default)]
    fixture_payload: Option<Value>,
}

#[derive(Serialize)]
struct PreviewResponse {
    rendered_system: Option<String>,
    rendered_user: Option<String>,
    /// Friendly error strings the UI shows inline. Empty on success.
    errors: Vec<String>,
}

async fn preview_render(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<PreviewRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !is_known_kind(&payload.media_kind) {
        return (StatusCode::BAD_REQUEST, "unknown media_kind").into_response();
    }

    // Resolve the fixture / pasted payload into a ReviewRequest.
    let request: ReviewRequest = match (payload.fixture_payload, payload.fixture_id) {
        (Some(v), _) => match serde_json::from_value(v) {
            Ok(r) => r,
            Err(e) => {
                return Json(PreviewResponse {
                    rendered_system: None,
                    rendered_user: None,
                    errors: vec![format!("fixture_payload could not be parsed: {e}")],
                })
                .into_response();
            }
        },
        (None, Some(id)) => match fixtures::get_fixture_payload(&id) {
            Some(r) => r,
            None => {
                return Json(PreviewResponse {
                    rendered_system: None,
                    rendered_user: None,
                    errors: vec![format!("unknown fixture id: {id}")],
                })
                .into_response();
            }
        },
        (None, None) => {
            return Json(PreviewResponse {
                rendered_system: None,
                rendered_user: None,
                errors: vec!["preview requires fixture_id or fixture_payload".to_string()],
            })
            .into_response();
        }
    };

    // Disambiguation / corrections aren't exercised in preview — the UI is
    // about template shape, not external state. Pass an empty ReviewContext.
    let ctx = ReviewContext::default();

    let overrides = prompt_engine::TemplateOverrides {
        system: payload.draft_system.as_deref(),
        user: payload.draft_user.as_deref(),
        variables: if payload.draft_variables.is_empty() {
            None
        } else {
            Some(&payload.draft_variables)
        },
    };

    match prompt_engine::build_messages_with_overrides(
        &state.db,
        &ctx,
        &request,
        &request.candidates,
        &overrides,
    )
    .await
    {
        Ok(pair) => Json(PreviewResponse {
            rendered_system: Some(pair.system),
            rendered_user: Some(pair.user),
            errors: vec![],
        })
        .into_response(),
        Err(e) => Json(PreviewResponse {
            rendered_system: None,
            rendered_user: None,
            errors: vec![format_engine_error(&e)],
        })
        .into_response(),
    }
}

fn format_engine_error(e: &prompt_engine::EngineError) -> String {
    match e {
        prompt_engine::EngineError::Template { slot, source } => {
            format!("{slot} template: {}", verbose_minijinja(source))
        }
        prompt_engine::EngineError::Variable { name, source } => {
            format!("variable {name}: {}", verbose_minijinja(source))
        }
        prompt_engine::EngineError::Repo(err) => format!("database: {err}"),
    }
}

fn verbose_minijinja(e: &minijinja::Error) -> String {
    let mut buf = format!("{} ({})", e, e.kind());
    if let Some(line) = e.line() {
        buf.push_str(&format!(" at line {line}"));
    }
    buf
}

// ── Fixtures ────────────────────────────────────────────────────────

async fn list_fixtures(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(fixtures::list_fixtures()).into_response()
}

async fn get_context_schema(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(prompt_engine::context_schema()).into_response()
}

// ── Validation helpers ──────────────────────────────────────────────

fn is_known_kind(k: &str) -> bool {
    k == prompt_engine::MEDIA_KIND_DEFAULT || prompt_engine::all_media_kinds().contains(&k)
}

fn is_known_slot(s: &str) -> bool {
    s == prompt_engine::SLOT_SYSTEM || s == prompt_engine::SLOT_USER
}
