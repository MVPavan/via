**UNSOUND.** I found three Important defects and four Minor defects. All line numbers below refer to **bb8d01f**.

1. **Important — Environment matches authorize killing a process without proving launch ownership.**  
   **`crates/via-cli/tests/s_launch.rs:351`, `:365`, `:382`, `:433`**  
   `pin_member` accepts any pinned process carrying either matching `VIA_RUNTIME_DIR` or `VIA_STATE_DIR`. The pidfd prevents signalling a reused PID, but neither the environment match nor the stat check proves the sandbox started that process. The parsed start time is discarded.

   **Evidence:** A non-signalling OS probe created a helper outside any Sandbox with a matching runtime variable. Its genuine environment, stat and pidfd satisfied every acceptance predicate. The existing outsider test only covers an outsider without the marker and substituted stat data.

   **Smallest fix:** Retain launch provenance and a pidfd during auto-start, and signal only those retained process generations. Add an outsider test carrying the correct marker and its own genuine stat.

   **Contract conflict:** The coordinator’s accepted scan-based authorization conflicts with runtime §11.2’s retained-child ownership requirement. Pinning resolves process generation; it does not resolve ownership.

2. **Important — Process scans can overrun the teardown deadline.**  
   **`crates/via-cli/tests/s_launch.rs:191`, `:195`, `:218`, `:231`**  
   These calls synchronously scan all of `/proc`, without passing a deadline. The delegated scanner reads complete environments and command lines and checks no cutoff between processes. Subsequent scans can begin after the cutoff. The check at line 240 reports an overrun after the work finishes; it cannot enforce the bound.

   **Evidence:** `support/evidenced.rs:250–306` contains the unrestricted enumeration and reads. Thus a large or slow scan can consume the fallback and anchor allowances, exceeding runtime §11.2’s ten-second teardown.

   **Smallest fix:** Pass the absolute cutoff into scanning, check it between entries and reads, cap read sizes, and return explicit uncertainty at expiry. Prefer observing retained owned processes over repeatedly scanning the system.

3. **Important — Refusal-cache retention has no hard space bound.**  
   **`crates/via-adapters/src/instance.rs:163`**  
   Every distinct identity/recipe pair inserts another entry. Sweeping expired entries does not bound the number or bytes of live entries. Each write also scans all retained refusals, making a burst of distinct writes quadratic in aggregate.

   **Evidence:** `crates/via-adapters/tests/s_launch.rs:443–446` explicitly inserts and retains 1,000 entries at one instant. There is no ceiling; recipe strings also have no byte limit. Cold expired entries remain resident until a subsequent refusal write or matching lookup.

   **Smallest fix:** Add count and byte ceilings, with defined eviction on insertion. Test many distinct recipes within one TTL, rather than treating eventual expiry as a space bound. This defect is latent until x.3.2 wires the cache.

4. **Minor — The unresponsive-daemon regression grants another teardown budget.**  
   **`crates/via-cli/tests/s_launch.rs:736–739`**  
   If the daemon survives Sandbox teardown, the test creates a fresh `now + TEARDOWN` deadline and runs another kill/wait cycle. This failure path can extend teardown beyond the required single deadline.

   **Smallest fix:** Carry the original deadline into this check and report remaining uncertainty once it expires.

5. **Minor — Invalid-configuration subprocess timeouts are classified as ordinary failures.**  
   **`crates/via-cli/tests/s_launch.rs:689–699`**  
   This test calls `run_command` directly. A timeout becomes a failed Boolean assertion and a generic boxed error, so `evidenced` records `fail` instead of `timeout`.

   **Smallest fix:** Use `sandbox.run_within(&["daemon"], …)`, which already returns `ScenarioError::Timeout` and preserves cleanup notes.

6. **Minor — Harness configuration is parsed twice.**  
   **`crates/via-cli/src/server/config.rs:160`; `crates/via-adapters/src/config.rs:232`, `:296`**  
   Early validation parses the section and discards the typed result. `AdapterConfig::load` then repeats the same parsing. The settings are correctly frozen, but this violates the requested parse-once behavior and coding-style §3’s boundary-parsing rule.

   **Smallest fix:** Retain the adapter parser’s validated typed result and pass it into AdapterConfig construction, preserving validation before startup effects.

7. **Minor — A test inspection API ships in release builds.**  
   **`crates/via-adapters/src/instance.rs:204`**  
   `InstanceCache::retained()` is an unconditional public method used only by retention tests. It introduces an unnecessary production contract for inspecting private implementation state.

   **Smallest fix:** Move the retention assertions into module-local tests and remove the public inspection method.

The remaining production behavior matches the scoped requirements: the nine bootstrap names are forwarded through `env_clear`; Debug hides bootstrap values; harness parsing enforces the path, boolean, default and duplicate-key rules; PATH resolution uses effective-access checking; and identity/version retention uses the specified stat tuple and sixteen-entry eviction rule. I found no new production path forwarding unlisted credential variables or exposing binary values in configuration refusals.

Resolution remains deliberately steerable through configuration and the captured PATH. A stat identity does not authenticate executable contents or pin them across a later launch. Real adapters still need their reviewed environment allow-lists, launch recipes, handshake checks, effective inheritance evidence and cache wiring. Those are x.3.2 responsibilities, rather than additional S-LAUNCH defects.

Verification and limits:

- `git diff --check` passed. All nine changed Rust files passed Rust 1.98.1 formatting checks using committed contents.
- The ownership predicate was checked with a bounded, non-signalling OS probe; the helper exited naturally.
- I did not execute Rust builds, test suites, Clippy, dependency checks or vendor qualification. Runtime leak freedom and deadline behavior remain unverified.
- The plan test cannot detect replacing configured vendor inheritance with OD2 defaults: vendor plans refuse before reaching that call. This coverage is explicitly deferred to x.3.2.
- Failure-first chronology is reported in the records, but was not independently reproduced.
- No files or Git state were changed; no Beads, vendor CLI or model was run.