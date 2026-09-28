# S1 Task 3: daemon lifecycle and process failure recovery (`via-jm4.7.7`)

Plan: `../s1-plan.md` §4 Task 3 (F1–F4, F6–F7, F9, F11–F12, F19–F23, F29;
cancel/close, queued cancellation, drain, force, verified identity).
Rules: `../w1/common.md`, except reports go to `reports/<task id>.md` here.
Review loop and convergence rules: `../cloud-and-local.md` §6. Task 3 adds
lifecycle and recovery states, so it starts design-first: T3-0 writes the
gap inventory and a normative design note, Sol high reviews it, then the
orchestrator dispatches implementation slices on disjoint files.

| Task | Scope | Model | Brief |
|---|---|---|---|
| T3-0 | Gap inventory, normative design note, slice plan; no production code | Opus 5.5 high (`session_01JvsKhcyrWwPXLVcSKKEB1J`) | `t0.md` |
| T3-S0 | Split `engine.rs` and `server.rs` by responsibility; moves only; merged `08fffce` (Sol medium SOUND, `sol-review-S0.md`) | Opus 5.5 medium (`session_01XipfAFqC6UPvZFDzGPoYph`) | `s0.md` |

## Carried into Task 3

Recorded on `via-jm4.7.7` from Task 2 reviews (workers do not run `bd`):

- **F12 remainder** (runtime §7): the 5 s diagnostic window, Host 3 s stop,
  `event_end: store_error`, status health, the failure-resolution batch and
  the 10 s bound. T2-B2 implemented only the dispatch part of the latch
  (refuse admission and grants, force shutdown, exit 4).
- **Force closes already-idle sessions.**
- **Outstanding Store read after the force cutoff**: a read already sent to
  the single Store worker stays outstanding after the force-path read
  cutoff; add a worker-side stalled-read regression and define shutdown
  with an already-sent request (it cannot produce exit 0 today).
- **Re-probe of unproven groups**: recovered groups whose absence startup
  could not prove, and unread unproven anchors counted at the recovery
  deadline, hold connection slots (runtime §8, §5) with no re-probe loop.
  Add bounded re-probing and re-reconciliation so capacity returns, and
  surface held slots in `daemon/status`.
- **Deterministic barriers**: replace the fixed-sleep capacity checks in
  `crates/via-cli/tests/s1_crash_points.rs` and the ignored force-handoff
  race in `crates/via-cli/tests/s1_daemon_stop.rs` with failpoint barriers.
- **Core idle deadline** (C1 §4 `deadlines.idle_ms`), then accept non-null
  `idle_ms` on the fake route (T2-E refuses it).
- **Queued-row read failure**: today a read error returns `Unread` and
  retries with no agent I/O and no latch; malformed frozen JSON fails the
  turn and latches. Define persistent read-failure behaviour and add a
  focused failure test.
- **Restart with a nondefault frozen value**: a test combining restart
  handoff and keyed replay with a nondefault frozen per-turn value.
- **Raw-log incompleteness after recovery** (T2-A review): startup recovery
  must carry recovered raw-log incompleteness evidence.

Design review (owner, 2026-09-28): GPT-6 Astra medium and Claude Fable 5.1
high. Round 1 on `42993bc`: `astra-review-design.md` (UNSOUND) and
`fable-review-design.md` (SOUND WITH CHANGES); orchestrator decisions in
`design-r1-decisions.md`. Owner decisions on the Store failure policy
(runtime §7), drain closing sessions and force's session scope:
`owner-decisions.md`.

Round 2 on `f145ff2`: `astra-review-design-r2.md` (UNSOUND) and
`fable-review-design-r2.md` (SOUND WITH CHANGES); decisions in
`design-r3-decisions.md`. S0 (moves only, `s0.md`) starts in parallel with
design round 3.

Round 3 on `4531dd0`: `astra-review-design-r3.md` (UNSOUND, no blocker)
and `fable-review-design-r3.md` (SOUND WITH CHANGES); decisions in
`design-r4-decisions.md`.
