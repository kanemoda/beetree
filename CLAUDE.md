# beetree — engineering rules

- NEVER run `git commit` or `git push`. The human commits manually.
- Commits never carry AI attribution of any kind — no `Co-Authored-By`
  trailers, no "Generated with" lines, nothing. The sole author is the git
  user already configured in this repo.
- Every feature/bugfix starts with a failing test.
- When a proptest case fails, record the shrunk minimal case as a permanent
  `#[test]` regression before fixing.
- Architectural decisions go in `docs/adr/` (short, numbered).
- `cargo clippy --all-targets -- -D warnings` must stay clean. rustfmt always.
- No `unsafe`. Single-threaded by design until further notice.
- Test parameters are deliberately tiny (F=4, B=8, L=8) to force deep trees
  and frequent structural operations. Never "fix" a failing test by enlarging
  capacities.
- Public API or semantics changes require updating `docs/SPEC.md` in the same
  change.
