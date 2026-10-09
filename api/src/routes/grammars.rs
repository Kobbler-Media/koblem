//! CRUD over the user-defined `grammars` table plus the synthetic built-in
//! `review` entry. Picked from the sampling block via `GrammarMode`.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get},
};
use koblem_client::types::{GRAMMAR_NAME_OFF, GRAMMAR_NAME_REVIEW, REVIEW_GRAMMAR};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::grammars as grammars_repo;

#[derive(Serialize)]
struct GrammarEntry {
    name: String,
    gbnf: String,
    description: Option<String>,
    /// `true` for the synthetic built-in entries (`off`, `review`); `false`
    /// for user-defined rows in the `grammars` table.
    is_builtin: bool,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    created_at: Option<OffsetDateTime>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    updated_at: Option<OffsetDateTime>,
}

#[derive(Deserialize)]
struct UpsertRequest {
    name: String,
    gbnf: String,
    description: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/grammars", get(list_grammars).put(upsert_grammar))
        .route("/api/v1/grammars/{name}", delete(delete_grammar))
}

fn is_reserved(name: &str) -> bool {
    name == GRAMMAR_NAME_OFF || name == GRAMMAR_NAME_REVIEW
}

async fn list_grammars(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let mut entries = vec![
        GrammarEntry {
            name: GRAMMAR_NAME_OFF.to_string(),
            gbnf: String::new(),
            description: Some("No grammar — model output is unconstrained.".to_string()),
            is_builtin: true,
            created_at: None,
            updated_at: None,
        },
        GrammarEntry {
            name: GRAMMAR_NAME_REVIEW.to_string(),
            gbnf: REVIEW_GRAMMAR.to_string(),
            description: Some(
                "koblem's built-in accept-list grammar (notes-first, idx+reason objects)."
                    .to_string(),
            ),
            is_builtin: true,
            created_at: None,
            updated_at: None,
        },
    ];

    match grammars_repo::list_grammars(&state.db).await {
        Ok(rows) => {
            for g in rows {
                entries.push(GrammarEntry {
                    name: g.name,
                    gbnf: g.gbnf,
                    description: g.description,
                    is_builtin: false,
                    created_at: Some(g.created_at),
                    updated_at: Some(g.updated_at),
                });
            }
            Json(entries).into_response()
        }
        Err(err) => {
            warn!(error = %err, "failed to list grammars");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn upsert_grammar(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<UpsertRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let name = payload.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name required").into_response();
    }
    if is_reserved(name) {
        return (
            StatusCode::BAD_REQUEST,
            format!("name `{name}` is reserved for the built-in entries"),
        )
            .into_response();
    }
    if payload.gbnf.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "gbnf required").into_response();
    }

    match grammars_repo::upsert_grammar(
        &state.db,
        name,
        &payload.gbnf,
        payload.description.as_deref(),
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            warn!(error = %err, name = %name, "failed to upsert grammar");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn delete_grammar(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if is_reserved(&name) {
        return (StatusCode::BAD_REQUEST, "cannot delete a built-in grammar").into_response();
    }

    match grammars_repo::delete_grammar(&state.db, &name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "grammar not found").into_response(),
        Err(err) => {
            warn!(error = %err, name = %name, "failed to delete grammar");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}
