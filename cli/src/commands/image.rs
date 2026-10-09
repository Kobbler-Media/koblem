use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use koblem_client::types::{
    GeneratedImage, ImageGenerateRequest, ImageGenerateResponse, ImageProps, Krea2GenerateRequest,
    Krea2GenerateResponse, Krea2SuggestPromptRequest, SuggestPromptRequest, SuggestPromptResponse,
};
use reqwest::multipart;
use serde::Serialize;

use crate::client::file_part;
use crate::commands::bg::BgArgs;
use crate::commands::refine::RefineArgs;
use crate::{Ctx, download, output};

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ImageCmd {
    /// Render an image from a prompt (FLUX.2 or Krea 2 Turbo).
    Generate(GenerateArgs),
    /// Ask the VL model to write a good prompt from a brief.
    Suggest(SuggestArgs),
    /// Print the image engine's capabilities/limits.
    Props(EngineArgs),
    /// Kill the in-flight render (frees the GPU).
    Cancel(EngineArgs),
}

/// FLUX.2 (`images`) is the default engine; `krea2` is the second one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum ImageEngine {
    #[default]
    Flux2,
    Krea2,
}

impl ImageEngine {
    fn base(self) -> &'static str {
        match self {
            ImageEngine::Flux2 => "/api/v1/images",
            ImageEngine::Krea2 => "/api/v1/krea2",
        }
    }
    /// Config section for per-engine defaults.
    fn cfg_key(self) -> &'static str {
        match self {
            ImageEngine::Flux2 => "image",
            ImageEngine::Krea2 => "krea2",
        }
    }
}

#[derive(Args, Debug)]
pub struct EngineArgs {
    #[arg(long, value_enum, default_value_t = ImageEngine::Flux2)]
    pub engine: ImageEngine,
}

#[derive(Args, Debug)]
pub struct GenerateArgs {
    /// Text prompt.
    pub prompt: String,
    #[arg(long, value_enum, default_value_t = ImageEngine::Flux2)]
    pub engine: ImageEngine,
    #[arg(long)]
    pub negative: Option<String>,
    /// FLUX.2: "auto" (default) | "base" | "edit". Krea2: "auto" | "t2i" | "edit".
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub width: Option<u32>,
    #[arg(long)]
    pub height: Option<u32>,
    #[arg(long)]
    pub steps: Option<u32>,
    #[arg(long)]
    pub cfg: Option<f32>,
    #[arg(long)]
    pub sampler: Option<String>,
    #[arg(long)]
    pub scheduler: Option<String>,
    #[arg(long)]
    pub seed: Option<i64>,
    #[arg(long)]
    pub batch: Option<u32>,
    /// FLUX.2 only: img2img denoise strength.
    #[arg(long)]
    pub strength: Option<f32>,
    #[arg(long)]
    pub vae_tiling: bool,
    /// Krea2 edit mode: longest side the reference is handed to the VL model at.
    /// Lower = stronger edit adherence, higher = stronger identity.
    #[arg(long)]
    pub grounding_px: Option<u32>,
    /// FLUX.2 only: init image for img2img / edit.
    #[arg(long, value_name = "PATH")]
    pub init: Option<PathBuf>,
    /// FLUX.2 only: inpaint mask.
    #[arg(long, value_name = "PATH")]
    pub mask: Option<PathBuf>,
    /// Reference image(s) for multi-ref edit. Repeatable. Krea2 takes at most 2,
    /// and the order matters: scene first, person second.
    #[arg(long = "ref", value_name = "PATH")]
    pub references: Vec<PathBuf>,
    /// Chain background removal onto each rendered image.
    #[arg(long)]
    pub rmbg: bool,
    #[command(flatten)]
    pub bg: BgArgs,
    /// Output file (default: image.png; batches get -1/-2 suffixes).
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct SuggestArgs {
    pub brief: String,
    #[arg(long, value_enum, default_value_t = ImageEngine::Flux2)]
    pub engine: ImageEngine,
    /// FLUX.2 only: reference image(s) the VL model reads.
    #[arg(long = "ref", value_name = "PATH")]
    pub references: Vec<PathBuf>,
    #[command(flatten)]
    pub refine: RefineArgs,
}

pub async fn run(ctx: &Ctx, cmd: ImageCmd) -> Result<()> {
    match cmd {
        ImageCmd::Generate(args) => generate(ctx, args).await,
        ImageCmd::Suggest(args) => suggest(ctx, args).await,
        ImageCmd::Props(a) => props(ctx, a.engine).await,
        ImageCmd::Cancel(a) => cancel(ctx, a.engine).await,
    }
}

/// Common shape of both engines' responses, so one writer/printer serves both.
struct Rendered {
    images: Vec<GeneratedImage>,
    summary: String,
    bg_error: Option<String>,
}

pub fn build_flux_request(args: &GenerateArgs, ctx: &Ctx) -> ImageGenerateRequest {
    let cfg = &ctx.config;
    let k = ImageEngine::Flux2.cfg_key();
    ImageGenerateRequest {
        prompt: args.prompt.clone(),
        negative_prompt: args.negative.clone().or_else(|| cfg.def_str(k, "negative")),
        model: args
            .model
            .clone()
            .or_else(|| cfg.def_str(k, "model"))
            .unwrap_or_else(|| "auto".to_string()),
        width: args.width.or_else(|| cfg.def_u32(k, "width")),
        height: args.height.or_else(|| cfg.def_u32(k, "height")),
        steps: args.steps.or_else(|| cfg.def_u32(k, "steps")),
        cfg: args.cfg.or_else(|| cfg.def_f32(k, "cfg")),
        sampler: args.sampler.clone().or_else(|| cfg.def_str(k, "sampler")),
        scheduler: args
            .scheduler
            .clone()
            .or_else(|| cfg.def_str(k, "scheduler")),
        seed: args.seed,
        batch: args.batch.or_else(|| cfg.def_u32(k, "batch")),
        strength: args.strength,
        vae_tiling: if args.vae_tiling {
            Some(true)
        } else {
            cfg.def_bool(k, "vae_tiling")
        },
        // File inputs ride as multipart parts, never inline base64.
        init_image: None,
        mask: None,
        reference_images: None,
        remove_background: args.rmbg.then(|| args.bg.resolve(cfg, true)),
    }
}

pub fn build_krea2_request(args: &GenerateArgs, ctx: &Ctx) -> Krea2GenerateRequest {
    let cfg = &ctx.config;
    let k = ImageEngine::Krea2.cfg_key();
    Krea2GenerateRequest {
        prompt: args.prompt.clone(),
        mode: args.model.clone().or_else(|| cfg.def_str(k, "mode")),
        reference_images: None,
        grounding_px: args.grounding_px.or_else(|| cfg.def_u32(k, "grounding_px")),
        negative_prompt: args.negative.clone().or_else(|| cfg.def_str(k, "negative")),
        width: args.width.or_else(|| cfg.def_u32(k, "width")),
        height: args.height.or_else(|| cfg.def_u32(k, "height")),
        steps: args.steps.or_else(|| cfg.def_u32(k, "steps")),
        cfg: args.cfg.or_else(|| cfg.def_f32(k, "cfg")),
        sampler: args.sampler.clone().or_else(|| cfg.def_str(k, "sampler")),
        scheduler: args
            .scheduler
            .clone()
            .or_else(|| cfg.def_str(k, "scheduler")),
        seed: args.seed,
        batch: args.batch.or_else(|| cfg.def_u32(k, "batch")),
        // Unset must stay None: the engine ships with tiling ON and sending
        // `false` would turn it off, breaking 2048² VAE decodes.
        vae_tiling: args.vae_tiling.then_some(true),
        remove_background: args.rmbg.then(|| args.bg.resolve(cfg, true)),
    }
}

async fn post_render<T: Serialize>(
    ctx: &Ctx,
    path: &str,
    req: &T,
    args: &GenerateArgs,
) -> Result<reqwest::Response> {
    let has_files = args.init.is_some() || args.mask.is_some() || !args.references.is_empty();
    if !has_files {
        return ctx.client.post_json(path, &[], req).await;
    }
    let mut form = multipart::Form::new().text("request", serde_json::to_string(req)?);
    if let Some(p) = &args.init {
        form = form.part("init_image", file_part(p).await?);
    }
    if let Some(p) = &args.mask {
        form = form.part("mask", file_part(p).await?);
    }
    for p in &args.references {
        form = form.part("reference_images", file_part(p).await?);
    }
    ctx.client.post_multipart(path, &[], form).await
}

async fn generate(ctx: &Ctx, args: GenerateArgs) -> Result<()> {
    if args.engine == ImageEngine::Krea2 && (args.init.is_some() || args.mask.is_some()) {
        bail!("krea2 has no img2img/inpaint path — use --ref for its edit mode");
    }
    let path = format!("{}/generate", args.engine.base());

    let rendered = match args.engine {
        ImageEngine::Flux2 => {
            let req = build_flux_request(&args, ctx);
            let resp = post_render(ctx, &path, &req, &args).await?;
            let body: ImageGenerateResponse = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            Rendered {
                summary: format!(
                    "model={} {}x{} steps={} cfg={} render={}ms",
                    body.model_used, body.width, body.height, body.steps, body.cfg, body.render_ms
                ),
                bg_error: body.background_removal_error.clone(),
                images: body.images,
            }
        }
        ImageEngine::Krea2 => {
            let req = build_krea2_request(&args, ctx);
            let resp = post_render(ctx, &path, &req, &args).await?;
            let body: Krea2GenerateResponse = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            Rendered {
                summary: format!(
                    "mode={} {}x{} steps={} cfg={} render={}ms",
                    body.mode_used, body.width, body.height, body.steps, body.cfg, body.render_ms
                ),
                bg_error: body.background_removal_error.clone(),
                images: body.images,
            }
        }
    };

    // The server serves both files from the gallery, so they stream to disk with
    // progress and resume rather than arriving as base64 in the response body.
    let target = output::single_target("image", "png", &args.output);
    for (i, img) in rendered.images.iter().enumerate() {
        let path = if rendered.images.len() == 1 {
            target.clone()
        } else {
            output::indexed(&target, i + 1)
        };
        download::to_named(ctx, &img.url, &path).await?;
        if let Some(cut) = &img.cutout_url {
            download::to_named(ctx, cut, &output::sibling(&path, "cutout")).await?;
        }
    }
    if let Some(err) = &rendered.bg_error {
        eprintln!("background removal failed: {err}");
    }
    eprintln!("{}", rendered.summary);
    Ok(())
}

async fn suggest(ctx: &Ctx, args: SuggestArgs) -> Result<()> {
    let (refine_source, refine_instruction) = args.refine.resolve().await?;
    let path = format!("{}/suggest-prompt", args.engine.base());
    let resp = match args.engine {
        ImageEngine::Flux2 => {
            let mut reference_images = Vec::new();
            for p in &args.references {
                reference_images.push(output::read_base64(p).await?);
            }
            let req = SuggestPromptRequest {
                brief: args.brief,
                reference_images: (!reference_images.is_empty()).then_some(reference_images),
                refine_source,
                refine_instruction,
            };
            ctx.client.post_json(&path, &[], &req).await?
        }
        ImageEngine::Krea2 => {
            if !args.references.is_empty() {
                eprintln!("note: krea2's prompt assist has no vision path; --ref ignored");
            }
            let req = Krea2SuggestPromptRequest {
                brief: args.brief,
                refine_source,
                refine_instruction,
            };
            ctx.client.post_json(&path, &[], &req).await?
        }
    };
    let body: SuggestPromptResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.prompt);
    if let Some(neg) = &body.negative_prompt {
        eprintln!("negative: {neg}");
    }
    Ok(())
}

async fn props(ctx: &Ctx, engine: ImageEngine) -> Result<()> {
    let resp = ctx
        .client
        .get(&format!("{}/props", engine.base()), &[])
        .await?;
    let body: ImageProps = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("healthy: {}", body.healthy);
    if let Some(c) = body.configured {
        println!("configured: {c}");
    }
    if !body.samplers.is_empty() {
        println!("samplers: {}", body.samplers.join(", "));
    }
    if !body.schedulers.is_empty() {
        println!("schedulers: {}", body.schedulers.join(", "));
    }
    if !body.supported_modes.is_empty() {
        println!("modes: {}", body.supported_modes.join(", "));
    }
    if let (Some(w), Some(h)) = (body.default_width, body.default_height) {
        println!("default size: {w}x{h}");
    }
    if let (Some(lo), Some(hi)) = (body.min_dim, body.max_dim) {
        println!("dim range: {lo}..{hi}");
    }
    if let Some(a) = body.edit_available {
        println!("edit available: {a}");
    }
    if let Some(n) = body.max_reference_images {
        println!("max reference images: {n}");
    }
    if !body.variants.is_null() {
        println!("variants: {}", body.variants);
    }
    if !body.limits.is_null() {
        println!("limits: {}", body.limits);
    }
    Ok(())
}

async fn cancel(ctx: &Ctx, engine: ImageEngine) -> Result<()> {
    let resp = ctx
        .client
        .post_json(&format!("{}/cancel", engine.base()), &[], &())
        .await?;
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if ctx.json {
        return output::print_json(&body);
    }
    println!("cancel requested");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::config::Config;
    use std::time::Duration;

    fn ctx() -> Ctx {
        Ctx {
            client: Client::new(
                "http://x".into(),
                None,
                Default::default(),
                false,
                Duration::from_secs(1),
            )
            .unwrap(),
            config: Config::default(),
            json: false,
        }
    }

    fn args() -> GenerateArgs {
        GenerateArgs {
            prompt: "a cat".into(),
            engine: ImageEngine::Flux2,
            negative: None,
            model: None,
            width: None,
            height: None,
            steps: None,
            cfg: None,
            sampler: None,
            scheduler: None,
            seed: None,
            batch: None,
            strength: None,
            vae_tiling: false,
            grounding_px: None,
            init: None,
            mask: None,
            references: Vec::new(),
            rmbg: false,
            bg: BgArgs::default(),
            output: None,
        }
    }

    #[test]
    fn chained_rmbg_carries_all_ten_matting_knobs() {
        let mut a = args();
        a.rmbg = true;
        a.bg = BgArgs {
            backend: Some("birefnet".into()),
            process_res: Some(2048),
            sensitivity: Some(0.4),
            mask_blur: Some(3),
            mask_offset: Some(-1),
            refine: true,
            invert: true,
            bg_mode: Some("color".into()),
            bg_color: Some("#00ff00".into()),
        };
        let bg = build_flux_request(&a, &ctx()).remove_background.unwrap();
        assert!(bg.enabled);
        assert_eq!(bg.backend.as_deref(), Some("birefnet"));
        assert_eq!(bg.process_res, Some(2048));
        assert_eq!(bg.sensitivity, Some(0.4));
        assert_eq!(bg.mask_blur, Some(3));
        assert_eq!(bg.mask_offset, Some(-1));
        assert_eq!(bg.refine, Some(true));
        assert_eq!(bg.invert, Some(true));
        assert_eq!(bg.bg_mode.as_deref(), Some("color"));
        assert_eq!(bg.bg_color.as_deref(), Some("#00ff00"));

        // The same knobs must reach krea2's chained step.
        let bg2 = build_krea2_request(&a, &ctx()).remove_background.unwrap();
        assert_eq!(bg2.process_res, Some(2048));
        assert_eq!(bg2.mask_offset, Some(-1));
        assert_eq!(bg2.invert, Some(true));
    }

    #[test]
    fn no_rmbg_flag_means_no_background_removal_block() {
        assert!(
            build_flux_request(&args(), &ctx())
                .remove_background
                .is_none()
        );
        assert!(
            build_krea2_request(&args(), &ctx())
                .remove_background
                .is_none()
        );
    }

    #[test]
    fn krea2_maps_model_onto_mode_and_keeps_grounding_px() {
        let mut a = args();
        a.engine = ImageEngine::Krea2;
        a.model = Some("edit".into());
        a.grounding_px = Some(640);
        let req = build_krea2_request(&a, &ctx());
        assert_eq!(req.mode.as_deref(), Some("edit"));
        assert_eq!(req.grounding_px, Some(640));
        // Unset tiling must stay None so the engine keeps its own default ON.
        assert_eq!(req.vae_tiling, None);
    }

    #[test]
    fn engine_bases_match_the_server_routes() {
        assert_eq!(ImageEngine::Flux2.base(), "/api/v1/images");
        assert_eq!(ImageEngine::Krea2.base(), "/api/v1/krea2");
    }

    #[test]
    fn props_parses_both_engines_payloads() {
        let flux: ImageProps = serde_json::from_str(
            r#"{"healthy":true,"samplers":["euler"],"schedulers":["simple"],"limits":{},
                "supported_modes":["base","edit"],"variants":{},"default_width":512,
                "default_height":512,"max_dim":2048,"min_dim":16}"#,
        )
        .unwrap();
        assert!(flux.healthy);
        assert_eq!(flux.supported_modes, vec!["base", "edit"]);
        assert_eq!(flux.configured, None);

        let krea: ImageProps = serde_json::from_str(
            r#"{"healthy":false,"configured":false,"samplers":[],"schedulers":[],"limits":{},
                "defaults":{},"variants":{},"edit_available":false,"max_reference_images":2}"#,
        )
        .unwrap();
        assert_eq!(krea.configured, Some(false));
        assert_eq!(krea.edit_available, Some(false));
        assert_eq!(krea.max_reference_images, Some(2));
        assert!(krea.supported_modes.is_empty());
    }
}
