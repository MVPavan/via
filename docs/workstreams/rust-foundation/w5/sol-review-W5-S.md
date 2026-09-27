**Verdict: SOUND (merge).**

1. **Behavior:** I found no changed statement, condition, ordering, lock scope, await point, error mapping, or existing documentation inside the moved code. All 38 function bodies match the base file exactly. The only signature difference beyond visibility is a trailing comma from reformatting `classify` at `crates/via-core/src/engine/terminal.rs:101`. The remaining differences are imports, module declarations and docs, `impl` wrappers, and the `StopMode`/`EngineShutdown` re-export.

2. **Boundaries and visibility:** `engine.rs` retains shared state and API entry points; `drive.rs` owns turn execution and commits; `stop.rs` owns stop and shutdown; `terminal.rs` owns classification and envelope assembly. The cross-module calls are narrow. Each new `pub(super)` item has a sibling or journal-test caller and stays within the original `engine` scope.

**Merge blockers:** None. **Deferrable items:** None required by this review.

I verified the ref through `git show`, source comparison, and `git diff --check`. I did not rerun the worker’s reported Rust gates: the review branch was not checked out, and this review was read-only.