# W4-I report: store_error data, bounded unresolved set, deadline class

Branch `claude/w4-i-rust-foundation-ce2rcm`. The findings are 1, 4 and 5
from [`../../w3/sol-review-W3-G.md`](../../w3/sol-review-W3-G.md).

Commits:
- `916756d` fix(routes): a raw append that outlives the turn deadline is `deadline_wall`
- `5448ee1` fix(core): C1 `store_error` data and a bounded unresolved-turn set

## 1. C1 `store_error` data (W3-G finding 1)

**Failure mode.** `journal::read_result` returned the static `ApiError::STORE`
for a turn whose terminal could not be made durable. `ApiError` was a `Copy`
value with static strings, and `server.rs` rendered `data` as `{"kind":…}`
only. So `result` and `wait` returned `store_error` without the C1 §3.8/§9
`data.session`, `data.turn`, `data.durable_state` and
`data.terminal_persisted:false`.

**Regressions.**
- `crates/via-core/src/engine/journal/tests.rs`
  - `result_and_wait_report_the_unpersisted_turn_with_c1_data`: a real
    `Engine` over a real Store. The turn is receipted and submitted, and its
    terminal fails through the existing fault journal (uncertain commit,
    unreadable head). The test calls the public `Engine::result` and
    `Engine::wait` and asserts the code, the message and the complete
    `error.data`.
  - `unsettled_turn_reads_as_store_error_not_running` (existing) now asserts
    the complete `data`.
  - `a_turn_whose_submission_cannot_commit_reports_its_queued_state`: see §2.
- `crates/via-cli/src/server.rs`
  `unpersisted_turn_renders_the_complete_store_error_response`: the complete
  JSON-RPC response (`jsonrpc`, `id`, `code`, `message`, `data`) that the
  server's `error()` renderer produces for that error.

I could not run this end to end through the real `via` binary. The fake agent
cannot make the daemon's Store fail, and via-cli has no failpoint wiring yet
(`via-jm4.7.6`/`.7.7`). So the proof has two layers: Engine `result`/`wait`
return the exact error, and the server renders that error completely.

Output before the fix. The old behaviour was restored at the fix sites:
`data()` renders `kind` only, and `read_result` returns plain `STORE`.

```
FAIL engine::journal::tests::result_and_wait_report_the_unpersisted_turn_with_c1_data
  left:  {"kind":"store_error"}
  right: {"durable_state":"running","kind":"store_error","session":"s_0123456789ab","terminal_persisted":false,"turn":1}
FAIL engine::journal::tests::unsettled_turn_reads_as_store_error_not_running   (same left/right)
```

The new tests call the new API, so they cannot compile against the pre-change
tree. This fix-disabled run is the failure evidence, as in W3-G.

**Fix.**
- `ApiError` gets `unpersisted: Option<Box<Unpersisted>>` (session, turn,
  last committed `TurnState`). It loses `Copy` and keeps `Clone`.
- `ApiError::unpersisted(..)` builds the error.
- `ApiError::data()` renders the C1 `error.data` object. `server.rs` now uses
  it and still adds `kind2`.
- Wire choices:
  - `turn` is the number, which matches `session` + `turn` in envelopes and
    events.
  - `durable_state` is the C1 turn-state word: `queued` or `running`.

## 2. Bounded unresolved set (W3-G finding 4)

**Failure mode.** `Unresolved` kept each failed turn until daemon exit, even
after a read found its terminal durable, and had no bound. Also, when `submit`
failed, `drive` returned early and left the turn `Pending` forever. That turn
then read as `turn_not_finished`, and `wait` spun to `wait_timeout`, instead
of `store_error`.

**Regressions** (`journal/tests.rs`):
- `a_failed_turn_whose_terminal_becomes_readable_is_removed`: once a read finds
  the terminal, the entry is gone and admission is restored.
- `failed_turns_are_bounded_and_each_keeps_store_error`: with
  `FAILED_TURNS_LIMIT` failed turns, `Engine::spawn` refuses with a plain
  `store_error` and the set does not grow. The first and last failed turns
  still read their full `store_error`.
- `only_failed_turns_count_toward_the_bound`: pending (in-flight) turns do not
  count toward the bound, and resolving a failed turn frees a slot.
- `a_turn_whose_submission_cannot_commit_reports_its_queued_state`.

Output before the fix. The old behaviour was restored: no settle on read, no
bound, and no `fail` on a submit error.

```
FAIL a_turn_whose_submission_cannot_commit_reports_its_queued_state
  left: {"kind":"turn_not_finished"}   right: {… "durable_state":"queued" …}
FAIL a_failed_turn_whose_terminal_becomes_readable_is_removed   (tests.rs:458, entry kept)
FAIL failed_turns_are_bounded_and_each_keeps_store_error
  left: ("harness_unavailable", true)  right: ("store_error", true)
FAIL only_failed_turns_count_toward_the_bound                   (tests.rs:477, never full)
Summary 9 tests run: 3 passed, 6 failed
```

**Fix** (`journal.rs`):
- Entries are `Pending` or `Failed(TurnState)`.
- `read_result` removes a `Failed` entry once it reads the durable terminal.
- `admits()` is false once `FAILED_TURNS_LIMIT` (256) failed entries are
  retained, and `spawn` then refuses with `store_error`.
- `drive` records a submit failure as `Failed(Queued)`.

**Decision for the orchestrator.** I bound the set by refusing admission, not
by evicting entries. Eviction would lose a turn's `store_error` read, and the
brief requires that read to keep working. Memory is bounded by the limit plus
the turns in flight when the limit is reached.

This overlaps F12's "stop admission" (`via-jm4.7.7`), which will stop
admission at the first Store failure, so this limit then becomes a backstop.
A failed turn whose terminal becomes durable but is never read keeps its slot
until final shutdown re-reads it.

## 3. Raw append past the turn deadline (W3-G finding 5)

**Failure mode.** Route's `wire_cause` mapped every `WireError::RawDeadline`
to `RouteError::Store`, so Core classified the turn `store`. Every Wire call
whose error reaches `wire_cause` runs under the turn's work deadline, so an
expired append there means the work deadline expired, and C1 §7.6 calls for
`deadline_wall`. The failure drain runs under the separate 3 s cleanup
deadline and reports only lost bytes (`raw_incomplete`), never a cause.

**Regression.** `crates/via-core/tests/route_stream.rs`
`raw_append_stalled_past_the_turn_deadline_is_a_wall_deadline`, through the
real Route, Wire, Host and Store:
1. The vendor emits acceptance.
2. The test stalls the raw worker.
3. The vendor emits an ordinary text line, then sleeps 30 s.
4. The test expects `RouteError::Deadline` (Core maps it to `deadline_wall`),
   `raw_incomplete: true` and completion within 10 s under a 3 s turn
   deadline.

The cleanup-deadline disposition is the existing
`stalled_raw_worker_cannot_hold_failure_cleanup_past_its_deadline`, where the
`Protocol` cause stays authoritative. To support the new test, `Child::execute`
now takes the turn deadline; the other cases pass the old 20 s.

Output before the fix:

```
panicked at crates/via-core/tests/route_stream.rs:399:5:
RouteFailure { cause: Store { turn: TurnNumber(1) }, evidence: None, exit: Some(ExitReport { code: None, signal: Some(15) }), raw_incomplete: true, cleanup: Some(Quiescent), forced: true }
Summary 4 tests run: 3 passed, 1 failed
```

**Fix.** `WireError::RawDeadline` now maps to `RouteError::Deadline`.
`WireError::Raw` stays `Store`.

## Shared hunks

These files are also edited by W4-H.
- `crates/via-cli/src/server.rs` (error rendering only):
  - `unpersisted: None` added to the five `ApiError` literals.
  - `Refusal` loses `Copy`.
  - `error()` uses `error.data()`.
  - One rendering test, placed first in `mod tests`.

  Any `ApiError` literal that W4-H adds needs `unpersisted: None`.
- `crates/via-core/src/engine.rs`:
  - the `TurnState` import;
  - the `admits()` check in `spawn`;
  - the submit-failure arm in `drive`;
  - the `fail(.., TurnState::Running)` call in `finish_turn`.

## Gate

Run from the repo root with `XDG_RUNTIME_DIR` set to a private 0700
directory:

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | clean |
| `cargo nextest run --locked --workspace` | 106 passed, 1 skipped (the existing root-only `#[ignore]`) |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | exit 0 |

The `test-failpoints` gate lines do not apply yet, because via-cli has no such
feature. `Cargo.lock` is unchanged, and I added no dependencies.

## Open or uncertain

- The shape of `durable_state` is my choice: a C1 turn-state string. C1 does
  not fix its type.
- **Deferred (Sol W4-I review):** after an uncertain `commit_submission`, the
  last *known* commit is `turn.queued`, so the error reports `queued` even if
  `turn.submitted` did persist. The round-2 regression covers only a
  submission that left nothing durable. The later task that settles
  uncertain submissions should reconcile the durable submission head before
  it reports the state.
- **Deferred (Sol W4-I r2 review):**
  `a_receipted_turn_whose_submission_cannot_commit_reports_its_queued_state`
  is a characterization test of the `submit` helper and its C1 reads.
  Round-1 production code already recorded `Failed(Queued)`, so it has no
  genuine round-1 red result; its reported failure came from removing that
  behaviour artificially. No regression drives a submission failure through
  `drive`.
- Daemon-wide Store health, cleanup and Store reply bounds belong to Task 3
  (`via-jm4.7.7`), as the W4 README says; the Sol review agrees.
- There is no true end-to-end test through the `via` binary until failpoints
  can fail the daemon's Store.
- The set is still in memory only. After a restart, nothing settles these
  turns; recovery is not in S1.

## Round 2

The review is [`../sol-review-W4-I.md`](../sol-review-W4-I.md). I merged
`origin/rust-foundation` first; the merge changed docs only. This round fixes
the three "Blocks merging" findings. Commit: `1618fbb`.

### R2-1. The set was unbounded through in-flight turns

**Failure mode.** `admits()` counted only `Failed` entries. Every accepted
spawn added a `Pending` entry, so any number of turns could be in flight, and
all of them could fail afterwards.

**Regression.** `in_flight_turns_count_toward_the_bound` fills the set with
`UNRESOLVED_LIMIT` pending receipts and expects `Engine::spawn` to refuse with
`store_error`. It also checks that one resolution admits again (the next
refusal is `harness_unavailable`, because the fake is unconfigured in unit
tests). This test replaces `only_failed_turns_count_toward_the_bound`, which
asserted the wrong rule.

**Fix.** `Unresolved::admits()` bounds the total number of entries.
`FAILED_TURNS_LIMIT` is renamed `UNRESOLVED_LIMIT` (256). `spawn` checks the
bound and inserts the receipt under the same `admission` lock, so the set
never exceeds the limit.

**Consequence.** The limit also caps concurrent in-flight turns at 256.

### R2-2. Durable-but-unread turns kept admission closed

**Failure mode.** A `Failed` entry was forgotten only when `read_result`
found its terminal. Terminals that became durable after a failed read-back,
but that no caller read, could hold admission closed indefinitely.

**Regression.** `durable_terminals_are_settled_before_admission_is_refused`:
1. It fills the set with 255 failed turns that have no terminal.
2. It adds one failed turn whose terminal it then commits straight to Store.
   It uses `commit_turn_ended` on the real Store, never a read through Core.
3. It expects `spawn` to be admitted, the durable turn to be forgotten and the
   other 255 to be kept.

**Fix.** `journal::admits` runs only when the set is full. It re-reads the
durable result of each failed turn, forgets the ones that have a terminal,
and then re-checks the bound. The whole sweep is bounded by `SETTLE_BOUND`
(2 s, the F12 outcome-resolution bound). A read that fails or times out keeps
its turn. `spawn` holds the `admission` lock for at most that bound.

### R2-3. The submission regression had no receipt

**Failure mode.** The round-1 test drove a session with no receipt, so
submission failed only because `turn.queued` was absent.

**Fix.** `submit` is now an associated function over the `TurnJournal` port,
like `finish_turn`. It records `Failed(Queued)` on any error. The port gains
`commit_submission`, and `StoreClient` implements it. `drive` calls
`Self::submit(&self.store, &self.unresolved, …)`.

**Regression.**
`a_receipted_turn_whose_submission_cannot_commit_reports_its_queued_state`:
1. It commits a real receipt through Store and tracks it as `spawn` does.
2. It injects a submission failure through the fault journal: an uncertain
   error with nothing durable.
3. It asserts the code, the message and the complete C1 data through
   `Engine::result` and `Engine::wait`.

This test replaces `a_turn_whose_submission_cannot_commit_reports_its_queued_state`.

### Output before the fix

I restored the round-1 behaviour at each fix site:
- `admits` counts `Failed` entries only;
- there is no settlement sweep;
- there is no `fail(Queued)` on a submission error, which is also the
  pre-W4 base behaviour.

```
FAIL durable_terminals_are_settled_before_admission_is_refused
  left: "store_error"         right: "harness_unavailable"
FAIL in_flight_turns_count_toward_the_bound
  left: "harness_unavailable" right: "store_error"
FAIL [30.170s] a_receipted_turn_whose_submission_cannot_commit_reports_its_queued_state
  left: (-32015, "turn has not finished")  right: (-32018, "durable storage failed")
Summary 10 tests run: 7 passed, 3 failed
```

The 30 s runtime is `wait` spinning to `wait_timeout` on the old code, while
`result` reports `turn_not_finished`.

### Files changed

- `crates/via-core/src/engine.rs` (shared): the admission call in `spawn`,
  the `submit` call in `drive`, and `submit` split into `submit` and
  `commit_submission` over the journal port.
- `crates/via-core/src/engine/journal.rs`
- `crates/via-core/src/engine/journal/tests.rs`

### Gate

Run with `XDG_RUNTIME_DIR` set to a private 0700 directory:

| Check | Result |
|---|---|
| fmt | clean |
| clippy | clean |
| nextest | 107 passed, 1 skipped (the existing root-only `#[ignore]`) |
| deny | ok |
| check-layers | exit 0 |
| Markdown links | 0 broken in this report |

## Round 3

The review is [`../sol-review-W4-I-r2.md`](../sol-review-W4-I-r2.md). I merged
`origin/rust-foundation` first; the merge changed docs only. This round fixes
both merge blockers in commit `78b4a52`. The deferred item is recorded under
Open.

### R3-1. Capacity was reported as a Store failure

**Failure mode.** With the set full of ordinary in-flight turns, `spawn`
returned `store_error` although no Store operation had failed. C1 §8.1
assigns resource admission refusal to `admission_refused`.

**Fix.** `journal::admission` replaces the boolean `admits`. When the set is
still full after settlement:
- it returns the new `ApiError::TURNS_AT_CAPACITY` (-32012
  `admission_refused`, "too many unresolved turns") if no failed turn
  remains;
- it returns `store_error` while a turn whose terminal could not be made
  durable is still retained.

**Tests.**
- `in_flight_turns_count_toward_the_bound` now expects
  `(-32012, "admission_refused")`.
- `failed_turns_are_bounded_and_each_keeps_store_error` still covers the
  actual Store failure: a full set of failed turns is `store_error`.

### R3-2. Settlement could starve a durable terminal

**Failure mode.** The sweep read failed turns one by one in `HashMap` order
under one 2 s budget. Slow early reads could use up the budget before a later
durable terminal was checked, and admission then stayed closed.

**Rejected alternative.** I first read all failed turns concurrently. The
full gate then failed intermittently (1 in 6 isolated runs), because Store's
command queue holds 128 and `send` uses `try_send`. The overflow reads
returned `Unavailable`, so the durable turn could be missed. That fan-out
would also crowd out live turns' commits.

**Fix.**
- A new read-only Store command, `StoreClient::terminated(turns)`, returns
  which of up to 1000 turns have a committed terminal envelope. It runs as
  one indexed `EXISTS` per key, in one worker operation that uses one queue
  slot.
- `TurnJournal` gains `terminated`.
- `settle_failed` asks once for the whole failed set, bounded by
  `SETTLE_BOUND` (2 s). A failed or expired query keeps every turn.
- Every candidate is inspected whatever the order or the number of entries.

**Regression.**
`a_durable_terminal_behind_delayed_reads_is_settled_within_the_bound`:
1. It sets up 255 failed turns and one failed turn whose terminal is durable.
2. In the fault journal, every other turn's per-turn `result` read stalls for
   3 s, past the bound.
3. It expects `admission` to succeed in under 3 s, the durable turn to be
   forgotten and the rest to be kept.

After the fix, 10 of 10 isolated runs pass.

### Output before the fix

The new `admission` entry point was first added with round-2 semantics (a
sequential sweep and always `store_error`), and the new tests were run
against it:

```
FAIL in_flight_turns_count_toward_the_bound
  left: (-32018, "store_error")   right: (-32012, "admission_refused")
FAIL [2.136s] a_durable_terminal_behind_delayed_reads_is_settled_within_the_bound
  called `Result::unwrap()` on an `Err` value: ApiError { code: -32018, kind: "store_error", … }
Summary 11 tests run: 9 passed, 2 failed
```

### Files changed

- `crates/via-core/src/api.rs`: `TURNS_AT_CAPACITY`.
- `crates/via-core/src/engine.rs` (shared): one line in `spawn`, which now
  calls `journal::admission(..)?`.
- `crates/via-core/src/engine/journal.rs`, `crates/via-core/src/engine/journal/tests.rs`.
- **Outside my owned paths, called out:**
  `crates/via-store/src/runtime.rs` and `crates/via-store/src/runtime/sql.rs`
  add the `Terminated` command, `StoreClient::terminated` and
  `read_terminated`. The command only reads. No schema change and no new
  dependency.

### Gate

Run with `XDG_RUNTIME_DIR` set to a private 0700 directory:

| Check | Result |
|---|---|
| fmt | clean |
| clippy | clean |
| nextest | 108 passed, 1 skipped (the existing root-only `#[ignore]`) |
| deny | ok |
| check-layers | exit 0 |
| Markdown links | 0 broken in this report |
