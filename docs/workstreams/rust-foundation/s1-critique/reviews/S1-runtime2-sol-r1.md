**UNSOUND**

1. **Important — late delivery still loses a held terminal under force or latch.** `crates/via-routes/src/runtime.rs:163` calls `flush()`, whose biased select checks force and latch before `hop.reserve()` at lines 530–542.

   **Failure scenario:** finalization selects the wall deadline with a decoded terminal held; force or the connection latch becomes set before the late flush polls. Even with hop capacity available, flush returns a failure without delivering the terminal and its final text. This is source-verified; the precise concurrent interleaving was not executed.

   **Smallest fix:** use a delivery-only flush for already-decoded held data, bounded by the existing cleanup deadline. Deliver it before applying force/latch result disposition; retain failure propagation for timeout or a closed hop. Add regressions with force and latch already set at late-path entry.

2. **Minor — the negative regression does not establish its claimed held-terminal state.** `crates/via-core/tests/route_stop.rs:398` accepts any `Deadline` failure with empty text.

   **Failure scenario:** slow acquisition or decoding exhausts the three-second wall deadline before Route decodes the terminal. The test passes without exercising late delivery.

   **Smallest fix:** acknowledge terminal decode/retention and assert that acknowledgement before accepting the expected delivery failure.

Finding 1 is addressed: every `Corrupt` acceptance-commit error retains its classification and immediately reaches the latch. Losing the head and retaining the sent event for reconciliation matches `journal::commit_event`; `NotCommitted` and `Uncertain` behavior remains unchanged. The seam follows the acceptance writes, rolls back through transaction drop, is feature-gated, and appears in the release checker. The daemon regression passed; its SQLite backup contains no acceptance timestamp or correlation.

Finding 2 is addressed with unset controls. The positive test’s channel accounting establishes the held-terminal condition on a successful run. Flush uses the three-second cleanup deadline; wake handling and pending-write completion introduce no additional wait or result change. Failed flush cannot return `Completed`. With nothing held, flush returns immediately, preserving rule 3.

The missing daemon-level test is not independently blocking: the positive Adapter test covers delivery, and Core’s inspected path consumes `FinalText`. The control cases above remain a real coverage gap.

**Could not verify**

- No executed regression covers force/latch already set at late-path entry.
- No daemon-level held-terminal regression exists.
- The full workspace gate was not rerun.

Executed checks passed: acceptance-corruption regression, all nine `route_stop` tests, release build, release-feature checker, and diff checks. Tracked files, index, and Git refs remained unchanged.
