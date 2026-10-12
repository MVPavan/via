# W2: close Task 1 after the W1 Sol reviews

W1-A, B and C are merged (`4f9d6e3`). GPT-6 Sol medium reviewed each merge
(verdicts: A unsound, B and C sound with changes). W1-D and W2-E run in
parallel on separate branches; the orchestrator merges both.

Sol reviews: `sol-reviews/`. Rules: `../w1/common.md`, except reports go to `reports/<task id>.md` here.

| Task | Findings | Model | Owned paths (summary) |
|---|---|---|---|
| W1-D | shutdown seam, T1-I7, T1-I4, `daemon/stop` drain | Opus 5.5 high | `../w1/d.md` |
| W2-E | T1-I5 end to end, evidence gaps, failure classes | Opus 5.5 high | `e.md` |

Fixed locally by the orchestrator: Store `raw_ref` invariant, monotonic
turn duration, root-only peer test reported as ignored.

Deferred to `via-jm4.7.8` (Task 4): per-pipe reader tasks (Wire must not
stop reading while a consumer is full), the 4 MiB per-session observation
budget, C1 JSON depth 64 / 65,536-node limits, `events`/`logs` paging
parameters. `wait.timeout_ms` goes with `via-jm4.7.6`.
