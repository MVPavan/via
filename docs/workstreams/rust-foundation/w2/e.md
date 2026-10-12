# W2-E: turn data path (T1-I5 end to end, evidence gaps, failure classes)

Model: Opus 5.5 high. Follow `../w1/common.md`; report to
`reports/W2-E.md`. Runs in parallel with W1-D.

Owned: `via-wire`, `via-routes`, `via-adapters`, `via-fake-agent`,
`via-store` event records, and in `via-core` only turn driving, event
emission and failure-envelope mapping. W1-D owns daemon stop/shutdown,
Host and CLI daemon wiring; if you must touch a shared file (likely
`via-core/src/engine.rs`), keep the hunk small and name it in the report.

1. **Observations reach Core (T1-I5).** Route now forwards text, tool and
   unknown observations, but Adapter drops them and Core only sees
   acceptance and the terminal. Carry them Route → Adapter → Core and
   commit them as C1 §6 events between `turn.started` and `turn.ended`,
   with dense sequence numbers and raw references. Enforce C2's 256 KiB
   observation payload limit, splitting text as C2 specifies, and its
   16 KiB cap with truncation marker for unknown payloads (C2 A1). Tests: an
   end-to-end `via` run asserting payloads, order and raw references; a
   Route-level test that fails if `forward` stops sending (the current
   Route tests only exercise `Phase::advance`).
2. **No silent evidence gaps.** (a) Wire removes a complete stdout line
   before checking the 1 MiB cap, so on overflow the drain cannot record
   the offending bytes; keep them until validated or recorded. (b) A raw
   append error during `drain_to_eof` returns early and Route ignores the
   result, so later bytes are neither drained nor marked. Keep a bounded
   discard drain and carry an incomplete-evidence indication, as the
   contracts name it, through the failure path. Regressions: an oversized
   line (retained raw prefix, incomplete status) and a failing raw append.
3. **Failure classes.** `engine.rs` maps every `AdapterError` to
   `failure.class = "protocol"`; process exit, overflow, deadline,
   transport and Store failures lose their cause. Keep typed causes across
   the Route and Adapter boundaries and map each to its C1 §8.2 class.
   Add end-to-end failure-envelope assertions for every class the fake
   agent can produce.

Out of scope: per-pipe reader tasks and the per-session byte budget
(`via-jm4.7.8`), moving capabilities into the adapter, stop/shutdown.
