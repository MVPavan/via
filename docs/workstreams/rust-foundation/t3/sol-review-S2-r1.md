GPT-6 Sol medium check of T3-S2 fix round 1 (`bc34ee2..77db485` on local `wt/t3-s2`).

**Verdict: SOUND WITH CHANGES.** Decision 1’s production fix appears sound, but decisions 2 and 3 do not fully meet their stated contracts. I would fix both before merging S2.

| Rank | Finding at `77db485` | Concrete fix |
|---|---|---|
| Minor | `crates/via-cli/tests/s1_turn_control.rs:784`: the fallback starts its monotonic interval at the fake’s gate, *after* acceptance and the idle clock’s origin. Delay before that gate is omitted, so an order more than 3 seconds late can still satisfy the 3 second bound. | Observe the submission clock and `cancel.requested` on the same monotonic clock, then bound that interval. |
| Minor | `crates/via-host/src/host.rs:869`: an unfinished filtered pass adds `remaining.len()`, which includes foreign groups. With the test’s already-spent deadline, the two-group session reports `held = 3`. This remains conservative for close, but violates the filtered count and can retain the cross-session delay. | Track ownership for unexamined held groups and count only the requested session; assert the exact count in the spent-deadline test. |

**Other checks:** The idle check and attach share the slot mutex with cancel and close attachments. An existing order therefore cannot have its `force_at` shortened by this idle path. The timer disarms, and its omitted slot wake does not lose the order: the run loop’s stop watch retains the change. The two new seams are acknowledgement-only and are listed in the release-feature check. The `Engine::cancel` extraction preserves the response fields. Completed filtered re-probe passes examine the session’s pages before reporting completion; the `None` path keeps its daemon-wide count.

The new ordering test uses seam acknowledgements; its 11.5 second sleep measures elapsed grace, rather than establishing the interleaving. The worker reports failure-first mutations and passing gates, which I did **not** reproduce. I inspected the specified refs and relevant callers by `git show`/`git diff`; I did not run tests, Cargo or Beads, edit files, or check out the branch.