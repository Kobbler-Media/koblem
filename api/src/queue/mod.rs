//! The global GPU job queue.
//!
//! **Position:** the queue is the *scheduler*. `kob-gpu-gate` stops being the
//! thing work waits on and becomes the enforcement/safety layer — it still owns
//! the VRAM ledger, placement and eviction, but the queue decides *what runs next*
//! and can drive unloads ahead of time because it knows what is coming.
//!
//! Reading order:
//!
//! * [`admit`] — the decision, pure and table-tested. Start here.
//! * [`lane`] — the second admission axis. VRAM is not the only resource.
//! * [`files`] — the working files a run owns on disk, and nothing that outlives one.
//! * [`dispatcher`] — the I/O around the decision, and the boot sequence.
//! * [`planner`] — §7's queue-driven unloads.
//! * [`estimate`] — "done by ~04:20", by replaying the dispatcher forward.
//! * [`runner`] — what each of the thirteen adapters implements.

pub mod adapters;
pub mod admit;
pub mod clock;
pub mod dispatcher;
pub mod estimate;
pub mod events;
pub mod files;
pub mod health;
pub mod lane;
pub mod notify;
pub mod planner;
pub mod push;
pub mod runner;

pub use dispatcher::{Dispatcher, Registry};
