**UNSOUND.** Reviewed `9bd13c6..bc694c9`. The original watchdog defects are fixed, but the reader introduces a possible false timeout, and finding 5 remains partly unresolved.

R = `crates/via-fake-agent/src/replay.rs`; T = `crates/via-fake-agent/tests/replay.rs`.

| r1 finding | Status | Evidence |
|---|---|---|
| 1. Obsolete watchdog expiry | Fixed | R:255, 418–438: only the run deadline replaces the load deadline; queued updates are consumed before expiry. |
| 2. Incorrect timing anchor | Fixed | R:307, 615–617, 647, 658–664: previous completion is retained; emit uses the required pre-write instant. |
| 3. Missing run-capped arrival check | Fixed | R:658–672: every received line is checked against the effective limit, including the run deadline. |
| 4. Ordered EOF and strict trailing input | Fixed per rulings | R:630–633 checks EOF order; R:302–305, 318–321, 531–544 checks trailing events. T:915 and T:953 exercise both requirements. |
| 5. Weak timing regression tests | **Partly fixed** | T:725 catches 300→5000; T:887 catches a persistent short limit. T:852–883 does **not** reliably exercise the late-arrival timestamp check. |
| 6. Partial input and concurrent launches | Fixed per rulings | T:987 covers held-open and closed partial input; T:1008 compares eight launches as an unordered PID set. Write-error injection remains the explicitly accepted limitation. |

**New defect — Important: a stamped, on-time line can lose to timeout.**  
[R:555](../../../../../crates/via-fake-agent/src/replay.rs#L555), [R:567](../../../../../crates/via-fake-agent/src/replay.rs#L567), [R:512](../../../../../crates/via-fake-agent/src/replay.rs#L512).

A valid interleaving is:

1. The reader completes a line and stamps it before the limit.
2. It is descheduled before `sender.send`.
3. The main thread’s empty-channel wait expires and returns failure.

The line’s recorded arrival satisfies `arrival ≤ limit`, yet the fixture exits 3. This contradicts the specified equality rule. It is established by source tracing; I did not reproduce the schedule.

**Smallest fix:** synchronize publication of the stamped event with the main thread’s timeout decision, preserving bounded buffering and avoiding a lock across blocking sends. Add a controlled regression for the stamp-before-send gap.

The two-event bound does not prove the author’s read-ahead argument. Delaying an EOF stamp can only miss an early-close violation, because EOF has a lower-bound check. Timed lines have an upper-bound check; delayed observation or delivery can produce false rejection.

**Finding 5’s remaining gap — Important.**  
[T:868–870](../../../../../crates/via-fake-agent/tests/replay.rs#L868) sleeps 800 ms before sending against a 300 ms limit. Normally the fake has already failed, and the test permits the send to fail. Removing the received-line timestamp guard at R:666 would therefore leave this test passing.

**Smallest fix:** add deterministic unit cases with queued early, equal and late timestamps, including a run-capped limit. These directly detect removal of the guard without relying on process scheduling. The existing delay-positive case also would not catch restoring the old “start timing inside the next step” implementation.

The reader’s earlier bounds are preserved: each read takes at most 1 MiB plus newline; oversized input becomes an error; partial input at EOF becomes an error; an open partial line remains bounded by the run watchdog; I/O errors terminate the reader; spawn failure propagates to exit 3. The new channel retains at most one queued event plus one reader-held event, increasing buffering over the former synchronous reader. Fixture, capture, step and diagnostic caps remain intact.

An `expect` after successful `await_eof` fails immediately on cached EOF, as expected. Delays and signals do not re-arm short watchdog deadlines; timed expects following them use their completion instants. The publication race above still applies.

**Verified:** `cargo nextest run --locked -p via-fake-agent` — **43 passed, 0 skipped**. Scoped diff check passed; Git status remained clean on `wt/replay-add`, tip `bc694c9`. The persisted broader gate records exit 0; I did not rerun it.

**Could not verify:** forced scheduling interleavings, reader/thread exhaustion, injected stdin I/O errors, blocked writes, or declined launch-write injection. No files were edited, and no Beads, vendor CLI or model was run.