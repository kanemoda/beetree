# ADR-0002: Node merges deferred to M2

Status: accepted (M0)

M0 is insert-only: no deletes, no tombstones. Nodes can only ever gain
messages, entries, and children, so underflow is impossible and merge (or
rebalance) logic would be unreachable code with no way to test it honestly.
We therefore defer merges until M2, when deletes introduce underflow and give
merges a real trigger. Consequently the M0 invariant set has capacity upper
bounds (I5) but deliberately no minimum-occupancy rule, and the M0.2 tree
only needs splits and flushes.
