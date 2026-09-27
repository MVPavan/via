**Verdict: SOUND (merge).** I found no round-5 merge blocker.

The three round-4 blockers are addressed at their owning layers:

1. Core recovery (`crates/via-core/src/engine/recovery.rs:58`) stops inventory paging at the five-second deadline. An incomplete inventory makes every unfinished turn’s cleanup uncertain, including turns with no anchor seen. The page-boundary daemon regression would catch the original unbounded paging and false quiescence.
2. Host recovery (`crates/via-host/src/host.rs:734`) distinguishes journal read and absence-commit failures from process-proof uncertainty. The failed-commit daemon regression would catch the original path that admitted after a Store failure.
3. Host shutdown (`crates/via-host/src/host.rs:689`) consumes bounded pages and retains aggregates only for requested turns. Core supplies (`crates/via-core/src/engine/stop.rs:135`) its capped unresolved-turn set, which includes force-stopped turns. The 10,001-anchor test now exercises clean final shutdown. The memory bound is established by the code structure; a 10,001-row test alone cannot prove it.

I found no ended-turn mis-settlement in the new incomplete path, no reversal of the Host error types, and no layer-direction violation in the passive Wire, Route and Adapter types. The whole-table production APIs are gone. The new failpoints are feature-gated and included in the release-exclusion check.

**Deferrable, assigned elsewhere:** the items excluded in the request, including the Route close-path failed-commit report. I ran read-only ref inspection and `git diff --check` (passed). I did not run the build or tests; the gate results are worker-reported and remain unverified by this review.