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
