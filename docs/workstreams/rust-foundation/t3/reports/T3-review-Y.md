# T3 review fixes Y (items 6 to 9)

Branch `wt/t3-rev-y`, cut from `rust-foundation` at 5f3c9f7. Scope: one CLI
bug (item 6) and three missing proofs (items 7 to 9). Nothing pushed.

## Item 6: explicit plain `daemon stop` against a version-mismatched idle daemon

Bug: `client::request_within` returned the `version_mismatch` hello reply for
any request made without auto-start, so the stop verb never reached the
Store-identity check nor the plain stop that design §6.2 permits on a
mismatched connection.

Fix (`crates/via-cli/src/client.rs`): a new `is_plain_stop(method, params)`
(`daemon/stop` without `drain` or `force`). The mismatch guard now returns
early only when already restarted or the error is not `version_mismatch`;
without auto-start it returns unless the request is a plain stop. The
existing `same_store` check then runs, the stop is sent on the mismatched
connection, and `if stop.stopping != true || !auto_start` returns the stop
reply, so an explicit stop never starts a replacement. Requests that
auto-start are unchanged. Drain and force stops on a mismatched daemon still
return the mismatch reply.

Test: `s1_f04_explicit_stop_from_mismatched_version_stops_idle_daemon_only`
(`s1_lifecycle.rs`, failpoint feature: `VIA_TEST_CLIENT_VERSION` makes the
real CLI binary report another version to a real daemon; end to end).

- RED (`scratchpad/t3-rev-y/i6-red.log`, before the fix): `store mismatch:
  {... "kind":"version_mismatch" ...}`: the CLI printed the mismatch reply.
- GREEN (`i6-green.log`): 2 passed (new test plus the existing sibling).

Commit 0df854c.

## Item 7: `s1_f12_evidence_before_terminal`

Tests (`s1_lifecycle.rs`, failpoint feature), sharing
`evidence_before_terminal(deferred)`:

- `s1_f12_evidence_before_terminal`: the anchor defers `begin_cleanup` and
  withholds `stopped_live` (`host.anchor.defer_cleanup`, persistent). A force
  ends the running turn. Final shutdown is paused at
  `core.shutdown.reconcile_entry`, where the group is alive, the turn running,
  a Stop was delivered and no absence proof was committed. The failpoint is
  then disarmed, so reconciliation's Stop is the only source of `forced`.
  Resuming reaches `core.shutdown.before_forced_terminal` (evidence in,
  terminal not committed): the vendor is gone, `host.recovery.absence_commit`
  and `host.anchor.stop_received` hits both grew, and the turn is still
  `running`. After release the envelope is `cancelled` / `forced` /
  `quiescent`.
- `s1_f12_evidence_before_terminal_lost_stop_evidence_is_unknown`: every Stop
  reply is lost (`host.anchor.final_reply_lost`, persistent). At the terminal
  seam the vendor is gone, the absence proof is in, the lost-reply ack exists,
  the turn is still `running`. Envelope: `unknown` / `requested` /
  `quiescent`: lost stop evidence gives `unknown`, cleanup is decided
  independently by the absence proof.

The code already satisfied both, so the positive test is a **characterization**
(no production change). Mutation RED (`i7-mut1.log`): in
`engine/stop.rs::forced_terminal`, `let forced = turn.close.forced ||
evidence.is_some_and(|record| record.forced);` replaced by `let forced =
turn.close.forced;` (drops reconciliation's evidence). The positive test fails
with terminal `state: unknown`, cancel `requested`/`quiescent`. Restored from a
`cp` backup and verified with `diff`. Five repeated runs of both tests were
green (about 3.3 s each). Development runs: `i7-run1.log`, `i7-run2.log`
(assertions relaxed: in the lost variant Route's close can already prove
absence, so a strict increase is required only in the deferred case),
`i7-run3.log` (green).

Commit b08fe9d.

## Item 8: force set includes `Cancelling`

Test: `s1_f07_force_set_includes_session_in_cancelling_state`. A queued turn's
cancellation is held at `store.commit.cancel` (the session is in
`Cancelling{request}`), force is accepted, then the commit is released. The
session is closed exactly once with `daemon_stop_force`. Raw connections for
the forcer and canceller are opened before the pause (a daemon paused at
`daemon.dispatcher.before_start` accepts no new connections).

Characterization; mutation RED (`i8-mutation-red.log`): excluding Cancelling
entries from the force set in `engine/queue.rs::Slot::unfinished` fails with
`0 closures, reason None`. Restored from backup and verified with `diff`.
Commit 4997465.

## Item 9: F2 lock before socket replacement

Test: `s1_f02_losing_daemon_leaves_live_socket_untouched`. With a live daemon,
a second daemon that loses `daemon.lock` (exit 75) and one that loses
`store.lock` (exit 4) must leave the socket's identity (device and inode) and
the live listener as they were. The `socket_identity` helper compares
identity with `.ok() == Some(before)` so a removed socket reads as a clear
failure.

Mutation RED (backup of `crates/via-cli/src/server.rs`, restored, `diff`
clean): moving the replacement before `daemon.lock` (`i9-mutation1-red.log`):
`the losing daemon removed or replaced the socket`; moving it before
`store.lock` (`i9-mutation2-red.log`): `the store-lock loser removed or
replaced the socket`. Commit 182b091.

## Files changed

- `crates/via-cli/src/client.rs` (production, item 6)
- `crates/via-cli/tests/s1_lifecycle.rs` (tests, items 6 to 9; one import
  line gained `OpenOptionsExt`)
- this report

No other production edit. No seam added: every failpoint used already existed.
`crates/via-core/src/engine/stop.rs`, `queue.rs` and `via-cli/src/server.rs`
were mutated for RED and restored byte for byte.

## Design edits needed (not made)

- §11 `s1_f12_evidence_before_terminal`: there is no Core-side seam for "the
  delivery acknowledgements". The test proxies them: absence by
  `host.recovery.absence_commit` hits, stop delivery by
  `host.anchor.stop_received` hits, lost stop evidence by the
  `host.anchor.final_reply_lost` ack, with `core.shutdown.before_forced_terminal`
  as the "evidence in, terminal not committed" seam. The design should state
  these proxies, or add a dedicated seam if an ordering assertion on the
  acknowledgements themselves is required. Also record that the "no absence
  proof before reconciliation" assertion holds only for the deferred case,
  because Route's close can already prove absence.
- §6.2: state that an explicit plain `daemon stop` against a Store-matched
  version-mismatched idle daemon stops it and never starts a replacement;
  drain and force stops still return the mismatch reply.
- §11: list items 8 and 9 as characterizations with their mutation evidence.

## Gate

Run from the worktree, in order, each step separately.

| Step | Result |
| --- | --- |
| fmt | clean |
| clippy, default and `via-cli/test-failpoints` | clean |
| default nextest | 287 passed, 1 skipped (base 286/1; +1: the item 9 test, the only new one without the failpoint feature) |
| failpoint nextest, three runs | 418 passed, 1 skipped, all three runs (base 413/1; +5 tests) |
| `s1_f(08\|09\|10\|12)_` selection | 53 passed |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `check-layers.py` | exit 0 |
| release build (`--no-default-features`) and `check-release-features.py` | ok, exit 0 |

No intermittent failure observed, including
`s1_f12_host_early_stop_independent_of_store`.

## Concerns

- Commit trailers: the brief specified `Co-Authored-By: Claude Sonnet 5.5`;
  after a context reset the harness attribution reminder specified `Claude
  Code`. Commits 0df854c, 182b091 and 4997465 carry the brief's form; b08fe9d
  and the report commit carry the harness form. Nothing amended; normalize on
  merge if wanted.
- The item 7 lost variant relies on persistent `fail_io_persist` on
  `final_reply_lost`; `verify_anchors` confirms the anchor points at the end.
