//! Shared helpers for tier-2 GPU playground routes.
//!
//! Fire-and-forget `gpu_logs` inserts (async) and opt-in sample persistence
//! under `AppState::samples_dir`.

use std::path::PathBuf;

use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tracing::warn;
use uuid::Uuid;

use crate::AppState;
use koblem_client::types::GenerationOutput;

use crate::repositories::generations::{self, CreateGeneration};
use crate::repositories::gpu_logs::{self, CreateLog, CreateSample};

/// Write sample bytes under samples_dir/<yyyy-mm>/<uuid>.<ext> and insert a
/// gpu_samples row. Returns the sample id (or None on failure — failures are
/// logged, not fatal).
pub async fn save_sample(
    pool: &PgPool,
    samples_dir: &std::path::Path,
    kind: &str,
    mime: &str,
    ext: &str,
    bytes: &Bytes,
) -> Option<i64> {
    let now = time::OffsetDateTime::now_utc();
    let month = format!("{:04}-{:02}", now.year(), u8::from(now.month()));
    let month_dir = samples_dir.join(&month);
    if let Err(e) = tokio::fs::create_dir_all(&month_dir).await {
        warn!(error = %e, "save_sample: mkdir failed");
        return None;
    }

    let id = Uuid::new_v4();
    let rel_path = PathBuf::from(&month).join(format!("{id}.{ext}"));
    let abs_path = samples_dir.join(&rel_path);

    match tokio::fs::File::create(&abs_path).await {
        Ok(mut f) => {
            if let Err(e) = f.write_all(bytes).await {
                warn!(error = %e, path = %abs_path.display(), "save_sample: write failed");
                return None;
            }
        }
        Err(e) => {
            warn!(error = %e, path = %abs_path.display(), "save_sample: create failed");
            return None;
        }
    }

    let sample = CreateSample {
        kind: kind.to_string(),
        mime: mime.to_string(),
        path: rel_path.to_string_lossy().into_owned(),
        size_bytes: bytes.len() as i64,
    };
    match gpu_logs::create_sample(pool, &sample).await {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(error = %e, "save_sample: db insert failed");
            None
        }
    }
}

/// Fire-and-forget log. Use in handlers' happy path + error path.
pub fn log_async(pool: PgPool, log: CreateLog) {
    tokio::spawn(async move {
        if let Err(e) = gpu_logs::create_log(&pool, &log).await {
            warn!(error = %e, service = %log.service, "gpu_logs insert failed");
        }
    });
}

/// Extract caller from headers (X-Caller > User-Agent > None).
pub fn extract_caller(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-caller")
        .or_else(|| headers.get(axum::http::header::USER_AGENT))
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
}

/// Resolve the on-disk root for a service's generation files.
///
/// avatar/music ALWAYS persist (they write to their own existing dirs, so a row
/// is recorded for free). Every other service persists ONLY when the
/// `generations_dir` setting is non-empty — until an operator configures it,
/// nothing new is written, so a fresh deploy never accidentally fills a volume.
/// Returns None when persistence is disabled for the service.
pub async fn generations_root(state: &AppState, service: &str) -> Option<PathBuf> {
    let generations_dir = crate::repositories::settings::get_setting(&state.db, "generations_dir")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(str::to_string));
    generations::resolve_root(
        service,
        &state.avatars_dir,
        &state.songs_dir,
        generations_dir.as_deref(),
    )
}

/// Where a persisted output is served from. The single place that knows the
/// gallery file route's shape, so a response hands back a usable URL instead of
/// an id plus a template every client has to reassemble. Relative, so it works
/// through whatever host/proxy the caller reached koblem on; `?token=` is
/// accepted there for elements that cannot set an Authorization header.
pub fn generation_file_url(id: &str, index: usize) -> String {
    format!("/api/v1/generations/{id}/file/{index}")
}

/// The same file, addressed by what it IS rather than by where it landed in the vector.
///
/// A response that hands back `file/0` for the model and
/// `file/{3 + i}` for LOD *i* is correct only while the `files` vector is assembled in exactly one
/// order, twenty lines away. Inserting one output silently re-points every URL after it — and the
/// caller gets a GLB where it asked for a PNG, with a 200. `OutputBytes` already carries a `kind`,
/// so use it: the kinds are a small closed vocabulary per service (`model`, `source`,
/// `processed_source`, `model_lod1`…), and unlike an index they cannot drift.
pub fn generation_file_url_by_kind(id: &str, kind: &str) -> String {
    format!("/api/v1/generations/{id}/file/by-kind/{kind}")
}

/// One file to persist for a writing service.
pub struct OutputBytes {
    pub ext: String,
    pub mime: String,
    pub kind: String,
    pub bytes: Bytes,
}

/// The on-disk name for a gallery blob: the SHA-256 of its own bytes (§6.2).
///
/// Content-addressing the gallery buys four things the `{uuid}-{i}.{ext}` scheme could not:
/// a rebuilt cut gets a NEW url so caching becomes correct and permanent (see `serve_file`'s
/// `immutable`), a retake that produces identical bytes costs no disk, the `asset:` id shape
/// becomes universal, and an agent can hand a gallery output straight back into the next tool.
///
/// The extension is kept so the browser and `file(1)` still see a type. It stays a BARE name in
/// the same root — deliberately, so the path-confinement check on the serve route (no `/`, no
/// `..`) needs no relaxing at all, and legacy `{uuid}-{i}.{ext}` rows keep working forever.
pub fn gallery_blob_name(bytes: &[u8], ext: &str) -> String {
    format!("{}.{ext}", crate::director_assets::hash_bytes(bytes))
}

/// True for a name this function produced — 64 lowercase hex plus an extension.
///
/// Used to decide whether a served file may be cached forever, and which files the orphan sweep
/// is allowed to consider. A legacy uuid name is neither.
pub fn is_gallery_blob_name(name: &str) -> bool {
    let Some((stem, _ext)) = name.split_once('.') else {
        return false;
    };
    stem.len() == 64
        && stem
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Persist a generation for a WRITING service (images/matting/tts/stt): write each file under
/// `generations_root(service)` NAMED BY ITS OWN HASH (§6.2) and insert a generations row. Returns
/// the new generation id, or None when persistence is disabled (generations_dir unset) or on
/// failure — callers treat None as "not saved" and fall back to session-only behaviour.
pub async fn persist_generation(
    state: &AppState,
    service: &str,
    endpoint: &str,
    request_meta: Value,
    files: &[OutputBytes],
) -> Option<String> {
    let root = generations_root(state, service).await?;
    if let Err(e) = tokio::fs::create_dir_all(&root).await {
        warn!(error = %e, path = %root.display(), "persist_generation: mkdir failed");
        return None;
    }
    let mut outputs: Vec<GenerationOutput> = Vec::with_capacity(files.len());
    // What THIS call created, which is not the same as what it recorded: a blob that was already
    // there is reused, and rolling THAT back would delete bytes another row is using. This is the
    // delete-side rule (`unlink_victims`) applied to the failure path.
    let mut wrote: Vec<String> = Vec::new();
    for f in files {
        let filename = gallery_blob_name(&f.bytes, &f.ext);
        let abs = root.join(&filename);
        // A file named after its own hash cannot disagree with what we would have written, so an
        // existing one is the same bytes and the write is skipped. This is the dedup: two renders
        // that come out identical — and a retake is often exactly that — cost one file.
        if !tokio::fs::try_exists(&abs).await.unwrap_or(false) {
            if let Err(e) = write_atomic(&abs, &f.bytes).await {
                warn!(error = %e, path = %abs.display(), "persist_generation: write failed");
                for name in &wrote {
                    let _ = tokio::fs::remove_file(root.join(name)).await;
                }
                return None;
            }
            wrote.push(filename.clone());
        }
        outputs.push(GenerationOutput {
            filename,
            mime: f.mime.clone(),
            size_bytes: f.bytes.len() as i64,
            kind: f.kind.clone(),
        });
    }
    record_row(state, service, endpoint, request_meta, outputs).await
}

/// Write to a temp sibling and rename, so a crash mid-write cannot leave a truncated file under a
/// name that claims to be the whole thing. The asset store has done this since it was written;
/// the gallery needs it now for the same reason — a hash name is a promise about the contents.
async fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("part-{}", Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// One file to persist that is ALREADY on disk somewhere else.
pub struct OutputFile {
    pub ext: String,
    pub mime: String,
    pub kind: String,
    /// Read side. Left where it is — this never moves or deletes the source.
    pub path: std::path::PathBuf,
}

/// Persist a generation whose bytes are already a file, WITHOUT reading them into memory (§9.1).
///
/// The video routes stream a finished render straight from the engine into the job store's clip
/// file, so by the time the gallery wants a copy the artifact exists on disk and holding it in RAM
/// again — which `persist_generation` requires, since it takes `Bytes` — is pure waste at exactly
/// the sizes that hurt: the p99 render is 220 MB and the largest on disk is 589 MB.
///
/// Tries a hard link first and falls back to a copy, so when the job store and the gallery share a
/// filesystem (the normal deployment) a saved clip costs no additional bytes at all. The fallback
/// matters: they are separately configurable, and `link` across devices is `EXDEV`.
pub async fn persist_generation_files(
    state: &AppState,
    service: &str,
    endpoint: &str,
    request_meta: Value,
    files: &[OutputFile],
) -> Option<String> {
    let root = generations_root(state, service).await?;
    if let Err(e) = tokio::fs::create_dir_all(&root).await {
        warn!(error = %e, path = %root.display(), "persist_generation_files: mkdir failed");
        return None;
    }
    let mut outputs: Vec<GenerationOutput> = Vec::with_capacity(files.len());
    // Only what this call linked into place is rolled back — see `persist_generation`.
    let mut wrote: Vec<String> = Vec::new();
    for f in files {
        // Hashed by STREAMING the file, not by reading it (§9.1): this path exists precisely
        // because the artifact is too big to want in memory, and a 589 MB clip must not be
        // slurped just to name it.
        let (hash, size) = match hash_file(&f.path).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, path = %f.path.display(), "persist_generation_files: hash failed");
                return None;
            }
        };
        let filename = format!("{hash}.{}", f.ext);
        let abs = root.join(&filename);
        if !tokio::fs::try_exists(&abs).await.unwrap_or(false) {
            if let Err(e) = link_or_copy(&f.path, &abs).await {
                warn!(error = %e, from = %f.path.display(), to = %abs.display(),
                      "persist_generation_files: link/copy failed");
                for name in &wrote {
                    let _ = tokio::fs::remove_file(root.join(name)).await;
                }
                return None;
            }
            wrote.push(filename.clone());
        }
        outputs.push(GenerationOutput {
            filename,
            mime: f.mime.clone(),
            size_bytes: size as i64,
            kind: f.kind.clone(),
        });
    }
    record_row(state, service, endpoint, request_meta, outputs).await
}

/// SHA-256 and byte length of a file, read in chunks. Never holds more than the buffer.
pub async fn hash_file(path: &std::path::Path) -> std::io::Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((crate::media_sign::hex(&hasher.finalize()), total))
}

/// Hard link `from` to `to`, falling back to a copy across filesystems. Returns the byte size.
pub(crate) async fn link_or_copy(
    from: &std::path::Path,
    to: &std::path::Path,
) -> std::io::Result<u64> {
    match tokio::fs::hard_link(from, to).await {
        Ok(()) => Ok(tokio::fs::metadata(to).await?.len()),
        // EXDEV (different filesystems) and EPERM (some overlay/bind mounts refuse links) both
        // mean "not linkable here", not "broken" — copy and move on.
        Err(_) => tokio::fs::copy(from, to).await,
    }
}

/// Record a generation for a service that ALREADY wrote its files to its own
/// dir (avatar/music). No file writing — just the row.
pub async fn record_generation(
    state: &AppState,
    service: &str,
    endpoint: &str,
    request_meta: Value,
    outputs: Vec<GenerationOutput>,
) -> Option<String> {
    let id = Uuid::new_v4().to_string();
    let g = CreateGeneration {
        id: id.clone(),
        service: service.to_string(),
        endpoint: endpoint.to_string(),
        request_meta,
        outputs,
    };
    match generations::create(&state.db, &g).await {
        Ok(()) => {
            spawn_title(state, id.clone(), g.service, g.request_meta);
            Some(id)
        }
        Err(e) => {
            warn!(error = %e, service, "record_generation: db insert failed");
            None
        }
    }
}

/// Insert the row for an already-written file set.
///
/// It used to unlink the files when the insert failed. It must not any more: gallery blobs are
/// content-addressed and therefore shared, so those files may belong to another row. An insert
/// that failed leaves at worst an orphan, and the hourly sweep collects those.
async fn record_row(
    state: &AppState,
    service: &str,
    endpoint: &str,
    request_meta: Value,
    outputs: Vec<GenerationOutput>,
) -> Option<String> {
    let id = Uuid::new_v4().to_string();
    let g = CreateGeneration {
        id: id.clone(),
        service: service.to_string(),
        endpoint: endpoint.to_string(),
        request_meta,
        outputs,
    };
    match generations::create(&state.db, &g).await {
        Ok(()) => {
            spawn_title(state, id.clone(), g.service, g.request_meta);
            Some(id)
        }
        Err(e) => {
            warn!(error = %e, service, "persist_generation: db insert failed");
            // The files are deliberately NOT unlinked here. Gallery blobs are content-addressed
            // and therefore shared: this generation's outputs may be another row's outputs, and
            // an insert that failed is no evidence about who else points at them. Whatever is
            // genuinely unreferenced is collected by the hourly orphan sweep
            // (`jobs::gpu_retention::sweep_orphan_blobs`) — which is the case that sweep exists
            // for, and the reason the plan chose mark-and-sweep over refcounting.
            None
        }
    }
}

/// Build a `request_meta` JSON object from a serializable request plus a few
/// extra keys (engine marker, resolved lyrics, an `had_*_inputs` flag). The
/// request must serialise to a JSON object and must NOT embed file bytes —
/// image/audio inputs ride as multipart parts, never in the request struct, so
/// the stored meta stays small and is safe to replay into a form.
pub fn meta_from_request<T: serde::Serialize>(req: &T, extra: &[(&str, Value)]) -> Value {
    let mut meta = serde_json::to_value(req).unwrap_or_else(|_| json!({}));
    if let Some(obj) = meta.as_object_mut() {
        for (k, v) in extra {
            obj.insert((*k).to_string(), v.clone());
        }
    }
    meta
}

const TITLE_SYSTEM_PROMPT: &str = "You write ultra-short gallery titles for a \
media-generation app. Given the inputs to one generation, reply with a single \
title of at most 6 words that captures the subject. No quotes, no trailing \
punctuation, no preamble or explanation — just the title in Title Case.";

/// Fields worth showing the model, in priority order. The first non-empty string
/// found becomes the "subject" line; a handful of knobs follow for flavour.
const TITLE_TEXT_FIELDS: &[&str] = &[
    "prompt", "brief", "caption", "lyrics", "script", "text", "input", "idea",
];
const TITLE_KNOB_FIELDS: &[&str] = &[
    "model",
    "mode",
    "voice",
    "duration",
    "duration_s",
    "style",
    "width",
    "height",
];

/// Build a compact, LLM-friendly digest of a generation's inputs. Long strings
/// (never base64 — those ride as multipart parts, not request_meta) are clipped
/// so the title prompt stays small.
fn summarise_request_meta(service: &str, request_meta: &Value) -> String {
    let obj = request_meta.as_object();
    let mut lines = vec![format!("Service: {service}")];

    if let Some(obj) = obj {
        for k in TITLE_TEXT_FIELDS {
            if let Some(s) = obj.get(*k).and_then(Value::as_str) {
                let s = s.trim();
                if !s.is_empty() {
                    let clipped: String = s.chars().take(500).collect();
                    lines.push(format!("{k}: {clipped}"));
                }
            }
        }
        let knobs: Vec<String> = TITLE_KNOB_FIELDS
            .iter()
            .filter_map(|k| {
                obj.get(*k).and_then(|v| match v {
                    Value::String(s) if !s.trim().is_empty() => Some(format!("{k}={s}")),
                    Value::Number(n) => Some(format!("{k}={n}")),
                    _ => None,
                })
            })
            .collect();
        if !knobs.is_empty() {
            lines.push(knobs.join(", "));
        }
    }
    lines.join("\n")
}

/// Ask the always-resident LLM for a tiny title from a generation's inputs.
/// Returns None on any failure (LLM down/disabled, empty reply) — shared by the
/// fire-and-forget persist path and the on-demand retitle endpoint.
pub async fn generate_title(
    llama: &crate::llama::LlamaClient,
    db: &PgPool,
    service: &str,
    request_meta: &Value,
) -> Option<String> {
    let digest = summarise_request_meta(service, request_meta);
    let mut body = json!({
        "messages": [
            {"role": "system", "content": TITLE_SYSTEM_PROMPT},
            {"role": "user", "content": digest},
        ],
        "max_tokens": 24,
        "temperature": 0.4,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(db, "sampling_title", &mut body).await;
    let content = match llama.chat_completion_json(&body).await {
        Ok((_lease, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Err(e) => {
            warn!(error = %e, service, "title generation: llm call failed");
            return None;
        }
    };
    // Reuse the prompt cleaner (strips fences/quotes/label lines), take the first
    // line, clamp length so a runaway reply can't bloat the card.
    let title: String = crate::routes::ltx_prompting::clean_prompt(&content)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(80)
        .collect();
    (!title.is_empty()).then_some(title)
}

/// Spawn a best-effort task that titles the generation and writes it to the row.
/// Every failure is logged and swallowed — the row just keeps `title = NULL` and
/// the UI falls back to summarising request_meta.
fn spawn_title(state: &AppState, id: String, service: String, request_meta: Value) {
    let db = state.db.clone();
    let llama = state.llama.clone();
    tokio::spawn(async move {
        let Some(title) = generate_title(&llama, &db, &service, &request_meta).await else {
            return;
        };
        if let Err(e) = generations::set_title(&db, &id, &title).await {
            warn!(error = %e, service, "title generation: set_title failed");
        }
    });
}

/// Inject the multi-GPU placement target into a JSON request body. When the gate
/// chose a physical card (`gpu = Some(uuid)`), set the C++ server's per-request
/// `gpu` field so the worker spawns on / relocates to that card. No-op when the
/// placement layer is unconfigured (`None`) — the server then uses its
/// `WORKER_DEFAULT_GPU` env (fully backward compatible).
pub fn inject_gpu(body: &mut Value, gpu: Option<&str>) {
    if let Some(uuid) = gpu
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert("gpu".into(), Value::String(uuid.to_string()));
    }
}

/// Multipart/form variant of [`inject_gpu`]: append a `gpu` text field when the
/// gate chose a card. For servers whose request is multipart (vision/stt/tts/
/// matting), the `gpu` field must arrive as a part (read server-side via the
/// files+params helper, not `has_param` alone — see the handoff §7b gotcha).
pub fn push_gpu_field(fields: &mut Vec<(String, String)>, gpu: Option<&str>) {
    if let Some(uuid) = gpu {
        fields.push(("gpu".to_string(), uuid.to_string()));
    }
}

/// Pull every image file part (`images`, `image`, or `file`) out of a multipart
/// body, in order. Non-image fields are drained and ignored. The error is a
/// message, not a `Response`, so each route renders it in its own error shape.
pub async fn collect_multipart_images(
    mut mp: axum::extract::Multipart,
) -> Result<Vec<(String, Bytes)>, String> {
    let mut images: Vec<(String, Bytes)> = Vec::new();
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                if name == "images" || name == "image" || name == "file" {
                    let fname = field
                        .file_name()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("image-{}.png", images.len()));
                    let bytes = field
                        .bytes()
                        .await
                        .map_err(|e| format!("reading image part: {e}"))?;
                    images.push((fname, bytes));
                } else {
                    let _ = field.bytes().await;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("malformed multipart: {e}")),
        }
    }
    Ok(images)
}

/// Derive a filename (extension matters to nothing downstream, but a sensible
/// default keeps logs readable) from a raw-body Content-Type.
pub fn image_filename_for(content_type: &str) -> String {
    let ext = match content_type.split(';').next().unwrap_or("").trim() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "img",
    };
    format!("image.{ext}")
}

/// Return a URL the STT Python worker can poll mid-transcription to check for
/// preemption. Uses the request's `Host` header if present, otherwise localhost.
pub fn stt_cancel_url(headers: &axum::http::HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8090");
    format!("http://{host}/v1/gpu/stt-preempt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_gpu_sets_and_overwrites_field() {
        // The gate-chosen card must land in the body — and win over any stale
        // `gpu` echoed back by the ace-server's enriched LM result (the synth path).
        let mut body = serde_json::json!({"seed": 7, "gpu": "GPU-stale"});
        inject_gpu(&mut body, Some("GPU-chosen"));
        assert_eq!(body["gpu"], "GPU-chosen");
        assert_eq!(body["seed"], 7);
    }

    #[test]
    fn inject_gpu_is_noop_when_unplaced() {
        let mut body = serde_json::json!({"seed": 7});
        inject_gpu(&mut body, None);
        assert!(
            body.get("gpu").is_none(),
            "no placement → no gpu field (back-compat)"
        );
    }

    #[test]
    fn summarise_request_meta_picks_subject_and_knobs() {
        let meta = serde_json::json!({
            "prompt": "a small wooden cabin, warm windows, deep snow",
            "model": "flux2",
            "width": 512,
            "height": 768,
            "seed": 7,
        });
        let digest = summarise_request_meta("images", &meta);
        assert!(digest.starts_with("Service: images"));
        assert!(digest.contains("prompt: a small wooden cabin"));
        assert!(digest.contains("model=flux2"));
        assert!(digest.contains("width=512"));
        // `seed` isn't a title knob — it shouldn't leak into the digest.
        assert!(!digest.contains("seed"));
    }

    #[test]
    fn summarise_request_meta_clips_long_text() {
        let long = "x".repeat(2000);
        let meta = serde_json::json!({ "lyrics": long });
        let digest = summarise_request_meta("music", &meta);
        // 500-char clip + "lyrics: " prefix + service line, nowhere near 2000.
        assert!(digest.len() < 600, "digest not clipped: {}", digest.len());
    }

    #[test]
    fn push_gpu_field_appends_only_when_placed() {
        let mut fields = Vec::new();
        push_gpu_field(&mut fields, None);
        assert!(fields.is_empty());
        push_gpu_field(&mut fields, Some("GPU-chosen"));
        assert_eq!(fields, vec![("gpu".to_string(), "GPU-chosen".to_string())]);
    }

    /// The name IS the promise: same bytes ⇒ same name ⇒ one file and a permanently cacheable
    /// URL; different bytes ⇒ different name, so a rebuilt cut can never be served from a cache
    /// holding the old one (§6.2).
    #[test]
    fn a_blob_name_is_its_own_hash() {
        let a = gallery_blob_name(b"the same bytes", "png");
        assert_eq!(a, gallery_blob_name(b"the same bytes", "png"));
        assert_ne!(a, gallery_blob_name(b"the same bytes!", "png"));
        // The extension is part of the name but not of the identity.
        assert_ne!(a, gallery_blob_name(b"the same bytes", "webp"));
        assert!(a.ends_with(".png"));
        assert_eq!(a.len(), 64 + 4);
    }

    /// The orphan sweep deletes files by name, so this predicate is the ONLY thing standing
    /// between it and a legacy output, a `.part` file, or something an operator dropped in the
    /// directory.
    #[test]
    fn only_a_hash_name_counts_as_a_blob() {
        assert!(is_gallery_blob_name(&gallery_blob_name(b"x", "webm")));
        // Legacy `{uuid}-{i}.{ext}` — the shape every pre-migration row carries.
        assert!(!is_gallery_blob_name(
            "412116f0-8f3f-43b0-9124-bb646a20c560-0.png"
        ));
        assert!(!is_gallery_blob_name("notes.txt"));
        assert!(!is_gallery_blob_name("model.glb"));
        // 64 characters but not hex, and hex but uppercase — neither is a name we write.
        assert!(!is_gallery_blob_name(&format!("{}.png", "z".repeat(64))));
        assert!(!is_gallery_blob_name(&format!("{}.png", "A".repeat(64))));
        // No extension at all.
        assert!(!is_gallery_blob_name(&"a".repeat(64)));
    }
}
