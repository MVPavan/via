## Verdict: SOUND — merge; Task 1 can be accepted

**Round-1 findings**

| Finding | Owning-layer fix and regression |
|---|---|
| Force disposition | Core now keeps Route’s proved stop and group absence through failed recovery (`crates/via-core/src/engine.rs:777`). The durable-result assertions detect the original `cancelled`/`unknown` mapping failures; the new test also detects loss of Route’s positive evidence (`crates/via-core/tests/force_stop.rs:374`). |
| Post-ARM acquisition failure | Host retains the pipes, Wire drains them, and Route preserves the deadline cause. The regression checks both `Deadline` and the vendor line in the raw log, so it detects the original failure (`crates/via-core/tests/route_stream.rs:508`). |
| Peer UID | The reported remove-and-restore mutation made the end-to-end test fail and then pass, detecting the missing peer check. I did not independently run it. |

**Merge blockers:** None found. Wire now observes force before an acquisition result when both are ready and treats a subsequent acquisition failure within the grace as forced (`crates/via-wire/src/runtime.rs:181`). I found no new lost settlement, layer violation, or vacuous regression in the round-2 delta.

**Deferrable:** The force/deadline ordering tests depend on elapsed timing and do not deterministically control both observations (`crates/via-core/tests/route_stream.rs:603`); strengthen them with the planned failpoints controller in `via-jm4.7.6/.7.7`. The final-shutdown commit reserve remains a daemon phase-budget issue (`crates/via-core/src/engine.rs:756`), already assigned to `via-jm4.7.7`.

I reviewed the supplied refs without checkout or `bd`. `git diff --check` passed. I did not run the Rust gates on this read-only review; their results are worker-reported.