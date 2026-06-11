# ADR-0013: A parallel v2 trace vocabulary

Status: accepted (M2.1)

The byte-frozen M0 harness (`tests/harness.rs`) matches EXHAUSTIVELY over
`TraceEvent` (its P5 filter) and `OpKind` (its P5 payload check), with no
wildcard arms. Adding enum variants would therefore break the frozen file
at compile time — Rust exhaustiveness is the freeze, extended from bytes
to types. Rather than touch the harness, the M2.1 vocabulary lives in
parallel types: `OpKind2`/`TraceEvent2`, recorded by every engine alongside
the v1 view through one shared `Recorder`.

Consequences: `trace()` keeps its frozen signature and returns the v1
view, which is FAITHFUL exactly for insert-only workloads (all the frozen
harness ever generates) and silently omits deletes and upserts otherwise —
documented on the trait; `trace2()` is the complete record and `replay2`
the only mixed-workload-faithful replay. The v1 vocabulary is closed for
good; when the v2 harness freezes (M2.2), `TraceEvent2`/`OpKind2` close
the same way, and any v3 vocabulary repeats this pattern.
