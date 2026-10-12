**UNSOUND**

### Findings

- **Important — cancelled shutdown loses Wire stragglers.** `crates/via-wire/src/connection.rs:548` removes every `JoinSet` from runtime ownership before awaiting joins. If shutdown is cancelled while a reader remains stuck, the local sets are dropped: their tasks are aborted, and the runtime can no longer join or count them. This violates the `.repo-context/coding-style.md:102`, which explicitly covers cancelled shutdown. Keep the sets in a guard that returns unjoined sets to `Stragglers` on drop.

The two round-one findings **are resolved**: cancelled `finish` now hands off through `Drop`, and panicked reader/writer joins are counted. The T4-2 merge preserves the inspected prompt, observation, Wire export, and release-check paths. The F27 gate still checks byte-exact split text and the huge line’s failure and saved prefix; it accounts for the in-flight drop permitted by design §8.5.

### Could not verify

I did not rerun runtime tests or the full release gate in this read-only review. `cargo fmt --all --check`, the layer check, `git diff --check`, and Git status passed; the worktree is clean. The observation-budget test is at `crates/via-core/tests/s1_observation_budget.rs`, rather than the `via-adapters/tests` path in the request.