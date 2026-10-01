**UNSOUND.** Five findings are resolved; teardown safety and cache identity semantics still have Important defects.

Status at `287e043`:

| r1 | Status | Evidence |
|---|---|---|
| #1 Execute permissions | **Resolved** | `crates/via-adapters/src/instance.rs:37`: kernel access check after regular-file check. |
| #2 Daemon kill fallback | **Partial** | `crates/via-cli/tests/s_launch.rs:162`: fallback exists, but N1–N2 remain. |
| #3 Diagnostic keys | **Resolved** | `crates/via-adapters/src/config.rs:337`, `crates/via-cli/src/server/config.rs:43`: escaping and bounded display cover both parsers. |
| #4 Embedded NUL | **Resolved** | `crates/via-adapters/src/config.rs:409`: explicitly rejected. |
| #5 Timeout classification | **Resolved** | `crates/via-cli/tests/s_launch.rs:130`: typed timeout retains cleanup notes. |
| #6 Cache retention | **Retention fixed as ruled; regression introduced** | `crates/via-adapters/src/instance.rs:155`: sweep works; path-keyed versions cause N3. |
| #7 Fixture Debug | **Resolved** | `crates/via-adapters/src/config.rs:179`: fixture paths redacted; complete-fixture test added. |

New defects:

1. **Important — ownership scanning and signalling can refer to different process generations.**  
   [crates/via-cli/tests/s_launch.rs:245](../../../../../crates/via-cli/tests/s_launch.rs#L245). The scan returns bare PIDs; start time is captured afterward. Code permits this interleaving: scan identifies sandbox process A; A exits; its PID becomes unrelated process B; the subsequent stat captures B’s start time; pidfd opening and recheck both succeed for B, which receives SIGKILL. The pidfd protects the later window, leaving the earlier ownership match unbound.  
   **Smallest fix:** open the pidfd before verifying sandbox membership, then retain it through signalling; alternatively pin the `/proc` generation while reading both membership and identity.

2. **Important — fallback grants fresh deadlines after the teardown deadline expires.**  
   [crates/via-cli/tests/s_launch.rs:222](../../../../../crates/via-cli/tests/s_launch.rs#L222). `stop_daemons` can consume all ten seconds before fallback begins. Each survivor then receives another five seconds, serially. This exceeds runtime §11.2’s single deadline and leaves no time for anchor cleanup. The executed SIGSTOP test’s artifact confirms anchor snapshot failure with “no time left.”  
   **Smallest fix:** reserve fallback time within the existing teardown budget and pass its absolute deadline through scanning, signalling and waiting. The accepted test duration does not resolve this contract conflict.

3. **Important — path-keyed versions lose C2’s identity-based semantics.**  
   [crates/via-adapters/src/instance.rs:137](../../../../../crates/via-adapters/src/instance.rs#L137). I reproduced three failures: a symlink alias with the same identity returns `None`; observing a newer version through that alias leaves the original path’s version stale; a delayed handshake from an older binary overwrites the path slot and makes the newer identity’s previously observed version disappear. C2 requires the last version seen **for that binary identity**. This remains dormant while unwired.  
   **Smallest fix:** preserve identity-keyed version records, with bounded path/instance references controlling retention. Add alias and delayed-handshake regression tests.

4. **Minor — observation failures can be reported as process absence.**  
   [crates/via-cli/tests/s_launch.rs:211](../../../../../crates/via-cli/tests/s_launch.rs#L211). After observing a live process, `process_stat(...)=None` makes `gone()` return true, although that helper also returns `None` for unreadable or malformed stat data. Fault injection using the committed function returned `Ok(true)` for precisely that case. The same ambiguity occurs at line 247; line 192 also discards every pidfd-open error.  
   **Smallest fix:** distinguish disappearance from observation errors; report other failures as unknown or failed.

Verdicts on the concerns:

| Concern | Verdict |
|---|---|
| Cargo.lock’s additional dependency line | **Sound.** Required by the approved dependency. Locked builds, dependency checks and layer checks pass. Merge reconciliation remains the coordinator’s responsibility. |
| Root branch runs only under root | **Sound conditional coverage.** The expectation is appropriate; I ran as UID 1000 and did not verify the root branch. |
| Ten-second test and retained failed sandbox | **Duration and retention are accepted.** Retaining uncertain evidence is correct. The additional deadlines remain defect N2. |

Independently verified: **610 default tests**, **29 selected failpoint tests**, **both doctests**, formatting, clippy with failpoints, dependency checks, layer checks and diff whitespace checks. Git remained clean at `287e043`.

I did not reproduce actual PID reuse or real `/proc` read failures; N1 follows from the permitted interleaving, and N4 used fault injection. Root, full failpoint, S1, musl and release gates were not independently rerun. RED→GREEN history and the eventual merge with S-CORE chunk 5 remain unverified.