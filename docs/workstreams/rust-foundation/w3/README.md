# W3: fix the Sol findings on W1-D and W2-E

W1-D and W2-E are merged into `rust-foundation`. GPT-6 Sol medium reviewed
each branch (`../w2/sol-reviews/W1-D.md`: unsound; `W2-E.md`: sound with
changes). W3-F and W3-G run in parallel; the orchestrator merges both.

Rules: `../w1/common.md`, except reports go to `reports/<task id>.md` here.

| Task | Findings | Model | Owned paths (summary) |
|---|---|---|---|
| W3-F | W1-D 1–6, W2-E 2 (stop, shutdown, cancel) | Opus 5.5 high | `f.md` |
| W3-G | W2-E 1, 3 (raw and Store waits and uncertainty) | Opus 5.5 high | `g.md` |

Deferred to the failpoint controller (runtime-contracts, `test-failpoints`;
built with `via-jm4.7.6`/`.7.7`): an end-to-end daemon exit-4 test, a real
`anchor.wait()` failure, and an end-to-end raw-append failure asserting the
`raw_log.incomplete` event and warning (W2-E finding 4).
