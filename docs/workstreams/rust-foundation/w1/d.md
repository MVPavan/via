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
4. **`daemon/stop` drain.** The strict DTO accepts `force` but not C1's
   `drain`; add it with its C1 behavior (Sol review of W1-B).

Runs in parallel with W2-E (`../w2/e.md`), which owns the turn data path
(Wire, Route, Adapter, event emission and failure-envelope mapping in
Core). Keep shared-file hunks (likely `via-core/src/engine.rs`) small and
name them in the report, `../w2/reports/W1-D.md`.
