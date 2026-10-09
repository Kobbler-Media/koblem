//! Per-lane engine reachability, polled once and read by every admission.
//!
//! 🔴 Three different `/health` shapes across the stack, and one parser will not
//! do. comfy-api publishes `comfy.reachable`, `busy`, `draining` and a probe
//! block; the sd.cpp supervisor publishes `status`, `busy`, `in_flight`,
//! `loaded_model`, `worker_pid`; the in-process sd.cpp path publishes a third
//! subset. Only comfy-api publishes memory numbers at all.
//!
//! 🔴 And `comfy.reachable: null` means "the background probe has not run yet",
//! NOT "down". The difference decides whether someone gets woken at 3am, so
//! unknown maps to reachable here and an `EngineUnreachable` never fires on it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tracing::debug;

use super::admit::HealthView;
use super::lane::Lane;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Default)]
pub struct LaneHealth {
    /// Set only when a probe POSITIVELY saw the engine down.
    pub down: Option<String>,
    /// The engine reports work in flight. For comfy-api this is the union with
    /// ComfyUI's own queue, which is the only reading that sees a render started
    /// from its web UI.
    pub busy: bool,
    /// Admission is closed. A drain sets it and only an unload clears it.
    pub draining: bool,
}

/// The shared fields worth reading across all three shapes. Everything is
/// `#[serde(default)]` because each family publishes a different subset, and a
/// missing field must never be read as a zero.
#[derive(Debug, Default, Deserialize)]
struct HealthBody {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    busy: Option<bool>,
    #[serde(default)]
    in_flight: Option<i64>,
    #[serde(default)]
    draining: Option<bool>,
    #[serde(default)]
    comfy: Option<ComfyBlock>,
    /// longcat-rig only, and the one field in this whole surface that reports a
    /// WEDGED engine. 🔴 The sd.cpp shapes cannot: their `status` is the literal
    /// `"ok"` on every path including a dead worker, so an `EngineUnreachable`
    /// there can only ever come from transport failure.
    #[serde(default)]
    fatal: Option<String>,
    /// longcat-rig only: it is queued behind its OWN FIFO, which koblem cannot
    /// see. Counts toward busy for the same reason ComfyUI's web-UI renders do.
    #[serde(default)]
    queued: Option<i64>,
    /// longcat-rig only: parked on its internal GPU lock. Busy in every sense
    /// that matters to admission, even though nothing is rendering yet.
    #[serde(default)]
    waiting_for_gpu_lock: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct ComfyBlock {
    /// 🔴 `null` until the probe has run once. Not-yet-known is not down.
    #[serde(default)]
    reachable: Option<bool>,
}

pub struct EngineHealth {
    /// Lane → base URL. Only lanes with a configured engine are polled.
    urls: Vec<(Lane, String)>,
    state: Mutex<HashMap<Lane, LaneHealth>>,
    http: reqwest::Client,
}

impl EngineHealth {
    pub fn new(urls: Vec<(Lane, String)>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .unwrap_or_default();
        EngineHealth {
            urls,
            state: Mutex::new(HashMap::new()),
            http,
        }
    }

    pub fn lanes(&self) -> Vec<Lane> {
        self.urls.iter().map(|(l, _)| *l).collect()
    }

    pub fn snapshot(&self, lane: Lane) -> LaneHealth {
        self.state
            .lock()
            .unwrap()
            .get(&lane)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn poll_once(&self) {
        for (lane, base) in &self.urls {
            let url = format!("{}/health", base.trim_end_matches('/'));
            let body: Option<HealthBody> = match self.http.get(&url).send().await {
                Ok(r) if r.status().is_success() => r.json().await.ok(),
                Ok(r) => {
                    self.record(
                        *lane,
                        LaneHealth {
                            down: Some(format!("health returned {}", r.status().as_u16())),
                            ..Default::default()
                        },
                    );
                    continue;
                }
                Err(e) => {
                    self.record(
                        *lane,
                        LaneHealth {
                            down: Some(short_error(&e)),
                            ..Default::default()
                        },
                    );
                    continue;
                }
            };
            let Some(body) = body else {
                // A 200 we cannot parse is not a down engine. Treat it as unknown
                // and keep the previous reading rather than inventing a failure.
                debug!(lane = lane.as_str(), "queue: unparseable /health body");
                continue;
            };

            let comfy_down = body
                .comfy
                .as_ref()
                .and_then(|c| c.reachable)
                .is_some_and(|r| !r);
            let status_down = body.status.as_deref().is_some_and(|s| s != "ok");
            let fatal = body.fatal.as_deref().filter(|f| !f.trim().is_empty());
            let down = if comfy_down {
                Some("ComfyUI unreachable".to_string())
            } else if let Some(f) = fatal {
                Some(f.to_string())
            } else if status_down {
                body.status.clone()
            } else {
                None
            };

            self.record(
                *lane,
                LaneHealth {
                    down,
                    // Four shapes, one reading. comfy-api's `busy` is the union
                    // with ComfyUI's own queue; the sd.cpp pair report `in_flight`;
                    // longcat-rig adds its own FIFO depth and its GPU-lock wait,
                    // both of which are work koblem did not schedule and cannot see.
                    busy: body.busy.unwrap_or(false)
                        || body.in_flight.unwrap_or(0) > 0
                        || body.queued.unwrap_or(0) > 0
                        || body.waiting_for_gpu_lock.unwrap_or(false),
                    draining: body.draining.unwrap_or(false),
                },
            );
        }
    }

    pub(super) fn record(&self, lane: Lane, h: LaneHealth) {
        self.state.lock().unwrap().insert(lane, h);
    }

    pub async fn run(self: std::sync::Arc<Self>) {
        loop {
            self.poll_once().await;
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl HealthView for EngineHealth {
    fn unreachable(&self, lane: Lane) -> Option<String> {
        let h = self.snapshot(lane);
        if let Some(d) = h.down {
            return Some(d);
        }
        // 🔴 Draining means admission is CLOSED and every generate answers 503.
        // Only an unload reopens it. Waiting is right; submitting into it is not.
        h.draining.then(|| "engine is draining".to_string())
    }

    fn engine_busy(&self, lane: Lane) -> bool {
        self.snapshot(lane).busy
    }
}

fn short_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "health probe timed out".to_string()
    } else if e.is_connect() {
        "connection refused".to_string()
    } else {
        "health probe failed".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> HealthBody {
        serde_json::from_str(json).unwrap()
    }

    /// 🔴 The one that decides whether someone gets paged: comfy-api reports
    /// `reachable: null` until its background probe has run once, and that is
    /// "not yet known", not "down".
    #[test]
    fn a_null_reachable_is_not_a_down_engine() {
        let h = EngineHealth::new(vec![(Lane::ComfyApi, "http://x".into())]);
        let body = parse(r#"{"status":"ok","busy":false,"comfy":{"reachable":null}}"#);
        let down = body
            .comfy
            .as_ref()
            .and_then(|c| c.reachable)
            .is_some_and(|r| !r);
        assert!(!down, "unknown must not read as down");
        h.record(
            Lane::ComfyApi,
            LaneHealth {
                down: None,
                busy: false,
                draining: false,
            },
        );
        assert!(h.unreachable(Lane::ComfyApi).is_none());
    }

    /// A positively-seen-down ComfyUI IS a down engine.
    #[test]
    fn a_false_reachable_is_a_down_engine() {
        let body = parse(r#"{"status":"ok","comfy":{"reachable":false}}"#);
        assert_eq!(body.comfy.and_then(|c| c.reachable), Some(false));
    }

    /// The three families publish different subsets and a missing field must not
    /// be read as a zero. The sd.cpp supervisor has no `comfy` block and reports
    /// in-flight as a count, not a bool.
    #[test]
    fn the_fleet_shape_parses_too() {
        let body = parse(r#"{"status":"ok","busy":true,"in_flight":1,"loaded_model":"flux2"}"#);
        assert!(body.comfy.is_none());
        assert_eq!(body.in_flight, Some(1));
        assert_eq!(body.busy, Some(true));
        let none = parse(r#"{"status":"ok"}"#);
        assert_eq!(none.busy, None, "absent is not false");
        assert_eq!(none.in_flight, None);
    }

    /// 🔴 A draining engine answers 503 to every generate and ONLY an unload
    /// reopens it. Admission must wait, not submit into it.
    #[test]
    fn a_draining_engine_is_not_admissible() {
        let h = EngineHealth::new(vec![(Lane::Flux2, "http://x".into())]);
        h.record(
            Lane::Flux2,
            LaneHealth {
                down: None,
                busy: false,
                draining: true,
            },
        );
        assert_eq!(
            h.unreachable(Lane::Flux2).as_deref(),
            Some("engine is draining")
        );
    }

    /// 🔴 The FOURTH shape. longcat-rig is neither sd.cpp path: `worker_pid` is
    /// `-1` rather than `null` when unloaded, and it carries `fatal`,
    /// `waiting_for_gpu_lock` and its own `queued` depth, which nothing else has.
    /// A single "the sd.cpp shape" parser would drop all three.
    #[test]
    fn the_rigs_own_shape_parses_and_its_fifo_counts_as_busy() {
        let body = parse(
            r#"{"status":"ok","role":"parent","worker_isolation":true,"worker_pid":-1,
                "loaded":false,"busy":false,"in_flight":0,"queued":2,
                "waiting_for_gpu_lock":false,"draining":false,"fatal":""}"#,
        );
        assert_eq!(body.queued, Some(2));
        assert_eq!(body.fatal.as_deref(), Some(""));
        // Work in ITS fifo is work koblem cannot see, exactly like a render
        // started from ComfyUI's own web UI.
        assert!(body.busy.unwrap_or(false) || body.queued.unwrap_or(0) > 0);
    }

    /// 🔴 `fatal` is the only field in the whole surface that reports a WEDGED
    /// engine. The sd.cpp shapes answer `status: "ok"` on every path including a
    /// dead worker, so without this the rig's own alarm would be invisible.
    #[test]
    fn a_fatal_rig_is_down_even_though_status_says_ok() {
        let body = parse(r#"{"status":"ok","busy":false,"fatal":"CUDA error: out of memory"}"#);
        let fatal = body.fatal.as_deref().filter(|f| !f.trim().is_empty());
        assert_eq!(fatal, Some("CUDA error: out of memory"));
        // ...and an EMPTY string is the healthy value, not a failure.
        let ok = parse(r#"{"status":"ok","fatal":""}"#);
        assert!(
            ok.fatal
                .as_deref()
                .filter(|f| !f.trim().is_empty())
                .is_none(),
            "an empty `fatal` is the normal state"
        );
    }

    /// An idle sd.cpp engine reports `loaded:false` with a null worker pid, and
    /// that is the steady state for most of the day — not a fault.
    #[test]
    fn an_unloaded_sdcpp_worker_is_not_down() {
        let h = EngineHealth::new(vec![(Lane::Flux2, "http://x".into())]);
        let body = parse(
            r#"{"status":"ok","busy":false,"in_flight":0,"accepts_part_refs":true,
                "draining":false,"loaded":false,"loaded_model":"","worker_pid":null}"#,
        );
        assert_eq!(body.status.as_deref(), Some("ok"));
        assert!(body.fatal.is_none());
        h.record(Lane::Flux2, LaneHealth::default());
        assert!(h.unreachable(Lane::Flux2).is_none());
    }

    /// An unpolled lane is healthy, not down — the dispatcher must not refuse
    /// every job for the five seconds before the first probe lands.
    #[test]
    fn an_unpolled_lane_is_not_down() {
        let h = EngineHealth::new(vec![]);
        assert!(h.unreachable(Lane::ComfyApi).is_none());
        assert!(!h.engine_busy(Lane::ComfyApi));
    }
}
