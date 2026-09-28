**SOUND WITH CHANGES.** No blocker or demonstrated new production race. One test-design gap and one incomplete decision application remain. All references below are at **`bf27db6`**.

1. **Major — S1 fixture / evidence-ordering test: the decisive reply may no longer be obtainable.**  
   **References:** `design.md:1445`, `design.md:1548`.

   The fixture suppresses positive `stopped_live` until reconciliation, but does not retain the live anchor/control until then. Early Stop starts group cleanup; the base anchor kills its own group after at most 200 ms (`crates/via-host/src/anchor.rs:197–203`, `:226–233`). Reconciliation runs only after Route and the dispatcher finish. A valid schedule therefore destroys the anchor before reconciliation can receive the withheld fact. Absence can be proved, but the truthful result is `unknown`, contrary to the positive variant’s required `cancelled/forced`.

   This is a missing test precondition, not a defect in retaining production stop facts.

   **Fix:** Use a scripted Host/Adapter fixture for the positive ordering case: withhold forced evidence from Route, publish it at an acknowledged reconciliation barrier, then release terminal finalization. Keep the real-process lost-all-evidence variant expecting `unknown`.

2. **Minor — S3: decision 11 leaves contradictory diagnostic-window entry rules.**  
   **References:** `design.md:799–802` versus `design.md:1160`.

   §6.8 correctly enters on phase one’s force signal, allowing phase two before or after entry. §7.4 still says the window starts at entry “on the latch’s phase two.” In the explicitly permitted schedule where entry wins `admission` before phase two, an implementation following §7.4 can unnecessarily defer diagnostic serving while `failure_pending` already reports the failure.

   **Fix:** Make §7.4 start the window at `enter_final_shutdown` triggered by phase one’s force signal, without waiting for phase two.

The twelve decisions are applied as follows:

| Decision | Assessment |
|---|---|
| 1 — Early-stop lifetime | Yes, `:894–907`. |
| 2 — Sticky stopping / pre-ARM registration | Yes, `:866–877`. |
| 3 — Concurrent stops | Yes, `:869–870`. |
| 4 — Retained forced evidence | Runtime rule yes, `:883–893`; test incomplete as above. |
| 5 — Store kind carried upward | Yes, `:968–975`, `:1025`. |
| 6 — Raw `Full` mapping | Yes, `:961–967`, A14 at `:1621`; the test explicitly requires raw `Full → Raw`. |
| 7 — Store-independent test seam | Yes, `:1443`, `:1547`; A’s writer operation can proceed while B is parked. |
| 8 — Retained close generations | Yes, `:111–129`, `:1550`. |
| 9 — Close pass observes force | Yes, `:515–519`. |
| 10 — Shutdown budgets / read cutoff | Yes, `:777–778`, `:834–851`. |
| 11 — Entry on force signal | Partial: §6.8 updated; §7.4 contradicts it. |
| 12 — Collect-release-insert / plain-stop order | Yes, `:667–681`. |

Within this narrow review, I found no additional lost wake, deadlock, double writer, untruthful production outcome, or unbounded wait introduced by round 5. Registration and snapshot share the ledger mutex; stop facts remain separate from durable absence proofs; close outcomes survive late subscription; and collect-release-insert preserves `admission` while eliminating the forbidden mutex nesting. The earlier read cutoff leaves reconciliation time without promising successful settlement of every turn. Connection-slot release, re-probe ownership and restart rules remain intact.

The affected test specifications can establish their orderings without fixed sleeps **except for the positive evidence test above**. The pre-send pause, publication/subscription pause, snapshot barrier and busy-control fixture provide causal witnesses; deadline assertions are bounds rather than substitutes for synchronization. These are assessments of test designs, not executed proof.

I checked the pinned diff, all twelve decisions, both round-4 reviews, the report mapping, and relevant base Host/anchor/Core/shutdown paths. I did **not** audit ongoing S1 implementation, execute test bodies, exhaustively inspect Wire/Route/Adapter internals, or reopen settled earlier design questions. No files were edited; no `bd` or `cargo` ran. Diff whitespace checks passed; Git status retained the two pre-existing `.beads` modifications.