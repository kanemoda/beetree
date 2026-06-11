# beetree specification — M0 + M1.1

This document is normative for milestones M0 (in-memory engine) and M1.1
(persistence). The generic property-test harness in `tests/harness.rs`
enforces the in-memory semantics mechanically; the invariant checker
enforces I1–I6 structurally. Public API or semantics changes must update
this file in the same change (see `CLAUDE.md`).

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
  assigned per public mutating op.
- Writes become Messages: `Put { seq, value }`. Messages live in internal-node
  buffers and migrate downward via flushes; leaves store materialized entries.
- `get` must observe the NEWEST message for the key: walk root→leaf, topmost
  buffer hit wins; fall through to leaf entry.

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
F = 2 is explicitly not promised.

## Tree structure (M0.2)

### Initial tree state

A new engine is a single empty Leaf as root.

### Pivot convention (normative)

An internal node with pivots p1 < p2 < ... < pk has k+1 children; child i
owns keys in [p_{i-1}, p_i) with p_0 = -inf, p_{k+1} = +inf. A pivot always
equals the smallest key of the subtree to its right. I1 is checked against
this convention.

Consequences: a key EQUAL to a pivot routes to the child on the pivot's
right, and every pivot is a real key of the tree (a leaf split promotes the
smallest key of its right piece; an internal split promotes an existing
pivot, which moves up and is kept in neither half).

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

## Invariants

The real engine in M0.2 must uphold all of these; the checker walks the whole
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
  (the M1.2 fault-injection entry points).

In-memory semantics (P1–P5) are unchanged between commits; `DiskEngine`
passes the frozen harness via a thin tempdir wrapper (`tests/disk.rs`).

## On-disk format v1 (M1.1, normative)

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
- format_version: u32 = 1
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
sane (watermark and root_offset inside the data region, legal params).
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
ADR-0008). Nothing is reclaimed in M1: the file only grows.

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

## Out of scope in M0

delete/tombstones, upserts, range scans, node merges (no deletes ⇒ no
underflow; ADR-0002), persistence/disk (arrived in M1.1), concurrency,
compression.

## Out of scope in M1.1

Write-ahead logging (permanently, by design; ADR-0007), auto-commit,
crash *injection* (M1.2 hooks it into `Vfs`; ADR-0009), space
reclamation/GC (ADR-0008 sketches the future path), cache eviction and
memory budgets (M3 — loaded nodes stay resident).
