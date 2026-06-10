# beetree specification — M0

This document is normative for milestone M0. The generic property-test
harness in `tests/harness.rs` enforces the semantics mechanically; the
invariant checker enforces I1–I6 structurally. Public API or semantics
changes must update this file in the same change (see `CLAUDE.md`).

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

F = 2 is legal but degenerate: with split-only rebalancing (no merges until
M2; ADR-0002), every internal split of a 3-child node must produce a
single-child piece, and sorted insertion drives tree height to Θ(n) and
node count to Θ(n²). Engines must SURVIVE such trees — in particular, no
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

## Out of scope in M0

delete/tombstones, upserts, range scans, node merges (no deletes ⇒ no
underflow; ADR-0002), persistence/disk, concurrency, compression.
