# beetree specification — M0 + M1 + M2 + M3.1

This document is normative for milestones M0 (in-memory engine), M1
(persistence and crash safety), M2 (deletes, upserts, reclamation, range
scans), and M3.1 (the bounded cache and observability).
The byte-frozen property-test harness in `tests/harness.rs` enforces the
insert-only semantics mechanically; the full-mix harness in
`tests/harness2.rs` (frozen when M2.2 ships) enforces the complete op
vocabulary; the invariant checker enforces I1–I7 structurally. Public API
or semantics changes must update this file in the same change (see
`CLAUDE.md`).

## Public API (M0 scope)

- `insert(&mut self, key: Key, value: Value)`
- `get(&mut self, key: &[u8]) -> Option<Value>`
- `check_invariants(&self) -> Result<(), InvariantViolation>`
- Every public mutating op is recorded as a trace event with a global seqno.
- Reads are traced as Get events without seqnos; only mutating ops consume
  seqnos. (`get` takes `&mut self` because recording mutates the trace;
  ADR-0006.)
- The concrete `BeTree` additionally exposes non-normative observability
  helpers — `height()` and `node_count()` — used by structural tests and
  reporting; they are not part of the `KvEngine` contract.

Key = `Vec<u8>`, Value = `Vec<u8>`.

## Semantics

- Last-writer-wins, ordered by a global monotonically increasing seqno (u64),
  assigned per public mutating op (insert, delete, upsert).
- Mutations become Messages: `Put { seq, value }`, `Delete { seq }`
  (a tombstone while in transit), or `Upsert { seq, op }` (M2.1). Messages
  live in internal-node buffers and migrate downward via flushes; leaves
  store materialized entries only — `Put` sets the entry, `Delete` REMOVES
  it outright, `Upsert` materializes per the upsert semantics below.
  Leaves never store tombstones: the leaf is the authoritative bottom;
  nothing exists below it.

### Upsert semantics (M2.1, normative)

Upserts are DATA, never code: `UpsertOp::Add(i64)` is the only operation
(ADR-0011) — trace/replay determinism forbids user closures. Add is total:
base = if the existing value is exactly 8 bytes, i64::from_le_bytes(it),
else 0 (including absent and deleted). Result is always the 8-byte LE
encoding of base.wrapping_add(delta). Wrapping, never panicking.

### Coalescing (M2.1, normative)

A buffer holds at most one effective message per key (I4); an arriving
message coalesces with the resident one per this table
("newer ∘ older = result"):

    Put(v)    ∘ anything      = Put(v)
    Delete    ∘ anything      = Delete
    Upsert(d) ∘ Put(v)        = Put(apply(v, d))      [folds immediately]
    Upsert(d) ∘ Delete        = Put(encode(d))        [base 0; folds]
    Upsert(d) ∘ Upsert(d_old) = Upsert(Add(d_old.0.wrapping_add(d.0)))

The result always carries the NEWER seq.

### Reads (M2.1, replaces "topmost hit wins")

`get` walks root→leaf accumulating a pending Upsert chain (a running
wrapping i64 sum suffices for Add; by I3 everything higher is newer).
Terminal cases: a buffered Put (apply the chain to its value), a buffered
Delete (apply the chain to base 0 if the chain is non-empty, else None), a
leaf entry (apply the chain), or leaf-absent (chain non-empty → the value
of the chain from base 0; chain empty → None). An empty chain returns the
terminal value untouched.

### Range scans (M2.2, normative)

`scan(&mut self, lo: Bound<Vec<u8>>, hi: Bound<Vec<u8>>) ->
Result<Vec<(Key, Value)>, EngineError>` joins `KvEngine`: every key in
the range with its resolved value, ascending. Collect semantics — a
streaming cursor is explicitly deferred (ADR-0014). An inverted range is
the empty result, never a panic.

Algorithm ("bottom-up application"): recurse top-down clipping the range
per child, but APPLY upward —

- leaf: emit entries within range (terminal resolutions);
- internal: union the children's (disjoint, ordered) results, then apply
  this node's in-range buffered messages onto that result: Put(v) → set;
  Delete → remove; Upsert(d) → transform (absent ⇒ base 0).

Correctness: I3 guarantees every message at a node is strictly newer than
anything produced below it, so plain overwrite/transform in depth order
IS seq order. The implementations use explicit frames, not machine
recursion (degenerate F=2 trees are linearly tall).

Semantic consequences, all property-tested (Q6): keys whose resolution is
Delete do NOT appear; keys that only ever received Upserts DO appear
(value folded from base 0) — scan agrees with `get` on every key; and
tombstones and upserts still in transit (resting in buffers) are
correctly folded.

Scans are traced as seqno-free `TraceEvent2::Scan { lo, hi }` events,
ignored by `replay2` and absent from the closed v1 vocabulary; Q5 is
unchanged (scans are not ops).

## Structure parameters (Params)

- F: max children per internal node (default test value 4)
- B: max messages per internal buffer (default test value 8)
- L: max entries per leaf (default test value 8)

Capacities are COUNT-based in M0 (ADR-0001); byte-based sizing comes with the
disk layer.

Legal ranges: F ≥ 2, B ≥ 1, L ≥ 1. With F < 2, invariants I2 and I5 are
jointly unsatisfiable for any internal node; engines may panic on parameters
outside these ranges.

F = 2 is legal but degenerate: every internal split of a 3-child node must
produce a single-child piece, and sorted insertion drives tree height to
Θ(n) and node count to Θ(n²). This is inherent to split-only insertion at
F = 2 (merges are delete-triggered; ADR-0002); a repair mechanism, if any,
will be decided in M2. Engines must SURVIVE such trees — in particular, no
operation may recurse proportionally to tree height — but performance under
F = 2 is explicitly not promised. Reclamation v1 (M2.1) adds fanout-1
internal nodes to the same degeneracy class: a parent reduced to a single
child by leaf reclamation persists, and full occupancy-based rebalancing
is explicitly out of scope for v1.

## Tree structure (M0.2)

### Initial tree state

A new engine is a single empty Leaf as root.

### Pivot convention (normative)

An internal node with pivots p1 < p2 < ... < pk has k+1 children; child i
owns keys in [p_{i-1}, p_i) with p_0 = -inf, p_{k+1} = +inf. A pivot
equals the smallest key of the subtree to its right AT THE MOMENT OF THE
SPLIT THAT PROMOTED IT. I1 is checked against this convention.

Consequences: a key EQUAL to a pivot routes to the child on the pivot's
right, and every pivot was a real key of the tree when promoted (a leaf
split promotes the smallest key of its right piece; an internal split
promotes an existing pivot, which moves up and is kept in neither half).
Amended in M2.1: deletes can remove the key while the pivot persists, and
reclamation's range absorption can detach a pivot from its subtree's
minimum — pivots are pure half-open-range separators thereafter, which is
all that routing and I1 ever rely on.

## Baseline flush policy (greedy-fullest, normative)

While an internal node's buffer exceeds B:

1. Compute per-child pending counts (messages routed by pivot ranges).
   Choose the child with the most; tie-break lowest index. Emit
   `TraceEvent::FlushDecision { node, child_occupancies, chosen }` — every
   decision, every time.
2. Remove ALL messages destined for the chosen child from the buffer.
3. Deliver: if the child is a Leaf, apply to entries; if Internal, coalesce
   into the child's buffer, then if the child's buffer now exceeds B, flush
   the child recursively.
4. Integrate any split pairs from the child into this node's
   pivots/children immediately, before re-evaluating (child indices shift —
   routing is never cached across iterations).

After the loop, the node itself splits if its fanout exceeds F. This named
policy is the baseline that later milestones measure alternative flush
policies against.

## Reclamation v1 (M2.1, normative)

- A flush delivery that leaves a leaf EMPTY signals removal upward: the
  child-delivery result is either promoted splits or `Removed` — a single
  delivery cannot both split and empty.
- The parent of a removed child drops the child and its adjacent pivot
  (the left pivot if one exists, else the right one); the neighbor absorbs
  the emptied key range.
- A parent reduced to a single child PERSISTS: fanout-1 internals are
  legal (the same degeneracy class as F=2; see "Structure parameters").
  Full occupancy-based rebalancing is explicitly out of scope for v1.
- An internal node that loses EVERY child (its buffer is necessarily
  drained by then) is itself `Removed`; if that node is the root, the tree
  is the empty tree again — a fresh empty root leaf, the initial state.
- Root collapse: after each public mutating op settles, while the root is
  Internal with exactly one child AND an empty buffer, the child is
  promoted to root. A non-empty root buffer is NOT force-flushed: a stale
  tall root is harmless and collapses on a later op once its buffer
  drains. A root leaf that empties simply IS the empty tree.
- Removed nodes leak in the in-memory arena by design (ADR-0004
  unchanged); on disk they are unreferenced by the next commit — CoW
  handles it.
- Consequence, checkable: invariant I7 — after a public mutating op
  returns, no leaf is empty unless it is the root.
- Caveat that follows from no-force-flush: deleting every key empties the
  tree SEMANTICALLY at once (every get returns None), but up to B resting
  tombstones per level move only on buffer overflow, so the structural
  collapse to the empty root leaf may need further mutating ops to drive
  them down (`src/betree.rs`, `delete_everything_collapses_to_the_empty_tree`).

## Invariants

Every tree engine must uphold all of these; the checker walks the whole
tree.

- I1 Key ownership: every buffered message and leaf entry lies within the key
  range its node owns, as induced by ancestor pivots.
- I2 Pivot order: pivots strictly increasing; an internal node with k pivots
  has exactly k+1 children.
- I3 Freshness order: for any key k, occurrences of k strictly decrease in seq
  along any root→leaf path (an ancestor's buffered message for k is always
  newer than any descendant occurrence of k, including the leaf entry).
- I4 Coalescing: a buffer holds at most one message per key (newest wins;
  ADR-0003).
- I5 Capacity at rest: after each public op returns, every buffer ≤ B, every
  leaf ≤ L, every fanout ≤ F. Transient overflow during an op is allowed.
- I6 Uniform height: all leaves are at the same depth.
- I7 No empty leaves at rest (M2.1): after a public mutating op returns, no
  leaf is empty unless it is the root (SPEC "Reclamation v1").

## Public API additions (M1.1)

`DiskEngine` adds persistence on top of the unchanged M0 semantics:

- `DiskEngine::create(path, params)` — new database file; errors if the
  file exists and is non-empty. Durably commits generation 0 (an empty
  tree) before returning.
- `DiskEngine::open(path)` — recover the newest committed state (see
  "Durability contract"). Params come from the superblock — a database
  file is self-describing; only *traces* still travel their params
  out-of-band (ADR-0006, as amended).
- `commit(&mut self) -> CommitStats { nodes_written, bytes_written }` —
  make everything since the last commit durable.
- `file_len()` — current backing-file length in bytes.
- `try_insert` / `try_get` — fallible twins of the `KvEngine` ops: storage
  failures and detected corruption surface as typed `DiskError`s
  (`CorruptNode`, …), never as panics, never as wrong data. The infallible
  `KvEngine` surface treats storage failure as fatal. An error that
  interrupts a mutation or a commit POISONS the engine (see "Durability
  contract").
- `load_all(&mut self)` — fault the entire committed tree into memory.
  `check_invariants` requires a fully resident tree (it cannot read disk
  through `&self`): that holds trivially for a `create()`d engine and
  after `load_all()`; nothing evicts in M1.
- `create_on(vfs, params)` / `open_on(vfs)` — the same over any `Vfs`
  (the M1.2 fault-injection entry points; `FaultyVfs` is the in-memory
  crash-model implementation, ADR-0010).
- `generation()` — non-normative observability helper (like
  `BeTree::height()`): the newest committed or recovered generation.
- `KvEngine::new(params)` is unsupported for `DiskEngine` — an engine
  cannot exist without backing storage — and panics; the harness mounts
  via a tempdir wrapper that calls `create()`.

In-memory semantics (P1–P5) are unchanged between commits; `DiskEngine`
passes the frozen harness via a thin tempdir wrapper (`tests/disk.rs`).

## Public API additions (M2.1)

- `delete(&mut self, key)` and `upsert(&mut self, key, op: UpsertOp)` join
  `KvEngine`. Both consume seqnos, are traced, and are replayed; deleting
  an absent key is a legal no-op with a seqno.
- `DiskEngine` gains the fallible twins `try_delete` / `try_upsert` with
  the same error/poisoning semantics as `try_insert`.
- The trace API is SPLIT (ADR-0013): the byte-frozen M0 harness matches
  exhaustively over `TraceEvent`/`OpKind`, closing those enums, so the
  full vocabulary lives in `TraceEvent2`/`OpKind2`. As of M2.2,
  `TraceEvent2`/`OpKind2` are the CANONICAL trace vocabulary;
  `trace()`'s v1 view is a legacy projection, faithful for insert-only
  workloads only (it silently omits deletes, upserts, and scans).
  `trace2()` is the complete record; `replay2` is the only replay that is
  faithful for mixed workloads.
- `tests/harness2.rs` is the full-mix harness (Q1–Q6, mirroring and
  extending P1–P5 over insert/delete/upsert/get/scan); FROZEN as of M2.2
  — byte-identical from here on, hash in the README freeze table beside
  `tests/harness.rs`.

## On-disk format v2 (M1.1, revised M2.1, normative)

A database is one file. All integers are little-endian and fixed-width
(bincode in the little-endian, fixed-int configuration; `usize` is encoded
as u64).

- Offsets 0 and 4096: two superblock SLOTS, 4096 bytes each.
- Offset 8192: the append-only data region of node records.

### Superblock

A slot holds the bincode-serialized superblock fields, zero-padded to 4092
bytes, followed by the CRC-32 of those 4092 bytes in the slot's final 4
bytes (so every byte of the slot is covered). Fields, in order:

- magic: 4 bytes, "BEET"
- format_version: u32 = 2 (bumped in M2.1: the Message encoding gained
  Delete and Upsert variants; v1 files are refused with a typed
  `UnsupportedVersion` error — no migration pre-release, ADR-0012)
- params: F, B, L (u64 each) — persisted here so `open()` needs no
  out-of-band params; traces remain out-of-band (ADR-0006, as amended)
- last_seq: u64 — seqno of the newest committed op; reopening continues
  the global seqno sequence from here, keeping cross-session
  last-writer-wins ordering and I3 sound (addition to the original M1.1
  field list)
- generation: u64 — commit counter, starting at 0 for `create()`
- root_offset: u64 — file offset of the root node record
- watermark: u64 — one past the end of valid data

A slot is VALID iff magic, version, and crc all check and its geometry is
sane (watermark and root_offset inside the data region, legal params). A
slot that authenticates (magic + crc) but carries a different
format_version makes `open()` refuse the file with `UnsupportedVersion`
(it IS a database, just not one this build reads; ADR-0012).
`open()` additionally ignores a slot whose watermark exceeds the actual
file length: data is synced before the superblock that points at it, so an
honest slot can never outrun the file — one that does (external
truncation) must not be repaired by zero-extending. The
ACTIVE superblock is the valid slot with the higher generation; generation
g is always written to slot g mod 2 — the active slot flips by generation,
not by position. No `root_is_leaf` flag is needed: the node record's enum
tag already distinguishes leaf from internal.

### Node records

A node record at offset X is

    [len: u32] [crc32: u32] [bincode-encoded node, len bytes]

where crc32 covers exactly the payload. A payload larger than u32::MAX
bytes cannot be represented; `commit()` refuses it with a typed
`NodeTooLarge` error before writing anything, rather than silently
truncating the len field into write-time corruption. Child pointers inside
a serialized internal node are u64 FILE OFFSETS of the children's records.
Children are always written before their parent, so a valid child offset
is strictly below its parent's; loading enforces this (along with the I2
arity k pivots ⇒ k+1 children), which keeps every walk over a
hypothetically crc-colliding corrupt file finite and panic-free. Records
are appended at the watermark and never modified in place (copy-on-write;
ADR-0008). Nothing is reclaimed on disk: the file only grows; nodes
unlinked by Reclamation v1 (M2.1) are simply unreferenced by the next
commit.

### Commit protocol

1. Serialize every dirty node, children before parents, appending records
   at the watermark.
2. fsync — data is durable before any pointer to it exists.
3. Write the INACTIVE superblock slot: generation+1, the new root_offset
   and watermark.
4. fsync.

A crash anywhere in this sequence leaves the previous superblock intact
and active; `open()` picks the valid slot with the highest generation and
truncates the file to its watermark, dropping any torn tail. There is no
write-ahead log by design (ADR-0007).

## Durability contract (M1.1, normative)

After commit() returns, the committed state survives any crash. Operations
since the last commit may be lost in their entirety; recovery yields
EXACTLY the last committed state — never a partial application, never
corruption. Uncommitted data has no durability guarantee.

Corruption is detected, not propagated: every superblock and node record
is checksummed, and a read of a record that fails validation returns a
typed `CorruptNode` error — never a panic, never wrong data.

Errors poison: a storage error that interrupts a mutation or a commit
leaves the previous generation intact on disk, but the in-memory state can
no longer be trusted to match any committable whole (an interrupted flush
may have moved committed messages out of a buffer; a failed commit may
have half-published a generation — whether it became durable is the
classic lost-ack ambiguity). The engine therefore refuses further commits
with a typed `Poisoned` error; reads stay best-effort; reopening the file
recovers the last committed state.

## Crash model and guarantees (M1.2, normative)

The durability contract is enforced mechanically by the crash harness
(`tests/crash.rs`) over the crash model of ADR-0010: a crash at any point
of the device-op history preserves everything synced by then plus an
arbitrary subset of the un-synced window, each pending op independently
dropped, fully applied, zero-filled over its extent, or torn to a byte
prefix; a failed sync durably applies an arbitrary subset and discards
the rest (fsyncgate). Under every such crash image, with oracle snapshots
taken at each commit:

- A1 — recovery succeeds: `open()` returns an engine. A typed-error
  failure is permitted only for crashes that precede the initial
  `create()` return (no generation was ever durably published).
- A2 — recovery is exact: the recovered generation g is one of the
  committed (or attempted) generations, and a full-domain sweep matches
  generation g's oracle snapshot EXACTLY.
- A3 — no durable commit is lost: g is at least the newest generation
  whose commit had fully synced before the crash point (and never beyond
  the last attempted generation).
- A4 — recovery is structurally sound: the recovered tree passes the full
  I1–I7 invariant checker.
- A5 — recovery is live: the recovered engine accepts new operations,
  commits them, and survives a further reopen with the combined state —
  including seqno continuity (`last_seq`).

Recovery is idempotent: crashing the recovery's own truncation window and
recovering again yields the same generation. The harness itself is
mutation-tested (docs/findings.md, "harness mutations"): deleting the
data sync, breaking superblock-slot alternation, or skipping superblock
CRC verification must each make it fail.

## Out of scope in M0

delete/tombstones, upserts (both arrived in M2.1), range scans (M2.2),
node merges (no deletes ⇒ no underflow; ADR-0002), persistence/disk
(arrived in M1.1), concurrency, compression.

## Out of scope in M1.1

Write-ahead logging (permanently, by design; ADR-0007), auto-commit,
crash *injection* (M1.2 hooks it into `Vfs`; ADR-0009), space
reclamation/GC (ADR-0008 sketches the future path), cache eviction and
memory budgets (M3 — loaded nodes stay resident).

## Out of scope in M2.1

Range scans (M2.2), occupancy-based rebalancing/merges beyond Reclamation
v1, additional `UpsertOp` variants, on-disk space reclamation, freezing
`tests/harness2.rs` (that happens when M2.2 ships).

## Observability (M3.1, normative)

- **Engine-level I/O is the contract metric**: `CountingVfs` wraps any
  `Vfs` transparently (byte-for-byte identical files, property-tested)
  and `DiskEngine::io_stats()` snapshots read/write ops and bytes, syncs,
  and set_lens — exactly the data-path operations the engine asked of the
  device (`len()`, a pure metadata query, is uncounted).
  `/proc/self/io` (linux-only, `proc_self_io()`) is the SECONDARY,
  OS-level metric for M3.2 benchmarks: it includes everything else the
  process does, and the page cache is not defeated in this build.
- **Bounded node cache** (`DiskEngine` only; `BeTree` stays unbounded):
  a byte budget configured at create/open (`*_bounded` constructors;
  `None`/plain constructors = unbounded, all previous behavior
  unchanged). Eviction is lazy LRU; clean nodes revert to their on-disk
  records (reloads re-verify CRCs); dirty nodes are unevictable (writing
  them back would publish uncommitted data, violating ADR-0007); every
  operation's frame stack pins the nodes it holds. THE BUDGET IS A SOFT
  TARGET UNDER PINNING PRESSURE: if pinned+dirty alone exceed it, the
  cache goes over budget and counts an overcommit event rather than
  failing (ADR-0015). `cache_stats()` reports hits, misses, evictions,
  overcommit events, and resident bytes. Eviction is invisible to
  durability, traces, and results — only the I/O and cache counters can
  tell it happened.
- `check_invariants` (trait, `&self`) still requires full residency;
  bounded engines use `check_invariants_full(&mut self)`: suspend the
  budget, fault everything in, check I1–I7, re-enforce.
- **drain()** (`DiskEngine` and `BeTree`) force-flushes every buffer
  until the tree is message-free: a benchmarking/analysis utility OUTSIDE
  the performance model; its internal flushes are NOT traced (no
  FlushDecision events) — never call it mid-workload when recording
  traces for policy analysis. (It cannot be traced even in principle
  without growing the frozen vocabulary; docs/findings.md.)

## Out of scope in M2.2

Streaming scan cursors (ADR-0014), reverse scans, retiring the v1 trace
view (deferred to a polish milestone together with a single documented
harness re-baseline; docs/findings.md, "Deferred decisions"), cache
eviction and memory budgets (M3).

## Out of scope in M3.1

Benchmarks themselves (M3.2), eviction policies beyond lazy LRU, byte
budgets for `BeTree`, write-back caching (forbidden by ADR-0007 as long
as there is no WAL), defeating the OS page cache.
