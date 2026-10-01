**SOUND** for `67d2a09..16f3aa3`. The reported defect is fixed per the ruling. No new defects found.

| Defect | Status | Evidence |
|---|---|---|
| `expect → await_eof` falsely rejects immediate close | **Fixed** | [input.rs:163](../../../../../crates/via-fake-agent/src/replay/input.rs#L163) returns the line’s arrival stamp; [replay.rs:512](../../../../../crates/via-fake-agent/src/replay.rs#L512) returns it as successful step completion. EOF published afterward passes the existing ordering check. |

The following `within_ms` now uses that arrival: [replay.rs:311](../../../../../crates/via-fake-agent/src/replay.rs#L311) assigns the returned completion to `previous`, which reaches the unchanged limit calculation.

Match, `absent`, missing-capture and capture-budget failures all occur **before** `Ok(arrived)`. They still abort replay with failure; the timestamp cannot turn an invalid line into a successful step. Strict trailing-input checks remain unchanged, and extra queued lines still fail.

The tests provide useful regression coverage:

- [input.rs:386](../../../../../crates/via-fake-agent/src/replay/input.rs#L386) deterministically rejects replacing the returned arrival with processing time.
- [tests/replay.rs:1028](../../../../../crates/via-fake-agent/tests/replay.rs#L1028) exercises the complete immediate-close path. Its ability to expose the old implementation depends on scheduling; the supplied RED 20/20 is reported evidence, not a repeat I performed.
- The unit test does not exercise `run_step`, and there is no new deterministic test chaining two expects to verify the following `within_ms` anchor. That connection is verified from source.

**Other step completions:** the same event-versus-processing-time mechanism remains in `await_signal` ([replay.rs:522](../../../../../crates/via-fake-agent/src/replay.rs#L522)): signal then immediate close can publish EOF before signal handling completes. `await_eof` also returns processing time, so consecutive `await_eof` steps reject the cached earlier EOF. These are pre-existing consequences of the explicitly preserved completion rules, outside this fix. Emit already stamps before writing; delay and exit retain their prescribed operation-end instants.

**New defects:** none; severity/smallest fix not applicable.

**Verified:** `cargo nextest run --locked -p via-fake-agent` — **52 passed, 0 skipped**. Scoped diff check passed; Git remained clean at `16f3aa3`.

**Could not verify:** forced scheduling, temporary mutation RED runs, or the reported repeat counts and broader gate. The other-step consequences above were source-traced, not dynamically reproduced. No source edits, Git changes, Beads, vendor CLI or model runs.