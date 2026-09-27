# W4-H report: force race, truthful force evidence, bounded client joins

Branch `claude/w4-h-rust-foundation-q1s7w7`, based on `fbd7ee6`. Findings from
`../../w3/sol-review-W3-F.md` 1, 2 and 4, plus brief item 4 (force and
in-flight output). Each regression below was run on the unchanged code first
and failed for the stated reason; all pass after the fix.

## 1. Force right after a receipt (W3-F Sol 1)

**Failure.** `dispatch("spawn")` committed the receipt, then sent the drive to
daemon main over `drive_rx`. A force accepted in between broke the serving
loop at once, so the queued drive never entered `drives`; final shutdown had
no `ForcedTurn` for it, counted the turn unresolved and exited 4.

**Regression.** `s1_daemon_stop_force_right_after_receipt_cancels_queued_turn`
(`crates/via-cli/tests/s1_daemon_stop.rs`), end to end through the real
binary. A first turn completes (raw evidence); then six connections each
pipeline `hello`, `spawn` and `daemon/stop {"force":true}` in one write,
four attempts. Every receipted turn must end `cancelled`, `interrupted`, no
failure, cleanup `quiescent`, outcome `forced` if an anchor was committed for
it and `requested` otherwise, with the C1 force lifecycle, and the daemon
must exit 0 clean. Before the fix, a single-connection version hit the race:

```
fail: exit exit status: 4, summary {"anchors":1,"disposition":"incomplete",...,
"mode":"force",...,"unresolved_turns":1}
```

The six-connection version failed on every run (3/3), mostly for a second,
related reason: Core's force branch dropped drives that were **mid-acquire**,
leaving anchors whose cleanup stayed unproved:

```
fail: exit exit status: 4, summary {"anchors":5,"disposition":"incomplete",
"elapsed_ms":5014,...,"uncertain_owners":4,...}
```

**Fix.** `crates/via-cli/src/server.rs`: a spawn reserves its handoff slot
(`drives.reserve()`) *before* its receipt commits, then hands off with the
permit. Final shutdown closes the queue and drains it under the final deadline
before joining drives. Tokio's `recv` returns `None` only after every
outstanding permit is used or released, so every receipted turn is driven.
Before launch, a force stops the drive with `requested`. A reserve that fails
because the queue is closed is `daemon_stopping`, not `store`. The fix for
item 4 below removed the mid-acquire drop.

## 2. `forced` needs evidence of a live stop (W3-F Sol 2)

**Failure.** `ProcessControl::close` set `forced` from an empty 50 ms polled
exit watch plus `Reply::Stopping`, which the anchor sends even after its
vendor has exited. Host shutdown inferred `forced` for any control released
unclosed with an empty exit watch.

**Regressions.** `crates/via-host/tests/anchor_process.rs`, against a real
anchor and vendor: `/bin/cat` ends when its stdin is dropped. The test waits
until the vendor is gone or a zombie while Host's exit watch is still empty.
The two tests then take the close path and the release path:
`force_close_after_unobserved_vendor_exit_is_not_forced` and
`released_control_after_unobserved_vendor_exit_is_not_forced`. Both failed:

```
the vendor had already exited: CloseReport { cleanup: GroupAbsent(..),
vendor_exit: Some(ExitReport { code: Some(0), signal: None }), forced: true }
the vendor had already exited: ShutdownReport { recovery: [RecoveryReport {
.., cleanup: GroupAbsent(..), forced: true }], .. }
```

**Fix.** The anchor (`anchor.rs`) records `stopped_live` when own-group
cleanup first begins. `stopped_live` is whether `child.try_wait()` still
showed the vendor running *before* the TERM. The anchor now begins cleanup
before replying and reports this in `Reply::Stopping { stopped_live }`
(`protocol.rs`). Host (`host.rs`) sets `forced` only from that reply. A
control released unclosed has no reply channel, so it is no longer force
evidence. `StopFacts::closed` is gone. `force_evidence_separates_host_stop_from_absence`
now expects a released live vendor to be *not* forced. The daemon's force
path no longer relies on release (item 4); it closes through Route and gets
the anchor's reply.

## 3. Client joins obey the final deadline (W3-F Sol 4)

**Failure.** After `abort_all()`, final shutdown awaited every client join
with no bound. A task that did not reach an abort point held daemon main past
the single 10 s bound.

**Regression.** I first extracted the join into `join_clients` in `server.rs`
without changing behavior. Then I added the unit test
`unabortable_client_join_stops_at_the_final_deadline`: a client blocks its
worker for 2 s, `clients_by` is +50 ms and the final deadline +200 ms. It
failed:

```
client join passed the final deadline: 2.000350856s
```

**Fix.** The post-abort join is bounded by the final deadline. The pending
count is still the snapshot at `clients_by`; unfinished tasks are left to
process exit, and the Engine then reports `store: not_released`.

## 4. Force and in-flight output (brief item 4)

**Decision (C1 §7.6 force and raw-overflow rows, C2 Close(Force)).** A force
stop owes the raw log every byte the vendor wrote before its group died. If
bytes cannot be recorded, it must say so explicitly: `raw_log.incomplete`
event, `raw_log_incomplete` warning. Frames Wire already read still become
events in order. Bytes still in the pipes at the force become raw-only
evidence, not events, as on Route's existing failure path. Force is therefore
a cooperative Close(Force) through Adapter → Route → Wire → Host. It no longer
drops the execution.

**Failure.** Core's biased force branch dropped the adapter future. Bytes in
Wire's buffer (for example, an unterminated line) and bytes unread in the
pipes were lost. Nothing marked the log incomplete, so it was silently short.
The in-flight observations were dropped too.

**Regression.** `s1_daemon_stop_force_keeps_in_flight_output_in_raw_log`,
end to end. The fake vendor emits acceptance and text, then an unterminated
line, then holds. After the durable `assistant.text`, a force must leave
those bytes in `state/raw/<connection>.raw`, or record `raw_log.incomplete`
plus the warning. It must also end `cancelled`/`forced`/`quiescent` with the
C1 lifecycle. It failed:

```
fail: raw log recorded in-flight bytes: false, warned: false
```

After the fix, the bytes are in the raw log (no warning) and the turn is
`forced`/`quiescent`.

**Fix.**

- **Wire** (`via-wire/src/runtime.rs`): `open_connection` takes a cancel
  `watch::Receiver<bool>`. It is checked only where waiting loses no bytes:
  the pipe read (outside drain mode), the stdin write and the exit wait. A
  set signal returns the new `WireError::Cancelled`.
- **Route** (`via-routes`): `execute` takes the signal. A signal already set
  launches nothing. The new `RouteError::ForceStopped` takes the existing
  failure path, Close(Force) then `drain_to_eof`, with `raw_incomplete`,
  `cleanup` and `forced` from that path.
- **Adapter** (`via-adapters`): passes the signal through, and still forwards
  every Route message.
- **Core** (`engine.rs`): passes `force.subscribe()` and keeps committing
  observations until the adapter returns. `ForceStopped` becomes
  `Driven::Forced`, which carries `raw_incomplete`: Core commits
  `raw_log.incomplete` before `cancel.requested`, and the terminal warns.
  `requested_at` is when the force was accepted (`force_requested_at`).

## Files changed

- `crates/via-cli/src/server.rs`: owned (items 1, 3).
- `crates/via-host/src/{anchor,host,protocol}.rs`: owned (item 2).
- `crates/via-core/src/engine.rs`: owned Core stop/force (item 4).
- Shared hunks, named:
  - `crates/via-wire/src/runtime.rs`: cancel signal and `WireError::Cancelled`.
  - `crates/via-routes/src/{lib,runtime}.rs`: `RouteError::ForceStopped`, the
    `force` parameter and the pre-launch check.
  - `crates/via-adapters/src/runtime.rs`: `force` parameter pass-through.
  - `crates/via-core/tests/route_stream.rs`: supplies a never-set signal.
  - `docs/specs/runtime-contracts.md` §6: the force bullet now describes
    Close(Force) plus drain and the anchor's force evidence. The old "Core
    abandons the execution" text contradicted the code.
- Tests: `crates/via-cli/tests/s1_daemon_stop.rs` (two scenarios),
  `crates/via-host/tests/anchor_process.rs` (two tests plus the updated
  assertion), and the `server.rs` unit test.

## Gate

Toolchain 1.98.1, `XDG_RUNTIME_DIR` set to a private 0700 directory.

- `cargo fmt --all --check`: pass.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: pass.
- `cargo nextest run --locked --workspace`: 103 passed, 1 skipped. The
  baseline before the change was 98 passed, 1 skipped.
- `cargo deny check`: advisories, bans, licenses and sources ok.
- `python3 scripts/check-layers.py`: ok.
- The Markdown link check finds nothing broken in the files this branch
  changes. It reports 44 pre-existing `path:line` links in earlier W2/W3
  review files (for example `../../w3/sol-review-W3-F.md`), which this task
  does not own.
- The daemon-stop, route-drain, Host and Core suites were rerun 5 more times
  with no failure.

The `test-failpoints` gates do not apply yet: no crate defines that feature,
and cargo refuses `--features via-cli/test-failpoints`. I did not run the
release build.

## Open or uncertain

- **Raw-incomplete branch untested end to end.** Core's handling of a force
  whose drain *could not* record every byte is not exercised: the fake route
  has no raw-append failpoint. The in-flight regression accepts either
  outcome but hit the "recorded" branch.
- **Race coverage is probabilistic.** The item 1 regression depends on
  scheduling. On the old code it failed 3 of 3 runs, mostly through the
  mid-acquire variant; the pure queued-drive variant was seen once in about
  40 single-connection attempts.
- **Pre-existing gap: anchor intent without identity.** A drive aborted at
  the final deadline while acquiring can still leave an anchor intent with no
  committed identity. Host reports it `uncertain`. The force path no longer
  causes this.
- **Definition of "live".** `stopped_live` means the vendor's own process
  had not exited when cleanup began. A vendor that exited while its
  grandchildren lived is not reported forced. If cleanup began because of an
  external SIGTERM to the anchor, a later Host stop reports that first
  cleanup's evidence.
- **Events for bytes after the force.** Frames still in the pipes at the
  force are recorded raw-only, not as events, which matches the decision
  above. The existing force e2e test still waits for a durable
  `assistant.text` before forcing, because it asserts on that event.
