//! A trivially correct reference engine backed by [`BTreeMap`].

use std::collections::BTreeMap;

use crate::engine::KvEngine;
use crate::trace::{OpKind, TraceEvent};
use crate::types::{InvariantViolation, Key, Params, Value};

/// The M0.1 reference engine: a thin shell around [`BTreeMap`].
///
/// It exists to validate the test harness before the real Bε-tree (M0.2)
/// does. It assigns seqnos and records trace events exactly like a real
/// engine, but it has no tree structure of its own, so `check_invariants`
/// is vacuously `Ok`.
///
/// ```
/// use beetree::{KvEngine, NaiveEngine, Params};
///
/// let mut engine = NaiveEngine::new(Params::default());
/// engine.insert(b"k".to_vec(), b"v1".to_vec());
/// engine.insert(b"k".to_vec(), b"v2".to_vec());
/// assert_eq!(engine.get(b"k"), Some(b"v2".to_vec()));
/// assert_eq!(engine.trace().len(), 3); // two Op events + one Get event
/// ```
pub struct NaiveEngine {
    map: BTreeMap<Key, Value>,
    next_seq: u64,
    trace: Vec<TraceEvent>,
}

impl KvEngine for NaiveEngine {
    fn new(_params: Params) -> Self {
        // The naive engine has no nodes, so the capacities in `params` have
        // nothing to bound; it accepts them only to satisfy the trait.
        NaiveEngine {
            map: BTreeMap::new(),
            next_seq: 0,
            trace: Vec::new(),
        }
    }

    fn insert(&mut self, key: Key, value: Value) {
        self.next_seq += 1;
        self.trace.push(TraceEvent::Op {
            seq: self.next_seq,
            op: OpKind::Insert {
                key: key.clone(),
                value: value.clone(),
            },
        });
        self.map.insert(key, value);
    }

    fn get(&mut self, key: &[u8]) -> Option<Value> {
        self.trace.push(TraceEvent::Get { key: key.to_vec() });
        self.map.get(key).cloned()
    }

    fn check_invariants(&self) -> Result<(), InvariantViolation> {
        Ok(())
    }

    fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }
}
