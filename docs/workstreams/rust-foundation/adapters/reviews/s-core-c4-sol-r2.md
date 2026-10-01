**UNSOUND at `83ebf06`.** Review limited to `85f05c3..83ebf06`; round-2 working-tree changes are excluded.

All locations below refer to that snapshot. `core/` abbreviates `crates/via-core/src/`; other paths are repository-relative.

| Finding | Status | Evidence / remaining work |
|---|---|---|
| F1 | **Partial** | `core/engine/drive.rs:1746`: confirmation commits before acceptance and survives restart. Remaining generation/verification and repeated-confirmation problems are below. The event-field placement is covered by your disposition. |
| F2 | **Fixed** | `core/engine/lane.rs:342`: independent tracker-owned health consumer, including abandonment. Retirement races remain below. |
| F3 | **Partial** | `core/engine/drive.rs:385`: retirement now precedes reservation. Retirement completion is not safely owned across cancellation. The required four-slots-held regression is absent from the final snapshot; the replacement test at `crates/via-core/tests/conformance_core.rs:1322` exercises one session. |
| F4 | **Partial** | `core/engine/lane.rs:306`; `core/engine/drive.rs:1687`: idle draining and warning events exist. Durable idle items still await another turn; closed-list warnings still do not reach the envelope. Handoff losses remain below. |
| F5 | **Not fixed; authorised deferral** | `core/engine/drive.rs:1702`, `:1894`; `core/engine/lane.rs:329`: late terminals are discarded. Routing verdict below. |
| F6 | **Partial** | `core/engine/lane.rs:207`: an unknown explicit ID still becomes current before acceptance. Tombstone eviction and restart also forget previously accepted IDs. |
| F7 | **Fixed** | `core/engine/stop.rs:398`: retained vendor stop reason survives forced finalisation. |
| F8 | **Fixed for the original size defect** | `core/api.rs:2198`; `core/engine/drive.rs:1171`; `core/engine/terminal.rs:652`: 32 KiB inline cap, spill, and expanded maximum-envelope coverage. Spill retry defect below. |
| F9 | **Fixed** | `core/engine/terminal.rs:606`: vendor code and bounded detail preserved. |
| F10 | **Fixed** | `core/engine/progress.rs:335`: overflowing components become unavailable. |
| F11 | **Fixed** | `crates/via-cli/tests/s1_bounds.rs:537`: the in-limit model must succeed. |
| F12 | **Partial** | `crates/via-store/src/runtime/sql.rs:1049`; `core/engine/recovery.rs:439`: stored routing and per-session recovery are wired, but version selection and recovery facts remain incorrect. |
| F13 | **Partial** | New Core boundaries are present at `crates/via-core/tests/conformance_core.rs:675`, `:978`, `:1125`, `:1395`. The after-acceptance mismatch test checks the envelope, **not the typed `AdapterError` and its cleanup/journal evidence**. The abandonment test at `core/engine/tests.rs:3056` directly drops `run_turn`, bypassing Core’s channel claim and handoff. |

The remaining F6 pre-acceptance rule needs an acceptance-specific path: establish the current mapping from `Accepted`, rather than treating every unfamiliar ID as current.

**Defects in the fixes**

1. **Important — `core/engine/lane.rs:357`, `:276`: retirement races with a turn claim.**  
   The monitor reads `running == false`, then closes the driver without an interlock with `claim()`. Another runtime thread can claim and start a dispatch after that read; retirement then force-closes its driver. Receiver ownership does not protect this decision.  
   **Smallest fix:** make dispatch claiming and retirement mutually exclusive under one lane lifecycle state, acquiring the claim before selecting/preparing the driver.

2. **Important — `core/engine/lane.rs:246`: “retirement started” is treated as “retirement completed.”**  
   The atomic flag is set before awaiting `driver.close()`. If the winning dispatcher is dropped during that await, subsequent callers return immediately; the monitor also returns without completing the close. Driver cancellation and publication of `Closed` can be skipped, leaving idle work alive. Concurrent callers likewise do not await completion.  
   **Smallest fix:** give retirement a tracker-owned task and shared completion result; callers await that result instead of interpreting a boolean as completion.

3. **Important — `core/engine/drive.rs:1425`; `core/engine/lane.rs:357`, `:563`: durable backlog can be lost during handoff.**  
   `take_held()` removes the entire deque before the first awaited commit. Dropping execution during that commit drops every remaining item. On failure/replacement, the monitor can stop with admitted items still in its receiver; replacement transfers only `held`, without joining the monitor or draining that receiver. A concurrently finishing `between()` can also append after the transfer. Close/shutdown paths at `lane.rs:625` and `:634` discard this backlog.  
   **Smallest fix:** keep backlog ownership in the lane until disposition, and synchronise monitor termination plus receiver draining before replacement or removal. Preserve unprocessed items on cancellation.

4. **Important — `core/engine/lane.rs:434`, `:450`; `core/engine/drive.rs:1769`: identity state is not scoped to the current generation.**  
   `verified()` means only that **some** generation committed. Reopening on the same driver leaves verification true before the new generation confirms, including a pre-init refusal. `opens()` accepts any different generation as a reopening; it never verifies that the observation belongs to the current connection. Transferred old identities can therefore establish verification on a replacement lane. Repeated same-generation confirmation also updates identity/transcript only in memory, losing a newly supplied transcript on restart.  
   **Smallest fix:** guard confirmations against the driver’s current generation, reset verification on reopening, and persist same-generation metadata updates without emitting another open event.

5. **Important — `core/engine/lane.rs:178`, `:128`: expired ownership becomes unfamiliar ownership again.**  
   After **1,089 accepted turns**, the first ID has neither a mapping nor a tombstone. Its traffic becomes session-level after acceptance, or current before acceptance. `recovered()` also reconstructs no accepted-ID ownership after restart. These violate the promise that previously accepted IDs never become current or null-turn traffic.  
   **Smallest fix:** retain a conservative expired-ID classification after the tombstone bound, or consult durable correlations; reconstruct the necessary ownership on restart.

6. **Important — `crates/via-store/src/runtime/sql.rs:1049`: adapter-version selection does not implement H3.**  
   Normal `Effective` values (`core/api.rs:1778`) contain no `adapter_version`, so the new query always falls back to the receipt. This was confirmed by executing the snapshot’s actual SQL in an in-memory database. Its `state <> 'queued'` predicate also admits cancelled, never-submitted turns rather than selecting the latest started turn. The test at `core/engine/tests.rs:2965` uses identical receipt/current versions and misses both cases.  
   **Smallest fix:** record the version on started turns as H3 requires, and read the latest started record, with receipt fallback only when none exists.

7. **Important — `core/engine/recovery.rs:806`, `:819`: per-session recovery receives incomplete Host evidence without completeness information.**  
   Facts are collected only for anchors whose owning turn remains running; missing reports are omitted. A surviving persistent anchor owned by an earlier completed turn is therefore excluded. A partial all-quiescent subset can make `AdapterSet::recover` report `Dead` even though another session anchor remains unproven.  
   **Smallest fix:** supply all relevant session-anchor facts and prevent `Dead` when the session’s inventory or reports are incomplete.

8. **Important — `core/engine/drive.rs:1127`, `:1187`: spill and terminal commit have separate retry budgets.**  
   A failed spill can consume its retry successfully, after which the terminal write receives another retry. The logical commit therefore gets more than its permitted single retry. A recovered spill failure also disappears from the Store failure hook because `spill()` returns only a boolean.  
   **Smallest fix:** carry spill failure/retry evidence into the terminal operation and share one retry budget and failure-reporting path.

9. **Important — `core/engine/lane.rs:331`, `:580`; `core/engine/drive.rs:1422`: the added backlog weakens admission bounds.**  
   Moving items into `held` frees channel item slots while retaining the items; the pre-turn drain calls `between()` without its `HELD` guard. Replacement additionally transfers permits from the old byte budget while creating a fresh 4 MiB budget. Old held payloads and new admitted payloads can coexist beyond the per-session bound.  
   **Smallest fix:** enforce admission across the entire unhandled backlog, and drain/commit old admitted items before resetting its budget—or retain one shared session budget across replacement.

10. **Minor — `crates/via-routes/src/fake/mod.rs:501`: the decoder admits noncanonical fake IDs.**  
    Numeric parsing accepts aliases such as `fake-turn-01` and `fake-turn-+1`, but Core maps the exact canonical string. These aliases can silently become unrelated session/current traffic.  
    **Smallest fix:** require equality with the canonical `fake-turn-{number}` string after parsing.

The test-only retirement seam at `crates/via-adapters/src/fake/driver.rs:449` is feature-gated; I found no production activation defect.

**Amendment verdicts**

- **(a) SOUND.** Vendor idle shutdown leaves the VIA session idle under C1 §7.1. Running-turn disposition belongs to `TurnEnd`; reconnecting through the next `prepare` preserves C2 lifecycle semantics.
- **(b) SOUND.** Optional current-handshake version evidence supplies C1’s opened/reopened field without inventing a cached version. Transcript storage in session columns agrees with C1 §6.
- **F5 routing: SOUND as an explicit deferral.** The runtime target predates chunk 4 and the fake has no producer. `via-jm4.35` must remain a gate before Codex verification/completion; the deferral does not satisfy AD4 yet.

**f24 leads**

The supplied raw means are **23,062 → 32,441 KiB**, an increase of **9,379 KiB ≈ 9.16 MiB**, with failures **1/10 → 4/10**.

I found no new retention proportional to the flooded text bytes. The added held backlog retains admitted payloads, and replacement can overlap budgets, but f24’s active-turn flood does not normally traverse those idle-backlog paths. Its fixture supplies neither structured output nor a retained terminal, so spill serialization/cloning is not a convincing cause. Monitor tasks, larger futures and changed scheduling remain allocator/layout leads, **not a demonstrated explanation**.

**Could not verify:** fresh tests/gates against an isolated `83ebf06`, historical RED/GREEN executions beyond the supplied artifacts, runtime reproduction of the retirement/handoff races, actual bead dependencies, or RSS causality. Snapshot diff checks and the in-memory SQL checks passed. No files or Git state were changed.