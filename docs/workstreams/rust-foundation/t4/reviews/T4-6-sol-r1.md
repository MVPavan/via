**UNSOUND**

### Findings

- **Important — F24 can pass despite more than 32 MiB of growth after 64 MiB.** `crates/via-cli/tests/s1_f24_memory.rs:504` uses the *highest* RSS sample before 64 MiB as its comparison point. In my passing run, that was 172,084 KiB, while RSS at the threshold was 160,360 KiB. A later rise of over 32 MiB from the threshold could therefore pass. Compare the post-threshold peak with the RSS sample at the threshold. Warming the sockets first is a faithful way to isolate flood growth; the peak-to-idle check still captures their allocations.

- **Important — new daemon scenarios inherit the auto-starting readiness race.** `crates/via-cli/tests/s1_f24_memory.rs:399`, `crates/via-cli/tests/s1_bounds.rs:335`, and `crates/via-cli/tests/s1_c1_reads.rs:127` use `Daemon::start`, whose readiness probe calls `via daemon status` without checking the returned PID against its child (`crates/via-cli/tests/support/daemon.rs:175`). A rival auto-started daemon can satisfy readiness; the crash scenario can then kill the wrong PID. Merge the planned `serving_pid` fix before landing and use its PID for the crash and F24 measurements.

- **Minor — `list_page` omits A49’s first-item assertion.** `crates/via-store/src/runtime/sql.rs:2134` breaks without the assertion present in `events_page`. Current intake maxima keep a summary well below 1 MiB, so a legitimate row cannot stall the cursor today. If that invariant regresses, the first row can produce an empty page without advancing. Add `debug_assert!` that the first summary fits.

- **Minor — `failure.message` can exceed its encoded 2 KiB maximum by two bytes.** `crates/via-core/src/api.rs:1861` allows 2,048 encoded characters and then JSON adds quotes. A 2,048 byte ASCII message serializes as 2,050 bytes. Reserve two bytes for the quotes.

- **Minor — a valid `events.after` above `i64::MAX` returns `store_error`.** `crates/via-store/src/runtime/sql.rs:2037` rejects the `u64` before querying. Such a cursor is past every stored sequence and should yield an empty page with `more: false`. Handle `after >= head` before the SQL conversion.

The 2 MiB paced chunks respect the 1,024-message queue. Running F24 alone in nextest is justified by its measured interference with timing tests. The 16 `types`, scoped `NotCommitted` treatment, and receipt maxima match T4-6’s design. The envelope list path is exercised through the test hook, although the fake has no producer.

### Measurement suggestions

- Arm `final_text.write.fail` in a focused test. The short-write and sync-failure paths are already tested.
- Once a route produces denials and declines, add a route-level check of list totals and event references.

### Could not verify

The focused T4-6 run passed **9/9** tests, and the isolated F24 run passed **1/1**. I did not rerun the full gate or test the branch after the planned T4-flake merge. Git status remained clean.