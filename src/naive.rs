//! A trivially correct reference engine backed by [`BTreeMap`].

use std::collections::BTreeMap;

use crate::engine::KvEngine;
use crate::trace::{OpKind2, Recorder, TraceEvent, TraceEvent2};
use crate::types::{InvariantViolation, Key, Params, UpsertOp, Value};

/// The M0.1 reference engine: a thin shell around [`BTreeMap`].
///
/// It exists to validate the test harnesses before the real engines do.
/// It assigns seqnos and records trace events exactly like a real engine,
/// but it has no tree structure of its own, so `check_invariants` is
/// vacuously `Ok`. Its delete/upsert behavior (M2.1) IS the oracle
/// semantics: remove the entry; transform per `UpsertOp::apply`.
///
/// ```
/// use beetree::{KvEngine, NaiveEngine, Params, UpsertOp};
///
/// let mut engine = NaiveEngine::new(Params::default());
/// engine.insert(b"k".to_vec(), b"v1".to_vec());
/// engine.upsert(b"n".to_vec(), UpsertOp::Add(2));
/// engine.delete(b"k".to_vec());
/// assert_eq!(engine.get(b"k"), None);
/// assert_eq!(engine.get(b"n"), Some(2i64.to_le_bytes().to_vec()));
/// ```
pub struct NaiveEngine {
    map: BTreeMap<Key, Value>,
    next_seq: u64,
    trace: Recorder,
}

impl KvEngine for NaiveEngine {
    fn new(_params: Params) -> Self {
        // The naive engine has no nodes, so the capacities in `params` have
        // nothing to bound; it accepts them only to satisfy the trait.
        NaiveEngine {
            map: BTreeMap::new(),
            next_seq: 0,
            trace: Recorder::default(),
        }
    }

    fn insert(&mut self, key: Key, value: Value) {
        self.next_seq += 1;
        self.trace.op(
            self.next_seq,
            OpKind2::Insert {
                key: key.clone(),
                value: value.clone(),
            },
        );
        self.map.insert(key, value);
    }

    fn delete(&mut self, key: Key) {
        self.next_seq += 1;
        self.trace
            .op(self.next_seq, OpKind2::Delete { key: key.clone() });
        self.map.remove(&key);
    }

    fn upsert(&mut self, key: Key, op: UpsertOp) {
        self.next_seq += 1;
        self.trace.op(
            self.next_seq,
            OpKind2::Upsert {
                key: key.clone(),
                op,
            },
        );
        let value = op.apply(self.map.get(&key).map(|v| v.as_slice()));
        self.map.insert(key, value);
    }

    fn get(&mut self, key: &[u8]) -> Option<Value> {
        self.trace.get(key);
        self.map.get(key).cloned()
    }

    fn check_invariants(&self) -> Result<(), InvariantViolation> {
        Ok(())
    }

    fn trace(&self) -> &[TraceEvent] {
        self.trace.v1()
    }

    fn trace2(&self) -> &[TraceEvent2] {
        self.trace.v2()
    }
}
