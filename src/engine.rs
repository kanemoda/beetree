//! The engine interface every key-value implementation must satisfy.

use crate::trace::{TraceEvent, TraceEvent2};
use crate::types::{InvariantViolation, Key, Params, UpsertOp, Value};

/// A single-threaded key-value engine (`docs/SPEC.md`; inserts since M0,
/// deletes and upserts since M2.1).
///
/// The frozen property-test harness in `tests/harness.rs` is written
/// against the M0 subset of this trait: [`NaiveEngine`](crate::NaiveEngine)
/// validates the harness in M0.1, and every real engine must keep passing
/// it unchanged. The full-mix harness (`tests/harness2.rs`, frozen when
/// M2.2 ships) exercises the complete surface.
pub trait KvEngine {
    /// Create an empty engine with the given structure parameters.
    fn new(params: Params) -> Self
    where
        Self: Sized;

    /// Insert or overwrite `key` with `value`.
    ///
    /// Assigns the next global seqno (starting at 1 for the first op) and
    /// records a [`TraceEvent::Op`]. Last-writer-wins: a later insert for the
    /// same key shadows all earlier ones.
    fn insert(&mut self, key: Key, value: Value);

    /// Remove `key` (M2.1). Consumes a seqno and is recorded and replayed
    /// like every mutating op; deleting an absent key is a legal no-op
    /// with a seqno.
    fn delete(&mut self, key: Key);

    /// Blindly transform the value at `key` per `op` (M2.1) — absent and
    /// deleted keys transform from the empty base (`docs/SPEC.md`,
    /// "Upsert semantics"). Consumes a seqno; recorded and replayed.
    fn upsert(&mut self, key: Key, op: UpsertOp);

    /// The newest value for `key` under the full message algebra: `None`
    /// for a key never written OR deleted since; a deleted-then-upserted
    /// key folds from base 0 (`docs/SPEC.md`, "Reads").
    ///
    /// Records a [`TraceEvent::Get`]: reads are traced for future
    /// workload-replay and cost analysis, but carry no seqno (ADR-0006).
    /// Recording is why `get` takes `&mut self` — the trace is engine
    /// state, and the engine is single-threaded by design (`CLAUDE.md`).
    fn get(&mut self, key: &[u8]) -> Option<Value>;

    /// Walk the entire structure and verify invariants I1–I7 (`docs/SPEC.md`).
    ///
    /// The harness calls this after every public op, so implementations may
    /// be slow — but they must be exhaustive.
    fn check_invariants(&self) -> Result<(), InvariantViolation>;

    /// The M0-vocabulary (v1) trace view: insert, get, and flush-decision
    /// events only. Faithful for insert-only workloads — exactly what the
    /// frozen harness generates; a mixed workload's deletes and upserts
    /// are NOT visible here (ADR-0013). Use [`KvEngine::trace2`] for the
    /// complete record.
    fn trace(&self) -> &[TraceEvent];

    /// The complete trace (M2.1): every mutating op, every read, every
    /// flush decision. The only view that [`replay2`](crate::replay2) can
    /// faithfully rebuild mixed workloads from (ADR-0013).
    fn trace2(&self) -> &[TraceEvent2];
}
