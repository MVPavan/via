**Verdict: SOUND WITH CHANGES.** Commit `a52d753` corrects the C1 names and shapes for the successful fake turn, but it does not fully resolve T1-I5 and it misreports several failure outcomes.

### Findings

1. **Important — T1-I5 remains incomplete.** `crates/via-adapters/src/runtime.rs:145` discards text, tool, and unknown observations; `crates/via-core/src/engine.rs:153` receives only acceptance and the terminal result. C1 §6 requires those observations in the event stream, and the checkpoint explicitly included their loss in T1-I5. The new end-to-end test expects exactly four lifecycle events, so it cannot detect the omission. Carry the observations to Core, assign dense sequence numbers, and commit their canonical events with raw references. This needs coordination with the route owner; the worker correctly disclosed the omission, but T1-I5 cannot be marked fully fixed.

2. **Important — distinct failures become `protocol` failures.** `crates/via-core/src/engine.rs:505` maps every `AdapterError` to `failure.class = "protocol"`. Route errors include confirmed process exit, overflow, and Store failure (`crates/via-routes/src/lib.rs:170`); transport and deadline failures also lose their cause. Callers therefore receive the wrong C1 §8.2 disposition. Preserve typed causes across the route and adapter boundaries and map each to its C1 class; add failure-path assertions.

3. **Important — Store does not enforce its stated raw-reference invariant.** `crates/via-store/src/runtime/sql.rs:347` compares `raw_ref` only *if the JSON key exists*. A caller can commit a raw span in the row while omitting it from the public event. The new test checks a *different* reference but not an absent one (`crates/via-store/tests/persistence.rs:426`). Require the document value, including explicit `null`, to equal the stored span, and test both omissions.

4. **Minor — duration uses a wall clock.** `crates/via-core/src/engine.rs:199` computes `duration_ms` from two `SystemTime` readings. A clock adjustment can make it inaccurate or `null` on an otherwise completed turn. Measure duration with `Instant`; retain `SystemTime` for event timestamps.

### Finding and regression disposition

T1-I6’s successful-turn receipt and envelope shapes are substantially corrected at Core. T1-I5’s lifecycle tags, common fields, sequence, and cited spans are corrected for that path; observation events remain missing. The new end-to-end regression would fail on the predecessor for the reported old tags, missing fields, and wrong shapes. It covers one completed turn, so it does not test failure envelopes. The Store test is useful for the new sequence and mismatch checks, but its new record API means it cannot run verbatim on pre-change code as a failure-first regression.

I agree with the worker’s disclosed capability ownership, one-turn assumptions, and interpretation uncertainties as follow-up items. The missing observation events are a present contract gap, and failure-class coverage and the absent-`raw_ref` case should be added to that list.

**Checks:** read the commit diff, relevant contracts, implementation, and tests; `git diff a52d753^ a52d753 --check` passed. I did not run Cargo checks in this read-only checkout. Git status showed only pre-existing `.beads` changes; I made no edits.