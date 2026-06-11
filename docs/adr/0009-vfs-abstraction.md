# ADR-0009: The Vfs abstraction

Status: accepted (M1.1)

The engine never touches `std::fs` directly: every byte goes through the
five-method `Vfs` trait (positional read/write, sync, len, set_len).
`FileVfs` is the production implementation — a real file via unix
positional I/O (unix-only for now), fsyncing the parent directory once on
file creation so the file's existence is as durable as its contents.

Vfs exists so M1.2 can substitute a fault-injecting in-memory
implementation: cut power after any write, tear any sector, fail any sync,
then reopen and check the durability contract. For that to prove anything,
production code must be byte-for-byte agnostic to which Vfs it runs on —
the engine may not branch on the implementation, and identical op
sequences must produce identical file images over any correct Vfs.
