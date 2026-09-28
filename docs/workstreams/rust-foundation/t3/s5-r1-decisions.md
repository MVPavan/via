# T3-S5 round 1: decisions on Sol's reviews

S5 delivered at `b08093c` (`reports/T3-S5.md`; gate 265 / 1, failpoints
387 / 1 three times, F08–F12 47). Sol medium reviewed it in two parts:
`sol-review-S5-latch.md` (SOUND WITH CHANGES) and `sol-review-S5-scoped.md`
(UNSOUND). Neither found an unowned state, lost wake or double owner. The
findings are classification and wiring gaps inside the designed paths.
All are accepted.

1. **Every uncertain Route or Store failure latches, even after a first
   failure** (scoped 1). `resolve.rs:283` `route_failed` returns once
   `first_failure` is set, so a later `WriterLost` or `Uncertain`, for
   example a raw write during cleanup, is not reported. Classify and report
   a latching failure before the `first_failure` guard; keep the first
   note for the turn's resolution write. Regression: a clean event failure,
   then an uncertain raw failure; the daemon latches.
2. **A corrupt session-head read latches** (scoped 2, §7.1). `Head::lock`
   errors are discarded at `journal.rs:384`, `drive.rs:1364`, `close.rs:357`
   and `receipt.rs:270`, and treated as not committed or a plain
   `store_error`. Keep the error, classify it with `WriteOutcome::of`, and
   report `Corrupt` through the hook before any scoped path. Regression
   per call site class.
3. **Execute completion attaches row 5's order** (scoped 3).
   `drive.rs:1319` drains queued observations through `observe` without
   `stop_for_store`, so a failed event there leaves `failed(store)` without
   row 5's cancel and cleanup evidence. Call `stop_for_store` after each
   drained observation. Regression: execute completion coinciding with a
   queued observation whose write fails.
4. **Force closure counts unclosed sessions after a latch** (latch 1).
   `stop.rs:485` `close_forced_sessions` returns 0 on the latch, so the
   summary reports `unclosed_sessions: 0` while forced sessions stay open.
   Count the remaining force sessions as unclosed when the closure pass
   cannot run.
5. **The restart handoff's corrupt rows reach `store_failure`** (latch 2,
   §7.5). `recovery.rs:143-147` fails a corrupt head turn without recording
   it, so the daemon serves with `store_failure: null`. Report it through
   the failure-record path before admission opens. This is S5's §7.5 work,
   not a Task 4 item; it replaces report choice 12's handoff half.
6. **Re-probe proof failures carry their session** (latch 3, minor).
   `reprobe.rs:107` records `FailureScope::Request` with no address. Carry
   the owner session and record `FailureScope::Session`.
7. **The S4 carried proof is closed now.** A corrupt row queued behind a
   durable `unknown` turn whose cleanup is `pending` makes progress once
   the cleanup settles. S4's barrier-held anchor that survives a restart
   (`s1_f22_surviving_anchor_verified_and_stopped_on_restart`) supplies the
   harness. An engine-level test is acceptable if the end-to-end form is
   impractical; say which, and why.
8. **Task 4 (`via-jm4.7.8`) keeps two items:** the reserved Store lifecycle
   slot (§7.4), and the batch's 2 s no-reply timeout end to end, which is a
   coverage gap for Task 4's Store-bounds work.
9. **Design edits** from the report are applied by the orchestrator at
   merge, with this round's readings.

## Round 1 (`24498df`)

Gate 268 / 1, failpoints 392 / 1 three times, F08–F12 48. Decision 3's
regression is engine-level: `tokio::select!` makes the drained branch
unreachable on demand end to end. Decision 7 uses S4's crash-behind-running
harness and writes the `pending` cleanup into the stored envelope while no
daemon runs, because no production path stores `pending`. Its RED comes from
removing the corrupt-row fallback.

10. **Decision 2 covers every head read.** The worker found two more
    `Head::lock` reads that discard corruption: the terminal commit in
    `drive.rs` and the force-closure step in `stop.rs`. Same defect class,
    same fix: classify with `WriteOutcome::of` and report `Corrupt` through
    the hook. Search for any other head read and treat it the same way.

## Round-1 check (`sol-review-S5-r1.md`): SOUND WITH CHANGES, do not merge

Round 1 plus decision 10 delivered at `72def69` (gate 272 / 1, failpoints
397 / 1 three times, F08–F12 49). Decisions 1–3, 5–7 and 10 hold, with no
new unowned state, lost wake, lock-order violation or double owner.
`of_read` is the right outcome for a failed read: nothing was written, and
`Corrupt` still latches. Decision 7's test is a characterization of a state
no production path writes today; decision 3's is engine-level. Both are
labelled so.

11. **Corruption is classified once, where a Store read reply reaches
    Core** (finding 1). Reads outside the head still discard SQLite
    corruption: `stop.rs:502,511`, `drive.rs:1197` and `batch.rs:95-101`.
    Design §7.1 latches corruption on any read. §7.3's "reads otherwise
    never latch" exempts other read failures only. Patching call sites one
    at a time is the Task 2 non-convergence pattern, so fix it at the owning
    boundary:
    - every Store read reply that carries `StoreError::Corrupt` reports it
      through Core's failure hook at one point, before the reply reaches
      its caller, so no read can omit it;
    - phase one runs synchronously there; phase two is deferred until
      `admission` can be taken, as the latch path already does;
    - remove the per-site corruption reports that the boundary makes
      redundant, and keep each site's reply to its request unchanged;
    - the layer rules hold: Store does not depend on Core. If the boundary
      needs Store to call into Core, use a callback or signal that Core
      registers.
    Tests: a read-corruption seam that reaches every read command, and
    regressions for the four paths above, each witnessed by an
    acknowledgement.
12. **`unclosed_sessions` counts what is durably open** (finding 2).
    Decision 4 counts every remaining force session after a latch, but a
    forced terminal can commit `session.closed` and then lose its reply.
    Check each remaining session's closed state with a read-only query.
    Count an unreadable state as unclosed; its corruption is classified by
    decision 11. The reply-loss test asserts the durable `closed` value
    beside the summary.

## Round-2 check (`sol-review-S5-r2.md`): SOUND WITH CHANGES

Round 2 delivered at `025c3ea` (gate 280 / 1, failpoints 406 / 1 three
times, F08–F12 50). Sol confirmed the read boundary covers all 20 read
commands and runs phase one before the reply is sent. The boundary runs on
the SQLite worker thread and takes the failure-record and stop mutexes
separately; no caller holds either while awaiting a read. Phase two runs at
final-shutdown entry, which §6.8 permits. A corrupt read during startup
recovery fails startup with exit 4 and never serves latched.

13. **One failure, one record** (finding 1). When a write path aborts
    because its prerequisite read returned `Corrupt`, that read has already
    reported the failure at the boundary. Carry an "already reported" mark
    through the aborted write path. Keep the turn's resolution and the
    latch, but record no second failure. Regression: one corrupt
    prerequisite read gives `store_failure.count` 1, for example on the
    acceptance path at `drive.rs:1405-1412`.
14. **Unjoined sessions are counted by their durable state too** (finding
    2). On the latch path, `stop.rs:485-488` counts an unjoined force
    session without reading it. Include unjoined sessions in decision 12's
    read-only closed-state check, but keep them out of closure writes.
    Regression: a durably closed, unjoined session is not counted.
15. **Small completions.** Remove the unread `Option<StoreError>` from
    `cancel_reads`. Add a direct regression: a corrupt read during startup
    recovery exits 4, unlinks the socket and releases both locks.

## Round-3 check (`sol-review-S5-r3.md`): SOUND WITH CHANGES (minor)

Round 3 delivered at `5f00971` (gate 285 / 1, failpoints 412 / 1 three
times, F08–F12 51). Decisions 13–15 hold: `ReadCorrupt` comes only from a
read `Corrupt` that the boundary already recorded, and it still latches.

16. **Both paths count unjoined sessions by durable state.** On the
    non-latch path, `stop.rs:507-513` still counts a skipped unjoined
    session as unclosed without a read. Use the same read-only
    `durably_open` check on both paths. Regression: a closed, unjoined
    session with no latch. The exit status is unchanged, because unjoined
    dispatchers already make shutdown incomplete.

Round 4 (`e7d0c09`, gate 286 / 1, failpoints 413 / 1 three times,
F08–F12 51): Sol SOUND, no findings (`sol-review-S5-r4.md`).
