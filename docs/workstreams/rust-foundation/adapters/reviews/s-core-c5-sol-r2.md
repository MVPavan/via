**UNSOUND.** Most r1 fixes are implemented, but version recovery, output finalization and the submitting boundary remain incorrect. The validator’s documented engineering limits also have accounting gaps.

References below are at **ae9acc6**, except references explicitly identifying the supplied spec diff.

| r1 | Status | Evidence |
|---|---|---|
| #1 | **Partial**: original error-tree growth is addressed; budget coverage remains incomplete | [validator.rs:163](../../../../../third_party/boon/src/validator.rs#L163) |
| #2 | **Addressed**: draft pin includes embedded schemas | [schema.rs:63](../../../../../crates/via-core/src/schema.rs#L63) |
| #3 | **Partial**: requested/effective values are separated; resume replanning is incomplete | [intake.rs:616](../../../../../crates/via-core/src/intake.rs#L616) |
| #4 | **Addressed**: shared copy routine, frozen text and content-based identity | [receipt.rs:125](../../../../../crates/via-core/src/engine/receipt.rs#L125) |
| #5 | **Addressed**: spawn and frozen resume capability checks | [intake.rs:469](../../../../../crates/via-core/src/intake.rs#L469), [receipt.rs:489](../../../../../crates/via-core/src/engine/receipt.rs#L489) |
| #6 | **Deferred by ruling** to via-jm4.36 | [api.rs:461](../../../../../crates/via-core/src/api.rs#L461) |
| #7 | **Partial**: opening/startup window covered; commit-to-publication gap remains | [drive.rs:2442](../../../../../crates/via-core/src/engine/drive.rs#L2442) |
| #8 | **Addressed**: canonical turn checked under the admission lock | [driver.rs:483](../../../../../crates/via-adapters/src/driver.rs#L483) |
| #9 | **Addressed**: all three discard paths handle delivery | [drive.rs:2231](../../../../../crates/via-core/src/engine/drive.rs#L2231), [drive.rs:2905](../../../../../crates/via-core/src/engine/drive.rs#L2905), [lane.rs:801](../../../../../crates/via-core/src/engine/lane.rs#L801) |
| #10 | **Addressed**: owned context, independent field/route/verb serialization | [intake.rs:330](../../../../../crates/via-core/src/intake.rs#L330) |
| #11 | **Addressed** against the new ruling | [receipt.rs:737](../../../../../crates/via-core/src/engine/receipt.rs#L737) |
| #12 | **Addressed** for frozen effective values | [recovery.rs:564](../../../../../crates/via-core/src/engine/recovery.rs#L564) |
| #13 | **Partial**: acceptance commit is atomic; recovery drops its instance | [sql.rs:1340](../../../../../crates/via-store/src/runtime/sql.rs#L1340), [recovery.rs:576](../../../../../crates/via-core/src/engine/recovery.rs#L576) |
| #14 | **Addressed** for live dispatch and queued restart handoff | [drive.rs:2472](../../../../../crates/via-core/src/engine/drive.rs#L2472), [recovery.rs:249](../../../../../crates/via-core/src/engine/recovery.rs#L249) |
| #15 | **Addressed** | [receipt.rs:198](../../../../../crates/via-core/src/engine/receipt.rs#L198) |
| #16 | **Partial**: every retained value is checked, but later classification can lose its warning | [drive.rs:874](../../../../../crates/via-core/src/engine/drive.rs#L874) |
| #17 | **Addressed**: requested daemon cases added | [conformance_daemon.rs:306](../../../../../crates/via-cli/tests/conformance_daemon.rs#L306) |
| #18 | **Addressed**: deterministic unit synchronization; integration outcomes retained | [receipt.rs:718](../../../../../crates/via-core/src/engine/receipt.rs#L718), [tests.rs:3660](../../../../../crates/via-core/src/engine/tests.rs#L3660) |
| #19 | **Partial**: Store comments corrected; spec inconsistencies remain | [runtime.rs:205](../../../../../crates/via-store/src/runtime.rs#L205) |
| #20 | **Addressed**: both requested extractions made | [output.rs:13](../../../../../crates/via-core/src/engine/output.rs#L13), [frozen.rs:1](../../../../../crates/via-core/src/intake/frozen.rs#L1) |

Remaining and newly exposed defects:

1. **Important — Some validation work bypasses the unit budget.**  
   [util.rs:537](../../../../../third_party/boon/src/util.rs#L537) uses `HashedValue` for arrays longer than 20. Its collision comparisons call unbudgeted `equals` at line 557; charging the input’s nodes once does not charge these comparisons. Separately, [validator.rs:296](../../../../../third_party/boon/src/validator.rs#L296) charges the number of `dependentRequired` entries, but their inner required-name lists are scanned at line 442 without corresponding charges.  
   **Smallest fix:** budget hash collision comparisons and actual dependency-name scans, stopping immediately on exhaustion. The recorded ordinary `uniqueItems` cases do not establish coverage of these paths.

2. **Important — Nested validation resets the depth counter.**  
   [validator.rs:1025](../../../../../third_party/boon/src/validator.rs#L1025) shares the unit budget but starts another root evaluation, whose depth is reset to zero at line 92. `propertyNames` therefore can add another evaluation chain while the original chain remains on the stack. The documented depth 512 is not a bound on total active evaluation nesting.  
   **Smallest fix:** track active depth in the shared budget, or carry the current depth into nested validation. Size the stack against that actual bound.

3. **Important — Compile limits do not cover the metaschema validation phase.**  
   [draft.rs:183](../../../../../third_party/boon/src/draft.rs#L183) still uses unlimited, detailed validation and clones its errors. Resource collection and that validation occur in [roots.rs:91](../../../../../third_party/boon/src/roots.rs#L91), before compiled-subschema counting limits the compilation loop.  
   **Smallest fix:** bound the preliminary metaschema/resource work too, using a bounded, detail-free check where appropriate. The supplied compile measurements support the measured cases; they do not establish that this uncovered phase meets the stated engineering target.

4. **Important — Resume normalization ignores refusals and omits relevant context.**  
   [intake.rs:622](../../../../../crates/via-core/src/intake.rs#L622) replans with harness, requested model, bound and cwd, but defaults vendor options and effort. Line 633 consumes `effective_bound` without examining `plan.refusals` or verifying the frozen route. A C2 plan can contain an effective bound alongside another refusal.  
   **Smallest fix:** supply the actual turn context, enforce the frozen route and process refusals. Prefer resolving the C2 gap with normalization against `SessionRef`, rather than relying on a fresh spawn-style plan.

5. **Important — The submitting publication race is narrowed, not eliminated.**  
   [drive.rs:2410](../../../../../crates/via-core/src/engine/drive.rs#L2410) awaits the submission commit; only later does line 2444 publish the watch. Another task can read the committed running row before that future resumes, then [receipt.rs:699](../../../../../crates/via-core/src/engine/receipt.rs#L699) returns `no_active_turn`. The new hold tests the interval after publication.  
   **Smallest fix:** serialize steer selection with the submission commit and watch publication, releasing that serialization before waiting for acceptance.

6. **Important — Recovery discards the newly recorded instance.**  
   [recovery.rs:576](../../../../../crates/via-core/src/engine/recovery.rs#L576) constructs a default vendor record; the unfinished-turn read does not retrieve the new version columns. Recovery writes an `unknown` envelope with null/untested values, and status subsequently prefers that envelope over the recorded columns. A tested accepted turn therefore loses known version facts after restart.  
   **Smallest fix:** read and preserve this turn’s recorded instance when assembling its recovered envelope.

7. **Important — Output validation precedes final failure classification.**  
   [drive.rs:874](../../../../../crates/via-core/src/engine/drive.rs#L874) handles invalid output while the provisional state is completed. Lines 895 and 898 can subsequently replace the failure with `store`. The final non-completed envelope then lacks the required `structured_output_invalid` warning.  
   **Smallest fix:** apply the validation result after final text/evidence/Store classification, preserving the primary failure and emitting the warning when appropriate. The forced-finalization path already uses the better ordering.

8. **Important — The spec amendment contradicts the existing failure-data definition.**  
   [via-api-v1.md:635](../../../../../docs/specs/via-api-v1.md#L635) still says `failure.data` exists **only** for adapter-side `submit_failed`. The supplied diff adds it for `structured_output_invalid` without updating that definition.  
   **Smallest fix:** extend the field definition with the new class, closed reason values and applicable cap.

9. **Minor — Storage and measurement documentation remain inconsistent.**  
   The runtime amendment leaves the introduction saying **Schema v7**, although it describes v8, and leaves `turns.effective` described as the receipt’s effective object. [schema.rs:9](../../../../../crates/via-core/src/schema.rs#L9) also records roughly 333k units and under 25 MiB, whereas the supplied measurement file reports **381,056 units** and **27,860 KiB** peak.  
   **Smallest fix:** correct these descriptions and label measurements as results for the recorded cases.

10. **Minor — Store admission accounting omits the new instance string.**  
    [runtime.rs:1303](../../../../../crates/via-store/src/runtime.rs#L1303) counts acceptance command bytes without `instance.vendor_version`. The observation channel accounts for it, but the Store command budget does not.  
    **Smallest fix:** include the new retained string in acceptance command accounting.

The configured validator limits are clearly named, and the draft pin, compiled-schema/pattern counts, regex program limit and disabled lazy DFA are implemented. The measurement file reports validation cases up to about **55.5 ms**, compile cases up to **69.2 ms**, and validation peak RSS of **27,860 KiB**. Those results support the sampled cases. They do not establish a general worst-case guarantee while the accounting gaps above remain. Regex weight is explicitly a syntax-derived estimate, not a demonstrated bound on expanded automaton work.

The concerns resolve as follows:

| Concern | Verdict |
|---|---|
| 1 | **Acceptable.** Deterministic unit notification proves entry into the wait; integration outcome tests need not duplicate that hook. |
| 2 | **Acceptable.** The driver test removes the incidental vendor-ID protection and tests the owning admission boundary. |
| 3 | **Incomplete.** Fresh planning needs the context, refusal and route checks in finding 4. |
| 4 | **Acceptable for valid immutable capabilities.** An unsupported frozen resume capability cannot have a legitimately accepted prior resume key. Replay-first ordering would nevertheless be clearer. |
| 5 | **Acceptable.** The same copy routine performs the mutation check; a duplicate instructions-specific race test is unnecessary. |
| 6 | **Acceptable.** `Named.delivery` is serialized independently and matches the ruling. |
| 7 | **Acceptable.** Declarative fake refusals exercise the generic mapping without a Core release hook. |
| 8 | **Acceptable caller-visible restrictions**, but documentation must distinguish counted compiled schemas/patterns and measured performance from general guarantees. |
| 9 | **Acceptable ownership choice.** Work runs off the executor in the bounded task pool. Late revision handling remains deferred. |
| 10 | **Sound.** Preserve upstream attribution. I verified the archive checksum and all 40 vendored archive files at the initial vendor commit. |
| 11 | **Acceptable Option.** `None` is appropriate for an instance with no handshake, including the ordinary fake. |
| 12 | **Acceptable pre-release transition.** Code consistently uses v8 and refuses older Stores; the runtime version label needs correction. |
| 13 | **Agree with the recovery proposal.** Use the turn’s recorded instance. Before acceptance and without recorded handshake facts, untested/null is reasonable and should be explicit in C1. |
| 14 | **Reject the per-turn-hello interpretation.** Later turns on the same persistent instance must report its already-read connection handshake. This is not a cached version from another instance. The fake’s per-turn-process emulation does not prove that persistent-server behavior. |

The **spec diff is not ready to approve**. Its steer binding/error mappings, instructions-copy rule, output disposition and acceptance transaction mostly match the rulings. Besides findings 8–9, clarify:

- acceptance carries the handshake of the current **connection instance**, even when read on an earlier turn;
- terminal evidence may expose a handshake version despite rejection before acceptance;
- `steer_failed` does not always mean “not applied”—`delivery: uncertain` must remain uncertain;
- draft declarations mean declarations in schema positions, rather than `$schema` appearing inside instance literals.

Independent verification passed: **636 workspace tests, 32 skipped**, formatting, layer guard and harness-literal guard. The supplied gate ends with exit 0. Git remained at `ae9acc6` with no source or Git changes.

I did not rerun the timing/RSS measurements, the differential suite, full failpoint gate or musl qualification. No new stress inputs, vendor CLI or model were run. The remaining race and accounting findings are source-based; I am not claiming a newly measured crash or performance overrun.