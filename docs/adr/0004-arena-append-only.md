# ADR-0004: Arena node storage, append-only in M0

Status: accepted (M0.2)

Nodes live in a `Vec<Node>` arena addressed by `NodeId` (a `u64` index), and
nothing is ever freed: M0 is insert-only, so there are no merges (ADR-0002)
and a split reuses the original slot for its leftmost piece while appending
the rest — every allocated node stays reachable. Plain indices avoid
`Rc`/lifetime knots in a single-threaded tree, give `TraceEvent::
FlushDecision` a stable node identifier for free, and make the invariant
checker a trivial recursive walk. M1's copy-on-write will replace this
scheme; until then `node_count()` is exactly the arena length.
