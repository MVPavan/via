**UNSOUND.** Six original findings are addressed; #2 is partial. Two Important deadline defects remain. No additional Blocker or Important production defects found in the full branch.

All locations below are at **d1e1af9**.

| r1 | Status | Evidence |
|---|---|---|
| #1 Ownership | Addressed | `crates/via-cli/tests/s_launch.rs:264`: fallback signals only the retained `Child`. Marker scans authorize no signals. `:312` checks the child before SIGSTOP; `:812` covers the marked outsider. |
| #2 Scan bounds | **Partial** | `crates/via-cli/tests/support/evidenced.rs:270`: cutoff and capped reads added, but late final observations can return success. See N1–N2. |
| #3 Refusal retention | Addressed | `crates/via-adapters/src/instance.rs:194`: 1,024-byte key limit; `:202`: 64-entry ceiling and oldest-write eviction. Focused retention tests passed. |
| #4 Renewed deadline | Addressed | `crates/via-cli/tests/s_launch.rs:675`: survivor check receives the original deadline and signals nothing. A separate late-observation defect remains—N3. |
| #5 Timeout classification | Addressed | `crates/via-cli/tests/s_launch.rs:576`, `:633`: invalid-config runs use the typed timeout path. |
| #6 Duplicate parsing | Addressed | `crates/via-cli/src/server/config.rs:160` parses into typed settings; `crates/via-cli/src/server.rs:304` consumes them without reparsing. |
| #7 Release inspection seam | Addressed | `crates/via-adapters/src/instance.rs:116`, `:235`: `retained()` removed; retention inspection is module-local under `cfg(test)`. |

1. **N1 — Important: a scan can report absence after its cutoff.**  
   **`crates/via-cli/tests/support/evidenced.rs:319`, `:336`, `:355`**

   The cutoff is checked between entries, but several read-result paths continue without checking elapsed time. There is no final cutoff check before `Ok(alive)`. If the final environment read finishes late and contains no marker, the function returns successful absence. Late vanished-process, command-line and stat observations have equivalent gaps.

   **Evidence:** A compiled probe using the unchanged function body, a single directory entry and its delayed read returned **`Ok([])` after the cutoff**. The new regression tests expiry before scanning and mid-scan, but misses the final-entry case.

   **Smallest fix:** Check the cutoff after every observation and before returning success. Add final-entry regressions for these paths. Expiry must return uncertainty.

2. **N2 — Important: the owned-child fallback can signal after its cutoff.**  
   **`crates/via-cli/tests/s_launch.rs:267`**

   `child.kill()` runs whenever the preceding proof failed, without checking `kill_by`. A delayed scan can finish with an error after that cutoff—or after the entire teardown deadline—and the fallback still starts destructive work. The overrun check at line 296 runs afterward.

   Ownership is now correct; the deadline guard present in the former fallback was lost.

   **Smallest fix:** Check `kill_by` immediately before signalling. At expiry, record incomplete cleanup and preserve the sandbox. Add a regression where observation consumes the fallback allowance.

3. **N3 — Minor: the survivor check accepts post-deadline absence as success.**  
   **`crates/via-cli/tests/s_launch.rs:394`**

   `gone()` runs before the deadline check, and a successful absence observation returns `None` without timestamp validation. That cannot establish exit by the deadline.

   **Evidence:** A compiled probe supplied an expired deadline and an absent PID; it returned **`None`**, rather than uncertainty.

   **Smallest fix:** Check expiry before observing and timestamp the observation afterward. A late observation cannot supply timely exit proof.

4. **N4 — Minor: the new startup wait does not enforce its twenty-second deadline.**  
   **`crates/via-cli/tests/s_launch.rs:119–127`**

   `serving_pid()` receives no deadline, and readiness is accepted before the outer timer is checked. Its RPC helper uses separate ten-second read timeouts (`support/daemon.rs:495`) rather than the remaining startup budget. A probe started near expiry can return readiness afterward and still succeed.

   **Smallest fix:** Pass the absolute deadline through the readiness RPC and check it before accepting success.

The coordinator’s withdrawal of scan-based signalling now agrees with runtime §11.2. The production environment, configuration, resolution and cache changes retain the intended scoped behavior. Cache wiring and actual vendor-plan inheritance coverage remain deferred to x.3.2.

Verification:

- Both diff whitespace checks passed; formatting passed for all twelve changed Rust files.
- Three committed cache module tests passed in a standalone build, with unrelated resolution code omitted.
- Two focused deadline regressions failed as described above. They used unchanged function bodies with isolated dependency shims; they were not daemon integration runs.
- I did not rerun the workspace gates, doctests, process-leak stress or vendor qualification. The reported gate results and `s1_f19` attribution remain independently unverified.
- Repository files and Git state were untouched. Only temporary build artifacts were created and removed; no Beads, vendor CLI or model ran.