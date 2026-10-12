**UNSOUND**

### Findings

- **Important — invalid config values can start the daemon.** `crates/via-cli/src/server/config.rs:61` deserializes optional numeric keys so an explicit `null` is treated as absent. For example, `{"disk":{"free_floor":null}}` silently uses the default instead of exiting 78 before side effects, contrary to design §5.5. The same issue affects the other numeric keys and top-level `disk`/`wal`. Preserve whether each key was present, then reject `null` with its key and rule.

- **Important — an oversized WAL is not refused immediately after restart.** `crates/via-store/src/runtime.rs:1410` initializes `wal_full` to false; `crates/via-store/src/runtime/disk.rs:79` checks the file length only after a mutation. If a reader holds an oversized WAL across a daemon restart, the first new receipt can commit despite `wal.max`. Check the existing WAL before admitting the first mutation, attempting `TRUNCATE` and refusing new work if it remains at the limit.

- **Minor — new scenarios inherit an auto-starting readiness probe.** `crates/via-cli/tests/s1_daemon_config.rs:153` starts each daemon through a helper that polls `via daemon status` (`crates/via-cli/tests/support/daemon.rs:175`). That CLI can start a replacement daemon, making readiness refer to a different process and causing flaky or misleading results. Apply the separate T4-flake direct-socket probe to these scenarios.

The reported `ApiError`/`Refusal` change, pre-recovery open tally, unreadable-space refusal, WAL scenario substitution, and queued-turn failure did not reveal another defect in this review.

### Measurement suggestions

None.

### Could not verify

Recovery, Latch-batch, and Host-absence writes under either limit; concurrent sharing and the 60-second refresh of the data-size walk. The full plan gate was reported by the worker but was not rerun here. Locally, formatting and diff checks passed, all six focused T4-7 scenarios passed, and Git status remained clean.