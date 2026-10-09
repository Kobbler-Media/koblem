use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use koblem_client::types::MusicTrack;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

pub fn mime_for_path(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "webm" => "video/webm",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
    .to_string()
}

pub fn ext_for_mime(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or(mime).trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/aac" => "aac",
        "audio/flac" => "flac",
        "audio/mp4" | "audio/m4a" => "m4a",
        "video/webm" => "webm",
        "video/mp4" => "mp4",
        _ => "bin",
    }
}

pub async fn read_base64(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(B64.encode(bytes))
}

pub async fn read_data_uri(path: &Path) -> Result<String> {
    let mime = mime_for_path(path);
    let b64 = read_base64(path).await?;
    Ok(format!("data:{mime};base64,{b64}"))
}

/// Write bytes to `path`, creating parent dirs, and report to stderr.
pub async fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    tokio::fs::write(path, bytes)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    eprintln!("wrote {} ({} bytes)", path.display(), bytes.len());
    Ok(())
}

/// Pick the output path for a single artifact: the `-o` override if given, else
/// `<stem>.<ext>` in the current directory.
pub fn single_target(stem: &str, ext: &str, o: &Option<PathBuf>) -> PathBuf {
    o.clone()
        .unwrap_or_else(|| PathBuf::from(format!("{stem}.{ext}")))
}

/// Insert a `-N` suffix before the extension: `foo.png` → `foo-2.png`.
pub fn indexed(path: &Path, idx: usize) -> PathBuf {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let name = if ext.is_empty() {
        format!("{stem}-{idx}")
    } else {
        format!("{stem}-{idx}.{ext}")
    };
    path.with_file_name(name)
}

/// Where each server-named artifact lands. `-o` renames the file only when the
/// server returned exactly one; a batch keeps the server filenames.
pub fn named_targets(names: &[String], o: &Option<PathBuf>) -> Vec<PathBuf> {
    match (names, o) {
        ([_], Some(p)) => vec![p.clone()],
        _ => names.iter().map(PathBuf::from).collect(),
    }
}

/// Say so rather than silently dropping `-o` when the server returned a batch.
pub fn note_output_ignored(names: &[String], o: &Option<PathBuf>) {
    if names.len() > 1 && o.is_some() {
        eprintln!(
            "note: -o ignored for {} tracks; using the server filenames",
            names.len()
        );
    }
}

/// Download a server-hosted track list (music / songgen / sa3 all return the
/// same `MusicTrack` shape) to disk.
pub async fn write_tracks(
    ctx: &crate::Ctx,
    tracks: &[MusicTrack],
    o: &Option<PathBuf>,
) -> Result<()> {
    let names: Vec<String> = tracks.iter().map(|t| t.filename.clone()).collect();
    note_output_ignored(&names, o);
    for (track, target) in tracks.iter().zip(named_targets(&names, o)) {
        crate::download::to_named(ctx, &track.url, &target).await?;
    }
    Ok(())
}

/// Sibling path with a `-suffix` stem: `foo.png` → `foo-cutout.png`.
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("png");
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    path.with_file_name(format!("{stem}-{suffix}.{ext}"))
}

pub fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_track_honours_o_and_a_batch_keeps_the_server_names() {
        let o = Some(PathBuf::from("out/mine.mp3"));
        assert_eq!(
            named_targets(&["srv-a.mp3".to_string()], &o),
            vec![PathBuf::from("out/mine.mp3")]
        );
        // A batch cannot honour a single -o without overwriting itself.
        let batch = ["srv-a.mp3".to_string(), "srv-b.mp3".to_string()];
        assert_eq!(
            named_targets(&batch, &o),
            vec![PathBuf::from("srv-a.mp3"), PathBuf::from("srv-b.mp3")]
        );
        assert_eq!(
            named_targets(&batch, &None),
            vec![PathBuf::from("srv-a.mp3"), PathBuf::from("srv-b.mp3")]
        );
        assert_eq!(
            named_targets(&["srv-a.mp3".to_string()], &None),
            vec![PathBuf::from("srv-a.mp3")]
        );
    }

    #[test]
    fn indexed_and_sibling_keep_the_directory() {
        assert_eq!(
            indexed(Path::new("out/a.png"), 2),
            PathBuf::from("out/a-2.png")
        );
        assert_eq!(
            sibling(Path::new("out/a.png"), "cutout"),
            PathBuf::from("out/a-cutout.png")
        );
    }
}
