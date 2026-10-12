GPT-6 Sol medium re-review of T3-S1 fix round 1 (`4352602..020ac95` on local `wt/t3-s1`).

**Verdict: SOUND (merge after decision 8).** I found no blocker, major, or minor finding in the round-1 diff.

Decisions **1–7 are addressed** at their owning layers. All 17 Store commit sites use the shared classifier; the failure batch checks both refusals before writing; and the rider seam fires only when `session.closed` is inserted. Route now propagates an interrupt raw-write failure with its Store kind and tolerates transport failures. Reconciliation opens no control connection to a pre-ARM anchor and still attempts an absence proof. Row 4 shares one absolute deadline across `Stop` and that proof. The late-registration test witnesses the snapshot and distinguishes registration from the ARM gate.

I found no introduced lost wake, deadlock, incorrect Store/Deadline classification, or loss of absence proof for an armed anchor. The migration commit’s error class changes only for SQLite corruption, as intended; an open still fails. The worker’s three choices—`RawDeadline` → `Deadline`, `StoreError::Refused`, and no pre-ARM control connection—are sound. Decision 8’s proposed use of the original early-stop deadline for **both** stopping and the subsequent absence check is correct; an unproven group remains held for reconciliation.

The new tests target the stated failures without using a fixed sleep as an ordering assertion. Decision 1’s corruption test uses synthetic SQLite codes through the shared helper, rather than inducing corruption in a real `COMMIT`; decision 7 is a test-only change whose discrimination is supported by the reported mutation run.

**Limits:** This was source review of Git refs and the worker’s reported RED/GREEN results. I did not run `bd`, `cargo`, tests, a checkout, or the worker’s local logs. I did not review changes outside the supplied round-1 diff.