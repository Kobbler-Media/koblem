//! Tier-2 GPU logs management endpoints — list, read, replay, delete, stats.
//! All auth-gated. Used by the web history page.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_logs as repo;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/gpu-logs", get(list).delete(clear_all))
        .route("/api/v1/gpu-logs/stats", get(stats))
        .route("/api/v1/gpu-logs/{id}", get(detail).delete(delete_one))
        .route("/api/v1/gpu-logs/{id}/sample", get(sample_bytes))
}

#[derive(Deserialize)]
struct ListQuery {
    service: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let service = q.service.as_deref();

    match (
        repo::list(&state.db, service, limit, offset).await,
        repo::count(&state.db, service).await,
    ) {
        (Ok(entries), Ok(total)) => Json(json!({
            "entries": entries,
            "total": total,
        }))
        .into_response(),
        (Err(e), _) | (_, Err(e)) => {
            warn!(error = %e, "gpu-logs list failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

async fn stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match repo::stats(&state.db).await {
        Ok(s) => Json(json!({
            "total_entries": s.total_entries,
            "total_samples": s.total_samples,
            "total_sample_bytes": s.total_sample_bytes,
            "oldest_created_at": s.oldest_created_at.map(|t| t.to_string()),
            "newest_created_at": s.newest_created_at.map(|t| t.to_string()),
        }))
        .into_response(),
        Err(e) => {
            warn!(error = %e, "gpu-logs stats failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

async fn detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match repo::get_log(&state.db, id).await {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            warn!(error = %e, "gpu-logs detail failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

#[derive(Deserialize)]
struct SampleQuery {
    /// Fallback auth param for `<img src>` / `<audio src>` which can't set
    /// an Authorization header. The bearer header still takes precedence.
    token: Option<String>,
}

async fn sample_bytes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<SampleQuery>,
) -> Response {
    let header_ok = require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_ok();
    if !header_ok {
        let Some(token) = q.token.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        if crate::auth::validate_token(&state.db, state.api_key.as_deref(), token)
            .await
            .is_err()
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }

    let log = match repo::get_log(&state.db, id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "log not found").into_response(),
        Err(e) => {
            warn!(error = %e, "gpu-logs sample_bytes: get_log failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    let Some(sample_id) = log.sample_id else {
        return (StatusCode::NOT_FOUND, "no sample linked").into_response();
    };

    let sample = match repo::get_sample(&state.db, sample_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return (StatusCode::NOT_FOUND, "sample not found").into_response(),
        Err(e) => {
            warn!(error = %e, "gpu-logs sample_bytes: get_sample failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    let abs = state.samples_dir.join(&sample.path);
    match crate::routes::byte_serve::serve_file(&headers, &abs, &sample.mime).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = %e, path = %abs.display(), "sample file missing");
            (StatusCode::NOT_FOUND, "sample file missing").into_response()
        }
    }
}

async fn delete_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match repo::delete_log(&state.db, id).await {
        Ok(sample_path) => {
            if let Some(rel) = sample_path {
                let abs = state.samples_dir.join(&rel);
                let _ = tokio::fs::remove_file(&abs).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            warn!(error = %e, "gpu-logs delete failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

async fn clear_all(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match repo::delete_all(&state.db).await {
        Ok(paths) => {
            for rel in paths {
                let abs = state.samples_dir.join(&rel);
                let _ = tokio::fs::remove_file(&abs).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            warn!(error = %e, "gpu-logs clear_all failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}
