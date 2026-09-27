# S1 Task 2: session continuity, queues and retry safety (`via-jm4.7.6`)

Plan: `../s1-plan.md` §4 Task 2 (F8, F10, F13, F14, F17, F28). Two workers
in parallel on separate branches, after the `engine.rs` split (W5-S).
Rules: `../w1/common.md`, except reports go to `reports/<task id>.md` here.
Review loop: `../cloud-and-local.md` §6.

| Task | Scope | Model | Brief |
|---|---|---|---|
| T2-A | `test-failpoints` controller; F8, F10 | Opus 5.5 high | `a.md` |
| T2-B | Multi-turn sessions, `resume`, per-session queue, retries; F13, F14, F17, F28; `wait.timeout_ms`; harness gaps | Opus 5.5 medium | `b.md` |

Shared files: `via-core/src/engine/drive.rs` and Store's spawn/submission
commit path. T2-A's hunks there are failpoint call sites only; T2-B owns
the logic. Each names its shared hunks in its report.
