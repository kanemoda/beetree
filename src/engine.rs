//! The engine interface every key-value implementation must satisfy.

use crate::trace::TraceEvent;
use crate::types::{InvariantViolation, Key, Params, Value};

/// A single-threaded, insert-only key-value engine (M0 scope; `docs/SPEC.md`).
///
/// The property-test harness in `tests/harness.rs` is written against this
/// trait alone: [`NaiveEngine`](crate::NaiveEngine) validates the harness in
/// M0.1, and the real Bε-tree (M0.2) must implement this trait and pass the
/// harness unchanged.
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

    /// The newest value written for `key`, or `None` if it was never written.
    ///
    /// Records a [`TraceEvent::Get`]: reads are traced for future
    /// workload-replay and cost analysis, but carry no seqno (ADR-0006).
    /// Recording is why `get` takes `&mut self` — the trace is engine
    /// state, and the engine is single-threaded by design (`CLAUDE.md`).
    fn get(&mut self, key: &[u8]) -> Option<Value>;

    /// Walk the entire structure and verify invariants I1–I6 (`docs/SPEC.md`).
    ///
    /// The harness calls this after every public op, so implementations may
    /// be slow — but they must be exhaustive.
    fn check_invariants(&self) -> Result<(), InvariantViolation>;

    /// All trace events recorded so far, in the order they happened.
    fn trace(&self) -> &[TraceEvent];
}
