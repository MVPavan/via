# W1-D report: shutdown seam, force stop (T1-I7), debug cleanup removal (T1-I4), `daemon/stop` drain

Branch `claude/w1-d-task-dby2u1`, based on `56e3f55` (`rust-foundation`).
Brief: [../../w1/d.md](../../w1/d.md). Design integrated:
[shutdown-ownership-seam.md](../../checkpoint/shutdown-ownership-seam.md)
(Sol PASS).

## 1. Shutdown seam: spec integration and implementation

**Spec.** Amended per the seam's §4 table:
`docs/specs/runtime-contracts.md` §2 (live-daemon owners are never
abandoned), §5 (Host `shutdown`/`ShutdownReport` sketch; the deadline
paragraph's "retain … afterward" holds while the daemon lives; a failed child
wait is a failed task), new §6.2 (stop receipt, drain/force/idle, one 10 s
final deadline, clean exit 0 vs incomplete exit 4, summary line,
limitations), §7 (F12's 10 s is measured from first failure, exit 4), §10
audit row, §11.2 (the harness implements outer cleanup itself). C1 §3.14 in
`docs/specs/via-api-v1.md`, C2 bootstrap shutdown in
`docs/specs/adapter-contract.md`, coding standard §5 in
`.repo-context/coding-style.md`.

**Failure modes.**
- (a) `Host::shutdown` returned `Err` on an expired deadline or journal
  failure, which dropped the pending join count.
- (b) `join_owned_tasks` treated a panicked or cancelled task as a successful
  join.
- (c) The reaper ignored `anchor.wait()` failure.
- (d) The daemon's final shutdown had no bound: it joined drives and dropped
  the Store without a deadline.
- (e) The daemon had no clean/incomplete disposition beyond `bail!`
  (exit 4 as `daemon_unreachable`).

**Regressions and pre-fix failures.**
- Host (a, b): a probe on a clean `56e3f55` worktree failed with
  `pre-fix: a panicked owned task is joined silently; no failed count exists
  (pending=0)` and `pre-fix: recovery error process journal unavailable:
  UncertainCommit replaces the report; the pending reaper count is lost`.
  The new unit tests in `crates/via-host/src/host.rs` are
  `panicked_or_failed_tasks_count_as_failed_not_joined`,
  `held_task_past_deadline_is_reported_pending_and_kept_owned` (seam
  regressions 1 and 3; this also covers the expired-deadline report) and
  `recovery_failure_keeps_pending_owner_in_report` (seam regression 4). The
  existing `cancelling_join_keeps_reaper_owned_for_later_join` still covers
  cancelling the shutdown future itself.
- Wire (seam regression 4 through the C2/Wire summary):
  `recovery_failure_and_pending_owner_both_survive_the_summary`.
- Daemon (seam regression 2): `s1_daemon_stop_receipt_precedes_clean_exit`.
  Pre-fix it failed with `fail: daemon wrote no final shutdown summary`: the
  old daemon exited 0 without any positive disposition evidence.
- Bounded Store join (d): `stalled_blocking_drop_is_abandoned_at_the_deadline`
  in `crates/via-cli/src/server.rs`.

**Fix.**
- Host: `shutdown` returns a `ShutdownReport` on every path, with
  `recovery`, `pending_tasks`, `failed_tasks` and `failure`. Tasks yield
  `Result`; a failed reaper wait is `Err`. Unjoined tasks stay in the
  registry.
- Wire, Route and Adapter pass the report through as a passive summary
  (`failure` is a bounded string).
- Core: `Engine::shutdown(deadline) -> EngineShutdown` with `is_clean()`.
- CLI daemon: `final_shutdown` stops listening, removes the socket and sets
  one absolute 10 s deadline. It joins clients and drives, then runs Host
  shutdown and final records, then drops the Store on the blocking pool under
  the same deadline. It writes one `{"daemon_shutdown":{…}}` stderr line and
  exits 0 only if everything is clean, otherwise 4.
- `main` ends with `runtime.shutdown_background()`, so an abandoned Store
  join cannot hold the process past its bound.

## 2. T1-I7: force enters final shutdown immediately

**Failure mode.** `--force` only skipped the active-work refusal. The daemon
then waited for every drive, up to the turn's 30 s wall deadline, and the
turn ended on its own terms, not `cancelled`.

**Regression.** `s1_daemon_stop_force_ends_active_turn_immediately` uses the
real binary and a turn held at a gate with a grandchild in the owned group.
Pre-fix it failed with `fail: force stop did not exit within the 10 s final
shutdown` (15.4 s run). Post-fix it exits 0 in about 1.3 s. It asserts the
following:
- vendor and grandchild are gone;
- the envelope has `state: cancelled`, `failure: null` and
  `stop_reason: interrupted`;
- `cancel: {outcome: forced, cleanup: quiescent, …}`;
- `turn.ended` carries the same `cancel` and ends the event range;
- the summary is `mode: force, disposition: clean`.

**Fix (Core).**
- `request_stop` sets a force watch.
- Each drive, in a `biased` select, drops its execution. Releasing the Host
  control makes the anchor's reviewed EOF cleanup stop the group. The drive
  then records a `ForcedTurn`.
- `Engine::shutdown` commits `cancelled` only after Host reconciliation,
  following C1 §7.6:
  - `forced`/`quiescent` only with Host-proved group absence;
  - `requested`/`uncertain` otherwise, plus the `cancel_cleanup_uncertain`
    warning;
  - `acknowledged`/`quiescent` when a complete journal has no anchor intent
    for the turn (nothing was launched).

## 3. T1-I4: remove the unauthorised cleanup debug path

**Failure mode.** At `56e3f55` the `daemon/verify_cleanup` RPC and the
`__via_verify_cleanup` CLI were already absent (grep), but
`Core::verify_cleanup` remained. The harness proved cleanup after the daemon
exited by reopening `Engine::open` (Core and the Store owner). The reviewed
R1 seam does not authorise that.

**Fix.**
- Removed `Engine::verify_cleanup`.
- New `crates/via-cli/tests/support/outer_cleanup.rs` implements runtime
  §11.2 directly:
  - a read-only single-transaction anchor snapshot;
  - per anchor, a peer-cred check and a fresh marker challenge, then a
    `Stop` only to a verified live anchor;
  - absence only through a same-boot, same-PID-namespace
    `test_kill_process_group` → `ESRCH` probe every 20 ms;
  - no numeric signal, and the marker is omitted from the summary.
- `s1_prompt_to_result.rs::verify_anchors_after_daemon` now uses it.

**Regression.** `s1_daemon_first_death_outer_cleanup_proves_absence` (seam
regression 5) snapshots the anchor, SIGKILLs the daemon through its retained
child handle, then proves group absence and vendor/grandchild absence through
the outer seam alone. This is a new positive gate, not a pre-fix failure: the
old code would also have cleaned up. The pre-fix defect was the Core reopen
path, which no longer exists (`grep verify_cleanup` is empty).

## 4. `daemon/stop` drain (Sol W1-B finding 2)

**Failure mode.** The strict DTO rejected C1's `drain` as `unknown_field`,
and the CLI had no `--drain` flag.

**Regression.** `s1_daemon_stop_drain_finishes_accepted_turn_then_exits`.
Pre-fix it failed with `error: unexpected argument '--drain' found`. It
asserts the following:
- a plain stop with active work gives `admission_refused`;
- `--drain --force` gives `invalid_params`;
- `--drain` returns `{"stopping":true}`;
- a later spawn gives `daemon_stopping`;
- the daemon stays up until the gate is released, then the turn completes
  and the daemon exits 0 with `mode: drain, disposition: clean`.

**Fix.**
- `DaemonStopParams.drain`, plus `ApiError::DAEMON_STOPPING` (-32017) and
  `SESSIONS_ACTIVE`.
- `Engine::request_stop`, under the admission lock, closes admission and
  returns the mode. A later `force` escalates an accepted drain.
- Daemon main keeps serving while draining and enters final shutdown once
  `active == 0` and all drives have joined.
- CLI `via daemon stop --drain`. `via daemon stop` no longer auto-starts a
  daemon: starting one only to stop it made teardown leak daemons.

## Files and shared-file hunks

- Owned: `crates/via-host/src/host.rs`,
  `crates/via-host/tests/anchor_process.rs` (new report shape),
  `crates/via-cli/src/{server,main}.rs`,
  `crates/via-cli/tests/s1_daemon_stop.rs` (new),
  `crates/via-cli/tests/support/outer_cleanup.rs` (new),
  `crates/via-cli/Cargo.toml` and `Cargo.lock` (dev-dependency `sha2`,
  already a workspace dependency; `rustix` features `net`, `time` for the
  harness challenge and deadline), plus the specs above.
- Shared hunks that W2-E should expect:
  - `crates/via-core/src/engine.rs`:
    - `drive` gains a force branch and its tail moved into `finish(started,
      seq, terminal, accepted)` so forced turns reuse it.
    - `Terminal.cancel` and a `cancel` field in `turn.ended`.
    - `verify_cleanup` removed; `shutdown` rewritten.
    - `StopMode`, `EngineShutdown`, `request_stop`, and a spawn check for the
      stop gate.
  - `crates/via-core/src/api.rs`: `DaemonStopParams.drain`, two `ApiError`
    constants, `Cancel`, `Warning::CANCEL_CLEANUP_UNCERTAIN`, and
    `TurnEnded.cancel` (omitted when `None`).
  - `crates/via-wire/src/runtime.rs`: `shutdown` returns `WireShutdown`
    through `summarize_shutdown`, with new fields and one unit test.
  - `crates/via-routes/src/runtime.rs`: one signature line.
  - `crates/via-adapters/src/runtime.rs`: `shutdown` returns `FakeShutdown`
    with the new fields.
  - `crates/via-cli/tests/s1_prompt_to_result.rs`: only the
    `verify_anchors_after_daemon` body and one `mod` line.

## Gate (`.repo-context/verification.md`)

- `cargo fmt --all --check` passes.
- `cargo clippy --locked --workspace --all-targets -- -D warnings` passes.
- `cargo nextest run --locked --workspace` passes 72/72, with 1 skipped: the
  pre-existing root-only peer test. It was 63 before the change; the 9 new
  tests are 3 Host, 1 Wire, 1 daemon unit and 4 end-to-end.
- The new end-to-end tests plus `s1_prompt_to_result` passed 5 of 5 repeated
  runs with no leaked processes.
- `cargo deny check`, `python3 scripts/check-layers.py` and
  `python3 .claude/scripts/skill-catalog.py --check` (0 FAIL) pass.
- Markdown link check: no broken links in any file this branch changes. The
  19 reported are all pre-existing `path:line` links in the untouched
  `w2/sol-reviews/` files.
- `XDG_RUNTIME_DIR` was a private 0700 temporary directory.

## Open or uncertain

- **No end-to-end exit-4 test.** Seam regression 3 (hold an owned task past
  the deadline, then the daemon exits 4) is covered only at the Host level and
  by the bounded blocking-drop test. Driving it through the real binary needs
  the spec'd `test-failpoints` controller, which does not exist yet. A real
  `anchor.wait()` failure was also not induced; it is covered with a
  synthetic `Err` task.
- **Missing cancel events.** Forced turns commit `cancel` on `turn.ended`
  and in the envelope only. C1 §6 `cancel.requested`/`cancel.settled` and
  `session.closed` are not emitted, because Store has no standalone event
  commit and event emission is W2-E's area. This is a follow-up.
- **Drain drops waiting clients.** After drain, final shutdown aborts client
  connections. A foreground `via spawn` still waiting can miss its reply
  (exit 4), although the terminal is durable and readable after restart.
- **Status count.** `daemon/status.sessions.closing` still reports 0 while
  draining.
- **Force during launch.** A turn forced while `acquire` is still starting
  an anchor can leave an anchor row without identity. That reconciles
  `uncertain`, so the daemon truthfully exits 4.
- **Cancelled shutdown loses forced turns.** If the final-shutdown future
  itself is cancelled, pending forced turns stay `running`. Restart
  reconciles them `unknown` (C1 §7.5); only daemon main runs it.
- **Harness checks changed.** The old harness's consistency check between
  the Store snapshot and Core recovery, and its `phase == arm_intent` check,
  are replaced by the per-row outer verification.
