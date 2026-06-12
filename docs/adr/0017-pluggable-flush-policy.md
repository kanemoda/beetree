# ADR-0017: Pluggable flush policy; the cost model is the commit, simulated

Status: accepted (M4.1)

The greedy-fullest child choice (`docs/SPEC.md`, "Baseline flush policy")
moves behind a `FlushPolicy` trait consulted at every overflow-driven
flush decision; engines take a `Box<dyn FlushPolicy>` at construction and
default to `GreedyFullest`, the extracted normative rule. The refactor is
gated on byte-identity, not intent: `tests/policy_regression.rs` replays
a fixed-seed full-algebra workload and asserts trace and database-file
hashes captured on the pre-refactor tree. A policy must choose a child
with pending messages; the engines PANIC on any other choice, because a
silently-tolerated empty flush would livelock the cascade (the buffer
never shrinks) and a silently-clamped one would corrupt every recorded
decision stream. `drain()` never consults the policy: forced drain
flushes are outside the performance model (SPEC "Observability") and a
buggy experimental policy must not be able to break a measurement
utility.

The decision context (`FlushCtx`) deliberately includes per-child DIRTY
flags and `ops_since_commit`: under the no-WAL CoW commit (ADR-0007),
flush cost depends on position in the commit window — writing into an
already-dirty subtree is free at the next commit (the "dirty-spine
discount") — and a decision log blind to that could not train or even
diagnose the policies M4 studies. For the same reason `BeTree` now
carries a SIMULATED dirty set, marked at exactly the sites where
`DiskEngine` marks slots dirty, and `simulate_commit()` prices it with
the REAL record encoder: Σ over dirty nodes (header + bincode payload)
+ one superblock slot. The analytic total is calibrated byte-exact
against `CountingVfs` write bytes (`tests/cost_model.rs`); the M4.1
falsification (`docs/analysis/FALSIFICATION.md`) is meaningful only
because the quantity the oracle optimizes IS the quantity the disk
engine pays.

`fork_for_sim` copies the reachable tree only, with compacted ids: the
rollout oracle forks at every evaluated alternative, and the in-memory
arena leaks unlinked nodes by design (ADR-0004) — a full-arena clone
would make forking cost proportional to history rather than to the live
tree. Node ids were never stable identifiers (docs/findings.md), so
compaction changes nothing observable.

The oracle's JSONL decision log opens with a versioned header
(`schema_version: 1`, SPEC "Observability"). The log is the designated
training-data format for any future learned policy; field additions bump
the version so two formats never silently mix in one corpus.
