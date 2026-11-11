//! Lanes: the second admission axis, independent of VRAM.
//!
//! 🔴 VRAM is not the only resource. `upscale`, `music3`, `h3` and `ltx25` are
//! four plugins on ONE comfy-api process in front of ONE ComfyUI with a
//! single-slot worker — they cannot render at once at any VRAM. Before this they
//! were all made `acquire_for_minimax_h3()`, i.e. a *budget line* abused to mean
//! *mutex*.
//!
//! Get this wrong and the queue happily admits two comfy jobs that then serialise
//! invisibly inside comfy-api, with koblem holding a VRAM reservation it cannot
//! use.
//!
//! And it is not only comfy. The gate's placer *reuses* a resident placement for a
//! second job of the same kind (refcount, no new budget) — correct for VRAM, since
//! it is one worker with one set of weights, and wrong for work. So **every kind
//! needs a lane**, keyed by the engine *deployment* rather than by the kind.

use std::collections::HashMap;

use koblem_client::types::JobKind;

/// An exclusive backend slot. The identity is the deployment, not the kind:
/// four kinds share `ComfyApi`, two share `LongcatRig`, and each sd.cpp
/// deployment is its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// One process, one ComfyUI, one worker — behind `ltx25`, `h3`, `music3`
    /// and `upscale`.
    ComfyApi,
    /// `pixal3d` rig + motion. 🔴 Advisory only: the service has its own FIFO and
    /// admits work koblem cannot see, which is why the gate deliberately excludes
    /// it from unloads. Never preempt it.
    LongcatRig,
    Flux2,
    Krea2,
    Songgen,
    Sa3,
}

impl Lane {
    /// Concurrent jobs koblem will admit here.
    ///
    /// 🔴 One, everywhere, and deliberately less than the engines allow: comfy-api
    /// accepts 8 pending and the sd.cpp fleet accepts 64. Using their queues would
    /// mean two queues with two orderings, and koblem's reorder / preempt / cancel
    /// would then have to chase work into a queue it does not own. Record this so
    /// nobody "fixes" it.
    pub fn capacity(self) -> usize {
        1
    }

    /// Can a per-request `gpu` field steer where this deployment renders?
    ///
    /// 🔴 comfy-api cannot. It takes no `gpu` field at all — one process in front
    /// of one ComfyUI, on whatever card the container was given. Every other
    /// deployment here is the sd.cpp fork or longcat-rig, both of which relocate
    /// their worker when the field changes.
    ///
    /// This is why an operator pin is refused for the comfy kinds rather than
    /// quietly ignored: the gate would move its RESERVATION to the pinned card
    /// while the render carried on where it always does, and the occupancy ledger
    /// would be describing a card that is not the busy one.
    pub fn steers_by_request(self) -> bool {
        !matches!(self, Lane::ComfyApi)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lane::ComfyApi => "comfy-api",
            Lane::LongcatRig => "longcat-rig",
            Lane::Flux2 => "flux2",
            Lane::Krea2 => "krea2",
            Lane::Songgen => "songgen",
            Lane::Sa3 => "sa3",
        }
    }

    pub fn of(kind: JobKind) -> Lane {
        match kind {
            JobKind::Ltx25
            | JobKind::LtxRelip
            | JobKind::H3
            | JobKind::Music3
            | JobKind::Upscale => Lane::ComfyApi,
            JobKind::Pixal3dRig | JobKind::Pixal3dMotion => Lane::LongcatRig,
            JobKind::Flux2 => Lane::Flux2,
            JobKind::Krea2 => Lane::Krea2,
            JobKind::Songgen => Lane::Songgen,
            JobKind::Sa3 => Lane::Sa3,
        }
    }
}

/// How many jobs koblem believes are occupying each lane right now.
///
/// 🔴 This is koblem's belief, and it is NOT authoritative. ComfyUI's own web UI
/// shares the port, and a render started from it is invisible here — comfy-api
/// tracks that separately as an external count, and its `/v1/gpu/status.busy` is
/// the real answer. A dispatcher that trusts "I have nothing running on this lane"
/// can submit on top of a human's render. [`LaneCensus::mark_engine_busy`] folds
/// that reading in.
#[derive(Debug, Clone, Default)]
pub struct LaneCensus {
    counts: HashMap<Lane, usize>,
}

impl LaneCensus {
    pub fn occupy(&mut self, lane: Lane) {
        *self.counts.entry(lane).or_insert(0) += 1;
    }

    /// The engine says it is busy with work koblem did not schedule. Raise the
    /// count to at least capacity so nothing is admitted on top of it.
    pub fn mark_engine_busy(&mut self, lane: Lane) {
        let c = self.counts.entry(lane).or_insert(0);
        *c = (*c).max(lane.capacity());
    }

    pub fn has_room(&self, lane: Lane) -> bool {
        self.counts.get(&lane).copied().unwrap_or(0) < lane.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four comfy plugins are ONE lane. This is the assertion that stops the
    /// queue admitting an upscale and an H3 render at once, which would serialise
    /// invisibly inside ComfyUI while koblem held two VRAM reservations.
    #[test]
    fn the_four_comfy_plugins_share_one_slot() {
        for k in [
            JobKind::Ltx25,
            JobKind::H3,
            JobKind::Music3,
            JobKind::Upscale,
        ] {
            assert_eq!(Lane::of(k), Lane::ComfyApi, "{}", k.as_str());
        }
        let mut census = LaneCensus::default();
        assert!(census.has_room(Lane::ComfyApi));
        census.occupy(Lane::of(JobKind::Upscale));
        assert!(
            !census.has_room(Lane::of(JobKind::H3)),
            "an upscale closes the lane to H3"
        );
        assert!(census.has_room(Lane::Krea2), "and to nothing else");
    }

    /// Every kind has a lane. Two jobs of one kind reuse the gate's placement
    /// (one worker, one set of weights) — correct for VRAM, and no protection at
    /// all against submitting two renders to the same server.
    #[test]
    fn every_kind_serialises_against_itself() {
        for k in JobKind::ALL {
            let mut census = LaneCensus::default();
            census.occupy(Lane::of(k));
            assert!(
                !census.has_room(Lane::of(k)),
                "{} does not serialise against itself",
                k.as_str()
            );
        }
    }

    /// 🔴 comfy-api takes no per-request `gpu`; everything else does. Getting this
    /// backwards means either a pin that silently does nothing, or a refusal on a
    /// deployment that could have honoured it.
    #[test]
    fn only_comfy_cannot_be_steered() {
        assert!(!Lane::ComfyApi.steers_by_request());
        for k in JobKind::ALL {
            let lane = Lane::of(k);
            assert_eq!(
                lane.steers_by_request(),
                lane != Lane::ComfyApi,
                "{}",
                k.as_str()
            );
        }
    }

    /// A render koblem did not schedule still closes the lane. Without this the
    /// dispatcher submits on top of a human's ComfyUI render.
    #[test]
    fn an_engine_busy_with_work_we_cannot_see_closes_the_lane() {
        let mut census = LaneCensus::default();
        assert!(census.has_room(Lane::ComfyApi));
        census.mark_engine_busy(Lane::ComfyApi);
        assert!(!census.has_room(Lane::ComfyApi));
    }
}
