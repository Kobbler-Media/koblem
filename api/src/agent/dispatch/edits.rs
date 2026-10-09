//! The tools that MODIFY something rather than conjure it: they all take the
//! most recent image or audio in the conversation as their subject, exactly as
//! `transcribe` and `remove_background` do, so the model never has to name a
//! file or guess an index.

use base64::Engine as _;
use serde_json::{Value, json};

use crate::routes::tier2_helpers::OutputBytes;

use super::outputs::{
    find_generation_by_filename, gate_detail, outputs_from_generation, persist_agent_outputs,
    resolve,
};
use super::transport::{fetch, file_part, poll_job, post_json, post_multipart};
use super::{Ctx, ToolOutcome, secs, str_arg, truncate};
use crate::agent::media;

/// The edit's OUTPUT frame is whatever `width`/`height` ask for — a reference image sets the
/// content, never the shape — so an edit asks for the source's own shape, scaled under this ceiling
/// and snapped to the ×16 grid `routes::images` clamps to anyway.
const EDIT_MAX_SIDE: u32 = 1536;

// ── image editing ────────────────────────────────────────────────────────────

pub(super) async fn edit_image(ctx: &Ctx, args: &Value) -> ToolOutcome {
    let Some(instruction) = str_arg(args, "instruction") else {
        return ToolOutcome::failed("edit_image needs an instruction — the change to make");
    };
    let Some(source) = ctx.media.latest_image() else {
        return ToolOutcome::failed("there is no image in this conversation to edit");
    };
    let Ok(bytes) = resolve(ctx, source).await else {
        return ToolOutcome::failed("the image could not be read back");
    };
    let redraw = args
        .get("redraw")
        .and_then(|v| v.as_f64())
        .map(|s| s.clamp(0.1, 1.0));
    let (width, height) = edit_dims(media::dimensions(&bytes));

    // §6.3: hand the route a REFERENCE, not the picture. This is koblem calling itself over HTTP,
    // and it used to base64 a full-size image into that request — which the route then staged
    // straight back out to a binary part. Putting the bytes in the content-addressed asset store
    // (a no-op when they are already there, since the name is the hash) and sending
    // `asset:<hash>` means the image never moves at all: the marker crosses the socket and the
    // engine client reads the file.
    let source = match stage_source_asset(ctx, &bytes).await {
        Some(reference) => reference,
        // No asset store configured: fall back to what this always did. An edit that renders is
        // better than an edit that refuses over a transport preference.
        None => base64::engine::general_purpose::STANDARD.encode(&bytes),
    };
    let body = edit_body(&instruction, &source, redraw, width, height);
    let resp = match post_json(ctx, "/api/v1/images/generate", &body).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("flux2 edit failed: {e}")),
    };
    let Some(gen_id) = resp.get("generation_id").and_then(|v| v.as_str()) else {
        return ToolOutcome::failed(
            "the edit rendered but nothing was saved to the gallery — check generations_dir",
        );
    };
    let outputs = outputs_from_generation(ctx, gen_id).await;
    let render_ms = resp.get("render_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "engine": "flux2",
            "images": outputs.len(),
            "redraw": redraw,
        }),
        detail: gate_detail(
            &resp,
            format!("edited at {width}×{height} in {}", secs(render_ms)),
        ),
        outputs,
    }
}

/// The `/api/v1/images/generate` body for an edit. `redraw` picks the path.
///
/// Default — Kontext. The source rides as a REFERENCE image, which FLUX.2 VAE-encodes and
/// concatenates onto the DiT's token sequence with an incrementing rotary index
/// (`src/model/diffusion/flux.hpp:1319-1324`, the `flux2` preset's `RefIndexMode::INCREASE` at
/// `src/model/diffusion/model.hpp:36` in the engine tree). The picture is CONTEXT the sampler
/// reads, so "put a hat on him" changes the hat and leaves the man.
///
/// `redraw` — img2img. The source becomes the init latent and `strength` truncates the schedule
/// (`src/stable-diffusion.cpp:6755-6776`), so sampling restarts from a noised copy of the picture
/// and everything in it is up for renegotiation. That is right for "make the whole thing a 1970s
/// polaroid" and catastrophic for "put a hat on him", which is what this route used to do to every
/// edit at strength 0.7.
///
/// One reference, so the ordering that matters for a multi-ref recipe (`routes::krea2`'s
/// scene-then-person) has nothing to order here.
/// Put `bytes` in the Director asset store and return the `asset:<hash>` reference for them.
///
/// Both stores are keyed on the SHA-256 of the bytes, so this is a write only the first time any
/// path in the system has seen this picture. `None` when the store is unreachable.
async fn stage_source_asset(ctx: &Ctx, bytes: &[u8]) -> Option<String> {
    let dir = ctx.state.director_assets_dir.as_ref();
    let hash = crate::director_assets::hash_bytes(bytes);
    match crate::director_assets::write_bytes(dir, &hash, bytes).await {
        Ok(()) => Some(crate::director_assets::make_ref(&hash)),
        Err(e) => {
            tracing::warn!(error = %e, "agent edit: could not stage the source asset; inlining");
            None
        }
    }
}

fn edit_body(instruction: &str, source_b64: &str, redraw: Option<f64>, w: u32, h: u32) -> Value {
    let mut body = json!({
        "prompt": instruction,
        "model": "edit",
        "width": w,
        "height": h,
        "batch": 1,
    });
    let obj = body.as_object_mut().expect("object literal");
    match redraw {
        Some(strength) => {
            obj.insert("init_image".into(), json!(source_b64));
            obj.insert("strength".into(), json!(strength));
        }
        None => {
            obj.insert("reference_images".into(), json!([source_b64]));
        }
    }
    body
}

/// The source's shape, scaled under [`EDIT_MAX_SIDE`] and snapped to ×16. An
/// unreadable header falls back to square rather than to the route's own 512,
/// which would shrink the picture as well as squash it.
fn edit_dims(source: Option<(u32, u32)>) -> (u32, u32) {
    let Some((w, h)) = source.filter(|(w, h)| *w > 0 && *h > 0) else {
        return (1024, 1024);
    };
    let long = w.max(h);
    let (w, h) = match long > EDIT_MAX_SIDE {
        true => (
            (w as u64 * EDIT_MAX_SIDE as u64 / long as u64) as u32,
            (h as u64 * EDIT_MAX_SIDE as u64 / long as u64) as u32,
        ),
        false => (w, h),
    };
    (snap16(w), snap16(h))
}

fn snap16(v: u32) -> u32 {
    (v.clamp(256, EDIT_MAX_SIDE) / 16) * 16
}

// ── talking head ─────────────────────────────────────────────────────────────

pub(super) async fn make_avatar(ctx: &Ctx, args: &Value) -> ToolOutcome {
    let Some(portrait) = ctx.media.latest_image() else {
        return ToolOutcome::failed(
            "there is no image in this conversation to animate — send a photo of a face first",
        );
    };
    let Ok(image) = resolve(ctx, portrait).await else {
        return ToolOutcome::failed("the portrait could not be read back");
    };
    let script = str_arg(args, "script");
    let orientation = match args.get("orientation").and_then(|v| v.as_str()) {
        Some("landscape") => "landscape",
        _ => "portrait",
    };

    // Either koblem speaks a script for us (the route runs TTS itself, against
    // the already-resident engine) or the conversation's own audio is uploaded.
    // `AvatarAudioSource::LibraryClipId` is deliberately unused: it names a
    // saved TTS render, and a chat has no way to have picked one.
    let spoken = match &script {
        Some(_) => None,
        None => {
            let Some(clip) = ctx.media.latest_audio() else {
                return ToolOutcome::failed(
                    "there is nothing for the face to say — give me a script, or send some audio \
                     to lip-sync",
                );
            };
            match resolve(ctx, clip).await {
                Ok(bytes) => Some((bytes, clip.mime.clone())),
                Err(_) => return ToolOutcome::failed("the audio could not be read back"),
            }
        }
    };

    let request = json!({
        "image_source": {"kind": "upload"},
        "audio_source": match &script {
            Some(s) => json!({"kind": "tts", "script": s}),
            None => json!({"kind": "upload"}),
        },
        "orientation": orientation,
    });
    let mut form = reqwest::multipart::Form::new()
        .text("request", request.to_string())
        .part("image", file_part(image, &portrait.mime, "portrait.png"));
    if let Some((bytes, mime)) = spoken {
        form = form.part("audio", file_part(bytes, &mime, "speech.wav"));
    }

    // 🔴 Avatar is a queued kind now: the submit answers 202 with a job id and the
    // clip only exists once the row reaches `done`. Reading the accept body for a
    // filename would find nothing on every render.
    let accepted = match post_multipart(ctx, "/api/v1/avatar/generate", form).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("avatar render failed: {e}")),
    };
    let Some(job_id) = accepted.get("job_id").and_then(|v| v.as_str()) else {
        return ToolOutcome::failed("the avatar submit did not return a job id");
    };
    let resp = match poll_job(ctx, "/api/v1/avatar/jobs", job_id).await {
        Ok(j) => j,
        Err(e) => return ToolOutcome::failed(format!("avatar render failed: {e}")),
    };
    let filename = resp
        .pointer("/clip/filename")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if filename.is_empty() {
        return ToolOutcome::failed("the avatar rendered but returned no clip");
    }
    let outputs = avatar_outputs(ctx, &resp, &filename).await;
    if outputs.is_empty() {
        return ToolOutcome::failed("the avatar rendered but its clip could not be saved");
    }

    let render_ms = resp.get("render_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    let seconds = resp
        .pointer("/clip/duration_ms")
        .and_then(|v| v.as_u64())
        .map(|ms| (ms as f64 / 100.0).round() / 10.0);
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "seconds": seconds,
            "spoken": resp.get("script_used").and_then(|v| v.as_str()).map(|s| truncate(s, 600)),
        }),
        detail: gate_detail(&resp, format!("talking clip in {}", secs(render_ms))),
        outputs,
    }
}

/// The avatar route saves under `AVATARS_DIR` and records a gallery row, so the
/// filename it echoes back is the key to outputs already on disk. The copy is
/// the fallback for a render that skipped the gallery — losing the clip would
/// leave the step with nothing to play.
async fn avatar_outputs(
    ctx: &Ctx,
    resp: &Value,
    filename: &str,
) -> Vec<crate::agent::wire::StepOutput> {
    if let Some(id) = find_generation_by_filename(ctx, "avatar", filename).await {
        let outputs = outputs_from_generation(ctx, &id).await;
        if !outputs.is_empty() {
            return outputs;
        }
    }
    let url = resp
        .pointer("/clip/url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("/api/v1/avatar/file/{filename}"));
    let Ok(bytes) = fetch(ctx, &url).await else {
        return Vec::new();
    };
    let mime = resp
        .pointer("/clip/content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("video/webm")
        .to_string();
    persist_agent_outputs(
        ctx,
        "make_avatar",
        json!({"filename": filename}),
        vec![OutputBytes {
            ext: "webm".to_string(),
            mime,
            kind: "video".to_string(),
            bytes,
        }],
    )
    .await
}

// ── music transforms ─────────────────────────────────────────────────────────

/// The five `MusicMode` variants that need source audio. `generate` is
/// `make_music`'s and is rejected by `/transform` itself.
const TRANSFORM_MODES: [&str; 5] = ["cover", "repaint", "extract", "lego", "complete"];

pub(super) async fn transform_music(ctx: &Ctx, args: &Value) -> ToolOutcome {
    let Some(mode) = str_arg(args, "mode") else {
        return ToolOutcome::failed("transform_music needs a mode");
    };
    if !TRANSFORM_MODES.contains(&mode.as_str()) {
        return ToolOutcome::failed(format!(
            "{mode} is not something transform_music can do — pick one of {}",
            TRANSFORM_MODES.join(", ")
        ));
    }
    let Some(source) = ctx.media.latest_audio() else {
        return ToolOutcome::failed("there is no audio in this conversation to rework");
    };
    let Ok(bytes) = resolve(ctx, source).await else {
        return ToolOutcome::failed("the audio could not be read back");
    };

    // `caption` is a bare String on the wire, so it is always sent even when the
    // stem modes leave it blank.
    let request = json!({
        "mode": mode,
        "caption": str_arg(args, "style").unwrap_or_default(),
        "lyrics": str_arg(args, "lyrics"),
        "track": str_arg(args, "track"),
    });
    let form = reqwest::multipart::Form::new()
        .text("request", request.to_string())
        .part("audio", file_part(bytes, &source.mime, "source.wav"));

    let resp = match post_multipart(ctx, "/api/v1/music/transform", form).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("{mode} failed: {e}")),
    };
    let tracks = resp
        .get("tracks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if tracks.is_empty() {
        return ToolOutcome::failed(format!("{mode} returned no audio"));
    }
    let filename = tracks[0]
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let outputs = match find_generation_by_filename(ctx, "music", filename).await {
        Some(id) => outputs_from_generation(ctx, &id).await,
        None => Vec::new(),
    };
    let render_ms = resp
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    transform_outcome(
        &mode,
        str_arg(args, "track"),
        tracks.len(),
        gate_detail(&resp, format!("{mode} in {}", secs(render_ms))),
        outputs,
    )
}

fn transform_outcome(
    mode: &str,
    track: Option<String>,
    rendered: usize,
    detail: String,
    outputs: Vec<crate::agent::wire::StepOutput>,
) -> ToolOutcome {
    if outputs.is_empty() {
        return ToolOutcome::failed(format!(
            "{mode} produced {rendered} track(s) but none could be linked to the gallery"
        ));
    }
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "mode": mode,
            "track": track,
            "tracks": outputs.len(),
        }),
        detail,
        outputs,
    }
}

// ── zero-shot classification ─────────────────────────────────────────────────

/// SigLIP-2 scores every label against the image in one pass, so the cap is
/// about the observation staying small, not about cost.
const MAX_LABELS: usize = 20;

pub(super) async fn describe_image(ctx: &Ctx, args: &Value) -> ToolOutcome {
    let labels = labels_from(args);
    if labels.len() < 2 {
        return ToolOutcome::failed(
            "describe_image scores an image against candidate labels — give it at least two",
        );
    }
    let Some(source) = ctx.media.latest_image() else {
        return ToolOutcome::failed("there is no image in this conversation to classify");
    };
    let Ok(bytes) = resolve(ctx, source).await else {
        return ToolOutcome::failed("the image could not be read back");
    };

    let mut form =
        reqwest::multipart::Form::new().part("images", file_part(bytes, &source.mime, "image.png"));
    for label in &labels {
        form = form.text("prompts", label.clone());
    }
    let resp = match post_multipart(ctx, "/api/v1/vision/classify", form).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("classification failed: {e}")),
    };
    let scores: Vec<f64> = resp
        .pointer("/scores/0")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_f64()).collect())
        .unwrap_or_default();
    let ranked = rank(&labels, &scores);
    if ranked.is_empty() {
        return ToolOutcome::failed("the classifier returned no scores");
    }
    let detail = format!("{} ({:.0}%)", ranked[0].0, ranked[0].1 * 100.0);
    ToolOutcome {
        ok: true,
        // The ranking IS the result; there is no media to hand back.
        observation: json!({
            "ok": true,
            "ranked": ranked
                .iter()
                .map(|(label, score)| json!({"label": label, "score": (score * 1000.0).round() / 1000.0}))
                .collect::<Vec<_>>(),
        }),
        detail,
        outputs: Vec::new(),
    }
}

fn labels_from(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(items) = args.get("labels").and_then(|v| v.as_array()) else {
        return out;
    };
    for label in items.iter().filter_map(|v| v.as_str()).map(str::trim) {
        if label.is_empty() || out.iter().any(|s| s == label) {
            continue;
        }
        out.push(label.to_string());
        if out.len() == MAX_LABELS {
            break;
        }
    }
    out
}

/// Labels paired with their scores, best first. A short score row (a partial
/// engine answer) truncates rather than mispairing.
fn rank(labels: &[String], scores: &[f64]) -> Vec<(String, f64)> {
    let mut ranked: Vec<(String, f64)> =
        labels.iter().cloned().zip(scores.iter().copied()).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default path must hand FLUX.2 the picture as CONTEXT. Sending it as `init_image` +
    /// strength is what redrew the subject away on "put a hat on him".
    #[test]
    fn an_ordinary_edit_sends_the_source_as_a_reference_not_an_init() {
        let b = edit_body("put a straw hat on the man", "SRC", None, 1024, 1024);
        assert_eq!(b["reference_images"], json!(["SRC"]));
        assert!(b.get("init_image").is_none(), "{b}");
        assert!(b.get("strength").is_none(), "{b}");
        assert_eq!(b["prompt"], "put a straw hat on the man");
        assert_eq!(b["model"], "edit");
    }

    #[test]
    fn only_a_redraw_reaches_the_img2img_path() {
        let b = edit_body(
            "make the whole picture a 1970s polaroid",
            "SRC",
            Some(0.8),
            832,
            1216,
        );
        assert_eq!(b["init_image"], "SRC");
        assert_eq!(b["strength"], 0.8);
        assert!(b.get("reference_images").is_none(), "{b}");
        assert_eq!(b["width"], 832);
        assert_eq!(b["height"], 1216);
    }

    #[test]
    fn an_edit_keeps_the_sources_shape() {
        assert_eq!(edit_dims(Some((1024, 576))), (1024, 576));
        assert_eq!(edit_dims(Some((832, 1216))), (832, 1216));
    }

    #[test]
    fn an_oversized_source_is_scaled_not_squashed() {
        let (w, h) = edit_dims(Some((4000, 3000)));
        assert_eq!(w, EDIT_MAX_SIDE);
        // 4:3 held to within the ×16 snap.
        assert!((w as f64 / h as f64 - 4.0 / 3.0).abs() < 0.02, "{w}×{h}");
    }

    #[test]
    fn every_edit_dimension_lands_on_the_engines_grid() {
        for src in [(4000, 3000), (37, 4001), (1023, 1), (800, 600)] {
            let (w, h) = edit_dims(Some(src));
            assert_eq!(w % 16, 0, "{src:?} width");
            assert_eq!(h % 16, 0, "{src:?} height");
            assert!(w >= 256 && h >= 256 && w <= EDIT_MAX_SIDE && h <= EDIT_MAX_SIDE);
        }
    }

    #[test]
    fn an_unreadable_header_falls_back_to_a_square_not_to_the_routes_512() {
        assert_eq!(edit_dims(None), (1024, 1024));
        assert_eq!(edit_dims(Some((0, 0))), (1024, 1024));
    }

    #[test]
    fn labels_are_trimmed_deduplicated_and_capped() {
        let args = json!({"labels": ["  a dog ", "a dog", "", "a wolf", 7]});
        assert_eq!(labels_from(&args), vec!["a dog", "a wolf"]);

        let many: Vec<String> = (0..40).map(|i| format!("label {i}")).collect();
        assert_eq!(labels_from(&json!({"labels": many})).len(), MAX_LABELS);
        assert!(labels_from(&json!({})).is_empty());
    }

    #[test]
    fn the_ranking_is_best_first() {
        let labels = vec!["cat".to_string(), "dog".to_string(), "fox".to_string()];
        let ranked = rank(&labels, &[0.1, 0.8, 0.4]);
        assert_eq!(ranked[0], ("dog".to_string(), 0.8));
        assert_eq!(ranked[2], ("cat".to_string(), 0.1));
    }

    #[test]
    fn a_short_score_row_truncates_rather_than_mispairing_labels() {
        let labels = vec!["cat".to_string(), "dog".to_string()];
        assert_eq!(rank(&labels, &[0.5]), vec![("cat".to_string(), 0.5)]);
        assert!(rank(&labels, &[]).is_empty());
    }

    /// `outputs.len().max(tracks.len())` claimed audio the step cannot play: the
    /// engine rendered it, but nothing linked it to a gallery row so there is no
    /// URL behind the number.
    #[test]
    fn a_transform_reports_the_tracks_the_step_can_actually_play() {
        let lost = transform_outcome(
            "cover",
            Some("vocals".to_string()),
            1,
            "cover in 20.0s".to_string(),
            Vec::new(),
        );
        assert!(!lost.ok);
        assert_eq!(lost.observation["ok"], false);

        let kept = transform_outcome(
            "cover",
            None,
            1,
            "cover in 20.0s".to_string(),
            vec![crate::agent::wire::StepOutput {
                kind: "audio".to_string(),
                url: "/api/v1/generations/abc/file/0".to_string(),
                generation_id: Some("abc".to_string()),
                mime: "audio/wav".to_string(),
            }],
        );
        assert!(kept.ok);
        assert_eq!(kept.observation["tracks"], 1);
        assert_eq!(kept.observation["mode"], "cover");
    }
}
