**Verdict: SOUND WITH CHANGES.** The recovery → handoff → serve order and the normal queued-turn paths implement §10 and the three design decisions. One Store failure path can still let startup succeed, so this is a merge blocker.

### Merge blocker

- **An uncertain cancellation can pass the handoff.** At `6b8a316`, `crates/via-core/src/engine/drive.rs:493-505` latches Store failure when a cancellation commit is uncertain but read-back finds the terminal, then returns `Cancelled::Committed`. `crates/via-core/src/engine/recovery.rs:107-115` treats that as success and can finish the handoff; `crates/via-cli/src/server.rs:201-213` can then enter serving. This violates §10’s requirement that *any* Store failure during handoff fail startup. **Fix:** return `Cancelled::Latched` after latching an uncertain commit, even when its terminal is durably readable. The handoff will then return an error; the dispatcher can retain its existing latched behavior. Add a restart test that loses the cancellation reply and checks startup fails.

### Other review answers

- **Coverage and ordering:** `hand_off_queued` pages durable queued rows in `(session, turn)` order. It cancels only behind a durably `unknown` submitted predecessor with settled cleanup and no unresolved earlier turn; otherwise it counts and enqueues the turn. Cursor paging skips already cancelled rows safely on a later start. Recovery and handoff finish before the server accepts requests. Confirmed submissions are never resent. See `crates/via-core/src/engine/recovery.rs:45-128`, `crates/via-store/src/runtime/sql.rs:489-520`, and `crates/via-cli/src/server.rs:109-153` at `6b8a316`.

- **`cleanup: pending` → Wait:** I found no valid live path that this change newly strands forever. Under P7, pending cleanup keeps the predecessor nonterminal, so the unresolved check already waits; settlement must precede dispatch. The new test constructs a terminal with pending cleanup solely to test the decision rule. Waiting on that synthetic state is consistent with C1 §7.3; cancelling it was not. See `crates/via-core/src/engine/drive.rs:263-276` and `crates/via-core/src/engine/tests.rs:360-389`.

- **Bounds, replay, and F08:** `Unresolved`, `active`, and `queued` count recovered work beyond admission limits; new receipts are refused while full. Starts can spill into the pending set, which has no 128-entry cap. Keyed replay returns the stored receipt with `enqueued: None`, so it cannot request a second start. The two revised F08 expectations are correct: a committed, unsubmitted queued turn survives the lost receipt and runs after restart under C1 §7.5 and §8.1. The new failpoint is listed in the release feature check. See `crates/via-core/src/engine.rs:368-399,433-481,535-601` and `crates/via-cli/tests/s1_crash_points.rs:691-835` at `6b8a316`.

- **Regressions:** The four daemon restart scenarios and the changed F08 assertions detect the original missing handoff: without it, successors remain queued or waits time out. The pending-cleanup test detects the former cancel decision. The 136-turn test detects missing counting or dispatch, though it does not exercise a full 128-start channel. The worker’s reported test runs were **not independently rerun** in this read-only review.

### Deferrable coverage

- `crates/via-core/tests/restart_handoff.rs:134-155` starts only 17 dispatchers, so it does not directly test pending starts after channel saturation. **Fix:** add a separate case with more than 128 recovered `Starting` sessions and verify all starts drain.

Static check: `git diff --check 6c37308...6b8a316` reported only an extra blank line at the end of the design document. No files were edited and `bd` was not run.