//! The dispatcher's only source of time.
//!
//! 🔴 Nothing in dispatcher logic may reach for `Instant::now()` directly. A
//! scheduler is ordering, time and concurrency — exactly what breaks silently and
//! gets discovered at 3am — and every interesting property (head reservation
//! after N seconds, blocked-for-hours, eviction cooldown, the ETA replay) is a
//! statement about elapsed time. A test that has to *sleep* to assert one of them
//! is a test nobody runs.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use time::OffsetDateTime;

pub trait ClockSource: Send + Sync + 'static {
    /// Unix milliseconds. One number, because the dispatcher only ever needs
    /// "now" and differences, and a single monotonic-ish integer is trivially
    /// controllable from a test.
    fn now_ms(&self) -> i64;
}

#[derive(Clone)]
pub struct Clock(Arc<dyn ClockSource>);

impl Clock {
    pub fn system() -> Self {
        Clock(Arc::new(SystemClock))
    }

    pub fn new(src: Arc<dyn ClockSource>) -> Self {
        Clock(src)
    }

    pub fn now_ms(&self) -> i64 {
        self.0.now_ms()
    }

    /// For the DB columns. Falls back to the epoch on an out-of-range value
    /// rather than panicking — a clock that has gone mad must not take the
    /// dispatcher with it.
    pub fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(self.now_ms() as i128 * 1_000_000)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH)
    }

    /// Milliseconds since `then`, floored at zero. Clamped because a queue row's
    /// timestamps come from the database's clock and this one is koblem's; a
    /// negative "waiting -3ms" is noise, not information.
    pub fn since_ms(&self, then: i64) -> i64 {
        (self.now_ms() - then).max(0)
    }
}

struct SystemClock;

impl ClockSource for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A clock the test drives by hand.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct TestClock(Arc<AtomicI64>);

#[cfg(test)]
impl TestClock {
    pub fn new(start_ms: i64) -> Self {
        TestClock(Arc::new(AtomicI64::new(start_ms)))
    }

    pub fn advance_ms(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::Relaxed);
    }

    pub fn advance_secs(&self, secs: i64) {
        self.advance_ms(secs * 1000);
    }

    pub fn clock(&self) -> Clock {
        Clock::new(Arc::new(self.clone()))
    }
}

#[cfg(test)]
impl ClockSource for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test clock only moves when the test moves it — that is the whole point.
    #[test]
    fn a_test_clock_stands_still() {
        let t = TestClock::new(1_000);
        let c = t.clock();
        assert_eq!(c.now_ms(), 1_000);
        assert_eq!(c.now_ms(), 1_000);
        t.advance_secs(90);
        assert_eq!(c.now_ms(), 91_000);
        assert_eq!(c.since_ms(1_000), 90_000);
    }

    /// A timestamp from the future reads as zero elapsed, not as negative. The
    /// row's clock is Postgres's and this one is koblem's; they disagree.
    #[test]
    fn elapsed_never_goes_backwards() {
        let c = TestClock::new(1_000).clock();
        assert_eq!(c.since_ms(9_000), 0);
    }
}
