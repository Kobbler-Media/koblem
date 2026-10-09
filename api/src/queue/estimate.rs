//! "Done by ~04:20" — the number you want before going to bed.
//!
//! Two halves:
//!
//! * a **per-unit** duration from `gpu_job_stats`, because that is what makes
//!   "remaining" work and what lets a shape estimate a length it has never been
//!   rendered at; and
//! * a **forward replay of the dispatcher** over the queue, because summing
//!   durations gets the answer wrong. Parallelism across cards and the comfy-api
//!   single slot are exactly what decide when the batch finishes.
//!
//! 🔴 Report it with the sample count. This is a claim about the future, not a
//! measurement, and a confident-looking ETA built from three samples is worse than
//! one that admits it is guessing.

use std::collections::HashMap;

use koblem_client::types::{Job, JobState};

use super::admit::service_of;
use super::lane::Lane;
use crate::repositories::gpu_job_stats::Stat;

/// Below this, the estimate is a prior rather than a measurement and the UI is
/// expected to say so.
pub const CONFIDENT_SAMPLES: i32 = 5;

/// What a shape is expected to cost, and how wide the spread around it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub ms: i64,
    /// The same job on the slow tail of its own history. Never below [`Estimate::ms`].
    pub p90_ms: i64,
    pub samples: i32,
}

/// The four numbers a shape's history yields: whole-job p50 and p90, per-unit
/// p50, and the sample count they all rest on.
#[derive(Debug, Clone, Copy)]
struct Measured {
    p50: i64,
    p90: i64,
    unit_p50: i64,
    samples: i32,
}

/// The measured table, indexed for the replay.
pub struct Estimator {
    by_shape: HashMap<(String, String), Stat>,
    /// Per-kind fallback: the median of every shape of that kind, so a brand-new
    /// shape is not estimated at zero.
    by_kind: HashMap<String, Measured>,
}

impl Estimator {
    pub fn new(stats: Vec<Stat>) -> Self {
        let mut by_shape = HashMap::new();
        let mut kind_unit: HashMap<String, Vec<Measured>> = HashMap::new();
        for s in stats {
            kind_unit.entry(s.kind.clone()).or_default().push(Measured {
                p50: s.p50_ms,
                p90: s.p90_ms,
                unit_p50: s.unit_p50_ms,
                samples: s.samples,
            });
            by_shape.insert((s.kind.clone(), s.shape_key.clone()), s);
        }
        let by_kind = kind_unit
            .into_iter()
            .map(|(kind, mut rows)| {
                rows.sort_by_key(|m| m.p50);
                let mid = rows[rows.len() / 2];
                let samples = rows.iter().map(|m| m.samples).sum();
                (kind, Measured { samples, ..mid })
            })
            .collect();
        Estimator { by_shape, by_kind }
    }

    /// How long this job will take from a standing start.
    ///
    /// Prefers `units × unit_p50` over the whole-job p50, because that is what
    /// generalises: a 16-generation cut and a 4-generation cut share a shape but
    /// not a duration.
    pub fn whole(&self, job: &Job) -> Estimate {
        let kind = job.kind.as_str().to_string();
        let shape = job.shape_key.clone().unwrap_or_default();
        let m = match self.by_shape.get(&(kind.clone(), shape)) {
            Some(s) => Measured {
                p50: s.p50_ms,
                p90: s.p90_ms,
                unit_p50: s.unit_p50_ms,
                samples: s.samples,
            },
            None => match self.by_kind.get(&kind) {
                Some(&m) => m,
                // No history at all. Zero rather than a guessed constant: a made-up
                // number that looks like a measurement is the failure mode being
                // designed against, and `samples: 0` tells the UI to say so.
                None => Measured {
                    p50: 0,
                    p90: 0,
                    unit_p50: 0,
                    samples: 0,
                },
            },
        };
        let ms = match (job.units_total, m.unit_p50) {
            (Some(units), unit) if units > 0 && unit > 0 => unit * units as i64,
            _ => m.p50,
        };
        Estimate {
            ms,
            p90_ms: spread(ms, &m),
            samples: m.samples,
        }
    }

    /// What is left of a job already running.
    ///
    /// Blends the measured per-unit rate with THIS job's own observed rate, so a
    /// run that is going badly stops claiming it will finish on the median.
    pub fn remaining(&self, job: &Job, elapsed_ms: i64) -> Estimate {
        let base = self.whole(job);
        let scale = |ms: i64| Estimate {
            ms,
            p90_ms: widen(ms, base.ms, base.p90_ms),
            samples: base.samples,
        };
        let Some(total) = job.units_total.filter(|t| *t > 0) else {
            return scale((base.ms - elapsed_ms).max(0));
        };
        let done = job.units_done.clamp(0, total);
        if done == 0 {
            return scale((base.ms - elapsed_ms).max(0));
        }
        let observed_unit = elapsed_ms / done as i64;
        let table_unit = base.ms / total as i64;
        // Equal weight: this job's own rate is the more relevant number, the table
        // is the more stable one, and there is no evidence for a cleverer blend.
        let unit = if table_unit > 0 {
            (observed_unit + table_unit) / 2
        } else {
            observed_unit
        };
        scale(unit * (total - done) as i64)
    }
}

/// The p90 of a duration derived from the table's own p90/p50 ratio.
///
/// 🔴 `gpu_job_stats` measures p90 per WHOLE JOB and p50 per unit, so a job
/// estimated as `units × unit_p50` has no p90 of its own. The ratio is the
/// honest carry-over: it is the spread the shape has actually shown.
fn spread(ms: i64, m: &Measured) -> i64 {
    widen(ms, m.p50, m.p90)
}

fn widen(ms: i64, p50: i64, p90: i64) -> i64 {
    if p50 <= 0 || p90 <= p50 {
        return ms;
    }
    (ms as i128 * p90 as i128 / p50 as i128) as i64
}

/// A simulated start/finish for one job, in ms from now.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub job_id: String,
    /// `None` for a job that is already running — it started in the past, and the
    /// number worth having about it is when it ENDS.
    pub start_ms: Option<i64>,
    pub finish_ms: i64,
    /// The same finish with every job in front of it on its own slow tail.
    pub finish_p90_ms: i64,
    pub samples: i32,
}

/// Replay the dispatcher forward over the queue.
///
/// 🔴 Not a sum of durations. With two cards, a krea2 job runs alongside an H3
/// render; with one comfy lane, an upscale and a music3 job do not, however much
/// VRAM is free. Those two facts are what decide when the batch actually finishes,
/// and only a replay captures them.
///
/// Simplified against the real admission in one way that is stated rather than
/// hidden: it models the LANE and the per-service exclusivity, not VRAM
/// bin-packing. Lane contention dominates in practice (four kinds share comfy-api)
/// and a full placement simulation would need the whole placer.
pub fn replay(
    estimator: &Estimator,
    live: &[(Job, i64)],
    pending: &[Job],
    cards: usize,
) -> Vec<Slot> {
    let median = schedule(estimator, live, pending, cards, |e| e.ms);
    let tail = schedule(estimator, live, pending, cards, |e| e.p90_ms);
    // Two passes over identical input, so the two runs emit the same jobs in the
    // same order.
    median
        .into_iter()
        .zip(tail)
        .map(|(m, t)| Slot {
            job_id: m.job_id,
            start_ms: m.start_ms,
            finish_ms: m.finish_ms,
            finish_p90_ms: t.finish_ms.max(m.finish_ms),
            samples: m.samples,
        })
        .collect()
}

struct Run {
    job_id: String,
    start_ms: Option<i64>,
    finish_ms: i64,
    samples: i32,
}

fn schedule(
    estimator: &Estimator,
    live: &[(Job, i64)],
    pending: &[Job],
    cards: usize,
    pick: fn(&Estimate) -> i64,
) -> Vec<Run> {
    let mut lane_free: HashMap<Lane, i64> = HashMap::new();
    let mut service_free: HashMap<kob_gpu_gate::ServiceKind, i64> = HashMap::new();
    // One busy-interval list per card. Tracking intervals rather than a single
    // "free from" mark is what makes the gaps usable: a job held back by its LANE
    // leaves the card idle in the meantime, and a job on a different engine
    // should be able to run there — which is exactly the parallelism the whole
    // replay exists to capture.
    let mut cards: Vec<Vec<(i64, i64)>> = vec![Vec::new(); cards.max(1)];
    let mut out = Vec::new();

    // 🔴 A live job gets a slot of its own. One long render with an empty queue is
    // the commonest overnight state, and without this it has no completion time at
    // all — neither on the row nor in the batch maximum.
    for (job, elapsed) in live {
        let est = estimator.remaining(job, *elapsed);
        let left = pick(&est);
        let lane = Lane::of(job.kind);
        let e = lane_free.entry(lane).or_insert(0);
        *e = (*e).max(left);
        let s = service_free.entry(service_of(job.kind)).or_insert(0);
        *s = (*s).max(left);
        // A running job is already holding a card for the rest of its run.
        if let Some(card) = cards.iter_mut().min_by_key(|c| busy_until(c)) {
            card.push((0, left));
        }
        out.push(Run {
            job_id: job.id.clone(),
            start_ms: None,
            finish_ms: left,
            samples: est.samples,
        });
    }

    for job in pending.iter().filter(|j| j.state == JobState::Queued) {
        let est = estimator.whole(job);
        let dur = pick(&est);
        let lane = Lane::of(job.kind);
        let service = service_of(job.kind);
        let earliest =
            (*lane_free.get(&lane).unwrap_or(&0)).max(*service_free.get(&service).unwrap_or(&0));

        let (idx, start) = cards
            .iter()
            .enumerate()
            .map(|(i, busy)| (i, first_gap(busy, earliest, dur)))
            .min_by_key(|(_, start)| *start)
            .unwrap_or((0, earliest));
        let finish = start + dur;

        cards[idx].push((start, finish));
        cards[idx].sort_unstable();
        lane_free.insert(lane, finish);
        service_free.insert(service, finish);
        out.push(Run {
            job_id: job.id.clone(),
            start_ms: Some(start),
            finish_ms: finish,
            samples: est.samples,
        });
    }
    out
}

fn busy_until(busy: &[(i64, i64)]) -> i64 {
    busy.iter().map(|(_, e)| *e).max().unwrap_or(0)
}

/// The earliest time at or after `earliest` where this card is free for `dur`.
fn first_gap(busy: &[(i64, i64)], earliest: i64, dur: i64) -> i64 {
    let mut t = earliest;
    // `busy` is kept sorted, so one forward pass settles it: any interval that
    // overlaps the candidate window pushes the candidate past its end.
    let mut moved = true;
    while moved {
        moved = false;
        for (s, e) in busy {
            if *s < t + dur.max(1) && *e > t {
                t = *e;
                moved = true;
            }
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use koblem_client::types::{JobKind, JobProgress};
    use time::OffsetDateTime;

    fn stat(kind: &str, shape: &str, p50: i64, unit: i64, samples: i32) -> Stat {
        Stat {
            kind: kind.into(),
            shape_key: shape.into(),
            samples,
            p50_ms: p50,
            p90_ms: p50 * 2,
            unit_p50_ms: unit,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn job(id: &str, kind: JobKind, shape: &str, units: Option<i32>) -> Job {
        Job {
            id: id.into(),
            kind,
            state: JobState::Queued,
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
            units_total: units,
            units_done: 0,
            shape_key: Some(shape.into()),
            attempt: 1,
            retry_of: None,
            blocked_history: vec![],
            label: String::new(),
            created_at: "2026-08-16T00:00:00Z".into(),
            started_at: None,
            finished_at: None,
            eta_start: None,
            eta_finish: None,
            eta_finish_p90: None,
            estimate_samples: 0,
        }
    }

    /// 🔴 The per-unit number is what makes a NEVER-SEEN LENGTH estimable. Both
    /// jobs share a shape; only the unit count differs, and a whole-job p50 would
    /// give them the same answer.
    #[test]
    fn a_length_never_rendered_is_still_estimable() {
        let e = Estimator::new(vec![stat("h3", "h3/1920x1088", 600_000, 200_000, 12)]);
        let four = e.whole(&job("a", JobKind::H3, "h3/1920x1088", Some(4)));
        let sixteen = e.whole(&job("b", JobKind::H3, "h3/1920x1088", Some(16)));
        assert_eq!(four.ms, 800_000);
        assert_eq!(sixteen.ms, 3_200_000);
        assert_eq!(sixteen.samples, 12);
    }

    /// No history means no number, and `samples: 0` so the UI can say so. An
    /// invented constant that looks like a measurement is the failure being
    /// designed against.
    #[test]
    fn no_history_admits_it_knows_nothing() {
        let e = Estimator::new(vec![]);
        let got = e.whole(&job("a", JobKind::Ltx25, "ltx25/anything", Some(3)));
        assert_eq!(
            got,
            Estimate {
                ms: 0,
                p90_ms: 0,
                samples: 0
            }
        );
    }

    /// 🔴 The spread travels with the estimate. A job scaled by its unit count has
    /// no p90 of its own — `gpu_job_stats` measures p90 per whole job — so it
    /// carries the shape's own measured p90/p50 ratio, and the UI can say "done
    /// ~02:50 (p90 ~04:10, n=7)" instead of a bare number.
    #[test]
    fn the_estimate_carries_its_own_spread() {
        let e = Estimator::new(vec![stat("h3", "s", 600_000, 200_000, 7)]);
        // `stat` builds p90 = 2 × p50.
        let four = e.whole(&job("a", JobKind::H3, "s", Some(4)));
        assert_eq!(four.ms, 800_000);
        assert_eq!(four.p90_ms, 1_600_000);
        assert_eq!(four.samples, 7);

        // ...and it survives the live blend rather than collapsing to the median.
        let mut running = job("b", JobKind::H3, "s", Some(4));
        running.units_done = 1;
        let left = e.remaining(&running, 200_000);
        assert!(left.p90_ms > left.ms, "a running job keeps a spread too");
    }

    /// A shape whose history is uniform has no spread to report, and must not
    /// invent one narrower than the estimate itself.
    #[test]
    fn a_shape_with_no_spread_reports_none() {
        let mut s = stat("krea2", "k", 10_000, 10_000, 30);
        s.p90_ms = 10_000;
        let e = Estimator::new(vec![s]);
        let got = e.whole(&job("a", JobKind::Krea2, "k", Some(1)));
        assert_eq!(got.p90_ms, got.ms);
    }

    /// An unseen shape falls back to the kind's own median rather than to zero.
    #[test]
    fn an_unseen_shape_falls_back_to_its_kind() {
        let e = Estimator::new(vec![stat("krea2", "krea2/1024/8steps", 9_000, 1_100, 30)]);
        let got = e.whole(&job("a", JobKind::Krea2, "krea2/2048/8steps", Some(1)));
        assert_eq!(got.ms, 1_100);
        assert_eq!(got.samples, 30);
    }

    /// A run going twice as slow as the table stops claiming it will finish on the
    /// median. The blend is what makes a live "remaining" honest.
    #[test]
    fn a_slow_run_revises_its_own_estimate_upward() {
        let e = Estimator::new(vec![stat("h3", "s", 400_000, 100_000, 20)]);
        let mut j = job("a", JobKind::H3, "s", Some(4));
        j.units_done = 1;
        // Table says 100 s/unit; this job took 200 s for its first unit.
        let left = e.remaining(&j, 200_000);
        // Blended 150 s/unit over the 3 remaining.
        assert_eq!(left.ms, 450_000);
    }

    /// 🔴 The comfy lane is why summing durations is wrong. Two comfy jobs
    /// serialise no matter how much VRAM is free; a krea2 job does not.
    #[test]
    fn the_replay_serialises_one_lane_and_parallelises_the_others() {
        let e = Estimator::new(vec![
            stat("upscale", "u", 60_000, 60_000, 9),
            stat("music3", "m", 30_000, 30_000, 9),
            stat("krea2", "k", 10_000, 10_000, 9),
        ]);
        let pending = vec![
            job("up", JobKind::Upscale, "u", Some(1)),
            job("mu", JobKind::Music3, "m", Some(1)),
            job("kr", JobKind::Krea2, "k", Some(1)),
        ];
        let slots = replay(&e, &[], &pending, 2);
        let by = |id: &str| slots.iter().find(|s| s.job_id == id).unwrap().clone();

        assert_eq!(by("up").start_ms, Some(0));
        assert_eq!(by("up").finish_ms, 60_000);
        assert_eq!(
            by("mu").start_ms,
            Some(60_000),
            "music3 waits for the comfy slot, not for VRAM"
        );
        assert_eq!(
            by("kr").start_ms,
            Some(0),
            "krea2 is a different engine and runs alongside"
        );
    }

    /// A running job pushes everything behind it on its lane.
    #[test]
    fn the_replay_waits_out_what_is_already_running() {
        let e = Estimator::new(vec![stat("h3", "s", 100_000, 100_000, 9)]);
        let mut running = job("live", JobKind::H3, "s", Some(1));
        running.state = JobState::Running;
        let pending = vec![job("next", JobKind::Upscale, "s", Some(1))];
        let slots = replay(&e, &[(running, 40_000)], &pending, 4);
        let next = slots.iter().find(|s| s.job_id == "next").unwrap();
        assert_eq!(next.start_ms, Some(60_000), "60 s left on the comfy lane");
    }

    /// 🔴 The commonest overnight state: one long render, nothing queued behind
    /// it. Without a slot of its own it has no completion time anywhere — not on
    /// the row, not in the batch maximum — which is the one number the pill wants.
    #[test]
    fn a_running_job_publishes_its_own_finish() {
        let e = Estimator::new(vec![stat("h3", "s", 100_000, 100_000, 9)]);
        let mut running = job("live", JobKind::H3, "s", Some(1));
        running.state = JobState::Running;
        let slots = replay(&e, &[(running, 40_000)], &[], 4);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].job_id, "live");
        assert_eq!(
            slots[0].start_ms, None,
            "it started in the past; the number worth having is the finish"
        );
        assert_eq!(slots[0].finish_ms, 60_000);
        assert!(slots[0].finish_p90_ms >= 60_000);
    }

    /// One card means one job at a time, whatever the lanes say. Without the wave
    /// cap the batch ETA reads as if every engine ran in parallel.
    #[test]
    fn a_single_slot_box_does_not_pretend_to_parallelise() {
        let e = Estimator::new(vec![
            stat("krea2", "k", 10_000, 10_000, 9),
            stat("flux2", "f", 10_000, 10_000, 9),
        ]);
        let pending = vec![
            job("a", JobKind::Krea2, "k", Some(1)),
            job("b", JobKind::Flux2, "f", Some(1)),
        ];
        let one = replay(&e, &[], &pending, 1);
        assert_eq!(one[1].start_ms, Some(10_000));
        let two = replay(&e, &[], &pending, 2);
        assert_eq!(two[1].start_ms, Some(0));
    }

    /// The pessimistic pass is a whole second replay, not the last job's spread
    /// bolted onto the median: a queue of four jobs on one lane is late by every
    /// job's tail, not just its own.
    #[test]
    fn the_p90_finish_compounds_down_the_lane() {
        let e = Estimator::new(vec![stat("music3", "m", 30_000, 30_000, 9)]);
        let pending = vec![
            job("a", JobKind::Music3, "m", Some(1)),
            job("b", JobKind::Music3, "m", Some(1)),
        ];
        let slots = replay(&e, &[], &pending, 2);
        assert_eq!(slots[1].finish_ms, 60_000);
        assert_eq!(
            slots[1].finish_p90_ms, 120_000,
            "both jobs on the slow tail, serialised on the comfy lane"
        );
    }
}
