//! One function per tool intent. Each builds the few fields the model supplied
//! into the engine route's own request shape and lets the route fill the rest
//! from its defaults — the model sees four arguments where `ImageGenerateRequest`
//! has seventeen.

use std::time::Instant;

use bytes::Bytes;
use serde_json::{Value, json};

use crate::routes::tier2_helpers::OutputBytes;

use super::outputs::{
    ext_for_audio, find_generation_by_filename, gate_detail, outputs_from_generation,
    persist_agent_outputs, resolve,
};
use super::transport::{
    fetch, file_part, poll_job, post_bytes, post_json, post_multipart, post_raw,
};
use super::{Ctx, ToolOutcome, secs, str_arg, truncate};
use crate::agent::tools::{EngineOption, aspect_dims};
use crate::agent::wire::StepOutput;

// ── image ────────────────────────────────────────────────────────────────────

pub(super) async fn make_image(ctx: &Ctx, engine: &EngineOption, args: &Value) -> ToolOutcome {
    let Some(prompt) = str_arg(args, "prompt") else {
        return ToolOutcome::failed("make_image needs a prompt");
    };
    let (width, height) = aspect_dims(args.get("aspect").and_then(|v| v.as_str()), 1024);
    let batch = args
        .get("count")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 4);
    let path = match engine.id {
        "flux2" => "/api/v1/images/generate",
        _ => "/api/v1/krea2/generate",
    };
    let body = json!({"prompt": prompt, "width": width, "height": height, "batch": batch});

    let started = Instant::now();
    let resp = match post_json(ctx, path, &body).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("{} render failed: {e}", engine.id)),
    };
    let Some(gen_id) = resp.get("generation_id").and_then(|v| v.as_str()) else {
        return ToolOutcome::failed(format!(
            "{} rendered but nothing was saved to the gallery — check generations_dir",
            engine.id
        ));
    };
    let outputs = outputs_from_generation(ctx, gen_id).await;
    let render_ms = resp
        .get("render_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| started.elapsed().as_millis() as u64);
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "engine": engine.id,
            "images": outputs.len(),
            "width": width,
            "height": height,
        }),
        detail: gate_detail(
            &resp,
            format!("{}×{} in {}", width, height, secs(render_ms)),
        ),
        outputs,
    }
}

// ── video ────────────────────────────────────────────────────────────────────

pub(super) async fn make_video(ctx: &Ctx, engine: &EngineOption, args: &Value) -> ToolOutcome {
    let Some(prompt) = str_arg(args, "prompt") else {
        return ToolOutcome::failed("make_video needs a prompt");
    };
    let seconds = args
        .get("seconds")
        .and_then(|v| v.as_f64())
        .unwrap_or(4.0)
        .clamp(2.0, 8.0);
    let (width, height) = aspect_dims(
        Some(
            args.get("aspect")
                .and_then(|v| v.as_str())
                .unwrap_or("landscape"),
        ),
        768,
    );
    // The container travels with the route: the two sd.cpp-fork engines deliver VP9-in-webm, and
    // LTX-2.5 delivers mp4 (ComfyUI's only audio-carrying save node is `SaveVideo`). The agent's
    // copy IS the clip, so naming it wrongly here is a file nothing will play later.
    let (path, jobs, body, ext) = match engine.id {
        "wan" => (
            "/api/v1/wan-vace/generate",
            "/api/v1/wan-vace/jobs",
            json!({
                "segments": [{"prompt": prompt, "seconds": seconds}],
                "width": width, "height": height,
            }),
            "webm",
        ),
        "ltx2b" => (
            "/api/v1/ltx-2b/generate",
            "/api/v1/ltx-2b/jobs",
            json!({
                "segments": [{"prompt": prompt, "seconds": seconds}],
                "width": width, "height": height,
            }),
            "webm",
        ),
        // `ltx`, and anything unrecognised: no `?engine=`, which the LTX route reads as LTX-2.5.
        _ => (
            "/api/v1/ltx-video/generate",
            "/api/v1/ltx-video/jobs",
            json!({
                "segments": [{"prompt": prompt, "seconds": seconds}],
                "width": width, "height": height,
            }),
            "mp4",
        ),
    };

    let accepted = match post_json(ctx, path, &body).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("{} submit failed: {e}", engine.id)),
    };
    let Some(job_id) = accepted.get("job_id").and_then(|v| v.as_str()) else {
        return ToolOutcome::failed(format!("{} did not return a job id", engine.id));
    };

    let job = match poll_job(ctx, jobs, job_id).await {
        Ok(j) => j,
        Err(e) => return ToolOutcome::failed(format!("{} render failed: {e}", engine.id)),
    };

    // The job store TTL-sweeps finished clips after an hour, so the agent must
    // own a copy or the conversation stops replaying.
    let media_url = format!("{jobs}/{job_id}/media");
    let Ok(bytes) = fetch(ctx, &media_url).await else {
        return ToolOutcome::failed(format!(
            "{} finished but its clip could not be read",
            engine.id
        ));
    };
    let outputs = persist_agent_outputs(
        ctx,
        "make_video",
        json!({"engine": engine.id, "prompt": prompt, "seconds": seconds}),
        vec![OutputBytes {
            ext: ext.to_string(),
            mime: format!("video/{ext}"),
            kind: "video".to_string(),
            bytes,
        }],
    )
    .await;

    let render_ms = job.get("render_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    video_outcome(
        engine.id,
        seconds,
        gate_detail(&job, format!("{seconds}s clip in {}", secs(render_ms))),
        outputs,
    )
}

/// The agent's copy IS the clip: `LtxJobStore::sweep` deletes the engine's after
/// an hour, so a lost copy is a lost render however well the GPU did.
fn video_outcome(
    engine: &str,
    seconds: f64,
    detail: String,
    outputs: Vec<StepOutput>,
) -> ToolOutcome {
    if outputs.is_empty() {
        return ToolOutcome::failed(format!(
            "{engine} rendered the clip but it could not be saved — check generations_dir"
        ));
    }
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "engine": engine,
            "seconds": seconds,
            "clips": outputs.len(),
        }),
        detail,
        outputs,
    }
}

// ── music ────────────────────────────────────────────────────────────────────

pub(super) async fn make_music(ctx: &Ctx, engine: &EngineOption, args: &Value) -> ToolOutcome {
    let Some(style) = str_arg(args, "style") else {
        return ToolOutcome::failed("make_music needs a style");
    };
    let lyrics = str_arg(args, "lyrics");
    let seconds = args.get("seconds").and_then(|v| v.as_f64());
    // 🔴 `jobs` is `Some` for the QUEUED engines. sa3 and songgen answer 202 with a
    // job id now, so a body read straight off the submit has no tracks in it —
    // acestep (`/api/v1/music`) is not a queued kind and still answers inline.
    let (path, jobs, body) = match engine.id {
        "sa3" => (
            "/api/v1/sa3/generate",
            Some("/api/v1/sa3/jobs"),
            json!({"prompt": style, "seconds": seconds.unwrap_or(30.0)}),
        ),
        "songgen" => (
            "/api/v1/songgen/generate",
            Some("/api/v1/songgen/jobs"),
            json!({
                "description": style,
                "lyric": lyrics.clone().unwrap_or_default(),
                "lyrics_source": if lyrics.is_some() { "ace" } else { "llm" },
                "duration": seconds,
            }),
        ),
        _ => (
            "/api/v1/music/generate",
            None,
            json!({"caption": style, "lyrics": lyrics, "duration": seconds}),
        ),
    };

    let accepted = match post_json(ctx, path, &body).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("{} render failed: {e}", engine.id)),
    };
    let resp = match jobs {
        None => accepted,
        Some(jobs) => {
            let Some(job_id) = accepted.get("job_id").and_then(|v| v.as_str()) else {
                return ToolOutcome::failed(format!("{} did not return a job id", engine.id));
            };
            match poll_job(ctx, jobs, job_id).await {
                Ok(j) => j,
                Err(e) => {
                    return ToolOutcome::failed(format!("{} render failed: {e}", engine.id));
                }
            }
        }
    };
    let tracks = resp
        .get("tracks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if tracks.is_empty() {
        return ToolOutcome::failed(format!("{} returned no tracks", engine.id));
    }
    // The music engines already persisted these under SONGS_DIR and recorded a
    // generations row; the filename in the response is the deterministic key
    // back to it, so there is nothing to copy.
    let filename = tracks[0]
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    // All three music engines record under the SHARED "music" gallery service
    // (`routes::{music,songgen,sa3}` each pass "music" to `record_generation`),
    // so the lookup key is the gallery's, not the engine's. Keying it by
    // `engine.id` made every sa3 and songgen render report "rendered N track(s)
    // but none could be linked to the gallery" — the row was there, under a
    // service name nothing writes. `edits.rs` already looks it up correctly.
    let outputs = match find_generation_by_filename(ctx, "music", filename).await {
        Some(id) => outputs_from_generation(ctx, &id).await,
        None => Vec::new(),
    };
    let render_ms = resp
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    music_outcome(
        engine.id,
        tracks.len(),
        resp.get("lyrics_used").is_some(),
        gate_detail(
            &resp,
            format!("{} track(s) in {}", tracks.len(), secs(render_ms)),
        ),
        outputs,
    )
}

/// Only tracks with a gallery row behind them are countable: the engine wrote
/// the audio under SONGS_DIR, but a number the conversation cannot play back is
/// a claim, not a track.
fn music_outcome(
    engine: &str,
    rendered: usize,
    wrote_lyrics: bool,
    detail: String,
    outputs: Vec<StepOutput>,
) -> ToolOutcome {
    if outputs.is_empty() {
        return ToolOutcome::failed(format!(
            "{engine} rendered {rendered} track(s) but none could be linked to the gallery"
        ));
    }
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "engine": engine,
            "tracks": outputs.len(),
            "wrote_lyrics": wrote_lyrics,
        }),
        detail,
        outputs,
    }
}

// ── speech ───────────────────────────────────────────────────────────────────

pub(super) async fn make_speech(ctx: &Ctx, engine: &EngineOption, args: &Value) -> ToolOutcome {
    let Some(text) = str_arg(args, "text") else {
        return ToolOutcome::failed("make_speech needs the text to speak");
    };
    let path = "/api/v1/tts/generate";
    // The tier-2 route defaults an unnamed transport to streamed pcm; this
    // caller wants a single wav blob it can hand straight to the chat turn, so
    // it names the buffered transport explicitly.
    let body = json!({
        "input": text,
        "stream_format": "none",
        "response_format": "wav",
        "instructions": str_arg(args, "instructions"),
    });

    // Unlike every other engine this one answers with the audio itself, not
    // JSON — so the agent owns the copy it hands back.
    let started = Instant::now();
    let (bytes, mime) = match post_bytes(ctx, path, &body).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("{} speech failed: {e}", engine.id)),
    };
    let outputs = persist_agent_outputs(
        ctx,
        "make_speech",
        json!({"engine": engine.id, "input": text}),
        vec![OutputBytes {
            ext: ext_for_audio(&mime).to_string(),
            mime,
            kind: "audio".to_string(),
            bytes,
        }],
    )
    .await;
    ToolOutcome {
        ok: !outputs.is_empty(),
        observation: json!({"ok": !outputs.is_empty(), "engine": engine.id, "words": text.split_whitespace().count()}),
        detail: format!("spoken in {}", secs(started.elapsed().as_millis() as u64)),
        outputs,
    }
}

// ── transcribe ───────────────────────────────────────────────────────────────

pub(super) async fn transcribe(ctx: &Ctx, args: &Value) -> ToolOutcome {
    let Some(source) = ctx.media.latest_audio() else {
        return ToolOutcome::failed("there is no audio in this conversation to transcribe");
    };
    let Ok(bytes) = resolve(ctx, source).await else {
        return ToolOutcome::failed("the audio could not be read back");
    };

    let mut form =
        reqwest::multipart::Form::new().part("file", file_part(bytes, &source.mime, "input.wav"));
    if let Some(lang) = str_arg(args, "language") {
        form = form.text("language", lang);
    }

    let resp = match post_multipart(ctx, "/api/v1/stt/transcribe", form).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("transcription failed: {e}")),
    };
    let text = resp
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return ToolOutcome::failed("the audio produced no transcript");
    }
    let outputs = persist_agent_outputs(
        ctx,
        "transcribe",
        json!({"chars": text.len()}),
        vec![OutputBytes {
            ext: "txt".to_string(),
            mime: "text/plain".to_string(),
            kind: "text".to_string(),
            bytes: Bytes::from(text.clone().into_bytes()),
        }],
    )
    .await;
    transcribe_outcome(&text, outputs)
}

/// Unlike a render, the transcript itself survives a failed persist — it rides
/// in the observation — so this stays a success. The step row must not imply a
/// file the browser can open, though.
fn transcribe_outcome(text: &str, outputs: Vec<StepOutput>) -> ToolOutcome {
    let detail = match outputs.is_empty() {
        true => format!(
            "{} characters (not saved to the gallery — check generations_dir)",
            text.len()
        ),
        false => format!("{} characters", text.len()),
    };
    ToolOutcome {
        ok: true,
        // The transcript IS the useful observation, so it rides in the payload
        // — capped, because it becomes prompt tokens.
        observation: json!({"ok": true, "text": truncate(text, 4000)}),
        detail,
        outputs,
    }
}

// ── background removal ───────────────────────────────────────────────────────

pub(super) async fn remove_background(ctx: &Ctx) -> ToolOutcome {
    let Some(source) = ctx.media.latest_image() else {
        return ToolOutcome::failed("there is no image in this conversation to cut out");
    };
    let Ok(bytes) = resolve(ctx, source).await else {
        return ToolOutcome::failed("the image could not be read back");
    };
    let resp = match post_raw(ctx, "/api/v1/matting/remove", &source.mime, bytes).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("background removal failed: {e}")),
    };
    let Some(gen_id) = resp.get("generation_id").and_then(|v| v.as_str()) else {
        return ToolOutcome::failed(
            "the cutout was produced but nothing was saved to the gallery — check generations_dir",
        );
    };
    let outputs = outputs_from_generation(ctx, gen_id).await;
    ToolOutcome {
        ok: true,
        observation: json!({"ok": true, "cutouts": outputs.len()}),
        detail: resp
            .get("backend")
            .and_then(|v| v.as_str())
            .map(|b| format!("cut out with {b}"))
            .unwrap_or_else(|| "cutout ready".to_string()),
        outputs,
    }
}

/// `make_3d_asset` — the most recent image → a GLB in the gallery (§5a-d).
///
/// Multipart out (a `request` JSON part plus the picture as bytes), URLs back. The bundle is
/// deliberately NOT downloaded or inlined here: the observation names it, and the user's client
/// fetches it if they want it — an agent turn must never carry an artifact (R5).
pub(super) async fn make_3d_asset(ctx: &Ctx, args: &Value) -> ToolOutcome {
    let Some(source) = ctx.media.latest_image() else {
        return ToolOutcome::failed("there is no image in this conversation to turn into a model");
    };
    let Ok(bytes) = resolve(ctx, source).await else {
        return ToolOutcome::failed("the image could not be read back");
    };
    let request = json!({
        "rig": args.get("rig").and_then(Value::as_bool).unwrap_or(false),
        "texture_mode": str_arg(args, "texture_mode").unwrap_or_else(|| "project_front".to_string()),
        // The cutout is what SPAR3D conditions best on, and the route chains it for us.
        "remove_background": true,
        "generate_lods": false,
    });
    let form = reqwest::multipart::Form::new()
        .text("request", request.to_string())
        .part(
            "image",
            reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name("input.png")
                .mime_str(&source.mime)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(bytes.to_vec())),
        );
    let started = Instant::now();
    let resp = match post_multipart(ctx, "/api/v1/3d/generate", form).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(format!("image-to-rig failed: {e}")),
    };
    let Some(gen_id) = resp.get("generation_id").and_then(|v| v.as_str()) else {
        return ToolOutcome::failed(
            "the model was generated but nothing was saved to the gallery — check generations_dir",
        );
    };
    let outputs = outputs_from_generation(ctx, gen_id).await;
    let rigged = request["rig"].as_bool().unwrap_or(false);
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "generation_id": gen_id,
            "rigged": rigged,
            // Names the bundle rather than carrying it: R5 — nothing that reaches the model's
            // context holds media.
            "bundle_url": resp.get("bundle_url").and_then(Value::as_str),
        }),
        detail: format!(
            "3D model ready in {}{}",
            secs(started.elapsed().as_millis() as u64),
            if rigged { " (rigged)" } else { "" }
        ),
        outputs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(kind: &str) -> Vec<StepOutput> {
        vec![StepOutput {
            kind: kind.to_string(),
            url: "/api/v1/generations/abc/file/0".to_string(),
            generation_id: Some("abc".to_string()),
            mime: "video/webm".to_string(),
        }]
    }

    /// `persist_generation` returns `None` on an unset `generations_dir`, a full
    /// disk or an EPERM and only `warn!`s. Reporting `ok: true, clips: 0` made
    /// the model narrate a success whose only copy the job store sweeps in an
    /// hour — the render is gone and nobody was told.
    #[test]
    fn a_video_whose_copy_was_lost_is_not_reported_as_a_success() {
        let lost = video_outcome("ltx", 4.0, "4s clip in 61.0s".to_string(), Vec::new());
        assert!(!lost.ok);
        assert_eq!(lost.observation["ok"], false);
        assert!(
            lost.observation["error"]
                .as_str()
                .unwrap_or_default()
                .contains("generations_dir"),
            "{}",
            lost.observation
        );

        let kept = video_outcome("ltx", 4.0, "4s clip in 61.0s".to_string(), saved("video"));
        assert!(kept.ok);
        assert_eq!(kept.observation["clips"], 1);
        assert_eq!(kept.detail, "4s clip in 61.0s");
    }

    /// The engine wrote the track under SONGS_DIR and the row exists, but the
    /// step could not link to it — claiming `tracks: 1` describes audio the
    /// conversation cannot play.
    #[test]
    fn music_reports_the_tracks_the_step_can_actually_play() {
        let lost = music_outcome(
            "music",
            1,
            false,
            "1 track(s) in 20.0s".to_string(),
            Vec::new(),
        );
        assert!(!lost.ok);
        assert_eq!(lost.observation["ok"], false);

        let kept = music_outcome(
            "music",
            1,
            true,
            "1 track(s) in 20.0s".to_string(),
            saved("audio"),
        );
        assert!(kept.ok);
        assert_eq!(kept.observation["tracks"], 1);
        assert_eq!(kept.observation["wrote_lyrics"], true);
    }

    #[test]
    fn a_two_track_render_that_linked_one_track_reports_one() {
        let outcome = music_outcome("sa3", 2, false, "2 track(s)".to_string(), saved("audio"));
        assert_eq!(outcome.observation["tracks"], 1);
    }

    /// The transcript is delivered in the observation whatever happens to the
    /// .txt copy, so this stays a success — but the step must not imply a file
    /// the browser can open.
    #[test]
    fn a_transcript_that_could_not_be_filed_still_reaches_the_model_but_says_so() {
        let outcome = transcribe_outcome("hello there", Vec::new());
        assert!(outcome.ok);
        assert_eq!(outcome.observation["text"], "hello there");
        assert!(outcome.detail.contains("not saved"), "{}", outcome.detail);

        let filed = transcribe_outcome("hello there", saved("text"));
        assert_eq!(filed.detail, "11 characters");
    }
}
