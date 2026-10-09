//! `kob h3` — MiniMax-H3, the joint audio+video engine.
//!
//! H3 was browser-only, so nothing scripted could reach the one
//! engine that generates its own soundtrack. It is not a peer of `kob video generate`, and this
//! command does not pretend otherwise: H3 renders a DIRECTOR PROJECT, because its unit is a
//! GENERATION holding several shots and the packing lives server-side. So this drives a project
//! and then behaves exactly like the video commands — poll the job, download the clip.
//!
//! H3's renders land in the SAME job store as LTX's (`ltx_jobs`), which is why the job and media
//! paths below are the ltx-video ones. That is the server's arrangement, not a shortcut here.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use koblem_client::types::VideoJobSubmitResponse;
use serde_json::Value;

use crate::jobs::{self, LTX_VIDEO, PollOpts};
use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
pub enum H3Cmd {
    /// Capabilities: health, canvas limits, and what a given geometry would render at.
    Props(PropsArgs),
    /// Render a Director project on H3 and wait for the clip.
    Render(RenderArgs),
    /// Cancel the in-flight H3 render.
    Cancel,
}

#[derive(Args, Debug)]
pub struct PropsArgs {
    /// Ask what THIS geometry would actually render at (frames snap up to H3's grid).
    #[arg(long, value_name = "PX")]
    pub width: Option<u32>,
    #[arg(long, value_name = "PX")]
    pub height: Option<u32>,
    #[arg(long, value_name = "N")]
    pub frames: Option<u32>,
}

#[derive(Args, Debug)]
pub struct RenderArgs {
    /// Director project id to render.
    pub project: String,
    /// `final` (default) or `proxy`.
    #[arg(long, default_value = "final")]
    pub tier: String,
    /// Where to write the finished mp4 (default: `<project>.mp4`). H3 encodes x265, which webm
    /// cannot carry — the container is mp4 and not a choice.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Submit and print the job id without waiting.
    #[arg(long)]
    pub no_wait: bool,
}

pub async fn run(ctx: &Ctx, cmd: H3Cmd) -> Result<()> {
    match cmd {
        H3Cmd::Props(args) => props(ctx, args).await,
        H3Cmd::Render(args) => render(ctx, args).await,
        H3Cmd::Cancel => cancel(ctx).await,
    }
}

async fn props(ctx: &Ctx, args: PropsArgs) -> Result<()> {
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(w) = args.width {
        query.push(("width", w.to_string()));
    }
    if let Some(h) = args.height {
        query.push(("height", h.to_string()));
    }
    if let Some(f) = args.frames {
        query.push(("frames", f.to_string()));
    }
    let resp = ctx.client.get("/api/v1/minimax-h3/props", &query).await?;
    let body: Value = resp.json().await?;
    output::print_json(&body)
}

async fn cancel(ctx: &Ctx) -> Result<()> {
    let resp = ctx
        .client
        .post_json("/api/v1/minimax-h3/cancel", &[], &Value::Null)
        .await?;
    let body: Value = resp.json().await?;
    output::print_json(&body)
}

async fn render(ctx: &Ctx, args: RenderArgs) -> Result<()> {
    if !matches!(args.tier.as_str(), "final" | "proxy") {
        bail!("--tier must be `final` or `proxy`, got `{}`", args.tier);
    }
    let path = format!("/api/v1/ltx-director/projects/{}/run", args.project);
    let resp = ctx
        .client
        .post_json(&path, &[("tier", args.tier.clone())], &Value::Null)
        .await?;
    let submitted: VideoJobSubmitResponse = resp.json().await?;
    if args.no_wait {
        return output::print_json(&submitted);
    }
    eprintln!("job {} submitted", submitted.job_id);
    // H3 shares LTX's job store and its poll/media routes, so the whole wait-and-download dance
    // is the shared one — including the resumable `.part` download.
    let opts = PollOpts::default();
    let status = jobs::wait(ctx, LTX_VIDEO, &submitted.job_id, &opts, &args.output).await?;
    if ctx.json {
        return output::print_json(&status);
    }
    jobs::download_media(ctx, LTX_VIDEO, &submitted.job_id, &args.output).await?;
    eprintln!(
        "generations={} gate={}ms render={}ms",
        status.segments, status.gate_wait_ms, status.render_ms
    );
    Ok(())
}
