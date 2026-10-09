//! HTTP surface for the agentic chat. Auth-gated.
//!
//! `POST /messages` answers 202 the moment the turn is recorded — the work runs
//! in a background task and the browser polls `GET /sessions/{id}` until no
//! message is `running`.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use tracing::warn;
use uuid::Uuid;

use crate::AppState;
use crate::agent::{
    dispatch::decode_data_url,
    run, store,
    wire::{Attachment, InboundAttachment, Message, SendMessage, SessionSummary, with_token},
};
use crate::auth::{require_auth, validate_token};
use crate::director_assets;

/// Attachments arrive as base64 data URLs, so the cap mirrors `routes::chat`'s:
/// enough for a few images plus a voice memo, small enough to reject a runaway.
const AGENT_BODY_LIMIT: usize = 40 * 1024 * 1024;
/// Attachments per send. A message carries a few pictures and a voice memo; a
/// hundred is a client loop, and every one of them is written to the asset disk
/// and kept for as long as the conversation exists.
const MAX_ATTACHMENTS: usize = 8;
/// Base64 length of one attachment — 4/3 of ~12 MB of real bytes.
const MAX_ATTACHMENT_ENCODED: usize = 16 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/agent/sessions",
            post(create_session).get(list_sessions),
        )
        .route(
            "/api/v1/agent/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/v1/agent/sessions/{id}/messages", post(send_message))
        .route("/api/v1/agent/sessions/{id}/cancel", post(cancel))
        .route("/api/v1/agent/assets/{hash}", get(serve_asset))
        .layer(DefaultBodyLimit::max(AGENT_BODY_LIMIT))
}

async fn create_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if bearer(&state, &headers).await.is_none() {
        return unauthorized();
    }
    match store::create_session(&state.db, "New chat").await {
        Ok(s) => Json(json!({
            "id": s.id.to_string(),
            "title": s.title,
            "created_at": rfc3339(s.created_at),
        }))
        .into_response(),
        Err(e) => db_error("create agent session", e),
    }
}

async fn list_sessions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if bearer(&state, &headers).await.is_none() {
        return unauthorized();
    }
    match store::list_sessions(&state.db).await {
        Ok(rows) => {
            let sessions: Vec<SessionSummary> = rows
                .into_iter()
                .map(|s| SessionSummary {
                    id: s.id.to_string(),
                    title: s.title,
                    created_at: rfc3339(s.created_at),
                    updated_at: rfc3339(s.updated_at),
                })
                .collect();
            Json(json!({ "sessions": sessions })).into_response()
        }
        Err(e) => db_error("list agent sessions", e),
    }
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(token) = bearer(&state, &headers).await else {
        return unauthorized();
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return err(StatusCode::BAD_REQUEST, "bad session id");
    };
    let session = match store::get_session(&state.db, id).await {
        Ok(Some(s)) => s,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such session"),
        Err(e) => return db_error("get agent session", e),
    };
    let rows = match store::messages(&state.db, id).await {
        Ok(r) => r,
        Err(e) => return db_error("list agent messages", e),
    };
    // Media URLs are stored token-less; the viewer's own token is attached here
    // so an <img>/<video>/<audio> src works without headers.
    let messages: Vec<Message> = rows
        .into_iter()
        .map(|m| Message {
            id: m.id.to_string(),
            seq: m.seq,
            role: m.role,
            content: m.content,
            attachments: m
                .attachments
                .into_iter()
                .filter_map(|a| {
                    let hash = director_assets::ref_hash(&a.asset_ref)?;
                    Some(Attachment {
                        kind: a.kind,
                        url: with_token(&format!("/api/v1/agent/assets/{hash}"), &token),
                        mime: a.mime,
                    })
                })
                .collect(),
            steps: m
                .steps
                .into_iter()
                .map(|mut s| {
                    for o in &mut s.outputs {
                        o.url = with_token(&o.url, &token);
                    }
                    s
                })
                .collect(),
            status: m.status,
            created_at: rfc3339(m.created_at),
        })
        .collect();

    Json(json!({
        "id": session.id.to_string(),
        "title": session.title,
        "messages": messages,
    }))
    .into_response()
}

/// Auth as an extractor rather than a check inside the handler: axum runs every
/// `FromRequestParts` before the body extractor, so an unauthenticated send is
/// refused without first buffering and serde-parsing up to `AGENT_BODY_LIMIT`.
struct Bearer(String);

impl axum::extract::FromRequestParts<AppState> for Bearer {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match bearer(state, &parts.headers).await {
            Some(token) => Ok(Self(token)),
            None => Err(unauthorized()),
        }
    }
}

async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Bearer(token): Bearer,
    Json(body): Json<SendMessage>,
) -> Response {
    let Ok(session_id) = Uuid::parse_str(&id) else {
        return err(StatusCode::BAD_REQUEST, "bad session id");
    };
    let content = body.content.trim().to_string();
    if content.is_empty() && body.attachments.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "content or an attachment is required",
        );
    }
    let session = match store::get_session(&state.db, session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such session"),
        Err(e) => return db_error("get agent session", e),
    };

    let attachments = match store_attachments(&state, &body.attachments).await {
        Ok(a) => a,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
    };

    let turn_id = match store::start_turn(&state.db, session_id, &content, &attachments).await {
        Ok(store::StartTurn::Started(t)) => t,
        Ok(store::StartTurn::Busy(t)) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "this conversation is still answering the last message",
                    "turn_id": t.to_string(),
                })),
            )
                .into_response();
        }
        Ok(store::StartTurn::NoSession) => return err(StatusCode::NOT_FOUND, "no such session"),
        Err(e) => return db_error("start agent turn", e),
    };
    if session.title == "New chat" && !content.is_empty() {
        let _ = store::set_title(&state.db, session_id, &store::title_from(&content)).await;
    }

    run::spawn(state, session_id, turn_id, token);
    (
        StatusCode::ACCEPTED,
        Json(json!({ "turn_id": turn_id.to_string() })),
    )
        .into_response()
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if bearer(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let Ok(id) = Uuid::parse_str(&id) else {
        return err(StatusCode::BAD_REQUEST, "bad session id");
    };
    match store::request_cancel(&state.db, id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error("cancel agent turn", e),
    }
}

async fn delete_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if bearer(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let Ok(id) = Uuid::parse_str(&id) else {
        return err(StatusCode::BAD_REQUEST, "bad session id");
    };
    // Best effort only — the watcher polls at CANCEL_POLL and usually reads the
    // row after the DELETE below has removed it. What actually stops the turn is
    // `is_cancel_requested` treating a missing row as cancelled.
    let _ = store::request_cancel(&state.db, id).await;
    match store::delete_session(&state.db, id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such session"),
        Err(e) => db_error("delete agent session", e),
    }
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// Serve a stored attachment. Takes `?token=` like the generations file route,
/// because an `<img src>` cannot set an Authorization header.
async fn serve_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
    Query(q): Query<TokenQuery>,
) -> Response {
    if bearer(&state, &headers).await.is_none() {
        let Some(token) = q.token.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return unauthorized();
        };
        if validate_token(&state.db, state.api_key.as_deref(), token)
            .await
            .is_err()
        {
            return unauthorized();
        }
    }
    let content_type =
        match crate::repositories::director_assets::touch_content_type(&state.db, &hash).await {
            Ok(Some(ct)) => ct,
            Ok(None) => return err(StatusCode::NOT_FOUND, "asset not found"),
            Err(e) => return db_error("agent asset lookup", e),
        };
    let Some(bytes) = director_assets::read_bytes(state.director_assets_dir.as_ref(), &hash).await
    else {
        return err(StatusCode::NOT_FOUND, "asset bytes missing");
    };
    crate::routes::byte_serve::serve_bytes(&headers, bytes, &content_type)
}

/// Decode inbound attachments into the content-addressed store and return the
/// refs to persist. Bytes never reach `agent_messages`.
async fn store_attachments(
    state: &AppState,
    inbound: &[InboundAttachment],
) -> Result<Vec<crate::agent::wire::StoredAttachment>, String> {
    validate_attachments(inbound)?;
    let dir = state.director_assets_dir.as_ref();
    let mut out = Vec::with_capacity(inbound.len());
    for a in inbound {
        let fallback = if a.mime.is_empty() {
            match a.kind.as_str() {
                "audio" => "audio/wav",
                _ => "image/png",
            }
        } else {
            &a.mime
        };
        let Some((bytes, mime)) = decode_data_url(&a.data_url, fallback) else {
            return Err("attachment is not valid base64".to_string());
        };
        let hash = director_assets::hash_bytes(&bytes);
        director_assets::write_bytes(dir, &hash, &bytes)
            .await
            .map_err(|e| format!("could not store attachment: {e}"))?;
        crate::repositories::director_assets::upsert(&state.db, &hash, &mime, bytes.len() as i64)
            .await
            .map_err(|e| format!("could not record attachment: {e}"))?;
        out.push(crate::agent::wire::StoredAttachment {
            kind: a.kind.clone(),
            asset_ref: director_assets::make_ref(&hash),
            mime,
        });
    }
    Ok(out)
}

/// Bound the write before it happens. Stored attachments are content-addressed
/// and live as long as the conversation does, so an unbounded send is a way to
/// fill the asset disk one request at a time.
fn validate_attachments(inbound: &[InboundAttachment]) -> Result<(), String> {
    if inbound.len() > MAX_ATTACHMENTS {
        return Err(format!("at most {MAX_ATTACHMENTS} attachments per message"));
    }
    for a in inbound {
        if !matches!(a.kind.as_str(), "image" | "audio") {
            return Err(format!("unsupported attachment kind: {}", a.kind));
        }
        if a.data_url.len() > MAX_ATTACHMENT_ENCODED {
            return Err("attachment is too large".to_string());
        }
    }
    Ok(())
}

/// The caller's raw bearer token, once validated. Returned rather than
/// discarded because the background task self-dispatches with it and the read
/// path stamps it onto media URLs.
async fn bearer(state: &AppState, headers: &HeaderMap) -> Option<String> {
    require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .ok()?;
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .map(str::to_string)
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

fn unauthorized() -> Response {
    StatusCode::UNAUTHORIZED.into_response()
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn db_error(what: &str, e: sqlx::Error) -> Response {
    warn!(error = %e, "{what} failed");
    err(StatusCode::INTERNAL_SERVER_ERROR, "db error")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(kind: &str, encoded_bytes: usize) -> InboundAttachment {
        InboundAttachment {
            kind: kind.to_string(),
            data_url: format!("data:image/png;base64,{}", "A".repeat(encoded_bytes)),
            mime: "image/png".to_string(),
        }
    }

    #[test]
    fn an_unsupported_attachment_kind_is_refused() {
        let e = validate_attachments(&[attachment("video", 8)]).unwrap_err();
        assert!(e.contains("video"), "{e}");
    }

    #[test]
    fn a_send_cannot_carry_an_unbounded_number_of_attachments() {
        let many: Vec<InboundAttachment> = (0..MAX_ATTACHMENTS + 1)
            .map(|_| attachment("image", 8))
            .collect();
        assert!(validate_attachments(&many).is_err());
        assert!(validate_attachments(&many[..MAX_ATTACHMENTS]).is_ok());
    }

    #[test]
    fn one_attachment_cannot_eat_the_whole_body_budget() {
        assert!(validate_attachments(&[attachment("image", MAX_ATTACHMENT_ENCODED + 1)]).is_err());
        assert!(validate_attachments(&[attachment("image", MAX_ATTACHMENT_ENCODED - 32)]).is_ok());
    }
}
