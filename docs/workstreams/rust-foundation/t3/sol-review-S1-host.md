GPT-6 Sol medium review of T3-S1 (host part) at `8d2a7b6`; line references are to that commit.

**Verdict: UNSOUND.** The Route can discard a raw Store failure while sending an interrupt, so the slice cannot reliably report row 6.

| Rank | Finding | Concrete fix |
|---|---|---|
| **Blocker** | At `crates/via-routes/src/runtime.rs:422–430`, `Control::on_wake` ignores every `write_frame` error. If recording the interrupt fails, Route keeps reading and may return a terminal or `Stopped` instead of `Store { kind }`; an uncertain writer failure can also miss the latch path. | Propagate and classify the write error, force-close the group, and add a raw-failure test at the interrupt write. |
| **Major** | At `crates/via-host/src/host.rs:1425–1443`, reconciliation sends `Stop` after verifying an anchor without checking its durable launch phase. An identified, pre-ARM anchor rejects `Stop` as invalid (`crates/via-host/src/anchor.rs:159`). This violates round-6 decision 1. | Check the journal phase; use EOF/absence handling for pre-ARM anchors and send `Stop` only where ARM may have launched a vendor. |
| **Major** | At `crates/via-host/src/host.rs:1118–1127` and `:673–679`, a vendor-facts failure can spend up to 3 seconds on `Stop`, then start a fresh 3-second absence check. Row 4 gives cleanup one 3-second allowance. | Carry one absolute cleanup deadline through both operations. |

The early-stop snapshot, ARM gate, phase changes, owner stop after `Spawned`, concurrent dispatch, and shutdown retirement follow the stated design on inspection. Host records the verified identity before the identified commit and does not signal a numeric vendor PID. Reconciliation counts a verified anchor’s `Stopping { stopped_live: true }` reply as force evidence; it does not derive that evidence from absence alone.

The stop watch passes through Adapter, Route and Wire. `Stopped` carries launch and cleanup evidence through `RouteFailure`; Store kinds and `journal_uncertain` are carried upward. The Core edits match the §13 compile allowance: an inert watch, today’s force row for `Stopped`, and the `WriterLost` rename in `may_have_committed`. With that inert watch, Core has no new stop-order behavior in this slice.

The tests use failpoint acknowledgements for several Host orderings, but the late-registration test at `crates/via-host/tests/s1_host.rs:502–515` relies on `yield_now` and never witnesses the snapshot; it can also pass through the ARM-gate path. Rule 2 has no isolated test, as the worker report states. Short polling sleeps appear in the harness; I found no fixed sleep used as the sole ordering assertion. The report’s failure-first runs are reported evidence, not independently verified here.

I reviewed the specified refs, design, decisions, report and affected source. I did **not** run bd, cargo, tests or a checkout, and did not review the Store implementation outside the interfaces needed for these findings. Git status still shows only the pre-existing `.beads` changes.