//! Web Push — the half of the notifier that reaches a phone with the browser closed.
//!
//! Same grain as the webhook, deliberately: batch completion, an individual failure, and
//! blocked-too-long. Progress never gets here, for the same reason it never gets to the webhook —
//! a 16-generation H3 job would be sixteen lock-screen interruptions for one render.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde::Serialize;
use serde_json::json;
use tracing::{debug, warn};
use web_push_native::jwt_simple::algorithms::{ECDSAP256PublicKeyLike, ES256KeyPair};
use web_push_native::p256::PublicKey;
use web_push_native::{Auth, WebPushBuilder};

use crate::repositories::{push_subscriptions as store, settings as settings_repo};

pub const PRIVATE_KEY: &str = "push_vapid_private_key";
pub const PUBLIC_KEY: &str = "push_vapid_public_key";
pub const SUBJECT_KEY: &str = "push_vapid_subject";

/// RFC8292 wants a `sub` a push service operator could contact. Nothing routes here; it exists
/// because Firefox rejects a VAPID JWT without one.
const DEFAULT_SUBJECT: &str = "mailto:koblem@localhost";

/// How long a push may sit on the push service before it is dropped, and how long the VAPID JWT
/// stays valid. A render result is stale long before this; twelve hours is only so a phone that was
/// off overnight still learns the queue drained.
const TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// What a service worker receives. The tag is load-bearing — see [`Push::send`].
#[derive(Debug, Clone, Serialize)]
pub struct PushNote {
    pub title: String,
    pub body: String,
    pub tag: String,
}

pub struct Vapid {
    key_pair: ES256KeyPair,
    pub public_key: String,
    pub subject: String,
}

/// The persisted keypair, or nothing. Never mints — see [`ensure_vapid`].
pub async fn load_vapid(pool: &sqlx::PgPool) -> Option<Vapid> {
    let subject = read_string(pool, SUBJECT_KEY)
        .await
        .unwrap_or_else(|| DEFAULT_SUBJECT.to_string());
    from_private(&read_string(pool, PRIVATE_KEY).await?, &subject)
}

/// Mints and persists a keypair on first boot, so browser push needs no hand configuration.
///
/// 🔴 The ONLY minter, and it runs once at startup rather than lazily per send: two concurrent
/// generations would each persist a different private key, and every subscription already taken
/// against the loser's public key would be undeliverable forever.
pub async fn ensure_vapid(pool: &sqlx::PgPool) -> Option<Vapid> {
    if let Some(vapid) = load_vapid(pool).await {
        return Some(vapid);
    }
    let subject = read_string(pool, SUBJECT_KEY)
        .await
        .unwrap_or_else(|| DEFAULT_SUBJECT.to_string());

    let key_pair = ES256KeyPair::generate();
    let private = URL_SAFE_NO_PAD.encode(key_pair.to_bytes());
    let public = URL_SAFE_NO_PAD.encode(key_pair.public_key().public_key().to_bytes_uncompressed());
    if let Err(e) = settings_repo::set_setting(pool, PRIVATE_KEY, &json!(private)).await {
        warn!(error = %e, "push: could not persist the VAPID private key");
        return None;
    }
    if let Err(e) = settings_repo::set_setting(pool, PUBLIC_KEY, &json!(public)).await {
        warn!(error = %e, "push: could not persist the VAPID public key");
        return None;
    }
    debug!("push: minted a VAPID keypair");
    Some(Vapid {
        key_pair,
        public_key: public,
        subject,
    })
}

/// P-256 scalar width. Checked here because `ES256KeyPair::from_bytes` PANICS on anything else
/// rather than erroring, and a truncated settings row would take the whole process down at boot.
const PRIVATE_KEY_LEN: usize = 32;

fn from_private(raw: &str, subject: &str) -> Option<Vapid> {
    let bytes = decode_b64(raw)?;
    if bytes.len() != PRIVATE_KEY_LEN {
        warn!("push: the stored VAPID private key is not a P-256 scalar; browser push is off");
        return None;
    }
    let key_pair = ES256KeyPair::from_bytes(&bytes).ok()?;
    let public = URL_SAFE_NO_PAD.encode(key_pair.public_key().public_key().to_bytes_uncompressed());
    Some(Vapid {
        key_pair,
        public_key: public,
        subject: subject.to_string(),
    })
}

pub struct Push {
    pool: sqlx::PgPool,
    http: reqwest::Client,
    vapid: Vapid,
}

impl Push {
    pub fn new(pool: sqlx::PgPool, http: reqwest::Client, vapid: Vapid) -> Self {
        Push { pool, http, vapid }
    }

    /// Fans one note out to every registered browser.
    ///
    /// 🔴 `note.tag` reuses the client's `queue:<job_id>:<state>` scheme on purpose. A tab that is
    /// open but hidden gets the same event down SSE and posts its own notification; browsers
    /// replace a same-tag notification rather than stacking it, so the two collapse into one.
    pub async fn send(&self, note: &PushNote) {
        let subs = match store::all(&self.pool).await {
            Ok(subs) => subs,
            Err(e) => {
                warn!(error = %e, "push: could not read subscriptions");
                return;
            }
        };
        let Ok(body) = serde_json::to_vec(note) else {
            return;
        };

        for sub in subs {
            let Some(request) = self.build(&sub.endpoint, &sub.p256dh, &sub.auth, &body) else {
                // Undecodable keys are not a delivery failure; the row can never work again.
                warn!(endpoint = %sub.endpoint, "push: unusable subscription keys; dropping it");
                let _ = store::remove(&self.pool, &sub.endpoint).await;
                continue;
            };
            self.deliver(&sub.endpoint, request).await;
        }
    }

    fn build(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        body: &[u8],
    ) -> Option<http::Request<Vec<u8>>> {
        let uri = endpoint.parse().ok()?;
        let ua_public = PublicKey::from_sec1_bytes(&decode_b64(p256dh)?).ok()?;
        let secret: [u8; 16] = decode_b64(auth)?.try_into().ok()?;
        WebPushBuilder::new(uri, ua_public, Auth::from(secret))
            .with_valid_duration(TTL)
            .with_vapid(&self.vapid.key_pair, &self.vapid.subject)
            .build(body.to_vec())
            .ok()
    }

    async fn deliver(&self, endpoint: &str, request: http::Request<Vec<u8>>) {
        let (parts, body) = request.into_parts();
        let sent = self
            .http
            .post(parts.uri.to_string())
            .headers(parts.headers)
            .body(body)
            .send()
            .await;

        match sent {
            Ok(r) if r.status().is_success() => {
                let _ = store::mark_delivered(&self.pool, endpoint).await;
            }
            // 🔴 404/410 is the push service saying this subscription is permanently gone — the
            // browser was uninstalled, or the user cleared site data. Delete it, or every future
            // send retries a corpse forever. Everything else is transient and only counts.
            Ok(r) if is_gone(r.status().as_u16()) => {
                debug!(endpoint = %endpoint, status = r.status().as_u16(), "push: subscription is gone; pruned");
                let _ = store::remove(&self.pool, endpoint).await;
            }
            Ok(r) => {
                warn!(status = r.status().as_u16(), "push: delivery refused");
                let _ = store::mark_failed(&self.pool, endpoint).await;
            }
            Err(e) => {
                warn!(error = %e, "push: delivery failed");
                let _ = store::mark_failed(&self.pool, endpoint).await;
            }
        }
    }
}

fn is_gone(status: u16) -> bool {
    status == 404 || status == 410
}

/// The Push API hands the browser's keys over as raw bytes; whether the page base64url-encodes them
/// padded is up to whoever wrote the page, so both are accepted.
fn decode_b64(raw: &str) -> Option<Vec<u8>> {
    let trimmed = raw.trim();
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| URL_SAFE.decode(trimmed))
        .ok()
}

async fn read_string(pool: &sqlx::PgPool, key: &str) -> Option<String> {
    settings_repo::get_setting(pool, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_permanently_gone_endpoint_is_pruned() {
        assert!(is_gone(404));
        assert!(is_gone(410));
        for transient in [201, 400, 401, 413, 429, 500, 502, 503] {
            assert!(!is_gone(transient), "{transient} must not delete the row");
        }
    }

    #[test]
    fn subscription_keys_decode_padded_or_not() {
        let raw = [1u8, 2, 3, 4, 5];
        assert_eq!(decode_b64(&URL_SAFE_NO_PAD.encode(raw)), Some(raw.to_vec()));
        assert_eq!(decode_b64(&URL_SAFE.encode(raw)), Some(raw.to_vec()));
        assert_eq!(decode_b64("not base64 at all!!"), None);
    }

    #[test]
    fn a_minted_keypair_round_trips_through_the_settings_value() {
        let generated = ES256KeyPair::generate();
        let private = URL_SAFE_NO_PAD.encode(generated.to_bytes());
        let vapid = from_private(&private, DEFAULT_SUBJECT).expect("re-reads its own private key");
        assert_eq!(
            vapid.public_key,
            URL_SAFE_NO_PAD.encode(generated.public_key().public_key().to_bytes_uncompressed())
        );
        // The applicationServerKey the browser subscribes with is an uncompressed P-256 point.
        assert_eq!(URL_SAFE_NO_PAD.decode(&vapid.public_key).unwrap().len(), 65);
    }

    /// The wrong length is not an error inside jwt-simple, it is an assertion — so a truncated or
    /// hand-edited settings row must be caught before it ever reaches `from_bytes`.
    #[test]
    fn a_corrupt_private_key_is_refused_rather_than_panicking() {
        assert!(from_private("", DEFAULT_SUBJECT).is_none());
        assert!(from_private("////", DEFAULT_SUBJECT).is_none());
        assert!(from_private(&URL_SAFE_NO_PAD.encode([0u8; 8]), DEFAULT_SUBJECT).is_none());
        assert!(from_private(&URL_SAFE_NO_PAD.encode([7u8; 64]), DEFAULT_SUBJECT).is_none());
    }
}
