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

use kob_gpu_gate::{GpuLock, ServiceKind};
use koblem_client::types::{Job, JobState};
use tracing::info;

use super::admit::service_of;

#[derive(Clone)]
pub struct Planner {
    gate: GpuLock,
}

impl Planner {
    pub fn new(gate: GpuLock) -> Self {
        Planner { gate }
    }

    pub async fn plan(&self, pending: &[Job], live: &[Job]) {
        for kind in self.reclaimable(pending, live) {
            if self.gate.unload_if_idle(kind).await {
                info!(
                    service = kind.as_str(),
                    "queue: unloaded a resident nothing in the queue wants"
                );
            }
        }
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
                // 🔴 Never the LLM. It is the always-resident service; unloading it
                // here would fight `rehome_standing` every tick, and the queue's
                // lever on it is `set_pending_demand`, not an unload.
                if matches!(kind, ServiceKind::Llm) {
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

    /// 🔴 Never the LLM and never pixal3d. The LLM's lever is `set_pending_demand`,
    /// and pixal3d admits work koblem cannot see — an unload racing a job it has
    /// already accepted kills that render.
    #[test]
    fn the_llm_and_the_rig_are_never_reclaimed() {
        let g = gate_with(&[(ServiceKind::Llm, 10000), (ServiceKind::Pixal3d, 11000)]);
        let p = Planner::new(g);
        assert!(p.reclaimable(&[], &[]).is_empty());
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
