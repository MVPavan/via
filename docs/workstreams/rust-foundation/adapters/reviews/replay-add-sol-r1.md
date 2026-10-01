**UNSOUND.**

Reviewed all five commits in `8bce325..HEAD`. Findings below are established by source tracing; adverse scheduling interleavings were not reproduced.

R = `crates/via-fake-agent/src/replay.rs`; T = `crates/via-fake-agent/tests/replay.rs`.

1. **Important — queued restoration can lose to an expired, obsolete deadline.** [R:390](../../../../../crates/via-fake-agent/src/replay.rs#L390), [R:459](../../../../../crates/via-fake-agent/src/replay.rs#L459).  
   The watchdog checks expiry before receiving queued updates. A valid interleaving is: watchdog consumes the short deadline; the main thread receives an on-time line and queues restoration; watchdog resumes after the short deadline and expires without consuming restoration. A compliant replay can exit 3. Capacity-one sends also mean restoration can block behind an obsolete update. At exact equality, the reader accepts (`>`), while the watchdog expires (`left.is_zero()`), leaving inconsistent boundary decisions.  
   **Smallest fix:** synchronize current deadline/generation and read completion with expiry; stale queued deadlines must not commit termination. Define one equality rule and add controlled race tests.

2. **Important — `within_ms` starts later than the specified anchor.** [R:472](../../../../../crates/via-fake-agent/src/replay.rs#L472).  
   The brief specifies the previous step’s completion. Instead, `Instant::now()` runs inside the next step, after loop bookkeeping and any intervening scheduling delay. A pause between steps grants extra time, allowing a late decline to pass.  
   **Smallest fix:** retain the previous completion instant in the replay loop and pass it into the timed expectation.

3. **Important — the run-capped timed-read path omits the late-line check.** [R:448](../../../../../crates/via-fake-agent/src/replay.rs#L448).  
   When `start + within_ms >= run_deadline`, the function returns ordinary `read_line` without checking completion time. If watchdog termination is delayed or in diagnostic grace, a line read after the effective deadline can advance the fixture and exit successfully. This exposes the existing asynchronous expiry window through the new timing contract.  
   **Smallest fix:** calculate the effective limit as `min(step_limit, run_deadline)` and apply the post-read check to every timed expectation.

4. **Important — §A’s ordered-EOF and strict-input requirements remain unmet.** [R:515](../../../../../crates/via-fake-agent/src/replay.rs#L515), [R:292](../../../../../crates/via-fake-agent/src/replay.rs#L292).  
   `await_eof` accepts EOF that happened earlier; [T:661](../../../../../crates/via-fake-agent/tests/replay.rs#L661) explicitly exercises already-closed stdin. Thus `emit terminal → await_eof` cannot detect closing stdin before the terminal. Separately, `expect → emit/exit` ignores buffered trailing input unless an explicit `await_eof` is reached. These conflict with interface-observations.md:8–9 (`scratchpad/execution/fixtures/interface-observations.md:8`).  
   The narrower additions brief permits the implemented behavior, so this is also a requirements discrepancy. `expect interrupt → await_eof` does establish interrupt bytes before EOF; it does not establish close after an emitted terminal.  
   **Smallest fix:** reconcile the strict-mode contract and provide bounded input/EOF observation for fixtures requiring those stronger assertions.

5. **Important — timing tests permit consequential regressions.** [T:725](../../../../../crates/via-fake-agent/tests/replay.rs#L725).  
   By inspection, replacing 300 ms with 5,000 ms still satisfies the negative case’s ten-second outer bound and assertions. Removing restoration also passes: the successful case exits immediately after the expectation. Removing the late-line check encounters no late-input case.  
   **Smallest fix:** check the requested timing bound, add an on-time expectation followed by work beyond its short deadline but within the run deadline, and cover late reads and queued restoration with controlled scheduling.

6. **Minor — partial-input and launch-write failure regressions lack coverage.** [T:659](../../../../../crates/via-fake-agent/tests/replay.rs#L659), [T:825](../../../../../crates/via-fake-agent/tests/replay.rs#L825).  
   The EOF test sends a complete line and closes stdin; a newline-waiting implementation could pass while mishandling an open partial line. Launch tests serialize starts and exercise an **open** failure, leaving actual write errors and short writes untested.  
   **Smallest fix:** test one byte with stdin held open, concurrent starts with unordered PID comparison, and injected write-error/short-write outcomes.

The other additions match the brief by inspection: `fill_buf` rejects any pending byte, null counts as present, exit validation happens at load, stderr remains verbatim and byte-bounded, and logging uses one append write with error/short-write checks under the load watchdog. Counting `--version` is appropriate: every start counts, and AD7 prohibits a separate version-probe process.

Existing fixture, line, capture and step caps remain intact. Watchdog/helper creation failures still reach exit 3; diagnostic waiting retains its 100 ms bound.

**Verified:** `cargo nextest run --locked -p via-fake-agent` — **37 passed, 0 skipped**. Diff check passed; working tree remained clean.

**Could not verify:** runtime race schedules, thread exhaustion, blocked I/O, concurrent append behavior or injected write failures; historical RED/gate claims; downstream fixture/conformance integration. No Beads, vendor CLI, or model was run.