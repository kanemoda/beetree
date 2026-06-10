//! beetree: a readable reference implementation of a Bε-tree storage engine.
//!
//! The executable specification came first (M0.1): the [`KvEngine`] trait,
//! trace recording and replay, a trivially correct [`NaiveEngine`], and the
//! generic property-test harness in `tests/harness.rs`. The real Bε-tree,
//! [`BeTree`] (M0.2), passes that harness unchanged. Semantics and
//! invariants are specified in `docs/SPEC.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod betree;
pub mod engine;
pub mod naive;
mod node;
pub mod trace;
pub mod types;

pub use betree::BeTree;
pub use engine::KvEngine;
pub use naive::NaiveEngine;
pub use trace::{OpKind, TraceEvent, from_jsonl, replay, to_jsonl};
pub use types::{CapacityKind, InvariantViolation, Key, Message, Params, Value};
