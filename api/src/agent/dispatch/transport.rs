//! The self-HTTP calls the dispatchers make against koblem's own engine routes,
//! plus the async-job poll loop. Nothing here knows what a tool is.

use std::time::Duration;

use bytes::Bytes;
use serde_json::Value;

use super::{Ctx, truncate};

/// Async renders poll this often. Matches the engine clients' own cadence.
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Engine error text reaches the model's prompt AND the browser verbatim, so a
/// multi-kilobyte CUDA traceback naming internal paths is capped here, whatever
/// shape it arrived in.
const ERROR_MAX: usize = 300;

pub(super) async fn post_json(ctx: &Ctx, path: &str, body: &Value) -> Result<Value, String> {
    let resp = ctx
        .http
        .post(format!("{}{path}", ctx.base))
        .bearer_auth(&ctx.token)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(error_message(status.as_u16(), &text));
    }
    serde_json::from_str(&text).map_err(|e| format!("unreadable response: {e}"))
}

pub(super) async fn post_bytes(
    ctx: &Ctx,
    path: &str,
    body: &Value,
) -> Result<(Bytes, String), String> {
    let resp = ctx
        .http
        .post(format!("{}{path}", ctx.base))
        .bearer_auth(&ctx.token)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/wav")
        .split(';')
        .next()
        .unwrap_or("audio/wav")
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(error_message(
            status.as_u16(),
            &String::from_utf8_lossy(&bytes),
        ));
    }
    Ok((bytes, mime))
}

pub(super) async fn post_raw(
    ctx: &Ctx,
    path: &str,
    mime: &str,
    bytes: Bytes,
) -> Result<Value, String> {
    let resp = ctx
        .http
        .post(format!("{}{path}", ctx.base))
        .bearer_auth(&ctx.token)
        .header(reqwest::header::CONTENT_TYPE, mime)
        .body(bytes)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(error_message(status.as_u16(), &text));
    }
    serde_json::from_str(&text).map_err(|e| format!("unreadable response: {e}"))
}

/// One file part of a multipart body. A mime the header parser rejects falls
/// back to an unlabelled part rather than dropping the upload.
pub(super) fn file_part(bytes: Bytes, mime: &str, filename: &str) -> reqwest::multipart::Part {
    let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string());
    match part.mime_str(mime) {
        Ok(p) => p,
        Err(_) => reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string()),
    }
}

pub(super) async fn post_multipart(
    ctx: &Ctx,
    path: &str,
    form: reqwest::multipart::Form,
) -> Result<Value, String> {
    let resp = ctx
        .http
        .post(format!("{}{path}", ctx.base))
        .bearer_auth(&ctx.token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(error_message(status.as_u16(), &text));
    }
    serde_json::from_str(&text).map_err(|e| format!("unreadable response: {e}"))
}

pub async fn fetch(ctx: &Ctx, path: &str) -> Result<Bytes, String> {
    let resp = ctx
        .http
        .get(format!("{}{path}", ctx.base))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    resp.bytes().await.map_err(|e| e.to_string())
}

/// What one poll of an async render's status route means.
#[derive(Debug, PartialEq)]
pub(super) enum JobState {
    Done(Value),
    Failed(String),
    Wait,
}

/// Read one status response. Split out of [`poll_job`] because "keep polling" is
/// the dangerous default: every shape that is NOT a live job has to end the
/// loop, or the turn sits in `running` until the process dies. A key revoked
/// mid-render and a job the store evicted (`MAX_JOBS` / `JOB_TTL_SECS`) both
/// answer with no `status` at all.
pub(super) fn job_state(status: u16, body: &str) -> JobState {
    let Ok(job) = serde_json::from_str::<Value>(body) else {
        return JobState::Failed(error_message(status, body));
    };
    // 🔴 TWO vocabularies. The engines' own job stores said `status: completed`;
    // the queue's per-engine views say `state: done`, and each maps the queue's
    // states onto its engine's words — so `rendering` and `queued` are both live.
    // A view that answers neither key is not a live job, and "keep polling" is the
    // dangerous default.
    let word = job
        .get("status")
        .or_else(|| job.get("state"))
        .and_then(|v| v.as_str());
    match word {
        Some("completed" | "done") => JobState::Done(job),
        Some("failed") => JobState::Failed(truncate(
            job.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("render failed"),
            ERROR_MAX,
        )),
        Some("cancelled") => JobState::Failed("cancelled".to_string()),
        Some(_) if status < 300 => JobState::Wait,
        _ => JobState::Failed(error_message(status, body)),
    }
}

/// Poll an async render to a terminal state, aborting on a cooperative cancel.
pub(super) async fn poll_job(ctx: &Ctx, jobs: &str, job_id: &str) -> Result<Value, String> {
    loop {
        if (ctx.cancelled)() {
            return Err("cancelled".to_string());
        }
        let resp = ctx
            .http
            .get(format!("{}{jobs}/{job_id}", ctx.base))
            .bearer_auth(&ctx.token)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        match job_state(status, &text) {
            JobState::Done(job) => return Ok(job),
            JobState::Failed(e) => return Err(e),
            JobState::Wait => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }
}

/// Engine routes answer `{"error": "..."}`; anything else is passed through
/// truncated so a stack of HTML never reaches the model as an observation.
pub(super) fn error_message(status: u16, body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| truncate(s, ERROR_MAX))
        })
        .unwrap_or_else(|| format!("HTTP {status}: {}", truncate(body.trim(), ERROR_MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn engine_route_errors_are_unwrapped_to_their_message() {
        assert_eq!(
            error_message(503, r#"{"error":"krea2 service not configured"}"#),
            "krea2 service not configured"
        );
    }

    #[test]
    fn a_non_json_error_body_is_truncated_not_forwarded_whole() {
        let html = "<html>".repeat(200);
        let msg = error_message(502, &html);
        assert!(msg.starts_with("HTTP 502: "));
        assert!(msg.chars().count() <= ERROR_MAX + 12);
    }

    /// A CUDA traceback arrives as a perfectly valid `{"error": ...}` — the JSON
    /// branch is exactly the one that must not forward kilobytes of internal
    /// paths into the prompt (twice) and the browser.
    #[test]
    fn a_json_error_message_is_truncated_too() {
        let body = json!({"error": format!("CUDA error at /opt/src/x.cu\n{}", "y".repeat(5000))});
        let msg = error_message(500, &body.to_string());
        assert!(msg.starts_with("CUDA error at"), "{msg}");
        assert!(msg.chars().count() <= ERROR_MAX + 1, "{}", msg.len());
    }

    #[test]
    fn a_live_job_is_the_only_reason_to_keep_polling() {
        assert_eq!(job_state(200, r#"{"status":"queued"}"#), JobState::Wait);
        assert_eq!(job_state(200, r#"{"status":"running"}"#), JobState::Wait);
        assert!(matches!(
            job_state(200, r#"{"status":"completed","render_ms":9}"#),
            JobState::Done(_)
        ));
    }

    /// The turn holds the caller's raw token for its whole life; revoking it
    /// mid-render used to leave the poll spinning at 750 ms forever.
    #[test]
    fn a_rejected_poll_ends_the_turn_rather_than_looping() {
        assert_eq!(
            job_state(401, r#"{"error":"invalid token"}"#),
            JobState::Failed("invalid token".to_string())
        );
        assert_eq!(
            job_state(404, r#"{"error":"job not found"}"#),
            JobState::Failed("job not found".to_string())
        );
        // MAX_JOBS eviction answers 404 with no body at all.
        assert!(matches!(job_state(404, ""), JobState::Failed(_)));
        assert!(matches!(
            job_state(502, "<html>bad gateway"),
            JobState::Failed(_)
        ));
    }

    /// Engines answer errors as `{"error": ...}` with no `status` key — on a 200
    /// that used to fall through to the sleep arm.
    #[test]
    fn a_statusless_body_is_not_a_live_job() {
        assert_eq!(
            job_state(200, r#"{"error":"engine not configured"}"#),
            JobState::Failed("engine not configured".to_string())
        );
        assert!(matches!(job_state(200, "{}"), JobState::Failed(_)));
        assert!(matches!(job_state(200, "not json"), JobState::Failed(_)));
    }

    #[test]
    fn a_failed_jobs_error_is_truncated_like_every_other() {
        let body = json!({"status": "failed", "error": "z".repeat(5000)});
        let JobState::Failed(msg) = job_state(200, &body.to_string()) else {
            panic!("expected a failure");
        };
        assert!(msg.chars().count() <= ERROR_MAX + 1, "{}", msg.len());
    }
}
