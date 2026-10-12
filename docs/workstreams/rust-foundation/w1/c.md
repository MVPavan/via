# W1-C: canonical events (T1-I5, Core/Store side) and receipt/envelope shapes (T1-I6)

Model: Opus 5.5 medium. Follow `common.md` in this directory.

Owned paths: `crates/via-core/src/engine.rs`, response/receipt/envelope/event
types in `crates/via-core/src/api.rs` (W1-B owns request types there),
event persistence in `crates/via-store/`, and new tests.

1. **T1-I5.** Artifact events carry the C2-only `turn.accepted` and an
   invented `turn.terminal` instead of C1 `turn.started` / `turn.ended`, and
   lack the canonical common fields. Emit C1 events with every required
   common field (C1 §6: type tag, dense per-session seq, turn, late, raw_ref)
   while keeping durable C2 evidence internally.
2. **T1-I6.** Receipts and envelopes omit required current-method metadata
   and use incorrect model and raw-span shapes. Make them match C1 §3 and §5
   for the methods that exist: capabilities that honestly say supported /
   partial / unsupported, effective settings, timestamps, usage and cost
   with explicit unavailable or null semantics, model, raw spans.

Update existing tests that asserted the old names or shapes; say so in the
report. Do not touch request parsing (W1-B) or the stop/shutdown path (W1-D).
