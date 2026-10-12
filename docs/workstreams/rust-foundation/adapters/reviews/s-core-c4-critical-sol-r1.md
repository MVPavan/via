**UNSOUND.** I found 12 Important and 3 Minor findings. Locations refer to **0e86995**. Findings involving `Recovery::Resumed` are legal C2 paths that the current fake adapter does not exercise; their runtime outcomes were not reproduced.

1. **Important — Turn drains can run indefinitely.**  
   [drive.rs:1509](../../../../../crates/via-core/src/engine/drive.rs#L1509), [drive.rs:1625](../../../../../crates/via-core/src/engine/drive.rs#L1625). Both drains continue until temporary channel emptiness while admission remains open. A persistent adapter producing observations fast enough can prevent that emptiness indefinitely. Before the turn, this prevents even constructing/polling `run_turn`; after its return, it prevents terminal publication. Checking controls between items does not terminate either drain. **Smallest fix:** drain a finite prefix defined by a handoff/return watermark, and leave subsequently arriving items to the actor’s next phase.

2. **Important — Resumed recovery has two independent session sequence owners.**  
   [recovery.rs:518](../../../../../crates/via-core/src/engine/recovery.rs#L518). `recover_session` installs and starts a resumed lane before `recover_turn`, but recovery constructs a separate `Head` from the previously read history. The actor writes through `slot.head`. An identity or durable observation committed between the history read and recovery’s commit makes recovery’s sequence stale; conversely, recovery can stale the actor’s head. This can fail startup with a sequence conflict. **Smallest fix:** use the shared session head for recovery writes, or defer actor consumption until recovery has transferred that head.

3. **Important — Restart close bypasses the lane drain barrier.**  
   [close.rs:468](../../../../../crates/via-core/src/engine/close.rs#L468). `finish_restart_close` performs an absence check and commits `Closed` without closing/joining a resumed lane. A recovered closing session can therefore close while its actor still owns admitted durable observations. Subsequent session writes are refused because the session is closed. Host absence does not prove channel drainage. **Smallest fix:** apply the same lane-close completion barrier used by live close; unfinished drainage must prevent `Closed`.

4. **Important — An uncertain retirement journal write loses the information required to latch Store failure.**  
   [fake/driver.rs:580](../../../../../crates/via-adapters/src/fake/driver.rs#L580), [lane.rs:752](../../../../../crates/via-core/src/engine/lane.rs#L752). Retirement combines unproved cleanup and uncertain journal writes into the unit `RetirementUncertain` cause. Core merely remembers that cause and retires the lane. The earlier logical turn reports `journal_uncertain: false`, so it cannot propagate the later write failure. Runtime §7 requires **every uncertain write** to latch daemon Store failure. **Smallest fix:** preserve the journal outcome in retirement health/reporting and send it through Core’s Store-failure hook, separately from cleanup uncertainty.

5. **Important — Vendor turn ownership survives connection-generation changes.**  
   [lane.rs:811](../../../../../crates/via-core/src/engine/lane.rs#L811). Ownership maps and tombstones reset when the lane is replaced, but not when the same driver opens another connection generation. The driver advances its generation for `NeedsConnection`. A legal reused opaque ID in the new generation consequently collides with an old mapping or tombstone; unrelated generations also accumulate toward exhaustion. This conflicts with the coordinator’s per-generation ownership ruling. **Smallest fix:** scope ownership to connection generation and drain/dispose the old generation before switching its ownership state.

6. **Important — Tombstone exhaustion does not stop the active nonterminal turn.**  
   [lane.rs:325](../../../../../crates/via-core/src/engine/lane.rs#L325). Exhaustion sets `overflowed`, adds another mapping, and returns success. Acceptance therefore proceeds normally. The actor checks the failure only after its job finishes, so a vendor that hangs after acceptance can remain active until its ordinary deadline. C2 requires exhaustion to escalate to connection failure. **Smallest fix:** return a distinct overflow result and promptly interrupt/resolve the affected nonterminal turn through the typed overflow path.

7. **Important — Discarded late progress resets the successor’s idle timer.**  
   [drive.rs:1528](../../../../../crates/via-core/src/engine/drive.rs#L1528). The idle deadline resets before attribution. Progress belonging to an older, expired, or genuinely unseen vendor turn can therefore keep the current stalled turn alive, even though `observe` subsequently discards that progress or treats it as session-level. Late Codex tool traffic can exercise this. **Smallest fix:** reset idle only after validating current-turn attribution and acceptance.

8. **Important — Completed historical sessions retain resident lane actors indefinitely.**  
   [lane.rs:681](../../../../../crates/via-core/src/engine/lane.rs#L681), [lane.rs:1074](../../../../../crates/via-core/src/engine/lane.rs#L1074). Normal turn completion retires the dispatch slot but leaves the healthy lane registered and its actor waiting. Sequentially creating sessions grows resident actors, drivers, ownership state, and budgets with historical session count. Runtime §8 explicitly prohibits one resident actor per historical/idle session. This is separate from the excluded allocator-arena issue. **Smallest fix:** evict/drain idle lanes without live connection obligations and bound retained live lanes.

9. **Important — Recovery paging still accumulates historical inventory in memory.**  
   [recovery.rs:813](../../../../../crates/via-core/src/engine/recovery.rs#L813). `Reconciled::add` retains every anchor report in `facts`, including completed turns, before checking `turn_running`; missing reports similarly grow `unreported`. Thus bounded pages are collected into an unbounded aggregate, even when there are no unfinished turns to recover. A time deadline is not a fixed holder bound. **Smallest fix:** retain only bounded recovery-relevant facts, stream historical reconciliation, and mark evidence incomplete if a retention cap is reached.

10. **Important — Recovery confuses legitimate vendor IDs with synthetic acceptance tokens.**  
    [recovery.rs:734](../../../../../crates/via-core/src/engine/recovery.rs#L734). Vendor IDs are persisted verbatim, while missing IDs use `token:<number>`. Recovery treats every value beginning with `token:` as synthetic. C2 reserves no such prefix, so a legitimate ID such as `token:1` disappears after restart. **Smallest fix:** persist an unambiguous tagged representation distinguishing vendor IDs from acceptance tokens.

11. **Important — Recovered envelopes discard persisted identity and transcript evidence.**  
    [recovery.rs:527](../../../../../crates/via-core/src/engine/recovery.rs#L527), [terminal.rs:48](../../../../../crates/via-core/src/engine/terminal.rs#L48). Recovery constructs a default vendor record and calls an envelope helper that supplies another default. Consequently, an unfinished turn recovered after identity/transcript persistence receives null identity/transcript fields, while `logs` can return the stored evidence. C1 §5 requires the evidence transcript to match `logs`. **Smallest fix:** restore the known persisted identity/transcript into the recovered envelope without inventing unavailable measurements.

12. **Important — The normal observation loop lacks the required hard 128-item control check.**  
    [drive.rs:1525](../../../../../crates/via-core/src/engine/drive.rs#L1525). The pre-turn and final drains have explicit control checks, but the running loop relies on unbiased `tokio::select!`. Randomized fairness does not establish runtime §8’s maximum of 128 ready items before another control/deadline check. **Smallest fix:** count normal-loop observations and explicitly service pending controls/deadlines at that bound.

13. **Minor — Acceptance observations undercharge the byte budget.**  
    [observation.rs:526](../../../../../crates/via-adapters/src/observation.rs#L526). `item_cost` ignores `Acceptance.vendor_turn_id`. The fake normalizer retains separate ID strings in both the item and acceptance, but only the outer string is charged. This violates the documented cost over every retained variable-size field. **Smallest fix:** charge the acceptance ID independently or retain only one copy.

14. **Minor — Hash-only tombstones cannot preserve exact opaque-ID ownership.**  
    [lane.rs:376](../../../../../crates/via-core/src/engine/lane.rs#L376). Tombstones retain only a 64-bit hash. Distinct IDs with equal hashes become indistinguishable: genuinely unseen traffic is dropped as expired and legitimate acceptance is rejected. No collision was reproduced; the defect is in the representation’s guarantee. **Smallest fix:** retain bounded exact IDs or perform exact comparison within hash buckets.

15. **Minor — Several race tests use sleeps to establish ordering.**  
    [conformance_core.rs:456](../../../../../crates/via-core/tests/conformance_core.rs#L456), [conformance_core.rs:586](../../../../../crates/via-core/tests/conformance_core.rs#L586), [conformance_core.rs:589](../../../../../crates/via-core/tests/conformance_core.rs#L589), [conformance_core.rs:1171](../../../../../crates/via-core/tests/conformance_core.rs#L1171), [conformance_core.rs:1514](../../../../../crates/via-core/tests/conformance_core.rs#L1514), [engine/tests.rs:3554](../../../../../crates/via-core/src/engine/tests.rs#L3554). These waits substitute elapsed time for consumer delivery, overflow, terminal decoding, or actor progress. They can fail under load or pass without reaching the intended race. **Smallest fix:** acknowledge those actual transitions through failpoints/barriers.

Two recorded exceptions also qualify the stated guarantees: [drive.rs:436](../../../../../crates/via-core/src/engine/drive.rs#L436) explicitly permits cancellation before actor handoff to strand a claimed/submitted turn until restart; this narrows “a dropped caller never strands work.” Late-terminal revision remains deferred despite C2’s target behavior. I treated these as accepted limits, not additional findings.

Verification passed:

- Core, Store and adapter selection: **349 tests passed**.
- Selected S1 tests, excluding `s1_f24`: **227 passed**.
- Formatting, workspace Clippy, layer check and chunk diff whitespace check.
- Git remained clean at **0e86995**.

I did not verify production Claude Code/Codex behavior, dynamically reproduce the new interleavings, run the full workspace test matrix, or run release qualification. The passing selected S1 suite revealed no additional unlisted outcome change, but it does not prove the missing cases above.