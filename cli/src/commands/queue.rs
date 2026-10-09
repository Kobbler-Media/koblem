//! `kob queue` — the batch interface.
//!
//! "Queue 5 renders overnight" is most naturally a shell loop, which makes the
//! CLI arguably the *primary* interface for the batch case rather than an
//! afterthought.
//!
//! 🔴 The render subcommands' `--queue` / `--now` flags exist because the default
//! is computed from the CALLER, and the CLI authenticates with an API key — whose
//! default is `now`, the opposite of what the CLI is for. An explicit mode on the
//! request always wins.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use futures::StreamExt;
use koblem_client::types::{
    EnqueueRequest, EnqueueResponse, Job, JobKind, JobState, QueueSnapshot, SubmitMode,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum QueueCmd {
    /// Queue a render from a raw engine payload.
    Add(AddArgs),
    /// List the queue: what is running, what is waiting, and why.
    Ls(LsArgs),
    /// Show one job.
    Show { id: String },
    /// Move a job within (or between) bands.
    Mv(MvArgs),
    /// Cancel a job. A queued one is dropped; a running one is asked to stop.
    Rm { id: String },
    /// Send a job to the head of the interactive band.
    Bump { id: String },
    /// Hold a job (or release it) without losing its place.
    Hold {
        id: String,
        /// Release instead of holding.
        #[arg(long)]
        release: bool,
    },
    /// Stop starting new jobs. The running one finishes.
    Pause {
        /// Resume instead of pausing.
        #[arg(long)]
        resume: bool,
    },
    /// Follow the queue's event stream until interrupted.
    Watch,
    /// When the queue expects to be finished, and what that rests on.
    Eta,
    /// The measured duration table the estimates are built from.
    Stats,
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// Engine kind: ltx25, h3, flux2, krea2, avatar, music3, songgen, sa3, upscale,
    /// wan_vace, ltx_2b, ltx_relip, pixal3d_rig, pixal3d_motion.
    pub kind: String,
    /// The engine's own request body. `@PATH` reads it from a file, `@-` from stdin.
    #[arg(long, value_name = "JSON|@FILE")]
    pub payload: String,
    /// Dedupe key. Defaults to a digest of the payload, so re-running a loop that
    /// half-failed re-queues only what is no longer live. Pass one explicitly to
    /// queue a byte-identical payload twice on purpose.
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Include finished jobs.
    #[arg(long)]
    pub history: bool,
    #[arg(long, default_value_t = 200)]
    pub limit: i64,
}

#[derive(Args, Debug)]
pub struct MvArgs {
    pub id: String,
    /// Place immediately after this job. Omit for the head of the band.
    #[arg(long)]
    pub after: Option<String>,
    /// Move to a band: 0 interactive, 1 normal, 2 background.
    #[arg(long)]
    pub band: Option<i16>,
}

pub async fn run(ctx: &Ctx, cmd: QueueCmd) -> Result<()> {
    match cmd {
        QueueCmd::Add(args) => add(ctx, args).await,
        QueueCmd::Ls(args) => ls(ctx, args).await,
        QueueCmd::Show { id } => show(ctx, &id).await,
        QueueCmd::Mv(args) => mv(ctx, args).await,
        QueueCmd::Rm { id } => rm(ctx, &id).await,
        QueueCmd::Bump { id } => bump(ctx, &id).await,
        QueueCmd::Hold { id, release } => hold(ctx, &id, !release).await,
        QueueCmd::Pause { resume } => pause(ctx, !resume).await,
        QueueCmd::Watch => watch(ctx).await,
        QueueCmd::Eta => eta(ctx).await,
        QueueCmd::Stats => stats(ctx).await,
    }
}

/// 🔴 No `--band` / `--gpu` / `--after` of its own: those are the global flags, so
/// `kob --queue --band 2 queue add …` reads exactly as it does in front of any
/// render subcommand. They travel in the BODY here — the generic endpoint takes its
/// options from `EnqueueRequest`, where a render route reads `X-Queue-*`.
async fn add(ctx: &Ctx, args: AddArgs) -> Result<()> {
    // 🔴 Before the payload is read: a typo in the kind is the CLI's to catch, not a
    // 400 that arrives after a file (or a whole stdin) has been consumed.
    let Some(kind) = JobKind::parse(args.kind.trim()) else {
        bail!(
            "unknown kind {:?} — one of: {}",
            args.kind,
            JobKind::ALL
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let raw = super::refine::read_arg(&args.payload).await?;
    let payload: Value = serde_json::from_str(&raw).context("payload is not valid JSON")?;

    let key = args
        .idempotency_key
        .unwrap_or_else(|| payload_key(kind, &payload));
    let flags = ctx.client.queue_flags();
    let req = EnqueueRequest {
        kind,
        mode: flags.mode.as_deref().and_then(|m| match m {
            "queue" => Some(SubmitMode::Queue),
            "now" => Some(SubmitMode::Now),
            _ => None,
        }),
        band: flags.band,
        target_gpu: flags.gpu.clone(),
        force_pin: flags.force,
        depends_on: flags.after.clone(),
        // 🔴 Body *and* header, one value: this route reads the key off the body, every
        // per-engine render route reads `Idempotency-Key`, and the two must never differ.
        idempotency_key: Some(key.clone()),
        payload,
    };
    let resp = ctx
        .client
        .post_json_idempotent("/api/v1/queue", &[], &req, &key)
        .await?;
    let body: EnqueueResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("{}", receipt(kind, &body));
    if body.deduplicated {
        eprintln!("this payload is already live under key {key} — nothing new was queued");
    }
    Ok(())
}

/// Stable across invocations on purpose: a loop re-run after a partial failure must
/// re-queue what died, not a second copy of what survived. The server's unique index
/// covers the non-terminal states only, so a payload whose render has already
/// finished is a legitimate second take and still goes through.
fn payload_key(kind: JobKind, payload: &Value) -> String {
    let mut h = Sha256::new();
    h.update(kind.as_str().as_bytes());
    h.update(payload.to_string().as_bytes());
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("kob-{}-{}", kind.as_str(), &digest[..16])
}

/// The accept receipt in [`line`]'s columns, so a queued render reads the same here
/// as it does in the listing.
fn receipt(kind: JobKind, resp: &EnqueueResponse) -> String {
    let mut s = format!(
        "{:<10}  {:<24}  {}",
        resp.state.as_str(),
        resp.job_id,
        kind.as_str()
    );
    if let Some(position) = resp.position {
        s.push_str(&format!("  #{position}"));
    }
    if let Some(reason) = &resp.blocked_reason {
        s.push_str(&format!("  — waiting: {}", reason.summary()));
    }
    if let Some(eta) = &resp.eta_start {
        s.push_str(&format!("  starts ~{eta}"));
    }
    s
}

async fn snapshot(ctx: &Ctx, history: bool, limit: i64) -> Result<QueueSnapshot> {
    let query: Vec<(&str, String)> = vec![
        ("history", history.to_string()),
        ("limit", limit.to_string()),
    ];
    let resp = ctx.client.get("/api/v1/queue", &query).await?;
    Ok(resp.json().await?)
}

/// One job on one line. The blocked reason is on the same line as the state
/// because "queued" on its own is never the answer to "why hasn't it started".
fn line(job: &Job) -> String {
    let mut s = format!("{:<10}  {:<24}  {}", job.state.as_str(), job.id, job.label);
    if let (Some(done), Some(total)) = (Some(job.units_done), job.units_total)
        && total > 0
    {
        s.push_str(&format!("  [{done}/{total}]"));
    }
    if let Some(reason) = &job.blocked_reason {
        s.push_str(&format!("  — waiting: {}", reason.summary()));
        if let Some(since) = &job.blocked_since {
            s.push_str(&format!(" (since {since})"));
        }
    }
    if let Some(eta) = &job.eta_start
        && job.state == JobState::Queued
    {
        s.push_str(&format!("  starts ~{eta}"));
    }
    if let Some(err) = &job.error {
        s.push_str(&format!("  !! {err}"));
    }
    for w in &job.warnings {
        s.push_str(&format!("  ⚠ {w}"));
    }
    s
}

async fn ls(ctx: &Ctx, args: LsArgs) -> Result<()> {
    let body = snapshot(ctx, args.history, args.limit).await?;
    if ctx.json {
        return output::print_json(&body);
    }
    if body.paused {
        println!("(the queue is PAUSED — the running job will finish, nothing new will start)");
    }
    if body.jobs.is_empty() {
        println!("(the queue is empty)");
        return Ok(());
    }
    for job in &body.jobs {
        println!("{}", line(job));
    }
    // 🔴 The one line that matters when the page is closed: something an operator
    // could clear has been blocking for hours.
    if !body.stuck.is_empty() {
        eprintln!(
            "\n{} job(s) BLOCKED on something you can fix: {}",
            body.stuck.len(),
            body.stuck.join(", ")
        );
    }
    eprintln!("{} running, {} queued", body.running, body.pending);
    if let Some(eta) = &body.eta_finish {
        eprintln!("done ~{eta}");
    }
    Ok(())
}

async fn show(ctx: &Ctx, id: &str) -> Result<()> {
    let resp = ctx
        .client
        .get(&format!("/api/v1/queue/jobs/{id}"), &[])
        .await?;
    let job: Option<Job> = resp.json().await?;
    let Some(job) = job else {
        bail!("no such job: {id}");
    };
    if ctx.json {
        return output::print_json(&job);
    }
    println!("{}", line(&job));
    println!("  kind        {}", job.kind.as_str());
    println!("  band        {}  rank {}", job.band, job.queue_rank);
    println!("  created by  {}", job.created_by);
    println!("  created at  {}", job.created_at);
    if let Some(gpu) = &job.target_gpu {
        println!(
            "  pinned to   {gpu}{}",
            if job.force_pin { " (forced)" } else { "" }
        );
    }
    if let Some(parent) = &job.depends_on {
        println!("  after       {parent}");
    }
    if let Some(sid) = &job.server_job_id {
        println!("  engine job  {sid}");
    }
    // 🔴 Three numbers, deliberately apart. Folding the eviction cost into the
    // queue wait hides eviction thrash completely.
    if let Some(ms) = job.queued_ms {
        println!("  queued      {}", human_ms(ms));
    }
    if let Some(ms) = job.admit_ms {
        println!("  admission   {} (eviction cost)", human_ms(ms));
    }
    if let Some(ms) = job.render_ms {
        println!("  render      {}", human_ms(ms));
    }
    if let Some(shape) = &job.shape_key {
        println!(
            "  shape       {shape}  ({} measured runs)",
            job.estimate_samples
        );
    }
    Ok(())
}

fn human_ms(ms: i64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

async fn mv(ctx: &Ctx, args: MvArgs) -> Result<()> {
    let body = json!({ "after": args.after, "band": args.band, "bump": false });
    let resp = ctx
        .client
        .post_json(
            &format!("/api/v1/queue/jobs/{}/reorder", args.id),
            &[],
            &body,
        )
        .await?;
    expect_ok(resp, "moved").await
}

async fn bump(ctx: &Ctx, id: &str) -> Result<()> {
    let body = json!({ "bump": true });
    let resp = ctx
        .client
        .post_json(&format!("/api/v1/queue/jobs/{id}/reorder"), &[], &body)
        .await?;
    expect_ok(resp, "moved to the head of the interactive band").await
}

async fn rm(ctx: &Ctx, id: &str) -> Result<()> {
    let resp = ctx
        .client
        .delete(&format!("/api/v1/queue/jobs/{id}"), &[])
        .await?;
    // 🔴 409 here is usually the honest one: a job past the engine/disk boundary
    // has already paid for its GPU and killing it loses the only copy.
    expect_ok(resp, "cancelled").await
}

async fn hold(ctx: &Ctx, id: &str, held: bool) -> Result<()> {
    let resp = ctx
        .client
        .post_json(
            &format!("/api/v1/queue/jobs/{id}/hold"),
            &[],
            &json!({ "held": held }),
        )
        .await?;
    expect_ok(resp, if held { "held" } else { "released" }).await
}

async fn pause(ctx: &Ctx, paused: bool) -> Result<()> {
    let resp = ctx
        .client
        .post_json("/api/v1/queue/pause", &[], &json!({ "paused": paused }))
        .await?;
    expect_ok(resp, if paused { "paused" } else { "resumed" }).await
}

async fn eta(ctx: &Ctx) -> Result<()> {
    let body = snapshot(ctx, false, 500).await?;
    if ctx.json {
        return output::print_json(&json!({
            "eta_finish": body.eta_finish,
            "pending": body.pending,
            "running": body.running,
        }));
    }
    match &body.eta_finish {
        Some(t) => println!("{} job(s), done ~{t}", body.pending + body.running),
        // 🔴 An honest blank. An estimate built from no measurements is not
        // published as a time — a confident-looking guess is worse than none.
        None => println!(
            "{} job(s), no estimate yet — nothing of this shape has been measured",
            body.pending + body.running
        ),
    }
    for job in body.jobs.iter().filter(|j| j.eta_start.is_some()) {
        println!(
            "  {}  starts ~{}  done ~{}  (n={})",
            job.id,
            job.eta_start.as_deref().unwrap_or("?"),
            job.eta_finish.as_deref().unwrap_or("?"),
            job.estimate_samples
        );
    }
    Ok(())
}

async fn stats(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/api/v1/queue/stats", &[]).await?;
    let rows: serde_json::Value = resp.json().await?;
    if ctx.json {
        return output::print_json(&rows);
    }
    let Some(rows) = rows.as_array() else {
        bail!("unexpected stats body");
    };
    if rows.is_empty() {
        println!("(nothing measured yet)");
        return Ok(());
    }
    for row in rows {
        println!(
            "{:<32}  n={:<4} p50={:<10} p90={:<10} per-unit={}{}",
            row["shape_key"].as_str().unwrap_or("?"),
            row["samples"].as_i64().unwrap_or(0),
            human_ms(row["p50_ms"].as_i64().unwrap_or(0)),
            human_ms(row["p90_ms"].as_i64().unwrap_or(0)),
            human_ms(row["unit_p50_ms"].as_i64().unwrap_or(0)),
            // 🔴 Reported so a caller can tell a measurement from a guess.
            if row["confident"].as_bool().unwrap_or(false) {
                ""
            } else {
                "  (few samples — treat as a guess)"
            },
        );
    }
    Ok(())
}

/// Tail the SSE stream. This is what makes a shell loop supervisable: queue five
/// renders, leave `kob queue watch` running, and the terminal says what happened.
async fn watch(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/api/v1/queue/events", &[]).await?;
    if !resp.status().is_success() {
        bail!("the event stream is unavailable: {}", resp.status());
    }
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
        // SSE frames are separated by a blank line; anything short of that is a
        // partial read and must stay in the buffer.
        while let Some(end) = buf.find("\n\n") {
            let frame: String = buf.drain(..end + 2).collect();
            for line in frame.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                print_event(&event);
            }
        }
    }
    Ok(())
}

fn print_event(event: &serde_json::Value) {
    let id = event["job_id"].as_str().unwrap_or("?");
    let state = event["state"].as_str().unwrap_or("?");
    let mut line = format!("{state:<10}  {id}");
    if let Some(reason) = event["blocked_reason"]["reason"].as_str() {
        line.push_str(&format!("  — waiting: {reason}"));
    }
    if let Some(phase) = event["progress"]["phase"].as_str() {
        line.push_str(&format!("  {phase}"));
    }
    if let Some(err) = event["error"].as_str() {
        line.push_str(&format!("  !! {err}"));
    }
    if let Some(warnings) = event["warnings"].as_array() {
        for w in warnings {
            if let Some(w) = w.as_str() {
                line.push_str(&format!("  ⚠ {w}"));
            }
        }
    }
    println!("{line}");
}

async fn expect_ok(resp: reqwest::Response, done: &str) -> Result<()> {
    if resp.status().is_success() {
        eprintln!("{done}");
        return Ok(());
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or(body);
    bail!("{status}: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the default key: the same payload in a re-run of the loop
    /// is the same key, and a different one is not.
    #[test]
    fn the_default_key_is_the_payload() {
        let a = json!({ "prompt": "a cat", "seed": 1 });
        let b = json!({ "prompt": "a cat", "seed": 2 });
        assert_eq!(
            payload_key(JobKind::Flux2, &a),
            payload_key(JobKind::Flux2, &a)
        );
        assert_ne!(
            payload_key(JobKind::Flux2, &a),
            payload_key(JobKind::Flux2, &b)
        );
        assert_ne!(
            payload_key(JobKind::Flux2, &a),
            payload_key(JobKind::Krea2, &a)
        );
    }

    #[test]
    fn a_typo_in_the_kind_never_reaches_the_server() {
        assert!(JobKind::parse("ltx26").is_none());
        assert_eq!(JobKind::parse("ltx25"), Some(JobKind::Ltx25));
    }
}
