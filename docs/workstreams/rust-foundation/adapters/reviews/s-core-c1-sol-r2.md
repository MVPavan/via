**UNSOUND.** The original guard reproductions pass, but exclusions still hide production literals. Replay collection and deadline guarantees also remain incomplete.

Paths: **G** = `scripts/check-harness-literals.py`; **R** = `crates/via-fake-agent/src/replay.rs`; **T** = `crates/via-fake-agent/tests/replay.rs`.

| Finding | Status | Reason | File:line |
|---|---|---|---|
| #1 Watchdog termination | Partly fixed | Once the helper starts, blocked stderr cannot hold the watchdog beyond its 100 ms grace. However, helper creation can panic before exit; loading failures still print without an armed watchdog. Diagnostics can total 1,038 bytes, exceeding the specified 1 KiB. | [R:278](../../../../../crates/via-fake-agent/src/replay.rs#L278), [R:407](../../../../../crates/via-fake-agent/src/replay.rs#L407) |
| #2 Output/substitution bounds | Fixed | Every append is checked before allocation; the shared writer checks version output too. Boundary tests pass. | [R:343](../../../../../crates/via-fake-agent/src/replay.rs#L343), [R:389](../../../../../crates/via-fake-agent/src/replay.rs#L389) |
| #3 Fixture/capture memory | Partly fixed | Fixture ingestion, capture count and retained capture bytes are capped. New expected-value substitution has no aggregate expansion bound; see new defects below. | [R:200](../../../../../crates/via-fake-agent/src/replay.rs#L200), [R:370](../../../../../crates/via-fake-agent/src/replay.rs#L370) |
| #4 Whole-invocation deadline | Partly fixed | Absolute deadline now covers argv and `--version`. Fixture lookup/loading remain outside enforcement; a byte cap does not bound I/O time. Loading errors return before watchdog creation. | [R:143](../../../../../crates/via-fake-agent/src/replay.rs#L143), [R:155](../../../../../crates/via-fake-agent/src/replay.rs#L155) |
| #5 Argv UUID binding | Fixed | Explicit captures bind the argument; surrounding arguments remain exact. UUID emit/expect and negative checks pass. | [R:238](../../../../../crates/via-fake-agent/src/replay.rs#L238), [T:232](../../../../../crates/via-fake-agent/tests/replay.rs#L232) |
| #6 Item extent | Partly fixed | Both original function-pointer reproductions now report production identifiers. However, exclusion still lacks context: item-shaped tokens inside macro input can be suppressed even when the macro emits production code. | [G:304](../../../../../scripts/check-harness-literals.py#L304) |
| #7 File exclusion | Partly fixed | Original nested/path/include/crate-root probes pass. Production inline declarations outside the selected parent files and computed include paths still permit suppression. | [G:420](../../../../../scripts/check-harness-literals.py#L420), [G:472](../../../../../scripts/check-harness-literals.py#L472) |
| #8 Table discovery | Fixed | Original helper-reference and commented-name reproductions pass. Missing, ambiguous and incomplete tables fail closed. | [G:350](../../../../../scripts/check-harness-literals.py#L350) |
| #9 Attribute/comment clusters | Fixed | Preceding doc attributes, commented `cfg(test)` and commented module declarations now behave correctly. Composite cfg remains scanned. | [G:279](../../../../../scripts/check-harness-literals.py#L279) |
| #10 Self-test discrimination | Fixed | Independent expectations pass; disabling raw-string scanning or either char/lifetime distinction now fails the relevant case. | [G:568](../../../../../scripts/check-harness-literals.py#L568) |
| #11 Test collection | Partly fixed | Stderr drains concurrently, with outer kill/reap. Stdout stops draining when its 16-line channel fills; `finish()` consumes it only after exit. Longer valid replays can still time out. Final stdout collection also lacks an aggregate cap. | [T:74](../../../../../crates/via-fake-agent/tests/replay.rs#L74), [T:120](../../../../../crates/via-fake-agent/tests/replay.rs#L120) |
| #12 Regression coverage | Fixed | Added no-signal, excessive-delay, shared-deadline, resource-boundary and exact failure-code checks pass. | [T:303](../../../../../crates/via-fake-agent/tests/replay.rs#L303) |
| #13 Deadline-test race | Partly fixed | One second and readiness reduce the race. Nevertheless, “ready” is emitted during step 1, before step 2 is recorded; the assertion requires step 2 and a diagnostic that is now best-effort. | [T:358](../../../../../crates/via-fake-agent/tests/replay.rs#L358), [R:193](../../../../../crates/via-fake-agent/src/replay.rs#L193) |
| #14 Non-UTF-8 argv | Fixed | Checked `args_os` conversion returns mismatch/exit 3; test passes. | [R:239](../../../../../crates/via-fake-agent/src/replay.rs#L239) |
| #15 Discarded I/O errors | Fixed | Stdout write/flush errors propagate; intentional discarded results have justification comments. | [R:389](../../../../../crates/via-fake-agent/src/replay.rs#L389) |
| Allow-entry concern | Fixed | Original mixed-name probe retains the unrelated finding. Allowing one occurrence also retains an uncovered occurrence of the same name. Shipped allow file remains empty. | [G:529](../../../../../scripts/check-harness-literals.py#L529) |

The remaining exclusion defects were reproduced **in memory** and also exist at the baseline:

- `lib.rs`: `mod engine { mod tests; }`; unused `engine.rs`: `#[cfg(test)] mod tests;`; `engine/tests.rs`: production `"codex"`. **No findings.**
- `#[cfg(test)] mod tests; include!(concat!("tests", ".rs"));`, with `"codex"` in `tests.rs`. **No findings.**
- A macro that consumes `#[cfg(test)] fn ...` and emits the function without that attribute suppresses `"codex"` in its invocation. **No findings.**

These require conservative scanning, not a module resolver.

**New defects introduced by fixes:**

1. **Important — helper creation can disable termination.** [R:278](../../../../../crates/via-fake-agent/src/replay.rs#L278): if the OS refuses the diagnostic thread, `thread::spawn` panics in the detached watchdog before it reaches exit. Panic reporting can itself block on stderr. Rust documents this [thread-creation failure behavior](https://doc.rust-lang.org/std/thread/fn.spawn.html#panics). The 100 ms grace therefore provides a conditional bound.
2. **Important — expected-value expansion bypasses aggregate memory limits.** [R:370](../../../../../crates/via-fake-agent/src/replay.rs#L370): 10,000 expected array elements containing `${pad}`, with a 600 KiB capture, allocate roughly **5.7 GiB** before reading the next input. Each string individually passes the limit. This is established by source tracing.
3. **Important — literal expected placeholders regress.** [R:372](../../../../../crates/via-fake-agent/src/replay.rs#L372): an expected request containing literal `echo ${HOME}` previously matched; it now fails as uncaptured, with no escape mechanism. This is the newly introduced **expect** portion of author concern 1; the emit limitation predates these fixes.

**Verification:** nextest **27 passed, 0 skipped**; guard self-test passed; original #6–#9 and allow probes rerun; raw/char/lifetime mutations detected. Real-tree scan with an in-memory table still reports **138 findings, all `fake`**. Diff check and final Git status clean; no source or Git state changed.

**Could not verify:** runtime blocked-pipe or thread-exhaustion reproductions, allocation amplification, or pathological startup scheduling under the permitted commands. Those conclusions are source-derived. Historical RED/gate claims, other platforms and live vendor behavior remain unverified.