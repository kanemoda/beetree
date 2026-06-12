//! The M3.2 benchmark suite: within-engine characterization of
//! `DiskEngine` (no cross-engine comparisons). Five experiments, each a
//! subcommand emitting CSV (with a machine/provenance header) into
//! docs/bench/results/. Engine-level I/O (`CountingVfs`) is the contract
//! metric; the OS page cache is NOT defeated (SPEC "Observability").
//!
//! Release-only: refuses to run under debug_assertions.

use std::io::Write as _;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use beetree::workload::{KeyDist, Mix, OpStream, WorkOp};
use beetree::{CountingVfs, DiskEngine, FileVfs, IoStats, KvEngine, Params, UpsertOp};

type Engine = DiskEngine<CountingVfs<FileVfs>>;

const HELP: &str = "\
bench — the beetree M3.2 benchmark suite (within-engine only)

USAGE: bench <e1|e2|e3|e4|e5|all> [key=value ...]

  e1   write amplification vs commit interval
  e2   read amplification vs cache budget (uniform + zipfian)
  e3   parameter grid F x B with derived eps_eff (ADR-0016)
  e4   named mix suite (load, point-read, ycsb-a/b/c, upsert-heavy, scan-mix)
  e5   space debt over an update-heavy run (the ADR-0008 curve)
  all  run every experiment

ARGS:
  seed=<u64>     workload seed (default 48879)
  trace=<path>   stream the measured phase's full trace2 as JSONL
                 (note: drain() is trace-invisible by design and excluded
                 from traced phases; docs/findings.md)

Output: docs/bench/results/<exp>.csv — run from the repository root.
Wall-clock metrics are median/min/max over 3 runs; engine-level io_stats
are deterministic (M3.1) and asserted identical across runs.";

fn main() {
    if cfg!(debug_assertions) {
        eprintln!(
            "bench refuses to run in a debug build: numbers would be \
             meaningless. Use: cargo run --release --bin bench -- <exp>"
        );
        std::process::exit(2);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{HELP}");
        std::process::exit(2);
    };
    let mut seed: u64 = 0xBEEF;
    let mut trace: Option<PathBuf> = None;
    for arg in &args[1..] {
        match arg.split_once('=') {
            Some(("seed", v)) => seed = v.parse().expect("seed=<u64>"),
            Some(("trace", v)) => trace = Some(PathBuf::from(v)),
            _ => die(&format!("unknown argument {arg:?}\n\n{HELP}")),
        }
    }
    std::fs::create_dir_all("docs/bench/results").expect("create docs/bench/results");
    let started = Instant::now();
    match cmd.as_str() {
        "e1" => e1(seed, trace.as_deref()),
        "e2" => e2(seed, trace.as_deref()),
        "e3" => e3(seed, trace.as_deref()),
        "e4" => e4(seed, trace.as_deref()),
        "e5" => e5(seed, trace.as_deref()),
        "all" => {
            e1(seed, None);
            e2(seed, None);
            e3(seed, None);
            e4(seed, None);
            e5(seed, None);
        }
        "--help" | "-h" | "help" => println!("{HELP}"),
        other => die(&format!("unknown subcommand {other:?}\n\n{HELP}")),
    }
    eprintln!("total bench wall: {:.1}s", started.elapsed().as_secs_f64());
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2)
}

// ---------------------------------------------------------------------
// Infrastructure.

/// A scratch directory under the OS temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("beetree-bench-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sh(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn proc_field(path: &str, key: &str) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

/// The provenance/machine header every CSV carries (as `# key: value`).
fn header(seed: u64, params: &[(&str, String)]) -> Vec<(String, String)> {
    let mut out = vec![
        ("git_commit".into(), sh("git", &["rev-parse", "--short", "HEAD"])),
        ("rustc".into(), sh("rustc", &["-V"])),
        ("cpu".into(), proc_field("/proc/cpuinfo", "model name")),
        ("ram".into(), proc_field("/proc/meminfo", "MemTotal")),
        (
            "kernel".into(),
            std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".into()),
        ),
        ("date".into(), sh("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])),
        ("seed".into(), seed.to_string()),
        ("reps".into(), "3 (wall median/min/max; io_stats identical across reps by assertion)".into()),
        ("caveats".into(), "page cache not defeated; engine-level I/O is the contract metric; single-threaded; CoW space unreclaimed".into()),
    ];
    for (k, v) in params {
        out.push(((*k).into(), v.clone()));
    }
    out
}

fn write_csv(name: &str, header: &[(String, String)], columns: &[&str], rows: &[Vec<String>]) {
    let path = Path::new("docs/bench/results").join(name);
    let mut out = String::new();
    for (k, v) in header {
        out.push_str(&format!("# {k}: {v}\n"));
    }
    out.push_str(&columns.join(","));
    out.push('\n');
    for row in rows {
        assert_eq!(row.len(), columns.len(), "CSV row width mismatch");
        out.push_str(&row.join(","));
        out.push('\n');
    }
    std::fs::write(&path, out).expect("write CSV");
    eprintln!("wrote {}", path.display());
}

/// Nearest-rank percentile over a SORTED slice: ceil(p·n)−1.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    assert!(!sorted.is_empty(), "no samples");
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Run `f` three times; assert engine-level io is identical across runs;
/// return (wall median, min, max in seconds) and the payload of the
/// median-wall run.
fn reps<T>(mut f: impl FnMut(usize) -> (Duration, IoStats, T)) -> (f64, f64, f64, IoStats, T) {
    let mut runs: Vec<(Duration, IoStats, T)> = (0..3).map(&mut f).collect();
    let io = runs[0].1;
    for (i, run) in runs.iter().enumerate() {
        assert_eq!(
            run.1, io,
            "io_stats diverged across reps (rep {i}): determinism violation"
        );
    }
    let mut walls: Vec<Duration> = runs.iter().map(|r| r.0).collect();
    walls.sort();
    let (min, median, max) = (walls[0], walls[1], walls[2]);
    let median_idx = runs
        .iter()
        .position(|r| r.0 == median)
        .expect("median wall exists");
    let payload = runs.swap_remove(median_idx).2;
    (
        median.as_secs_f64(),
        min.as_secs_f64(),
        max.as_secs_f64(),
        io,
        payload,
    )
}

fn key_bytes(idx: u64) -> Vec<u8> {
    idx.to_be_bytes().to_vec()
}

fn value_bytes(counter: u64) -> Vec<u8> {
    counter.to_le_bytes().to_vec()
}

fn create(path: &Path, params: Params, budget: Option<u64>) -> Engine {
    let vfs = CountingVfs::new(FileVfs::create(path).expect("create file"));
    match budget {
        Some(b) => DiskEngine::create_on_bounded(vfs, params, b),
        None => DiskEngine::create_on(vfs, params),
    }
    .expect("create engine")
}

fn open(path: &Path, budget: Option<u64>) -> Engine {
    let vfs = CountingVfs::new(FileVfs::open(path).expect("open file"));
    match budget {
        Some(b) => DiskEngine::open_on_bounded(vfs, b),
        None => DiskEngine::open_on(vfs),
    }
    .expect("open engine")
}

/// Apply one op; returns logical bytes (key+value) read or written.
fn apply(engine: &mut Engine, op: &WorkOp, counter: u64) -> (u64, u64) {
    match op {
        WorkOp::Insert(k) => {
            engine.insert(key_bytes(*k), value_bytes(counter));
            (0, 16)
        }
        WorkOp::Read(k) => {
            let got = engine.get(&key_bytes(*k));
            (got.map_or(0, |v| 8 + v.len() as u64), 0)
        }
        WorkOp::UpsertAdd(k, d) => {
            engine.upsert(key_bytes(*k), UpsertOp::Add(*d));
            (0, 16)
        }
        WorkOp::Scan(start, count) => {
            let lo = Bound::Included(key_bytes(*start));
            let hi = Bound::Excluded(key_bytes(start + count));
            let got = engine.scan(lo, hi).expect("scan");
            (got.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum(), 0)
        }
    }
}

/// Sequentially load keys 0..n (8-byte values), commit every
/// `commit_every`, then drain + final commit so the on-disk tree is
/// message-free; returns (live_bytes, live_nodes).
fn load_base(engine: &mut Engine, n: u64, commit_every: u64) -> (u64, u64) {
    for i in 0..n {
        engine.insert(key_bytes(i), value_bytes(i));
        if i % commit_every == commit_every - 1 {
            engine.commit().expect("commit");
        }
    }
    engine.drain().expect("drain");
    engine.commit().expect("commit");
    (
        engine.live_bytes().expect("live_bytes"),
        engine.live_node_count().expect("live_node_count"),
    )
}

fn dump_trace(engine: &Engine, path: &Path) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("trace file"));
    for event in engine.trace2() {
        serde_json::to_writer(&mut out, event).expect("serialize trace event");
        out.write_all(b"\n").expect("write trace");
    }
    eprintln!("trace2 written to {}", path.display());
}

fn fmt_f(x: f64) -> String {
    format!("{x:.4}")
}

// ---------------------------------------------------------------------
// E1: write amplification vs commit interval.

fn e1(seed: u64, trace: Option<&Path>) {
    const N: u64 = 100_000;
    let mut rows = Vec::new();
    for interval in [1u64, 10, 100, 1000, 10_000] {
        let (median, min, max, io, (file_size, commits)) = reps(|_| {
            let scratch = Scratch::new("e1");
            let mut engine = create(&scratch.path("e1.db"), Params::default(), None);
            let ops: Vec<WorkOp> = OpStream::new(Mix::Load, KeyDist::Uniform, N, N, seed).collect();
            let mut commits = 0u64;
            let start = Instant::now();
            for (i, op) in ops.iter().enumerate() {
                apply(&mut engine, op, i as u64);
                if (i as u64) % interval == interval - 1 {
                    engine.commit().expect("commit");
                    commits += 1;
                }
            }
            if (N % interval) != 0 {
                engine.commit().expect("commit");
                commits += 1;
            }
            let wall = start.elapsed();
            let io = engine.io_stats();
            let file = engine.file_len().expect("file_len");
            if let Some(path) = trace {
                dump_trace(&engine, path);
            }
            (wall, io, (file, commits))
        });
        let logical = N * 16;
        rows.push(vec![
            interval.to_string(),
            logical.to_string(),
            io.write_bytes.to_string(),
            fmt_f(io.write_bytes as f64 / logical as f64),
            (commits * 4096).to_string(),
            file_size.to_string(),
            fmt_f(median),
            fmt_f(min),
            fmt_f(max),
        ]);
    }
    write_csv(
        "e1.csv",
        &header(
            seed,
            &[
                ("experiment", "e1 write-amp vs commit interval".into()),
                ("n", N.to_string()),
            ],
        ),
        &[
            "interval",
            "logical_bytes",
            "physical_write_bytes",
            "write_amp",
            "superblock_bytes",
            "file_size",
            "wall_median_s",
            "wall_min_s",
            "wall_max_s",
        ],
        &rows,
    );
}

// ---------------------------------------------------------------------
// E2: read amplification vs cache budget.

fn e2(seed: u64, trace: Option<&Path>) {
    const N: u64 = 1_000_000;
    const READS: u64 = 200_000;
    let scratch = Scratch::new("e2");
    let base = scratch.path("e2.db");
    let mut engine = create(&base, Params::default(), None);
    let (live, _) = load_base(&mut engine, N, 10_000);
    drop(engine);

    let mut rows = Vec::new();
    for dist in [KeyDist::Uniform, KeyDist::Zipfian] {
        let dist_name = match dist {
            KeyDist::Uniform => "uniform",
            KeyDist::Zipfian => "zipfian",
            KeyDist::Sequential => unreachable!(),
        };
        for pct in [None, Some(50u64), Some(25), Some(10), Some(5), Some(2)] {
            let budget = pct.map(|p| live * p / 100);
            let (median, min, max, io, (cache, lat)) = reps(|_| {
                let mut engine = open(&base, budget);
                let ops: Vec<WorkOp> =
                    OpStream::new(Mix::PointRead, dist, N, READS, seed).collect();
                let mut latencies: Vec<u64> = Vec::with_capacity(READS as usize);
                let start = Instant::now();
                for op in &ops {
                    let t = Instant::now();
                    let (read, _) = apply(&mut engine, op, 0);
                    latencies.push(t.elapsed().as_micros() as u64);
                    assert!(read > 0, "every loaded key must be found");
                }
                let wall = start.elapsed();
                let io = engine.io_stats();
                let cache = engine.cache_stats();
                if let Some(path) = trace {
                    dump_trace(&engine, path);
                }
                latencies.sort_unstable();
                (wall, io, (cache, latencies))
            });
            rows.push(vec![
                dist_name.into(),
                pct.map_or("unbounded".into(), |p| p.to_string()),
                io.read_ops.to_string(),
                io.read_bytes.to_string(),
                fmt_f(io.read_bytes as f64 / (READS * 16) as f64),
                fmt_f(cache.hits as f64 / (cache.hits + cache.misses) as f64),
                cache.evictions.to_string(),
                cache.overcommit_events.to_string(),
                percentile(&lat, 50.0).to_string(),
                percentile(&lat, 95.0).to_string(),
                percentile(&lat, 99.0).to_string(),
                fmt_f(median),
                fmt_f(min),
                fmt_f(max),
            ]);
        }
    }
    write_csv(
        "e2.csv",
        &header(
            seed,
            &[
                ("experiment", "e2 read-amp vs cache budget".into()),
                ("n_load", N.to_string()),
                ("n_reads", READS.to_string()),
                ("live_bytes", live.to_string()),
                ("budgets", "pct of LIVE bytes, never file size".into()),
            ],
        ),
        &[
            "distribution",
            "budget_pct",
            "read_ops",
            "read_bytes",
            "read_amp",
            "hit_rate",
            "evictions",
            "overcommit",
            "p50_us",
            "p95_us",
            "p99_us",
            "wall_median_s",
            "wall_min_s",
            "wall_max_s",
        ],
        &rows,
    );
}

// ---------------------------------------------------------------------
// E3: parameter grid.

fn e3(seed: u64, trace: Option<&Path>) {
    const N_LOAD: u64 = 200_000;
    const N_PHASE: u64 = 100_000;
    let mut rows = Vec::new();
    for f in [4usize, 8, 16, 32] {
        for b in [16usize, 64, 256] {
            let params = Params {
                fanout: f,
                buffer_capacity: b,
                leaf_capacity: b,
            };
            let (median, _min, _max, io_phase, (height, live_nodes, live, lat)) = reps(|_| {
                let scratch = Scratch::new("e3");
                let mut engine = create(&scratch.path("e3.db"), params, None);
                let (live, live_nodes) = load_base(&mut engine, N_LOAD, 10_000);
                drop(engine);
                let mut engine = open(&scratch.path("e3.db"), Some(live / 10));
                let io_before = engine.io_stats();
                let ops: Vec<WorkOp> =
                    OpStream::new(Mix::YcsbA, KeyDist::Uniform, N_LOAD, N_PHASE, seed).collect();
                let mut latencies = Vec::with_capacity(N_PHASE as usize);
                let start = Instant::now();
                for (i, op) in ops.iter().enumerate() {
                    let t = Instant::now();
                    apply(&mut engine, op, i as u64);
                    latencies.push(t.elapsed().as_micros() as u64);
                    if (i as u64) % 1000 == 999 {
                        engine.commit().expect("commit");
                    }
                }
                engine.commit().expect("commit");
                let wall = start.elapsed();
                let io_after = engine.io_stats();
                let height = engine.height().expect("height");
                if let Some(path) = trace {
                    dump_trace(&engine, path);
                }
                latencies.sort_unstable();
                let phase_io = IoStats {
                    read_ops: io_after.read_ops - io_before.read_ops,
                    read_bytes: io_after.read_bytes - io_before.read_bytes,
                    write_ops: io_after.write_ops - io_before.write_ops,
                    write_bytes: io_after.write_bytes - io_before.write_bytes,
                    syncs: io_after.syncs - io_before.syncs,
                    set_lens: io_after.set_lens - io_before.set_lens,
                };
                (wall, phase_io, (height, live_nodes, live, latencies))
            });
            let mean_node_bytes = live as f64 / live_nodes as f64;
            // Roughly half the ycsb-a ops write, half read; logical bytes
            // per op are 16 either way.
            let logical_writes = (N_PHASE / 2) * 16;
            let logical_reads = (N_PHASE / 2) * 16;
            rows.push(vec![
                f.to_string(),
                b.to_string(),
                height.to_string(),
                live_nodes.to_string(),
                fmt_f(mean_node_bytes),
                fmt_f((f as f64).ln() / mean_node_bytes.ln()),
                fmt_f(io_phase.write_bytes as f64 / logical_writes as f64),
                fmt_f(io_phase.read_bytes as f64 / logical_reads as f64),
                fmt_f(N_PHASE as f64 / median),
                percentile(&lat, 99.0).to_string(),
            ]);
        }
    }
    write_csv(
        "e3.csv",
        &header(
            seed,
            &[
                (
                    "experiment",
                    "e3 parameter grid (L = B; cache 10% live; ycsb-a phase, commit/1000)".into(),
                ),
                ("n_load", N_LOAD.to_string()),
                ("n_phase", N_PHASE.to_string()),
                (
                    "eps_eff",
                    "derived annotation: ln F / ln mean_node_bytes (ADR-0016)".into(),
                ),
            ],
        ),
        &[
            "F",
            "B",
            "height",
            "live_nodes",
            "mean_node_bytes",
            "eps_eff",
            "write_amp",
            "read_amp",
            "throughput_ops_s",
            "p99_us",
        ],
        &rows,
    );
}

// ---------------------------------------------------------------------
// E4: the named mix suite.

fn e4(seed: u64, trace: Option<&Path>) {
    const KEYSPACE: u64 = 200_000;
    const N_OPS: u64 = 500_000;
    let scratch = Scratch::new("e4");
    let base = scratch.path("base.db");
    let mut engine = create(&base, Params::default(), None);
    let (live, _) = load_base(&mut engine, KEYSPACE, 10_000);
    drop(engine);
    let budget = Some(live / 10);

    let mut rows = Vec::new();
    for mix in Mix::all() {
        let (median, _min, _max, io, (cache, lat)) = reps(|rep| {
            let mut engine = if mix == Mix::Load {
                create(
                    &scratch.path(&format!("load-{rep}.db")),
                    Params::default(),
                    budget,
                )
            } else {
                let copy = scratch.path(&format!("work-{rep}.db"));
                std::fs::copy(&base, &copy).expect("copy base");
                open(&copy, budget)
            };
            let ops: Vec<WorkOp> =
                OpStream::new(mix, KeyDist::Uniform, KEYSPACE, N_OPS, seed).collect();
            let writes = ops
                .iter()
                .any(|op| matches!(op, WorkOp::Insert(_) | WorkOp::UpsertAdd(..)));
            let mut latencies = Vec::with_capacity(N_OPS as usize);
            let start = Instant::now();
            for (i, op) in ops.iter().enumerate() {
                let t = Instant::now();
                apply(&mut engine, op, i as u64);
                latencies.push(t.elapsed().as_micros() as u64);
                if writes && (i as u64) % 1000 == 999 {
                    engine.commit().expect("commit");
                }
            }
            if writes {
                engine.commit().expect("commit");
            }
            let wall = start.elapsed();
            let io = engine.io_stats();
            let cache = engine.cache_stats();
            if let Some(path) = trace {
                dump_trace(&engine, path);
            }
            latencies.sort_unstable();
            (wall, io, (cache, latencies))
        });
        rows.push(vec![
            mix.name().into(),
            fmt_f(N_OPS as f64 / median),
            percentile(&lat, 50.0).to_string(),
            percentile(&lat, 95.0).to_string(),
            percentile(&lat, 99.0).to_string(),
            io.read_ops.to_string(),
            io.read_bytes.to_string(),
            io.write_ops.to_string(),
            io.write_bytes.to_string(),
            fmt_f(cache.hits as f64 / (cache.hits + cache.misses).max(1) as f64),
        ]);
    }
    write_csv(
        "e4.csv",
        &header(
            seed,
            &[
                ("experiment", "e4 mix suite (default params F=4/B=8/L=8; cache 10% live; commit/1000 on write mixes)".into()),
                ("keyspace", KEYSPACE.to_string()),
                ("n_ops", N_OPS.to_string()),
                ("live_bytes", live.to_string()),
            ],
        ),
        &[
            "mix",
            "throughput_ops_s",
            "p50_us",
            "p95_us",
            "p99_us",
            "read_ops",
            "read_bytes",
            "write_ops",
            "write_bytes",
            "hit_rate",
        ],
        &rows,
    );
}

// ---------------------------------------------------------------------
// E5: space debt.

fn e5(seed: u64, trace: Option<&Path>) {
    const KEYSPACE: u64 = 100_000;
    const N_UPDATES: u64 = 1_000_000;
    const SAMPLE_EVERY: u64 = 50_000;
    let scratch = Scratch::new("e5");
    let mut engine = create(&scratch.path("e5.db"), Params::default(), None);
    load_base(&mut engine, KEYSPACE, 1000);

    let mut rows = Vec::new();
    let ops: Vec<WorkOp> =
        OpStream::new(Mix::Load, KeyDist::Uniform, KEYSPACE, N_UPDATES, seed).collect();
    for (i, op) in ops.iter().enumerate() {
        apply(&mut engine, op, i as u64);
        let n = i as u64 + 1;
        if n % 1000 == 0 {
            engine.commit().expect("commit");
        }
        if n % SAMPLE_EVERY == 0 {
            let live = engine.live_bytes().expect("live_bytes");
            let file = engine.file_len().expect("file_len");
            rows.push(vec![
                n.to_string(),
                live.to_string(),
                file.to_string(),
                fmt_f(file as f64 / live as f64),
            ]);
        }
    }
    if let Some(path) = trace {
        dump_trace(&engine, path);
    }
    write_csv(
        "e5.csv",
        &header(
            seed,
            &[
                ("experiment", "e5 space debt: file/live ratio over update-heavy run (single run; all metrics deterministic byte counts)".into()),
                ("keyspace", KEYSPACE.to_string()),
                ("n_updates", N_UPDATES.to_string()),
                ("commit_every", "1000".into()),
            ],
        ),
        &["ops", "live_bytes", "file_size", "file_over_live"],
        &rows,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_nearest_rank_over_sorted_input() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), 50);
        assert_eq!(percentile(&v, 95.0), 95);
        assert_eq!(percentile(&v, 99.0), 99);
        assert_eq!(percentile(&v, 0.0), 1);
        assert_eq!(percentile(&v, 100.0), 100);
        assert_eq!(percentile(&[7], 99.0), 7);
    }

    #[test]
    fn csv_header_block_is_comment_lines() {
        let header = header(42, &[("experiment", "unit".into())]);
        assert!(header.iter().any(|(k, _)| k == "seed"));
        assert!(header.iter().any(|(k, _)| k == "git_commit"));
        assert!(
            header
                .iter()
                .any(|(k, v)| k == "caveats" && v.contains("page cache"))
        );
        // The writer renders every header pair as a `# ` comment line.
        let rendered: String = header
            .iter()
            .map(|(k, v)| format!("# {k}: {v}\n"))
            .collect();
        for line in rendered.lines() {
            assert!(line.starts_with("# "), "header line {line:?} not a comment");
        }
    }
}
