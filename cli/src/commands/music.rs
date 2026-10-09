use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand};
use koblem_client::types::{
    MusicDraftLyricsResponse, MusicFixRhymesRequest, MusicGenerateRequest, MusicGenerateResponse,
    MusicLyricsSource, MusicMode, MusicProps, MusicSuggestRequest, MusicSuggestion,
    MusicUnderstood,
};
use reqwest::multipart;

use crate::client::file_part;
use crate::commands::refine::{RefineArgs, read_arg, read_opt};
use crate::config::Config;
use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum MusicCmd {
    /// Compose a song from a caption (text-to-music).
    Generate(GenerateArgs),
    /// Cover, repaint, extract, lego or complete a source song.
    Transform(TransformArgs),
    /// Describe what an uploaded song sounds like.
    Understand(UnderstandArgs),
    /// Expand an idea into a caption (+lyrics) — no render.
    Suggest(SuggestArgs),
    /// Draft lyrics with the LLM from a caption — no render.
    DraftLyrics(DraftArgs),
    /// Rewrite only the lyric lines whose endings don't rhyme when sung.
    FixRhymes(FixRhymesArgs),
    /// Print the music engine's capabilities/health.
    Props,
}

#[derive(Args, Debug)]
pub struct SuggestArgs {
    /// Idea to expand into a music caption (and matching lyrics).
    pub brief: String,
    /// Only write the caption, not lyrics.
    #[arg(long)]
    pub caption_only: bool,
}

#[derive(Args, Debug)]
pub struct DraftArgs {
    /// Music description the lyrics should suit.
    #[arg(long)]
    pub caption: Option<String>,
    /// What the lyrics should be about.
    #[arg(long)]
    pub lyrics_prompt: Option<String>,
    #[arg(long)]
    pub vocal_language: Option<String>,
    #[command(flatten)]
    pub refine: RefineArgs,
}

#[derive(Args, Debug)]
pub struct FixRhymesArgs {
    /// Lyrics to repair. `@PATH` reads a file, `@-` reads stdin.
    #[arg(value_name = "TEXT|@FILE")]
    pub lyrics: String,
    /// Style context, so the rewrite stays on-brief.
    #[arg(long)]
    pub caption: Option<String>,
}

#[derive(Args, Debug)]
pub struct GenerateArgs {
    /// Style/mood/instrument description.
    pub caption: String,
    /// Lyrics. Blank → written for you; "[Instrumental]" → no vocals.
    /// `@PATH` reads a file, `@-` reads stdin.
    #[arg(long, value_name = "TEXT|@FILE")]
    pub lyrics: Option<String>,
    /// Brief for what the lyrics should be about (with --lyrics-source llm).
    #[arg(long)]
    pub lyrics_prompt: Option<String>,
    /// Who writes blank lyrics: ace (default) | llm.
    #[arg(long)]
    pub lyrics_source: Option<String>,
    /// Target seconds (default: auto from word count).
    #[arg(long)]
    pub duration: Option<f64>,
    #[arg(long)]
    pub seed: Option<i64>,
    /// Output format: mp3 (default) | wav16 | wav24 | wav32.
    #[arg(long)]
    pub format: Option<String>,
    /// Distinct songs to compose (1-4).
    #[arg(long)]
    pub songs: Option<u32>,
    /// Seed-variations per song (1-9).
    #[arg(long)]
    pub takes: Option<u32>,
    /// DiT denoising steps.
    #[arg(long)]
    pub steps: Option<u32>,
    /// DiT GGUF filename override (see `kob music props`).
    #[arg(long)]
    pub synth_model: Option<String>,
    /// Flow-matching schedule shift override.
    #[arg(long)]
    pub shift: Option<f64>,
    #[arg(long)]
    pub vocal_language: Option<String>,
    /// Don't persist the render to the gallery.
    #[arg(long)]
    pub no_save: bool,
    /// Output file (single track only; default: server filename).
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct TransformArgs {
    /// Source audio (WAV).
    pub audio: PathBuf,
    /// cover (default) | repaint | extract | lego | complete.
    #[arg(long, default_value = "cover")]
    pub mode: String,
    /// Style/mood description for the result. Required except for
    /// extract/lego/complete.
    #[arg(long)]
    pub caption: Option<String>,
    /// Replacement lyrics. `@PATH` reads a file, `@-` reads stdin.
    #[arg(long, value_name = "TEXT|@FILE")]
    pub lyrics: Option<String>,
    /// cover: fraction of steps that see the source (0.7-0.9).
    #[arg(long)]
    pub cover_strength: Option<f64>,
    /// cover: FSQ roundtrip ("in the style of") instead of the faithful
    /// cover-nofsq remix — tolerates new/different lyrics.
    #[arg(long)]
    pub cover_reference: bool,
    /// cover: SDEdit init blend, 0.0 (pure noise, default) → 1.0 (near-identical).
    #[arg(long)]
    pub cover_noise_strength: Option<f64>,
    /// extract/lego/complete: the stem to isolate/generate/add — e.g. vocals,
    /// drums, bass, guitar, keyboard. `complete` accepts "VOCALS | DRUMS".
    #[arg(long)]
    pub track: Option<String>,
    /// repaint: region start seconds.
    #[arg(long)]
    pub repaint_start: Option<f64>,
    /// repaint: region end seconds (-1 = source end).
    #[arg(long)]
    pub repaint_end: Option<f64>,
    /// Optional timbre reference audio.
    #[arg(long, value_name = "PATH")]
    pub ref_audio: Option<PathBuf>,
    #[arg(long)]
    pub seed: Option<i64>,
    #[arg(long)]
    pub format: Option<String>,
    #[arg(long)]
    pub steps: Option<u32>,
    /// Don't persist the render to the gallery.
    #[arg(long)]
    pub no_save: bool,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct UnderstandArgs {
    /// Song to analyse (WAV).
    pub audio: PathBuf,
}

fn parse_lyrics_source(s: &str) -> Result<MusicLyricsSource> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| anyhow!("unknown lyrics source '{s}' (ace|llm)"))
}

fn parse_mode(s: &str) -> Result<MusicMode> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| anyhow!("unknown mode '{s}' (cover|repaint|extract|lego|complete)"))
}

/// Modes that isolate/add a stem and therefore need `--track`.
fn needs_track(mode: MusicMode) -> bool {
    matches!(
        mode,
        MusicMode::Extract | MusicMode::Lego | MusicMode::Complete
    )
}

pub async fn run(ctx: &Ctx, cmd: MusicCmd) -> Result<()> {
    match cmd {
        MusicCmd::Generate(args) => generate(ctx, args).await,
        MusicCmd::Transform(args) => transform(ctx, args).await,
        MusicCmd::Understand(args) => understand(ctx, args).await,
        MusicCmd::Suggest(args) => suggest(ctx, args).await,
        MusicCmd::DraftLyrics(args) => draft_lyrics(ctx, args).await,
        MusicCmd::FixRhymes(args) => fix_rhymes(ctx, args).await,
        MusicCmd::Props => props(ctx).await,
    }
}

async fn suggest(ctx: &Ctx, args: SuggestArgs) -> Result<()> {
    let req = MusicSuggestRequest {
        brief: args.brief,
        caption_only: args.caption_only,
    };
    let resp = ctx
        .client
        .post_json("/api/v1/music/suggest", &[], &req)
        .await?;
    let body: MusicSuggestion = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.caption);
    if let Some(l) = &body.lyrics {
        eprintln!("--- lyrics ---\n{l}");
    }
    Ok(())
}

pub fn build_draft_request(
    caption: Option<String>,
    lyrics_prompt: Option<String>,
    vocal_language: Option<String>,
    refine: (Option<String>, Option<String>),
) -> Result<MusicGenerateRequest> {
    let (refine_source, refine_instruction) = refine;
    if caption.is_none() && refine_source.is_none() {
        bail!("--caption is required (or --refine-source to improve an existing draft)");
    }
    Ok(MusicGenerateRequest {
        caption: caption.unwrap_or_default(),
        lyrics_prompt,
        lyrics_source: MusicLyricsSource::Llm,
        vocal_language,
        refine_source,
        refine_instruction,
        ..Default::default()
    })
}

async fn draft_lyrics(ctx: &Ctx, args: DraftArgs) -> Result<()> {
    let req = build_draft_request(
        args.caption,
        args.lyrics_prompt,
        args.vocal_language,
        args.refine.resolve().await?,
    )?;
    let resp = ctx
        .client
        .post_json("/api/v1/music/draft-lyrics", &[], &req)
        .await?;
    let body: MusicDraftLyricsResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.lyrics);
    print_draft_notes(&body);
    Ok(())
}

fn print_draft_notes(body: &MusicDraftLyricsResponse) {
    for f in &body.rhyme_flags {
        eprintln!("eye-rhyme: {} / {}", f.a, f.b);
    }
    if let Some(s) = body.suggested_seconds {
        eprintln!("suggested duration: {s:.0}s");
    }
}

async fn fix_rhymes(ctx: &Ctx, args: FixRhymesArgs) -> Result<()> {
    let req = MusicFixRhymesRequest {
        lyrics: read_arg(&args.lyrics).await?,
        caption: args.caption,
    };
    let resp = ctx
        .client
        .post_json("/api/v1/music/fix-rhymes", &[], &req)
        .await?;
    let body: MusicDraftLyricsResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.lyrics);
    print_draft_notes(&body);
    Ok(())
}

async fn props(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/api/v1/music/props", &[]).await?;
    let body: MusicProps = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("healthy: {}", body.healthy);
    for m in &body.dit_models {
        println!("dit  {m}");
    }
    for m in &body.lm_models {
        println!("lm   {m}");
    }
    Ok(())
}

pub async fn build_generate_request(
    args: GenerateArgs,
    cfg: &Config,
) -> Result<MusicGenerateRequest> {
    let lyrics_source = match args.lyrics_source {
        Some(ref s) => parse_lyrics_source(s)?,
        None => MusicLyricsSource::default(),
    };
    Ok(MusicGenerateRequest {
        mode: MusicMode::Generate,
        caption: args.caption,
        lyrics: read_opt(&args.lyrics).await?,
        lyrics_source,
        lyrics_prompt: args.lyrics_prompt,
        duration: args.duration,
        seed: args.seed,
        vocal_language: args
            .vocal_language
            .or_else(|| cfg.def_str("music", "vocal_language")),
        output_format: args.format.or_else(|| cfg.def_str("music", "format")),
        songs: args.songs,
        takes: args.takes,
        synth_model: args.synth_model,
        shift: args.shift,
        inference_steps: args.steps.or_else(|| cfg.def_u32("music", "steps")),
        save_song: !args.no_save,
        ..Default::default()
    })
}

async fn generate(ctx: &Ctx, args: GenerateArgs) -> Result<()> {
    let output = args.output.clone();
    let req = build_generate_request(args, &ctx.config).await?;
    let resp = ctx
        .client
        .post_json("/api/v1/music/generate", &[], &req)
        .await?;
    let body: MusicGenerateResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    output::write_tracks(ctx, &body.tracks, &output).await?;
    eprintln!("duration={:.1}s", body.duration_used);
    Ok(())
}

/// A whitespace-only flag is an empty flag: it survives `is_none()` but is
/// trimmed away downstream, so the engine renders against nothing.
fn non_blank(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub async fn build_transform_request(
    args: &TransformArgs,
    cfg: &Config,
) -> Result<MusicGenerateRequest> {
    let mode = parse_mode(&args.mode)?;
    if mode == MusicMode::Generate {
        bail!("mode 'generate' has no source audio — use `kob music generate`");
    }
    let track = non_blank(&args.track);
    let caption = non_blank(&args.caption);
    if needs_track(mode) && track.is_none() {
        bail!(
            "--track is required for {} (e.g. --track vocals)",
            args.mode
        );
    }
    if !needs_track(mode) && caption.is_none() {
        bail!(
            "--caption is required for {} — describe the music style",
            args.mode
        );
    }
    Ok(MusicGenerateRequest {
        mode,
        caption: caption.unwrap_or_default(),
        lyrics: read_opt(&args.lyrics).await?,
        seed: args.seed,
        cover_strength: args.cover_strength,
        cover_reference: args.cover_reference,
        cover_noise_strength: args.cover_noise_strength,
        track,
        repaint_start: args.repaint_start,
        repaint_end: args.repaint_end,
        inference_steps: args.steps.or_else(|| cfg.def_u32("music", "steps")),
        output_format: args
            .format
            .clone()
            .or_else(|| cfg.def_str("music", "format")),
        save_song: !args.no_save,
        ..Default::default()
    })
}

async fn transform(ctx: &Ctx, args: TransformArgs) -> Result<()> {
    let req = build_transform_request(&args, &ctx.config).await?;
    let mut form = multipart::Form::new()
        .text("request", serde_json::to_string(&req)?)
        .part("audio", file_part(&args.audio).await?);
    if let Some(p) = &args.ref_audio {
        form = form.part("ref_audio", file_part(p).await?);
    }
    let resp = ctx
        .client
        .post_multipart("/api/v1/music/transform", &[], form)
        .await?;
    let body: MusicGenerateResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    output::write_tracks(ctx, &body.tracks, &args.output).await?;
    Ok(())
}

async fn understand(ctx: &Ctx, args: UnderstandArgs) -> Result<()> {
    let form = multipart::Form::new().part("audio", file_part(&args.audio).await?);
    let resp = ctx
        .client
        .post_multipart("/api/v1/music/understand", &[], form)
        .await?;
    let body: MusicUnderstood = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.caption);
    if let Some(l) = &body.lyrics {
        eprintln!("--- lyrics ---\n{l}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform_args(mode: &str) -> TransformArgs {
        TransformArgs {
            audio: PathBuf::from("src.wav"),
            mode: mode.to_string(),
            caption: Some("warm lo-fi".into()),
            lyrics: None,
            cover_strength: None,
            cover_reference: false,
            cover_noise_strength: None,
            track: None,
            repaint_start: None,
            repaint_end: None,
            ref_audio: None,
            seed: None,
            format: None,
            steps: None,
            no_save: false,
            output: None,
        }
    }

    fn generate_args() -> GenerateArgs {
        GenerateArgs {
            caption: "c".into(),
            lyrics: None,
            lyrics_prompt: None,
            lyrics_source: None,
            duration: None,
            seed: None,
            format: None,
            songs: None,
            takes: None,
            steps: None,
            synth_model: None,
            shift: None,
            vocal_language: None,
            no_save: false,
            output: None,
        }
    }

    #[test]
    fn all_five_transform_modes_parse() {
        for (s, want) in [
            ("cover", MusicMode::Cover),
            ("repaint", MusicMode::Repaint),
            ("extract", MusicMode::Extract),
            ("lego", MusicMode::Lego),
            ("complete", MusicMode::Complete),
        ] {
            assert_eq!(parse_mode(s).unwrap(), want, "mode {s}");
        }
    }

    #[test]
    fn unknown_mode_error_lists_every_accepted_mode() {
        let e = parse_mode("bogus").unwrap_err().to_string();
        for m in ["cover", "repaint", "extract", "lego", "complete"] {
            assert!(e.contains(m), "error {e:?} should mention {m}");
        }
    }

    #[tokio::test]
    async fn generate_is_rejected_on_the_transform_path() {
        let err = build_transform_request(&transform_args("generate"), &Config::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("kob music generate"), "{err}");
    }

    #[tokio::test]
    async fn stem_modes_demand_a_track_selector() {
        for mode in ["extract", "lego", "complete"] {
            let mut args = transform_args(mode);
            args.caption = None;
            let err = build_transform_request(&args, &Config::default())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("--track"), "mode {mode}: {err}");

            args.track = Some("vocals".into());
            let req = build_transform_request(&args, &Config::default())
                .await
                .unwrap();
            assert_eq!(req.track.as_deref(), Some("vocals"), "mode {mode}");
        }
    }

    /// A blank flag reaches the engine as an empty string and renders garbage
    /// (track) or a bare 400 (caption) — reject it the way an absent flag is.
    #[tokio::test]
    async fn blank_caption_and_track_are_treated_as_absent() {
        for blank in ["", "  ", "\n\t"] {
            let mut a = transform_args("cover");
            a.caption = Some(blank.into());
            let err = build_transform_request(&a, &Config::default())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("--caption"), "caption {blank:?}: {err}");

            let mut a = transform_args("extract");
            a.track = Some(blank.into());
            let err = build_transform_request(&a, &Config::default())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("--track"), "track {blank:?}: {err}");
        }
        // A padded-but-real value survives, trimmed.
        let mut a = transform_args("extract");
        a.track = Some("  vocals ".into());
        let req = build_transform_request(&a, &Config::default())
            .await
            .unwrap();
        assert_eq!(req.track.as_deref(), Some("vocals"));
    }

    #[tokio::test]
    async fn cover_reinterpretation_knobs_reach_the_request() {
        let mut args = transform_args("cover");
        args.cover_reference = true;
        args.cover_noise_strength = Some(0.35);
        args.cover_strength = Some(0.8);
        let req = build_transform_request(&args, &Config::default())
            .await
            .unwrap();
        assert!(req.cover_reference, "cover_reference must be settable");
        assert_eq!(req.cover_noise_strength, Some(0.35));
        assert_eq!(req.cover_strength, Some(0.8));
    }

    #[tokio::test]
    async fn transform_defaults_to_the_faithful_cover_path() {
        let req = build_transform_request(&transform_args("cover"), &Config::default())
            .await
            .unwrap();
        assert!(!req.cover_reference);
        assert_eq!(req.cover_noise_strength, None);
    }

    #[tokio::test]
    async fn save_song_is_on_unless_the_caller_opts_out() {
        // The wire default is true, and so is the Rust one — a `..Default::default()`
        // builder can no longer silently opt out of saving.
        assert!(MusicGenerateRequest::default().save_song);

        assert!(
            build_transform_request(&transform_args("cover"), &Config::default())
                .await
                .unwrap()
                .save_song
        );
        let mut args = transform_args("cover");
        args.no_save = true;
        assert!(
            !build_transform_request(&args, &Config::default())
                .await
                .unwrap()
                .save_song
        );

        assert!(
            build_generate_request(generate_args(), &Config::default())
                .await
                .unwrap()
                .save_song
        );
        let mut g = generate_args();
        g.no_save = true;
        assert!(
            !build_generate_request(g, &Config::default())
                .await
                .unwrap()
                .save_song
        );
    }

    #[tokio::test]
    async fn generate_forwards_the_engine_overrides() {
        let mut args = generate_args();
        args.synth_model = Some("ace-turbo.gguf".into());
        args.shift = Some(3.0);
        let req = build_generate_request(args, &Config::default())
            .await
            .unwrap();
        assert_eq!(req.synth_model.as_deref(), Some("ace-turbo.gguf"));
        assert_eq!(req.shift, Some(3.0));
    }

    /// `--lyrics @file` must reach ACE-Step as the file's CONTENTS: a verbatim
    /// copy burns a full GPU render singing the literal string "@lyrics.txt".
    #[tokio::test]
    async fn lyrics_honour_the_at_file_convention_on_both_render_paths() {
        let path = std::env::temp_dir().join("kob-music-lyrics.txt");
        tokio::fs::write(&path, "[Verse]\nrain on the window")
            .await
            .unwrap();
        let at = format!("@{}", path.display());

        let mut g = generate_args();
        g.lyrics = Some(at.clone());
        let req = build_generate_request(g, &Config::default()).await.unwrap();
        assert_eq!(req.lyrics.as_deref(), Some("[Verse]\nrain on the window"));

        let mut t = transform_args("cover");
        t.lyrics = Some(at);
        let req = build_transform_request(&t, &Config::default())
            .await
            .unwrap();
        assert_eq!(req.lyrics.as_deref(), Some("[Verse]\nrain on the window"));

        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[test]
    fn draft_lyrics_carries_the_refine_pair() {
        let req = build_draft_request(
            Some("dream pop".into()),
            None,
            None,
            (Some("old lyrics".into()), Some("punchier".into())),
        )
        .unwrap();
        assert_eq!(req.refine_source.as_deref(), Some("old lyrics"));
        assert_eq!(req.refine_instruction.as_deref(), Some("punchier"));
        assert_eq!(req.lyrics_source, MusicLyricsSource::Llm);
    }

    #[test]
    fn draft_lyrics_accepts_a_refine_with_no_caption() {
        let req = build_draft_request(None, None, None, (Some("old".into()), None)).unwrap();
        assert!(req.caption.is_empty());
        assert_eq!(req.refine_source.as_deref(), Some("old"));
        assert!(build_draft_request(None, None, None, (None, None)).is_err());
    }
}
