# T3-S3 round 1: decisions on Sol's reviews

S3 delivered at `e3d542d` (`reports/T3-S3.md`; gate 241 / 1, failpoints
318 / 1 three times, F08–F12 17). Sol medium reviewed it in two parts,
`sol-review-S3-shutdown.md` and `sol-review-S3-serving.md`, both SOUND WITH
CHANGES. All findings are accepted.

1. **The force set reads a slot atomically** (shutdown 1).
   `unfinished_sessions()` reads `queued()` and `running_turn()` under
   separate slot locks, so a queued → running move between them can drop
   the session from the force set. One slot method checks both under one
   state lock. Regression: the move interleaved between the two reads,
   witnessed by a seam.
2. **A timed-out dispatcher is joined, not only aborted** (shutdown 2).
   After step 3's timeout, `abort_all()` is followed directly by Host
   reconciliation, so a dispatcher may still own a turn. Join the aborted
   tasks within the bound before reconciling. A dispatcher that still
   cannot join makes shutdown incomplete, and its turn is excluded from
   final terminal settlement and left to restart recovery. Record the exit
   status and the design reading.
3. **Resumed paging makes progress while this daemon owns groups**
   (serving 1). The own-groups fence can starve an earlier daemon's unread
   anchors, and their permits, indefinitely, even in a drain. Page safely
   while current groups exist: either bound paging to the startup cohort,
   or exclude this daemon's own anchors. Choose the mechanism, and keep any
   Store change a read-only query. Regression: a live current group plus an
   unread earlier anchor holding the only slot; the queued turn runs once
   the earlier anchor is proven.
4. **Auto-start bounds `hello` by the startup budget** (serving 2). Each
   pre-`hello` read uses the remaining 15 s budget, not the request's read
   timeout, and reset or EOF is retried only within it (§6.1 step 5).
   Regression: a silent peer that accepts but never answers `hello`.
5. **The Store probe** (serving 3).
   - Enforce the writer-exclusion boundary: `probe` runs only under
     `store.lock`, and the code makes that visible (for example, a lock
     guard parameter). Writers outside VIA's locks are unsupported.
   - WAL present without SHM: check SQLite's documentation (wal.html,
     "Read-Only Databases"; uri.html, `immutable`). If no read-only open can
     read the WAL without creating `-shm`, reading correctly wins: keep the
     ordinary read-only open for that state, document the sidecar as a
     limit, and cover it with an F11 test that asserts the refusal is
     correct. If a non-mutating read does exist, use it.
6. **Re-probe backoff resets on every added holding** (serving 4, minor).
   Signal a holding-generation change to the loop, so an addition during a
   wait or a pass resets the timer to 1 s.
7. **No nested ledger and `RecoveredSlots` locks** (serving, lock order).
   Host drops a replaced `RecoveredGroup` token under the ledger mutex,
   which then takes `RecoveredSlots`. Design §1 says those are never
   nested. Move the token's drop outside the ledger guard. This is a
   minimal Host edit with a present requirement.
8. **Test ownership.**
   - Unassigned: the force variant of
     `s1_close_waiter_resolves_on_force_and_latch` goes to S3 in this
     round. Its latch variant goes to S5.
   - S5: `s1_cancel_wait_across_force_handoff`'s two variants (they need a
     commit failure at the forced terminal), and the failure half of
     `s1_close_failed_closed_keeps_closing_count`, using `fail_io`.
9. **Design edits 1–9 from the report** are applied by the orchestrator at
   merge, with round 1's readings.

## Round-1 check (`sol-review-S3-r1.md`): SOUND WITH CHANGES

Round 1 delivered at `ec5880d` (gate 250 / 1, failpoints 329 / 1 three
times, F08–F12 17). Decisions 1–4, 6 and 7 hold. Sol confirmed the cohort
bound's reading: it is sound because Store never deletes, replaces or
vacuums an anchor, and because startup reads the bound after recovery and
before any dispatcher starts or admission opens. Design §8 records both
conditions, with anchor retention as the revisit condition.

10. **Bind `StoreLock` to its State directory** (finding, decision 5).
    `Store::open_locked(state, lock)` accepts a lock taken for any
    directory. Bind the lock to the State directory's filesystem identity
    (device and inode) and reject a mismatch in `open_locked`. Test it with
    a wrong-directory guard.

## Round-2 check (`sol-review-S3-r2.md`): SOUND WITH CHANGES

Round 2 delivered at `0079b0d` (gate 251 / 1, failpoints 330 / 1 three
times, F08–F12 17). Decision 10 closes the wrong-directory case.

11. **Directory replacement between lock and open: deferred to the
    platform gate (`via-pvj.2`).** Sol found that the State directory could
    be replaced between `StoreLock::acquire`'s lock and its identity lookup,
    or after `open_locked`'s identity check and before SQLite's path-based
    open. The fix it proposes is to open every Store resource relative to a
    retained directory descriptor. Runtime §6 already assigns this: "Native
    race resistance remains a platform gate." Only the owning user, or
    root, can replace a 0700 State directory, which is inside the per-user
    trust boundary. SQLite opens by path, so descriptor-relative opens need
    a custom VFS or `/proc/self/fd` paths, which is platform work. S3's
    binding stops accidental and cross-directory misuse, which is its
    present requirement. `via-pvj.2` carries the race with a regression that
    replaces the directory. Design §6.1 records the limit.
