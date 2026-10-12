# W3-F report: stop, shutdown and cancel correctness

Branch `claude/w3-f-task-execution-ygldap`, based on `7ab3936`
(`rust-foundation`). Brief: [../f.md](../f.md). Findings:
[W1-D Sol review](../../w2/sol-reviews/W1-D.md) 1–6 and
[W2-E Sol review](../../w2/sol-reviews/W2-E.md) 2, plus brief items 7 and 8.

Every regression below was run against the unchanged code first. The
failing output is quoted. All end-to-end tests use the real `via` binary,
daemon, SQLite Store, Host anchor and fake vendor.

## 1. Stop reaches daemon main before the reply write

**Failure mode.** `stop_request` awaited the `{"stopping":true}` write
before it sent the mode to daemon main. The reply echoes the request `id`. A
caller that sends a large id and stops reading blocks that write, so the
daemon never starts final shutdown.

**Regression.** `s1_daemon_stop_unread_reply_still_stops`. A raw socket client
sends `hello`, then `daemon/stop` with an 8 MiB id, and never reads the
reply. Before the fix, the daemon never exited (a 15.4 s run hit the test
bound).

**Fix** (`crates/via-cli/src/server.rs`).
- An accepted stop wakes main through a `Notify` before any reply is written.
  Main reads the authoritative mode from `Engine::stop_mode()`.
- The receipt write is bounded by `STOP_REPLY` (2 s).
- The old bounded `mpsc` channel is removed. With that channel, a repeated
  stop could block once main stopped receiving.

## 2. No false clean exit

**Failure mode.** The serving loop discarded drives that returned
`Ok(Err(ApiError))`. A receipted turn whose terminal commit failed then left
no forced-turn entry, and a later stop reported `clean` and exited 0.

**Regression.** `s1_daemon_stop_store_failure_is_not_a_clean_exit`.
1. A held turn commits its acceptance and text.
2. The test takes the Store's SQLite write lock from outside the daemon
   (`BEGIN IMMEDIATE`). This is a real Store write failure after the Store's
   250 ms busy timeout, not a failpoint.
3. The test releases the vendor, waits until `daemon status` reports no
   active turn, drops the lock and sends a plain stop.

Before the fix the daemon exited 0 with this summary:
`{"disposition":"clean","failed_joins":0,…}`.

**Fix.**
- Daemon main counts every failed drive or client join while serving
  (`drive_joined`) and carries the count into the final disposition.
- Core records each receipted session as unresolved until its terminal
  commits. At final shutdown it re-reads the Store for every unresolved turn,
  since a failed commit may still have landed. A turn with no durable terminal
  counts in `EngineShutdown::unresolved_turns`, which `is_clean` requires to
  be 0.
- The daemon summary gains `unresolved_turns`.
- After the fix the test exits 4, with `unresolved_turns: 1` and
  `failed_joins ≥ 1`.

## 3. No invented acknowledgement; `forced` from Host force evidence

**Failure mode.**
- Final shutdown recorded `acknowledged`/`quiescent` when the journal had no
  anchor intent for the turn. C1 §7.4 reserves `acknowledged` for vendor
  evidence.
- `forced` was derived from group absence alone.

**Regressions.**
- `crates/via-core/tests/force_stop.rs::force_before_launch_claims_no_acknowledgement`.
  It uses the public `Engine` over a real Store and Host. The test controls
  drive timing: `spawn`, then `request_stop(force)`, then `drive`, so the
  force lands before anything launches. Before the fix it failed with
  `left: String("acknowledged") right: "requested"`.
- `s1_daemon_stop_force_before_acceptance_is_forced` (end to end). The vendor
  is launched but gated before `accepted`. The test asserts
  `forced`/`quiescent` from Host evidence, no `turn.started`, the lifecycle
  events and a clean exit. Before the fix it failed with event types
  `[turn.queued, turn.submitted, turn.ended]`.
- `crates/via-host/tests/anchor_process.rs::force_evidence_separates_host_stop_from_absence`.
  - A released, unclosed control whose vendor is live gives `forced: true`.
  - A graceful close after the vendor exited on its own gives
    `CloseReport.forced == false` and `RecoveryReport.forced == false`, with
    group absence proved.
  - The existing live-control shutdown test now also asserts `forced`.
  - Before the fix this test cannot compile, because the evidence field does
    not exist.

**Fix.**
- Host keeps per-control `StopFacts`.
  - `ProcessControl::close` sets `forced` only if the verified anchor replied
    `Stopping` while no vendor exit was observed.
  - `Host::shutdown` marks a control that was released unclosed while its
    vendor was live. The anchor's EOF cleanup stopped that group.
  - Forced generations stay in Host state.
  - `RecoveryReport.forced` and `CloseReport.forced` carry the fact up through
    Wire and Adapter.
- Core's `stop_outcome` gives `forced` only with force evidence and proved
  absence. Otherwise the outcome is `requested`, with `quiescent` only when
  absence is proved and `uncertain` plus `cancel_cleanup_uncertain`
  otherwise.
- **Prelaunch decision:** with a complete journal and no anchor intent, the
  result is `requested`/`quiescent`. The cancel was requested but no vendor
  existed to acknowledge it, and nothing was launched to clean up.

## 4. Drain and force deliver committed results to waiting callers

**Failure mode.** Final shutdown aborted every client at once. A foreground
`via spawn` already in `wait` lost its reply, even though the result
committed; force terminals commit only during final shutdown.

**Regression.** `s1_daemon_stop_force_delivers_waiting_foreground_result`. A
foreground `via spawn` waits on a held turn, then a force stop arrives.
Before the fix the foreground exited 4 and printed only its receipt. After
the fix it exits 3 with the `cancelled`/`forced` envelope, and the daemon
exits 0.

**Fix.**
- Final shutdown signals `closing` instead of aborting clients.
  - Idle connections close at once.
  - A request already being served completes.
- Order: join drives, run `Engine::shutdown` (Host evidence and final
  records), then join clients until `deadline − STORE_RESERVE` (2 s), then
  drop the Store.
- Clients still running at that point are aborted and counted as
  `pending_joins`, so the exit is 4.
- `Engine::wait` checks a `finalized` flag. The flag is read before the
  Store read, and `Engine::shutdown` sets it after its last commit. A wait
  whose result can never commit ends with `daemon_stopping` instead of
  holding shutdown for its 30 s bound.

## 5. Host keeps failure facts

**Failure mode.** `join_owned_tasks` counted a failed join in a local
variable and removed the task. A later `Host::shutdown` reported
`failed_tasks: 0`.

**Regression.**
`crates/via-host/src/host.rs::failed_join_is_reported_by_every_later_shutdown`
(two shutdown calls after one failed join). Before the fix:
`a later shutdown forgot the failed join … left: (0, 0) right: (0, 1)`.

**Fix.** `HostTasks.failed` is incremented under the registry lock when the
failed result is collected. There is no await between marking the task
joined and counting it, so a cancelled caller cannot lose it. Every call
returns the running total.

## 6. Cancel lifecycle events

**Failure mode.** Forced turns committed only `turn.ended`. C1 §6 also
requires `cancel.requested`, `cancel.settled` and `session.closed`.

**Regressions.**
- `s1_daemon_stop_force_ends_active_turn_immediately` now checks the full
  sequence through `check_forced_lifecycle`:
  1. the turn's events;
  2. `cancel.requested`;
  3. `cancel.settled` (same outcome and cleanup as the envelope);
  4. `turn.ended`, which ends the envelope event range;
  5. session-level `session.closed` (`turn: null`,
     `reason: "daemon_stop_force"`).

  Sequence numbers must be dense.
- The Core and force-before-acceptance tests above check the same sequence.
- Before the fix: `event types [turn.queued, turn.submitted, turn.started,
  assistant.text, turn.ended]`.

**Fix (Core).**
- The drive commits `cancel.requested` when it abandons its execution.
- Final shutdown commits `cancel.settled`, then `turn.ended` and
  `session.closed` in one transaction, and marks the session `closed`.
- The atomic step uses a new Store method, `commit_closing_terminal`
  (see shared hunks).

## 7. Deadline fills `cancel`

**Failure mode.** A wall-deadline result had `cancel: null`. C1 §7.6 requires
the evidenced cancel outcome and cleanup certainty.

**Regression.** `route_drain.rs::failure_class_deadline_wall_after_hang` (30 s)
now asserts:
- `cancel` is `forced`/`quiescent`;
- `cancel.requested` and `cancel.settled` come before `turn.ended`;
- `turn.ended.cancel` equals the envelope's `cancel`;
- sequence numbers are dense.

Before the fix: `left: Null right: "forced"`.

**Fix.**
- `RouteFailure` gains `cleanup` and `forced`, taken from Route's forced
  close report.
- Core computes the deadline once in `drive` and passes it into `execute`.
  - `requested_at` is the deadline's wall time.
  - `settled_at` is when Route returned.
- On a `Deadline` cause Core commits both cancel events and fills `cancel`
  through `stop_outcome`. The failure class stays `deadline_wall`.

## 8. Force after partial progress

- **Observations before the force.** The held fixture now emits a text
  observation before its gate. The force test asserts the text is committed
  as `assistant.text` at seq 4, before the cancel events. It also asserts the
  envelope's raw span covers that event's `raw_ref`.
- **Decision: Store failure before the force ends `failed(store)`, not
  `cancelled`.** C1 §8.2 `store` means "Store write failed after dispatch".
  The durable stream already lost an event, and a `cancelled` terminal would
  present an incomplete record as complete. This matches the unforced drive,
  where `store_failed` also overrides the disposition. `cancel` is still
  filled with the Host evidence.
  - Regression: `s1_daemon_stop_force_after_store_failure_ends_failed_store`.
    The outside SQLite lock makes the text commit fail; the test then drops
    the lock and forces.
  - Before the fix the envelope was `state: cancelled, failure: null`.
  - After the fix it is `failed`/`store`/`error` with a `forced` cancel.
    Events are `turn.queued`, `turn.submitted`, `turn.started`, `turn.ended`,
    `session.closed`, dense.
  - The cancel events are skipped after a Store failure, following the
    existing rule that later turn events are dropped once one fails.

## Files and shared hunks

**Owned:**
- `crates/via-cli/src/server.rs`
- `crates/via-host/src/host.rs`
- `crates/via-core/src/engine.rs` (stop, shutdown, forced turns, cancel)
- `crates/via-core/src/api.rs` (three `EventBody` variants)
- Tests:
  - `crates/via-cli/tests/s1_daemon_stop.rs`
  - `crates/via-cli/tests/route_drain.rs` (deadline test only)
  - `crates/via-core/tests/force_stop.rs` (new)
  - `crates/via-host/tests/anchor_process.rs`

**Shared hunks outside owned paths (small, for W3-G and the orchestrator):**
- `crates/via-store/src/runtime.rs`: `Command::ClosingTerminal` and
  `StoreClient::commit_closing_terminal`.
- `crates/via-store/src/runtime/sql.rs`: `commit_terminal` takes
  `closed: Option<&Value>`. When given, it inserts `session.closed` after
  the terminal event and sets session state `closed` instead of `idle`.
- `crates/via-wire/src/runtime.rs`: `forced` on `WireRecovery` and
  `WireCloseReport`, set in `close` and `normalize_recovery`.
- `crates/via-routes/src/lib.rs` and `runtime.rs`: `RouteFailure.cleanup`
  and `RouteFailure.forced`, set at the two construction sites.
- `crates/via-adapters/src/lib.rs` and `runtime.rs`: re-export `WireCleanup`;
  add `FakeRecovery.forced`; set both new `RouteFailure` fields at the
  overflow site.
- `crates/via-core/src/engine.rs` hunks that touch the terminal path:
  - `finish` gains `close_session` and removes the session from `unresolved`
    after commit.
  - `execute` takes `deadline` as a parameter.
  - The unit-test `route()` helper sets the two new fields.
  - The observation-commit path (`observe`, `commit_event`) is unchanged.

## Gate (`.repo-context/verification.md`)

- `cargo fmt --all --check`: pass.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: pass.
- `cargo nextest run --locked --workspace`: 94 passed, 1 skipped. That skip
  is the existing root-only peer test. The baseline at `7ab3936` was 86
  passed, 1 skipped.
  - The 8 new tests are 1 Host unit, 1 Host integration, 1 Core integration
    and 5 daemon end-to-end.
  - Two existing tests were extended: force and deadline.
- The stop, force and Host tests passed 5 of 5 repeated runs, with no
  leftover fake-agent, anchor or daemon processes.
- `cargo deny check`: pass.
  - cargo-deny 0.18.3 cannot parse the CVSS 4.0 advisories now in the
    database, so 0.19.0 was used.
  - Its built-in fetch cannot reach GitHub through this machine's proxy. The
    database was fetched once with a temporary copy of `deny.toml` that adds
    only `git-fetch-with-cli = true`.
  - The repository config then passes with `cargo deny check
    --disable-fetch`.
- `python3 scripts/check-layers.py`: pass.
- `XDG_RUNTIME_DIR` was a private 0700 temporary directory.

## Open or uncertain

- **The anchor-intent-without-identity window is not reproduced end to end.**
  A force landing between Host's intent commit and identity commit lasts
  milliseconds and needs the `test-failpoints` controller. That turn
  reconciles `requested`/`uncertain` and the daemon exits 4. The cases
  covered are "before launch" (Core, deterministic) and "launched, not
  accepted" (end to end).
- **Contract text.** C1 has no specific rule for four choices made here. The
  orchestrator may want to record them in C1 §7.4/§7.6 and runtime §6.2:
  - prelaunch force is `requested`/`quiescent`;
  - a force after a Store failure is `failed(store)` with `cancel` filled;
  - a `wait` still pending when final records are done ends with
    `daemon_stopping`;
  - `session.closed.reason` is `"daemon_stop_force"`.
- **Clients that never read.** A client still writing a reply to a peer that
  does not read, at `deadline − 2 s`, is aborted and counted pending (exit 4).
  Only the stop receipt write has its own bound.
- **Timing in two regressions.** The Store-failure regressions rely on the
  Store's 250 ms busy timeout. One of them waits 1.5 s while holding the
  lock. Both passed in every run here.
- **Force evidence is in memory only.** Host's force facts live only in this
  daemon's memory. After a restart, recovery reports `forced: false`, which
  is consistent with C1 §7.5: restarted turns become `unknown`.
- `daemon/status.sessions.closing` still reports 0 while draining. This is
  unchanged and outside this brief.
