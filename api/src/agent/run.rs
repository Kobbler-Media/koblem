//! The agent loop: one background task per turn.
//!
//! # The GPU-gate rule this file exists to honour
//!
//! An `LlmLease` bundles llama-server's single concurrency permit with the
//! gate's LLM budget guard, and is held until the response body is consumed.
//! llama-server runs `--parallel 1`, so a tool executed while the lease is held
//! would deadlock against the drain an evicting render performs. [`complete`]
//! therefore takes the lease, consumes the body and drops it BEFORE returning —
//! the loop is acquire → one completion → drop → run tools → acquire again, and
//! nothing between those points holds a permit.
//!
//! The loop cannot run inside the browser's request either: a turn is minutes of
//! GPU work, so the task owns it and the browser polls the row. `LogDropGuard`
//! in `llama.rs` does the OPPOSITE of what is wanted here (it marks cancelled on
//! drop) and is deliberately not reused — a turn survives its HTTP request.
//!
//! Tool calls run SEQUENTIALLY even when the model emits several: one GPU job at
//! a time is the standing rule (`routes::ltx_critic`), and the gate would
//! serialise them anyway, just with the waits hidden.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use futures::FutureExt as _;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::SamplingConfig;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::repositories::llm_logs::{self, CreateLog};

use super::availability;
use super::dispatch::{self, Ctx, ToolOutcome, args_summary, self_base_url};
use super::fold::{self, ToolCall, ToolResult};
use super::media::MediaIndex;
use super::prompt;
use super::store;
use super::tools::Tool;
use super::wire::{Step, StepState, StoredAttachment, TurnStatus};

/// Tool-call rounds per turn. Six is enough for "image, then video of it, then
/// a track over the top" and short enough that a confused model stops.
const MAX_ROUNDS: usize = 6;
/// A tool that has already failed twice this turn is not tried again. The model
/// retries a failed render on a different engine otherwise, which costs GPU
/// minutes to arrive at the same failure.
const MAX_FAILURES_PER_TOOL: usize = 2;
/// Calls the model may make in ONE round. `MAX_ROUNDS` bounds the conversation,
/// not the width of a single reply: fifteen `make_video` calls in one round used
/// to run all fifteen sequentially without the failure counter ever tripping.
const MAX_CALLS_PER_ROUND: usize = 4;
/// How often the turn re-reads its own `cancel_requested`.
const CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(900);
/// Nothing joins the turn task and the stale-row sweep only runs at startup, so
/// this is the only thing that can resolve a turn wedged in a poll loop.
const TURN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

pub fn spawn(state: AppState, session_id: Uuid, turn_id: Uuid, token: String) {
    tokio::spawn(async move {
        let mut steps: Vec<Step> = Vec::new();
        let result = guarded(
            TURN_DEADLINE,
            run(&state, session_id, turn_id, &token, &mut steps),
        )
        .await;
        let (status, content) = match result {
            Ok(Outcome::Replied(text)) => (TurnStatus::Complete, text),
            Ok(Outcome::Cancelled) => (TurnStatus::Cancelled, String::new()),
            Err(e) => {
                tracing::warn!(%turn_id, error = %e, "agent turn failed");
                (TurnStatus::Error, e)
            }
        };
        if let Err(e) = store::finish_turn(&state.db, turn_id, status, &content, &steps).await {
            tracing::warn!(%turn_id, error = %e, "failed to finalize agent turn");
        }
    });
}

#[derive(Debug)]
enum Outcome {
    Replied(String),
    Cancelled,
}

async fn run(
    state: &AppState,
    session_id: Uuid,
    turn_id: Uuid,
    token: &str,
    steps: &mut Vec<Step>,
) -> Result<Outcome, String> {
    let history = store::messages(&state.db, session_id)
        .await
        .map_err(|e| format!("could not read the conversation: {e}"))?;

    let live = availability::live_tools(state).await;
    if live.is_empty() {
        return Err("no engines are configured or answering right now".to_string());
    }
    let system = prompt::system_prompt(&state.db, &live).await;
    let schemas: Vec<Value> = live.iter().map(|t| t.schema()).collect();

    // Only the turn being answered inlines its attachment bytes. Older turns
    // replay as text: the refs exist so a conversation can be reread without
    // re-sending megabytes, and the tools that need those bytes reach them
    // through `MediaIndex` instead of the prompt.
    let newest_user = history
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.seq)
        .max();

    let mut media = MediaIndex::default();
    let mut messages = vec![json!({"role": "system", "content": system})];
    for m in &history {
        if m.id == turn_id {
            continue;
        }
        media.push_attachments(&m.attachments);
        media.push_steps(&m.steps);
        messages.push(replay(state, m, Some(m.seq) == newest_user).await);
    }

    let watch = db_cancel_watch(state, turn_id);
    let mut ctx = Ctx {
        state: state.clone(),
        token: token.to_string(),
        base: self_base_url(&state.config.bind_address),
        http: reqwest::Client::builder()
            // No request timeout, by the same rule the engine clients follow: a
            // gated render legitimately takes minutes. Cancellation is the
            // cooperative flag, not a clock.
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?,
        media,
        cancelled: watch.flag.clone(),
    };

    let sampling = sampling(state).await;
    let mut failures: Vec<(Tool, usize)> = Vec::new();

    for round in 0..MAX_ROUNDS {
        if (ctx.cancelled)() {
            return Ok(Outcome::Cancelled);
        }
        note_gpu_wait(state, turn_id, steps).await;

        let mut body = json!({
            "model": "local",
            "messages": messages,
            "tools": schemas,
            "tool_choice": "auto",
        });
        sampling.apply_to(&mut body);

        let response = tokio::select! {
            biased;
            () = wait_for_cancel(CANCEL_POLL, &ctx.cancelled) => return Ok(Outcome::Cancelled),
            r = complete(state, &body, round) => r?,
        };
        let calls = bounded_calls(fold::tool_calls_from(&response));
        if calls.is_empty() {
            let text = fold::content_from(&response);
            return Ok(Outcome::Replied(match text.is_empty() {
                true => "I could not come up with a reply for that.".to_string(),
                false => text,
            }));
        }

        // Every call in this round is recorded as `pending` before any of them
        // runs, so the UI shows the whole plan rather than one row at a time.
        let first = steps.len();
        for call in &calls {
            let mut step = Step::new(steps.len() as i32, &call.name, args_summary(&call.args()));
            step.state = StepState::Pending.as_str().to_string();
            steps.push(step);
        }
        let _ = store::put_steps(&state.db, turn_id, steps).await;

        let mut results = Vec::with_capacity(calls.len());
        for (n, call) in calls.iter().enumerate() {
            if (ctx.cancelled)() {
                return Ok(Outcome::Cancelled);
            }
            let result = run_one(
                &mut ctx,
                turn_id,
                first + n,
                call,
                &live,
                &mut failures,
                steps,
            )
            .await;
            // A cancel that landed DURING the tool ends the turn as cancelled.
            // Folding the aborted call into the exchange instead would tell the
            // model a render failed and invite it to retry on the way out.
            if (ctx.cancelled)() {
                return Ok(Outcome::Cancelled);
            }
            results.push(result);
        }
        fold::append_tool_exchange(&mut messages, &calls, &results);

        if failures.iter().any(|(_, n)| *n >= MAX_FAILURES_PER_TOOL) {
            let reason = results
                .iter()
                .rev()
                .find_map(|r| r.observation.get("error").and_then(|v| v.as_str()))
                .unwrap_or("the engine kept failing");
            return Ok(Outcome::Replied(format!(
                "I could not finish that — {reason}."
            )));
        }
    }
    Ok(Outcome::Replied(
        "I ran out of steps on that one — tell me which part to finish and I will pick it up."
            .to_string(),
    ))
}

/// One completion. The lease is dropped before this returns; nothing holds a
/// llama-server permit while a tool runs.
async fn complete(state: &AppState, body: &Value, round: usize) -> Result<Value, String> {
    let started = Instant::now();
    let (lease, response) = state
        .llama
        .chat_completion_json(body)
        .await
        .map_err(|e| match e {
            crate::llama::Error::HeavyBusy => "the language model is disabled".to_string(),
            other => format!("the language model failed: {other}"),
        })?;
    drop(lease);

    log_completion(state, body, &response, started, round);
    Ok(response)
}

async fn run_one(
    ctx: &mut Ctx,
    turn_id: Uuid,
    index: usize,
    call: &ToolCall,
    live: &[super::tools::LiveTool],
    failures: &mut Vec<(Tool, usize)>,
    steps: &mut [Step],
) -> ToolResult {
    let args = call.args();
    steps[index].state = StepState::Running.as_str().to_string();
    let _ = store::put_steps(&ctx.state.db, turn_id, steps).await;

    let outcome = match Tool::parse(&call.name).and_then(|t| availability::find(live, t)) {
        Some(tool) => dispatch::dispatch(&*ctx, tool, &args).await,
        None => ToolOutcome {
            ok: false,
            observation: json!({"ok": false, "error": format!("{} is not available", call.name)}),
            outputs: Vec::new(),
            detail: format!("{} is not available", call.name),
        },
    };

    let verdict = verdict(outcome.ok, (ctx.cancelled)(), outcome.detail);
    if verdict.counts_as_failure
        && let Some(t) = Tool::parse(&call.name)
    {
        match failures.iter_mut().find(|(k, _)| *k == t) {
            Some((_, n)) => *n += 1,
            None => failures.push((t, 1)),
        }
    }

    steps[index].state = verdict.state.as_str().to_string();
    steps[index].detail = verdict.detail;
    steps[index].outputs = outcome.outputs;
    // Media this call just produced must be visible to the NEXT call in the same
    // turn, or "make me an image, then cut the background out" operates on
    // whatever came before the turn.
    ctx.media.push_steps(&steps[index..=index]);
    let _ = store::put_steps(&ctx.state.db, turn_id, steps).await;

    ToolResult {
        call_id: call.id.clone(),
        observation: outcome.observation,
    }
}

/// Record a GPU wait honestly instead of letting the turn look hung.
///
/// The LLM's `candidate_gpus` is the 3060 alone, so an eviction DROPS it rather
/// than relocating; the next `acquire_for_llm` BLOCKS while the card is freed
/// and the server cold-reloads (~5-10 s, KV cache gone). Only `flux` and
/// `songgen` can do this — every other engine is 5060-Ti-only with an empty
/// `evictable_by` — so the step names the culprit when there is one.
async fn note_gpu_wait(state: &AppState, turn_id: Uuid, steps: &mut Vec<Step>) {
    let occupancy = state.gpu.occupancy();
    let llm_resident = occupancy
        .iter()
        .any(|c| c.residents.iter().any(|(k, _)| *k == ServiceKind::Llm));
    if llm_resident {
        return;
    }
    let evictor = occupancy
        .iter()
        .flat_map(|c| c.residents.iter())
        .map(|(k, _)| *k)
        .find(|k| matches!(k, ServiceKind::Flux | ServiceKind::Songgen))
        .map(|k| k.as_str());
    let detail = match evictor {
        Some(who) => format!("waiting for GPU (LLM evicted by {who} render)"),
        None => "waiting for GPU (language model reloading)".to_string(),
    };
    // One note per wait, not one per round: the LLM stays evicted across several
    // rounds of a long turn and repeating the row says nothing new.
    if steps.last().is_some_and(|s| s.detail == detail) {
        return;
    }
    let mut step = Step::new(steps.len() as i32, "wait_for_gpu", String::new());
    step.detail = detail;
    step.state = StepState::Done.as_str().to_string();
    steps.push(step);
    let _ = store::put_steps(&state.db, turn_id, steps).await;
}

/// The cancel flag plus the task that maintains it.
///
/// `request_cancel` is `WHERE status = 'running'`, so a turn that ends normally
/// never flips the flag and the watcher has no other exit. Dropping the `Arc` is
/// not enough — the task holds its own — so the turn's ownership of this struct
/// IS the task's lifetime.
struct CancelWatch {
    flag: dispatch::CancelFlag,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn watch_cancel<F, Fut>(every: std::time::Duration, poll: F) -> CancelWatch
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = bool> + Send,
{
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = flag.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            if poll().await {
                watcher.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
    });
    CancelWatch {
        flag: Arc::new(move || flag.load(std::sync::atomic::Ordering::Relaxed)),
        task,
    }
}

fn db_cancel_watch(state: &AppState, turn_id: Uuid) -> CancelWatch {
    let db = state.db.clone();
    watch_cancel(CANCEL_POLL, move || {
        let db = db.clone();
        async move { store::is_cancel_requested(&db, turn_id).await }
    })
}

/// Block until the turn is cancelled. Raced against the completion so the wait
/// inside `acquire_for_llm` — the multi-minute one [`note_gpu_wait`] describes —
/// is interruptible; the flag itself is only refreshed by the watcher task.
async fn wait_for_cancel(every: std::time::Duration, flag: &dispatch::CancelFlag) {
    loop {
        if flag() {
            return;
        }
        tokio::time::sleep(every).await;
    }
}

/// What a finished tool call means for the step row and the failure counter.
#[derive(Debug, PartialEq)]
struct Verdict {
    state: StepState,
    detail: String,
    counts_as_failure: bool,
}

fn verdict(ok: bool, cancelled: bool, detail: String) -> Verdict {
    if ok {
        return Verdict {
            state: StepState::Done,
            detail,
            counts_as_failure: false,
        };
    }
    if cancelled {
        return Verdict {
            state: StepState::Failed,
            // The engines only expose a coarse `cancel_current`, so cancelling
            // the turn does NOT stop the render already on the card. Say so
            // rather than implying the GPU is free.
            detail: "cancelled — the GPU is still finishing this render".to_string(),
            counts_as_failure: false,
        };
    }
    Verdict {
        state: StepState::Failed,
        detail,
        counts_as_failure: true,
    }
}

fn bounded_calls(mut calls: Vec<ToolCall>) -> Vec<ToolCall> {
    calls.truncate(MAX_CALLS_PER_ROUND);
    calls
}

/// Wall-clock ceiling and panic net for one turn. Without both, a wedged poll
/// or a panicking dispatcher leaves the row `running` for the life of the
/// process — nothing joins the task and the startup sweep only runs at startup.
async fn guarded<F>(limit: std::time::Duration, fut: F) -> Result<Outcome, String>
where
    F: std::future::Future<Output = Result<Outcome, String>>,
{
    match AssertUnwindSafe(tokio::time::timeout(limit, fut))
        .catch_unwind()
        .await
    {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_elapsed)) => Err("this turn hit its time limit and was stopped".to_string()),
        Err(_panic) => Err("this turn stopped unexpectedly".to_string()),
    }
}

/// Replay one stored turn for the model. A completed assistant turn is replayed
/// as its narration only: its tool calls already produced media the user can
/// see, and re-sending them would invite the model to redo the work.
async fn replay(state: &AppState, m: &store::MessageRow, inline: bool) -> Value {
    if m.role == "assistant" {
        return json!({"role": "assistant", "content": m.content});
    }
    if !inline {
        return json!({
            "role": "user",
            "content": with_attachment_note(&m.content, &m.attachments),
        });
    }
    let (images, audio) = inline_attachments(state, &m.attachments).await;
    // 🔴 Same drop-and-continue rule as the raw chat route, for the same reason and through the
    // same helper: with Beefy mode on the LLM's mmproj is vision-only, and a stored turn that
    // carried a voice note must still REPLAY — a session recorded on gemma would otherwise become
    // unusable the moment the 27B is loaded. The warning is discarded here rather than surfaced:
    // this is a replay of history, not a new send, and re-warning about the same attachment on
    // every round of every turn would bury the one that matters. `agent_chat`'s send path is where
    // the user is told, via `/health`'s `llama_audio_input`.
    let (audio_parts, _dropped) = crate::routes::chat::admit_audio(state, &audio);
    crate::routes::chat::build_user_message(&m.content, &images, audio_parts)
}

/// Rehydrate stored `asset:<hash>` refs into the data URLs the multimodal path
/// wants (`routes::chat::build_user_message` keeps the full `data:` URI for an
/// image and strips it for audio — that asymmetry is llama-server's, not ours).
/// A ref whose bytes are gone is dropped: losing the picture is better than
/// losing the turn.
async fn inline_attachments(
    state: &AppState,
    attachments: &[StoredAttachment],
) -> (Vec<String>, Vec<String>) {
    use base64::Engine as _;
    let dir = state.director_assets_dir.as_ref();
    let (mut images, mut audio) = (Vec::new(), Vec::new());
    for a in attachments {
        let Some(hash) = crate::director_assets::ref_hash(&a.asset_ref) else {
            continue;
        };
        let Some(bytes) = crate::director_assets::read_bytes(dir, hash).await else {
            continue;
        };
        let url = format!(
            "data:{};base64,{}",
            a.mime,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        match a.kind.as_str() {
            "image" => images.push(url),
            "audio" => audio.push(url),
            _ => {}
        }
    }
    (images, audio)
}

fn with_attachment_note(content: &str, attachments: &[StoredAttachment]) -> String {
    if attachments.is_empty() {
        return content.to_string();
    }
    let kinds: Vec<&str> = attachments.iter().map(|a| a.kind.as_str()).collect();
    format!("{content}\n[attached earlier: {}]", kinds.join(", "))
}

/// The agent loop's own sampling surface (`sampling_agent`), not the chat box's.
/// It used to borrow `sampling_chat_default`, which meant tuning the chat box for
/// prose silently retuned every tool-calling round — a loop that has to emit exact
/// tool JSON wants its own knob. Baseline still pins thinking off; everything else
/// inherits until an operator sets it in Settings.
async fn sampling(state: &AppState) -> SamplingConfig {
    let baseline = SamplingConfig {
        enable_thinking: Some(false),
        ..SamplingConfig::default()
    };
    baseline.overlay(&crate::sampling::resolve_sampling(&state.db, "sampling_agent").await)
}

fn log_completion(
    state: &AppState,
    body: &Value,
    response: &Value,
    started: Instant,
    round: usize,
) {
    let mut log = CreateLog::new(
        Some("agent_chat".to_string()),
        crate::routes::chat::sanitize_messages_for_log(
            body.get("messages").cloned().unwrap_or(json!([])),
        ),
    );
    log.response_content = Some(dispatch::truncate(&fold::content_from(response), 4000));
    log.finish_reason = response
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    log.duration_ms = Some(started.elapsed().as_millis() as i64);
    log.prompt_tokens = response
        .pointer("/usage/prompt_tokens")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    log.completion_tokens = response
        .pointer("/usage/completion_tokens")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let db = state.db.clone();
    tokio::spawn(async move {
        if let Err(e) = llm_logs::create_log(&db, &log).await {
            tracing::warn!(round, "failed to store agent chat log: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    fn flag_pair() -> (Arc<AtomicBool>, dispatch::CancelFlag) {
        let raw = Arc::new(AtomicBool::new(false));
        let read = raw.clone();
        (raw, Arc::new(move || read.load(Ordering::Relaxed)))
    }

    fn calls(n: usize) -> Vec<ToolCall> {
        (0..n)
            .map(|i| ToolCall {
                id: format!("c{i}"),
                name: "make_video".to_string(),
                arguments: "{}".to_string(),
            })
            .collect()
    }

    /// `request_cancel` is `WHERE status = 'running'`, so a turn that finishes
    /// normally never flips the flag — the watcher has to be stopped by the turn
    /// ending, or every completed turn leaves a task querying Postgres forever.
    #[tokio::test]
    async fn a_completed_turn_stops_its_cancel_watcher() {
        let polls = Arc::new(AtomicUsize::new(0));
        let watch = watch_cancel(Duration::from_millis(5), {
            let polls = polls.clone();
            move || {
                let polls = polls.clone();
                async move {
                    polls.fetch_add(1, Ordering::Relaxed);
                    false
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            polls.load(Ordering::Relaxed) >= 3,
            "the watcher should poll while the turn runs"
        );

        drop(watch);
        let at_drop = polls.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            polls.load(Ordering::Relaxed),
            at_drop,
            "the watcher outlived the turn"
        );
    }

    /// Bounded from the outside as well: a regression here would otherwise wedge
    /// the whole suite with no diagnostic rather than failing.
    #[tokio::test]
    async fn a_wedged_turn_is_stopped_by_its_deadline() {
        let e = tokio::time::timeout(
            Duration::from_secs(5),
            guarded(
                Duration::from_millis(10),
                std::future::pending::<Result<Outcome, String>>(),
            ),
        )
        .await
        .expect("a turn must not outlive its own deadline")
        .expect_err("a turn past its deadline must resolve");
        assert!(e.contains("time limit"), "{e}");
    }

    /// Nothing joins the turn's `JoinHandle`, so a panicking dispatcher used to
    /// leave the row `running` until the process restarted.
    #[tokio::test]
    async fn a_panicking_turn_resolves_instead_of_stranding_the_row() {
        let e = guarded(Duration::from_secs(30), async {
            panic!("a dispatcher blew up");
        })
        .await
        .expect_err("a panicking turn must resolve");
        assert!(!e.is_empty(), "the row needs a reason to show");
    }

    #[tokio::test]
    async fn a_cancel_is_noticed_while_the_turn_waits_for_the_gpu() {
        let (raw, cancelled) = flag_pair();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            raw.store(true, Ordering::Relaxed);
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_cancel(Duration::from_millis(5), &cancelled),
        )
        .await
        .expect("a cancel during a GPU wait must interrupt the wait");
    }

    #[tokio::test]
    async fn an_uncancelled_turn_is_never_woken_by_the_cancel_watch() {
        let (_raw, cancelled) = flag_pair();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(60),
                wait_for_cancel(Duration::from_millis(5), &cancelled),
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn a_cancelled_tool_is_not_scored_as_a_tool_failure() {
        let done = verdict(true, false, "4s clip in 61.0s".to_string());
        assert_eq!(done.state, StepState::Done);
        assert!(!done.counts_as_failure);
        assert_eq!(done.detail, "4s clip in 61.0s");

        let failed = verdict(false, false, "ltx render failed: OOM".to_string());
        assert_eq!(failed.state, StepState::Failed);
        assert!(failed.counts_as_failure);
        assert_eq!(failed.detail, "ltx render failed: OOM");

        // Two cancelled renders used to trip MAX_FAILURES_PER_TOOL, ending the
        // turn `complete` with "I could not finish that" instead of `cancelled`.
        let cancelled = verdict(false, true, "ltx render failed: cancelled".to_string());
        assert!(!cancelled.counts_as_failure);
        // Cancelling the turn does NOT cancel the engine's render — the engines
        // only expose a coarse `cancel_current`, so the step says so.
        assert!(cancelled.detail.contains("still"), "{}", cancelled.detail);
    }

    #[test]
    fn one_round_cannot_queue_more_calls_than_the_turn_accounts_for() {
        assert_eq!(bounded_calls(calls(15)).len(), MAX_CALLS_PER_ROUND);
        assert_eq!(bounded_calls(calls(2)).len(), 2);
        assert!(bounded_calls(calls(0)).is_empty());
    }
}
