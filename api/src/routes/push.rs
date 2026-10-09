//! `/api/v1/push` — the browser subscription surface for queue notifications.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::auth::require_auth;
use crate::queue::push;
use crate::repositories::push_subscriptions as store;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/push/key", get(public_key))
        .route("/api/v1/push/subscribe", post(subscribe))
        .route("/api/v1/push/unsubscribe", post(unsubscribe))
}

#[derive(Deserialize)]
struct Keys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
struct SubscribeBody {
    endpoint: String,
    keys: Keys,
}

#[derive(Deserialize)]
struct UnsubscribeBody {
    endpoint: String,
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// The `applicationServerKey` the browser subscribes with, plus how many endpoints are already
/// registered so the UI can say whether this is the only one.
async fn public_key(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    let Some(vapid) = push::load_vapid(&state.db).await else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no VAPID keypair; browser push is unavailable on this instance",
        );
    };
    Json(json!({
        "public_key": vapid.public_key,
        "subscriptions": store::count(&state.db).await.unwrap_or(0),
    }))
    .into_response()
}

async fn subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> Response {
    let principal = match principal(&state, &headers).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if body.endpoint.trim().is_empty()
        || body.keys.p256dh.trim().is_empty()
        || body.keys.auth.trim().is_empty()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "endpoint, keys.p256dh and keys.auth are all required",
        );
    }
    match store::upsert(
        &state.db,
        body.endpoint.trim(),
        body.keys.p256dh.trim(),
        body.keys.auth.trim(),
        &principal.label(),
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Turning the toggle off. Idempotent: a browser that already dropped its subscription still gets
/// a 204, because the caller's intent — "stop sending here" — is satisfied either way.
async fn unsubscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UnsubscribeBody>,
) -> Response {
    if let Err(r) = principal(&state, &headers).await {
        return r;
    }
    match store::remove(&state.db, body.endpoint.trim()).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn principal(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::auth::Principal, Response> {
    require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .map_err(|c| c.into_response())
}
