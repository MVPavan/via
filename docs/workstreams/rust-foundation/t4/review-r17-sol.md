**UNSOUND**

The round-16 fixes address B1–B3, I5–I6, and M7. I4’s boundary ordering is fixed, but a terminal-row race remains. Storing retry identity as only `identity_len` and `identity_sha256` is consistent with the round-17 decision; I found no separate defect in that choice.

### Findings

**Blocker**

- `docs/workstreams/rust-foundation/t4/design.md:1106` — A new prompt-file request hashes the file for identity before key lookup, then copies it in a second pass. A change between passes escapes the copy’s before/after `fstat` check. VIA can commit a prompt blob whose bytes differ from the stored retry identity, so a retry using the admitted prompt conflicts. **Smallest fix:** look up the key first; for new work, derive the identity from the same pass that writes the blob.

**Important**

- `docs/workstreams/rust-foundation/t4/design.md:1121` — The up-to-10-second copy holds `admission`. Other receipts queue behind it, but so do `close` and daemon stop (`crates/via-core/src/engine/stop.rs:114`); several copies can delay those controls far beyond one copy’s bound. That is unacceptable for the shared lock. **Smallest fix:** copy outside the lock, then reacquire it and repeat the key lookup and admission checks before committing. This also permits the single-pass identity fix above.
- `docs/workstreams/rust-foundation/t4/design.md:400` — `status` may read the terminal transaction’s last step row while the matching in-memory `Running` entry still exists: the current drive clears it *after* terminal commit (`crates/via-core/src/engine/drive.rs:699`). The response can show a completed turn with non-null progress whose `current_step` already has a row, contrary to the stated snapshot rule. **Smallest fix:** return `progress: null` when the Store-selected turn is terminal; test the interval between terminal commit and `finish_running`.

**Minor:** none.

### Could not verify

Round 17 changes documents only. No implementation or vendor probe establishes the proposed behavior; I did not run Rust gates. `git diff --check` and relative-link checks for both changed documents passed. The worktree remained clean, with no files or Git state changed.