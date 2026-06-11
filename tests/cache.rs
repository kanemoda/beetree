// M3.1 white-box cache and accounting tests: eviction under pinning
// (scans, flush cascades), the reload-CRC path, overcommit, stats
// coherence, CountingVfs transparency, io_stats determinism, drain on
// disk, and the read-amplification experiment.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::Instant;

use beetree::{
    CountingVfs, DiskEngine, DiskError, FileVfs, Key, KvEngine, Params, UpsertOp, Value,
};
use tempfile::TempDir;

/// Dependency-free deterministic PRNG (xorshift64).
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 32) as u8
    }
}

fn db_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("bee.db")
}

/// 0..=255 single-byte keys with 16-byte values; oracle alongside.
fn build_domain(engine: &mut DiskEngine<impl beetree::Vfs>) -> BTreeMap<Key, Value> {
    let mut oracle = BTreeMap::new();
    for b in 0..=255u8 {
        let value = vec![b; 16];
        engine.insert(vec![b], value.clone());
        oracle.insert(vec![b], value);
    }
    oracle
}

/// CountingVfs is transparent: the same workload over FileVfs and over
/// CountingVfs<FileVfs> produces byte-identical files — and the counters
/// actually counted.
#[test]
fn counting_vfs_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let plain_path = dir.path().join("plain.db");
    let counted_path = dir.path().join("counted.db");

    let mut plain = DiskEngine::create(&plain_path, Params::default()).unwrap();
    let mut counted = DiskEngine::create_on(
        CountingVfs::new(FileVfs::create(&counted_path).unwrap()),
        Params::default(),
    )
    .unwrap();
    for engine_first in [true, false] {
        // Identical op sequences, interleaved commits.
        let mut rng = XorShift::new(0xc0117);
        for i in 0..600u32 {
            let key = vec![rng.byte()];
            let value = vec![rng.byte(); (i % 9) as usize];
            if engine_first {
                plain.insert(key.clone(), value.clone());
                counted.insert(key, value);
            } else {
                plain.delete(key.clone());
                counted.delete(key);
            }
            if i % 100 == 99 {
                plain.commit().unwrap();
                counted.commit().unwrap();
            }
        }
    }
    plain.commit().unwrap();
    counted.commit().unwrap();
    let stats = counted.io_stats();
    drop(plain);
    drop(counted);

    assert_eq!(
        std::fs::read(&plain_path).unwrap(),
        std::fs::read(&counted_path).unwrap(),
        "CountingVfs must be byte-for-byte transparent"
    );
    assert!(stats.write_ops > 0 && stats.write_bytes > 0 && stats.syncs > 0);
}

/// Same workload, same budget ⇒ identical io_stats and cache stats on
/// two independent runs (no hidden nondeterminism in eviction).
#[test]
fn bounded_runs_are_deterministic() {
    let run = || {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = DiskEngine::create_on_bounded(
            CountingVfs::new(FileVfs::create(db_path(&dir)).unwrap()),
            Params::default(),
            2048,
        )
        .unwrap();
        let oracle = build_domain(&mut engine);
        engine.commit().unwrap();
        for key in oracle.keys() {
            assert_eq!(engine.get(key), Some(oracle[key].clone()));
        }
        engine.commit().unwrap();
        (engine.io_stats(), engine.cache_stats())
    };
    assert_eq!(run(), run(), "two identical runs must account identically");
}

/// Eviction during a long scan: the scan's frame stack pins its path, so
/// the result stays oracle-exact while clean nodes around it get evicted.
#[test]
fn scan_survives_eviction_pressure() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create_bounded(db_path(&dir), Params::default(), 2048).unwrap();
    let oracle = build_domain(&mut engine);
    engine.commit().unwrap(); // everything clean ⇒ evictable

    let before = engine.cache_stats();
    let scanned = engine.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
    let after = engine.cache_stats();

    let expected: Vec<(Key, Value)> = oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(scanned, expected, "scan under eviction diverged");
    assert!(
        after.evictions > before.evictions,
        "a 2048-byte budget must evict during a full-domain scan \
         (evictions {} -> {})",
        before.evictions,
        after.evictions
    );
}

/// Eviction raced against flush cascades: a second write wave over a
/// committed (clean, evictable) tree loads and evicts mid-cascade with
/// the spine pinned; contents and invariants stay exact.
#[test]
fn flush_cascades_survive_eviction_pressure() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create_bounded(db_path(&dir), Params::default(), 2048).unwrap();
    let mut oracle = build_domain(&mut engine);
    engine.commit().unwrap();

    let before = engine.cache_stats();
    let mut rng = XorShift::new(0xcafe5);
    for _ in 0..600 {
        let key = vec![rng.byte()];
        match rng.next_u64() % 3 {
            0 => {
                let value = vec![rng.byte(); 12];
                engine.insert(key.clone(), value.clone());
                oracle.insert(key, value);
            }
            1 => {
                engine.delete(key.clone());
                oracle.remove(&key);
            }
            _ => {
                let delta = rng.next_u64() as i64;
                engine.upsert(key.clone(), UpsertOp::Add(delta));
                let base = match oracle.get(&key) {
                    Some(v) if v.len() == 8 => i64::from_le_bytes(v.as_slice().try_into().unwrap()),
                    _ => 0,
                };
                oracle.insert(key, base.wrapping_add(delta).to_le_bytes().to_vec());
            }
        }
    }
    let after = engine.cache_stats();
    assert!(
        after.evictions > before.evictions,
        "cascades over a committed tree must evict under a 2048-byte budget"
    );
    engine.check_invariants_full().unwrap();
    for b in 0..=255u8 {
        assert_eq!(
            engine.get(&[b]),
            oracle.get(&vec![b]).cloned(),
            "key {b} diverged under eviction + cascades"
        );
    }
}

/// The reload path is the verified-load path: after evictions, reloads
/// re-check the CRC — corrupt bytes surface as typed CorruptNode, never
/// as data. Positive half: a tiny budget forces evict→reload cycles and
/// every value stays exact.
#[test]
fn reload_after_evict_verifies_crc() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut engine = DiskEngine::create_bounded(&path, Params::default(), 1024).unwrap();
    let oracle = build_domain(&mut engine);
    let live_start = engine.file_len().unwrap();
    engine.commit().unwrap();
    let live_end = engine.file_len().unwrap();

    // Two full sweeps: the second re-loads what the first evicted.
    for _ in 0..2 {
        for (key, value) in &oracle {
            assert_eq!(engine.get(key), Some(value.clone()));
        }
    }
    let stats = engine.cache_stats();
    assert!(stats.evictions > 0, "1 KiB must evict during the sweeps");
    drop(engine);

    // Negative half: flip one byte inside the live data region; a
    // bounded reopen reads every record through the same verified path.
    let mut rng = XorShift::new(0xc4c);
    let mut bytes = std::fs::read(&path).unwrap();
    let target = live_start + rng.next_u64() % (live_end - live_start);
    bytes[target as usize] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();

    let mut engine = DiskEngine::open_bounded(&path, 1024).unwrap();
    let err = engine
        .load_all()
        .expect_err("the flipped byte must be detected on (re)load");
    assert!(matches!(err, DiskError::CorruptNode { .. }), "got {err:?}");
}

/// A budget smaller than a single root-to-leaf path: the engine goes
/// over budget (counting overcommit events) instead of failing, and
/// still answers exactly.
#[test]
fn overcommit_counts_and_answers_stay_exact() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create_bounded(db_path(&dir), Params::default(), 1).unwrap();
    let oracle = build_domain(&mut engine);
    engine.commit().unwrap();
    for (key, value) in &oracle {
        assert_eq!(engine.get(key), Some(value.clone()));
    }
    let stats = engine.cache_stats();
    assert!(
        stats.overcommit_events > 0,
        "a 1-byte budget must overcommit on every pinned path"
    );
    engine.check_invariants_full().unwrap();
}

/// Stats coherence: every miss is exactly one record read (= two vfs
/// read ops: header + payload) on an engine that was created, never
/// opened; hits and misses both occur under a tight budget.
#[test]
fn cache_stats_match_vfs_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create_on_bounded(
        CountingVfs::new(FileVfs::create(db_path(&dir)).unwrap()),
        Params::default(),
        2048,
    )
    .unwrap();
    let oracle = build_domain(&mut engine);
    engine.commit().unwrap();
    for key in oracle.keys() {
        engine.get(key);
    }
    let cache = engine.cache_stats();
    let io = engine.io_stats();
    assert!(cache.hits > 0 && cache.misses > 0 && cache.evictions > 0);
    assert_eq!(
        io.read_ops,
        2 * cache.misses,
        "every miss is exactly one verified record read (header + payload)"
    );
}

/// drain() on a bounded DiskEngine: contents unchanged, nothing traced,
/// invariants green — and the drained state commits and reopens.
#[test]
fn disk_drain_is_exact_and_untraced() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut engine = DiskEngine::create_bounded(&path, Params::default(), 2048).unwrap();
    let mut oracle = build_domain(&mut engine);
    for b in (0..=255u8).step_by(3) {
        engine.delete(vec![b]);
        oracle.remove(&vec![b]);
    }
    let trace2_len = engine.trace2().len();
    let trace_len = engine.trace().len();

    engine.drain().unwrap();

    assert_eq!(engine.trace2().len(), trace2_len, "drain must not trace");
    assert_eq!(engine.trace().len(), trace_len, "drain must not trace (v1)");
    engine.check_invariants_full().unwrap();
    let scanned = engine.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
    let expected: Vec<(Key, Value)> = oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(scanned, expected, "drain changed the contents");

    engine.commit().unwrap();
    drop(engine);
    let mut engine = DiskEngine::open(&path).unwrap();
    for (key, value) in &oracle {
        assert_eq!(engine.get(key), Some(value.clone()));
    }
}

/// drain() must work from ANY legal engine state: straight after open()
/// (root not yet loaded) and after commit-boundary eviction emptied the
/// cache (root evicted under a 1-byte budget). Both once panicked.
#[test]
fn drain_works_with_a_non_resident_root() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut engine = DiskEngine::create(&path, Params::default()).unwrap();
    let oracle = build_domain(&mut engine);
    engine.commit().unwrap();
    drop(engine);

    // Case 1: open() leaves the root OnDisk; drain is the first call.
    let mut engine = DiskEngine::open(&path).unwrap();
    engine.drain().unwrap();
    for (key, value) in &oracle {
        assert_eq!(engine.get(key), Some(value.clone()));
    }
    drop(engine);

    // Case 2: a 1-byte budget evicts everything (root included) at the
    // commit boundary; drain must reload, not panic.
    let mut engine = DiskEngine::open_bounded(&path, 1).unwrap();
    engine.insert(vec![1], b"x".to_vec());
    engine.commit().unwrap();
    engine.drain().unwrap();
    assert_eq!(engine.get(&[1]), Some(b"x".to_vec()));
}

/// check_invariants_full suspends the budget to fault the tree in; it
/// must then actually evict back down — the post-check resident bytes
/// stay within the budget (nothing is pinned, everything is clean).
#[test]
fn check_invariants_full_reenforces_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create_bounded(db_path(&dir), Params::default(), 2048).unwrap();
    build_domain(&mut engine);
    engine.commit().unwrap();
    engine.check_invariants_full().unwrap();
    let stats = engine.cache_stats();
    assert!(
        stats.resident_bytes <= 2048,
        "after check_invariants_full the cache must be back under budget          (resident {} > 2048)",
        stats.resident_bytes
    );
}

/// Reclamation must RELEASE unlinked slots from cache accounting: after
/// deleting everything (and driving resting tombstones down), a commit
/// settles the cache back under budget — emptied leaves must not linger
/// as unevictable dirty garbage.
#[test]
fn reclamation_releases_cache_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create_bounded(db_path(&dir), Params::default(), 2048).unwrap();
    build_domain(&mut engine);
    engine.commit().unwrap();
    for _pass in 0..4 {
        for b in 0..=255u8 {
            engine.delete(vec![b]);
        }
        engine.commit().unwrap();
    }
    for b in 0..=255u8 {
        assert_eq!(engine.get(&[b]), None);
    }
    let stats = engine.cache_stats();
    assert!(
        stats.resident_bytes <= 2048,
        "reclaimed slots must leave cache accounting (resident {} > 2048)",
        stats.resident_bytes
    );
    engine.check_invariants_full().unwrap();
}

/// Reporting helper, not a correctness gate — the project's first
/// read-amplification datapoint:
/// `cargo test --release --test cache read_amplification -- --ignored --nocapture`
#[test]
#[ignore = "reporting helper; run explicitly with --ignored --nocapture"]
fn read_amplification_sweep() {
    const OPS: u64 = 100_000;
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);

    // Build: 100k random 8-byte keys with 8-byte values, commit every
    // 1,000 ops, then drain + commit so the on-disk tree is message-free.
    let keys = |seed: u64| {
        let mut rng = XorShift::new(seed);
        (0..OPS).map(move |i| (rng.next_u64().to_be_bytes().to_vec(), i))
    };
    let mut engine = DiskEngine::create(&path, Params::default()).unwrap();
    for (i, (key, n)) in keys(0xfeed).enumerate() {
        engine.insert(key, n.to_le_bytes().to_vec());
        if i % 1000 == 999 {
            engine.commit().unwrap();
        }
    }
    engine.drain().unwrap();
    engine.commit().unwrap();
    let file_size = engine.file_len().unwrap();
    drop(engine);

    println!("file size after build+drain: {file_size} bytes");
    println!(
        "{:<12} {:>10} {:>14} {:>9} {:>10} {:>11} {:>9}",
        "budget", "read_ops", "read_bytes", "hit_rate", "evictions", "overcommit", "wall"
    );
    for (label, budget) in [
        ("unbounded", None),
        ("25% of file", Some(file_size / 4)),
        ("5% of file", Some(file_size / 20)),
    ] {
        let vfs = CountingVfs::new(FileVfs::open(&path).unwrap());
        let mut engine = match budget {
            Some(b) => DiskEngine::open_on_bounded(vfs, b).unwrap(),
            None => DiskEngine::open_on(vfs).unwrap(),
        };
        let start = Instant::now();
        for (key, n) in keys(0xfeed) {
            assert_eq!(
                engine.get(&key),
                Some(n.to_le_bytes().to_vec()),
                "sweep diverged at key {key:?}"
            );
        }
        let wall = start.elapsed();
        let io = engine.io_stats();
        let cache = engine.cache_stats();
        println!(
            "{:<12} {:>10} {:>14} {:>8.1}% {:>10} {:>11} {:>8.2}s",
            label,
            io.read_ops,
            io.read_bytes,
            100.0 * cache.hits as f64 / (cache.hits + cache.misses) as f64,
            cache.evictions,
            cache.overcommit_events,
            wall.as_secs_f64(),
        );
    }
}
