**UNSOUND.** The green tests miss a reproducible validation denial of service and several C1/C2 contract violations.

All references below are at **b8a3f62**, reviewed against **40bc0b4**.

1. **Blocker — Schema validation has no computational or memory bound.**  
   [schema.rs:33](../../../../../crates/via-core/src/schema.rs#L33) calls Boon’s full validation synchronously, accumulating its error tree. A schema defining each successive `$defs` entry as two references to its predecessor produces exponential work. Using the same compiler configuration, an **1,178-byte schema took about 413 ms**; a **1,562-byte schema exhausted a 256 MiB process limit and aborted**. Both are far below the intake limits.  
   **Smallest fix:** enforce actual bounds on validation work, recursion and error allocation. Moving it to `spawn_blocking`, or timing out its caller, does not stop the work or prevent process-wide OOM.

2. **Important — The validator does not enforce draft 2020-12.**  
   [schema.rs:23](../../../../../crates/via-core/src/schema.rs#L23) sets Boon’s *default* draft; an explicit `$schema` overrides it. I verified that a draft-07 declaration causes `dependentRequired` to be ignored and accepts `{"a":1}` despite requiring `b`. This contradicts C1 §4 and design #37.  
   **Smallest fix:** reject incompatible dialect declarations, including declarations in embedded resources, or otherwise enforce 2020-12 throughout compilation.

3. **Important — The plan’s effective bound is discarded.**  
   [intake.rs:399](../../../../../crates/via-core/src/intake.rs#L399) builds `Effective` from the resolved model and caller overrides, ignoring `RoutePlan.effective_bound`. [intake.rs:790](../../../../../crates/via-core/src/intake.rs#L790) subsequently reports requested and effective bounds as identical. A real adapter that normalizes a bound will disagree with the receipt, launch input and envelope. The fake echoes the requested bound and conceals this.  
   **Smallest fix:** retain requested and effective bounds separately, take the latter from the plan, and enforce its 32 KiB cap.

4. **Important — Valid `instructions {path}` requests are refused.**  
   [intake.rs:258](../../../../../crates/via-core/src/intake.rs#L258) unconditionally returns “not supported yet.” C1 §4 supports both `{text}` and `{path}`; no ruling defers this form.  
   **Smallest fix:** copy and freeze the file’s contents at intake, then apply the route’s instructions capability check.

5. **Important — Unsupported spawn and resume verbs can reach execution.**  
   [intake.rs:383](../../../../../crates/via-core/src/intake.rs#L383) checks instructions support, but never checks `capabilities.verbs.spawn`. [receipt.rs:443](../../../../../crates/via-core/src/engine/receipt.rs#L443) similarly resumes without checking frozen `verbs.resume`. `check_turn` validates values and compatibility; it receives no operation and does not supply these checks. This violates C2 rule 3.  
   **Smallest fix:** reject each unsupported verb before its receipt and before driver execution.

6. **Important — Steer lacks C1 idempotency.**  
   [api.rs:461](../../../../../crates/via-core/src/api.rs#L461) omits `op_key` and denies unknown fields; [receipt.rs:582](../../../../../crates/via-core/src/engine/receipt.rs#L582) has no durable operation lookup or result. A C1 steer request containing `op_key` is rejected, and a lost response cannot be retried safely. This omission predates the diff but becomes operationally significant when chunk 5 enables delivery.  
   **Smallest fix:** implement durable keyed steer admission/result replay without resending an uncertain delivery. **Contract conflict:** runtime §6 still lists keyed steer as future work, while C1 §§3 and 3.4 already require it.

7. **Important — A durably submitting turn can incorrectly return `no_active_turn`.**  
   [drive.rs:461](../../../../../crates/via-core/src/engine/drive.rs#L461) commits submission before awaiting lane opening. The steering watch exists only after [queue.rs:522](../../../../../crates/via-core/src/engine/queue.rs#L522) installs `Running`. During that interval, [receipt.rs:599](../../../../../crates/via-core/src/engine/receipt.rs#L599) finds no steering target. C1 §3.4 requires waiting for acceptance.  
   **Smallest fix:** publish the submitting target at the durable submission boundary and retain it through lane opening and actor startup.

8. **Important — Steer can target a successor after selecting the original turn.**  
   [receipt.rs:604](../../../../../crates/via-core/src/engine/receipt.rs#L604) checks the canonical turn, but after acceptance it obtains the current lane and sends only an optional vendor turn ID. The selected turn can finish and another start between selection and driver admission. Acceptance may have no vendor ID; C2 also expressly permits ID reuse across generations. Thus `--expect-turn` can pass while delivery reaches a successor and the reply names the original turn.  
   **Smallest fix:** bind control admission to the selected turn and connection generation using a turn-specific ticket or equivalent atomic guard. Another non-atomic recheck is insufficient.

9. **Important — Three paths discard durable steer-delivery observations.**  
   Newly enabled delivery activates these existing discard paths:
   - [drive.rs:2272](../../../../../crates/via-core/src/engine/drive.rs#L2272): late/session-attributed observations during a turn;
   - [drive.rs:2910](../../../../../crates/via-core/src/engine/drive.rs#L2910): retained session observations;
   - [lane.rs:817](../../../../../crates/via-core/src/engine/lane.rs#L817): observations between turns.  
   
   A delivery racing terminal settlement can consequently succeed without a durable `steer.delivered`. C1 §6 and C2’s session drain require these durable items to be committed; the late-terminal deferral does not cover them.  
   **Smallest fix:** support `SteerDelivered` in all three paths, preserving turn attribution and `late`.

10. **Important — Refusal metadata is dropped.**  
    [intake.rs:291](../../../../../crates/via-core/src/intake.rs#L291) preserves a route only when a refusal supplies a field, and never preserves `verb`. Known-route refusals without fields therefore lose their route. Both steer unsupported paths—[receipt.rs:594](../../../../../crates/via-core/src/engine/receipt.rs#L594) and line 634—lose verb, harness and route. This disagrees with C2’s refusal DTO and C1 §8’s unsupported-verb representation.  
    **Smallest fix:** give error context owned or `Cow` strings and serialize the available context independently of `data.field`.

11. **Important — Failed delivery is misclassified as admission refusal.**  
    [receipt.rs:637](../../../../../crates/via-core/src/engine/receipt.rs#L637) conflates `OverCapacity` with `NotDelivered`, whose C2 meaning is “input was not written whole.” [api.rs:1183](../../../../../crates/via-core/src/api.rs#L1183) maps both to `admission_refused`. C1 §8 does not define a partial/failed vendor write as admission refusal, and §3.4 does not specify these mappings.  
    **Smallest fix:** resolve the C1/C2 mapping explicitly and preserve capacity refusal versus failed or uncertain delivery. This needs a contract ruling; choosing another existing code arbitrarily would not establish fidelity.

12. **Important — Recovery invents effective envelope values despite having durable ones.**  
    [recovery.rs:377](../../../../../crates/via-core/src/engine/recovery.rs#L377) deliberately constructs `TurnPlan` without the turn’s stored `effective`. Recovery consequently substitutes requested model for resolved model, null effort/bound, false inheritance and empty vendor options.  
    **Smallest fix:** read the recovering turn’s own frozen values and pass them into envelope assembly. The latest queued turn’s values are not an adequate substitute.

13. **Important — Running status loses known handshake versions.**  
    [sql.rs:2409](../../../../../crates/via-store/src/runtime/sql.rs#L2409) reads version fields exclusively from the terminal envelope. [read.rs:367](../../../../../crates/via-core/src/engine/read.rs#L367) interprets their absence as untested. After a tested handshake, a running turn still reports null version and an untested warning. C1 §3.7 permits null only before a handshake.  
    **Smallest fix:** expose handshake instance facts generically while the turn runs and combine them with the selected Store snapshot. C2’s terminal-only `InstanceReport` path is insufficient for this C1 requirement.

14. **Important — Corrupt frozen JSON silently becomes defaults.**  
    [intake.rs:684](../../../../../crates/via-core/src/intake.rs#L684) discards parse errors for both session parameters and capabilities. For example, malformed frozen instructions can erase instructions and session vendor options while leaving a valid route available for execution. T3 §7.3 requires unparseable frozen JSON to fail submission, both live and during restart.  
    **Smallest fix:** make frozen decoding fallible and route corruption through the existing `commit_submit_failed` handling.

15. **Important — Omitted cwd bypasses the encoded member cap.**  
    [receipt.rs:131](../../../../../crates/via-core/src/engine/receipt.rs#L131) returns the startup cwd before `cwd_fits`. A filesystem-valid path containing escaped characters can fit the OS path limit while exceeding 4 KiB encoded. Explicit cwd is rejected; the identical default cwd is accepted.  
    **Smallest fix:** check the resolved cwd in both branches.

16. **Important — Non-completed structured output bypasses validation.**  
    [drive.rs:3027](../../../../../crates/via-core/src/engine/drive.rs#L3027) returns before validating any failed, cancelled or unknown turn’s retained output. C1 §5 requires validation before storing the value; design #37 restricts only the *missing-output warning* to completed turns, while present invalid output becomes `structured_output_invalid`. No ruling approves this narrowing.  
    **Smallest fix:** validate every present value before inline storage or spilling, and reconcile failure precedence explicitly where another terminal cause exists.

17. **Important — Required daemon conformance coverage is incomplete.**  
    [conformance_daemon.rs:82](../../../../../crates/via-cli/tests/conformance_daemon.rs#L82) and [line 153](../../../../../crates/via-cli/tests/conformance_daemon.rs#L153) cover only handshake refusal and model-only spawn/steer. The brief requires Core **and daemon** proof for partial requirements, AD12, inheritance/clearing, envelope fields, schema validation and AD13/AC7 warnings. Those additional cases are exercised directly through Engine, not the daemon boundary.  
    **Smallest fix:** add the missing raw-protocol daemon cases, including their relevant refusal and recovery paths.

18. **Minor — Acceptance-wait tests can pass without entering the wait.**  
    [conformance_intake.rs:573](../../../../../crates/via-core/tests/conformance_intake.rs#L573), line 609, and [conformance_daemon.rs:223](../../../../../crates/via-cli/tests/conformance_daemon.rs#L223) sleep 200 ms and assert the task/thread is unfinished. An unscheduled request satisfies that assertion too.  
    **Smallest fix:** synchronize on observable request entry into the submitting wait before releasing acceptance; use timeouts only as failure bounds.

19. **Minor — Frozen-data documentation no longer describes the implementation.**  
    [runtime.rs:205](../../../../../crates/via-store/src/runtime.rs#L205) and runtime contract §6 still describe four session-param keys. Chunk 5 correctly adds instructions, vendor and inheritance facts. The internal turn `effective` also now contains frozen fields beyond the public receipt DTO.  
    **Smallest fix:** update the storage description to distinguish internal frozen values from public receipt fields. Keeping effective inheritance in `sessions.params` agrees with C2 §6.2 and design #25.

20. **Minor — New code continues growing oversized modules.**  
    [drive.rs:3018](../../../../../crates/via-core/src/engine/drive.rs#L3018) adds output-validation policy to a roughly 3,000-line module; new [intake.rs:665](../../../../../crates/via-core/src/intake.rs#L665) mixes parsing, planning, persistence decoding and envelope projection in over 800 production lines. This misses the coding-style instruction to place new code in another module beyond that size.  
    **Smallest fix:** extract output finalization and frozen-value decoding/projection along their existing responsibilities.

The deviations and concerns resolve as follows:

| # | Verdict |
|---|---|
| 1 | Fixed refusal messages are permissible; C1 does not require verbatim adapter messages. Losing structured context is not permissible—finding 10. |
| 2 | Unacceptable. `&'static` is an implementation constraint, not authorization to omit error context. |
| 3 | Unacceptable—finding 4. |
| 4 | Unresolved contract mapping, with an incorrect conflation of capacity and failed delivery—finding 11. |
| 5 | Unacceptable—finding 12. |
| 6 | Unacceptable—finding 13. |
| 7 | Effective inheritance belongs in frozen session parameters. The spawn transaction stores it consistently; runtime §6 needs updating. Custom configuration wiring remains S-LAUNCH’s responsibility. |
| 8 | Sound: `bundled` matches C2 `ModelEntry` and the design’s migration. |
| 9 | Sound: C1-shaped bound/vendor maxima restore a meaningful bounds test without changing its outcomes. |
| 10 | Sound read extensions and one Store call. Their envelope-only version source and permissive frozen decoding remain defective. |
| 11 | Sound under the accepted chunk-4 ruling: verification derives from the committed identity and applies to the current in-memory connection generation. |

The inspected transaction paths correctly commit spawn’s frozen values together, serialize resume inheritance under admission, and advance `adapter_version` with `turn.started`. Structured-output spills write and sync the file and directory before the naming commit; failed writes do not name a partial file. I found no additional unapproved S1 outcome changes, surviving Core harness-literal paths, or newly exposed test-only seams in release code.

Validation passed: formatting, strict workspace Clippy, layer and harness-literal guards, offline `cargo deny`, **608 default workspace tests**, and **32 selected failpoint tests**, including the combined envelope maximum. The targeted Core/daemon conformance tests also passed. Source and Git state remained unchanged.

I did **not** independently run the full failpoint suite, musl/F24 qualification, release-feature check, or reconstruct every baseline RED run. No vendor CLI or model was run. Adapter-bound normalization and steer race findings are grounded in the permitted C2 behavior and source interleavings, rather than live-vendor reproductions.