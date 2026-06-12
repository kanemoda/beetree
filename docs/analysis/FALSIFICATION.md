# Phase-2 falsification: is there headroom above greedy-fullest?

M4.1. The question this document answers with numbers: **how much write
cost does the normative greedy-fullest flush policy leave on the table?**
If the answer is "almost none," the learned-flush-policy program dies
here, before any model is trained. No machine learning appears in this
phase; the deliverable is the gap table below and the verdict the
pre-registered rule forces.

## The honest frame, stated up front

The measurement is a **hindsight rollout oracle** (`src/bin/oracle.rs`):
one policy-improvement step over the baseline, scored by a calibrated
analytic cost model. Three structural caveats bound what it can prove:

1. **The oracle LOWER-BOUNDS true headroom.** Its action space is child
   choice at the baseline's own trigger points (buffer overflow, B+1
   messages). Flush *timing* freedom — flushing early, holding past the
   trigger, batching across boundaries — is not explored and is
   documented future work. A large gap proves headroom exists; a small
   gap is evidence against, **not proof**.
2. **One improvement step, finite horizon.** Sampled decisions are
   optimized against a W-op greedy rollout, not against the optimal
   continuation; unsampled decisions stay greedy. Both truncations bias
   the measured gap toward zero.
3. **The cost model is write bytes under the M1 commit protocol** (CoW
   records + superblock per commit). Read costs, cache effects, and
   wall-clock are out of scope — this phase falsifies the WRITE-side
   claim only, because that is the claim the dirty-spine mechanism makes.

## Method

- **Engine state**: `BeTree` with the M4.1 `FlushPolicy` plumbing. The
  refactor is byte-identical under the default policy
  (`tests/policy_regression.rs` pins pre-refactor trace and file
  hashes).
- **Cost model**: `BeTree::simulate_commit()` — Σ over dirty nodes
  (8-byte record header + real bincode payload length) + 4096 bytes of
  superblock slot per commit, with dirty flags mirroring `DiskEngine`'s
  marking sites one for one. **Calibration**: on identical workloads and
  commit cadence, the analytic total equals `CountingVfs` physical write
  bytes **exactly — 0.000000% error on all four calibration workloads**
  (`tests/cost_model.rs`; the contract is ≤ 2%, the tests additionally
  pin observed equality). Scope of that calibration (M4.1 review): both
  sides price record payloads with the SAME encoder, so payload
  byte-exactness is shared-by-construction; the calibration's
  falsifiable content is the dirty-set and tree-evolution mirror —
  independent implementations on the two engines — counted at the VFS
  boundary (`CountingVfs`), never via the engine's own analytic
  `CommitStats`. Payload pricing is independently pinned by the
  `entry_bytes` property test (`src/policy.rs`) and the hand-derived
  absolute window costs in the oracle's dirty-spine test. Costs below
  exclude the constant create-time
  generation-0 commit: every reported number is the cost of the workload
  alone.
- **Baseline run**: pure greedy-fullest over the N workload ops,
  simulated commits every K ops (boundaries count ALL ops, the bench
  convention) → `base_cost`.
- **Oracle run**: same ops. Decisions are counted globally along the
  improved trajectory; decision g is sampled iff
  `splitmix64(seed ^ g·0x9E3779B97F4A7C15) % S == 0` (expected coverage
  1/S, seeded, no aliasing against cascade periodicity). At a sampled
  decision, every legal child (pending > 0) is evaluated: fork the tree
  (`fork_for_sim`), force that child, finish the op, roll out the next W
  ops under greedy, score = simulated commits at the run's global
  boundaries within the window + a terminal commit charging the dirty
  residue (its superblock constant cancels across alternatives). Argmin
  wins; **ties prefer the greedy choice** (no claimed disagreement
  without a measured saving), then the lowest index. Unsampled decisions
  stay greedy.
- **Accounting**: the improved trajectory's full decision script is
  replayed from scratch (`scripted_cost`) — one clean pass that both
  totals `improved_cost` and proves the trajectory reproducible (the
  script must be consumed exactly).
- **Gap**: `(base_cost − improved_cost) / base_cost`. A negative gap is
  possible in principle (window myopia) and would be reported as
  measured.
- **Determinism**: everything flows from one u64 seed; two runs produce
  identical gaps and byte-identical decision logs
  (`oracle_is_deterministic_under_seed`).
- **The oracle's own oracle**: a hand-constructed dirty-spine scenario
  where greedy is provably suboptimal by exactly 252 bytes — the cold
  child's whole record minus the buffered bytes left behind — and the
  oracle must take the cheaper action
  (`oracle_beats_greedy_on_the_dirty_spine_scenario`).
- **Decision log**: every sampled decision's `FlushCtx` +
  per-alternative window costs + both choices, as versioned JSONL
  (`schema_version: 1`; SPEC "Observability"). The logs under
  `docs/analysis/data/` are the raw material for the mechanism reading
  below and the training-data format for any M4.2.

## Grid

| dimension | values |
|---|---|
| workloads | uniform-load, zipfian-load, ycsb-a, ycsb-a-zipfian, upsert-heavy, ascending |
| commit interval K | 100, 1000 |
| ops N | 50,000 over a 20,000-key space (8-byte BE keys, 8-byte values) |
| params | F=4, B=8, L=8 (defaults), plus one row F=16/B=64 (L=8) for ycsb-a-zipfian at K=1000 |
| oracle budget | S=8 (sample 1 in 8 decisions), W=1000 (rollout horizon) |
| seed | 48879 |

The F=16/B=64 row asks whether the headroom survives realistic
capacities; K=1000 is the M3.2-standard cadence (E3/E4). Achieved
coverage (`decisions_sampled / decisions_total`) and wall time are
reported per cell.

## Pre-registered decision rule

Recorded before any grid cell was run:

> If the **maximum gap across the whole grid is below 10%**, the
> learned-flush-policy idea is **dead**: child choice at fixed trigger
> points does not carry enough recoverable write cost to justify a
> learning stack, and the project pivots (ZNS direction). If **any cell
> reaches 10% or more**, M4.2 gets a green light, and this document
> names the workloads that carry the headroom.

No reading of the results section may soften this rule after the fact.
The rule binds the child-choice question only; the flush-timing question
(caveat 1) stays open either way and would need its own falsification.

## Results

The registered grid (S=8, W=1000, N=50,000, seed 48879; raw CSVs and the
full decision logs in `docs/analysis/data/` — logs committed
gzip-compressed, ~50 MB raw; the oracle writes plain `.jsonl`). Costs in
bytes; gap =
(base − improved)/base; coverage = decisions_sampled/decisions_total;
wall is per cell, cells ran concurrently on 12 cores (Ryzen 5 3600, the
M3.2 bench machine).

| workload | K | base_cost | improved_cost | gap | decisions | sampled (cov.) | wall |
|---|---|---|---|---|---|---|---|
| uniform-load | 100 | 16,349,204 | 16,192,820 | 0.96% | 53,731 | 6,663 (12.4%) | 223.6s |
| uniform-load | 1000 | 8,815,424 | 8,608,300 | 2.35% | 54,146 | 6,717 (12.4%) | 221.9s |
| zipfian-load | 100 | 10,668,136 | 10,349,428 | 2.99% | 35,482 | 4,433 (12.5%) | 93.9s |
| zipfian-load | 1000 | 4,483,012 | 4,347,392 | 3.03% | 35,887 | 4,491 (12.5%) | 84.8s |
| ycsb-a | 100 | 9,520,940 | 9,432,108 | 0.93% | 24,944 | 3,054 (12.2%) | 86.8s |
| ycsb-a | 1000 | 4,847,632 | 4,773,664 | 1.53% | 25,086 | 3,077 (12.3%) | 35.5s |
| ycsb-a-zipfian | 100 | 6,818,220 | 6,708,296 | 1.61% | 16,884 | 2,039 (12.1%) | 25.8s |
| ycsb-a-zipfian | 1000 | 2,693,460 | 2,637,032 | 2.10% | 17,116 | 2,071 (12.1%) | 25.2s |
| upsert-heavy | 100 | 14,342,900 | 14,097,276 | 1.71% | 47,833 | 5,969 (12.5%) | 185.5s |
| upsert-heavy | 1000 | 7,683,748 | 7,546,884 | 1.78% | 48,256 | 6,002 (12.4%) | 182.4s |
| ascending | 100 | 5,451,820 | 5,376,584 | 1.38% | 51,787 | 6,424 (12.4%) | 173.9s |
| ascending | 1000 | 2,964,628 | 2,959,544 | 0.17% | 50,919 | 6,332 (12.4%) | 167.9s |
| ycsb-a-zipfian, F=16/B=64 | 1000 | 3,617,320 | 3,498,100 | **3.30%** | 2,152 | 256 (11.9%) | 5.9s |

Registered-grid maximum: **3.30%**, on the F=16/B=64 row. Total oracle
CPU time for the grid: 1,513s (≈8 minutes wall, parallel). Every cell
verified its improved trajectory by exact scripted replay; all runs are
seed-deterministic.

### Supplementary sensitivity runs (not part of the registered grid)

Registered before reading these: the S=8 grid only ever improves ~1/8 of
decisions — unsampled decisions stay greedy by construction — so the
grid's gap is a budget-truncated lower bound on the one-step-improved
policy's gap. The same oracle at S=1 (every decision evaluated) and at
varied W measures how much the budget truncates
(`docs/analysis/data/s1/`, `docs/analysis/data/sensitivity/`):

| cell | S | W | gap | (registered S=8 gap) |
|---|---|---|---|---|
| ycsb-a-zipfian, F=16/B=64, K=1000 | 1 | 1000 | **13.47%** | 3.30% |
| ycsb-a-zipfian, K=1000 | 1 | 1000 | 5.88% | 2.10% |
| zipfian-load, K=1000 | 1 | 1000 | 7.15% | 3.03% |
| ycsb-a, K=1000 | 1 | 1000 | 3.69% | 1.53% |
| zipfian-load, K=1000 | 8 | 2000 | 3.47% | 3.03% |
| zipfian-load, K=1000 | 8 | 200 | 1.87% | 3.03% |

Full coverage lifts every measured gap by 2.4–4.1×, and a longer
horizon lifts it further (W 200 → 1000 → 2000 gives 1.87% → 3.03% →
3.47% on zipfian-load) — both confirm the registered grid measures a
truncated lower bound, exactly as the method section warned. At the
deliberately tiny default capacities (F=4/B=8) the fully-covered gap
stays in single digits on the three rows rerun at S=1; uniform-load
(2.35% at S=8, the second-highest default cell) and upsert-heavy
(1.78%) were NOT rerun at full coverage, so that bound holds only for
the measured rows. At the one realistic-capacity row (F=16/B=64) the
fully-covered gap reaches **13.47%**.

## Mechanism reading

From the decision logs (aggregations over every sampled decision; the
JSONL files reproduce all of these numbers). The oracle disagrees with
greedy-fullest on 33–37% of sampled decisions on every mixed or skewed
workload at default params, on 77% at F=16/B=64, and on only 0.6–2.5%
on ascending — sorted insertion leaves greedy nearly nothing to fix. The
dominant disagreement is NOT the dirty-spine pattern: in ~90% of
disagreements the oracle picks a child with FEWER pending bytes than
greedy's choice (mean deficit ≈ 2.4–2.9 messages), i.e. it flushes
SMALLER batches — spending the minimum to clear the overflow and keeping
the hot, fullest ranges buffered upstairs where future updates coalesce
for free instead of materializing into child records that the very next
overflow re-dirties. (Per cell the fewer-bytes share is 89–92%; the
residual ~10% are exact byte-ties — the oracle never picks the LARGER
batch. Message sizes are constant within each workload here, so byte-
and count-economics are indistinguishable in this grid; separating them
needs variable-size values.) The pure dirty-spine signature (oracle's
child dirty, greedy's clean) accounts for only 10–14% of disagreements
at default params (26% at F=16/B=64, 32–36% on ascending); at K=1000
roughly half of disagreements have BOTH candidates dirty (40–54% across
the default-param cells; 27% at F=16/B=64), so dirtiness cannot be the
discriminator there. Disagreements spread across all depths with a
mid-tree peak (mild by disagreement count; by RATE the mid-tree peak
runs 2.6–2.9× the root's, and ascending has no mid peak at all), and —
against the expectation that motivated adding
`ops_since_commit` to the context — the disagreement rate is essentially
FLAT across the commit window (e.g. uniform-load K=100: 34.2/34.0/36.4/
35.5% by quartile; note `ops_since_commit` counts MUTATING ops while
boundaries count all ops, so on read-mixed workloads the observed
positions span only the lower part of the K-op window): position in the
window carries little signal at
these settings, while batch-byte economics carry most of it. Per
disagreement the oracle saves ≈ 250–730 bytes of window cost (≈ 1,600 at
F=16/B=64, where records are bigger and each decision moves more);
summed window savings overstate the realized end-to-end gap by large,
cell-dependent factors — 2.4–9.5× on the mixed and skewed workloads
(median ≈ 6×), while ascending-K100 even UNDERstates (0.63×) — because
windows overlap and later greedy decisions re-absorb part of each
saving; that is why the trajectory-level accounting pass, not the sum
of window deltas, is the reported number.

The full-coverage (S=1) logs add one dynamic the registered grid cannot
see: on a trajectory that is being improved at EVERY decision, the
disagreement rate drops — ycsb-a 20.1% vs 36.6%, F=16/B=64 56.9% vs
76.6% — because the oracle's earlier choices leave later decision points
in states greedy handles better. Disagreement also rises with depth at
full coverage (e.g. zipfian-load: 3.2% at the root to ~33% mid-tree;
F=16/B=64: 43.7% → 70.4%): the root's fullest child is usually the
right flush target, lower levels are where count-greediness and byte
economics diverge. Both facts matter for M4.2: a learned policy shifts
the state distribution it then faces, and its inputs must include
depth.

## Interim verdict

Two findings, kept separate because the rule was registered before the
runs:

1. **On the registered grid (S=8), the kill threshold was not reached
   anywhere**: maximum gap 3.30%, on the F=16/B=64 row. Read against the
   pre-registered rule's letter, that is a kill.
2. **The registered instrument is now measured to truncate.** The S=8
   oracle improves only ~1/8 of decisions by construction; the
   supplementary runs — the same oracle, same workload rows, same seed,
   with the sampling budget removed — realize 3–4× the gap everywhere,
   and the F=16/B=64 cell measures **13.47%**, a verified-by-replay,
   seed-deterministic trajectory cost reduction ≥ the 10% threshold.

By the registered rule's letter, finding 1 is the experiment's result:
**the registered grid is a kill.** The supplementary runs cannot
retroactively satisfy that rule — they were observed outside the
registered grid, and data observed before a rule exists cannot satisfy
the rule; the registration's own anti-softening clause binds this
document. What finding 2 DOES establish is an instrument diagnosis: the
S=8 budget truncates the measurement by construction (the method
section warned of exactly this before any run), full coverage lifts
every measured gap by 2.4–4.1×, and the one realistic-capacity cell
reaches 13.47% — verified by replay, seed-deterministic. A diagnosed
instrument is grounds to re-register and measure again, not to declare
victory on the old registration.

The interim verdict is therefore: **CONDITIONAL green light for M4.2,
pending the confirmation experiment registered below.** The diagnosis
names the likely carriers — skewed and mixed workloads (zipfian-load
7.15% at full coverage, ycsb-a-zipfian 5.88%, ycsb-a 3.69%), amplified
~2× by realistic node capacities (13.47% at F=16/B=64, with greedy
contradicted on 57% of ALL decisions at full coverage) — and the
confirmation experiment tests exactly that claim, out of sample. Known
bounds either way: at the deliberately tiny normative test parameters
(F=4/B=8) no MEASURED full-coverage cell exceeds 7.2% — S=1 was run on
three of the twelve default rows; uniform-load and upsert-heavy remain
unmeasured at full coverage — and ascending workloads carry essentially
nothing (0.17–1.38% at S=8; greedy is already near-optimal under sorted
insertion). Any M4.2 must therefore (a) train and evaluate at realistic
capacities, not at the test params; (b) target the measured mechanism —
batch-byte economics and coalescing preservation ("flush small and
cold, keep hot ranges buffered"), with the dirty-spine discount as a
secondary feature — rather than the window-position feature, which the
logs show is nearly flat; and (c) beat the honest yardstick this phase
fixes: the full-coverage oracle gap, not the S=8 one.

## Confirmation experiment (re-registered)

Recorded before any of its cells were run. The interim verdict above is
conditional on this experiment; its rule is final.

- **Grid**: {ycsb-a-zipfian, zipfian-load, ycsb-a} × {F=16/B=64,
  F=32/B=256} (L=8 throughout) — six cells; K=1000, N=50,000 over the
  standard 20,000-key space, S=1 (full coverage), W=1000.
- **Seed**: **271828** — a FRESH seed, deliberately not 48879.
  Rationale: a new seed draws a new workload instance from the same
  distribution, making the confirmation out-of-sample relative to every
  number observed so far; no run recorded in this document has seen
  seed-271828 data.
- **Rule**: if the gap is **≥ 10% in AT LEAST 2 of the 6 cells**, M4.2
  gets its final green light. Otherwise the learned-flush-policy idea
  is **killed**, and the project pivots (ZNS direction). No softening
  after the fact.
- **Note**: the F=32/B=256 column also answers whether the capacity
  amplification keeps growing or saturates — informative for M4.2's
  training parameters either way.

Raw CSVs and decision logs: `docs/analysis/data/confirmation/`.

## Confirmation results

Run after the section above was recorded. Seed 271828, S=1, W=1000,
K=1000, N=50,000; raw CSVs and gzipped decision logs in
`docs/analysis/data/confirmation/`; all six cells seed-deterministic
and verified by exact scripted replay as before; walls measured with
all six cells running concurrently.

| workload | params | base_cost | improved_cost | gap | decisions (S=1) | wall |
|---|---|---|---|---|---|---|
| ycsb-a-zipfian | F=16/B=64 | 3,718,996 | 3,156,504 | **15.12%** | 2,571 | 30.7s |
| ycsb-a-zipfian | F=32/B=256 | 4,509,696 | 3,794,312 | **15.86%** | 716 | 15.3s |
| zipfian-load | F=16/B=64 | 6,294,244 | 5,419,312 | **13.90%** | 5,541 | 108.4s |
| zipfian-load | F=32/B=256 | 9,349,632 | 8,351,400 | **10.68%** | 1,801 | 93.9s |
| ycsb-a | F=16/B=64 | 6,492,212 | 5,942,452 | 8.47% | 5,097 | 144.0s |
| ycsb-a | F=32/B=256 | 8,569,992 | 7,142,068 | **16.66%** | 1,225 | 55.3s |

**Five of six cells meet the ≥ 10% threshold; the rule required two.
FINAL GREEN LIGHT for M4.2.** The out-of-sample run confirms the
diagnosis: the headroom is real at realistic capacities on skewed and
mixed workloads, and it is not a property of the original seed. On the
capacity question, the F=32/B=256 column is informative but NOT
monotone: the gap keeps growing for ycsb-a (8.47% → 16.66%) and
ycsb-a-zipfian (15.12% → 15.86%) but recedes for zipfian-load (13.90%
→ 10.68%) — capacity amplification is workload-dependent, so M4.2 must
treat (F, B) as part of its evaluation grid rather than assume headroom
grows with capacity. The one sub-threshold cell (ycsb-a at F=16/B=64,
8.47%) is consistent with the interim diagnosis that ycsb-a is the
weakest of the three carriers. Oracle-vs-greedy disagreement rates on
the confirmation trajectories: 48.7–78.8% of all decisions.

The flush-TIMING question (caveat 1 of the framing) remains open and
unmeasured; it is the other place headroom could hide, and it needs its
own falsification before anyone builds on it.
