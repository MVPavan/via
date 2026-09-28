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

## Round 2: Core-side acknowledgements for item 7

Sol medium review (SOUND WITH CHANGES): items 6, 8 and 9 passed. Major
finding: the item 7 test asserted Host and anchor hits, which do not show
that Core received the same turn's `stopped_live` and absence evidence before
the terminal commit (the absence failpoint fires before the journal commit,
and Core can infer quiescence when the report has no failure).

Change (Core engine stop path only, `crates/via-core/src/engine/stop.rs`,
`finalize_forced`; no `reprobe.rs`, `close.rs`, `drive.rs` or `batch.rs`
edit): two test-only, count-or-pause seams, acknowledgement style as
`core.shutdown.reconcile_entry`:

- `core.shutdown.evidence_stopped_live`: hit once for a forced turn whose
  reconciliation record (matched by session and turn) has `forced`.
- `core.shutdown.evidence_absent`: hit once for a forced turn whose
  reconciliation record has `cleanup == Quiescent`.

Both fire ahead of `core.shutdown.before_forced_terminal`, after
`adapter.shutdown` returned the report, inside
`#[cfg(feature = "test-failpoints")]`. Both are listed in
`scripts/check-release-features.py` (marker scan: 90 markers absent from the
release binary, was 88). Failpoint points are static names, so "keyed to the
turn" is by construction: a seam fires per matching record, so N forced
turns give occurrences 1..N in `finalize_forced` order; the test drives one
forced turn.

Test (`evidence_before_terminal`): counts both seams; at the
`before_forced_terminal` pause the group is gone, the turn has no terminal,
and, deferred: 1 `stopped_live` and 1 absence receipt; lost-stop variant: 0
`stopped_live` receipts and 1 absence receipt, the `final_reply_lost` ack
present, envelope `unknown` / `requested` / `quiescent`. The Host/anchor
hit-count proxies were dropped (except the pre-reconciliation `proofs == 0`
check in the deferred case). Five repeated runs of both green.

Mutation RED (backup `cp`, restored, `diff` clean):
- skip the `stopped_live` delivery (`if false`): `Core received 0
  stopped_live and 1 absence facts` (`i7r2-mut-skip.log`); the deferred test
  fails.
- deliver both after the terminal seam (`before_forced_terminal` moved above
  them): `Core received 0 stopped_live and 0 absence facts` in both tests
  (`i7r2-mut-after.log`). This is the observable "delivered after the
  terminal" position: the harness holds the terminal seam.
- Round 1 mutation (ignore reconciliation's `forced` in `forced_terminal`)
  still fails the envelope check.

The test is a characterization of ordering that the code already had; the
seams make the ordering observable.

Commit 0785aaa (trailer `Claude Sonnet 5.5`, per the coordinator).

### Design edits needed (round 2)

- §10 seam table: add `core.shutdown.evidence_stopped_live` (acknowledgement
  when Core reads a forced turn's reconciliation record with `forced`, in
  `finalize_forced`, before `core.shutdown.before_forced_terminal`) and
  `core.shutdown.evidence_absent` (same, for `cleanup == Quiescent`).
- §11 `s1_f12_evidence_before_terminal`: "their delivery is acknowledged"
  means these two seams. The lost-stop variant: the `stopped_live` seam does
  not fire, the absence seam does, terminal `unknown` / `requested` /
  `quiescent`. Drop the earlier remark about Host-side proxies from the
  round 1 edits above.
- Seam semantics limitation: the seams report facts in the reconciliation
  record, not Route-close evidence (`turn.close`); cleanup proved only by
  Route's close fires no `evidence_absent`.

### Gate (round 2)

| Step | Result |
| --- | --- |
| fmt, clippy (default, `via-cli/test-failpoints`) | clean |
| default nextest | 287 passed, 1 skipped |
| failpoint nextest | 418 passed, 1 skipped on 7 of 9 full runs (see below) |
| `s1_f(08\|09\|10\|12)_` selection | 53 passed |
| `cargo deny check`, `check-layers.py` | ok |
| release build and `check-release-features.py` | ok, 90 markers absent |

Intermittent failures over nine full failpoint runs (not retried away; the
next full run was repeated, and each test passed on rerun):
- `s1_f12_host_early_stop_independent_of_store` failed once (known flaky
  under load, another worker's fix pending): `B did not end by the force
  row`.
- `s1_shutdown_budget_read_cutoff_before_reconciliation` failed once
  (2.1 s, log not captured), then passed 20 of 20 in an isolated
  `--stress-count 20` and in every later full run. Cause unknown; the seams
  added here fire only in `finalize_forced`, after that test's checkpoint
  (`reconcile_entry`), so it is not evidently related. Reported as a new
  intermittent failure to investigate.

## Round 3

Sol review of round 2 (SOUND WITH CHANGES; Host-hit drop and occurrence keying
accepted). Two items.

### 1. The absence seam must observe what the terminal consumes

Finding: the seam read the reconciliation record in `finalize_forced`, while
`forced_terminal` read it again, so the test passed even if the terminal's
cleanup calculation ignored the record and inferred quiescence from a
failure-free report.

Option taken: single-read restructuring, at the owning point
(`crates/via-core/src/engine/stop.rs`); it was small, so the mutation-only
alternative was not needed. New `Engine::forced_facts(&turn, report)` reads the
turn's record once and returns `(quiescent, forced)` (Route's close OR the
record's fact); `forced_terminal` takes that pair instead of the report, and
`finalize_forced` computes it once before the terminal seam. In test builds
each seam fires in the branch that consumes the field:
`evidence_absent` inside the `Some(record)` arm of the cleanup calculation,
`evidence_stopped_live` where `record.forced` is read. The cleanup expression
is now computed without short-circuiting on Route's close (pure, same
values). No production behaviour change. Because a function whose only awaits
are test seams trips `clippy::unused_async` and `unused_async_trait_impl` in
default builds, it carries a `cfg_attr(not(test-failpoints), expect(...,
reason))`.

Why the seam had to move into the arm: replacing the cleanup calculation with
inference gives the same value in these scenarios (the report has no failure
and the record is Quiescent), so the envelope alone cannot detect it; only the
seam's position in the consuming branch does.

RED (`i7r3-mut-infer.log`; backup, restored, `diff` clean): the record arm
replaced by `Some(_) => report.failure.is_none()`. Both tests fail: `Core
received 1 stopped_live and 0 absence facts` (deferred), `0 stopped_live and 0
absence facts` (lost). GREEN after restore: both pass, three repeated runs.
A mutation that keeps the seam and changes only the value is not caught by
this test; that is inherent to observing a value at its use.

Commit 91b5d1b.

### 2. Intermittent `s1_shutdown_budget_read_cutoff_before_reconciliation`

Not reproduced; no code change. The one failure (2.1 s, round 2, log not
captured) is unexplained. Attempts, logs under `scratchpad/t3-rev-y/`
(`r3-stress1.log`, `r3-stress2.log`, `r3-stress3.log`, `r3-gate*.log`):

- `--stress-count 150` of the target, with eight concurrent failpoint suite
  runs alongside: 150/150 passed (`r3-stress1.log`).
- `--stress-count 60` of the target plus `s1_force_cutoff_worker_stalled_read_is_never_clean`
  (120 test runs) with three concurrent looping failpoint suites:
  60/60 iterations passed (`r3-stress2.log`); 18 concurrent full suite runs
  had no failures.
- `--stress-count 300` of the target with two concurrent looping suites:
  300/300 passed (`r3-stress3.log`); the 20 concurrent suite runs had no
  failures.
- 18 unloaded full failpoint suite runs, plus 3 gate runs and the round 2
  runs: no failure.

That is about 510 target iterations under load and about 60 full-suite runs,
short of the 1000 iteration budget but with no failure. The failed
assertion is unknown, so neither the global-read-arming hypothesis
(`force_with_stalled_read` arms "the next global Store read" after pausing
`core.force.cancel_read`, so a different read could reach the marker) nor the
Route exit-versus-force race (c966a71) can be confirmed or ruled out. A 2.1 s
failure is well under the 5 s reconciliation bound, so it was an outcome
assertion (exit, summary, or the `cancelled forced quiescent` check), not the
timing check. Sol's source reading is plausible; the fix (arming a named
`store.read.<command>` seam) was not applied because it would be a guess
without a reproduction. If it recurs, capture the nextest output for the
failure.

### Gate (round 3)

| Step | Result |
| --- | --- |
| fmt, clippy (default, `via-cli/test-failpoints`) | clean |
| default nextest | 287 passed, 1 skipped |
| failpoint nextest, three runs | 418 passed, 1 skipped, all three |
| `s1_f(08\|09\|10\|12)_` selection | 53 passed |
| `cargo deny check`, `check-layers.py` | ok |
| release build and `check-release-features.py` | ok, 90 markers absent |

Design edits still needed (owner adds at merge): the two seam names in the §10
table, described as reporting reconciliation-record fields, consumed where the
terminal calculation reads them (`forced_facts`), not Route-close evidence.
