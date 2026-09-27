# Task 1 closeout: Sol high findings 1–3

Findings are from `../task1-sol-high-review.md`, reviewed at `3b07b83`. Finding 4
(session closure after drain and force) is deferred to `via-jm4.7.7` and is not
touched here. Each fix follows the orchestrator's decisions. Regressions were
run failing first, except where noted.

## Finding 1: force disposition from evidence

**Failure.** Every forced turn committed `cancelled`. `stop_outcome` turned
Host's positive force evidence into `requested` whenever cleanup was
uncertain. Host shutdown could spend the whole shared deadline proving
absence, leaving no time to commit forced turns' terminals.

**Regressions.** In `crates/via-core/tests/force_stop.rs`, Engine over a real
Store and Host, with the scripted stand-in anchor in
`crates/via-core/tests/support/stand_in_anchor.rs`. The stand-in speaks the
real control protocol and reports chosen stop evidence. Each test forces once
a barrier flag proves the launch was confirmed, then reads the durable
result.

- `proved_stop_without_proved_absence_is_forced_uncertain`. The anchor
  reports `stopped_live: true`, then ignores TERM and lingers. The test
  expects `cancelled`, `forced`, `uncertain`, the warning
  `cancel_cleanup_uncertain`, and `cancel.settled` showing `forced`. Before
  the fix, Host shutdown consumed the whole deadline and the terminal never
  committed:

  ```
  EngineShutdown { .., uncertain_owners: 1, .., failure: Some("Host deadline expired"),
  uncommitted_turns: 1, unresolved_turns: 1 }
  ```

- `launched_turn_without_proved_stop_is_unknown`. The anchor reports
  `stopped_live: false` and exits, so absence is proved. The test expects
  `unknown`, no failure, `requested`, `quiescent`. Before the fix:

  ```
  left: String("cancelled")  right: "unknown"
  ```

**Fix** (`crates/via-core/src/engine.rs`):

- `stop_outcome` keeps outcome and cleanup independent: `forced` only from
  Host's live-stop evidence, `quiescent` only with proved absence.
- Forced-turn settlement:
  - Host proved a live stop → `cancelled`/`forced`, with cleanup as
    evidenced.
  - No vendor could have launched, because ARM was never sent → `cancelled`/
    `requested`. Cleanup is `quiescent` if the journal is complete and no
    intent exists; otherwise it is as proved.
  - Otherwise → `unknown`, `stop_reason: error`, `requested`.
- "ARM was never sent" comes from a new `RouteFailure::launched` flag. Route
  sets it once a connection existed or Wire reports `AfterLaunch`.
- Host shutdown now gets the final deadline minus `FORCED_COMMIT_RESERVE`
  (1 s), so terminals can still commit.

## Finding 2: post-ARM acquisition errors

**Failure.** Only the force-timeout branch used the ARM fact. Any other
post-ARM Host error, such as a lost spawn reply, a failed vendor-facts commit
or the acquisition deadline, dropped the vendor pipes unrecorded with
`raw_incomplete: false`. `HostError::Deadline` became transport loss
(`unknown`).

**Regression.** `post_arm_acquisition_deadline_keeps_cause_and_vendor_output`
in `crates/via-core/tests/route_stream.rs`, through the real Adapter, Route,
Wire and Host, with no force. A stand-in anchor answers ARM by launching a
vendor that writes a line and sets a `wrote` flag, then never confirms. The
acquisition deadline expires after 3 s. The test expects cause `Deadline`,
`raw_incomplete: false`, and the line in the raw log. Before the fix:

```
RouteFailure { cause: TransportLost { .. }, evidence: None, exit: None,
raw_incomplete: false, cleanup: None, forced: false }
```

**Fix (first preference: retain and drain).**

- Host: `acquire_retaining` moves the vendor pipes into a caller-owned
  `LaunchPipes` slot just before sending ARM, and takes them back on success.
  `acquire` delegates to it.
- Wire: on any acquisition failure or abandonment, Wire drops the acquisition,
  which closes the anchor control so the anchor stops its group. It then
  takes the pipes and drains both through its raw writer (`drain_pipes`,
  bounded to 3 s). It returns `WireError::AfterLaunch { cause, raw }`, where
  `raw` is `Complete` only if both pipes reached EOF and every chunk was
  appended.
- Route takes the cause from `cause`: `HostError::Deadline` maps to
  `Deadline`, so the turn fails `deadline_wall`. `raw_incomplete` now comes
  from the drain result.

Host always holds the pipes from anchor spawn, which is before ARM, so they
are reachable on every post-ARM path. No `raw_log_unverified` warning was
needed. The round-3 `CancelledAfterLaunch` "possibly lost" flag is removed.

The round-3 force test is replaced by
`force_after_arm_abandonment_drains_vendor_output`. It forces only after the
stand-in's `wrote` barrier, which proves ARM happened and the vendor wrote,
replacing the 1 s sleep. It requires the line in the raw log, no
`raw_log.incomplete`, and state `unknown`, since a vendor launched with
neither stop nor terminal proved. The earlier stalled-before-`Ready` test
still ends `cancelled`/`requested`/`uncertain`: ARM was never sent.

## Finding 3: peer UID acceptance evidence

This VM runs as root (`id -u` = 0). The ignored end-to-end test passes:

```
$ cargo nextest run --locked --workspace --run-ignored only \
    -E 'test(c1_client_refuses_daemon_socket_of_another_uid)'
    Starting 1 test across 25 binaries (120 tests skipped)
     Summary [   0.016s] 1 test run: 1 passed, 120 skipped
```

**Mutation, reverted.** I replaced `request()`'s
`verified_peer(stream, ..)?` in `crates/via-cli/src/client.rs` with the
unchecked stream. The same test then failed:

```
assertion `left == right` failed: client sent 113 protocol bytes to a foreign-uid peer
  left: 113
 right: 0
```

After restoring the file (no diff), it passes again.

**Flag note.** `.repo-context/verification.md` now has the root/CI line. It
uses `--run-ignored only`: this nextest (0.9.146) rejects `ignored` (the
accepted values are `default`, `only` and `all`).

## Files changed

- `crates/via-core/src/engine.rs`: owned Core stop/force.
- `crates/via-host/src/{host,lib}.rs`: `LaunchPipes` and
  `acquire_retaining`.
- `crates/via-wire/src/{runtime,lib}.rs` (shared): `AfterLaunch`,
  `drain_pipes`, and the `HostError` re-export.
- `crates/via-routes/src/{lib,runtime}.rs` (shared): `RouteFailure::launched`
  and the cause mapping.
- `crates/via-adapters/src/runtime.rs` (shared): sets the new field.
- Tests: `crates/via-core/tests/{force_stop,route_stream}.rs` and
  `crates/via-core/tests/support/stand_in_anchor.rs`.
- `.repo-context/verification.md`: the root/CI line.

## Gate

Toolchain 1.98.1, `XDG_RUNTIME_DIR` set to a private 0700 directory, running
as root.

- `cargo fmt --all --check`: pass.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: pass.
- `cargo nextest run --locked --workspace`: 119 passed, 2 skipped.
- `cargo deny check`: ok.
- `python3 scripts/check-layers.py`: ok.
- Skill catalog: 0 FAIL.
- Core and Host suites were rerun twice with no failure.

## Open

- **Finding 4** is deferred to `via-jm4.7.7`.
- **Contract text.** C1 and runtime text for the `unknown`-after-force row
  is being recorded by the orchestrator. The runtime §6 force bullet I wrote
  in W4-H does not yet mention the `unknown` case.
- **Failing-first evidence is partly mutation-based.** Finding 1's
  forced/uncertain regression failed first through the deadline-reserve
  defect, not the outcome mapping; the mapping was then checked by the fixed
  run. Finding 3's regression evidence is a reverted mutation.
- **Earlier deferred item.** The drained-pipe path cannot prove bytes still
  held by a stalled writer past its 3 s bound; it reports them
  `raw_log_incomplete`, as Route's failure drain does.
