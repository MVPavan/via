# T4-fix report: the confirmed Task 4 critic findings

Bead `via-jm4.7.8.12`. Branch `wt/t4-fix`, cut from `rust-foundation` at
`178e41a`. Logs are under `scratchpad/t4/t4-fix/` (gitignored). Sources:
the Astra and Fable Task 4 critic reviews; design §4.1, §5.3, §5.5, §7.2,
§7.3, §7.6, §12.2 A50, §15; coding-style §5 and §10.

## Status

DONE_WITH_CONCERNS. All seven fixes are in, each with a test written
first, RED recorded, then GREEN. Gate G, the failpoint suite 3 times and
the Task 4 selector 5 times are green. The concerns are listed in the
Concerns section: a new test-only failpoint, and one fix-3 commit that
fails clippy on its own.

## Commits

| Commit | Fix | Summary |
|---|---|---|
| `9bbf73c` | 1 | cache a failed or overrun data-size walk as `null` for its minute; `store.data_size.walk` point |
| `f409ec8` | 2 | undecoded write, folder creation and `logs` `lstat`s run as owned Store blob steps |
| `d1bfa42` | 3 | a spawn's `cwd` check is applied only after its key lookup |
| `f81409d` | 3 | the keyed replay moves into `spawn_replay` (line limit; behaviour unchanged) |
| `94e59dd` | 4 | a FIFO `daemon.json` or `via.log` is refused at once |
| `31c2634` | 5 | `wait` schedules its next check a second after its last read |
| `e3299fd` | 6 | two wall-clock bounds replaced with count and order proofs |
| `ced6296` | 7 | the readiness scenario finalizes its evidence and declares no Store |
| this commit | — | this report |

## Per fix

### 1. Data-size walk overrun (Astra 5, Fable F1)

- **Defect.** `Engine::storage` cached nothing when `data_bytes()` failed
  or overran its 2 s step. Every `daemon/status` started another walk, and
  every CLI command sends one. Each overrun walk kept one of the 16 blob-step
  slots.
- **Change.** `crates/via-core/src/engine/status.rs`: `DataSize.bytes`
  is `Option<u64>`, and the cache is stamped whatever the outcome.
  - A failed or overrun walk reports `data_bytes`, `data_measured_at` and
    `over_warn_size` as `null` until the next walk, a minute later
    (design §15 row).
  - There was no seam to hold a walk: `blob.step.stall` is hit only in
    `BlobTasks::run`, and `disk_step` uses `run_until`. So I added the
    test-only point `store.data_size.walk` in `disk::data_bytes`. It is
    listed in `scripts/check-release-features.py` `POINTS`, which now arms
    105 points.
- **Test.** New `s1_store_data_size_overrun_walks_once`
  (`crates/via-cli/tests/s1_daemon_config.rs`). With the walk paused,
  the CLI runs spawn, wait and `daemon status` twice. The test asserts that
  `core.data_size.walks` is hit exactly once, and that both status replies
  have `data_bytes`, `data_measured_at` and `over_warn_size` `null` and a
  positive `free_bytes`.
  - RED: `fail: 2 walks within 60 s`.
  - GREEN: 2/2 `s1_store_data_size*` passed.

### 2. Unowned `spawn_blocking` calls (Astra 1, Fable F3)

- **Defect.** Three `spawn_blocking` calls had no owner:
  - `keep_undecoded` dropped its handle when its 2 s timeout fired;
  - `create_turn` was awaited with no bound, and was lost if the dispatcher
    was aborted;
  - the `lstat`s in `logs` were lost if the client task was aborted.

  None of them appeared in the shutdown summary.
- **Change.** All three now run through the Store's existing capped
  `BlobTasks` via `BlobTasks::run`, which gives the 2 s bound and keeps the
  step owned until it ends. `blob_tasks` in the shutdown summary counts
  them.
  - `EvidenceRoot` now carries the Store's `BlobTasks`, the same `Arc` as
    `Store::blob_tasks`, and exposes it as `blob_tasks()`. It reaches Wire
    through the existing `RuntimeResources`. There is no new dependency, and
    the layer graph passes (via-wire → via-store).
  - `BlobTasks::run` became `pub`.
  - Wire (`crates/via-wire/src/runtime.rs`, `connection.rs`): the folder
    creation failure path is `WireError::Evidence` (turn fails `store`
    before launch). The undecoded failure path is the note "…; not saved:
    <error>". The unused `UNDECODED_WRITE` constant was removed.
  - `logs` (`crates/via-core/src/engine/read.rs`) uses
    `StoreClient::blocking_step` and maps a refusal to `ApiError::STORE`,
    as before.
  - Test pipes (`via_wire::testing`) own a default `BlobTasks` and expose
    `TestInput::blob_tasks()`.
- **Tests.** One per site, each holding the step with `blob.step.stall`:
  - `s1_wire_held_undecoded_write_stays_owned` (`crates/via-wire/tests/s1_wire.rs`):
    a 1 MiB + 1 line. The note says "not saved" after the 2 s bound;
    `blob_tasks() == 1` while the step is held; it is reaped after release.
    - RED (old body): the write was not held and the note named the
      saved file.
    - GREEN: 11/11 via-wire tests.
  - `s1_blob_stalled_evidence_folder_is_owned_until_shutdown`
    (`crates/via-cli/tests/s1_daemon_stop.rs`): a second turn's folder
    creation is held. The turn ends `failed` with class `store`. The idle
    stop's summary has `blob_tasks` 1, `pending_joins` 1 and `incomplete`,
    and the daemon exits 4.
    - RED: the turn completed.
  - `s1_blob_stalled_logs_step_is_owned_until_shutdown` (same file): a held
    `logs` answers `store_error`, and the summary counts the step as above.
    - RED: `logs` answered normally.
  - GREEN: 3/3 (with the updated test below).
  - The cancelled-caller case (aborted dispatcher or client task) is not
    forced separately. It would need an abort seam at shutdown. The owned
    step is the same `JoinSet` entry either way, so the overrun tests show
    the step is counted.

### 3. Keyed spawn replay refused once its `cwd` is gone (Astra 3)

- **Defect.** `Engine::spawn` ran `session_cwd(...).await?` before the
  idempotency lookup.
- **Change.** `crates/via-core/src/engine/receipt.rs`: the `cwd` check
  still runs outside the admission lock. Its `Result` is carried into
  `spawn_admitted` and applied (`cwd?`) only after the key lookup finds
  nothing, just before the floor, as `free` is.
  - The replay moved into `spawn_replay` to keep `spawn_admitted` under the
    100-line lint.
  - `resume` has no `cwd` check. Its only check before the key lookup that
    is not about the store's state is the empty-prompt check, and an
    identical request repeats that check with the same result. The ordering
    is correct.
- **Test.** New `s1_c1_keyed_spawn_replays_after_its_cwd_is_removed`
  (`crates/via-cli/tests/s1_c1_intake.rs`):
  1. a keyed spawn in a temporary `cwd` runs to `completed`;
  2. the directory is removed;
  3. the identical request returns a `result` equal to the first one;
  4. an unkeyed spawn gets `-32602 invalid_params` with `field: cwd`.
  - RED: the replay got `invalid_params` `cwd is not an existing directory`.
  - GREEN: 2/2, with `s1_c1_cwd_is_frozen_applied_and_reported`.

### 4. FIFO `daemon.json` or `via.log` hangs startup (Astra 4)

- **Change.**
  - `config::read` opens with `O_NOFOLLOW | O_NONBLOCK`. The existing
    `File::metadata` check then refuses the FIFO with "must be a regular
    file", which exits 78 before any change.
  - `log::open`: the rotate step's existing `symlink_metadata` now refuses
    any existing entry that is not a regular file.
    - Reason: `O_WRONLY | O_NONBLOCK` on a FIFO with no reader fails with
      `ENXIO`, whose text does not name the problem.
    - The open then uses `O_NOFOLLOW | O_NONBLOCK`, and the opened
      descriptor is checked with `File::metadata`.
    - The rotate-then-open order is kept.
  - `server.rs` formats the error as `open via.log: <cause>`, so the
    message reaches stderr. Before, `.context` showed only
    "open via.log".
- **Test.** New `s1_config_and_daemon_log_fifo_are_refused_at_once`
  (`crates/via-cli/tests/s1_daemon_config.rs`). It uses fresh sandboxes
  and a 10 s bound on each start:
  - a `daemon.json` FIFO exits 78 with
    `via: daemon config invalid: daemon.json: must be a regular file`;
  - a `via.log` FIFO exits 4 with `open via.log: must be a regular file`;
  - neither creates `store.sqlite3*` or a socket. `store.lock` precedes
    `via.log` by design (§7.6).
  - RED: `a daemon.json FIFO hung the start` (10 s). With the config fixed,
    `a via.log FIFO hung the start` (10 s).
  - GREEN: 3/3 `s1_config_|s1_daemon_log_`.

### 5. `wait` catches up after a slow read (Fable F4)

- **Change.** `crates/via-core/src/engine/read.rs`: the next check is
  `Instant::now() + WAIT_CHECK`, taken after each read.
- **Test.** New `s1_c1_wait_after_a_slow_read_keeps_its_cadence`
  (`s1_c1_intake.rs`). Every Store read from the waiter's first read on is
  delayed 800 ms (`delay_persist:800`) and counted by its acknowledgements.
  - A 5 s wait may make at most 4 reads: the existence read plus
    facts reads at about 0, 2.6 and 4.4 s.
  - The catch-up schedule makes 7.
  - The bound is an upper bound: slower reads only lower the count, so it
    holds under load.
  - RED: `made 7 Store reads, at most 4`.
  - GREEN: 4 reads. `s1_c1_wait_checks_each_second_…` also passes.

### 6. Wall-clock assertions (Fable F2; A50)

- `s1_c1_status_latency_under_bounded_store_delay` (`s1_progress.rs`):
  - The `took < 300 ms` bound is replaced by a count: each `status` call
    adds exactly one acknowledged `store.read.delay_ms` hit.
  - A new `acked` helper counts the acknowledgement files, because the
    point is armed at occurrence 1 after earlier unacknowledged hits.
  - Each call's `took_ms` and read count go to `status_latency.json`. In
    the run: 9 calls, 1 read each, about 204 ms each.
- The stderr-idle part of `s1_evidence_stderr_is_written_by_the_os_and_listed`:
  - The `ordered_after < 2200 ms` bound is replaced by the daemon's and the
    OS's own timestamps, compared with the 1500 ms idle budget:
    - `turn.started.at` < `stderr.log` mtime < `cancel.requested.at`: the
      bytes landed inside the window;
    - `cancel.requested.at − mtime < 1500 ms`: a reset by the bytes would
      put the order a whole budget after them.
  - `idle_timing.json` records the timings together with the test-clock
    `ordered_after`. In three runs: started to order 1501 ms, stderr to
    order 689–690 ms.
  - An RFC 3339 parser (`unix_ms`) was added to the test file, because
    via-cli has no date dependency.
- `s1_c1_intake.rs` `seen < 3 s` is unchanged, as the brief says.

### 7. Close finding 1: the readiness scenario's evidence

- **Change.**
  - `support/evidence.rs` gains `Evidence.store_expected` (default `true`).
    When a scenario clears it:
    - `finish` requires only `daemon.trace` and `cleanup.json`, and skips
      the `evidence/*` check;
    - the summary adds `"store_expected": false`.

    It is a field, not a method: `support/evidence.rs` is compiled into
    every test binary, so a method used in one would be dead code in the
    others, and the lints refuse `allow`.
  - `s1_evidence_harness_readiness_never_starts_a_daemon` now runs through
    `run_scenario` and clears the flag. It writes the following, and keeps
    its assertion unchanged:
    - `readiness.json`: the readiness message, the child's served pid, the
      sandbox socket and Store flags, and the stop probe's exit;
    - `daemon_shutdown.json`: the child's summary.
- **Evidence.** The artifact's summary is `outcome: pass`,
  `evidence_complete: true`, `missing_evidence: []`,
  `store_expected: false`. It used to be
  `infrastructure_failure: scenario did not finalize evidence`.
- The rule is unchanged for every other scenario: the flag defaults to
  `true`, and the test that clears it is the only use.

## Updated tests

- `s1_daemon_stop_stalled_blob_step_is_not_a_clean_exit`: arms
  `blob.step.stall` occurrence 2, not 1.
  - Reason: the small turn's folder creation is now an owned blob step and
    the run's first hit, so occurrence 1 would hold that turn instead of
    the large-prompt blob step.
  - Its assertions are unchanged.
- `s1_c1_status_latency_under_bounded_store_delay`: the latency bound is
  replaced by a one-read-per-call proof, and latency is recorded (fix 6).
- `s1_evidence_stderr_is_written_by_the_os_and_listed`: the test-clock
  bound is replaced by daemon and OS timestamps (fix 6).
- `s1_evidence_harness_readiness_never_starts_a_daemon`: now finalized
  with a no-Store declaration (fix 7).
- `s1_daemon_config.rs` loses `#[expect(dead_code)]` on its `failpoints`
  module, because the new test uses the rest of it.

## Gate counts

Log: `scratchpad/t4/t4-fix/gate.log`, `gate exit 0`.

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy … -D warnings` | pass |
| `cargo nextest run --locked --workspace` | 330 passed, 1 skipped |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | pass |
| `cargo clippy … --features via-cli/test-failpoints -D warnings` | pass |
| failpoint suite, run 1 | 528 passed, 1 skipped |
| failpoint suite, runs 2 and 3 (`failpoint-suite-2-3.log`) | 528 passed, 1 skipped, each |
| `s1_f(08|09|10|12)_` | 56 passed |
| release build and `check-release-features.py` | pass; 105 points armed, ignored |
| Task 4 selector ×5 | 84 passed each run (29.5–30.3 s) |

## Deviations

- **New failpoint `store.data_size.walk`** (fix 1).
  - The brief named `blob.step.stall` or an existing seam, but neither can
    hold the walk: `disk_step` goes through `run_until`, which does not hit
    `blob.step.stall`.
  - Putting `blob.step.stall` into `run_until` would have shifted every
    existing occurrence, because each CLI command makes a free-space read.
  - Design §13.1's failpoint list does not name the new point. I did not
    edit `design.md`.
- **Files outside the named list:**
  - `crates/via-store/src/{blob.rs, evidence.rs, runtime.rs}` and
    `crates/via-store/src/runtime/disk.rs`: the ownership plumbing and the
    seam;
  - `crates/via-cli/src/server.rs`: the `via.log` error text;
  - `scripts/check-release-features.py`: the `POINTS` entry;
  - `crates/via-cli/tests/support/evidence.rs`: fix 7;
  - test files `s1_c1_intake.rs`, `s1_daemon_stop.rs` and
    `crates/via-wire/tests/s1_wire.rs`: the new tests.
- **`via.log` check order** (fix 4): the entry's type is also checked
  before the open, from the existing `symlink_metadata`. Only that check
  gives a clear message; the open on a FIFO with no reader returns
  `ENXIO`. A symlinked `via.log` now reads "must be a regular file"
  instead of ELOOP.

## Concerns

- `d1bfa42` fails `clippy::too_many_lines` on its own (`spawn_admitted`
  101/100). It builds and its tests pass; the next commit, `f81409d`,
  fixes the lint. I did not amend, because amending was not authorized.
- The undecoded write, folder creation and `logs` now share the 16-slot
  blob pool with prompt copies and final-text steps. A stalled filesystem
  can therefore refuse these as well, which is the intended bounded
  behaviour. Two note texts changed:
  - the undecoded note on overrun now reads "not saved: blob I/O exceeded
    2 s";
  - `WireError::Evidence` wraps the Store error text.

## Round 1

Sol high r1 returned UNSOUND with one important finding
(`scratchpad/execution/t4-impl/review-t4-fix-sol-r1.md`). The
orchestrator's commit `3cf689d`, which lists the `store.data_size.walk`
seam in design §13.1, is kept. Logs: `scratchpad/t4/t4-fix/r1-*.log`.

### Finding: `logs` shared the 16-slot blob-step pool

- **Defect.** After fix 2, `logs` file checks ran as Store blob steps.
  Sixteen concurrent stalled `logs` calls could fill the pool, so a prompt
  write, a final-text step or a turn-folder creation would be refused.
- **Change** (`05b40a3`), as the orchestrator decided:
  - The Engine (`crates/via-core/src/engine.rs`) holds
    `diagnostics: Arc<tokio::sync::Semaphore>` with
    `DIAGNOSTIC_STEPS = 2` permits, shared by the `logs` file checks and the
    data-size walk.
  - Each step takes a permit with `try_acquire_owned` before the step is
    admitted. The permit moves into the blocking closure, so it is released
    only when the blocking work ends, even after the 2 s timeout. It is also
    released if the Store refuses the step at its cap.
  - `logs` with no permit returns `ApiError::STORE`: `-32018`
    `store_error`, "durable storage failed". That is the same error `logs`
    returns when the Store refuses or fails its step.
  - The walk with no permit counts as a failed walk: it is cached as `null`
    for its minute.
    - `StoreClient::data_bytes` now takes a `held: impl Send + 'static`
      value, which it drops when the walk ends. This is how the permit moves
      into the Store's closure.
  - Diagnostics can therefore hold at most 2 of the 16 slots. There is no
    new dependency, and the layer graph is unchanged.
- **Test.** New `s1_blob_stalled_logs_are_capped_and_turns_still_start`
  (`crates/via-cli/tests/s1_daemon_stop.rs`):
  1. After one small turn, whose folder creation is blob step 1, two `logs`
     calls are held with `blob.step.stall` at occurrences 2 and 3. Each
     answers `store_error` at its bound.
  2. A third `logs` is sent with occurrence 4 armed. It must answer
     `store_error` without reaching a blob step, so no occurrence-4
     acknowledgement may exist. This proves the refusal by order, not by
     time.
  3. With the two steps still held, a new foreground turn creates its
     folder and completes.
  4. After release, `daemon/stop` exits 0 with `blob_tasks` 0.
  - RED (before the fix): `a third logs reached a blob step (true)`, so
    there was no cap.
  - GREEN: 7/7 of `s1_blob_stalled|s1_store_data_size|logs`.
  - This is the smallest form that fails today, as the orchestrator
    allowed. It shows that the cap exists and that turn work proceeds. It
    does not fill all 16 slots.
- **Not separately tested:** the walk with no permit. It uses the same
  semaphore, and its outcome is the already-tested failed-walk path
  (`s1_store_data_size_overrun_walks_once`).

### Minor finding

`d1bfa42` fails clippy on its own. As instructed, the history is not
rewritten; the orchestrator records it in the merge message.

### Round 1 gate counts

Log: `r1-gate.log`, `gate exit 0`.

| Check | Result |
|---|---|
| fmt, both clippy runs, deny, layers | pass |
| `cargo nextest run --locked --workspace` | 330 passed, 1 skipped |
| failpoint suite, run 1 (gate) | 529 passed, 1 skipped |
| failpoint suite, run 2 (`r1-failpoint-2.log`) | 529 passed, 1 skipped |
| `s1_f(08|09|10|12)_` | 56 passed |
| release build and `check-release-features.py` | pass; 105 points armed, ignored |
| Task 4 selector ×3 | 85 passed each time |
