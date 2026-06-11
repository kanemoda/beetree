//! beetree: a readable reference implementation of a Bε-tree storage engine.
//!
//! The executable specification came first (M0.1): the [`KvEngine`] trait,
//! trace recording and replay, a trivially correct [`NaiveEngine`], and the
//! generic property-test harness in `tests/harness.rs`. The real Bε-tree,
//! [`BeTree`] (M0.2), passes that harness unchanged. M1.1 adds persistence:
//! [`DiskEngine`] keeps copy-on-write nodes in a single file behind a
//! [`Vfs`], commits atomically through dual superblocks, and recovers to
//! exactly the last committed state. Semantics and invariants are specified
//! in `docs/SPEC.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod betree;
mod check;
pub mod disk;
pub mod engine;
mod format;
pub mod naive;
mod node;
pub mod trace;
pub mod types;
pub mod vfs;

pub use betree::BeTree;
pub use disk::{CommitStats, DiskEngine, DiskError};
pub use engine::{EngineError, KvEngine};
pub use naive::NaiveEngine;
pub use trace::{OpKind, OpKind2, TraceEvent, TraceEvent2, from_jsonl, replay, replay2, to_jsonl};
pub use types::{CapacityKind, InvariantViolation, Key, Message, Params, UpsertOp, Value};
#[cfg(unix)]
pub use vfs::FileVfs;
pub use vfs::{Fate, FaultyVfs, Vfs, VfsOp};
