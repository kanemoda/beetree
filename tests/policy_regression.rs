//! The M4.1 policy-refactor regression gate.
//!
//! Step 1 of M4.1 extracts the normative greedy-fullest flush rule into a
//! `FlushPolicy` trait. The refactor must be BEHAVIOR-PRESERVING down to
//! the byte: under the default policy, a fixed-seed mixed workload must
//! produce byte-identical traces on both engines and a byte-identical
//! database file. The constants below were captured on the PRE-refactor
//! tree (commit 4094735, before `src/policy.rs` existed) by running this
//! very test; they are the proof, not an assertion of intent.

use beetree::workload::SplitMix64;
use beetree::{BeTree, DiskEngine, KvEngine, Params, UpsertOp};

/// FNV-1a 64-bit, hand-rolled (no new dependencies; collisions are not
/// adversarial here — this is a fixed-input regression check).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const SEED: u64 = 0xBEE7_4101;
const N_OPS: u64 = 6_000;
const KEYSPACE: u64 = 300;
const COMMIT_EVERY: u64 = 100;

/// The frozen fixture workload: a full-algebra mix (55% insert, 15%
/// delete, 15% upsert, 15% get) over a small keyspace, so flushes,
/// splits, coalescing, AND reclamation all fire. Changing this function
/// invalidates the captured constants — don't.
fn drive<E: KvEngine>(engine: &mut E, mut at_op: impl FnMut(&mut E, u64)) {
    let mut rng = SplitMix64(SEED);
    for i in 0..N_OPS {
        let roll = rng.below(100);
        let key = rng.below(KEYSPACE).to_be_bytes().to_vec();
        if roll < 55 {
            engine.insert(key, i.to_le_bytes().to_vec());
        } else if roll < 70 {
            engine.delete(key);
        } else if roll < 85 {
            engine.upsert(key, UpsertOp::Add(rng.below(1000) as i64 - 500));
        } else {
            let _ = engine.get(&key);
        }
        at_op(engine, i);
    }
}

/// trace2 as JSONL bytes (one serde_json line per event, the bench
/// `dump_trace` framing), hashed.
fn trace2_hash<E: KvEngine>(engine: &E) -> (u64, usize) {
    let mut out = String::new();
    for event in engine.trace2() {
        out.push_str(&serde_json::to_string(event).expect("serialize trace event"));
        out.push('\n');
    }
    (fnv1a64(out.as_bytes()), engine.trace2().len())
}

// Captured pre-refactor (see module docs). The triple is (trace2 fnv64,
// trace2 event count) per engine plus (file fnv64, file length) for the
// disk engine.
const BETREE_TRACE2_FNV64: u64 = 0x30d9_7704_07f8_67b9;
const BETREE_TRACE2_EVENTS: usize = 8_834;
const DISK_TRACE2_FNV64: u64 = 0x30d9_7704_07f8_67b9;
const DISK_TRACE2_EVENTS: usize = 8_834;
const DISK_FILE_FNV64: u64 = 0xae58_4d31_41f9_74a5;
const DISK_FILE_LEN: u64 = 528_304;

#[test]
fn betree_trace_is_byte_identical_to_pre_refactor() {
    let mut tree = BeTree::new(Params::default());
    drive(&mut tree, |_, _| {});
    let (hash, events) = trace2_hash(&tree);
    assert_eq!(
        (hash, events),
        (BETREE_TRACE2_FNV64, BETREE_TRACE2_EVENTS),
        "BeTree trace2 diverged from the pre-refactor baseline \
         (got hash {hash:#018x}, {events} events)"
    );
}

#[test]
fn disk_trace_and_file_are_byte_identical_to_pre_refactor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gate.db");
    let mut engine = DiskEngine::create(&path, Params::default()).unwrap();
    drive(&mut engine, |engine, i| {
        if i % COMMIT_EVERY == COMMIT_EVERY - 1 {
            engine.commit().expect("commit");
        }
    });
    if N_OPS % COMMIT_EVERY != 0 {
        engine.commit().expect("commit");
    }
    let (hash, events) = trace2_hash(&engine);
    drop(engine);
    let file = std::fs::read(&path).unwrap();
    let file_hash = fnv1a64(&file);
    assert_eq!(
        (hash, events),
        (DISK_TRACE2_FNV64, DISK_TRACE2_EVENTS),
        "DiskEngine trace2 diverged from the pre-refactor baseline \
         (got hash {hash:#018x}, {events} events)"
    );
    assert_eq!(
        (file_hash, file.len() as u64),
        (DISK_FILE_FNV64, DISK_FILE_LEN),
        "DiskEngine file bytes diverged from the pre-refactor baseline \
         (got hash {file_hash:#018x}, {} bytes)",
        file.len()
    );
}
