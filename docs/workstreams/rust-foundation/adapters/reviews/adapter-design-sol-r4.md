**UNSOUND.** The delayed-acknowledgement fix works, but the wall-expiry path remains incomplete and introduces conflicting deadline rules.

References below point to revision 4 unless stated otherwise.

| Item | Status | Reason and reference |
|---|---|---|
| R3-1 | fixed | Abort error plus idle establishes acknowledgement independently of assistant/tool completion; delayed-update fixture added. [L934](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L934), [L495](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L495) |
| R3-2 | partly | Vendor interrupts are specified, but the implicit wall stop lacks a complete Core handoff and conflicts with the `force_at` cutoff. [L440](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L440) |
| R3-3 | partly | Unknown inheritance now consistently warns, including without a switch; AC7 still specifies the previous payload shape. [L755](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L755), [L877](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L877) |
| R3-4 | partly | `failure.data` and §8.2’s broader class meaning are defined; C1 §7.2’s vendor-only transition remains outside the amendment audit. [L875](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L875), [L879](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L879) |
| Round-2 9 | fixed | Acknowledgement no longer waits for completed assistants; cleanup follows P7 separately. [L934](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L934) |
| Round-2 N1 | fixed | Delaying final assistant/tool updates past `force_at` no longer delays recognition of an already-established acknowledgement. [L495](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L495) |
| Round-2 N5 | fixed | The warning trigger and default table cover every unverified category, including Codex skills/agents and Claude instruction files. [L1156](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1156) |

The **affected-route split is correct at the design level**: S1’s fake deadline failure invokes force cleanup before returning (Route `crates/via-routes/src/runtime.rs:164` (since moved to `crates/via-routes/src/fake/runtime.rs`)), through [Wire](../../../../../crates/via-wire/src/connection.rs#L340) and [Host](../../../../../crates/via-host/src/host.rs#L1943). Claude is specified to use the private-process lifecycle; Codex and OpenCode need explicit vendor interrupts. However, S1 implements only the fake route. The new assertion that a vendor “cannot outlive the turn” ([L450](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L450)) overstates what this proves: cleanup is requested, and unproven-stop outcomes remain possible under [C1 §7.6](../../../../../docs/specs/via-api-v1.md#L731).

**New defects:**

1. **Important — the implicit wall stop does not reach Core’s disposition logic.**  
   **Location:** [AD4 L444–447](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L444).  
   **Defect/evidence:** The driver creates an implicit stop locally, then returns `Interrupted`. But the cited “as today” Core path populates `cancel` only when `dispose` supplies stop facts. Without a Core-visible order, current `dispose` recognizes only a `RouteError::Deadline` as a wall cancellation ([L153](../../../../../crates/via-core/src/engine/terminal.rs#L153)); an accepted `Interrupted` result instead becomes `failed(vendor_error)` ([L347](../../../../../crates/via-core/src/engine/terminal.rs#L347)). The proposed surface declares no returned wall-stop cause.  
   **Smallest fix:** Specify how Core receives the wall cause and acknowledgement facts, records cancellation, and produces `failed(deadline_wall)` or `unknown`. Preserve existing-order precedence; add wall fixtures both with and without an earlier stop.

2. **Important — wall acknowledgement has conflicting cutoffs.**  
   **Location:** [AD4 L436](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L436), [L445](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L445), [L452](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L452).  
   **Defect/evidence:** The new order sets `force_at = wall`, while accepting acknowledgement until `wall + 3 s`. The existing AD4 rule and test require `unknown` without acknowledgement at `force_at` ([L482](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L482)). C1’s shared-server force-deadline rule is also unchanged ([L732](../../../../../docs/specs/via-api-v1.md#L732)).  
   **Smallest fix:** Declare an explicit wall-specific acknowledgement cutoff and amend the conflicting rules, tests and contract occurrences, or retain `force_at` as the cutoff. Describe private cleanup as a request with evidence-dependent outcomes, rather than guaranteed vendor death.

3. **Important — VO1 loses natural terminals racing cancellation.**  
   **Location:** [VO1 L934](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L934).  
   **Defect/evidence:** Terminal reconciliation now applies only with “no cancel”; cancellation switches exclusively to acknowledgement step (0). If ordinary completion or failure races `/abort`, there may be no `MessageAbortedError`. VO1 supplies no path for retaining that natural terminal, allowing `unknown` despite definite terminal evidence. C1 preserves natural completed/failed terminals ([L728](../../../../../docs/specs/via-api-v1.md#L728)); S1 does likewise ([L202](../../../../../crates/via-core/src/engine/terminal.rs#L202)).  
   **Smallest fix:** Continue recognizing natural terminals while awaiting cancellation acknowledgement. Add completion- and failure-racing-cancel fixtures.

4. **Minor — warning payloads disagree.**  
   **Location:** [AD13 L762](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L762), [AC7 L877](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L877).  
   **Defect/evidence:** AD13 requires one aggregated warning with `data.categories: [...]`; AC7 still defines singular `data {category, requested, effective}`.  
   **Smallest fix:** Update AC7 to the aggregated shape and assert it in the warning regression.

5. **Minor — adapter-side rejection is missing from the state-transition amendment.**  
   **Location:** [AC5 L875](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L875), [audit L957](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L957).  
   **Defect/evidence:** AC5 expands `submit_failed` to adapter-side rejection, but [C1 §7.2 L660](../../../../../docs/specs/via-api-v1.md#L660) still defines that transition exclusively as vendor rejection. The amendment audit omits this occurrence.  
   **Smallest fix:** Add §7.2’s transition row to AC5 and the audit, using the same pre-acceptance rejection meaning.

**Out of scope, noticed:** None added. Unchanged text was inspected only where needed to check changed rules. OD3’s substance was excluded.

**Could not verify:** Implementation or fixture execution, live vendor qualification, or Beads dependencies. The saved OpenCode trace confirms event ordering, not delays beyond `force_at`.

Reviewed the full rev3→rev4 diff. No edits, `bd`, vendor CLIs, models or tests were run. Final branch, HEAD and Git status matched the initial observations.