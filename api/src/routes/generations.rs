//! Per-service generation gallery — list, serve an output file, delete.
//! All auth-gated. Drives the per-page history component. Generations own their
//! on-disk files: delete here unlinks them (the asset lifecycle, §decision-a).

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use koblem_client::types::{GenerationListResponse, GenerationRow, GenerationTitle};
use serde::Deserialize;
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::generations as repo;
use crate::routes::oneshot::key_ok;
use crate::routes::three_d;
use crate::routes::tier2_helpers::generations_root;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/generations", get(list).delete(clear))
        .route(
            "/api/v1/generations/{id}",
            get(detail).patch(update_title).delete(delete_one),
        )
        .route("/api/v1/generations/{id}/retitle", post(retitle))
        .route("/api/v1/generations/{id}/file/{index}", get(serve_file))
        .route(
            "/api/v1/generations/{id}/file/by-kind/{kind}",
            get(serve_file_by_kind),
        )
        .route("/api/v1/generations/{id}/bundle", get(serve_bundle))
        .route("/api/v1/media/sign", post(sign_media_urls))
}

#[derive(Deserialize)]
struct SignRequest {
    /// Server-relative media paths, exactly as a route handed them back
    /// (`/api/v1/generations/{id}/file/0`). Query strings are stripped: the signature covers the
    /// path, and a caller that pasted a `?token=` URL in here should get a clean one out.
    paths: Vec<String>,
}

/// `POST /api/v1/media/sign` — turn media paths into short-lived signed URLs (§6.1).
///
/// The browser cannot compute the HMAC, so the server issues them: authenticate here with a
/// header, once, and get back URLs that can go straight into `<img src>` without carrying a
/// credential. Each is good for ~15 minutes and for exactly the file it names.
///
/// Paths outside `/api/v1/generations/` are refused. Signing is a grant, and the one thing worse
/// than a token in a URL would be a signing oracle that will sign any path in the API.
async fn sign_media_urls(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SignRequest>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if body.paths.len() > 500 {
        return (StatusCode::BAD_REQUEST, "at most 500 paths per call").into_response();
    }
    let mut urls = Vec::with_capacity(body.paths.len());
    for path in &body.paths {
        let bare = path.split('?').next().unwrap_or(path);
        // 🔴 A DELIBERATELY SHORT ALLOWLIST. A signature is a bearer grant for one path, so
        // "sign anything the caller asks for" would turn this route into a way to mint a token
        // for any GET on the service. Each entry here is a route that serves stored media AND
        // verifies the signature at the other end.
        //
        // ⚠️ `/api/v1/upscale/jobs/…` is here because an upscale's output is a large video played
        // in a `<video src>`, which cannot set an Authorization header. Without it `mediaUrl`
        // falls back to `?token=`, i.e. a real API key in browser history, `Referer` headers and
        // access logs — exactly what signed media URLs exist to remove.
        //
        // ⚠️ `/api/v1/pixal3d/characters/…` is here for the same reason and one more: a GLB is
        // handed to `<model-viewer src>` and to a `download` anchor (the asset bundle), neither of
        // which can set a header either.
        let signable = bare.starts_with("/api/v1/generations/")
            || (bare.starts_with("/api/v1/upscale/jobs/")
                && (bare.ends_with("/file") || bare.contains("/segments/")))
            || (bare.starts_with("/api/v1/pixal3d/characters/")
                && (bare.ends_with("/model")
                    || bare.ends_with("/source")
                    || bare.ends_with("/bundle")));
        if !signable {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "refusing to sign `{bare}` — only gallery media paths, upscale outputs and \
                     Pixal3D character assets can be signed"
                ),
            )
                .into_response();
        }
        urls.push(
            state
                .media_signer
                .sign(bare, crate::media_sign::DEFAULT_TTL),
        );
    }
    Json(serde_json::json!({ "urls": urls })).into_response()
}

/// Unlink a deleted generation's files — EXCEPT the ones another row still points at (§6.2).
///
/// Gallery files are content-addressed, so two rows can share one file: a retake that produced
/// identical bytes, the same cutout saved twice. Unlinking blindly would pull the bytes out from
/// under the survivor, which is the one real cost content addressing brings and the reason the
/// row delete happens FIRST — what `still_referenced` returns after that is what must be kept.
///
/// A blob whose last row is deleted between the query and the unlink is the losing side of a
/// race that cannot corrupt anything: it is unlinked, and it had no references. The opposite race
/// — a NEW row taking a reference to a blob being deleted — is why the hourly orphan sweep
/// (`jobs::gpu_retention`) only considers files older than an hour.
async fn unlink_victims(state: &AppState, victims: Vec<repo::Victim>) {
    for victim in victims {
        let Some(root) = generations_root(state, &victim.service).await else {
            continue;
        };
        let keep = match repo::still_referenced(&state.db, &victim.filenames).await {
            Ok(keep) => keep,
            Err(e) => {
                // Cannot prove a file is unreferenced ⇒ do not delete it. An orphan costs disk;
                // a wrongly-deleted blob is somebody else's render gone.
                warn!(error = %e, "generations delete: reference check failed, keeping the files");
                continue;
            }
        };
        for name in victim.filenames {
            if name.contains('/') || name.contains('\\') || name.contains("..") {
                continue;
            }
            if keep.contains(&name) {
                continue;
            }
            let _ = tokio::fs::remove_file(root.join(&name)).await;
        }
    }
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
        (Ok(items), Ok(total)) => Json(GenerationListResponse { items, total }).into_response(),
        (Err(e), _) | (_, Err(e)) => {
            warn!(error = %e, "generations list failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

async fn detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match repo::get(&state.db, &id).await {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            warn!(error = %e, "generations detail failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `PATCH /api/v1/generations/{id}` — rename a generation. Empty/blank title is
/// rejected (use the row's summary fallback instead of storing an empty title).
async fn update_title(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<GenerationTitle>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let title: String = body.title.trim().chars().take(120).collect();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title is required").into_response();
    }
    match repo::set_title(&state.db, &id, &title).await {
        Ok(()) => Json(GenerationTitle { title }).into_response(),
        Err(e) => {
            warn!(error = %e, "generations rename failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `POST /api/v1/generations/{id}/retitle` — regenerate the LLM title from the
/// stored inputs and persist it. Returns the new title, or 502 if the LLM path
/// produced nothing (down/disabled/empty reply).
async fn retitle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let row = match repo::get(&state.db, &id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            warn!(error = %e, "generations retitle: get failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    match crate::routes::tier2_helpers::generate_title(
        &state.llama,
        &state.db,
        &row.service,
        &row.request_meta,
    )
    .await
    {
        Some(title) => {
            if let Err(e) = repo::set_title(&state.db, &id, &title).await {
                warn!(error = %e, "generations retitle: set_title failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
            }
            Json(GenerationTitle { title }).into_response()
        }
        None => (
            StatusCode::BAD_GATEWAY,
            "title generation unavailable — the model may be busy or disabled",
        )
            .into_response(),
    }
}

/// A real koblem credential, by header or by the `?token=` fallback. Distinct
/// from the doorstop, which is a published shared secret and so is trusted for
/// game-asset rows only — takes the pool and key directly so the distinction is
/// testable without an [`AppState`].
async fn credentialled(
    pool: &sqlx::PgPool,
    api_key: Option<&str>,
    headers: &HeaderMap,
    token: Option<&str>,
) -> bool {
    if require_auth(pool, api_key, headers).await.is_ok() {
        return true;
    }
    let Some(token) = token.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    crate::auth::validate_token(pool, api_key, token)
        .await
        .is_ok()
}

/// Which rows the doorstop alone may read. A published shared secret buys the
/// game its own generated assets and nothing else in the gallery.
fn doorstop_may_read(service: &str) -> bool {
    service == three_d::SERVICE
}

#[derive(Deserialize)]
struct FileQuery {
    /// DEPRECATED fallback auth for `<img>`/`<audio>`/`<video>` src, which can't set an
    /// Authorization header (§6.1). This is a REAL credential in a URL: it lands in browser
    /// history, in `Referer` on any outbound link, in access logs, and in `agent_messages.steps`.
    /// Prefer `exp`/`sig` — same ergonomics, no credential. Still accepted so existing pages and
    /// already-issued links keep working.
    token: Option<String>,
    /// Signed-URL pair (§6.1): `sig` is an HMAC over `(path, exp)` with a server-side secret, and
    /// `exp` is a unix timestamp. Grants exactly one file for ~15 minutes and nothing else.
    exp: Option<i64>,
    sig: Option<String>,
}

/// Auth + row lookup, shared by every route that hands back a stored file.
///
/// Returns the row, or the response to send instead. The doorstop rule lives here once rather
/// than once per route: a second copy is how the two would drift.
async fn authorised_row(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    q: &FileQuery,
    path: &str,
) -> Result<GenerationRow, Response> {
    // A valid signature for THIS path is a full grant for this file and nothing else (§6.1) — it
    // is issued by the server to something that already proved it was credentialled, and it is
    // the form an `<img src>` should be using.
    let signed = match (q.exp, q.sig.as_deref()) {
        (Some(exp), Some(sig)) => state.media_signer.verify(path, exp, sig),
        _ => false,
    };
    let credentialled = signed
        || credentialled(
            &state.db,
            state.api_key.as_deref(),
            headers,
            q.token.as_deref(),
        )
        .await;
    // The paperworld doorstop reaches `/api/v1/3d/generate`, and that route
    // answers with URLs pointing here — a generated GLB it cannot then download
    // is not a result. It buys nothing else: which row this is decides whether
    // the doorstop counts, and every other service's outputs stay behind a real
    // credential.
    let doorstop = key_ok(headers);
    if !credentialled && !doorstop {
        return Err(StatusCode::UNAUTHORIZED.into_response());
    }
    let row = match repo::get(&state.db, id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err((StatusCode::NOT_FOUND, "generation not found").into_response());
        }
        Err(e) => {
            warn!(error = %e, "generations: row lookup failed");
            return Err((StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response());
        }
    };
    if !credentialled && !doorstop_may_read(&row.service) {
        return Err(StatusCode::UNAUTHORIZED.into_response());
    }
    Ok(row)
}

/// `GET /api/v1/generations/{id}/file/by-kind/{kind}` — the output that IS this thing (§5a-c).
///
/// The index route stays: most services emit one homogeneous list where position is the only
/// meaningful name. This one is for the bundles, where position is an accident.
async fn serve_file_by_kind(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, kind)): Path<(String, String)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let path = format!("/api/v1/generations/{id}/file/by-kind/{kind}");
    let row = match authorised_row(&state, &headers, &id, &q, &path).await {
        Ok(row) => row,
        Err(resp) => return resp,
    };
    let Some(index) = row.outputs.iter().position(|o| o.kind == kind) else {
        // Name what IS there: a caller asking for `model_lod1` on a generation rendered without
        // LODs should not have to guess whether the kind is wrong or the file is missing.
        let known: Vec<&str> = row.outputs.iter().map(|o| o.kind.as_str()).collect();
        return (
            StatusCode::NOT_FOUND,
            format!(
                "generation has no output of kind `{kind}` — it has: {}",
                known.join(", ")
            ),
        )
            .into_response();
    };
    serve_output(&state, &headers, &row, index).await
}

async fn serve_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, index)): Path<(String, usize)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let path = format!("/api/v1/generations/{id}/file/{index}");
    let row = match authorised_row(&state, &headers, &id, &q, &path).await {
        Ok(row) => row,
        Err(resp) => return resp,
    };
    serve_output(&state, &headers, &row, index).await
}

/// Serve output `index` of an already-authorised row.
async fn serve_output(
    state: &AppState,
    headers: &HeaderMap,
    row: &GenerationRow,
    index: usize,
) -> Response {
    let Some(output) = row.outputs.get(index) else {
        return (StatusCode::NOT_FOUND, "output index out of range").into_response();
    };
    let filename = output.filename.clone();
    let mime = output.mime.clone();

    // Bare filename only — never let a stored name escape its root.
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
    {
        return (StatusCode::BAD_REQUEST, "invalid filename").into_response();
    }

    let Some(root) = generations_root(state, &row.service).await else {
        return (StatusCode::NOT_FOUND, "generations storage not configured").into_response();
    };
    let abs = root.join(&filename);
    // A content-addressed name cannot describe different bytes tomorrow, so it is cacheable
    // forever; a legacy `{uuid}-{i}` name still must revalidate, because a rebuilt cut kept its
    // path (§6.2).
    let cache = if crate::routes::tier2_helpers::is_gallery_blob_name(&filename) {
        crate::routes::byte_serve::CACHE_CONTROL_IMMUTABLE
    } else {
        "private, max-age=0, must-revalidate"
    };
    match crate::routes::byte_serve::serve_file_cached(headers, &abs, &mime, cache).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = %e, path = %abs.display(), "generations file missing");
            (StatusCode::NOT_FOUND, "file missing").into_response()
        }
    }
}

/// `GET /api/v1/generations/{id}/bundle` — the image-to-rig asset bundle, built on demand.
///
/// §5a-b: this zip used to be a *stored* output alongside the very files it contains, so every
/// image-to-rig generation cost about twice its own size on disk — and content-addressing could
/// never see through it, because a zip of deduplicated blobs is a distinct blob. Building it here
/// costs a few hundred milliseconds of deflate on data that barely compresses, and nothing on disk.
///
/// The manifest and README are regenerable rather than stored, which is what makes this possible:
/// both are pure functions of the request meta and the outputs' hashes.
async fn serve_bundle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Response {
    let path = format!("/api/v1/generations/{id}/bundle");
    let row = match authorised_row(&state, &headers, &id, &q, &path).await {
        Ok(row) => row,
        Err(resp) => return resp,
    };
    if row.service != three_d::SERVICE {
        return (
            StatusCode::NOT_FOUND,
            "only image-to-rig generations have a bundle",
        )
            .into_response();
    }
    let Some(root) = generations_root(&state, &row.service).await else {
        return (StatusCode::NOT_FOUND, "generations storage not configured").into_response();
    };
    match three_d::bundle_for_generation(&root, &row).await {
        Ok(zip) => (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "application/zip"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    // A browser saving `bundle` with no extension is a support ticket.
                    "attachment; filename=\"asset-bundle.zip\"",
                ),
            ],
            zip,
        )
            .into_response(),
        Err(e) => {
            warn!(error = %e, id = %row.id, "generations bundle build failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

async fn delete_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match repo::delete(&state.db, &id).await {
        Ok(Some(victim)) => {
            unlink_victims(&state, vec![victim]).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            warn!(error = %e, "generations delete failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `DELETE /api/v1/generations?service=X` — clear all history for one service.
async fn clear(
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
    let Some(service) = q.service.as_deref().filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "service is required").into_response();
    };

    match repo::delete_all_for_service(&state.db, service).await {
        Ok(victims) => {
            unlink_victims(&state, victims).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            warn!(error = %e, "generations clear failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The doorstop is published in the source. It reaches the route that makes
    /// game assets, so it has to reach the files that route hands back — and
    /// stop there. A doorstop that read the whole gallery would be a credential.
    #[test]
    fn the_doorstop_reads_game_assets_and_nothing_else() {
        assert!(doorstop_may_read(three_d::SERVICE));
        for other in ["images", "matting", "depth", "summon", "music"] {
            assert!(!doorstop_may_read(other), "{other}");
        }
    }

    /// The doorstop is NOT a credential: whether or not it is even configured
    /// (it isn't, in this test binary — see `oneshot::key_ok`), an
    /// `x-api-key` header must never be accepted by `credentialled`, which is
    /// the only thing standing between it and every other service's outputs.
    #[tokio::test]
    async fn a_doorstop_header_is_not_a_koblem_credential() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set, skipping");
            return;
        };
        let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
        else {
            eprintln!("test database unreachable, skipping");
            return;
        };
        let key = Some("secret-key");
        let header = |name: &str, value: &str| {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
            h
        };
        let doorstop = header("x-api-key", "paperworld-shoddy-key");

        // Not a credential, by header or as a query token — regardless of
        // whether this doorstop-shaped value happens to be the configured one.
        assert!(!credentialled(&pool, key, &doorstop, None).await);
        assert!(!credentialled(&pool, key, &HeaderMap::new(), Some("paperworld-shoddy-key")).await);

        // A real credential still works both ways round.
        assert!(
            credentialled(
                &pool,
                key,
                &header("authorization", "Bearer secret-key"),
                None
            )
            .await
        );
        assert!(credentialled(&pool, key, &HeaderMap::new(), Some("secret-key")).await);
        assert!(!credentialled(&pool, key, &HeaderMap::new(), None).await);
    }
}
