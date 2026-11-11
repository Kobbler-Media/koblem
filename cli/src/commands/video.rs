use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use koblem_client::types::{
    LtxAudioSource, LtxImageSource, LtxQualityProfile, LtxRelipRequest, LtxSegment,
    LtxUpscalerRatio, LtxVideoGenerateRequest, LtxVideoProps,
};
use reqwest::multipart;

use crate::client::file_part;
use crate::config::Config;
use crate::jobs::{self, Engine, PollOpts};
use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum VideoCmd {
    /// Render a clip from one or more shot prompts (async job).
    Generate(GenerateArgs),
    /// List render jobs, or poll/fetch one by id.
    Jobs(JobsArgs),
    /// Kill the in-flight render (frees the GPU).
    Cancel(EngineArgs),
    /// Print the engine's defaults/health.
    Props(EngineArgs),
    /// Lip-dub: re-sync a video's mouths to a new audio track (renders on LTX-2.5).
    Relip(RelipArgs),
}

#[derive(Args, Debug)]
pub struct EngineArgs {
    /// ltx (LTX-2.5) — the only video engine.
    #[arg(long, default_value = "ltx")]
    pub engine: String,
}

#[derive(Args, Debug)]
pub struct JobsArgs {
    #[arg(long, default_value = "ltx")]
    pub engine: String,
    /// Poll one job to completion and download its clip.
    #[arg(long)]
    pub id: Option<String>,
    /// With --id: print the status once and exit instead of waiting.
    #[arg(long)]
    pub once: bool,
    #[arg(long, default_value_t = 2)]
    pub poll_secs: u64,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct GenerateArgs {
    /// Shot prompt. Repeat for a multi-shot chain, in order.
    #[arg(required = true)]
    pub prompts: Vec<String>,
    /// ltx (LTX-2.5) — the only video engine.
    #[arg(long, default_value = "ltx")]
    pub engine: String,
    /// Base DiT variant. ⚠️ LTX-2.5 ships ONE DiT and 501s a named variant.
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub negative: Option<String>,
    #[arg(long)]
    pub width: Option<u32>,
    #[arg(long)]
    pub height: Option<u32>,
    #[arg(long)]
    pub fps: Option<u32>,
    /// Frames per shot (default 97 ≈ 4s @ 24fps).
    #[arg(long)]
    pub segment_frames: Option<u32>,
    /// Overlap latent frames carried between shots.
    #[arg(long)]
    pub cont_latent_frames: Option<u32>,
    #[arg(long)]
    pub steps: Option<u32>,
    #[arg(long)]
    pub cfg: Option<f64>,
    #[arg(long)]
    pub sampler: Option<String>,
    #[arg(long)]
    pub scheduler: Option<String>,
    #[arg(long)]
    pub seed: Option<i64>,
    /// standard (default) | high | upscale_4x | custom. ⚠️ LTX-2.5 declines its
    /// two-stage upscaler for any shot carrying an opening frame, keyframes or
    /// character references, so this only bites on an unconditioned render.
    #[arg(long)]
    pub profile: Option<String>,
    /// none | x1.5 | x2 — overrides the profile's own upscale. Same LTX-2.5
    /// caveat as --profile.
    #[arg(long)]
    pub upscaler_ratio: Option<String>,
    /// Treat --width/--height as the OUTPUT size, sizing the base pass down by
    /// the upscale factor so every profile lands on the same resolution.
    #[arg(long)]
    pub dims_are_target: bool,
    /// Opening frame (i2v).
    #[arg(long, value_name = "PATH")]
    pub image: Option<PathBuf>,
    /// Drive-audio track for the whole chain.
    #[arg(long, value_name = "PATH")]
    pub audio: Option<PathBuf>,
    /// Background music track.
    #[arg(long, value_name = "PATH")]
    pub bgm: Option<PathBuf>,
    /// Continue a failed/cancelled render from its banked segments. Take the
    /// value from `resume_job_id` in the failed job's status.
    #[arg(long)]
    pub resume_job_id: Option<String>,
    /// Don't persist the clip to the gallery.
    #[arg(long)]
    pub no_save: bool,
    /// Submit and print the job id instead of waiting for the render.
    #[arg(long)]
    pub detach: bool,
    /// Download each shot as the engine finishes it.
    #[arg(long)]
    pub save_partials: bool,
    #[arg(long, default_value_t = 2)]
    pub poll_secs: u64,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct RelipArgs {
    /// Source video whose mouths get re-synced.
    #[arg(long, value_name = "PATH")]
    pub video: PathBuf,
    /// Drive audio.
    #[arg(long, value_name = "PATH")]
    pub audio: PathBuf,
    /// Mouth-style / scene prompt.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Audio→video guidance. 1.0 = distilled default.
    #[arg(long)]
    pub a2v_guidance: Option<f64>,
    /// Put the ×2 latent upsampler between two samplers. Off by default: guides
    /// are cropped before it, so the frozen source conditions stage 1 only and
    /// the refine runs with nothing holding the face being dubbed.
    #[arg(long)]
    pub two_stage: bool,
    #[arg(long)]
    pub width: Option<u32>,
    #[arg(long)]
    pub height: Option<u32>,
    #[arg(long)]
    pub fps: Option<u32>,
    #[arg(long)]
    pub seed: Option<i64>,
    /// Strength of the lipdub adapter (None → its trained 1.0).
    #[arg(long)]
    pub lipdub_strength: Option<f64>,
    #[arg(long)]
    pub no_save: bool,
    #[arg(long)]
    pub detach: bool,
    #[arg(long, default_value_t = 2)]
    pub poll_secs: u64,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

fn parse_enum<T: serde::de::DeserializeOwned>(s: &str, what: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| anyhow::anyhow!("unknown {what} '{s}'"))
}

/// Flags LTX-2.5 takes on the wire and does not act on. Sending them anyway is silent — the render
/// succeeds and quietly ignores them — so they are rejected client-side.
///
/// 🔴 The checkpoint is DISTILLED, so the sampling knobs are accepted and ignored — refusing them
/// upstream would hard-fail every render, since the LTX wire carries them always — and it ships ONE
/// DiT, so a named variant is a 501.
///
/// ⚠️ `--profile` and `--upscaler-ratio` are deliberately NOT here, and the Director's own
/// capability record says the opposite (`upscaleChain: false`). Both are right. The engine
/// declines its two-stage upscaler for any request carrying an opening frame, keyframes or
/// character references — which is nearly every DIRECTOR shot, and why the Director hides the
/// picker and forces the pair off in `apply_engine_delivery_limits`. That function runs on the
/// Director routes only. A bare `kob video generate` with no conditioning is exactly the case
/// the upscaler still serves, so the flags travel.
fn unsupported_flags(a: &GenerateArgs) -> Vec<&'static str> {
    [
        ("--model", a.model.is_some()),
        ("--steps", a.steps.is_some()),
        ("--cfg", a.cfg.is_some()),
        ("--sampler", a.sampler.is_some()),
        ("--scheduler", a.scheduler.is_some()),
    ]
    .into_iter()
    .filter(|(_, set)| *set)
    .map(|(flag, _)| flag)
    .collect()
}

/// The multipart part names the route reads. Everything else lands in its
/// `_ => {}` arm and is dropped without a word.
fn file_parts(a: &GenerateArgs) -> Vec<(&'static str, &PathBuf)> {
    [
        ("image", a.image.as_ref()),
        ("audio", a.audio.as_ref()),
        ("bgm", a.bgm.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, path)| path.map(|p| (name, p)))
    .collect()
}

/// `LtxVideoGenerateRequest`, the type `routes/ltx_video.rs` parses.
pub fn build_generate_request(
    engine: Engine,
    args: &GenerateArgs,
    cfg: &Config,
) -> Result<LtxVideoGenerateRequest> {
    let bad = unsupported_flags(args);
    if !bad.is_empty() {
        bail!(
            "engine '{}' can't honour {} — drop the flag(s)",
            engine.name,
            bad.join(", ")
        );
    }
    let profile = match args
        .profile
        .clone()
        .or_else(|| cfg.def_str("video", "profile"))
    {
        Some(s) => Some(parse_enum::<LtxQualityProfile>(&s, "profile")?),
        None => None,
    };
    let upscaler_ratio = match args
        .upscaler_ratio
        .clone()
        .or_else(|| cfg.def_str("video", "upscaler_ratio"))
    {
        Some(s) => Some(parse_enum::<LtxUpscalerRatio>(&s, "upscaler ratio")?),
        None => None,
    };
    Ok(LtxVideoGenerateRequest {
        segments: args
            .prompts
            .iter()
            .map(|p| LtxSegment {
                prompt: p.clone(),
                ..Default::default()
            })
            .collect(),
        model: args.model.clone().or_else(|| cfg.def_str("video", "model")),
        // File inputs ride as multipart parts; the enum only declares the source.
        image_source: if args.image.is_some() {
            LtxImageSource::Upload
        } else {
            LtxImageSource::None
        },
        audio_source: match (&args.audio, &args.bgm) {
            (Some(_), Some(_)) => LtxAudioSource::MixedUpload,
            (Some(_), None) => LtxAudioSource::VoiceUpload,
            (None, Some(_)) => LtxAudioSource::BgmUpload,
            (None, None) => LtxAudioSource::None,
        },
        negative_prompt: args
            .negative
            .clone()
            .or_else(|| cfg.def_str("video", "negative")),
        width: args.width.or_else(|| cfg.def_u32("video", "width")),
        height: args.height.or_else(|| cfg.def_u32("video", "height")),
        fps: args.fps.or_else(|| cfg.def_u32("video", "fps")),
        segment_frames: args
            .segment_frames
            .or_else(|| cfg.def_u32("video", "segment_frames")),
        cont_latent_frames: args
            .cont_latent_frames
            .or_else(|| cfg.def_u32("video", "cont_latent_frames")),
        steps: args.steps.or_else(|| cfg.def_u32("video", "steps")),
        cfg_scale: args.cfg.or_else(|| cfg.def_f64("video", "cfg")),
        sampling_method: args
            .sampler
            .clone()
            .or_else(|| cfg.def_str("video", "sampler")),
        scheduler: args
            .scheduler
            .clone()
            .or_else(|| cfg.def_str("video", "scheduler")),
        seed: args.seed,
        profile,
        upscaler_ratio,
        dims_are_target: args.dims_are_target.then_some(true),
        resume_job_id: args.resume_job_id.clone(),
        save_clip: !args.no_save,
        ..Default::default()
    })
}

pub fn build_relip_request(args: &RelipArgs) -> LtxRelipRequest {
    LtxRelipRequest {
        prompt: args.prompt.clone(),
        a2v_guidance: args.a2v_guidance,
        two_stage: args.two_stage,
        width: args.width,
        height: args.height,
        fps: args.fps,
        seed: args.seed,
        save_clip: !args.no_save,
        lipdub_strength: args.lipdub_strength,
    }
}

pub async fn run(ctx: &Ctx, cmd: VideoCmd) -> Result<()> {
    match cmd {
        VideoCmd::Generate(args) => generate(ctx, args).await,
        VideoCmd::Jobs(args) => list_or_poll(ctx, args).await,
        VideoCmd::Cancel(a) => cancel(ctx, Engine::parse(&a.engine)?).await,
        VideoCmd::Props(a) => props(ctx, Engine::parse(&a.engine)?).await,
        VideoCmd::Relip(args) => relip(ctx, args).await,
    }
}

async fn generate(ctx: &Ctx, args: GenerateArgs) -> Result<()> {
    let engine = Engine::parse(&args.engine)?;
    let req = build_generate_request(engine, &args, &ctx.config)?;
    let body = serde_json::to_string(&req)?;

    let parts = file_parts(&args);
    let path = format!("{}/generate", engine.base);
    let resp = if parts.is_empty() {
        ctx.client
            .post_raw(&path, &[], "application/json", body.into_bytes())
            .await?
    } else {
        let mut form = multipart::Form::new().text("request", body);
        for (name, p) in parts {
            form = form.part(name, file_part(p).await?);
        }
        ctx.client.post_multipart(&path, &[], form).await?
    };

    let opts = PollOpts {
        interval: Duration::from_secs(args.poll_secs.max(1)),
        save_partials: args.save_partials,
    };
    jobs::run_to_completion(ctx, engine, resp, &opts, args.detach, &args.output).await
}

async fn relip(ctx: &Ctx, args: RelipArgs) -> Result<()> {
    let engine = jobs::LTX_VIDEO;
    let req = build_relip_request(&args);
    let form = multipart::Form::new()
        .text("request", serde_json::to_string(&req)?)
        .part("video", file_part(&args.video).await?)
        .part("audio", file_part(&args.audio).await?);
    let resp = ctx
        .client
        .post_multipart(&format!("{}/relip", engine.base), &[], form)
        .await?;
    let opts = PollOpts {
        interval: Duration::from_secs(args.poll_secs.max(1)),
        save_partials: false,
    };
    jobs::run_to_completion(ctx, engine, resp, &opts, args.detach, &args.output).await
}

async fn list_or_poll(ctx: &Ctx, args: JobsArgs) -> Result<()> {
    let engine = Engine::parse(&args.engine)?;
    let Some(id) = args.id else {
        let all = jobs::list_jobs(ctx, engine).await?;
        if ctx.json {
            return output::print_json(&all);
        }
        if all.is_empty() {
            println!("(no jobs)");
        }
        for j in &all {
            println!("{}", jobs::summarize(j));
        }
        return Ok(());
    };

    if args.once {
        let status = jobs::job_status(ctx, engine, &id).await?;
        if ctx.json {
            return output::print_json(&status);
        }
        println!("{}", jobs::summarize(&status));
        return Ok(());
    }

    let opts = PollOpts {
        interval: Duration::from_secs(args.poll_secs.max(1)),
        save_partials: false,
    };
    let status = jobs::wait(ctx, engine, &id, &opts, &args.output).await?;
    if ctx.json {
        return output::print_json(&status);
    }
    jobs::download_media(ctx, engine, &id, &args.output).await?;
    jobs::warn_if_partial(&status);
    Ok(())
}

async fn cancel(ctx: &Ctx, engine: Engine) -> Result<()> {
    let resp = ctx
        .client
        .post_json(&engine.cancel_path(), &[], &())
        .await?;
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if ctx.json {
        return output::print_json(&body);
    }
    println!("cancel requested");
    Ok(())
}

async fn props(ctx: &Ctx, engine: Engine) -> Result<()> {
    let resp = ctx.client.get(&engine.props_path(), &[]).await?;
    let body: LtxVideoProps = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("healthy: {}", body.healthy);
    println!("resolution: {}", body.default_resolution);
    println!(
        "frames/shot: {}  overlap: {}  steps: {}  fps: {}",
        body.default_segment_frames,
        body.default_cont_latent_frames,
        body.default_steps,
        body.default_fps
    );
    if let Some(s) = body.last_render_sec {
        println!("last render: {s:.1}s");
    }
    if !body.variants.is_empty() {
        println!("variants: {}", body.variants.join(", "));
    }
    for l in &body.loras {
        println!("lora  {}  x{}", l.path, l.default_multiplier);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> GenerateArgs {
        GenerateArgs {
            prompts: vec!["a wide shot".into(), "a close up".into()],
            engine: "ltx".into(),
            model: None,
            negative: None,
            width: None,
            height: None,
            fps: None,
            segment_frames: None,
            cont_latent_frames: None,
            steps: None,
            cfg: None,
            sampler: None,
            scheduler: None,
            seed: None,
            profile: None,
            upscaler_ratio: None,
            dims_are_target: false,
            image: None,
            audio: None,
            bgm: None,
            resume_job_id: None,
            no_save: false,
            detach: false,
            save_partials: false,
            poll_secs: 2,
            output: None,
        }
    }

    fn ltx(a: &GenerateArgs) -> LtxVideoGenerateRequest {
        build_generate_request(jobs::LTX_VIDEO, a, &Config::default()).unwrap()
    }

    #[test]
    fn one_segment_is_built_per_prompt_in_order() {
        let req = ltx(&args());
        assert_eq!(req.segments.len(), 2);
        assert_eq!(req.segments[0].prompt, "a wide shot");
        assert_eq!(req.segments[1].prompt, "a close up");
    }

    #[test]
    fn attached_media_selects_the_matching_source_variant() {
        let mut a = args();
        assert!(matches!(ltx(&a).image_source, LtxImageSource::None));
        a.image = Some(PathBuf::from("f.png"));
        assert!(matches!(ltx(&a).image_source, LtxImageSource::Upload));

        a.audio = Some(PathBuf::from("v.wav"));
        assert!(matches!(ltx(&a).audio_source, LtxAudioSource::VoiceUpload));
        a.bgm = Some(PathBuf::from("b.wav"));
        assert!(matches!(ltx(&a).audio_source, LtxAudioSource::MixedUpload));
        a.audio = None;
        assert!(matches!(ltx(&a).audio_source, LtxAudioSource::BgmUpload));
    }

    #[test]
    fn save_clip_is_on_unless_the_caller_opts_out() {
        assert!(ltx(&args()).save_clip);
        let mut a = args();
        a.no_save = true;
        assert!(!ltx(&a).save_clip);
    }

    #[test]
    fn resume_id_rides_through_to_the_engine() {
        let mut a = args();
        a.resume_job_id = Some("srv-9".into());
        assert_eq!(ltx(&a).resume_job_id.as_deref(), Some("srv-9"));
    }

    #[test]
    fn an_unknown_profile_is_rejected_rather_than_silently_dropped() {
        let mut a = args();
        a.profile = Some("ultra".into());
        assert!(build_generate_request(jobs::LTX_VIDEO, &a, &Config::default()).is_err());
        a.profile = Some("high".into());
        assert!(ltx(&a).profile.is_some());
    }

    #[test]
    fn file_part_names_match_the_route_that_reads_them() {
        let mut a = args();
        a.image = Some(PathBuf::from("f.png"));
        a.audio = Some(PathBuf::from("v.wav"));
        a.bgm = Some(PathBuf::from("b.wav"));
        let names: Vec<&str> = file_parts(&a).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["image", "audio", "bgm"]);
        assert!(file_parts(&args()).is_empty());
    }

    #[test]
    fn config_defaults_fill_in() {
        let cfg = Config::with_defaults(
            r#"[video]
negative = "blurry, watermark"
width = 1280
height = 704
fps = 24
steps = 8
segment_frames = 97
cont_latent_frames = 3
sampler = "euler_a"
scheduler = "simple"
model = "audioreactive"
profile = "high""#,
        );

        let req = build_generate_request(jobs::LTX_VIDEO, &args(), &cfg).unwrap();
        assert_eq!(req.negative_prompt.as_deref(), Some("blurry, watermark"));
        assert_eq!(req.width, Some(1280));
        assert_eq!(req.fps, Some(24));
        assert_eq!(req.steps, Some(8));
        assert_eq!(req.segment_frames, Some(97));
        assert_eq!(req.cont_latent_frames, Some(3));
        assert_eq!(req.sampling_method.as_deref(), Some("euler_a"));
        assert_eq!(req.scheduler.as_deref(), Some("simple"));
        assert_eq!(req.model.as_deref(), Some("audioreactive"));
        assert_eq!(req.profile, Some(LtxQualityProfile::High));
    }

    /// An explicit flag still beats a config default.
    ///
    /// ⚠️ Deliberately NOT `steps`, which this used until LTX-2.3 was retired: the surviving
    /// checkpoint is distilled, so a step count is refused up front now — see
    /// `the_distilled_knobs_are_refused_on_ltx` below.
    #[test]
    fn flags_win_over_config_defaults() {
        let cfg = Config::with_defaults("[video]\nwidth = 1280\nfps = 48");
        let mut a = args();
        a.width = Some(768);
        a.fps = Some(24);
        let req = build_generate_request(jobs::LTX_VIDEO, &a, &cfg).unwrap();
        assert_eq!(req.width, Some(768));
        assert_eq!(req.fps, Some(24));
    }

    /// 🔴 A flag whose effect is nothing is worse than a missing flag: the render succeeds and
    /// quietly ignores what was asked for. LTX-2.5 is distilled — it takes the sampling knobs on
    /// the wire because every LTX request carries them, and honours none of them — and it ships
    /// one DiT, so a named variant is a 501 after the GPU wait.
    #[test]
    fn the_distilled_knobs_are_refused_on_ltx() {
        for (name, set) in [
            (
                "--model",
                (|a: &mut GenerateArgs| a.model = Some("echo-e50".into())) as fn(&mut GenerateArgs),
            ),
            ("--steps", |a: &mut GenerateArgs| a.steps = Some(30)),
            ("--cfg", |a: &mut GenerateArgs| a.cfg = Some(3.0)),
            ("--sampler", |a: &mut GenerateArgs| {
                a.sampler = Some("euler".into())
            }),
            ("--scheduler", |a: &mut GenerateArgs| {
                a.scheduler = Some("simple".into())
            }),
        ] {
            let mut a = args();
            set(&mut a);
            assert_eq!(unsupported_flags(&a), vec![name]);
            let e = build_generate_request(jobs::LTX_VIDEO, &a, &Config::default())
                .unwrap_err()
                .to_string();
            assert!(e.contains(name), "{e}");
        }
    }

    /// The other half, and the reason the two lists differ from the Director's: the engine
    /// declines its upscaler for a CONDITIONED shot, and a bare CLI render is the case where it
    /// still works. `apply_engine_delivery_limits` runs on the Director routes, not this one.
    #[test]
    fn the_upscale_ladder_still_travels_from_the_cli() {
        let mut a = args();
        a.profile = Some("high".into());
        a.upscaler_ratio = Some("x2".into());
        assert!(unsupported_flags(&a).is_empty());
    }

    #[test]
    fn props_payload_decodes() {
        const LTX: &str = r#"{"healthy":true,"default_segment_frames":97,
            "default_cont_latent_frames":2,"default_steps":8,"default_fps":48,
            "default_resolution":"1280x704","variants":["base"],"loras":[]}"#;
        let ltx: LtxVideoProps = serde_json::from_str(LTX).unwrap();
        assert_eq!(ltx.default_segment_frames, 97);
        assert_eq!(ltx.variants, vec!["base"]);
    }

    #[test]
    fn relip_defaults_to_single_stage_and_saves() {
        let a = RelipArgs {
            video: PathBuf::from("v.mp4"),
            audio: PathBuf::from("a.wav"),
            prompt: None,
            a2v_guidance: None,
            two_stage: false,
            width: None,
            height: None,
            fps: None,
            seed: None,
            lipdub_strength: None,
            no_save: false,
            detach: false,
            poll_secs: 2,
            output: None,
        };
        let req = build_relip_request(&a);
        assert!(!req.two_stage);
        assert!(req.save_clip);

        let two = RelipArgs {
            two_stage: true,
            no_save: true,
            ..a
        };
        let req = build_relip_request(&two);
        assert!(req.two_stage);
        assert!(!req.save_clip);
    }
}
