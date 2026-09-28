# T3-force-row report: an exit Route observes under the force is the force row

Branch `wt/t3-force-row`, cut from `rust-foundation` at `5f3c9f7`. Trigger:
the merged-tree gate on `rust-foundation` (`9efbab8`) failed its third
failpoint run on `s1_f12_host_early_stop_independent_of_store` with B ended
`failed` / `process_exited` instead of the force row. The test passed alone
25 of 25 times. Normative source: `design.md` §6.8 pipeline step 5 [S3]
("a vendor exit Route observes under the daemon force is the force row
(`ForceStopped`), not `process_exited`"). Status: **DONE_WITH_CONCERNS**
(see "Concerns and limits").

Commits:

| Commit | Content |
|---|---|
| `e8a2d0e` | Test-only seam `wire.exit.observed`, its entry in `scripts/check-release-features.py`, and the first version of the regression (RED). |
| `019f1a7` | The regression drops its group member and asserts the force row's shape (RED). |
| `c966a71` | The fix: `FakeRoute::finalize` reads the force after a recorded exit (GREEN). |
| this report | `reports/T3-force-row.md` |

The two test commits fail on their own by design (the regression is
first); the tip is green. Squash `e8a2d0e` and `019f1a7` if bisectability
matters.

## The window

Route's future runs inline inside Core's dispatcher `select!`
(`engine/drive.rs`, `execute`: `Box::pin(self.adapter.execute(..))`). The
Host early-stop task and the vendor are separate, so they keep running when
the dispatcher is parked.

1. The test parks B's dispatcher at `core.commit.before_send`. Route and the
   adapter are frozen with it. B's vendor is released, emits `assistant_text`
   and the terminal, and then waits (fake `finalize_input`, up to 2 s) for
   stdin EOF, which a frozen Route never sends.
2. The latch raises the daemon force (`latch.rs`, `signal.force.send_replace(true)`).
   Host's early-stop task, subscribed through `Engine::watch_force`, sends
   `Stop`. The anchor SIGTERMs the vendor. Host's 50 ms status poll
   (`Host::track_control`) then publishes `ExitReport { code: None, signal: 15 }`
   on the connection's `exits` watch.
3. The dispatcher resumes. Route now has, all ready at once: the buffered
   frames (`accepted`, `text`, terminal), stdout EOF, the recorded exit, and
   the force. Wire's `read_either` uses a `tokio::select!` that is not
   biased, so when `cancel` and a read are both ready the pick is random. In
   the failing runs the reads won each time.
4. `drive` reads the terminal. `finalize` calls `close_input`, reads to EOF
   (`Next::Eof`, whose arm has no force check) and calls `wire.wait_exit`.
   `wait_exit` returns `Ok(exit)` from the recorded `exits` value before its
   `select!` over `cancel`, so it never sees the force. `finalize` then did
   `Ok(exit) => return Ok(exit)`, with no `after_terminal()`.
5. `drive` closes gracefully and returns the completed result carrying the
   exit `{code: null, signal: 15}`. Core's `classify` maps a completed
   terminal whose `exit.code != Some(0)` to `failed` / `process_exited`, "the
   vendor exited unsuccessfully".

So the interleaving that yields `process_exited` is: force set, then Host
stops the vendor, then Route consumes the recorded exit without reading the
force. S3 put `control.after_terminal()` before and after `wait_exit` in
`drive`'s pre-terminal EOF branch and on the `Woken` arms of `finalize`, but
not on `finalize`'s own `Ok(exit)` arm. That is the remaining gap.

Evidence that this is the path (a temporary trace in Route, since removed,
run 400 times at 128 parallel): every failure printed "terminal read
force=true", "finalize eof force=true", "finalize exit ... signal 15
force=true". A like-for-like count on the unfixed tree, 1200 runs of the
F12 test at 128 parallel (test binary run under `xargs -P 128`): **73 of
1200 failed**, all 73 with `state: failed`, `failure.class: process_exited`
and `exit: {code: null, signal: 15}`.

## Deterministic regression

Freezing Route at an existing await leaves Wire's random select in play, so
such a test is red only some of the time. The seam pins the one ordering the
defect needs: **a recorded exit not yet returned to Route, while the force
is set**.

- **Seam.** `wire.exit.observed`, a test-only failpoint in
  `WireConnection::wait_exit`, where the recorded exit is about to be
  returned (`crates/via-wire/src/runtime.rs`). It is behind
  `test-failpoints`, listed in `POINTS` of `scripts/check-release-features.py`,
  and release builds do not contain it (checked below). Both routes into the
  return, the top-of-loop read and the read after `exits.changed()`, pass
  through it. The watch guard is copied out first so it is not held across
  the await.
- **Test.** `s1_f12_exit_observed_under_force_is_the_force_row`
  (`crates/via-cli/tests/s1_store_failure.rs`). The vendor emits its
  terminal and exits 1. Wire is paused at the seam with the exit recorded.
  `daemon stop --force` is sent; its receipt comes after the force is set
  (`stop.rs`: `send_replace(true)` precedes the reply). The seam is then
  released and the daemon exits. Assertion: B's envelope has no `failure`,
  no `exit` and a settled `cancel`, which marks Core's forced terminal, and
  not the vendor's own `process_exited`.
- **RED on the unfixed tree**, 30 of 30 runs, the same failure every time:

  ```
  Error: "B did not end by the force row (daemon exit status: 0): {...
    "cancel":null,... "exit":{"code":1,"signal":null},
    "failure":{"class":"process_exited","message":"the vendor exited unsuccessfully","retryable":false},
    "final_text":"done",... "state":"failed","stop_reason":"error",...}"
  ```

- **GREEN with the fix**, 30 of 30 runs; the envelope is the forced
  terminal (`state: unknown`, `failure: null`, `exit: null`,
  `cancel: {outcome: requested, cleanup: quiescent, settled_at: ...}`).

Fidelity limit. The regression's vendor exits 1 by itself; the flake's
vendor died by SIGTERM from Host. Route's decision cannot depend on which,
since it reads only the exit and the force. A SIGTERM-caused variant is not
deterministic without also removing Wire's random select (the vendor must be
alive when the force arrives, which puts Route back at an await where
`read_either` runs). That variant stays as the existing F12 test, which
still asserts `cancel.outcome == "forced"`. The regression's outcome is
`requested` because the vendor was gone before Host's `Stop`, so there was
nothing live to stop (`anchor.rs`: `stopped_live` records that the vendor
child was live).

## The fix and the layer

`FakeRoute::finalize` (`crates/via-routes/src/runtime.rs`):

```rust
Ok(exit) => {
    control.after_terminal()?;
    return Ok(exit);
}
```

- **Why sufficient.** Host raises the force before it stops any vendor
  (§6.8: the early-stop task waits on the force, then sends `Stop`). An exit
  the early stop caused therefore happens after the force is set, through
  the chain force set, `Stop` sent, vendor killed, anchor status, Host poll,
  exit recorded, Route reads it. Each hop synchronizes, so a force read
  after Route has the exit sees it set. No exit that Host caused can reach
  Core as `process_exited`.
- **Why Route.** Route owns what an observed exit means to the turn, and
  already holds the force (`Control.force`). It mirrors the pre-terminal EOF
  branch of `drive`, which reads the force before and after `wait_exit`, and
  the `Woken` arms of `finalize` (S3, commit `b814876`). The design puts this
  row in Route ("a vendor exit Route observes under the daemon force").
  It adds no state, no side set and no polling.
- **Why not elsewhere.** Wire's contract is right: a recorded exit is a
  fact and Wire holds no force policy after the exit. Core's `classify` sees
  only Route's result, so it could tell the two cases apart only with new
  state or timing. Biasing `read_either` toward `cancel` would shrink the
  window but not close it, and it changes every Wire read for one caller.

## Is the existing test racy?

`s1_f12_host_early_stop_independent_of_store` is unchanged. Its
expectations are the design's; the race was in the product (the window
above), which the test's parked dispatcher makes reachable. Its waits are on
acknowledgements, durable rows and process absence, not sleeps. With the fix
it passed **1200 of 1200** runs at 128 parallel (73 of 1200 failed
without it), and in every gate run below.

## Files changed

| File | Reason |
|---|---|
| `crates/via-routes/src/runtime.rs` | The fix: read the force after `finalize`'s recorded exit. |
| `crates/via-wire/src/runtime.rs` | Test-only seam `wire.exit.observed` in `wait_exit`; the guard is copied out so it is not held across the await. Behaviour is unchanged without `test-failpoints`. |
| `scripts/check-release-features.py` | Every failpoint must be in `POINTS` so the release check can prove it absent. |
| `crates/via-cli/tests/s1_store_failure.rs` | The regression and its one-turn script helper `exits_failing`. |
| `docs/workstreams/rust-foundation/t3/reports/T3-force-row.md` | This report. |

## Design edits needed (not made)

- `design.md` §10 seam list: add `wire.exit.observed`, "pause in
  `WireConnection::wait_exit` with the exit recorded and not yet returned
  (force-row regression, §6.8 step 5)".
- `design.md` §11 test list: add `s1_f12_exit_observed_under_force_is_the_force_row`.
- `design.md` §6.8 step 5 (optional): say that Route reads the force after a
  recorded exit in `finalize` as well as in `drive`'s EOF branch, and that the
  ordering guarantee is "force before `Stop`".

## Gate

Run from the worktree at `c966a71`, in order, each a separate step.

| Step | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings`, default and `--features via-cli/test-failpoints` | pass, pass |
| `cargo nextest run --locked --workspace` | 286 passed, 1 skipped (base 286/1) |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints`, run 1 to 5 | 414 passed, 1 skipped each time (base 413/1; the +1 is the new test). No failure, retry or timeout in any of the five runs |
| `cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 52 passed (base 51; +1 is the new test, whose name is in this selection) |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | pass |
| `cargo build --locked --release -p via-cli --no-default-features`, then `python3 scripts/check-release-features.py target/release/via` | pass: no `test-failpoints`, 82 points armed and ignored, none of 89 markers present (includes `wire.exit.observed`) |

The Task 4 selection `s1_(f2[4567]|raw|bounds|store)_` was skipped as
instructed.

## Concerns and limits

- The five sequential gate runs are not a load test. The flake needed
  parallel load; the 1200-run 128-parallel comparison is the evidence for the
  end-to-end test, and the regression does not depend on load.
- The regression's vendor exits 1 rather than dying by SIGTERM (fidelity
  limit above). The end-to-end SIGTERM case stays in the F12 test.
- `read_either` (Wire) picks randomly between a ready `cancel` and a ready
  read, although its doc comment says cancel ends the wait before any byte is
  read. It is not a defect after this fix, since Route now reads the force
  after every exit it can observe, but the comment and the behaviour differ.
  Left as is; a design or Wire follow-up if the owner wants the comment true.
- A force set after Route has read the exit and passed the check still ends
  the turn as an ordinary completion. That is correct: no Host stop preceded
  that exit.
- The vendor's exit under the force is reported as the force row even when
  the vendor exited on its own just before the force (outcome `requested`,
  state `unknown` in the regression). That follows the design's rule and the
  existing `drive` EOF branch, and it is the same conservative row.
