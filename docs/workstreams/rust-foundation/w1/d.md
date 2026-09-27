# W1-D (after A–C merge): shutdown design, force stop (T1-I7), debug RPC removal (T1-I4)

Model: Opus 5.5 high. Follow `common.md`. Starts only after W1-A, B and C
are merged into `rust-foundation`.

1. Integrate `../checkpoint/shutdown-ownership-seam.md` (reviewed PASS by Sol,
   `../checkpoint/shutdown-ownership-sol-review.md`) into
   `docs/specs/runtime-contracts.md`, then implement it.
2. **T1-I7.** `daemon/stop --force` only bypasses admission refusal and then
   waits for active work; force must enter final shutdown immediately.
3. **T1-I4.** Remove the unauthorised `daemon/verify_cleanup` RPC,
   `Core::verify_cleanup` and the `__via_verify_cleanup` CLI; prove cleanup
   after daemon-first death through the approved outer snapshot/control
   path in the test harness instead.
