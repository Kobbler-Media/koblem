use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use koblem_client::types::{VisionClassifyRequest, VisionClassifyResponse};
use reqwest::multipart;

use crate::client::file_part;
use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum VisionCmd {
    /// Score image(s) against text phrases (SigLIP zero-shot).
    Classify(ClassifyArgs),
}

#[derive(Args, Debug)]
pub struct ClassifyArgs {
    /// Image file(s) to score.
    #[arg(required = true)]
    pub images: Vec<PathBuf>,
    /// Phrase to score the image against. Repeatable.
    #[arg(long = "prompt", required = true)]
    pub prompts: Vec<String>,
    #[arg(long)]
    pub max_num_patches: Option<u32>,
    /// Also return raw logits alongside scores (implies --json to see them).
    #[arg(long)]
    pub return_logits: bool,
    /// Keep the uploaded images as testbed samples on the server.
    #[arg(long)]
    pub save_sample: bool,
}

pub fn build_request(args: &ClassifyArgs) -> VisionClassifyRequest {
    VisionClassifyRequest {
        prompts: args.prompts.clone(),
        max_num_patches: args.max_num_patches,
        return_logits: args.return_logits,
        save_sample: args.save_sample,
    }
}

pub async fn run(ctx: &Ctx, cmd: VisionCmd) -> Result<()> {
    match cmd {
        VisionCmd::Classify(args) => classify(ctx, args).await,
    }
}

async fn classify(ctx: &Ctx, args: ClassifyArgs) -> Result<()> {
    let req = build_request(&args);
    let mut form = multipart::Form::new();
    for p in &args.images {
        form = form.part("images", file_part(p).await?);
    }
    for prompt in &req.prompts {
        form = form.text("prompts", prompt.clone());
    }
    if let Some(n) = req.max_num_patches {
        form = form.text("max_num_patches", n.to_string());
    }
    if req.return_logits {
        form = form.text("return_logits", "true");
    }
    if req.save_sample {
        form = form.text("save_sample", "true");
    }

    let resp = ctx
        .client
        .post_multipart("/api/v1/vision/classify", &[], form)
        .await?;
    let body: VisionClassifyResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }

    // `scores` is image-major: scores[image][prompt].
    for (i, row) in body.scores.iter().enumerate() {
        if body.scores.len() > 1 {
            println!("image {}:", i + 1);
        }
        for (prompt, v) in req.prompts.iter().zip(row) {
            println!("  {v:>6.3}  {prompt}");
        }
    }
    if let Some(ms) = body.duration_ms {
        eprintln!("{ms}ms");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_classify_field_is_reachable() {
        let args = ClassifyArgs {
            images: vec![PathBuf::from("a.png")],
            prompts: vec!["a cat".into(), "a dog".into()],
            max_num_patches: Some(256),
            return_logits: true,
            save_sample: true,
        };
        let req = build_request(&args);
        assert_eq!(req.prompts.len(), 2);
        assert_eq!(req.max_num_patches, Some(256));
        assert!(req.return_logits);
        assert!(req.save_sample, "save_sample must be reachable from a flag");
    }

    #[test]
    fn scores_parse_image_major_with_optional_logits() {
        let body: VisionClassifyResponse = serde_json::from_str(
            r#"{"scores":[[0.9,0.1],[0.2,0.8]],"logits":[[3.0,-1.0],[-2.0,4.0]],"duration_ms":42}"#,
        )
        .unwrap();
        assert_eq!(body.scores.len(), 2);
        assert_eq!(body.scores[0].len(), 2);
        assert_eq!(body.logits.as_ref().unwrap()[1][1], 4.0);
        assert_eq!(body.duration_ms, Some(42));

        let bare: VisionClassifyResponse = serde_json::from_str(r#"{"scores":[[1.0]]}"#).unwrap();
        assert!(bare.logits.is_none());
    }
}
