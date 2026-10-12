# S1 Task 2: session continuity, queues and retry safety (`via-jm4.7.6`)

Plan: `../s1-plan.md` §4 Task 2 (F8, F10, F13, F14, F17, F28). Two workers
in parallel on separate branches, after the `engine.rs` split (W5-S).
Rules: `../w1/common.md`, except reports go to `reports/<task id>.md` here.
Review loop: `../cloud-and-local.md` §6.

| Task | Scope | Model | Brief |
|---|---|---|---|
| T2-A | `test-failpoints` controller; F8, F10 | Opus 5.5 high | `a.md` |
| T2-B | Multi-turn sessions, `resume`, per-session queue, retries; F13, F14, F17, F28; `wait.timeout_ms`; harness gaps | Opus 5.5 medium | `b.md` |
| T2-B2 | Per-session dispatcher redesign replacing T2-B's drives; Store-failed latch (runtime §7) | Opus 5.5 high | `b2.md`, `dispatch-design.md` |
| T2-C | Restart handoff of surviving queued turns; keyed replay after restart | Opus 5.5 high (T2-B2 session) | `c.md` |
| T2-D | Runtime §8 active-connection slot limit (4 daemon-wide) in dispatch | Opus 5.5 high (T2-B2 session) | `d.md` |
| T2-E | Frozen per-turn parameters; one launch per turn in F13/F14/F17/F28; version-0 Store refusal (Task 2 slice review blockers); merged `f24a91d` | Opus 5.5 high (fresh session) | `e.md` |

Shared files: `via-core/src/engine/drive.rs` and Store's spawn/submission
commit path. T2-A's hunks there are failpoint call sites only; T2-B owns
the logic. Each names its shared hunks in its report.

Task 2 slice review r2 (`task2-sol-high-review-r2.md`, on T2-E at 847b9a0),
orchestrator dispositions: `bound: null` → `invalid_params` (C1 §1 field
rules; applied also to `effort` and `deadlines` null); the fixed sleep in the
deadline test → a bounded `wait` asserting `wait_timeout`; the `events` FK
finding is withdrawn (the FK is declared inline at `sql.rs:107`). Deferred:
queued-row read error vs latch and a restart test with a nondefault frozen
value → `via-jm4.7.7`; the fake's 30 000 ms default and other spawn CLI
options → `via-jm4.7.8`.
