# T2-A report: failpoint controller, F8 and F10

Branch `claude/task-t2a-report-x4b9yl`, based on `64419c0`. Brief:
[../a.md](../a.md). Contract: `docs/specs/runtime-contracts.md` §7, §11;
C1 `docs/specs/via-api-v1.md` §7.5.

## Controller (runtime-contracts §11)

- **Feature.** `test-failpoints`, default off, in `via-cli` → `via-core` →
  `via-adapters` → `via-routes` → `via-wire` → `via-store`, along existing
  edges only (the layer check passes). Adapters and Routes only forward it to
  Wire, which hosts a point; `via-host` does not get it (no Task 2 point).
- **One controller**, `crates/via-store/src/failpoint.rs`, in the lowest crate
  every host crate already depends on. The whole module, including environment
  parsing, is `#[cfg(feature = "test-failpoints")]`. Each call site is also
  under that cfg.
- **Activation.** `VIA_FAILPOINT_DIR` (absolute, 0700, owned by the daemon's
  uid) and `VIA_FAILPOINT_TOKEN` (16–128 URL-safe chars), read once in
  `Engine::open` before Store opens. Neither set: inactive. One without the
  other, an unsafe directory or a bad token: daemon startup fails with a named
  error, so a scenario never runs with its failpoints silently off.
- **Commands.** `<dir>/<point>.json` =
  `{"token","occurrence","action"}`; action `pause`, `crash` (abort, no
  unwinding) or `fail_io`. The occurrence counts that point's hits in this
  process, from one. On the matching hit the controller writes
  `<point>.<n>.ack` atomically (tmp + fsync + rename, 0600). The ack holds only
  point, occurrence, action and pid: no prompt, handle or token. Then it acts.
  Pause continues when `<point>.<n>.release` exists, unless the harness kills
  the daemon first. A wrong token or bad shape is not acted on and writes an
  empty `<point>.<n>.refused`.
- **Harness.** `crates/via-cli/tests/support/failpoints.rs`: private dir,
  random token, `arm`/`disarm`/`release`, and `wait_ack` with a bounded wait.
  `wait_ack` checks the ack is private, carries exactly
  `{action,occurrence,pid,point}` and has no token.
- **Isolated tests** (`failpoint::tests`, 3): unsafe dir, mode or token is
  refused; only the armed occurrence acts and gets an ack, and the ack has no
  token; a foreign token is refused without acting.
- **Release exclusion.** New `scripts/check-release-features.py`:
  1. The `cargo tree` feature graph (no default features) has no
     `test-failpoints` and no `via-fake-agent`.
  2. It launches release `via` with all six points armed to `pause` and a
     valid token. A fake turn must reach `completed` with no ack or refused
     file.
  3. It starts a daemon with a partial configuration, which a feature build
     refuses.
  4. As supporting evidence only, it scans the binary for nine marker strings.

  Negative control: the same script on a release build *with* the feature
  fails with `FAIL: armed spawn timed out: the daemon paused at a failpoint`
  (exit 1).

## Points (Task 2 only)

| Point | Site | `fail_io` means |
|---|---|---|
| `store.spawn.before_commit` | `commit_spawn`, all rows inserted, before `tx.commit()` (`via-store/src/runtime/sql.rs`) | `Write`: not committed, rolled back |
| `store.spawn.after_commit` | right after `tx.commit()` | `Uncertain`: committed, reply says uncertain |
| `store.commit.reply_lost` | `send_commit`, for replies to Core lifecycle mutations in the writer loop (spawn, submission, acceptance, event, terminal, closing terminal) | reply dropped: caller sees `Unavailable` |
| `core.intent.after_commit` | `Engine::submit` after a positive submission commit (`engine/drive.rs`) | turn recorded failed at `running`, drive returns `store_error` |
| `wire.prompt.after_write` | `WireConnection::write_frame` after a whole frame is written (`via-wire/src/runtime.rs`) | `WireError::Io` |
| `core.accept.before_commit` | `Engine::accept`, before `commit_acceptance` | acceptance not committed, turn fails `store` |

`wire.prompt.after_write` counts every whole outbound frame. In S1 the first
frame on a connection is the start carrying the prompt. Interrupts count too.
ProcessJournal (anchor) replies are left out of `reply_lost`: Task 3's
`host.anchor.*` points own those seams.

## F8: crash inside `spawn`'s write

**Failure mode.** A crash between the session, turn and event inserts and the
commit could leave a partial session. A crash after the commit but before the
reply could leave a session without its hash or queued event. Either way the
caller gets no receipt. A lost reply could also be dispatched or re-created.

**Regressions** (`crates/via-cli/tests/s1_crash_points.rs`, real `via`,
daemon and SQLite):

- `s1_f08_crash_inside_spawn_write_leaves_nothing`: pause at `before_commit`.
  While paused, the consistent read shows 0 sessions, turns, events and
  anchors. SIGKILL the daemon; the client gets no receipt. After restart
  there are still no rows. A new spawn then completes and is the only session.
- `s1_f08_crash_after_spawn_commit_keeps_the_whole_session`: crash (SIGABRT)
  at `after_commit`; no receipt. After restart the rows are exactly
  `[1,1,1,0]`:
  - the session has a 32-byte hash, the receipt and `next_seq` 2;
  - turn 1 is queued, keeps prompt `f08` and has no `submitted_at`;
  - the events are only `turn.queued`.

  `steer` with the handle gets `unsupported_verb` (it authenticated); another
  handle gets `invalid_handle`. The unacknowledged turn is never dispatched.
- `s1_f08_lost_spawn_reply_leaves_one_whole_undispatched_session`: `fail_io`
  at `store.commit.reply_lost`. The client gets `store_error` and no receipt.
  One whole queued session exists with zero anchors, and it stays undispatched
  while a later spawn completes.

**Before.** With the controller and harness built but no call sites, every
test failed as `no acknowledgement of <point> #1` after 10 s: the seam was
missing (`scratchpad/t2a/before.txt`, not committed). **With the call sites
added, all three F8 tests passed with no Store change.** The single
IMMEDIATE transaction was already atomic. F8 was therefore a missing-seam
gap, not a Store defect; the tests now guard that atomicity.

**Not covered: keyed replay of one receipt.** Store has no spawn
`idempotency_key` yet (no `spawn_keys` table, and C1 `idempotency_key` is not
parsed). T2-B owns that (F13). Once it lands, the lost-reply scenario should
also retry with the same key and assert the byte-identical receipt and one
session. The point it needs, `store.commit.reply_lost`, is here.

## F10: crash after the prompt reached the agent

**Failure mode.** The daemon dies after submission intent, after the prompt
reached the vendor, or before acceptance was recorded. On restart nothing
resolved the turn. It stayed `running` forever and `result` returned
`turn_not_finished`, against C1 §7.5 and runtime §7, which require `unknown`,
committed before admission, and no resend.

**Regressions:**

- `s1_f10_submission_precedes_agent_io_and_restarts_unknown`: pause at
  `core.intent.after_commit`. The turn is `running` with `submitted_at`, and
  the events are `[turn.queued, turn.submitted]`. No anchor row exists and
  the fake has not been reached. SIGKILL, then restart: `unknown`, still zero
  anchors.
- `s1_f10_crash_after_prompt_write_restarts_unknown_without_resend`: pause at
  `wire.prompt.after_write`. The fake's `prompted.entered` gate proves it read
  the start. The turn is submitted and not accepted. SIGKILL, then restart:
  `unknown`, `vendor.turn_id` null, one anchor (one launch), and
  `turn.submitted` exactly once, even after a later spawn completes.
- `s1_f10_crash_before_acceptance_commit_restarts_unknown`: the fake accepts
  and the daemon crashes (SIGABRT) at `core.accept.before_commit`. No
  `accepted_at`, correlation or envelope. After restart: `unknown`,
  `accepted_at` null, one anchor.
- `s1_f10_released_intent_pause_launches_once_and_completes`: normal path.
  Paused after intent there are zero anchors; after release the turn launches
  once and ends `completed`.

The restart checks read `via result` and require:

- the state is `unknown`;
- the last event is `turn.ended`;
- sequences are dense;
- there is exactly one `turn.submitted`.

**Before the fix** (call sites present, no recovery;
`scratchpad/t2a/before-recovery.txt`), the three restart tests failed with:

```
fail: restarted result is not an envelope: {"code":-32015,"data":{"kind":"turn_not_finished"},"message":"turn has not finished"}
```

**Fix.** New `crates/via-core/src/engine/recovery.rs`: `Engine::recover`
runs in `serve` after `Engine::open` and before the accept loop, so it
commits before admission. A recovery failure fails startup. It pages
`StoreClient::unfinished_turns` (new closed read of `state='running'` turns,
at most 1000 per page, which already have submission intent). For each turn
it reads the committed events in pages and commits a `turn.ended` `unknown`
envelope through the existing terminal commit. The envelope:

- `stop_reason: error`, `failure: null`;
- `duration_ms: null`: the crashed daemon's clock is gone;
- `raw_spans` bound every raw reference the turn committed;
- acceptance fields only when both the correlation and `turn.started`
  committed.

Queued turns without submission intent stay queued. Nothing is resent.

## Files changed

- Feature wiring: `crates/{via-cli,via-core,via-adapters,via-routes,via-wire}/Cargo.toml`, `crates/via-store/Cargo.toml` (comment).
- Controller: `crates/via-store/src/failpoint.rs`, `crates/via-store/src/lib.rs`.
- Store: `crates/via-store/src/runtime.rs` (`UnfinishedTurn`, `Command::Unfinished`, `unfinished_turns`); `crates/via-store/src/runtime/sql.rs` (spawn points, `send_commit`, `read_unfinished`).
- Core: `crates/via-core/src/engine.rs` (activation, `mod recovery`), `crates/via-core/src/engine/drive.rs` (two call sites), `crates/via-core/src/engine/recovery.rs`.
- Wire: `crates/via-wire/src/runtime.rs` (one call site).
- CLI: `crates/via-cli/src/server.rs` (calls `recover` before the accept loop).
- Tests: `crates/via-cli/tests/s1_crash_points.rs`, `crates/via-cli/tests/support/failpoints.rs`.
- Gate: `scripts/check-release-features.py`, `.repo-context/verification.md` (script wording).

**Shared hunks** (T2-B owns the logic):

- `engine/drive.rs`: two cfg'd call sites only, in `submit` (after a positive
  commit) and `accept` (before `commit_acceptance`).
- Store's commit path in `runtime/sql.rs`:
  - two cfg'd hits in `commit_spawn`;
  - writer-loop mutation replies routed through `send_commit`;
  - one new read arm;
  - `#[expect(clippy::too_many_lines, reason = …)]` on `writer_loop`, which
    reached 105 lines. T2-B's new commands will hit the same limit.

Outside the call-site-only rule, the F10 fix needed three things: the Store
read (`read_unfinished`, `UnfinishedTurn`), the Core recovery module and one
`server.rs` call.

## Gate (Rust 1.98.1, nextest 0.9.146, cargo-deny 0.20.2, 4 vCPU cloud VM)

| Command | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | pass |
| `cargo nextest run --locked --workspace` | 125 passed, 2 skipped, 31 s |
| `cargo deny check` | pass |
| `python3 scripts/check-layers.py` | pass |
| `cargo clippy … --features via-cli/test-failpoints -- -D warnings` | pass |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints` | 132 passed, 2 skipped, 31 s |
| `… -p via-cli --features test-failpoints -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 7 passed, 3.7 s; also 5 repeated runs, 7/7 each |
| `… -E 'test(/^s1_(f2[4567]\|raw\|bounds\|store)_/)'` | **exit 4, `no tests to run`**: these are Task 4 scenarios (F24–F27, raw, bounds, store), none exist yet |
| `cargo build --locked --release -p via-cli --no-default-features` | pass |
| `python3 scripts/check-release-features.py target/release/via` | pass: 643 graph nodes clean, 6 points armed and ignored, partial config ignored, 0 of 9 markers |

The 2 skipped tests are pre-existing ignored tests: the root-only peer-UID
check and the scheduling-dependent force race (`via-jm4.7.7`). The default
count rose 122 → 125: the three controller unit tests compile in the default
workspace test build too. That is because `via-core`'s existing dev-dependency
enables `via-store/test-failpoints`. The default `via` test binary therefore
contains Store's inert points, but not Core's activation call, so nothing can
activate them. The release build is separate and verified by the script.

Hashes: `Cargo.lock` `26c05a7b…c73ce` (unchanged); release `via`
`ef7e63d1…a991a`. Each scenario writes its artifact under
`scratchpad/execution/rust-foundation-release/s1-harness/runs/`. An artifact
holds:

- summary, sha256 manifest and REPORT;
- a Store backup;
- raw logs, envelopes and events;
- the traces of the crashed and final daemons;
- `cleanup.json`: the final teardown over every committed anchor, crashed
  runs included (all `quiescent` with ESRCH absence proof);
- the ack.

Hygiene: `git diff` has no secrets or machine paths. The Markdown link check
reports 0 broken links, and the skill catalog reports 0 FAIL.

## Open or uncertain

- **Keyed receipt replay (F8/F13)**: waits for T2-B's spawn idempotency
  keys, as above.
- **Queued successors of an `unknown` turn** are not cancelled on restart
  (C1 §7.5, P6). S1 has one turn per session. T2-B's multi-turn queue should
  extend `Engine::recover`.
- **Raw log incomplete on recovery**: the contract attaches a
  `raw_log.incomplete` warning when the crashed connection is unsealed. S1 has
  no `connections` table recording sealed or unsealed state, so recovery adds
  no warning.
- **`failure` on a recovered `unknown` envelope** is `null`, following the
  existing `TransportLost` → `unknown` precedent. C1 §8.2 lists
  `daemon_restart` as a class, but no `FailureClass::DaemonRestart` exists
  yet. Reviewer decision.
- **Host anchor reconciliation at startup** (§7.5 cleanup of a crashed
  daemon's anchor) is not part of this task. The tests prove cleanup through
  the outer harness and the final daemon's shutdown recovery.
- **`fail_io` that stays active** across a best-effort failure write (§11,
  for F12) is not implemented. Commands are single-occurrence; Task 3 can add
  a persistent form.
- **The F24–F27/raw/bounds/store gate line** stays red (no tests) until
  Task 4.

## Round 2

Response to [../sol-review-T2-A.md](../sol-review-T2-A.md) (Sol medium,
SOUND WITH CHANGES). I merged `origin/rust-foundation` (`6ff1e99`, docs
only) first.

| Finding | Fix | Evidence |
|---|---|---|
| **B1: an action could run without an ack** | `Controller::enter` now returns `io::Result`. The ack write error is propagated as the point's error, and no crash, pause or `fail_io` runs until the ack is published: write → fsync → rename → fsync of the directory, so the ack survives a crash. A lost `refused` marker stays ignored, because nothing acts on a refused command. | New unit test `an_unpublished_acknowledgement_is_an_error_and_never_acts`: a directory in the ack's place makes the rename fail; `enter` returns `Err` and nothing crashes. |
| **B2: stale acks** | `Failpoints::arm` removes the point's earlier `.ack`, `.refused` and `.release` markers before it writes the command. `wait_ack` takes the daemon-under-test pid, and every scenario passes `daemon.child.id()`; an ack from another pid is rejected. | `failpoint_harness_rejects_stale_acknowledgements`: arming clears a planted ack and release, and a foreign-pid ack is refused. |
| **B3: restart skipped Host reconciliation** | `Engine::recover` first runs Host's reconciliation through the existing `AdapterRuntime::recover` → Route → Wire → `Host::recover` path, with a 5 s deadline. That path verifies each committed anchor, stops a verified live one, or proves group absence. Each recovered turn then commits `cancel.requested` and `cancel.settled`, and its `unknown` envelope carries `cancel`. | See below. |
| **Deferrable: unreaped client** | `PendingClient` now kills and waits in `Drop`, and on `finish`'s timeout and wait-error paths. | Covered by the scenarios' failure paths. |

**B3 in detail.** The `cancel` object follows the existing force-stop rule,
`stop_outcome`:

- **Outcome:** `forced` only when Host's stop found the vendor live;
  otherwise `requested`.
- **Cleanup `quiescent`** only when every anchor the turn owns has proved
  absence, or when no anchor intent committed. With no intent, no process
  could exist.
- **Cleanup `uncertain`** in every other case, including a failed Host
  reconciliation. An uncertain cleanup adds the `cancel_cleanup_uncertain`
  warning.
- The state stays `unknown` (C1 §7.5), and nothing is resent.
- Recovery still fails startup when its commits fail.

**B3 regression.** All three F10 restart checks now call `restart_cleanup`,
which requires:

- the event tail is `cancel.requested`, `cancel.settled`, `turn.ended`;
- every anchor of the session has committed `absence_time` by the time
  `result` is served, so the proof comes from startup reconciliation and not
  from the final shutdown;
- `cancel.cleanup` is `quiescent`, with outcome `requested` when there is no
  anchor, and `requested` or `forced` otherwise.

Before the fix, with Round 1's `recovery.rs` and the new tests, three tests
failed with:

```
fail: recovery recorded no cleanup settlement: ["turn.queued", "turn.submitted", "turn.ended"]
```

The released-pause test, which does not restart, still passed. After the fix
all pass. A recovered envelope reads
`cancel {outcome: requested, cleanup: quiescent}`. The `uncertain` branch
(unverifiable anchor or Host failure) has no end-to-end regression; producing
it deterministically needs a Host seam that belongs to a later task.

**Files (Round 2):**

- `crates/via-core/src/engine/recovery.rs`
- `crates/via-store/src/failpoint.rs`
- `crates/via-cli/tests/support/failpoints.rs`
- `crates/via-cli/tests/s1_crash_points.rs`

The shared-file hunks are unchanged since Round 1.

**Gate (Round 2):**

- fmt, both clippy runs, `cargo deny` and the layer check pass.
- Default nextest: 126 passed, 2 skipped.
- nextest with the failpoint feature: 134 passed, 2 skipped.
- The F08–F12 line: 7 passed, and 3 more runs of the F08/F10 selection
  passed 7/7 each.
- The release build and `check-release-features.py` pass.
- The `s1_(f2[4567]|raw|bounds|store)_` line still exits 4 with no tests
  (Task 4).

**Still open, as the orchestrator assigned:**

- keyed receipt replay and queued-successor cancellation (T2-B);
- recovered raw-log incompleteness (`via-jm4.7.7`);
- the recovered envelope's `failure: null` versus `daemon_restart`.

## Round 3

Response to [../sol-review-T2-A-r2.md](../sol-review-T2-A-r2.md) (SOUND WITH
CHANGES). I merged `origin/rust-foundation` (`63661b6`, docs only) first.

**B1: startup reconciliation must finish before admission.**
`Engine::recover` now returns a named error instead of discarding Host's
result. `serve` fails startup with `crash recovery failed: <name>`, and the
accept loop never starts. There are three named errors:

- `host_reconciliation_failed`: Host recovery returned an error.
- `host_reconciliation_incomplete`: Host returned at or after its 5 s
  deadline, when later anchors may have got a report without a stop attempt.
- `host_reconciliation_incomplete`: some committed anchor has no report.
  Coverage is checked against a new Store read, `StoreClient::anchor_owners`,
  which returns the ids and owning turns of all committed anchors and no
  marker, identity or path.

Store changes:

- Neither anchor read is silently capped now. `read_anchor_records`, which
  Host uses, and the new owners read both fetch 10,001 rows and fail
  explicitly past 10,000 (`anchor inventory exceeds 10000 records`). A
  failure fails startup through the paths above.

Per-turn cleanup is computed from that inventory (`Reconciled`):

- Every anchor of the turn needs a report: an unreported anchor makes the
  turn `uncertain` and is counted missing, and any missing anchor fails
  startup before recovery commits anything.
- A report's own verified `Uncertain` stays `uncertain`.
- `forced` comes only from the reports.
- A turn with no committed anchor intent is `quiescent`, because no process
  could exist.

**B2: `daemon_restart`.** New `FailureClass::DaemonRestart`, which
serializes as `daemon_restart`. A recovered turn commits it in both the
envelope and `turn.ended`, and the state stays `unknown`.

**Regressions.**

- `recovery::tests` (isolated, 2):
  - An anchor missing from the reports is counted missing, and its turn is
    `(quiescent=false, forced=false)`.
  - A verified `Uncertain` report stays uncertain, and full proof is
    `quiescent`.
- `s1_f10_failed_host_reconciliation_refuses_admission` (real binary): the
  daemon crashes after the prompt write, and the harness makes the anchor row
  unreadable by writing an invalid phase. The restarted daemon exits before
  admission with:

  ```
  crash recovery failed: host_reconciliation_failed: adapter runtime failed: host acquisition failed: process journal unavailable: Write
  ```

  Nothing was committed: the turn is still `running`, has no envelope, and
  its events are only `[turn.queued, turn.submitted]`. After the row is
  restored, a restart recovers the turn as `unknown`.
- All F10 restart checks now also require `failure.class: daemon_restart` in
  the envelope and in `turn.ended`.

**Before** (Round 2 code with the new tests, in a throwaway worktree;
`scratchpad/t2a/r3-before.txt`): 4 of 5 F10 tests failed.

- The three restart tests failed with
  `recovered turn lacks daemon_restart: null`.
- The new test failed with
  `daemon admitted requests after a failed reconciliation`: Round 2's `.ok()`
  let the daemon come up.

After the fix, all pass.

**Not covered deterministically:** the omitted-report and deadline branches
have no end-to-end test. Host always reports every record it lists, so these
branches are covered by the isolated `Reconciled` tests and by code review.
Deterministic end-to-end tests for Host's uncertain and forced paths stay
deferred to the planned Host seam.

**Files (Round 3):**

- `crates/via-core/src/engine/recovery.rs`
- `crates/via-core/src/api.rs` (`DaemonRestart`)
- `crates/via-cli/src/server.rs` (named error)
- `crates/via-store/src/runtime.rs` (`AnchorOwner`, `anchor_owners`)
- `crates/via-store/src/runtime/anchor.rs` (explicit bound, owners read)
- `crates/via-store/src/runtime/sql.rs` (one read arm)
- `crates/via-store/src/lib.rs`
- `crates/via-cli/tests/s1_crash_points.rs`

**Gate (Round 3):**

- fmt, both clippy runs, `cargo deny` and the layer check pass.
- Default nextest: 128 passed, 2 skipped.
- nextest with the failpoint feature: 137 passed, 2 skipped.
- The F08–F12 line: 8 passed; the F08/F10 selection passed in 3 more runs.
- The release build and `check-release-features.py` pass.
- The `s1_(f2[4567]|raw|bounds|store)_` line still exits 4 with no tests
  (Task 4).

## Round 4

Response to [../sol-review-T2-A-r3.md](../sol-review-T2-A-r3.md) (SOUND WITH
CHANGES; both blockers are about availability). I merged
`origin/rust-foundation` (`e374e90`, docs only) first.

**B1: the reconciliation deadline no longer fails startup.**
`Engine::reconcile` walks the complete committed anchor inventory in
`anchor_id` order, 256 anchors per page, from `StoreClient::anchor_owners_page`.
For each page it asks Host for reports on the same id range, through the new
`recover_page` path: Adapter → Route → Wire → `Host::recover_page`.

- **Not fatal; the anchor stays uncertain:**
  - no report because the deadline passed before the page;
  - `HostError::Deadline`;
  - other unproven evidence;
  - an anchor missing from a report page.

  Such an anchor is counted missing and its turn is `uncertain`, never
  `quiescent`. Recovery still commits and the daemon admits requests.
- **Fatal: a Store read or write failure.** This covers Host's journal read,
  via `AdapterError::is_store_failure` → `WireError::is_store_failure` →
  `HostError::Store`/`StoreUnavailable`/`WireError::Raw`, Core's own owners
  read, and the recovery commits (runtime §7). The error is still named
  `host_reconciliation_failed`, and the Round 3 regression still holds.
- A verified `Uncertain` report stays uncertain; `daemon_restart` and the
  coverage check are unchanged.

**B2: no 10,000-anchor limit.** Both anchor reads now page with a cursor
(`WHERE anchor_id > ? ORDER BY anchor_id LIMIT ?`, at most
`ANCHOR_PAGE_LIMIT` = 256), and neither has a cap.

- Core retains per-turn state only for anchors whose owning turn is
  `running`. The owners read carries `turn_running` from a join with `turns`.
  Memory is therefore bounded by one page plus the running turns, not by
  table size.
- `Host::recover`, used by final shutdown, and
  `ProcessJournal::list_anchor_records` now loop over pages instead of
  truncating. Their callers and tests are unchanged.

**Regressions.**

- `s1_f10_recovery_pages_past_ten_thousand_anchors_and_admits`: after a
  crash, the harness commits 10,001 synthetic intent-only anchors for the
  crashed turn. Host cannot verify those. The restarted daemon pages through
  all of them, admits requests, and recovers the turn:
  - `unknown` with `daemon_restart`;
  - `cancel.cleanup: uncertain` with the `cancel_cleanup_uncertain` warning;
  - a later turn completes normally.

  The harness then deletes the synthetic rows so that teardown verifies only
  anchors that ran. On the Round 3 code, in a throwaway worktree:
  `daemon exited before readiness: exit status: 4`.
- `recovery::tests` (3): an unreported anchor is uncertain and counted; a
  page with no reports (deadline passed) stays uncertain; a verified
  `Uncertain` report stays uncertain, and ended turns are not retained.

The deadline-passed path end to end, and Host's own uncertain and forced
paths, remain without deterministic end-to-end tests (Host seam, deferred).

**Files (Round 4):**

- Store:
  - `crates/via-store/src/runtime.rs`: `ANCHOR_PAGE_LIMIT`,
    `AnchorOwner.turn_running`, `anchor_owners_page`,
    `list_anchor_records_page`, and a paging `list_anchor_records`.
  - `crates/via-store/src/runtime/anchor.rs` and `sql.rs`: cursor queries.
  - `crates/via-store/src/lib.rs`.
- Host, Wire, Route and Adapter (outside T2-A's owned paths; `recover_page`
  pass-throughs plus store-failure classification):
  - `crates/via-host/src/host.rs`: `recover_page`, and `recover` looping over
    pages.
  - `crates/via-wire/src/runtime.rs`.
  - `crates/via-routes/src/runtime.rs`.
  - `crates/via-adapters/src/runtime.rs`.
- Core and tests: `crates/via-core/src/engine/recovery.rs`,
  `crates/via-cli/tests/s1_crash_points.rs`.

**Gate (Round 4):**

- fmt, both clippy runs, `cargo deny` and the layer check pass.
- Default nextest: 129 passed, 2 skipped.
- nextest with the failpoint feature: 139 passed, 2 skipped.
- The F08–F12 line: 9 passed; the F08/F10 selection passed in 3 more runs.
- The release build and `check-release-features.py` pass.
- The `s1_(f2[4567]|raw|bounds|store)_` line still exits 4 with no tests
  (Task 4).
