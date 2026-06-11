# ADR-0014: Scans collect; a streaming cursor is deferred

Status: accepted (M2.2)

`scan` materializes its full result (`Vec<(Key, Value)>`). A streaming
cursor would have to hold positions in node buffers and leaves across
calls while `DiskEngine` lazily loads — and possibly later evicts (M3) —
the very nodes it points into, which is a borrow-checker and lifetime
design problem worth solving only if benchmarks demand it: M3+ work.
Collect semantics also keep the bottom-up application algorithm (SPEC,
"Range scans") simple enough to argue correct from I3 alone. The trait
signature already returns a `Result`, so a cursor can arrive later as a
new method without disturbing this one.
