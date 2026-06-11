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
| M2.2 | Range scans; freezing the full-mix harness (`tests/harness2.rs`) | planned |

## Build & test

```sh
cargo test                                      # unit tests + property harness
cargo clippy --all-targets -- -D warnings       # must stay clean
cargo fmt --check
```

The test harness (`tests/harness.rs`) is generic over the `KvEngine` trait
and deliberately uses tiny structure parameters (F=4, B=8, L=8) to force deep
trees and frequent structural operations.
