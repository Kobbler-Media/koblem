//! What each engine hop actually put on the wire.
//!
//! Every item in the binary-transport plan was phrased as "the request body drops
//! from MB to KB", and nothing in `api/src` measured a request or a response, so none of it was
//! checkable — and, worse, a regression back to base64 was invisible. This module is the
//! instrument, and it is deliberately the cheapest thing that can be trusted:
//!
//! * [`WireForm`] wraps the `reqwest` multipart builder every engine client already uses and counts
//!   what it hands over. The count is of the *payloads*, not the encoded body — multipart framing
//!   adds a couple of hundred bytes per part and would only blur the number the plan cares about,
//!   which is "how much of this request is media, and is any of it base64".
//! * [`WireMeter`] hangs off each client and holds the most recent submit and the most recent media
//!   fetch, so the route can stamp them into the render-log row it was already writing. It is a
//!   *last-value* register, not a running total: the engines are single-flight behind the GPU gate,
//!   so the last submit on a client is this render's submit. A client that ever runs two renders
//!   concurrently must return the size from the call instead of reading it back.
//!
//! Both directions also emit a `tracing` line at `info`, because the DB row is only written when a
//! render finishes and a failed render is exactly when the size matters.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::multipart::{Form, Part};
use serde::Serialize;
use serde_json::{Value, json};

/// Payload bytes in one direction of one hop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct WireSize {
    /// The JSON document — the `request` part, or the whole body on a JSON-only route.
    pub json_bytes: u64,
    /// Everything that is media: binary parts out, artifact bytes in.
    pub media_bytes: u64,
    /// How many binary parts carried that media. `0` with a non-zero `media_bytes` means the media
    /// travelled *inside* `json_bytes` — i.e. base64, i.e. the thing this plan exists to remove.
    pub parts: u32,
}

impl WireSize {
    pub fn total(&self) -> u64 {
        self.json_bytes + self.media_bytes
    }
}

/// Per-client register: the last submit and the last media fetch.
///
/// Cloneable and shared with the client's `Clone` impl, so `state.ltx25.wire()` in a route sees
/// what the background render task recorded.
#[derive(Debug, Clone, Default)]
pub struct WireMeter {
    inner: Arc<Meter>,
}

#[derive(Debug, Default)]
struct Meter {
    req_json: AtomicU64,
    req_media: AtomicU64,
    req_parts: AtomicU64,
    /// Artifact bytes that arrived raw, from a `/media`-style route.
    resp_media: AtomicU64,
    /// Artifact bytes that arrived as base64 inside the poll body, counted DECODED so the two are
    /// comparable. Non-zero here is the §4.1 flag still being on.
    resp_media_b64: AtomicU64,
}

impl WireMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an outbound request and log it. `service`/`endpoint` are only used for the log line.
    pub fn submitted(&self, service: &str, endpoint: &str, size: WireSize) {
        self.inner
            .req_json
            .store(size.json_bytes, Ordering::Relaxed);
        self.inner
            .req_media
            .store(size.media_bytes, Ordering::Relaxed);
        self.inner
            .req_parts
            .store(u64::from(size.parts), Ordering::Relaxed);
        tracing::info!(
            target: "wire",
            service,
            endpoint,
            dir = "out",
            json_bytes = size.json_bytes,
            media_bytes = size.media_bytes,
            parts = size.parts,
            "engine submit"
        );
    }

    /// Record artifact bytes arriving raw (a `/media` fetch).
    pub fn fetched_raw(&self, service: &str, bytes: u64) {
        self.inner.resp_media.store(bytes, Ordering::Relaxed);
        self.inner.resp_media_b64.store(0, Ordering::Relaxed);
        tracing::info!(target: "wire", service, dir = "in", encoding = "raw", bytes, "engine media");
    }

    /// Record artifact bytes arriving base64-inlined in the poll body. `bytes` is the DECODED
    /// length; the wire cost was ~4/3 of it.
    pub fn fetched_inline_b64(&self, service: &str, bytes: u64) {
        self.inner.resp_media_b64.store(bytes, Ordering::Relaxed);
        self.inner.resp_media.store(0, Ordering::Relaxed);
        tracing::info!(
            target: "wire",
            service,
            dir = "in",
            encoding = "base64",
            bytes,
            wire_bytes = bytes.div_ceil(3) * 4,
            "engine media"
        );
    }

    pub fn request_size(&self) -> WireSize {
        WireSize {
            json_bytes: self.inner.req_json.load(Ordering::Relaxed),
            media_bytes: self.inner.req_media.load(Ordering::Relaxed),
            parts: self.inner.req_parts.load(Ordering::Relaxed) as u32,
        }
    }

    /// The `wire` object stamped into a render log row's `response_meta`.
    ///
    /// `request.parts == 0` with a fat `request.json_bytes` is the signature of base64 conditioning
    /// inputs; `response.inline_b64_bytes` non-zero is the signature of an inlined artifact. Those
    /// two numbers are the whole point of the field.
    pub fn as_meta(&self) -> Value {
        let req = self.request_size();
        json!({
            "request": {
                "json_bytes": req.json_bytes,
                "media_bytes": req.media_bytes,
                "parts": req.parts,
                "total_bytes": req.total(),
            },
            "response": {
                "media_bytes": self.inner.resp_media.load(Ordering::Relaxed),
                "inline_b64_bytes": self.inner.resp_media_b64.load(Ordering::Relaxed),
            },
        })
    }
}

/// Does this engine understand `part:<name>` media references?
///
/// §9.4 — B1 (engine accepts `part:`) has to reach every service before B2 (koblem sends it), and
/// nothing enforces deploy order. Roll an engine back while koblem is on the new path and the
/// marker is read as literal base64, fails to decode, and lands in whichever fallback that field
/// has. So koblem ASKS, on the route every sd.cpp service answers without waking its worker, and
/// falls back to inlining when the answer is no.
///
/// Cached after the first successful answer: it is a property of the deployed binary, and a render
/// is minutes long, but a service that has never answered is re-asked rather than assumed either
/// way. `None` (unreachable /health) means "assume not" for this submit — inlining works against
/// every engine ever deployed, and a marker sent to one that does not understand it does not.
#[derive(Debug, Clone, Default)]
pub struct PartSupport {
    known: Arc<std::sync::atomic::AtomicU8>,
}

const PART_SUPPORT_UNKNOWN: u8 = 0;
const PART_SUPPORT_NO: u8 = 1;
const PART_SUPPORT_YES: u8 = 2;

impl PartSupport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget what was learned — call when the service URL changes, since a different host is a
    /// different binary.
    pub fn reset(&self) {
        self.known.store(PART_SUPPORT_UNKNOWN, Ordering::Relaxed);
    }

    /// The cached answer, or `None` when nothing has been learned yet.
    pub fn cached(&self) -> Option<bool> {
        match self.known.load(Ordering::Relaxed) {
            PART_SUPPORT_YES => Some(true),
            PART_SUPPORT_NO => Some(false),
            _ => None,
        }
    }

    /// Record what a `/health` body said. A body without the key is an OLD engine, which is
    /// exactly the case this exists for — absent means no.
    pub fn observe(&self, health: &Value) -> bool {
        let yes = health
            .get("accepts_part_refs")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.known.store(
            if yes {
                PART_SUPPORT_YES
            } else {
                PART_SUPPORT_NO
            },
            Ordering::Relaxed,
        );
        yes
    }
}

/// What went wrong streaming an artifact to disk.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("reading the response body failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
}

/// Stream a response body straight into `dest`, never holding the artifact whole (§9.1).
///
/// The plan's §9.1 is blunt about this: killing the 1.33× base64 encode on a 589 MB clip still
/// leaves ~2× its size resident across two processes, because koblem reads the whole body into a
/// `Vec<u8>` and then writes that. A chunked copy costs one 8 KB-ish buffer instead.
///
/// Writes to a `.part` sibling and renames, so an interrupted transfer can never be mistaken for a
/// finished artifact by anything that only checks the path exists.
pub async fn stream_to_file(
    mut resp: reqwest::Response,
    dest: &std::path::Path,
) -> Result<u64, StreamError> {
    use tokio::io::AsyncWriteExt as _;

    let tmp = dest.with_extension("part");
    let write_err = |path: &std::path::Path| {
        let path = path.display().to_string();
        move |source: std::io::Error| StreamError::Write {
            path: path.clone(),
            source,
        }
    };
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(write_err(&tmp))?;
    let mut total = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        total += chunk.len() as u64;
        file.write_all(&chunk).await.map_err(write_err(&tmp))?;
    }
    file.flush().await.map_err(write_err(&tmp))?;
    drop(file);
    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(write_err(dest))?;
    Ok(total)
}

/// A `reqwest` multipart form that knows how big it is.
///
/// Same call shape as the raw builder so the engine clients change by one word, except that
/// [`json`](Self::json) replaces the `.text("request", to_string(..))` idiom all six of them had
/// copied.
pub struct WireForm {
    form: Form,
    size: WireSize,
}

impl WireForm {
    pub fn new() -> Self {
        Self {
            form: Form::new(),
            size: WireSize::default(),
        }
    }

    /// The `request` JSON part. Counts as JSON, not media — including any base64 hiding in it,
    /// which is exactly how the fat-JSON regression shows up.
    pub fn json(mut self, name: &str, value: &Value) -> Self {
        let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
        self.size.json_bytes += body.len() as u64;
        self.form = self.form.text(name.to_string(), body);
        self
    }

    /// The same JSON part, but sent **as a file** — a `filename` in its `Content-Disposition`.
    ///
    /// 🔴 THIS IS NOT A STYLE CHOICE. `cpp-httplib` sorts multipart parts by whether they carry a
    /// filename, and a server reading its request through `req.form.has_file("json")` **cannot see
    /// a filename-less part at all**. `longcat-rig`'s `parse_body` does exactly that: measured
    /// 2026-08-15, the identical `POST /v1/rig` was accepted with every field silently defaulted
    /// without a filename, and correctly rejected an unknown tier with one. Every knob — the tier
    /// list, `rig_seed`, the re-run handles — would have gone nowhere and the failure would have
    /// looked like a working feature.
    ///
    /// Still counted as JSON, because it is: the metering column follows what the bytes ARE, not
    /// which multipart bucket the receiver files them under.
    pub fn json_file(
        mut self,
        name: &str,
        file_name: &str,
        value: &Value,
    ) -> Result<Self, reqwest::Error> {
        let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
        self.size.json_bytes += body.len() as u64;
        let part = Part::text(body)
            .file_name(file_name.to_string())
            .mime_str("application/json")?;
        self.form = self.form.part(name.to_string(), part);
        Ok(self)
    }

    /// A binary part. This is the number the plan is trying to grow at the JSON column's expense.
    pub fn bytes(
        mut self,
        name: &str,
        file_name: impl Into<String>,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<Self, reqwest::Error> {
        self.size.media_bytes += bytes.len() as u64;
        self.size.parts += 1;
        let part = Part::bytes(bytes)
            .file_name(file_name.into())
            .mime_str(mime)?;
        self.form = self.form.part(name.to_string(), part);
        Ok(self)
    }

    pub fn size(&self) -> WireSize {
        self.size
    }

    pub fn into_form(self) -> Form {
        self.form
    }
}

impl Default for WireForm {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two columns must not blur into each other: a `part:` migration is only visible as JSON
    /// shrinking *while* media grows, so a builder that counted a binary part as JSON (or an
    /// inlined base64 blob as media) would report every state of the world as fine.
    #[test]
    fn the_builder_separates_json_from_media() {
        let doc = json!({ "prompt": "a cat", "init_image": "asset:ff" });
        let form = WireForm::new()
            .json("request", &doc)
            .bytes("a_00", "a.png", "image/png", vec![0u8; 4096])
            .expect("mime")
            .bytes("a_01", "b.wav", "audio/wav", vec![0u8; 1024])
            .expect("mime");
        let size = form.size();
        assert_eq!(size.media_bytes, 5120);
        assert_eq!(size.parts, 2);
        assert_eq!(
            size.json_bytes,
            serde_json::to_string(&doc).unwrap().len() as u64
        );
        assert_eq!(size.total(), size.json_bytes + 5120);
    }

    /// Base64 conditioning inputs are indistinguishable from a big prompt *unless* the part count
    /// is reported alongside the byte count. This is the regression guard the plan asked for.
    #[test]
    fn base64_in_the_document_reads_as_json_with_no_parts() {
        let inlined = json!({ "init_image": "A".repeat(40_000) });
        let form = WireForm::new().json("request", &inlined);
        assert_eq!(form.size().parts, 0);
        assert!(form.size().json_bytes > 40_000);
        assert_eq!(form.size().media_bytes, 0);
    }

    /// The meter is what the render-log row reads back, so both directions have to survive the
    /// round trip through `as_meta`, and an inline artifact must land in its own field rather than
    /// being counted as if it had arrived raw.
    #[test]
    fn the_meter_reports_both_directions() {
        let meter = WireMeter::new();
        meter.submitted(
            "ltx25",
            "generate",
            WireSize {
                json_bytes: 900,
                media_bytes: 12_345,
                parts: 3,
            },
        );
        meter.fetched_inline_b64("ltx25", 70_000_000);
        let meta = meter.as_meta();
        assert_eq!(meta["request"]["json_bytes"], 900);
        assert_eq!(meta["request"]["parts"], 3);
        assert_eq!(meta["request"]["total_bytes"], 13_245);
        assert_eq!(meta["response"]["inline_b64_bytes"], 70_000_000u64);
        assert_eq!(meta["response"]["media_bytes"], 0);

        // A later raw fetch replaces it rather than adding to it: the two fields are alternatives,
        // and a row showing both would be read as "we did it twice".
        meter.fetched_raw("ltx25", 70_000_000);
        let meta = meter.as_meta();
        assert_eq!(meta["response"]["media_bytes"], 70_000_000u64);
        assert_eq!(meta["response"]["inline_b64_bytes"], 0);
    }
}
