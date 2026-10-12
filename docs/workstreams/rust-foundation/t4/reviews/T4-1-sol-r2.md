**UNSOUND**

The original write-order finding is resolved: `write_new` now syncs `undecoded.bin` and its folder before the failure message can name the file. I found no new defect in that production fix.

## Findings

- **Important — `crates/via-wire/src/runtime.rs:790`:** The new regression test relies on mode `0300` making the folder unreadable. A test process with DAC override privileges can still open and sync it, so the assertion at line 793 fails even though the fix works. The repo explicitly provides for root or CI test runs. **Smallest fix:** inject a deterministic folder-sync failure in this unit test instead of relying on permissions.

- **Minor — `crates/via-core/src/engine/read.rs:176`:** `logs` treats every file metadata error as “file absent.” If a present evidence file cannot be stated because of `EACCES` or I/O failure, `logs` returns an incomplete `files` list, contrary to the C1 contract. **Smallest fix:** omit only `NotFound`; return a Store error for other metadata failures.

## Could not verify

The read-only review did not rerun the Rust test suites or a privileged test run. `cargo fmt --all --check`, the layer check, `git diff --check`, and Git status passed; the worktree is clean.