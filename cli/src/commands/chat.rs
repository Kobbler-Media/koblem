use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;
use koblem_client::types::{
    ChatRequest, ChatResponse, GrammarMode, SamplingConfig, SamplingPreset,
};

use crate::config::Config;
use crate::{Ctx, output};

#[derive(Args, Debug)]
pub struct ChatArgs {
    /// The prompt / question for the LLM.
    pub prompt: String,
    /// System prompt.
    #[arg(long)]
    pub system: Option<String>,
    /// Image file(s) to attach (vision). Repeatable.
    #[arg(long = "image", value_name = "PATH")]
    pub images: Vec<PathBuf>,
    /// Audio file(s) to attach (audio-capable model). Repeatable.
    #[arg(long = "audio", value_name = "PATH")]
    pub audio: Vec<PathBuf>,
    #[arg(long)]
    pub temperature: Option<f64>,
    #[arg(long)]
    pub top_p: Option<f64>,
    #[arg(long)]
    pub top_k: Option<i32>,
    #[arg(long)]
    pub min_p: Option<f64>,
    #[arg(long)]
    pub presence_penalty: Option<f64>,
    #[arg(long)]
    pub repetition_penalty: Option<f64>,
    #[arg(long)]
    pub max_tokens: Option<i32>,
    /// Sampling preset: deterministic | qwen_instruct_general | qwen_instruct_reasoning.
    #[arg(long)]
    pub preset: Option<String>,
    /// Grammar-constrained decoding: "off", the built-in "review", or the name
    /// of a grammar stored in koblem's grammars table.
    #[arg(long, value_name = "MODE")]
    pub grammar: Option<String>,
    /// Enable the model's thinking pass.
    #[arg(long)]
    pub think: bool,
}

fn parse_preset(s: &str) -> Result<SamplingPreset> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(|_| {
        anyhow!(
            "unknown preset '{s}' (deterministic|qwen_instruct_general|qwen_instruct_reasoning)"
        )
    })
}

/// Every knob `SamplingConfig` carries, resolved from flags then config.
pub fn build_sampling(args: &ChatArgs, cfg: &Config) -> Result<SamplingConfig> {
    let preset = match args.preset {
        Some(ref s) => Some(parse_preset(s)?),
        None => None,
    };
    Ok(SamplingConfig {
        preset,
        temperature: args
            .temperature
            .or_else(|| cfg.def_f64("chat", "temperature")),
        top_p: args.top_p.or_else(|| cfg.def_f64("chat", "top_p")),
        top_k: args.top_k.or_else(|| cfg.def_i32("chat", "top_k")),
        min_p: args.min_p.or_else(|| cfg.def_f64("chat", "min_p")),
        presence_penalty: args.presence_penalty,
        repetition_penalty: args.repetition_penalty,
        max_tokens: args
            .max_tokens
            .or_else(|| cfg.def_i32("chat", "max_tokens")),
        enable_thinking: if args.think { Some(true) } else { None },
        // No CLI flag yet: the effort budget is a Settings-page knob, and it is
        // inert unless thinking is on anyway.
        reasoning_effort: None,
        grammar: args
            .grammar
            .clone()
            .or_else(|| cfg.def_str("chat", "grammar"))
            .map(|s| GrammarMode::from_wire(&s)),
    })
}

pub async fn run(ctx: &Ctx, args: ChatArgs) -> Result<()> {
    let mut images = Vec::with_capacity(args.images.len());
    for path in &args.images {
        images.push(output::read_data_uri(path).await?);
    }
    let mut audio = Vec::with_capacity(args.audio.len());
    for path in &args.audio {
        audio.push(output::read_data_uri(path).await?);
    }

    let sampling = build_sampling(&args, &ctx.config)?;

    let req = ChatRequest {
        prompt: args.prompt,
        system: args.system,
        images,
        audio,
        sampling,
    };

    let resp = ctx.client.post_json("/api/v1/chat", &[], &req).await?;
    let body: ChatResponse = resp.json().await?;

    if ctx.json {
        output::print_json(&body)?;
    } else {
        println!("{}", body.response);
        // On stderr and BEFORE the stats line: the answer on stdout is still the
        // answer, but "your audio never reached the model" changes how to read it,
        // and a `--audio` run that silently behaves like a text run is exactly the
        // confusion the drop-and-notify policy exists to prevent.
        for w in &body.warnings {
            eprintln!("warning: {w}");
        }
        if let (Some(p), Some(c)) = (body.prompt_tokens, body.completion_tokens) {
            eprintln!(
                "({p} prompt + {c} completion tokens, {} ms)",
                body.duration_ms
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ChatArgs {
        ChatArgs {
            prompt: "hi".into(),
            system: None,
            images: Vec::new(),
            audio: Vec::new(),
            temperature: None,
            top_p: None,
            top_k: None,
            min_p: None,
            presence_penalty: None,
            repetition_penalty: None,
            max_tokens: None,
            preset: None,
            grammar: None,
            think: false,
        }
    }

    #[test]
    fn every_sampling_knob_is_reachable_from_a_flag() {
        let mut a = args();
        a.temperature = Some(0.3);
        a.top_p = Some(0.9);
        a.top_k = Some(40);
        a.min_p = Some(0.05);
        a.presence_penalty = Some(1.5);
        a.repetition_penalty = Some(1.1);
        a.max_tokens = Some(256);
        a.preset = Some("deterministic".into());
        a.grammar = Some("review".into());
        a.think = true;
        let s = build_sampling(&a, &Config::default()).unwrap();
        assert_eq!(s.temperature, Some(0.3));
        assert_eq!(s.top_p, Some(0.9));
        assert_eq!(s.top_k, Some(40));
        assert_eq!(s.min_p, Some(0.05));
        assert_eq!(s.presence_penalty, Some(1.5));
        assert_eq!(s.repetition_penalty, Some(1.1));
        assert_eq!(s.max_tokens, Some(256));
        assert_eq!(s.preset, Some(SamplingPreset::Deterministic));
        assert_eq!(s.enable_thinking, Some(true));
        assert_eq!(s.grammar, Some(GrammarMode::Review));
    }

    #[test]
    fn grammar_serialises_under_the_grammar_mode_wire_key() {
        let mut a = args();
        a.grammar = Some("my-schema".into());
        let s = build_sampling(&a, &Config::default()).unwrap();
        assert_eq!(s.grammar, Some(GrammarMode::Named("my-schema".into())));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["grammar_mode"], serde_json::json!("my-schema"));
    }

    #[test]
    fn grammar_off_is_distinct_from_unset() {
        let mut a = args();
        a.grammar = Some("off".into());
        let s = build_sampling(&a, &Config::default()).unwrap();
        assert_eq!(s.grammar, Some(GrammarMode::Off));

        let unset = build_sampling(&args(), &Config::default()).unwrap();
        assert_eq!(unset.grammar, None);
        let v = serde_json::to_value(&unset).unwrap();
        assert!(v.get("grammar_mode").is_none());
    }

    #[test]
    fn unknown_preset_names_the_valid_ones() {
        let mut a = args();
        a.preset = Some("bogus".into());
        let e = build_sampling(&a, &Config::default())
            .unwrap_err()
            .to_string();
        assert!(e.contains("deterministic"), "{e}");
    }
}
