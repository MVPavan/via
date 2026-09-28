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
| T3-S1 | Lower-layer primitives: Store schema v5 and F12 ops, Host early stop and re-probe, Wire/Route/Adapter stop orders (design §13 S1 at `1a814d6`); three local fix rounds; merged `4a1c11c` (Sol medium SOUND, `sol-review-S1-r3.md`) | Opus 5.5 high (`session_01YVUZbU8b2HpcRwvyHqKvSB`); rounds 1–3: local `implementer-high` subagent | `s1.md`, `s1-r1-decisions.md` |
| T3-S2 | Turn control: cancel, close and closing, idle deadline, failure-record migration (design §13 S2); two local fix rounds; merged `dd2a81d` (Sol medium SOUND, `sol-review-S2-r2.md`) | Opus 5.5 high, local `implementer-high` subagent, worktree branch `wt/t3-s2` | `s2.md`, `s2-r1-decisions.md` |
| T3-S3 | Daemon lifecycle: stop, drain, force, idle exit, final-shutdown pipeline, status, re-probe loop, early-stop wiring (design §13 S3); two local fix rounds; merged `d69e6d0` (Sol medium; last finding deferred to `via-pvj.2`, `sol-review-S3-r2.md`) | Opus 5.5 high, local `implementer-high` subagent, worktree branch `wt/t3-s3` | `s3.md`, `s3-r1-decisions.md` |
| T3-S5 | Store failures: F12 and O1, scoped and latch paths, status health, carried items from S1–S4 (design §13 S5) | Opus 5.5 high, local `implementer-high` subagent, worktree branch `wt/t3-s5` | `s5.md` |
| T3-S4 | Recovery evidence: raw incompleteness, durable cancel events, corrupt-row handoff, F9, F22, F23 (design §13 S4); parallel with S3; two local fix rounds; merged `3ca53e4` (Sol medium SOUND, `sol-review-S4-r2.md`) | Opus 5.5 high, local `implementer-high` subagent, worktree branch `wt/t3-s4` | `s4.md`, `s4-r1-decisions.md` |

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

Round 4 on `1a814d6`: `astra-review-design-r4.md` (UNSOUND, no blocker)
and `fable-review-design-r4.md` (SOUND WITH CHANGES, one blocker in S1's
early-stop lifetime); decisions in `design-r5-decisions.md`, with 1–7 sent
to S1 directly.

Round 5 on `bf27db6` (final narrow pass): `astra-review-design-r5.md` and
`fable-review-design-r5.md`, both SOUND WITH CHANGES with no blocker;
final decisions in `design-r6-decisions.md` (1–5 sent to S1 directly).
The design is final after round 6; later findings go to slice code
reviews.

S1 review on `8d2a7b6` (GPT-6 Sol medium, in two parts):
`sol-review-S1-store.md` (SOUND WITH CHANGES) and `sol-review-S1-host.md`
(UNSOUND: the interrupt write discards a raw Store failure). The local
gate at `8d2a7b6` was clean: default tests 207/2 three times, failpoint
tests 253/2 five times, F08–F12 17. Decisions are in `s1-r1-decisions.md`.
Round 1 (local `wt/t3-s1`, `4352602..020ac95`) fixed decisions 1–7. Gate:
213 / 2, failpoints 261 / 2 three times, F08–F12 17. The Sol medium
re-review (`sol-review-S1-r1.md`) is SOUND: merge after decision 8, with no
findings. Round 2 is decision 8 alone.

Rounds 2 and 3: decision 8 (`a35a5c6`); Sol found that the caller's stop
check bypassed it (`sol-review-S1-r2.md`, decision 11), fixed in `aac45b7`.
Sol round-3 check SOUND (`sol-review-S1-r3.md`). S1 merged at `4a1c11c`.
The gate on the merged tree: 213 / 2, failpoints 264 / 2 three times,
F08–F12 17, release check clean. The design edits S1 needed are in
`8eb4ed0` (tags `[s1.N]`, `[S1]`, amendment A20).

S2 delivered on local `wt/t3-s2` at `bc34ee2` (report `reports/T3-S2.md`;
gate 231 / 2, failpoints 293 / 2 three times, F08–F12 17). Sol medium, in
two parts: `sol-review-S2-core.md` and `sol-review-S2-close.md`, both SOUND
WITH CHANGES with no blocker. Round-1 decisions are in
`s2-r1-decisions.md`: the idle timer disarms under an existing order, F19
bounds the idle order, and the held count follows the session filter.

Round 1 (`77db485`): Sol's check (`sol-review-S2-r1.md`) found F19's
fallback interval started too late and an unfinished filtered re-probe
counted foreign groups: decisions 4 and 5. Round 2 (`d6e3452`) fixed both;
counting by session required passing each held group's owner through
`hold_capacity` (Wire, Route, Adapter, one call in `recovery.rs`), ratified
by the orchestrator. Sol round-2 check SOUND (`sol-review-S2-r2.md`). S2
merged at `dd2a81d`. The gate on the merged tree: 231 / 2, failpoints
295 / 2 three times, F08–F12 17, release check clean. The design edits S2
needed are tagged `[s2.N]` and `[S2]`, with amendments A21 and A22. S3 and
S4 then start in parallel on disjoint files (design §13).

S4 delivered at `e8a9631` blocked on one occurrence count in S3's
`s1_crash_points.rs`, which it was granted. Round 1 (`2d3a477`) also fixed a
duplicate `cancel.settled` on re-recovery. Sol medium
(`sol-review-S4.md`) found two gaps: a corrupt row broke the `unknown`
barrier, and a durable `raw_log.incomplete` could lose its warning. Round 2
(`5916bd2`) cancels Store-unreadable rows on the P6 and close paths instead
of failing them, and carries the warning. Sol round-2 check SOUND
(`sol-review-S4-r2.md`). S4 merged at `3ca53e4` (gate: 243 / 2, failpoints 316 / 2 three times,
F08–F12 19, release check clean); decisions 1–8 are in
`s4-r1-decisions.md`, and the items carried into S5 are in design §13.

S3 delivered at `e3d542d`. Sol medium reviewed it in two parts
(`sol-review-S3-shutdown.md`, `sol-review-S3-serving.md`), both SOUND WITH
CHANGES: a force-set read across two locks, a dispatcher aborted but not
joined before Host reconciliation, resumed paging starved while this
daemon owned groups, an unbounded pre-`hello` read, the Store probe's
writer boundary, the re-probe backoff reset and a nested ledger lock.
Round 1 (`ec5880d`) fixed all of them (decisions 1–9); the resumed-paging
fix bounds paging to the startup anchor cohort. Round 2 (`0079b0d`) bound
`StoreLock` to its State directory (decision 10). Sol's last finding, a
same-user replacement of the State directory between lock and open, is
deferred to the platform gate `via-pvj.2` under runtime §6 (decision 11).
S3 merged at `d69e6d0`, on top of S4. The gate on the merged tree:
263 / 1, failpoints 351 / 1 three times, F08–F12 19, release check clean. S5 then starts (`s5.md`); its §13 entry carries S1–S4's
leftovers.
