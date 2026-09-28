# T3-S2 round 1: orchestrator decisions

GPT-6 Sol medium reviewed S2 at `bc34ee2` in two parts:

- `sol-review-S2-core.md` (cancel, stop orders, idle, migration): SOUND
  WITH CHANGES, with one major and one minor finding.
- `sol-review-S2-close.md` (close and closing): SOUND WITH CHANGES, with one
  minor finding.

Neither found a blocker. The worker's gate at `d6aff6d` was: default 231/2,
failpoints 293/2 three times, F08–F12 17. The fix round runs locally on
`wt/t3-s2`.

1. **The idle timer disarms once any order exists** (core, major; design
   §5).
   - Before the run loop issues the idle order, it checks the turn's stop
     watch under the same slot transition that attaches orders.
   - If an order is already attached, the timer disarms and nothing is
     issued. A cancel or close order must never have its `force_at`
     shortened by the idle deadline.
   - Test with a controlled interleaving: an order attached while the run
     loop is busy committing an observation past `idle_at`. Use an
     existing seam if one reaches that point. Otherwise add an
     acknowledgement-only seam and name it in the report.
2. **F19 bounds the idle order itself** (core, minor).
   - When the harness sees the wall and monotonic clocks diverge, bound a
     monotonic test observation of `cancel.requested` (the idle order),
     not terminal completion.
3. **The held count follows the session filter** (close, minor).
   - This scope expansion into `crates/via-host` is authorized, limited to
     `reprobe_held` and `ReprobeReport`.
   - `ReprobeReport.held` counts only the groups eligible for the supplied
     session filter; `None` keeps the daemon-wide count.
   - Test both cases, and that the check does not end before every group
     of the session has been examined.
   - Correct the report: another session's group can delay a close by up
     to its bound, but it does not make the close result `uncertain`. The
     Store derives cleanup from the closing session's anchors.

The orchestrator makes the design and contract edits the S2 report lists
when S2 merges:

- design §10: `core.cancel.settling`, plus any seam from decision 1;
- design §11: the adapted tests;
- C1: the `idle_ms` rules and the fake route's capabilities.
