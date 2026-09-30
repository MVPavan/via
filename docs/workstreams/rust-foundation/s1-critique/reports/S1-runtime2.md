# S1-runtime2 report: acceptance-write corruption latches; a late terminal delivers its final text

**Status: DONE_WITH_CONCERNS.** S1 critic round 2 findings 1 and 2 are
fixed. Each has a regression that failed before its fix (RED) and passes
after it (GREEN). Gate G is green with 5 selector repeats, and the
failpoint suite passed 3 times on the tip. The concerns are minor (see
Deviations and concerns): I did not add a daemon-level test for
finding 2, and one of the two finding 2 tests only characterizes the new
failure mapping.

- Bead: `via-jm4.7.9.5`.
- Branch: `wt/s1-runtime2`, cut from `rust-foundation` at `c226b1f`.
- Review source: `docs/workstreams/rust-foundation/s1-critique/reviews/S1-critic-r2.md`, findings 1 and 2.
- Logs: `scratchpad/s1/runtime2/` (main checkout).

## Commits

| Commit | Finding | Summary |
|---|---|---|
| `87ec408` | 1 | The acceptance commit carries `WriteOutcome::of(&error)`, so corruption latches. Adds the `store.commit.corrupt.acceptance` seam and its regression. |
| `6191814` | 2 | The late path flushes the held message to the hop under its cleanup allowance before returning. Adds two regressions. |

## Finding 1: an acceptance write that SQLite reports corrupt latches

**Defect, confirmed.**
- `Engine::accept` (`crates/via-core/src/engine/drive.rs`) reduced the
  `commit_acceptance` error to
  `journal::may_have_committed(&error).then_some(..)`, and
  `may_have_committed` matches only `Uncertain | WriterLost`.
- So `StoreError::Corrupt` became `Err(None)`, and `observe` then passed
  `WriteOutcome::NotCommitted` to `event_failed`. The result was a scoped
  turn failure, with the daemon still `healthy` and still admitting and
  dispatching.

**Change.**
- `accept` now returns `Err((WriteOutcome::of(&error), evidence))`. The
  evidence (the acceptance and the event sent) is kept when
  `outcome.head_unknown()`, the rule `journal::commit_event` already
  uses.
- In `observe`, a head-unknown outcome loses the head and records the
  uncertain event if there is evidence; otherwise the head is dropped.
  `observe` passes the outcome itself to `event_failed`, so `Corrupt`
  latches as it does at every other write site.
- The encode failure and the `core.accept.before_commit` failpoint are
  still `NotCommitted`.
- New test-only seam `store.commit.corrupt.acceptance`, in
  `commit_acceptance` (`crates/via-store/src/runtime/sql.rs`). It fires
  after the turn read, the update and the event insert, just before the
  existing `store.commit.event` seam. Its `fail_io` returns
  `StoreError::Corrupt`, and the transaction rolls back. The seam is added
  to `scripts/check-release-features.py`'s point list.

**Other `may_have_committed` call sites.** None of them loses `Corrupt`:
- `latch.rs` `WriteOutcome::of` is the classifier itself and checks
  `Corrupt` first.
- `journal.rs` `commit_terminal_with` checks it twice. The retry
  condition excludes `Corrupt` explicitly, and the
  reconciliation arm includes it explicitly.

`drive.rs` `accept` was the only site that lost `Corrupt`.

**Regression:** `s1_f12_corrupt_acceptance_write_latches`
(`crates/via-cli/tests/s1_store_failure.rs`).
1. Turn 1 of session "first" pauses at `core.accept.before_commit`.
2. Session "second" is spawned and its dispatch pauses at
   `core.dispatch.before_grant` (hit 2).
3. The acceptance write meets `store.commit.corrupt.acceptance`.
4. The test then asserts:
   - `daemon/status` `health == store_failed`, and `store_failure` is
     `corrupt_store` with scope `daemon`;
   - a new spawn is refused with `store_error`;
   - once the grant is released, the daemon exits 4 with
     `store_failed: true` (`latched_exit`), and the second session's turn
     has no anchor: nothing more was dispatched.

- RED (`scratchpad/s1/runtime2/f1-red.log`): `Error: "timed out waiting until the latch"`. The daemon stayed healthy.
- GREEN (`f1-green.log`): 1 passed. Then `package(via-core) | test(/^s1_f12_/)` gave 164 passed (`f1-core-f12.log`).

## Finding 2: a decoded terminal is delivered before a late result returns

**Defect, confirmed.** The regression below reproduces the critic's
finding at the Adapter level. After wall expiry the result was
`Completed`, but Core never received the terminal's final text:
`left: "" right: "done"`.

**Change** (`crates/via-routes/src/runtime.rs`, `run_turn`'s
`Finished::Late` arm):
- The arm takes the cleanup allowance it already used
  (`cleanup_deadline()`), sets it as `serving.deadline`, and calls
  `serving.flush()`, the existing bounded hop delivery.
- Every control that delivery already serves still applies: force, the
  latch, a closed hop, the allowance.
- On success the force close and the late `Ok(terminal.result(..))`
  follow as before, so design §2 rule 3 [r1.23] holds.
- On failure the delivery's own failure goes to the existing failure
  path. For a timeout that is `Deadline`; for a closed hop, `Overflow` or
  `ForceStopped`. Its `close_by` is set to the same allowance, so the
  force close and drain stay under one bound. The comment on
  `Failed::close_by` says so.

**Regressions** (`crates/via-core/tests/route_stop.rs`):
- **Why this crate.** Route cannot open a Store, and the layer check
  forbids `via-routes` or `via-core` dev-edges that would allow the
  critic's bare `FakeRoute` shape.
- **The setup (`held_terminal`).** The shape is kept one layer up:
  1. Nothing is drained until the 3 s wall deadline.
  2. The acceptance and 1,023 texts fill Core's 1,024-item channel.
  3. The Adapter's delivery holds the 1,024th text, the hop of one holds
     the 1,025th, and Route holds the terminal.
- **`a_held_terminal_is_delivered_after_wall_expiry`.** The test drains
  from the deadline on and asserts `Completed` with final text `"done"`.
  - RED (`f2-red.log`, `f2-red2.log`): `left: "" right: "done"`.
  - GREEN (`f2-green.log`): passes.
- **`an_undelivered_held_terminal_is_not_a_completion`.** The test never
  drains, and the result must be a Route failure with cause `Deadline`
  (the allowance) and no final text.
  - RED (`f2-red2.log`): the cause was `Overflow`, because the Adapter
    already turned the undelivered rest into a failure after its 10 s
    stall.
  - GREEN: `Deadline` within the 3 s allowance.
  - This test characterizes the new failure mapping. It does not detect
    the original loss (see concerns).

The existing `a_decoded_terminal_survives_wall_expiry_in_finalization` [r1.23] still passes.

## Updated tests

None. No existing test was changed or weakened.

## Gate counts (tip `6191814`)

Gate G (`scratchpad/t4/gate.sh … 5`; `scratchpad/s1/runtime2/gate.log`): `gate exit 0`.

| Check | Result |
|---|---|
| `cargo fmt --all --check` | ok |
| clippy, default features | ok |
| `cargo nextest run --locked --workspace` | 345 passed, 1 skipped |
| `cargo deny check` | ok |
| `check-layers.py` | ok |
| clippy, `via-cli/test-failpoints` | ok |
| failpoint suite (run 1) | 552 passed, 1 skipped |
| `s1_f(08\|09\|10\|12)_` | 59 passed |
| release build and `check-release-features.py` | ok |
| selector, 5 repeats | 88 passed each time |

The failpoint suite ran 2 more times (`failpoints-2.log`, `failpoints-3.log`): 552 passed and 1 skipped each time. After the runs, `pgrep -a -f via` showed no process from this worktree.

## Deviations and concerns

- **No daemon-level test for finding 2.** A deterministic daemon
  version needs the same channel-filling fixture (1,026 scripted texts
  with Core held at `core.observations.pause`). It also needs a signal
  that Route took the late path before Core resumes, and no existing seam
  gives one. Without that signal the test could pass without reaching
  the late path. The Adapter-level test covers the path Core uses: Core
  builds final text only from the `FinalText` observations the Adapter
  delivers.
- **`an_undelivered_held_terminal_is_not_a_completion` is a
  characterization.** Before the fix the Adapter already reported
  `Overflow` in that shape, after its 10 s stall. The test pins the new
  mapping, `Deadline` within the allowance, not the original defect.
- **`store.commit.corrupt.acceptance` is a new test-only seam** and is
  listed in `check-release-features.py`. I found no existing Store seam
  that reports corruption on a write.
- **Evidence for an uncertain `Corrupt` acceptance.** For a `Corrupt`
  acceptance the sent event is now recorded as an uncertain event,
  reconciled later. Before, it was treated as not committed. This matches
  `journal::commit_event` for `Corrupt`, and the daemon latches in either
  case.

## Fix round 1

**Status: DONE_WITH_CONCERNS.**
- Review source: the coordinator's Sol r1 review, with two findings, plus
  the added class sweep.
- Commit: `2122c52` (fix and tests). This report section is committed
  separately.
- Logs: `scratchpad/s1/runtime2/r1-*.log`.

### Item 1 (important): the late-path delivery is delivery-only

**Defect, confirmed.** In round 0 the late path delivered the held message
with `serving.flush()`, which runs through `serve_once`. That select is
biased and checks the daemon force and the connection latch before
`hop.reserve()`. A force or latch raised during the late flush therefore
dropped the decoded terminal, even when the hop had room.

**Change** (`crates/via-routes/src/runtime.rs`):
- New `Serving::deliver_held(by)`. It takes the held message and runs a
  `biased` select with only two arms:
  - `hop.reserve()`, which sends with `permit.send(message)` exactly as the
    reserve arm of `serve_once` does, or on a closed hop returns the
    existing `hop_closed()` failure;
  - `sleep_until(by)`, which returns `Deadline`.
- It has no force, latch, wake or pending-write arm. With nothing held it
  returns at once.
- The `Finished::Late` arm calls it under `cleanup_deadline()`. The normal
  path is unchanged.
- **Round-0 correction, found by the new tests.** Round 0 set a failed late
  delivery's `close_by` to the allowance that had just elapsed. The force
  close then had no time left, and the group ended with
  `cleanup: Some(Uncertain)`.
  - A failed delivery now takes the existing failure path's own fresh
    cleanup bound, as every other failure after the turn deadline does.
  - Consequence: teardown after a failed late delivery is bounded by 3 s
    of delivery plus 3 s of cleanup. This touches round-2 finding 4 (one
    absolute teardown deadline), which is not this chunk's; flagged for
    the coordinator.
  - The comment on `Failed::close_by` is restored.

**Regression:** `a_force_during_late_delivery_keeps_the_held_terminal`
(`crates/via-core/tests/route_stop.rs`).
- Same held-terminal setup as before: nothing is drained until the wall
  deadline.
- Timing (documented in the test comment):
  - Route enters the late path at the wall deadline. Its own timer never
    fires early.
  - The test raises the daemon force 500 ms after the deadline.
  - Draining starts 500 ms after the force, inside the 3 s allowance.
- Assertion: Core receives the final text `"done"`; if the Adapter reports
  a result, it must be `Completed`.
- The asserted result is not `Completed` because the Adapter itself ends
  a forced turn without its post-Route drain. `Adapter::execute`'s `rest`
  is biased on the force, and a Route success whose rest was not drained
  becomes `Overflow`. A temporary debug run confirmed this:
  `Err(Route(RouteFailure { cause: Overflow, .. }))`.
  - So "`Completed` with `done`" cannot be observed at the Adapter or Core
    level under a force. Only the delivered text can.
  - Changing the Adapter's force rule is outside this chunk. I flag it
    for the coordinator.
- RED, against round-0 routes code (`r1-red.log`): `left: "" right: "done"`.
- GREEN (`r1-green.log`): passes, 3 of 3 runs.

**Latch regression: not added.** Raising the connection latch needs a Wire
connection failure (a Host journal or evidence failure, or a transport
loss) during the late delivery. No seam sets it for a live connection at
a chosen moment without also ending the stdout read or the process. A
deterministic setup is not cheap. The latch arm is removed by the same
code change the force test covers.

### Item 2 (minor): the undelivered test proves that the late delivery ran

`an_undelivered_held_terminal_is_not_a_completion` now proves the late
delivery was taken.

**Why not the suggested timing assertion.** The coordinator suggested
asserting a resolution time of at least deadline + 3 s. That does not
discriminate here. With nothing drained, the Adapter's post-Route drain
waits for its 10 s event stall on either path, so the Adapter returns at
about 10 s whether or not Route took the late path.

**What the test uses instead:**
1. The vendor script writes its pid to `vendor.pid` after printing the
   terminal.
2. `LATE_PROBE`, 2.5 s after the wall deadline, checks that the vendor is
   still alive (`/proc/<pid>/stat`, not a zombie).
3. After the wall deadline, Route force-closes the group at once on every
   other exit. A turn that never decoded its terminal fails at the
   deadline itself. Only the late delivery keeps the group open, for up
   to its 3 s allowance.
4. The constant is named and commented: a 500 ms margin inside the
   allowance.

The test also asserts `cleanup == Quiescent`, which checks the round-0
correction above.

- RED against round-0 routes code (`r1-red.log`): `left: Some(Uncertain) right: Some(Quiescent)`.
- RED against the pre-chunk routes code at `c226b1f`, which has no late
  delivery (`r1-red-probe.log`): `the vendor was not alive 2.5s after the
  wall deadline: no late delivery`, `left: Some(false)`.
- GREEN: passes.

### Class sweep (coordinator's added scope)

**(a) Route exit paths in `crates/via-routes/src/runtime.rs`: can a
decoded message be dropped, or `Completed` be built without its final
text on the hop?**

| Site | Path | Verdict |
|---|---|---|
| `:150` `Finished::Result` | normal | ok: `finalize` delivers everything before returning. `next()` (`:585`) flushes before each read, and `serving.flush().await?` (`:408`) runs after EOF. |
| `:154` `Finished::Late` → `deliver_held` (`:525`) | late | fixed (rounds 0 and 1): the held message is delivered, delivery-only; otherwise the turn fails. |
| `:371` `Deadline` in `finalize` → `Late` | late entry | ok: the only `Deadline` after the terminal. Any other `finalize` error is a failure, never `Completed`. |
| `:186` `Err(failed)` | failed or stopped (stop order, force, latch, overflow, protocol) | ok for this class: a held message may be dropped, but the result is a failure. After the terminal, a stop order no longer acts (`on_wake`), and force or latch are failures by design §2. |
| `:330` EOF or unterminated before the terminal | EOF | ok: `next()` flushed first, so nothing is held. The result is `ProcessExited` or transport, never `Completed`. |
| late path, messages Wire read but Route did not decode | late | not in class: observations after the terminal that Route has not decoded are discarded by `messages.finish`. The final text travels only in the terminal message, which is always decoded and held first. |

**(b) Store writes in `crates/via-core/src/engine/`: does any classified
error reduce `Corrupt` or `ReadCorrupt` to a turn-scoped `NotCommitted`?**

| Site | Write or read | Verdict |
|---|---|---|
| `drive.rs` `accept` | acceptance commit | fixed in round 0 (`WriteOutcome::of`). |
| `journal.rs` `commit_event`; `drive.rs` `commit_step`, `commit_submission` commit arm | event, step, submission | ok: `WriteOutcome::of`. |
| `drive.rs:1446`, `:1207`, `:1215`; `journal.rs:397`; `close.rs:381`; `batch.rs:155`; `resolve.rs:145` | head or reconcile reads before a write | ok: `WriteOutcome::of_read` gives `ReadCorrupt`, which latches. |
| `drive.rs:1670`, `:1701` (`commit_submission` reads → `Unread`) | queued-row and head reads | ok: every worker read's `Corrupt` reaches Store's read-corruption observer (`engine.rs:318` → `Signal::read_corrupt`), which latches before the caller has its reply. `Unread` rolls back with no scoped report, and `read_expired` / `cancel_expired` stop at `grant()` (`resolve.rs:229`, `:270`) once latched. |
| `receipt.rs` `receipt_failed`, `:316`, `:525`; `close.rs` `closing_failed`, `commit_closed`; `stop.rs:622`; `resolve.rs:463` `commit_submit_failed` | receipt, closing, closed, session closed, submit failed | ok: `WriteOutcome::of`. |
| `journal.rs:546` `commit_terminal_with` → `Durable.uncertain` or `RECEIPT_UNKNOWN` → `outcome_of` | every terminal, including queued cancel, closure and resolution | latches, but as `Uncertain`, not `Corrupt`: `store_failure.kind` is `commit_uncertain`, not `corrupt_store`. This is not a reduction to a turn-scoped `NotCommitted`, so it is outside this class, and I left it unchanged. Flagged for the coordinator. |
| Host journal writes (Route `StoreFailure`); `reprobe.rs:139` absence proofs | journal | ok: via-store `journal_outcome` maps `Corrupt` to `CommitOutcome::Uncertain`, then to `StoreFailure::Uncertain`, which latches. |
| `close.rs:393`, `batch.rs:160`, `drive.rs:1713`, `:1768`, `:1776`, `receipt.rs:61`, `:78`, `:153`, `drive.rs:1545`, `recovery.rs:281`, `:632`, `:644`–`645` | encode, file steps, the test failpoint, blob or prompt-file copies, corrupt-row evidence (`CorruptEvidence`), recovery history | ok: none of these carries a SQLite `Corrupt` from a write. |

### Updated tests

- `a_held_terminal_is_delivered_after_wall_expiry`: moved onto the shared
  `held_terminal(child, drain, force)` / `HeldRun` helper. Its assertions
  are unchanged.
- `an_undelivered_held_terminal_is_not_a_completion`: strengthened, not
  weakened. It adds the late-delivery proof and `cleanup == Quiescent`.

### Gate counts (tip `2122c52`; `r1-gate.log`, `gate exit 0`)

| Check | Result |
|---|---|
| fmt, both clippy runs, deny, check-layers, release build and release-feature check | ok |
| default nextest | 346 passed, 1 skipped |
| failpoint suite (run 1) | 553 passed, 1 skipped |
| `s1_f(08\|09\|10\|12)_` | 59 passed |
| selector, 5 repeats | 88 passed each time |
| failpoint suite, runs 2 and 3 (`r1-failpoints-2.log`, `r1-failpoints-3.log`) | 553 passed, 1 skipped each time |

After the runs, no process from this worktree remained.

### Concerns

- **The Adapter's force rule:** a Route success under force becomes
  `Overflow`, and the post-Route drain is skipped. So a forced late
  terminal delivers its text to Core, but the turn is not reported
  `Completed`. This is outside this chunk.
- **Teardown bound:** after a failed late delivery, teardown can take up
  to 3 s of delivery plus 3 s of cleanup. This relates to round-2
  finding 4.
- **Failure kind:** terminal-commit corruption is reported as
  `commit_uncertain`, not `corrupt_store`. It still latches.
- **No latch regression:** there is no cheap deterministic seam for a
  latch during the late delivery.
- **Timing in the tests:** both late-path tests rely on 500 ms margins,
  which are documented in the tests.

## Fix round 2

**Status: DONE_WITH_CONCERNS.**
- Review source: the Sol r2 exhaustive review
  (`scratchpad/execution/s1-critic/review-s1-runtime2-sol-r2.md`),
  findings 1–3 and 5–9. Finding 4 (Host-journal corruption kind) is bead
  `via-jm4.21`; via-host is untouched.
- Commit: `fc14ec7`, which carries every fix and test.
- Logs: `scratchpad/s1/runtime2/r2-*.log`.

### Dispositions

| Finding | Disposition | Change |
|---|---|---|
| 1 (important) | fixed | `engine/terminal.rs` `classify`: the exit-code and cleanup guards are removed, so a decoded `completed` is `completed` (C1 §7.6 row 3). Exit and cleanup stay evidence (C1 §7.5). |
| 2 (blocker) | fixed | `journal::commit_terminal_with` takes `(retry, latch)`. A failure that may have written, or corruption, runs `Signal::fail_pending` (the latch's phase one) before the read-back, which is bounded by `READ_BACK` (2 s, runtime §7) and keeps a committed result. Every caller passes the engine's latch through `Commit { retry, latch }`: natural and resolution terminals (`finish_with`), queued cancellations for both owners (`commit_cancellation`), and forced terminals (`finish`). Startup recovery passes none: it fails startup instead. |
| 3 (important) | fixed | `Durable.uncertain` is now `Option<WriteOutcome>`, and `commit_terminal_with` returns `Unended { error, outcome }` with `WriteOutcome::of(&error)`. `Corrupt` therefore stays `corrupt_store` whether or not the read-back finds the terminal. `finished()` and the queued-cancel path classify from that outcome. |
| 5 (important) | fixed | Route: `Finished::Result` and the late path reapply the daemon force after draining (`Serving::unless_forced`): `ForceStopped`, with the close's exit, cleanup, `forced` and journal evidence. `FakeRouteResult` gains `forced`. Adapter: the post-Route delivery is polled before the force (`biased`), so data deliverable at once still goes. A rest ended by the force is `ForceStopped`, and a real delivery failure is `Overflow`; both keep Route's exit, cleanup, `forced` and `journal_uncertain` (no longer `cleanup: None`). |
| 6 (important; the review says blocker) | fixed | On `Driven::Forced`, Core settles its final text (`settle_text`: inline, or file synced) and carries it in `ForcedTurn.text` (`drive::TurnText`). `forced_terminal` applies it in both ordinary and Store-failure shutdown. A failed file step still fails the turn `store` ("the final text could not be written"). |
| 7 (important) | fixed | `FakeRoute::late`: at wall expiry the force close starts at once, and the held message is delivered concurrently (`tokio::join!`). Delivery, close and `finish` share one absolute deadline `by`, with no second allowance. |
| 8 (important) | fixed | `deliver_held` returns `Overflow` when the deadline expires, never `Deadline`, and a closed hop gives the existing hop-closed cause. The daemon force outranks either (`failure_with`). |
| 9 (minor) | fixed | New test-only seam `routes.late.entered`, hit with the terminal decoded and held before anything is closed or delivered, reached through the new `via_wire::failpoint` re-export. The late tests act only on its acknowledgement; the 500 ms gaps are gone. A latch case is added. |

Test-only seams added in this round are listed in
`scripts/check-release-features.py`:
- `routes.late.entered`;
- `core.terminal.read_back`, after the latch's phase one and before the
  read-back;
- `store.commit.corrupt.terminal`, which makes the terminal write report
  `Corrupt` and roll back.

### Tests, RED then GREEN

RED ran against the pre-round code while keeping the new tests and seams:
- Route and Adapter tests: `b2eade0`'s `via-routes` and `via-adapters`
  runtimes with the late seam inserted (`r2-red-route.log`); for the latch
  case, round 0's `6191814` (`r2-red-latch.log`).
- Core tests: `b2eade0`'s `terminal.rs`, plus the three Core fixes
  toggled back (no phase one before the read-back; the outcome reduced to
  `Uncertain` as `outcome_of` did; forced text not applied)
  (`r2-red-core.log`).

GREEN is the gate (`r2-gate.log`). `r2-green-new.log` is an intermediate
run from before `s1_f12_corrupt_terminal_write_latches_as_corruption`'s
end-state assertion was corrected to `failed` (design §7.4); it shows
that test failing (coordinator's correction, from Sol r3).

| Test | Finding | RED | GREEN |
|---|---|---|---|
| `route_drain::a_completed_terminal_then_a_failed_exit_is_completed` (replaces `failure_class_process_exited_after_completed_terminal`) | 1 | `left: "failed" right: "completed"` | `completed`, `final_text` `"done"`, `exit.code` 5 |
| `route_drain::a_completed_terminal_whose_vendor_outlives_the_wall_is_completed` (the reviewer's daemon probe) | 1 | `failed` | `completed`, `"done"`, `exit.signal` set |
| `s1_store_failure::s1_f12_uncertain_terminal_latches_before_its_read_back` (the probe: lost terminal reply, read-back held) | 2 | `health: healthy`, `store_failure: null` during the hold | `store_failed` during the hold; spawn refused `store_error`; then `commit_uncertain` scope `daemon`, exit 4, turn `completed` |
| `s1_store_failure::s1_f12_corrupt_terminal_write_latches_as_corruption` | 3 | `kind: commit_uncertain` | `corrupt_store` scope `daemon`; exit 4; the batch ends the turn `failed` |
| `s1_store_failure::s1_force_keeps_the_final_text_core_received`, ordinary (`daemon stop --force`) and latch (lost receipt reply) variants (the probe: FinalText handled, then force) | 6 | durable `final_text: ""` | `cancelled`, `forced`, `final_text: "done"`, in both variants |
| `route_stop::a_force_during_late_delivery_keeps_the_held_terminal` | 5, 9 | `Overflow`, `cleanup: None`, `forced: false` | `ForceStopped`, cleanup `Quiescent`, `forced`, text `"done"` |
| `route_stop::a_latch_during_late_delivery_keeps_the_held_terminal` | 9 | against round-0 `flush`: `Overflow` from the latch (`vendor message over the 1048576 byte cap`), which proves the latch was set | `completed`, cleanup `Quiescent`, text `"done"` |
| `route_stop::an_undelivered_held_terminal_is_not_a_completion` | 7, 8, 9 | `Deadline` | `Overflow`, cleanup `Quiescent`, `forced`, text `""`, late-path acknowledgement seen |
| `route_stop::a_held_terminal_is_delivered_after_wall_expiry` | round-0 regression | (round 0) | `completed`, cleanup `Quiescent`, `"done"` |

**Order proofs, with no timing gaps:**
- Late tests: the `routes.late.entered` acknowledgement proves that the
  terminal is decoded and held and that the late path was entered, with
  nothing closed or delivered yet.
- Force: the test raises the force after that acknowledgement and before
  release.
- Latch: the vendor writes a 2 MiB line after the acknowledgement. Its
  `wrote` marker appears only once the pipe has taken the line, so the
  reader has passed the 1 MiB bound and latched. Only then is Route
  released. Draining starts after release.
- Daemon tests: `core.run.settling`, `core.terminal.read_back`, and
  `core.observations.pause` hit 3, which comes after the FinalText
  observation.

**Updated tests:**
- `failure_class_process_exited_after_completed_terminal` is replaced.
  The guard it relied on contradicts C1 §7.6; the new test asserts the
  exit evidence instead.
- The three late-path tests are rewritten for the new seam and are now
  test-failpoints only. The default build therefore runs one test fewer
  (345).
- The round-1 time probe (`LATE_PROBE`) is removed.

### Gate counts (tip `fc14ec7`; `r2-gate.log`, `gate exit 0`)

| Check | Result |
|---|---|
| fmt, both clippy runs, deny, check-layers, release build and release-feature check | ok |
| default nextest | 345 passed, 1 skipped |
| failpoint suite (run 1) | 558 passed, 1 skipped |
| `s1_f(08\|09\|10\|12)_` | 61 passed |
| selector, 5 repeats | 88 passed each time |
| failpoint suite, runs 2 and 3 (`r2-failpoints-2.log`, `r2-failpoints-3.log`) | 558 passed, 1 skipped each time |

After the runs, no process from this worktree remained.

### Design sentences I believe change (for the coordinator to write)

1. **T3 design §2 rule 3 [r1.23].** At wall expiry with a decoded
   terminal, Route starts Host's force close at once and delivers any
   held message concurrently. Delivery, close and drain share one
   absolute cleanup deadline. Delivery that cannot finish by then is
   `Overflow`. The result is the terminal with the force close's evidence
   unless the daemon force is set (rule 4), which makes it `ForceStopped`
   with that evidence. The connection latch does not change a decoded
   late terminal's result. The normal path is unchanged: a latch during
   finalization still fails the turn.
2. **T3 design §2 rule 4.** Route reapplies the daemon force after any
   successful exit's drain. The Adapter hands over post-Route data that
   is deliverable without waiting even under the force. A rest the force
   ended is `ForceStopped` with Route's evidence, and `Overflow` is only
   a real delivery failure.
3. **T3 §7.2 / runtime §7 (terminal commits).** A terminal commit that
   may have written, or hit corruption, raises the latch's phase one
   before its read-back. The read-back is bounded at 2 s and keeps a
   committed result. The failure is classified from the commit's typed
   outcome, so corruption is `corrupt_store`.
4. **T3 §6.8 / design §2 rule 4 (forced terminals).** A forced terminal
   carries the final text Core received before the force: inline, or its
   synced file. A failed file step fails it `store`, as on the natural
   path.
5. **C1 §7.6 row 3.** Core's classifier no longer turns a decoded
   `completed` into `failed(process_exited)` for a non-zero exit or
   uncertain cleanup. Earlier design text or tests that implied it are
   superseded.

### Concerns

- **One commit for the whole round.** Findings 2, 3 and 6 share
  `drive.rs` hunks, findings 5, 7 and 8 share `run_turn`, and the three
  seams share one hunk of the release list. Splitting per finding would
  need hunk staging. This is a deviation from "one or two commits per
  finding".
- **Latch-case result is my choice.** Under a latch, the late path
  returns the delivered terminal (`completed`); only the force changes
  the result. The coordinator's disposition fixed force but not latch.
  I chose this because pre-chunk behaviour and rule 3 [r1.23] return a
  decoded late terminal, and because a latch raised by Route's own force
  close must not fail it. Design sentence 1 records it.
- **Unbounded forced-path file sync.** The forced path now awaits the
  final-text settle, a file sync, before the handoff. It is a bounded
  Store blob step, but it runs after the force. A stalled filesystem
  delays that dispatcher's handoff until final shutdown aborts it
  (`dispatchers_by`), and then the text is lost as before.
- **Finding 4 is not addressed.** Host-journal corruption still reports
  `commit_uncertain`; it is bead `via-jm4.21`.
