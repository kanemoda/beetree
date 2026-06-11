//! The full-op-mix property harness (M2.1–M2.2): Q1–Q6 over
//! insert/delete/upsert/get/scan, generic over [`KvEngine`].
//!
//! Mirrors the byte-frozen M0 harness (`tests/harness.rs`) — which stays
//! the compatibility gate for the insert-only surface — and adds the
//! M2 vocabulary via `trace2`/`replay2`/`scan` (ADR-0013, ADR-0014).
//! FROZEN as of M2.2: this file must stay byte-identical (hash in the
//! README freeze table); future engines instantiate it from their own
//! test files exactly like `tests/harness.rs`. Instantiated below for
//! NaiveEngine, BeTree, and DiskEngine (via a tempdir wrapper).

use std::collections::BTreeMap;
use std::ops::Bound;

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
            // Deliberately independent of the crate's UpsertOp::apply
            // (which every engine routes through): the oracle restates
            // the SPEC Add semantics from scratch, so a regression in the
            // canonical fold cannot hide in lockstep.
            let base = match oracle.get(key) {
                Some(v) if v.len() == 8 => {
                    i64::from_le_bytes(v.as_slice().try_into().expect("8 bytes"))
                }
                _ => 0,
            };
            oracle.insert(
                key.clone(),
                base.wrapping_add(*delta).to_le_bytes().to_vec(),
            );
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
            TraceEvent2::Get { .. }
            | TraceEvent2::Scan { .. }
            | TraceEvent2::FlushDecision { .. } => None,
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

/// One step of a Q6 workload: a mutating op or an interleaved scan.
#[derive(Debug, Clone)]
pub(crate) enum Step6 {
    Op(Op2),
    Scan(Bound<Key>, Bound<Key>),
}

/// Bound keys range over 0..=2 bytes — deliberately WIDER than the
/// single-byte key domain: the empty key, exact domain keys, and 2-byte
/// keys that fall strictly between domain keys or stand in prefix
/// relation to pivots all exercise the length-asymmetric lexicographic
/// comparisons in the engines' clipping logic.
fn bound_key_strategy() -> impl Strategy<Value = Key> {
    proptest::collection::vec(any::<u8>(), 0..=2)
}

fn bound_strategy() -> impl Strategy<Value = Bound<Key>> {
    prop_oneof![
        bound_key_strategy().prop_map(Bound::Included),
        bound_key_strategy().prop_map(Bound::Excluded),
        Just(Bound::Unbounded),
    ]
}

fn steps6_with(wi: u32, wd: u32, wu: u32) -> impl Strategy<Value = Vec<Step6>> {
    proptest::collection::vec(
        prop_oneof![
            7 => op2_strategy(wi, wd, wu).prop_map(Step6::Op),
            1 => (bound_strategy(), bound_strategy())
                .prop_map(|(lo, hi)| Step6::Scan(lo, hi)),
        ],
        0..=1500,
    )
}

/// Q6 workloads: the same three weightings as Q1–Q5 (balanced,
/// delete-heavy so scans hit reclaimed structure, upsert-heavy so scans
/// fold deep pending stacks), with scans sprinkled in. Random bounds
/// cover every Included/Excluded/Unbounded combination — including
/// inverted ranges (empty) and double-Unbounded (full domain).
pub(crate) fn steps6_strategy() -> impl Strategy<Value = Vec<Step6>> {
    prop_oneof![
        steps6_with(5, 2, 2),
        steps6_with(2, 6, 1),
        steps6_with(2, 1, 6),
    ]
}

/// Does `key` lie within `(lo, hi)`? (The oracle-side bound predicate;
/// deliberately independent of the engines' range plumbing.)
fn in_bounds(key: &[u8], lo: &Bound<Key>, hi: &Bound<Key>) -> bool {
    let lo_ok = match lo {
        Bound::Unbounded => true,
        Bound::Included(a) => key >= a.as_slice(),
        Bound::Excluded(a) => key > a.as_slice(),
    };
    let hi_ok = match hi {
        Bound::Unbounded => true,
        Bound::Included(b) => key <= b.as_slice(),
        Bound::Excluded(b) => key < b.as_slice(),
    };
    lo_ok && hi_ok
}

/// Q6: scan equivalence — every interleaved scan returns exactly the
/// oracle's in-range contents in ascending key order, AND agrees with
/// per-key `get` over the domain∩range (in-transit tombstones suppress,
/// pending upsert stacks fold; SPEC "Range scans").
pub(crate) fn check_q6_scan_equivalence<E: KvEngine>(steps: &[Step6]) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    let mut oracle = BTreeMap::new();
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step6::Op(op) => {
                apply2(&mut engine, op);
                apply_oracle2(&mut oracle, op);
            }
            Step6::Scan(lo, hi) => {
                let got = match engine.scan(lo.clone(), hi.clone()) {
                    Ok(got) => got,
                    Err(e) => {
                        return Err(TestCaseError::fail(format!(
                            "Q6: scan {lo:?}..{hi:?} failed at step {i}: {e}"
                        )));
                    }
                };
                let expected: Vec<(Key, Value)> = oracle
                    .iter()
                    .filter(|(k, _)| in_bounds(k, lo, hi))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                prop_assert_eq!(
                    &got,
                    &expected,
                    "Q6: scan {:?}..{:?} diverged from the oracle at step {}",
                    lo,
                    hi,
                    i
                );
                // And scan must agree with get, key by key, over the
                // domain ∩ range.
                let by_key: BTreeMap<&Key, &Value> = got.iter().map(|(k, v)| (k, v)).collect();
                for key in full_domain().filter(|k| in_bounds(k, lo, hi)) {
                    prop_assert_eq!(
                        by_key.get(&key).map(|v| (*v).clone()),
                        engine.get(&key),
                        "Q6: scan and get disagree on {:?} (step {})",
                        &key,
                        i
                    );
                }
            }
        }
    }
    // The scan trace contract (SPEC "Range scans"): every mutating step
    // is exactly one Op event with seqnos contiguous from 1 — scans
    // consume NO seqno — and every scan is recorded as exactly one
    // seqno-free Scan event.
    let n_ops = steps.iter().filter(|s| matches!(s, Step6::Op(_))).count();
    let n_scans = steps
        .iter()
        .filter(|s| matches!(s, Step6::Scan(..)))
        .count();
    let mut op_events = 0usize;
    let mut scan_events = 0usize;
    for event in engine.trace2() {
        match event {
            TraceEvent2::Op { seq, .. } => {
                op_events += 1;
                prop_assert_eq!(
                    *seq,
                    op_events as u64,
                    "Q6: op seqnos must stay contiguous around scans"
                );
            }
            TraceEvent2::Scan { .. } => scan_events += 1,
            TraceEvent2::Get { .. } | TraceEvent2::FlushDecision { .. } => {}
        }
    }
    prop_assert_eq!(
        op_events,
        n_ops,
        "Q6: every mutating step is exactly one Op event (scans are not ops)"
    );
    prop_assert_eq!(
        scan_events,
        n_scans,
        "Q6: every scan is recorded exactly once, seqno-free"
    );
    Ok(())
}

/// Instantiate the full Q1–Q6 harness for an engine type:
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

                #[test]
                fn q6_scan_equivalence(steps in steps6_strategy()) {
                    check_q6_scan_equivalence::<$engine>(&steps)?;
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

    fn scan(
        &mut self,
        lo: std::ops::Bound<Vec<u8>>,
        hi: std::ops::Bound<Vec<u8>>,
    ) -> Result<Vec<(Key, Value)>, beetree::EngineError> {
        self.engine.scan(lo, hi)
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
