**UNSOUND**

### Findings

- **Important — cancelled `finish` loses task ownership.** `crates/via-wire/src/connection.rs:465` sets `finished = true` before its first await. If daemon shutdown aborts a drive while `finish` is draining or joining, `Drop` skips the straggler handoff; dropping the `JoinSet` aborts its tasks without a runtime owner or pending-task report. Set `finished` only after adoption, so cancellation takes the fallback handoff path.

- **Minor — Wire task failures are discarded.** `crates/via-wire/src/connection.rs:547` and `crates/via-wire/src/connection.rs:554` ignore join results. If a reader or writer panics, its owner never records that failure, contrary to the repository’s task-ownership rule. Collect failed joins and include them in Wire’s shutdown failure count.

The reported deviations **(a–d, f–h)** add no finding: the seeded splitter respects the no-new-dependency constraint; delaying `next_message` preserves the start-write seam while Route services controls; the retained open signature and Route-provided escape function preserve the layer boundary; the Adapter-level budget test exercises the stated bound; the two-burst test still reaches blocked forwarding; nextest isolates the process-wide counter; and the `assistant.text` assertions are explicitly due for T4-4.

### Measurement suggestions

- **(e)** Add a focused stall test that accepts an item, then blocks again. The code clears the deadline only after acceptance, and neither §13 nor the T4-3 plan requires a dedicated restart test, so its absence is not a finding.
- Track real vendor burst sizes as A47 directs.

### Could not verify

The T4-3 selector passed **9/9** tests; `cargo fmt --all --check` and the layer check passed. I did not rerun the full Gate G or reproduce the shutdown-cancellation path. The two named pre-existing flakes were not shown to worsen by this diff. Git status remained clean.