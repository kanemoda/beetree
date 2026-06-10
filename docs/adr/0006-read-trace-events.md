# ADR-0006: Read trace events

Status: accepted (M0.2)

`get` records `TraceEvent::Get { key }`. Reads carry no seqno — they do not
participate in last-writer-wins ordering, so they consume nothing — and they
are skipped by `replay` and by P5's op count. Read events exist for future
workload-replay and cost analysis: read/write mix, hot-key detection, and
evaluating flush policies against recorded `FlushDecision` streams. Traces
remain non-self-describing: `Params` still travel out-of-band alongside a
saved trace. Consequence: recording reads mutates the trace, so `get` takes
`&mut self` — honest for a single-threaded engine, and it avoids interior
mutability that would break `trace(&self) -> &[TraceEvent]`.
