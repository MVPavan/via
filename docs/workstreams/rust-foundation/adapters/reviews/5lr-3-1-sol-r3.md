**SOUND** for `87ffe23..4f995d8`.

**R2 defect: fixed.**

- Observation arrays and non-negative integer counts are validated before comparison: [conformance_expect.rs:216](../../../../../crates/via-core/tests/support/conformance_expect.rs#L216). The accepted-turn count rule rejects the contradictory `ideal()` case at line 435.
- **37 independent negative witnesses** require rejection for the specific violated rule: [conformance_codex.rs:110](../../../../../crates/via-core/tests/conformance_codex.rs#L110).

**New defects: none found.**

The checker rejects retired `terminal.usage` and unknown schema fields while accepting every key used by Claude’s 16 expectation files. The adoption preserves existing protocol steps, places EOF gates immediately after the required replies, and uses the correct strict `absent` pointers. c0’s exit code and stderr match the recording after declared sanitization.

The `drive()` obligations cover launch-log counting, replay failure status, and deterministic idle retirement consistently with the coordinator rulings.

**Checks:** checker tests **3 passed**; replay tests **29 passed**; ignored conformance cases **16 expected failures** because `drive()` remains unimplemented.

**Could not verify:** integrated fixture fidelity, real driver timing/retirement/launch counting, and c7’s per-turn I/O ownership. Its replay pins the whole-case sequence; attribution still depends on the future driver. The local smoke driver and full gate were not independently rerun.

No files or Git state changed; final Git status was clean.