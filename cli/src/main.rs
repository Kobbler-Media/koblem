mod client;
mod commands;
mod config;
mod download;
mod jobs;
mod output;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use crate::client::{Client, QueueFlags};
use crate::config::{Config, ConfigCmd};

/// CLI for koblem — generate images and video, remove backgrounds, synthesize
/// speech, transcribe audio, compose music and sound effects, render avatars,
/// browse the gallery, and chat with the LLM.
#[derive(Parser, Debug)]
#[command(name = "kob", version, about, long_about = None)]
struct Cli {
    /// koblem base URL (overrides config + KOBLEM_URL).
    #[arg(long, global = true, env = "KOBLEM_URL")]
    url: Option<String>,
    /// API token (overrides config + KOBLEM_TOKEN).
    #[arg(long, global = true, env = "KOBLEM_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Print the raw JSON response instead of writing output files.
    #[arg(long, global = true)]
    json: bool,
    /// Don't wait out a 503 "GPU busy" — fail immediately instead of retrying.
    #[arg(long, global = true)]
    no_wait: bool,
    /// Max seconds to spend retrying 503 "GPU busy" before giving up.
    #[arg(long, global = true, default_value_t = 600)]
    max_wait: u64,
    /// Queue this render instead of running it now. The default for an API key is
    /// `now`, which is the opposite of what a shell loop wants.
    #[arg(long, global = true, conflicts_with = "now")]
    queue: bool,
    /// Jump the queue's ordering. Still waits for the running job — it does not preempt.
    #[arg(long, global = true)]
    now: bool,
    /// Band: 0 interactive, 1 normal, 2 background.
    #[arg(long, global = true, value_parser = clap::value_parser!(i16).range(0..=2))]
    band: Option<i16>,
    /// Pin this render to one GPU by UUID.
    #[arg(long, global = true)]
    gpu: Option<String>,
    /// With --gpu, evict whatever is on that card even if the rules say otherwise.
    #[arg(long, global = true, requires = "gpu")]
    force: bool,
    /// Do not start until this job id has finished (e.g. render, then upscale).
    #[arg(long, global = true, value_name = "JOB_ID")]
    after: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Manage CLI configuration (URL, token, per-command defaults).
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Ask the LLM a question.
    Chat(commands::chat::ChatArgs),
    /// Generate / suggest / inspect images (FLUX.2, Krea 2 Turbo).
    Image {
        #[command(subcommand)]
        cmd: commands::image::ImageCmd,
    },
    /// Remove image backgrounds (matting).
    Rmbg(commands::rmbg::RmbgArgs),
    /// Synthesize speech, or manage cloned voices.
    Tts(commands::tts::TtsArgs),
    /// Transcribe audio (STT).
    Stt(commands::stt::SttArgs),
    /// Compose / transform / understand music (ACE-Step).
    Music {
        #[command(subcommand)]
        cmd: commands::music::MusicCmd,
    },
    /// Compose / clone / continue / separate songs (songgen, LeVo2).
    Song {
        #[command(subcommand)]
        cmd: commands::song::SongCmd,
    },
    /// Generate sound effects and short audio (Stable Audio 3).
    Sfx {
        #[command(subcommand)]
        cmd: commands::sfx::SfxCmd,
    },
    /// Render video clips (LTX-2.5 / LTX-2B / Wan-VACE).
    Video {
        #[command(subcommand)]
        cmd: commands::video::VideoCmd,
    },
    /// Render a talking-head avatar clip, or manage saved portraits.
    Avatar(commands::avatar::AvatarArgs),
    /// Render a Director project on MiniMax-H3 (joint audio+video).
    H3 {
        #[command(subcommand)]
        cmd: commands::h3::H3Cmd,
    },
    /// Turn an image into a rigged 3D asset (SPAR3D + RigAnything).
    Rig {
        #[command(subcommand)]
        cmd: commands::rig::RigCmd,
    },
    /// Score images against text phrases (vision classify).
    Vision {
        #[command(subcommand)]
        cmd: commands::vision::VisionCmd,
    },
    /// Browse the generation gallery.
    Gen {
        #[command(subcommand)]
        cmd: commands::gallery::GenCmd,
    },
    /// Inspect and reorder the global GPU job queue.
    Queue {
        #[command(subcommand)]
        cmd: commands::queue::QueueCmd,
    },
    /// Probe koblem and its GPU services.
    Health,
    /// Create, list and revoke API keys.
    Keys {
        #[command(subcommand)]
        cmd: commands::admin::KeysCmd,
    },
    /// Inspect the GPU scheduler.
    Gpu {
        #[command(subcommand)]
        cmd: commands::admin::GpuCmd,
    },
    /// Print a shell completion script (bash, zsh, fish, …).
    Completions { shell: Shell },
}

/// Shared context handed to every command handler.
pub struct Ctx {
    pub client: Client,
    pub config: Config,
    pub json: bool,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Groups that don't need a live client.
    match cli.command {
        Command::Config { cmd } => return config::run(cmd),
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "kob", &mut std::io::stdout());
            return Ok(());
        }
        _ => {}
    }

    let config = Config::load();
    let base = config.resolved_url(cli.url);
    let token = config.resolved_token(cli.token);
    let queue_flags = QueueFlags {
        mode: match (cli.queue, cli.now) {
            (true, _) => Some("queue".to_string()),
            (_, true) => Some("now".to_string()),
            _ => None,
        },
        band: cli.band,
        gpu: cli.gpu.clone(),
        force: cli.force,
        after: cli.after.clone(),
    };
    let client = Client::new(
        base,
        token,
        queue_flags,
        !cli.no_wait,
        std::time::Duration::from_secs(cli.max_wait),
    )?;
    let ctx = Ctx {
        client,
        config,
        json: cli.json,
    };

    match cli.command {
        Command::Config { .. } | Command::Completions { .. } => unreachable!("handled above"),
        Command::Chat(args) => commands::chat::run(&ctx, args).await,
        Command::Image { cmd } => commands::image::run(&ctx, cmd).await,
        Command::Rmbg(args) => commands::rmbg::run(&ctx, args).await,
        Command::Tts(args) => commands::tts::run(&ctx, args).await,
        Command::Stt(args) => commands::stt::run(&ctx, args).await,
        Command::Music { cmd } => commands::music::run(&ctx, cmd).await,
        Command::Song { cmd } => commands::song::run(&ctx, cmd).await,
        Command::Sfx { cmd } => commands::sfx::run(&ctx, cmd).await,
        Command::Video { cmd } => commands::video::run(&ctx, cmd).await,
        Command::Avatar(args) => commands::avatar::run(&ctx, args).await,
        Command::H3 { cmd } => commands::h3::run(&ctx, cmd).await,
        Command::Rig { cmd } => commands::rig::run(&ctx, cmd).await,
        Command::Vision { cmd } => commands::vision::run(&ctx, cmd).await,
        Command::Gen { cmd } => commands::gallery::run(&ctx, cmd).await,
        Command::Queue { cmd } => commands::queue::run(&ctx, cmd).await,
        Command::Health => commands::admin::health(&ctx).await,
        Command::Keys { cmd } => commands::admin::keys(&ctx, cmd).await,
        Command::Gpu { cmd } => commands::admin::gpu(&ctx, cmd).await,
    }
}
