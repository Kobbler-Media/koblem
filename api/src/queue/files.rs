//! The queue's on-disk working files, keyed by queue job id.
//!
//! 🔴 Nothing under these roots is a deliverable. A finished asset is hard-linked into the gallery
//! and lives until the user deletes it; what is here is scaffolding for one run — segments waiting
//! to be joined, a clip being streamed out of an engine, a source captured at enqueue — and
//! `jobs::gpu_retention` sweeps it once no live row needs it. The per-engine stores this replaced
//! expired the OUTPUT on a TTL, which is the one thing a sweep must never touch.
//!
//! 🔴 The capture root is configured separately and defaults INSIDE the shared upscale staging
//! mount. `queue::adapters::upscale` links the capture into that mount at dispatch, and
//! `link_or_copy` falls back to a copy across filesystems — putting captures on another device
//! turns every dispatch into a whole-video copy. It is still its own subdirectory so the sweep can
//! only ever see captures, never a source another run has staged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct QueueFiles {
    work: Arc<PathBuf>,
    clips: Arc<PathBuf>,
    captures: Arc<PathBuf>,
}

impl QueueFiles {
    pub fn open(root: &Path, captures: &Path) -> Self {
        let files = QueueFiles {
            work: Arc::new(root.join("work")),
            clips: Arc::new(root.join("clips")),
            captures: Arc::new(captures.to_path_buf()),
        };
        for dir in [files.work_root(), files.clips_root(), files.captures_root()] {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(path = %dir.display(), error = %e,
                               "queue files: directory is not writable");
            }
        }
        files
    }

    pub fn work_dir(&self, job_id: &str) -> PathBuf {
        self.work.join(sanitize(job_id, "job"))
    }

    pub fn clip_path(&self, job_id: &str) -> PathBuf {
        self.clips.join(format!("{}.webm", sanitize(job_id, "job")))
    }

    pub fn capture_path(&self, token: &str, ext: &str) -> PathBuf {
        self.captures.join(format!(
            "{}.{}",
            sanitize(token, "capture"),
            sanitize(&ext.to_ascii_lowercase(), "mp4")
        ))
    }

    /// The capture a token names, whatever extension it was uploaded with.
    pub async fn find_capture(&self, token: &str) -> Option<PathBuf> {
        let prefix = format!("{}.", sanitize(token, "capture"));
        let mut rd = tokio::fs::read_dir(self.captures_root()).await.ok()?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                return Some(entry.path());
            }
        }
        None
    }

    pub fn work_root(&self) -> &Path {
        &self.work
    }

    pub fn clips_root(&self) -> &Path {
        &self.clips
    }

    pub fn captures_root(&self) -> &Path {
        &self.captures
    }
}

/// A job id off the queue is a uuid and a token is minted here, but neither is proof: a
/// hand-crafted one reaching any of these constructors must not be able to name a path outside its
/// root.
fn sanitize(name: &str, fallback: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    if kept.is_empty() {
        fallback.to_string()
    } else {
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(tag: &str) -> QueueFiles {
        let root =
            std::env::temp_dir().join(format!("koblem-queue-files-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        QueueFiles::open(&root, &root.join("captures"))
    }

    /// A traversal in any of the three keys resolves inside its own root.
    #[test]
    fn no_key_can_escape_its_root() {
        let f = files("traversal");
        assert!(f.work_dir("../../etc").starts_with(f.work_root()));
        assert!(f.clip_path("../../etc").starts_with(f.clips_root()));
        assert!(
            f.capture_path("../../etc", "../passwd")
                .starts_with(f.captures_root())
        );
        assert_eq!(
            f.capture_path("up_a1", "MP4").file_name().unwrap(),
            "up_a1.mp4"
        );
        let _ = std::fs::remove_dir_all(f.work_root().parent().unwrap());
    }

    /// 🔴 The three roots are disjoint. The sweep deletes by root, so a clip landing under the
    /// capture root — or a capture under `work` — would be swept against the wrong liveness rule.
    #[test]
    fn the_three_roots_do_not_overlap() {
        let f = files("roots");
        let work = f.work_dir("a");
        let clip = f.clip_path("a");
        let capture = f.capture_path("a", "mp4");
        assert!(!clip.starts_with(f.work_root()));
        assert!(!capture.starts_with(f.work_root()));
        assert!(!capture.starts_with(f.clips_root()));
        assert_ne!(work, clip);
        let _ = std::fs::remove_dir_all(f.work_root().parent().unwrap());
    }
}
