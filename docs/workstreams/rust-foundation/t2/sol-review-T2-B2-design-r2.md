**Verdict: SOUND WITH CHANGES.** The revision has the right ownership structure, but three decisions still need to be explicit before implementation follows it.

### 1. Earlier decisions

| Decision | Re-review |
|---|---|
| Writer lease and slot retirement | **Incorporated.** Retirement takes `admission → orphans → sessions → slot`; the head lease protects independent writers. The lease must be acquired before an external user can retain a slot for a later write. |
| Granted/submitted ownership and failed commits | **Partly.** The turn stays owned and counted, but the proposed retries after Store write failures conflict with runtime §7. |
| Settle orphans before number reuse; count them | **Incorporated.** Settlement under admission precedes the new receipt, and orphans enter both capacity counts. |
| Daemon-owned pending starts | **Incorporated in ownership.** Final shutdown still needs an explicit interleaved, deadline-bounded receive/retry rule. |
| Force `session.closed` | **Partly.** Queued-only closure is specified; other force cases and failed earlier cancellations are not settled. |
| Unresolvable predecessor | **Incorporated.** Drain waits with work counted; Task 3 owns recovery. |
| Inline execution | **Incorporated.** The grant orders against force; execution remains in the dispatcher. |
| One Engine start channel | **Incorporated.** Daemon main owns pending-start retries. |

### 2. Transition check

The orphan handoff, number-reuse settlement, writer lease, and pending-start set each have a stated owner, wake, and count. The declared `admission → orphans → sessions → slot` order is coherent; `stop` is taken alone, and the design forbids a synchronous lock across `.await`.

Three paths remain unsafe or underspecified:

1. **Store failure can resume dispatch.** The design retries a definite submission write failure, treats an uncertain write as dispatchable after a read-back, and retries failed terminal or cancellation writes during drain (revision §§2.2, 2.3). Runtime §7 (`docs/specs/runtime-contracts.md`) instead latches the *first state-write failure or uncertain commit* into Store-failed mode, stops admission and dispatch, and requires bounded failure shutdown. Its write-order rule also says an unknown submission outcome permits no speculative send. Thus the proposed “definite failure … later runs” test expectation conflicts with the contract. This is the principal launch and repeat-work risk.

2. **Force can close a session with an earlier queued turn unresolved.** Section 2.3 continues through queued cancellations and makes the *last* cancellation close the session. If an earlier cancellation failed, that can durably close a session while an earlier turn remains queued. A forced running turn’s final-shutdown terminal has the same problem if a queued successor’s cancellation failed. Force also lacks a close owner for a session whose last turn ended just before force, or one already idle. C1 §3.14 and §7.1 (`docs/specs/via-api-v1.md`) require force to close every session durably; a failed close must preclude exit 0.

3. **Shutdown can miss or wait indefinitely for a late start.** Section 5 gives daemon main the pending set, but §6 lists “retry pending starts and drain the channel” without saying to alternate receiving and retrying under the *same absolute deadline*. A full channel needs a receive before retry can succeed. Existing keyed replay adopts before the stop check in T2-B (`ec40440`, `engine.rs`); the revision retains keyed adoption without specifying a final-shutdown barrier. Such an adoption can occur after main’s last channel drain. It must either be included in the shutdown handoff or remain visibly unresolved for exit 4.

### 3. Required decisions

1. **Follow runtime §7 on Store writes:** latch Store-failed on a state-write failure or uncertain commit; stop new grants and vendor launches; retain the turn and its last durable state for bounded failure settlement and exit 4. Do not retry submission or cancellation as ordinary dispatch. Change the “later runs” test expectation accordingly. A confirmed queued turn can run only through the contract’s subsequent recovery path.

2. **Close only after all of a session’s turns have durable force dispositions.** Give force closure a session-level owner even when there is no queued or forced turn. If any cancellation or close remains uncommitted, retain that failure for shutdown accounting and exit 4; do not commit `session.closed` ahead of an unresolved queued turn.

3. **Make the final start handoff atomic with shutdown accounting.** Under the existing admission ordering, prevent keyed adoption from appearing after main’s last drain; alternate channel receives and pending-set retries until both empty or the shared deadline expires. Count any remaining `Starting` slot as incomplete. State explicitly that a grant retains its queued count until submission is confirmed or cancellation is durable (§2.2 and §4 currently describe that point differently).

This was a read-only design review. I edited no files, ran no tests, and did not run `bd`.