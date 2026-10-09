use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Args;
use koblem_client::types::MattingRemoveResponse;
use reqwest::multipart;

use crate::client::file_part;
use crate::commands::bg::{self, BgArgs};
use crate::{Ctx, download, output};

#[derive(Args, Debug)]
pub struct RmbgArgs {
    /// Image file(s) to cut out. One uses a raw upload; many batch together.
    #[arg(required = true)]
    pub images: Vec<PathBuf>,
    #[command(flatten)]
    pub bg: BgArgs,
    /// Return the raw mask instead of the cutout.
    #[arg(long)]
    pub mask_only: bool,
    /// Output file (single input only; default: <name>-cutout.png).
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

fn cutout_name(input: &Path) -> PathBuf {
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    PathBuf::from(format!("{stem}-cutout.png"))
}

pub async fn run(ctx: &Ctx, args: RmbgArgs) -> Result<()> {
    let mut query = bg::to_query(&args.bg.resolve(&ctx.config, true));
    if args.mask_only {
        query.push(("mask_only", "true".to_string()));
    }

    let resp = if args.images.len() == 1 {
        let path = &args.images[0];
        let bytes = tokio::fs::read(path).await?;
        let ct = output::mime_for_path(path);
        ctx.client
            .post_raw("/api/v1/matting/remove", &query, &ct, bytes)
            .await?
    } else {
        let mut form = multipart::Form::new();
        for p in &args.images {
            form = form.part("images", file_part(p).await?);
        }
        ctx.client
            .post_multipart("/api/v1/matting/remove", &query, form)
            .await?
    };

    let body: MattingRemoveResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }

    if args.images.len() == 1 {
        let target = args
            .output
            .clone()
            .unwrap_or_else(|| cutout_name(&args.images[0]));
        let Some(r) = body.results.first() else {
            bail!("server returned no results");
        };
        download::to_named(ctx, &r.url, &target).await?;
    } else {
        if args.output.is_some() {
            eprintln!("note: -o ignored for multi-image batches; using <name>-cutout.png");
        }
        for (input, r) in args.images.iter().zip(&body.results) {
            download::to_named(ctx, &r.url, &cutout_name(input)).await?;
        }
    }
    if let Some(b) = &body.backend {
        eprintln!(
            "backend={b}{}",
            body.elapsed_seconds
                .map(|s| format!(" {s:.2}s"))
                .unwrap_or_default()
        );
    }
    Ok(())
}
