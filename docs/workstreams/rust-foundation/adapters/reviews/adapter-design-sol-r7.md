**UNSOUND.** The scope decisions are correctly reflected. Remaining problems concern the scan specification and its acceptance checks.

All design-line references below are to revision 7.

| Item | Status | One-line reason | Line |
|---|---|---|---|
| Part 1 #4 — VX14 clean tests | fixed | Per-thread clean tests removed; server-close tests retained. | [1002](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1002) |
| Part 1 #7 — AD9/AC2 | fixed | Exclusion now covers untracked descendants; reported server items still count. | [692](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L692), [942](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L942) |
| Part 1 #9 — recovery | fixed | Decision 2 removes recovery scanning and marker persistence; recovered reports are `null`. | [723](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L723), [957](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L957) |
| Part 1 #10 — tests | partly | Survivor expectations corrected, but tests (6)–(8) still specify only negative assertions under failure-first. | [742](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L742) |
| Part 1 #12 — VC7 | fixed | Early EOF completion is qualified by absence of an active tool. | [975](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L975) |
| Part 2 #1 — recovery prefilter | fixed | Resolved by decision 2; the bound is memory-only. | [726](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L726) |
| Part 2 #2 — process identity/liveness | partly | Retained proc descriptor fixes PID-reuse attribution; final `stat` cannot recheck UID. | [726](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L726) |
| Part 2 #3 — coverage failures | partly | Denials and `hidepid` are covered, but failed reads before eligibility is established have no explicit disposition. | [727](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L727) |
| Part 2 #4 — bounds | partly | Trigger, absolute deadline and cancellation specified; exact-cap versus oversized environments remain ambiguous. | [725](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L725), [727](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L727) |
| Part 2 #5 — report semantics | fixed | Timestamp basis, count semantics, deterministic retention, encoding and truncation signal defined. | [728](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L728) |
| Part 2 #6 — privacy | fixed | Marker persistence removed; guarantees scoped, `comm` disclosed, sentinel checks specified. | [729](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L729), [745](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L745) |
| Part 2 #7 — environment fixture | fixed | Uses independently verified procfs-visible marker absence after exec. | [743](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L743) |
| Part 2 #8 — carrying surfaces | fixed | Host → Wire → Route → Adapter → Core/Store seams and retained destinations are named. | [306](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L306), [724](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L724) |
| Part 2 #9 — ordering/replay | fixed | Shared loss snapshot precedes fan-out; close/event/operation results persist atomically. | [722](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L722), [746](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L746) |
| Part 2 #10 — missing destinations | fixed | Named limitations and `via-jm4.24`; no scan/log without a destination. Last-lease wording remains only an idle-retirement rule. | [723](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L723), [887](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L887) |
| Part 2 #11 — cleanup/no-launch | fixed | Reported-item waiting retained; no-launch row matches C1 §7.4. | [688](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L688) |
| Part 2 #12 — acceptance tests | partly | Controlled fixtures and survivor expectations improved; negative-only tests remain, and server-loss acceptance improperly permits `null`. | [733](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L733), [1386](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1386) |
| Part 2 #13 — dependencies/ownership | fixed | Every `x.3.3` precedes S-LEFTOVER, which gates both integration beads; shared writes explicitly serialized. | [1361](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1361) |
| Part 2 #14 — residual wording | fixed | VC7, VX14, P11 and pending-choice introduction corrected. | [14](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L14), [89](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L89) |
| Part 2 #15 — amendment audit | fixed | Required C2/C1/runtime surfaces and owners added; maximum report and capped `stopped` fixture assigned. | [1033](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1033), [1384](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1384) |
| Conflict-4 audit completeness | fixed | All round-6 occurrences included; platform 269/328 explicitly unchanged. | [957](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L957), [1040](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1040) |
| VO11 note | fixed | Supervised server death distinguished from recovery after daemon death. | [1018](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1018) |

**New defects introduced by the fixes**

1. **Important — the final UID check is impossible as written.**  
   **Location:** [AD20 How, 726](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L726).  
   **Defect/evidence:** The final `stat` reread cannot show “the same start ticks and uid”: UID belongs to `status`, not `stat`. The retained descriptor correctly prevents reads switching to a replacement PID, but does not freeze credentials or process state. [Linux procfs documentation](https://www.kernel.org/doc/html/latest/filesystems/proc.html).  
   **Smallest fix:** Reread `status` through the retained directory descriptor for real UID, and explicitly apply zombie/dead exclusion to the final state observation. Preserve the separate disappearance rule.

2. **Important — capturing the vendor’s start bound has an unspecified race and failure path.**  
   **Location:** [AD20 How, 726](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L726).  
   **Defect/evidence:** “At spawn Host reads” is not an established synchronization point. The anchor sends only the PID in [anchor.rs:321](../../../../../crates/via-host/src/anchor.rs#L321), then can reap the vendor at [261](../../../../../crates/via-host/src/anchor.rs#L261), before Host processes the reply at [host.rs:1443](../../../../../crates/via-host/src/host.rs#L1443). A fast vendor may leave survivors while its start metadata disappears or its PID is reused. Missing or unverified bounds have no specified report outcome.  
   **Smallest fix:** Capture start ticks in the spawning anchor before reaping and carry them in `Spawned`, retaining them only in memory. If unavailable, return an incomplete report without opening candidate environments. Add the fast-exit fixture and corresponding owned paths.

3. **Important — the integration check permits omitting a required server-loss snapshot.**  
   **Location:** [S-LEFTOVER, 1386](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1386).  
   **Defect/evidence:** “Server loss or close gives an empty or `null` report” permits `null` for supervised `server_lost` turns. AD20 requires their shared completed snapshot; exhausted budget produces an incomplete report, not `null`. LH’s no-survivor observation establishes emptiness, not absence of scanning.  
   **Smallest fix:** Split the assertions: Codex C1 unsubscribe and idle retirement return `null`; supervised server loss with in-flight turns returns a non-null shared report, potentially empty or incomplete.

4. **Minor — the environment cap lacks an EOF boundary rule.**  
   **Location:** [726–727](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L726).  
   **Defect/evidence:** A stream capped at exactly 256 KiB cannot distinguish an exactly-full environment from a longer one without another read or separately verified length. Yet only environments *exceeding* the cap are rejected. An early marker match also cannot establish that the whole environment fits.  
   **Smallest fix:** Either reject whenever the cap is reached, or explicitly permit one lookahead byte. Require complete size validation before counting a match; test both boundary sizes.

5. **Minor — the detection replacement inventory is incomplete.**  
   **Location:** [174](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L174), [725](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L725), [1390](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1390).  
   **Defect/evidence:** The “only … differ” inventory excludes the unqualified scanner trigger/task rule and the three option-A marker qualification clauses. Those also need disposition under B or C.  
   **Smallest fix:** Mark scanner-specific timing/task rules as option A and include the qualification clauses in the replacement inventory. Keep delivery, shape and persistence independent of detection.

**Could not verify**

No scanner or persistent-route implementation exists to exercise these guarantees. The single-generation rule is specified explicitly and supported by AD16’s no-retry rule; runtime enforcement remains unverified. Native PID/UID races, mount-option handling, timestamp conversion, cancellation timing, marker inheritance and actual failure-first results were not exercised.

Mount options can identify the filtering mode; current Linux emits `hidepid=ptraceable`, so detection must recognize that spelling as equivalent to `4`. [Kernel implementation](https://raw.githubusercontent.com/torvalds/linux/master/fs/proc/inode.c).

No files or Git state changed. No tests, `bd`, vendor CLI or model runs were performed; HEAD and final status matched the initial observations.