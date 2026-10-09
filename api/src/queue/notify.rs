//! The out-of-process sinks on the event spine: NATS, the outbound webhook, and browser push.
//!
//! The SSE stream feeds the UI and lives in `routes::queue`. This module is the
//! half that reaches someone who is asleep.
//!
//! 🔴 The rules matter more than the transport:
//!
//! * **Notify on the BATCH, not just the job.** For five queued renders the thing
//!   you want at 04:00 is "all 5 finished, 4 ok, 1 failed" — not three pushes at
//!   02:00, 03:10 and 04:20.
//! * **Coalesce.** A 16-generation H3 job emits per-generation progress. Those go
//!   to SSE and nowhere else.
//! * **`warnings` ride along.** A render that "succeeded" having silently dropped
//!   a generation looks exactly like "I forgot to press render", and nobody is
//!   reading docker logs at 3am — they rotate (50m × 3) within hours.
//! * **Blocked-too-long is a notification.** Waking up to five jobs that never
//!   started because a service was left disabled is the failure this whole design
//!   exists to prevent.

use std::time::Duration;

use koblem_client::types::{JobEvent, JobState, WebhookPayload};
use nats_jobs::{Event, NatsClient, NatsConfig};
use tracing::{debug, info, warn};

use crate::repositories::{gpu_jobs as store, settings as settings_repo};

use super::events::EventBus;
use super::push::{Push, PushNote};

/// How long a job must sit blocked on a reason nothing will clear by itself
/// before it is worth waking someone. Settings key `queue_stuck_alert_minutes`.
const DEFAULT_STUCK_MINUTES: i64 = 30;

/// How often the stuck sweep runs. Cheap — one indexed query.
const STUCK_POLL: Duration = Duration::from_secs(120);

/// Settings key holding the outbound webhook URL. Empty / absent = no webhook,
/// which is the default and is not an error.
const WEBHOOK_URL_KEY: &str = "queue_webhook_url";

pub struct Notifier {
    pool: sqlx::PgPool,
    events: EventBus,
    http: reqwest::Client,
    push: Option<Push>,
    nats_url: Option<String>,
}

/// The running tally of one unattended batch.
#[derive(Default)]
struct Batch {
    ok: i64,
    failed: i64,
    /// Job ids that finished carrying warnings, with the first warning each.
    warned: Vec<(String, String)>,
    open: bool,
}

impl Notifier {
    pub async fn new(pool: sqlx::PgPool, events: EventBus, nats_url: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let push = super::push::load_vapid(&pool)
            .await
            .map(|vapid| Push::new(pool.clone(), http.clone(), vapid));
        Notifier {
            pool,
            events,
            http,
            push,
            nats_url,
        }
    }

    pub async fn run(self) {
        let jetstream = connect_nats(self.nats_url.as_deref()).await;
        let mut rx = self.events.subscribe();
        let mut batch = Batch::default();
        let mut stuck_tick = tokio::time::interval(STUCK_POLL);
        // The first tick fires immediately; skip it so a restart does not
        // re-announce everything that was already blocked before it.
        stuck_tick.tick().await;

        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Ok(event) => self.on_event(&jetstream, &mut batch, event).await,
                    // A lagged notifier has missed transitions it cannot reconstruct.
                    // Reset the tally rather than report a wrong one.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(missed = n, "queue: the notifier lagged; the batch tally restarts");
                        batch = Batch::default();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                _ = stuck_tick.tick() => self.sweep_stuck().await,
            }
        }
    }

    async fn on_event(
        &self,
        jetstream: &Option<async_nats::jetstream::Context>,
        batch: &mut Batch,
        event: JobEvent,
    ) {
        // 🔴 Progress never reaches here as a push. Only transitions.
        if !event.state.is_terminal() {
            return;
        }

        publish_nats(jetstream, &event).await;

        batch.open = true;
        match event.state {
            JobState::Done => batch.ok += 1,
            JobState::Failed => {
                batch.failed += 1;
                // An individual failure is worth its own push: it is actionable
                // now, and the batch summary may be hours away.
                let why = event
                    .error
                    .as_deref()
                    .unwrap_or("the engine gave no reason");
                self.announce(
                    &WebhookPayload {
                        event: "job_failed".into(),
                        title: format!("{} render failed", event.kind.as_str()),
                        text: format!(
                            "{} job {} failed: {}",
                            event.kind.as_str(),
                            event.job_id,
                            why
                        ),
                        job_id: Some(event.job_id.clone()),
                        warnings: event.warnings.clone(),
                        ok: 0,
                        failed: 1,
                        at: event.at.clone(),
                    },
                    PushNote {
                        title: format!(
                            "Render failed — {} · {}",
                            event.kind.as_str(),
                            event.job_id
                        ),
                        body: why.to_string(),
                        tag: format!("queue:{}:failed", event.job_id),
                    },
                )
                .await;
            }
            _ => {}
        }
        if let Some(first) = event.warnings.first() {
            batch.warned.push((event.job_id.clone(), first.clone()));
        }

        // The batch closes when the queue drains: nothing pending, nothing live.
        let pending = store::pending(&self.pool).await.unwrap_or_default();
        let live = store::live(&self.pool).await.unwrap_or_default();
        if !pending.is_empty() || !live.is_empty() {
            return;
        }

        let total = batch.ok + batch.failed;
        if batch.open && total > 0 {
            let mut text = format!("{total} finished, {} ok, {} failed", batch.ok, batch.failed);
            for (id, warning) in &batch.warned {
                text.push_str(&format!(" — {id} warned: {warning}"));
            }
            self.announce(
                &WebhookPayload {
                    event: "batch_complete".into(),
                    title: format!("{total} renders finished"),
                    text: text.clone(),
                    job_id: None,
                    warnings: batch.warned.iter().map(|(_, w)| w.clone()).collect(),
                    ok: batch.ok,
                    failed: batch.failed,
                    at: event.at.clone(),
                },
                PushNote {
                    title: format!("{total} renders finished"),
                    body: text,
                    // No job to collapse against, and stamping the batch rather than reusing one
                    // tag keeps this evening's summary from replacing last night's unread one.
                    tag: format!("queue:batch:{}", event.at),
                },
            )
            .await;
        }
        *batch = Batch::default();
    }

    /// Jobs blocked past the threshold on something an operator can clear.
    ///
    /// 🔴 Filtered to operator-clearable reasons on purpose. A busy comfy lane at
    /// 03:00 is the queue working correctly and waking someone for it trains them
    /// to ignore the alert; a service left disabled at 22:00 is a lost night.
    async fn sweep_stuck(&self) {
        let minutes = read_i64(
            &self.pool,
            "queue_stuck_alert_minutes",
            DEFAULT_STUCK_MINUTES,
        )
        .await;
        if minutes <= 0 {
            return;
        }
        let stuck = store::stuck(&self.pool, minutes * 60)
            .await
            .unwrap_or_default();
        for job in stuck {
            let Some(reason) = job.blocked_reason.as_ref() else {
                continue;
            };
            if !reason.is_operator_clearable() {
                continue;
            }
            // Once per episode: the row is stamped so the alert does not repeat
            // every two minutes for the rest of the night.
            if store::mark_alerted(&self.pool, &job.id)
                .await
                .unwrap_or(false)
            {
                self.announce(
                    &WebhookPayload {
                        event: "job_blocked".into(),
                        title: format!("a {} job has not started", job.kind.as_str()),
                        text: format!(
                            "{} job {} has been waiting {minutes}+ minutes: {}",
                            job.kind.as_str(),
                            job.id,
                            reason.summary()
                        ),
                        job_id: Some(job.id.clone()),
                        warnings: vec![],
                        ok: 0,
                        failed: 0,
                        at: job.blocked_since.clone().unwrap_or_default(),
                    },
                    PushNote {
                        title: format!("Queue blocked — {}", job.kind.as_str()),
                        body: format!("{} for {minutes}+ minutes", reason.summary()),
                        tag: format!("queue:{}:blocked", job.id),
                    },
                )
                .await;
            }
        }
    }

    /// Both out-of-process sinks, from one call site.
    ///
    /// 🔴 Kept as a pair on purpose: the webhook's grain — batch completion, an individual failure,
    /// blocked-too-long — is the grain a phone can stand. Pushing anything the webhook does not
    /// send would mean sixteen lock-screen notifications for one H3 render.
    async fn announce(&self, payload: &WebhookPayload, note: PushNote) {
        self.webhook(payload).await;
        if let Some(push) = &self.push {
            push.send(&note).await;
        }
    }

    /// 🔴 One POST, raw JSON, with a pre-rendered `text`. ntfy, gotify, Discord
    /// and Slack all render something useful from that, and a per-service template
    /// is a maintenance burden for a one-line message.
    async fn webhook(&self, payload: &WebhookPayload) {
        let Some(url) = read_string(&self.pool, WEBHOOK_URL_KEY).await else {
            return;
        };
        match self.http.post(&url).json(payload).send().await {
            Ok(r) if r.status().is_success() => {
                debug!(event = %payload.event, "queue: webhook delivered")
            }
            Ok(r) => warn!(
                status = r.status().as_u16(),
                "queue: webhook returned non-success"
            ),
            Err(e) => warn!(error = %e, "queue: webhook failed"),
        }
    }
}

async fn connect_nats(nats_url: Option<&str>) -> Option<async_nats::jetstream::Context> {
    nats_url.filter(|url| !url.is_empty())?;
    let config = NatsConfig::from_env("koblem-queue-publisher");
    match NatsClient::connect(&config).await {
        Ok(nats) => {
            info!("queue: publishing job events to NATS");
            Some(nats.jetstream().clone())
        }
        // No NATS is the normal single-box deployment, not a failure.
        Err(e) => {
            debug!(error = %e, "queue: no NATS; job events stay on SSE and the webhook");
            None
        }
    }
}

/// `events.gpu.job.{completed,failed,blocked}` — for kobbler and anything else in
/// the ecosystem reacting programmatically. Reuses the existing client and
/// envelope rather than inventing a second convention.
async fn publish_nats(jetstream: &Option<async_nats::jetstream::Context>, event: &JobEvent) {
    let Some(js) = jetstream else { return };
    let leaf = match event.state {
        JobState::Done => "completed",
        JobState::Failed => "failed",
        _ => "blocked",
    };
    let Ok(payload) = serde_json::to_value(event) else {
        return;
    };
    let envelope = Event::new(format!("gpu.job.{leaf}"), payload, "koblem");
    let Ok(bytes) = serde_json::to_vec(&envelope) else {
        return;
    };
    match js
        .publish(format!("events.gpu.job.{leaf}"), bytes.into())
        .await
    {
        Ok(ack) => {
            if let Err(e) = ack.await {
                warn!(error = %e, "queue: NATS ack failed");
            }
        }
        Err(e) => warn!(error = %e, "queue: NATS publish failed"),
    }
}

async fn read_i64(pool: &sqlx::PgPool, key: &str, fallback: i64) -> i64 {
    settings_repo::get_setting(pool, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_i64())
        .unwrap_or(fallback)
}

async fn read_string(pool: &sqlx::PgPool, key: &str) -> Option<String> {
    settings_repo::get_setting(pool, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    // Without a URL there must be no connection attempt at all: async-nats reports a
    // refused localhost connect as success and then logs its reconnect loop forever.
    #[tokio::test]
    async fn unset_nats_url_publishes_nowhere() {
        let publisher = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            super::connect_nats(None),
        )
        .await
        .expect("no connection attempt without NATS_URL");
        assert!(publisher.is_none());
    }
}
