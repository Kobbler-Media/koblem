//! The admission decision, as a pure function.
//!
//! 🔴 Skip-with-a-reason is the dispatcher's MAIN LOOP, not an error branch:
//!
//! ```text
//! for job in queue.order() {          // band, then rank
//!     match admit(job) {
//!         Ok(())      => start(job),
//!         Err(reason) => { stamp(job, reason); continue }   // <- this IS the backfill
//!     }
//! }
//! ```
//!
//! Which is why the head reservation is load-bearing rather than a refinement: it
//! is the only thing stopping the big job at the head from being skipped forever.
//!
//! Everything here is pure and synchronous over a snapshot taken once per tick.
//! That is deliberate — reasons must not flap between rows, and a scheduler is
//! ordering, time and concurrency, which is exactly what breaks silently and gets
//! discovered at 3am. Ordering, backfill, head reservation, starvation and the
//! whole reason matrix are all assertable from here with no GPU and no database.

use std::collections::HashMap;

use kob_gpu_gate::{AdmitFailure, ServiceKind};
use koblem_client::types::{BlockedReason, Job, JobKind, JobState};

use super::lane::{Lane, LaneCensus};

/// The gate, as admission sees it. A trait so the tests can script placement.
pub trait GateView: Send + Sync {
    /// `None` = it would place. Never reserves.
    fn can_place(
        &self,
        service: ServiceKind,
        pin: Option<&str>,
        force: bool,
    ) -> Option<AdmitFailure>;
    /// Cards this service may be placed on. Used only by the head reservation, to
    /// tell a backfill job that could run elsewhere from one that would take the
    /// very card the head is waiting for.
    fn candidates(&self, service: ServiceKind) -> Vec<String>;
}

/// Engine reachability, as admission sees it.
pub trait HealthView: Send + Sync {
    /// `Some(detail)` only when a probe has POSITIVELY seen the engine down.
    ///
    /// 🔴 comfy-api reports `comfy.reachable: null` until its background probe has
    /// run once, and "not yet known" is not "down" — the difference decides
    /// whether someone gets woken. Unknown must map to `None` here.
    fn unreachable(&self, lane: Lane) -> Option<String>;

    /// The engine is busy with work koblem did not schedule.
    ///
    /// 🔴 ComfyUI's own web UI shares the port and its renders are invisible to
    /// koblem's bookkeeping. A dispatcher that trusts its own "nothing is running
    /// on this lane" can submit on top of a human's render — that has already
    /// killed one, under WanGP.
    fn engine_busy(&self, lane: Lane) -> bool;
}

/// The head job's claim, held against backfill once it has waited too long.
#[derive(Debug, Clone)]
pub struct Reservation {
    pub head_id: String,
    pub lane: Lane,
    pub service: ServiceKind,
    /// Cards the head could be placed on. A backfill job whose own candidates all
    /// lie inside this set would take the card the head is waiting for; one with a
    /// card outside it can still run, which is the whole point of backfill.
    pub cards: Vec<String>,
}

/// Everything the decision reads, sampled once per tick.
pub struct AdmitContext<'a> {
    pub paused: bool,
    pub lanes: &'a LaneCensus,
    pub gate: &'a dyn GateView,
    pub health: &'a dyn HealthView,
    /// State of every job named by a `depends_on`.
    pub parents: &'a HashMap<String, JobState>,
    pub reservation: Option<&'a Reservation>,
    /// Lanes whose engine-unreachable backoff has not expired. A dead service must
    /// not be hammered once a second all night.
    pub backing_off: &'a HashMap<Lane, String>,
}

/// The VRAM claim for a kind.
///
/// Not derivable from `JobKind` alone in general, but it is here: the mapping is
/// fixed and the *price* lives in `gpu_engine_config.peak_mb`, which is DB-editable.
pub fn service_of(kind: JobKind) -> ServiceKind {
    match kind {
        // Relip shares ltx25's engine and therefore its VRAM claim and its lane.
        JobKind::Ltx25 | JobKind::LtxRelip => ServiceKind::Ltx,
        JobKind::H3 => ServiceKind::MinimaxH3,
        // 🔴 music3 and upscale are comfy-api plugins with no budget line of their
        // own. They were made `acquire_for_minimax_h3()` because they share the
        // ComfyUI process — a budget line abused to mean a mutex. The mutex is now
        // the LANE, and the VRAM claim stays H3's because that is the process whose
        // weights are resident.
        JobKind::Music3 | JobKind::Upscale => ServiceKind::MinimaxH3,
        JobKind::Krea2 => ServiceKind::Krea2,
        JobKind::Flux2 => ServiceKind::Flux,
        // Retired engines: still named so their historical rows parse; never enqueued.
        JobKind::Avatar => ServiceKind::Avatar,
        JobKind::WanVace => ServiceKind::WanVace,
        JobKind::Ltx2b => ServiceKind::Ltx2b,
        JobKind::Songgen => ServiceKind::Songgen,
        JobKind::Sa3 => ServiceKind::Sa3,
        JobKind::Pixal3dRig | JobKind::Pixal3dMotion => ServiceKind::Pixal3d,
    }
}

/// Should the head's claim be held against backfill?
///
/// 🔴 Only for reasons that a running job will eventually clear. A head blocked
/// because its service was left DISABLED must never freeze the queue behind it —
/// that turns one operator mistake into a lost night for every other job, which is
/// the precise failure this design exists to prevent.
/// 🔴 `BoxLocked` is deliberately ABSENT for the same reason as
/// `ServiceDisabled`: Beefy mode is cleared by a human, not by a running job, so
/// a head blocked on it must not hold a reservation. Nothing in the type system
/// enforces this list — if you add a variant here, check it can actually clear
/// itself.
pub fn reservation_worthy(reason: &BlockedReason) -> bool {
    matches!(
        reason,
        BlockedReason::NoCardFits { .. } | BlockedReason::LaneBusy { .. }
    )
}

/// Can this job start right now?
pub fn admit(job: &Job, ctx: &AdmitContext<'_>) -> Result<(), BlockedReason> {
    if ctx.paused || job.state == JobState::Held {
        return Err(BlockedReason::Paused);
    }

    // A dependent stays queued and is simply not admissible until its parent is
    // done. No reservation, no priority inheritance. A parent that failed or was
    // cancelled cascades this job to `cancelled` — handled by the caller, which
    // owns the write.
    if let Some(parent) = job.depends_on.as_deref() {
        let done = ctx.parents.get(parent).copied() == Some(JobState::Done);
        if !done {
            return Err(BlockedReason::DependencyPending {
                parent: parent.to_string(),
            });
        }
    }

    let lane = Lane::of(job.kind);
    let service = service_of(job.kind);

    if let Some(detail) = ctx.backing_off.get(&lane) {
        return Err(BlockedReason::EngineUnreachable {
            detail: detail.clone(),
        });
    }
    if let Some(detail) = ctx.health.unreachable(lane) {
        return Err(BlockedReason::EngineUnreachable { detail });
    }

    if !ctx.lanes.has_room(lane) {
        return Err(BlockedReason::LaneBusy {
            lane: lane.as_str().to_string(),
        });
    }

    if let Some(res) = ctx.reservation
        && res.head_id != job.id
        && conflicts_with_head(job.kind, lane, service, res, ctx.gate)
    {
        return Err(BlockedReason::HeadReserved {
            head: res.head_id.clone(),
        });
    }

    match ctx
        .gate
        .can_place(service, job.target_gpu.as_deref(), job.force_pin)
    {
        None => Ok(()),
        Some(AdmitFailure::ServiceDisabled) => Err(BlockedReason::ServiceDisabled {
            service: service.as_str().to_string(),
        }),
        Some(AdmitFailure::EvictionUnconfirmed) => Err(BlockedReason::EvictionCooldown),
        Some(AdmitFailure::BoxLocked { keep }) => Err(BlockedReason::BoxLocked {
            service: keep.as_str().to_string(),
        }),
        Some(AdmitFailure::Place(block)) => Err(BlockedReason::NoCardFits {
            detail: block.to_string(),
        }),
    }
}

fn conflicts_with_head(
    kind: JobKind,
    lane: Lane,
    service: ServiceKind,
    res: &Reservation,
    gate: &dyn GateView,
) -> bool {
    if lane == res.lane || service == res.service {
        return true;
    }
    let _ = kind;
    // A job that can only run on cards the head also needs would consume the very
    // budget being held. One that has a card outside the head's candidate set can
    // still backfill there, and should.
    let mine = gate.candidates(service);
    !mine.is_empty() && mine.iter().all(|c| res.cards.contains(c))
}

/// The head's claim, if it has waited long enough to deserve one.
pub fn reservation(
    head: &Job,
    blocked_ms: i64,
    threshold_ms: i64,
    gate: &dyn GateView,
) -> Option<Reservation> {
    if blocked_ms < threshold_ms {
        return None;
    }
    let reason = head.blocked_reason.as_ref()?;
    if !reservation_worthy(reason) {
        return None;
    }
    let service = service_of(head.kind);
    Some(Reservation {
        head_id: head.id.clone(),
        lane: Lane::of(head.kind),
        service,
        cards: gate.candidates(service),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kob_gpu_gate::PlaceBlock;
    use koblem_client::types::JobProgress;

    /// A gate whose answers the test writes.
    #[derive(Default)]
    struct FakeGate {
        blocked: HashMap<ServiceKind, AdmitFailure>,
        cards: HashMap<ServiceKind, Vec<String>>,
    }

    impl FakeGate {
        fn block(mut self, k: ServiceKind, f: AdmitFailure) -> Self {
            self.blocked.insert(k, f);
            self
        }
        fn cards(mut self, k: ServiceKind, c: &[&str]) -> Self {
            self.cards
                .insert(k, c.iter().map(|s| s.to_string()).collect());
            self
        }
    }

    impl GateView for FakeGate {
        fn can_place(
            &self,
            service: ServiceKind,
            _pin: Option<&str>,
            _force: bool,
        ) -> Option<AdmitFailure> {
            self.blocked.get(&service).cloned()
        }
        fn candidates(&self, service: ServiceKind) -> Vec<String> {
            self.cards.get(&service).cloned().unwrap_or_default()
        }
    }

    #[derive(Default)]
    struct FakeHealth {
        down: HashMap<Lane, String>,
        busy: Vec<Lane>,
    }

    impl HealthView for FakeHealth {
        fn unreachable(&self, lane: Lane) -> Option<String> {
            self.down.get(&lane).cloned()
        }
        fn engine_busy(&self, lane: Lane) -> bool {
            self.busy.contains(&lane)
        }
    }

    fn job(id: &str, kind: JobKind) -> Job {
        Job {
            id: id.into(),
            kind,
            state: JobState::Queued,
            band: 1,
            queue_rank: 1024.0,
            target_gpu: None,
            force_pin: false,
            server_job_id: None,
            progress: JobProgress::default(),
            depends_on: None,
            created_by: "user:t".into(),
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

    #[derive(Default)]
    struct Ctx {
        lanes: LaneCensus,
        gate: FakeGate,
        health: FakeHealth,
        parents: HashMap<String, JobState>,
        backoff: HashMap<Lane, String>,
        paused: bool,
    }

    impl Ctx {
        fn check(&self, j: &Job, res: Option<&Reservation>) -> Result<(), BlockedReason> {
            admit(
                j,
                &AdmitContext {
                    paused: self.paused,
                    lanes: &self.lanes,
                    gate: &self.gate,
                    health: &self.health,
                    parents: &self.parents,
                    reservation: res,
                    backing_off: &self.backoff,
                },
            )
        }
    }

    /// The §11e matrix, table-driven. Every variant, and the one thing that must be
    /// true of all of them: a refusal is a REASON, never a failure.
    #[test]
    fn the_reason_matrix() {
        struct Case {
            name: &'static str,
            build: fn(&mut Ctx, &mut Job),
            want: BlockedReason,
        }

        let cases = [
            Case {
                name: "the operator paused the queue",
                build: |c, _| c.paused = true,
                want: BlockedReason::Paused,
            },
            Case {
                name: "this row is held",
                build: |_, j| j.state = JobState::Held,
                want: BlockedReason::Paused,
            },
            Case {
                name: "the parent has not finished",
                build: |c, j| {
                    j.depends_on = Some("parent".into());
                    c.parents.insert("parent".into(), JobState::Running);
                },
                want: BlockedReason::DependencyPending {
                    parent: "parent".into(),
                },
            },
            Case {
                name: "the engine has been seen down",
                build: |c, _| {
                    c.health
                        .down
                        .insert(Lane::ComfyApi, "connection refused".into());
                },
                want: BlockedReason::EngineUnreachable {
                    detail: "connection refused".into(),
                },
            },
            Case {
                name: "the engine is down and we are backing off it",
                build: |c, _| {
                    c.backoff.insert(Lane::ComfyApi, "retry in 30s".into());
                },
                want: BlockedReason::EngineUnreachable {
                    detail: "retry in 30s".into(),
                },
            },
            Case {
                name: "the comfy slot is taken",
                build: |c, _| c.lanes.occupy(Lane::ComfyApi),
                want: BlockedReason::LaneBusy {
                    lane: "comfy-api".into(),
                },
            },
            Case {
                name: "a human's ComfyUI render owns the slot",
                build: |c, _| c.lanes.mark_engine_busy(Lane::ComfyApi),
                want: BlockedReason::LaneBusy {
                    lane: "comfy-api".into(),
                },
            },
            Case {
                name: "the service is administratively disabled",
                build: |c, _| {
                    c.gate = std::mem::take(&mut c.gate)
                        .block(ServiceKind::MinimaxH3, AdmitFailure::ServiceDisabled);
                },
                want: BlockedReason::ServiceDisabled {
                    service: "minimax_h3".into(),
                },
            },
            Case {
                name: "an eviction is still settling",
                build: |c, _| {
                    c.gate = std::mem::take(&mut c.gate)
                        .block(ServiceKind::MinimaxH3, AdmitFailure::EvictionUnconfirmed);
                },
                want: BlockedReason::EvictionCooldown,
            },
            Case {
                name: "no card fits",
                build: |c, _| {
                    c.gate = std::mem::take(&mut c.gate).block(
                        ServiceKind::MinimaxH3,
                        AdmitFailure::Place(PlaceBlock::NoEnabledCard),
                    );
                },
                want: BlockedReason::NoCardFits {
                    detail: "no enabled card is a candidate".into(),
                },
            },
        ];

        for case in cases {
            let mut ctx = Ctx::default();
            let mut j = job("j1", JobKind::H3);
            (case.build)(&mut ctx, &mut j);
            assert_eq!(
                ctx.check(&j, None),
                Err(case.want.clone()),
                "case: {}",
                case.name
            );
        }
    }

    /// Nothing in the way → it starts. The happy path has to be in the table too,
    /// or a bug that blocks everything passes every other case.
    #[test]
    fn an_unobstructed_job_admits() {
        let ctx = Ctx::default();
        assert_eq!(ctx.check(&job("j1", JobKind::H3), None), Ok(()));
    }

    /// A parent that is DONE stops blocking. The dependency clears by itself.
    #[test]
    fn a_finished_parent_releases_its_dependent() {
        let mut ctx = Ctx::default();
        let mut j = job("child", JobKind::Upscale);
        j.depends_on = Some("parent".into());
        ctx.parents.insert("parent".into(), JobState::Done);
        assert_eq!(ctx.check(&j, None), Ok(()));
    }

    /// Backfill: the comfy lane is busy, so an H3 job cannot start — and a krea2
    /// job on an entirely different engine starts anyway. This is the case strict
    /// head-of-line ordering gets wrong, and it is why skipping exists.
    #[test]
    fn a_busy_lane_does_not_block_a_different_engine() {
        let mut ctx = Ctx::default();
        ctx.lanes.occupy(Lane::ComfyApi);
        assert!(ctx.check(&job("h3", JobKind::H3), None).is_err());
        assert_eq!(ctx.check(&job("krea", JobKind::Krea2), None), Ok(()));
    }

    /// Head reservation: once the head has waited past the threshold, backfill
    /// stops taking the resources it needs — otherwise a trickle of small jobs
    /// starves it forever.
    #[test]
    fn the_head_reservation_stops_starvation() {
        let ctx = Ctx {
            gate: FakeGate::default()
                .cards(ServiceKind::MinimaxH3, &["fast"])
                .cards(ServiceKind::Krea2, &["fast"]),
            ..Default::default()
        };

        let mut head = job("head", JobKind::H3);
        head.blocked_reason = Some(BlockedReason::NoCardFits {
            detail: "full".into(),
        });

        // Not yet: below the threshold there is no reservation at all.
        assert!(reservation(&head, 60_000, 300_000, &ctx.gate).is_none());

        let res = reservation(&head, 301_000, 300_000, &ctx.gate).expect("head has waited");
        // krea2 can only run on the card the head needs → held back.
        assert_eq!(
            ctx.check(&job("krea", JobKind::Krea2), Some(&res)),
            Err(BlockedReason::HeadReserved {
                head: "head".into()
            })
        );
        // The head itself is never blocked by its own reservation.
        assert_eq!(ctx.check(&head, Some(&res)), Ok(()));
    }

    /// ...but a job that can run somewhere the head cannot still backfills. A
    /// reservation that stopped everything would trade starvation for an idle box.
    #[test]
    fn the_reservation_still_lets_a_disjoint_card_backfill() {
        let ctx = Ctx {
            gate: FakeGate::default()
                .cards(ServiceKind::MinimaxH3, &["fast"])
                .cards(ServiceKind::Pixal3d, &["slow"]),
            ..Default::default()
        };
        let mut head = job("head", JobKind::H3);
        head.blocked_reason = Some(BlockedReason::NoCardFits {
            detail: "full".into(),
        });
        let res = reservation(&head, 400_000, 300_000, &ctx.gate).unwrap();
        assert_eq!(
            ctx.check(&job("rig", JobKind::Pixal3dRig), Some(&res)),
            Ok(())
        );
    }

    /// 🔴 A head blocked because someone left a service DISABLED must NOT freeze
    /// the queue behind it. Nothing running will clear that reason, so reserving
    /// for it turns one operator mistake into a lost night for every other job.
    #[test]
    fn a_disabled_head_never_reserves() {
        let gate = FakeGate::default().cards(ServiceKind::MinimaxH3, &["fast"]);
        let mut head = job("head", JobKind::H3);
        head.blocked_reason = Some(BlockedReason::ServiceDisabled {
            service: "minimax_h3".into(),
        });
        assert!(
            reservation(&head, 4 * 3_600_000, 300_000, &gate).is_none(),
            "four hours disabled and still no reservation"
        );

        head.blocked_reason = Some(BlockedReason::DependencyPending { parent: "p".into() });
        assert!(reservation(&head, 4 * 3_600_000, 300_000, &gate).is_none());
        head.blocked_reason = Some(BlockedReason::EngineUnreachable {
            detail: "down".into(),
        });
        assert!(reservation(&head, 4 * 3_600_000, 300_000, &gate).is_none());
    }

    /// Every kind maps to a VRAM claim, and the four comfy plugins share H3's —
    /// that is the process whose weights are actually resident.
    #[test]
    fn the_comfy_plugins_share_one_budget_line() {
        assert_eq!(service_of(JobKind::Music3), ServiceKind::MinimaxH3);
        assert_eq!(service_of(JobKind::Upscale), ServiceKind::MinimaxH3);
        assert_eq!(service_of(JobKind::H3), ServiceKind::MinimaxH3);
        assert_eq!(service_of(JobKind::Ltx25), ServiceKind::Ltx);
        assert_eq!(service_of(JobKind::Pixal3dMotion), ServiceKind::Pixal3d);
    }
}
