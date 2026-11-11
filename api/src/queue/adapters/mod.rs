//! The engine adapters.
//!
//! One per [`koblem_client::types::JobKind`], each ~100–200 lines wrapping the
//! client code that already exists. This is not a rewrite: every runner already
//! had exactly this shape — submit, poll, collect, persist — and boot already
//! called five separate reattach functions by hand.

pub mod flux2;
pub mod h3;
pub mod krea2;
pub mod ltx25;
pub mod ltx_relip;
pub mod music3;
pub mod pixal3d_motion;
pub mod pixal3d_rig;
pub mod sa3;
pub mod songgen;
pub mod upscale;
mod verdict;

#[cfg(test)]
pub mod fake;
