**UNSOUND.** Most r1 findings are resolved, but #5 and #9 remain incomplete. The whole chunk also has Important defects in frozen launch configuration, receipt caps, and regex semantics.

All source references below are at **e8a9214**. I reviewed the revision diff first, then the whole chunk and its integration.

**R1 status**

| R1 | Status | Evidence at e8a9214 |
|---|---|---|
| #1 Quadratic ECMA conversion | Resolved | [ecma.rs:14](../../../../../third_party/boon/src/ecma.rs#L14) removes repeated whole-pattern conversion; [compiler.rs:75](../../../../../third_party/boon/src/compiler.rs#L75) charges pattern bytes. Measurements support removal of the reported performance failure. |
| #2 Hardcoded inheritance | Resolved as ruled; broader gap below | [lane.rs:1377](../../../../../crates/via-core/src/engine/lane.rs#L1377) passes the required, typed, frozen effective inheritance into `SessionSpec`. |
| #3 Forced spill before validation | Resolved | [stop.rs:281](../../../../../crates/via-core/src/engine/stop.rs#L281) validates before spilling. |
| #4 Validation lost during final classification | Resolved | [drive.rs:2667](../../../../../crates/via-core/src/engine/drive.rs#L2667) projects the retained validation outcome onto the final terminal. |
| #5 Reply before durable steer | **Partial** | [receipt.rs:762](../../../../../crates/via-core/src/engine/receipt.rs#L762) waits for the commit, but completion can hang or be evicted. Findings 1–2. |
| #6 Decimal `multipleOf` | Resolved | [util.rs:462](../../../../../third_party/boon/src/util.rs#L462) implements decimal comparison without the previous floating division test. |
| #7 Numeric bounds | Resolved | [util.rs:393](../../../../../third_party/boon/src/util.rs#L393) supplies the corrected comparison used by the bounds checks. |
| #8 `uniqueItems` numeric hashing | Resolved | [util.rs:405](../../../../../third_party/boon/src/util.rs#L405) makes numeric keys consistent with numeric equality. |
| #9 Every schema position counted | **Partial** | Ordinary positions are counted, including booleans, but [roots.rs:65](../../../../../third_party/boon/src/roots.rs#L65) admits promoted reference targets without the census. Finding 4. |
| #10 Corrupt frozen values during recovery | Resolved | [recovery.rs:582](../../../../../crates/via-core/src/engine/recovery.rs#L582) fails recovery instead of substituting defaults. The queued corrupt-row path intentionally records `failed(store)`. |
| #11 Process-wide cwd test | Resolved | [conformance_intake.rs:1155](../../../../../crates/via-core/tests/conformance_intake.rs#L1155) isolates it in a child process. Ordinary `cargo test` passed 21/21. |
| #12 Strict instructions shape | Resolved | [intake.rs:277](../../../../../crates/via-core/src/intake.rs#L277) requires exactly one typed `text` or `path` member. |
| #13 Uncertain partial-write message | Resolved for that case | [api.rs:1232](../../../../../crates/via-core/src/api.rs#L1232) preserves uncertainty. A different delivery failure is incorrectly mapped to this case; finding 3. |

**Important findings**

1. **Steer can wait forever after forced delivery teardown.**  
   Sources: [driver.rs:545](../../../../../crates/via-adapters/src/driver.rs#L545), [driver.rs:576](../../../../../crates/via-adapters/src/driver.rs#L576), [fake/driver.rs:838](../../../../../crates/via-adapters/src/fake/driver.rs#L838).

   After vendor acknowledgement, steer waits for its observation-emission notification. Force or cutoff can drop the delivery future without resolving that notification. The waiting steer retains the `Arc` containing its own oneshot sender, so turn teardown does not close the receiver.

   **Reproduced:** fill the observation channel, acknowledge a steer, then force while its observation is blocked. The turn returned `ForceStopped`; steer remained pending beyond a subsequent timeout.

   The registry also lacks request-cancellation cleanup: a cancelled request followed by refusal can leave an entry with no future emission to remove it.

   **Smallest fix:** add turn-scoped teardown that resolves every outstanding emission on every exit, and request-scoped cancellation cleanup. Add a regression covering acknowledgement followed by force/cutoff with a full observation channel.

2. **The 1,024-entry outcome queue can evict a live caller’s committed outcome.**  
   Source: [lane.rs:746](../../../../../crates/via-core/src/engine/lane.rs#L746).

   `pop_front()` assumes the oldest outcome belongs to an abandoned caller. No ownership or cancellation bookkeeping establishes that. Unconsumed outcomes can accumulate while an older live caller is suspended. Once its outcome is evicted, `steer_committed` cannot distinguish eviction from “still pending”; it waits until lane retirement or returns `store_error` despite the event having committed.

   This requires **1,024 unconsumed reports**, not 1,024 ordinary completed calls.

   **Smallest fix:** use bounded per-request completion tickets with cancellation retirement; never discard a live request’s completion merely because it is oldest.

3. **Known delivery followed by observation loss is misclassified as an uncertain partial write.**  
   Sources: [driver.rs:511](../../../../../crates/via-adapters/src/driver.rs#L511), [driver.rs:581](../../../../../crates/via-adapters/src/driver.rs#L581), [receipt.rs:760](../../../../../crates/via-core/src/engine/receipt.rs#L760).

   The driver explicitly describes this case as “the vendor took it, but VIA holds no record.” It nevertheless returns `NotDelivered`, which C1 and C2 define as an input not written whole, with uncertain application. Observation overflow after confirmed delivery does not establish that condition.

   **Smallest fix:** distinguish failure to record an acknowledged delivery from partial-write uncertainty. Map the former through an explicit recording failure, with its C1 mapping documented; retain `NotDelivered` for actual incomplete-write uncertainty.

4. **Reference targets promoted from extension fields bypass compile limits.**  
   Source: [roots.rs:65](../../../../../third_party/boon/src/roots.rs#L65).

   `ensure_subschema` validates and promotes a pointer target but does not run the compile census over its schema positions.

   **Reproduced under the 256 KiB input cap:** `{"$ref":"#/x","x":...}` was accepted with:

   - 2,050 boolean schemas in `x.$defs`;
   - 65 patterns in `x.$defs`;
   - an unused pattern exceeding the regex program limit.

   These violate the amended “every schema position whether referenced or not” limit.

   **Smallest fix:** census each promoted target and its schema descendants using shared counts and canonical-position deduplication. Add all three reference-target regressions.

5. **Effective inheritance loses the frozen switch direction needed by real adapters.**  
   Sources: [frozen.rs:70](../../../../../crates/via-core/src/intake/frozen.rs#L70), [SessionSpec:56](../../../../../crates/via-adapters/src/driver.rs#L56), [plan.rs:381](../../../../../crates/via-adapters/src/plan.rs#L381).

   Both requested `on` and requested `off` become effective `unknown` for an unverified switch. Core passes only that effective state into the reopened driver. The requested direction survives in warning data but is not supplied as launch configuration.

   After a daemon configuration change and restart, a real adapter cannot reliably reconstruct the original session’s switch direction from `unknown`. C2 §6.2 freezes settings per session and places the applied switches in the launch recipe/server key.

   **Smallest fix:** freeze and forward requested inheritance or equivalent launch settings separately from the effective states exposed by status. The r1 ruling fixes the literal hardcoded value but does not fully satisfy the frozen-launch-setting contract.

6. **Resolved model names bypass the receipt member cap.**  
   Sources: [api.rs:257](../../../../../crates/via-core/src/api.rs#L257), [intake.rs:501](../../../../../crates/via-core/src/intake.rs#L501), [terminal.rs:813](../../../../../crates/via-core/src/engine/terminal.rs#L813).

   Only the requested model is checked against the 1 KiB encoded cap. The resolved catalog value enters `Effective` unchecked, although the envelope size calculation assumes both model strings obey that cap.

   **Reproduced:** a short alias resolving to a 2,048-byte model produced a successful receipt containing that model.

   **Smallest fix:** check the resolved model’s encoded size before committing the receipt; test alias expansion.

7. **`\s` and `\S` use an incomplete ECMA whitespace set.**  
   Source: [ecma.rs:148](../../../../../third_party/boon/src/ecma.rs#L148).

   **Reproduced:** `^\s$` rejects U+2009 and U+2028; `^\S$` accepts them. ECMA includes Unicode space separators and line terminators in this class. [ECMA character-class semantics](https://tc39.es/ecma262/multipage/text-processing.html#sec-runtime-semantics-compilecharacterclass).

   **Smallest fix:** translate the complete ECMA whitespace set and its complement; add positive and negative cases.

8. **Dot has Rust regex line-terminator semantics.**  
   Sources: [ecma.rs:190](../../../../../third_party/boon/src/ecma.rs#L190), [util.rs:623](../../../../../third_party/boon/src/util.rs#L623).

   **Reproduced:** `^.$` accepts CR, U+2028 and U+2029. Without dot-all, ECMA dot excludes all four line terminators. [ECMA atom semantics](https://tc39.es/ecma262/multipage/text-processing.html#sec-runtime-semantics-compileatom).

   **Smallest fix:** translate dot to the appropriate ECMA class, accounting for supported flags.

9. **Word boundaries retain Rust’s Unicode word definition.**  
   Sources: [ecma.rs:190](../../../../../third_party/boon/src/ecma.rs#L190), [util.rs:623](../../../../../third_party/boon/src/util.rs#L623).

   **Reproduced:** `\bfoo\b` rejects `αfooβ`, while `\bβ\b` accepts ` β `. These differ from ECMA word-boundary semantics for these expressions. [ECMA assertion semantics](https://tc39.es/ecma262/multipage/text-processing.html#sec-runtime-semantics-compileassertion).

   **Smallest fix:** translate `\b` and `\B` using the ECMA word definition; test both directions around non-ASCII characters.

   Findings 7–9 are inherited upstream defects, not conversion regressions. They still affect this chunk’s admitted schemas. An upstream differential oracle necessarily preserves them.

**Minor findings**

- **Duplicate inheritance keys are accepted.** [plan.rs:327](../../../../../crates/via-adapters/src/plan.rs#L327) deserializes into a `BTreeMap`, which overwrites duplicates before the six-entry check. A seven-member object containing all six categories and duplicate `hooks` was accepted. Use a visitor that rejects repeated categories.
- **The extended-control limit has an off-by-one error.** [ecma.rs:55](../../../../../third_party/boon/src/ecma.rs#L55) performs the final fix without parsing its result. Probes accepted 31 required fixes and rejected 32, although the documented cap is 32. Parse after the last allowed fix; test 31/32/33.
- **Vendored regression tests are outside the normal gate.** [Cargo.toml:4](../../../../../Cargo.toml#L4) excludes boon, while the documented gate runs workspace tests. Add an explicit offline vendored-unit-test step.
- **Recovery loses useful corruption diagnostics.** [recovery.rs:582](../../../../../crates/via-core/src/engine/recovery.rs#L582) collapses the decode error to `ApiError::STORE`; [recovery.rs:108](../../../../../crates/via-core/src/engine/recovery.rs#L108) then prints `store_error: store_error`. Preserve the decode cause and session/turn context.
- **A test inspection accessor is exposed in release builds.** [driver.rs:365](../../../../../crates/via-adapters/src/driver.rs#L365) exposes `spec()` for the new inheritance fixture. Gate the test seam or verify the configuration through the fake’s observable input.

**Worker concerns**

| Concern | Verdict |
|---|---|
| Boolean counting and refusal in unused schema positions | Correct admission policy. Enforcement remains incomplete for promoted reference targets. The extended-control cap also has the boundary error above. |
| Divergence from upstream boon | The numeric behavior fixes are appropriately identified in `VIA-PATCH.md`. The patch is auditable, but upstream equivalence does not establish semantic correctness. Missing full-suite coverage and exclusion of vendored unit tests from the gate remain material limitations. |
| Plain steer `store_error` | Acceptable under the current interpretation: receipt commit-outcome data describes durable intake receipts. Clarify this explicitly in §8.1. |
| Overflow/force answers `NotDelivered` | Incorrect for acknowledged whole delivery; finding 3. Force can also leave the answer pending indefinitely; finding 1. |
| Oldest outcome dropped at 1,024 | Unsafe; finding 2. |
| Lane ends without an outcome → `store_error` | Correct conservative behavior. |
| Corrupt running frozen value prevents startup | Consistent with the ruling and treatment of other corrupt recovery evidence. The diagnostic is deficient, not the refusal. |
| Exactly-six check rejects duplicates | False; independently reproduced. |
| Forced validation outside `FINALIZE_WRITE` | Not itself an Important defect. Validation has a work bound, and [shutdown.rs:272](../../../../../crates/via-cli/src/server/shutdown.rs#L272) imposes the overall shutdown deadline. The work budget is not a millisecond guarantee. |

The new measurements substantiate removal of the repeated-conversion performance failure. They do not establish a universal latency ceiling. Some recorded cases exceed C1’s 256 KiB schema cap, and the work-unit rates remain engineering estimates. The configured depth, validation, metaschema, stack and regex-program limits are useful bounds; finding 4 prevents claiming complete compile-limit enforcement.

**Spec diff: not ready.**

The additions for `unsupported_verb`, the durable steer reply boundary, accepted-instance storage, validation projection and every-position counting are sound directions. Before accepting the diff:

- Define acknowledged delivery whose observation cannot be recorded, separately from uncertain partial writing.
- Clarify frozen requested launch settings versus effective inheritance.
- Document the shared compile-budget charges and the additional extended-control admission limit.
- Finish the runtime target-table cleanup. The amended row still lists implemented closing admission/timestamps, and the surrounding table still lists implemented event columns, steps, `evidence_dir` and `session_ord` as future work. See [runtime-contracts.md:777](../../../../../docs/specs/runtime-contracts.md#L777).
- Record the additional ECMA behavior fixes if findings 7–9 are corrected.

I found no additional unlisted S1 expectation change in this revision. The cwd test is now isolated. I did not reconstruct historical failure-first runs; performance assertions remain dependent on machine load.

**Verification and limits**

Passed independently: release `via-core` build; 18 schema unit tests; 21 intake conformance tests; 21 selected failpoint tests; 15 vendored boon unit tests; harness-literal and layer guards. The bounded probes above supplied the counterexamples.

I did not repeat the complete reported gate, musl checks or Clippy. The full JSON-Schema-Test-Suite was unavailable offline. Real vendor execution was prohibited. Outcome eviction and frozen launch-direction loss were established from source/interface behavior rather than real-vendor end-to-end runs.

No repository files or Git state were changed; final Git status was clean.