# Review findings ledger

Findings from the M1.1 adversarial review (multi-agent, 26 raw findings; 3
confirmed by adversarial verification, 1 refuted, 22 left unverified when
the verifier budget ran out). Each entry: status + one-line triage.
Statuses: fixed-in-M1.1 / fixed-in-M1.2 / wont-fix / open.

## Confirmed by verification

1. **commit() silently truncates record len to u32 for ≥4 GiB payloads**
   (critical) — fixed-in-M1.1: `DiskError::NodeTooLarge` refuses before any
   byte is written; SPEC "Node records" states the limit.
2. **Unchecked u64 adds in superblock geometry / record bounds checks**
   (minor) — fixed-in-M1.1: overflow-proof comparisons; regression test
   `insane_geometry_is_rejected_not_panicked`.
3. **No arity validation on loaded internal records → index-OOB panic on a
   CRC-valid malformed record** (minor) — fixed-in-M1.1: I2 arity check in
   `ensure_loaded`; regression test
   `crc_valid_malformed_arity_is_corrupt_not_panic`.

## Refuted by verification

4. **Superblock carries `last_seq` not in the prescribed field list** —
   refuted: documented deliberate deviation (SPEC "Superblock"); required
   for cross-session last-writer-wins and I3.

## Unverified (triaged by hand)

5. **open() never validates watermark vs file length; set_len can
   zero-extend** (major) — fixed-in-M1.1: outrunning slots are invalid;
   test `truncated_tail_falls_back_to_previous_generation`.
6. **Failed flush during try_insert drops the extracted batch; a later
   commit durably loses committed data** (major) — fixed-in-M1.1: errors
   that interrupt a mutation poison the engine (SPEC "Errors poison").
7. **An errored commit can still become the recovered generation** (minor)
   — fixed-in-M1.1: poisoning forbids commit-after-error; the lost-ack
   ambiguity itself is inherent and documented (ADR-0007); M1.2's
   sync-failure harness exercises it.
8. **A crashed/failed create() wedges the path (NotEmpty + 
   NoValidSuperblock)** (minor) — wont-fix (M1): no committed data is at
   stake; the remedy is deleting the file. create() durably commits
   generation 0 before returning, so any post-create file opens — crash
   assertion A1 codifies exactly this boundary.
9. **Failed try_insert silently drops committed messages** (major) — dup
   of 6; fixed-in-M1.1.
10. **ensure_loaded skips structural validation → try_get panic** (major)
    — dup of 3; fixed-in-M1.1.
11. **FlushDecision node ids are session-dependent for DiskEngine**
    (minor) — wont-fix: FlushDecision is descriptive, not normative;
    replay skips it (ADR-0006). Arena ids were never stable identifiers
    across engines.
12. **DiskEngine's KvEngine::new panics; liberty not recorded in SPEC**
    (minor) — fixed-in-M1.2: SPEC "Public API additions" now states it.
13. **SPEC 'never a panic' vs panicking KvEngine surface** (minor) —
    wont-fix: the contract binds the fallible storage layer; the
    infallible adapter's treat-as-fatal policy is itself documented in
    SPEC and rustdoc.
14. **Record len silently truncates >u32 payloads** (minor) — dup of 1;
    fixed-in-M1.1.
15. **open() zero-extends when file shorter than watermark** (minor) —
    dup of 5; fixed-in-M1.1.
16. **Mid-cascade I/O error caveat absent from SPEC** (minor) — dup of 6;
    fixed-in-M1.1 ("Errors poison" paragraph).
17. **Superblock validity rules beyond the CRC have zero test coverage**
    (major) — fixed-in-M1.1/M1.2: insane-geometry unit test (M1.1); the
    M1.2 crash harness tears superblock writes at every field boundary.
18. **uncommitted_ops test: fresh key collides with the random domain**
    (minor) — fixed-in-M1.1: 2-byte sentinels outside the workload domain.
19. **No test for the truncation side of recovery (set_len in open)**
    (minor) — fixed-in-M1.2: the idempotent-recovery step crash-images the
    truncation window itself and reopens.
20. **create() on an existing but EMPTY file untested** (minor) —
    fixed-in-M1.1: covered in `create_and_open_reject_bad_files`.
21. **commit() on a freshly opened, untouched engine untested** (minor) —
    fixed-in-M1.1: covered in `commit_stats_track_dirty_spine`.
22. **Seqno continuity across reopen has no targeted test** (minor) —
    fixed-in-M1.2: `seqnos_continue_across_reopen` asserts the first
    post-reopen op's seqno directly; crash assertion A5 exercises it on
    every recovered image.
23. **No degenerate F=2 persistence test** (minor) — fixed-in-M1.1:
    `f2_degenerate_tall_tree_round_trips`.
24. **commit stats bytes_written assertions vacuous** (minor) —
    fixed-in-M1.2: bytes_written now asserted exactly as data-region
    growth + the 4096-byte superblock slot.
25. **NoValidSuperblock only tested via an all-garbage file** (minor) —
    fixed-in-M1.2: `both_slots_corrupted_is_no_valid_superblock` corrupts
    both slots of a real database.

## Harness mutations (M1.2, Step 3)

Each mutant was applied to a scratch copy of the source, run against the
crash harness (`cargo test --test crash`, 32 cases), and reverted; the
restored files were verified byte-identical by hash.

- **M-A — data sync removed** (`commit_inner`: deleted the `vfs.sync()`
  between the record writes and the superblock write): KILLED by **A2** —
  `A2 (gen 16, pos 49): get([0]) failed: corrupt node record at offset
  14411: UnexpectedEnd` in `crash_random_workloads` (also
  `crash_ascending_keys`, gen 0 pos 2). The superblock and its data share
  one sync window, so an image can durably publish the pointer while
  zero-filling the records it points at. The sync-failure test killed it
  too, incidentally: with one sync per commit instead of two, the
  superblock-targeted injection (`skip = 1`) outlived its commit and the
  expected commit error never came.
- **M-B — superblock alternation removed** (`commit_inner`: every
  generation written to slot 0): KILLED by **A1** in all four tests —
  `A1: open failed (no valid superblock in either slot) for a crash at
  log pos 20` — a torn superblock write destroys the only valid slot.
- **M-C — superblock CRC verification skipped** (`decode_slot` trusting
  magic+version+geometry only): KILLED by **A2** —
  `A2 (gen 4, pos 19): get([251]) diverged from the oracle: None vs
  Some([140, 209])` in `crash_overwrite_heavy` (and the equivalent in the
  other two): a field-boundary tear splices a new-generation prefix onto
  stale slot bytes, decoding to a plausible superblock that claims the
  new generation while pointing at old data.

No mutant survived, so no post-hoc strengthening was needed — but only
because two defenses were designed in up front after dry-run analysis of
exactly these mutants showed a naive drop/apply/tear model would leave
their kills probabilistic or impossible:

1. **`Fate::Zero` (zero-fill) as a first-class crash fate.** With only
   drop/tear, a missing record write always shortens the file, and the
   recovery-side watermark-vs-file-length check would mask M-A — the
   superblock pointing at unwritten data would be rejected for the wrong
   reason (short file) instead of being caught serving garbage. Zero-fill
   models metadata-before-data (file grows, payload never lands) and
   makes M-A's corruption reachable and therefore killable (ADR-0010).
2. **The deterministic superblock tear grid.** M-C's kill window is a
   ~16-byte band of the 4096-byte slot write (a tear between the
   `generation` and `root_offset` fields). Uniform random tear points hit
   it with probability ≈ 16/4097 per image; at 32 cases the mutant would
   usually survive. Canonical danger point (c) therefore tears the
   superblock write at every 4-byte field boundary (0..=72, plus padding
   and near-crc points), making the splice deterministic.

## Deferred decisions

- **v1 trace retirement + harness re-baseline** (recorded M2.2): the v1
  trace view (`trace()`, `TraceEvent`/`OpKind`, `replay`) survives only
  to keep `tests/harness.rs` byte-frozen (ADR-0013); as of M2.2 the v2
  vocabulary is canonical (SPEC, "Public API additions (M2.1)"). Retiring
  the v1 surface requires ONE documented re-baseline of the frozen
  harness — new file, new hash, recorded rationale — and is deferred to a
  polish milestone so it can ride along with any other accumulated
  freeze-debt in a single, auditable break.
- **drain() is deliberately invisible to traces** (recorded M3.1): the
  frozen-matched trace enums cannot gain a Drain/FlushDecision-suppressing
  variant (CLAUDE.md standing rule, learned from ADR-0013), and drain is in
  any case OUTSIDE the performance model (SPEC "Observability") — its
  forced flushes are not policy decisions and must not contaminate
  recorded FlushDecision streams. So drain emits no trace events at all.
