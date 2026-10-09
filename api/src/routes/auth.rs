use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use base64::{Engine as _, engine::general_purpose};
use koblem_client::types::{
    ApiKeySummary, AuthMeResponse, CreateApiKeyRequest, CreateApiKeyResponse,
};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use tracing::warn;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{hash_api_key, hash_password, require_auth, verify_password};
use crate::repositories::auth as auth_repo;

#[derive(Deserialize)]
pub struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    token: String,
}

#[derive(Deserialize)]
pub struct SetupRequest {
    email: String,
    password: String,
}

#[derive(Serialize)]
pub struct SetupResponse {
    user_id: Uuid,
    token: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/setup", post(setup))
        .route("/api/v1/auth/me", get(me))
        .route(
            "/api/v1/auth/api-keys",
            get(list_api_keys).post(create_api_key),
        )
        .route("/api/v1/auth/api-keys/{id}", delete(revoke_api_key))
}

async fn login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    let email = payload.email.trim().to_lowercase();

    let credentials = match auth_repo::get_user_by_email(&state.db, &email).await {
        Ok(Some(creds)) => creds,
        Ok(None) => return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response(),
        Err(err) => {
            warn!(error = %err, "failed to fetch user");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };

    if !verify_password(&payload.password, &credentials.password_hash) {
        return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }

    let mut raw = [0u8; 32];
    rand::rng().fill(&mut raw[..]);
    let token = general_purpose::URL_SAFE_NO_PAD.encode(raw);

    if let Err(err) = auth_repo::create_auth_token(&state.db, credentials.id, &token).await {
        warn!(error = %err, "failed to create auth token");
        return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
    }

    Json(LoginResponse { token }).into_response()
}

/// First user creation — only works if no users exist.
async fn setup(
    State(state): State<AppState>,
    Json(payload): Json<SetupRequest>,
) -> impl IntoResponse {
    let count = match auth_repo::user_count(&state.db).await {
        Ok(c) => c,
        Err(err) => {
            warn!(error = %err, "failed to check user count");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };

    if count > 0 {
        return (StatusCode::CONFLICT, "setup already completed").into_response();
    }

    let email = payload.email.trim().to_lowercase();
    if email.is_empty() || payload.password.is_empty() {
        return (StatusCode::BAD_REQUEST, "email and password required").into_response();
    }

    let password_hash = match hash_password(&payload.password) {
        Ok(hash) => hash,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "hash failed").into_response(),
    };

    let user_id = match auth_repo::create_user(&state.db, &email, &password_hash).await {
        Ok(id) => id,
        Err(err) => {
            warn!(error = %err, "failed to create user");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };

    let mut raw = [0u8; 32];
    rand::rng().fill(&mut raw[..]);
    let token = general_purpose::URL_SAFE_NO_PAD.encode(raw);

    if let Err(err) = auth_repo::create_auth_token(&state.db, user_id, &token).await {
        warn!(error = %err, "failed to create auth token");
        return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
    }

    Json(SetupResponse { user_id, token }).into_response()
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let authenticated = require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_ok();
    Json(AuthMeResponse { authenticated })
}

async fn list_api_keys(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match auth_repo::list_api_keys(&state.db).await {
        Ok(keys) => {
            let summaries: Vec<ApiKeySummary> = keys
                .into_iter()
                .map(|k| ApiKeySummary {
                    id: k.id,
                    name: k.name,
                    key: k.key,
                    last_used_at: k.last_used_at.and_then(|t| t.format(&Rfc3339).ok()),
                    created_at: k.created_at.format(&Rfc3339).unwrap_or_default(),
                    revoked: k.revoked,
                })
                .collect();
            Json(summaries).into_response()
        }
        Err(err) => {
            warn!(error = %err, "failed to list API keys");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn create_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let name = payload.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }

    let mut key_bytes = [0u8; 32];
    rand::rng().fill(&mut key_bytes[..]);
    let key = general_purpose::URL_SAFE_NO_PAD.encode(key_bytes);
    let key_hash = hash_api_key(&key);

    match auth_repo::create_api_key(&state.db, name, &key_hash, &key).await {
        Ok(id) => Json(CreateApiKeyResponse { id, key }).into_response(),
        Err(err) => {
            warn!(error = %err, "failed to create API key");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn revoke_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match auth_repo::revoke_api_key(&state.db, id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "not found or already revoked").into_response(),
        Err(err) => {
            warn!(error = %err, "failed to revoke API key");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}
