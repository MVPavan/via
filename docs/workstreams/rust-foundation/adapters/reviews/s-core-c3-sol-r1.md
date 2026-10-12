**UNSOUND.** The existing tests pass, but the driver has lifecycle, deadline, identity and conformance gaps that block acceptance.

Reviewed all 17 changed files in `a979d34..c5a29d5`. Verification performed:

- Four-crate nextest suite: **170 passed**.
- Same suite with `via-core/test-failpoints`: **175 passed**.
- Lowered-stall selection: **1 passed** in **0.39 s**.
- Layer checker and diff whitespace check: passed.
- Git status remained clean; no remaining VIA/fake-agent processes from this worktree were observed.

Findings below are source-verified; the counterexamples were not added as tests because this review prohibits file edits.

**Findings**

1. **Blocker — session close reports cleanup without performing it.**  
   [crates/via-adapters/src/driver.rs:320](../../../../../crates/via-adapters/src/driver.rs#L320)  
   Both modes ignore their arguments, clear the persistent slot and return `vendor_closed=true`, `cleanup=Quiescent`. A concurrently running `run_turn` continues independently. Neither its process nor its connection tasks are closed or joined. This also releases capacity before that active process has stopped. C2 explicitly permits concurrent session close and requires actual close evidence.  
   **Smallest fix:** connect close to the active turn’s owned stop/cleanup operation; retain capacity through cleanup and report only established facts. Test both modes during handshake, acceptance wait and an active tool.

2. **Important — persistent reservations survive failed acquisition and cancelled futures.**  
   [driver.rs:265](../../../../../crates/via-adapters/src/driver.rs#L265), [fake/driver.rs:131](../../../../../crates/via-adapters/src/fake/driver.rs#L131)  
   `connect` installs capacity and marks the connection live **before acquisition**. Cleanup invalidates it only for `ServerLost`, `TransportLost`, or a delivered `VendorClosed`. Acquisition failure, handshake refusal, protocol failure, observation overflow and daemon force leave it pinnable. Dropping or unwinding the turn future also skips `state.steer=None` and the disconnect logic. Capacity can therefore remain occupied with no usable connection until explicit close or driver destruction.  
   **Smallest fix:** use an owned reservation/active-turn guard. Roll back unsuccessful launches, invalidate failed generations, and preserve ownership of launched cleanup across cancellation and unwind. Cover every listed exit, including future drop.

3. **Important — cancellation during handshake can send input after cancellation.**  
   [fake/runtime.rs:425](../../../../../crates/via-routes/src/fake/runtime.rs#L425), [fake/runtime.rs:797](../../../../../crates/via-routes/src/fake/runtime.rs#L797)  
   The pre-start stop check precedes the awaited handshake. During that wait, `on_wake` enqueues an interrupt although no start was sent. If the handshake then completes before `force_at`, execution proceeds to write the start without rechecking the order. This violates the post-ARM/pre-submission stop rule.  
   **Smallest fix:** track whether submission has begun; before submission, an order closes without interrupt or prompt. Recheck immediately after handshake and before writing start.

4. **Important — persistent return conditions do not implement AD4/P7.**  
   [fake/runtime.rs:461](../../../../../crates/via-routes/src/fake/runtime.rs#L461), [fake/runtime.rs:492](../../../../../crates/via-routes/src/fake/runtime.rs#L492)  
   Every terminal still enters per-process finalization:
   - `Completed` and `Failed` wait for EOF/exit rather than returning on semantic completion.
   - `Interrupted` with no open tools still waits for EOF/exit.
   - Tools ending after acknowledgement do not trigger return.
   - Conversely, EOF/exit with unresolved tools can return before P7 expires.

   The grace timer only handles an open tool while another wait remains pending.  
   **Smallest fix:** implement explicit persistent completion conditions: natural terminal; or interrupted terminal plus settled tools/P7 bound. Retire the emulation process separately under owned, bounded cleanup.

5. **Important — wall cleanup starts successive budgets.**  
   [fake/runtime/lane.rs:369](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L369), [fake/runtime.rs:268](../../../../../crates/via-routes/src/fake/runtime.rs#L268), [fake/driver.rs:191](../../../../../crates/via-adapters/src/fake/driver.rs#L191)  
   Wall expiry grants soft stop a fresh three seconds, then force-close/drain another fresh three seconds. Remaining observation delivery can subsequently wait for the ten-second stall timer. This is not AD4’s single cutoff of wall failure plus three seconds.  
   **Smallest fix:** calculate one absolute cleanup cutoff at failure and propagate it through soft stop, helper retirement, draining and remaining delivery.

6. **Important — acknowledgement can precede confirmed delivery and phase validation.**  
   [fake/runtime/lane.rs:234](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L234), [fake/runtime/lane.rs:319](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L319), [fake/runtime.rs:767](../../../../../crates/via-routes/src/fake/runtime.rs#L767)  
   `interrupted=true` means enqueued, not written; interrupt write outcomes are discarded. `note` sets acknowledgement before `Phase::advance` validates the terminal. Wall soft-stop never performs that phase validation. Likewise, `SteerDelivered` resolves its reply before successful steer-write completion is established. Buffered or contradictory vendor messages can therefore manufacture successful control outcomes.  
   **Smallest fix:** separate queued/written/acknowledged states, validate message phase first, and reconcile vendor evidence with the corresponding write result. Keep RPC `interrupt_ack` insufficient for cancellation settlement.

7. **Important — persistent result rewriting discards physical evidence.**  
   [fake/runtime/lane.rs:395](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L395)  
   Successful results overwrite Host cleanup and force facts; most failures overwrite them too. Thus housekeeping can expose a force-caused process exit while reporting `forced=false`. The rewrite also turns a pre-launch stop with no cleanup requirement into `Some(Uncertain)`. H1 authorizes emulation, but these fields mix helper-process evidence with logical-server outcomes without distinguishing them.  
   **Smallest fix:** explicitly separate helper retirement facts from logical shared-connection stop facts; preserve no-launch and Host evidence instead of blanket rewriting.

8. **Important — health never reports failure; required session ownership inputs are absent.**  
   [driver.rs:23](../../../../../crates/via-adapters/src/driver.rs#L23), [driver.rs:205](../../../../../crates/via-adapters/src/driver.rs#L205), [driver.rs:343](../../../../../crates/via-adapters/src/driver.rs#L343)  
   Health only transitions `Open → Closed`; protocol, transport, overflow, Store and task failures never publish `Failed`. `SessionCx` also omits C2’s tracker and cancellation inputs. Lower layers own their tasks, but that does not supply the missing session failure/cancellation interface or repair cancelled driver state.  
   **Smallest fix:** wire first-failure health and session cancellation/cleanup ownership into the driver. Resolve the dependency seam with the coordinator rather than silently omitting the contract.

9. **Important — steer lacks the contract’s target check and control bounds.**  
   [driver.rs:109](../../../../../crates/via-adapters/src/driver.rs#L109), [driver.rs:286](../../../../../crates/via-adapters/src/driver.rs#L286), [fake/runtime/lane.rs:344](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L344)  
   `SteerInput.expected_vendor_turn` is missing. Arbitrarily large steer strings enter a count-bounded queue without C2’s 64-KiB control budget, then use Wire’s data/`Start` path, bypassing its control-size enforcement. A declared partial steer profile also returns `Injected` unconditionally rather than its declared semantics. ID 3 itself is not the problem.  
   **Smallest fix:** carry and validate the expected target, enforce encoded control-byte admission, preserve partial semantics, and provide an appropriate bounded Wire control write.

10. **Important — identity continuity is neither retained nor enforced.**  
    [fake/driver.rs:114](../../../../../crates/via-adapters/src/fake/driver.rs#L114), [fake/driver.rs:505](../../../../../crates/via-adapters/src/fake/driver.rs#L505)  
    Every turn reconstructs the normalizer from the original `SessionSpec` ID. An identity confirmed in turn 1 is not retained for turn 2; multiple identities within a turn are also not checked against the first confirmation. With a historical ID, mismatch merely emits an observation while acceptance and successful completion remain possible. C2 requires mismatch to fail without replacing identity.  
    **Smallest fix:** retain confirmed identity in session state, track generation verification, and make mismatches terminate the lane before acceptance.

11. **Important — the between-turn session lane is not exercised or implemented.**  
    [conformance_driver.rs:807](../../../../../crates/via-core/tests/conformance_driver.rs#L807), [fake/driver.rs:492](../../../../../crates/via-adapters/src/fake/driver.rs#L492)  
    `VendorClosed` is read before `run_turn` returns. The test proves null turn attribution, not delivery between turns. There is no session-owned source polled while the driver is idle, so an idle close cannot arrive, release its slot or invalidate a previously obtained pin.  
    **Smallest fix:** add an owned, independently gated session event source to the emulation; emit the close only after the previous `TurnEnd` has returned and assert its idle effect.

12. **Important — accepted parameters are discarded, and test (18) misses catalog validation.**  
    [fake/driver.rs:57](../../../../../crates/via-adapters/src/fake/driver.rs#L57), [fake/driver.rs:92](../../../../../crates/via-adapters/src/fake/driver.rs#L92), [conformance_driver.rs:681](../../../../../crates/via-core/tests/conformance_driver.rs#L681)  
    Per-turn values are checked, but only the prompt reaches `TurnStart`. Native-profile effort—and profile-supported bound/schema/step-limit values—cannot be asserted or applied by the fake request. Session model/instructions likewise have no request representation. Test (18) rejects `"turbo"` against the same compiled table before launch; it never exercises a value accepted by planning but rejected after catalog discovery.  
    **Smallest fix:** extend the C2 fake request/fixture surface for supported effective values, preserving legacy requests. Add a distinct catalog-only mismatch with an explicit no-submission witness.

13. **Important — new vendor payload handling violates tolerance and size rules.**  
    [fake/mod.rs:128](../../../../../crates/via-routes/src/fake/mod.rs#L128), [fake/mod.rs:152](../../../../../crates/via-routes/src/fake/mod.rs#L152), [fake/mod.rs:456](../../../../../crates/via-routes/src/fake/mod.rs#L456)  
    `FakeUsage` and `FakeCost` reject unknown vendor fields despite the tolerant vendor-input rule. Decode enforces the larger Wire cap, but not C2’s 256-KiB known-payload cap: large handshake feature collections and structured-output payloads are accepted. Final-text splitting does not bound those other payloads.  
    **Smallest fix:** ignore unknown fields in these vendor DTOs; enforce the known-payload limit while retaining the explicit final-text splitting exception.

14. **Minor — observation byte accounting omits retained payloads.**  
    [observation.rs:387](../../../../../crates/via-adapters/src/observation.rs#L387)  
    Cost excludes identity transcript paths, warning data and most `LateTerminal` contents: detail, code, structured output, usage key, cost scope and vendor data. These omissions invalidate the advertised accounting when those observations are emitted.  
    **Smallest fix:** account for every retained variable-size field, using checked/saturating arithmetic and admission limits.

15. **Minor — newly discarded errors lack required explanations.**  
    [fake/runtime/lane.rs:243](../../../../../crates/via-routes/src/fake/runtime/lane.rs#L243)  
    Instances include reply sends at lines 243, 337 and 360; the flush result at 380; and ID conversions using `.ok()` in `fake/driver.rs:381` and `:415`. Coding-style §4 requires explaining why discarding each error is safe. The interrupt-write outcome at `fake/runtime.rs:716` needs actual handling, as finding 6 describes.  
    **Smallest fix:** document benign receiver disappearance and proven conversions; handle consequential write/flush failures.

**Conformance verdicts**

The brief’s numbered items come from the design’s S-CORE acceptance list. This base’s C2 §7 has **17 items**, with different numbering.

| Brief item | What the test establishes | Missing proof |
|---|---|---|
| 5 | Unknown stop reason and failed `MaxSteps` fields survive | Adequate for the named adapter half |
| 7 | A decoded terminal survives this stalled-queue layout | Does not prove independent controls under saturation |
| 8 | Hanging open tool becomes uncertain; tool ended before acknowledgement becomes quiescent | Post-ack completion, immediate settled return, EOF-before-bound, and `force_at`/`close_by` independence |
| 11 | One native steer succeeds; idle/default unsupported refusals work | Target mismatch, partial semantics, byte bounds, failed writes and congestion |
| 16 | Instance survives the listed failures; pre-handshake wall yields null | Good evidence for those cases |
| 17 | This fake distinguishes exit from live stdout loss and releases slots | Does not cover other failing-generation exits |
| 18 | Compiled-table rejection and pre-start handshake refusal | **Catalog-only mismatch is absent** |
| 20 | Wall interrupt obtains an interrupted response, or remains unacknowledged | Single cutoff, earlier/capped order cases, actual write failure and physical/logical force distinction |
| Vendor close | Null turn attribution after a terminal | **No actual between-turn delivery** |
| Recover | Quiescent supplied facts yield `Dead`; partial/empty facts yield `Unknown` | Adequate for the fact-only adapter half |

The reported RED runs replaced whole operations with `Unavailable`/`Unknown`. They demonstrate sensitivity to a missing implementation, not to the individual contract defects. They also do not satisfy the mandated test-before-code order. Targeted failing cases are needed; another whole-lane stub would not resolve this validation gap.

**Four deviations**

| Deviation | Verdict |
|---|---|
| Editing chunk 2’s `plan.rs` | **Acceptable.** Required constructor/runtime ownership migration; planning expectations remain intact. |
| Parallel lane error types | **Acceptable temporary bridge.** Preserves exhaustive legacy matches. Chunk 4 must converge on the C2 error surface. |
| Planning-kit rename in `7815d9c` | **Minor commit-organization deviation.** Related migration, no independent correctness issue; no history rewrite needed merely for grouping. |
| Tests hosted in via-core | **Correct.** Core can access Adapter and Store without adding a forbidden Adapter→Store edge. Layer checker passed. This adds no production Core harness logic. |

**Eight author concerns**

| Concern | Verdict |
|---|---|
| Steer through `Start`, ID 3 | ID is acceptable; unbounded data-lane admission and missing target/partial semantics are not—finding 9. |
| Persistent `forced=false`; post-wall terminal discarded | Force evidence needs separation—finding 7. Keeping a post-wall terminal out of the retained pre-wall terminal is consistent with AD4; acknowledgement still needs finding 6 fixed. |
| `HandshakeRefused` wrapped as Route failure | Acceptable transitional typed representation **if chunk 4 maps it to `submit_failed`, reason `handshake_refused`**, preserving instance/cleanup evidence. It must not become generic protocol failure. |
| Health only Open→Closed; leftovers always None | Health is defective—finding 8. Null leftovers are explicitly authorized by H5 pending S-LEFTOVER. |
| VendorClosed inside the turn process | Does not satisfy between-turn delivery—finding 11. |
| No SessionCx tracker/cancellation token | Unresolved contract omission—finding 8. No Adapter-owned `tokio::spawn` was found, but ownership/cancellation across the driver lifetime remains incomplete. |
| Tests precommit turns 2–3 | Appropriate lower-layer setup; not evidence of Core dispatch correctness. |
| Fact-only recovery; never resumes | Correct for the fake’s unsupported recover capability, assuming Core supplies the complete relevant Host facts. |

The ten-second test is unnecessary: the existing failpoint override can safely be supplied through the child environment. I verified `VIA_TEST_EVENT_STALL_MS=250` with the test selected; no unsafe Rust environment mutation or new injection API is needed.

**Out of scope, noticed**

- Version/refusal caching belongs to S-LAUNCH; leftover detection belongs to S-LEFTOVER.
- The harness-literal guard is absent on this base as expected. No production Core/CLI files changed.
- Three commit messages contain Claude session URLs (`7815d9c`, `18a9fb4`, `c5a29d5`). These are session identifiers in public history; remove unnecessary private provenance when preparing public commits. I did not access the links.
- Existing pre-acceptance unknown-notification rejection remains in the shared phase machine; it predates this chunk and conflicts with C2’s activity-only rule.

**Could not verify**

The full CLI/daemon S1 suite, workspace/release gates, author’s historical RED executions, and adversarial panic/drop scenarios were not rerun. The permitted legacy Core/Route tests passed without changed assertions; that supports preserved tested behaviour, not complete S1 equivalence. Tests used the existing sibling `via` anchor binary.