# ADR-0003: Buffers coalesce to one message per key

Status: accepted (M0)

An internal-node buffer keeps at most one message per key, the newest
(invariant I4). With last-writer-wins puts as the only message kind, an older
buffered message for a key can never be observed once a newer one sits in the
same buffer — keeping it would only waste buffer slots and trigger earlier
flushes. Coalescing on arrival keeps `get`'s "topmost buffer hit wins" rule
unambiguous within a node, makes I3/I4 cheap to check, and bounds buffer
growth under skewed workloads. Revisit when upserts arrive: delta messages
cannot be blindly collapsed into the newest one.
