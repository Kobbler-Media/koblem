//! Agentic chat: the local model orchestrates every GPU engine through real
//! OpenAI tool calls.
//!
//! * [`tools`] is the single registry — schemas, chooser text and dispatch all
//!   derive from it, so there is no second copy of the tool list.
//! * [`fold`] is the ONE place the tool-result wire shape is decided.
//! * [`director`] is the read side of the saved video projects — retrieval only,
//!   because `routes::ltx_director` owns every timeline mutation.
//! * [`run`] owns the loop, and honours the rule that no `LlmLease` is ever held
//!   across a tool call.
//! * [`store`] is the durable record the browser polls; a disconnect is a
//!   non-event.

pub mod availability;
pub mod director;
pub mod dispatch;
pub mod fold;
pub mod media;
pub mod prompt;
pub mod run;
pub mod store;
pub mod tools;
pub mod wire;
