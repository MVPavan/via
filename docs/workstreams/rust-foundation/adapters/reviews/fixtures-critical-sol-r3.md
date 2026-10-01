**UNSOUND.** The original r2 witnesses are addressed, but the fixes introduce valid-expectation rejections, stale progress markers and a privacy bypass.

At `ea0ebd6`, **79 active tests passed; 31 conformance cases remain ignored**. Current fixtures pass validation and hygiene scanning without false positives. Repository files and Git state remained unchanged.

“Routed” means assigned to a sufficiently explicit named test—not implemented or passing. Spec links below point to the coordinator’s uncommitted main-checkout edits.

| R2 item | Status and evidence |
|---|---|
| 1 — Pipelined causality | **Fixed.** All 15 exceptions name valid earlier emits; c11 pins the `thread/start` reply. Invalid predecessors are rejected. [replay:516](../../../../../crates/via-fake-agent/src/replay.rs#L516), [c11:120](../../../../../crates/via-adapters/tests/fixtures/codex/c11_failed_command.replay.json#L120). |
| 2 — Cold-initialize readiness | **Fixed as a driver obligation.** Explicit readiness, polling through the guarded wait and processing expired timers replace `SigCgt`. Implementation remains unavailable. [checker:130](../../../../../crates/via-core/tests/support/conformance_expect.rs#L130). |
| 3 — SIGNAL_SKEW | **Fixed mechanism; new launch-reuse defect below.** Completion precedes the explicit consumption marker; no skew allowance remains. [replay:739](../../../../../crates/via-fake-agent/src/replay.rs#L739). |
| 4 — Late signals | **Fixed.** Queued arrivals after the deadline fail independently of watchdog scheduling; unit witness passes. [signals:87](../../../../../crates/via-fake-agent/src/replay/signals.rs#L87). |
| 5 — Nested validation | **Partly fixed.** Original malformed witnesses are rejected. New usage-shape and provenance defects remain below. [checker:831](../../../../../crates/via-core/tests/support/conformance_expect.rs#L831), [checker:1040](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1040). |
| 6 — Gate enums | **Fixed enums; new identity-order regression below.** `pending` is restricted to gates. [checker:1293](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1293). |
| 7 — Close ordering | **Fixed.** Each session closes after its own last turn, allowing c4’s A unsubscribe before B completes. [checker:155](../../../../../crates/via-core/tests/support/conformance_expect.rs#L155). |
| 8 — Replay exits | **Fixed for step replays and individual lifetimes.** Every current ending is judged correctly; the `--version` obligation conflicts with the helper, below. [replay_exit:35](../../../../../crates/via-core/tests/support/replay_exit.rs#L35). |
| 9 — Hygiene gaps | **Original probes fixed; new bypass below.** Prefixed identities and bare credential arguments are detected. [fixtures:992](../../../../../crates/via-fake-agent/tests/fixtures.rs#L992). |
| 10 — Symlinks | **Fixed.** Recursive scanning rejects symlinks. [fixtures:117](../../../../../crates/via-fake-agent/tests/fixtures.rs#L117). |
| 11 — Null baseline documentation | **Fixed.** `accepted` is explicitly boolean or a reasoned omission. [checker:50](../../../../../crates/via-core/tests/support/conformance_expect.rs#L50). |
| 12 — Resume provenance | **Fixed.** Both descriptions now state literal pins. [c1a:2](../../../../../crates/via-adapters/tests/fixtures/claude/c1a.replay.json#L2), [c9a:2](../../../../../crates/via-adapters/tests/fixtures/claude/c9a.replay.json#L2). |
| F8 routing gap | **Soundly routed in the proposed spec.** `codex_bound_gate` explicitly requires frozen instruction bytes on fresh starts and every resume, including null omission. [Codex:495](../../../../../docs/specs/vendors/codex.md#L495). |
| F18 routing gaps | **Soundly routed in the proposed spec.** Pagination failure/cache rejection and malformed/contradictory terminals have explicit named assertions. [Codex:490](../../../../../docs/specs/vendors/codex.md#L490), [Codex:491](../../../../../docs/specs/vendors/codex.md#L491). |
| Crash wording | **Fixed and sound.** Positive Host `GroupAbsent` evidence is required for quiescence; otherwise cleanup is uncertain. This agrees with C2 §2/§4.1 and C1 §3.5. [C2:208](../../../../../docs/specs/adapter-contract.md#L208), [checker:112](../../../../../crates/via-core/tests/support/conformance_expect.rs#L112). |

The new defects are:

1. **Important — Progress markers are ambiguous across repeated single-fixture launches.**  
   [replay.rs:375](../../../../../crates/via-fake-agent/src/replay.rs#L375), [fixtures.rs:672](../../../../../crates/via-fake-agent/tests/fixtures.rs#L672).  
   Only lifetimes fixtures qualify markers. Single fixtures append indistinguishable markers, and readers match anywhere in the accumulated log. A probe followed the documented protocol twice using one fixture path: the first launch exited 0; the second consumed stale readiness and died from SIGUSR1 before installing handlers.  
   **Smallest fix:** qualify every progress marker with its launch ordinal and require consumers to match that launch.

2. **Important — Valid keyed usage samples are rejected; malformed sample members pass.**  
   [checker.rs:1028](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1028), [checker.rs:607](../../../../../crates/via-core/tests/support/conformance_expect.rs#L607).  
   `Ty::Counts` requires every value to be null or an unsigned integer. C2 `UsageSample.key` is a nullable string. A valid keyed sample returned validation error; `{"key":123,"invented_counter":456}` returned `Ok`. This prevents precise assertions of keyed-sample supersession.  
   **Smallest fix:** validate the actual `UsageSample` members: nullable string `key` and the five named nullable unsigned counters.

3. **Important — Gate validation rejects a valid identity-before-acceptance snapshot.**  
   [checker.rs:1346](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1346).  
   Reusing the final-order rule makes an identity observation require an acceptance observation in the same snapshot. A gate with `accepted:false` and `observations_order:["session.vendor_identity_confirmed"]` is rejected, although identity legitimately precedes acceptance.  
   **Smallest fix:** allow identity-only prefixes; enforce relative order when acceptance is asserted or present.

4. **Important — Hyphen-prefixed credential values bypass the new hygiene checks.**  
   [fixtures.rs:1027](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1027), [fixtures.rs:1058](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1058).  
   Both `{"stderr":"--password \"-hunter2\""}` and `{"argv":["--password","-hunter2"]}` returned no finding. The exemption treats every hyphen-prefixed value as another option—even quoted values.  
   **Smallest fix:** remove the blanket exemption, preserve quotation information and narrowly recognize any permitted structural sentinel.

5. **Minor — Token provenance accepts the cost-only value `estimated`.**  
   [checker.rs:1055](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1055).  
   A probe with `usage.provenance:"estimated"` validated. C1 §5 permits `reported`/`unavailable` for usage; only cost permits `estimated`.  
   **Smallest fix:** use a separate usage-provenance enum.

6. **Minor — “Every launch” exit checking incorrectly includes version probes.**  
   [checker.rs:165](../../../../../crates/via-core/tests/support/conformance_expect.rs#L165), [replay_exit.rs:13](../../../../../crates/via-core/tests/support/replay_exit.rs#L13).  
   The fake’s `--version` path exits 0 with empty stderr independently of the fixture’s steps. For `c0_bad_model`, that real fake result was rejected by the shared helper, which expected the recorded exit 1 and diagnostic. The generic driver already checks probes separately.  
   **Smallest fix:** distinguish version-probe launches in the shared verdict contract and require their version output, exit 0 and empty stderr.

7. **Minor — The slowed c11 witness still depends on reader scheduling.**  
   [fixtures.rs:402](../../../../../crates/via-fake-agent/tests/fixtures.rs#L402).  
   The 200 ms delay does not prove that the hoisted line was published before the predecessor emit. If the input thread is delayed longer, replay may legitimately miss the early write under its documented arrival policy, causing the negative test to fail. Renumbering is correct; this is a synchronization weakness, not a recording-fidelity defect.  
   **Smallest fix:** acknowledge input publication before releasing the predecessor emit.

8. **Minor — The early-EOF witness has the same scheduling dependency.**  
   [replay test:1225](../../../../../crates/via-fake-agent/tests/replay.rs#L1225), [replay test:1227](../../../../../crates/via-fake-agent/tests/replay.rs#L1227).  
   Fixed sleeps do not establish readiness or EOF publication before signal consumption. Delayed publication can produce exit 0 and fail the test.  
   **Smallest fix:** wait for explicit readiness and EOF-publication acknowledgements. I did not induce scheduler starvation for findings 7–8.

9. **Minor — The new causal-deviation assertion silently skips lifetimes fixtures.**  
   [fixtures.rs:1328](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1328).  
   Execution handles lifetimes, but verdict inspection reads only top-level `steps`. A lifetimes wrapper yields an empty list and reaches `continue`, dropping its result.  
   **Smallest fix:** inspect the relevant lifetimes through `lifetimes_of()` and assert their deviation verdicts.

10. **Minor — Replay-exit self-checks discard directory-entry errors.**  
    [checker.rs:203](../../../../../crates/via-core/tests/support/conformance_expect.rs#L203).  
    `entry.ok()` silently omits failed entries. With other fixtures still checked, the test can pass despite incomplete enumeration.  
    **Smallest fix:** propagate entry errors rather than filtering them out. This failure path was identified statically, not induced.

I could not verify private-recording fidelity, executable adapter `drive()` implementations, controlled-clock gate behavior inside those drivers, or acceptance metadata through Beads. The named adapter assertions remain future obligations. No vendor CLI/model or `bd` command ran.