//! Signed media URLs — the replacement for `?token=` on gallery files (§6.1).
//!
//! `<img src>` cannot set an Authorization header, so the gallery file route grew a `?token=`
//! query fallback that takes a REAL API key. That key then lands in browser history, in the
//! `Referer` of any outbound link on the page, in nginx access logs — and, because agent replies
//! embed those URLs, in `agent_messages.steps` in Postgres. A long-lived credential in all four
//! places, to show a thumbnail.
//!
//! A signed URL has the same ergonomics and none of that: an HMAC over `(path, expiry)` with a
//! server-side secret, valid for minutes, and useless for anything except the one file it names.
//! It cannot mint renders, list the gallery, or read a different row.
//!
//! The header path stays primary and unchanged; this is only for element `src` attributes.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// How long a minted URL stays valid. Long enough to open a page, scrub a video and reload;
/// short enough that a leaked one in a log is worthless by the time anyone reads it.
pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

/// Holds the signing secret. One per process, in [`crate::AppState`].
pub struct MediaSigner {
    secret: Vec<u8>,
}

impl MediaSigner {
    /// Derive the secret from the server's API key when there is one, so signed URLs survive a
    /// restart (a browser holding a page open does not get 403s), and rotating the key
    /// invalidates every outstanding URL — which is what rotating a key should do.
    ///
    /// With no API key configured the secret is random per process: links then die on restart,
    /// which is correct-but-inconvenient rather than insecure, and that deployment has no
    /// credential to derive from anyway.
    pub fn new(api_key: Option<&str>) -> Self {
        let secret = match api_key.map(str::trim).filter(|k| !k.is_empty()) {
            // Domain-separated: the signing key must not BE the API key, so that a signature
            // cannot be replayed anywhere the raw key is expected.
            Some(key) => {
                let mut hasher = Sha256::new();
                hasher.update(b"koblem media url v1\0");
                hasher.update(key.as_bytes());
                hasher.finalize().to_vec()
            }
            None => uuid::Uuid::new_v4()
                .as_bytes()
                .iter()
                .chain(uuid::Uuid::new_v4().as_bytes())
                .copied()
                .collect(),
        };
        Self { secret }
    }

    /// `path` plus `?exp=…&sig=…`, valid for `ttl`.
    ///
    /// `path` must be the request path with no query string — the signature covers exactly what
    /// the route will verify, and a caller that appends its own parameters afterwards gets a URL
    /// that still validates (extra parameters are outside the signature by design; nothing else
    /// on this route reads any).
    pub fn sign(&self, path: &str, ttl: Duration) -> String {
        let exp = now() + ttl.as_secs() as i64;
        let sig = self.signature(path, exp);
        let sep = if path.contains('?') { '&' } else { '?' };
        format!("{path}{sep}exp={exp}&sig={sig}")
    }

    /// Is this a signature this server issued for this exact path, and is it still valid?
    pub fn verify(&self, path: &str, exp: i64, sig: &str) -> bool {
        if exp < now() {
            return false;
        }
        constant_time_eq(self.signature(path, exp).as_bytes(), sig.as_bytes())
    }

    fn signature(&self, path: &str, exp: i64) -> String {
        let mut message = Vec::with_capacity(path.len() + 24);
        message.extend_from_slice(path.as_bytes());
        message.push(b'\n');
        message.extend_from_slice(exp.to_string().as_bytes());
        hex(&hmac_sha256(&self.secret, &message))
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// HMAC-SHA256, by the book (RFC 2104).
///
/// Hand-rolled rather than pulled in: it is nine lines over a hasher the crate already has, and
/// the alternative was a dependency for one call site.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let mut inner_key = [0u8; BLOCK];
    let mut outer_key = [0u8; BLOCK];
    for i in 0..BLOCK {
        inner_key[i] = padded[i] ^ 0x36;
        outer_key[i] = padded[i] ^ 0x5c;
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner);
    outer.finalize().into()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Comparison that does not leak where two signatures first differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test case 2 — proves the hand-rolled HMAC is HMAC and not merely a hash of two
    /// concatenated things. Getting this subtly wrong would still "work" (sign and verify would
    /// agree) while being trivially forgeable.
    #[test]
    fn hmac_matches_the_published_vector() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// A key longer than the 64-byte block takes the hashed-key branch — RFC 4231 case 4 covers
    /// the short-key path, this one covers the branch a real API key can actually reach.
    #[test]
    fn a_long_key_is_hashed_first() {
        let mac = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_signed_url_validates_and_a_tampered_one_does_not() {
        let signer = MediaSigner::new(Some("secret-key"));
        let url = signer.sign("/api/v1/generations/g1/file/0", DEFAULT_TTL);
        let (path, query) = url.split_once('?').expect("query");
        assert_eq!(path, "/api/v1/generations/g1/file/0");
        let exp: i64 = query
            .split('&')
            .find_map(|p| p.strip_prefix("exp="))
            .expect("exp")
            .parse()
            .expect("exp is a number");
        let sig = query
            .split('&')
            .find_map(|p| p.strip_prefix("sig="))
            .expect("sig");

        assert!(signer.verify(path, exp, sig));
        // The signature is bound to the PATH: this is the whole point, and it is what a bearer
        // token in the query never gave us.
        assert!(!signer.verify("/api/v1/generations/g1/file/1", exp, sig));
        // ...and to the expiry, so nobody extends their own URL.
        assert!(!signer.verify(path, exp + 3600, sig));
        assert!(!signer.verify(path, exp, "deadbeef"));
        // A different server secret cannot mint URLs for this one.
        assert!(!MediaSigner::new(Some("other-key")).verify(path, exp, sig));
    }

    #[test]
    fn an_expired_url_is_refused() {
        let signer = MediaSigner::new(Some("secret-key"));
        let path = "/api/v1/generations/g1/file/0";
        let exp = now() - 1;
        let sig = signer.signature(path, exp);
        assert!(
            !signer.verify(path, exp, &sig),
            "a correctly signed but expired URL must still be refused"
        );
    }

    /// The signing secret must not be the API key itself, or a signature would be a credential
    /// in a different coat.
    #[test]
    fn the_secret_is_derived_not_the_key() {
        let signer = MediaSigner::new(Some("secret-key"));
        assert_ne!(signer.secret, b"secret-key".to_vec());
        assert_eq!(signer.secret.len(), 32);
    }
}
