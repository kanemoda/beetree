//! The full-op-mix property harness (M2.1): Q1–Q5 over
//! insert/delete/upsert/get, generic over [`KvEngine`].
//!
//! Mirrors the byte-frozen M0 harness (`tests/harness.rs`) — which stays
//! the compatibility gate for the insert-only surface — and adds the M2.1
//! vocabulary via `trace2`/`replay2` (ADR-0013). NOT frozen until M2.2
//! ships. Instantiated below for NaiveEngine, BeTree, and DiskEngine (via
//! a tempdir wrapper).

use std::collections::BTreeMap;

use beetree::{
    DiskEngine, FileVfs, Key, KvEngine, OpKind2, Params, TraceEvent2, UpsertOp, Value, replay2,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tempfile::TempDir;

/// One public mutating operation in a generated workload (M2.1 full mix).
#[derive(Debug, Clone)]
pub(crate) enum Op2 {
    Insert { key: Key, value: Value },
    Delete { key: Key },
    Upsert { key: Key, delta: i64 },
}

impl Op2 {
    /// The key this op touches.
    fn key(&self) -> &Key {
        match self {
            Op2::Insert { key, .. } | Op2::Delete { key } | Op2::Upsert { key, .. } => key,
        }
    }
}

/// Keys come from a small domain — a single byte, as `Vec<u8>` — so
/// workloads collide constantly and the full domain stays cheap to sweep.
fn key_strategy() -> impl Strategy<Value = Key> {
    any::<u8>().prop_map(|b| vec![b])
}

/// Values are short random byte strings; empty values are legal, and the
/// range straddles 8 deliberately — EXACTLY 8 bytes is an upsert base,
/// while shorter AND longer values must both fold from base 0 (SPEC,
/// "Upsert semantics").
fn value_strategy() -> impl Strategy<Value = Value> {
    proptest::collection::vec(any::<u8>(), 0..=10)
}

/// One op with the given insert/delete/upsert weights.
fn op2_strategy(wi: u32, wd: u32, wu: u32) -> impl Strategy<Value = Op2> {
    prop_oneof![
        wi => (key_strategy(), value_strategy())
            .prop_map(|(key, value)| Op2::Insert { key, value }),
        wd => key_strategy().prop_map(|key| Op2::Delete { key }),
        wu => (key_strategy(), any::<i64>())
            .prop_map(|(key, delta)| Op2::Upsert { key, delta }),
    ]
}

/// Op sequences of length 0..=2000, drawn from one of three weightings:
/// balanced, delete-heavy, and upsert-heavy.
pub(crate) fn ops2_strategy() -> impl Strategy<Value = Vec<Op2>> {
    prop_oneof![
        proptest::collection::vec(op2_strategy(5, 2, 2), 0..=2000),
        proptest::collection::vec(op2_strategy(2, 6, 1), 0..=2000),
        proptest::collection::vec(op2_strategy(2, 1, 6), 0..=2000),
    ]
}

/// Apply one op to the engine under test.
pub(crate) fn apply2<E: KvEngine>(engine: &mut E, op: &Op2) {
    match op {
        Op2::Insert { key, value } => engine.insert(key.clone(), value.clone()),
        Op2::Delete { key } => engine.delete(key.clone()),
        Op2::Upsert { key, delta } => engine.upsert(key.clone(), UpsertOp::Add(*delta)),
    }
}

/// Apply one op to the shadow oracle, mirroring the SPEC semantics.
pub(crate) fn apply_oracle2(oracle: &mut BTreeMap<Key, Value>, op: &Op2) {
    match op {
        Op2::Insert { key, value } => {
            oracle.insert(key.clone(), value.clone());
        }
        Op2::Delete { key } => {
            oracle.remove(key);
        }
        Op2::Upsert { key, delta } => {
            let value = UpsertOp::Add(*delta).apply(oracle.get(key).map(|v| v.as_slice()));
            oracle.insert(key.clone(), value);
        }
    }
}

/// Every key in the generated domain, for full sweeps.
pub(crate) fn full_domain() -> impl Iterator<Item = Key> {
    (0..=u8::MAX).map(|b| vec![b])
}

/// Q1: interleaved full-mix oracle — after every op, `get` agrees with the
/// shadow BTreeMap (which carries the same Add semantics) for the touched
/// key.
pub(crate) fn check_q1_interleaved_oracle<E: KvEngine>(ops: &[Op2]) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    let mut oracle = BTreeMap::new();
    for (i, op) in ops.iter().enumerate() {
        apply2(&mut engine, op);
        apply_oracle2(&mut oracle, op);
        let key = op.key();
        prop_assert_eq!(
            engine.get(key),
            oracle.get(key).cloned(),
            "Q1: get({:?}) diverged from oracle after op {} ({:?})",
            key,
            i,
            op
        );
    }
    Ok(())
}

/// Q2: final sweep — after the whole sequence, `get` agrees with the
/// oracle for every key in the domain, including keys never written.
pub(crate) fn check_q2_final_sweep<E: KvEngine>(ops: &[Op2]) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    let mut oracle = BTreeMap::new();
    for op in ops {
        apply2(&mut engine, op);
        apply_oracle2(&mut oracle, op);
    }
    for key in full_domain() {
        prop_assert_eq!(
            engine.get(&key),
            oracle.get(&key).cloned(),
            "Q2: get({:?}) diverged from oracle on the final sweep",
            &key
        );
    }
    Ok(())
}

/// Q3: invariants — `check_invariants()` (I1–I7) is Ok on the empty engine
/// and after every op.
pub(crate) fn check_q3_invariants<E: KvEngine>(ops: &[Op2]) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    if let Err(violation) = engine.check_invariants() {
        return Err(TestCaseError::fail(format!(
            "Q3: empty engine violates invariants: {violation}"
        )));
    }
    for (i, op) in ops.iter().enumerate() {
        apply2(&mut engine, op);
        if let Err(violation) = engine.check_invariants() {
            return Err(TestCaseError::fail(format!(
                "Q3: invariant violated after op {i} ({op:?}): {violation}"
            )));
        }
    }
    Ok(())
}

/// Q4: replay determinism — replaying the recorded v2 trace into a fresh
/// engine yields identical answers on a full-domain sweep.
pub(crate) fn check_q4_replay_determinism<E: KvEngine>(ops: &[Op2]) -> Result<(), TestCaseError> {
    let params = Params::default();
    let mut engine = E::new(params);
    for op in ops {
        apply2(&mut engine, op);
    }
    let mut replayed: E = replay2(params, engine.trace2());
    for key in full_domain() {
        prop_assert_eq!(
            replayed.get(&key),
            engine.get(&key),
            "Q4: replayed engine diverged from the original at {:?}",
            &key
        );
    }
    Ok(())
}

/// Q5: trace well-formedness — n mutating ops ⇒ n `Op` events in trace2
/// with seqnos contiguous from 1 and payloads identical to what was
/// issued, including Delete and Upsert. Get and FlushDecision events may
/// be interleaved anywhere; they are ignored.
pub(crate) fn check_q5_trace_well_formedness<E: KvEngine>(
    ops: &[Op2],
) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    for op in ops {
        apply2(&mut engine, op);
    }
    let recorded: Vec<(u64, &OpKind2)> = engine
        .trace2()
        .iter()
        .filter_map(|event| match event {
            TraceEvent2::Op { seq, op } => Some((*seq, op)),
            TraceEvent2::Get { .. } | TraceEvent2::FlushDecision { .. } => None,
        })
        .collect();
    prop_assert_eq!(
        recorded.len(),
        ops.len(),
        "Q5: {} ops were issued but {} Op events were recorded",
        ops.len(),
        recorded.len()
    );
    for (i, ((seq, kind), issued)) in recorded.iter().zip(ops).enumerate() {
        prop_assert_eq!(
            *seq,
            i as u64 + 1,
            "Q5: op {} was recorded with seq {}, expected {}",
            i,
            seq,
            i + 1
        );
        let matches = match (kind, issued) {
            (OpKind2::Insert { key, value }, Op2::Insert { key: ik, value: iv }) => {
                key == ik && value == iv
            }
            (OpKind2::Delete { key }, Op2::Delete { key: ik }) => key == ik,
            (
                OpKind2::Upsert {
                    key,
                    op: UpsertOp::Add(d),
                },
                Op2::Upsert { key: ik, delta },
            ) => key == ik && d == delta,
            _ => false,
        };
        prop_assert!(
            matches,
            "Q5: op {} was recorded as {:?} but issued as {:?}",
            i,
            kind,
            issued
        );
    }
    Ok(())
}

/// Instantiate the full Q1–Q5 harness for an engine type:
/// `instantiate_harness2!(module_name, EngineType);`
macro_rules! instantiate_harness2 {
    ($module:ident, $engine:ty) => {
        mod $module {
            use super::*;

            proptest! {
                #[test]
                fn q1_interleaved_oracle(ops in ops2_strategy()) {
                    check_q1_interleaved_oracle::<$engine>(&ops)?;
                }

                #[test]
                fn q2_final_sweep(ops in ops2_strategy()) {
                    check_q2_final_sweep::<$engine>(&ops)?;
                }

                #[test]
                fn q3_invariants(ops in ops2_strategy()) {
                    check_q3_invariants::<$engine>(&ops)?;
                }

                #[test]
                fn q4_replay_determinism(ops in ops2_strategy()) {
                    check_q4_replay_determinism::<$engine>(&ops)?;
                }

                #[test]
                fn q5_trace_well_formedness(ops in ops2_strategy()) {
                    check_q5_trace_well_formedness::<$engine>(&ops)?;
                }
            }
        }
    };
}

/// A `DiskEngine` on a fresh tempdir so Q1–Q5 can drive a disk-backed
/// engine through `KvEngine::new` (same pattern as `tests/disk.rs`).
struct TempDiskEngine2 {
    engine: DiskEngine<FileVfs>,
    _dir: TempDir,
}

impl KvEngine for TempDiskEngine2 {
    fn new(params: Params) -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let engine =
            DiskEngine::create(dir.path().join("bee.db"), params).expect("create database");
        TempDiskEngine2 { engine, _dir: dir }
    }

    fn insert(&mut self, key: Key, value: Value) {
        self.engine.insert(key, value);
    }

    fn delete(&mut self, key: Key) {
        self.engine.delete(key);
    }

    fn upsert(&mut self, key: Key, op: UpsertOp) {
        self.engine.upsert(key, op);
    }

    fn get(&mut self, key: &[u8]) -> Option<Value> {
        self.engine.get(key)
    }

    fn check_invariants(&self) -> Result<(), beetree::InvariantViolation> {
        self.engine.check_invariants()
    }

    fn trace(&self) -> &[beetree::TraceEvent] {
        self.engine.trace()
    }

    fn trace2(&self) -> &[TraceEvent2] {
        self.engine.trace2()
    }
}

instantiate_harness2!(naive_engine, beetree::NaiveEngine);
instantiate_harness2!(betree_engine, beetree::BeTree);
instantiate_harness2!(disk_engine, TempDiskEngine2);

// Shrunk minimal cases from proptest failures get recorded here as
// permanent #[test] regressions before the fix lands (CLAUDE.md). None
// yet.
