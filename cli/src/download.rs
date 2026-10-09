//! Streaming, resumable artifact downloads.
//!
//! A render is tens of megabytes and takes minutes; buffering the whole body
//! before the first byte hits disk gives no progress and throws the work away on
//! a dropped connection. Bytes land in a `.part` file — whose name never depends
//! on the response, so a resume can find it before the request is made — and the
//! part is renamed to its final name once the content type is known.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::header::HeaderMap;
use tokio::io::AsyncWriteExt;

use crate::Ctx;

/// Sibling `.part` path for a download target. Mime-independent by construction:
/// the extension is only known once the response arrives, but the resume offset
/// has to be known before it is sent.
pub fn part_path(stem: &Path) -> PathBuf {
    let name = stem
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    stem.with_file_name(format!("{name}.part"))
}

/// Stream `url` into a `.part` file beside `stem`, resuming an interrupted
/// download, then rename it to whatever `name` derives from the response
/// headers. Returns the final path.
pub async fn to_file<F>(ctx: &Ctx, url: &str, stem: &Path, name: F) -> Result<PathBuf>
where
    F: FnOnce(&HeaderMap) -> PathBuf,
{
    let part = part_path(stem);
    if let Some(dir) = part.parent().filter(|d| !d.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }

    let mut have = tokio::fs::metadata(&part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut resp = ctx.client.get_range(url, &[], have).await?;
    // 416 means the part is at least as long as the resource — it is stale or
    // from a different artifact, so start over rather than trust it.
    if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        have = 0;
        resp = ctx.client.get_range(url, &[], 0).await?;
    }
    let resumed = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT && have > 0;
    if !resumed {
        have = 0;
    }

    let target = name(resp.headers());
    let total = resp.content_length().map(|n| n + have);
    let mut progress = Progress::new(&target, have, total, resumed);

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&part)
        .await
        .with_context(|| format!("opening {}", part.display()))?;

    let mut written = have;
    while let Some(chunk) = resp.chunk().await.context("reading response body")? {
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", part.display()))?;
        written += chunk.len() as u64;
        progress.tick(written);
    }
    file.flush().await.context("flushing download")?;
    drop(file);
    progress.finish();

    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    tokio::fs::rename(&part, &target)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), target.display()))?;
    eprintln!("wrote {} ({written} bytes)", target.display());
    Ok(target)
}

/// [`to_file`] for a target whose name is already known.
pub async fn to_named(ctx: &Ctx, url: &str, target: &Path) -> Result<PathBuf> {
    to_file(ctx, url, target, |_| target.to_path_buf()).await
}

/// In-place transfer readout: bytes, percentage when the length is known, and
/// throughput. Repaints at most every [`REPAINT`]; on a non-tty it says nothing
/// until the final `wrote …` line, so piped output stays clean.
struct Progress {
    label: String,
    tty: bool,
    start: Instant,
    from: u64,
    total: Option<u64>,
    last: Instant,
    painted: usize,
}

const REPAINT: Duration = Duration::from_millis(200);

impl Progress {
    fn new(target: &Path, from: u64, total: Option<u64>, resumed: bool) -> Self {
        let label = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".to_string());
        if resumed {
            eprintln!("resuming {label} at {}", human(from));
        }
        Self {
            label,
            tty: std::io::stderr().is_terminal(),
            start: Instant::now(),
            from,
            total,
            last: Instant::now() - REPAINT,
            painted: 0,
        }
    }

    fn tick(&mut self, written: u64) {
        if !self.tty || self.last.elapsed() < REPAINT {
            return;
        }
        self.last = Instant::now();
        let line = self.line(written);
        let pad = self.painted.saturating_sub(line.chars().count());
        eprint!("\r{line}{}", " ".repeat(pad));
        let _ = std::io::stderr().flush();
        self.painted = line.chars().count();
    }

    fn line(&self, written: u64) -> String {
        let secs = self.start.elapsed().as_secs_f64().max(0.001);
        let rate = (written - self.from) as f64 / secs;
        match self.total {
            Some(total) if total > 0 => format!(
                "{} {}/{} ({:.0}%) {}/s",
                self.label,
                human(written),
                human(total),
                written as f64 / total as f64 * 100.0,
                human(rate as u64),
            ),
            _ => format!("{} {} {}/s", self.label, human(written), human(rate as u64)),
        }
    }

    fn finish(&mut self) {
        if self.tty && self.painted > 0 {
            eprintln!();
            self.painted = 0;
        }
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resume offset has to be readable before the request goes out, so the
    /// part name must not depend on the response's content type.
    #[test]
    fn the_part_name_is_independent_of_the_extension() {
        assert_eq!(
            part_path(Path::new("ltx-j1.webm")),
            PathBuf::from("ltx-j1.part")
        );
        assert_eq!(
            part_path(Path::new("ltx-j1.mp4")),
            PathBuf::from("ltx-j1.part")
        );
        assert_eq!(
            part_path(Path::new("out/clip.webm")),
            PathBuf::from("out/clip.part")
        );
        assert_eq!(part_path(Path::new("noext")), PathBuf::from("noext.part"));
    }

    #[test]
    fn sizes_read_as_units() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(53 * 1024 * 1024), "53.0 MB");
    }

    #[test]
    fn the_readout_shows_a_percentage_only_when_the_length_is_known() {
        let mut p = Progress::new(Path::new("clip.webm"), 0, Some(1000), false);
        p.tty = true;
        assert!(p.line(250).contains("25%"));
        assert!(p.line(250).contains("clip.webm"));
        let q = Progress::new(Path::new("clip.webm"), 0, None, false);
        assert!(!q.line(250).contains('%'));
    }
}
