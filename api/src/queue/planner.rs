//! Queue-driven unloads: the thing the queue can do that the gate cannot.
//!
//! Because the dispatcher can see the next N jobs, each tick it can *plan* rather
//! than react:
//!
//! 1. **Don't unload what's next.** If the resident on a card is the VRAM claim of
//!    the next admissible job, leave it warm — the gate has no way to know that.
//! 2. **Unload early.** If a resident is wanted by nothing in the queue, drain and
//!    unload it the moment the running job finishes, so the next job's admission
//!    does not pay the drain.
//! 3. **Suppress `rehome_standing`** while pending demand would evict it. Handled
//!    inside the gate via `set_pending_demand`; the dispatcher pushes it each tick.
//!
//! None of these change placement. The gate stays authoritative.
//!
//! 🔴 THE RULE THAT CONSTRAINS ALL OF IT: on both engine families `drain` closes
//! admission and **only `unload` reopens it**, and koblem calls `/v1/admin/load`
//! nowhere. A planner that drains speculatively and then changes its mind leaves
//! the engine answering 503 to every render until the container restarts. So this
//! never drains on its own — it calls `unload_if_idle`, which is the gate's
//! existing eviction path and always completes drain→force-unload.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kob_gpu_gate::{GpuLock, ServiceKind};
use koblem_client::types::{Job, JobState};
use tracing::info;

use super::admit::service_of;

/// How long a resident must stay continuously reclaimable before rule 2 unloads it.
/// TTS gets [`TTS_RECLAIM_IDLE_GRACE`] instead.
///
/// Some services take work the queue never sees. kobbler's read-along sends a
/// chapter to TTS as back-to-back passage requests, and between two of them the
/// worker holds no guard. With no grace the planner unloaded it in that gap, so
/// every passage paid a ~2.5 s cold load, and an unload landing as the next
/// passage began killed it outright.
const RECLAIM_IDLE_GRACE: Duration = Duration::from_secs(30);

/// TTS reloads in ~2.5 s, and a reader pausing between chapters or a koblem user
/// generating line after line comes back within minutes. Matches breeze's own
/// 300 s idle unload. Idle still means unheld, so a job that needs the card
/// evicts it through the gate as before; only the queue's early unload waits.
const TTS_RECLAIM_IDLE_GRACE: Duration = Duration::from_secs(300);

fn reclaim_grace(kind: ServiceKind) -> Duration {
    match kind {
        ServiceKind::Tts => TTS_RECLAIM_IDLE_GRACE,
        _ => RECLAIM_IDLE_GRACE,
    }
}

#[derive(Clone)]
pub struct Planner {
    gate: GpuLock,
    /// When each resident was first seen reclaimable in an unbroken run of ticks.
    idle_since: Arc<Mutex<HashMap<ServiceKind, Instant>>>,
}

impl Planner {
    pub fn new(gate: GpuLock) -> Self {
        Planner {
            gate,
            idle_since: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn plan(&self, pending: &[Job], live: &[Job]) {
        let due = self.due(self.reclaimable(pending, live), Instant::now());
        for kind in due {
            if self.gate.unload_if_idle(kind).await {
                info!(
                    service = kind.as_str(),
                    "queue: unloaded a resident nothing in the queue wants"
                );
            }
        }
    }

    /// The `candidates` that have been reclaimable for at least
    /// its [`reclaim_grace`]. A service that drops out of the candidates (a
    /// request took a guard, a job wants it, it went away) starts over.
    fn due(&self, candidates: Vec<ServiceKind>, now: Instant) -> Vec<ServiceKind> {
        let mut seen = self.idle_since.lock().unwrap();
        seen.retain(|kind, _| candidates.contains(kind));
        candidates
            .into_iter()
            .filter(|kind| {
                let since = *seen.entry(*kind).or_insert(now);
                now.duration_since(since) >= reclaim_grace(*kind)
            })
            .collect()
    }

    /// Residents that are warm, idle, and wanted by nothing pending or live.
    ///
    /// Deliberately conservative — the cost of getting this wrong is a cold load
    /// on the next request, and the cost of getting it wrong the other way is a
    /// drain that a queued job then has to pay for anyway.
    pub fn reclaimable(&self, pending: &[Job], live: &[Job]) -> Vec<ServiceKind> {
        let wanted: Vec<ServiceKind> = pending
            .iter()
            .filter(|j| j.state == JobState::Queued)
            .chain(live.iter())
            .map(|j| service_of(j.kind))
            .collect();

        let mut out = Vec::new();
        for card in self.gate.occupancy() {
            for (kind, _peak) in card.residents {
                // 🔴 Never a standing service (the LLM, the text embedder). They are
                // always-resident; unloading one here would fight `rehome_standing`
                // every tick, and the queue's lever on them is `set_pending_demand`
                // (or a heavy job's eviction), not an unload.
                if kind.is_standing() {
                    continue;
                }
                // 🔴 Never pixal3d. The gate deliberately excludes it from unloads:
                // it has its own FIFO and admits work koblem cannot see, so an
                // unload racing a job it has already accepted kills that render.
                if matches!(kind, ServiceKind::Pixal3d) {
                    continue;
                }
                if wanted.contains(&kind) {
                    continue; // rule 1: it is what's next, leave it warm
                }
                if self.gate.holders(kind) > 0 {
                    continue; // busy, and `unload_if_idle` would refuse anyway
                }
                out.push(kind);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kob_gpu_gate::{Card, EngineDesc};
    use koblem_client::types::{JobKind, JobProgress};

    const A: &str = "GPU-a";

    fn gate_with(residents: &[(ServiceKind, u32)]) -> GpuLock {
        let g = GpuLock::new();
        g.set_cards(vec![Card {
            uuid: A.into(),
            total_mb: 48000,
            fast: true,
            enabled: true,
        }]);
        for (kind, peak) in residents {
            g.set_engine_config(
                *kind,
                EngineDesc {
                    peak_mb: *peak,
                    home_gpu: A.into(),
                    candidate_gpus: vec![A.into()],
                    evictable_by: vec![],
                    relocatable: false,
                    rules_json: String::new(),
                },
            );
            g.observe(*kind, A);
        }
        g
    }

    fn job(id: &str, kind: JobKind, state: JobState) -> Job {
        Job {
            id: id.into(),
            kind,
            state,
            band: 1,
            queue_rank: 1.0,
            target_gpu: None,
            force_pin: false,
            server_job_id: None,
            progress: JobProgress::default(),
            depends_on: None,
            created_by: "t".into(),
            blocked_reason: None,
            blocked_since: None,
            error: None,
            warnings: vec![],
            queued_ms: None,
            admit_ms: None,
            render_ms: None,
            units_total: None,
            units_done: 0,
            shape_key: None,
            label: String::new(),
            created_at: "2026-08-16T00:00:00Z".into(),
            started_at: None,
            finished_at: None,
            attempt: 1,
            retry_of: None,
            blocked_history: vec![],
            eta_start: None,
            eta_finish: None,
            eta_finish_p90: None,
            estimate_samples: 0,
        }
    }

    /// Rule 1: the resident the next queued job needs stays warm. This is the
    /// whole point — the gate cannot see what is coming, and unloading here means
    /// the next job pays a cold load for nothing.
    #[test]
    fn what_is_next_stays_warm() {
        let g = gate_with(&[(ServiceKind::Krea2, 8000)]);
        let p = Planner::new(g);
        let pending = vec![job("j1", JobKind::Krea2, JobState::Queued)];
        assert!(p.reclaimable(&pending, &[]).is_empty());
    }

    /// Rule 2: a resident nothing in the queue wants is reclaimed, so the next
    /// job's admission does not pay its drain.
    #[test]
    fn a_resident_nothing_wants_is_reclaimed() {
        let g = gate_with(&[(ServiceKind::Krea2, 8000)]);
        let p = Planner::new(g);
        let pending = vec![job("j1", JobKind::Flux2, JobState::Queued)];
        assert_eq!(p.reclaimable(&pending, &[]), vec![ServiceKind::Krea2]);
    }

    /// A job already running keeps its own service warm — it is using it.
    #[test]
    fn a_running_job_protects_its_own_resident() {
        let g = gate_with(&[(ServiceKind::Krea2, 8000)]);
        let p = Planner::new(g);
        let live = vec![job("j1", JobKind::Krea2, JobState::Running)];
        assert!(p.reclaimable(&[], &live).is_empty());
    }

    /// 🔴 Never a standing service and never pixal3d. The LLM's lever is `set_pending_demand`,
    /// and pixal3d admits work koblem cannot see — an unload racing a job it has
    /// already accepted kills that render.
    #[test]
    fn the_llm_and_the_rig_are_never_reclaimed() {
        let g = gate_with(&[
            (ServiceKind::Llm, 10000),
            (ServiceKind::Pixal3d, 11000),
            (ServiceKind::TextEmbed, 1600),
        ]);
        let p = Planner::new(g);
        assert!(p.reclaimable(&[], &[]).is_empty());
    }

    /// Rule 2 waits out the grace: a resident is not unloaded on the first tick it
    /// looks idle, only once it has stayed idle, and any tick where it is not
    /// reclaimable restarts the clock.
    #[test]
    fn reclaim_waits_out_the_idle_grace() {
        let p = Planner::new(gate_with(&[]));
        let t0 = Instant::now();
        let krea = vec![ServiceKind::Krea2];

        assert!(
            p.due(krea.clone(), t0).is_empty(),
            "reclaimed on first sight"
        );
        assert!(
            p.due(
                krea.clone(),
                t0 + RECLAIM_IDLE_GRACE - Duration::from_secs(1)
            )
            .is_empty()
        );
        assert_eq!(p.due(krea.clone(), t0 + RECLAIM_IDLE_GRACE), krea);

        // A job takes a guard for one tick: the clock restarts.
        let t1 = t0 + RECLAIM_IDLE_GRACE + Duration::from_secs(5);
        assert!(p.due(vec![], t1).is_empty());
        assert!(p.due(krea.clone(), t1).is_empty());
        assert_eq!(p.due(krea.clone(), t1 + RECLAIM_IDLE_GRACE), krea);
    }

    /// TTS waits five minutes, not thirty seconds: between read-along passages
    /// and between chapters it is idle but about to be used again.
    #[test]
    fn tts_waits_out_the_longer_grace() {
        let p = Planner::new(gate_with(&[]));
        let t0 = Instant::now();
        let tts = vec![ServiceKind::Tts];

        assert!(p.due(tts.clone(), t0).is_empty());
        assert!(p.due(tts.clone(), t0 + RECLAIM_IDLE_GRACE).is_empty());
        assert_eq!(p.due(tts.clone(), t0 + TTS_RECLAIM_IDLE_GRACE), tts);
    }

    /// An empty queue reclaims everything idle — that is the overnight case after
    /// the last render finishes.
    #[test]
    fn an_empty_queue_reclaims_the_idle_residents() {
        let g = gate_with(&[(ServiceKind::Krea2, 8000), (ServiceKind::Flux, 9000)]);
        let p = Planner::new(g);
        let mut got = p.reclaimable(&[], &[]);
        got.sort_by_key(|k| k.as_str());
        assert_eq!(got, vec![ServiceKind::Flux, ServiceKind::Krea2]);
    }
}
