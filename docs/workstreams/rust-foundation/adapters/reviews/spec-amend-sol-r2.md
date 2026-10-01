**UNSOUND**

| r1 finding | Status | Evidence |
|---|---|---|
| 1. Rejection evidence missing | Fixed | Evidence-bearing `Rejected` and updated uses: [adapter-contract.md:208](../../../../../docs/specs/adapter-contract.md#L208), :219, :598, :806 |
| 2. Mismatch outcome unspecified | Fixed | Explicit `Err(ResumeMismatch { evidence })`: [adapter-contract.md:257](../../../../../docs/specs/adapter-contract.md#L257) |
| 3. Acceptance/terminal preservation | Partly | Three cases are specified at [adapter-contract.md:256](../../../../../docs/specs/adapter-contract.md#L256), but §4 still mandates unconditional failure at :384 |
| 4. Retirement health trigger | Fixed | [adapter-contract.md:209](../../../../../docs/specs/adapter-contract.md#L209) matches the launched/persistent, nonquiescent-or-journal-uncertain conditions |
| 5. Inventory overclaim | Fixed | Reported paths and unverified completeness agree: [codex.md:264](../../../../../docs/specs/vendors/codex.md#L264), :266 |
| 6. Pagination exhaustion | Fixed | Protocol failure, no cache and no partial catalog: [codex.md:154](../../../../../docs/specs/vendors/codex.md#L154) |

Two defects remain from the fixes:

| Severity | File:line | Defect | Smallest fix |
|---|---|---|---|
| Important | [adapter-contract.md:208](../../../../../docs/specs/adapter-contract.md#L208) | “Otherwise” assigns `{exit: None, cleanup: Quiescent}` to every turn that did not launch a **per-turn** process, including work on a persistent server. That erases server-loss evidence and can falsely declare quiescence despite unresolved reported tools. This contradicts §2’s cleanup table (:288–291), §4.1 (:435–442), Codex §6 and OpenCode §6. | Restrict the default to genuine no-launch/no-submission cases. Preserve server/tool evidence under the existing route-specific cleanup rules; retain the complete-journal requirement for no-launch quiescence. |
| Important | [adapter-contract.md:264](../../../../../docs/specs/adapter-contract.md#L264), [adapter-contract.md:384](../../../../../docs/specs/adapter-contract.md#L384) | The new retained-terminal case preserves the turn and fails health only, yet every mismatch still emits `resume.mismatch`, whose observation-table entry unconditionally commits `failed(resume_mismatch)`. Following that table can overwrite the preserved result. | Qualify the table entry: fail the turn only when no valid terminal was retained; otherwise preserve its result and fail driver health. Core must reconcile the observation with `TurnEnd` before disposition. |

**Could not verify:** live vendor behavior, pagination across multiple pages or at exhaustion, complete configuration inventory, or runtime handling of the new mismatch cases. The recorded Codex fixtures contain reported AGENTS.md paths and only null pagination cursors.

The saved r2 delta matches the live spec diff; `git diff --check` passed. No edits, Git mutations, `bd`, vendor CLI or model runs were performed.