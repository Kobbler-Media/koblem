//! `kob rig` — image → rigged 3D asset (SPAR3D + RigAnything).
//!
//! The two newest pipelines were browser-only, so none of the
//! CLI's transport properties — streamed uploads, resumable downloads — reached them. The bundle
//! is the artifact where resume actually earns its keep: it is the largest single download the
//! gallery serves, and it is exactly what someone scripting asset generation wants.
//!
//! The image goes up as a multipart part (streamed off disk, `client::file_part`), and every
//! output comes back by URL — no base64 anywhere on this path.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use koblem_client::types::{
    ImageToRigProps, ImageToRigRequest, ImageToRigResponse, RigSemanticPart, RigTextureMode,
};
use reqwest::multipart;

use crate::client::file_part;
use crate::{Ctx, download, output};

#[derive(Subcommand, Debug)]
pub enum RigCmd {
    /// Turn an image into a textured, optionally skinned GLB.
    Generate(GenerateArgs),
    /// Service health + what this deployment supports.
    Props,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum TextureMode {
    /// The model's own generated texture.
    Generated,
    /// Project the source image onto the visible front (lossless where it is visible).
    ProjectFront,
    /// Project the front and mirror it onto the back.
    TwoSidedMirror,
}

impl From<TextureMode> for RigTextureMode {
    fn from(mode: TextureMode) -> Self {
        match mode {
            TextureMode::Generated => RigTextureMode::Generated,
            TextureMode::ProjectFront => RigTextureMode::ProjectFront,
            TextureMode::TwoSidedMirror => RigTextureMode::TwoSidedMirror,
        }
    }
}

#[derive(Args, Debug)]
pub struct GenerateArgs {
    /// Source image. A cutout works best; `--no-remove-background` if it already is one.
    pub image: PathBuf,
    /// Where to write the GLB (default: `<image stem>.glb`).
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Also download the asset bundle (zip: model, LODs, sources, manifest, README).
    #[arg(long)]
    pub bundle: bool,
    /// Also download the LOD sidecars as separate GLBs.
    #[arg(long)]
    pub lods: bool,
    #[arg(long, value_enum, default_value = "project-front")]
    pub texture_mode: TextureMode,
    /// 512 or 1024. Applies to the projection modes only.
    #[arg(long, value_name = "PX")]
    pub source_texture_resolution: Option<u32>,
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,
    /// Emit glTF skin data (a skeleton). Off by default, as on the API.
    #[arg(long)]
    pub rig: bool,
    /// Skip the RMBG-2.0 cutout — pass this only when the image already has an alpha channel.
    #[arg(long)]
    pub no_remove_background: bool,
    /// Skip LOD generation server-side.
    #[arg(long)]
    pub no_lods: bool,
    /// Character parts to name in the rig, as `name:motion:count` (e.g. `arm:swing:2`).
    /// Repeatable. Requires `--rig`.
    #[arg(long = "part", value_name = "NAME:MOTION:COUNT")]
    pub parts: Vec<String>,
}

fn parse_part(spec: &str) -> Result<RigSemanticPart> {
    let bits: Vec<&str> = spec.split(':').collect();
    let [name, motion, count] = bits.as_slice() else {
        bail!("--part wants NAME:MOTION:COUNT (e.g. arm:swing:2), got `{spec}`");
    };
    let count: u32 = count
        .parse()
        .map_err(|_| anyhow::anyhow!("--part count must be 1-8, got `{count}`"))?;
    if name.trim().is_empty() || motion.trim().is_empty() || !(1..=8).contains(&count) {
        bail!("--part wants a non-empty name and motion and a count of 1-8, got `{spec}`");
    }
    Ok(RigSemanticPart {
        name: name.trim().to_string(),
        motion: motion.trim().to_string(),
        count,
    })
}

fn glb_name(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("model");
    PathBuf::from(format!("{stem}.glb"))
}

pub async fn run(ctx: &Ctx, cmd: RigCmd) -> Result<()> {
    match cmd {
        RigCmd::Props => props(ctx).await,
        RigCmd::Generate(args) => generate(ctx, args).await,
    }
}

async fn props(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/api/v1/3d/props", &[]).await?;
    let props: ImageToRigProps = resp.json().await?;
    if ctx.json {
        return output::print_json(&props);
    }
    println!(
        "configured={} healthy={} project_front={} two_sided_mirror={} texture_res={:?}",
        props.configured,
        props.healthy,
        props.supports_project_front,
        props.supports_two_sided_mirror,
        props.source_texture_resolutions,
    );
    Ok(())
}

async fn generate(ctx: &Ctx, args: GenerateArgs) -> Result<()> {
    let semantic_parts = if args.parts.is_empty() {
        None
    } else {
        Some(
            args.parts
                .iter()
                .map(|s| parse_part(s))
                .collect::<Result<Vec<_>>>()?,
        )
    };
    if semantic_parts.is_some() && !args.rig {
        bail!("--part requires --rig (parts name joints in a skeleton)");
    }

    let req = ImageToRigRequest {
        seed: args.seed,
        texture_mode: args.texture_mode.into(),
        source_texture_resolution: args.source_texture_resolution,
        rig: args.rig,
        remove_background: !args.no_remove_background,
        generate_lods: !args.no_lods,
        semantic_parts,
    };
    // `request` as JSON text, the picture as a streamed binary part — the shape every other media
    // command here uses, and the one the whole media-transport plan is about.
    let form = multipart::Form::new()
        .text("request", serde_json::to_string(&req)?)
        .part("image", file_part(&args.image).await?);

    let resp = ctx
        .client
        .post_multipart("/api/v1/3d/generate", &[], form)
        .await?;
    let body: ImageToRigResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }

    let target = args.output.clone().unwrap_or_else(|| glb_name(&args.image));
    download::to_named(ctx, &body.url, &target).await?;

    if args.lods {
        for (i, url) in body.lod_urls.iter().enumerate() {
            let stem = target
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("model");
            let lod = target.with_file_name(format!("{stem}.lod{}.glb", i + 1));
            download::to_named(ctx, url, &lod).await?;
        }
    }
    if args.bundle {
        let stem = target
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("model");
        let zip = target.with_file_name(format!("{stem}-bundle.zip"));
        // Built on demand from the persisted outputs (§5a-b), and resumable like any other
        // download here — which is the reason this command exists.
        download::to_named(ctx, &body.bundle_url, &zip).await?;
    }

    eprintln!(
        "generation={} texture_mode={:?}{}{}",
        body.generation_id,
        body.texture_mode,
        body.matting_elapsed_seconds
            .map(|s| format!(" matting={s:.2}s"))
            .unwrap_or_default(),
        body.worker_wall_seconds
            .map(|s| format!(" worker={s:.2}s"))
            .unwrap_or_default(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec is positional and easy to get wrong, and a silently-misparsed part changes the
    /// rig rather than failing — so every rejection path is worth pinning.
    #[test]
    fn part_specs_parse_or_say_why() {
        let ok = parse_part("arm:swing:2").expect("valid");
        assert_eq!(ok.name, "arm");
        assert_eq!(ok.motion, "swing");
        assert_eq!(ok.count, 2);
        assert!(parse_part("arm:swing").is_err());
        assert!(parse_part("arm:swing:0").is_err());
        assert!(parse_part("arm:swing:9").is_err());
        assert!(parse_part(":swing:2").is_err());
        assert!(parse_part("arm:swing:many").is_err());
    }

    #[test]
    fn the_default_output_name_follows_the_input() {
        assert_eq!(
            glb_name(Path::new("/tmp/hero.png")),
            PathBuf::from("hero.glb")
        );
    }
}
