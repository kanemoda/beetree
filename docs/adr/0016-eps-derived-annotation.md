# ADR-0016: ε is a derived annotation, not a configuration knob

Status: accepted (M3.2)

The Bε-tree literature parameterizes nodes of B bytes with fanout B^ε.
beetree's capacities are count-based (F children, B messages, L entries;
ADR-0001), so ε does not exist as an input anywhere. The benchmark suite
still reports an `eps_eff` column — ln F / ln mean_node_bytes, measured
per configuration over the LIVE tree — because it locates each grid point
on the read/write-optimization spectrum the literature talks about. It is
strictly a DERIVED annotation over measurements: comparing it across
configurations whose node sizes differ wildly says less than the raw
write_amp/read_amp columns next to it. Byte-budgeted nodes (a real ε
knob, with flush/split thresholds in bytes) remain future work and would
revisit ADR-0001.
