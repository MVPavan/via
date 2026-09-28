GPT-6 Sol medium review of T3-S5, latch part (`4e8e217..b08093c` on local `wt/t3-s5`).

**Verdict for this part: SOUND WITH CHANGES.** The uncertain-outcome latch, two-phase force signal, diagnostic window, ordered shutdown pipeline, and 2 s batch timeouts follow §7.4. Choice 16 is consistent with O1.D4 and the runtime §7 amendment: the batch cancels queued turns of an affected session; queued turns in sessions without an affected turn remain for restart handoff. I found no new unowned side set or polling loop, and no demonstrated lock-order or lost-wake defect. A latch still produces exit 4; a scoped failure alone does not.

### Findings

1. **Important — force shutdown undercounts unclosed sessions after a latch.** In `wt/t3-s5:crates/via-core/src/engine/stop.rs:485`, `close_forced_sessions` returns `0` as soon as it sees the latch. If force was accepted for a session and a later forced-terminal write latches, that session remains open while the summary reports `unclosed_sessions: 0`. Exit 4 is correct, but the shutdown account is false. **Fix:** count the remaining force sessions as unclosed when the closure pass cannot run.

2. **Important — restart handoff corruption is absent from `store_failure`.** `wt/t3-s5:crates/via-core/src/engine/recovery.rs:143-147` fails a corrupt head turn as `failed(store)` without recording the failure. The daemon can then serve with `store_failure: null`, contrary to §7.5’s latest-failure record. This is S5 work, not a sound Task 4 deferral. **Fix:** report the handoff’s corrupt-row finding through the failure-record path before continuing admission.

3. **Minor — re-probe proof failures lose their affected address.** The daemon-wide pass calls `proof_failures` with `FailureScope::Request` at `wt/t3-s5:crates/via-core/src/engine/reprobe.rs:107`; `FailureSite::Absence` labels the record `session`, but the request scope supplies no address. A known affected session therefore appears as `scope: session` with an empty `affected.addresses`. **Fix:** carry the owner session with the proof failure and record `FailureScope::Session`.

### Checks and limits

Row 12’s uncertain proof reaches the latch through the new journal accessors; a not-committed proof keeps its holding for the next pass. The re-pointed latch tests I inspected use an uncertain reply or a failed retry, rather than treating one scoped failure as a latch. I found no change to that property in `s1_crash_points.rs`.

The durable-unknown-with-pending-cleanup end-to-end case and reserved Store lifecycle slot fit Task 4’s stated scope. The batch’s no-reply 2 s case also fits its Store-bounds work, but remains a **coverage gap**: the current skipped-batch test injects an immediate failure and does not exercise timeout. I reviewed source and tests with `git show`/`git diff`; I did not run tests, `bd`, or `cargo`, and did not edit or check out files. The worker’s reported gate results were not independently verified.