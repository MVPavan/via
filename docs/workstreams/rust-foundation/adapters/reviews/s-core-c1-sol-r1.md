**UNSOUND.** The guard can hide production literals. Replay can exceed its memory and line bounds, and its watchdog can hang instead of terminating the process.

Findings below distinguish executed guard reproductions from replay defects established by source inspection.

1. **Blocker — watchdog termination depends on blocking stderr.** [src/replay.rs:175](../../../../../crates/via-fake-agent/src/replay.rs#L175). The watchdog locks and writes stderr *before* calling `exit`. Meanwhile, line 95 can hold that lock while printing an arbitrarily large mismatch or capture error. A full stderr pipe blocks both threads indefinitely. **Smallest fix:** bound error diagnostics and ensure watchdog termination cannot wait for a stderr lock or write.

2. **Important — emitted lines and substitution allocations exceed the bound.** [src/replay.rs:206](../../../../../crates/via-fake-agent/src/replay.rs#L206). `load` checks the template, but `substitute` appends captures without checking the resulting size. Repeating a large capture can allocate vastly more than 1 MiB before output begins. `--version` also reaches `write_line` without any line-size validation. **Smallest fix:** check each append before allocating, and enforce the output bound in the shared writer, including version output.

3. **Important — fixture and capture memory are unbounded.** [src/replay.rs:150](../../../../../crates/via-fake-agent/src/replay.rs#L150). `fs::read` and deserialization allocate the entire fixture before checking 10,000 steps. Expected values, argv, provenance, version, and capture maps have no size limits. At line 198, arbitrarily many capture names can each retain another serialization of the same large input value. **Smallest fix:** bound fixture ingestion and deserialization, capture count, and aggregate retained capture bytes; reject before excessive allocation.

4. **Important — the deadline does not cover the whole invocation.** [src/replay.rs:103](../../../../../crates/via-fake-agent/src/replay.rs#L103). Loading, parsing, and argv checking precede watchdog creation. `--version` bypasses it entirely. The watchdog starts a relative sleep when its thread gets scheduled, extending the budget further. **Smallest fix:** record an absolute start/deadline, account for setup time, and enforce the remaining budget across every path, including version and diagnostic output.

5. **Important — Claude’s generated session UUID has no fixture binding.** [src/replay.rs:43](../../../../../crates/via-fake-agent/src/replay.rs#L43). Argv consists exclusively of fixed strings; captures come exclusively from stdin. The pinned Claude contract launches with `--session-id UUID`, and its user input does not contain that UUID. A fixed recording cannot validate a newly generated argv UUID and echo it in init/result messages. **Smallest fix:** settle an approved deterministic UUID test seam or explicit argv binding that preserves exact checking of the remaining arguments. This is a downstream compatibility gap; basic Codex request/response pairing is representable.

6. **Important — test-item extent can hide following production fields and parameters.** [check-harness-literals.py:166](../../../../../scripts/check-harness-literals.py#L166). Any encountered `fn` sets `declaration = True`, including function-pointer types. Executed reproductions both returned **no findings**:
   - `struct X { #[cfg(test)] cb: fn(), codex: u8 }`
   - `fn f(#[cfg(test)] cb: fn(), codex: u8) {}`

   Extent also ends prematurely at generic commas and the first const-expression brace, exposing portions of test-only items. **Smallest fix:** determine item category from declaration context; track field/type extent without treating type keywords as declarations.

7. **Important — file exclusion does not establish sole test reachability.** [check-harness-literals.py:221](../../../../../scripts/check-harness-literals.py#L221). Declarations are flattened by basename; inline scope, `#[path]`, `include!`, and implicit crate-root reachability are absent. Executed reproductions hid production literals when:
   - a nested test declaration was mistaken for a declaration of a different file;
   - a test module’s file was also imported through a production `#[path]` alias;
   - that file was included by production `include!`;
   - production `lib.rs` was also imported as a binary’s test-only module.

   **Smallest fix:** resolve actual module paths and production reachability, protect crate roots, and retain scanning whenever sole test reachability cannot be established.

8. **Important — harness-table discovery can silently select the wrong names.** [check-harness-literals.py:258](../../../../../scripts/check-harness-literals.py#L258). Discovery starts at any identifier named `HARNESSES`, then searches forward for `=`. An earlier helper referencing `HARNESSES` caused it to read an unrelated row initializer and miss all three real harnesses. Also, `name /*canonical*/: "claude"` silently omits that row while accepting the remaining table. Both were reproduced in memory. **Smallest fix:** identify the pinned constant declaration, ignore comments during syntax matching, and reject incomplete row extraction.

9. **Important — test exclusion misses attributes belonging to excluded items.** [check-harness-literals.py:194](../../../../../scripts/check-harness-literals.py#L194). `#[doc = "codex"]` before `#[cfg(test)]` remains scanned. Likewise, `#[cfg(/* comment */ test)]` is not recognized; comments interrupting `mod name;` prevent file exclusion. Executed probes confirmed the first two false positives. **Smallest fix:** exclude the complete attribute/doc-comment cluster and ignore comments when matching Rust syntax.

10. **Important — guard self-test is insufficiently discriminating.** [check-harness-literals.py:352](../../../../../scripts/check-harness-literals.py#L352). All §5.6 named examples are present, but several rules can break without failing it. In-memory mutations disabling raw-string word scanning and the exercised char/lifetime distinction still produced exactly the expected findings. There is no custom table-name sentinel, mixed production/test file reachability, or difficult item-extent coverage. **Smallest fix:** add independent positive and negative expectations for these rules and the reproduced defects above.

11. **Important — test collection can deadlock or force otherwise valid replays to time out.** [tests/replay.rs:56](../../../../../crates/via-fake-agent/tests/replay.rs#L56). `finish` waits for process exit before draining either pipe. Large stdout fills the pipe and prevents successful completion; large stderr encounters finding 1. Collected output and `read_line` allocations are also unbounded. **Smallest fix:** drain both pipes concurrently into bounded collectors, with an outer deadline that kills and reaps the child.

12. **Important — replay tests permit meaningful regressions.** [tests/replay.rs:179](../../../../../crates/via-fake-agent/tests/replay.rs#L179). Static counterexamples:
   - An `await_signal` changed to a no-op can still pass: both output lines are buffered, and signalling the unreaped child can succeed.
   - Removing the delay leaves the full-fixture test’s assertions unchanged.
   - A deadline reset per step passes the single-step deadline test.

   Step-count, expanded-output/version bounds, aggregate memory, and exact failure exit codes lack coverage. **Smallest fix:** add a no-signal negative case, a delay exceeding the deadline, a multi-step shared-budget case, boundary cases, and exact exit-code assertions.

13. **Minor — the deadline test has a startup timing race.** [tests/replay.rs:214](../../../../../crates/via-fake-agent/tests/replay.rs#L214). Its 100 ms watchdog starts while the step counter is zero, before runtime initialization. Scheduling delay can produce “step 0” and fail the assertion requiring “step 1.” **Smallest fix:** provide explicit readiness evidence and a generous startup allowance, or controlled deadline activation.

14. **Minor — non-UTF-8 argv panics instead of reporting replay failure.** [src/replay.rs:105](../../../../../crates/via-fake-agent/src/replay.rs#L105). `env::args()` panics on unsupported OS argument encoding, bypassing exit code 3 and the normal diagnostic. **Smallest fix:** use `args_os` with checked conversion or comparison and return an argv mismatch.

15. **Minor — discarded I/O errors lack the required justification.** [src/replay.rs:95](../../../../../crates/via-fake-agent/src/replay.rs#L95). Lines 95, 99, and 175 discard write/flush errors without explaining why that is safe, contrary to coding-style §4. **Smallest fix:** handle consequential failures and document intentional best-effort diagnostics.

The author’s concerns resolve as follows:

| Concern | Verdict |
|---|---|
| **1 — nested inline modules** | **Can hide findings**, through incorrect file association; not merely extra scanning. Finding 7. |
| **2 — item extent** | **Can hide findings**, especially function-pointer fields/parameters. Generic commas also add false positives. Finding 6. |
| **3 — exact `cfg(test)`** | Composite forms being scanned add findings. Exact spelling with comments and preceding attributes also causes false positives. Finding 9. |
| **4 — allow entries** | **Can hide findings:** one matching substring suppresses every harness finding on that line, including unrelated names. The new header explicitly documents line-wide exceptions; the approved format does not pin narrower semantics. Stale-entry detection is not required. The shipped file is empty. |
| **5 — real-tree guard** | Missing table correctly exits 2. Supplying the fixture table **in memory** reproduced **138 findings, all `fake`**. This does not replace the coordinator’s merged-tree RED run. |
| **6 — fixture format** | Basic Codex pairing and Claude control-message IDs work. Claude session-UUID binding remains unresolved; the format is not yet sufficient for unrestricted first-spawn recordings. Finding 5. |
| **7 — detached watchdog** | Process-lifetime ownership is reasonable for this test binary. Blocking diagnostics and incomplete deadline coverage are unsound. Findings 1 and 4. |
| **8 — early signals** | Configured handlers are installed before the first step, and early signals are retained. This intentionally changes those signals’ default disposition. It does not mean handlers exist during fixture loading or process startup. |
| **9 — substitution bound** | **Confirmed defect**, including allocation before any output validation. Finding 2. |
| **10 — unchanged lockfile** | **Accepted.** No ACP references remain in manifest/lockfile; the locked workspace build passes. |

Outside this chunk, noticed: constructed or escaped spellings such as `"co\x64ex"` evade the raw-word matcher. The guard therefore still needs K2’s human review. Future stderr-flood, partial-message, oversized-message, and nonzero-exit scenarios also need mechanisms beyond this fixture format; those are not explicitly required by §8 item 3.

Actual verification:

- `cargo build --locked --workspace`: passed.
- Guard self-test: passed.
- Named replay nextest selection: **6 passed, 0 skipped**.
- Real guard: expected missing-table failure, **exit 2**.
- Read-only, in-memory guard probes confirmed the reported suppression cases. Nested comments, ordinary byte strings, chars/lifetimes, joined subwords, and raw/byte-raw strings at hash depths **0, 1, 2, 12, 255** passed the additional probes.
- `git diff --check`: clean; final Git status clean. No source or Git state changed. Public-repository content hygiene otherwise passed inspection.

Could not verify: live vendor behavior, other platforms, the author’s historical RED runs or broader gate claims. Replay hang and allocation findings were established by source tracing; no additional standalone vendor invocation was run. Start-request mode is unchanged by inspection when no replay fixture is selected; its legacy tests were outside the permitted test selection.