**UNSOUND**

| r4 defect | Status | Evidence |
|---|---|---|
| Complete envelope budget unspecified | Partly fixed | [via-api-v1.md:588](../../../../../docs/specs/via-api-v1.md#L588) adds substantial caps, but the completeness claim at :597 remains unsupported. |
| Spill failure undefined | Partly fixed | [via-api-v1.md:581](../../../../../docs/specs/via-api-v1.md#L581) defines `store` and null references; disposition precedence still needs reconciliation. |
| Publication ordering undefined | Fixed | [via-api-v1.md:579](../../../../../docs/specs/via-api-v1.md#L579) requires whole-file and folder sync before initial/revision commit, plus immutability. |

| Severity | File:line | Remaining/new defect | Smallest fix |
|---|---|---|---|
| Important | [via-api-v1.md:595](../../../../../docs/specs/via-api-v1.md#L595) | **Not every envelope field has a stated bound.** `evidence.folder`, `final_text_file.path` and `structured_output_file.path` have no encoded-byte cap here; limiting `cwd` and `transcript` does not limit managed evidence paths. The receipt cap also does not explicitly cap `bound.effective`. Several numeric ranges remain implementation assumptions rather than stated C1 limits. | Specify or explicitly reference the remaining encoded caps, including derived filename overhead, effective bound and numeric ranges. Reject oversized configured roots before accepting work; do not truncate file paths. |
| Important | [via-api-v1.md:581](../../../../../docs/specs/via-api-v1.md#L581), [§7.6:803](../../../../../docs/specs/via-api-v1.md#L803) | **Spill failure introduces an unreconciled disposition exception.** §5 mandates `failed(store)`, while §7.6 first selects natural terminals and restricts `failed(store)` to the referenced resolution cases. The late-terminal revision row likewise says to revise to the vendor’s state. | Explicitly include structured-output create/write/sync failure in the storage-resolution path and state its precedence for initial results and revisions, preserving cancel/cleanup evidence. |

A conservative in-memory budget totaled **990,088 bytes**, including maximum warning data, escaped leftover names, and both file references alongside inline values. It fits **conditionally**: the calculation supplies additional 4 KiB encoded path caps and fixed-width numeric assumptions absent from this amendment.

`vendor_stop_reason` already has C2 A1’s short-field cap. Current code bounds `exit` with `i32`, usage/events/counters with `u64`, and cost with `f64`; these are not unbounded implementation fields.

A storage failure does **not** contradict “no turn fails for the size of its result”: successful spilling handles size. §7.5’s crash recovery remains `unknown`, without resend.

**Could not verify:** The unconditional contract maximum, proposed conformance test, or runtime failure/revision behavior. Saved and live deltas match; `git diff --check` passed. No edits, Git mutations, `bd`, vendor CLI or model runs occurred.