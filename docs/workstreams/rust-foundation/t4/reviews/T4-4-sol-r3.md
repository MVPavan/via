**SOUND**

**Findings:** None. The round-2 important finding is resolved. In `crates/via-core/src/engine/stop.rs:402`, `forced_terminal` recognizes either a `Protocol` stop cause or a recorded token refusal. Store failure still overrides protocol, which overrides idle and cancel.

The new test holds Core before the refusal, waits for the forced vendor exit, then lets Core record the refusal. That exercises the forced handoff: the saved RED run committed `turn.ended` without `failed(protocol)`, while the GREEN run passed. With earlier findings cleared, I found no new defect in `git diff c837490..HEAD` against T4-4’s plan and design.

**Could not verify:** I did not rerun the runtime gate in this read-only review. The saved gate log reports 315/315 default and 481/481 failpoint tests passed. `git diff --check` passed for both requested ranges; Git status is clean.