**SOUND** for `git diff 10807e1 22517cc`. All three r3 findings are resolved; I found no new defects.

| Finding | Status | Evidence at `22517cc` |
|---|---|---|
| #1 Dynamic-anchor accounting | **Resolved** | [validator.rs:786](../../../../../third_party/boon/src/validator.rs#L786) charges before comparison; [validator.rs:848](../../../../../third_party/boon/src/validator.rs#L848) charges scopes × name units using saturating multiplication. Exhaustion prevents the reference from running. [schema.rs:338](../../../../../crates/via-core/src/schema.rs#L338) covers the original gap. |
| #2 Inherited-bound normalization | **Resolved** | [receipt.rs:539](../../../../../crates/via-core/src/engine/receipt.rs#L539) applies the checked bound for both supplied and inherited requests. [intake.rs:700](../../../../../crates/via-core/src/intake.rs#L700) preserves inheritance. [conformance_intake.rs:1804](../../../../../crates/via-core/tests/conformance_intake.rs#L1804) exercises changed normalization across restart. |
| #3 Acceptance byte accounting | **Resolved** | [runtime.rs:1311](../../../../../crates/via-store/src/runtime.rs#L1311) includes `adapter_version`; [runtime.rs:2663](../../../../../crates/via-store/src/runtime.rs#L2663) verifies its contribution. |

The validator’s previously identified accounting gaps are closed. The updated [audit](../../../../../third_party/boon/VIA-PATCH.md#L55) matches the anchor changes. I found no additional omission in this delta affecting the configured engineering limits: work budgets, shared active depth, subschema/pattern counts or regex program size. Those limits do not establish a universal millisecond or RSS ceiling.

The supplied **spec diff is now consistent with the build within the reviewed scope and existing deferrals**. In particular, Core now consumes `TurnCheck.effective_bound` for inherited requests, and anchor-name work matches the documented proportional accounting.

**New defects:** None found.

Independently verified: **649 tests passed**, with 32 skipped; Clippy, formatting, layer/harness guards and diff whitespace checks passed. Git status remained clean.

**Could not verify independently:** the reported upstream differential, fresh timing/RSS measurements, and the complete failpoint, S1, musl and selector gates. I inspected the gate log: it records the `s1_f19` wall-clock discrepancy and a subsequent **901-test pass at `22517cc`**. The external test fix `07c0e78` was not applied or tested here. No vendor CLI/model or new stress inputs were run.