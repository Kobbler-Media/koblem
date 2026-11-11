use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use koblem_client::types::{GenerationListResponse, GenerationRow, GenerationTitle};

use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum GenCmd {
    /// List saved generations (newest first).
    List(ListArgs),
    /// Show one generation's metadata.
    Show { id: String },
    /// Download a generation's output file.
    Download(DownloadArgs),
    /// Delete a generation.
    Delete { id: String },
    /// Set a generation's title.
    Rename { id: String, title: String },
    /// Have the LLM write a title for a generation.
    Retitle { id: String },
    /// Delete every generation for one service.
    Clear {
        /// Service to wipe (images, matting, music, tts, stt, …).
        #[arg(long)]
        service: String,
        /// Skip the confirmation guard.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Filter by service (images, matting, music, tts, stt).
    #[arg(long)]
    pub service: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
    #[arg(long, default_value_t = 0)]
    pub offset: i64,
}

#[derive(Args, Debug)]
pub struct DownloadArgs {
    pub id: String,
    /// Which output file to fetch (0-based).
    #[arg(long, default_value_t = 0)]
    pub index: usize,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

pub async fn run(ctx: &Ctx, cmd: GenCmd) -> Result<()> {
    match cmd {
        GenCmd::List(args) => list(ctx, args).await,
        GenCmd::Show { id } => show(ctx, &id).await,
        GenCmd::Download(args) => download(ctx, args).await,
        GenCmd::Delete { id } => delete(ctx, &id).await,
        GenCmd::Rename { id, title } => rename(ctx, &id, title).await,
        GenCmd::Retitle { id } => retitle(ctx, &id).await,
        GenCmd::Clear { service, yes } => clear(ctx, &service, yes).await,
    }
}

pub fn summarize(row: &GenerationRow) -> String {
    let label = match row.title.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!("  \"{t}\""),
        None => String::new(),
    };
    format!(
        "{}  {}/{}{label}  {} file(s)  {}",
        row.id,
        row.service,
        row.endpoint,
        row.outputs.len(),
        row.created_at
    )
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<()> {
    let mut query: Vec<(&str, String)> = vec![
        ("limit", args.limit.to_string()),
        ("offset", args.offset.to_string()),
    ];
    if let Some(s) = &args.service {
        query.push(("service", s.clone()));
    }
    let resp = ctx.client.get("/api/v1/generations", &query).await?;
    let body: GenerationListResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    if body.items.is_empty() {
        println!("(no generations)");
        return Ok(());
    }
    for row in &body.items {
        println!("{}", summarize(row));
    }
    eprintln!("{} shown, {} total", body.items.len(), body.total);
    Ok(())
}

async fn show(ctx: &Ctx, id: &str) -> Result<()> {
    let resp = ctx
        .client
        .get(&format!("/api/v1/generations/{id}"), &[])
        .await?;
    let body: GenerationRow = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", summarize(&body));
    for (i, o) in body.outputs.iter().enumerate() {
        println!(
            "  [{i}] {}  {}  {} bytes  {}",
            o.filename, o.mime, o.size_bytes, o.kind
        );
    }
    println!("{}", serde_json::to_string_pretty(&body.request_meta)?);
    Ok(())
}

async fn download(ctx: &Ctx, args: DownloadArgs) -> Result<()> {
    let path = format!("/api/v1/generations/{}/file/{}", args.id, args.index);
    let stem = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("gen-{}-{}", args.id, args.index)));
    crate::download::to_file(ctx, &path, &stem, |headers| match &args.output {
        Some(p) => p.clone(),
        None => {
            let ext = headers
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| output::ext_for_mime(ct).to_string())
                .unwrap_or_else(|| "bin".to_string());
            PathBuf::from(format!("gen-{}-{}.{ext}", args.id, args.index))
        }
    })
    .await?;
    Ok(())
}

async fn delete(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.client
        .delete(&format!("/api/v1/generations/{id}"), &[])
        .await?;
    println!("deleted {id}");
    Ok(())
}

async fn rename(ctx: &Ctx, id: &str, title: String) -> Result<()> {
    let resp = ctx
        .client
        .patch_json(
            &format!("/api/v1/generations/{id}"),
            &[],
            &GenerationTitle { title },
        )
        .await?;
    let body: GenerationTitle = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.title);
    Ok(())
}

async fn retitle(ctx: &Ctx, id: &str) -> Result<()> {
    let resp = ctx
        .client
        .post_json(&format!("/api/v1/generations/{id}/retitle"), &[], &())
        .await?;
    let body: GenerationTitle = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", body.title);
    Ok(())
}

async fn clear(ctx: &Ctx, service: &str, yes: bool) -> Result<()> {
    if !yes {
        anyhow::bail!("this deletes every {service} generation and its files — pass --yes");
    }
    ctx.client
        .delete("/api/v1/generations", &[("service", service.to_string())])
        .await?;
    println!("cleared {service}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: &str = r#"{
        "id":"abc","service":"images","endpoint":"generate",
        "request_meta":{"width":512},
        "outputs":[{"filename":"a.png","mime":"image/png","size_bytes":42,"kind":"image"}],
        "title":"A cat","created_at":"2026-07-31T08:00:00Z"
    }"#;

    #[test]
    fn a_gallery_row_parses_with_its_typed_outputs() {
        let row: GenerationRow = serde_json::from_str(ROW).unwrap();
        assert_eq!(row.outputs.len(), 1);
        assert_eq!(row.outputs[0].mime, "image/png");
        assert_eq!(row.outputs[0].kind, "image");
        assert_eq!(row.request_meta["width"], 512);
        let line = summarize(&row);
        assert!(line.contains("images/generate"));
        assert!(line.contains("\"A cat\""));
        assert!(line.contains("1 file(s)"));
    }

    #[test]
    fn an_untitled_row_prints_without_an_empty_quote_pair() {
        let mut row: GenerationRow = serde_json::from_str(ROW).unwrap();
        row.title = None;
        assert!(!summarize(&row).contains('"'));
        row.title = Some(String::new());
        assert!(!summarize(&row).contains('"'));
    }

    #[test]
    fn the_list_envelope_carries_items_and_total() {
        let body: GenerationListResponse =
            serde_json::from_str(&format!(r#"{{"items":[{ROW}],"total":7}}"#)).unwrap();
        assert_eq!(body.total, 7);
        assert_eq!(body.items.len(), 1);
        let empty: GenerationListResponse = serde_json::from_str("{}").unwrap();
        assert!(empty.items.is_empty());
        assert_eq!(empty.total, 0);
    }
}
