# W3-G report: bounded raw waits and uncertain Store commits

Branch `claude/w3-g-task-57zmxa`. The findings are 1 and 3 from
[`../../w2/sol-reviews/W2-E.md`](../../w2/sol-reviews/W2-E.md).

## 1. Raw appends obey the cleanup deadline (W2-E finding 1)

**Failure mode.** `WireConnection::record` awaited `RawWriter::append`, and
`append` waited on the Store raw worker's oneshot reply with no bound. During
the failure drain, `drain_to_eof` records the retained stdout buffer and later
pipe chunks through `record`. If the raw worker stalled, `drain_to_eof` never
returned, and Route's 3 s `cleanup_deadline()` had no effect. Every other
`record` caller (`write_frame`, `next_frame`) had the same unbounded wait.

**Regression.** `crates/via-core/tests/route_stream.rs`
`stalled_raw_worker_cannot_hold_failure_cleanup_past_its_deadline` runs through
the real Route, Wire, Host and Store. It uses a scripted vendor:

1. The vendor emits acceptance.
2. The test stalls Store's raw worker.
3. The vendor emits a line of about 1.05 MiB.
4. `next_frame` fails with `FrameTooLarge` and records nothing, so the first
   blocked append happens in the failure drain.

The test asserts a `Protocol` cause, `raw_incomplete: true` and that the turn
ends within 8 s of the stall (cleanup bound 3 s; turn deadline 20 s).
`run_child` now kills a child that runs longer than 60 s and fails, so a hang
cannot stall the suite.

Output before the fix:

```
panicked at crates/via-core/tests/route_stream.rs:80:13:
stalled_raw_worker_cannot_hold_failure_cleanup_past_its_deadline child did not finish within 60s
Summary [  60.129s] 1 test run: 0 passed, 1 failed
```

After the fix, it passes in about 3.4 s.

**Stall hook.** Store now has a closed fault hook, `Store::stall_raw_worker() ->
RawStall`. It holds the raw worker until the guard drops. The hook and its
`RawCommand::Stall` arm are compiled only under a new, default-off via-store
feature, `test-failpoints` (the name from runtime-contracts). No environment
input or runtime switch can reach it. The only thing that enables the feature
is via-core's dev-dependency on via-store, so a release `-p via-cli` build
does not include it.

**Fix.**
- `record` now takes the caller's absolute deadline and waits on the append
  with `timeout_at`. On expiry it returns the new `WireError::RawDeadline` and
  latches `RawEvidence::Incomplete`. A unit that was not confirmed counts as
  lost.
- `drain_to_eof` treats `RawDeadline` like a raw failure and keeps draining,
  discarding what it reads. The loop now also stops at the same deadline even
  when a pipe is always ready, because tokio's `timeout_at` polls the inner
  read before it checks the deadline.
- Route's `wire_cause` maps `RawDeadline` to `RouteError::Store`. The stalled
  component is Store, and C1 class `store` describes that better than
  `deadline_wall`.

**Files.** `crates/via-wire/src/runtime.rs`, `crates/via-routes/src/runtime.rs`,
`crates/via-store/{Cargo.toml,src/lib.rs,src/runtime.rs,src/runtime/raw.rs}`,
`crates/via-core/{Cargo.toml,tests/route_stream.rs}`.

## 2. Uncertain observation commits (W2-E finding 3)

**Failure mode.** `Engine::commit_event` treated every `commit_event` error as
"not committed" and kept `record.seq`. Store returns `StoreError::Uncertain`
when SQLite fails inside `COMMIT`. It returns `StoreError::Unavailable` when
the reply is dropped, which can happen after the worker has committed. In
either case the event may already be durable. The turn then fails `store`, and
`finish` sends `turn.ended` at a sequence that is already taken. Store rejects
it ("event sequence is not the next one"), `drive` returns `Err`, and the
receipted turn stays `running`. `wait` then spins to `wait_timeout` and
`result` says `turn_not_finished`, where C1 requires `store_error`.

**Regression.** The tests are at the Store/Core boundary, in
`crates/via-core/src/engine/journal/tests.rs`. A closed `#[cfg(test)]` fault
backend, `FaultJournal`, wraps a real Store. It has two fixed event faults,
`CommittedThenUncertain` (commits, then reports `Uncertain`) and
`UncertainNotCommitted`, plus an optional unreadable event head. Each test
commits one observation through Core's real `commit_event` and then runs
Core's real `Engine::finish_turn`.

- `committed_uncertain_observation_is_settled_before_turn_ended` expects:
  - a durable `failed`/`store` terminal;
  - `last_seq` 4 and events 1–4 dense;
  - the observation's raw span inside `raw_spans`.
- `uncommitted_uncertain_observation_keeps_the_sequence` is a control: with
  nothing durable, `turn.ended` takes seq 3.
- `unsettled_turn_reads_as_store_error_not_running` makes the head unreadable
  and expects:
  - `finish` returns `store_error`;
  - no terminal is invented;
  - the `result`/`wait` read path returns `store_error`.

Output before the fix (settlement and the unresolved read turned off, which
matches the old behaviour):

```
FAIL engine::journal::tests::committed_uncertain_observation_is_settled_before_turn_ended
  panicked at crates/via-core/src/engine/journal/tests.rs:212:10   (finish -> Err store_error: terminal insert refused, turn left running)
FAIL engine::journal::tests::unsettled_turn_reads_as_store_error_not_running
  panicked at crates/via-core/src/engine/journal/tests.rs:277:21   (read returned Ok(None): looks still running)
Summary 3 tests run: 1 passed, 2 failed
```

**Fix.** A new child module, `crates/via-core/src/engine/journal.rs`, holds:

- `TurnJournal`, Core's narrow Store port for a turn. `StoreClient` implements
  it in production; the tests use the fault backend. There is no dynamic
  plugin and no runtime switch.
- `commit_event`, moved unchanged from `Engine`. When a failure may have
  committed (`Uncertain` or `Unavailable`), it keeps that event as
  `TurnRecord::uncertain`. Core stops committing after the first Store
  failure, so a turn holds at most one such event.
- `reconcile`, which runs before `turn.ended` is sequenced. It reads the
  durable head at the uncertain sequence:
  - If the event is there with the same raw ref, it advances `seq`, the raw
    spans and any acceptance, exactly as a confirmed commit would.
  - If nothing is there, it leaves the record unchanged.
  - If a different event is there, or the read fails, the turn is unresolved.
- `commit_terminal`, which settles an uncertain terminal commit by reading the
  result back.
- `Unresolved`, a set of receipted turns whose terminal did not commit, with
  `read_result`. A durable terminal is still returned as is. An unresolved
  turn with no terminal reads as `store_error`.

An uncertain acceptance is now handled the same way. `accept` returns the
unconfirmed `Accepted` value and `observe` keeps it, so settlement can restore
`accepted_at` and `vendor.turn_id`.

**Shared hunks in `crates/via-core/src/engine.rs`** (W3-F also edits this
file):
- `mod journal;` and its imports.
- The `Engine.unresolved` field and its initialiser.
- `TurnRecord.uncertain` and its initialiser in `drive`.
- `finish` is split into `finish` → `finish_turn` → `commit_turn_ended`. The
  body is unchanged apart from `reconcile` at the top and
  `journal::commit_terminal` at the end.
- The `accept` error type and the `observe` acceptance arm.
- `commit_event` now delegates to the journal.
- `result`/`wait` read through `journal::read_result`.

`shutdown` is not touched. Force-stopped turns go through `finish` and get the
same settlement.

## Gate

The gate ran from the repo root with `XDG_RUNTIME_DIR` set to a private
0700 directory:

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | clean |
| `cargo nextest run --locked --workspace` | 90 passed, 1 skipped (the existing root-only `#[ignore]` in `c1_protocol.rs`) |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | exit 0 |
| Markdown link check | 0 in this report; 30 pre-existing `path:line` links in `w2/sol-reviews/*.md` (not mine) |

`Cargo.lock` is unchanged, and no dependencies were added.

## Open or uncertain

- **C1 `store_error` data is missing.** C1 §3.8/§9 requires `data.session`,
  `data.turn`, `data.durable_state` and `data.terminal_persisted:false`. Core
  now returns the right kind, but `ApiError` is a `Copy` value with static
  strings, and the JSON-RPC error body is built in `via-cli/src/server.rs`,
  which is outside my paths and which W3-F may be editing. Carrying the data
  means adding a payload to `ApiError` and rendering it in `server.rs`.
- **The unresolved set is in memory only.** After a daemon restart, a turn
  whose terminal never committed is still `running` in SQLite. Settling it at
  restart belongs to recovery, which S1 does not implement yet.
- **The `test-failpoints` feature is only partly wired.** via-store defines it
  and via-core's dev-dependency enables it. The via-cli forwarding chain and
  the failpoint controller are still deferred (`via-jm4.7.6`/`.7.7`). The
  future `check-release-features.py` should confirm that the via-store feature
  is absent from the release graph. I expect it to be, since only a
  dev-dependency enables it.
- **An uncertain `commit_submission` in `submit` is not settled here.** It is
  not the observation or terminal path, and the turn has no events yet.
- **The raw-deadline cause is Store.** A raw append that expires at the turn
  deadline during normal reading now reports `store`, not `deadline_wall`.
  This is deliberate, because the stalled component is Store; it is flagged
  here for the orchestrator.
