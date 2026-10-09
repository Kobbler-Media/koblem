use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use koblem_client::types::{Sa3GenerateRequest, Sa3GenerateResponse, Sa3Props};
use reqwest::multipart;

use crate::client::file_part;
use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SfxCmd {
    /// Text → audio (Stable Audio 3). Game SFX / short music.
    Generate(GenerateArgs),
    /// Audio → audio: restyle, inpaint, or continue an input clip.
    Edit(EditArgs),
    /// Print the sa3 engine's health/version.
    Props,
}

#[derive(Args, Debug)]
pub struct GenerateArgs {
    /// Description of the audio to generate.
    pub prompt: String,
    /// Output length in seconds (server default 10; max 380).
    #[arg(long)]
    pub seconds: Option<f32>,
    #[arg(long)]
    pub seed: Option<i64>,
    /// pingpong (default, stochastic) | euler (reproducible with a seed).
    #[arg(long)]
    pub sampler: Option<String>,
    #[arg(long)]
    pub steps: Option<u32>,
    /// Weight quant: q8 (default) | q6.
    #[arg(long)]
    pub model: Option<String>,
    /// Don't persist the render to the gallery.
    #[arg(long)]
    pub no_save: bool,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct EditArgs {
    /// Input audio (WAV).
    pub input: PathBuf,
    /// a2a (SDEdit restyle) | inpaint | continuation.
    #[arg(long)]
    pub mode: String,
    /// Description of the result.
    pub prompt: String,
    /// inpaint only: regions to regenerate, "s0:e0,s1:e1" in seconds.
    #[arg(long)]
    pub mask: Option<String>,
    /// continuation: total length. a2a/inpaint: unused.
    #[arg(long)]
    pub seconds: Option<f32>,
    /// a2a only: SDEdit strength.
    #[arg(long)]
    pub strength: Option<f32>,
    #[arg(long)]
    pub seed: Option<i64>,
    #[arg(long)]
    pub sampler: Option<String>,
    #[arg(long)]
    pub steps: Option<u32>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub no_save: bool,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

pub fn build_generate_request(args: &GenerateArgs) -> Sa3GenerateRequest {
    Sa3GenerateRequest {
        prompt: args.prompt.clone(),
        seconds: args.seconds,
        seed: args.seed,
        sampler: args.sampler.clone(),
        steps: args.steps,
        model: args.model.clone(),
        // `save` defaults true on the wire; a derived Default would flip it.
        save: !args.no_save,
    }
}

/// The `edit` route reads plain multipart text fields, not a JSON `request` part.
pub fn edit_fields(args: &EditArgs) -> Result<Vec<(&'static str, String)>> {
    if !matches!(args.mode.as_str(), "a2a" | "inpaint" | "continuation") {
        bail!("unknown mode '{}' (a2a|inpaint|continuation)", args.mode);
    }
    if args.mode == "inpaint" && args.mask.as_ref().is_none_or(|m| m.trim().is_empty()) {
        bail!("inpaint needs --mask \"s0:e0,s1:e1\" (seconds)");
    }
    if args.mode == "continuation" && args.seconds.is_none() {
        bail!("continuation needs --seconds (the total length)");
    }
    let mut f: Vec<(&'static str, String)> = vec![
        ("mode", args.mode.clone()),
        ("prompt", args.prompt.clone()),
        ("save", (!args.no_save).to_string()),
    ];
    let mut push = |k: &'static str, v: Option<String>| {
        if let Some(v) = v {
            f.push((k, v));
        }
    };
    push("model", args.model.clone());
    push("seed", args.seed.map(|v| v.to_string()));
    push("sampler", args.sampler.clone());
    push("steps", args.steps.map(|v| v.to_string()));
    push("strength", args.strength.map(|v| v.to_string()));
    push("mask", args.mask.clone());
    push("seconds", args.seconds.map(|v| v.to_string()));
    Ok(f)
}

pub async fn run(ctx: &Ctx, cmd: SfxCmd) -> Result<()> {
    match cmd {
        SfxCmd::Generate(args) => generate(ctx, args).await,
        SfxCmd::Edit(args) => edit(ctx, args).await,
        SfxCmd::Props => props(ctx).await,
    }
}

async fn generate(ctx: &Ctx, args: GenerateArgs) -> Result<()> {
    let req = build_generate_request(&args);
    let resp = ctx
        .client
        .post_json("/api/v1/sa3/generate", &[], &req)
        .await?;
    finish(ctx, resp, &args.output).await
}

async fn edit(ctx: &Ctx, args: EditArgs) -> Result<()> {
    let mut form = multipart::Form::new().part("input", file_part(&args.input).await?);
    for (k, v) in edit_fields(&args)? {
        form = form.text(k, v);
    }
    let resp = ctx
        .client
        .post_multipart("/api/v1/sa3/edit", &[], form)
        .await?;
    finish(ctx, resp, &args.output).await
}

async fn finish(ctx: &Ctx, resp: reqwest::Response, o: &Option<PathBuf>) -> Result<()> {
    let body: Sa3GenerateResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    output::write_tracks(ctx, &body.tracks, o).await?;
    if let Some(s) = body.seconds_used {
        eprintln!("{s:.1}s");
    }
    Ok(())
}

async fn props(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/api/v1/sa3/props", &[]).await?;
    let body: Sa3Props = resp.json().await?;
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

    fn edit_args(mode: &str) -> EditArgs {
        EditArgs {
            input: PathBuf::from("in.wav"),
            mode: mode.into(),
            prompt: "rain on a tin roof".into(),
            mask: None,
            seconds: None,
            strength: None,
            seed: None,
            sampler: None,
            steps: None,
            model: None,
            no_save: false,
            output: None,
        }
    }

    #[test]
    fn sa3_save_is_on_unless_the_caller_opts_out() {
        // The wire default is true; a derived Default would silently disable saving.
        assert!(Sa3GenerateRequest::default().save);
        let args = GenerateArgs {
            prompt: "a door slam".into(),
            seconds: None,
            seed: None,
            sampler: None,
            steps: None,
            model: None,
            no_save: false,
            output: None,
        };
        assert!(build_generate_request(&args).save);
        let off = GenerateArgs {
            no_save: true,
            ..args
        };
        assert!(!build_generate_request(&off).save);
    }

    #[test]
    fn inpaint_without_a_mask_is_rejected_client_side() {
        let mut a = edit_args("inpaint");
        assert!(edit_fields(&a).unwrap_err().to_string().contains("--mask"));
        a.mask = Some("  ".into());
        assert!(edit_fields(&a).is_err());
        a.mask = Some("0:1".into());
        assert!(edit_fields(&a).is_ok());
    }

    #[test]
    fn continuation_without_seconds_is_rejected_client_side() {
        let mut a = edit_args("continuation");
        assert!(
            edit_fields(&a)
                .unwrap_err()
                .to_string()
                .contains("--seconds")
        );
        a.seconds = Some(30.0);
        assert!(edit_fields(&a).is_ok());
    }

    #[test]
    fn unknown_edit_mode_names_the_three_valid_ones() {
        let e = edit_fields(&edit_args("remix")).unwrap_err().to_string();
        for m in ["a2a", "inpaint", "continuation"] {
            assert!(e.contains(m), "{e}");
        }
    }

    #[test]
    fn edit_fields_carry_every_knob_the_server_reads() {
        let mut a = edit_args("a2a");
        a.strength = Some(0.6);
        a.seed = Some(3);
        a.sampler = Some("euler".into());
        a.steps = Some(12);
        a.model = Some("q6".into());
        a.seconds = Some(20.0);
        let f = edit_fields(&a).unwrap();
        let keys: Vec<&str> = f.iter().map(|(k, _)| *k).collect();
        for k in [
            "mode", "prompt", "save", "model", "seed", "sampler", "steps", "strength", "seconds",
        ] {
            assert!(keys.contains(&k), "missing {k} in {keys:?}");
        }
        assert!(f.contains(&("save", "true".to_string())));
    }
}
