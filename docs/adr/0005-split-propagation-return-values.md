# ADR-0005: Split propagation via return values

Status: accepted (M0.2)

`split_if_needed` and the flush cascade report promoted (pivot, new-node)
pairs to the level above; a node never reaches back up the tree, and there
are no parent pointers to maintain or corrupt. The parent integrates
returned pairs immediately — before its flush loop re-evaluates routing,
since child indices shift — and root growth is handled by one top-level
loop that stacks new roots above promoted pieces (which also preserves I6:
all pieces sit at the same depth). The cascade walks strictly downward, so
its depth is bounded by tree height — but it is processed with an explicit
frame stack rather than machine recursion, because legal-but-degenerate
parameters make height linear in the number of inserts (F=2 under sorted
insertion; SPEC "Structure parameters") and recursion would overflow the
call stack. The invariant walk is iterative for the same reason.
