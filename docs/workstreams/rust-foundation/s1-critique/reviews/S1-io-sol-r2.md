**UNSOUND**

1. **Important — pruning still retains facts from live, unfinished closes.** `crates/via-host/src/host.rs:360`  
   `prune_controls` copies facts before checking whether the control is dropped. `close` sets `forced` before awaiting absence and sets `reported` afterwards. An acquisition during that wait permanently inserts the closing turn’s generation; reporting it later never removes the entry. Repeated overlapping caller cancellations therefore grow `forced` indefinitely. The serial regression misses this.  
   **Smallest fix:** transfer unreported facts only when actually pruning a dropped control; add an overlapping acquisition/close regression.

2. **Important — the unchanged row-4 cache is unbounded per daemon.** `crates/via-host/src/host.rs:1472`  
   This inserts once per failed acquisition, not once per daemon. A `VendorFacts` write that is **not committed** causes a scoped turn failure; Core can successfully commit its resolution and continue serving (`crates/via-core/src/engine/resolve.rs:303`). Repeated failures with successful cleanup therefore accumulate generations even after `AcquireFailure.forced` reaches Core. This is pre-existing but remains within the explicitly requested bounds audit.  
   **Smallest fix:** settle these cached facts after ownership handoff, preserving evidence for abandoned acquisitions; test repeated scoped failures and subsequent shutdown evidence.

3. **Important — the draining regression contains an implicit wall-clock assertion.** `crates/via-wire/tests/s1_wire.rs:779`  
   `take_undecoded().is_none()` requires completion before `BlobTasks::run`’s two-second timeout, which publishes an error note even while the save remains paused. Pausing only the test process for three seconds after its save acknowledgement reproduced this assertion failure on the fixed implementation. This violates T4-A50.  
   **Smallest fix:** control the timer or hold the save’s completion through a test seam independent of elapsed time, then assert draining before release.

**Could not verify**

- Independently replaying the old poll body; its supplied RED log was inspected.
- Dedicated reproductions of findings 1 and 2.
- Full workspace/release gates during this review. The normal Host/Wire run passed 82 tests; formatting, layer checks and diff checks passed. Git state remained unchanged.
