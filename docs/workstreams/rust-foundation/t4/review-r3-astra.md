# T4-0 design review round 3: GPT-6 Astra (high)

Review of `wt/t4-0` at `f63252d`, verbatim, local links converted to repo paths.

**UNSOUND for implementation.** The principal ownership fixes are present, but memory accounting, reserved capacity, follow delivery, and slice interfaces still have blocking gaps.

Reviewed clean `wt/t4-0` at `f63252d`, against `bb54709`, both decision records, my Astra round-2 review, the contracts, and relevant source.

**1. Round-2 decisions**

APPLIED: **1, 2, 3, 5, 7, 8, 10, 11, 13, 15**. None wholly MISSING.

| Decision | Status | Remaining gap |
|---|---|---|
| 4 — Memory accounting | PARTIAL | The counting formula misses a real `serde_json` reparsing path; raw-unit and page allocations remain undercharged. Findings 1–2. |
| 6 — `list` | PARTIAL | Filter-change semantics are documented, but termination assumes finitely many concurrent creations. Finding 5. |
| 9 — Lifecycle capacity | PARTIAL | The dedicated Latch slot has no dedicated byte reserve. Finding 3. |
| 12 — Counting visitor | PARTIAL | It bounds the syntactic input, but does not necessarily bound the subsequent `Value` construction. Finding 1. |
| 14 — Vertical slices | PARTIAL | Required callers cross ownership boundaries; S1’s end-to-end scenario lacks its admission plumbing. Finding 7. |

**2. My round-2 findings**

- **Closed at design level:** findings **1–3**: pending writes, in-flight raw-worker failure publication, and one cleanup deadline with explicit adoption.
- **Still open:** finding **4**, memory accounting.
- **Closed as originally raised:** finding **5**, subscription charging/deadline/reset ownership. However, the rewrite introduces terminal-delivery loss below.
- **Partially closed:** finding **6**, `list`: mutable-filter semantics are corrected; unconditional termination is not.
- **Closed for the stated fake-only scope:** findings **7–8**, process evidence/response sizing and applied `cwd`.
- **Slice feasibility remains open:** the DTO ordering improved, but the encoded-result interface creates another dependency conflict.

These are design dispositions, not runtime verification.

**3. Amendments**

- **A21 — Reject as written.** Its premise is wrong: C1’s `turn.ended` carries `state`, `failure`, `stop_reason`, and `cancel`; the envelope is a separate result. Current `TerminalRecord` likewise stores them separately. Reducing the envelope limit therefore does not establish event pageability. Moreover, arbitrary string request IDs defeat “always fits,” as §6.1 itself acknowledges. Keep separate envelope and event bounds, measure the full response, and retain oversized-item refusal. Any envelope-limit reduction must also amend **C1 §5**, omitted from the restatement list. `docs/specs/via-api-v1.md:466`, `docs/specs/via-api-v1.md:529`, `docs/specs/via-api-v1.md:322`.
- **A12 — Accept the revised membership and ordering guarantee; reject the termination proof.** The filter-change example correctly weakens C1’s original reachability promise. Freeze the traversal population before claiming unconditional termination. `docs/specs/via-api-v1.md:304`.
- **Other changes:** accept the dedicated-slot shape, CHECK-based `ended_seq`, Host-evidence liveness, encoded read path, and shared raw/blob worker as mechanisms. Their implementation constraints below still apply. These fit `docs/specs/runtime-contracts.md:694`, `docs/specs/runtime-contracts.md:1019`, and `docs/specs/via-api-v1.md:274`. No basis to reopen the binding A17/A20 rejections.

**4. Defects and concrete fixes**

1. **Blocker — The JSON allocation proof can be bypassed.**  
   `docs/workstreams/rust-foundation/t4/design.md:230` assumes the counted tree is the allocated tree. This workspace enables `serde_json/raw_value`. Its `Value` deserializer recognizes a first key named `$serde_json::private::RawValue` and reparses its string value through `from_str`. Thus a `vendor` value containing an encoded array of over 65,536 elements passes the outer count as a string, then allocates the larger tree. This defeats both the node limit and `tree_charge`. `serde_json-1.0.151/src/value/de.rs:131`.  
   **Fix:** make every peer-controlled `Value` field use construction consistent with the counting visitor, preserving literal keys without hidden reparsing. Add this adversarial case to S1. Prove capacity bounds from the enabled implementation; allocator samples alone are not a general proof.

2. **Blocker — Several allocations still escape their charges.**  
   `docs/workstreams/rust-foundation/t4/design.md:432` assumes every append holds at least 512 B, but `docs/workstreams/rust-foundation/t4/design.md:799` charges only chunk length; only stdout explicitly tops up. Tiny stderr reads invalidate the queue-depth bound. `docs/workstreams/rust-foundation/t4/design.md:1168` charges text lengths but omits `Vec<Box<RawValue>>` backing capacity and does not prevent allocating an over-budget lookahead row.  
   **Fix:** enforce minimum charges at the shared append boundary; meter container capacity; check borrowed row lengths before allocating owned text. Also charge the request envelope’s owned ID/method before §7.3’s envelope decode.

3. **Blocker — Lifecycle traffic can consume the Latch byte reserve.**  
   `docs/workstreams/rust-foundation/t4/design.md:300` reserves one slot but shares all 2 MiB with Lifecycle. Abandoned lifecycle payloads can leave the Latch slot empty but unusable.  
   **Fix:** reserve the maximum failure-resolution payload’s bytes exclusively for Latch, or prove a simultaneous-byte bound. Test retained near-limit payloads after timeouts, not just slot counts.

4. **Important — Normal terminal detection discards undelivered events.**  
   `docs/workstreams/rust-foundation/t4/design.md:1383` applies “discard unsent data” to **every** end reason. A follower can enqueue the terminal page, detect terminal, and immediately discard those events while reporting `terminal`. C1 specifies discard on exhaustion and unsubscribe, not successful completion.  
   **Fix:** retain queued matching events before a terminal notice, within the same absolute deadline; close on timeout. Test a reading client whose writer is briefly behind the scanner.

5. **Important — A12 lacks a fixed completion boundary.**  
   `docs/workstreams/rust-foundation/t4/design.md:1737` explicitly depends on finitely many creations, contrary to “always terminates.”  
   **Fix:** capture an immutable creation watermark and restrict both phases to that initial population; retain mutable stamps for updates. Test continuous creation as well as continuous updates.

6. **Important — Counts are not disjoint merely because locks are coherent.**  
   `docs/workstreams/rust-foundation/t4/design.md:1602` counts unresolved sessions as active and closing sessions separately. One running session being closed gives `open=1, active=1, closing=1`. Saturating subtraction cannot repair that.  
   **Fix:** give closing precedence and count active unresolved sessions excluding the closing set. This is an inherited defect, newly identified here.

7. **Important — Slice boundaries are not implementation-ready.**  
   `docs/workstreams/rust-foundation/t4/s4.md:25` changes Store results to `Box<RawValue>`, but `crates/via-core/src/engine/journal.rs:70` requires `Value`, and S3-owned `crates/via-core/src/engine/control.rs:210` indexes it. S4 cannot complete this change within its ownership. Also, `docs/workstreams/rust-foundation/t4/s1.md:170` requires Public admission/refusal plumbing assigned to S4; Public alone cannot fill all 64 ordinary slots because its cap is 32.  
   **Fix:** introduce a separate encoded public-result API or land shared caller changes before the parallel phase. Move minimal Public plumbing into S1 and specify separate Public/Internal saturation fixtures.

**5. S1–S7 assessment**

- **S3 ∥ S4 ownership:** disjoint as listed, **insufficient for their required changes**, as finding 7 demonstrates.
- **Compilable boundaries:** **not established**. Also specify S1’s compatibility path for existing raw callers before S3 installs staging permits.
- **End-to-end endings:** all seven name one; S1’s is currently infeasible. S5’s prompt-digest echo needs a fake-agent capability and an assigned owner.
- **Self-sufficient briefs:** **not yet**. Resolve those interfaces; explicitly add `instructions`/`require` to S2’s DTO work; define how S5 discards a finished blob after `finish(self)` consumes its writer.

No files changed. No Cargo, Beads, tests, benchmarks, or runtime probes ran. I did not verify timing/RSS, allocator measurements, every failure-path caller, or every F1–F30 artifact mapping.