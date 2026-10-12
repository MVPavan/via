**Verdict: SOUND (merge).** No blockers found in the round-2 change.

1. The count is bounded: the query seeks through the `anchors_unproven` partial index and applies `LIMIT` before counting (`crates/via-store/src/runtime/anchor.rs:231`); Core passes the four-slot pool (`crates/via-core/src/engine/recovery.rs:149`). Because unidentified holdings are not released during admission, a count saturated at four has the same permit effect as the exact count.

2. Schema v3 is consistent. The open path checks `user_version` through a read-only connection before opening for writes; v1/v2 receive the named recreate error (`crates/via-store/src/runtime.rs:29`, `crates/via-store/src/runtime.rs:603`). The test checks that both files’ bytes remain untouched. Runtime §6 accurately describes v3 and the index. I found no live code or contract dependency on v2; remaining v2 references are historical reports.

**Checks:** Read-only inspection of `e1221f9..9a1c34e`, referenced contracts, and affected callers; `git diff --check` passed. I did not run tests or `bd`.