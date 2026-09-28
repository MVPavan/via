**UNSOUND.** No blocker-level finding; four major gaps remain, including two in S1’s Host work. All references below are to **1a814d6**. These are design findings, not claims about the implementation underway.

| Round-4 decision | Assessment |
|---|---|
| 1 — Incomplete keyed-close fence | Implemented at `design.md:460`. |
| 2 — Evidence before terminals | Ordering and reserve implemented at `:804–823`; the new evidence test has an invalid inference below. |
| 3 — Independent Host early stop | Present at `:834–859`, but acquisition coverage and normal-shutdown lifetime are incomplete. |
| 4 — Mutex order | Implemented at `:72–82`. |
| 5 — Store classification | Writer/raw loss correctly latches; raw `Full` has conflicting enum requirements. |
| 6 — Close-watch resolution | Subscription and publishers added, but their synchronization remains insufficient. |
| 7 — Final-shutdown entry | Implemented at `:776–797`. |
| 8 — Plain stop versus durable closing | Implemented at `:1585–1586`. |
| 9 — Test witnesses | Added at `:1378–1382` and corresponding tests; not all specified interleavings are executable or conclusive. |

1. **Major — S1: the early-stop snapshot can miss a live acquisition.**  
   **Reference:** `design.md:838–841`; `crates/via-host/src/host.rs:590–636`.

   An acquisition passes the pre-ARM force check, then force fires before its control enters the ledger. The early-stop task snapshots the ledger without it. That acquisition can send ARM or remain blocked on the ARM reply/vendor-facts commit. The base registers the control only after those operations. The new design specifies neither earlier registration nor a force check synchronized with registration, so stopping this group still depends on acquisition/Route progress.

   **Fix:** Register the verified control with the independent owner before ARM, and make registration observe a sticky stopping state atomically with the ledger snapshot. A late registration must immediately inherit the original force deadline. Add an acquisition-versus-snapshot barrier test.

2. **Major — S1: the early-stop task has no exit path for ordinary shutdown.**  
   **Reference:** `design.md:834–837`, `:851–857`.

   The task is created at Host construction, wakes on force, and is joined/counts as pending at shutdown. Plain stop and drain explicitly do not trigger it. Following this lifecycle literally leaves a healthy daemon waiting on its own dormant task until the shutdown deadline, then reporting pending cleanup instead of a clean exit.

   **Fix:** Give the task an explicit shutdown cancellation signal, separate from force. Host shutdown must cancel and join the idle watcher; an already-running stop operation must retain its bounded ownership. Test plain stop and drain with no force notification.

3. **Major — subscribing under `admission` does not exclude the new force/latch publishers.**  
   **Reference:** `design.md:111–120`, `:457–459`, `:545–550`; `crates/via-core/src/engine/drive.rs:117–120`.

   A keyed caller sees an existing close order under `admission`. Before it subscribes, the dispatcher publishes its force/latch result, then stops. Those exit publications are not required to acquire `admission`. A newly subscribed watch receiver can therefore regard the terminal value as already seen and wait for another publication that never arrives. The stated rule protects publication **after subscription**, not publication between checking the order and subscribing.

   **Fix:** Make close outcomes retained, attempt-specific state: subscribe and immediately inspect the current outcome, waiting only while that attempt is pending. Atomically associate the order and outcome generation. Test publication between the caller’s order check and subscription.

4. **Major — the new Store-independence test cannot produce its stated latch.**  
   **Reference:** `design.md:1357–1359`, `:1484`, `:1611–1613`.

   The test pauses B at `store.commit.event`, then requires A’s uncertain event commit to latch before B is released. The seam is inside the transaction, and Store has one SQLite worker. While B holds that worker, A cannot reach its commit or lost-reply seam. The test either stalls or eventually latches through a watchdog, which proves a different scenario.

   **Fix:** Add a caller-side barrier after B’s operation is enqueued but before B consumes its reply, leaving the writer runnable. Then inject A’s uncertain commit and require B’s early-stop acknowledgement and group absence before releasing B.

5. **Minor — the evidence test conflates absence with proof of forced cancellation.**  
   **Reference:** `design.md:1485`; `docs/specs/runtime-contracts.md:873–877`.

   Losing the stop reply and later proving absence does not establish that cleanup began while the vendor was live. The anchor may already have disappeared before reconciliation. In that case `unknown` with `quiescent` cleanup is truthful; the test nevertheless requires `cancelled`. Conversely, if the early-stop owner already retained a positive stop reply, the test can pass without proving that reconciliation supplied the decisive evidence.

   **Fix:** Specify a fixture that withholds a positive `stopped_live` report until reconciliation and witnesses its delivery before terminal commit. Keep a separate lost-all-stop-evidence variant expecting `unknown` with independently determined cleanup.

6. **Minor — raw `Full` has contradictory S1 acceptance requirements.**  
   **Reference:** `design-r4-decisions.md`, decision 5; `design.md:895–897`, `:1493`.

   Decision 5 describes `Full` as `NotEnqueued` for both threads. The design restricts that variant to the SQLite writer; the unit-test row first requires `NotEnqueued` for both threads, then requires raw `Full` to be `Raw`. S1 cannot satisfy both assertions. The scoped row-6 policy itself is consistent.

   **Fix:** Choose one raw-queue error representation and use it consistently in the decision, classification and test. The design’s explicit `Raw` → row 6 mapping is sufficient; retain `WriterLost` for raw disconnection/dropped replies.

The reordered pipeline correctly places reconciliation before terminal settlement. Its deadline handling permits unresolved work and counts incomplete closure rather than claiming success; the five-second reserve is not a guarantee that every per-turn write completes. I found no additional round-4 double-writer or lock-cycle defect in the reviewed paths.

The new seams improve observability, but the tests do **not yet collectively prove** the stated orderings without sleeps: findings 3–5 need explicit interleavings or evidence witnesses.

I checked the pinned diff, nine decisions, round-3 findings, report map, and relevant base Host/Core/Store and runtime-contract paths. I did not audit the ongoing S1 implementation, exhaustively review Wire/Route/Adapter internals, rerun earlier settled reviews, or execute tests/OS timing checks. No files were edited; no `bd` or `cargo` ran. Git status retained the two pre-existing `.beads` modifications.