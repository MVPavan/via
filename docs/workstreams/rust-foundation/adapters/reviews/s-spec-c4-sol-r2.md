**SOUND** — fix check of `096f817..HEAD`.

| Item | Fixed, partly or not | Reason | File:line |
|---|---|---|---|
| Finding 1: credential prohibition | Fixed | Exception permits only transient scan reading. Copying, hashing, extraction and auth-file access remain forbidden. | [opencode.md:280](../../../../../docs/specs/vendors/opencode.md#L280) |
| Finding 2: pending close report | Fixed | Close now references the best-effort report under C2 §4.2/runtime §5. | [opencode.md:693](../../../../../docs/specs/vendors/opencode.md#L693) |
| Finding 3: exclusive launch-marker wording | Fixed | Both instances permit matching solely by the report-only leftover scan. | [opencode.md:274](../../../../../docs/specs/vendors/opencode.md#L274), [coding-style.md:175](../../../../../.repo-context/coding-style.md#L175) |
| Three stale design lines | Fixed | All now identify option A/report-only marker detection. | [design.md:89](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L89), [design.md:148](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L148), [design.md:719](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L719) |

No pending **detection** wording remains in the requested scope.

**New defects:** None found.

**Could not verify:** Runtime implementation and AD20 test behavior; outside this documentation fix check.

`git diff --check` passed; broken Markdown links: 0; skill catalog passed with advisory warnings. Git status remained clean. No files or Git state changed; no `bd`, vendor CLI or model ran.