//! Turning what an engine produced into step outputs the browser can render.
//!
//! Two paths, both landing on `/api/v1/generations/{id}/file/{n}`. When the
//! engine route already returned a `generation_id` (images, krea2, matting) the
//! row is reused as-is. When it did not, the agent persists its own copy — which
//! is not a fallback for video: the LTX job store TTL-sweeps a finished clip
//! after an hour, so an agent-owned copy is what keeps the conversation
//! replayable.

use bytes::Bytes;
use serde_json::Value;

use crate::routes::tier2_helpers::{OutputBytes, persist_generation};

use super::transport::fetch;
use super::{Ctx, secs};
use crate::agent::media::MediaRef;
use crate::agent::wire::StepOutput;

/// Gallery bucket for media the agent had to persist itself (video clips lifted
/// out of the TTL'd job store, TTS audio, transcripts).
const AGENT_SERVICE: &str = "agent";

/// Turn an existing gallery row into step outputs. The URL is stored token-less;
/// the read path attaches the viewer's own token so it works as an element src.
pub(super) async fn outputs_from_generation(ctx: &Ctx, generation_id: &str) -> Vec<StepOutput> {
    let Ok(Some(row)) = crate::repositories::generations::get(&ctx.state.db, generation_id).await
    else {
        return Vec::new();
    };
    row.outputs
        .iter()
        .enumerate()
        .map(|(i, o)| StepOutput {
            kind: o.kind.clone(),
            url: format!("/api/v1/generations/{generation_id}/file/{i}"),
            generation_id: Some(generation_id.to_string()),
            mime: o.mime.clone(),
        })
        .collect()
}

/// Persist media the agent had to copy (async video clips, TTS audio,
/// transcripts) into its own gallery bucket, and return its outputs.
///
/// §6.3: this "copy" is now nearly free. The gallery is content-addressed, so re-persisting bytes
/// that are already in the bucket writes NOTHING — the name is the hash, the file is already
/// there, and the row simply points at it. What used to be a second full copy of every clip the
/// agent touched is a row insert.
pub(super) async fn persist_agent_outputs(
    ctx: &Ctx,
    endpoint: &str,
    meta: Value,
    files: Vec<OutputBytes>,
) -> Vec<StepOutput> {
    match persist_generation(&ctx.state, AGENT_SERVICE, endpoint, meta, &files).await {
        Some(id) => outputs_from_generation(ctx, &id).await,
        None => {
            tracing::warn!(endpoint, "agent generation persist failed; no output URL");
            Vec::new()
        }
    }
}

/// The gallery row a music engine just wrote, found by the filename it echoed
/// back. Deterministic — the filename is unique per render — so this is a
/// lookup, not a "most recent row" guess.
pub(super) async fn find_generation_by_filename(
    ctx: &Ctx,
    service: &str,
    filename: &str,
) -> Option<String> {
    if filename.is_empty() {
        return None;
    }
    let row = sqlx::query!(
        "SELECT id FROM generations \
         WHERE service = $1 AND outputs::text LIKE '%' || $2 || '%' \
         ORDER BY created_at DESC LIMIT 1",
        service,
        filename,
    )
    .fetch_optional(&ctx.state.db)
    .await
    .ok()??;
    Some(row.id)
}

pub(super) async fn resolve(ctx: &Ctx, r: &MediaRef) -> Result<Bytes, String> {
    match r {
        MediaRef {
            asset_hash: Some(hash),
            ..
        } => crate::director_assets::read_bytes(ctx.state.director_assets_dir.as_ref(), hash)
            .await
            .ok_or_else(|| "asset missing".to_string()),
        MediaRef { url: Some(url), .. } => fetch(ctx, url).await,
        _ => Err("nothing to resolve".to_string()),
    }
}

/// Fold a gate wait into the step detail so a queued render reads as queued
/// rather than as a hang.
pub(super) fn gate_detail(resp: &Value, base: String) -> String {
    match resp.get("gate_wait_ms").and_then(|v| v.as_u64()) {
        Some(ms) if ms > 1500 => format!("{base} (waited {} for the GPU)", secs(ms)),
        _ => base,
    }
}

pub(super) fn ext_for_audio(mime: &str) -> &'static str {
    match mime {
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        _ => "wav",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_gate_wait_is_surfaced_only_when_it_was_real() {
        assert_eq!(
            gate_detail(&json!({"gate_wait_ms": 12}), "done".into()),
            "done"
        );
        assert_eq!(
            gate_detail(&json!({"gate_wait_ms": 8200}), "done".into()),
            "done (waited 8.2s for the GPU)"
        );
    }
}
