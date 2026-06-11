# ADR-0008: Variable-length records, physical offsets, append-only data

Status: accepted (M1.1)

Node records are variable-length — `[len][crc32][bincode node]` — appended
at the watermark, and child pointers inside serialized internal nodes are
the children's u64 FILE OFFSETS. Physical offsets need no translation
table, keep `open()` trivial (one superblock read), and make the
children-before-parents commit order self-evident: every stored pointer
leads to bytes that were already durable when the pointer was written, and
every child offset is strictly below its parent's.

Variable-length records avoid inventing a block size before there is data
to size against; capacities stay count-based (ADR-0001) until byte budgets
matter. The data region is append-only and NO space is reclaimed in M1 —
the file only grows, dead versions and all. The future GC path is a block-
translation layer (logical block ids mapped to physical locations), which
can relocate live nodes without rewriting parents; committing to physical
offsets *inside records* is acceptable precisely because that layer would
sit beneath them.
