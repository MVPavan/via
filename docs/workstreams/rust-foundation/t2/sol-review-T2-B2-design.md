**Verdict: SOUND WITH CHANGES.** The per-session dispatcher and daemon-owned reconciler are a sound direction, but the proposed transitions do not yet guarantee that every committed turn retains an owner. Do not implement the note unchanged.

### Round-3 blockers

- **Orphan liveness:** The reconciler gives an isolated orphan a retry wake. Liveness still fails if adoption removes the orphan and the session-start channel is full: the slot remains `Starting`, while the reconciler can exit with no orphans left.
- **Force versus submission:** The shared stop mutex makes the *grant* a valid ordering point against force. The design does not define what happens when submission fails after the grant, or cancellation fails after force.
- **Store-read load:** One dispatcher per session and one grouped reconciler pass remove the per-turn polling multiplier. This blocker is closed by the proposed ownership model.

### Correctness findings

**Locks and event heads.** `adopt` follows `orphans → sessions → slot`, and `grant` can follow `slot → stop`. `request_stop` must release `stop` before scanning and waking slots. The dispatcher’s stated exit check cannot read `orphans` *after* taking `sessions` and slot: that reverses the declared order. Its exit must also serialize with admission. At `ec40440`, `queue_turn` holds an `Arc<Slot>` and its head across a receipt commit; an otherwise empty dispatcher could remove that slot before the receipt is enqueued. A replacement slot with `Head::new(None)` would then coexist with the old writer. The async `admission` lock is intentionally held across Store reads; the prohibition on crossing `.await` applies to the synchronous locks.

**Grant and durable transitions.** Grant-before-force may submit, then must take the force disposition using actual launch evidence. If force is already latched before execution, the route must observe it before launch. A grant is not a successful submission commit: a definite failure leaves an unsubmitted queued turn, while an uncertain failure must remain unresolved and must never cause speculative vendor I/O. Likewise, popping a turn after a failed or uncertain `queued → cancelled` commit can leave durable queued work without a dispatcher. This matters under both drain and force.

**Orphans and starts.** Counting orphans in `queued` and `active`, then transferring those counts on adoption, is correct if each transition is atomic under admission. They must also count against the 256 unresolved-turn bound and remain visible to final shutdown. Before a new receipt can reuse number `n`, admission must settle an orphan for `(session, n)` against Store; otherwise the reconciler’s `turns >= n` test can mistake the *new* receipt for the old one. The 128-capacity start-channel argument holds only while every `Starting` slot owns a counted queued turn. The proposed smaller-capacity fallback needs its own owner; the reconciler is not that owner once adoption removes its last orphan.

**Stop and shutdown.** Idle refusal follows if `active` includes orphans and every nonterminal granted or queued turn. Drain must not reach `active == 0` merely because a drive returned with a failed terminal or cancellation commit. Force must count those failures as unresolved and exit 4 if they cannot be settled by the final deadline. Force must also durably close sessions containing only queued turns, or report an incomplete shutdown. The deferred Task 3 predecessor case remains a drain wait; it cannot justify an exit 0.

### Decisions on the worker’s open questions

1. **Unresolvable predecessor:** Keep the drain wait assigned to Task 3. Preserve the unresolved count and never report a clean exit for it.
2. **Inline execution:** Keep execution inline in the session dispatcher. Retain the grant ordering point; do not hold a daemon-wide lock across `commit_submission`.
3. **Channel replacement:** Use one Engine-owned session-start channel, with daemon main owning retries of pending starts, including during final shutdown.

### Required design changes

1. Make slot retirement acquire `admission` before `orphans → sessions → slot`; retire only after receipt commits and all other event-head users have released that slot. Keep a writer lease for any independent close or cancel path.
2. Specify `queued → granted/uncommitted → submitted → durably terminal` ownership. A failed or uncertain submission or cancellation commit must retain a counted owner until reconciled; force reaches exit 4 at its deadline if settlement fails.
3. Settle a same-session orphan under admission before number reuse, and include orphans in unresolved capacity and shutdown accounting.
4. Give daemon main a bounded pending-start set and retry it on channel capacity return and during final shutdown. Do not make that retry depend on the orphan reconciler.
5. Specify the durable `session.closed` transition for force when a session has queued work but no running turn.

**Deferrable:** Task 3 restart recovery and a drain waiting behind a predecessor this daemon cannot resolve.

This was a read-only review of the specified refs and contracts. I made no edits, ran no tests, and did not run `bd`.