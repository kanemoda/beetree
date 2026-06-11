# ADR-0015: The bounded node cache

Status: accepted (M3.1)

`DiskEngine` takes an optional byte budget (`None` = unbounded, the
default; `BeTree` stays unbounded by design). Eviction is **lazy LRU**
with no new dependencies: every access pushes `(NodeId, tick)` onto a
`VecDeque`; victims pop from the front, entries whose tick no longer
matches the slot's latest are stale and skipped; the deque is compacted
when it outgrows 4× the resident count. Evicting a clean node reverts its
slot to `OnDisk{offset}` — the reload path is the ordinary verified load,
CRC check included.

**Sizes.** One `est_bytes(node)` mirrors the bincode record encoding
(calibration property: 0.5 ≤ est/serialized ≤ 2.0). Clean nodes carry
their exact record length (set at load and at commit); dirty nodes carry
an estimate refreshed on each mutable access, trailing the latest
mutation by exactly one touch — self-correcting, and irrelevant to
eviction since dirty nodes are unevictable anyway.

**Dirty nodes are unevictable** for a durability reason, not a caching
one: evicting dirty means writing back, and writing uncommitted data to
the file before `commit()` would break the no-WAL atomic-commit contract
(ADR-0007) — recovery could observe state no commit published.

**Pinning.** Every operation's explicit frame stack (get's current node,
scan's frame path, the flush cascade's `flushing` stack) pins the ids it
still holds and will re-access without re-loading; eviction skips pinned
nodes (and re-stamps them recent so the scan terminates). Enforcement
runs after loads and at commit boundaries, examines at most one deque
length per pass, and if pinned+dirty alone exceed the budget the cache
simply goes over and counts an `overcommit_events` — **the budget is a
soft target under pinning pressure**: never panic, never deadlock, never
evict a pinned or dirty node (debug-asserted).

Consequence for checking: `check_invariants(&self)` still requires full
residency, which a bounded cache cannot promise; bounded engines use
`check_invariants_full(&mut self)` (suspend budget → load_all → check →
re-stamp everything resident into the LRU → re-enforce; the re-stamp
matters because loads during the suspension bypass LRU bookkeeping).

Two accounting consequences found by the M3.1 review and built in:
reclamation RELEASES unlinked slots (`NodeSlot::Freed`) — emptied leaves
and collapsed roots must not linger as unevictable dirty garbage eroding
the budget — and record offsets are INTERNED (offset → slot id), so
reloading an evicted parent reconnects to its still-resident children
instead of allocating duplicate slots per miss.
