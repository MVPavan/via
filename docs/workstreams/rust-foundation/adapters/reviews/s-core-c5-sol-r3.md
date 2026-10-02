**UNSOUND.** Most r2 fixes are complete. Two Important gaps remain: validation undercharges dynamic-anchor operations, and resume ignores the checked effective bound when inheriting a bound.

All source references below are at `10807e1`.

| r2 item | Status | Evidence |
|---|---|---|
| #1 Budget gaps and audit | **Partial** | Collision comparisons and dependency scans are charged in [util.rs:548](../../../../../third_party/boon/src/util.rs#L548) and [validator.rs:1147](../../../../../third_party/boon/src/validator.rs#L1147). Anchor-name accounting remains incomplete; finding 1 below. |
| #2 Shared evaluation depth | **Resolved** | [validator.rs:183](../../../../../third_party/boon/src/validator.rs#L183) uses the shared active depth, including nested validation. Existing tests cover combined nesting and exhaustion. |
| #3 Metaschema budget | **Resolved** | [draft.rs:190](../../../../../third_party/boon/src/draft.rs#L190) performs detail-free checks using the compiler’s shared budget; VIA configures it in [schema.rs:77](../../../../../crates/via-core/src/schema.rs#L77). |
| #4 Resume normalization | **Partial** | `TurnCheck` replaces bound replanning, but [receipt.rs:542](../../../../../crates/via-core/src/engine/receipt.rs#L542) consumes its bound only for an explicitly supplied bound; finding 2 below. |
| #5 Submission/publication race | **Resolved** | [drive.rs:2414](../../../../../crates/via-core/src/engine/drive.rs#L2414) holds selection serialization through publication. [receipt.rs:706](../../../../../crates/via-core/src/engine/receipt.rs#L706) releases it before awaiting acceptance. The regression test controls the gap deterministically. |
| #6 Recovered instance | **Resolved** | [runtime/sql.rs:2093](../../../../../crates/via-store/src/runtime/sql.rs#L2093) decodes the recorded instance; [recovery.rs:623](../../../../../crates/via-core/src/engine/recovery.rs#L623) passes it into the recovered envelope. |
| #7 Validation after classification | **Resolved** | [drive.rs:902](../../../../../crates/via-core/src/engine/drive.rs#L902) checks output after final-text and Store classification. The new Store-failure regression passed. |
| #8 Persistent handshake / spec correction | **Resolved within the ruling’s scope** | [conformance_driver.rs:1041](../../../../../crates/via-core/tests/conformance_driver.rs#L1041) covers the fake’s persistent emulation. Actual connection-handshake retention remains an obligation of the future persistent adapters. The `failure.data` amendment resolves the accompanying spec defect. |
| #9 Measurement comments | **Resolved** | [schema.rs:10](../../../../../crates/via-core/src/schema.rs#L10) distinguishes recorded results from guarantees and corrects units and RSS. |
| #10 Acceptance bytes | **Resolved for the requested instance field** | [runtime.rs:1307](../../../../../crates/via-store/src/runtime.rs#L1307) counts `instance.vendor_version`. A separate omission remains; finding 3 below. |

**Findings**

1. **Important — dynamic-anchor operations omit the anchor’s byte cost.**  
   [validator.rs:785](../../../../../third_party/boon/src/validator.rs#L785) compares schema-controlled anchor strings without charging their length. [validator.rs:837](../../../../../third_party/boon/src/validator.rs#L837) charges one unit per scope, while line 846 performs a `HashMap<String, …>` lookup that hashes the complete anchor name at each scope.

   Anchor names can be long within the schema’s byte cap, and these operations repeat across value items and evaluations. Consequently, substantial byte work remains outside the configured proportional accounting. The audit’s claim of completeness in [VIA-PATCH.md:46](../../../../../third_party/boon/VIA-PATCH.md#L46) is too strong.

   **Smallest fix:** charge anchor-string comparison and each dynamic-anchor lookup proportionally to name length, stopping before either operation when the budget is exhausted. Update the audit. This finding follows from source inspection; I did not generate a new stress input or measure its runtime.

2. **Important — inherited bounds bypass `TurnCheck` normalization.**  
   [receipt.rs:542](../../../../../crates/via-core/src/engine/receipt.rs#L542) uses `checked.effective_bound` only when `overrides.bound()` returned a supplied value. With an omitted bound, it keeps the previous effective value unchanged.

   Nevertheless, [intake.rs:754](../../../../../crates/via-core/src/intake.rs#L754) passes the inherited requested bound to `check_turn`, whose result expressly describes what the current route will apply. After a compatible adapter change that normalizes that request differently, Core discards the new result and freezes/reports the old effective bound. The new integration test covers an explicit bound, so it misses this branch.

   **Smallest fix:** process the checked bound for inherited requests too, retaining their requested value and `inherited` flag. If preserving the previous effective bound exactly is required, refuse a differing checked result rather than silently discard it.

3. **Minor — acceptance accounting still excludes `adapter_version`.**  
   [runtime.rs:1307](../../../../../crates/via-store/src/runtime.rs#L1307) omits that separately stored string. The assertion that it is always VIA’s short version is not true of this build: [driver.rs:321](../../../../../crates/via-adapters/src/driver.rs#L321) returns the fake profile’s configurable `String`.

   **Smallest fix:** add `record.adapter_version.as_ref().map_or(0, String::len)` to acceptance bytes. This omission predates this delta; it is a remaining accounting defect, not a regression introduced by `b9378ba`.

**Validator engineering limits**

The configured limits are documented: 1,000,000 validation units, a separate shared 1,000,000-unit metaschema budget, active depth 512, 2,048 subschemas, 64 patterns, a 1 MiB regex program limit, and a 16 MiB scoped-thread stack. The patch and existing tests support the collision, dependency, shared-depth and metaschema fixes. Finding 1 prevents accepting the accounting audit as complete.

The measurement file (`scratchpad/execution/s-core/c5r2-budget-measure.txt:7`) supports the stated range **for its recorded cases**:

- Slowest recorded validation: **53.2 ms**.
- Largest recorded process RSS: **36,864 KiB**, including value construction.
- The realistic value needed **381,056 units**; the large schema needed **324,795 metaschema units**.
- The nested-stack probe supports the configured margin for that tested shape.

These observations do not establish a universal time or memory ceiling. The corrected source comments appropriately say so.

**Worker concerns**

| Concern | Verdict |
|---|---|
| 1. Intermediate Clippy failure | Acceptable. The reviewed final commit passes Clippy; the intermediate failure does not make its final state defective. |
| 2. Coverage-only tests | Accurate characterization. The schema-data and fake-handshake tests cover already-correct behavior. The resume test also lacks behavioral discrimination for finding 2. |
| 3. Compile-level RED for #4 | Valid evidence of the API change, but insufficient evidence that Core consumes the result correctly on every branch. |
| 4. Measurement caveats | Correct and material. Keep per-case timings and process RSS qualified as recorded observations. |
| 5. Uncounted `adapter_version` | Not justified by a universal short-version invariant in this build; finding 3. |

**Spec diff**

The amendments correctly resolve the prior `failure.data`, uncertain steer delivery, schema-position, schema-v8, internal frozen-value and connection-handshake wording problems. `TurnCheck` is a sensible contract repair.

The diff is **not ready for a claim of consistency with this build** because finding 2 violates its effective-bound promise, and finding 1 leaves the documented proportional accounting incomplete. I found no additional substantive spec defect. The persistent-adapter obligation is explicit and appropriately remains with those adapters.

**Verification and limits**

Independently run at `10807e1`: **647 default tests passed**, the new finalization failpoint test passed, Clippy, formatting, layer and harness guards passed, and the committed diff passed whitespace checking. Git status remained clean.

I read the recorded full gate, including its 899-test failpoint and musl results, but did not rerun those full selections. I did not independently reproduce the timing/RSS measurements, uncommitted stack probe or upstream differential run. No vendor CLI/model or new stress inputs were run. Actual Codex/OpenCode persistent connections remain unverified in this slice.