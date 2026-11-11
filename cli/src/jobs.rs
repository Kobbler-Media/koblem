use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use koblem_client::types::{
    VideoJobListResponse, VideoJobState, VideoJobStatus, VideoJobSubmitResponse,
};

use crate::{Ctx, download, output};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Engine {
    pub name: &'static str,
    pub base: &'static str,
}

pub const LTX_VIDEO: Engine = Engine {
    name: "ltx",
    base: "/api/v1/ltx-video",
};

impl Engine {
    pub fn parse(s: &str) -> Result<Engine> {
        match s {
            "ltx" | "ltx-video" | "ltx25" | "ltx-2.5" => Ok(LTX_VIDEO),
            _ => bail!("unknown engine '{s}' (ltx)"),
        }
    }

    pub fn jobs_path(&self) -> String {
        format!("{}/jobs", self.base)
    }
    pub fn job_path(&self, id: &str) -> String {
        format!("{}/jobs/{id}", self.base)
    }
    pub fn media_path(&self, id: &str) -> String {
        format!("{}/jobs/{id}/media", self.base)
    }
    pub fn cancel_path(&self) -> String {
        format!("{}/cancel", self.base)
    }
    pub fn props_path(&self) -> String {
        format!("{}/props", self.base)
    }
}

pub struct PollOpts {
    pub interval: Duration,
    /// Download each progressively-delivered shot as it lands (LTX only).
    pub save_partials: bool,
}

impl Default for PollOpts {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            save_partials: false,
        }
    }
}

async fn submitted_job(resp: reqwest::Response) -> Result<VideoJobSubmitResponse> {
    let status = resp.status();
    let body = resp.text().await.context("reading submit response")?;
    serde_json::from_str(&body).with_context(|| {
        format!(
            "expected a {{job_id}} submit response ({status}), got: {}",
            body.trim()
        )
    })
}

pub async fn job_status(ctx: &Ctx, engine: Engine, id: &str) -> Result<VideoJobStatus> {
    let resp = ctx.client.get(&engine.job_path(id), &[]).await?;
    resp.json().await.context("parsing job status")
}

pub async fn list_jobs(ctx: &Ctx, engine: Engine) -> Result<Vec<VideoJobStatus>> {
    let resp = ctx.client.get(&engine.jobs_path(), &[]).await?;
    let body: VideoJobListResponse = resp.json().await.context("parsing job list")?;
    Ok(body.jobs)
}

/// Consecutive failed polls tolerated before giving up on a running render.
const POLL_RETRIES: u32 = 5;

/// A dropped connection or a koblem restart must not kill the client: the job
/// store is disk-backed, re-attaches on boot, and holds the clip for an hour.
#[derive(Default)]
struct PollRetry(u32);

impl PollRetry {
    fn reached(&mut self) {
        self.0 = 0;
    }

    /// `Some` once `POLL_RETRIES` polls in a row have failed — give up, naming
    /// the command that re-attaches to the still-running render.
    fn lost(&mut self, e: anyhow::Error, engine: Engine, id: &str) -> Option<anyhow::Error> {
        self.0 += 1;
        if self.0 < POLL_RETRIES {
            eprintln!("poll {}/{POLL_RETRIES} failed: {e:#} — retrying", self.0);
            return None;
        }
        Some(e.context(reattach_hint(engine, id)))
    }
}

fn reattach_hint(engine: Engine, id: &str) -> String {
    format!(
        "lost contact with koblem — the render keeps going; re-attach with \
         `kob video jobs --engine {} --id {id}`",
        engine.name
    )
}

/// Where a progressively-delivered shot lands. `-o` names the whole clip, so its
/// stem + directory carry over to the per-shot files.
fn partial_target(engine: Engine, id: &str, segment: u32, o: &Option<PathBuf>) -> PathBuf {
    match o {
        Some(p) => {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
            p.with_file_name(format!("{stem}-shot{segment}.webm"))
        }
        None => PathBuf::from(format!("{}-{id}-shot{segment}.webm", engine.name)),
    }
}

/// Where the finished clip lands. Without `-o` the name carries the engine and
/// job id, so back-to-back downloads don't overwrite each other.
fn media_target(engine: Engine, id: &str, ext: &str, o: &Option<PathBuf>) -> PathBuf {
    output::single_target(&format!("{}-{id}", engine.name), ext, o)
}

/// The same location minus the extension, which only the response knows. The
/// `.part` file a resume looks for is derived from this.
fn media_stem(engine: Engine, id: &str, o: &Option<PathBuf>) -> PathBuf {
    o.clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}-{id}", engine.name)))
}

/// Fetch every shot not yet on disk. A shot that isn't fetchable stays unsaved
/// and is retried on the next poll rather than being skipped for good.
async fn save_fresh_partials(
    ctx: &Ctx,
    engine: Engine,
    id: &str,
    status: &VideoJobStatus,
    saved: &mut HashSet<u32>,
    progress: &mut Progress,
    o: &Option<PathBuf>,
) {
    for p in &status.partials {
        if saved.contains(&p.segment_index) {
            continue;
        }
        progress.suspend();
        let target = partial_target(engine, id, p.segment_index, o);
        match download::to_named(ctx, &p.url, &target).await {
            Ok(_) => {
                saved.insert(p.segment_index);
            }
            Err(e) => eprintln!(
                "shot {} not fetchable yet ({e:#}) — retrying next poll",
                p.segment_index
            ),
        }
    }
}

/// Poll a submitted render to a terminal state, reporting state transitions and
/// per-shot partials on stderr. Renders run for minutes to tens of minutes, so
/// there is no wall-clock deadline here — the client sets no request timeout
/// either.
pub async fn wait(
    ctx: &Ctx,
    engine: Engine,
    id: &str,
    opts: &PollOpts,
    o: &Option<PathBuf>,
) -> Result<VideoJobStatus> {
    let mut progress = Progress::new(engine.name);
    let mut retry = PollRetry::default();
    let mut saved: HashSet<u32> = HashSet::new();
    loop {
        let status = match job_status(ctx, engine, id).await {
            Ok(s) => {
                retry.reached();
                s
            }
            Err(e) => {
                progress.suspend();
                if let Some(fatal) = retry.lost(e, engine, id) {
                    return Err(fatal);
                }
                tokio::time::sleep(opts.interval).await;
                continue;
            }
        };
        progress.update(&status);
        if opts.save_partials {
            save_fresh_partials(ctx, engine, id, &status, &mut saved, &mut progress, o).await;
        }
        if status.status.is_terminal() {
            progress.finish();
            for p in &status.partials {
                if !saved.contains(&p.segment_index) && opts.save_partials {
                    eprintln!("shot {} was never fetchable", p.segment_index);
                }
            }
            return match status.status {
                VideoJobState::Completed => Ok(status),
                VideoJobState::Failed => {
                    let msg = status.error.as_deref().unwrap_or("render failed");
                    match &status.resume_job_id {
                        Some(r) if status.resumable => {
                            bail!("{msg}\nresume with --resume-job-id {r}")
                        }
                        _ => bail!("{msg}"),
                    }
                }
                _ => bail!("render cancelled"),
            };
        }
        tokio::time::sleep(opts.interval).await;
    }
}

/// Say so when the bytes just written are a FRAGMENT of the timeline rather than the timeline.
///
/// 🔴 Reachable from `kob video jobs --id`, which re-attaches to ANY job in koblem's shared store —
/// including a Director H3 generation retake or a resumed LTX-2.5 render, both of which finish
/// `completed` with a whole segment count and hand back one unit's `/media`. Nothing else in the
/// output distinguishes that file from the render it looks like.
pub fn warn_if_partial(status: &VideoJobStatus) {
    if status.job_is_partial {
        eprintln!(
            "warning: the engine reports this render as PARTIAL — the file above is a fragment of \
             the timeline (one render unit, or a resumed sub-range), not the whole cut"
        );
    }
}

pub async fn download_media(
    ctx: &Ctx,
    engine: Engine,
    id: &str,
    o: &Option<PathBuf>,
) -> Result<PathBuf> {
    let stem = media_stem(engine, id, o);
    download::to_file(ctx, &engine.media_path(id), &stem, |headers| {
        let ext = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| output::ext_for_mime(ct).to_string())
            .filter(|e| e != "bin")
            .unwrap_or_else(|| "webm".to_string());
        media_target(engine, id, &ext, o)
    })
    .await
}

/// Submit → poll → download, the whole async-job dance every video engine shares.
pub async fn run_to_completion(
    ctx: &Ctx,
    engine: Engine,
    resp: reqwest::Response,
    opts: &PollOpts,
    detach: bool,
    o: &Option<PathBuf>,
) -> Result<()> {
    let submitted = submitted_job(resp).await?;
    let id = submitted.job_id;
    if detach {
        println!("{id}");
        eprintln!(
            "detached — poll with `kob video jobs --engine {} --id {id}`",
            engine.name
        );
        return Ok(());
    }
    eprintln!("job {id} submitted");
    let status = wait(ctx, engine, &id, opts, o).await?;
    if ctx.json {
        return output::print_json(&status);
    }
    download_media(ctx, engine, &id, o).await?;
    warn_if_partial(&status);
    eprintln!(
        "segments={} gate={}ms render={}ms",
        status.segments, status.gate_wait_ms, status.render_ms
    );
    Ok(())
}

pub fn summarize(s: &VideoJobStatus) -> String {
    let mut line = format!("{}  {:<9}  {} seg", s.job_id, s.status.as_str(), s.segments);
    if s.render_ms > 0 {
        line.push_str(&format!("  {:.1}s", s.render_ms as f64 / 1000.0));
    }
    if !s.partials.is_empty() {
        line.push_str(&format!("  {} shot(s) ready", s.partials.len()));
    }
    // What `/media` on this job IS. A Director retake and a resumed 2.5 render both deliver a
    // fragment of the timeline under a `completed` status, and a listing that omits it invites
    // downloading one and keeping it as the render.
    if s.job_is_partial {
        line.push_str("  partial");
    }
    if s.resumable
        && let Some(r) = &s.resume_job_id
    {
        line.push_str(&format!("  resume={r}"));
    }
    if let Some(e) = &s.error {
        line.push_str(&format!("  error: {e}"));
    }
    line
}

/// Progress readout for a job with no numeric progress: state transitions,
/// elapsed wall time, and the count of shots delivered so far.
struct Progress {
    engine: &'static str,
    tty: bool,
    painted: usize,
    state: Option<VideoJobState>,
    partials: usize,
    start: Instant,
}

impl Progress {
    fn new(engine: &'static str) -> Self {
        Self {
            engine,
            tty: std::io::stderr().is_terminal(),
            painted: 0,
            state: None,
            partials: 0,
            start: Instant::now(),
        }
    }

    fn update(&mut self, s: &VideoJobStatus) {
        let changed = self.state != Some(s.status) || self.partials != s.partials.len();
        self.state = Some(s.status);
        self.partials = s.partials.len();
        let line = self.line(s);
        if self.tty {
            self.paint(&line);
        } else if changed {
            eprintln!("{line}");
        }
    }

    fn line(&self, s: &VideoJobStatus) -> String {
        let secs = self.start.elapsed().as_secs();
        let mut line = format!(
            "[{}] {}  {}:{:02}",
            self.engine,
            s.status.as_str(),
            secs / 60,
            secs % 60
        );
        if s.gate_wait_ms > 0 {
            line.push_str(&format!("  gate {:.1}s", s.gate_wait_ms as f64 / 1000.0));
        }
        if s.segments > 1 {
            line.push_str(&format!("  shots {}/{}", self.partials, s.segments));
        }
        line
    }

    fn paint(&mut self, line: &str) {
        let pad = self.painted.saturating_sub(line.chars().count());
        eprint!("\r{line}{}", " ".repeat(pad));
        let _ = std::io::stderr().flush();
        self.painted = line.chars().count();
    }

    /// Drop the in-place status line so another writer can print cleanly.
    fn suspend(&mut self) {
        if self.tty && self.painted > 0 {
            eprintln!();
            self.painted = 0;
        }
    }

    fn finish(&mut self) {
        self.suspend();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koblem_client::types::VideoJobPartial;

    fn status(state: VideoJobState, partials: usize) -> VideoJobStatus {
        VideoJobStatus {
            job_id: "j1".into(),
            status: state,
            created_at: None,
            server_job_id: Some("srv-1".into()),
            resume_job_id: Some("srv-1".into()),
            resumable: state == VideoJobState::Failed,
            segments: 6,
            scripts: Vec::new(),
            gate_wait_ms: 400,
            render_ms: 12_000,
            error: None,
            result_url: None,
            partials: (0..partials)
                .map(|i| VideoJobPartial {
                    segment_index: i as u32,
                    url: format!("/api/v1/ltx-video/jobs/j1/segments/{i}"),
                })
                .collect(),
            job_is_partial: false,
            total_eta_seconds: None,
            progress: None,
            current_step: None,
            total_steps: None,
            phase: None,
            preview_seq: None,
            generation_index: None,
            generations_done: None,
        }
    }

    #[test]
    fn engine_aliases_resolve() {
        assert_eq!(Engine::parse("ltx").unwrap(), LTX_VIDEO);
        assert_eq!(Engine::parse("ltx-video").unwrap(), LTX_VIDEO);
        assert!(Engine::parse("wan").is_err());
        assert!(Engine::parse("sora").is_err());
    }

    #[test]
    fn engine_paths_match_the_server_routes() {
        assert_eq!(LTX_VIDEO.job_path("x"), "/api/v1/ltx-video/jobs/x");
        assert_eq!(LTX_VIDEO.media_path("x"), "/api/v1/ltx-video/jobs/x/media");
        assert_eq!(LTX_VIDEO.jobs_path(), "/api/v1/ltx-video/jobs");
        assert_eq!(LTX_VIDEO.cancel_path(), "/api/v1/ltx-video/cancel");
    }

    #[test]
    fn job_state_wire_names_are_snake_case() {
        let s: VideoJobStatus =
            serde_json::from_str(r#"{"job_id":"a","status":"rendering"}"#).unwrap();
        assert_eq!(s.status, VideoJobState::Rendering);
        assert_eq!(s.segments, 0);
        assert!(s.partials.is_empty());
        for st in [
            VideoJobState::Queued,
            VideoJobState::Rendering,
            VideoJobState::Completed,
            VideoJobState::Failed,
            VideoJobState::Cancelled,
        ] {
            let json = serde_json::to_string(&st).unwrap();
            assert_eq!(json, format!("\"{}\"", st.as_str()));
        }
    }

    #[test]
    fn submit_response_parses_the_202_body() {
        let r: VideoJobSubmitResponse =
            serde_json::from_str(r#"{"job_id":"abc","status":"queued"}"#).unwrap();
        assert_eq!(r.job_id, "abc");
        assert_eq!(r.status, Some(VideoJobState::Queued));
    }

    #[test]
    fn progress_tracks_the_shot_count_it_has_painted() {
        let mut p = Progress::new("ltx");
        p.tty = false;
        p.update(&status(VideoJobState::Queued, 0));
        assert_eq!(p.partials, 0);
        p.update(&status(VideoJobState::Rendering, 2));
        assert_eq!(p.partials, 2);
        p.update(&status(VideoJobState::Rendering, 3));
        assert_eq!(p.partials, 3);
    }

    /// One dropped connection during a 40-minute render used to kill the client
    /// even though the job store survives a koblem restart.
    #[test]
    fn a_run_of_transient_poll_failures_is_ridden_out() {
        let mut r = PollRetry::default();
        for i in 1..POLL_RETRIES {
            assert!(
                r.lost(anyhow::anyhow!("connection reset"), LTX_VIDEO, "j1")
                    .is_none(),
                "failure {i} must not abort the poll"
            );
        }
        // A single good poll clears the run.
        r.reached();
        for _ in 1..POLL_RETRIES {
            assert!(
                r.lost(anyhow::anyhow!("connection reset"), LTX_VIDEO, "j1")
                    .is_none()
            );
        }
        // …but a full run of consecutive failures gives up, naming the re-attach.
        let e = r
            .lost(anyhow::anyhow!("connection reset"), LTX_VIDEO, "j1")
            .expect("must give up after POLL_RETRIES")
            .to_string();
        assert!(e.contains("kob video jobs --engine ltx --id j1"), "{e}");
    }

    #[test]
    fn downloads_do_not_overwrite_each_other_and_partials_follow_o() {
        // Without -o the clip carries engine + job id, so two renders coexist.
        assert_eq!(
            media_target(LTX_VIDEO, "j1", "webm", &None),
            PathBuf::from("ltx-j1.webm")
        );
        assert_eq!(
            media_target(LTX_VIDEO, "j2", "mp4", &None),
            PathBuf::from("ltx-j2.mp4")
        );
        assert_ne!(
            media_target(LTX_VIDEO, "j1", "webm", &None),
            media_target(LTX_VIDEO, "j2", "webm", &None)
        );
        let o = Some(PathBuf::from("out/clip.webm"));
        assert_eq!(
            media_target(LTX_VIDEO, "j1", "webm", &o),
            o.clone().unwrap()
        );

        // Per-shot files land beside the -o clip, not in the CWD.
        assert_eq!(
            partial_target(LTX_VIDEO, "j1", 2, &o),
            PathBuf::from("out/clip-shot2.webm")
        );
        assert_eq!(
            partial_target(LTX_VIDEO, "j1", 0, &None),
            PathBuf::from("ltx-j1-shot0.webm")
        );
    }

    #[test]
    fn summary_surfaces_the_resume_id_for_a_failed_render() {
        let mut s = status(VideoJobState::Failed, 1);
        s.error = Some("engine 502".into());
        let line = summarize(&s);
        assert!(line.contains("failed"));
        assert!(line.contains("resume=srv-1"));
        assert!(line.contains("engine 502"));
        assert!(line.contains("1 shot(s) ready"));
    }

    /// A `completed` job whose `/media` is one render unit reads exactly like a whole render
    /// otherwise — same status, same segment count — so the listing has to say which it is.
    #[test]
    fn summary_marks_a_render_that_delivers_a_fragment() {
        let whole = status(VideoJobState::Completed, 3);
        assert!(!summarize(&whole).contains("partial"));
        let mut fragment = whole.clone();
        fragment.job_is_partial = true;
        assert!(summarize(&fragment).contains("partial"));
    }
}
