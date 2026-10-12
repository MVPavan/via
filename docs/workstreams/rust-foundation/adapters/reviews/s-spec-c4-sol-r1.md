**UNSOUND.** Reviewed all four commits in `ef66700..096f817`. Two substantive contradictions remain, plus stale marker wording.

**Findings**

1. **Medium — [docs/specs/vendors/opencode.md:280](../../../../../docs/specs/vendors/opencode.md#L280): unconditional credential-read prohibition remains.**  
   “VIA never reads, copies, hashes or extracts any user/provider credential” still forbids the transient environment read permitted by option A. The scanner can encounter credentials in unrelated eligible same-uid processes; a no-login vendor environment does not remove that possibility. Evidence: design conflict 4 explicitly recognizes this exposure; runtime §5 permits only transient exact-marker matching.  
   **Smallest fix:** qualify the **read** prohibition with the C2 §4.2/runtime §5 exception. Preserve the prohibitions on credential extraction, copying, hashing and auth-file access.

2. **Medium — [docs/specs/vendors/opencode.md:691](../../../../../docs/specs/vendors/opencode.md#L691): detection is still described as pending.**  
   The close lifecycle says its report is “`null` while detection is pending.” This contradicts the owner’s decision and C2 §4.2’s server-stopping-close destination. This is the remaining pending-detection instance in the requested specification/guidance scope.  
   **Smallest fix:** replace that qualification with a reference to the best-effort report under C2 §4.2/runtime §5.

3. **Low — stale exclusive marker descriptions at both instances:**  
   - [docs/specs/vendors/opencode.md:274](../../../../../docs/specs/vendors/opencode.md#L274): “Launch-only marker”.
   - [.repo-context/coding-style.md:175](../../../../../.repo-context/coding-style.md#L175): “The marker is launch data only”.

   These descriptions omit its now-approved report-matching use. The surrounding exceptions clarify the intended behavior, so this is wording inconsistency rather than additional signalling authority.  
   **Smallest fix:** describe it as launch data that may also be matched solely by the report-only leftover scan.

**Verified**

Runtime §5 faithfully carries AD20’s substance: the unreaped-child start bound in `Spawned`, memory-only retention, one descriptor per process instance, real-uid/start-tick/state rechecks, 256 KiB plus lookahead byte and EOF requirement, all `incomplete` conditions and disappearance exceptions, deadline/task bound, detection limits, and privacy restrictions.

C2 §4.2 and C1 §5 agree with those rules and destinations. The added permissions stay within AR2. Platform marker proof and persisted-marker/liveness prohibitions remain unchanged; C1 §7.5’s signalling restrictions and runtime §5.1’s environment-free anchor verification remain intact. The durable vendor-facts commit and Store `anchors` schema remain unchanged.

Invariant rule 1 preserves the approved exception’s substantive wording and adds no broader permission. The coordinator’s citation correction is accurate.

**Author’s concerns 1–5**

| Concern | Verdict |
|---|---|
| 1. Invariant punctuation/sentence structure | Acceptable editorial change; no substantive change to the approved boundary. |
| 2. “C2 AD20” citation | Resolved by `096f817`: C2 §4.2 is correct; AD20 belongs to the design. |
| 3. Additional consistency edits | Appropriate and within scope. The consistency sweep remains incomplete because of findings 1–3. |
| 4. Sixteen-process bound in §8 | Keep it. It accurately summarizes AD20’s existing shape limit without changing total-count semantics. |
| 5. Tests (6)–(13) not duplicated into specs | Acceptable. They remain explicit in AD20 and required by S-LEFTOVER’s acceptance row; duplication is unnecessary. This establishes requirements, not test execution. |

**Out of scope, noticed**

- The design itself still says detection is pending at `design.md:89`, `:148`, and `:719–720`, despite its recorded option-A decision.
- `runtime-contracts.md:1232` still calls P-OWNER-1 pending; platform §1 records its acceptance and deferred macOS execution.
- `.repo-context/CONTEXT.md:117` says Host reports survivors “after a crash” without distinguishing supervised vendor loss from daemon-crash recovery, which produces no report.

**Validation and limits**

Markdown links: `broken links: 0`. Skill catalog: no failures; unchanged advisory warnings. `git diff --check` passed. The file diff contains no apparent secrets, personal data or machine-local absolute paths. Git status remained clean.

Implementation behavior and AD20 tests were not verified. No files or Git state were changed; no `bd`, vendor CLI or model was run.