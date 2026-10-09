//! NATS consumer for async review jobs.
//!
//! When `NATS_URL` is configured, koblem subscribes to the JOBS stream for
//! `jobs.koblem.review_candidates` messages, processes them through the same
//! review pipeline as the HTTP endpoint, and publishes results to
//! `events.review.completed`.
//!
//! Jobs are priority-ordered: download-verify > backlog > rss. When multiple
//! jobs are pending while the GPU is busy, the highest-priority job runs next.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use koblem_client::types::{ReviewCompletedEvent, ReviewJob};
use nats_jobs::{Event, Job, NatsClient, NatsConfig};
use tracing::{debug, error, info, warn};

use crate::AppState;
use crate::review_pipeline;

/// Map priority string to a numeric rank (higher = processed first).
fn priority_rank(priority: &str) -> u8 {
    match priority {
        "download-verify" => 3,
        "backlog" => 2,
        "rss" => 1,
        _ => 0,
    }
}

/// Wrapper around ReviewJob that implements Ord for BinaryHeap ordering.
struct PrioritizedJob {
    rank: u8,
    /// Arrival order — lower = earlier. Used to break ties (FIFO within same priority).
    seq: u64,
    job: ReviewJob,
}

impl PartialEq for PrioritizedJob {
    fn eq(&self, other: &Self) -> bool {
        self.rank == other.rank && self.seq == other.seq
    }
}

impl Eq for PrioritizedJob {}

impl PartialOrd for PrioritizedJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PrioritizedJob {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher rank first, then earlier arrival first (lower seq)
        self.rank
            .cmp(&other.rank)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// Start the NATS review consumer. Blocks forever (intended for `tokio::spawn`).
///
/// Spawns a receiver task that drains NATS messages into a channel, and a
/// processor task that collects pending jobs into a priority heap before each
/// LLM call. This ensures higher-priority jobs (download-verify > backlog > rss)
/// are processed first when the GPU is busy.
pub async fn start_review_consumer(state: AppState) {
    let nats_url = match state.config.nats_url.as_deref() {
        Some(url) if !url.is_empty() => url.to_string(),
        _ => return,
    };

    info!(url = %nats_url, "Starting NATS review consumer");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ReviewJob>();

    // Spawn the NATS receiver task
    tokio::spawn(nats_receiver(nats_url, tx));

    // Run the processor on this task (holds the AppState)
    process_jobs(rx, state).await;
}

/// Receive NATS messages and forward parsed ReviewJobs to the processor channel.
async fn nats_receiver(nats_url: String, tx: tokio::sync::mpsc::UnboundedSender<ReviewJob>) {
    let mut backoff = std::time::Duration::from_secs(1);

    'reconnect: loop {
        let config = NatsConfig::from_env("koblem-review");
        let nats = match NatsClient::connect(&config).await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to connect to NATS at {nats_url}: {e}, retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                continue 'reconnect;
            }
        };

        info!("Connected to NATS");
        backoff = std::time::Duration::from_secs(1);

        let jetstream = nats.jetstream().clone();

        let stream = match jetstream.get_stream("JOBS").await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "JOBS stream not found: {e}, retrying in {backoff:?} (kobload may not be running yet)"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                continue 'reconnect;
            }
        };

        let consumer_config = async_nats::jetstream::consumer::pull::Config {
            durable_name: Some("koblem-review".to_string()),
            filter_subject: "jobs.koblem.review_candidates".to_string(),
            ..Default::default()
        };

        let consumer = match stream
            .get_or_create_consumer("koblem-review", consumer_config.clone())
            .await
        {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to create review consumer: {e}, retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                continue 'reconnect;
            }
        };

        let mut messages = match consumer.messages().await {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to get review messages stream: {e}, retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                continue 'reconnect;
            }
        };

        info!("Subscribed to review jobs on jobs.koblem.review_candidates");
        backoff = std::time::Duration::from_secs(1);

        use futures::StreamExt;
        while let Some(msg_result) = messages.next().await {
            let msg = match msg_result {
                Ok(m) => m,
                Err(e) => {
                    error!("Error receiving review job message: {e}, reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                    continue 'reconnect;
                }
            };

            // Ack immediately — LLM inference can take minutes and we don't
            // want NATS to redeliver. If koblem crashes mid-review, the job
            // is lost (acceptable for Phase 1 log-only).
            let _ = msg.ack().await;

            let envelope: Job = match serde_json::from_slice(&msg.payload) {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to deserialize job envelope: {e}");
                    continue;
                }
            };

            let job: ReviewJob = match serde_json::from_value(envelope.payload) {
                Ok(j) => j,
                Err(e) => {
                    error!("Failed to deserialize ReviewJob payload: {e}");
                    continue;
                }
            };

            info!(
                correlation_id = %job.correlation_id,
                candidates = job.request.candidates.len(),
                wanted_title = %job.request.wanted_title,
                priority = %job.priority,
                "Received review job, queuing"
            );

            if tx.send(job).is_err() {
                error!("Processor channel closed, stopping receiver");
                return;
            }
        }

        error!("Review messages stream ended, reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
    }
}

/// Process review jobs from the channel, using a priority heap to reorder
/// pending jobs when the GPU is busy.
async fn process_jobs(mut rx: tokio::sync::mpsc::UnboundedReceiver<ReviewJob>, state: AppState) {
    let mut seq: u64 = 0;
    let mut heap: BinaryHeap<PrioritizedJob> = BinaryHeap::new();

    // Connect to NATS for publishing results (separate from the receiver's connection)
    let jetstream = loop {
        let config = NatsConfig::from_env("koblem-review-publisher");
        match NatsClient::connect(&config).await {
            Ok(nats) => break nats.jetstream().clone(),
            Err(e) => {
                error!("Processor failed to connect to NATS: {e}, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };

    loop {
        // If heap is empty, block until at least one job arrives
        if heap.is_empty() {
            match rx.recv().await {
                Some(job) => {
                    heap.push(PrioritizedJob {
                        rank: priority_rank(&job.priority),
                        seq,
                        job,
                    });
                    seq += 1;
                }
                None => {
                    error!("Job channel closed, stopping processor");
                    return;
                }
            }
        }

        // Drain any additional pending jobs (non-blocking) to reorder by priority
        while let Ok(job) = rx.try_recv() {
            heap.push(PrioritizedJob {
                rank: priority_rank(&job.priority),
                seq,
                job,
            });
            seq += 1;
        }

        let pending = heap.len();
        let pj = heap.pop().unwrap();
        let job = pj.job;
        let correlation_id = job.correlation_id;

        if pending > 1 {
            info!(
                %correlation_id,
                priority = %job.priority,
                pending = pending - 1,
                "Processing highest-priority job ({} remaining in queue)",
                pending - 1,
            );
        }

        info!(
            %correlation_id,
            candidates = job.request.candidates.len(),
            wanted_title = %job.request.wanted_title,
            priority = %job.priority,
            "Processing review job"
        );

        let queue_start = std::time::Instant::now();

        match review_pipeline::run_review(&state, job.request, true, "review_pipeline:nats").await {
            Ok(response) => {
                let queue_wait_ms = queue_start.elapsed().as_millis() as u64;

                info!(
                    %correlation_id,
                    agreed = response.agreed.len(),
                    vetoed = response.vetoed.len(),
                    promoted = response.promoted.len(),
                    queue_wait_ms,
                    llm_ms = ?response.llm_inference_ms,
                    "Review job completed"
                );

                let event = ReviewCompletedEvent {
                    correlation_id,
                    response,
                    queue_wait_ms: Some(queue_wait_ms),
                };

                publish_review_completed(&jetstream, &event).await;
            }
            Err(e) => {
                error!(
                    %correlation_id,
                    error = %e,
                    "Review job failed"
                );
            }
        }
    }
}

/// Publish a review-completed event to `events.review.completed`.
async fn publish_review_completed(
    jetstream: &async_nats::jetstream::Context,
    event: &ReviewCompletedEvent,
) {
    let payload = match serde_json::to_value(event) {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to serialize ReviewCompletedEvent: {e}");
            return;
        }
    };
    let envelope = Event::new("review.completed", payload, "koblem");

    let bytes = match serde_json::to_vec(&envelope) {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to serialize event envelope: {e}");
            return;
        }
    };

    match jetstream
        .publish("events.review.completed", bytes.into())
        .await
    {
        Ok(ack) => {
            if let Err(e) = ack.await {
                warn!("NATS ack failed for review.completed event: {e}");
            } else {
                debug!(
                    correlation_id = %event.correlation_id,
                    "Published review.completed event"
                );
            }
        }
        Err(e) => {
            warn!("Failed to publish review.completed event: {e}");
        }
    }
}
