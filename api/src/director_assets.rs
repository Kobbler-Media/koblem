//! Content-addressed store for Director editor assets, and the `asset:<hash>` reference the
//! saved timeline carries in place of inline base64.
//!
//! # Why
//!
//! Every image / clip / audio the editor touches used to be stored as base64 INSIDE the project
//! document. The document is autosaved 2.5s after any edit, re-inlined into every render request,
//! snapshotted for undo, and mirrored into localStorage — so a project's whole asset payload was
//! being re-serialised on all four paths continuously. Cost tracked the assets attached to a
//! project rather than its size.
//!
//! Assets now live here once, addressed by the SHA-256 of their bytes, and the document carries a
//! ~70-byte `asset:<hash>` reference. Content addressing buys three things for free: the store
//! de-duplicates (one still reused across ten shots is one file), an upload of bytes already held
//! is a metadata touch with no write, and every asset URL is permanently immutable, so the editor
//! and the browser can cache them forever.
//!
//! # Reading them back
//!
//! Requests are normalised at the edge ([`expand_refs`]): a ref is swapped for the base64 it
//! stands for before the typed request is deserialised, so every downstream decode point — render
//! compile, LLM vision calls, keyframe pins — keeps seeing exactly what it saw before and needed
//! no changes. Legacy documents whose fields still hold literal base64 pass through untouched.
//!
//! Every route that deserialises a document-derived body therefore goes through [`AssetJson`] (or
//! [`parse_asset_json`], for the multipart submits), and a ref the store cannot resolve fails the
//! request with [`missing_refs_message`] instead of travelling on. Both rules are load-bearing:
//! `prompt-from-llm` was left on a plain `Json` and handed llama-server `asset:<hash>` as an image
//! (`Failed to load image or audio file`), and the engine's answer to an unusable image is a
//! body-less 400 — neither says which shot, or that an asset is what's missing.

use std::path::{Path, PathBuf};

use base64::Engine;
use bytes::Bytes;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Marker for a stored asset. The remainder is a lowercase hex SHA-256.
pub const REF_PREFIX: &str = "asset:";

/// SHA-256 of `bytes`, lowercase hex — an asset's identity and its filename.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    crate::media_sign::hex(&hasher.finalize())
}

/// The reference form stored in a document.
pub fn make_ref(hash: &str) -> String {
    format!("{REF_PREFIX}{hash}")
}

/// The hash inside an `asset:` reference, if `s` is one and the hash is well formed.
///
/// The shape check is the confinement guard: a hash becomes a path component, so only 64 hex
/// characters are ever accepted and a crafted ref can't traverse out of the store.
pub fn ref_hash(s: &str) -> Option<&str> {
    let hash = s.strip_prefix(REF_PREFIX)?;
    let ok = hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    ok.then_some(hash)
}

/// Absolute path of an asset's bytes. `None` for anything that isn't a well-formed ref.
pub fn path_for(dir: &Path, hash: &str) -> Option<PathBuf> {
    let ok = hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    ok.then(|| dir.join(hash))
}

/// Write `bytes` into the store. A no-op when these exact bytes are already present — the file is
/// named by its own hash, so an existing file cannot disagree with what we would have written.
pub async fn write_bytes(dir: &Path, hash: &str, bytes: &[u8]) -> std::io::Result<()> {
    let Some(path) = path_for(dir, hash) else {
        return Err(std::io::Error::other("bad asset hash"));
    };
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(());
    }
    tokio::fs::create_dir_all(dir).await?;
    // Write-then-rename so a crash mid-write can't leave a truncated file under a hash that
    // claims to be the whole thing.
    let tmp = dir.join(format!(".{hash}.{}", uuid::Uuid::now_v7()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, &path).await
}

/// An asset's bytes, or `None` when the store doesn't hold it.
pub async fn read_bytes(dir: &Path, hash: &str) -> Option<Bytes> {
    let path = path_for(dir, hash)?;
    tokio::fs::read(path).await.ok().map(Bytes::from)
}

/// Why a document field did not yield bytes, when it was not simply blank.
///
/// 🔴 These two used to share [`resolve_media`]'s `Ok(None)` with "nothing attached", and the H3
/// render path reported that arm as *"the project soundtrack asset is missing from the store"* —
/// the one thing it could not be, because a lost ref returns `Err`. Project `7250d8df` rendered a
/// music video with no song and printed exactly that while the store held the file. Whoever reads
/// the message next has to know whether to re-attach the asset (`Missing`) or to go and look at
/// what the edge inlined into the field (`Undecodable`); collapsing them sent that investigation
/// to the wrong repo for a day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaFault {
    /// A well-formed `asset:<hash>` the store no longer holds — swept, or never written. The hash
    /// is the only handle on WHICH asset, so it is carried rather than formatted away.
    Missing(String),
    /// Non-empty, not a ref, and not base64 either. The length is the whole diagnostic: a handful
    /// of bytes is a truncated field, megabytes is a payload that arrived intact and got mangled
    /// (a data URI whose prefix was rewritten, a raw binary body serialised as a string).
    Undecodable { bytes: usize },
}

impl std::fmt::Display for MediaFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(hash) => write!(f, "asset {hash} is missing from the store"),
            Self::Undecodable { bytes } => {
                write!(f, "the payload is not decodable base64 ({bytes} bytes)")
            }
        }
    }
}

impl std::error::Error for MediaFault {}

/// The bytes a document field stands for — an `asset:<hash>` read straight out of the store, or
/// literal base64 decoded. `Ok(None)` means the field is BLANK and nothing more: no asset was
/// attached. Everything else that fails is a [`MediaFault`] naming which of the two ways it failed.
///
/// This is the path for fields whose consumer wants BYTES (the multipart render submits). Going
/// through [`expand_refs`] instead would read the file, base64 it, and hand back a string the caller
/// immediately decodes again — a 1.33x allocation and two passes over every still, clip and audio
/// track in the project, for nothing.
pub async fn resolve_media(dir: &Path, s: &str) -> Result<Option<Bytes>, MediaFault> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if let Some(hash) = ref_hash(s) {
        return read_bytes(dir, hash)
            .await
            .map(Some)
            .ok_or_else(|| MediaFault::Missing(hash.to_string()));
    }
    let payload = s.rsplit_once(";base64,").map(|(_, b)| b).unwrap_or(s);
    match base64::engine::general_purpose::STANDARD.decode(payload.trim()) {
        Ok(bytes) => Ok(Some(Bytes::from(bytes))),
        // The TRIMMED FIELD's length, not the payload's — the question a reader has is how much
        // arrived, and a `data:` prefix is part of what arrived.
        Err(_) => Err(MediaFault::Undecodable { bytes: s.len() }),
    }
}

/// Rewrite every `asset:<hash>` string ANYWHERE in `value` into the base64 of its bytes, in place.
/// Returns the hashes the store could not resolve (deduplicated, document order) — empty on the
/// happy path.
///
/// Walking the JSON generically rather than naming fields is deliberate: the editor puts assets in
/// a dozen places (opener image, per-shot scene images, keyframe pins, references, v2v sources,
/// chain audio, per-shot audio) and new ones keep appearing. A structural pass covers all of them,
/// including any added later, and cannot drift out of step with the request types.
///
/// A ref the store can't resolve is REPORTED rather than passed on. It used to be left in place to
/// "fail at the same decode point as any other unusable value", but the decode points are the LLM
/// and the render engine: llama-server answered `Failed to load image or audio file` and the engine
/// a body-less 400, neither naming the asset or the shot. Every caller must therefore treat a
/// non-empty return as a failed request — see [`missing_refs_message`].
pub async fn expand_refs(dir: &Path, value: &mut Value) -> Vec<String> {
    let mut missing = Vec::new();
    expand_walk(dir, value, &mut missing).await;
    missing
}

async fn expand_walk(dir: &Path, value: &mut Value, missing: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            // Copied out so the immutable borrow of `s` ends before the rewrite below.
            let Some(hash) = ref_hash(s).map(str::to_owned) else {
                return;
            };
            match read_bytes(dir, &hash).await {
                Some(bytes) => *s = base64::engine::general_purpose::STANDARD.encode(&bytes),
                None => {
                    if !missing.contains(&hash) {
                        missing.push(hash);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                Box::pin(expand_walk(dir, item, missing)).await;
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                Box::pin(expand_walk(dir, v, missing)).await;
            }
        }
        _ => {}
    }
}

/// Marker a staged asset becomes in the request JSON, in place of the base64 [`expand_refs`] would
/// have written there. The remainder is the multipart field name carrying the bytes.
pub const PART_PREFIX: &str = "part:";

/// The multipart field name inside a `part:<name>` marker, if `s` is one.
pub fn part_name(s: &str) -> Option<&str> {
    s.strip_prefix(PART_PREFIX).filter(|n| !n.is_empty())
}

/// What [`stage_refs`] pulled out of a request: the bytes behind each `part:<name>` marker it wrote.
///
/// A lookup table, NOT the upload list. Which parts actually get attached is decided by the caller
/// that walks the typed request afterwards — a document can carry an asset in a field the engine
/// does not read, and staging it must not put it on the wire.
#[derive(Debug, Default)]
pub struct StagedParts {
    /// Name → bytes, in first-seen traversal order. One entry per distinct hash.
    entries: Vec<(String, Bytes)>,
}

#[cfg(test)]
impl StagedParts {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl StagedParts {
    /// The bytes a marker's name stands for. Linear: a request carries a handful of distinct
    /// assets, and the order is worth keeping stable for the error messages.
    pub fn bytes(&self, name: &str) -> Option<&Bytes> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, b)| b)
    }

    /// The parts a document actually uses, in traversal order: `(name, bytes)` for every `part:`
    /// marker still present in `value`, deduplicated.
    ///
    /// This is the upload list, and it is derived from the FINAL document rather than from what
    /// was staged — the two differ. A route may stage a whole saved timeline and then send the
    /// engine a subset of it (a retake sends one shot's chain), and shipping the assets of shots
    /// that are not in the request would put megabytes on the wire for nothing.
    pub fn used_by(&self, value: &Value) -> Vec<(String, Bytes)> {
        let mut names = Vec::new();
        collect_part_names(value, &mut names);
        names
            .into_iter()
            .filter_map(|name| self.bytes(&name).map(|bytes| (name, bytes.clone())))
            .collect()
    }
}

fn collect_part_names(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if let Some(name) = part_name(s)
                && !out.iter().any(|n| n == name)
            {
                out.push(name.to_string());
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_part_names(item, out)),
        Value::Object(map) => map.values().for_each(|v| collect_part_names(v, out)),
        _ => {}
    }
}

/// The media parts of ONE engine submit: what will be attached, and the markers the document
/// holds in their place.
///
/// Two sources feed it, and they are different in kind:
///
/// * assets [`stage_refs`] pulled out of the caller's document, which arrive already named;
/// * bytes koblem produced itself on the way to the engine — an upload it resolved, a control
///   frame it decoded — which used to be base64-encoded straight into the request.
///
/// Both end up as binary parts, deduplicated by CONTENT: the same picture used as the opener and
/// again as shot 4's scene image is one upload, and a retake presents byte-identical conditioning
/// by construction because the name is derived from the bytes rather than from a position.
#[derive(Debug, Default)]
pub struct PartUploads {
    entries: Vec<(String, Bytes)>,
}

impl PartUploads {
    /// Add every staged asset the FINISHED document still references.
    ///
    /// Called last, once the request is fully built, because what the engine is being sent is not
    /// always what the caller staged: a retake stages a whole saved timeline and submits one
    /// shot's chain, and uploading the other shots' stills would put megabytes on the wire for
    /// nothing.
    pub fn extend_from_document(&mut self, value: &Value, staged: &StagedParts) {
        for (name, bytes) in staged.used_by(value) {
            if !self.entries.iter().any(|(n, _)| *n == name) {
                self.entries.push((name, bytes));
            }
        }
    }

    /// Add bytes koblem itself produced and return the `part:<name>` marker to put in the
    /// document in their place.
    ///
    /// Named `a_<first 16 hex of the sha256>`, which is also the shape the engine verifies on
    /// receipt (§9.3) — so a truncated upload is a 400 naming the part rather than a subtly wrong
    /// render. Adding the same bytes twice returns the same marker and uploads once.
    pub fn add(&mut self, bytes: Bytes) -> String {
        let hash = hash_bytes(&bytes);
        let name = format!("a_{}", &hash[..16]);
        if !self.entries.iter().any(|(n, _)| *n == name) {
            self.entries.push((name.clone(), bytes));
        }
        format!("{PART_PREFIX}{name}")
    }

    /// Take over everything a parse staged, whether or not the request ends up using it.
    ///
    /// Pair with [`retain_used_by`](Self::retain_used_by) once the request is built: absorbing
    /// first keeps the marker→bytes mapping available while the body is being assembled, and the
    /// filter afterwards is what stops an unused asset being uploaded.
    pub fn absorb(&mut self, staged: StagedParts) {
        for (name, bytes) in staged.entries {
            if !self.entries.iter().any(|(n, _)| *n == name) {
                self.entries.push((name, bytes));
            }
        }
    }

    /// Drop every part the finished request does not reference.
    pub fn retain_used_by(&mut self, value: &Value) {
        let mut names = Vec::new();
        collect_part_names(value, &mut names);
        self.entries.retain(|(name, _)| names.contains(name));
    }

    pub fn entries(&self) -> &[(String, Bytes)] {
        &self.entries
    }
}

/// Put part bytes BACK into the document as base64, undoing the staging for one request.
///
/// The rollback half of §9.4: koblem asks each engine whether it understands `part:`, and one
/// that does not must receive the request it has always received rather than a marker it will
/// read as a corrupt image. Without this, rolling an engine back while koblem is on the new path
/// breaks every render carrying conditioning — the exact failure the capability flag exists to
/// prevent, which needs both halves to work.
///
/// A marker with no matching part is left alone: it is not ours, and rewriting it to an empty
/// string would turn a nameable error into a blank image.
pub fn inline_parts(value: &mut Value, parts: &[(String, Bytes)]) {
    match value {
        Value::String(s) => {
            if let Some(name) = part_name(s)
                && let Some((_, bytes)) = parts.iter().find(|(n, _)| n == name)
            {
                *s = base64::engine::general_purpose::STANDARD.encode(bytes);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| inline_parts(item, parts)),
        Value::Object(map) => map.values_mut().for_each(|v| inline_parts(v, parts)),
        _ => {}
    }
}

/// Rewrite every `asset:<hash>` string ANYWHERE in `value` into a `part:<name>` marker, returning
/// the bytes each marker stands for. The staging counterpart of [`expand_refs`], for the engines
/// that take their media as multipart parts rather than inline base64.
///
/// The walk is structural for the same reason [`expand_refs`]'s is: the editor puts assets in a
/// dozen places and new ones keep appearing, so naming fields here would guarantee drift. Identical
/// traversal, identical missing-ref accounting — only the rewrite target differs.
///
/// **Deduplicated by hash.** One character reference used across ten shots is one part, because it
/// is one hash. That is the whole reason the store is content-addressed, and it is what stops a
/// retake re-uploading conditioning it already sent.
///
/// Names are `<prefix><n>` over DISTINCT assets in traversal order — deterministic, since
/// `serde_json::Map` is a `BTreeMap` here (no `preserve_order`) and arrays keep their index. The
/// same document therefore stages to the same names on every run, which is what lets a retake
/// present byte-identical conditioning to the LTX bank. `prefix` exists because the engines
/// disagree: H3's contract
/// documents `ref_<n>` and its shim keeps a legacy alias keyed on that prefix
/// (`docker/h3-wangp/app/main.py:388-391`).
///
/// `Err` carries the hashes the store could not resolve, deduplicated in document order — hand it
/// to [`missing_refs_message`]. Reporting rather than passing them on is load-bearing for the same
/// reason it is in [`expand_refs`]: a ref left in place reaches the engine as an unusable string and
/// comes back as a body-less 400 naming nothing.
pub async fn stage_refs(
    dir: &Path,
    value: &mut Value,
    prefix: &str,
) -> Result<StagedParts, Vec<String>> {
    let mut staged = StagedParts::default();
    let mut by_hash: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut missing = Vec::new();
    // 🔴 The DELIVERY soundtrack is lifted out before the walk and put back after, because it is
    // not a reference: the shim's request model has no top-level `soundtrack` field at all. The
    // engine hears the song as per-generation `<Audio N>` SLICES; this block is koblem's own copy
    // of the original track, read back AFTER the render to mux over the finished cut.
    //
    // The walk is structural on purpose — it covers reference images, video containers and
    // reference audio without naming any of them — and that generality reached this field too:
    // `asset:<hash>` became `part:ref_2`, a ten-byte part NAME, and the mux then tried to
    // base64-decode it. Measured 2026-08-09 on job ab5602e0: the mux failed, and since a mux
    // failure is deliberately not a job failure, the render shipped carrying the MODEL'S OWN
    // audio — a reconstruction of the song rather than the song, audible as the tail "going funny".
    //
    // ⚠️ Lifted at the TOP LEVEL only, and by removing rather than by matching the key inside
    // `stage_walk`: this function also serves the LTX paths, and a key named `soundtrack` nested
    // somewhere that DOES need staging must keep being staged.
    let delivery_soundtrack = value
        .as_object_mut()
        .and_then(|map| map.remove("soundtrack"));
    stage_walk(dir, value, prefix, &mut staged, &mut by_hash, &mut missing).await;
    if let (Some(map), Some(st)) = (value.as_object_mut(), delivery_soundtrack) {
        map.insert("soundtrack".to_string(), st);
    }
    if missing.is_empty() {
        Ok(staged)
    } else {
        Err(missing)
    }
}

async fn stage_walk(
    dir: &Path,
    value: &mut Value,
    prefix: &str,
    staged: &mut StagedParts,
    by_hash: &mut std::collections::HashMap<String, String>,
    missing: &mut Vec<String>,
) {
    match value {
        Value::String(s) => {
            // Copied out so the immutable borrow of `s` ends before the rewrite below.
            let Some(hash) = ref_hash(s).map(str::to_owned) else {
                return;
            };
            // Second and later uses of the same asset point at the part the first use created.
            if let Some(name) = by_hash.get(&hash) {
                *s = format!("{PART_PREFIX}{name}");
                return;
            }
            match read_bytes(dir, &hash).await {
                Some(bytes) => {
                    let name = format!("{prefix}{}", staged.entries.len());
                    *s = format!("{PART_PREFIX}{name}");
                    by_hash.insert(hash, name.clone());
                    staged.entries.push((name, bytes));
                }
                None => {
                    if !missing.contains(&hash) {
                        missing.push(hash);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                Box::pin(stage_walk(dir, item, prefix, staged, by_hash, missing)).await;
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                Box::pin(stage_walk(dir, v, prefix, staged, by_hash, missing)).await;
            }
        }
        _ => {}
    }
}

/// The 400 message for refs [`expand_refs`] could not resolve. Hashes are truncated to their first
/// 12 characters — enough to match against the store or a `GET /assets/{hash}` URL, short enough to
/// read. Says what to do about it, because the only fix is on the editor side: re-attach the file.
pub fn missing_refs_message(missing: &[String]) -> String {
    let list: Vec<&str> = missing
        .iter()
        .map(|h| &h[..h.len().min(12)])
        .take(8)
        .collect();
    let more = missing.len().saturating_sub(list.len());
    format!(
        "{} referenced asset(s) are no longer in the store ({}{}) — re-attach the image / audio / clip on the affected shot(s)",
        missing.len(),
        list.join(", "),
        if more > 0 {
            format!(", +{more} more")
        } else {
            String::new()
        },
    )
}

/// The stored content type for bytes, from their magic number. Only used for what a handler stores
/// on the client's behalf (an upload names its own type); `GET /assets/{hash}` serves it back, so a
/// wrong guess is a browser hint, not a decode failure.
fn sniff_content_type(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', ..] if bytes.get(8..12) == Some(&b"WEBP"[..]) => "image/webp",
        [b'R', b'I', b'F', b'F', ..] if bytes.get(8..12) == Some(&b"WAVE"[..]) => "audio/wav",
        [b'I', b'D', b'3', ..] | [0xFF, 0xFB, ..] => "audio/mpeg",
        [b'O', b'g', b'g', b'S', ..] => "audio/ogg",
        [b'f', b'L', b'a', b'C', ..] => "audio/flac",
        [0x1A, 0x45, 0xDF, 0xA3, ..] => "video/webm",
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// The `asset:<hash>` ref standing for `bytes`, storing them first if the store doesn't already
/// hold them.
///
/// 🔴 THE CAPTURE EDGE FOR AN ENQUEUE. A queued render dispatches hours after the request that
/// carried its images, so an upload cannot stay a multipart part and a URL cannot stay a URL —
/// the bytes have to become something the row can name. Content addressing makes a re-uploaded
/// file a metadata touch with no write.
///
/// Unlike [`ref_for_b64`] this FAILS rather than falling back: there is nothing to fall back to,
/// because the caller is about to write a row that must still resolve tomorrow.
pub async fn ref_for_bytes(dir: &Path, db: &sqlx::PgPool, bytes: &[u8]) -> Result<String, String> {
    let hash = hash_bytes(bytes);
    write_bytes(dir, &hash, bytes)
        .await
        .map_err(|e| format!("storing the asset: {e}"))?;
    // The row is what `GET /assets/{hash}` resolves and what the sweep accounts for: a file with
    // no row would serve a 404 and never be collected.
    crate::repositories::director_assets::upsert(
        db,
        &hash,
        sniff_content_type(bytes),
        bytes.len() as i64,
    )
    .await
    .map_err(|e| format!("recording the asset: {e}"))?;
    Ok(make_ref(&hash))
}

/// The `asset:<hash>` ref standing for `b64`'s bytes, storing them first if the store doesn't
/// already hold them. The inverse of [`expand_refs`].
///
/// For the one handler that MANUFACTURES a timeline document — the auto-director's `plan`. It is
/// handed bytes (it needs them for vision + STT) but must emit refs: echoing the base64 back into
/// the document it produces re-inlines every reference image and the whole song into a project that
/// is then autosaved, undo-snapshotted and mirrored to localStorage, which is exactly the cost this
/// store exists to remove.
///
/// Content addressing makes the round-trip exact rather than duplicating: bytes the editor already
/// uploaded hash to the file that is already there, so this is a metadata touch and no write.
/// Returns `b64` unchanged if it can't be decoded or stored — a document holding literal base64
/// still works on every path, so a failure here costs size, not correctness.
pub async fn ref_for_b64(dir: &Path, db: &sqlx::PgPool, b64: &str) -> String {
    let payload = b64.trim();
    if payload.is_empty() || ref_hash(payload).is_some() {
        return b64.to_string();
    }
    match capture_ref(dir, db, payload).await {
        Ok(reference) => reference,
        Err(e) => {
            tracing::warn!(error = %e, "director asset store-back failed; leaving base64 inline");
            b64.to_string()
        }
    }
}

/// The `asset:<hash>` a render input becomes on the way INTO the global queue.
///
/// 🔴 An enqueued job can sit for eight hours, and the browser that POSTed the multipart is long
/// gone by dispatch time. Uploaded bytes therefore have to reach job-owned storage on the way in,
/// which is this store — the adapters already call [`stage_refs`] at dispatch to turn the refs
/// back into engine parts, so a captured input survives a queue wait and a koblem restart without
/// megabytes of base64 sitting in a JSONB column.
///
/// An `asset:` ref is passed through, but only after checking the store still holds it: a ref that
/// has already been swept must fail the submit, not the render six hours later.
pub async fn capture_ref(dir: &Path, db: &sqlx::PgPool, s: &str) -> Result<String, String> {
    let s = s.trim();
    if let Some(hash) = ref_hash(s) {
        return match read_bytes(dir, hash).await {
            Some(_) => Ok(s.to_string()),
            None => Err(missing_refs_message(&[hash.to_string()])),
        };
    }
    // Tolerate a `data:<mime>;base64,` prefix, as every other decode point here does.
    let raw = s
        .rsplit_once(";base64,")
        .map(|(_, b)| b)
        .unwrap_or(s)
        .trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .map_err(|_| MediaFault::Undecodable { bytes: s.len() }.to_string())?;
    capture_bytes(dir, db, &bytes).await
}

/// [`capture_ref`] for bytes koblem was handed directly — a multipart file part, which never went
/// through base64 and must not start now.
pub async fn capture_bytes(dir: &Path, db: &sqlx::PgPool, bytes: &[u8]) -> Result<String, String> {
    let hash = hash_bytes(bytes);
    write_bytes(dir, &hash, bytes)
        .await
        .map_err(|e| format!("storing an uploaded file: {e}"))?;
    // The row is what `GET /assets/{hash}` resolves and what the sweep accounts for: a file with no
    // row would serve a 404 to the editor and never be collected.
    crate::repositories::director_assets::upsert(
        db,
        &hash,
        sniff_content_type(bytes),
        bytes.len() as i64,
    )
    .await
    .map_err(|e| format!("recording an uploaded file: {e}"))?;
    Ok(make_ref(&hash))
}

/// `Json<T>`, but with any `asset:<hash>` string in the body swapped for the base64 it stands for
/// before `T` is deserialised.
///
/// This is the single edge where references become bytes, and EVERY route that deserialises a
/// document-derived body must use it — a plain `Json<T>` there forwards the literal `asset:<hash>`
/// string to whatever consumes the field, which is how `prompt-from-llm` came to hand llama-server
/// a ref as an image. Handlers behind it are unchanged and unaware: they still receive the same
/// fully-inlined request they always did. Deliberately NOT used on the project save/load routes:
/// the stored document is exactly what must keep the references, and expanding there would inline
/// every asset straight back into it.
pub struct AssetJson<T>(pub T);

impl<T> axum::extract::FromRequest<crate::AppState> for AssetJson<T>
where
    T: serde::de::DeserializeOwned,
{
    type Rejection = axum::response::Response;

    async fn from_request(
        req: axum::extract::Request,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        use axum::response::IntoResponse;
        let reject = |msg: String| {
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        };
        let bytes =
            <Bytes as axum::extract::FromRequest<crate::AppState>>::from_request(req, state)
                .await
                .map_err(|e| reject(format!("body: {e}")))?;
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|e| reject(format!("bad json: {e}")))?;
        let missing = expand_refs(state.director_assets_dir.as_ref(), &mut value).await;
        if !missing.is_empty() {
            return Err(reject(missing_refs_message(&missing)));
        }
        serde_json::from_value(value)
            .map(AssetJson)
            .map_err(|e| reject(format!("bad request: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The render submits want BYTES. Resolving straight to them is what stops a project's whole
    /// asset payload being read, base64'd and immediately decoded again on every render — and it
    /// must still report a ref the store has lost rather than rendering a shot without its image.
    #[tokio::test]
    async fn resolve_media_returns_bytes_for_refs_literals_and_absence() {
        let dir = std::env::temp_dir().join(format!("koblem-assets-{}", uuid::Uuid::now_v7()));
        let raw = b"\x89PNG\r\n\x1a\n not really a png".to_vec();
        let hash = hash_bytes(&raw);
        write_bytes(&dir, &hash, &raw).await.expect("store");

        assert_eq!(
            resolve_media(&dir, &make_ref(&hash)).await.expect("ref"),
            Some(Bytes::from(raw.clone())),
        );

        // Legacy documents still hold literal base64, bare or as a data URI.
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        assert_eq!(
            resolve_media(&dir, &b64).await.expect("bare b64"),
            Some(Bytes::from(raw.clone())),
        );
        assert_eq!(
            resolve_media(&dir, &format!("data:image/png;base64,{b64}"))
                .await
                .expect("data uri"),
            Some(Bytes::from(raw)),
        );

        // Blank is the ONLY thing that resolves to "nothing attached".
        assert_eq!(resolve_media(&dir, "   ").await.expect("empty"), None);

        let gone = "0".repeat(64);
        assert_eq!(
            resolve_media(&dir, &make_ref(&gone)).await,
            Err(MediaFault::Missing(gone)),
            "a lost ref must be named, not silently dropped"
        );

        // 🔴 The arm that used to be indistinguishable from "nothing attached", and so got reported
        // as a missing asset while the store was intact.
        assert_eq!(
            resolve_media(&dir, "not base64 at all!!").await,
            Err(MediaFault::Undecodable { bytes: 19 }),
            "an undecodable payload is a fault, not an absence"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// The point of staging: one asset used in several places is ONE part. A reference reused
    /// across shots is the common case, and inlining used to pay for every use of it.
    /// 🔴 The delivery soundtrack survives staging as an `asset:` ref.
    ///
    /// It is koblem's own copy of the track, read back after the render to mux over the cut — the
    /// shim has no such field. Staged, it became `part:ref_N` and the mux base64-decoded a ten-byte
    /// part NAME, failed, and shipped the model's reconstruction of the song instead (job
    /// ab5602e0, 2026-08-09). A mux failure is deliberately not a job failure, so this is silent.
    #[tokio::test]
    async fn staging_leaves_the_delivery_soundtrack_alone() {
        let dir = std::env::temp_dir().join(format!("koblem-assets-{}", uuid::Uuid::now_v7()));
        let raw = b"song bytes".to_vec();
        let hash = hash_bytes(&raw);
        write_bytes(&dir, &hash, &raw).await.expect("store");
        let mut doc = serde_json::json!({
            "soundtrack": { "audio": make_ref(&hash), "mode": "replace" },
            "generations": [{ "references": [{ "kind": "audio", "audio": make_ref(&hash) }] }]
        });
        let staged = stage_refs(&dir, &mut doc, "ref_").await.expect("staged");
        // The generation's reference IS staged — the engine receives it as a part.
        assert!(
            doc["generations"][0]["references"][0]["audio"]
                .as_str()
                .unwrap()
                .starts_with("part:"),
            "{doc}"
        );
        assert_eq!(staged.entries.len(), 1, "one part, for the reference only");
        // The delivery block is untouched, so `resolve_media` can still read it.
        assert_eq!(
            doc["soundtrack"]["audio"].as_str().unwrap(),
            make_ref(&hash),
            "{doc}"
        );
        assert_eq!(doc["soundtrack"]["mode"].as_str().unwrap(), "replace");
    }

    #[tokio::test]
    async fn stage_refs_dedupes_by_hash_and_marks_every_use() {
        let dir = std::env::temp_dir().join(format!("koblem-assets-{}", uuid::Uuid::now_v7()));
        let one = b"\x89PNG\r\n\x1a\n first".to_vec();
        let two = b"\x89PNG\r\n\x1a\n second".to_vec();
        let (h1, h2) = (hash_bytes(&one), hash_bytes(&two));
        write_bytes(&dir, &h1, &one).await.expect("store one");
        write_bytes(&dir, &h2, &two).await.expect("store two");

        // h1 three times, in three different shapes of position; h2 once.
        let mut doc = serde_json::json!({
            "opener": make_ref(&h1),
            "segments": [
                { "image": make_ref(&h1), "refs": [make_ref(&h2)] },
                { "image": make_ref(&h1) },
            ],
            "steps": 20,
            "prompt": "not a ref",
        });

        let staged = stage_refs(&dir, &mut doc, "ref_")
            .await
            .expect("all present");

        assert_eq!(staged.len(), 2, "two distinct assets, four uses");
        assert_eq!(staged.bytes("ref_0").map(|b| b.as_ref()), Some(&one[..]));
        assert_eq!(staged.bytes("ref_1").map(|b| b.as_ref()), Some(&two[..]));

        // Every use is marked, and the repeats point at the FIRST use's part.
        assert_eq!(doc["opener"], serde_json::json!("part:ref_0"));
        assert_eq!(doc["segments"][0]["image"], serde_json::json!("part:ref_0"));
        assert_eq!(doc["segments"][1]["image"], serde_json::json!("part:ref_0"));
        assert_eq!(
            doc["segments"][0]["refs"][0],
            serde_json::json!("part:ref_1")
        );
        // Non-ref values are untouched — including strings that merely look like content.
        assert_eq!(doc["steps"], serde_json::json!(20));
        assert_eq!(doc["prompt"], serde_json::json!("not a ref"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A lost asset must fail the request naming the hash, exactly as `expand_refs` does. On the H3
    /// path an unresolved ref used to fall through to the "treat it as a filesystem path" branch and
    /// come back from the shim as `no such path`, naming a hash as if it were a filename.
    #[tokio::test]
    async fn stage_refs_reports_lost_assets_and_leaves_literals_alone() {
        let dir = std::env::temp_dir().join(format!("koblem-assets-{}", uuid::Uuid::now_v7()));
        let raw = b"\x89PNG\r\n\x1a\n present".to_vec();
        let here = hash_bytes(&raw);
        write_bytes(&dir, &here, &raw).await.expect("store");
        let gone = "0".repeat(64);

        let mut doc = serde_json::json!({
            "a": make_ref(&here),
            "b": make_ref(&gone),
        });
        assert_eq!(
            stage_refs(&dir, &mut doc, "ref_")
                .await
                .expect_err("must refuse"),
            vec![gone],
            "the missing hash is named, and the present one does not mask it"
        );

        // Legacy documents hold literal base64. It is not a ref, so it stays put and whatever
        // decodes it downstream keeps working.
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let mut legacy = serde_json::json!({ "image": b64.clone() });
        let staged = stage_refs(&dir, &mut legacy, "ref_")
            .await
            .expect("literals are not refs");
        assert!(staged.is_empty(), "literal base64 stages nothing");
        assert_eq!(legacy["image"], serde_json::json!(b64));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Names must not move when the document does not, or a retake would present different
    /// conditioning inputs to the LTX bank than the render it is redoing.
    #[tokio::test]
    async fn stage_refs_names_are_stable_across_repeat_runs() {
        let dir = std::env::temp_dir().join(format!("koblem-assets-{}", uuid::Uuid::now_v7()));
        let (a, b) = (b"aaaa-asset".to_vec(), b"bbbb-asset".to_vec());
        let (ha, hb) = (hash_bytes(&a), hash_bytes(&b));
        write_bytes(&dir, &ha, &a).await.expect("a");
        write_bytes(&dir, &hb, &b).await.expect("b");

        let source = serde_json::json!({ "x": make_ref(&ha), "y": make_ref(&hb) });
        let mut first = source.clone();
        let mut second = source.clone();
        let s1 = stage_refs(&dir, &mut first, "a_").await.expect("first");
        let s2 = stage_refs(&dir, &mut second, "a_").await.expect("second");

        assert_eq!(first, second, "same document stages to the same markers");
        assert_eq!(s1.bytes("a_0"), s2.bytes("a_0"));
        assert_eq!(s1.bytes("a_1"), s2.bytes("a_1"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// §9.5 — the one rule this whole document exists to defend, as a test.
    ///
    /// Nothing stops `expand_refs` being reintroduced on a render submit path, and that is
    /// precisely how the H3 round trip came to exist: a new engine path was added and reached for
    /// the edge that was already there, so a reference image travelled disk → bytes → base64
    /// String → back to bytes → multipart part. Crude as it is, this reads the source and would
    /// have caught that.
    ///
    /// `expand_refs` itself is NOT going away: the prompt / vision routes feed a data URI, where
    /// base64 is the wire contract. What must never come back is a SUBMIT path calling it.
    #[test]
    fn no_render_submit_path_expands_refs_to_base64() {
        // Files whose job is to build a request for a GPU engine. Anything here that expands a
        // ref has put an artifact into a JSON string.
        const SUBMIT_PATHS: &[&str] = &[
            "routes/ltx_video.rs",
            "routes/ltx_director.rs",
            "routes/minimax_h3.rs",
            "routes/images.rs",
            "routes/krea2.rs",
            "routes/three_d.rs",
        ];
        // The expanding entry points. `AssetJson` is deliberately absent: it is the extractor for
        // the prompt routes, several of which live in the same files.
        const EXPANDERS: &[&str] = &[
            "expand_refs(",
            "parse_asset_json::<",
            "parse_asset_json_strict(",
        ];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for file in SUBMIT_PATHS {
            let path = src.join(file);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for line in text.lines() {
                // Comments and doc comments talk about the old design on purpose.
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                for expander in EXPANDERS {
                    assert!(
                        !code.contains(expander),
                        "{file} calls `{expander}` — a render submit must STAGE refs into `part:` \
                         markers (`stage_refs` / `stage_asset_json_strict`), or capture them into \
                         the store (`capture_ref` / `capture_bytes`), \
                         not expand them to base64.\n  {line}"
                    );
                }
            }
        }
    }
}
