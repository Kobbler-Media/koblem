use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand};
use koblem_client::types::{
    AvatarAsset, AvatarAssetsResponse, AvatarAudioSource, AvatarGenerateRequest,
    AvatarGenerateResponse, AvatarImageSource, AvatarOrientation, AvatarProps,
    AvatarScriptFromLlmRequest, AvatarScriptFromLlmResponse,
};
use reqwest::multipart;

use crate::client::file_part;
use crate::commands::refine::{RefineArgs, read_opt};
use crate::config::Config;
use crate::{Ctx, output};

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct AvatarArgs {
    #[command(subcommand)]
    pub cmd: Option<AvatarCmd>,

    /// Portrait image to animate (upload).
    #[arg(long, value_name = "PATH")]
    pub image: Option<PathBuf>,
    /// Portrait by URL.
    #[arg(long)]
    pub image_url: Option<String>,
    /// Portrait from a saved avatar asset id.
    #[arg(long)]
    pub asset_id: Option<String>,

    /// Driving audio (upload).
    #[arg(long, value_name = "PATH")]
    pub audio: Option<PathBuf>,
    /// Driving audio by URL.
    #[arg(long)]
    pub audio_url: Option<String>,
    /// Speak this script via TTS. `@PATH` reads a file, `@-` reads stdin.
    #[arg(long, value_name = "TEXT|@FILE")]
    pub tts_script: Option<String>,
    /// Have the LLM write a script from this brief, then TTS it.
    #[arg(long)]
    pub tts_brief: Option<String>,
    /// Voice id for --tts-script / --tts-brief.
    #[arg(long)]
    pub voice: Option<String>,

    #[arg(long)]
    pub prompt: Option<String>,
    /// "480p" (default) | "720p" | "WxH".
    #[arg(long)]
    pub resolution: Option<String>,
    /// portrait (default) | landscape.
    #[arg(long)]
    pub orientation: Option<String>,
    #[arg(long)]
    pub duration: Option<f64>,
    #[arg(long)]
    pub segments: Option<u32>,
    #[arg(long)]
    pub segment_frames: Option<u32>,
    #[arg(long)]
    pub cont_cond_frames: Option<u32>,
    #[arg(long)]
    pub steps: Option<u32>,
    #[arg(long)]
    pub cfg: Option<f64>,
    #[arg(long)]
    pub seed: Option<i64>,
    /// Don't persist the clip to the gallery.
    #[arg(long)]
    pub no_save: bool,
    /// Output file (default: server filename, .webm).
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum AvatarCmd {
    /// List saved portrait assets, or `assets add|delete` to manage them.
    Assets(AssetsArgs),
    /// Print the avatar engine's defaults/health.
    Props,
    /// Have the LLM write a short spoken script from a brief.
    Script(ScriptArgs),
    /// Kill the in-flight render (frees the GPU).
    Cancel,
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct AssetsArgs {
    #[command(subcommand)]
    pub cmd: Option<AssetsCmd>,
}

#[derive(Subcommand, Debug)]
pub enum AssetsCmd {
    /// Save a portrait image for reuse across renders.
    Add {
        #[arg(value_name = "PATH")]
        image: PathBuf,
    },
    /// Delete a saved portrait asset by id.
    Delete { id: String },
}

#[derive(Args, Debug)]
pub struct ScriptArgs {
    /// What the avatar should talk about.
    pub brief: String,
    /// Target seconds at normal speech rate (~2.5 wps).
    #[arg(long)]
    pub duration: Option<f64>,
    #[command(flatten)]
    pub refine: RefineArgs,
}

fn parse_orientation(s: &str) -> Result<AvatarOrientation> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| anyhow!("unknown orientation '{s}' (portrait|landscape)"))
}

pub async fn run(ctx: &Ctx, args: AvatarArgs) -> Result<()> {
    match args.cmd {
        Some(AvatarCmd::Assets(a)) => {
            return match a.cmd {
                None => list_assets(ctx).await,
                Some(AssetsCmd::Add { image }) => add_asset(ctx, &image).await,
                Some(AssetsCmd::Delete { id }) => delete_asset(ctx, &id).await,
            };
        }
        Some(AvatarCmd::Props) => return props(ctx).await,
        Some(AvatarCmd::Script(a)) => return script(ctx, a).await,
        Some(AvatarCmd::Cancel) => return cancel(ctx).await,
        None => {}
    }
    generate(ctx, args).await
}

async fn props(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/api/v1/avatar/props", &[]).await?;
    let body: AvatarProps = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("healthy: {}", body.healthy);
    println!("resolution: {}", body.default_resolution);
    println!(
        "segment_frames: {}  cont_cond_frames: {}  steps: {}",
        body.default_segment_frames, body.default_cont_cond_frames, body.default_steps
    );
    if let Some(s) = body.last_render_sec {
        println!("last render: {s:.1}s");
    }
    Ok(())
}

async fn script(ctx: &Ctx, args: ScriptArgs) -> Result<()> {
    let (refine_source, refine_instruction) = args.refine.resolve().await?;
    let req = AvatarScriptFromLlmRequest {
        brief: args.brief,
        duration_sec: args.duration,
        refine_source,
        refine_instruction,
    };
    let resp = ctx
        .client
        .post_json("/api/v1/avatar/script-from-llm", &[], &req)
        .await?;
    let body: AvatarScriptFromLlmResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.script);
    Ok(())
}

async fn cancel(ctx: &Ctx) -> Result<()> {
    let resp = ctx
        .client
        .post_json("/api/v1/avatar/cancel", &[], &())
        .await?;
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if ctx.json {
        return output::print_json(&body);
    }
    println!("cancel requested");
    Ok(())
}

async fn add_asset(ctx: &Ctx, image: &std::path::Path) -> Result<()> {
    let form = multipart::Form::new().part("image", file_part(image).await?);
    let resp = ctx
        .client
        .post_multipart("/api/v1/avatar/assets", &[], form)
        .await?;
    let body: AvatarAsset = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}\t{}", body.id, body.filename);
    Ok(())
}

async fn list_assets(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/api/v1/avatar/assets", &[]).await?;
    let body: AvatarAssetsResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    if body.assets.is_empty() {
        println!("(no portrait assets)");
    } else {
        for a in &body.assets {
            println!("{}\t{}", a.id, a.filename);
        }
    }
    Ok(())
}

async fn delete_asset(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.client
        .delete(&format!("/api/v1/avatar/assets/{id}"), &[])
        .await?;
    println!("deleted asset {id}");
    Ok(())
}

pub async fn build_generate_request(
    args: &AvatarArgs,
    cfg: &Config,
) -> Result<AvatarGenerateRequest> {
    let image_source = if args.image.is_some() {
        AvatarImageSource::Upload
    } else if let Some(url) = args.image_url.clone() {
        AvatarImageSource::Url { url }
    } else if let Some(id) = args.asset_id.clone() {
        AvatarImageSource::LibraryAssetId { id }
    } else {
        bail!("provide one of --image, --image-url, --asset-id");
    };

    let audio_source = if args.audio.is_some() {
        AvatarAudioSource::Upload
    } else if let Some(url) = args.audio_url.clone() {
        AvatarAudioSource::Url { url }
    } else if let Some(script) = read_opt(&args.tts_script).await? {
        AvatarAudioSource::Tts {
            script: Some(script),
            script_from_brief: None,
            voice: args.voice.clone(),
            engine: None,
        }
    } else if let Some(brief) = args.tts_brief.clone() {
        AvatarAudioSource::Tts {
            script: None,
            script_from_brief: Some(brief),
            voice: args.voice.clone(),
            engine: None,
        }
    } else {
        bail!("provide one of --audio, --audio-url, --tts-script, --tts-brief");
    };

    let orientation = match args.orientation {
        Some(ref s) => parse_orientation(s)?,
        None => AvatarOrientation::default(),
    };

    Ok(AvatarGenerateRequest {
        image_source,
        audio_source,
        prompt: args.prompt.clone(),
        resolution: args
            .resolution
            .clone()
            .or_else(|| cfg.def_str("avatar", "resolution")),
        orientation,
        duration_sec: args.duration,
        segments: args.segments,
        segment_frames: args
            .segment_frames
            .or_else(|| cfg.def_u32("avatar", "segment_frames")),
        cont_cond_frames: args.cont_cond_frames,
        steps: args.steps.or_else(|| cfg.def_u32("avatar", "steps")),
        cfg_scale: args.cfg,
        seed: args.seed,
        save_clip: !args.no_save,
        ..Default::default()
    })
}

async fn generate(ctx: &Ctx, args: AvatarArgs) -> Result<()> {
    let req = build_generate_request(&args, &ctx.config).await?;

    let needs_multipart = args.image.is_some() || args.audio.is_some();
    let resp = if needs_multipart {
        let mut form = multipart::Form::new().text("request", serde_json::to_string(&req)?);
        if let Some(p) = &args.image {
            form = form.part("image", file_part(p).await?);
        }
        if let Some(p) = &args.audio {
            form = form.part("audio", file_part(p).await?);
        }
        ctx.client
            .post_multipart("/api/v1/avatar/generate", &[], form)
            .await?
    } else {
        ctx.client
            .post_json("/api/v1/avatar/generate", &[], &req)
            .await?
    };

    let body: AvatarGenerateResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }

    let target = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(&body.clip.filename));
    crate::download::to_named(ctx, &body.clip.url, &target).await?;
    if let Some(script) = &body.script_used {
        eprintln!("script: {script}");
    }
    eprintln!(
        "segments={} render={}ms",
        body.segments_rendered, body.render_ms
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> AvatarArgs {
        AvatarArgs {
            cmd: None,
            image: Some(PathBuf::from("face.png")),
            image_url: None,
            asset_id: None,
            audio: None,
            audio_url: None,
            tts_script: Some("hi there".into()),
            tts_brief: None,
            voice: None,
            prompt: None,
            resolution: None,
            orientation: None,
            duration: None,
            segments: None,
            segment_frames: None,
            cont_cond_frames: None,
            steps: None,
            cfg: None,
            seed: None,
            no_save: false,
            output: None,
        }
    }

    #[tokio::test]
    async fn save_clip_is_on_unless_the_caller_opts_out() {
        assert!(
            build_generate_request(&args(), &Config::default())
                .await
                .unwrap()
                .save_clip
        );
        let mut a = args();
        a.no_save = true;
        assert!(
            !build_generate_request(&a, &Config::default())
                .await
                .unwrap()
                .save_clip
        );
    }

    #[tokio::test]
    async fn tts_script_honours_the_at_file_convention() {
        let path = std::env::temp_dir().join("kob-avatar-script.txt");
        tokio::fs::write(&path, "line one\nline two").await.unwrap();
        let mut a = args();
        a.tts_script = Some(format!("@{}", path.display()));
        let req = build_generate_request(&a, &Config::default())
            .await
            .unwrap();
        let AvatarAudioSource::Tts { script, .. } = req.audio_source else {
            panic!("expected a TTS audio source");
        };
        assert_eq!(script.as_deref(), Some("line one\nline two"));
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn a_portrait_and_a_voice_are_both_required() {
        let mut a = args();
        a.image = None;
        let e = build_generate_request(&a, &Config::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("--image"), "{e}");

        let mut a = args();
        a.tts_script = None;
        let e = build_generate_request(&a, &Config::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("--audio"), "{e}");
    }

    #[tokio::test]
    async fn config_defaults_fill_in_for_unset_render_knobs() {
        let cfg = Config::with_defaults(
            r#"[avatar]
resolution = "720p"
segment_frames = 53
steps = 6"#,
        );
        let req = build_generate_request(&args(), &cfg).await.unwrap();
        assert_eq!(req.resolution.as_deref(), Some("720p"));
        assert_eq!(req.segment_frames, Some(53));
        assert_eq!(req.steps, Some(6));
    }
}
