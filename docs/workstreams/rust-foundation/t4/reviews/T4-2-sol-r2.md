**UNSOUND** for the whole T4-2 chunk. The fix addresses both round 1 findings during normal operation: timed-out steps remain in a capped JoinSet, and `BlobWriter::drop` schedules unlink off a Tokio worker. The shutdown path still has a correctness defect.

## Findings

- **Important — `crates/via-store/src/runtime.rs:1300`:** `Store::drop` reports a nonzero blob drain count only to stderr. The daemon’s `crates/via-cli/src/server/shutdown.rs:119` sees `drop_blocking` finish and can report `clean` with exit 0 while blob work is still running. For example, a step that remains stalled past the one second drain is logged, then omitted from `pending_joins`. Propagate the drain result to final shutdown, count it as pending work, and select `incomplete`. Add a shutdown test for that case.

- **Minor — `crates/via-store/src/runtime.rs:1303`:** The new diagnostic writes directly to stderr after startup. The runtime contract routes daemon diagnostics to `via.log` after startup; an auto-starting CLI may no longer read that stderr pipe, so this report can be lost. Pass the count to the daemon shutdown diagnostic path instead of writing it from Store.

## Could not verify

I did not rerun runtime tests or reproduce a stalled filesystem. The new timeout and cap tests exercise their stated paths, but neither checks the final shutdown disposition; `blob.step.stall` is present in `POINTS`. `cargo fmt --all --check`, `scripts/check-layers.py`, `git diff --check`, and final Git status passed; the worktree is clean. The CLI’s absolute `drop_blocking` deadline bounds its wait even when the one second drain follows a slow writer join, but that does not correct the false clean result above.