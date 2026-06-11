//! The blind-increment showcase (M2.2): 1,000 counters, 100,000 random
//! `Add(1..=10)` upserts with ZERO reads, against the same logical
//! workload done as read-modify-write — both on `DiskEngine`, committing
//! every 1,000 ops. One full scan at the end verifies every counter
//! against an oracle.
//!
//!     cargo run --release --example counter

use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::{Duration, Instant};

use beetree::{DiskEngine, KvEngine, Params, UpsertOp};

const COUNTERS: u64 = 1_000;
const OPS: u64 = 100_000;
const COMMIT_EVERY: u64 = 1_000;
const SEED: u64 = 0xC0_FFEE;

/// Dependency-free deterministic PRNG (xorshift64).
struct XorShift(u64);

impl XorShift {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// The next (counter, delta) pair: counters 0..1000, deltas 1..=10.
    fn op(&mut self) -> (u64, i64) {
        let c = self.next_u64() % COUNTERS;
        let d = (self.next_u64() % 10 + 1) as i64;
        (c, d)
    }
}

fn key(c: u64) -> Vec<u8> {
    (c as u16).to_be_bytes().to_vec()
}

/// The expected final counter values.
fn oracle() -> BTreeMap<Vec<u8>, i64> {
    let mut rng = XorShift(SEED);
    let mut sums: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
    for _ in 0..OPS {
        let (c, d) = rng.op();
        *sums.entry(key(c)).or_insert(0) += d;
    }
    sums
}

struct ArmStats {
    wall: Duration,
    nodes_written: usize,
    bytes_written: u64,
    file_size: u64,
}

/// Run one arm; `apply` performs a single logical increment.
fn run_arm(
    dir: &std::path::Path,
    name: &str,
    mut apply: impl FnMut(&mut DiskEngine<beetree::FileVfs>, Vec<u8>, i64),
) -> ArmStats {
    let mut engine = DiskEngine::create(dir.join(name), Params::default()).expect("create");
    let mut rng = XorShift(SEED);
    let (mut nodes_written, mut bytes_written) = (0usize, 0u64);
    let start = Instant::now();
    for i in 0..OPS {
        let (c, d) = rng.op();
        apply(&mut engine, key(c), d);
        if i % COMMIT_EVERY == COMMIT_EVERY - 1 {
            let stats = engine.commit().expect("commit");
            nodes_written += stats.nodes_written;
            bytes_written += stats.bytes_written;
        }
    }
    let wall = start.elapsed();

    // One full scan verifies every counter against the oracle.
    let scanned = engine
        .scan(Bound::Unbounded, Bound::Unbounded)
        .expect("scan");
    let expected: Vec<(Vec<u8>, Vec<u8>)> = oracle()
        .into_iter()
        .map(|(k, sum)| (k, sum.to_le_bytes().to_vec()))
        .collect();
    assert_eq!(
        scanned, expected,
        "{name}: final state diverged from the oracle"
    );

    ArmStats {
        wall,
        nodes_written,
        bytes_written,
        file_size: engine.file_len().expect("file_len"),
    }
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");

    let upsert = run_arm(dir.path(), "upsert.db", |engine, k, d| {
        engine.upsert(k, UpsertOp::Add(d));
    });
    let rmw = run_arm(dir.path(), "rmw.db", |engine, k, d| {
        let base = match engine.get(&k) {
            Some(v) if v.len() == 8 => i64::from_le_bytes(v.try_into().expect("8 bytes")),
            _ => 0,
        };
        engine.insert(k, (base.wrapping_add(d)).to_le_bytes().to_vec());
    });

    println!(
        "{COUNTERS} counters, {OPS} random Add(1..=10) ops, commit every {COMMIT_EVERY}, DiskEngine:"
    );
    println!(
        "{:<18} {:>9} {:>11} {:>14} {:>14} {:>11}",
        "arm", "wall", "ops/sec", "nodes_written", "bytes_written", "file_size"
    );
    for (name, s) in [("blind upsert", &upsert), ("read-modify-write", &rmw)] {
        println!(
            "{:<18} {:>8.2}s {:>11.0} {:>14} {:>14} {:>11}",
            name,
            s.wall.as_secs_f64(),
            OPS as f64 / s.wall.as_secs_f64(),
            s.nodes_written,
            s.bytes_written,
            s.file_size,
        );
    }
    println!(
        "cache is unbounded in this build; the read-path gap widens under memory pressure (M3)."
    );
}
