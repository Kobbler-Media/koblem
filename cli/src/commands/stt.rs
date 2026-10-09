use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;
use koblem_client::types::{SttSegment, SttTranscribeRequest, SttTranscribeResponse};
use reqwest::multipart;

use crate::client::file_part;
use crate::config::Config;
use crate::{Ctx, output};

#[derive(Args, Debug)]
pub struct SttArgs {
    /// Audio file to transcribe.
    pub file: PathBuf,
    #[arg(long)]
    pub language: Option<String>,
    #[arg(long)]
    pub beam_size: Option<u32>,
    /// Timestamp granularity: segment (default) | word | char.
    #[arg(long)]
    pub granularity: Option<String>,
    /// Keep the uploaded audio as a testbed sample on the server.
    #[arg(long)]
    pub save_sample: bool,
    /// Print one timestamped line per row at the requested granularity.
    #[arg(long)]
    pub timestamps: bool,
}

pub const GRANULARITIES: [&str; 3] = ["segment", "word", "char"];

pub fn build_request(args: &SttArgs, cfg: &Config) -> Result<SttTranscribeRequest> {
    let timestamp_granularity = args
        .granularity
        .clone()
        .or_else(|| cfg.def_str("stt", "granularity"));
    // A typo ("words") is forwarded verbatim, comes back with an empty rows
    // array, and `rows()` then quietly shows segment rows the request never got.
    if let Some(g) = &timestamp_granularity
        && !GRANULARITIES.contains(&g.as_str())
    {
        bail!("unknown granularity '{g}' ({})", GRANULARITIES.join("|"));
    }
    Ok(SttTranscribeRequest {
        timestamp_granularity,
        beam_size: args.beam_size.or_else(|| cfg.def_u32("stt", "beam_size")),
        language: args
            .language
            .clone()
            .or_else(|| cfg.def_str("stt", "language")),
        save_sample: args.save_sample,
    })
}

/// The timed rows matching the requested granularity. `word`/`char` come back in
/// their own arrays, so asking for them and reading `segments` shows nothing.
pub fn rows<'a>(body: &'a SttTranscribeResponse, granularity: Option<&str>) -> &'a [SttSegment] {
    match granularity {
        Some("word") => &body.words,
        Some("char") => &body.chars,
        _ => &body.segments,
    }
}

fn form_fields(req: &SttTranscribeRequest, mut form: multipart::Form) -> multipart::Form {
    if let Some(g) = &req.timestamp_granularity {
        form = form.text("timestamp_granularity", g.clone());
    }
    if let Some(b) = req.beam_size {
        form = form.text("beam_size", b.to_string());
    }
    if let Some(l) = &req.language {
        form = form.text("language", l.clone());
    }
    if req.save_sample {
        form = form.text("save_sample", "true");
    }
    form
}

pub async fn run(ctx: &Ctx, args: SttArgs) -> Result<()> {
    let req = build_request(&args, &ctx.config)?;
    let form = form_fields(
        &req,
        multipart::Form::new().part("file", file_part(&args.file).await?),
    );

    let resp = ctx
        .client
        .post_multipart("/api/v1/stt/transcribe", &[], form)
        .await?;
    let body: SttTranscribeResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }

    let granularity = req.timestamp_granularity.as_deref();
    let rows = rows(&body, granularity);
    if args.timestamps && !rows.is_empty() {
        for r in rows {
            println!("[{:>8.2} → {:>8.2}]  {}", r.start, r.end, r.text);
        }
    } else {
        println!("{}", body.text);
    }

    if let Some(l) = &body.language {
        eprintln!("language: {l}");
    }
    if !rows.is_empty() {
        eprintln!("{} {} row(s)", rows.len(), granularity.unwrap_or("segment"));
    }
    if body.preempted {
        eprintln!("warning: transcription was preempted mid-run — the text may be truncated");
    }
    if let Some(ms) = body.duration_ms {
        eprintln!("{ms}ms");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(granularity: Option<&str>) -> SttArgs {
        SttArgs {
            file: PathBuf::from("a.wav"),
            language: None,
            beam_size: None,
            granularity: granularity.map(str::to_string),
            save_sample: false,
            timestamps: false,
        }
    }

    fn seg(text: &str) -> SttSegment {
        SttSegment {
            start: 0.0,
            end: 1.0,
            text: text.into(),
            id: None,
        }
    }

    fn body() -> SttTranscribeResponse {
        SttTranscribeResponse {
            text: "hello world".into(),
            segments: vec![seg("hello world")],
            words: vec![seg("hello"), seg("world")],
            chars: vec![seg("h"), seg("e")],
            language: Some("en".into()),
            preempted: false,
            duration_ms: Some(120),
            gate_wait_ms: None,
            gpu_log_id: None,
        }
    }

    #[test]
    fn word_granularity_reads_the_words_array_not_segments() {
        assert_eq!(rows(&body(), Some("word")).len(), 2);
        assert_eq!(rows(&body(), Some("char")).len(), 2);
        assert_eq!(rows(&body(), Some("segment")).len(), 1);
        assert_eq!(rows(&body(), None).len(), 1);
    }

    #[test]
    fn every_stt_form_field_is_reachable() {
        let mut a = args(Some("word"));
        a.beam_size = Some(8);
        a.language = Some("de".into());
        a.save_sample = true;
        let req = build_request(&a, &Config::default()).unwrap();
        assert_eq!(req.timestamp_granularity.as_deref(), Some("word"));
        assert_eq!(req.beam_size, Some(8));
        assert_eq!(req.language.as_deref(), Some("de"));
        assert!(req.save_sample, "save_sample must be reachable from a flag");
    }

    /// A typo'd granularity used to ride through and come back as segment rows
    /// for a request that returned none.
    #[test]
    fn only_the_three_real_granularities_are_accepted() {
        for good in GRANULARITIES {
            assert!(
                build_request(&args(Some(good)), &Config::default()).is_ok(),
                "{good} must be accepted"
            );
        }
        assert!(build_request(&args(None), &Config::default()).is_ok());
        for bad in ["words", "chars", "Word", "sentence", ""] {
            let e = build_request(&args(Some(bad)), &Config::default())
                .unwrap_err()
                .to_string();
            assert!(e.contains("segment|word|char"), "{bad}: {e}");
        }
        // Config defaults get the same check — they reach the wire identically.
        let cfg = Config::with_defaults("[stt]\ngranularity = \"words\"");
        assert!(build_request(&args(None), &cfg).is_err());
    }

    #[test]
    fn the_response_keeps_the_fields_the_old_value_probe_dropped() {
        let b = body();
        assert!(!b.segments.is_empty());
        assert!(!b.words.is_empty());
        assert!(!b.chars.is_empty());
        assert_eq!(b.language.as_deref(), Some("en"));
        assert!(!b.preempted);
        assert_eq!(b.duration_ms, Some(120));
    }
}
