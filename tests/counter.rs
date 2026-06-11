// The blind-increment showcase as a correctness gate (M2.2): both arms —
// 100,000 blind upserts and the same logical workload as read-modify-
// write — must land every one of 1,000 counters EXACTLY on the oracle's
// sum, verified by one full scan. `examples/counter.rs` is the printing
// twin of this test.

use std::collections::BTreeMap;
use std::ops::Bound;

use beetree::{DiskEngine, FileVfs, KvEngine, Params, UpsertOp};

const COUNTERS: u64 = 1_000;
const OPS: u64 = 100_000;
const COMMIT_EVERY: u64 = 1_000;
const SEED: u64 = 0xC0_FFEE;

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

    fn op(&mut self) -> (u64, i64) {
        let c = self.next_u64() % COUNTERS;
        let d = (self.next_u64() % 10 + 1) as i64;
        (c, d)
    }
}

fn key(c: u64) -> Vec<u8> {
    (c as u16).to_be_bytes().to_vec()
}

fn oracle() -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut rng = XorShift(SEED);
    let mut sums: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
    for _ in 0..OPS {
        let (c, d) = rng.op();
        *sums.entry(key(c)).or_insert(0) += d;
    }
    sums.into_iter()
        .map(|(k, sum)| (k, sum.to_le_bytes().to_vec()))
        .collect()
}

fn run_arm(
    dir: &tempfile::TempDir,
    name: &str,
    mut apply: impl FnMut(&mut DiskEngine<FileVfs>, Vec<u8>, i64),
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut engine = DiskEngine::create(dir.path().join(name), Params::default()).unwrap();
    let mut rng = XorShift(SEED);
    for i in 0..OPS {
        let (c, d) = rng.op();
        apply(&mut engine, key(c), d);
        if i % COMMIT_EVERY == COMMIT_EVERY - 1 {
            engine.commit().unwrap();
        }
    }
    engine.scan(Bound::Unbounded, Bound::Unbounded).unwrap()
}

/// 100k blind increments with zero reads land exactly on the oracle —
/// and so does the read-modify-write formulation of the same workload.
#[test]
fn blind_increment_showcase_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    let expected = oracle();

    let upsert = run_arm(&dir, "upsert.db", |engine, k, d| {
        engine.upsert(k, UpsertOp::Add(d));
    });
    assert_eq!(
        upsert, expected,
        "blind-upsert arm diverged from the oracle"
    );

    let rmw = run_arm(&dir, "rmw.db", |engine, k, d| {
        let base = match engine.get(&k) {
            Some(v) if v.len() == 8 => i64::from_le_bytes(v.try_into().expect("8 bytes")),
            _ => 0,
        };
        engine.insert(k, (base.wrapping_add(d)).to_le_bytes().to_vec());
    });
    assert_eq!(
        rmw, expected,
        "read-modify-write arm diverged from the oracle"
    );
}
