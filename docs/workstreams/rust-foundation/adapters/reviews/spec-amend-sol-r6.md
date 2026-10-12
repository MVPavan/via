**UNSOUND**

| r5 defect | Status | Evidence |
|---|---|---|
| Missing path, effective-bound and numeric caps | Fixed | [via-api-v1.md:591](../../../../../docs/specs/via-api-v1.md#L591), [runtime-contracts.md:892](../../../../../docs/specs/runtime-contracts.md#L892). A conservative encoded budget is **991,707 bytes**, below 1 MiB by **56,869 bytes**. |
| Spill failure conflicts with disposition rules | Fixed | [via-api-v1.md:581](../../../../../docs/specs/via-api-v1.md#L581), [§7.6:809](../../../../../docs/specs/via-api-v1.md#L809) explicitly place spill failures in the commit-resolution path. |

| Severity | File:line | New defect | Smallest fix |
|---|---|---|---|
| Important | [via-api-v1.md:811](../../../../../docs/specs/via-api-v1.md#L811) | **“A revision whose commit fails is not made” incorrectly includes uncertain outcomes.** The revision may have committed before its reply was lost or the watchdog expired. Runtime §7 requires latching and transaction-outcome resolution; it explicitly forbids assuming non-persistence. The durable turn may already contain the revised result. | Restrict this sentence to a revision **known not committed after applicable retry handling**. For uncertain outcomes, retain runtime §7’s latching and reconciliation rules without asserting that the revision is absent. |

Keeping `unknown` after a confirmed uncommitted revision is consistent with the late-terminal row: no revision or `turn.revised` event became durable. Required retry failures must still latch under [runtime-contracts.md:1043](../../../../../docs/specs/runtime-contracts.md#L1043).

**Could not verify:** Runtime enforcement of the caps, the proposed maximum-envelope conformance test, or revision failure handling.

Saved and live deltas match; `git diff --check` passed. No edits, Git mutations, `bd`, vendor CLI or model runs occurred.