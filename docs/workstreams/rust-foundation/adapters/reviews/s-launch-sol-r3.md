**UNSOUND.** N1, N3 and N4 are resolved. N2 is partial: waiting uses the shared deadline, but starting work and signalling do not.

All locations below refer to **f33e810**.

| Finding | Status | Evidence |
|---|---|---|
| N1: process ownership race | Resolved | [via-cli/tests/s_launch.rs:345](../../../../../crates/via-cli/tests/s_launch.rs#L345): pidfd opened before membership reads; fdinfo and stat PID checked before signalling through that fd. |
| N2: teardown deadline | Partial — Important | [via-cli/tests/s_launch.rs:416](../../../../../crates/via-cli/tests/s_launch.rs#L416): expired cutoff still permits pinning and signalling. |
| N3: identity-keyed versions | Resolved | [instance.rs:136](../../../../../crates/via-adapters/src/instance.rs#L136): identity keys, shared aliases, independent late handshakes and eviction at 16 entries. |
| N4: errors mistaken for absence | Resolved | [via-cli/tests/s_launch.rs:351](../../../../../crates/via-cli/tests/s_launch.rs#L351): observation failures become errors/`unknown`; disappearance is handled separately. |

Remaining and new defects:

1. **Important — N2 residual: fallback work continues after its cutoff.**  
   [via-cli/tests/s_launch.rs:416](../../../../../crates/via-cli/tests/s_launch.rs#L416), signalling at line 424. `kill_by` gates only polling. A probe using the committed helpers with an already-expired cutoff still killed its owned helper with SIGKILL. Multiple candidates or slow reads can therefore consume the anchor cleanup reserve; recording an overrun afterward does not preserve that reserve.  
   **Smallest fix:** check the cutoff before each candidate and immediately before signalling; stop starting fallback work once expired and record the deadline failure.

2. **Minor — partial helper setup leaks the first child.**  
   [via-cli/tests/s_launch.rs:786](../../../../../crates/via-cli/tests/s_launch.rs#L786). If the second `spawn()` fails, `Children` has not been constructed. Dropping the first `Child` neither kills nor reaps it. An injected second-spawn failure confirmed the first helper remained alive; the review probe subsequently killed and reaped it.  
   **Smallest fix:** construct an empty guard and push each successfully spawned child immediately.

3. **Minor — helper failure paths use unbounded waits.**  
   [via-cli/tests/s_launch.rs:816](../../../../../crates/via-cli/tests/s_launch.rs#L816), also line 763. The positive kill test calls `wait()` before checking the fallback result. An unsuccessful kill can delay the assertion until the helper’s 60-second lifetime ends. Guard cleanup also ignores kill errors before blocking in `wait()`.  
   **Smallest fix:** inspect the fallback result first and use bounded reaping for both waits.

4. **Minor — stale cache test documentation.**  
   [via-adapters/tests/s_launch.rs:432](../../../../../crates/via-adapters/tests/s_launch.rs#L432) still describes path-keyed versions replacing old identities. That contradicts N3’s implementation.  
   **Smallest fix:** remove the version claim from this refusal-retention test’s comment.

Verdicts on the concerns:

- **Local duplication:** acceptable for this slice. Keep the specialized phase budgeting local until another caller needs it. Extraction is not required to fix the deadline defect.
- **fdinfo `Pid:` dependency:** sound for the repository’s Linux 5.15 baseline; that kernel emits the PID and `-1` when its task is absent. Missing or malformed data correctly fails closed. [Linux 5.15 implementation](https://github.com/torvalds/linux/blob/v5.15/kernel/fork.c#L1684)
- **System `sleep` helpers:** suitable with cleared environments, but the claim that every helper is killed and reaped is incomplete because of findings 2 and 3.

Verified: **613 workspace tests**, **32 focused tests with failpoints enabled**, both doctests, formatting, Clippy with and without failpoints, layer checks, and `cargo deny` passed. Dependency checks emitted duplicate-version warnings. Tracked files and Git state remained unchanged.

I did not independently reproduce the full reported failpoint/S1/musl gates, execute on Linux 5.15 or as root, or force actual PID reuse. Runtime checks ran on Linux 6.6 WSL2; ownership mismatch coverage used the supplied test seam.