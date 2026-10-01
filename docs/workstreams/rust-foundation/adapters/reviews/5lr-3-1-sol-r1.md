**UNSOUND.** No privacy leak was found in the 33 changed files. The important findings concern contract expectations, fidelity, and whether the tests pin the promised behavior.

Paths beginning `F/` below mean `crates/via-adapters/tests/fixtures/codex/`.

**Findings**

1. **Important — hygiene checks miss sensitive-value classes.**  
   The Claude-owned `crates/via-fake-agent/tests/fixtures.rs:333` scans raw text for patterns, while its recursive check only looks for credential fields. At line 416, parsing an embedded Codex emit containing `"id":${request}` fails, silently skipping its credential fields. Its placeholder rule at line 395 also accepts arbitrary values containing `REDACTED` or `PLACEHOLDER`.

   Read-only reproductions missed all five classes: credential fields inside templated emits, lowercase bearer authorization, Unicode-escaped home paths, doubly escaped Windows home paths, and credentials accepted by the broad placeholder rule. The self-test at line 453 covers individual examples, not these representations. The local Python scanner also misses escaped credential keys inside emit strings.

   **Smallest fix:** scan decoded strings recursively; replace capture expressions with safe sentinels before parsing emit JSON; match bearer authorization case-insensitively; use exact placeholder values; add these witnesses to the shared self-test. This belongs to the Claude/coordinator-owned driver. No committed leak was found from these classes.

2. **Important — a required terminal is marked unasserted.**  
   [c3_wall_interrupt.expect.json:59](../../../../../crates/via-adapters/tests/fixtures/codex/c3_wall_interrupt.expect.json#L59) excludes `terminal` from comparison because its retention is allegedly unspecified. C2 §4.1 explicitly requires retaining the decoded terminal, including alongside a failure ([adapter-contract.md:406](../../../../../docs/specs/adapter-contract.md#L406)); only evidence after the cleanup cutoff is late. This fixture supplies the interrupted terminal within that cutoff.

   `usage` is also unasserted without identifying a contract ambiguity: the replay supplies the same keyless sample asserted in the interrupt case.

   **Smallest fix:** assert the interrupted terminal alongside `deadline`; assert the supplied usage under AD6, or document a concrete unresolved delivery question. The `c0` leftovers omission is a declared workstream deferral, rather than an unsettled C2 rule.

3. **Important — the effort case imposes an ordering C2 does not require.**  
   [c7_effort_catalog.expect.json:37](../../../../../crates/via-adapters/tests/fixtures/codex/c7_effort_catalog.expect.json#L37) forbids identity confirmation, and its replay only permits `thread/start` after the rejected attempts. AD18 requires validation after discovery and before **`turn/start`**, not before thread creation ([adapter-contract.md:579](../../../../../docs/specs/adapter-contract.md#L579)).

   A conforming driver that creates the thread and then rejects the effort would fail this fixture.

   **Smallest fix:** remove the extra ordering constraint, or have the coordinator explicitly adopt it as an implementation requirement. No C2 vocabulary addition is needed.

4. **Important — the checker does not assert launch counts or close results.**  
   [conformance_codex.rs:33](../../../../../crates/via-core/tests/conformance_codex.rs#L33) represents only turn outcomes; `check()` compares only turn fields. Consequently, every top-level `launches` value and every `session.close.vendor_closed` value is unchecked.

   Instances: `launches` in **all 16 expectation files**; close results in **c11, c1, c2, both c3 cases, both c4 sessions, c5_resume, c6, both c7 cases, c8, and c9**. The critical consequence is that c4’s advertised AD16 launch-count assertion is absent.

   **Smallest fix:** return and compare actual launch counts and close reports in `Outcome`. The future helper should collect facts; the checker should enforce these expectations.

5. **Important — P7’s settlement time is not pinned.**  
   [c3_interrupt_uncertain.expect.json:32](../../../../../crates/via-adapters/tests/fixtures/codex/c3_interrupt_uncertain.expect.json#L32) and [c4_two_sessions.expect.json:45](../../../../../crates/via-adapters/tests/fixtures/codex/c4_two_sessions.expect.json#L45) supply a 300 ms grace period, but neither the outcome nor checker records when settlement occurs. The fake simply waits for unsubscribe.

   A driver returning `uncertain` immediately after acknowledgement would satisfy the same assertions, violating C2 §4.1’s wait until tool completion or the P7 bound.

   **Smallest fix:** use controlled time in `drive()` and assert that the turn remains pending before the bound and settles at it.

6. **Important — recording structure was changed beyond the declared adaptations.**

   | Instance | Defect and evidence | Smallest fix |
   |---|---|---|
   | [c7_bad_model.replay.json:151](../../../../../crates/via-adapters/tests/fixtures/codex/c7_bad_model.replay.json#L151) | The recorded `thread/status/changed` state was replaced with `idle`, and its order relative to the final `error` changed. Evidence: private `out/c7/20260930T165806.026766Z/raw.jsonl`, fields `params.status.type`, `method`. | Preserve the recorded state and notification order. |
   | [c8_auth.replay.json:161](../../../../../crates/via-adapters/tests/fixtures/codex/c8_auth.replay.json#L161) | The same state replacement and final-error ordering change occur here. Evidence: private `out/c8/20260930T165831.626418Z/raw.jsonl`, same fields. | Preserve both while sanitizing error details. |
   | [c1_commentary_usage.replay.json:274](../../../../../crates/via-adapters/tests/fixtures/codex/c1_commentary_usage.replay.json#L274) | An `item/agentMessage/delta` was invented for the async-question item; its recording contains no delta for that item. Evidence: private `out/c1/20260930T165007.108896Z/raw.jsonl`, fields `method`, `params.itemId`. | Remove that synthetic delta. |

7. **Minor — fixture provenance omits borrowed catalog responses.**  
   All 14 launched fixtures use `cat`’s catalog, but only c6 and c7_effort name `cat` in `source`. The missing instances are line 2 of **both `.replay.json` and `.expect.json`** for:

   `c0_server_lost`, `c11_failed_command`, `c1_commentary_usage`, `c2_steer`, `c3_interrupt_uncertain`, `c3_wall_interrupt`, `c4_two_sessions`, `c5_resume`, `c5_resume_missing`, `c7_bad_model`, `c8_auth`, `c9_output_schema`.

   c7_effort additionally transfers a successful exchange and usage from c5 while describing only its text as borrowed.

   **Smallest fix:** name the spliced sources and describe the adapted exchange completely.

8. **Minor — “same fidelity checks” overstates the local driver.**  
   The hand-back’s local `fidelity.py:44–45` drains and discards stdout. It checks loading, version output and successful exit, but does not compare emitted lines as the shared Rust driver does at fixtures.rs:253 (`crates/via-fake-agent/tests/fixtures.rs:253`).

   **Smallest fix:** report this as a load/exit smoke check, or run the actual shared driver once the slices are integrated.

**Verdicts on the 11 interface observations**

| # | Verdict |
|---|---|
| **1. Two sessions** | **Expectation-driver gap, not C2 gap.** C2 §3 already specifies shared connection admission; §4 explicitly promises ordering only within a session. Cross-session scheduling and launch counting belong in the test driver. Finding 4 remains. |
| **2. Two servers** | **Replay selection limitation; the isolated reopen fixture is sound.** Replay selects one script by executable name; C2 §2 Reopen allows the exact stored ID on a new connection. This fixture does not prove the preceding retirement. |
| **3. EOF wait** | **Real replay-format gap.** `Step` has no EOF operation and the fake exits when steps end ([replay.rs:89](../../../../../crates/via-fake-agent/src/replay.rs#L89)). A 500 ms delay cannot establish an EOF-controlled lifetime. This agrees with Claude observation 1. |
| **4. Error text** | **Vendor mapping issue, largely already settled.** Vendor §3 expressly permits operation/expected-ID/known-message-shape mapping and forbids mapping every `-32600` to mismatch. `SessionGone` fits a missing rollout; local-versus-forwarded stale checking is an implementation choice, not missing vocabulary. The fixture tests the forwarded vendor-error path. |
| **5. Effort-check position** | **Fixture overconstraint, not C2 gap.** AD18 specifies the submission boundary sufficiently; it leaves thread-creation order open. Finding 3. |
| **6. Final text** | **Misreading of the supposed ambiguity.** Vendor §5 explicitly says each completed `final_answer` item becomes ordered final-text pieces ([codex.md:306](../../../../../docs/specs/vendors/codex.md#L306)). A terminal summary omitting an earlier item does not override that rule. Both expected pieces are correct. |
| **7. Post-wall terminal** | **Misreading.** C2 §4.1 retains terminals decoded before return within the cleanup cutoff. Finding 2. |
| **8. Absences/environment** | **Real replay coverage gaps, not C2 gaps.** Subset matching permits extra fields; replay does not inspect environment. Vendor §§1, 3 and 4 already prescribe these restrictions. The driver can inspect actual outbound requests and launch environment without changing C2. |
| **9. Instruction inventory** | **Real vendor-packet evidence gap.** `ThreadStartResponse.instructionSources` identifies loaded instruction files; the recording contains it. The packet’s blanket inventory-unavailable statement at [codex.md:259](../../../../../docs/specs/vendors/codex.md#L259) should be narrowed. C2’s inventory vocabulary already accommodates it. |
| **10. Pagination** | **Real implementation requirement, not C2 gap.** The local schemas define `ModelListResponse.nextCursor` and `ModelListParams.cursor`. Null-only fixtures do not qualify pagination; this is follow-up coverage. |
| **11. Version parse** | **Sound qualification rule for these recordings.** It follows vendor §1’s handshake source. All fixture CLI and handshake versions agree, so these cases cannot distinguish correct parsing from substitution of the CLI/cached version. |

The full-access echoes are schema-valid and declared synthetic; they qualify normalization, not real bound enforcement. Borrowing the catalog for c8 gives a useful accepted-turn auth-error test, while unauthenticated catalog discovery remains unverified. The c0 adaptation is sound for **Host-confirmed server death with no terminal**; it does not prove real crash cleanup under full access.

**Checks and could not verify**

- Focused workspace gate: **14 passed**.
- Default Codex conformance: **1 passed, 16 skipped**.
- Ignored run: **16 expected failures**, all naming `via-5lr.3.2`.
- Local schema validation: **350 messages passed**.
- Usage sums matched every asserted replay sample.
- Claude’s current capture-aware driver already supplies omitted `/id` values; compatibility passes static inspection.
- The integrated fidelity test was not executed: it is absent from this worktree. The historical full lint/release gate was not independently rerun.
- No files were edited; Git remained clean.

The helper correctly targets Codex bead `via-5lr.3.2`; `via-p98.3.2` owns Claude. It is replaceable, but findings 4–5 must be addressed before replacing only `drive()` can establish the advertised coverage.