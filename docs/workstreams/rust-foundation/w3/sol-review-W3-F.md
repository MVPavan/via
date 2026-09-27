**Verdict: UNSOUND.** The branch fixes several reported failures, but a force stop can still strand a receipted turn, and Host can report `forced` without evidence that it stopped a live vendor.

### Findings

1. **Important — A force stop can skip a queued drive.** `crates/via-cli/src/server.rs:130` breaks immediately for `Force`, while a receipted spawn may still be in `drive_rx` and absent from the `drives` JoinSet (`crates/via-cli/src/server.rs:149`). Shutdown then has no `ForcedTurn` to finalize; it reports an unresolved turn and exits 4. The direct Engine prelaunch test does not exercise this daemon race. Drain or classify queued drives before final shutdown, and add an end-to-end force immediately after the spawn receipt.

2. **Important — `forced` can be inferred from stale exit observation.** `crates/via-host/src/host.rs:776` treats an empty 50 ms polled exit receiver plus `Reply::Stopping` as force evidence. The anchor sends that reply even if the vendor has already exited. The released-control path makes the same inference at `crates/via-host/src/host.rs:552`. A naturally exited vendor can therefore produce `cancel.outcome: forced` once group absence is proved. Have the anchor report whether cleanup actually stopped a live group, and test an exit between the last status poll and stop.

3. **Important — The Store-failure force test accepts missing cancel events.** `crates/via-core/src/engine.rs:308` latches `store_failed`, so `cancel.requested` and `cancel.settled` are skipped; `crates/via-cli/tests/s1_daemon_stop.rs:1136` explicitly expects their absence while the committed envelope contains `cancel`. That does not satisfy C1 §6’s cancel lifecycle. Reconcile the durable event head after a failed write, then commit the lifecycle events when persistence is possible; otherwise report that the terminal could not be persisted.

4. **Important — Client joins can pass the final deadline.** `crates/via-cli/src/server.rs:249` awaits every client join after `abort_all()` with no remaining timeout. If a task does not reach an abort point promptly, daemon main can exceed the contract’s single 10 second bound. Snapshot the pending count at the deadline and leave unfinished joins to process exit.

### Claim and regression assessment

Items **1, 2, 4, 5, and 7** have fixes at the relevant layer for the demonstrated cases. Their new or extended regressions would fail on the base for the reported behavior. Items **3 and 6** cover prelaunch acknowledgement and the normal force lifecycle, but do not establish truthful force evidence or cover the queued-drive race. Item **8** checks retained partial text and spans; its Store-failure regression catches the old `cancelled` disposition, but enshrines the lifecycle gap above. The Host evidence test could not compile on the base because its new field did not exist, so it is not a behavioral pre-change regression.

I agree with the worker’s disclosed launch window and timing uncertainty. The queued-drive race, false force evidence, missing cancel events after Store failure, and unbounded post-abort join are missing from that list. I inspected the named Git ref and contracts read-only; `git diff --check` passed. I did not rerun tests because the shared checkout has unrelated edits and an unresolved conflict. The worker’s gate results remain unverified here.

