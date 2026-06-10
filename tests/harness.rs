//! Generic property-test harness for any [`KvEngine`] (`docs/SPEC.md`).
//!
//! M0.1 instantiates it for `NaiveEngine` to validate the harness itself.
//! Every later engine must pass this file UNCHANGED. From a new test file,
//! mount it with `#[macro_use] #[path = "harness.rs"] mod harness;` plus
//! `use harness::*;` and `use proptest::prelude::*;`, then add one
//! `instantiate_harness!` line. Items are `pub(crate)` for exactly that
//! reuse.

use std::collections::BTreeMap;

use beetree::{Key, KvEngine, OpKind, Params, TraceEvent, Value, replay};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// One public operation in a generated workload.
///
/// M0 is insert-only; M1 will add variants. Everything below matches on
/// `Op` exhaustively, so the compiler will point at what to extend.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    Insert { key: Key, value: Value },
}

impl Op {
    /// The key this op touches.
    fn key(&self) -> &Key {
        match self {
            Op::Insert { key, .. } => key,
        }
    }
}

/// Keys come from a small domain — a single byte, as `Vec<u8>` — so workloads
/// collide constantly and the full domain stays cheap to sweep.
pub(crate) fn key_strategy() -> impl Strategy<Value = Key> {
    any::<u8>().prop_map(|b| vec![b])
}

/// Values are short random byte strings; empty values are deliberately legal.
pub(crate) fn value_strategy() -> impl Strategy<Value = Value> {
    proptest::collection::vec(any::<u8>(), 0..=8)
}

pub(crate) fn op_strategy() -> impl Strategy<Value = Op> {
    (key_strategy(), value_strategy()).prop_map(|(key, value)| Op::Insert { key, value })
}

/// Op sequences of length 0..=2000 — with the tiny default capacities
/// (F=4, B=8, L=8) this drives the M0.2 tree several levels deep.
pub(crate) fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op_strategy(), 0..=2000)
}

/// Apply one op to the engine under test.
pub(crate) fn apply<E: KvEngine>(engine: &mut E, op: &Op) {
    match op {
        Op::Insert { key, value } => engine.insert(key.clone(), value.clone()),
    }
}

/// Apply one op to the shadow oracle, mirroring the engine's semantics.
pub(crate) fn apply_oracle(oracle: &mut BTreeMap<Key, Value>, op: &Op) {
    match op {
        Op::Insert { key, value } => {
            oracle.insert(key.clone(), value.clone());
        }
    }
}

/// Every key in the generated domain, for full sweeps.
pub(crate) fn full_domain() -> impl Iterator<Item = Key> {
    (0..=u8::MAX).map(|b| vec![b])
}

/// P1: interleaved oracle — after every op, `get` agrees with a shadow
/// `BTreeMap` for the touched key.
pub(crate) fn check_p1_interleaved_oracle<E: KvEngine>(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    let mut oracle = BTreeMap::new();
    for (i, op) in ops.iter().enumerate() {
        apply(&mut engine, op);
        apply_oracle(&mut oracle, op);
        let key = op.key();
        prop_assert_eq!(
            engine.get(key),
            oracle.get(key).cloned(),
            "P1: get({:?}) diverged from oracle after op {}",
            key,
            i
        );
    }
    Ok(())
}

/// P2: final sweep — after the whole sequence, `get` agrees with the oracle
/// for every key in the domain, including keys never written.
pub(crate) fn check_p2_final_sweep<E: KvEngine>(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    let mut oracle = BTreeMap::new();
    for op in ops {
        apply(&mut engine, op);
        apply_oracle(&mut oracle, op);
    }
    for key in full_domain() {
        prop_assert_eq!(
            engine.get(&key),
            oracle.get(&key).cloned(),
            "P2: get({:?}) diverged from oracle on the final sweep",
            &key
        );
    }
    Ok(())
}

/// P3: invariants — `check_invariants()` is Ok on the empty engine and after
/// every op.
pub(crate) fn check_p3_invariants<E: KvEngine>(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    if let Err(violation) = engine.check_invariants() {
        return Err(TestCaseError::fail(format!(
            "P3: empty engine violates invariants: {violation}"
        )));
    }
    for (i, op) in ops.iter().enumerate() {
        apply(&mut engine, op);
        if let Err(violation) = engine.check_invariants() {
            return Err(TestCaseError::fail(format!(
                "P3: invariant violated after op {i} ({op:?}): {violation}"
            )));
        }
    }
    Ok(())
}

/// P4: replay determinism — replaying the recorded trace into a fresh engine
/// yields identical answers on a full-domain sweep.
pub(crate) fn check_p4_replay_determinism<E: KvEngine>(ops: &[Op]) -> Result<(), TestCaseError> {
    let params = Params::default();
    let mut engine = E::new(params);
    for op in ops {
        apply(&mut engine, op);
    }
    let mut replayed: E = replay(params, engine.trace());
    for key in full_domain() {
        prop_assert_eq!(
            replayed.get(&key),
            engine.get(&key),
            "P4: replayed engine diverged from the original at {:?}",
            &key
        );
    }
    Ok(())
}

/// P5: trace well-formedness — every public mutating op is recorded as an
/// `Op` event with the issued payload, and seqnos are contiguous from 1.
/// `Get` and `FlushDecision` events may be interleaved anywhere; they are
/// ignored (reads and flush decisions consume no seqno).
pub(crate) fn check_p5_trace_well_formedness<E: KvEngine>(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut engine = E::new(Params::default());
    for op in ops {
        apply(&mut engine, op);
    }
    let recorded: Vec<(u64, &OpKind)> = engine
        .trace()
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Op { seq, op } => Some((*seq, op)),
            TraceEvent::Get { .. } | TraceEvent::FlushDecision { .. } => None,
        })
        .collect();
    prop_assert_eq!(
        recorded.len(),
        ops.len(),
        "P5: {} ops were issued but {} Op events were recorded",
        ops.len(),
        recorded.len()
    );
    for (i, ((seq, kind), issued)) in recorded.iter().zip(ops).enumerate() {
        prop_assert_eq!(
            *seq,
            i as u64 + 1,
            "P5: op {} was recorded with seq {}, expected {}",
            i,
            seq,
            i + 1
        );
        match (kind, issued) {
            (
                OpKind::Insert { key, value },
                Op::Insert {
                    key: issued_key,
                    value: issued_value,
                },
            ) => {
                prop_assert_eq!(
                    (key, value),
                    (issued_key, issued_value),
                    "P5: op {} was recorded with a different payload than issued",
                    i
                );
            }
        }
    }
    Ok(())
}

/// Instantiate the full P1–P5 harness for an engine type:
/// `instantiate_harness!(module_name, EngineType);`
macro_rules! instantiate_harness {
    ($module:ident, $engine:ty) => {
        mod $module {
            use super::*;

            proptest! {
                #[test]
                fn p1_interleaved_oracle(ops in ops_strategy()) {
                    check_p1_interleaved_oracle::<$engine>(&ops)?;
                }

                #[test]
                fn p2_final_sweep(ops in ops_strategy()) {
                    check_p2_final_sweep::<$engine>(&ops)?;
                }

                #[test]
                fn p3_invariants(ops in ops_strategy()) {
                    check_p3_invariants::<$engine>(&ops)?;
                }

                #[test]
                fn p4_replay_determinism(ops in ops_strategy()) {
                    check_p4_replay_determinism::<$engine>(&ops)?;
                }

                #[test]
                fn p5_trace_well_formedness(ops in ops_strategy()) {
                    check_p5_trace_well_formedness::<$engine>(&ops)?;
                }
            }
        }
    };
}

instantiate_harness!(naive_engine, beetree::NaiveEngine);

// Shrunk minimal cases from proptest failures get recorded here as permanent
// #[test] regressions before the fix lands (CLAUDE.md). None yet.
