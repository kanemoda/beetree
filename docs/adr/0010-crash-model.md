# ADR-0010: The crash model

Status: accepted (M1.2)

`FaultyVfs` models a device with a volatile write cache. Writes and
`set_len`s accumulate in a *sync window*; `sync()` makes the window
durable. A crash at any point in the op-log history yields the bytes
durable at that point plus an **arbitrary subset** of the pending window,
where each op is independently dropped, applied in full, applied as a
zero-fill of its full extent, or torn to a byte-length prefix. A failed
sync follows the fsyncgate model: the caller gets an error, an arbitrary
subset of the window becomes durable anyway, and the rest is discarded
forever (marked clean, never retried).

**No reordering, by construction.** The model asserts at write time that
un-synced writes never overlap byte-wise — our commit protocol never
does this, and the assertion turns the model's simplification into an
engine check. Disjoint writes commute: any interleaving of their partial
applications is byte-for-byte equal to *some* per-op subset/tear choice,
so per-op fates already cover every reachable reordering outcome.

**Why zero-fill is a first-class fate.** Filesystems can journal a size
update before the data lands (metadata-before-data): the file grows but
the payload reads back as zeros or garbage. Without this fate, a missing
data write is always betrayed by a short file, and a harness built only
on drop/tear would let a commit protocol that syncs nothing slip through
whenever file length alone gives the crash away.

Known simplifications: tearing keeps a contiguous *prefix* (real devices
can persist arbitrary sector subsets of one write — but CRC validation
treats any partial record as garbage, so finer tearing adds no new
observable class); `set_len` is atomic (drop-or-apply); directory-entry
durability is not modeled (`FileVfs::create` fsyncs the parent, and
`create_on` durably commits generation 0 before returning); read faults
and bit rot are out of scope here (bit rot is covered by the corruption
tests in `tests/disk.rs`).
