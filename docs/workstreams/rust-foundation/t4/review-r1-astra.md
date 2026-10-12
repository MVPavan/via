# T4-0 design review: GPT-6 Astra (high)

Review of `wt/t4-0` at `df9b8d2`, verbatim, local links converted to repo paths.

**UNSOUND for implementation.** The ownership structure is workable, but control serviceability, cleanup, accounting and follower lifetime have blocking gaps.

Reviewed clean branch `wt/t4-0` at `df9b8d269bd84fc4b1546a738a5a2563703ec8ff`. Findings are ranked below.

1. **Blocker — Observation stalls do not reliably interrupt the turn.**  
   `docs/workstreams/rust-foundation/t4/design.md:451`: dropping `route_rx` only fails a subsequent `forward`. A vendor that goes silent leaves Route awaiting `crates/via-routes/src/runtime.rs:308`. Separately, disabling the stop arm after `interrupted` prevents a blocked forward from enforcing `force_at`; `crates/via-routes/src/runtime.rs:436`.  
   **Fix:** deliver stall expiry through the owning turn-control path, independently of further vendor output. Continue polling stop changes, `force_at`, force and sticky health during forwarding and pending delivery.

2. **Blocker — Wire failure publication lacks a reliable consumer wake.**  
   `docs/workstreams/rust-foundation/t4/design.md:351`: `next_frame` selects cancellation, Route wake and frames, but not health. A stderr failure need not close stdout’s sender; a raw-worker failure currently only sets `incomplete`, losing its failure classification. Deferring health until forwarding resumes also contradicts `docs/specs/adapter-contract.md:55`.  
   **Fix:** retain the classified first failure in one owning health state, wake every relevant consumer independently of data capacity, and specify how preceding durable frames are handled without delaying cleanup.

3. **Blocker — Reader cancellation can precede raw-tail drain, and abort is not join.**  
   `docs/workstreams/rust-foundation/t4/design.md:320` ends readers on `close`, while `crates/via-routes/src/runtime.rs:178`. Dropping their `JoinSet` requests cancellation; it does not establish that readers finished before Store shutdown.  
   **Fix:** keep readers alive through group stop and EOF/deadline, then flush the raw barrier and join them. On deadline, mark incomplete, abort and reap under the existing shutdown bound. Make drop an emergency fallback.

4. **Blocker — The claimed global memory bound omits retained allocations.**  
   `docs/workstreams/rust-foundation/t4/design.md:123` charges encoded length plus 512 bytes per item. That does not account for decoded JSON nodes, normalization copies or unfinished stdout framing; staging is acquired only for completed units. These are explicitly covered by `docs/specs/runtime-contracts.md:1041` and `docs/specs/runtime-contracts.md:342`.  
   **Fix:** specify acquisition-before-allocation and permit transfer for each retained representation, including AST charges and partial frames. Resolve vendor JSON and envelope bounds before claiming F24/global-budget conformance.

5. **Blocker — Reserved Store capacity is not reserved end to end.**  
   `docs/workstreams/rust-foundation/t4/design.md:154`: global exhaustion can refuse a lifecycle request despite its reserved slot and byte allowance. Public reads can also consume the ordinary byte budget despite leaving Internal slots available. This undermines `docs/specs/runtime-contracts.md:944`.  
   **Fix:** reserve lifecycle bytes within the global budget too; give ordinary commits protected byte capacity against Public reads. Route required failure-resolution reads through protected admission, and state whether the latch has a dedicated slot or prove eight lifecycle slots suffice.

6. **Blocker — Follower registration and retirement are incomplete.**  
   `docs/workstreams/rust-foundation/t4/design.md:566` assumes a mapped `Head`, but `crates/via-core/src/engine.rs:381`. A follower lease also prevents retirement; dropping it later does not trigger another retirement pass. Finally, scanning only `seq > after` misses an already-terminal target when `after` includes its terminal.  
   **Fix:** specify atomic get-or-create registration using the existing slot/head mechanism, explicit release-and-retire handling, and terminal-state metadata from the Store snapshot. Test idle/restarted sessions, repeated subscribe/unsubscribe, and follow starting at or beyond terminal.

7. **Blocker — Outbox pacing and termination are not an executable concurrency protocol.**  
   `docs/workstreams/rust-foundation/t4/design.md:619` always reserves room before advancing, which can wait during live saturation instead of detecting immediate lag. Aborting a follower before removing entries does not prove it has stopped enqueueing. Freezing `resume_after` before finishing a partial frame can also make the notice stale. `docs/specs/via-api-v1.md:335`.  
   **Fix:** define one synchronized subscription state with a closed/generation check on enqueue; distinguish replay waiting from live refusal; let the serializer finalize the cursor after completing any started frame. Use one socket termination deadline and keep its reader, handler and writer serviceable concurrently.

8. **Important — Store shutdown and lock rules contradict existing guarantees.**  
   `docs/workstreams/rust-foundation/t4/design.md:76` prohibits taking `Lanes` under *any* lock, although `crates/via-core/src/engine/close.rs:399`. Prioritizing `Shutdown` also lets it overtake ordinary accepted mutations, contrary to `crates/via-store/src/runtime.rs:1082`.  
   **Fix:** explicitly allow async-owner locks → short `Lanes` lock, forbid reverse acquisition/callbacks, and specify the condvar predicate loop. Make shutdown an admission fence followed by draining accepted work. Define writer-death publication and rejection/drop of pending replies.

9. **Important — `logs` can monopolize SQLite despite a bounded response.**  
   `docs/workstreams/rust-foundation/t4/design.md:545` equates a 1 MiB response with bounded file work. `crates/via-store/src/runtime/raw.rs:163`, potentially traversing arbitrarily long history repeatedly on the sole SQLite thread.  
   **Fix:** bound lookup work independently of returned bytes, using indexed/direct validated lookup or a separately owned bounded raw-read path. Test late references in long logs while lifecycle commits are pending.

10. **Important — Two conformance limits are deferred rather than implemented.**  
    `docs/workstreams/rust-foundation/t4/design.md:39`: refusing approximately 7 MiB prompts contradicts `docs/specs/runtime-contracts.md:1029`. A 1 MiB vendor-frame cap also does not establish C2’s 256 KiB observation cap; `crates/via-adapters/src/runtime.rs:353`.  
    **Fix:** implement the blob path or obtain a numbered contract amendment. Specify encoded-size checks for every observation: split text, otherwise protocol failure with raw evidence, and enforce unknown-payload truncation.

11. **Important — Session counts use the wrong owner, and the slice plan misses required files.**  
    `docs/workstreams/rust-foundation/t4/design.md:805` derives unresolved sessions from slots, although `crates/via-core/src/engine/journal.rs:250` retains failed turns beyond dispatch activity. “Increment on receipt” also needs to exclude resume/replay. Closing occurs through standalone and terminal-bearing paths, including `crates/via-core/src/engine/stop.rs:567`.  
    **Fix:** derive active sessions from `Unresolved`; specify coherent count snapshots and exactly-once open-session transitions. Assign `stop.rs`, `close.rs`, `journal.rs` and affected `drive.rs` changes explicitly. Move these hooks into S1 or serialize the affected work: S3’s correct implementation otherwise overlaps S2’s `drive.rs`. S1 also omits `stop.rs` for lifecycle-handle routing.

12. **Important — The list cursor skips equal-timestamp rows.**  
    `docs/workstreams/rust-foundation/t4/design.md:555` orders `updated_at DESC, id ASC` but uses tuple `<`, which compares both components in the same direction. It also cannot guarantee that concurrently updated unseen rows remain reachable, as `docs/specs/via-api-v1.md:304`.  
    **Fix:** use `updated_at < t OR (updated_at = t AND id > id_cursor)` for static traversal, and separately specify a snapshot/version strategy—or an approved amendment—for concurrent updates.

Answers to the report’s questions:

| Question | Recommendation |
|---|---|
| **Q1** | **Agree.** Keep immediate framed overflow; it is distinct from observation stall. Health/control must bypass it. |
| **Q2** | **Change.** Slot sizing is reasonable; add protected global bytes, prerequisite-read admission and a latch-capacity proof. |
| **Q3** | **Agree for admission refusal.** “Reads never latch” is too broad: writer loss and qualifying corruption retain T3’s classification. |
| **Q4** | **Conditional.** Paced replay needs approved A7 and the fixes above; do not extend the required ownership-release bound. Otherwise retain the literal contract. |
| **Q5** | **Agree.** Deterministic seeded generators plus explicit boundary cases satisfy the no-new-dependency constraint. |
| **Q6** | **Agree on primitives.** `JoinSet`/`watch` suffice only with explicit cancellation and bounded joining. |
| **Q7** | **Agree.** Use 3,600,000 ms; tests needing shorter deadlines should request them. |
| **Q8** | **Agree.** Defer while every supporting request is explicitly refused. |
| **Q9** | **Disagree as conformance.** Runtime explicitly requires blob-backed large inputs; refusal requires an approved amendment. |
| **Q10** | **Agree.** Nested non-nullable deadline members reject `null`. |
| **Q11** | **Change.** Keep an open-session tally, but fix transitions, snapshot consistency and the existing unresolved-state owner. |
| **Q12** | **Agree.** Constant/check only is sufficient while retention never prunes. |
| **Q13** | **Agree.** Immediate close without bytes is a reasonable pre-handshake refusal. |

No files were changed; no cargo or bd commands were run. I did not verify runtime timing, RSS, platform behavior, every event-write path, or the complete existing test suite. Concurrency findings above are source-level traces, not executed reproductions.