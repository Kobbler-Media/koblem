//! One event spine, three sinks.
//!
//! There was no notification infrastructure in koblem at all — no SSE, no webhook,
//! no push. What existed is NATS, so that is the convention followed rather than a
//! new one invented.
//!
//! | Sink | For |
//! |---|---|
//! | NATS `events.gpu.job.*` | kobbler and anything else reacting programmatically |
//! | SSE `/api/v1/queue/events` | the queue page, the pill, a browser Notification |
//! | outbound webhook | the overnight case — the one that actually matters |
//!
//! Rules that matter more than the transport:
//!
//! * **Notify on the BATCH, not just the job.** For five queued renders the thing
//!   you want at 04:00 is "all 5 finished, 4 ok, 1 failed" — not five pings.
//! * **Coalesce.** A 16-generation H3 job emits per-generation progress. Those go
//!   to the SSE stream and nowhere else.
//! * **`warnings` ride along.** A render that "succeeded" having silently dropped
//!   a generation looks exactly like "I forgot to press render".

use koblem_client::types::{BlockedReason, JobEvent, JobKind, JobProgress, JobState};
use tokio::sync::broadcast;

/// Enough headroom that a slow SSE client lags rather than wedging the sender.
const CHANNEL: usize = 256;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<JobEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL);
        EventBus { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.tx.subscribe()
    }

    fn emit(&self, event: JobEvent) {
        // No subscribers is the normal state (nobody has the page open), and a
        // send error means exactly that. Never a failure.
        let _ = self.tx.send(event);
    }

    pub fn publish_progress(
        &self,
        job_id: &str,
        kind: JobKind,
        progress: JobProgress,
        _units_done: i32,
    ) {
        self.emit(JobEvent {
            job_id: job_id.to_string(),
            kind,
            state: JobState::Running,
            progress: Some(progress),
            blocked_reason: None,
            error: None,
            warnings: vec![],
            at: now(),
        });
    }

    /// A row entered a new state: admitted to the queue, or claimed and started.
    ///
    /// 🔴 The spine used to carry progress, blocked and terminal — and nothing else. So the two
    /// transitions a page most needs, "a job appeared" and "a job started", were the two it could
    /// not be told about, and every page that cared had to poll for them on a timer. The Director
    /// asked `/active` every twelve seconds for exactly this, all night, mostly to be told no.
    ///
    /// Deliberately not published for `Collecting`: it is a real transition, but the row is already
    /// past anything a client can act on and the terminal frame is moments away.
    pub fn publish_state(&self, job_id: &str, kind: JobKind, state: JobState) {
        self.emit(JobEvent {
            job_id: job_id.to_string(),
            kind,
            state,
            progress: None,
            blocked_reason: None,
            error: None,
            warnings: vec![],
            at: now(),
        });
    }

    pub fn publish_blocked(&self, job_id: &str, kind: JobKind, reason: &BlockedReason) {
        self.emit(JobEvent {
            job_id: job_id.to_string(),
            kind,
            state: JobState::Queued,
            progress: None,
            blocked_reason: Some(reason.clone()),
            error: None,
            warnings: vec![],
            at: now(),
        });
    }

    pub fn publish_terminal(
        &self,
        job_id: &str,
        kind: JobKind,
        state: JobState,
        error: Option<&str>,
        warnings: &[String],
    ) {
        self.emit(JobEvent {
            job_id: job_id.to_string(),
            kind,
            state,
            progress: None,
            blocked_reason: None,
            error: error.map(str::to_string),
            warnings: warnings.to_vec(),
            at: now(),
        });
    }
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An event with nobody listening is not an error — the page being closed is
    /// the normal state, and it is also exactly when the webhook matters.
    #[test]
    fn publishing_into_the_void_is_fine() {
        let bus = EventBus::new();
        bus.publish_terminal("j1", JobKind::H3, JobState::Done, None, &[]);
    }

    /// Warnings ride the terminal event. A "succeeded" render that dropped a
    /// generation must not look like a clean success.
    #[tokio::test]
    async fn warnings_reach_the_subscriber() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish_terminal(
            "j1",
            JobKind::Ltx25,
            JobState::Done,
            None,
            &["segment 3 was delivered short".to_string()],
        );
        let got = rx.recv().await.unwrap();
        assert_eq!(got.state, JobState::Done);
        assert_eq!(got.warnings.len(), 1);
    }

    /// A blocked reason reaches the stream as the tagged enum, so the page shows
    /// the same words the CLI does.
    #[tokio::test]
    async fn blocked_reasons_reach_the_subscriber() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish_blocked(
            "j1",
            JobKind::Upscale,
            &BlockedReason::LaneBusy {
                lane: "comfy-api".into(),
            },
        );
        let got = rx.recv().await.unwrap();
        assert_eq!(got.blocked_reason.unwrap().summary(), "comfy-api slot busy");
    }
}
