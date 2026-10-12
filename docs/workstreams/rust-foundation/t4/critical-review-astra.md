**Verdict: ready after named small changes.** Before planning slices, fix dispatch-memory acquisition, specify `status` consistency, and qualify the progress semantics. The overall architecture is defensible; its liveness and caller-facing guarantees are less settled than its detailed mechanisms suggest.

“Verified” below means established from repository text or code. “Inference” identifies consequences or risks that have not been demonstrated experimentally.

**Effectiveness**

**Verified:** The basic interface fits a program caller: `wait` supplies completion, `status` supplies progress/history, events preserve consequential facts, and logs retain diagnostic detail. Removing per-message persistence and follow streams directly implements the approved requirements. Keep this separation. `docs/workstreams/rust-foundation/t4/requirements.md:8`

**Inference — progress is not yet a dependable cross-vendor model-call history.** The reducer advances on model output after tool results and clears outstanding tools at that boundary. Claude tool requests mark model output; the Codex/OpenCode mappings do not explicitly do so. Tool-only iterations, repeated snapshots, or output interleaved with parallel tools could therefore undercount steps or clear still-running tools. Require fixtures for these cases before declaring the reducer portable. `docs/workstreams/rust-foundation/t4/design.md:253`

**Verified:** Live tokens count completed steps, vendor accuracy remains unprobed, and the envelope may use a different, vendor-reported step count. **Recommendation:** document that history length need not equal envelope `steps`; expose whether progress/counts are measured, inferred, or unavailable. A scope label alone does not establish accuracy. `docs/workstreams/rust-foundation/t4/design.md:276`

**Inference — status can mix different moments.** Durable status comes from SQLite; progress and process liveness are read separately. Define cross-field consistency: pin the selected turn, suppress mismatched progress, and disclose that step rows may lag the snapshot. Otherwise an orchestrator must reverse-engineer apparently contradictory answers. `docs/workstreams/rust-foundation/t4/design.md:412`

**Verified:** A pending `wait` occupies a sequential socket; there are 32 sockets, and waits poll Store every 20 ms. **Inference:** clients need a separate control connection, and 32 waiters could generate roughly 1,600 reads/second while excluding new control connections. Document connection usage and test saturated waiters plus cancellation; the existing vendor-flood test does not establish that case. `docs/workstreams/rust-foundation/t4/design.md:403`

**Most consequential gap: memory admission**

**Verified:** Default permanent charges plus four connection/drive pairs consume 112 MiB. Validation permits a 114 MiB pool, leaving 2 MiB; dispatched blob prompts require a separate charge. `docs/workstreams/rust-foundation/t4/design.md:523`

**Inference:** An acquisition order currently permitted by the design lets four dispatchers hold those charges while each waits for a prompt larger than 2 MiB. None can start and release memory. “Dispatcher waits only for the pool” does not prove acyclicity when dispatchers already hold pool permits.

**Required change:** acquire a complete dispatch reservation atomically, or release partial reservations before waiting. Include prompt-loading working space. Test four recovered large queued prompts at the minimum valid pool, with no external cancellation needed for progress. `docs/workstreams/rust-foundation/t4/design.md:517`

**Simplicity**

The 1,907 lines are large for progress reporting, but approximately 635 are contract amendments, and the original task explicitly includes Wire, Store, ingestion and C1 conformance. This is a broad runtime completion task, not wholly accidental scope growth. `docs/workstreams/rust-foundation/t4/design.md:1178`, `docs/workstreams/rust-foundation/t4/t0.md:34`

I would simplify these mechanisms:

- **Two-phase `list` pagination:** replace it with immutable creation-order pagination **if the owner releases recency ordering**. This removes `stamp`, the second scan and duplicate-return reasoning. Cost: an explicit C1 ordering change; this cannot be done silently. `docs/workstreams/rust-foundation/t4/design.md:897`
- **Fixed 20 ms wait polling:** use bounded backoff. Cost: increased completion latency; benefit: lower idle Store load without introducing subscriptions. `docs/workstreams/rust-foundation/t4/design.md:403`
- **Unused Codex configuration:** defer `memory.codex_shared` and its validation to the Codex task. Cost: an additive configuration change later; its adequacy cannot be established in S1 anyway. `docs/workstreams/rust-foundation/t4/design.md:529`
- **Embedded amendment catalogue:** after approval, consolidate contracts and keep a concise change index. Cost: following references; benefit: fewer competing normative descriptions. `docs/workstreams/rust-foundation/t4/design.md:1178`

Keep raw durability barriers, terminal headroom and carried step rows: they discharge explicit evidence, recovery and durability obligations. `docs/workstreams/rust-foundation/t4/design.md:364`

**Adherence**

**Verified:** Ownership broadly respects Core lifecycle decisions, Adapter normalization, Route decoding, Wire byte handling, Host supervision and Store persistence. The per-turn restriction is explicit and matches round 11. `docs/workstreams/rust-foundation/t4/design.md:69`, `docs/workstreams/rust-foundation/t4/design-r11-decisions.md:6`

One boundary needs clarification: placing `json_limits` in Store does not itself make it available to Routes without another dependency. Specify the permitted facade re-export; do not add Routes → Store. `docs/workstreams/rust-foundation/t4/design.md:1091`, `docs/specs/runtime-contracts.md:44`

The material requirement changes are disclosed, but remain decisions: expanded R2 decoding, weaker R3 accuracy/availability, R4 “last committed” durability, and transaction-cap exceptions. Obtain explicit acceptance and update requirements before treating them as conformance. `docs/workstreams/rust-foundation/t4/reports/T4-0.md:620`

**Implementation risks and first slices**

1. **Memory liveness first:** implement reservation/config validation together; prove the counterexample cannot stall. RSS alone cannot prove progress. `docs/workstreams/rust-foundation/t4/design.md:540`
2. **Raw durability next:** offset assignment, worker failure, deadline expiry and late append/restart are the highest concurrency/crash-risk chain. Build the specified failpoints before integrating full progress. `docs/workstreams/rust-foundation/t4/design.md:690`
3. **Progress vertical slice:** fake tests establish mechanics; captured vendor fixtures and bounded live probes must establish semantic validity separately. `docs/workstreams/rust-foundation/t4/design.md:292`
4. **Disk-pressure slice:** test realistic database shapes, terminal reserve and checkpoint failure. Recording WAL growth without asserting an accepted ceiling is measurement, not a passed bound. `docs/workstreams/rust-foundation/t4/design.md:875`

**Owner-question recommendations**

This covers the carried-forward list; Q-R10-1 moved to OpenCode. `docs/workstreams/rust-foundation/t4/reports/T4-0.md:1407`

| Question | Recommendation and reason |
|---|---|
| Q-R5-1 — transaction cap | **Accept**, with measured whole-command/lane limits; preserving the approved 1 MiB envelope requires the exception. `docs/workstreams/rust-foundation/t4/design.md:706` |
| Q-R5-2 — 256 B request ID | **Accept**; bounded correlation identifiers make response bounds independent of caller input. `docs/workstreams/rust-foundation/t4/design.md:1585` |
| Q-R5-4 — refused step row | **Accept**; failing visibly and carrying rows preserves R4 instead of silently degrading history. `docs/workstreams/rust-foundation/t4/design.md:364` |
| Q-R5-5 — replace F25/F26 | **Accept corrected wording**: progress adds no Store read; complete `status` still requires one. `docs/workstreams/rust-foundation/t4/design.md:1853` |
| Q-R5-7 — fake usage message | **Accept**; useful deterministic reducer input, without implying vendor qualification. `docs/workstreams/rust-foundation/t4/design.md:300` |
| Q-R5-8 — large-request refusal | **Accept provisionally**, after fixing dispatch reservations; explicit overload is preferable to unbounded allocation. `docs/workstreams/rust-foundation/t4/design.md:532` |
| Q-R5-9 — oversized short fields | **Accept**, subject to vendor fixtures; truncating correlation IDs would corrupt meaning. `docs/workstreams/rust-foundation/t4/design.md:142` |
| Q-R5-10 — reply deadline | **Accept**, but start the timer when reply ownership begins, including a blocked first write. `docs/workstreams/rust-foundation/t4/design.md:1590` |
| Q-R5-11 — token accuracy | **Accept unavailable/unverified values provisionally**; keep vendor qualification explicit rather than declaring R3 satisfied. `docs/workstreams/rust-foundation/t4/design.md:276` |
| Q-R5-13 — overflow stop cause | **Accept**; overrun must remain failure even if a successful vendor terminal follows. `docs/workstreams/rust-foundation/t4/design.md:1692` |
| Q-R5-14 — bounded failure summary | **Accept**; explicit truncation and retained evidence preserve honest failure reporting. `docs/workstreams/rust-foundation/t4/design.md:1565` |
| Q-R5-15 — requirement edits | **Accept explicitly**; these change guarantees and decoding scope, not merely wording. `docs/workstreams/rust-foundation/t4/reports/T4-0.md:620` |
| Q-R6-1 — final-text pieces | **Accept**; one Core-owned accumulation meter provides a consistent overflow decision. `docs/workstreams/rust-foundation/t4/design.md:173` |
| Q-R7-1 — disk defaults | **Accept provisionally**; configurable starting values need measurement, not another speculative sizing round. `docs/workstreams/rust-foundation/t4/design.md:892` |
| Q-R7-2 — memory charges | **Accept separate connection/drive lifetimes**, with atomic admission and measured adequacy. `docs/workstreams/rust-foundation/t4/design.md:525` |
| Q-R7-3 — SQLite cache inside pool | **Accept**; the cache consumes daemon memory and belongs in the same accounting policy. `docs/workstreams/rust-foundation/t4/design.md:523` |
| Q-R8-1 — config validation | **Accept after the liveness fix**; arithmetic fit alone does not validate a usable configuration. `docs/workstreams/rust-foundation/t4/design.md:576` |
| Q-R8-2 — invalid-config exit | **Accept**; named failure before Store/socket changes is predictable for automation. `docs/workstreams/rust-foundation/t4/design.md:599` |
| Q-R8-3 — lowered-budget startup | **Refuse startup**; raising the configuration again is simpler than adding a recovery-only daemon mode. `docs/workstreams/rust-foundation/t4/design.md:836` |
| Q-R8-4 — effective limits API | **Accept, without `source`**; callers need effective settings to diagnose refusals. `docs/workstreams/rust-foundation/t4/reports/T4-0.md:1231` |
| Q-R9-1 — WAL overshoot | **Accept the already-approved one-transaction policy; retain the byte-size gate.** The 73 MiB estimate is not a verified ceiling. `docs/workstreams/rust-foundation/t4/design.md:883` |
| Q-R9-2 — Codex shared charge | **Defer implementation/configuration to Codex**; retain the requirement that it consumes the global pool. `docs/workstreams/rust-foundation/t4/design.md:529` |

Read-only inspection at `778752c`; Git remained clean. No tests or vendor probes were run.