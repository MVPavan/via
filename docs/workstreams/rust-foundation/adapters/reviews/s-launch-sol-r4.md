**SOUND** for `git diff f33e810 bb8d01f`. All four r3 findings are resolved; no new defects found.

| r3 item | Status | Location at bb8d01f and evidence |
|---|---|---|
| #1: cutoff enforcement | Resolved | [via-cli/tests/s_launch.rs:419](../../../../../crates/via-cli/tests/s_launch.rs#L419), also line 430: checks before pinning and immediately before signalling; expired candidates receive `deadline_passed`. |
| #2: partial helper setup | Resolved | [via-cli/tests/s_launch.rs:806](../../../../../crates/via-cli/tests/s_launch.rs#L806), also lines 883–884: guards exist before spawning and immediately own each successful child. |
| #3: unbounded helper waits | Resolved | [via-cli/tests/s_launch.rs:777](../../../../../crates/via-cli/tests/s_launch.rs#L777): bounded kill/reap with failure reporting. Lines 838–843 check the fallback result before bounded reaping. |
| #4: stale cache comment | Resolved | [via-adapters/tests/s_launch.rs:432](../../../../../crates/via-adapters/tests/s_launch.rs#L432): the obsolete path-keyed version claim is removed. |

Verification passed: **614 default workspace tests**, **33 focused tests with failpoints enabled**, both doctests, formatting, and Clippy with and without failpoints. Tracked files and Git state remained unchanged.

I did not rerun the full failpoint, separate S1 or musl gates. Spawn failures and unreapable-child cleanup were inspected rather than fault-injected. The separately tracked f19 intermittency remained outside scope.