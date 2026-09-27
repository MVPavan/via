**Verdict: UNSOUND.** The change fixes the demonstrated post-terminal cases, but its new drain and observation path do not meet the required bounds or evidence guarantees.

### Findings

1. **Important — Failure drain can lose the offending stdout frame.** [via-wire/src/runtime.rs](crates/via-wire/src/runtime.rs:204) removes a complete line before checking its 1 MiB cap. If that check fails, the new [drain](crates/via-wire/src/runtime.rs:237) cannot record the removed bytes. Keep the frame in the buffer until it is validated or durably recorded; add an oversized-line regression that checks the retained raw prefix and incomplete-evidence status.

2. **Important — Observation memory has no byte budget.** The new [64-item channel](crates/via-adapters/src/runtime.rs:102) can hold messages approaching 1 MiB each. C2 limits observation payloads to 256 KiB and the session queue to 4 MiB. Enforce payload and aggregate byte limits before admission, splitting text as C2 specifies; test saturation with large frames.

3. **Important — Raw-store failure stops the drain without declaring a gap.** The new [drain_to_eof](crates/via-wire/src/runtime.rs:240) returns on the first append error. [Route ignores that result](crates/via-routes/src/runtime.rs:68), so later bytes are neither drained nor marked incomplete. Continue bounded discard draining and propagate an incomplete-evidence indication through the failure path.

4. **Important — T1-I5 has no effective forwarding regression.** The new [Route tests](crates/via-routes/src/runtime.rs:303) exercise only `Phase::advance`; they would pass if `forward` stopped sending text, tool, and unknown observations. Add a test that receives the Route stream and asserts payloads, order, and raw references.

The first late-tool test would fail on the pre-change code for the reported protocol error. The duplicate-terminal test also distinguishes the old *wrong* error from the intended one and checks its raw tail. The stderr-flood test passed before the change, as the worker reports; it is a guard, not a regression for this fix.

I agree that Adapter currently drops non-acceptance observations before Core; that remains an integration gap, not a completed C2 observation path. I also agree with the worker’s noted backpressure and cleanup-deadline concerns. The missing oversized-frame evidence case and byte budget above should be added to that list. I inspected the commit, contracts, tests, and current call path; I did not run tests in this read-only review. `git diff --check 19f8d47^ 19f8d47` passed.