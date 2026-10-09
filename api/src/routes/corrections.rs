use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::corrections as corrections_repo;

#[derive(Deserialize)]
pub struct CreateGenericRuleRequest {
    scope: String,
    rule: String,
}

#[derive(Deserialize)]
pub struct CreateEntityRuleRequest {
    entity_id: String,
    entity_type: String,
    entity_name: Option<String>,
    rule: String,
}

#[derive(Deserialize)]
pub struct CreateDecisionCorrectionRequest {
    scope: Option<String>,
    release_name: String,
    entity_id: Option<String>,
    model_decision: String,
    correct_decision: String,
    reason: String,
}

#[derive(Serialize)]
pub struct CreatedResponse {
    id: Uuid,
}

#[derive(Deserialize)]
pub struct ListCorrectionsQuery {
    scope: Option<String>,
    entity_id: Option<String>,
    #[serde(rename = "type")]
    correction_type: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    #[allow(dead_code)]
    offset: i64,
}

fn default_limit() -> i64 {
    50
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/corrections/rules", post(create_generic_rule))
        .route("/api/v1/corrections/entity-rules", post(create_entity_rule))
        .route(
            "/api/v1/corrections/decisions",
            post(create_decision_correction),
        )
        .route("/api/v1/corrections", get(list_corrections))
        .route(
            "/api/v1/corrections/{correction_type}/{id}",
            delete(delete_correction),
        )
}

async fn create_generic_rule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateGenericRuleRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match corrections_repo::create_generic_rule(&state.db, &payload.scope, &payload.rule).await {
        Ok(id) => Json(CreatedResponse { id }).into_response(),
        Err(err) => {
            warn!(error = %err, "failed to create generic rule");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn create_entity_rule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateEntityRuleRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match corrections_repo::create_entity_rule(
        &state.db,
        &payload.entity_id,
        &payload.entity_type,
        payload.entity_name.as_deref(),
        &payload.rule,
    )
    .await
    {
        Ok(id) => Json(CreatedResponse { id }).into_response(),
        Err(err) => {
            warn!(error = %err, "failed to create entity rule");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn create_decision_correction(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateDecisionCorrectionRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let scope = payload.scope.as_deref().unwrap_or("release-classification");

    match corrections_repo::create_decision_correction(
        &state.db,
        scope,
        &payload.release_name,
        payload.entity_id.as_deref(),
        &payload.model_decision,
        &payload.correct_decision,
        &payload.reason,
    )
    .await
    {
        Ok(id) => Json(CreatedResponse { id }).into_response(),
        Err(err) => {
            warn!(error = %err, "failed to create decision correction");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn list_corrections(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListCorrectionsQuery>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // If a specific type is requested, return just that type
    match query.correction_type.as_deref() {
        Some("generic_rule") => {
            match corrections_repo::list_generic_rules(&state.db, query.scope.as_deref()).await {
                Ok(rules) => Json(rules).into_response(),
                Err(err) => {
                    warn!(error = %err, "failed to list generic rules");
                    (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
                }
            }
        }
        Some("entity_rule") => {
            match corrections_repo::list_entity_rules(&state.db, query.entity_id.as_deref()).await {
                Ok(rules) => Json(rules).into_response(),
                Err(err) => {
                    warn!(error = %err, "failed to list entity rules");
                    (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
                }
            }
        }
        Some("decision_correction") => {
            match corrections_repo::list_recent_corrections(
                &state.db,
                query.entity_id.as_deref(),
                query.limit,
            )
            .await
            {
                Ok(corrections) => Json(corrections).into_response(),
                Err(err) => {
                    warn!(error = %err, "failed to list decision corrections");
                    (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
                }
            }
        }
        _ => {
            // Return all types grouped
            let generic = corrections_repo::list_generic_rules(&state.db, query.scope.as_deref())
                .await
                .unwrap_or_default();
            let entity = corrections_repo::list_entity_rules(&state.db, query.entity_id.as_deref())
                .await
                .unwrap_or_default();
            let decisions = corrections_repo::list_recent_corrections(
                &state.db,
                query.entity_id.as_deref(),
                query.limit,
            )
            .await
            .unwrap_or_default();

            Json(serde_json::json!({
                "generic_rules": generic,
                "entity_rules": entity,
                "decision_corrections": decisions,
            }))
            .into_response()
        }
    }
}

async fn delete_correction(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((correction_type, id)): Path<(String, Uuid)>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let result = match correction_type.as_str() {
        "generic_rule" | "generic-rule" => {
            corrections_repo::delete_generic_rule(&state.db, id).await
        }
        "entity_rule" | "entity-rule" => corrections_repo::delete_entity_rule(&state.db, id).await,
        "decision_correction" | "decision-correction" => {
            corrections_repo::delete_decision_correction(&state.db, id).await
        }
        _ => return (StatusCode::BAD_REQUEST, "invalid correction type").into_response(),
    };

    match result {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(err) => {
            warn!(error = %err, "failed to delete correction");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}
