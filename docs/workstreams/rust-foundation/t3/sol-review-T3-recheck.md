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
## Confirmation round 2

**Remaining wording issues — do not close Task 3 yet.**

- `docs/specs/runtime-contracts.md:958` and `docs/workstreams/rust-foundation/t3/design.md:1873` say a launch past the ARM gate is stopped “as soon as it spawns.” The owner sends `Stop` after **receiving `Spawned`** (`crates/via-host/src/host.rs:1261`); a lost reply takes the failed-acquisition EOF cleanup path. Qualify both statements.
- `docs/workstreams/rust-foundation/t3/design.md:1873` says the 3 s bound covers the `Stop` attempt, but the control lock and write are unbounded; only the reply wait has that deadline (`crates/via-host/src/host.rs:2034`). The design also retains absolute “stops every group within 3 s” claims at `docs/workstreams/rust-foundation/t3/design.md:918` and `docs/workstreams/rust-foundation/t3/design.md:1307`.
- `docs/specs/runtime-contracts.md:889` and `docs/specs/runtime-contracts.md:969` still measure shutdown from “first Store failure” or “first failure”; both should identify the **latching failure (`failed_at`)**, as `docs/workstreams/rust-foundation/t3/design.md:1865` specifies.
## Confirmation round 3

Remaining issue — **do not close Task 3 yet.** `docs/workstreams/rust-foundation/t3/design.md:931` still says each `Stop` is bounded at force `+ 3 s`; `docs/workstreams/rust-foundation/t3/design.md:1308` still implies the sends finish by then. In `crates/via-host/src/host.rs:2034`, the control lock and write are unbounded; only the reply wait has that deadline.

The `Spawned` and `failed_at` wording fixes match the code and stated timing. Runtime §7 has no remaining absolute 3 s cleanup claim.
## Confirmation round 4

Remaining issue: `docs/workstreams/rust-foundation/t3/design.md:1780` still says the running group is gone within 3 seconds of the latch. That contradicts A23 and runtime §7, which bound only the `Stop` reply wait; the control lock, write, and group disappearance have no such bound. Task 3 is not ready to close.
## Confirmation round 5

ACCEPT (close Task 3). The cited sections and `host.rs` agree: the three-second deadline bounds only the `Stop` reply wait. The §11 disappearance check is expressly a cooperative-fixture expectation.