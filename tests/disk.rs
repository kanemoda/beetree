// Integration tests for the M1.1 DiskEngine.
//
// The frozen generic harness (tests/harness.rs, byte-identical since the
// Step-0 API freeze) is mounted as a module and instantiated for
// TempDiskEngine — a thin wrapper that gives every proptest case a fresh
// tempdir-backed database file. Everything else here exercises what the
// harness cannot: commit, reopen, recovery, and corruption.

#[macro_use]
#[path = "harness.rs"]
mod harness;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use beetree::{DiskEngine, DiskError, FileVfs, Key, KvEngine, Params, Value, Vfs};
use harness::*;
use proptest::prelude::*;
use tempfile::TempDir;

/// A `DiskEngine` on a fresh tempdir, so the frozen harness — which only
/// knows `KvEngine::new(params)` — can drive a disk-backed engine. The
/// engine field precedes the dir so the file closes before its directory
/// is removed.
struct TempDiskEngine {
    engine: DiskEngine<FileVfs>,
    _dir: TempDir,
}

impl KvEngine for TempDiskEngine {
    fn new(params: Params) -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let engine =
            DiskEngine::create(dir.path().join("bee.db"), params).expect("create database");
        TempDiskEngine { engine, _dir: dir }
    }

    fn insert(&mut self, key: Key, value: Value) {
        self.engine.insert(key, value);
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
}

instantiate_harness!(disk_engine, TempDiskEngine);

/// Dependency-free deterministic PRNG (xorshift64) for random workloads.
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

fn db_path(dir: &TempDir) -> PathBuf {
    dir.path().join("bee.db")
}

/// One random single-byte-key op applied to both engine and oracle.
fn random_op(
    rng: &mut XorShift,
    engine: &mut DiskEngine<FileVfs>,
    oracle: &mut BTreeMap<Key, Value>,
) {
    let key = vec![rng.byte()];
    let len = (rng.next_u64() % 9) as usize;
    let value: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
    engine.insert(key.clone(), value.clone());
    oracle.insert(key, value);
}

/// Full single-byte-domain sweep against an oracle (P2-style).
fn assert_sweep(engine: &mut DiskEngine<FileVfs>, oracle: &BTreeMap<Key, Value>, context: &str) {
    for key in full_domain() {
        assert_eq!(
            engine.get(&key),
            oracle.get(&key).cloned(),
            "{context}: get({key:?}) diverged from oracle"
        );
    }
}

/// Random workload + commit, drop, open: the reopened engine answers a
/// full-domain sweep exactly like the oracle, the persisted params come
/// back (deliberately non-default, so the superblock — not a caller —
/// must supply them), and the reloaded tree passes the full invariant
/// checker.
#[test]
fn round_trip_random_workload() {
    let params = Params {
        fanout: 5,
        buffer_capacity: 3,
        leaf_capacity: 6,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut rng = XorShift::new(0xd15c);
    let mut oracle = BTreeMap::new();

    let mut engine = DiskEngine::create(db_path(&dir), params).unwrap();
    for _ in 0..2000 {
        random_op(&mut rng, &mut engine, &mut oracle);
    }
    engine.commit().unwrap();
    drop(engine);

    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    assert_eq!(
        engine.params(),
        params,
        "params must come from the superblock"
    );
    assert_sweep(&mut engine, &oracle, "round trip");
    engine.load_all().unwrap();
    engine.check_invariants().unwrap();
}

/// 5 cycles of {ops, commit, drop, open} with the oracle carried across
/// sessions: seqnos, params, and contents all survive repeated reopens.
#[test]
fn multi_session_oracle_carried() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = XorShift::new(0x5e5510);
    let mut oracle = BTreeMap::new();

    let mut engine = DiskEngine::create(db_path(&dir), Params::default()).unwrap();
    for cycle in 0..5 {
        for _ in 0..400 {
            random_op(&mut rng, &mut engine, &mut oracle);
        }
        engine.commit().unwrap();
        drop(engine);
        engine = DiskEngine::open(db_path(&dir)).unwrap();
        assert_sweep(&mut engine, &oracle, &format!("cycle {cycle}"));
        engine.load_all().unwrap();
        engine.check_invariants().unwrap();
    }
}

/// Ops after the last commit are absent after reopen: the recovered state
/// equals the commit-time oracle snapshot EXACTLY — post-commit overwrites
/// roll back to their committed values and post-commit fresh keys are gone
/// (SPEC, "Durability contract").
#[test]
fn uncommitted_ops_are_lost_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = XorShift::new(0x10577);
    let mut oracle = BTreeMap::new();

    let mut engine = DiskEngine::create(db_path(&dir), Params::default()).unwrap();
    for _ in 0..800 {
        random_op(&mut rng, &mut engine, &mut oracle);
    }
    // Sentinels are 2 bytes long so the single-byte random domain can
    // never touch them: every assertion on them is deterministic.
    engine.insert(vec![7, 7], b"committed".to_vec());
    engine.commit().unwrap();
    let snapshot = oracle.clone();

    // Uncommitted tail: overwrite a committed key and write a fresh key
    // that the committed tree has never seen.
    engine.insert(vec![7, 7], b"uncommitted".to_vec());
    engine.insert(vec![9, 9], b"uncommitted too".to_vec());
    for _ in 0..300 {
        random_op(&mut rng, &mut engine, &mut BTreeMap::new());
    }
    assert_eq!(engine.get(&[7, 7]), Some(b"uncommitted".to_vec()));
    drop(engine);

    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    assert_sweep(&mut engine, &snapshot, "after dropping uncommitted ops");
    assert_eq!(engine.get(&[7, 7]), Some(b"committed".to_vec()));
    assert_eq!(engine.get(&[9, 9]), None, "the fresh key must be gone");
    // The reopened engine keeps working: new ops, new commit, new session.
    engine.insert(vec![8, 8], b"second life".to_vec());
    engine.commit().unwrap();
    drop(engine);
    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    assert_eq!(engine.get(&[8, 8]), Some(b"second life".to_vec()));
}

/// Round trip under strictly ascending keys — the M0.2 lesson: random-key
/// tests mask ordering-pattern bugs, so persistence gets the sorted-insert
/// treatment too.
#[test]
fn round_trip_ascending_keys() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create(db_path(&dir), Params::default()).unwrap();
    let mut oracle = BTreeMap::new();
    for i in 0..1000u16 {
        let key = i.to_be_bytes().to_vec();
        let value = vec![(i % 251) as u8];
        engine.insert(key.clone(), value.clone());
        oracle.insert(key, value);
    }
    engine.commit().unwrap();
    drop(engine);

    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    for (key, value) in &oracle {
        assert_eq!(
            engine.get(key),
            Some(value.clone()),
            "stale read at {key:?}"
        );
    }
    assert_eq!(engine.get(&1000u16.to_be_bytes()), None);
    assert_eq!(engine.get(&u16::MAX.to_be_bytes()), None);
    engine.load_all().unwrap();
    engine.check_invariants().unwrap();
}

/// Round trip under a 3-distinct-keys overwrite-heavy workload (the other
/// M0.2 ordering lesson), with commits interleaved every 100 ops so the
/// same nodes are rewritten copy-on-write again and again.
#[test]
fn round_trip_three_key_overwrite_heavy() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = XorShift::new(0x3ce5);
    let mut engine = DiskEngine::create(db_path(&dir), Params::default()).unwrap();
    let mut oracle = BTreeMap::new();
    let keys: [Key; 3] = [vec![10], vec![128], vec![200]];
    for op in 0..900u32 {
        let key = keys[(rng.next_u64() % 3) as usize].clone();
        let value = op.to_be_bytes().to_vec();
        engine.insert(key.clone(), value.clone());
        oracle.insert(key, value);
        if op % 100 == 99 {
            engine.commit().unwrap();
        }
    }
    engine.commit().unwrap();
    drop(engine);

    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    assert_sweep(&mut engine, &oracle, "overwrite-heavy round trip");
    engine.load_all().unwrap();
    engine.check_invariants().unwrap();
}

/// Flipping a random byte inside a live node record must surface as a
/// typed `CorruptNode` error — never a panic, never wrong data. `load_all`
/// visits every reachable record, so it must report the corruption; reads
/// that do succeed must still agree with the oracle.
#[test]
fn corrupt_node_record_reads_typed_error() {
    for seed in 0..5u64 {
        let dir = tempfile::tempdir().unwrap();
        let mut rng = XorShift::new(0xbadbee + seed);
        let mut oracle = BTreeMap::new();

        let mut engine = DiskEngine::create(db_path(&dir), Params::default()).unwrap();
        // Everything in [live_start, live_end) is written by the single
        // post-create commit below, so every record there is reachable
        // from the committed root.
        let live_start = engine.file_len().unwrap();
        for _ in 0..600 {
            random_op(&mut rng, &mut engine, &mut oracle);
        }
        engine.commit().unwrap();
        let live_end = engine.file_len().unwrap();
        drop(engine);
        assert!(
            live_end > live_start,
            "the commit must have appended records"
        );

        let path = db_path(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        let target = live_start + rng.next_u64() % (live_end - live_start);
        bytes[target as usize] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();

        let mut engine = DiskEngine::open(&path).unwrap();
        let err = engine
            .load_all()
            .expect_err("a flipped byte in a live record must be detected");
        assert!(
            matches!(err, DiskError::CorruptNode { .. }),
            "seed {seed}: expected CorruptNode, got {err:?}"
        );
        // Reads must never return wrong data: every key either errors
        // with CorruptNode or answers exactly like the oracle.
        for key in full_domain() {
            match engine.try_get(&key) {
                Ok(value) => assert_eq!(
                    value,
                    oracle.get(&key).cloned(),
                    "seed {seed}: corrupt tree returned WRONG data for {key:?}"
                ),
                Err(DiskError::CorruptNode { .. }) => {}
                Err(other) => panic!("seed {seed}: unexpected error {other:?}"),
            }
        }
    }
}

/// Flipping a byte anywhere in the newest superblock slot makes open()
/// fall back to the previous generation: the sweep matches that
/// generation's oracle snapshot, and the engine commits onward from there.
#[test]
fn corrupt_newest_superblock_falls_back() {
    // One flip in the serialized fields, one in the zero padding, one in
    // the stored crc — all 4096 slot bytes are covered by the checksum.
    for &flip_at in &[40u64, 2048, 4094] {
        let dir = tempfile::tempdir().unwrap();
        let mut rng = XorShift::new(0x5b + flip_at);
        let mut oracle = BTreeMap::new();
        let path = db_path(&dir);

        // The sentinel key is 2 bytes long, so the single-byte random
        // workload can never overwrite it: its value tells the surviving
        // generation apart deterministically.
        let sentinel = vec![42, 42];

        // create() commits generation 0 to slot 0.
        let mut engine = DiskEngine::create(&path, Params::default()).unwrap();
        engine.insert(sentinel.clone(), vec![1]);
        for _ in 0..300 {
            random_op(&mut rng, &mut engine, &mut oracle);
        }
        engine.commit().unwrap(); // generation 1 → slot 1
        let snapshot = oracle.clone();

        engine.insert(sentinel.clone(), vec![2]);
        for _ in 0..300 {
            random_op(&mut rng, &mut engine, &mut oracle);
        }
        engine.commit().unwrap(); // generation 2 → slot 0 (newest)
        drop(engine);

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[flip_at as usize] ^= 0xff; // newest superblock lives in slot 0
        std::fs::write(&path, &bytes).unwrap();

        let mut engine = DiskEngine::open(&path).unwrap();
        assert_sweep(
            &mut engine,
            &snapshot,
            &format!("fallback to generation 1 (flip at {flip_at})"),
        );
        assert_eq!(
            engine.get(&sentinel),
            Some(vec![1]),
            "generation 2 must be gone"
        );
        engine.load_all().unwrap();
        engine.check_invariants().unwrap();

        // Committing from the fallback generation works and survives
        // another reopen (the corrupt slot is simply overwritten).
        engine.insert(vec![99, 99], vec![9]);
        engine.commit().unwrap();
        drop(engine);
        let mut engine = DiskEngine::open(&path).unwrap();
        assert_eq!(engine.get(&[99, 99]), Some(vec![9]));
        assert_eq!(engine.get(&sentinel), Some(vec![1]));
    }
}

/// Commit stats: a commit writes the dirty nodes only. A single insert
/// into a fresh tree writes exactly its one leaf; a single insert into a
/// committed deep tree rewrites a small root spine, not the whole tree;
/// an op-free commit writes no nodes at all.
#[test]
fn commit_stats_track_dirty_spine() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create(db_path(&dir), Params::default()).unwrap();

    engine.insert(vec![1], vec![1]);
    let stats = engine.commit().unwrap();
    assert_eq!(
        stats.nodes_written, 1,
        "a fresh tree is a single leaf; one insert dirties exactly it"
    );
    assert!(stats.bytes_written > 0);

    let mut rng = XorShift::new(0x57a75);
    for _ in 0..500 {
        let key = vec![rng.byte()];
        engine.insert(key, vec![0]);
    }
    let full = engine.commit().unwrap();
    assert!(
        full.nodes_written > 10,
        "500 inserts under F=4/B=8/L=8 must dirty a real tree, wrote {}",
        full.nodes_written
    );

    engine.insert(vec![1], vec![2]);
    let before = engine.file_len().unwrap();
    let spine = engine.commit().unwrap();
    let after = engine.file_len().unwrap();
    assert_eq!(
        spine.bytes_written,
        (after - before) + 4096,
        "bytes_written is exactly the data-region growth plus the superblock slot"
    );
    assert!(
        spine.nodes_written > 0,
        "the insert dirtied at least the root"
    );
    assert!(
        spine.nodes_written < full.nodes_written,
        "one insert must rewrite a spine ({}), not the tree ({})",
        spine.nodes_written,
        full.nodes_written
    );
    assert!(
        spine.nodes_written <= 8,
        "one buffered insert touches at most a short root spine, wrote {}",
        spine.nodes_written
    );

    let empty = engine.commit().unwrap();
    assert_eq!(empty.nodes_written, 0, "nothing was dirty");
    assert_eq!(
        empty.bytes_written, 4096,
        "an op-free commit writes exactly one superblock slot"
    );

    // The op-free commit still advanced a generation; everything reopens,
    // and committing a freshly opened, untouched engine (root not even
    // loaded yet) writes no nodes either.
    drop(engine);
    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    let untouched = engine.commit().unwrap();
    assert_eq!(untouched.nodes_written, 0, "nothing loaded, nothing dirty");
    assert_eq!(engine.get(&[1]), Some(vec![2]));
}

/// create() refuses a non-empty file (but accepts an existing EMPTY one);
/// open() refuses a missing file and a file with no valid superblock.
#[test]
fn create_and_open_reject_bad_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);

    // An existing but empty file is fine: only existing DATA is protected.
    let empty = dir.path().join("empty.db");
    std::fs::write(&empty, b"").unwrap();
    drop(DiskEngine::create(&empty, Params::default()).unwrap());

    let engine = DiskEngine::create(&path, Params::default()).unwrap();
    drop(engine);
    let err = DiskEngine::create(&path, Params::default())
        .expect_err("create over an existing database must fail");
    assert!(matches!(err, DiskError::NotEmpty { .. }), "got {err:?}");

    let missing = dir.path().join("nothing.db");
    assert!(matches!(
        DiskEngine::open(&missing).expect_err("open of a missing file must fail"),
        DiskError::Io(_)
    ));

    let garbage = dir.path().join("garbage.db");
    std::fs::write(&garbage, vec![0xa5u8; 32 * 1024]).unwrap();
    assert!(matches!(
        DiskEngine::open(&garbage).expect_err("garbage has no valid superblock"),
        DiskError::NoValidSuperblock
    ));
}

/// Seqnos continue across reopen (the superblock's `last_seq`): the first
/// op of a new session gets exactly `last committed seqno + 1`, so
/// cross-session last-writer-wins ordering and I3 stay sound.
#[test]
fn seqnos_continue_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = DiskEngine::create(db_path(&dir), Params::default()).unwrap();
    for i in 0..300u32 {
        engine.insert(vec![(i % 50) as u8], i.to_be_bytes().to_vec());
    }
    engine.commit().unwrap();
    drop(engine);

    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    engine.insert(vec![7], b"after reopen".to_vec());
    let first_seq = engine
        .trace()
        .iter()
        .find_map(|e| match e {
            beetree::TraceEvent::Op { seq, .. } => Some(*seq),
            _ => None,
        })
        .expect("the insert was traced");
    assert_eq!(
        first_seq, 301,
        "the new session must continue the persisted seqno sequence"
    );
    assert_eq!(engine.get(&[7]), Some(b"after reopen".to_vec()));
    engine.load_all().unwrap();
    engine.check_invariants().unwrap();
}

/// A real database with BOTH superblock slots corrupted must fail open()
/// with the typed NoValidSuperblock — not fall back to garbage.
#[test]
fn both_slots_corrupted_is_no_valid_superblock() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut engine = DiskEngine::create(&path, Params::default()).unwrap();
    for b in 0..=255u8 {
        engine.insert(vec![b], vec![b]);
    }
    engine.commit().unwrap();
    drop(engine);

    let mut bytes = std::fs::read(&path).unwrap();
    bytes[100] ^= 0x01; // slot 0
    bytes[4096 + 100] ^= 0x01; // slot 1
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        DiskEngine::open(&path).expect_err("both slots are corrupt"),
        DiskError::NoValidSuperblock
    ));
}

/// A [`Vfs`] whose next sync can be made to fail on demand — just enough
/// fault injection to pin down M1.1's own error semantics (the systematic
/// fault matrix is M1.2's job).
struct FlakySyncVfs {
    inner: FileVfs,
    fail_next_sync: Rc<Cell<bool>>,
}

impl Vfs for FlakySyncVfs {
    fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.inner.read_exact_at(off, buf)
    }

    fn write_all_at(&mut self, off: u64, data: &[u8]) -> std::io::Result<()> {
        self.inner.write_all_at(off, data)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        if self.fail_next_sync.take() {
            return Err(std::io::Error::other("injected sync failure"));
        }
        self.inner.sync()
    }

    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }

    fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
}

/// A failed commit POISONS the engine: whether the failed generation
/// became durable is unknowable (the lost-ack window), so retrying in
/// place could overwrite records a durable superblock already points at.
/// Further commits must refuse with `Poisoned`; reads stay best-effort;
/// reopening recovers a consistent committed state.
#[test]
fn commit_failure_poisons_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let fail_next_sync = Rc::new(Cell::new(false));
    let vfs = FlakySyncVfs {
        inner: FileVfs::create(db_path(&dir)).unwrap(),
        fail_next_sync: Rc::clone(&fail_next_sync),
    };

    let mut engine = DiskEngine::create_on(vfs, Params::default()).unwrap();
    engine.insert(vec![1], b"v".to_vec());

    fail_next_sync.set(true);
    let err = engine
        .commit()
        .expect_err("the injected sync failure surfaces");
    assert!(matches!(err, DiskError::Io(_)), "got {err:?}");

    // The sync would succeed now, but the engine must refuse anyway.
    let err = engine
        .commit()
        .expect_err("a poisoned engine refuses commits");
    assert!(matches!(err, DiskError::Poisoned), "got {err:?}");

    // In-memory reads still serve.
    assert_eq!(engine.try_get(&[1]).unwrap(), Some(b"v".to_vec()));
    drop(engine);

    // Reopen recovers the last committed state (here: the empty tree of
    // create(); the failed commit's fate is filesystem-dependent and this
    // injected failure happened before its superblock landed).
    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    assert_eq!(engine.get(&[1]), None);
    engine.insert(vec![1], b"second life".to_vec());
    engine.commit().unwrap();
}

/// External truncation (or any corruption that leaves a superblock
/// claiming more bytes than the file holds) must not be "repaired" by
/// zero-extending the file: the outrun slot is invalid and open() falls
/// back to the previous generation.
#[test]
fn truncated_tail_falls_back_to_previous_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut engine = DiskEngine::create(&path, Params::default()).unwrap(); // generation 0
    for b in 0..=255u8 {
        engine.insert(vec![b], vec![b]);
    }
    engine.commit().unwrap(); // generation 1
    drop(engine);

    // Clip one byte off the tail: generation 1's watermark now points past
    // the end of the file (its root record is damaged to boot).
    let len = std::fs::metadata(&path).unwrap().len();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(len - 1).unwrap();
    drop(file);

    let mut engine = DiskEngine::open(&path).unwrap();
    for b in 0..=255u8 {
        assert_eq!(
            engine.get(&[b]),
            None,
            "generation 0 was an empty tree; key {b} must be gone"
        );
    }
    engine.load_all().unwrap();
    engine.check_invariants().unwrap();
    // And the fallback generation is a live base for new work.
    engine.insert(vec![3], b"again".to_vec());
    engine.commit().unwrap();
    drop(engine);
    let mut engine = DiskEngine::open(&path).unwrap();
    assert_eq!(engine.get(&[3]), Some(b"again".to_vec()));
}

/// Persistence under the degenerate-but-legal F=2 corner (the M0.2
/// lesson): ascending inserts at F=2/B=1/L=1 build a linearly tall tree,
/// so commit's dirty walk, open's lazy loads, and load_all must all stay
/// iterative — no operation may recurse proportionally to tree height.
#[test]
fn f2_degenerate_tall_tree_round_trips() {
    let params = Params {
        fanout: 2,
        buffer_capacity: 1,
        leaf_capacity: 1,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut engine = DiskEngine::create(&path, params).unwrap();
    for i in 0..400u32 {
        engine.insert(i.to_be_bytes().to_vec(), vec![1]);
    }
    engine.commit().unwrap();
    drop(engine);

    let mut engine = DiskEngine::open(&path).unwrap();
    assert_eq!(engine.get(&0u32.to_be_bytes()), Some(vec![1]));
    assert_eq!(engine.get(&199u32.to_be_bytes()), Some(vec![1]));
    assert_eq!(engine.get(&399u32.to_be_bytes()), Some(vec![1]));
    engine.load_all().unwrap();
    engine.check_invariants().unwrap();
    // Keep growing the spine after a reopen, across another session.
    for i in 400..500u32 {
        engine.insert(i.to_be_bytes().to_vec(), vec![2]);
    }
    engine.commit().unwrap();
    drop(engine);
    let mut engine = DiskEngine::open(&path).unwrap();
    assert_eq!(engine.get(&450u32.to_be_bytes()), Some(vec![2]));
    engine.load_all().unwrap();
    engine.check_invariants().unwrap();
}

/// Reporting helper, not a correctness gate:
/// `cargo test --release --test disk stats_10k_disk -- --ignored --nocapture`
#[test]
#[ignore = "reporting helper; run explicitly with --ignored --nocapture"]
fn stats_10k_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = XorShift::new(0xbee7ee);
    let mut engine = DiskEngine::create(db_path(&dir), Params::default()).unwrap();

    let mut total_nodes = 0usize;
    let mut total_bytes = 0u64;
    let mut commits = 0u32;
    for op in 0..10_000u32 {
        let key = vec![rng.byte(), rng.byte()];
        let value = vec![rng.byte(), rng.byte(), rng.byte(), rng.byte()];
        engine.insert(key, value);
        if op % 100 == 99 {
            let stats = engine.commit().unwrap();
            total_nodes += stats.nodes_written;
            total_bytes += stats.bytes_written;
            commits += 1;
        }
    }
    let file_size = engine.file_len().unwrap();
    drop(engine);

    let reopen_start = Instant::now();
    let mut engine = DiskEngine::open(db_path(&dir)).unwrap();
    let mut present = 0u32;
    for hi in 0..=255u8 {
        for lo in 0..=255u8 {
            if engine.get(&[hi, lo]).is_some() {
                present += 1;
            }
        }
    }
    let sweep_elapsed = reopen_start.elapsed();
    println!(
        "10k random 2-byte-key inserts, commit every 100 ops: \
         file_size={file_size} bytes, commits={commits}, \
         total_nodes_written={total_nodes}, mean_nodes_per_commit={:.1}, \
         total_bytes_written={total_bytes}, reopen+full-sweep(65536 keys)={:?}, \
         distinct_keys_present={present}",
        total_nodes as f64 / commits as f64,
        sweep_elapsed,
    );
}
