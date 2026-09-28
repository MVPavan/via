# Task 3 re-check after the review fixes: Sol high

GPT-6 Sol (high) on `5f3c9f7..9be7c72`, verbatim, local links converted to repo paths.

**Verdict: ACCEPT AFTER CHANGES.** The nine assigned findings are closed in the merged source. I found no new code defect in the three requested integration paths: the force watch has one value, Host’s ledger assigns stop ownership by phase, and Core retains proof failures and forced-turn evidence through finalization. This is a static assessment, not an interleaving proof.

| Row | Status | Merged-tree evidence |
|---|---|---|
| 1 | CLOSED | Resumed-page errors reach the proof failure hook; uncertain writes latch: `crates/via-core/src/engine/reprobe.rs:188`. |
| 2 | CLOSED | Host returns the first failed proof; restart close fails before `Closed`: `crates/via-host/src/host.rs:987`, `crates/via-core/src/engine/close.rs:453`. |
| 3 | CLOSED | Terminal and batch read back the uncertain event before adding `raw_log.incomplete`: `crates/via-core/src/engine/drive.rs:1193`, `crates/via-core/src/engine/batch.rs:88`. |
| 4 | CLOSED | The batch read and write have separate two-second bounds; the no-reply binary test checks the skipped outcome: `crates/via-core/src/engine/batch.rs:116`, `crates/via-cli/tests/s1_store_failure.rs:2596`. |
| 5 | CLOSED | First force raise supplies the instant; ledger sections derive one deadline, and late early-stop replies supply no force fact: `crates/via-core/src/engine/latch.rs:565`, `crates/via-host/src/host.rs:149`, `crates/via-host/src/host.rs:564`. |
| 6 | CLOSED | Explicit plain stop passes the mismatch path after Store identity verification, without auto-start: `crates/via-cli/src/client.rs:487`. |
| 7 | CLOSED | `forced_facts` consumes reconciliation facts before the terminal seam; both evidence variants have binary tests: `crates/via-core/src/engine/stop.rs:265`, `crates/via-cli/tests/s1_lifecycle.rs:1562`. |
| 8 | CLOSED | A binary test holds a turn in `Cancelling` across force acceptance and checks one durable closure: `crates/via-cli/tests/s1_lifecycle.rs:1357`. |
| 9 | CLOSED | The losing-daemon test checks socket identity under both lock-contention cases: `crates/via-cli/tests/s1_lifecycle.rs:620`. |

**Ranked new finding**

1. **Important — resolve the three-second contract before closure.** `docs/specs/runtime-contracts.md:954` still says Host stops groups *within* three seconds, while the added text permits a `Stop` attempt after that bound. The code likewise waits for the control lock and writes before applying the reply deadline (`crates/via-host/src/host.rs:2039`, `crates/via-host/src/host.rs:564`). The exception is stated, but it materially qualifies the earlier absolute promise; it is more than a clarification. **Fix:** make §7’s opening sentence state the enforceable guarantee and record the qualification as a contract change. Align design §6.8’s stale “`now + 3 s`” lifetime sentence (`docs/workstreams/rust-foundation/t3/design.md:1009`) with the force instant deadline.

Row 10 and Wire `read_either` remain with `via-jm4.7.8`; intermittent bugs `via-jm4.15` and `via-jm4.16` retain their named owners. None blocks Task 3 closure once the contract wording is resolved.

I inspected the merged source, diff, tests, design, and spec. I did **not** run cargo, bd, or tests, so passing behavior and timing under load were not independently verified.