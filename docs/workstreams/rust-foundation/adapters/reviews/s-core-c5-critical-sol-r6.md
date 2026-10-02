**SOUND.** r5 #1 is addressed. I found no new defect or remaining Blocker/Important issue in the reviewed chunk.

| Finding | Status at c0b2cb7 |
|---|---|
| r5 #1: acknowledged steer misclassified while Wire’s reply is pending | **Resolved.** [lane.rs:453](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L453) records acknowledgement before forwarding the report. [driver.rs:743](../../../../../crates/via-adapters/src/driver.rs#L743) gives that acknowledgement precedence over refusals and turn-end uncertainty, then follows the emission result. Both new regressions pass. |

Verdicts on the concerns:

1. **Accept acknowledgement precedence over `NotWritten`.** Established vendor delivery should survive a later local write failure. The branch implements this correctly; that particular failure ordering remains dynamically untested.
2. **Accept the late-event semantics, with a factual correction.** A later emitted delivery observation can commit without a surviving ticket; this is consistent with an earlier uncertain answer. Core’s late-steer tests pass. However, current fake retirement closes the helper and drains Wire tasks—it does **not** decode and forward new delivery reports during retirement. See [lane.rs:841](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L841).
3. **Accept failpoint-only tests.** Both ran in this review. Their checkpoints establish the relevant ordering without sleeps.

**Spec diff:** ready as contract text; the reviewed fix matches it.

Independent verification passed: **23 focused tests**, workspace Clippy with failpoints, and both source guards. I verified 271 tracked build-input files against c0b2cb7. Git status remained clean.

I did not rerun the full reported gate, musl checks, release measurements, upstream JSON-Schema-Test-Suite, or real vendor adapters. The write-failure-after-acknowledgement case was checked from source, not reproduced.