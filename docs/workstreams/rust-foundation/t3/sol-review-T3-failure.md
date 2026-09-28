GPT-6 Sol high Task 3 review, part failure (`724ef3d..9efbab8`, local `rust-foundation`).

## Verdict: UNSOUND — Task 3 part B

The integrated result has a path that continues serving after an uncertain Host proof write without latching. Two other paths break the specified recovery behavior. These findings come from read-only inspection of `git diff 724ef3d rust-foundation` and the named contracts; I did not rerun the slice reviews.

| Rank | Finding and concrete fix |
|---|---|
| **Blocker — fix now in Task 3** | **Resumed startup-cohort paging drops an uncertain proof-write failure.** `crates/via-core/src/engine/reprobe.rs:166` returns on any `recover_cohort_page` error without calling the failure hook. Host can return `Journal { site: Absence, uncertain: true }` when its absence-proof commit has an unknown outcome (`crates/via-host/src/host.rs:1869`). The cursor remains for a retry, but the daemon remains healthy and may dispatch more work, violating O1’s latch rule. **Fix:** classify the page error before returning; send uncertain journal outcomes through `proof_failures` and stop the pass when the latch is pending. |
| **Major — fix now in Task 3** | **Restart close completion can write `Closed` and admit a latched startup.** The restart path awaits `absence_check` and then unconditionally attempts `commit_closed` (`crates/via-core/src/engine/close.rs:426`). If a previously held group becomes absent during this check and its proof commit fails, `absence_check` returns after recording or latching the error (`crates/via-core/src/engine/close.rs:325`). Its caller discards that outcome. A successful `Closed` then lets `hand_off_queued` return success, contrary to the design’s rule that restart-close write failures fail startup. **Fix:** return a typed Store-failure result from the startup absence check; fail startup before `Closed` on a failed proof write, including an uncertain one. Keep ordinary unproved group absence as `cleanup: uncertain`. |
| **Major — fix now in Task 3** | **A lost acknowledgement can duplicate `raw_log.incomplete`.** When that event’s commit outcome is uncertain, `raw_owed` remains true (`crates/via-core/src/engine/drive.rs:733`). The batch re-reads the uncertain event through `journal::reconcile`, but does not clear the owed flag (`crates/via-core/src/engine/batch.rs:93`); `build` appends another event at the next sequence (`crates/via-core/src/engine/batch.rs:203`). The ordinary terminal path has the same owed-event decision (`crates/via-core/src/engine/drive.rs:1204`). **Fix:** make the durable read-back establish whether this turn already has `raw_log.incomplete`, and add it to a terminal or batch only when absent. Cover “commit succeeded, reply lost” followed by finalization and restart. |

### Durable-step trace

| Step reached before crash | Restart and failed-write reading |
|---|---|
| Receipt | A committed queued turn is handed off; a known failed receipt returns `store_error`; an uncertain receipt latches and requires keyed resolution. |
| Submission | A committed, unterminated submission becomes `unknown` without resend. A known failed submission resolves as `failed(store)` before agent I/O; an uncertain one latches. |
| Acceptance and events | Recovery reads committed events and raw references. A known failed event leaves its sequence available and resolves the turn as `failed(store)`; an uncertain event calls for latch and read-back. |
| `cancel.requested` | Recovery retains a durable request and its timestamp. A known running-turn write failure takes the store-stop resolution path; an uncertain one latches. |
| `cancel.settled` | Recovery retains a durable settlement rather than appending a second one. A failed write takes the failure path. |
| Terminal | A durable terminal is retained. An unterminated submitted turn becomes `unknown`; a known failed natural terminal gets one same-sequence retry, while an uncertain outcome latches even if read-back finds the terminal. |
| `Closing` and `Closed` | Durable `Closing` is finished before admission; a known failed `Closed` leaves it closing for retry. The restart-close finding above breaks failure propagation during that completion. |

The Store read-corruption observer is registered before recovery and reports SQLite corruption before returning a read reply. Startup recovery read errors fail startup; serving-time corruption begins the latch. The Host report and forced-terminal code require positive absence evidence before claiming `quiescent` in the paths inspected. The cohort’s rowid bound excludes anchors created by the new daemon, but its resumed-page error path breaks O1. The shutdown pipeline reports incomplete exit 4 for a latch; that reporting cannot repair a failure the cohort path never latches.

I did **not** run `bd`, Cargo, tests, crash experiments, or a checkout; this is a static integrated review, not a proof of every interleaving. `git diff --check` reported whitespace in added review/design Markdown, which I have not treated as a part B finding. The existing Beads working-tree edits were left untouched.

