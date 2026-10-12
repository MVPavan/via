**UNSOUND.** Two Important defects remain. No Blocker found. Findings below refer to `4524c6f`.

1. **Important — PATH lookup can select a file the daemon cannot execute.**  
   [crates/via-adapters/src/instance.rs:34](../../../../../crates/via-adapters/src/instance.rs#L34) checks whether *any* execute bit is set. A file owned by the daemon’s user with mode `0601` passes, although that user cannot execute it. My reproduction selected that file ahead of a usable `0700` candidate; effective-access checks confirmed the first was unusable. This can break the first real adapter’s default binary lookup.  
   **Smallest fix:** respect the applicable owner/group/other execute class. Document the remaining ACL and `noexec` limitations of stat-only resolution.

2. **Important — daemon-test teardown lacks a kill fallback.**  
   [crates/via-cli/tests/s_launch.rs:145](../../../../../crates/via-cli/tests/s_launch.rs#L145) delegates cleanup to a stop RPC followed by exit polling. The fixture retains no daemon process handle. If startup stalls or the daemon becomes unreachable, this path records incomplete cleanup and leaves the daemon running; killing the stop client does not kill the daemon. The successful runs I inspected had proven exit, but this does not satisfy failure-path supervision.  
   **Smallest fix:** supervise auto-started daemon generations with retained, identity-safe process ownership and bounded kill/reap fallback after the ordinary stop attempt.

3. **Minor — configuration keys can corrupt or truncate diagnostics.**  
   [crates/via-adapters/src/config.rs:122](../../../../../crates/via-adapters/src/config.rs#L122) formats decoded key text verbatim. This affects harness names, member names, inheritance keys and duplicate-key errors. A reproduced key containing `\n` and ESC produced multiline stderr containing a terminal escape. A 6,000-character harness name also displaced `unknown harness` beyond the auto-start client’s 4 KiB capture limit. Input is bounded by the file’s 64 KiB limit, so this is not unlimited output.  
   **Smallest fix:** escape control characters and bound the displayed key while preserving the error rule.

4. **Minor — binary configuration accepts embedded NUL.**  
   [crates/via-adapters/src/config.rs:368](../../../../../crates/via-adapters/src/config.rs#L368) accepts `"/opt/tool\u0000suffix"`. I verified that `check_harnesses` returns success. Such a string cannot name a Unix filesystem object, so failure is deferred beyond configuration validation.  
   **Smallest fix:** reject embedded NUL with the named binary error.

5. **Minor — timeout artifacts receive the wrong outcome.**  
   [crates/via-cli/tests/s_launch.rs:124](../../../../../crates/via-cli/tests/s_launch.rs#L124) converts `Captured::timed_out` into an ordinary string error. The evidence wrapper classifies that as `fail`, rather than runtime §11.2’s `timeout`.  
   **Smallest fix:** return the existing `ScenarioError::Timeout`, retaining attached cleanup details.

6. **Minor — expired cache entries can accumulate indefinitely.**  
   [crates/via-adapters/src/instance.rs:133](../../../../../crates/via-adapters/src/instance.rs#L133) inserts without sweeping; expiry removes only the exact recipe subsequently queried. Version identities also have no retention bound. My test left 1,000 expired refusals retained after an unrelated lookup. Lookup expiry is correct, but memory reclamation is incomplete. This is dormant while the cache remains unwired.  
   **Smallest fix:** sweep expired refusals on writes and settle bounded retention before x.3.2 connects the cache.

7. **Minor, pre-existing — fixture-backed configuration Debug exposes bootstrap values.**  
   [crates/via-adapters/src/config.rs:155](../../../../../crates/via-adapters/src/config.rs#L155) derives `Debug` for `FakeFixture`, exposing all three `VIA_FAKE_*` paths through `AdapterConfig::Debug`. I reproduced this. The new redaction test uses no fake fixture, so it misses this exposure. `BootstrapEnv::Debug` itself correctly prints names only. I found no production logging call that currently emits these Debug values.  
   **Smallest fix:** redact fixture paths in Debug and extend the test to a complete fixture.

The five worker concerns:

| Concern | Verdict |
|---|---|
| Unknown harness uses `unknown harness` | **Sound.** It names the offending key and accurately distinguishes the rule. |
| Harness validation precedes disk/WAL validation | **Sound.** No supplied contract requires a different error priority. |
| User key text repeated without a length limit | **Minor defect**, finding 3. The file limit bounds input, but escaping and diagnostic truncation remain problems. |
| Doctests run separately from nextest | **Sound.** Both the positive and compile-fail doctests passed separately. |
| Plan inheritance test passes at base | **Accepted limitation.** It proves fake defaults and vendor refusal, not configured vendor-plan inheritance. Direct configuration tests cover parsing; x.3.2 still owes the observable plan test. |

The bootstrap allow-list and auto-start forwarding match the nine required names; credentials and other client variables are excluded. PATH lookup skips empty/relative entries, directories and files without execute bits. Symlinks are deliberately followed, including for identity. Configured binaries take precedence without filesystem validation, as the brief specifies.

The cache genuinely restricts refusal writes to the two incompatibility categories. It cannot prove that a caller actually observed a handshake incompatibility; that remains an adapter responsibility. Expiry is correct **one nanosecond before** and **exactly at** ten minutes. Leaving cache integration until x.3.2 is explicitly approved, rather than accidental dead code. I found no newly introduced test-only activation path in release code.

Verification passed: **605 default tests**, **24 selected failpoint tests**, **both doctests**, formatting, workspace clippy, dependency checks and layer checks. The literal guard reports **91 pre-existing violations**, all in unchanged files. Git status remained clean.

I could not independently establish the original failure-first history: the persisted gate contains green runs, and tests and implementation share commits. Daemon tests use bounded polling sleeps in shared helpers, so they are not literally sleep-free; cache tests use controlled time without sleeps. Real vendor behavior, session freezing across daemon restart, full failpoint coverage, musl/release gates and macOS behavior were not independently verified in this review.