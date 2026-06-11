# beetree

A readable, honestly-benchmarked reference implementation of a Bε-tree
storage engine. The point is to be read: semantics live in an executable
specification (`docs/SPEC.md` plus a generic property-test harness that every
engine must pass unchanged), structural invariants are checked exhaustively
under test, and design decisions are recorded as short ADRs in `docs/adr/`.
There are no performance claims here, and there won't be any until there are
honest benchmarks to back them.

## Status

| Milestone | Scope | Status |
|-----------|-------|--------|
| M0.1 | Project skeleton, SPEC, generic property-test harness, naive reference engine | done |
| M0.2 | In-memory Bε-tree that passes the M0.1 harness unchanged | done |
| M1.1 | Persistence: copy-on-write nodes in a single file, dual-superblock atomic commit, recovery — no WAL by design (ADR-0007..0009) | done |
| M1.2 | Crash injection: `FaultyVfs` crash images at arbitrary points (ADR-0010), the A1–A5 recovery assertions, mutation-tested harness | done |
| M2.1 | Full message algebra — deletes (tombstones annihilate at leaves) and upserts (data, not code; ADR-0011) — plus Reclamation v1 and on-disk format v2 | done |
| M2.2 | Range scans (bottom-up application; collect semantics, ADR-0014), the blind-increment showcase, harness2 freeze | done |
| M3 | Memory budgets: cache eviction, bounded scans | planned |

## Frozen harnesses

Both property harnesses are byte-frozen; every engine must pass them
unchanged, and every milestone report records their hashes.

| File | Frozen since | sha256 |
|------|--------------|--------|
| `tests/harness.rs` (P1–P5, insert-only) | M0.2 | `ff4b837e1df664de1b31a5768e2ad64e23263cefae6f7c396d128d41bf4f8de9` |
| `tests/harness2.rs` (Q1–Q6, full op mix) | M2.2 | `3e8391d4aafea597b77d67e3fbb72cb0c079387edf5f976423c9e00958c99eda` |

## Blind-increment showcase

1,000 counters, 100,000 random `Add(1..=10)` ops, commit every 1,000, on
`DiskEngine` (release build; `cargo run --release --example counter`; both
arms verified exactly against an oracle by one full scan):

| arm | wall | ops/sec | nodes written | bytes written | file size |
|-----|------|---------|---------------|---------------|-----------|
| blind upsert | 0.28s | 362,276 | 19,262 | 5,249,684 | 4,848,296 |
| read-modify-write | 0.38s | 266,359 | 19,262 | 5,448,208 | 5,046,820 |

cache is unbounded in this build; the read-path gap widens under memory
pressure (M3).

## Build & test

```sh
cargo test                                      # unit tests + property harness
cargo clippy --all-targets -- -D warnings       # must stay clean
cargo fmt --check
```

The test harness (`tests/harness.rs`) is generic over the `KvEngine` trait
and deliberately uses tiny structure parameters (F=4, B=8, L=8) to force deep
trees and frequent structural operations.
