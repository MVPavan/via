**UNSOUND.** The r3 fixes address the reported defects, but the new whole-write/unacknowledged path conflicts with the unchanged contract and produces an inaccurate API message. I found no other Blocker or Important issue in the whole chunk.

All source references below are at **56879ba**.

| r3 item | Status | Evidence |
|---|---|---|
| #1: steer waiting beyond turn end | Resolved | [driver.rs:729](../../../../../crates/via-adapters/src/driver.rs#L729) selects the Route reply and turn-end notification together; an available reply wins. |
| #2: forced-test ordering | Resolved | [conformance_driver.rs:2225](../../../../../crates/via-core/tests/conformance_driver.rs#L2225) waits for acknowledgement while emission remains blocked. |
| #3: differential oracle rewriting caller text | Resolved | [ecma.rs:539](../../../../../third_party/boon/src/ecma.rs#L539) compares conversion using upstream replacement texts directly with upstream output. Caller-authored replacement forms are covered. |
| #4: obsolete Core steer comment | Resolved | [receipt.rs:685](../../../../../crates/via-core/src/engine/receipt.rs#L685) describes Core’s token, ticket registration and commit barrier. |
| `[\b]` limitation | Recorded correctly | [VIA-PATCH.md:165](../../../../../third_party/boon/VIA-PATCH.md#L165), with a refusal test at [ecma.rs:552](../../../../../third_party/boon/src/ecma.rs#L552). C1 permits this refusal. |

**New finding #1 — Important: `NotDelivered` now covers whole writes, but still asserts an incomplete write.**

Locations: [driver.rs:739](../../../../../crates/via-adapters/src/driver.rs#L739), its enum documentation at [driver.rs:150](../../../../../crates/via-adapters/src/driver.rs#L150), and the public message at [api.rs:1239](../../../../../crates/via-core/src/api.rs#L1239).

The new tests establish that the helper read a complete steer request, then omit its acknowledgement. Both the persistent-turn end and per-turn drop return `NotDelivered`. Those tests pass. Nevertheless, Core says:

> the steer input was not written whole; whether it was applied is unknown

The first clause is false on these paths. The unchanged spec diff repeats it in C2’s `SteerError` row and C1 §3.4; C1 §8.1 also describes uncertainty specifically about partial application. Thus the accepted r3 ruling conflicts with the amended contracts.

**Smallest fix:** keep the uncertainty classification, but change the API message and enum documentation to say delivery was not confirmed and application is unknown. Amend those three contract locations to allow an input written **in part or whole without established delivery**. Keep `NotRecorded` for acknowledged delivery whose observation could not be emitted.

**Verdicts on the six decisions and concerns**

1. **Accept the classification decision, with finding #1 addressed.** After Route starts writing, `NoActiveTurn` would overstate what is known. Uncertain delivery is appropriate.
2. **Accept `SteerAnswer`.** Its per-request marker is set before constructing the write future at [lane.rs:626](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L626). Both profiles use this path. Refusals precede the marker. I found no additional answer-selection or write-ordering defect.
3. **Accept failpoint-only coverage.** The acknowledgement seam is absent from ordinary release builds, and the failpoint steer tests ran successfully.
4. **Accept the one-second guards.** The tests establish their phases through the helper gate or completed turn future. The timeout checks responsiveness; it does not establish ordering. They remain sensitive to extreme scheduler delays, as process tests generally do.
5. **The 200 ms sleep does not establish Route-refusal ordering.** However, cancellation removes the registry entry synchronously in [driver.rs:384](../../../../../crates/via-adapters/src/driver.rs#L384), so the cleanup assertion does not depend on that ordering. Remove the unnecessary sleep/comment, or add a refusal checkpoint if that later phase is intended to be asserted.
6. **The reported forced-test RED proves the checkpoint is required.** Removing acknowledgement marking makes the checkpoint wait fail before forcing; it does not independently demonstrate the production turn-end regression. The persistent-turn regression is the relevant proof for that behaviour.

**Spec verdict:** **not ready unchanged**, because of finding #1. Otherwise, the reviewed amendments still match the code.

For validator engineering limits, the AST-specific oracle is now credible without normalizing caller text. The configured budgets, shared depth, schema-position census and regex limits remain intact. The recorded measurements support the documented bounded cases, including refusal of larger ECMA programs; they are not general latency guarantees. I found no new Important limit bypass.

Independent checks passed:

- Boon: **22 unit, 3 doc tests**.
- Core schema: **20 tests**.
- Failpoint steer selection: **21 tests**.
- Frozen values, recovery and structured-output selection: **9 tests**.
- Intake: **22 tests**.
- Workspace Clippy with failpoints; layer and harness-literal guards.

I verified 271 tracked build-input files against the pinned commit. Git status remained clean.

I did not rerun the entire reported gate, musl checks, release measurements, upstream JSON-Schema-Test-Suite, or real vendor adapters. I did not independently reproduce the worker’s historical mutation REDs or exhaustively test concurrent interleavings.