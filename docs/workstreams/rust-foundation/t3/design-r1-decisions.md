# T3-0 design round 1: orchestrator decisions

Reviews of `design.md` at `42993bc`: GPT-6 Astra medium
(`astra-review-design.md`, UNSOUND) and Claude Fable 5.1 high
(`fable-review-design.md`, SOUND WITH CHANGES). Both excluded the Store
failure policy. One decision per finding; overlapping findings are merged.
"A#" is Astra's numbering, "F-" is Fable's.

## Ownership, wakes and completion

1. **Close reaches a claimed turn** (A1, F-M2). Close attaches its order to
   a `Claimed` turn exactly as cancel does (§3.1). The claimed → running
   transition carries the order. A claim step that finds a close order
   releases its permit and decides again; close's queue pass then cancels
   the turn.
2. **Capacity wait sees cancel and close** (A2). The reservation wait also
   wakes on its slot's order or claim changes (cancel, close, rollback). On
   waking it re-checks the queue head, its claim and any close order before
   keeping a permit.
3. **One owner per cancellation** (A5). When a claimed turn with a cancel
   order rolls back, it moves directly to a dispatcher-owned `Cancelling`.
   A later cancel or close on a `Cancelling` turn subscribes to that
   owner's outcome and never takes ownership. Wakes for a `Cancelling`
   entry under force: pop, rollback, latch, read cutoff (F-minor).
4. **Cancel always completes** (A6, F-M1). The run loop moves running →
   settling atomically under slot state, and drops the turn's order sender
   when it exits. A cancel waiter selects on the order acknowledgement or
   that drop. On the drop it reads the terminal and replies
   `already_terminal: true` with the envelope's `cancel`. A cancel arriving
   while settling sends no order.
5. **No new dispatcher after final shutdown** (A7). The `closing` commit
   checks, under `admission`, the same stop state that refuses new receipts
   once final shutdown is accepted. A close after that point gets
   `daemon_stopping`, and a keyed replay of a committed close still
   replays. During drain, before final shutdown, close is served.
6. **Close result survives restarts** (A8). Every cancellation that close
   makes records the close cause durably in the same transaction. Close's
   `cancelled` list is derived from durable rows when the close completes,
   never from memory, so repeated restarts give the same result.
7. **Refused `Closed` commit** (F-minor). If Store refuses `session.closed`
   because a turn is queued or running, close runs its queue pass once
   more. If the refusal repeats, it replies `admission_refused`, and
   `closing` stays durable for restart to finish.

## Evidence and classes

8. **`quiescent` only with positive proof** (A4, F-B1). A cancel or close
   result says `cleanup: quiescent` only with Host `GroupAbsent` for every
   group the turn or session owns, or when no anchor intent was committed.
   That covers pre-ARM anchors, and earlier turns' groups still held
   unproven. Otherwise the result is `uncertain`, or `pending` while a
   bounded proof runs. Close runs a bounded absence check for held groups
   the session owns. Tests assert this for a stop at
   `host.anchor.after_arm_intent_commit` with slow anchor exit, and for a
   transport-loss terminal that leaves a group unproven.
9. **The idle deadline's class** (F-M3). When Route's `Deadline` coincides
   with a stop order's `force_at`, the order's cause decides the class. A
   `Deadline` with no order stays `deadline_wall`.
10. **Idle progress** (A10). Only meaningful progress resets idle time:
    acceptance, assistant text and tool events, and observations the route
    declares as progress. Unknown observations, stderr and raw noise do
    not. Add a test with repeated unknown frames.
11. **Deadline origin** (A11). Wall and idle deadlines are both computed
    from the submission clock retained before the submission commit. Add a
    barrier test that delays the submission commit past a short budget.
    Fix the report's F19 row.
12. **One `cancel.requested`** (F-minor). The wall-deadline path skips its
    commit when an order already committed one, and keeps that order's
    `requested_at`.
13. **Transient read failure** (F-minor). A single failed read returns a
    plain `store_error`, without affected-turn fields.

## Lifecycle

14. **Version mismatch** (A9, replaces A4 and Q4). A mismatched `hello`
    returns `version_mismatch` with `data.daemon_version` and
    `data.store_path`, and stops nothing. On that connection only a plain
    `daemon/stop` (no drain, no force) is accepted afterwards. The CLI
    compares Store identity first (runtime §6.1). On a match it sends that
    stop; otherwise it exits 4 as today. The plain stop refuses unless
    daemon main's idle predicate holds, counting clients other than this
    one. One idle predicate, evaluated only by daemon main; the request
    reaches it on the existing stop path (F-minor). Amend C1 §1 and
    runtime §6.1.
15. **Re-probe through drain** (A3, F-minor). Re-probing continues through
    drain. It stops and is joined at final-shutdown entry, on force, or on
    the latch.
16. **Q2, Q3, Q5, Q6, Q7:** as recommended (exit 75 for lock contention;
    the narrow SIGINT handler; `event_end: store_error` and the reserved
    Store slot go to Task 4; A7 and A8 with decision 4). Also as recommended:
    §6.4's idle exit ignores settled-uncertain and earlier-daemon holdings.
17. **F23** (F-M4). Reverse contradiction 9. Runtime §5's vendor marker is
    correct: Host adds its own `VIA_PROCESS_MARKER` (`host.rs:572`). The
    F23 test expects the allow list plus that marker, whose value is not
    the anchor's marker.
18. **C3 `interrupt`** (F-minor). List as an amendment: `execute` with a
    stop watch supersedes C3's separate `interrupt` entrypoint.

## Store-failure dependence

19. **Mark policy-dependent branches** (A14, F-minor). The "F12 (pending
    owner decision)" section lists every latch-dependent branch: §2's latch
    steps, §3.2's `Cancelling` kept on latch, §4's closing completion via
    restart, §8's re-probe stop on latch. For each, give its recovery owner
    under both policies. §14's independence claim is corrected to match.

## Slices and tests

20. **S1 compiles alone** (A12, F-minor). S1 may make the minimal
    mechanical edits at the two Core call sites (`drive.rs`'s `execute`
    call and `terminal.rs`'s `RouteError` match) to keep the workspace
    compiling, and must list them in its report. S2 owns their behaviour.
21. **S3 and S4 disjoint** (F-M5). S0 also moves `RecoveredSlots` and the
    reconciliation cursor into `engine/slots.rs`, owned by S3.
22. **S4 runs at Opus 5.5 high** (A15, F-minor).
23. **Tests** (A13, F-minor):
    - add barriers that acknowledge waiter registration and an actually
      pending reservation;
    - add controlled interleavings for claim rollback with cancel, close
      during submission, cancel during settlement, partial-close restart,
      drain with recovered holdings and re-probe, and an acknowledged
      cancel surviving a later wall expiry (C1 §7.4);
    - add the `test-failpoints` seams for F1 (pause after `daemon.lock`),
      F4 (test-build client-version override), and F6 (pause in idle final
      shutdown);
    - mark the scenarios that pass on current code as characterization
      tests: `s1_f02_`, `s1_f09_` without the warning, and
      `s1_f22_autonomous_eof_`.

## Owner decisions pending (do not design yet)

- The Store failure policy (runtime §7).
- Whether `daemon stop --drain` closes sessions durably (report Q1).
- Which sessions `daemon stop --force` closes (F-M6): every open session on
  disk, or only those whose work the force interrupts.

Keep §4 and §6.3 as written for those two points until the decisions
arrive.
