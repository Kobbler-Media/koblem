//! Runs one tool call against the real engine route and turns it into a
//! [`ToolOutcome`]: a compact observation for the model, and step outputs whose
//! URLs a browser can use directly.
//!
//! **Dispatch goes over koblem's own HTTP API, not into the engine clients.**
//! The engine routes own their defaults, their GPU-gate acquire, their gallery
//! persistence and their logging; re-deriving any of that here would be a second
//! copy that drifts. It also means the agent honours the gate exactly as the UI
//! does — including the rule that matters most: **no `LlmLease` is held while a
//! tool runs**. The loop drops the lease before calling in here, so a render
//! that evicts the LLM can drain it instead of deadlocking against a permit the
//! agent is still holding (llama-server is `--parallel 1`).
//!
//! Async engines (LTX-2.5, LTX-2B, Wan) answer 202 + `job_id`; we poll their
//! status route and then fetch the finished clip. Cancellation is checked on
//! every poll: [`CancelFlag`] is the same cooperative flag `ltx_jobs` uses.

mod edits;
mod engines;
mod library;
mod outputs;
mod transport;

use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use serde_json::{Value, json};

use crate::AppState;

use super::media::MediaIndex;
use super::tools::{LiveTool, RequiredWhen, Tool, missing_required};
use super::wire::StepOutput;

pub type CancelFlag = Arc<dyn Fn() -> bool + Send + Sync>;

pub struct Ctx {
    pub state: AppState,
    /// The caller's own bearer token. Held for the task's lifetime only — never
    /// persisted, because the stored URLs are token-less and the read path
    /// re-attaches whichever token the viewer presents.
    pub token: String,
    pub base: String,
    pub http: reqwest::Client,
    pub media: MediaIndex,
    pub cancelled: CancelFlag,
}

pub struct ToolOutcome {
    pub ok: bool,
    /// Compact JSON handed back to the model. NEVER media bytes.
    pub observation: Value,
    pub outputs: Vec<StepOutput>,
    /// Human-facing one-liner for the step row.
    pub detail: String,
}

impl ToolOutcome {
    pub(super) fn failed(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self {
            ok: false,
            observation: json!({"ok": false, "error": detail}),
            outputs: Vec::new(),
            detail,
        }
    }
}

/// koblem's own base URL, derived from the bind address. `0.0.0.0` / `[::]`
/// mean "every interface", which from inside the process is the loopback.
pub fn self_base_url(bind_address: &str) -> String {
    let port = bind_address.rsplit(':').next().unwrap_or("8090");
    format!("http://127.0.0.1:{port}")
}

/// Dispatch one call. The match is exhaustive over [`Tool`], so a tool cannot
/// be advertised without a dispatcher.
pub async fn dispatch(ctx: &Ctx, live: &LiveTool, args: &Value) -> ToolOutcome {
    let Some(engine) = live.resolve_engine(args) else {
        return ToolOutcome::failed("no engine available for this tool");
    };
    // Registry-wide, before any GPU is touched: an argument that only matters
    // for the mode the model picked is worth one cheap round trip, because the
    // engines below happily render nonsense instead of refusing.
    let missing = missing_required(live.tool, args);
    if !missing.is_empty() {
        return ToolOutcome::failed(missing_args_message(live.tool, &missing, args));
    }
    match live.tool {
        Tool::MakeImage => engines::make_image(ctx, engine, args).await,
        Tool::EditImage => edits::edit_image(ctx, args).await,
        Tool::MakeVideo => engines::make_video(ctx, engine, args).await,
        Tool::MakeAvatar => edits::make_avatar(ctx, args).await,
        Tool::MakeMusic => engines::make_music(ctx, engine, args).await,
        Tool::TransformMusic => edits::transform_music(ctx, args).await,
        Tool::MakeSpeech => engines::make_speech(ctx, engine, args).await,
        Tool::Transcribe => engines::transcribe(ctx, args).await,
        Tool::DescribeImage => edits::describe_image(ctx, args).await,
        Tool::RemoveBackground => engines::remove_background(ctx).await,
        Tool::MakeRig => engines::make_3d_asset(ctx, args).await,
        Tool::FindShots => library::find_shots(ctx, args).await,
    }
}

/// Name the gap, the value that opened it, and the tool to call again.
///
/// That last part is load-bearing and was measured: a message that only said
/// `"style" is required when mode is "cover"` sent the model to `make_music`
/// 2/3 of the time — it read "needs a style", found the tool whose required
/// list is `[style, engine]`, and composed a brand-new song instead of covering
/// the one in hand. Naming the tool to retry keeps it in lane.
fn missing_args_message(tool: Tool, missing: &[&RequiredWhen], args: &Value) -> String {
    let name = tool.name();
    let parts: Vec<String> = missing
        .iter()
        .map(|r| {
            let chose = args.get(r.when).and_then(Value::as_str).unwrap_or("");
            format!("\"{}\" is required when {} is \"{chose}\"", r.arg, r.when)
        })
        .collect();
    let args_wanted: Vec<&str> = missing.iter().map(|r| r.arg).collect();
    format!(
        "{name}: {} — nothing was run. Call {name} again with the same arguments plus {}.",
        parts.join("; "),
        args_wanted.join(" and "),
    )
}

/// One-line summary of the arguments for the step row. Long prompts are elided
/// — the step is a progress row, not a transcript.
pub fn args_summary(args: &Value) -> String {
    let Some(obj) = args.as_object() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in obj {
        let rendered = match v {
            Value::String(s) if s.chars().count() > 72 => {
                let head: String = s.chars().take(72).collect();
                format!("{head}…")
            }
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        parts.push(format!("{k}: {rendered}"));
    }
    parts.join(" · ")
}

pub(super) fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// Decode a `data:` URL (or bare base64) into bytes + mime.
pub fn decode_data_url(data_url: &str, fallback_mime: &str) -> Option<(Bytes, String)> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let (mime, payload) = match data_url.strip_prefix("data:") {
        Some(rest) => {
            let mime = rest.split(';').next().unwrap_or(fallback_mime);
            let payload = data_url.split_once(',').map(|(_, b)| b)?;
            (mime.to_string(), payload)
        }
        None => (fallback_mime.to_string(), data_url),
    };
    let bytes = b64.decode(payload.trim()).ok()?;
    Some((Bytes::from(bytes), mime))
}

pub(super) fn secs(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    format!("{:.1}s", ms as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_self_base_url_is_loopback_on_the_bound_port() {
        assert_eq!(self_base_url("0.0.0.0:8090"), "http://127.0.0.1:8090");
        assert_eq!(self_base_url("127.0.0.1:9999"), "http://127.0.0.1:9999");
        assert_eq!(self_base_url("[::]:8090"), "http://127.0.0.1:8090");
    }

    #[test]
    fn args_summary_elides_long_prompts() {
        let s = args_summary(&json!({"prompt": "x".repeat(200), "engine": "krea2"}));
        assert!(s.contains("engine: krea2"));
        assert!(s.contains('…'));
        assert!(s.chars().count() < 130);
    }

    #[test]
    fn args_summary_of_a_non_object_is_empty() {
        assert_eq!(args_summary(&json!("nope")), "");
        assert_eq!(args_summary(&json!({})), "");
    }

    #[test]
    fn data_urls_and_bare_base64_both_decode() {
        let (bytes, mime) = decode_data_url("data:image/png;base64,QUJD", "image/png").unwrap();
        assert_eq!(&bytes[..], b"ABC");
        assert_eq!(mime, "image/png");
        let (bytes, mime) = decode_data_url("QUJD", "audio/wav").unwrap();
        assert_eq!(&bytes[..], b"ABC");
        assert_eq!(mime, "audio/wav");
        assert!(decode_data_url("data:image/png;base64,!!!!", "image/png").is_none());
    }

    #[test]
    fn a_failed_outcome_reports_the_reason_to_both_the_model_and_the_step() {
        let o = ToolOutcome::failed("krea2 is down");
        assert!(!o.ok);
        assert_eq!(o.observation["ok"], false);
        assert_eq!(o.observation["error"], "krea2 is down");
        assert_eq!(o.detail, "krea2 is down");
        assert!(o.outputs.is_empty());
    }

    #[test]
    fn a_missing_conditional_argument_names_the_value_that_made_it_required() {
        let args = json!({"mode": "extract"});
        let missing = missing_required(Tool::TransformMusic, &args);
        let msg = missing_args_message(Tool::TransformMusic, &missing, &args);
        assert!(msg.contains("transform_music"));
        assert!(msg.contains("\"track\""));
        assert!(msg.contains("\"extract\""), "{msg}");
        assert!(msg.contains("nothing was run"), "{msg}");
    }

    #[test]
    fn a_missing_argument_message_points_the_retry_back_at_the_same_tool() {
        // Without this the model reads "needs a style" and jumps to make_music,
        // composing a new song instead of transforming the one in hand.
        let args = json!({"mode": "cover"});
        let missing = missing_required(Tool::TransformMusic, &args);
        let msg = missing_args_message(Tool::TransformMusic, &missing, &args);
        assert!(msg.contains("Call transform_music again"), "{msg}");
        assert!(msg.ends_with("plus style."), "{msg}");
    }

    #[test]
    fn several_missing_arguments_are_reported_in_one_pass() {
        // No `style` and no `track` on a stem mode: the model should not need
        // two round trips to learn both.
        let args = json!({"mode": "cover"});
        let mut missing = missing_required(Tool::TransformMusic, &args).to_vec();
        missing.extend(missing_required(
            Tool::TransformMusic,
            &json!({"mode": "lego"}),
        ));
        let msg = missing_args_message(Tool::TransformMusic, &missing, &args);
        assert!(
            msg.contains("\"style\"") && msg.contains("\"track\""),
            "{msg}"
        );
        assert!(msg.contains(';'), "{msg}");
    }

    #[test]
    fn durations_read_as_humans_expect() {
        assert_eq!(secs(120), "120ms");
        assert_eq!(secs(11200), "11.2s");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("héllo", 10), "héllo");
        assert_eq!(truncate("héllo", 3), "hél…");
    }
}
