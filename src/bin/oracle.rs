//! The M4.1 hindsight rollout oracle: one policy-improvement step over
//! greedy-fullest, scored by the calibrated analytic cost model
//! (`tests/cost_model.rs`), measuring the optimality gap of the baseline
//! flush policy. NO machine learning — the deliverable is numbers
//! (`docs/analysis/FALSIFICATION.md`).
//!
//! One forward pass over a deterministic workload on `BeTree`. At each
//! SAMPLED flush decision (rate 1/S over the improved trajectory's global
//! decision counter, seeded hash — see [`sampled`]): for every legal
//! child alternative, fork the tree, force that choice, roll the next W
//! ops out under greedy-fullest, and score the window by the analytic
//! cost (simulated commits at the run's global boundaries plus a terminal
//! commit charging the dirty residue); take the argmin (ties prefer the
//! greedy choice, then the lowest index). Unsampled decisions stay
//! greedy. The total analytic cost of the resulting trajectory is
//! directly comparable to the pure greedy run on identical ops:
//! `gap = (base − improved) / base`.
//!
//! HONEST FRAMING (also in FALSIFICATION.md): this LOWER-BOUNDS the true
//! headroom. The action space is child choice at the baseline's fixed
//! trigger points; flush *timing* freedom is documented future work, and
//! a one-step improvement with a W-op horizon is itself an
//! approximation. A large gap proves headroom exists; a small gap is
//! evidence against, not proof.
//!
//! Everything is deterministic from one u64 seed: two runs produce
//! identical gaps, byte-identical CSV rows (wall aside) and decision
//! logs. Outputs: one CSV row per run, and a JSONL decision log — header
//! line (`schema_version: 1`) then one record per sampled decision — the
//! future training-data format (SPEC "Observability").
//!
//! Release-only: refuses to run under debug_assertions.

use std::cell::RefCell;
use std::io::Write as _;
use std::ops::Bound;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use beetree::workload::{KeyDist, Mix, OpStream, WorkOp};
use beetree::{
    BeTree, FlushCtx, FlushPolicy, GreedyFullest, KvEngine, Params, TraceEvent2, UpsertOp,
};
use serde::{Deserialize, Serialize};

const HELP: &str = "\
oracle — the M4.1 hindsight rollout oracle (docs/analysis/FALSIFICATION.md)

USAGE: oracle workload=<name> [key=value ...]

WORKLOADS:
  uniform-load zipfian-load ycsb-a ycsb-a-zipfian upsert-heavy ascending

ARGS:
  interval=<u64>   commit every K ops (default 1000)
  n=<u64>          ops (default 50000)
  keyspace=<u64>   key space (default 20000)
  sample=<u64>     sample 1 decision in S (default 8)
  window=<u64>     rollout horizon W in ops (default 1000)
  seed=<u64>       master seed (default 48879)
  fanout=<usize> buffer=<usize> leaf=<usize>   params (default 4/8/8)
  out=<dir>        output directory (default docs/analysis/data)

Writes <out>/<workload>-i<K>[-f<F>b<B>l<L>].{csv,jsonl} and prints the
CSV row to stderr.";

fn main() {
    if cfg!(debug_assertions) {
        eprintln!(
            "oracle refuses to run in a debug build: rollouts would be \
             10x slower. Use: cargo run --release --bin oracle -- ..."
        );
        std::process::exit(2);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        eprintln!("{HELP}");
        std::process::exit(2);
    }
    let mut workload: Option<&str> = None;
    let mut interval = 1000u64;
    let mut n = 50_000u64;
    let mut keyspace = 20_000u64;
    let mut sample = 8u64;
    let mut window = 1000u64;
    let mut seed = 0xBEEFu64;
    let mut params = Params::default();
    let mut out = PathBuf::from("docs/analysis/data");
    for arg in &args {
        match arg.split_once('=') {
            Some(("workload", v)) => workload = Some(v),
            Some(("interval", v)) => interval = v.parse().expect("interval=<u64>"),
            Some(("n", v)) => n = v.parse().expect("n=<u64>"),
            Some(("keyspace", v)) => keyspace = v.parse().expect("keyspace=<u64>"),
            Some(("sample", v)) => sample = v.parse().expect("sample=<u64>"),
            Some(("window", v)) => window = v.parse().expect("window=<u64>"),
            Some(("seed", v)) => seed = v.parse().expect("seed=<u64>"),
            Some(("fanout", v)) => params.fanout = v.parse().expect("fanout=<usize>"),
            Some(("buffer", v)) => params.buffer_capacity = v.parse().expect("buffer=<usize>"),
            Some(("leaf", v)) => params.leaf_capacity = v.parse().expect("leaf=<usize>"),
            Some(("out", v)) => out = PathBuf::from(v),
            _ => {
                eprintln!("unknown argument {arg:?}\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }
    let Some(workload) = workload else {
        eprintln!("missing workload=<name>\n\n{HELP}");
        std::process::exit(2);
    };
    let Some((mix, dist)) = parse_workload(workload) else {
        eprintln!("unknown workload {workload:?}\n\n{HELP}");
        std::process::exit(2);
    };

    let ops: Vec<WorkOp> = OpStream::new(mix, dist, keyspace, n, seed).collect();
    let cfg = OracleCfg {
        commit_interval: interval,
        sample_rate: sample,
        window,
        seed,
    };
    // The pre-workload state: a fresh tree with its durable generation 0
    // mirrored (`create()` commits before returning). Base, improved, and
    // verification runs all fork from here, so every reported cost is the
    // cost of the WORKLOAD alone.
    let start = {
        let mut tree = BeTree::new(params);
        tree.simulate_commit();
        tree
    };

    let wall_start = Instant::now();
    let (base_cost, base_decisions) = greedy_cost(&start, &ops, interval);
    let outcome = improved_run(&start, &ops, &cfg);
    let improved_cost = scripted_cost(&start, &ops, interval, &outcome.script);
    let wall = wall_start.elapsed().as_secs_f64();
    let gap_pct = (base_cost as f64 - improved_cost as f64) / base_cost as f64 * 100.0;
    let decisions_total = outcome.script.len() as u64;
    let decisions_sampled = outcome.records.len() as u64;

    std::fs::create_dir_all(&out).expect("create output directory");
    // Output names are self-describing: non-default params and budget
    // knobs land in the file name, so runs never silently overwrite.
    let mut tag = format!("{workload}-i{interval}");
    if params != Params::default() {
        tag.push_str(&format!(
            "-f{}b{}l{}",
            params.fanout, params.buffer_capacity, params.leaf_capacity
        ));
    }
    if sample != 8 {
        tag.push_str(&format!("-s{sample}"));
    }
    if window != 1000 {
        tag.push_str(&format!("-w{window}"));
    }

    let header = LogHeader {
        schema_version: 1,
        kind: "beetree-flush-decision-log".into(),
        seed,
        workload: workload.into(),
        n_ops: n,
        keyspace,
        commit_interval: interval,
        sample_rate: sample,
        window,
        params,
        sampler: "sampled(g) := splitmix64(seed XOR g*0x9E3779B97F4A7C15) mod S == 0 \
                  over the improved trajectory's global decision counter g"
            .into(),
        cost_model: "window cost := sum of simulated commit costs (sum over dirty nodes of \
                     8 + bincode payload bytes, + 4096 superblock) at every global K-op \
                     boundary inside the window, plus a terminal commit charging the \
                     dirty residue; calibrated exactly to DiskEngine write bytes \
                     (tests/cost_model.rs)"
            .into(),
    };
    let mut log = std::io::BufWriter::new(
        std::fs::File::create(out.join(format!("{tag}.jsonl"))).expect("create jsonl"),
    );
    serde_json::to_writer(&mut log, &header).expect("write log header");
    log.write_all(b"\n").expect("write log");
    for record in &outcome.records {
        serde_json::to_writer(&mut log, record).expect("write log record");
        log.write_all(b"\n").expect("write log");
    }
    drop(log);

    let row = format!(
        "{workload},{interval},{base_cost},{improved_cost},{gap_pct:.4},\
         {decisions_total},{decisions_sampled},{wall:.1}"
    );
    let csv = format!(
        "# seed: {seed}\n# n: {n}\n# keyspace: {keyspace}\n# sample_rate: {sample}\n\
         # window: {window}\n# params: F={} B={} L={}\n\
         # base_decisions: {base_decisions}\n\
         # coverage: {:.4}\n\
         workload,interval,base_cost,improved_cost,gap_pct,decisions_total,decisions_sampled,wall\n\
         {row}\n",
        params.fanout,
        params.buffer_capacity,
        params.leaf_capacity,
        decisions_sampled as f64 / decisions_total.max(1) as f64,
    );
    std::fs::write(out.join(format!("{tag}.csv")), csv).expect("write csv");
    eprintln!("{row}");
}

fn parse_workload(name: &str) -> Option<(Mix, KeyDist)> {
    Some(match name {
        "uniform-load" => (Mix::Load, KeyDist::Uniform),
        "zipfian-load" => (Mix::Load, KeyDist::Zipfian),
        "ycsb-a" => (Mix::YcsbA, KeyDist::Uniform),
        "ycsb-a-zipfian" => (Mix::YcsbA, KeyDist::Zipfian),
        "upsert-heavy" => (Mix::UpsertHeavy, KeyDist::Uniform),
        "ascending" => (Mix::Load, KeyDist::Sequential),
        _ => return None,
    })
}

// ---------------------------------------------------------------------
// The decision-log JSONL schema, version 1 (SPEC "Observability").

/// Line 1 of every decision log: provenance and the exact semantics of
/// the numbers that follow. `schema_version` gates future field
/// additions — formats never silently mix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LogHeader {
    schema_version: u32,
    kind: String,
    seed: u64,
    workload: String,
    n_ops: u64,
    keyspace: u64,
    commit_interval: u64,
    sample_rate: u64,
    window: u64,
    params: Params,
    sampler: String,
    cost_model: String,
}

/// One sampled decision: the full context the policy saw, every legal
/// alternative's measured window cost, and both the oracle's and the
/// baseline's choices. This is one training example.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LogRecord {
    /// Workload op (0-based) whose flush cascade contained the decision.
    op_index: u64,
    /// Global decision counter on the improved trajectory.
    decision_index: u64,
    ctx: FlushCtx,
    /// Window cost per legal child (children with pending messages), in
    /// ascending child order.
    alternatives: Vec<AltCost>,
    /// The oracle's argmin choice (ties prefer greedy, then lowest index).
    chosen: usize,
    /// What greedy-fullest would have chosen from `ctx`.
    greedy_choice: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct AltCost {
    child: usize,
    window_cost: u64,
}

// ---------------------------------------------------------------------
// Deterministic decision sampling.

/// Decision `g` (global counter on the improved trajectory) is sampled
/// iff `splitmix64(seed ^ g·φ) % s == 0` — expected rate 1/S, seeded,
/// and free of aliasing against the periodic structure of cascade
/// decisions (a plain `g % S` could synchronize with per-level decision
/// patterns).
fn sampled(seed: u64, g: u64, s: u64) -> bool {
    if s <= 1 {
        return true;
    }
    let mut z = seed ^ g.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z % s == 0
}

// ---------------------------------------------------------------------
// The scripted policy: forced prefix, greedy tail, optional recording.

#[derive(Debug, Default)]
struct ScriptState {
    /// Choices to force, in decision order; decisions beyond the script
    /// fall back to greedy-fullest.
    forced: Vec<usize>,
    /// Decisions made so far (index into `forced` while it lasts).
    cursor: usize,
    /// Recorded (ctx, choice) per decision when `record` is set.
    recorded: Vec<(FlushCtx, usize)>,
    record: bool,
}

/// A driver-controlled [`FlushPolicy`]: the driver holds the other end of
/// the `Rc` and reads back what happened after each op (single-threaded
/// by design, like everything here).
#[derive(Debug, Clone)]
struct Scripted(Rc<RefCell<ScriptState>>);

impl FlushPolicy for Scripted {
    fn choose(&mut self, ctx: &FlushCtx) -> usize {
        let mut state = self.0.borrow_mut();
        let choice = if state.cursor < state.forced.len() {
            state.forced[state.cursor]
        } else {
            GreedyFullest::pick(&ctx.child_pending)
        };
        state.cursor += 1;
        if state.record {
            state.recorded.push((ctx.clone(), choice));
        }
        choice
    }
}

/// A `BeTree` paired with the handle to its scripted policy.
struct SimTree {
    tree: BeTree,
    script: Rc<RefCell<ScriptState>>,
}

impl SimTree {
    fn fork_from(source: &BeTree) -> SimTree {
        let script = Rc::new(RefCell::new(ScriptState::default()));
        let tree = source.fork_for_sim(Box::new(Scripted(script.clone())));
        SimTree { tree, script }
    }

    /// Run one op under `forced`-then-greedy, recording every decision.
    fn run_op(&mut self, op: &WorkOp, counter: u64, forced: Vec<usize>) -> Vec<(FlushCtx, usize)> {
        {
            let mut state = self.script.borrow_mut();
            state.forced = forced;
            state.cursor = 0;
            state.recorded.clear();
            state.record = true;
        }
        apply(&mut self.tree, op, counter);
        let mut state = self.script.borrow_mut();
        state.record = false;
        std::mem::take(&mut state.recorded)
    }
}

/// Apply one workload op (bench.rs conventions: 8-byte BE keys, the
/// global op index as the 8-byte LE value — replay-stable, so every fork
/// rebuilds byte-identical messages).
fn apply(tree: &mut BeTree, op: &WorkOp, op_index: u64) {
    match op {
        WorkOp::Insert(k) => tree.insert(k.to_be_bytes().to_vec(), op_index.to_le_bytes().to_vec()),
        WorkOp::Read(k) => {
            let _ = tree.get(&k.to_be_bytes());
        }
        WorkOp::UpsertAdd(k, d) => tree.upsert(k.to_be_bytes().to_vec(), UpsertOp::Add(*d)),
        WorkOp::Scan(start, count) => {
            let _ = tree.scan(
                Bound::Included(start.to_be_bytes().to_vec()),
                Bound::Excluded((start + count).to_be_bytes().to_vec()),
            );
        }
    }
}

// ---------------------------------------------------------------------
// Cost accounting (the calibrated analytic model over a whole run).

/// Total analytic cost of running `ops` from `start` under the tree's
/// own policy: simulated commits at every K-op boundary plus the
/// trailing partial window, exactly the cadence `tests/cost_model.rs`
/// calibrates against physical write bytes.
fn run_cost(tree: &mut BeTree, ops: &[WorkOp], commit_interval: u64) -> u64 {
    let mut cost = 0;
    for (i, op) in ops.iter().enumerate() {
        apply(tree, op, i as u64);
        if (i as u64 + 1) % commit_interval == 0 {
            cost += tree.simulate_commit();
        }
    }
    if ops.len() as u64 % commit_interval != 0 {
        cost += tree.simulate_commit();
    }
    cost
}

/// Baseline: pure greedy-fullest from `start`; returns (cost, decisions).
fn greedy_cost(start: &BeTree, ops: &[WorkOp], commit_interval: u64) -> (u64, u64) {
    let mut tree = start.fork_for_sim(Box::new(GreedyFullest));
    let cost = run_cost(&mut tree, ops, commit_interval);
    let decisions = tree
        .trace2()
        .iter()
        .filter(|e| matches!(e, TraceEvent2::FlushDecision { .. }))
        .count() as u64;
    (cost, decisions)
}

/// Accounting/verification pass: replay `ops` from `start` forcing the
/// improved trajectory's complete decision script; returns its total
/// cost. Panics if the script length does not match the decision count —
/// the improved trajectory must be exactly reproducible.
fn scripted_cost(start: &BeTree, ops: &[WorkOp], commit_interval: u64, script: &[usize]) -> u64 {
    let sim = SimTree::fork_from(start);
    sim.script.borrow_mut().forced = script.to_vec();
    let mut tree = sim.tree;
    let cost = run_cost(&mut tree, ops, commit_interval);
    let consumed = sim.script.borrow().cursor;
    assert_eq!(
        consumed,
        script.len(),
        "the improved trajectory must replay to exactly its own decision count"
    );
    cost
}

// ---------------------------------------------------------------------
// The oracle itself.

#[derive(Debug, Clone, Copy)]
struct OracleCfg {
    commit_interval: u64,
    sample_rate: u64,
    window: u64,
    seed: u64,
}

struct Outcome {
    /// Every decision of the improved trajectory, in global order.
    script: Vec<usize>,
    /// One record per sampled (evaluated) decision.
    records: Vec<LogRecord>,
}

/// Window cost of: completing op `i` from the pre-op state `pre` under
/// `forced`-then-greedy, then rolling the next W ops out greedily, with
/// simulated commits at the run's global boundaries and a terminal
/// commit charging the dirty residue (the terminal superblock constant
/// cancels across alternatives).
fn rollout_cost(
    pre: &BeTree,
    ops: &[WorkOp],
    i: usize,
    forced: Vec<usize>,
    cfg: &OracleCfg,
) -> u64 {
    let sim = SimTree::fork_from(pre);
    sim.script.borrow_mut().forced = forced;
    let mut tree = sim.tree;
    let mut cost = 0;
    let end = (i + 1 + cfg.window as usize).min(ops.len());
    for (m, op) in ops.iter().enumerate().take(end).skip(i) {
        apply(&mut tree, op, m as u64);
        if (m as u64 + 1) % cfg.commit_interval == 0 {
            cost += tree.simulate_commit();
        }
    }
    cost + tree.simulate_commit()
}

/// Argmin over window costs; ties prefer the greedy choice (no claimed
/// disagreement without a measured saving), then the lowest child index.
fn pick_argmin(alternatives: &[AltCost], greedy_choice: usize) -> usize {
    let min = alternatives
        .iter()
        .map(|a| a.window_cost)
        .min()
        .expect("a flush decision has at least one legal child");
    if alternatives
        .iter()
        .any(|a| a.child == greedy_choice && a.window_cost == min)
    {
        return greedy_choice;
    }
    alternatives
        .iter()
        .find(|a| a.window_cost == min)
        .expect("min exists")
        .child
}

/// One forward pass with one-step policy improvement at sampled
/// decisions (see the module docs for the full picture).
///
/// Mechanics: ops run directly on the improved-trajectory tree T under
/// greedy. The moment an op turns out to contain a sampled decision, T is
/// rebuilt from the last checkpoint (a fork taken after the previous
/// sampled op): replay the clean span, then resolve the op decision by
/// decision — re-running it from the pre-op state with the resolved
/// prefix forced — evaluating every legal alternative by rollout at each
/// sampled decision. Checkpoint replay is exact because clean spans are
/// all-greedy and everything is deterministic.
fn improved_run(start: &BeTree, ops: &[WorkOp], cfg: &OracleCfg) -> Outcome {
    let mut t = SimTree::fork_from(start);
    let mut checkpoint = start.fork_for_sim(Box::new(GreedyFullest));
    let mut checkpoint_at = 0usize;
    let mut counter = 0u64;
    let mut script: Vec<usize> = Vec::new();
    let mut records: Vec<LogRecord> = Vec::new();

    for i in 0..ops.len() {
        let ds = t.run_op(&ops[i], i as u64, Vec::new());
        let first_sampled =
            (0..ds.len()).find(|&j| sampled(cfg.seed, counter + j as u64, cfg.sample_rate));

        if let Some(j0) = first_sampled {
            // T ran this op fully greedy, which is wrong from decision j0
            // on. Rebuild the pre-op state from the checkpoint...
            let mut pre = checkpoint.fork_for_sim(Box::new(GreedyFullest));
            for (m, op) in ops.iter().enumerate().take(i).skip(checkpoint_at) {
                apply(&mut pre, op, m as u64);
                if (m as u64 + 1) % cfg.commit_interval == 0 {
                    pre.simulate_commit();
                }
            }
            // ...and resolve the op decision by decision. `fixed` holds
            // the resolved choices; the greedy prefix before j0 is
            // already known from the throwaway run.
            let mut fixed: Vec<usize> = ds[..j0].iter().map(|&(_, c)| c).collect();
            let accepted = loop {
                let mut attempt = SimTree::fork_from(&pre);
                let ds = attempt.run_op(&ops[i], i as u64, fixed.clone());
                let mut next_sampled = None;
                for (j, &(_, choice)) in ds.iter().enumerate().skip(fixed.len()) {
                    if sampled(cfg.seed, counter + j as u64, cfg.sample_rate) {
                        next_sampled = Some(j);
                        break;
                    }
                    // Unsampled: lock in the greedy choice it just made.
                    fixed.push(choice);
                }
                let Some(j) = next_sampled else {
                    break (attempt, ds);
                };
                let ctx = ds[j].0.clone();
                let greedy_choice = GreedyFullest::pick(&ctx.child_pending);
                let alternatives: Vec<AltCost> = ctx
                    .child_pending
                    .iter()
                    .enumerate()
                    .filter(|&(_, &pending)| pending > 0)
                    .map(|(child, _)| {
                        let mut forced = fixed.clone();
                        forced.push(child);
                        AltCost {
                            child,
                            window_cost: rollout_cost(&pre, ops, i, forced, cfg),
                        }
                    })
                    .collect();
                let chosen = pick_argmin(&alternatives, greedy_choice);
                records.push(LogRecord {
                    op_index: i as u64,
                    decision_index: counter + j as u64,
                    ctx,
                    alternatives,
                    chosen,
                    greedy_choice,
                });
                fixed.push(chosen);
            };
            let (accepted, ds) = accepted;
            t = accepted;
            counter += ds.len() as u64;
            script.extend(ds.iter().map(|&(_, c)| c));
            if (i as u64 + 1) % cfg.commit_interval == 0 {
                t.tree.simulate_commit();
            }
            checkpoint = t.tree.fork_for_sim(Box::new(GreedyFullest));
            checkpoint_at = i + 1;
        } else {
            counter += ds.len() as u64;
            script.extend(ds.iter().map(|&(_, c)| c));
            if (i as u64 + 1) % cfg.commit_interval == 0 {
                t.tree.simulate_commit();
            }
        }
    }
    Outcome { script, records }
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_cfg() -> OracleCfg {
        OracleCfg {
            commit_interval: 50,
            sample_rate: 4,
            window: 80,
            seed: 0x0AC1E,
        }
    }

    fn started(params: Params) -> BeTree {
        let mut tree = BeTree::new(params);
        tree.simulate_commit();
        tree
    }

    /// Two runs from one seed are byte-identical: same script, same
    /// costs, same serialized decision log.
    #[test]
    fn oracle_is_deterministic_under_seed() {
        let ops: Vec<WorkOp> = OpStream::new(Mix::YcsbA, KeyDist::Uniform, 64, 600, 7).collect();
        let cfg = tiny_cfg();
        let start = started(Params::default());
        let run = |start: &BeTree| {
            let outcome = improved_run(start, &ops, &cfg);
            let cost = scripted_cost(start, &ops, cfg.commit_interval, &outcome.script);
            let log: Vec<String> = outcome
                .records
                .iter()
                .map(|r| serde_json::to_string(r).unwrap())
                .collect();
            (outcome.script, cost, log)
        };
        let (script_a, cost_a, log_a) = run(&start);
        let (script_b, cost_b, log_b) = run(&start);
        assert_eq!(script_a, script_b, "decision scripts diverged");
        assert_eq!(cost_a, cost_b, "improved costs diverged");
        assert_eq!(log_a, log_b, "decision logs diverged");
        assert!(!log_a.is_empty(), "the tiny run must sample something");
    }

    /// sample=1 evaluates EVERY decision: full coverage, and the base
    /// cost is reproduced by the all-greedy script when the oracle never
    /// disagrees... which it may; so assert only the coverage identity
    /// and that the improved trajectory verifies.
    #[test]
    fn sample_rate_one_gives_full_coverage() {
        let ops: Vec<WorkOp> = OpStream::new(Mix::Load, KeyDist::Uniform, 64, 400, 9).collect();
        let cfg = OracleCfg {
            sample_rate: 1,
            ..tiny_cfg()
        };
        let start = started(Params::default());
        let outcome = improved_run(&start, &ops, &cfg);
        assert_eq!(
            outcome.records.len(),
            outcome.script.len(),
            "S=1 must evaluate every decision"
        );
        scripted_cost(&start, &ops, cfg.commit_interval, &outcome.script);
    }

    /// The decision-log schema round-trips through its JSONL form.
    #[test]
    fn decision_log_schema_round_trips() {
        let header = LogHeader {
            schema_version: 1,
            kind: "beetree-flush-decision-log".into(),
            seed: 42,
            workload: "ycsb-a".into(),
            n_ops: 50_000,
            keyspace: 20_000,
            commit_interval: 1000,
            sample_rate: 8,
            window: 1000,
            params: Params::default(),
            sampler: "doc".into(),
            cost_model: "doc".into(),
        };
        let line = serde_json::to_string(&header).unwrap();
        assert_eq!(serde_json::from_str::<LogHeader>(&line).unwrap(), header);

        let record = LogRecord {
            op_index: 17,
            decision_index: 5,
            ctx: FlushCtx {
                node: 9,
                depth: 1,
                child_pending: vec![1, 3, 0],
                child_pending_bytes: vec![37, 111, 0],
                child_dirty: vec![true, false, false],
                buffer_total: 4,
                ops_since_commit: 12,
            },
            alternatives: vec![
                AltCost {
                    child: 0,
                    window_cost: 4476,
                },
                AltCost {
                    child: 1,
                    window_cost: 4728,
                },
            ],
            chosen: 0,
            greedy_choice: 1,
        };
        let line = serde_json::to_string(&record).unwrap();
        assert_eq!(serde_json::from_str::<LogRecord>(&line).unwrap(), record);
    }

    /// The rollout window covers exactly the trigger op plus the next W
    /// ops, with commit boundaries at the (m+1)-multiples — pinned by
    /// counting clean commits: read ops dirty nothing, so with
    /// commit_interval=1 every in-window op commits exactly 4096 bytes
    /// (superblock only) and the terminal commit adds one more. An
    /// off-by-one in the window's far edge shifts the total by 4096.
    #[test]
    fn rollout_window_covers_exactly_the_trigger_plus_w_ops() {
        let start = started(Params::default());
        let n = 100u64;
        let w = 10u64;
        let ops: Vec<WorkOp> = (0..n).map(WorkOp::Read).collect();
        let cfg = OracleCfg {
            commit_interval: 1,
            sample_rate: 1,
            window: w,
            seed: 1,
        };
        let cost = rollout_cost(&start, &ops, 0, Vec::new(), &cfg);
        // Ops applied: indices 0..=W — the trigger plus W — each followed
        // by a 4096-byte clean commit, then the terminal commit.
        assert_eq!(cost, (w + 2) * 4096);
    }

    #[test]
    fn argmin_tie_breaks_prefer_greedy_then_lowest_index() {
        let alts = |costs: &[(usize, u64)]| -> Vec<AltCost> {
            costs
                .iter()
                .map(|&(child, window_cost)| AltCost { child, window_cost })
                .collect()
        };
        // Plain argmin.
        assert_eq!(pick_argmin(&alts(&[(0, 9), (1, 5), (2, 7)]), 0), 1);
        // Tie including greedy: keep greedy (no claimed disagreement).
        assert_eq!(pick_argmin(&alts(&[(0, 5), (1, 5), (2, 7)]), 1), 1);
        // Tie excluding greedy: lowest index.
        assert_eq!(pick_argmin(&alts(&[(0, 5), (1, 7), (2, 5)]), 1), 0);
    }

    /// The oracle's own oracle: a hand-constructed state where
    /// greedy-fullest is PROVABLY suboptimal — a cold child barely
    /// fullest (3 pending, clean) vs a dirty-spine child one message
    /// behind... here 1 vs 3, but the cold child's whole record price
    /// dwarfs the buffered-bytes difference. The oracle must pick the
    /// dirty child.
    ///
    /// Construction (F=4, B=3, L=8; 8-byte BE keys, 8-byte values):
    /// keys 0..=11 split the root leaf into X = {0..3} (4 entries) and
    /// Y = {4..11} (8 entries, pivot 4); drain + simulate_commit cleans
    /// everything. Updating keys 0,1,2,3 overflows the root buffer and
    /// flushes all four to X — X and the root are now dirty, Y is clean.
    /// Updates to 8, 9 (→Y) and 0 (→X) fill the buffer to B; the trigger
    /// update to 10 overflows it: occupancies [X: 1, Y: 3], dirty
    /// [true, false]. Greedy flushes Y.
    ///
    /// Exact terminal costs (records: 8-byte header + bincode payload;
    /// leaf payload = 12 + 40·entries; root payload = 60 + 44·buffered):
    ///   flush X: root(60+3·44=192→200) + X(172→180)            = 380 + 4096
    ///   flush Y: root(60+1·44=104→112) + X(180) + Y(332→340)   = 632 + 4096
    /// Flushing the dirty child saves exactly 252 bytes: Y's whole
    /// record (340) minus the two extra buffered puts left behind
    /// (2·44 = 88). The window is empty (no further ops), so the rollout
    /// scores are exactly these terminal commits.
    #[test]
    fn oracle_beats_greedy_on_the_dirty_spine_scenario() {
        let params = Params {
            fanout: 4,
            buffer_capacity: 3,
            leaf_capacity: 8,
        };
        let mut tree = BeTree::new(params);
        for k in 0..12u64 {
            apply(&mut tree, &WorkOp::Insert(k), 100 + k);
        }
        tree.drain();
        tree.simulate_commit();
        // Dirty X's spine: four updates, the fourth overflows and greedy
        // flushes the only legal child — X.
        for k in 0..4u64 {
            apply(&mut tree, &WorkOp::Insert(k), 200 + k);
        }
        // Pending state: 2 messages for Y, then 1 for X (buffer == B).
        for (i, k) in [8u64, 9, 0].into_iter().enumerate() {
            apply(&mut tree, &WorkOp::Insert(k), 300 + i as u64);
        }

        // The trigger op is the workload; the decision happens inside it.
        let ops = vec![WorkOp::Insert(10)];
        let cfg = OracleCfg {
            commit_interval: 1_000_000, // no boundary inside the window
            sample_rate: 1,             // evaluate the decision
            window: 10,
            seed: 99,
        };
        let outcome = improved_run(&tree, &ops, &cfg);
        assert_eq!(outcome.records.len(), 1, "exactly one decision expected");
        let record = &outcome.records[0];
        assert_eq!(record.ctx.child_pending, vec![1, 3], "construction drifted");
        assert_eq!(
            record.ctx.child_dirty,
            vec![true, false],
            "X must be dirty, Y clean"
        );
        assert_eq!(record.greedy_choice, 1, "greedy flushes the fullest (Y)");
        assert_eq!(
            record.chosen, 0,
            "the oracle must flush the dirty-spine child X"
        );
        let cost = |child: usize| {
            record
                .alternatives
                .iter()
                .find(|a| a.child == child)
                .expect("both children are legal")
                .window_cost
        };
        assert!(
            cost(0) < cost(1),
            "flushing the dirty child must be measurably cheaper ({} vs {})",
            cost(0),
            cost(1)
        );
        assert_eq!(
            cost(1) - cost(0),
            252,
            "the saving is exactly Y's record (340) minus the two extra \
             buffered puts left in the root (88)"
        );
        // The ABSOLUTE window costs, pinned to the hand-derived terminal
        // commits above. The difference alone is not enough: a uniform
        // shift — e.g. a misphased commit boundary inside the rollout
        // charging one spurious commit to every alternative — cancels in
        // the difference but not here.
        assert_eq!(cost(0), 380 + 4096, "flush-X window cost drifted");
        assert_eq!(cost(1), 632 + 4096, "flush-Y window cost drifted");
        // And the improved trajectory's total reflects the saving against
        // the pure greedy baseline on identical ops (both runs end in the
        // same trailing commit, whose superblock constant cancels).
        let (base, _) = greedy_cost(&tree, &ops, cfg.commit_interval);
        let improved = scripted_cost(&tree, &ops, cfg.commit_interval, &outcome.script);
        assert_eq!(base - improved, 252, "the end-to-end gap is the saving");
    }
}
