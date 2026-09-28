# Task 3 review: decisions on Sol high's findings

Sol high reviewed the whole of Task 3 (`724ef3d..9efbab8`) in three parts:
`sol-review-T3-control.md` (SOUND WITH CHANGES), `sol-review-T3-failure.md`
(UNSOUND) and `sol-review-T3-conformance.md` (SOUND WITH CHANGES). The
findings are cross-slice gaps. Fixes go to Sonnet 5.5 workers (owner
decision, `../model-observations.md`), on disjoint files.

| # | Finding | Rank | Disposition |
|---|---|---|---|
| 1 | Resumed cohort paging drops an uncertain proof-write failure: no latch (`reprobe.rs:166`) | blocker | fix now, worker X |
| 2 | Restart close completion ignores a failed proof write and admits startup (`close.rs:325,426`) | major | fix now, worker X |
| 3 | A lost acknowledgement can duplicate `raw_log.incomplete` (`drive.rs:733,1204`, `batch.rs:93,203`) | major | fix now, worker X |
| 4 | F12 batch no-reply timeout has no end-to-end proof | major | fix now, worker X. This reverses `s5-r1-decisions.md` 8's second item: it is F12's bound, not Task 4's. The reserved Store lane stays with Task 4. |
| 5 | Host force cleanup computes its 3 s deadline when its watcher runs, not at the force (`host.rs:184`) | major | fix now, after the force-row race, by the same worker (same Host early-stop code) |
| 6 | Explicit plain `via daemon stop` cannot stop an idle version-mismatched daemon (`main.rs:270`, `client.rs:478`) | major | fix now, worker Y |
| 7 | §11 evidence-before-terminal proof and its lost-evidence variant missing | major | fix now, worker Y |
| 8 | Force set: the `Cancelling` state is not proved | minor | fix now, worker Y |
| 9 | F2 lock-before-replace ordering has no failing-capable test | minor | fix now, worker Y |
| 10 | `daemon/status` lacks `started_at` (C1 §3.14, A15) | minor | Task 4 (`via-jm4.7.8`), with status parity |

Also open: the intermittent force-row race found by the S5 merge gate
(`s1_f12_host_early_stop_independent_of_store`), on `wt/t3-force-row`.
