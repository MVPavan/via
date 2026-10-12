# Contract specs review r1 (C1 + C2 draft 1): GPT-6 Astra medium

Run: `codex exec -s read-only -m gpt-6-astra -c model_reasoning_effort=medium`, 2026-09-26. Verbatim output; local links shown as code spans. Probe result paths are local and private (summaries: `docs/workstreams/rust-foundation/session-handoff.md` §6). Every finding was fixed or turned into an owner decision in draft 2.

**Verdict: SOUND WITH CHANGES.** The architecture remains viable, but C1/C2 are not ready to implement as binding contracts. The blockers concern permission enforcement, durable submission, handle recovery, and backpressure.

References: C1 (`docs/specs/via-api-v1.md`), C2 (`docs/specs/adapter-contract.md`), owner decisions (`scratchpad/rust-foundation/decisions.md, local; now `docs/brainstorms/README.md` §15`). Probe conclusions below apply to the recorded runs, not all vendor versions.

1. **Blocking — C1 §4; C2 §6: bound enforcement is underspecified.** Codex `dangerFullAccess` has no network restriction field, so `full + network:false` cannot use the documented mapping faithfully. Claude’s restricted bounds remain unverified, and passthrough options have no precedence rule preventing permission overrides. **Fix:** define supported bound combinations, refuse unenforceable combinations, and reject vendor options that conflict with canonical bounds or never-ask policy.

2. **Blocking — C1 §§2, 3.2, 9: idempotency cannot recover the promised handle.** A random handle stored only as a hash cannot be returned after restart when the original receipt was lost. Persisting a receipt containing it contradicts hash-only storage. **Fix:** choose an explicit recovery design before v1—for example, caller-generated authority persisted only as a hash—or revise the storage/security promise. Define key scope, retention, request equivalence, and atomic key-to-receipt persistence.

3. **Blocking — C1 §§7.2–7.5; C2 §2: crash recovery lacks a durable submission boundary.** Persisting `queued`, sending input, then recording vendor acceptance leaves a crash window where accepted input looks safely queued and can be dispatched again. **Fix:** durably record a submission attempt before vendor I/O; recover ambiguous attempts as `unknown`. This can be internal metadata without adding a public turn state. Only provably unsubmitted queued turns may dispatch automatically.

4. **Blocking — C2 §§2, 7.12/A1: bounded buffers cannot guarantee indefinite lossless draining.** If Core stops consuming, the adapter channel and L5 buffers eventually fill. Blocking the adapter may also delay automatic declines and interrupt processing. **Fix:** specify finite spool limits and an explicit overflow/failure policy, keep control traffic serviceable, and define shared-connection consequences. “Never drops, never blocks pipe reads” is not implementable under unbounded producer output.

5. **Major — C1 §§7.3–7.4; C2 §§2, 6: cancellation acknowledgement is not tool quiescence.** P2 (`scratchpad/probes/out/p2_codex_interrupt/20260926T090235.112561Z/result.json`) recorded `interrupted` while the shell’s `sleep` survived five seconds. Automatically dispatching the next turn can therefore overlap surviving work, potentially across a bound change. **Fix:** retain `acknowledged` as protocol acknowledgement, expose cleanup certainty separately, and specify whether dispatch is blocked until quiescence is established. Never imply that acknowledgement proves all side effects stopped.

6. **Major — C1 §§7–8; C2 §§2, 4: lifecycle and failure classification are incomplete.** There is no clear transition for opening/submission failure before acceptance; deadline cancellation can imply both `failed` and `cancelled`; server loss can imply `failed` or `unknown`; terminal turns that never received cancellation have no “recorded outcome” to return. **Fix:** supply a state/event disposition table with evidence-based precedence, explicit already-terminal cancellation results, and mappings for every C2 error. Once a receipt is accepted, later failures must resolve that turn rather than retroactively become request errors.

7. **Major — C2 §2: the Rust trait-object sketch is invalid.** `Box<dyn AdapterSession>` cannot dispatch its methods returning `impl Future`. Furthermore, an outstanding `start_turn(&mut self)` can prevent interrupting a submission awaiting acceptance. **Fix:** use boxed `Send` futures or closed-enum dispatch, and define how control commands remain available during submission and interruption. See Rust’s [dyn-compatibility rules](https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility).

8. **Major — C2 §§2, 4, 7: recovery and idle-session reopening lack necessary interfaces.** `recover` returns a session but accepts no event sender; the only event hookup is `start_turn`, which recovery must not call. Reopening an idle vendor session after process shutdown has no explicit stored vendor-id input in `SessionSpec`. **Fix:** establish a session event stream independently of submission, provide explicit reopen/recovery identity, and correlate recovered events to their original turn.

9. **Major — C1 §§5, 7.5; C2 §§6, 8/B3: recovery claims exceed evidence.** C2 treats Codex rejoin as supported while B3 still asks whether in-flight notifications reach the new connection. P3 (`scratchpad/probes/out/p3_codex_disconnect/20260926T090235.112378Z/result.json`) also shows that closing stdin killed this stdio server and its tool. **Fix:** gate live recovery by tested transport/version behavior; preserve D9’s unresolved transport choice. Define how late evidence revises an `unknown` envelope and notifies clients whose follow subscription already ended.

10. **Major — C2 §§2–3: per-turn bounds conflict with the shared-server key.** The adapter may change a thread’s bound while its server remains keyed by the original bound. That contradicts the stated separation of sessions by bound. **Fix:** surface this owner-decision conflict explicitly: either prove identity-preserving migration to an appropriately keyed server, refuse those changes, or obtain a revised server-sharing rule. Do not silently reinterpret the key.

11. **Major — C1 §§3.3, 4, 7.3; C2 §6: queued parameter inheritance is ambiguous.** If queued turn 2 widens the bound, turn 3 inherits it, and turn 2 is cancelled, the contract does not say whether turn 3 remains widened. Other per-turn omission/reset rules are missing. Claude’s effort/schema/step-limit mapping also uses process-start flags. **Fix:** define inheritance at acceptance, cancellation effects, and explicit reset/null behavior; record frozen effective values in receipts. Specify verified process restart or control mechanisms for every mutable Claude parameter.

12. **Major — C1 §§3.6, 3.14, 7.1: close/drain behavior leaves races.** Resume can arrive while close awaits vendor shutdown; an `idle` session can still contain queued work; drain does not specify whether accepted queued turns run or terminate. The idle-policy row simultaneously says “closed” and “stays idle.” **Fix:** define an internal closing/draining admission gate, disposition of all accepted turns, and separate vendor-process idle shutdown from session closure.

13. **Major — C1 §§3.11, 6.3: follow and pagination can lose events.** A first page limited to 200 events has no defined handoff to notifications when more history exists. Ordering, cursor/filter semantics, slow followers, and retention gaps are unspecified despite the “nothing is lost” promise. **Fix:** define an atomic replay-to-live boundary, scan cursor semantics, bounded follower behavior, and explicit pruned-history errors. Give `list` a stable ordering and cursor contract too.

14. **Major — C1 §§6.2–6.3; C2 §4: terminal-event ownership and attribution conflict.** C1 assigns `turn.ended` to Core; C2 requires adapters to emit exactly one terminal event, including failures owned exclusively by Core. Session-level events have no defined nullable turn identity. **Fix:** adapters report observations; Core alone commits the terminal transition, envelope, and public `turn.ended`. Define session-level attribution and disposition of late events from previous turns.

15. **Major — C1 §§1, 3.1, 6; C2 §5: serialization is not one consistent contract.** C1 flattens verb capabilities; C2 nests them under `verbs` and adds unsupported reasons. `#[non_exhaustive]` does not implement unknown-value deserialization, and Serde’s `other` attribute only supports a unit variant, not payload preservation. **Fix:** specify canonical wire DTOs, required/default/null fields, and explicit unknown-value decoding—including enums inside otherwise known events. See [Serde’s variant attributes](https://serde.rs/variant-attrs.html).

16. **Major — C2 §§6–8/A3: Claude queue, steer, and interrupt need separate treatment.** P5 (`scratchpad/probes/out/p5_claude_busy_input/20260926T090235.112389Z/result.json`) merged busy input into one result; forwarding a queued `resume` early would violate one-prompt/one-turn semantics. Its interrupt produced `error_during_execution` / `aborted_tools`, which the generic `is_error → vendor_error` mapping would misclassify. **Fix:** keep resume input in Core until dispatch; define separately gated steer semantics; recognize cancellation-specific terminal evidence before generic errors. One successful probe is not blanket conformance.

17. **Major — C1 §5; C2 §6: usage scope cannot be assigned by route alone.** P5’s second Claude result has lower input/output counts than the first while cost increases, contradicting a blanket cumulative label for result usage. Codex’s schema names `.last` but does not establish that it aggregates an entire VIA turn. **Fix:** verify each field’s accounting interval separately; distinguish cumulative cost from token scope, and label unavailable rather than asserting an unproven turn total.

18. **Major — C1 §§3.2–3.4: lost mutation responses remain unsafe to retry.** Spawn has proposed deduplication, but retrying a timed-out resume can enqueue another identical turn; retrying steer can inject twice. JSON-RPC request IDs provide no declared durable deduplication. **Fix:** extend operation-key semantics to resume and steer, or explicitly define an operation lookup/reconciliation mechanism before clients implement automatic retries.

19. **Major — C1 §§2, 3.2/P1: the default CLI flow loses mutation authority.** Foreground spawn prints only an envelope, the envelope excludes the handle, and the CLI stores nothing. That leaves the ordinary caller unable to resume or cancel its session. **Fix:** define a receipt/output channel that delivers the handle before waiting, with an unambiguous machine-readable format and foreground-interruption behavior.

20. **Minor — C1 §5; C2 §4: one raw-log span cannot represent the lifecycle promised.** A turn can span reconnections, and a contiguous interval on a shared connection includes unrelated sessions. **Fix:** represent raw references as connection-specific spans or make event references authoritative; never treat the envelope’s broad interval as a session-safe log extraction range.

**Proposed decisions**

| Decision | Assessment |
|---|---|
| P1 | Agree with method names; revise foreground output to preserve the receipt and handle. |
| P2 | Agree with stateless CLI transport, provided handle delivery is fixed; support a file/stdin source to avoid command-line exposure. |
| P3 | Agree within the explicitly stated same-OS-user trust boundary. |
| P4 | Disagree as written: hash-only storage cannot replay the random handle. |
| P5 | Agree with fixed session parameters; complete per-turn defaults, resets, and Claude mappings. |
| P6 | Agree with a configurable initial limit of 8 and cancelling queued successors; also prevent fresh dispatch while predecessor execution remains unresolved. |
| P7 | Agree; distinguish protocol cancellation from process cleanup. |
| P8 | Agree with stable codes/kinds; complete C2 mappings and lifecycle precedence first. |
| P9 | Agree; “removal only in v2” must govern regardless of the one-minor minimum. |
| P10 | Agree for the selected Unix platforms. |
| A1 | Disagree: specify bounded overload behavior instead of an impossible unconditional lossless guarantee. |
| A2 | Disagree as a safety gate: same-major versions can change protocols and bounds; use tested ranges and capability-specific refusal rules. |
| A3 | Revise: retain version/probe gates, incorporate P5, and assess steer and interrupt independently. |
| A4 | Agree; reject unenforceable network restrictions and leave external sandboxing to D9. |
| A5 | Agree conditionally on verified protocol shapes; do not treat permission declines as sandbox enforcement. |
| A6 | Agree as a configurable initial ceiling; ensure declines remain serviceable under overload. |

C1’s additional recommendations: **Q1 agree** with session-wide follow; **Q2 agree** with schema validation after fixing dialect/resource limits; **Q3 agree** with latest turn resolved at call acceptance; **Q4 agree provisionally** with configurable 15-minute process shutdown; **Q5 agree** with catalog-only describe. **D9 remains open.**

Read-only review completed; no files edited. Checked the decisions, contracts, local schema, saved probe results and selected raw messages, plus Rust/Serde documentation. No new vendor probes or build tests were run; existing worktree changes were preserved.