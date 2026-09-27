**Verdict: UNSOUND for merge.** Round 2 fixes the three round-1 blockers in their intended layers, but the new dispatch decision can permanently cancel queued work that should remain queued.

### Round-1 findings

| Finding | Assessment |
|---|---|
| Uncertain receipt commit | **Fixed for keyed requests.** Core reconciles a lost commit reply, registers a confirmed turn, and returns the C1 §8.1 error fields if the outcome remains unknown (`crates/via-core/src/engine.rs:173-219,344-355,482-501`). The fault-backend tests detect the original stranded-turn failure for spawn and resume (`engine/tests.rs:120-226`). Their handoff checks are sequential; the end-to-end failpoint test remains pending T2-A. |
| Schema v1 compatibility | **Fixed under the agreed fresh-Store rule.** Store now creates v2 and refuses older nonzero versions before writable open (`crates/via-store/src/runtime.rs:18-38,561-564`; `runtime/sql.rs:75-78,112`). The persistence regression exercises an older Store. |
| Eight-turn session ceiling | **Fixed at Store.** `commit_resume` counts queued turns and rejects the ninth inside its receipt transaction (`runtime/sql.rs:359-407`). The persistence regression targets that transaction directly. |
| Sticky cancellation | **The sticky flag is removed**, and the settled-terminal regression detects its original failure (`engine/queue.rs:19-46`; `engine/tests.rs:308-333`). The replacement dispatch decision has the blockers below. |
| Key bound | **Fixed.** Both keys use the documented 1–64 byte printable-ASCII rule, with boundary and invalid-input coverage (`crates/via-core/src/api.rs:385-401,1017-1044`). |

### Merge blockers

1. **An unresolved queued predecessor is treated as a reason to cancel, rather than wait.** An unknown resume receipt marks its turn “done” for the slot (`engine.rs:488-493`). Its successor can pass `Slot::turn`, see that predecessor still `queued`, and immediately commit itself `cancelled` (`engine/drive.rs:87-115,220-265`; `via-store/src/runtime/sql.rs:483-505`). A later keyed retry can adopt and run the predecessor, but cannot restore the cancelled successor. This also creates a retry-versus-drive race: the successor can base its cancellation on a read just before adoption. C1 §7.3 permits cancellation behind an **unknown** predecessor; a queued predecessor awaiting receipt reconciliation remains unresolved and should hold dispatch. **Fix:** return distinct `run`, `wait`, and `cancel` decisions from the durable read; recheck after adoption or predecessor completion. Add a gated regression in which the successor starts before the keyed retry, then verify it eventually submits.

2. **A predecessor Store read failure can become a durable cancellation.** `dispatchable` maps every read error to `false`, and `drive` interprets `false` as permission to call `cancel_queued` (`engine/drive.rs:90-114`). If the next Store operation succeeds, a transient read failure permanently cancels a valid turn. The sticky-cancel regressions do not inject this failure. **Fix:** propagate the read failure or retry the read without changing the turn; cancel only on confirmed durable cancellation eligibility. Add a fault regression where the first predecessor read fails and the turn remains queued.

3. **An unkeyed unknown receipt has no recovery path in this daemon.** `receipt_outcome` records an orphan for any unknown outcome (`engine.rs:177-193`), but `adopt` is reached only through keyed replay (`engine.rs:266-284,387-411`). The worker’s open list correctly identifies a committed unkeyed turn that remains queued and never drives. The returned `retry: same_key_only` is unusable when the request had no key. **Fix:** reconcile and hand off such turns when Store reads recover, or obtain an explicit contract decision requiring a key for requests whose uncertain outcome must be recoverable. Test an unkeyed committed receipt with a lost reply and temporarily unavailable reconciliation read.

### Deferrable and verified points

Two matching retries do **not** independently adopt the same orphan: both pass through the admission mutex, and `adopt` removes the orphan once (`engine.rs:211-219,264,366`). The server reserves drive-channel capacity before committing and sends the returned drive without another await (`crates/via-cli/src/server.rs:485-502`). I found no duplicate handoff in that path.

The predecessor read runs outside both admission and event-head locks (`engine/drive.rs:87-115`); the subsequent submission takes the head separately. Cancelled queued turns are passed over by the Store query, which selects the latest **submitted** predecessor (`runtime/sql.rs:491-505`). These points need no merge change. Restart recovery and status remain outside this review’s scope.

**Checks:** inspected the branch by `git show` and `git diff`; `git diff --check c45337d origin/claude/t2-b-rust-foundation-juogko` passed. I did not check out the branch or run its tests. The reported test results are the worker’s, not independently reproduced here. I made no edits and did not run `bd`.