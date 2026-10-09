use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use kob_providers::types::BuildMediaWantRequest;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
}

#[derive(Serialize)]
pub struct EpisodeTitleResponse {
    title: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/library/search", get(search_shows))
        .route("/api/v1/library/shows/{id}", get(get_show_detail))
        .route(
            "/api/v1/library/shows/{id}/episodes/{season}/{episode}/title",
            get(get_episode_title),
        )
        .route("/api/v1/library/audiobooks", get(search_audiobooks))
        .route("/api/v1/library/audiobooks/{id}", get(get_audiobook))
        .route(
            "/api/v1/library/audiobooks/{id}/clip",
            get(get_audiobook_clip),
        )
        .route("/api/v1/library/build-media-want", post(build_media_want))
}

async fn get_show_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.library_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "library provider not configured",
            )
                .into_response();
        }
    };

    match provider.get_show_detail(&id).await {
        Ok(Some(detail)) => Json(detail).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "show not found").into_response(),
        Err(err) => {
            warn!(error = %err, "show detail lookup failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("show detail lookup failed: {err}"),
            )
                .into_response()
        }
    }
}

async fn search_shows(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.library_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "library provider not configured",
            )
                .into_response();
        }
    };

    match provider.search_shows(&query.q).await {
        Ok(shows) => Json(shows).into_response(),
        Err(err) => {
            warn!(error = %err, "library search failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("library search failed: {err}"),
            )
                .into_response()
        }
    }
}

async fn search_audiobooks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.library_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "library provider not configured",
            )
                .into_response();
        }
    };

    match provider.search_audiobooks(&query.q).await {
        Ok(books) => Json(books).into_response(),
        Err(err) => {
            warn!(error = %err, "audiobook search failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("audiobook search failed: {err}"),
            )
                .into_response()
        }
    }
}

async fn get_audiobook(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.library_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "library provider not configured",
            )
                .into_response();
        }
    };

    match provider.get_audiobook(&id).await {
        Ok(Some(detail)) => Json(detail).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "audiobook not found").into_response(),
        Err(err) => {
            warn!(error = %err, "audiobook detail lookup failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("audiobook detail lookup failed: {err}"),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct ClipQuery {
    chapter: u32,
    offset: f64,
    duration: f64,
}

/// `GET /api/v1/library/audiobooks/{id}/clip` — proxy to kobbler's audiobook
/// clip cutter for the trim-preview UX in the playground. Returns a mono
/// WAV. This is a frontend convenience; tier-2 register_from_library still
/// fetches the audio itself for the final upload.
async fn get_audiobook_clip(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ClipQuery>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.library_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "library provider not configured",
            )
                .into_response();
        }
    };

    match provider
        .fetch_audiobook_clip(&id, q.chapter, q.offset, q.duration)
        .await
    {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "audio/wav")
            .header("cache-control", "no-store")
            .body(Body::from(bytes))
            .unwrap(),
        Err(err) => {
            warn!(error = %err, "audiobook clip fetch failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("audiobook clip fetch failed: {err}"),
            )
                .into_response()
        }
    }
}

async fn get_episode_title(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, season, episode)): Path<(String, String, String)>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.library_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "library provider not configured",
            )
                .into_response();
        }
    };

    match provider.get_episode_title(&id, &season, &episode).await {
        Ok(Some(title)) => Json(EpisodeTitleResponse { title }).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "episode not found").into_response(),
        Err(err) => {
            warn!(error = %err, "episode title lookup failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("episode title lookup failed: {err}"),
            )
                .into_response()
        }
    }
}

async fn build_media_want(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<BuildMediaWantRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.library_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "library provider not configured",
            )
                .into_response();
        }
    };

    match provider.build_media_want(&payload).await {
        Ok(media_want) => Json(media_want).into_response(),
        Err(err) => {
            warn!(error = %err, "build_media_want failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("build_media_want failed: {err}"),
            )
                .into_response()
        }
    }
}
