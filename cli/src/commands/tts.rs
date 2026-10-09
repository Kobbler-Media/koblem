use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use koblem_client::types::{TtsGenerateRequest, TtsUploadResponse, TtsVoicesResponse};
use reqwest::multipart;

use crate::client::file_part;
use crate::config::Config;
use crate::{Ctx, output};

/// There is one TTS engine, so the route base is a constant rather than a
/// per-engine lookup.
const BASE: &str = "/api/v1/tts";
const CFG_KEY: &str = "tts";

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct TtsArgs {
    #[command(subcommand)]
    pub cmd: Option<TtsCmd>,

    /// Text to synthesize (omit when using a subcommand).
    pub text: Option<String>,
    /// A cloned voice id. Omit to let the engine pick one and hold it.
    #[arg(long)]
    pub voice: Option<String>,
    /// Output codec: wav (default) | mp3 | pcm.
    #[arg(long)]
    pub format: Option<String>,
    /// Voice-design instructions, e.g. "a gruff sea captain, slow and gravelly".
    /// Only meaningful without --voice.
    #[arg(long)]
    pub instructions: Option<String>,
    /// Built-in speaker token S0..S9. For a stable narrator use --voice.
    #[arg(long)]
    pub speaker: Option<String>,
    /// mp3 only.
    #[arg(long)]
    pub bitrate: Option<u32>,
    /// Render rate. 1.0 is natural. This is a real re-render, not playback
    /// speed — a rate the engine can't do falls back to 1.0 and says so.
    #[arg(long)]
    pub speed: Option<f64>,
    #[arg(long)]
    pub temperature: Option<f64>,
    #[arg(long)]
    pub top_p: Option<f64>,
    #[arg(long)]
    pub top_k: Option<u32>,
    #[arg(long)]
    pub repetition_penalty: Option<f64>,
    #[arg(long)]
    pub seed: Option<u64>,
    /// Generation cap in 12.5 Hz FRAMES, not audio tokens. Raising this does
    /// not buy longer output — past the context window it turns a silent
    /// truncation into a hard error. Leave it alone and rely on long-form mode.
    #[arg(long)]
    pub max_new_frames: Option<u32>,
    /// Disable long-form mode. On by default: it chunks internally with rolling
    /// context, which is what holds one voice across a long passage. Turning it
    /// off truncates anything past ~60 s.
    #[arg(long)]
    pub no_long: bool,
    /// Words per internal chunk in long-form mode.
    #[arg(long)]
    pub chunk_words: Option<u32>,
    /// Keep the rendered audio as a testbed sample on the server.
    #[arg(long)]
    pub save_sample: bool,
    /// Output file (default: speech.<format>).
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum TtsCmd {
    /// Manage cloned voices.
    Voices(VoicesArgs),
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct VoicesArgs {
    #[command(subcommand)]
    pub cmd: Option<VoicesCmd>,
}

#[derive(Subcommand, Debug)]
pub enum VoicesCmd {
    /// Clone a voice from a reference clip.
    Add(AddVoiceArgs),
    /// Delete a cloned voice by id.
    Delete { name: String },
}

#[derive(Args, Debug)]
pub struct AddVoiceArgs {
    /// Name/id to register the voice under.
    pub name: String,
    /// Reference audio clip (WAV).
    #[arg(long, value_name = "PATH")]
    pub sample: PathBuf,
    /// Transcript of the clip — raises clone fidelity.
    #[arg(long)]
    pub ref_text: Option<String>,
}

pub fn build_request(
    args: &TtsArgs,
    text: String,
    format: String,
    cfg: &Config,
) -> TtsGenerateRequest {
    TtsGenerateRequest {
        input: text,
        voice: args.voice.clone().or_else(|| cfg.def_str(CFG_KEY, "voice")),
        instructions: args.instructions.clone(),
        speaker: args.speaker.clone(),
        response_format: Some(format),
        bitrate_kbps: args.bitrate.or_else(|| cfg.def_u32(CFG_KEY, "bitrate")),
        speed: args.speed.or_else(|| cfg.def_f64(CFG_KEY, "speed")),
        temperature: args
            .temperature
            .or_else(|| cfg.def_f64(CFG_KEY, "temperature")),
        top_p: args.top_p,
        top_k: args.top_k,
        repetition_penalty: args.repetition_penalty,
        seed: args.seed,
        max_new_frames: args.max_new_frames,
        // Only send `long` when turning it OFF; the server default is on, and
        // leaving the field absent keeps `request_meta` to the keys that matter.
        long: args.no_long.then_some(false),
        chunk_words: args.chunk_words,
        save_sample: args.save_sample,
        ..Default::default()
    }
}

pub async fn run(ctx: &Ctx, args: TtsArgs) -> Result<()> {
    if let Some(TtsCmd::Voices(v)) = args.cmd {
        return match v.cmd {
            None => list_voices(ctx).await,
            Some(VoicesCmd::Add(a)) => add_voice(ctx, a).await,
            Some(VoicesCmd::Delete { name }) => delete_voice(ctx, &name).await,
        };
    }
    let text = args
        .text
        .clone()
        .context("missing TEXT to synthesize (see `kob tts --help`)")?;
    generate(ctx, args, text).await
}

async fn generate(ctx: &Ctx, args: TtsArgs, text: String) -> Result<()> {
    let cfg = &ctx.config;
    let format = args
        .format
        .clone()
        .or_else(|| cfg.def_str(CFG_KEY, "format"))
        .unwrap_or_else(|| "wav".to_string());
    let req = build_request(&args, text, format.clone(), cfg);
    let resp = ctx
        .client
        .post_json(&format!("{BASE}/generate"), &[], &req)
        .await?;

    // Prefer the server's content-type for the extension; fall back to format.
    let ext = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| output::ext_for_mime(ct).to_string())
        .filter(|e| e != "bin")
        .unwrap_or_else(|| format.clone());

    // What the render ACTUALLY ran at. A rate the engine can't do is not an
    // error — it silently becomes 1.0 — so it has to be said out loud, or the
    // file looks like a failed --speed rather than a refused one.
    let speed_applied = resp
        .headers()
        .get("x-speed-applied")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(1.0);
    let requested_speed = req.speed.unwrap_or(1.0);

    let bytes = resp.bytes().await?;
    if ctx.json {
        output::print_json(&serde_json::json!({
            "bytes": bytes.len(),
            "format": ext,
            "speed_applied": speed_applied,
        }))?;
        return Ok(());
    }
    if (speed_applied - requested_speed).abs() > f64::EPSILON {
        eprintln!("note: engine refused speed {requested_speed}; rendered at {speed_applied}");
    }
    let target = output::single_target("speech", &ext, &args.output);
    output::write_file(&target, &bytes).await
}

async fn list_voices(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get(&format!("{BASE}/voices"), &[]).await?;
    let body: TtsVoicesResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    if body.voices.is_empty() {
        println!("(no voices)");
    }
    for v in &body.voices {
        println!(
            "{}\t{} frames{}",
            v.id,
            v.frames.unwrap_or(0),
            if v.has_ref_text { "\tref_text" } else { "" }
        );
    }
    if let Some(m) = &body.model_id {
        eprintln!("model: {m}");
    }
    Ok(())
}

async fn add_voice(ctx: &Ctx, args: AddVoiceArgs) -> Result<()> {
    let mut form = multipart::Form::new()
        .text("name", args.name.clone())
        .part("audio_sample", file_part(&args.sample).await?);
    if let Some(t) = &args.ref_text {
        form = form.text("ref_text", t.clone());
    }
    let resp = ctx
        .client
        .post_multipart(&format!("{BASE}/voices"), &[], form)
        .await?;
    let body: TtsUploadResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}\t{}", body.id, body.name);
    if let Some(f) = body.ref_frames.or(body.frames) {
        eprintln!("{f} reference frames");
    }
    Ok(())
}

async fn delete_voice(ctx: &Ctx, name: &str) -> Result<()> {
    ctx.client
        .delete(&format!("{BASE}/voices/{name}"), &[])
        .await?;
    println!("deleted voice {name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> TtsArgs {
        TtsArgs {
            cmd: None,
            text: Some("hi".into()),
            voice: None,
            format: None,
            instructions: None,
            speaker: None,
            bitrate: None,
            speed: None,
            temperature: None,
            top_p: None,
            top_k: None,
            repetition_penalty: None,
            seed: None,
            max_new_frames: None,
            no_long: false,
            chunk_words: None,
            save_sample: false,
            output: None,
        }
    }

    /// Long-form is the server's default, so the flag is absent unless the user
    /// explicitly opts out. Sending `long: true` on every request would bloat
    /// `request_meta` for no behaviour change.
    #[test]
    fn long_is_only_sent_when_disabled() {
        let req = build_request(&args(), "hi".into(), "wav".into(), &Config::default());
        assert_eq!(req.long, None);

        let mut a = args();
        a.no_long = true;
        let req = build_request(&a, "hi".into(), "wav".into(), &Config::default());
        assert_eq!(req.long, Some(false));
    }

    /// `speed` stays off the wire unless asked for: an unconditional `1.0`
    /// would be one engine-validation change away from 400-ing every render.
    #[test]
    fn speed_is_absent_unless_requested() {
        let req = build_request(&args(), "hi".into(), "wav".into(), &Config::default());
        assert_eq!(req.speed, None);

        let mut a = args();
        a.speed = Some(1.5);
        let req = build_request(&a, "hi".into(), "wav".into(), &Config::default());
        assert_eq!(req.speed, Some(1.5));
    }

    #[test]
    fn request_keeps_save_sample_and_format() {
        let mut a = args();
        a.save_sample = true;
        a.speaker = Some("S3".into());
        let req = build_request(&a, "hi".into(), "mp3".into(), &Config::default());
        assert!(req.save_sample);
        assert_eq!(req.response_format.as_deref(), Some("mp3"));
        assert_eq!(req.speaker.as_deref(), Some("S3"));
    }

    #[test]
    fn voice_rows_parse_without_a_name_field() {
        let body: TtsVoicesResponse = serde_json::from_str(
            r#"{"model_id":"breeze-tts-2","voices":[{"id":"nar","frames":300,"codebooks":16,
                "has_ref_text":true,"has_sample":true}]}"#,
        )
        .unwrap();
        assert_eq!(body.voices[0].id, "nar");
        assert_eq!(body.voices[0].frames, Some(300));
        assert!(body.voices[0].name.is_none());
        assert_eq!(body.model_id.as_deref(), Some("breeze-tts-2"));
    }

    /// The clone-upload reply reports the encoded reference as `ref_frames`
    /// and/or `frames`; either must survive the decode.
    #[test]
    fn upload_reply_accepts_either_frame_field() {
        let a: TtsUploadResponse =
            serde_json::from_str(r#"{"id":"nar","name":"nar","mode":"clone","ref_frames":150}"#)
                .unwrap();
        assert_eq!(a.ref_frames.or(a.frames), Some(150));
        let b: TtsUploadResponse =
            serde_json::from_str(r#"{"id":"nar","frames":150,"codebooks":16}"#).unwrap();
        assert_eq!(b.ref_frames.or(b.frames), Some(150));
    }
}
