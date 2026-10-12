# T2-B report: multi-turn sessions, resume, queues and retries

Branch `claude/t2-b-rust-foundation-juogko`, based on `64419c0`. Scope:
[`../b.md`](../b.md) (F13, F14, F17, F28, `wait.timeout_ms`, harness gaps on
`via-jm4.7.6`). Toolchain 1.98.1. `XDG_RUNTIME_DIR` was set to a private
0700 directory for every run.

## What changed in Core

Core assumed one turn per session. The drive hard-coded turn 1, submission
read `turn.queued` at seq 1 and wrote `turn.submitted` at seq 2, and every
event took `record.seq + 1` from a per-turn counter. A second writer on the
same session, such as a `resume` committing `turn.queued` while a turn runs,
would have collided on the sequence. The rework has four parts.

- **Shared event head** (`engine/journal.rs` `Head`). Each session has one
  next-sequence cell, and every writer of the session allocates and commits
  under its lock: drive events, acceptance, submission, `turn.ended` and
  `session.closed`, resume's `turn.queued`, and queued cancellations. When a
  commit's outcome is unknown, the head becomes unknown and is re-read from
  Store (`sessions.next_seq`) before the next event. `TurnRecord.seq` is
  gone. `reconcile` now treats an event at the uncertain sequence as the
  turn's own only if it carries the turn's number. That event is either the
  turn's own or another writer's. Before, a foreign event there was
  `CorruptEvidence`.
- **Per-session dispatch** (new `engine/queue.rs`). A `Slot` holds the head
  and a watch of "turns up to N finished". The drive for turn N waits until
  N-1 has finished, so there is one running turn and FIFO order. This does
  not depend on handoff order. A predecessor that did not end cleanly
  (`unknown`, a terminal that could not be persisted, a failed submission,
  or a force-stopped turn) sets `cancel_queue`. Each later turn is then
  committed `cancelled` in order, with no submission (C1 P6, §7.2). The
  Store also enforces one running turn per session with a partial unique
  index.
- **Verbs.** `spawn` takes `idempotency_key`. The new `resume` takes an
  optional `op_key`. `wait` takes `timeout_ms`. `result` and `wait` resolve a
  bare session address to its latest turn and return `session_not_found` or
  `turn_not_found`. Before, a bare session meant turn 1 and a missing turn
  waited 30 s. The capability DTO now reports `resume` as `native`.
- **Retry identity** (`api.rs` `retry_identity`). C1 P4 and runtime §6
  define it as the original params bytes, taken from the request line
  through `serde_json` `RawValue`, with only the top-level `handle` value
  replaced by its SHA-256 hash in hex. Whitespace and member order are kept.
  Duplicate top-level members are refused. Replay lookup comes after
  authentication and before stop, admission, queue and capability checks.
  A replay creates no drive.

Store (`via-store`): schema v1 gains `spawn_keys`, `operations` (PK
`(session, op_key)`, FK to the turn) and `turns.queued_at`/`queued_seq`, plus
the one-running index. New ops are `commit_keyed_spawn`, `spawn_key`,
`commit_resume` (turn, `turn.queued` and operation in one transaction;
refuses a closed session or a turn that is not the next one),
`operation`, `session_snapshot`, `queued_turn` and `next_seq`.
`commit_terminal` also accepts `queued → cancelled`. Session state stays
`active` while queued or running work exists, and `closed` is final.

CLI (`via-cli`): `via resume <session> --prompt P [--op-key K]
[--handle-file|--handle-stdin|--handle]` prints the turn receipt. `via spawn
--idempotency-key K` is new. `via wait <addr> --timeout-ms N` sets the
client read timeout to N + 5 s. The server passes raw params to Core and
hands `(SessionId, TurnNumber)` to drives. The drive reads the prompt from
Store at submission, so queued prompts are not held in memory (runtime §8).

## Findings: failure mode, regression, failure before the fix

All scenario tests are in `crates/via-cli/tests/s1_sessions.rs`. They run
through the real `via` binary and daemon with full evidence: summary, sha256
manifest, SQLite backup, raw logs, events and report. The shared harness is
`tests/support/daemon.rs`.

| Finding | How it failed | Regression | Output before the fix |
|---|---|---|---|
| F13 spawn retry | `idempotency_key` did not exist. `SpawnParams` refused it as `unknown_field`, so a keyed spawn could not commit and a keyless retry made a second session. | `s1_f13_spawn_retry_after_lost_reply_replays_one_session`: a raw spawn whose reply is dropped, then a CLI retry that gets the same receipt; one session and one turn in Store; changed prompt, other handle and whitespace-only change are each `idempotency_conflict`. | `fail: SELECT count(*) FROM sessions never reached 1` (the daemon refused the keyed spawn) |
| F14 resume retry | There was no `resume` verb (`method_not_found`) and no `op_key`. | `s1_f14_resume_retry_with_op_key_adds_one_turn`: a raw keyed resume whose reply is lost, then a retry that gives turn 2; a replay after the turn ended; changed params are a conflict; `invalid_handle` and `session_not_found`; two keyless retries become turns 3 and 4; a session address reads the latest turn; each envelope's event range starts at its own `turn.queued`. | `fail: SELECT count(*) FROM turns never reached 2` |
| F17 queue | There was no second turn, so no queue. | `s1_f17_ninth_queued_turn_is_queue_full_and_order_kept`: turn 1 is gated; turns 2–9 are queued at positions 0–7; the ninth queued resume is `queue_full`; nothing else is submitted while turn 1 holds; all ten turns submit only after their predecessor ended; the refused request consumed no turn number. | `error: unrecognized subcommand 'resume'` |
| F28 independent sessions | Needed multi-turn sessions. The shared head is per session. | `s1_f28_two_callers_drive_two_sessions_without_crosstalk`: two threaded callers; both first turns are live at once; each resumes while running; a cross-session handle is `invalid_handle`; each history is dense from 1 and holds only its own events and output. | `error: unrecognized subcommand 'resume'` |
| `wait.timeout_ms` | `ReadParams` refused `timeout_ms` as `unknown_field`, and the wait was fixed at 30 s. | `c1_wait_timeout_ms_bounds_the_wait` (CLI and raw). | `error: unexpected argument '--timeout-ms' found` |
| Daemon-wide queue bound | There was no bound. | `via-core/tests/queue_bounds.rs`: at 128 queued turns (127 spawns and 1 keyed resume), the next spawn and resume are `admission_refused` ("too many queued turns"), and the keyed resume still replays. This is at Core level because 128 live e2e turns is impractical. | This is new code, so I checked it by mutation. With the limit raised the test FAILs. |
| Multi-writer reconcile | The old `reconcile` returned `CorruptEvidence` when another writer's event held the uncertain sequence. | `journal::tests::an_unused_uncertain_sequence_taken_by_another_writer_is_not_the_turns` | Mutation that drops the turn check: `unwrap()` on `store_error` |
| Harness: 0700 dirs | `tempfile` directories in this environment are 0755. The evidence directory, its `raw/` (plain `create_dir` under umask 022) and the new sandbox root were all 0755. | `evidence_collector::evidence_and_raw_directories_are_private`, plus `support/daemon.rs` `private_dir`/`assert_private` for sandbox, state, runtime and sync. | `left: 493 right: 448` (0o755 vs 0o700) and `<tempdir> has mode 755, expected 700` |
| Harness: `infrastructure_failure` | This was a coverage gap, not a defect. The classification already existed. | `scenario_runner::infrastructure_failures_are_classified_as_infrastructure`: an action error and a cleanup panic after a passing action. | It passed on the pre-fix code; nothing needed fixing. |

A mutation check that removes the dispatch gate (`Slot::turn` returns
immediately) makes F17 fail with `turn 4 receipt: … queue_position 1`.

## Files

Owned: `crates/via-core/src/{api.rs,lib.rs,engine.rs,engine/drive.rs,
engine/journal.rs,engine/journal/tests.rs,engine/terminal.rs,engine/queue.rs}`,
`crates/via-core/tests/{force_stop.rs,queue_bounds.rs}`,
`crates/via-store/src/{lib.rs,runtime.rs,runtime/sql.rs}`,
`crates/via-cli/src/{client.rs,main.rs,server.rs}`,
`crates/via-cli/tests/{s1_sessions.rs,support/daemon.rs,support/evidence.rs,
evidence_collector.rs,scenario_runner.rs}`.

Outside the owned paths, and each kept minimal:

- `crates/via-fake-agent/src/main.rs` and `tests/scenarios.rs`: a fixture
  may be `{"scripts":[...]}`, and each launch runs the first script whose
  `expected_request` its start request contains. Route pins
  `vendor_turn_id` to `fake-turn-N`, so one static script cannot serve turn
  2. The plan lists fake-agent scripts under Task 2.
- `docs/specs/runtime-contracts.md` §11 (fixture shape): one sentence
  documenting that form.
- Root `Cargo.toml`: enables the `raw_value` feature of the existing
  `serde_json` dependency. It adds no crate, and `Cargo.lock` is unchanged.
  Byte-identical retry identity needs the raw params slice.

**Hunks shared with T2-A** (T2-A's hunks there are failpoint call sites
only). In `engine/drive.rs`, `drive` is split into queue prologue and `run`,
and `submit`/`commit_submission`, `observe`'s acceptance, `commit_turn_ended`
and `cancel_queued` changed. In the Store spawn and submission commit path,
`Command::Spawn` gained an `Option<SpawnKey>`, `commit_spawn` inserts the key
and `queued_at`/`queued_seq`, and `writer_loop` was split into
`serve_read`/`serve_write` for the line limit. `commit_submission`'s SQL is
unchanged. Failpoints around submission belong between `head.lock` and
`journal.commit_submission` in `commit_submission`.

## Gate

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | pass |
| `cargo nextest run --locked --workspace` | 134 passed, 2 skipped (the same two ignored as at base; 122 → 134) in about 31 s |
| `cargo deny check` | advisories, bans, licenses and sources ok (existing duplicate warnings) |
| `python3 scripts/check-layers.py` | pass (exit 0) |

The failpoint gates do not apply yet because `via-cli/test-failpoints` does
not exist on this base (T2-A).

## Open or uncertain

1. **Schema.** I extended v1 in place instead of adding a v2 migration.
   Runtime §6 already lists `spawn_keys` and `operations` as v1 and no Store
   has been released. A Store created by an earlier dev build lacks the new
   tables and columns and fails at the first spawn.
2. **Key bound.** `idempotency_key` uses C1's `op_key` bound (1–64
   characters). C1 gives no bound for it, so this is Proposed and needs
   owner confirmation.
3. **Sticky queue cancel.** After a turn ends `unknown` or fails to persist,
   `cancel_queue` stays set for that session for the daemon's lifetime, so
   later resumes are cancelled without dispatch. On the fake route that only
   follows a force stop or a Store failure. A session left nonterminal by an
   earlier daemon is treated the same way (no restart recovery in S1; Task 3).
4. **Queued cancellation shape.** `state: cancelled`, `stop_reason:
   interrupted`, `cancel: null`, no `submitted_at`. There is no C1 warning
   code for "cancelled behind predecessor", so none is emitted.
5. **Daemon-wide count is in memory.** It counts this daemon's receipted,
   not-yet-submitted turns. The per-session count comes from Store under
   Core's admission lock. Both are checked before commit.
6. **Not done, out of scope.** `store_error` before a receipt does not carry
   `data.commit_outcome`/`retry` (C1 §8.1). There is no `status` verb, so
   queued `op_key`s cannot be listed yet. Duplicate keys are refused only at
   the top level of params, not in nested objects.
7. **Turn connection ids.** Turn 1 keeps `c_<session>` and later turns use
   `c_<session>t<N>`, which keeps existing tests and evidence names stable.
8. `resume` prints only the receipt. C1 P1 defines foreground waiting for
   `spawn` only.

## Round 2

Sol medium (`../sol-review-T2-B.md`, UNSOUND) found three merge blockers and
the sticky-cancel item. I merged `origin/rust-foundation` (`a7b60c2`, docs
only) and applied the orchestrator decisions. Round 1 open items 1 (schema),
2 (key bound) and 3 (sticky cancel) are closed below. The missing §8.1
fields from item 6 are also done.

| Item | Failure mode | Regression and its output before the fix | Fix |
|---|---|---|---|
| B1 uncertain receipt | SQLite commits the spawn or resume, then reports an uncertain outcome. Core returned `store_error` before registering the turn. A keyed retry then replayed a receipt for a turn nobody would drive. | `engine::tests::a_committed_{spawn,resume}_whose_reply_is_lost_is_handed_off_once` and `an_unknown_receipt_outcome_is_store_error_and_its_keyed_retry_adopts_it_once`. Output before: `unwrap()` on `store_error` (both), and `left: {"kind":"store_error"}` vs `right: {…"commit_outcome":"unknown","retry":"same_key_only"}`. | `receipt_outcome`: a definite failure is `store_error` with `commit_outcome: not_committed`. An uncertain one is reconciled with a Store read (turn N exists → committed; admission is held, so only this request could have created it). If committed, the turn is registered and handed off once. If still unknown, it is `store_error` with `commit_outcome: unknown` and `retry: same_key_only`, and the turn is kept as an orphan. The keyed retry that replays it adopts it (registers and hands off) exactly once. Later retries hand off nothing. An unknown resume marks its number done in the slot, so successors check durable state instead of waiting on a drive that does not exist. |
| B2 schema | A Store already at v1 skipped the new tables. | `persistence::unreleased_v1_store_is_refused_with_a_recreate_instruction`: before, `a v1 Store opened`. | `SCHEMA_VERSION = 2`. Any older nonzero version is refused while still read-only, leaving its bytes untouched, with "Store schema v1 is an unreleased development format with no migration; stop the daemon and recreate the Store by removing store.sqlite3 from the State directory". The rule is in runtime §6. |
| B3 queue ceiling | Only Core checked the 8-per-session ceiling. | `persistence::a_ninth_queued_turn_is_refused_inside_the_receipt_transaction`: before, `Ok(())`. | `commit_resume` counts the session's queued turns inside its transaction and refuses a ninth (`Constraint`). `via_store::SESSION_QUEUE_LIMIT` is the one constant, and Core keeps its check to answer `queue_full`. |
| Sticky cancel | `cancel_queue` was set by any unclean drive and never cleared. | `engine::tests::a_turn_behind_a_settled_terminal_runs_however_its_drive_ended`: before, the assertion "turn 2 was dispatched" failed. `successors_are_cancelled_while_the_predecessor_is_unknown_or_unresolved` passed before and after; it is the kept behaviour. | The slot now only orders drives. `Engine::dispatchable` reads Store `predecessors`. A turn runs only if no earlier turn is queued or running, and the latest submitted earlier turn is terminal, not `unknown`, and not `cleanup: pending`. Turns cancelled while queued are passed over. A Store read failure permits nothing. |
| Key bound | The check counted UTF-8 bytes and allowed spaces and control characters. | `api::tests::retry_keys_are_one_to_sixty_four_printable_ascii_characters`: before, `unwrap_err()` on `Ok(Some(" "))`. | Keys are 1–64 bytes, each 0x21–0x7E. C1 §3 records this in the `op_key` sentence and the `spawn` params. |

The fault backend is `#[cfg(test)]` only (`Engine.faults`): it loses a
receipt commit's reply after it succeeds, and makes the reconcile read fail.
Production builds have no such seam. The `store.commit.reply_lost` end-to-end
test waits for T2-A's controller.

**Files (round 2):** `via-core` `api.rs`, `lib.rs`, `engine.rs`,
`engine/{drive,queue,tests}.rs`; `via-store` `lib.rs`, `runtime.rs`,
`runtime/sql.rs`, `tests/persistence.rs`; `via-cli/src/server.rs`
(`commit_outcome` field; two error literals moved to consts for the line
limit); `docs/specs/runtime-contracts.md` §6; `docs/specs/via-api-v1.md` §3,
§3.2. `ApiError.commit_outcome` is a one-byte `ReceiptOutcome` enum, which
keeps the refusal type under clippy's `result_large_err` bound. The shared
drive hunk in `drive.rs` changed again (`drive`/`dispatchable`/`run`).

**Gate (round 2):** fmt, clippy `-D warnings`, deny and check-layers pass.
nextest ran 142 passed, 2 skipped (the same two ignored). Markdown links: 0
broken.

**Still open:**
- An *unkeyed* receipt whose outcome stays unknown can never be adopted,
  since C1 says retry with the same key only. If it did commit, it stays
  queued and unresolved, so later turns of that session are cancelled.
- A turn behind an `unknown` predecessor is cancelled for as long as that
  predecessor stays unknown. Revision by late evidence is Task 3.
- Out of scope, as before: `status` and restart recovery.

## Round 3

Sol medium re-review (`../sol-review-T2-B-r2.md`, UNSOUND): the round-1
blockers were fixed, but the round-2 dispatch decision could permanently
cancel valid queued work. I merged `origin/rust-foundation` (`c8b571e`, docs
only) and applied the orchestrator decisions.

| Item | Failure mode | Regression and its output before the fix | Fix |
|---|---|---|---|
| R3-1 wait, not cancel | A successor saw an orphan predecessor (a committed receipt whose reply was lost, still `queued`) and cancelled itself. A later adoption of the predecessor could not restore it. | `engine::tests::a_successor_waits_for_an_orphan_predecessor_to_be_adopted_then_runs`: turn 3's drive starts, and after 400 ms the keyed retry adopts turn 2, which then settles. Before: panic at the "turn 3 stays queued" check, because turn 3 was already cancelled. | `Engine::dispatch` returns `Run`, `Wait` or `Cancel` from the durable read. Any unresolved earlier turn (queued, orphan, running, or no durable terminal) means `Wait`. Only a latest submitted predecessor that is durably `unknown` or has `cleanup: pending` means `Cancel` (C1 §7.3). A waiting drive rechecks when any drive of the session finishes, or every 250 ms (`DISPATCH_RECHECK`), or on a force stop. Under `daemon/stop --force` a waiting turn is cancelled without submission, since the session closes. |
| R3-2 read failure | Any failure of the predecessor read was mapped to a durable cancellation. | `a_failed_predecessor_read_waits_and_the_turn_later_runs`: 2 injected read failures (`Faults.predecessors_unreadable`). Before: the "the turn runs once the read succeeds" check failed because the turn was cancelled. | A read failure means `Wait`, and the read is retried on the recheck interval. It never cancels. |
| R3-3 unkeyed orphan | An unkeyed receipt with an unknown outcome could only be adopted by a keyed replay, which it would never get. | `an_unkeyed_committed_receipt_is_reconciled_and_handed_off_once`: lost reply, reconciliation unreadable across one admission, then readable. Before: `try_recv` found no adoption. | `reconcile_orphans` reconciles every orphan, keyed or not, while holding admission. It runs at each spawn and resume (after the keyed-replay lookup, so a keyed retry still adopts through its own receipt) and on each wait recheck. A committed orphan is registered and sent to daemon main exactly once: capacity on a bounded adoption channel (128) is reserved first, and removal from the orphan set happens under its lock. A turn Store shows never committed is forgotten. A turn that cannot be read yet stays. Nothing is adopted after a stop is accepted. Daemon main drives adoptions like handoffs, including a drain of the channel in final shutdown. C1 §8.1 now says committed work always runs and only keyed callers may retry. |

The round-2 test `successors_are_cancelled_while_the_predecessor_is_unknown_or_unresolved`
is renamed `..._behind_an_unknown_or_cleanup_pending_predecessor`, because
an unresolved predecessor now means wait. It covers `unknown` and
`cleanup: pending`.

**Files (round 3):** `via-core/src/engine.rs` (orphan reconciliation,
adoption channel, `take_adoptions`, predecessor-read fault),
`engine/drive.rs` (`Dispatch`, the wait loop and `DISPATCH_RECHECK`; this is
the shared drive hunk), `engine/queue.rs` (`subscribe`),
`engine/tests.rs`, `via-cli/src/server.rs` (adoption receiver in daemon main
and final shutdown), and `docs/specs/via-api-v1.md` §8.1.

**Gate (round 3):** fmt, clippy `-D warnings`, deny and check-layers pass.
nextest ran 145 passed, 2 skipped (the same two ignored). Markdown links: 0
broken.

**Still open:**
- A turn waits for as long as a predecessor stays unresolved. If the
  predecessor's terminal can never be made durable (persistent Store
  failure), `daemon/stop --drain` waits on that turn until a force stop.
  Only a force stop or restart recovery (Task 3) ends it.
- Orphans are held in memory. After a daemon restart, a committed orphan
  stays `queued` until Task 3's recovery.
- The end-to-end `store.commit.reply_lost` test waits for T2-A's failpoint
  controller.
