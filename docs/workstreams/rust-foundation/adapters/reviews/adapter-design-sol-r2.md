**UNSOUND.** Revision 2 substantially improves the design, but several fixes remain incomplete and new text introduces contradictions.

Reviewed the 1185-line document at `3a6cc3e`. I recovered revision 1 from the round-1 log to identify changed text. No files, Git state, or Beads were changed; no vendor CLI, model, or build/test command was run. Final Git status matches the initial status.

**Findings 1–32**

All links in this table refer to revision 2 of `design.md`.

| # | Disposition | Reason and location |
|---|---|---|
| 1 | fixed | Admission now counts live connections; pinned reuse avoids acquiring another slot, with four-server/fifth-server regressions specified. [L682](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L682) |
| 2 | fixed | `TurnEnd` retains the decoded terminal independently of observation delivery and preserves S1 disposition rules. [L383](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L383) |
| 3 | fixed | Codex’s post-acknowledgement P7 window is explicitly independent of `force_at`/`close_by`; the 20-second completion regression is included. [L405](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L405) |
| 4 | fixed | `SessionCx` attaches observations during logical open/recovery; missing session observations and attribution are restored. [L341](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L341) |
| 5 | fixed | Canonical `recover` and its `Resumed`/`Unknown`/`Dead` outcomes remain, with Host evidence and AD9 cleanup rules. [L368](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L368) |
| 6 | fixed | Tombstones, late durable commits, and late-terminal revision of `unknown` are restored. [L416](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L416) |
| 7 | partly fixed | Route-specific return conditions and optional terminals are present, but the no-terminal server rule conflates confirmed death with uncertain loss. [L397](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L397) |
| 8 | partly fixed | Group/tool/recovery distinctions are added, but AD9 contradicts its fake regression and leaves open tools `Pending` even at settlement. [L572](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L572) |
| 9 | partly fixed | Failure shapes and repeated reconciliation are specified, but OpenCode reconciliation still uses `close_by` after acknowledgement and requires tool completion before identifying the terminal. [L784](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L784) |
| 10 | fixed | Declines correlate with tool IDs; matching denials are suppressed, including c11b’s zero-denial expectation. [L543](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L543) |
| 11 | fixed | Claude uses authoritative `result.usage`; partial assistant snapshots are discarded. [L754](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L754) |
| 12 | fixed | A separate turn-wide ledger now specifies replacement, bounds, explicitly degraded overflow scope, null components, and boundary/overflow tests. [L457](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L457) |
| 13 | fixed | Terminal cost/vendor fields and identity transcript data have explicit envelope destinations. [L477](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L477) |
| 14 | fixed | All named override/inheritance sites and omitted/null/given semantics are inventoried, including queued inheritance tests. [L892](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L892) |
| 15 | fixed | Spawn’s harness becomes optional; unique, ambiguous, and absent catalog resolution cases are specified. [L937](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L937) |
| 16 | fixed | Core steer now includes capability checking, active-turn selection, driver delivery, error mapping, and durable reporting. [L913](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L913) |
| 17 | partly fixed | Envelope placeholders and failed stop reasons are covered; Core schema validation is specified, but the requested malformed/missing-output regressions are not explicit in S-CORE. [L924](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L924), [L1119](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1119) |
| 18 | fixed | Named literals, `FakeTurnRecovery`, and erroneous method/test citations are accounted for. [L888](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L888) |
| 19 | fixed | The startup probe is removed; remaining persistent-server ownership work is explicitly assigned across Store, Host, and Wire through AR6. [L741](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L741) |
| 20 | fixed | Bundled/discovered catalogs, acquisition points, cache lifetime, and pre-discovery behavior are now explicit; planning launches nothing. [L960](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L960) |
| 21 | fixed | The fake owns its null-version/untested/warning policy and remains available without a Core exception. [L1023](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1023) |
| 22 | fixed | Handshake limitations and the owner’s accepted unchecked-version risk are explicit; Codex limited-bound qualification is acknowledged. [L515](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L515) |
| 23 | partly fixed | Instance versions, persistent-server provenance, capability ownership, and compatible advancement are specified, but failure results cannot carry the observed version. [L290](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L290), [L502](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L502) |
| 24 | fixed | The category table distinguishes verified, unverified, unavailable, and coupled switches instead of claiming complete enforcement/inventory. [L989](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L989) |
| 25 | fixed | The subreaper limitation is narrowed, deployment assumptions are labelled, and the already-observed survival case is acknowledged. [L108](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L108) |
| 26 | fixed | Vendor replay now owns fixture selection, argv/input sequencing, ID capture, bounds, and probe responses while preserving the fake protocol. [L1149](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1149) |
| 27 | fixed | Joined-token matching addresses vendor names; lexer-based test exclusions and the required self-test cases replace unsafe path/column exclusions. [L1029](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1029) |
| 28 | fixed | Missing observation/attribution rows and all specifically named overstated cells are corrected. [L204](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L204) |
| 29 | fixed | The omitted feature changes, git-root resolution, inferred instruction absence, and format addendum receive explicit dispositions. [L776](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L776), [L844](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L844) |
| 30 | partly fixed | The audit covers many missed occurrences, but omits OpenCode’s version gate and 1-second decline rule, plus C1’s recorded P13 row. [L799](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L799) |
| 31 | partly fixed | Dependency order and characterization migrations are corrected; OD3’s implementation still proceeds before its owner gate. [L1096](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1096) |
| 32 | fixed | Existing upstream reports are tracked; the sharper pagination failure and text-format failure are incorporated, and plain turns omit `format`. [L785](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L785) |

**New defects introduced by the revision**

1. **important — OpenCode reconciliation reinstates the old cancellation deadline.**  
   **Location:** [VO1, L784](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L784), alongside AD4 L405–414 and AD15 L674–676.  
   **Defect:** VO1 bounds reconciliation by `close_by` and requires no pending/running tool before determining the terminal. AD15 acknowledges abort before reconciliation, while AD4 requires the post-acknowledgement P7 window. An acknowledged abort whose final tool update arrives at 20 seconds can therefore become `unknown` around 13 seconds. Terminal recognition is also coupled to the cleanup evidence that AD4 expects to await *after* recognition.  
   **Evidence:** [C1 L257](../../../../../docs/specs/via-api-v1.md#L257) separates acknowledgement from cleanup; [OpenCode re-probe L120](../../../../../docs/workstreams/rust-foundation/adapters/reprobe-opencode.md#L120) confirms idle precedes final updates.  
   **Smallest fix:** Retain abort acknowledgement and its timestamp separately from cleanup reconciliation. After acknowledgement, use `min(ack + tool_grace, wall)`, and report `uncertain` at that bound when tools remain unresolved.

2. **important — AD9’s cleanup state rule conflicts with settlement and its own test.**  
   **Location:** [L572](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L572), L580, L589.  
   **Defect:** “Otherwise … `Pending` while an item is open” has no deadline exception, although AD4 returns at the cleanup deadline. Separately, the fake declares containment, so group absence satisfies L573–574 even with an open reported item; L589 expects `uncertain` for that same case.  
   **Evidence:** [C1 L259](../../../../../docs/specs/via-api-v1.md#L259) requires terminal settlement to `quiescent` or `uncertain`, never `pending`.  
   **Smallest fix:** Limit `Pending` to an ongoing wait before its deadline; unresolved cleanup becomes `Uncertain` at settlement. Change the fake test’s expectation to `quiescent`, or use an explicitly non-contained profile for the uncertain case.

3. **important — Failure paths lose actual vendor-version provenance.**  
   **Location:** [surface, L290](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L290), [AD7, L502](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L502).  
   **Defect:** `vendor_version` exists only inside the successful `TurnEvidence` arm. A known instance that later fails protocol, overflows, loses transport, or is force-stopped returns an error without that field. Falling back to the session plan can report a different cached instance version.  
   **Evidence:** The retained [RouteFailure, L284](../../../../../crates/via-routes/src/lib.rs#L284) has no version field; AD7 promises the actual instance version per turn, implementing OD1.  
   **Smallest fix:** Carry instance version/status outside the fallible outcome, or persist an attributed instance observation before submission. Test an observed version followed by a typed failure.

4. **important — The no-terminal server rule contradicts retained failure classification.**  
   **Location:** [AD4, L408](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L408).  
   **Defect:** The rule assigns `unknown` when the connection fails without a terminal, although the preceding result definition retains typed server-loss and other failures. Host-confirmed server death and ambiguous transport loss require different dispositions.  
   **Evidence:** [C1 L734](../../../../../docs/specs/via-api-v1.md#L734) requires `failed(server_lost)` for confirmed death and `unknown` for unconfirmed transport loss.  
   **Smallest fix:** Specify those cases separately. Reserve the no-acknowledgement `unknown` rule for the control deadline and uncertain transport/submission cases.

5. **important — Unsupported inheritance-on settings are silently ineffective.**  
   **Location:** [AD13, L650](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L650), [category table, L995](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L995).  
   **Defect:** The warning rule covers only an **off** setting with an unavailable switch. OpenCode hooks, MCP servers, plugins, and agents are declared “not switchable to on.” Selecting `true` therefore cannot implement OD2’s requested behavior and has no specified warning/refusal. The proposed default already selects `true` for plugins and agents.  
   **Evidence:** The contradiction is explicit between AD13 and §5.4.1.  
   **Smallest fix:** Warn or refuse whenever the requested state cannot be applied, in either direction, and record the effective state.

6. **important — A failed check can poison every later plan for an unchanged binary.**  
   **Location:** [AD7, L506](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L506), especially L511–512.  
   **Defect:** Any qualifying startup/handshake failure causes later plans to refuse until executable identity changes. A transient startup failure or a configuration/turn-specific policy-readback failure does not establish permanent binary incompatibility. The refusal itself prevents a later handshake from checking whether the breakage remains.  
   **Evidence:** AD7 checks policy, sandbox, tool surface, and permission readback—facts that are not determined solely by executable inode/mtime. OD1 permits refusal on actual breakage.  
   **Smallest fix:** Cache only demonstrated stable incompatibility under all relevant recipe/configuration inputs. Keep transient failures out of that cache and provide bounded revalidation.

7. **important — Undiscovered Codex effort bypasses C1 parameter validation.**  
   **Location:** [catalog lifecycle, L965](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L965).  
   **Defect:** Before discovery, explicit effort passes to the vendor and a 400 becomes `vendor_error`. This includes values already invalid under the supported wire effort enum and can turn a parameter error into a receipted vendor failure.  
   **Evidence:** [C1 L462](../../../../../docs/specs/via-api-v1.md#L462) requires unknown effort values to be refused; AD2 also assigns effort validation to `plan`/`check_turn`.  
   **Smallest fix:** Validate known wire-level effort values during pure planning. Apply discovered model-specific constraints before `turn/start`, with a definite parameter rejection.

8. **important — OD3’s owner gate excludes the implementation it governs.**  
   **Location:** [L1107](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1107), especially L1109–1111.  
   **Defect:** Final spec changes wait for OD3, but code implements the pending recommendation immediately because it “only reports less.” That still changes approved cleanup outcomes and successor behavior.  
   **Evidence:** OD3 is expressly pending; [C1 L716](../../../../../docs/specs/via-api-v1.md#L716) currently permits positive group absence to establish quiescence. Round-1 finding 31 explicitly required an implementation gate for owner-dependent behavior.  
   **Smallest fix:** Gate the OD3-dependent behavior as well as its final specification, while allowing independent interface and evidence plumbing.

The two requested positions have these dispositions:

- **AD17’s 5-second choice is sound:** [C2 A6](../../../../../docs/specs/adapter-contract.md#L60) records the owner-approved value, and revision 2 explicitly flags the Codex amendment. However, the amendment audit must also disposition [OpenCode’s 1-second rule](../../../../../docs/specs/vendors/opencode.md#L546).
- **Compatible adapter-version advancement is a coherent, explicitly flagged proposal:** it implements compatibility refusal and records per-turn execution provenance. Its conflict with [invariant 2](../../../../../.repo-context/invariants.md#L16) is disclosed, so it is not an unflagged conflict. The required owner edit remains a prerequisite; §3.1’s “rules 1–7 unchanged” should explicitly except rule 2.
- **The proposed validator use is consistent with C1:** Core validates the frozen draft-2020-12 schema and owns `structured_output_invalid`. Missing-output handling should remain distinct from that failure class. The missing regression coverage is finding 17 above.

**Out of scope, noticed**

[repo-map.md](../../../../../.repo-context/repo-map.md#L3) still describes a partial S1 checkpoint with a compilation failure, while the later [session handoff](../../../../../docs/workstreams/rust-foundation/session-handoff.md#L208) records subsequent critique closure. This predates revision 2 and does not affect the verdict.

**Could not verify**

- Implementation, build, test, and live qualification results: none were run.
- Unqualified configuration switches, effective effort, Codex live decline acceptance, and bound enforcement.
- Current upstream issue/PR status; OD4 was checked against the supplied local addendum and raw format-read evidence.
- Live Beads state; dependency order was checked against the existing JSONL export only.
- The JSON Schema library’s implementation or suitability: no dependency is selected, and that assessment is outside this round’s scope.