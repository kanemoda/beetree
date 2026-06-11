// M1.2 crash-injection harness: proves the durability contract (SPEC,
// "Crash model and guarantees") under arbitrary crash points.
//
// Per proptest case: run a workload with sprinkled commits on a
// DiskEngine<FaultyVfs>, snapshot the oracle at every commit, then crash
// the device at sampled log positions — always including the three
// canonical danger points of the final commit — and at every crash image
// assert A1–A5: recovery succeeds, yields exactly some committed
// generation, loses no fully-synced commit, passes the invariant checker,
// and remains a live engine. The harness itself is mutation-tested
// (docs/findings.md, "harness mutations").
//
// Everything random flows from proptest-generated values; PROPTEST_CASES
// defaults to 32 for this file (the soak overrides it). Shrunk failures
// must be hand-recorded as permanent regressions (CLAUDE.md); failure
// persistence is off because these tests use a hand-rolled TestRunner to
// report evaluated-image counts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use beetree::{DiskEngine, DiskError, Fate, FaultyVfs, Key, KvEngine, Params, Value, VfsOp};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

/// One step of a generated workload.
#[derive(Debug, Clone)]
enum PlanOp {
    Ins(Key, Value),
    Commit,
}

/// Everything one crash case needs; all randomness lives here.
#[derive(Debug)]
struct CrashCase {
    body: Vec<PlanOp>,
    /// Values for the three forced tail inserts (the tail guarantees a
    /// non-empty final commit and at least 3 generations, so superblock
    /// slots carry real history).
    tail_vals: Vec<Value>,
    /// Seeds for the C=6 uniform crash positions.
    crash_seeds: Vec<u64>,
    /// The R=6 fate vectors applied at every sampled crash position.
    crash_fates: Vec<Vec<Fate>>,
    /// Fates for the idempotent-recovery second crash (truncation window).
    idem_fates: Vec<Fate>,
    /// 20 post-recovery ops proving liveness (A5).
    live_ops: Vec<(Key, Value)>,
}

fn cases_budget() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32)
}

fn value_strategy() -> impl Strategy<Value = Value> {
    proptest::collection::vec(any::<u8>(), 0..=4)
}

fn fate_strategy() -> impl Strategy<Value = Fate> {
    prop_oneof![
        Just(Fate::Drop),
        Just(Fate::Apply),
        Just(Fate::Zero),
        any::<u64>().prop_map(Fate::Tear),
    ]
}

/// Random single-byte keys over a small domain, commits ~1 in 15 ops.
fn random_body() -> impl Strategy<Value = Vec<PlanOp>> {
    proptest::collection::vec(
        prop_oneof![
            14 => (0u8..16, value_strategy()).prop_map(|(k, v)| PlanOp::Ins(vec![k], v)),
            1 => Just(PlanOp::Commit),
        ],
        50..=400,
    )
}

/// Strictly ascending 2-byte keys (the M0.2 ordering lesson).
fn ascending_body() -> impl Strategy<Value = Vec<PlanOp>> {
    proptest::collection::vec((value_strategy(), 0u8..15), 50..=400).prop_map(|items| {
        let mut ops = Vec::new();
        for (i, (v, c)) in items.into_iter().enumerate() {
            ops.push(PlanOp::Ins((i as u16).to_be_bytes().to_vec(), v));
            if c == 0 {
                ops.push(PlanOp::Commit);
            }
        }
        ops
    })
}

/// Three distinct keys, overwrite-heavy (the other ordering lesson).
fn overwrite_body() -> impl Strategy<Value = Vec<PlanOp>> {
    proptest::collection::vec(
        prop_oneof![
            14 => (0usize..3, value_strategy())
                .prop_map(|(k, v)| PlanOp::Ins(vec![[10u8, 128, 200][k]], v)),
            1 => Just(PlanOp::Commit),
        ],
        50..=400,
    )
}

fn live_ops_strategy() -> impl Strategy<Value = Vec<(Key, Value)>> {
    proptest::collection::vec(
        (0u8..16, value_strategy()).prop_map(|(k, v)| (vec![k], v)),
        20,
    )
}

fn crash_case(body: impl Strategy<Value = Vec<PlanOp>>) -> impl Strategy<Value = CrashCase> {
    (
        body,
        proptest::collection::vec(value_strategy(), 3),
        proptest::collection::vec(any::<u64>(), 6),
        proptest::collection::vec(proptest::collection::vec(fate_strategy(), 0..=8), 6),
        proptest::collection::vec(fate_strategy(), 0..=4),
        live_ops_strategy(),
    )
        .prop_map(
            |(body, tail_vals, crash_seeds, crash_fates, idem_fates, live_ops)| CrashCase {
                body,
                tail_vals,
                crash_seeds,
                crash_fates,
                idem_fates,
                live_ops,
            },
        )
}

/// Shared context for evaluating one crash image.
struct CaseCtx<'a> {
    h: &'a FaultyVfs,
    /// Log length when create() returned: crashes before this may fail
    /// open() with a typed error (A1's only exemption).
    create_done: usize,
    /// Oracle snapshot per generation (index = generation).
    snapshots: &'a [BTreeMap<Key, Value>],
    /// Log length at which each generation finished its final sync.
    commit_done_len: &'a [usize],
    /// Sweep domain: every key the workload may have touched + probes.
    keys: &'a BTreeSet<Key>,
    live_ops: &'a [(Key, Value)],
    images: &'a AtomicU64,
}

fn sweep(
    eng: &mut DiskEngine<FaultyVfs>,
    oracle: &BTreeMap<Key, Value>,
    keys: &BTreeSet<Key>,
    ctx: &str,
) -> Result<(), TestCaseError> {
    for k in keys {
        match eng.try_get(k) {
            Ok(v) => prop_assert_eq!(
                &v,
                &oracle.get(k).cloned(),
                "{}: get({:?}) diverged from the oracle",
                ctx,
                k
            ),
            Err(e) => prop_assert!(false, "{}: get({:?}) failed: {}", ctx, k, e),
        }
    }
    Ok(())
}

/// Build the crash image for (pos, fates), reopen, and assert A1–A5.
fn check_image(ctx: &CaseCtx, pos: usize, fates: &[Fate]) -> Result<(), TestCaseError> {
    let img = ctx.h.crash_image_at(pos, fates);
    ctx.images.fetch_add(1, Ordering::Relaxed);
    let vfs = FaultyVfs::from_image(img);
    let h2 = vfs.clone();

    // A1: open succeeds; typed failure is allowed only for crashes that
    // precede the initial create() return.
    let mut eng = match DiskEngine::open_on(vfs) {
        Ok(eng) => eng,
        Err(e) => {
            prop_assert!(
                pos < ctx.create_done,
                "A1: open failed ({e}) for a crash at log pos {pos}, \
                 at/after the create() boundary {}",
                ctx.create_done
            );
            return Ok(());
        }
    };

    // A2: the recovered generation is a committed one and the sweep
    // matches its oracle snapshot exactly.
    let g = eng.generation() as usize;
    prop_assert!(
        g < ctx.snapshots.len(),
        "A2: recovered unknown generation {g} (crash pos {pos})"
    );
    sweep(
        &mut eng,
        &ctx.snapshots[g],
        ctx.keys,
        &format!("A2 (gen {g}, pos {pos})"),
    )?;

    // A3: no fully-synced commit may be lost.
    let fully_synced = ctx
        .commit_done_len
        .iter()
        .rposition(|&done_at| done_at <= pos);
    if let Some(lb) = fully_synced {
        prop_assert!(
            g >= lb,
            "A3: lost a durable commit: recovered generation {g} but \
             generation {lb} was fully synced before the crash at pos {pos}"
        );
    }

    // A4: the recovered tree passes the full invariant checker.
    eng.load_all()
        .map_err(|e| TestCaseError::fail(format!("A4: load_all failed at pos {pos}: {e}")))?;
    eng.check_invariants()
        .map_err(|e| TestCaseError::fail(format!("A4: invariants violated at pos {pos}: {e}")))?;

    // A5: liveness — the recovered engine accepts work, commits, and
    // survives another reopen.
    let mut oracle = ctx.snapshots[g].clone();
    for (k, v) in ctx.live_ops {
        eng.insert(k.clone(), v.clone());
        oracle.insert(k.clone(), v.clone());
    }
    eng.commit()
        .map_err(|e| TestCaseError::fail(format!("A5: post-recovery commit failed: {e}")))?;
    drop(eng);
    let mut eng = DiskEngine::open_on(h2)
        .map_err(|e| TestCaseError::fail(format!("A5: post-recovery reopen failed: {e}")))?;
    sweep(&mut eng, &oracle, ctx.keys, &format!("A5 (pos {pos})"))?;
    Ok(())
}

/// Deterministic fate combinations for a canonical danger point: the full
/// basis cartesian over the first two window ops (everything else fully
/// applied). Windows here are 1–2 ops, so this is exhaustive in practice.
fn cartesian_fates(window: usize) -> Vec<Vec<Fate>> {
    const BASIS: [Fate; 4] = [Fate::Drop, Fate::Apply, Fate::Zero, Fate::Tear(7919)];
    match window {
        0 => vec![Vec::new()],
        1 => BASIS.iter().map(|f| vec![*f]).collect(),
        _ => {
            let mut out = Vec::new();
            for a in BASIS {
                for b in BASIS {
                    let mut v = vec![Fate::Apply; window];
                    v[0] = a;
                    v[1] = b;
                    out.push(v);
                }
            }
            out
        }
    }
}

/// Fates that apply everything except the LAST window op, which gets `f`.
fn last_op_fates(window: usize, f: Fate) -> Vec<Fate> {
    let mut v = vec![Fate::Apply; window.max(1)];
    *v.last_mut().expect("non-empty") = f;
    v
}

/// The deterministic superblock tear grid: every serialized-field
/// boundary of the 64-byte superblock payload (step 4 covers each u32/u64
/// edge), plus mid-padding and just-shy-of-the-crc tears. Field-boundary
/// tears are what catch a recovery that trusts unchecksummed bytes:
/// a prefix of NEW superblock spliced onto STALE slot bytes can decode to
/// a plausible-but-wrong root unless the CRC rejects it.
fn tear_grid() -> Vec<Fate> {
    let mut grid: Vec<Fate> = (0..=72).step_by(4).map(Fate::Tear).collect();
    grid.extend([
        Fate::Tear(100),
        Fate::Tear(2048),
        Fate::Tear(4090),
        Fate::Drop,
        Fate::Zero,
    ]);
    grid
}

/// Run one full crash case: workload, oracle snapshots, crash points,
/// images, idempotent recovery.
fn run_case(case: &CrashCase, images: &AtomicU64) -> Result<(), TestCaseError> {
    let vfs = FaultyVfs::new();
    let h = vfs.clone();
    let mut engine = DiskEngine::create_on(vfs, Params::default())
        .map_err(|e| TestCaseError::fail(format!("create failed: {e}")))?;
    let create_done = h.log_len();

    let mut oracle: BTreeMap<Key, Value> = BTreeMap::new();
    let mut snapshots = vec![oracle.clone()]; // generation 0: empty tree
    let mut commit_done_len = vec![create_done];
    let mut keys: BTreeSet<Key> = BTreeSet::from([vec![255], vec![254, 254]]);

    // The forced tail: a non-empty final commit and >= 3 generations, so
    // each superblock slot has held a real previous generation.
    let tail = vec![
        PlanOp::Ins(vec![250], case.tail_vals[0].clone()),
        PlanOp::Commit,
        PlanOp::Ins(vec![251], case.tail_vals[1].clone()),
        PlanOp::Commit,
        PlanOp::Ins(vec![252], case.tail_vals[2].clone()),
        PlanOp::Commit,
    ];
    let ops: Vec<PlanOp> = case.body.iter().cloned().chain(tail).collect();
    for (k, _) in &case.live_ops {
        keys.insert(k.clone());
    }

    let mut before_final = 0;
    for (i, op) in ops.iter().enumerate() {
        match op {
            PlanOp::Ins(k, v) => {
                keys.insert(k.clone());
                engine.insert(k.clone(), v.clone());
                oracle.insert(k.clone(), v.clone());
            }
            PlanOp::Commit => {
                if i == ops.len() - 1 {
                    before_final = h.log_len();
                }
                engine
                    .commit()
                    .map_err(|e| TestCaseError::fail(format!("workload commit failed: {e}")))?;
                snapshots.push(oracle.clone());
                commit_done_len.push(h.log_len());
            }
        }
    }
    drop(engine);

    let log = h.op_log();
    let ctx = CaseCtx {
        h: &h,
        create_done,
        snapshots: &snapshots,
        commit_done_len: &commit_done_len,
        keys: &keys,
        live_ops: &case.live_ops,
        images,
    };

    // C=6 uniform crash positions, R=6 images each.
    for seed in &case.crash_seeds {
        let pos = (*seed % (log.len() as u64 + 1)) as usize;
        for fates in &case.crash_fates {
            check_image(&ctx, pos, fates)?;
        }
    }

    // The three canonical danger points of the final commit. The span is
    // located structurally (first sync / last write after `before_final`)
    // so the points stay meaningful even for a mutated commit protocol.
    let first_sync = (before_final..log.len())
        .find(|&i| matches!(log[i], VfsOp::Sync { .. }))
        .expect("a commit always syncs");
    let last_write = (before_final..log.len())
        .rev()
        .find(|&i| matches!(log[i], VfsOp::Write { .. }))
        .expect("the final commit always writes (its tail insert is dirty)");

    // (a) just before the data sync: the record writes are pending.
    let pos_a = first_sync;
    for fates in cartesian_fates(h.window_len_at(pos_a)) {
        check_image(&ctx, pos_a, &fates)?;
    }
    // (b) between the data sync and the superblock write.
    let pos_b = first_sync + 1;
    for fates in cartesian_fates(h.window_len_at(pos_b)) {
        check_image(&ctx, pos_b, &fates)?;
    }
    // (c) mid-superblock-write: tear the slot at every field boundary.
    let pos_c = last_write + 1;
    let w_c = h.window_len_at(pos_c);
    for f in tear_grid() {
        check_image(&ctx, pos_c, &last_op_fates(w_c, f))?;
    }

    // Idempotent recovery: open a torn image (open truncates the torn
    // tail via set_len), crash the truncation window itself, open again.
    let img1 = h.crash_image_at(pos_c, &last_op_fates(w_c, Fate::Tear(2048)));
    images.fetch_add(1, Ordering::Relaxed);
    let v1 = FaultyVfs::from_image(img1);
    let h1 = v1.clone();
    if let Ok(eng) = DiskEngine::open_on(v1) {
        let g = eng.generation() as usize;
        drop(eng);
        let img2 = h1.crash_image_at(h1.log_len(), &case.idem_fates);
        images.fetch_add(1, Ordering::Relaxed);
        let mut eng = DiskEngine::open_on(FaultyVfs::from_image(img2)).map_err(|e| {
            TestCaseError::fail(format!("idempotent recovery: second open failed: {e}"))
        })?;
        prop_assert_eq!(
            eng.generation() as usize,
            g,
            "idempotent recovery changed the recovered generation"
        );
        sweep(&mut eng, &snapshots[g], &keys, "idempotent A2")?;
        eng.load_all()
            .map_err(|e| TestCaseError::fail(format!("idempotent A4: load_all: {e}")))?;
        eng.check_invariants()
            .map_err(|e| TestCaseError::fail(format!("idempotent A4: invariants: {e}")))?;
    }

    Ok(())
}

fn run_crash_suite(name: &str, strategy: impl Strategy<Value = CrashCase>) {
    let images = AtomicU64::new(0);
    let mut config = Config {
        cases: cases_budget(),
        ..Config::default()
    };
    config.failure_persistence = None;
    let mut runner = TestRunner::new(config);
    let result = runner.run(&strategy, |case| run_case(&case, &images));
    println!(
        "{name}: crash images evaluated: {}",
        images.load(Ordering::Relaxed)
    );
    if let Err(e) = result {
        panic!("{name} failed: {e}");
    }
}

#[test]
fn crash_random_workloads() {
    run_crash_suite("crash_random_workloads", crash_case(random_body()));
}

#[test]
fn crash_ascending_keys() {
    run_crash_suite("crash_ascending_keys", crash_case(ascending_body()));
}

#[test]
fn crash_overwrite_heavy() {
    run_crash_suite("crash_overwrite_heavy", crash_case(overwrite_body()));
}

// ---------------------------------------------------------------------
// Sync-failure injection (fsyncgate): a failed sync poisons the engine,
// and recovery from any crash image afterwards still honors A1–A5.

#[derive(Debug)]
struct SyncFailCase {
    body: Vec<PlanOp>,
    /// Selects which workload commit gets the injected failure.
    commit_seed: u64,
    /// false: fail the data sync; true: fail the superblock sync.
    fail_superblock_sync: bool,
    /// What the failed sync durably applied anyway.
    fail_fates: Vec<Fate>,
    /// Fate vectors for the post-failure crash images.
    img_fates: Vec<Vec<Fate>>,
    live_ops: Vec<(Key, Value)>,
}

fn sync_fail_case(body: impl Strategy<Value = Vec<PlanOp>>) -> impl Strategy<Value = SyncFailCase> {
    (
        body,
        any::<u64>(),
        any::<bool>(),
        proptest::collection::vec(fate_strategy(), 0..=4),
        proptest::collection::vec(proptest::collection::vec(fate_strategy(), 0..=4), 3),
        live_ops_strategy(),
    )
        .prop_map(
            |(body, commit_seed, fail_superblock_sync, fail_fates, img_fates, live_ops)| {
                SyncFailCase {
                    body,
                    commit_seed,
                    fail_superblock_sync,
                    fail_fates,
                    img_fates,
                    live_ops,
                }
            },
        )
}

fn run_sync_fail_case(case: &SyncFailCase, images: &AtomicU64) -> Result<(), TestCaseError> {
    let vfs = FaultyVfs::new();
    let h = vfs.clone();
    let mut engine = DiskEngine::create_on(vfs, Params::default())
        .map_err(|e| TestCaseError::fail(format!("create failed: {e}")))?;
    let create_done = h.log_len();

    let mut oracle: BTreeMap<Key, Value> = BTreeMap::new();
    let mut snapshots = vec![oracle.clone()];
    let mut commit_done_len = vec![create_done];
    let mut keys: BTreeSet<Key> = BTreeSet::from([vec![255], vec![254, 254]]);
    for (k, _) in &case.live_ops {
        keys.insert(k.clone());
    }

    let mut ops = case.body.clone();
    ops.push(PlanOp::Ins(vec![250], vec![9]));
    ops.push(PlanOp::Commit);
    let n_commits = ops.iter().filter(|o| matches!(o, PlanOp::Commit)).count() as u64;
    let target = (case.commit_seed % n_commits) as usize;

    let mut commit_index = 0;
    for op in &ops {
        match op {
            PlanOp::Ins(k, v) => {
                keys.insert(k.clone());
                engine.insert(k.clone(), v.clone());
                oracle.insert(k.clone(), v.clone());
            }
            PlanOp::Commit => {
                if commit_index == target {
                    // The attempted generation: record its would-be
                    // snapshot, since a failed superblock sync may still
                    // surface it durably (the lost-ack window).
                    snapshots.push(oracle.clone());
                    h.fail_nth_sync(
                        u32::from(case.fail_superblock_sync),
                        case.fail_fates.clone(),
                    );
                    let err = engine
                        .commit()
                        .expect_err("the injected sync failure must surface");
                    prop_assert!(
                        matches!(err, DiskError::Io(_)),
                        "expected the injected Io error, got {:?}",
                        err
                    );
                    let err = engine
                        .commit()
                        .expect_err("a poisoned engine must refuse commits");
                    prop_assert!(
                        matches!(err, DiskError::Poisoned),
                        "expected Poisoned, got {:?}",
                        err
                    );
                    break;
                }
                engine
                    .commit()
                    .map_err(|e| TestCaseError::fail(format!("workload commit failed: {e}")))?;
                snapshots.push(oracle.clone());
                commit_done_len.push(h.log_len());
                commit_index += 1;
            }
        }
    }
    drop(engine);

    let ctx = CaseCtx {
        h: &h,
        create_done,
        snapshots: &snapshots,
        commit_done_len: &commit_done_len,
        keys: &keys,
        live_ops: &case.live_ops,
        images,
    };
    let end = h.log_len();
    for fates in &case.img_fates {
        check_image(&ctx, end, fates)?;
    }
    Ok(())
}

#[test]
fn crash_sync_failure_poisons_and_recovers() {
    let images = AtomicU64::new(0);
    let mut config = Config {
        cases: cases_budget(),
        ..Config::default()
    };
    config.failure_persistence = None;
    let mut runner = TestRunner::new(config);
    let result = runner.run(&sync_fail_case(random_body()), |case| {
        run_sync_fail_case(&case, &images)
    });
    println!(
        "crash_sync_failure_poisons_and_recovers: crash images evaluated: {}",
        images.load(Ordering::Relaxed)
    );
    if let Err(e) = result {
        panic!("crash_sync_failure_poisons_and_recovers failed: {e}");
    }
}
