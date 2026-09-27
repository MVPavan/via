**Verdict: SOUND WITH CHANGES.** I found no code safety blocker in the round-3 change. Correct one contradictory sentence in the normative design before merging.

1. **Closure rule: sound.** Store checks for queued or running turns in the same transaction that would write `session.closed` (`f394821:crates/via-store/src/runtime/sql.rs:731, 786-804, 808-817`). An uncertain receipt that committed leaves a queued row; a failed terminal leaves a running row; an uncertain terminal either committed a durable terminal or leaves that running row. The check also sees turns left by an earlier daemon. I found no case in these paths where the close commits while a turn remains durably unfinished. C1 §§3.14 and 7.1 do not forbid this in-flight close, and runtime §7 permits best-effort writes after failure while requiring exit 4 (`f394821:docs/specs/runtime-contracts.md:908-933`).

2. **Three-session test: addressed.** C pauses after deciding `Run` (`f394821:crates/via-core/src/engine/drive.rs:167-174`); A then holds admission; B makes failure pending; only then is C released (`f394821:crates/via-core/src/engine/tests.rs:891-919`). Round-1’s delayed pending latch would fail the pending assertion. With that assertion removed to reach C’s grant, the worker reports that C submitted, which is also the expected result from the inspected ordering. I did not independently run that mutation.

3. **Other round-3 changes:** no further unsafe code change found. The new close-race tests cover both a permitted close and Store’s refusal when an earlier daemon left a queued turn.

**Merge blocker — normative contradiction:** `f394821:docs/workstreams/rust-foundation/t2/dispatch-design.md:286-287` still says a close “never closes after a latch.” That directly contradicts the new rule at lines 238-247 and the permitted-close test. **Fix:** replace those two lines with the rule that a close which passed its check may finish after `failure_pending`, subject to Store’s transaction guard.

**Deferrable items:** none newly identified in scope. The already assigned Store-worker read remains excluded.

**Checks:** inspected the specified ref diffs, Store callers, tests, and contracts; no files were edited and no `bd` command or Rust gate was run. The worker’s reported gate results remain unverified by this review.

