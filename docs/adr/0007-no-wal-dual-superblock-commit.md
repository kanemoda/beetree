# ADR-0007: No write-ahead log — atomic commit via dual superblocks

Status: accepted (M1.1)

Persistence is copy-on-write: a commit appends every dirty node as a new
record (children first), fsyncs, then publishes the new root by writing the
*inactive* of two superblock slots and fsyncing again. The active slot is
the valid one with the higher generation; generation g lives in slot
g mod 2, so the slots alternate and the previous generation's superblock is
never touched while the new one lands. Recovery is just `open()`: pick the
newest valid slot, truncate to its watermark.

There is deliberately no WAL. The tree itself is the log: nothing is ever
modified in place, so there is no partial-update state to repair and no
replay logic to get wrong — recovery yields exactly the last committed
state (SPEC, "Durability contract"). The cost is commit-granularity
durability only: ops since the last commit vanish as a unit, which the
contract embraces. Fine-grained durability, if a milestone ever needs it,
means revisiting this ADR rather than bolting a log onto a design that was
shaped by not having one.

A consequence: a commit that errors part-way poisons the engine. Whether
the failed generation became durable is unknowable (the lost-ack window),
and retrying at the same watermark could overwrite records that a durable
superblock already points at — so the engine refuses further commits and
the caller reopens, letting recovery arbitrate. M1.2's fault injection
hammers exactly this edge.
