// The M3.1 acceptance centerpiece: BOTH byte-frozen harnesses (P1–P5 and
// Q1–Q6) instantiated for a DiskEngine running under a brutal 4096-byte
// node-cache budget — eviction pressure on every op, zero harness
// changes.
//
// Two wrapper subtleties, both consequences of frozen trait signatures:
// `check_invariants(&self)` needs `&mut` work under a budget (the tree
// cannot be fully resident), so the engine lives in a RefCell and the
// wrapper calls `check_invariants_full` (suspend budget → load_all →
// check → re-enforce). And `trace()/trace2()` return slices, which a
// RefCell borrow cannot, so the wrapper mirrors op/get/scan events into
// its own vectors — seqnos match the engine's exactly (both count
// mutating ops from 1), and the harnesses never depend on FlushDecision
// events (replay skips them; P5/Q5/Q6 filter them out).

#[macro_use]
#[path = "harness.rs"]
mod harness;

#[macro_use]
#[path = "harness2.rs"]
mod harness2;

use std::cell::RefCell;
use std::ops::Bound;

use beetree::{
    DiskEngine, FileVfs, Key, KvEngine, OpKind, OpKind2, Params, TraceEvent, TraceEvent2, UpsertOp,
    Value,
};
use harness::*;
use harness2::*;
use proptest::prelude::*;
use tempfile::TempDir;

/// A `DiskEngine` with a 4096-byte cache budget on a fresh tempdir.
struct TinyCacheDiskEngine {
    engine: RefCell<DiskEngine<FileVfs>>,
    v1: Vec<TraceEvent>,
    v2: Vec<TraceEvent2>,
    next_seq: u64,
    _dir: TempDir,
}

impl TinyCacheDiskEngine {
    fn record_op(&mut self, op: OpKind2) {
        self.next_seq += 1;
        if let OpKind2::Insert { key, value } = &op {
            self.v1.push(TraceEvent::Op {
                seq: self.next_seq,
                op: OpKind::Insert {
                    key: key.clone(),
                    value: value.clone(),
                },
            });
        }
        self.v2.push(TraceEvent2::Op {
            seq: self.next_seq,
            op,
        });
    }
}

impl KvEngine for TinyCacheDiskEngine {
    fn new(params: Params) -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let engine = DiskEngine::create_bounded(dir.path().join("bee.db"), params, 4096)
            .expect("create database");
        TinyCacheDiskEngine {
            engine: RefCell::new(engine),
            v1: Vec::new(),
            v2: Vec::new(),
            next_seq: 0,
            _dir: dir,
        }
    }

    fn insert(&mut self, key: Key, value: Value) {
        self.engine.borrow_mut().insert(key.clone(), value.clone());
        self.record_op(OpKind2::Insert { key, value });
    }

    fn delete(&mut self, key: Key) {
        self.engine.borrow_mut().delete(key.clone());
        self.record_op(OpKind2::Delete { key });
    }

    fn upsert(&mut self, key: Key, op: UpsertOp) {
        self.engine.borrow_mut().upsert(key.clone(), op);
        self.record_op(OpKind2::Upsert { key, op });
    }

    fn get(&mut self, key: &[u8]) -> Option<Value> {
        let value = self.engine.borrow_mut().get(key);
        self.v1.push(TraceEvent::Get { key: key.to_vec() });
        self.v2.push(TraceEvent2::Get { key: key.to_vec() });
        value
    }

    fn scan(
        &mut self,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Vec<(Key, Value)>, beetree::EngineError> {
        let result = self.engine.borrow_mut().scan(lo.clone(), hi.clone());
        self.v2.push(TraceEvent2::Scan { lo, hi });
        result
    }

    fn check_invariants(&self) -> Result<(), beetree::InvariantViolation> {
        self.engine.borrow_mut().check_invariants_full()
    }

    fn trace(&self) -> &[TraceEvent] {
        &self.v1
    }

    fn trace2(&self) -> &[TraceEvent2] {
        &self.v2
    }
}

instantiate_harness!(tiny_cache_v1, TinyCacheDiskEngine);
instantiate_harness2!(tiny_cache_v2, TinyCacheDiskEngine);
