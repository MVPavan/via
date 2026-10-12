**UNSOUND**

No blocker finding. The amendment rows I checked are reflected in the diff, including the adapted A28 schema wording and the deliberate Codex and OpenCode deferrals. Three live instructions still conflict with the approved removal of raw logging or memory permits.

### Important

- `docs/workstreams/rust-foundation/t3/design.md:1776` still requires tests for the removed raw thread, raw I/O failures, and `StoreError::Raw`; `docs/workstreams/rust-foundation/t3/design.md:1059` also retains live `Raw` error classification. This contradicts A41/A46 and T3’s voided raw-failure row. **Smallest fix:** remove those raw-thread and `Raw` assertions from the live classification rules and test row, retaining the SQLite writer cases.

- `.repo-context/verification.md:63` still requires `s1_raw_...` scenarios and raw-log artifacts, while `.repo-context/verification.md:46` selects those tests. This live acceptance gate conflicts with A41/A46 and the amended runtime test matrix. **Smallest fix:** remove `raw` from the selector and scenario list, and require evidence folders in the artifact list.

### Minor

- `docs/specs/runtime-contracts.md:984` says OpenCode caps consume common *memory permits*, while `docs/specs/runtime-contracts.md:1035` says there is no memory pool or byte counter under A43. The row’s vendor-task note does not resolve that instruction. **Smallest fix:** remove the memory-permit clause from this runtime row; leave OpenCode’s buffer bounds to its named task.

### Could not verify

This was a documentation review, so runtime behavior and vendor probes were not verified. `git diff --check`, Markdown link integrity, and the skill catalog check passed; the catalog reported six advisory warnings. Git status remained clean.
