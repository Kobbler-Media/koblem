//! Whether a failed run is worth a second GPU slot — decided from the TYPED
//! engine error, never from the sentence it was flattened into.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use crate::queue::runner::Failure;

/// 🔴 [`crate::queue::runner::JobRunner::classify`] is handed the flattened
/// `String`, and the only part of that string an adapter can trust is the part it
/// wrote itself — the rest is engine prose that changes under us on any engine
/// bump. So the verdict is taken from the typed error at the instant it is
/// flattened and parked here under the exact message that goes to the queue, and
/// `classify` looks that message up instead of reading it.
///
/// Two runs that produced an identical message necessarily reached an identical
/// verdict — the message is a pure function of the same typed value — so
/// concurrent adapters cannot disagree over an entry and the ring races
/// harmlessly. A message that is not in here is `Permanent`, which is also what a
/// miss degrades to: a wrongly-permanent failure costs one click on
/// `POST /api/v1/queue/jobs/{id}/retry`, a wrongly-transient one costs three
/// renders and the queue behind them.
static TRANSIENT: LazyLock<Mutex<VecDeque<String>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// The dispatcher classifies the moment `run` returns, so only the failures of
/// currently-running jobs are ever in flight.
const REMEMBERED: usize = 64;

pub(super) fn note(message: String, failure: Failure) -> String {
    if failure == Failure::Transient {
        let mut ring = TRANSIENT.lock().unwrap();
        if !ring.iter().any(|m| m == &message) {
            if ring.len() == REMEMBERED {
                ring.pop_front();
            }
            ring.push_back(message.clone());
        }
    }
    message
}

pub(super) fn recall(message: &str) -> Failure {
    if TRANSIENT.lock().unwrap().iter().any(|m| m == message) {
        Failure::Transient
    } else {
        Failure::Permanent
    }
}

/// The statuses shared by every engine in the fleet.
///
/// 🔴 429 is the fleet's back-pressure, not a client mistake: comfy-api answers it
/// when its pending queue is full and the sd.cpp servers when theirs are at 64 —
/// "try again shortly" in both. 503 is a drain. Everything else in the 4xx range
/// says the request itself is wrong and will be wrong again on every attempt.
pub(super) fn upstream(status: u16) -> Failure {
    match status {
        408 | 425 | 429 | 500..=599 => Failure::Transient,
        _ => Failure::Permanent,
    }
}

/// A request that never got an answer. A build error is ours and permanent; a
/// decode error means koblem and the engine disagree about the schema and will
/// disagree again. Everything else is a wire that broke — including a body that
/// stopped arriving, which is what a worker dying mid-render looks like from here.
pub(super) fn transport(e: &reqwest::Error) -> Failure {
    if e.is_connect() || e.is_timeout() || e.is_request() || e.is_body() {
        Failure::Transient
    } else {
        Failure::Permanent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_transient_only_if_its_typed_error_said_so() {
        let msg = note(
            "engine returned 429: queue full".to_string(),
            Failure::Transient,
        );
        assert_eq!(recall(&msg), Failure::Transient);
        let permanent = note(
            "engine returned 400: bad frames".to_string(),
            Failure::Permanent,
        );
        assert_eq!(recall(&permanent), Failure::Permanent);
        assert_eq!(recall("something nobody noted"), Failure::Permanent);
    }

    #[test]
    fn back_pressure_and_breakage_are_worth_another_slot_and_bad_requests_are_not() {
        for code in [408, 425, 429, 500, 502, 503, 504] {
            assert_eq!(upstream(code), Failure::Transient, "{code}");
        }
        for code in [400, 401, 403, 404, 409, 410, 413, 422] {
            assert_eq!(upstream(code), Failure::Permanent, "{code}");
        }
    }
}
