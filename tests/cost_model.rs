//! M4.1 Step 2: calibration of the analytic cost model.
//!
//! `BeTree::simulate_commit` claims to compute the EXACT bytes a
//! `DiskEngine::commit` would write for the same state: Σ over dirty
//! nodes (8-byte record header + real bincode payload) + 4096 for the
//! superblock slot. The whole falsification phase rests on that
//! equivalence — the rollout oracle scores schedules with the analytic
//! model, and its gaps are only meaningful if the model is the same
//! quantity a real engine pays. So: same workload, same policy, same
//! commit cadence on both engines; the analytic total must match
//! `CountingVfs` physical write bytes within 2% (it matches exactly —
//! the assertions below pin both the ≤2% contract and the observed
//! equality so a future divergence is loud).
//!
//! What the calibration can and cannot falsify (M4.1 review): both sides
//! price record payloads with the SAME encoder (`encode_node`), so
//! payload byte-exactness is shared-by-construction, not independently
//! tested here. The falsifiable content is everything else — that
//! `BeTree`'s simulated dirty set, tree evolution, and commit walk
//! mirror `DiskEngine`'s real ones (fully independent implementations
//! on the two engines), measured at the VFS boundary (`CountingVfs`),
//! never via the engine's own `CommitStats::bytes_written` (which is
//! itself analytic and would be circular). The pricing leg has its own
//! independent witnesses: the `entry_bytes` property test
//! (`src/policy.rs`) and the hand-derived absolute window costs pinned
//! in the oracle's dirty-spine test (`src/bin/oracle.rs`).

use beetree::workload::{KeyDist, Mix, OpStream, SplitMix64, WorkOp};
use beetree::{BeTree, CountingVfs, DiskEngine, FileVfs, KvEngine, Params, UpsertOp};

/// The calibration op vocabulary: `WorkOp` plus DELETE, which the bench
/// mixes never issue but the cost model must price (reclamation rewrites
/// parents and unlinks children).
#[derive(Debug, Clone)]
enum CalOp {
    Insert(u64),
    Delete(u64),
    UpsertAdd(u64, i64),
    Read(u64),
}

impl From<&WorkOp> for CalOp {
    fn from(op: &WorkOp) -> CalOp {
        match op {
            WorkOp::Insert(k) => CalOp::Insert(*k),
            WorkOp::Read(k) => CalOp::Read(*k),
            WorkOp::UpsertAdd(k, d) => CalOp::UpsertAdd(*k, *d),
            WorkOp::Scan(..) => unreachable!("no scans in the calibration mixes"),
        }
    }
}

/// Apply one op to any engine (bench.rs conventions: 8-byte BE keys,
/// 8-byte LE op-counter values).
fn apply<E: KvEngine>(engine: &mut E, op: &CalOp, counter: u64) {
    match op {
        CalOp::Insert(k) => engine.insert(k.to_be_bytes().to_vec(), counter.to_le_bytes().to_vec()),
        CalOp::Delete(k) => engine.delete(k.to_be_bytes().to_vec()),
        CalOp::UpsertAdd(k, d) => engine.upsert(k.to_be_bytes().to_vec(), UpsertOp::Add(*d)),
        CalOp::Read(k) => {
            let _ = engine.get(&k.to_be_bytes());
        }
    }
}

/// Run the calibration: identical ops and commit cadence on a `BeTree`
/// (simulated commits) and a `DiskEngine` over `CountingVfs` (physical
/// bytes); returns (analytic, physical).
fn calibrate(ops: &[CalOp], params: Params, commit_every: u64) -> (u64, u64) {
    // Analytic side. The initial simulate_commit mirrors create()'s
    // durably committed generation 0.
    let mut tree = BeTree::new(params);
    let mut analytic = tree.simulate_commit();
    for (i, op) in ops.iter().enumerate() {
        apply(&mut tree, op, i as u64);
        if (i as u64) % commit_every == commit_every - 1 {
            analytic += tree.simulate_commit();
        }
    }
    if ops.len() as u64 % commit_every != 0 {
        analytic += tree.simulate_commit();
    }

    // Physical side: every byte the engine writes, generation 0 included.
    let dir = tempfile::tempdir().unwrap();
    let vfs = CountingVfs::new(FileVfs::create(dir.path().join("calib.db")).unwrap());
    let mut engine = DiskEngine::create_on(vfs, params).unwrap();
    for (i, op) in ops.iter().enumerate() {
        apply(&mut engine, op, i as u64);
        if (i as u64) % commit_every == commit_every - 1 {
            engine.commit().unwrap();
        }
    }
    if ops.len() as u64 % commit_every != 0 {
        engine.commit().unwrap();
    }
    (analytic, engine.io_stats().write_bytes)
}

fn assert_calibrated(name: &str, analytic: u64, physical: u64) {
    let error = (analytic as f64 - physical as f64).abs() / physical as f64;
    println!(
        "calibration [{name}]: analytic {analytic} vs physical {physical} \
         bytes — relative error {:.6}%",
        error * 100.0
    );
    assert!(
        error <= 0.02,
        "[{name}] analytic cost diverged from physical write bytes by \
         {:.4}% (analytic {analytic}, physical {physical}; contract: ≤ 2%)",
        error * 100.0
    );
    // The model is meant to be byte-exact, not merely within tolerance:
    // record lengths come from the real encoder and the superblock is a
    // constant. If this ever fires while the 2% gate holds, something
    // subtle changed in the commit path — find out what before trusting
    // new oracle numbers.
    assert_eq!(
        analytic, physical,
        "[{name}] the model has historically been exact; investigate the drift"
    );
}

/// A full-algebra mix (55% insert / 15% delete / 15% upsert / 15% get)
/// over a small keyspace: flushes, splits, coalescing, AND reclamation
/// all fire, and gets prove reads are free.
fn mixed_ops(n: u64, keyspace: u64, seed: u64) -> Vec<CalOp> {
    let mut rng = SplitMix64(seed);
    (0..n)
        .map(|_| {
            let roll = rng.below(100);
            let key = rng.below(keyspace);
            if roll < 55 {
                CalOp::Insert(key)
            } else if roll < 70 {
                CalOp::Delete(key)
            } else if roll < 85 {
                CalOp::UpsertAdd(key, rng.below(1000) as i64 - 500)
            } else {
                CalOp::Read(key)
            }
        })
        .collect()
}

fn stream_ops(mix: Mix, dist: KeyDist, keyspace: u64, n: u64, seed: u64) -> Vec<CalOp> {
    OpStream::new(mix, dist, keyspace, n, seed)
        .map(|op| CalOp::from(&op))
        .collect()
}

#[test]
fn analytic_cost_matches_physical_writes_uniform_load() {
    let ops = stream_ops(Mix::Load, KeyDist::Uniform, 1_000, 3_000, 0xCA11B);
    let (analytic, physical) = calibrate(&ops, Params::default(), 100);
    assert_calibrated("uniform-load K=100", analytic, physical);
}

#[test]
fn analytic_cost_matches_physical_writes_with_trailing_commit() {
    // K=137 does not divide 3000: the trailing partial-window commit must
    // be mirrored too.
    let ops = stream_ops(Mix::Load, KeyDist::Uniform, 1_000, 3_000, 0xCA11B);
    let (analytic, physical) = calibrate(&ops, Params::default(), 137);
    assert_calibrated("uniform-load K=137 (trailing)", analytic, physical);
}

#[test]
fn analytic_cost_matches_physical_writes_full_algebra() {
    let ops = mixed_ops(4_000, 300, 0xCA11B + 1);
    let (analytic, physical) = calibrate(&ops, Params::default(), 100);
    assert_calibrated("full-algebra K=100", analytic, physical);
}

#[test]
fn analytic_cost_matches_physical_writes_ycsb_a_zipfian() {
    let ops = stream_ops(Mix::YcsbA, KeyDist::Zipfian, 2_000, 4_000, 0xCA11B + 2);
    let (analytic, physical) = calibrate(&ops, Params::default(), 250);
    assert_calibrated("ycsb-a-zipfian K=250", analytic, physical);
}

#[test]
fn simulate_commit_on_a_clean_tree_charges_only_the_superblock() {
    let mut tree = BeTree::new(Params::default());
    tree.simulate_commit();
    assert_eq!(
        tree.simulate_commit(),
        4096,
        "a commit with no dirty nodes writes only the superblock slot"
    );
}

#[test]
fn fork_for_sim_preserves_contents_and_dirty_state() {
    let mut tree = BeTree::new(Params::default());
    let ops = mixed_ops(2_000, 200, 0xF02C);
    for (i, op) in ops.iter().enumerate() {
        apply(&mut tree, op, i as u64);
        if i == 1_000 {
            tree.simulate_commit();
        }
    }
    let mut fork = tree.fork_for_sim(Box::new(beetree::GreedyFullest));
    // Same logical contents...
    let mine = tree
        .scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
        .unwrap();
    let theirs = fork
        .scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
        .unwrap();
    assert_eq!(mine, theirs, "fork must preserve the logical contents");
    // ...same dirty set, byte for byte: the simulated commit costs match.
    assert_eq!(
        tree.simulate_commit(),
        fork.simulate_commit(),
        "fork must carry the dirty flags (the rollout cost depends on them)"
    );
    // ...and the same future: identical op suffixes produce identical
    // costs (next_seq and ops_since_commit carried over).
    for (i, op) in ops.iter().enumerate().take(500) {
        apply(&mut tree, op, 9_000 + i as u64);
        apply(&mut fork, op, 9_000 + i as u64);
    }
    assert_eq!(tree.simulate_commit(), fork.simulate_commit());
}
