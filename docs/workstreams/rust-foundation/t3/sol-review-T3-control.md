GPT-6 Sol high Task 3 review, part control (`724ef3d..9efbab8`, local `rust-foundation`).

## Verdict: SOUND WITH CHANGES — Task 3 Part A

The merged paths preserve turn ownership across cancel, close, drain, force, and final shutdown in the cases I traced. Close publication and the final-shutdown fence are ordered under `admission`; retained watches cover late subscribers; dispatcher joins precede Host reconciliation. I found two lifecycle gaps to fix now.

| Rank | Finding | Concrete fix | Owner |
|---|---|---|---|
| **Major** | **Force cleanup can receive a fresh three-second budget after the force signal.** Host computes the deadline when its watcher runs at `crates/via-host/src/host.rs:184`. If that task is delayed while a run loop is blocked, an armed group can remain live beyond the design’s three seconds *from notification*. | Record a monotonic deadline when Core raises force and pass that same deadline to Host’s snapshot and late-registration paths. | **Fix now — Task 3** |
| **Major** | **Explicit plain `via daemon stop` cannot stop an idle version-mismatched daemon.** The CLI invokes it with `auto_start: false` at `crates/via-cli/src/main.rs:270`, and `crates/via-cli/src/client.rs:478` returns the `version_mismatch` reply before the Store-identity check and permitted plain stop. The auto-start path handles that handshake, but the explicit stop verb does not. | For an explicit plain stop, verify the reported Store path and send the permitted stop on the mismatched connection; keep it from auto-starting a replacement. | **Fix now — Task 3** |
| **Minor** | **`daemon/status` omits required `started_at`.** The response at `crates/via-cli/src/server/dispatch.rs:212` lacks the field required by C1 §3.14 and design amendment A15. | Add the daemon start timestamp to the status DTO and its conformance check. | **Task 4, `via-jm4.7.8`**, which owns daemon-status parity |

Plain stop, drain, force, and idle exit otherwise follow the specified admission and shutdown modes in the inspected paths. A cancel or close of a running turn uses the slot’s stop order; force transfers an unfinished turn to final shutdown, where Host evidence precedes its terminal. I found no additional lock-order inversion or lost slot wake in those paths. Exit 75 on daemon-lock contention, 0/4 on shutdown disposition, and 130 on foreground SIGINT are wired as specified.

**Limits:** This was source inspection of refs and the focused Part A paths, not execution or a line-by-line audit of the entire diff. I ran no Beads, Cargo, or tests and made no checkout or edits. The working tree already had two modified `.beads` files; the review left them untouched.

