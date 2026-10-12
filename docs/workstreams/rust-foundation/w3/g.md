# W3-G: bounded raw waits and uncertain Store commits

Model: Opus 5.5 high. Follow `../w1/common.md`; report to
`reports/W3-G.md`. Findings in full: `../w2/sol-reviews/W2-E.md` (1, 3).
Runs in parallel with W3-F.

Owned: `via-wire`, `via-routes`, `via-store`, and in `via-core` the
observation-commit and terminal-commit path. W3-F owns stop, shutdown and
cancel; keep any shared hunk in `via-core/src/engine.rs` small and name it.

1. **Raw appends obey the cleanup deadline.** `drain_to_eof` awaits
   `record()`, and `RawWriter::append` waits for the Store worker without a
   timeout, so a stalled worker defeats Route's cleanup bound. Bound each
   append by the same absolute deadline, latch incomplete evidence on
   expiry, continue a bounded discard drain. Regression: stalled Store
   worker, cleanup still returns by its deadline with incomplete evidence.
2. **Uncertain observation commits.** Core treats every `commit_event`
   error as not committed and keeps the old sequence; Store allows an
   uncertain outcome. If it did commit, the terminal insert fails and the
   receipted turn stays running. Reconcile the durable event head before
   the terminal commit; if the turn cannot be resolved durably, report
   C1's `store_error`. Regression at the Store/Core boundary with an
   injected uncertain commit (a closed fault backend inside
   `#[cfg(test)]` or the spec'd `test-failpoints` feature, not a runtime
   switch).
