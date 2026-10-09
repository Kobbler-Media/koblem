use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand};
use koblem_client::types::{
    MusicLyricsSource, SonggenDraftLyricsResponse, SonggenFixRhymesRequest, SonggenGenType,
    SonggenGenerateRequest, SonggenGenerateResponse, SonggenMode, SonggenProps,
    SonggenSeparateResponse, SonggenSuggestRequest, SonggenSuggestion,
};
use reqwest::multipart;

use crate::client::file_part;
use crate::commands::refine::{RefineArgs, read_arg, read_opt};
use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SongCmd {
    /// Compose a song from a description + lyrics (songgen / LeVo2).
    Generate(GenerateArgs),
    /// Clone an audio prompt's style into a new song.
    Clone(TransformArgs),
    /// Extend an audio prompt — the output is the continuation only.
    Continue(TransformArgs),
    /// htdemucs-split a mix into vocal + bgm stems (what /clone runs internally).
    Separate(SeparateArgs),
    /// Expand an idea into a description (+lyrics) — no render.
    Suggest(SuggestArgs),
    /// Draft songgen-format lyrics with the LLM — no render.
    DraftLyrics(DraftArgs),
    /// Rewrite only the lyric lines whose endings don't rhyme when sung.
    FixRhymes(FixRhymesArgs),
    /// Print the songgen engine's health/version.
    Props,
}

#[derive(Args, Debug, Clone)]
pub struct RenderArgs {
    /// Sung lyrics with [verse]/[chorus] tags. `@PATH` reads a file. songgen
    /// SINGS these — it cannot write them, so a blank lyric renders instrumental
    /// unless --lyrics-source llm.
    #[arg(long, value_name = "TEXT|@FILE")]
    pub lyrics: Option<String>,
    /// Who writes blank lyrics: ace (default → instrumental) | llm.
    #[arg(long)]
    pub lyrics_source: Option<String>,
    /// Brief for what LLM-written lyrics should be about.
    #[arg(long)]
    pub lyrics_prompt: Option<String>,
    /// Target seconds. Unset → renders to model-max (~270s) and stops on EOS.
    #[arg(long)]
    pub duration: Option<f64>,
    #[arg(long)]
    pub seed: Option<i64>,
    /// mixed (default) | vocal | bgm.
    #[arg(long)]
    pub gen_type: Option<String>,
    #[arg(long)]
    pub temp: Option<f64>,
    #[arg(long)]
    pub top_k: Option<u32>,
    #[arg(long)]
    pub cfg: Option<f64>,
    /// Output fade-in/out in ms.
    #[arg(long)]
    pub fade: Option<f64>,
    /// Don't persist the render to the gallery.
    #[arg(long)]
    pub no_save: bool,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct GenerateArgs {
    /// Style/mood/instrument description.
    pub description: String,
    #[command(flatten)]
    pub render: RenderArgs,
}

#[derive(Args, Debug)]
pub struct TransformArgs {
    /// Style/mood description for the result.
    #[arg(long)]
    pub description: Option<String>,
    /// Pre-split vocal stem.
    #[arg(long, value_name = "PATH")]
    pub vocal_stem: Option<PathBuf>,
    /// Pre-split backing stem.
    #[arg(long, value_name = "PATH")]
    pub bgm_stem: Option<PathBuf>,
    /// Full mix; the server separates it itself. Enough on its own.
    #[arg(long, value_name = "PATH")]
    pub full_mix: Option<PathBuf>,
    #[command(flatten)]
    pub render: RenderArgs,
}

#[derive(Args, Debug)]
pub struct SeparateArgs {
    /// Mix to split.
    pub full_mix: PathBuf,
}

#[derive(Args, Debug)]
pub struct SuggestArgs {
    /// Idea to expand into a songgen description (and matching lyrics).
    pub brief: String,
    /// Only write the description, not lyrics.
    #[arg(long)]
    pub description_only: bool,
}

#[derive(Args, Debug)]
pub struct DraftArgs {
    /// Description the lyrics should suit.
    #[arg(long)]
    pub description: Option<String>,
    /// What the lyrics should be about.
    #[arg(long)]
    pub lyrics_prompt: Option<String>,
    #[command(flatten)]
    pub refine: RefineArgs,
}

#[derive(Args, Debug)]
pub struct FixRhymesArgs {
    /// Lyrics to repair. `@PATH` reads a file, `@-` reads stdin.
    #[arg(value_name = "TEXT|@FILE")]
    pub lyrics: String,
    #[arg(long)]
    pub description: Option<String>,
}

fn parse_gen_type(s: &str) -> Result<SonggenGenType> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| anyhow!("unknown gen type '{s}' (mixed|vocal|bgm)"))
}

fn parse_lyrics_source(s: &str) -> Result<MusicLyricsSource> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| anyhow!("unknown lyrics source '{s}' (ace|llm)"))
}

pub async fn build_render_request(
    mode: SonggenMode,
    description: String,
    args: &RenderArgs,
) -> Result<SonggenGenerateRequest> {
    let gen_type = match &args.gen_type {
        Some(s) => parse_gen_type(s)?,
        None => SonggenGenType::default(),
    };
    let lyrics_source = match &args.lyrics_source {
        Some(s) => parse_lyrics_source(s)?,
        None => MusicLyricsSource::default(),
    };
    Ok(SonggenGenerateRequest {
        mode,
        lyric: read_opt(&args.lyrics).await?.unwrap_or_default(),
        description,
        lyrics_source,
        lyrics_prompt: args.lyrics_prompt.clone(),
        duration: args.duration,
        seed: args.seed,
        gen_type,
        temp: args.temp,
        top_k: args.top_k,
        cfg: args.cfg,
        fade: args.fade,
        save_song: !args.no_save,
        ..Default::default()
    })
}

pub async fn run(ctx: &Ctx, cmd: SongCmd) -> Result<()> {
    match cmd {
        SongCmd::Generate(args) => generate(ctx, args).await,
        SongCmd::Clone(args) => transform(ctx, SonggenMode::Clone, args).await,
        SongCmd::Continue(args) => transform(ctx, SonggenMode::Continue, args).await,
        SongCmd::Separate(args) => separate(ctx, args).await,
        SongCmd::Suggest(args) => suggest(ctx, args).await,
        SongCmd::DraftLyrics(args) => draft_lyrics(ctx, args).await,
        SongCmd::FixRhymes(args) => fix_rhymes(ctx, args).await,
        SongCmd::Props => props(ctx).await,
    }
}

async fn generate(ctx: &Ctx, args: GenerateArgs) -> Result<()> {
    let req = build_render_request(SonggenMode::Generate, args.description, &args.render).await?;
    let resp = ctx
        .client
        .post_json("/api/v1/songgen/generate", &[], &req)
        .await?;
    finish_render(ctx, resp, &args.render.output).await
}

async fn transform(ctx: &Ctx, mode: SonggenMode, args: TransformArgs) -> Result<()> {
    let have_stems = args.vocal_stem.is_some() && args.bgm_stem.is_some();
    if !have_stems && args.full_mix.is_none() {
        bail!("provide --vocal-stem plus --bgm-stem, or a --full-mix to auto-separate");
    }
    let req = build_render_request(
        mode,
        args.description.clone().unwrap_or_default(),
        &args.render,
    )
    .await?;
    let mut form = multipart::Form::new().text("request", serde_json::to_string(&req)?);
    if let Some(p) = &args.vocal_stem {
        form = form.part("vocal_stem", file_part(p).await?);
    }
    if let Some(p) = &args.bgm_stem {
        form = form.part("bgm_stem", file_part(p).await?);
    }
    if let Some(p) = &args.full_mix {
        form = form.part("full_mix", file_part(p).await?);
    }
    let path = match mode {
        SonggenMode::Clone => "/api/v1/songgen/clone",
        SonggenMode::Continue => "/api/v1/songgen/continue",
        SonggenMode::Generate => unreachable!("generate has no upload path"),
    };
    let resp = ctx.client.post_multipart(path, &[], form).await?;
    finish_render(ctx, resp, &args.render.output).await
}

async fn finish_render(ctx: &Ctx, resp: reqwest::Response, o: &Option<PathBuf>) -> Result<()> {
    let body: SonggenGenerateResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    output::write_tracks(ctx, &body.tracks, o).await?;
    if let Some(l) = &body.lyrics_used {
        eprintln!("--- lyrics ---\n{l}");
    }
    if let Some(d) = body.duration_used {
        eprintln!("duration={d:.1}s");
    }
    Ok(())
}

async fn separate(ctx: &Ctx, args: SeparateArgs) -> Result<()> {
    let form = multipart::Form::new().part("full_mix", file_part(&args.full_mix).await?);
    let resp = ctx
        .client
        .post_multipart("/api/v1/songgen/separate", &[], form)
        .await?;
    let body: SonggenSeparateResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    output::write_tracks(ctx, &[body.vocal.clone(), body.bgm.clone()], &None).await?;
    eprintln!("{}ms (gate {}ms)", body.duration_ms, body.gate_wait_ms);
    Ok(())
}

async fn suggest(ctx: &Ctx, args: SuggestArgs) -> Result<()> {
    let req = SonggenSuggestRequest {
        brief: args.brief,
        description_only: args.description_only,
    };
    let resp = ctx
        .client
        .post_json("/api/v1/songgen/suggest", &[], &req)
        .await?;
    let body: SonggenSuggestion = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.description);
    if let Some(l) = &body.lyrics {
        eprintln!("--- lyrics ---\n{l}");
    }
    Ok(())
}

async fn draft_lyrics(ctx: &Ctx, args: DraftArgs) -> Result<()> {
    let (refine_source, refine_instruction) = args.refine.resolve().await?;
    if args.description.is_none() && refine_source.is_none() {
        bail!("--description is required (or --refine-source to improve an existing draft)");
    }
    let req = SonggenGenerateRequest {
        description: args.description.unwrap_or_default(),
        lyrics_prompt: args.lyrics_prompt,
        lyrics_source: MusicLyricsSource::Llm,
        refine_source,
        refine_instruction,
        ..Default::default()
    };
    let resp = ctx
        .client
        .post_json("/api/v1/songgen/draft-lyrics", &[], &req)
        .await?;
    print_draft(ctx, resp).await
}

async fn fix_rhymes(ctx: &Ctx, args: FixRhymesArgs) -> Result<()> {
    let req = SonggenFixRhymesRequest {
        lyrics: read_arg(&args.lyrics).await?,
        description: args.description,
    };
    let resp = ctx
        .client
        .post_json("/api/v1/songgen/fix-rhymes", &[], &req)
        .await?;
    print_draft(ctx, resp).await
}

async fn print_draft(ctx: &Ctx, resp: reqwest::Response) -> Result<()> {
    let body: SonggenDraftLyricsResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.lyrics);
    for f in &body.rhyme_flags {
        eprintln!("eye-rhyme: {} / {}", f.a, f.b);
    }
    Ok(())
}

async fn props(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/api/v1/songgen/props", &[]).await?;
    let body: SonggenProps = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("healthy: {}", body.healthy);
    if let Some(v) = &body.version {
        println!("version: {v}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_args() -> RenderArgs {
        RenderArgs {
            lyrics: None,
            lyrics_source: None,
            lyrics_prompt: None,
            duration: None,
            seed: None,
            gen_type: None,
            temp: None,
            top_k: None,
            cfg: None,
            fade: None,
            no_save: false,
            output: None,
        }
    }

    #[tokio::test]
    async fn songgen_save_song_is_on_unless_the_caller_opts_out() {
        // songgen READS save_song; the Rust Default now matches the `true` wire
        // default, so a `..Default::default()` builder can't silently skip saving.
        assert!(SonggenGenerateRequest::default().save_song);

        let req = build_render_request(SonggenMode::Generate, "pop".into(), &render_args())
            .await
            .unwrap();
        assert!(req.save_song, "a plain `kob song generate` must save");

        let mut a = render_args();
        a.no_save = true;
        let req = build_render_request(SonggenMode::Generate, "pop".into(), &a)
            .await
            .unwrap();
        assert!(!req.save_song);
    }

    #[tokio::test]
    async fn every_songgen_knob_reaches_the_request() {
        let mut a = render_args();
        a.duration = Some(90.0);
        a.seed = Some(7);
        a.gen_type = Some("vocal".into());
        a.temp = Some(0.9);
        a.top_k = Some(50);
        a.cfg = Some(1.5);
        a.fade = Some(200.0);
        a.lyrics = Some("[verse]\nhi".into());
        a.lyrics_source = Some("llm".into());
        a.lyrics_prompt = Some("about rain".into());
        let req = build_render_request(SonggenMode::Clone, "dream pop".into(), &a)
            .await
            .unwrap();
        assert_eq!(req.mode, SonggenMode::Clone);
        assert_eq!(req.description, "dream pop");
        assert_eq!(req.lyric, "[verse]\nhi");
        assert_eq!(req.duration, Some(90.0));
        assert_eq!(req.seed, Some(7));
        assert_eq!(req.gen_type, SonggenGenType::Vocal);
        assert_eq!(req.temp, Some(0.9));
        assert_eq!(req.top_k, Some(50));
        assert_eq!(req.cfg, Some(1.5));
        assert_eq!(req.fade, Some(200.0));
        assert_eq!(req.lyrics_source, MusicLyricsSource::Llm);
        assert_eq!(req.lyrics_prompt.as_deref(), Some("about rain"));
    }

    #[tokio::test]
    async fn lyrics_honour_the_at_file_convention() {
        let path = std::env::temp_dir().join("kob-song-lyrics.txt");
        tokio::fs::write(&path, "[verse]\nfrom disk").await.unwrap();
        let mut a = render_args();
        a.lyrics = Some(format!("@{}", path.display()));
        let req = build_render_request(SonggenMode::Generate, "pop".into(), &a)
            .await
            .unwrap();
        assert_eq!(req.lyric, "[verse]\nfrom disk");
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_gen_type_and_lyrics_source_are_rejected() {
        let mut a = render_args();
        a.gen_type = Some("stereo".into());
        assert!(
            build_render_request(SonggenMode::Generate, "x".into(), &a)
                .await
                .is_err()
        );

        let mut a = render_args();
        a.lyrics_source = Some("gpt".into());
        assert!(
            build_render_request(SonggenMode::Generate, "x".into(), &a)
                .await
                .is_err()
        );
    }

    #[test]
    fn separate_response_parses_both_stems() {
        let body: SonggenSeparateResponse = serde_json::from_str(
            r#"{"vocal":{"filename":"v.wav","url":"/api/v1/songgen/file/v.wav","seed":-1,
                 "content_type":"audio/wav","bytes":10},
                "bgm":{"filename":"b.wav","url":"/api/v1/songgen/file/b.wav","seed":-1,
                 "content_type":"audio/wav","bytes":20},
                "duration_ms":900,"gate_wait_ms":10}"#,
        )
        .unwrap();
        assert_eq!(body.vocal.filename, "v.wav");
        assert_eq!(body.bgm.bytes, 20);
        assert_eq!(body.duration_ms, 900);
    }
}
