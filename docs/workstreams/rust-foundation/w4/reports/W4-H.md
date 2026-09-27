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
- **Race coverage is probabilistic (updated in round 2).** The item 1
  regression now requires a receipted turn and loops until the daemon's
  summary shows a queued handoff: about 1 attempt in 15, capped at 100
  attempts, so a miss is near 0.1%.
- **Host exit-window tests are probabilistic (deferred, W4-H Sol).** The two
  Host tests for item 2 need an unobserved-exit window within 20 tries. On a
  slow machine they can fail without a product defect. A controlled
  status-poll seam would make them deterministic.
- **Pre-existing gap: anchor intent without identity.** An anchor intent
  with no committed identity is reported `uncertain`. Since round 2, a force
  during an acquisition that stalls past the 2 s grace abandons it this way,
  and so does a drive aborted at the final deadline.
- **What `stopped_live` shows (W4-H Sol, deferred).** `stopped_live`, and so
  `forced`, is `child.try_wait()` on the direct vendor child when cleanup
  begins. It is not evidence about the whole group. It does not prove that
  the cleanup stopped any group member, and a vendor that exited while its
  descendants lived is not reported forced. If cleanup began because of an
  external SIGTERM to the anchor, a later Host stop reports that first
  cleanup's evidence.
- **Events for bytes after the force.** Frames still in the pipes at the
  force are recorded raw-only, not as events, which matches the decision
  above. The existing force e2e test still waits for a durable
  `assistant.text` before forcing, because it asserts on that event.

## Round 2

Addresses `../sol-review-W4-H.md` "Blocks merging" 1–4. I merged
`origin/rust-foundation` first (docs only). Each regression failed on the
round-1 code before the fix.

### Sol 1: a force during a stalled acquisition left the turn unresolved

**Failure.** Wire awaited `Host::acquire` under the 30 s turn deadline and did
not watch for force. Final shutdown aborted the drive at 10 s, so there was
no `ForcedTurn`.

**Regression.** `force_during_stalled_acquisition_settles_the_turn` in
`crates/via-core/tests/force_stop.rs`. It uses the Engine over a real Store
and Host. The anchor is a stand-in that accepts Host's connection and never
sends `Ready`. The test forces 500 ms into the drive; the drive must end
within 4 s, and the turn must end `cancelled`/`requested`/`uncertain` with no
unresolved turn. Before the fix:

```
a force must end a stalled acquisition's drive: Elapsed(())
```

The drive was still stuck 8 s after the force.

**Fix.** In `WireConnection::open`, once cancelled, the acquisition gets
`CANCELLED_ACQUIRE_GRACE` (2 s):

- A normal acquisition finishes, and its group is force-closed and proved
  absent as before.
- A stalled one is dropped with `WireError::Cancelled`, which Route maps to
  `ForceStopped`. Dropping it closes the anchor control: an anchor that had
  connected exits on EOF, and one that had not exits at its own 5 s bootstrap
  deadline.

Host recovery then proves absence when the identity was committed and
otherwise reports `uncertain`. Final shutdown's exit status stays truthful:
exit 4 when cleanup is uncertain.

### Sol 2: a force during a blocked observation send lost bytes silently

**Failure.** Route's `forward` and the Adapter's `deliver` waited for
observation capacity until the turn deadline, and neither watched for force.
Route never reached its close-and-drain path before the drive was aborted.

**Regression.** `force_while_forwarding_is_blocked_drains_every_byte` in
`crates/via-core/tests/route_stream.rs`. The consumer never drains; the
vendor writes 300 lines plus an unterminated tail, then sleeps. The test
forces after 1 s. The route must end `ForceStopped` within 5 s,
`raw_incomplete` must be false, and the raw log must contain `line 299` and
the tail. Before the fix:

```
force took 19.267352511s
```

**Fix.** `forward` (Route) and `deliver` (Adapter) wait for capacity first,
so a draining consumer still gets frames already read. They end on force
otherwise. A closed channel while forced is `ForceStopped`, not `Overflow`.
The unsent message's bytes were already durable in the raw log, and Route's
drain records everything else or marks the raw log incomplete.

### Sol 3: the client-join summary miscounted pending joins

**Failure.** `join_clients` returned the count taken *before* the abort, so
clients that joined after the abort still forced exit 4.

**Regression.** Unit test `aborted_client_that_joins_is_not_pending`, with
an abortable 5 s sleep. Before the fix:

```
left: (1, 0)
right: (0, 0)
```

**Fix.** The abort-then-join loop now collects post-abort results, and
`pending` counts only tasks still unjoined at the final deadline. Cancellation
by the abort is not a failure. As before, a client's own I/O error is not a
failed join. The blocked-client test still reports `(1, 0)` within the
deadline.

### Sol 4: the queued-drive regression did not establish the race

**Changes.**

- The daemon summary gains `queued_drives`: receipted turns that final
  shutdown took from the handoff queue.
- The scenario now keeps spawn traffic continuous: four connections each
  pipeline eight spawns, and a separate connection forces after the first
  receipt.
- It requires at least one receipt.
- It asserts each turn's outcome from Host's vendor facts (`vendor_pid`),
  not from the anchor intent: `forced` if a vendor launched, `requested` if
  there is no anchor, and failure for an anchor without vendor facts.
- It repeats until `queued_drives > 0` has been seen (at least 4 attempts,
  at most 100).

**Check.** I temporarily made the drain skip driving the turns it takes from
the queue, as before round 1. The test then failed for the intended reason:

```
fail: exit exit status: 4, summary {..,"queued_drives":1,..,"unresolved_turns":1}
```

### Files changed in round 2

- `crates/via-cli/src/server.rs`: `join_clients` and the `queued_drives`
  summary field.
- `crates/via-wire/src/runtime.rs` (shared): the cancelled-acquisition
  grace.
- `crates/via-routes/src/runtime.rs` (shared): `forward` ends on force.
- `crates/via-adapters/src/runtime.rs` (shared): `deliver` ends on force.
- Tests: `crates/via-core/tests/{force_stop,route_stream}.rs` and
  `crates/via-cli/tests/s1_daemon_stop.rs`.

### Gate

- `cargo fmt --all --check`: pass.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: pass.
- `cargo nextest run --locked --workspace`: 106 passed, 1 skipped.
- `cargo deny check`: ok.
- `python3 scripts/check-layers.py`: ok.
- The daemon-stop suite was rerun 3 more times with no failure.

## Round 3

Addresses the merge blocker in `../sol-review-W4-H-r2.md`. I merged
`origin/rust-foundation` first (docs only).

### Blocker: an abandoned post-ARM acquisition lost vendor output silently

**Failure.** After Host sends ARM, the vendor runs and can write to its
pipes while Host still awaits the spawn reply or vendor facts. If a force
expires the 2 s grace, Wire drops the acquisition and, with it, the pipes
Host still owned. Route reported `raw_incomplete: false`, so Core emitted
neither `raw_log.incomplete` nor its warning.

**Fix (smallest truthful one).** At that point Wire does not own the pipes
yet, so the fix reports possible loss instead of draining. `Host` gains
`acquire_marking_arm`, which sets a flag just before ARM is sent; `acquire`
delegates to it. Wire abandons an acquisition with the flag set as
`WireError::CancelledAfterLaunch`. Route maps it to `ForceStopped` with
`raw_incomplete: true`. Core already turns that into `raw_log.incomplete`
plus the `raw_log_incomplete` warning. An acquisition abandoned before ARM
launched no vendor, so it stays `raw_incomplete: false`.

**Regression.** `force_after_arm_abandonment_reports_raw_log_incomplete` in
`crates/via-core/tests/force_stop.rs`, over the Engine with a real Store and
Host. It uses a stand-in anchor that:

1. passes Host's identity checks;
2. answers Configure;
3. on ARM, launches a vendor that writes a line to the inherited stdout
   pipe;
4. never sends the spawn reply.

The test forces after 1 s, then requires a cancelled turn with the
`raw_log_incomplete` warning and a `raw_log.incomplete` event. To show it
fails without the fix, I set Route's flag back to `false`, as a mutation of
the fixed tree. The test then failed:

```
lost vendor output must be reported: {..,"cancel":{"cleanup":"quiescent","outcome":"requested",..}
```

### Deferrable test note (addressed)

`force_while_forwarding_is_blocked_drains_every_byte` now forces only after
observing backpressure. It polls the turn until the observation channel
reports zero capacity, waits 200 ms more, and asserts the channel is still
full.

### Files changed in round 3

- `crates/via-host/src/host.rs`: `acquire_marking_arm`.
- `crates/via-wire/src/runtime.rs` (shared): `CancelledAfterLaunch`.
- `crates/via-routes/src/runtime.rs` (shared): maps it to `ForceStopped`
  with `raw_incomplete`.
- Tests: `crates/via-core/tests/{force_stop,route_stream}.rs`.

### Gate

- `cargo fmt --all --check`: pass.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: pass.
- `cargo nextest run --locked --workspace`: 107 passed, 1 skipped.
- `cargo deny check`: ok.
- `python3 scripts/check-layers.py`: ok.
- Core and Host suites were rerun twice more with no failure.

### Open

- **Unrecorded vendor bytes are not drained.** An abandoned post-ARM
  acquisition reports possible loss; it does not recover the bytes, because
  Wire never owned those pipes.
- **Other post-ARM failures.** A non-force acquisition failure after ARM,
  such as a failed vendor-facts commit, reports raw completeness as before.
  That is outside this finding.
