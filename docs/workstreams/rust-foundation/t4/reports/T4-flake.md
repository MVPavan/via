# T4-flake report: daemon scenarios that fail under parallel runs

Bead `via-jm4.7.8.11`. Branch `wt/t4-flake`, cut from `rust-foundation` at
`9a9b09f`. Logs are under `scratchpad/t4/flake/` (gitignored). Tags: **[V]**
verified from artifacts or runs; **[I]** inferred.

Selector ("the selector" below):

```bash
cargo nextest run --locked --workspace --features via-cli/test-failpoints --no-fail-fast \
  -E 'test(/^s1_(c1|f05|progress|store_steps|f27|f24|wire|store|blob|bounds_json|evidence)_/)'
```

"Parallel stress" below means 24 concurrent loops of 5 runs each (120 runs)
of one test binary with `--exact <test>`, which reproduces the load of the
selector run.

## 1. Root causes

Four independent causes. None is a daemon race in the Core or Store. One is a
product bug in `serve --stdio`, and three are harness or test ordering
assumptions.

### 1.1 The harness readiness probe started a rival daemon (most failures)

Tests: `s1_c1_status_progress_only_for_the_selected_turn`,
`s1_progress_step_rows_survive_crash_to_last_commit`,
`s1_c1_status_alive_false_after_exit_before_control_drop`,
`s1_c1_prompt_file_copies_hashes_and_refuses_changes`,
`s1_c1_spawn_members_label_require_and_instructions`, and in my
reproduction `s1_progress_step_rule_counts_output_after_tool_results`.

Cause. `Daemon::start_with` (`crates/via-cli/tests/support/daemon.rs`)
spawned the child `via daemon` with the failpoint environment, then polled
readiness with `via daemon status --json` through `Sandbox::command()`.
`daemon status` auto-starts a daemon (`crates/via-cli/src/main.rs`,
`client::call("daemon/status", …, true, …)`). When the probe ran before the
child had bound `via.sock`, the CLI started a second daemon. That daemon had no
`VIA_FAILPOINT_DIR` because the probe's environment lacks it. The two raced
for `daemon.lock`. When the second daemon won, the child exited 75, and the
probe then read the rival's status and declared the child ready. Every armed
failpoint then went unhit. The rival also held the turn's anchor, so the
outer cleanup could not verify it.

Evidence [V]. Each of the five artifacts named in the historical logs
(`scratchpad/t4/flake/run-{4,7,9,10}.log`, `scratchpad/t4/merge-t4-5/gate.log`),
plus my reproduction (`scratchpad/t4/flake/repro/before-10.log`, 1 failure in
10 runs), has:

- a `daemon.trace` of exactly `another VIA daemon holds <tmp>/runtime/daemon.lock`,
  which is the child's stderr;
- `cleanup.json` `direct_child.was_alive: false`, `stop: not_attempted`;
- a `daemon_status.stdout` pid that differs from the child's pid. For
  example, `s1_progress_rows_survive_crash-HrLHlX` has child 1167242 and status
  pid 1167701, and my `s1_progress_step_rule-wAbDBa` has child 1493838 and
  status pid 1494651;
- an anchor still `present` with `verification: challenge_refused`, because
  the rival daemon still owned it.

`/proc/1494651/environ` (the rival daemon, still running after the test)
had `VIA_RUNTIME_DIR` but no `VIA_FAILPOINT_DIR`. Two more such daemons, from
passing runs, were alive at the same time. The rival can therefore win even
when a test passes. Rivals exit by themselves at the 60 s idle interval.

The `spawn_members` artifact's anchor was at phase `intent` with no pid. It
is the same rival daemon, caught before its launch recorded the pid [I].

Fix. The readiness probe now sends `hello` and `daemon/status` over a direct
socket connection (`Raw`, which never auto-starts). It accepts only a status
whose `pid` equals the child's pid (`support/daemon.rs`). `s1_lifecycle.rs`
already uses this pattern ("readiness never auto-starts another daemon"). Two
other private copies of the unguarded probe had the same race:
`s1_prompt_to_result.rs` `start_daemon` (in the selector:
`s1_c1_events_receipt_and_envelope_shapes`) and `s1_turn_control.rs`
`Sandbox::start`. Round 1 gave them a socket-exists guard and a pid match.
Review round 1 showed that this was not enough (see §2.1), and both now use
the direct probe.

Regression test: `s1_evidence_harness_readiness_never_starts_a_daemon`
(`crates/via-cli/tests/s1_evidence.rs`, failpoint builds). The harness child
serves another deployment and idle-exits after 3 s, so the sandbox's socket
never appears. The test is deterministic. Since review round 1 it requires
positive evidence for this case:

- a direct probe saw the child serving its own socket;
- readiness reported exactly
  `fail: daemon exited before readiness: exit status: 0`;
- the child's trace holds a clean `idle` shutdown summary;
- the sandbox has neither a socket nor a Store, which any daemon started
  there would create.

An unrelated startup failure therefore no longer passes the test. Results:

- RED, with the old CLI probe restored in `support/daemon.rs`:
  `readiness reported "ready"; child served Some(3799579); … sandbox socket
  true, Store true` (`scratchpad/t4/flake/r3/regression-red.log`; the round-1
  form is in `repro/regression-red.log`).
- GREEN: `1 test run: 1 passed` (`r3/regression-green.log`).

### 1.2 `serve --stdio` could exit 0 after a failed stdin read (product bug)

Test: `s1_c1_serve_stdio_matches_the_socket`. Error: `a failed stdin read
exited 0`. It appeared once in my first post-fix 20-run loop
(`scratchpad/t4/flake/final/run-1.log`).

Cause. In `client::serve_stdio` the stdin copy task shut the socket's write
side and only then returned `Copied::Stdin(Err(..))`. The shutdown makes the
daemon close its side, which ends the stdout copy. When that completion
reached `join_next` first, the loop broke on `Copied::Stdout(Ok(()))` and never
saw the stdin error, so the proxy exited 0. The T4-5 review round 1 contract
requires non-zero.

Fix (`crates/via-cli/src/client.rs`). The stdin task records a failed read in
a shared slot before it shuts the write side. After the loop the proxy
checks that slot. The daemon's EOF that ends the stdout copy is caused by
that shutdown, so the failure is always visible by then. There is no new
dependency and no new wait. A proxy whose daemon ends first still exits at
once.

RED and GREEN come from the existing scenario under parallel stress. The
race window is a few instructions long, and making it deterministic would
need a new failpoint in the CLI, which the brief's simple-first rule
excludes:

- RED: 3 of 120 failed, all `a failed stdin read exited 0`
  (`repro/stdio-par-before/`).
- GREEN: 120 of 120 passed (`repro/stdio-par-after/`).

### 1.3 F5 filled 32 sockets before earlier permits returned (test ordering)

Test: `s1_f05_33rd_socket_is_closed_without_bytes`. Error:
`infrastructure_failure: Connection reset by peer`
(`scratchpad/t4/flake/after/run-10.log`).

Cause. `full()` opened its 32 sockets with plain `Conn::open` twice:

- right after the CLI's `spawn` and `wait` connections closed;
- right after the test dropped 33 connections.

A permit returns only when the daemon's task sees the close (design §10.1).
The test's own `retry_open` says so. Under load, one permit was still held,
so one of the 32 was the 33rd and got closed. The reset is the hello read on
a socket the daemon closed with unread input. The daemon's trace and cleanup
are clean.

Fix (test). `full()` opens each of the 32 with the existing `retry_open`
(bounded, 5 s), the same sync the test already uses for its refills. The
33rd socket is still opened with a single `Conn::connect` and must be closed
without bytes. No assertion changed.

- RED: 3 of 120 failed with the same reset (`repro/f05-par/`).
- GREEN: 120 of 120 passed (`repro/f05-par-after/`).

### 1.4 F24 stall: the first burst could overflow the Wire queue (test ordering)

Test: `s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output`.
Error: `no acknowledgement of core.observations.pause #2`
(`scratchpad/t4/flake/final/run-10.log`).

Cause. The fixture wrote `accepted`, the model output `first`, and at once
341 blocks of 3 messages. That is 1,025 messages, and the Wire queue holds
1,024 (A47). If Route had taken nothing yet (under load it was still busy
with the turn's start), the reader overflowed the queue. The turn then
failed `overflow` before Core handled a single observation, so pause #2 was
never hit.

Evidence [V]. All 5 failures in parallel stress (`repro/f24-par-before/`,
5 of 120) have events `turn.queued, turn.submitted, turn.ended` with no
`turn.started`, failure class `overflow`, and `duration_ms` 250-257. That is
below the 500 ms lowered stall, so it is the Wire queue and not the
Adapter's stall.

Fix (test). A fake gate `flood` sits between `first` and the first burst.
`held_turn` releases it only after the pause #2 acknowledgement. That
acknowledgement means Route took `accepted` and `first`, so the burst's
1,023 messages fit the queue however late Route reads. The design's step
arithmetic (1,023 items plus the held model output) is unchanged.

- RED: 5 of 120 failed.
- GREEN: 120 of 120 passed (`repro/f24-par-after/`).

### 1.5 Tests named without details

`s1_progress_snapshot_adds_no_store_read`,
`s1_c1_wait_checks_each_second_and_32_waiters_leave_status_served` and
`s1_progress_step_commit_refused_rows_ride_in_terminal` have no captured
logs. All three start through `support/daemon.rs` and depend on the daemon
under test (failpoints or its pid), so they are exposed to cause 1.1 [I].
They were not reproduced separately, and none failed in the 20-run record.

### 1.6 `s1_shutdown_budget_read_cutoff_before_reconciliation` (`via-jm4.15`): not the same cause

This test reproduces under parallel stress: 5 of 120 failed, all
`Error: "turn 1: cancelled requested quiescent"`, where
`cancelled forced quiescent` was expected (`repro/jm415-par-fp/`, and the
same text in `scratchpad/t4/t4-3/a47/nextest-fp.log` and
`t4/t4-4/r1-gate-nextest-fp.log`). `s1_lifecycle.rs` readiness never
auto-starts, and the daemon runs to its own summary. The difference is in
the product outcome: `stop_outcome` (`crates/via-core/src/engine/stop.rs`)
reports `forced` only with Host's evidence that its stop found the vendor
live. A plausible cause is an ordering between Route's own force stop and
Host's stop [I, not verified]. It is left for `via-jm4.15`, unchanged.

## 2. Round 2: the socket-guarded probes (same cause as 1.1)

`s1_crash_points.rs`, `s1_recovery.rs` and `s1_daemon_stop.rs` polled
readiness with `via daemon status` once `via.sock` existed. After a SIGKILL
restart the killed daemon's socket file remains. The guard then passes, the
connect is refused, and the CLI auto-starts a daemon without the failpoint
environment: cause 1.1 again (coordinator's finding; no failure was
observed here).

Fix. The shared harness now has `direct_status(runtime)` and
`serving_pid(runtime)` (`support/daemon.rs`), built on the existing `Raw`
direct connection (`Raw::open_at`). `Daemon::start_with` uses them, and the
three files include `support/daemon.rs` and use them:

- `wait_ready` (crash points, recovery) and `Daemon::start_with` (daemon
  stop) require `serving_pid == child pid`;
- `s1_daemon_stop.rs` `wait_latched` polls `direct_status` for
  `health: store_failed`. Its old exists-then-CLI probe could also
  auto-start if the socket was unlinked between the check and the connect.

No replaced probe relied on CLI auto-start. The other CLI calls in these
files (`spawn`, `wait`, `status`, and the `daemon status` evidence call in
`s1_f12_corrupt_frozen_row_fails_turn_on_restart`) are unchanged. They run
against a daemon already proven to be the child and do not rely on
auto-start.

### 2.1 Review round 1: the last CLI probes, and the audit

The reviewer found the problem. In `s1_prompt_to_result.rs` `start_daemon`
and `s1_turn_control.rs` `Sandbox::start`, the socket-exists guard still
passed a stale socket, or one removed before the CLI connected. The CLI
then auto-started a rival daemon. The pid match prevented false readiness
but not the rival. Both now use `daemon::serving_pid(runtime) == child pid`
and include `support/daemon.rs` (`s1_turn_control.rs` also includes
`support/scenario.rs` and `support/mod.rs`, which it needs).

Audit of `crates/via-cli/tests/` for auto-starting CLI calls that can run
before the harness's own daemon is proven serving:

- Every harness readiness path now uses a direct connection:
  - `support/daemon.rs`, `s1_prompt_to_result.rs`, `s1_turn_control.rs`,
    `s1_crash_points.rs`, `s1_recovery.rs` and `s1_daemon_stop.rs` use
    `serving_pid` with a pid match;
  - `s1_lifecycle.rs` and `s1_store_failure.rs` use their own `Raw`
    `daemon/status`;
  - `c1_protocol.rs` uses a raw connect.
- `Daemon::spawn` without readiness (`s1_recovery.rs`, `s1_crash_points.rs`)
  is followed only by failpoint acknowledgements, child exit or a later
  `wait_ready`, never by a CLI call.
- The other `via daemon status` calls either run after readiness against
  the proven child (`s1_turn_control.rs`, `s1_recovery.rs`, and the
  `daemon_pid` or `settled` loops in `s1_progress.rs`, `s1_c1_intake.rs`
  and `s1_vendor_pipeline.rs`) or test CLI auto-start or startup refusal
  on purpose, so they stay:
  - `s1_lifecycle.rs`: `s1_f01_concurrent_auto_start_one_daemon`, the F4
    version-mismatch `other_version` call, the late client after the idle
    exit, the unsafe runtime-directory variants, and
    `s1_silent_peer_before_hello_is_bounded_by_the_startup_budget`;
  - `c1_protocol.rs:408`: `c1_client_refuses_daemon_socket_of_another_uid`,
    which is root-only.

## 3. Verification

The 20-run record, on the final tree (`scratchpad/t4/flake/final2/`): 20 of
20 runs exited 0, and each ran `60 tests run: 60 passed, 443 skipped`. There
are 60 tests because the new regression test joins the selector.

History of the loops:

- Before any fix: 1 of 10 runs failed (`repro/before-*.log`). The historical
  logs had 4 of 10.
- After fix 1.1 only: 1 of 20 failed, on F5 (`after/`).
- After fixes 1.1 and 1.3: 2 of 20 failed, on stdio and F24 (`final/`).
- After all four fixes: 0 of 20 failed (`final2/`).

Gate G, all exit 0 (`scratchpad/t4/flake/gate.log`):

| Check | Result |
|---|---|
| `cargo fmt --all --check` | exit 0 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo nextest run --locked --workspace` | 321 passed, 1 skipped |
| `cargo deny check` | exit 0 |
| `python3 scripts/check-layers.py` | exit 0 |
| clippy, `--features via-cli/test-failpoints` | exit 0 |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints` | 502 passed, 1 skipped |
| `-E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 56 passed |
| `cargo build --locked --release -p via-cli --no-default-features` | exit 0 |
| `python3 scripts/check-release-features.py target/release/via` | exit 0 |

### 3.1 Round 2 verification

Logs are in `scratchpad/t4/flake/r2/`.

- The three changed files, `-E 'binary(s1_crash_points) | binary(s1_recovery)
  | binary(s1_daemon_stop)'`, 5 runs: every run had 57 tests and all 57
  passed.
- The selector, 10 runs: all 10 exited 0, each with 60 passed.
- Gate G, all exit 0 (`r2/gate.log`): the default suite ran 321 tests with
  321 passed and 1 skipped; the failpoint suite ran 502 with 502 passed and
  1 skipped; the `s1_f(08|09|10|12)_` selector ran 56 with 56 passed.

### 3.2 Review round 1 verification

Logs are in `scratchpad/t4/flake/r3/`.

- The changed files, `-E 'binary(s1_evidence) | binary(s1_prompt_to_result)
  | binary(s1_turn_control)'`, 5 runs: each had 42 tests and all 42 passed.
- The selector, 10 runs: each had 60 tests and all 60 passed.
- Gate G, all exit 0 (`r3/gate.log`): the default suite ran 321 tests with
  321 passed and 1 skipped; the failpoint suite ran 502 with 502 passed and
  1 skipped; the `s1_f(08|09|10|12)_` selector ran 56 with 56 passed.

## Round 3: two via-wire tests assumed the consumer runs first

Base: `wt/t4-flake` merged `rust-foundation` at `d636fc9`. The merged gate
ran the selector 5 times. One run failed two T4-3 tests
(`scratchpad/t4/merge-flake/gate.log`):

- `s1_wire_burst_of_1040_lines_reaches_a_live_consumer`: "the burst failed:
  Some(Message(Overflow))" after 8 ms;
- `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_messages`:
  `left: []` after 180 ms.

### Cause (verified)

Design §8.2 and T4-A47 are clear on both points. The reader never waits
for the consumer. A message past the 1,024-message or 4 MiB queue fails
`Reader(Overflow)`, and after that the reader discards. `next_message`
(`crates/via-wire/src/connection.rs`) returns a latched failure before
any message still queued, in two places: the check at its entry and the
check in `received`.

- The burst test wrote 1,040 lines in one write. When the reader queued
  1,025 before the consumer ran, it overflowed. That outcome is the
  designed one, and the test assumed it would not happen.
- The F27 test wrote 40 messages and then, at once, the over-cap line.
  When the reader reached the huge line before the consumer took the 40,
  the `MessageTooLarge` latch came first, which is also designed. The
  consumer then saw no message.

### Fix (test only, `crates/via-wire/tests/s1_wire.rs`)

Each test now syncs on a count the consumer publishes after every message
it takes (`tokio::sync::watch`), not on time:

- Burst: the first 1,024 lines are still one write. The queue holds them
  even before the consumer runs. The last 16 are written once the consumer
  took 16, so unconsumed messages never exceed 1,024. It still proves a
  flow larger than the queue reaches a live consumer whole and in order.
  `s1_wire_queue_holds_1024_messages_or_4_mib_then_overflows`, unchanged,
  still proves that 1,024 are held and the 1,025th overflows.
- F27: the huge line is written only once the consumer took all 40
  messages. The byte-exact, randomly chunked split through the reader,
  `MessageTooLarge`, the saved prefix, the naming and the discard to EOF
  are all asserted as before.

### Other tests checked for the same pattern

None other needed a change:

- via-wire:
  - the queue test is deliberately unconsumed;
  - the `finish` and deadline tests carry no message flow;
  - `contracts.rs` is splitter-only.
- `s1_f24_stall` was fixed in round 1 (gate `flood`).
- `s1_f27_daemon_split_writes_…` already waits for the running tool before
  it releases the huge line.
- `s1_f24_observation_budget_…` (via-core) delivers in admitted batches.
- In `s1_progress.rs`, three tests keep unconsumed messages at or below
  1,024 behind gates:
  - `s1_c1_status_latency_under_bounded_store_delay` (302 messages at
    most, even with none consumed);
  - `s1_progress_many_steps_all_have_rows` (300 per round, released after
    the step is seen);
  - `s1_progress_unknown_messages_send_no_observation` (1,000 per burst,
    after the pause acknowledgement and settled activity).
- In `route_drain.rs`, the oversize test expects the failure, and the
  stderr flood does not use the queue.

### RED and GREEN

Parallel stress: 24 concurrent loops of 10 runs of each test, run directly
from the `s1_wire` test binary (`scratchpad/t4/flake/r4/{before,after}/`).

| Test | Before | After |
|---|---|---|
| burst | 176 of 240 passed; 64 failed `the burst failed: Some(Message(Overflow))` | 240 of 240 passed |
| F27 | 235 of 240 passed; 5 failed `left: []` | 240 of 240 passed |

### Verification (`scratchpad/t4/flake/r4/`)

- `cargo nextest run --locked -p via-wire --features test-failpoints`, 20
  runs: every run had 10 tests and all 10 passed.
- The selector, 10 runs: every run had 60 tests and all 60 passed.
- Gate G, all exit 0 (`r4/gate.log`): the default suite ran 321 tests with
  321 passed and 1 skipped; the failpoint suite ran 502 with 502 passed and
  1 skipped; the `s1_f(08|09|10|12)_` selector ran 56 with 56 passed.
