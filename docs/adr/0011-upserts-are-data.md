# ADR-0011: Upserts are data, never code

Status: accepted (M2.1)

`UpsertOp` is a closed enum of pure data — `Add(i64)` for now — applied by
total, deterministic, engine-owned semantics (base 0 for anything that is
not exactly 8 bytes; wrapping arithmetic, never panicking). RocksDB-style
user merge operators (callbacks) are deliberately rejected: a trace must
replay to an identical engine on any machine at any time (P4/Q4), and a
closure in a trace is unserializable, unversionable, and undiffable. New
blind-update kinds become new `UpsertOp` variants with spec'd semantics —
data in the trace, code in the engine, exactly one implementation of each.
