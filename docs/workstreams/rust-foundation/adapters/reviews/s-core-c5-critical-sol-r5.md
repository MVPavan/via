**UNSOUND.** The r4 fixes are addressed, but an additional whole-chunk defect misclassifies an acknowledged steer. It predates these two commits.

| Item | Status at 6d47cd6 |
|---|---|
| r4 #1: inaccurate delivery wording | **Addressed.** [driver.rs:150](../../../../../crates/via-adapters/src/driver.rs#L150), [api.rs:1240](../../../../../crates/via-core/src/api.rs#L1240), and the mapping expectation at [conformance_intake.rs:1695](../../../../../crates/via-core/tests/conformance_intake.rs#L1695) match the revised wording. |
| r4 concern 5: cancellation sleep | **Addressed.** [conformance_driver.rs:2330](../../../../../crates/via-core/tests/conformance_driver.rs#L2330) checks cleanup immediately after dropping the future. |

**Additional finding — Important: an acknowledgement received before local write completion can become `NotDelivered`.**

Locations: [lane.rs:424](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L424), [driver.rs:741](../../../../../crates/via-adapters/src/driver.rs#L741).

Route accepts the vendor’s delivery report while its write future remains pending, but defers its reply until Wire confirms completion. The report can already become a `steer.delivered` observation. If the logical turn ends meanwhile, Driver sees no Route reply and returns `NotDelivered`, ignoring the established acknowledgement and emission.

An isolated probe paused `wire.prompt.after_write` on occurrence 2—after the whole steer input was written, before Wire answered. The helper read it, acknowledged delivery, then emitted a completed terminal on the persistent profile. Twice, the probe produced:

```text
vendor delivery observation emitted=true
steer answer=Ok(Err(NotDelivered))
```

Both runs failed the expected successful-delivery assertion, exit 101. This violates the revised error definition and permits a `steer.delivered` record alongside a response claiming no acknowledgement.

**Smallest fix:** preserve vendor acknowledgement independently of the delayed write-completion reply. At turn end, an established acknowledgement must select the existing emission/commit path: success if emitted and committed, `NotRecorded` if emission failed. Add the delayed-write-completion regression using the existing failpoint.

**Spec verdict:** ready as contract text. The three amendments resolve the r4 wording conflict consistently; the additional implementation defect violates them.

Independent verification: all **21 focused steer tests passed**; the additional probe failed twice. I verified 271 tracked build-input files against 6d47cd6. Git status remained clean. No other Blocker or Important finding was established.

I did not rerun the full reported gate, musl checks, release measurements, upstream JSON-Schema-Test-Suite, or real vendor adapters. The probe verifies Driver’s emitted observation and result; it does not independently exercise the final Core RPC transaction.