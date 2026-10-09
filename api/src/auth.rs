use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::http::{HeaderMap, StatusCode};
use password_hash::SaltString;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use subtle::ConstantTimeEq;

use crate::repositories::auth as auth_repo;

/// Who is making this request.
///
/// 🔴 `require_auth` used to return `()`, which threw the caller identity away —
/// and downstream there was then no way to tell a UI session from an API key. That
/// blocks two things at once: the queue-vs-immediate default (a web session
/// queues, a machine caller carries on exactly as today) and `created_by` on the
/// job row.
///
/// `extract_caller` is NOT a substitute. It is the `x-caller` header falling back
/// to `User-Agent` — self-declared and unauthenticated. Fine as a telemetry label,
/// wrong as a policy input.
///
/// A sum type rather than a user id because the material is asymmetric: a session
/// reaches `users`, an `api_keys` row has no `user_id` column at all, and the env
/// key and the media doorstop have no row anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A web-login session. The interactive caller.
    Session { user_id: uuid::Uuid, email: String },
    /// A named key from the `api_keys` table — the CLI, kobbler, a script.
    ApiKey { id: uuid::Uuid, name: String },
    /// The `API_KEY` env var: a single shared machine credential with no row.
    EnvKey,
}

impl Principal {
    /// The `created_by` value written on a job row. Prefixed by kind so a user
    /// named `cli` and the API key named `cli` are never confused for each other.
    pub fn label(&self) -> String {
        match self {
            Principal::Session { email, .. } => format!("user:{email}"),
            Principal::ApiKey { name, .. } => format!("key:{name}"),
            Principal::EnvKey => "key:env".to_string(),
        }
    }

    /// Whether this caller's work should QUEUE by default.
    ///
    /// 🔴 The goal is "carry on exactly as today", not privileged access. The
    /// failure being designed against is narrow: an external call landing at the
    /// back of a queue holding five hours of renders. Most of that is already true
    /// by construction — everything kobbler calls (vision, tts, stt, matting) is in
    /// the direct/interactive set and never enters the queue at all. This default
    /// only bites for a *queueable* kind invoked externally.
    ///
    /// It is only a DEFAULT: an explicit mode on the request always wins, because
    /// the CLI authenticates with an API key and would otherwise be forced into
    /// `now` — the opposite of what it is for.
    pub fn queues_by_default(&self) -> bool {
        matches!(self, Principal::Session { .. })
    }
}

pub async fn require_auth(
    pool: &PgPool,
    api_key_env: Option<&str>,
    headers: &HeaderMap,
) -> Result<Principal, StatusCode> {
    let auth = headers
        .get("authorization")
        .and_then(|val| val.to_str().ok())
        .unwrap_or("");
    let token = auth.strip_prefix("Bearer ").unwrap_or("").trim();
    if token.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    validate_token(pool, api_key_env, token).await
}

pub async fn validate_token(
    pool: &PgPool,
    api_key_env: Option<&str>,
    token: &str,
) -> Result<Principal, StatusCode> {
    // Check environment variable API key first. Constant-time: this is a shared
    // secret compared against attacker-controlled input, and `==` leaks timing
    // per matching byte.
    if let Some(expected) = api_key_env
        && token.as_bytes().ct_eq(expected.as_bytes()).into()
    {
        return Ok(Principal::EnvKey);
    }

    // Check auth_tokens (user login sessions)
    if let Some((user_id, email)) = auth_repo::session_user(pool, token)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        return Ok(Principal::Session { user_id, email });
    }

    // Check api_keys table (named API keys)
    let key_hash = hash_api_key(token);
    if let Some((id, name)) = auth_repo::api_key_name_by_hash(pool, &key_hash)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        // Update last_used_at asynchronously (fire-and-forget)
        let pool = pool.clone();
        let hash = key_hash.clone();
        tokio::spawn(async move {
            let _ = auth_repo::touch_api_key(&pool, &hash).await;
        });
        return Ok(Principal::ApiKey { id, name });
    }

    Err(StatusCode::UNAUTHORIZED)
}

pub fn hash_password(password: &str) -> Result<String, password_hash::Error> {
    let salt = SaltString::generate(&mut password_hash::rand_core::OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub fn hash_api_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    crate::media_sign::hex(&hasher.finalize())
}
