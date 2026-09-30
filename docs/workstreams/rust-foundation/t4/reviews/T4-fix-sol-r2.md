**UNSOUND**

### Findings

- **Important — `crates/via-core/src/engine/status.rs:201`:** `daemon/status` reads free space through the 16-slot blob pool without taking a diagnostics permit. Sixteen concurrent status requests stalled in `statvfs` can occupy every slot after their 2 s replies, causing turn-folder creation or other turn work to be refused. The two-permit guarantee therefore does not cover all diagnostics. The smallest fix is to use the shared diagnostics semaphore for the status free-space read, with its permit held by the blocking closure; keep the turn admission free-space read independent.

### Could not verify

I did not rerun the test on `14ccbb1` because that would change the checkout. The saved RED log shows the third `logs` call reached a blob step, and the old source explains that result. The current regression test passed; five focused logs and data-size tests passed. I did not rerun the full gate or dynamically test sixteen stalled status requests. Git status remained clean.
