# Task 4 epic critical review (Fable)

Subject: `git diff 76001d0..7790912` on `rust-foundation`, integrated result of T4-1..T4-7, T4-flake, T4-close. Read-only; no files other than this one were created, no Git or Beads state changed. Sources in the brief's precedence order; per-chunk Sol findings already resolved are not repeated.

## Verdict: UNSOUND (one important integration defect; the rest minor)

The seam between T4-7's `daemon/status` data walk and T4-2's shared, capped blob-step pool lets a slow State directory starve the pool that prompt copies, final-text writes and every free-space read depend on. Everything else I checked holds: the bounds of R7/§5 are met by construction where claimed, schema v6, the write order (final text synced before the terminal; lifecycle terminals on their lane), retry identity, `NotCommitted` vs latch, `create_new`/`NOFOLLOW`/0600 on every new file, the C1 members and error kinds, and the layer graph (`scripts/check-layers.py` passes).

## Findings

### Important

**F1. A data-size walk that overruns 2 s is relaunched by every `daemon/status` and each overrun occupies one of the 16 shared blob-step slots; the pool can fill and refuse turn-critical steps.**

- `crates/via-core/src/engine/status.rs:197-220` — `storage()` recomputes when the cache is absent or older than 60 s; on `Err` from `data_bytes()` nothing is cached ("a failed walk keeps the last one"), so the next call walks again. [verified]
- `crates/via-store/src/runtime.rs:1690-1707` — `data_bytes()` runs through `disk_step`, `run_until(now + 2 s)`; on overrun it returns `Err("disk step exceeded 2 s")` while the walk "stays owned until it ends" inside the `BlobTasks` `JoinSet`. [verified]
- `crates/via-store/src/blob.rs:119,142-156` — `BLOB_TASKS = 16`; `admit` counts unfinished steps and refuses at the cap at once. [verified]
- `crates/via-cli/src/client.rs:623` — every CLI command (`spawn`, `wait`, `result`, ...) issues `daemon/status` after `hello`, so `storage()` runs once per CLI invocation, not only on explicit `via daemon status`. [verified]
- `crates/via-store/src/runtime/disk.rs:157-215` — the walk covers every file under `evidence/` (one folder per turn, three files each) and `blobs/`; nothing prunes it in this task. [verified]
- Consumers of the same pool: `FinalTextFile::create/append/finish` (`final_text.rs:129-142`: a refused step sets `failed` and the turn ends `failed(store)`), `blob_writer`/`copy_prompt_file` (`receipt.rs:74,95`), the `cwd` check (`receipt.rs:149`, `not_committed`), and `free_bytes` for every receipt (`receipt.rs:185,335` then `status.rs:253` → `RECEIPT_NOT_COMMITTED`) and every dispatch (`drive.rs:376`, `status.rs:268` → the queued turn fails `store`, `FREE_UNREAD`). [verified]

Failure scenario (walk time above 2 s is inferred; the chain is verified): a State directory with tens of thousands of turn folders on a slow or cold disk makes the walk take W > 2 s. With a monitor polling `daemon status` or a user issuing CLI commands every few seconds, each call blocks 2 s behind the `data_size` mutex, reports `data_bytes: null`, and leaves one more walk running; about W/interval walks run concurrently and contend for the same disk. Once 16 are outstanding, `admit` refuses every blob step: spawns and resumes get `store_error not_committed` (free-space read refused), queued turns fail `store` at dispatch, a running turn whose final text spills fails `store`, prompt files cannot be copied. Nothing latches, so the daemon stays up in this state until the walks finish.

Smallest fix: in `Engine::storage`, stamp the cache on failure too so at most one walk starts per `DATA_SIZE_TTL` regardless of outcome (make `DataSize.bytes: Option<u64>`, set `at`/`measured_at` on `Err`, keep `data_bytes: null`). A one-struct change; `s1_store_data_size_warning_is_cached` should gain a case with `blob.step.stall` armed on the walk asserting `core.data_size.walks` is hit once across two calls.

### Minor

**F2. Wall-clock assertions outside F24, the class A50 removed.** Design A50 (`design.md:1699-1707`): a bound of 100 ms failed at 181 ms under a loaded parallel run without any lock being held; "does not block" is proven by order and latencies are recorded as evidence.

- `crates/via-cli/tests/s1_progress.rs:624-630` — `took < 300 ms` per `status` call while `store.read.delay_ms` injects a 200 ms read delay and the fake floods 300 lines per round: 100 ms of slack, the same margin A50 rejected. [verified] Scenario: a loaded nextest run delays one reply by 120 ms and the test fails without any second Store read having happened. Fix: prove "one read" the way `s1_progress_snapshot_adds_no_store_read` (`s1_progress.rs:565-577`) does, by `hits::hits(READ_DELAY)` advancing by exactly 1 per call, and write `took` to evidence.
- `crates/via-cli/tests/s1_evidence.rs:203-219` — `ordered_after < 2200 ms` measured from the test's own clock across `await_gate`, a 800 ms sleep and a 10 ms `events` poll loop; the buggy case (stderr resets idle) starts at 2300 ms, so the test discriminates by 100 ms and tolerates about 600 ms of harness delay on the passing side. [verified] Fix: compare the daemon's own timestamps (`cancel.requested.at` minus `turn.started.at` < idle budget + slack) or release the stderr gate later so the two hypotheses are seconds apart; record `ordered_after` as evidence.
- `crates/via-cli/tests/s1_c1_intake.rs:853-860` — `seen < 3 s` for 31 waiters after the gate release; loose, but the same class. [verified] Fix: record only, or assert the per-second cadence through read counts as the first half of the test already does.

**F3. Three `spawn_blocking` calls outside any owner, against coding-style §5 (`.repo-context/coding-style.md:102-109`: "bound how much blocking work is admitted and keep ownership until it completes").**

- `crates/via-wire/src/connection.rs:198-203` — `keep_undecoded` spawns `write_new` raw and drops the `JoinHandle` when the 2 s timeout fires: the write then runs unowned and is counted nowhere in the shutdown summary. [verified]
- `crates/via-wire/src/runtime.rs:70-73` — `create_turn` spawned raw and awaited with no bound; if the dispatcher is aborted at `join_dispatchers` (`shutdown.rs:266-269`) while it waits here, the mkdir/fsync work continues unowned. [verified]
- `crates/via-core/src/engine/read.rs:260-263` — `logs` spawns `evidence_files` raw; a client task aborted at `join_clients` leaves the `lstat`s unowned. [verified]

Scenario: a stalled filesystem at shutdown; the summary says `clean`/`blob_tasks: 0` while blocking threads are still in these calls. Bounded in practice (one write per connection, one folder per turn, one `logs` per socket), so minor. Fix: run them through the owned pool the Store already exposes (`StoreClient::blocking_step`, or hand Wire a `BlobTasks` clone in `RuntimeResources` next to `EvidenceRoot`), which also gives them the 2 s bound design §7.2/§7.3 states.

**F4. `wait` catches up after a slow read instead of keeping a 1 s cadence.** `crates/via-core/src/engine/read.rs:128,153-154` — `check_at` advances from the loop's start; `sleep_until(deadline.min(check_at))` returns at once while `check_at` is in the past. [verified] Scenario: the Public lane answers a facts read after 5 s (lane full, slow disk); the waiter then issues five back-to-back reads, and 32 waiters 160, exactly when the Store is slow, contrary to design §4.1's "32 reads per second". Fix: `check_at = tokio::time::Instant::now() + WAIT_CHECK`.

**F5. Duplication.** The test-build environment override is written five times (`dispatch.rs:64-73`, `serving.rs:67-73`, `final_text.rs:28-34`, `via-adapters/src/runtime.rs:33-39`, `resolve.rs:45-50`); `sleep_until_some` (`drive.rs:1881`) and `sleep_until` (`serving.rs:312`) are the same function. [verified] Fix: one `via_store::failpoint::override_ms(name, default)` (the feature-gated module already exists) and one shared optional-sleep helper.

## Measurement suggestions

- Time `disk::data_bytes` on a State directory with 10^4 and 10^5 turn folders (SSD warm, SSD cold, HDD) to decide whether F1's 2 s bound is the right one or whether the walk should be incremental (`via-d9o.2.3` already routes measurement items).
- `receipt.rs:169-201`: `stage_prompt` (a copy of up to 16 MiB) runs before the lock-free refusals that need no staging (`harness`, `model`, `store_failed`, an empty prompt). Count refused receipts that carried a `prompt_file` before deciding to reorder.
- Record how often `Wal::committed`'s per-commit `stat` of the WAL file shows up in the writer thread's profile (`disk.rs:115-126`); it is one syscall per commit, likely negligible.

## Could not verify

- I did not run the test suite, the Task 4 selector or `check-release-features.py`; findings rest on reading the code and the reports' recorded runs. I ran only `scripts/check-layers.py` (passes).
- The actual wall time of the evidence walk on large trees (F1's trigger) is inferred, not measured.
- via-host beyond the Task 4 diff (`stderr_path`, `live_armed`), the Store's v5→v6 migration path, `json_limits::scan`'s equivalence beyond `s1_bounds_json_limits_agree_with_serde_json`, and the internals of `s1_c1_reads.rs`, `s1_blob_prompt.rs` and the via-store `s1_*` tests were skimmed for structure, not read line by line.
- Whether every design §13 row has a shipped test with the evidence the verification rule requires: T4-close's mapping was taken as given (out of scope per the brief for the Task 2-3 files and the `s1_evidence_readiness` artifact).
- The forced turn's partially written `final_text.txt` left unreferenced in the evidence folder is recorded in T4-6 (`reports/T4-6.md:166-167`) and not re-raised.
