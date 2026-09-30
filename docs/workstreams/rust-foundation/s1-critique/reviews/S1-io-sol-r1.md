**UNSOUND**

1. **Important — live collection introduces another unbounded history cache.** `crates/via-host/src/host.rs:355`  
   `prune_controls` copies every forced-stop generation into `HostTasks::forced`, which never removes entries. Repeated forced turns therefore grow resident state indefinitely, even after their tasks, controls and groups are gone. The new registry-size test uses graceful `/bin/true` turns and misses this path. This violates runtime §8’s bounded-holder invariant.

   **Smallest fix:** give forced facts a settlement lifecycle: retain them for unresolved results and evict them after durable handoff. Preserve the sticky failed-join counter. Extend the collection test with repeated forced turns and include this set in its counts.

2. **Important — the busy-lock regression can pass without exercising contention.** `crates/via-host/src/host.rs:2660`  
   Sleeping 400 ms does not establish that supervision attempted the held lock. If supervision first runs after that sleep, the test releases the lock before its first poll; the old defective implementation can then report the exit and pass.

   **Smallest fix:** acknowledge an actual busy-lock poll before releasing the guard, using controlled timer advancement or an observed poll event. Then assert the subsequent exit report. This supplies the order/state proof required by check 7.

**Measurement suggestions**

- **Concern (a):** the current uncertain cleanup follows the brief and is truthful. Socket shutdown on retirement is also correct and provides a simpler cleanup disposition: runtime §5.1 explicitly makes pre-ARM EOF exit without spawning and post-ARM EOF initiate group cleanup. The anchor implements that rule at `crates/via-host/src/anchor.rs:229`. Prefer that alternative if revising retirement; test peer EOF and group absence while retaining `ProcessControl`. EOF alone must not invent forced-stop evidence.
- **Concern (b):** retiring after an unread `Stop` reply times out is correct. The request may already have initiated cleanup; continuing exit supervision through the uncertain stream would risk consuming that reply as `Status`. A completely consumed late reply can safely preserve framing while refusing deadline-sensitive evidence, as `transact_by` does.

**Could not verify**

- Independently replaying the regressions on `7370e0e`; I inspected the worker’s RED logs.
- Full workspace and release gates during this review.

Current verification: **six new regressions passed; 26 Host/Wire unit tests passed; formatting, layer checks and diff checks passed**. No existing assertion was changed. Tracked files and Git state remained unchanged.