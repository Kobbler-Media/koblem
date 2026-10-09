use anyhow::{Context, Result};
use clap::Args;

/// The server-wide "improve this draft in place" pair (`routes/refine.rs`).
/// Every creative-draft endpoint reads the same two fields, so the CLI declares
/// them once and flattens them into each drafting subcommand.
#[derive(Args, Debug, Default, Clone)]
pub struct RefineArgs {
    /// Existing draft to improve in place. `@PATH` reads the draft from a file,
    /// `@-` from stdin.
    #[arg(long, value_name = "TEXT|@FILE")]
    pub refine_source: Option<String>,
    /// Change to apply to --refine-source. Omit for a general polish.
    #[arg(long, value_name = "TEXT", requires = "refine_source")]
    pub refine_instruction: Option<String>,
}

impl RefineArgs {
    pub async fn resolve(&self) -> Result<(Option<String>, Option<String>)> {
        let source = match &self.refine_source {
            Some(s) => Some(read_arg(s).await?),
            None => None,
        };
        Ok((source, self.refine_instruction.clone()))
    }
}

/// Read a text argument, honouring the `@PATH` / `@-` file conventions.
/// Multi-line drafts (lyrics, prompts) are painful to pass inline.
pub async fn read_arg(value: &str) -> Result<String> {
    let Some(path) = value.strip_prefix('@') else {
        return Ok(value.to_string());
    };
    if path == "-" {
        let mut buf = String::new();
        use tokio::io::AsyncReadExt as _;
        tokio::io::stdin()
            .read_to_string(&mut buf)
            .await
            .context("reading stdin")?;
        return Ok(buf);
    }
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {path}"))
}

/// [`read_arg`] for an optional flag. Every long-text flag must route through
/// this — a raw `.clone()` makes the engine render the literal `@lyrics.txt`.
pub async fn read_opt(value: &Option<String>) -> Result<Option<String>> {
    match value {
        Some(v) => Ok(Some(read_arg(v).await?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn plain_text_passes_through() {
        assert_eq!(read_arg("hello world").await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn at_prefix_reads_a_file() {
        let dir = std::env::temp_dir().join("kob-refine-test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("draft.txt");
        tokio::fs::write(&path, "[Verse]\nline one\n")
            .await
            .unwrap();
        let got = read_arg(&format!("@{}", path.display())).await.unwrap();
        assert_eq!(got, "[Verse]\nline one\n");
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn resolve_returns_both_halves() {
        let args = RefineArgs {
            refine_source: Some("old draft".into()),
            refine_instruction: Some("punchier chorus".into()),
        };
        let (src, instr) = args.resolve().await.unwrap();
        assert_eq!(src.as_deref(), Some("old draft"));
        assert_eq!(instr.as_deref(), Some("punchier chorus"));
    }

    #[tokio::test]
    async fn absent_refine_stays_absent() {
        let (src, instr) = RefineArgs::default().resolve().await.unwrap();
        assert!(src.is_none());
        assert!(instr.is_none());
    }

    #[tokio::test]
    async fn read_opt_resolves_the_at_convention_and_passes_none_through() {
        assert_eq!(read_opt(&None).await.unwrap(), None);
        assert_eq!(
            read_opt(&Some("inline".into())).await.unwrap().as_deref(),
            Some("inline")
        );
        let path = std::env::temp_dir().join("kob-read-opt.txt");
        tokio::fs::write(&path, "[Chorus]\nfrom a file")
            .await
            .unwrap();
        let got = read_opt(&Some(format!("@{}", path.display())))
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("[Chorus]\nfrom a file"));
        tokio::fs::remove_file(&path).await.unwrap();
    }
}
