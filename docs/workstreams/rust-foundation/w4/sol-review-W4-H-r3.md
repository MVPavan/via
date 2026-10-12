**Verdict: SOUND WITH CHANGES.** The round-2 merge blocker is fixed for the stated force path. I found no new merge blocker in the round-3 diff.

### Round-2 findings

| Finding | Assessment |
|---|---|
| Post-ARM acquisition abandoned by force silently loses raw output | **Addressed.** Host marks the point just before ARM (`crates/via-host/src/host.rs:445`); when the force grace expires, Wire distinguishes that path (`crates/via-wire/src/runtime.rs:181`) and Route carries `raw_incomplete` to Core (`crates/via-routes/src/runtime.rs:83`). Core emits the event and terminal warning. The regression detects the missing marker: the worker reports that changing Route’s flag back to `false` makes it fail. It does **not** independently prove that the vendor wrote its line before force. |
| Forwarding regression did not establish backpressure | **Addressed for the channel-full condition.** The test now observes zero capacity, continues polling the execution for 200 ms, and checks capacity again before forcing (`crates/via-core/tests/route_stream.rs:399`). It does not directly observe the pending send, but the undrained channel and queued vendor frames make that path credible. |

The other round-2 assessments remain unchanged: stalled acquisition and blocked forwarding are fixed at their owning layers; the client-join count regression detects its original failure; the queued-drive test has the reported skipped-drain mutation evidence.

### Deferrable

- **Make the post-ARM regression deterministic.** It sleeps one second and assumes the stand-in anchor has launched and written output (`crates/via-core/tests/force_stop.rs:288`). A slow run can force before ARM and fail without a product defect; a passing run need not prove the line was written. Have the anchor signal the test after writing, then force.
- **Clarify the raw-completeness contract at this boundary.** The ARM flag proves output *may* have been lost, not that bytes were lost (`crates/via-host/src/host.rs:445`). C1 says `raw_log_incomplete` applies when raw bytes were actually lost (`docs/specs/via-api-v1.md:648`). An anchor that receives ARM but launches no vendor can therefore get a conservative false warning. Drain the retained pipes or define an explicit “completeness uncertain” outcome when this case is implemented more fully.

**Checks:** I inspected the named refs and contracts without checking out or editing files. I did not run branch tests; the worker reports 107 passed and 1 skipped. `git diff --check` reports only a blank line at EOF in an out-of-scope W4-I review document.