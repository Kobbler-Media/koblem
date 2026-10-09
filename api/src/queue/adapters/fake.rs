//! A scriptable [`JobRunner`] for the dispatcher's tests.
//!
//! 🔴 There is no engine on this box, and a dispatcher is ordering, time and
//! concurrency — exactly what breaks silently and gets discovered at 3am. So the
//! safety net is this plus the injectable clock plus the fake gate: ordering,
//! backfill, head reservation, skip-with-reason, preempt-and-resume, starvation,
//! restart-reattach ordering and double-submit dedupe are all assertable with no
//! GPU and no network.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{Job, JobKind, JobProgress, Preempt};
use serde_json::{Value, json};

use crate::queue::admit::service_of;
use crate::queue::runner::{Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};

/// What a scripted run should do.
#[derive(Debug, Clone)]
pub enum Script {
    /// Finish immediately.
    Succeed,
    Fail(&'static str),
    /// Report `units` units of progress, checking the cancel token between each,
    /// then finish. Lets a test preempt or cancel at a known point.
    Units(i32),
    /// Park until the token is tripped, then honour it.
    WaitForInterrupt,
}

/// What actually happened, for the test to assert on.
#[derive(Debug, Clone, Default)]
pub struct Log {
    pub ran: Vec<String>,
    pub reattached: Vec<String>,
    pub cancelled: Vec<String>,
    /// Rows the queue retired before they ever ran, in the order the funnel reported them.
    pub abandoned: Vec<String>,
    /// The card each run was handed. `None` when the gate was unconfigured.
    pub cards: Vec<Option<String>>,
    /// Whether each run released the GPU before returning.
    pub released: Vec<bool>,
}

pub struct FakeRunner {
    kind: JobKind,
    script: Mutex<Script>,
    pub log: Arc<Mutex<Log>>,
}

impl FakeRunner {
    pub fn new(kind: JobKind, script: Script) -> Self {
        FakeRunner {
            kind,
            script: Mutex::new(script),
            log: Arc::new(Mutex::new(Log::default())),
        }
    }

    pub fn set_script(&self, s: Script) {
        *self.script.lock().unwrap() = s;
    }

    pub fn log(&self) -> Log {
        self.log.lock().unwrap().clone()
    }
}

#[async_trait]
impl JobRunner for FakeRunner {
    fn kind(&self) -> JobKind {
        self.kind
    }

    fn service(&self) -> ServiceKind {
        service_of(self.kind)
    }

    fn preempt(&self) -> Preempt {
        Preempt::AtCheckpoint
    }

    fn shape_key(&self, payload: &Value) -> String {
        payload
            .get("shape")
            .and_then(Value::as_str)
            .unwrap_or("fake/default")
            .to_string()
    }

    fn units(&self, payload: &Value) -> Option<i32> {
        payload
            .get("units")
            .and_then(Value::as_i64)
            .map(|n| n as i32)
    }

    fn label(&self, _payload: &Value) -> String {
        format!("fake {}", self.kind.as_str())
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        {
            let mut log = self.log.lock().unwrap();
            log.ran.push(ctx.job.id.clone());
            log.cards.push(ctx.gpu.clone());
        }
        let script = self.script.lock().unwrap().clone();
        let out = match script {
            Script::Succeed => Ok(JobOutcome::Done { warnings: vec![] }),
            Script::Fail(e) => Err(e.to_string()),
            Script::Units(n) => {
                let start = ctx
                    .resume
                    .as_ref()
                    .and_then(|r| r["done"].as_i64())
                    .unwrap_or(0) as i32;
                for i in start..n {
                    match ctx.cancel.get() {
                        Interrupt::Cancel => return Ok(JobOutcome::Cancelled),
                        // The checkpoint is BETWEEN units, which is what
                        // `AtCheckpoint` costs: at most one unit.
                        Interrupt::Preempt => {
                            return Ok(JobOutcome::Preempted {
                                resume: json!({ "done": i }),
                            });
                        }
                        Interrupt::None => {}
                    }
                    ctx.progress(
                        JobProgress {
                            phase: Some("rendering".into()),
                            ..Default::default()
                        },
                        i + 1,
                    )
                    .await;
                }
                Ok(JobOutcome::Done { warnings: vec![] })
            }
            Script::WaitForInterrupt => loop {
                match ctx.cancel.get() {
                    Interrupt::Cancel => return Ok(JobOutcome::Cancelled),
                    Interrupt::Preempt => {
                        return Ok(JobOutcome::Preempted {
                            resume: json!({ "done": 1 }),
                        });
                    }
                    Interrupt::None => tokio::task::yield_now().await,
                }
            },
        };
        // The real adapters release at the engine/disk boundary; this one does it
        // here so `released` asserts the ctx plumbing, not the adapter's manners.
        ctx.release_gpu();
        self.log.lock().unwrap().released.push(ctx.gpu_released());
        out
    }

    async fn reattach(&self, ctx: JobCtx) -> RunResult {
        self.log.lock().unwrap().reattached.push(ctx.job.id.clone());
        ctx.release_gpu();
        Ok(JobOutcome::Done { warnings: vec![] })
    }

    async fn abandon(&self, job_id: &str, _payload: &Value) {
        self.log.lock().unwrap().abandoned.push(job_id.to_string());
    }

    async fn cancel(&self, job: &Job) -> Result<(), String> {
        self.log.lock().unwrap().cancelled.push(job.id.clone());
        Ok(())
    }
}
