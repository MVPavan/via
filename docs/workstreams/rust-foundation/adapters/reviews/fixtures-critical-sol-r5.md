**UNSOUND.** All four original r4 defects are fixed, but the new coverage assertion introduces one Minor false red.

At `20dee59`: **85 active tests passed; 31 conformance cases remain ignored**. Current fixtures pass hygiene without false positives. All **15 recorded `after_emit` steps** are exercised individually. Repository files and Git state remained unchanged.

| R4 item | Status and evidence |
|---|---|
| 1 — Failed acknowledgements | **Fixed.** Line and EOF failures replace the event under the queue lock, retain its original arrival stamp and stop the reader. Injected-error probes also verified that trailing checks preserve the error text. [input.rs:257](../../../../../crates/via-fake-agent/src/replay/input.rs#L257), [input.rs:216](../../../../../crates/via-fake-agent/src/replay/input.rs#L216). |
| 2 — Credential exemption | **Fixed.** Listed flags no longer exempt credential values; the original argv/text probes are detected. Current fixtures remain clean. [fixtures.rs:1072](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1072), [fixtures.rs:1127](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1127). |
| 3 — Null version | **Fixed.** Only string versions trigger probing; the null-version witness passes. [fixtures.rs:480](../../../../../crates/via-fake-agent/tests/fixtures.rs#L480). |
| 4 — Batched hoist deadlock | **Fixed.** Each run hoists one targeted request. All 15 current targets pass their causal-failure checks. The eligibility restriction has the defect below. [fixtures.rs:679](../../../../../crates/via-fake-agent/tests/fixtures.rs#L679), [fixtures.rs:1453](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1453). |

**Minor — Independent hoistability becomes an unsupported fixture-validity requirement.**  
[fixtures.rs:1447](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1447), [fixtures.rs:376](../../../../../crates/via-fake-agent/tests/fixtures.rs#L376).

The eligibility rule correctly avoids testing a later request by introducing an earlier order violation. It also prevents silent skips: excluded recorded steps fail the coverage assertion.

However, that assertion rejects legitimate fixture shapes. The commit’s own three-request witness replays successfully, but has three named `after_emit` steps and only one independently hoistable target, `[5]`. A probe confirmed that the main coverage condition rejects it (`1 != 3`). Including such a recording in the corpus would therefore fail despite valid replay behavior.

**Smallest fix:** distinguish complete causal coverage from independent hoistability. Account for excluded steps through a proven FIFO/preceding-floor implication or another witness strategy, while retaining direct mutations for all 15 current targets.

I found no other introduced defect in ack replacement, timestamp ordering or current fixture validation.

I could not verify private-recording fidelity, real adapter `drive()` implementations and controlled-clock behavior, or non-Linux behavior. I did not rerun the full workspace gate or inject blocked/short filesystem writes. No vendor CLI/model or `bd` command ran.