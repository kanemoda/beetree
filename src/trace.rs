//! Trace recording and replay.
//!
//! Every public mutating op is recorded as a [`TraceEvent::Op`] carrying the
//! global seqno it was assigned; reads are recorded as seqno-less
//! [`TraceEvent::Get`] events (ADR-0006). [`replay`] rebuilds an engine
//! from the `Op` events alone. [`TraceEvent::FlushDecision`] is produced
//! from M0.2 onward; it is a research hook for studying flush policies
//! offline.

use serde::{Deserialize, Serialize};

use crate::engine::KvEngine;
use crate::types::{Key, Params, Value};

/// What a recorded public mutating operation did — the M0 (v1)
/// vocabulary.
///
/// This enum is CLOSED: the byte-frozen `tests/harness.rs` matches over
/// it exhaustively, so deletes and upserts live in [`OpKind2`] instead
/// (M2.1; ADR-0013).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpKind {
    /// `insert(key, value)`.
    Insert {
        /// The key written.
        key: Key,
        /// The value written.
        value: Value,
    },
}

/// One event in an engine's trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceEvent {
    /// A public mutating op and the global seqno it was assigned.
    Op {
        /// Global sequence number assigned to this op.
        seq: u64,
        /// The operation performed.
        op: OpKind,
    },
    /// A public read.
    ///
    /// Reads carry no seqno (they do not participate in last-writer-wins
    /// ordering) and are skipped by [`replay`]; they are recorded for
    /// future workload-replay and cost analysis (ADR-0006).
    Get {
        /// The key probed.
        key: Key,
    },
    /// An internal node chose which child to flush its buffer toward.
    ///
    /// Unused by `NaiveEngine` and not produced until M0.2. Replay skips it:
    /// it describes what an engine did internally, not what it must do.
    FlushDecision {
        /// Identifier of the flushing node.
        node: u64,
        /// Pending buffered-message count per child at decision time.
        child_occupancies: Vec<usize>,
        /// Index into `child_occupancies` of the child flushed to.
        chosen: usize,
    },
}

/// Serialize `events` as JSON lines: one event per line.
///
/// The serialized form records events only — it does not record [`Params`],
/// so a trace file is not self-describing. Keep the originating parameters
/// alongside a saved trace and pass them to [`replay`]. (Database files are
/// different since M1.1: they persist their params in the superblock —
/// `docs/SPEC.md`, "On-disk format v1" — but traces remain out-of-band.)
pub fn to_jsonl(events: &[TraceEvent]) -> serde_json::Result<String> {
    let mut out = String::new();
    for event in events {
        out.push_str(&serde_json::to_string(event)?);
        out.push('\n');
    }
    Ok(out)
}

/// Parse JSON-lines text produced by [`to_jsonl`]. Blank lines are ignored.
pub fn from_jsonl(input: &str) -> serde_json::Result<Vec<TraceEvent>> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect()
}

/// Rebuild an engine by re-applying the `Op` events of a trace, in order.
///
/// `Op` events carry everything needed to reproduce the public-op sequence,
/// so the rebuilt engine assigns the same seqnos the original did.
/// `Get` and `FlushDecision` events are skipped (reads consume no seqno;
/// flush decisions are descriptive, not normative).
/// `params` should match the original engine's parameters.
pub fn replay<E: KvEngine>(params: Params, events: &[TraceEvent]) -> E {
    let mut engine = E::new(params);
    for event in events {
        match event {
            TraceEvent::Op {
                op: OpKind::Insert { key, value },
                ..
            } => engine.insert(key.clone(), value.clone()),
            TraceEvent::Get { .. } | TraceEvent::FlushDecision { .. } => {}
        }
    }
    engine
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_round_trips_all_event_kinds() {
        let events = vec![
            TraceEvent::Op {
                seq: 1,
                op: OpKind::Insert {
                    key: vec![7],
                    value: vec![1, 2, 3],
                },
            },
            TraceEvent::Get { key: vec![7] },
            TraceEvent::FlushDecision {
                node: 42,
                child_occupancies: vec![3, 0, 5],
                chosen: 2,
            },
        ];
        let text = to_jsonl(&events).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(from_jsonl(&text).unwrap(), events);
    }

    #[test]
    fn from_jsonl_ignores_blank_lines() {
        let events = vec![TraceEvent::Op {
            seq: 1,
            op: OpKind::Insert {
                key: vec![0],
                value: vec![],
            },
        }];
        let text = format!("\n{}\n\n", to_jsonl(&events).unwrap());
        assert_eq!(from_jsonl(&text).unwrap(), events);
    }
}

// ---------------------------------------------------------------------
// The v2 trace vocabulary (M2.1; ADR-0013).
//
// The frozen M0 harness matches exhaustively over `TraceEvent` and
// `OpKind`, so those enums are CLOSED — adding variants would break the
// byte-frozen `tests/harness.rs` at compile time. Delete and upsert
// therefore live in a parallel, full-fidelity vocabulary. `KvEngine::
// trace()` keeps returning the v1 view (faithful for insert-only
// workloads — exactly what the frozen harness generates); `trace2()` is
// the complete record and the only view safe to replay for mixed
// workloads.

use crate::types::UpsertOp;

/// What a recorded public mutating operation did — the complete M2.1
/// vocabulary (ADR-0013).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpKind2 {
    /// `insert(key, value)`.
    Insert {
        /// The key written.
        key: Key,
        /// The value written.
        value: Value,
    },
    /// `delete(key)`.
    Delete {
        /// The key removed.
        key: Key,
    },
    /// `upsert(key, op)`.
    Upsert {
        /// The key transformed.
        key: Key,
        /// The transformation applied.
        op: UpsertOp,
    },
}

/// One event in an engine's complete (v2) trace. Mirrors [`TraceEvent`]
/// with the full op vocabulary; the same recording rules apply (mutating
/// ops carry seqnos, reads and flush decisions do not).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceEvent2 {
    /// A public mutating op and the global seqno it was assigned.
    Op {
        /// Global sequence number assigned to this op.
        seq: u64,
        /// The operation performed.
        op: OpKind2,
    },
    /// A public read (seqno-free; skipped by replay; ADR-0006).
    Get {
        /// The key probed.
        key: Key,
    },
    /// An internal node chose which child to flush its buffer toward
    /// (descriptive, not normative; skipped by replay).
    FlushDecision {
        /// Identifier of the flushing node.
        node: u64,
        /// Pending buffered-message count per child at decision time.
        child_occupancies: Vec<usize>,
        /// Index into `child_occupancies` of the child flushed to.
        chosen: usize,
    },
}

/// Rebuild an engine by re-applying the `Op` events of a complete (v2)
/// trace, in order — the only replay that is faithful for workloads
/// containing deletes or upserts (ADR-0013).
pub fn replay2<E: KvEngine>(params: Params, events: &[TraceEvent2]) -> E {
    let mut engine = E::new(params);
    for event in events {
        match event {
            TraceEvent2::Op { op, .. } => match op {
                OpKind2::Insert { key, value } => engine.insert(key.clone(), value.clone()),
                OpKind2::Delete { key } => engine.delete(key.clone()),
                OpKind2::Upsert { key, op } => engine.upsert(key.clone(), *op),
            },
            TraceEvent2::Get { .. } | TraceEvent2::FlushDecision { .. } => {}
        }
    }
    engine
}

/// The dual-view trace store every engine records into: one push feeds
/// both the complete v2 trace and the frozen-vocabulary v1 mirror (which
/// silently omits ops the v1 vocabulary cannot express; ADR-0013).
#[derive(Debug, Default)]
pub(crate) struct Recorder {
    v1: Vec<TraceEvent>,
    v2: Vec<TraceEvent2>,
}

impl Recorder {
    /// Record a public mutating op under its assigned seqno.
    pub fn op(&mut self, seq: u64, op: OpKind2) {
        if let OpKind2::Insert { key, value } = &op {
            self.v1.push(TraceEvent::Op {
                seq,
                op: OpKind::Insert {
                    key: key.clone(),
                    value: value.clone(),
                },
            });
        }
        self.v2.push(TraceEvent2::Op { seq, op });
    }

    /// Record a public read.
    pub fn get(&mut self, key: &[u8]) {
        self.v1.push(TraceEvent::Get { key: key.to_vec() });
        self.v2.push(TraceEvent2::Get { key: key.to_vec() });
    }

    /// Record a flush decision.
    pub fn flush_decision(&mut self, node: u64, child_occupancies: Vec<usize>, chosen: usize) {
        self.v1.push(TraceEvent::FlushDecision {
            node,
            child_occupancies: child_occupancies.clone(),
            chosen,
        });
        self.v2.push(TraceEvent2::FlushDecision {
            node,
            child_occupancies,
            chosen,
        });
    }

    /// The v1 view (insert-only-faithful; for the frozen harness).
    pub fn v1(&self) -> &[TraceEvent] {
        &self.v1
    }

    /// The complete v2 record.
    pub fn v2(&self) -> &[TraceEvent2] {
        &self.v2
    }
}
