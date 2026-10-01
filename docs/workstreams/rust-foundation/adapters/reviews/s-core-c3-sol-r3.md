**UNSOUND** — the delivery fix introduces premature capacity release for an already-pinned connection.

Source review is limited to `871c91f..7df0b3c`.

| Item | Verdict | Reason | File:line |
|---|---|---|---|
| 1 — Close | Fixed | A timed-out close leaves capacity with retirement. | `via-adapters/src/driver.rs:459` |
| 2 — Reservation | Partly fixed | Commit follows delivery, but pinned delivery failure releases capacity before retirement; see below. | `via-adapters/src/fake/driver.rs:207` |
| 7 — Logical/helper facts | Fixed | Persistent failures use reported-tool cleanup and suppress housekeeping exit/force; ServerLost retains Host facts. | `via-routes/src/fake/runtime/lane.rs:867` |
| 8 — Health | Fixed | The owned task publishes Route failures independently of delivery; Store, server loss, mismatch and uncertain retirement are covered. | `via-adapters/src/fake/driver.rs:471` |
| 9 — Steer | Fixed | Count and byte permits survive through resolution; partial semantics use the approved `Cow`. | `via-routes/src/fake/runtime/lane.rs:145`; `via-adapters/src/observation.rs:139` |
| 13 — Payload cap | Fixed | The retained-payload wording follows the coordinator’s ruling. | `via-routes/src/fake/mod.rs:824` |
| 15 — Discarded errors | Fixed | Failed idle-close delivery latches overflow health. | `via-adapters/src/fake/driver.rs:608` |
| R2 new 1 — Unpolled close | Fixed | State changes and stop posting occur together on first poll. | `via-adapters/src/driver.rs:418` |
| R2 new 2 — Early commit | Partly fixed | Early commit is eliminated; retirement ownership remains incomplete for pinned capacity. | `via-adapters/src/fake/driver.rs:204` |
| R2 new 3 — Nine steers | Fixed | The command permit remains held while awaiting evidence. | `via-routes/src/fake/runtime/lane.rs:648` |
| R2 new 4 — Lost idle observation | Fixed | Rejection publishes failure after releasing capacity and invalidating the pin. | `via-adapters/src/fake/driver.rs:611` |

Paths above are under `crates/`; line numbers refer to `7df0b3c`.

**New defect:** **Important — pinned capacity can be released while its helper remains alive.** After a successful first turn, the token resides in `DriverState.capacity`; a pinned successor’s reservation owns no token ([driver.rs:326](../../../../../crates/via-adapters/src/driver.rs#L326)). If that successor reaches logical success but final delivery fails while retirement continues, the new `settle()` calls `release()` ([fake/driver.rs:207](../../../../../crates/via-adapters/src/fake/driver.rs#L207)). `invalidate()` immediately takes and drops the committed token ([driver.rs:547](../../../../../crates/via-adapters/src/driver.rs#L547)), permitting replacement admission before retirement finishes. Invalidate the pin immediately, but retain the token until retirement. The new regression test covers only `NeedsConnection`, whose token stays inside the shared reservation.

**Concern verdicts:**

| Concern | Verdict |
|---|---|
| 4 — Detection after bounded Route close | Acceptable under the supplied definition. Publication no longer waits for Adapter delivery. |
| 5 — Dropped future becomes `Route(Overflow)` | `Failed` is truthful; **Overflow is a diagnostic mislabel**. Dropping the consumer can close an empty hop without exhausting a queue. The classification predates this delta; the fix now exposes it through sticky health. |
| 7 — Three `DriverFailure` variants | Acceptable. They represent distinct required failures, and C2 does not enumerate a closed variant set. |

**Verification:** 230/230 tests passed normally; 235/235 passed with `via-core/test-failpoints`, across the four permitted crates. Force-close coverage passed.

**Could not verify:** the pinned-capacity counterexample at runtime, deterministic `RetirementUncertain`, or historical RED executions. The checkout advanced externally to `6eb0771` during review, so the test results are not an exact-`7df0b3c` certification. Final Git status is clean. I made no edits or Git changes and ran no `bd`, vendor CLI or model.