UNSOUND.

Reviewed `125c42f..3cedfea`, all seven changed files, revision 9 of the approved design, and the author’s hand-back. The main amendments are present, but stale normative text and several incomplete shapes prevent a SOUND verdict. The deferred detection passages remain unchanged; invariant 2 matches AD12 exactly.

File abbreviations below:

| Label | File |
|---|---|
| D | [approved adapter design](../../../../../docs/workstreams/rust-foundation/adapters/design.md) |
| C1 | [via-api-v1.md](../../../../../docs/specs/via-api-v1.md) |
| C2 | [adapter-contract.md](../../../../../docs/specs/adapter-contract.md) |
| RT | [runtime-contracts.md](../../../../../docs/specs/runtime-contracts.md) |
| CC | [vendors/claude-code.md](../../../../../docs/specs/vendors/claude-code.md) |
| CX | [vendors/codex.md](../../../../../docs/specs/vendors/codex.md) |
| OC | [vendors/opencode.md](../../../../../docs/specs/vendors/opencode.md) |

**Findings**

1. **Important — C1:20–21, 233–234: the public session contract still freezes the adapter version.**  
   AD12 says “the session’s persisted `adapter_version` advances to the running adapter’s version” on compatible resume (D:798–801). C1 still defines a session as having “one route and adapter version for life.” Its resume error list also omits the newly required compatibility refusal, `harness_unavailable`.  
   **Smallest fix:** retain the lifetime route constraint, describe compatible adapter-version advancement, and add the compatibility refusal to `resume`.

2. **Important — CX:553; minor residual wording at CX:187: the old version policy survives.**  
   AD7 says “Every vendor version is supported by default” (D:605); VX1 makes the schema hash a check record rather than a runtime gate. CX §9 still directs “Pin 0.157.1.” CX:187 also calls `allow_untested` a “version override,” although it now has no effect.  
   **Smallest fix:** supersede the pin directive with AD7 and replace “version override” with “compatibility parameter with no effect.” Historical references to inspected binaries or fixture schemas need not be removed.

3. **Important — C1:303–360: `status` does not expose the newly required version and configuration facts.**  
   AD7 requires the unchecked-version warning in “every receipt, status and envelope” (D:645–648). AD12’s acceptance text requires status to show the advanced adapter version (D:822). AD13 requires effective category states to be “visible in status” (D:843–846). C1’s status shape and prose define none of those fields.  
   **Smallest fix:** define status’s current adapter version, vendor-version status/warning, and frozen inherited-configuration effective states.

4. **Important — C2:190, 569–581: pure effort validation lacks an input in the planning request.**  
   AD18 requires “`plan` and `check_turn` validate effort purely” before a receipt (D:926–937). C2 repeats that rule, but its `DescribeRequest`, the input to `plan`, has no `effort` field.  
   **Smallest fix:** give the internal planning request the effort value needed for spawn validation, or explicitly define the separate pure validation entrypoint. This need not expand public `describe` parameters.

5. **Important — CX:70–72 versus 150–157: the permitted typed-route surface excludes required catalog discovery.**  
   VX11 requires catalog discovery through `model/list` (D:1009); design §5.3 specifies that the driver sends it immediately after `initialize` (D:1214). CX says to extend the route with “only” its listed methods, which omit `model/list`, then requires that method later.  
   **Smallest fix:** add typed `model_list` to the enumerated route surface.

6. **Important — RT:839, 1059–1062, 1128–1132: the configuration whitelist contradicts OD2.**  
   AD13 freezes settings from `AdapterConfig`; design §5.4 expressly defines optional `daemon.json` `harnesses.<name>.binary` and `harnesses.<name>.inherit.*` settings (D:1229–1231). C2:649–651 adopts the inheritance keys. RT still says **only** disk/WAL keys are configurable, and its `daemon.json` description lists only those settings.  
   **Smallest fix:** include the adapter-owned `harnesses` configuration and startup/freeze rules in these three locations while retaining fixed resource limits.

7. **Important — C1:627: the durable warning shape cannot carry `data.categories`.**  
   AC7/AD13 require `config_switch_unverified` with `data.categories: [{category, requested, effective}]` (D:842–844, 960). C1 §5 includes that requirement at line 601, but §6 still defines a warning event as only `code`, `message`.  
   **Smallest fix:** add optional structured warning `data` to the event shape and explicitly preserve the configuration warning’s categories.

8. **Important — OC:493; CC:505: vendor I/O during `open_session` remains permitted.**  
   AD3 says “`open_session` is logical: it performs no vendor I/O”; creation/reopening occurs in the first `run_turn` (D:381–390). OC’s `open_session / spawn` row still subscribes SSE and performs `POST /session`. CC’s mirrored amendment still says other routes may “confirm during open” and immediately return confirmed identity.  
   **Smallest fix:** distinguish logical driver creation from first-`run_turn` vendor opening in OC, and remove CC’s obsolete exception.

9. **Important — OC:119–123, 381, 498, 678–684, 799; C2:635: OpenCode C1 close still has incompatible detach and server-stop definitions.**  
   AD19’s OpenCode close recipe is “`POST /instance/dispose` … then S1’s hard stop” (D:714); VO18 and AD20 identify the server-stopping C1 close as a report destination (D:1034, 726). The retained passages describe logical detach and restrict server shutdown to idle/daemon lifecycle. OC:678–684 specifically retains the prohibition on asking Host to kill the dedicated server for force-close, immediately before the new dispose/hard-stop recipe.  
   **Smallest fix:** state one C1 close lifecycle: cancel active work, dispose the owned server, perform the approved bounded fallback, preserve history, and commit the close result. Keep idle retirement distinct and keep `leftovers` null pending detection.

10. **Important — obsolete raw-logging requirements remain across the packets and runtime.**  
    AD14 replaces raw logging with “still read and counted; normal observations stop” (D:861–864), which C2:393–394 now says. RT:344 expressly says VIA keeps no copy of vendor traffic. Conflicting instances remain:
    - **CX:358–364:** “raw-logging,” `raw_log_incomplete`, and “Raw evidence is otherwise retained exactly.”
    - **CX:477:** the acceptance fixture still requires “raw-only” flood handling and distinguishes actual raw gaps.
    - **OC:78, 149–150, 511:** production raw capture, retained vendor body bytes in a raw log, and outbound raw evidence.
    - **OC:642–655:** quarantined data stays “raw-only”; preserve raw evidence and fail when “exact log capture” cannot keep up.
    - **RT:1080:** retain body/message-boundary evidence through transport logging/capture, without reconciling this with §4’s no-copy rule.  
    **Smallest fix:** align production traffic handling and its acceptance assertions with read/count/discard plus the existing bounded decode-failure evidence. Retain credential-handling prohibitions and test-supervisor evidence requirements.

11. **Important — C2:772–773; CX:419–421: cleanup is narrowed beyond AD9.**  
    AD9 includes “the vendor’s reported tool items on a server route” and says a reported item counts “wherever its process runs” (D:683–700). C2 conformance item 11 says cleanup covers “only the agent’s own group.” CX categorically excludes background terminals from cleanup. Those statements can exclude reported, still-open items that AD9 requires tracking.  
    **Smallest fix:** scope the group-only statement to process-group cleanup and exclude background work only when it is outside the reported-item set. RT:39–41 should likewise distinguish group-scoped quiescence from an all-descendants guarantee.

12. **Important — cancellation outcome and deadline summaries lose AD4’s conditions.**  
    AD4 preserves private routes’ `requested` outcome, changes the specific unacknowledged shared-server force branch to `unknown`, and permits an acknowledged wall cleanup to report `acknowledged` (D:480–490, 521–531). Conflicting or incomplete instances:
    - **C2:468–470:** any “launched, unforced stop” on a shared connection is described as `unknown`, omitting the branch conditions.
    - **C2:766–767; C1:275:** no acknowledgement is universally described as outcome `unknown`, rather than distinguishing private and shared force paths.
    - **C1:262:** `uncertain` is defined as “acknowledged but not provable,” excluding AD4’s unacknowledged `requested/uncertain` wall result.
    - **OC:765:** OC08 describes cleanup bounded by `ack + 60 s` without the required wall cap.  
    **Smallest fix:** retain the exact route/order conditions and use `min(ack + tool_grace, wall)` consistently.

13. **Important — OC:809–811: the mirrored usage amendment remains superseded.**  
    VO6 says keyed assistant samples sum with scope `turn` (D:1026). OC §7 now says that, but §9 still says “OpenCode usage remains vendor_interval until measured.”  
    **Smallest fix:** supersede that sentence with VO6’s token accounting rule and child-session exclusion; retain the separate uncertainty about billing scope.

14. **Important — CC:271–273, 457; CX:160–162: structured-output failure rules disagree with the approved design.**  
    Design §5.1 row 37 distinguishes present-but-invalid output → `structured_output_invalid` from completed-with-missing-output → retained status plus `structured_output_missing` (D:1173). CC’s prose and test still fail missing output. CX’s “otherwise use `structured_output_missing`” also covers text that failed parsing/validation, conflating invalid with missing.  
    **Smallest fix:** state and test the two cases separately in all three locations.

15. **Important — CX:380–382; RT:1133–1135: resource qualification still assumes four active turns.**  
    AD16 says the four slots are held per live connection, “not … per turn” (D:887–898). CX:92–100 correctly permits multiple leased sessions on one connection. Nevertheless, both resource-measurement passages still qualify only “four active turns,” leaving the newly admitted shared-server concurrency outside the stated qualification load.  
    **Smallest fix:** measure the maximum admitted concurrent session/turn load under per-connection admission and update the buffer accounting accordingly.

16. **Minor — C1:295–298, 563–582: nullable `leftovers` is presented inconsistently.**  
    AC10 adds the nullable field to close results and envelopes (D:962); C1’s field convention requires nullable fields to be present (C1:120–121). The envelope example omits `leftovers`, and close prose says it is “present only” when the close stopped the server, although it then says otherwise `null`.  
    **Smallest fix:** add `"leftovers": null` to the example and say **non-null only** for a qualifying report. The pending decision still makes every current value null.

17. **Minor — C2:500; C1:602: AD20’s nondeferred timestamp semantics are incomplete.**  
    AD20’s **Shape** row specifies ordering by “start ticks, then pid,” and defines `started_at` from boot time plus start ticks, accurate to about one second and emitted with second precision (D:733). C2 substitutes “start time,” and both specs retain only the output format/precision, dropping derivation and accuracy.  
    **Smallest fix:** restore the agreed Shape-row semantics. This does not select or implement the deferred detection method.

18. **Minor — CC:486: wrong C1 section reference.**  
    AD3 concerns identity/status and session events. The mirror labels `status` as “C1 §3.8”; status is §3.7, while §3.8 is `wait`.  
    **Smallest fix:** change the citation to §3.7. The explicit C2 §4.1/§4.2 references inspected correctly distinguish turn end from leftover reporting.

**§3.7 row audit**

“Applied” below means the intended replacement block exists; the findings above identify remaining contradictions or incomplete integration.

| §3.7 row, quoted target | Spec evidence and disposition |
|---|---|
| C2: “§1 rules 1 and 2 … §2 enum, sketch, types, contract points … §4 table … §5 … §6.2 … §7 … `TurnEnd`/driver `CloseReport` fields” | C2:78 “Core names no harness”; :145 includes `Fake`; :210 uses `run_turn`; :403 retains one `TurnEnd`; :516 supports every version; :593 defines the ledger; :657 defines category warnings. **Applied with partial/distorted integration:** AD2 planning input, AD4 outcome summaries, AD9 cleanup scope and AD20 Shape; findings 4, 7, 11, 12, 17. AD14’s requested C2 deletions are applied. |
| C1: “Summary P13 row … §3.1 … §3.2 … §3.5 … §3.6 … §5 … §6 … §4 … §7.2 … §7.5 … §8.1 … §8.2” | C1:76/865 contain OD1; :218 says `allow_untested` has no effect; :264 bounds `pending`; :587 adds `failure.data`; :601 adds category warnings; :675 expands adapter rejection; :786 adds compatibility refusal. **AC1–AC9 replacement text applied; AC10 partially integrated.** Residual defects: findings 1, 3, 7, 12, 16, 17. P7 unchanged as directed. |
| RT: “§2 … §3 and §4 … §5 `ProcessOwner` … §5 Host … §5.2 … §6 Store … §6.2 … §7 … §8 … §6.1 … §11.1” | RT:56 assigns adapter DTOs; :147/:311 carry the report; :412 defines the non-turn-owner extension; :677 preserves close reports atomically; :924/:1052 exclude shutdown/recovery reports; :1067 makes admission per connection; :820 forwards bootstrap names; :1203 assigns fake config to `AdapterConfig`. **AR1, AR3–AR6 and AD20 delivery applied**, with residual findings 6, 10, 15. AR2 portions deferred. |
| CC: “§1 … §2 describe … §3 … §5 … §9 tests … §7 cancel and close … §10 items 1–3” | CC:18 removes the exact set; :57 removes HostProbe; :132 adopts AD7; :259/:296 use the aggregate; :346 orders interrupt then EOF; :452/:460/:464 amend the named tests; :479–485 mark items 1–3 superseded. **VC1–VC12 requested replacements applied**, with residual findings 8, 14, 18. |
| CX: “§1 pin and ‘no experimental capabilities’ … §3 effort … §4 deadline … §5 final text … §6 … §7 usage … §8 tests … §9 P7/C2 item 6” | CX:25 makes SHA a record; :30 adopts K17; :156 checks discovered effort; :218 uses 5 s; :302 restricts final text; :399 uses the driver window; :424 defines server close; :446 sums `last`; :468–479 amend fixtures; :530/:562 supersede old P7/terminal text. **VX1–VX17 replacement blocks applied**, but catalog integration and residual rules remain defective: findings 2, 5, 10–12, 14, 15. |
| OC: “§3 version-gate … output-schema … effort and steer … §4 terminal … §6 … §5 decline … §7 … §8 OC01/05/07/08/11” | OC:362 adopts AD7; :379 refuses steer; :383 refuses schema/omits format; :384 validates discovered variants; :540–570 separates acknowledgement, terminal and cleanup; :620 uses 5 s; :710 sums keyed calls; :730 maps 401/403 to auth; :758–768 amend the named cases. **VO1–VO18 replacement blocks applied**, with incomplete close integration and residual findings 8–10, 12, 13. VO8/VO9/VO12/VO13 additions are present. |
| Invariants rule 2: “‘The adapter version also stays fixed’” | `.repo-context/invariants.md:16–23` exactly matches AD12’s owner-approved replacement. **Faithful and complete.** |
| Invariants rule 1: “never reads … vendor credentials” | Original rule retained exactly. **Deferred, untouched.** |
| Coding-style: “lines 164–165 … 173–174 … 175–176” | File unchanged in the reviewed diff. **Deferred, untouched.** |
| Platform packet: “§5 … P-I3” | File unchanged in the reviewed diff. **Deferred, untouched.** |
| C1: “§7.5 lines 710–711 … §9 lines 814–823” | Baseline passages retained exactly. **Deferred, untouched.** |
| CX: “line 205” | Credential-content prohibition retained exactly. **Deferred, untouched.** |
| OC: “lines 223–224, OC12 … mirrored C1 amendment” | All listed baseline passages retained exactly. **Deferred, untouched.** |

AR2’s listed runtime passages remain exact baseline text; no scan bounds were added. AD20’s option-A rows and the marker-qualification clauses were not applied. The pending-detection rule is present in C2, C1 and runtime. No applied provision authorizes detection while that rule remains pending.

**Author’s nine concerns**

| Concern | Verdict |
|---|---|
| 1. Remaining “pinned” wording | **Partly valid concern.** CX’s imperative “Pin 0.157.1” is defective. Historical binary/schema evidence and qualification references are not themselves contrary to OD1. OC’s stale `vendor_interval` proposal is a separate actual defect. |
| 2. Extra C2 §6.2 describe/open edits | **Sound.** Necessary to reconcile AD3/AD7 with the mapping table. |
| 3. Constructor chain | **Sound.** Matches design §3.2 and preserves Wire-only resource splitting. |
| 4. CX §2 slots | **Sound correction**, but the four-active-turn measurement remnants still need correction. |
| 5. CX raw logging left stale | **Valid defect.** It requires changes; related instances also remain in OC/runtime. |
| 6. `turn.late_terminal` naming | **Sound.** It is an internal observation-table name, preserving `LateTerminal` semantics without creating a public C1 event. |
| 7. Runtime §8 citation | **Sound.** C2 §3 now contains AD16’s admission rules. |
| 8. “Collected or logged” | **Sound.** Preserves the no-destination limitation without choosing detection. |
| 9. C1 §8.1 citation | **Sound.** C2 §5 contains AD7; the section citation faithfully substitutes for the amendment ID. |

**Out of scope, noticed**

- Repository-wide link checking reproduces six broken scratchpad links in design reviews r3/r5. None is in the seven changed files.
- CX:382 retains a 256 MiB RSS target, whereas runtime:1121–1124 describes roughly 332 MiB plus a 25% margin. This target mismatch predates the chunk; finding 15 addresses the newly exposed concurrency assumption.
- The hand-back reports a preparatory fast-forward despite its original “never merge” instruction. The final diff cannot establish the historical command or its authorization.

**Could not verify**

Vendor behavior, private probe artifacts, live qualification and implementation correctness were not exercised. I did not run Beads, vendor CLIs, models, builds or runtime tests.

Read-only checks passed for changed-file relative links, diff whitespace and public-repository hygiene; the skill catalog reported no FAIL. No added secrets, personal paths or machine-local absolute paths were found. Git status remained clean.