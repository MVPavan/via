**UNSOUND.** R2 #1 is only partially resolved: a steer waiting for vendor acknowledgement can still outlive its logical turn. The other Important findings and the five previous Minors are resolved. I found three additional Minor issues.

References below are at **70dfb1f**.

**Status**

| R2 finding | Verdict | Source |
|---|---|---|
| #1 Steer lifetime and cancellation cleanup | **Partial**: acknowledged-delivery teardown and cancellation cleanup are fixed; the first acknowledgement wait remains open | [driver.rs:672](../../../../../crates/via-adapters/src/driver.rs#L672) |
| #2 Outcome eviction | Resolved: per-request tickets replace the queue, with cancellation retirement and lane-end resolution | [lane.rs:345](../../../../../crates/via-core/src/engine/lane.rs#L345) |
| #3 Delivery lost before recording | Resolved: `NotRecorded` preserves delivery and maps to the amended C1 error | [receipt.rs:769](../../../../../crates/via-core/src/engine/receipt.rs#L769) |
| #4 Promoted-reference census | Resolved: targets join the document’s census; stable positions deduplicate counts | [roots.rs:88](../../../../../third_party/boon/src/roots.rs#L88) |
| #5 Frozen requested inheritance | Resolved: both halves are frozen, decoded and forwarded; status exposes effective states | [frozen.rs:61](../../../../../crates/via-core/src/intake/frozen.rs#L61), [read.rs:433](../../../../../crates/via-core/src/engine/read.rs#L433) |
| #6 Resolved-model cap | Resolved: checked before receipt commit | [intake.rs:475](../../../../../crates/via-core/src/intake.rs#L475) |
| #7 ECMA whitespace | Resolved | [ecma.rs:42](../../../../../third_party/boon/src/ecma.rs#L42) |
| #8 ECMA dot | Resolved | [ecma.rs:44](../../../../../third_party/boon/src/ecma.rs#L44) |
| #9 ECMA word boundaries | Resolved for admitted expressions | [ecma.rs:45](../../../../../third_party/boon/src/ecma.rs#L45) |
| Minor: duplicate inheritance keys | Resolved: duplicates are rejected before collection | [plan.rs:343](../../../../../crates/via-adapters/src/plan.rs#L343) |
| Minor: extended-control boundary | Resolved: the final allowed rewrite is parsed | [ecma.rs:73](../../../../../third_party/boon/src/ecma.rs#L73) |
| Minor: recovery diagnostic | Resolved for frozen-value decoding: cause and session/turn are retained | [recovery.rs:109](../../../../../crates/via-core/src/engine/recovery.rs#L109) |
| Minor: release `spec()` seam | Resolved: conditionally compiled | [driver.rs:469](../../../../../crates/via-adapters/src/driver.rs#L469) |
| Minor: vendored tests outside gate | Resolved: the documented root command works offline under the root lockfile | [verification.md:18](../../../../../.repo-context/verification.md#L18) |

**Remaining and new findings**

1. **Important — Turn termination does not resolve the first steer wait.**  
   [driver.rs:672](../../../../../crates/via-adapters/src/driver.rs#L672), [driver.rs:296](../../../../../crates/via-adapters/src/driver.rs#L296).

   `SteerTurn` closes the emission registry, but `driver.steer` first awaits the separate vendor-answer receiver. It cannot observe the turn-end notification while blocked there. A persistent Route can retain that answer sender during process retirement after its logical turn has ended.

   **Reproduced:** the helper read the steer, emitted a completed terminal without a steer report, and remained behind a gate. `run_turn` returned successfully; polling steer returned pending, and another 400 ms wait timed out. This is the remaining part of r2 #1, rather than a regression in the acknowledged case.

   **Smallest fix:** make logical turn termination resolve or interrupt the vendor-answer wait as well as the emission wait. Preserve acknowledged-delivery information when choosing the result. Add this persistent-terminal-without-steer-report regression.

2. **Minor — The forced-steer regression relies on likely ordering.**  
   [conformance_driver.rs:2219](../../../../../crates/via-core/tests/conformance_driver.rs#L2219).

   The helper’s gate establishes that it wrote the delivery report. It does not establish that Route consumed it or resolved the acknowledgement. Waiting another 500 ms establishes no synchronization relationship. Under delayed processing, force can arrive before acknowledgement and produce a different error.

   **Smallest fix:** replace the sleep with a checkpoint after Route has acknowledged delivery, while observation emission remains blocked.

3. **Minor — The differential oracle rewrites caller-authored classes.**  
   [ecma.rs:494](../../../../../third_party/boon/src/ecma.rs#L494).

   `as_upstream` globally replaces output strings, without distinguishing generated replacements from unchanged input.

   **Reproduced:** adding the literal pattern `[^\n\r\u{2028}\u{2029}]` to `same()` fails. Both converters correctly leave it unchanged, but the helper changes only the new converter’s result to `.` before comparison.

   **Smallest fix:** normalize only replacements originating from translated AST nodes, or use an oracle that preserves this distinction. Add explicit caller-authored replacement-form cases.

4. **Minor — Core’s steer documentation still describes the removed receipt.**  
   [receipt.rs:686](../../../../../crates/via-core/src/engine/receipt.rs#L686).

   The comment says the driver answers with the observation token. Core now mints that token and the driver returns `SteerDelivery`.

   **Smallest fix:** update the comment to describe Core’s registration and the subsequent commit barrier.

I found no additional verified Blocker or Important defect in the whole chunk.

**Worker concerns**

| Concern | Verdict |
|---|---|
| 1. Re-exports outside the owned list | Necessary interface updates; accepted scope expansion. No defect. |
| 2. Larger ECMA programs admit fewer copies | Acceptable documented engineering limit. The measurement file supports the reduced counts. |
| 3. Removed dev-dependencies and bench | Acceptable and recorded in `VIA-PATCH`. The root offline command passed 20 unit and 3 doc tests. It does not restore the excluded upstream integration suite. |
| 4. Process-wide token counter | Sound for live calls: atomic allocation provides uniqueness without requiring a separate counter per session. |
| 5. Forced-test ordering | **Only likely**, not established. Finding 2. |
| 6. Default overflow test takes about 10 s | Acceptable verification of the production stall bound. The failpoint variant provides faster coverage. |
| 7. Early recovery decoding and Store diagnostics | Appropriate: malformed effective values fail before `recover_session` for that turn. `RecoverError::Store` retains its text; some other paths still deliberately map failures to generic API errors. |
| 8. Reused live token could retire another entry | True if the uniqueness precondition is violated, but Core’s allocator prevents that for live requests. No reachable defect established. |

The live Core ticket bound comes from live requests; C1 permits **32 sockets with one in-flight request each**. The control lane alone does not bound tickets after vendor acknowledgement. The new ownership-based retirement is the correct mechanism.

**Validator and spec verdict**

The configured census, shared budgets, depth and regex-program limits now cover the previous reference-target counterexamples. The ECMA fixes are explicit behavior changes against upstream and are recorded accordingly.

The audit needs one qualification: `[\b]` remains refused by the parser, as independently probed. The amended C1 explicitly permits refusal of expressions VIA cannot compile; this is a supported-language limitation, not silent misvalidation.

The release measurements support removal of the earlier repeated-conversion performance failure and the larger class-program costs. They remain measurements of selected cases, not universal time or memory ceilings. The faulty differential helper weakens the preservation audit but does not invalidate the separate semantic tests.

**The spec diff is ready as contract text.** It consistently resolves the earlier requested/effective inheritance, recording-error, member-cap and runtime-inventory ambiguities. The implementation still violates its explicit steer lifetime clause. I found no new conflict between the rulings and the amended contracts.

The S1 expectation changes in this revision correspond to the declared inheritance representation change. I did not reconstruct historical RED runs. The new regressions have meaningful expectations, with the ordering and oracle exceptions reported above.

**Verification and limits**

Passed independently:

- Release `via-core` build.
- 20 schema tests.
- 22 ordinary intake conformance tests.
- 28 selected failpoint tests.
- Boon’s 20 unit and 3 doc tests.
- Harness-literal and layer guards.

Disposable probes reproduced findings 1 and 3. A separate nonpersistent turn-drop probe returned `NoActiveTurn`.

I did not repeat the complete reported gate, Clippy, musl checks or the full JSON-Schema-Test-Suite. Real vendor execution remained prohibited.

Repository sources, lockfiles and Git state were unchanged; final Git status was clean.