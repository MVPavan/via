UNSOUND

Reviewed the 777-line working-tree design. The checkout advanced from `f8b1d1f` to `3a6cc3e` during review; that commit only appends OpenCode evidence. Reviewed code and contracts are unchanged. I made no changes and ran no Beads, vendor CLI, or model.

1. **blocker — [§3.2, L185](docs/workstreams/rust-foundation/adapters/design.md:185), §5.1 #27. Connection admission still assumes one process per turn.**  
   Core currently acquires a connection permit before every dispatch (`engine/drive.rs:365–370`), and Host retains it for the process group’s lifetime. Four persistent OpenCode servers can consume all four permits; subsequent turns then wait for another permit before reaching their existing drivers. Codex server reuse has the same underlying mismatch. **Smallest fix:** acquire connection capacity only when creating a connection/server; keep turn admission separate. Add a four-server resume regression.

2. **blocker — [AD4, L253](docs/workstreams/rust-foundation/adapters/design.md:253). Moving the only terminal copy onto the observation queue loses S1’s retained evidence.**  
   `TurnEvidence` contains no terminal. A terminal decoded before wall expiry can therefore disappear when its observation cannot be delivered before cleanup finishes. T3 §2 `[s1c.r2]` explicitly preserves decoded terminals across late finalization and distinguishes actual delivery failure from daemon force. Existing Adapter code retains the Route result separately. **Smallest fix:** retain the decoded terminal in the end result, with explicit disposition and ordering rules; add saturated-queue, wall-expiry, latch, and force tests.

3. **blocker — [AD4, L261](docs/workstreams/rust-foundation/adapters/design.md:261). “Shorten `close_by`” cannot implement P7’s acknowledgement deadline.**  
   S1 cancel creates `close_by = requested_at + force_after + 3 s` (`engine/queue.rs:169–170`); by default that is approximately 13 seconds. Shortening it cannot produce `min(acknowledged_at + 60 s, wall)`. Queue order merging also retains the earlier deadline. **Smallest fix:** distinguish the pre-acknowledgement control deadline from P7’s post-acknowledgement cleanup deadline. Test that a reported tool finishing after 13 seconds but before P7’s deadline settles quiescent.

4. **important — [§3.2, L173](docs/workstreams/rust-foundation/adapters/design.md:173). The surface has no session-level observation attachment.**  
   `open_session` receives no sink; the only sink arrives with `run_turn`. This contradicts §3.1’s retained “observations before turns” rule and C2’s session-level identity, closure, late-event, and tombstone behavior. The enum also omits C2’s canonical warning, vendor-closed, and resume-mismatch observations. **Smallest fix:** retain a session observation context attached during open/recovery, with timestamps and attribution; distinguish it from turn execution context.

5. **important — [§3.2, L174](docs/workstreams/rust-foundation/adapters/design.md:174). Host recovery facts do not replace C2’s complete `recover` operation.**  
   The proposed API lists S1 anchor operations but no canonical recovery result capable of returning a resumed driver with observations attached. C2 defines `Resumed`, `Unknown`, and `Dead`; §3.1 says that contract remains. **Smallest fix:** retain the C2 recovery operation and its ownership/context requirements, or explicitly amend the contract to the narrower R1 behavior rather than claiming it is unchanged.

6. **important — [AD4, L263](docs/workstreams/rust-foundation/adapters/design.md:263). Dropping post-report observations reverses approved late-event behavior without flagging the conflict.**  
   C1 §6.1/§7.6 and Codex packet §5 require attributed late durable observations and allow late terminals to revise `unknown`. Codex retains session sinks after settlement and lease release for this purpose. Requiring a future qualification observation before implementing the contract is not an approved exception. **Smallest fix:** preserve attribution and commit required durable observations with `late:true`; ignore only nondurable updates that cannot affect the immutable result.

7. **important — [AD4, L256](docs/workstreams/rust-foundation/adapters/design.md:256). The end-report return conditions are internally inconsistent.**  
   The alternatives permit a private Claude turn to return as soon as its reported tools end—even before process exit and group cleanup. Conversely, “exactly one terminal per turn” excludes legitimate rejection, transport-loss, and death paths. No explicit normal-path cleanup deadline is specified when no stop order exists. **Smallest fix:** define return conditions separately for per-turn processes and persistent servers; retain the wall/cleanup bound and permit an evidence-backed nonterminal failure result.

8. **important — [AD9, L335](docs/workstreams/rust-foundation/adapters/design.md:335), AC2/AR2. The new cleanup predicate is not propagated consistently.**  
   OpenCode is a private server per session, so group absence is unavailable after an ordinary completed/aborted turn while the server remains alive. Recovery still returns unchanged Host group facts, while AD9 additionally requires reported tools to have ended. C1 §7.5 and runtime §5.2 still permit group absence alone to settle cleanup. **Smallest fix:** distinguish process-group cleanup from turn-tool cleanup, specify the persistent-server case, and amend recovery rules. Unknown tool completion after restart must remain uncertain under the proposed predicate.

9. **important — [VO1, L461](docs/workstreams/rust-foundation/adapters/design.md:461). The OpenCode terminal rule does not cover its own failure fixtures.**  
   `t14` creates no assistant, so status cannot come from “the last correlated assistant.” On abort, idle precedes the terminal message and final tool update; one immediate bounded read can still find incomplete state. The permission-rejection shape also needs an explicit canonical status. **Smallest fix:** specify the no-assistant error path, repeated bounded reconciliation after idle, required completion evidence, and the status for each observed terminal shape.

10. **important — [AD8, L324](docs/workstreams/rust-foundation/adapters/design.md:324). The denial implementation and test contradict the once-only rule.**  
    AD8 says a decline-caused denial produces only `vendor.request_declined`, but then derives Claude denials from `permission_denials` and expects `c11b` to produce a denial. Raw `c11b` shows precisely the permission denial caused by VIA’s control reply. Deduplicating only against `permission_denied` does not remove this second report. **Smallest fix:** retain decline-to-tool correlation and suppress its corresponding denial-list entry; expect one decline and zero duplicate denials in `c11b`.

11. **important — [VC6, L445](docs/workstreams/rust-foundation/adapters/design.md:445). Claude assistant usage summing is already disproved by available evidence.**  
    Keying raw assistant snapshots by message ID gives output sums of **6 versus 177** in `c9a`, **3 versus 156** in `c1a`, **4 versus 167** in `c1b`, **4 versus 52** in `c1c`, and **4 versus 139** in `c7`. The snapshots do not contain final output totals. Calling equivalence “unverified” postpones a settled choice. **Smallest fix:** select authoritative `result.usage` now; amend AD6 to permit a turn aggregate rather than requiring every route to provide per-call samples.

12. **important — [AD6, L295](docs/workstreams/rust-foundation/adapters/design.md:295). Core’s existing summing rule is not a turn-wide keyed ledger.**  
    `engine/progress.rs` clears its usage keys at step boundaries and retains only 16 keys per step; further samples enter an unkeyed additive sum. A repeated authoritative key after a boundary or beyond that bound can be counted again. The proposed `a,a,b` test does not exercise either case. **Smallest fix:** define bounded turn-wide replacement semantics separately from step accounting, including overflow and missing-component behavior; add both regressions.

13. **important — [§3.2, L187](docs/workstreams/rust-foundation/adapters/design.md:187). Adapter-produced cost and vendor metadata have no delivery path.**  
    Neither observations nor `TurnEvidence` can carry Claude cumulative cost, `fallback_credit`/`costBasis`, Codex cumulative totals/cache-write/context-window data, or transcript hints. Yet §2 and VC6/VX4 promise these records. **Smallest fix:** extend an existing normalized accounting/evidence payload with the required scoped cost, bounded vendor data, and transcript information.

14. **important — [§5.1 #14, L572](docs/workstreams/rust-foundation/adapters/design.md:572). Per-turn inheritance remains fake-shaped.**  
    `Overrides` (`api.rs:166`) contains only deadlines; `Effective::inherit` (`api.rs:1803–1811`) updates only deadlines. Replacing `Effective::fake` does not implement effort, schema, step-limit, and bound overrides or their null/omission behavior. **Smallest fix:** inventory these sites explicitly and add generic inheritance/clearing tests, including queued resumes and inheritance from the latest accepted turn.

15. **important — [§5.2, L607](docs/workstreams/rust-foundation/adapters/design.md:607). The proposed optional-harness resolution is unreachable through current spawn input.**  
    `SpawnParams.harness` is a required `String` (`api.rs:24`). The inventory changes its documentation but not its input shape. Model-only spawn therefore fails deserialization before `AdapterSet::plan` sees it. **Smallest fix:** make the canonical input optional as C1 requires and test unique, ambiguous, and absent catalog matches.

16. **important — [§5.1, L557](docs/workstreams/rust-foundation/adapters/design.md:557). Core’s unconditional steer refusal is missing from the inventory.**  
    `engine/receipt.rs:548–554` always returns `UNSUPPORTED_VERB`. Adding `SessionDriver::steer` and vendor fixtures does not connect the C1 method to it. **Smallest fix:** inventory this method and implement generic capability checking, active-turn selection, driver delivery, error mapping, and the durable delivery event.

17. **important — [§5.1 #31, L589](docs/workstreams/rust-foundation/adapters/design.md:589). Several fake result/status placeholders remain outside the proposed replacements.**  
    Missed sites in `engine/terminal.rs` include empty denial/decline collectors (**51–53**), null vendor identity (**81**), null structured output (**83**), null vendor steps (**88**), unavailable cost (**90**), null transcript (**97–100**), empty vendor options (**102**), and turn-ID-only vendor fields (**103–105**). `engine/read.rs:407` always reports identity verification false. `classify` (**359–362**) also replaces failed terminals’ canonical stop reasons with `error`, which would lose Claude’s required `max_steps`. **Smallest fix:** inventory and populate each field generically, retain the appropriate failure stop reason, and include Core schema validation with malformed/missing-output regressions.

18. **minor — [§5.1, L553](docs/workstreams/rust-foundation/adapters/design.md:553). “Every production site” omits remaining literals and one public fake type.**  
    Unlisted literal/comment sites are `api.rs:` **16, 38, 165, 231–232, 567, 1045, 1275, 1647, 1922, 1973, 2266**; `engine/drive.rs:` **621**; `engine/receipt.rs:` **547**; and `engine/terminal.rs:` **97**. `FakeTurnRecovery` remains exported at `via-adapters/src/lib.rs:152` and used in `runtime.rs:304,620,634`. Also, #5 names `ResumeParams::fake_overrides`, but the method belongs to `PerTurn`; `envelope_at_maximum` starts at **439**, with its attribute at 438. **Smallest fix:** add all sites and correct those citations.

19. **important — [AR4, L432](docs/workstreams/rust-foundation/adapters/design.md:432), S-LAUNCH. A journaled startup probe cannot use the current owner schema.**  
    `ProcessOwner` requires a session and turn. `AnchorIntent` requires both, and `anchors` has a foreign key to `turns`. Wire also creates a turn evidence directory from that owner. A daemon-start `Probe { harness }` has no such turn. S-LAUNCH omits Store ownership/schema/recovery changes. **Smallest fix:** design the non-turn owner and its evidence, admission, recovery, and shutdown behavior; include the owning Store/Wire paths and tests.

20. **important — [§5.2, L611](docs/workstreams/rust-foundation/adapters/design.md:611), AR4. The catalog cache has no population lifecycle.**  
    `plan`, `models`, model-only resolution, and effort checks are synchronous/pure. Codex’s catalog requires a live app-server; OpenCode’s requires a server request. AR4 discovers only executable versions. **Smallest fix:** specify bundled versus discovered metadata, how discovery occurs through owned lower layers, cache freshness, and truthful behavior before discovery. Do not hide server launches inside pure planning.

21. **important — [AD7, L311](docs/workstreams/rust-foundation/adapters/design.md:311), §5.4. The version rule makes the fake unavailable.**  
    AD7 refuses unparseable versions for every route. The existing fake deliberately reports no vendor version and is untested (`api.rs:1835–1851`), yet must remain available for S1 and the release-feature check. **Smallest fix:** define an adapter-owned deterministic version policy for the test double, or explicitly exempt version-inapplicable routes without adding a Core harness branch.

22. **important — [OD1, L64](docs/workstreams/rust-foundation/adapters/design.md:64). The recommendation overstates what handshakes prove and understates R1 scope.**  
    A successful handshake/init can check exposed settings and wire shape; it cannot establish unchanged never-ask behavior, effective effort, hidden execution surfaces, or complete cleanup. The bad-effort probes demonstrate that accepted configuration can be ignored. “R1 offers only `full`” also overlooks planned Codex read-only/workspace-write qualification in `via-5lr.3.4`. **Smallest fix:** state the checks and residual risks precisely; present proceeding on untested versions as an owner-approved risk choice, and correct the bounds rationale.

23. **important — [AD7, L314](docs/workstreams/rust-foundation/adapters/design.md:314), AD12. Per-turn version provenance and frozen capabilities are underspecified.**  
    The new result surface carries no actual launch-version update. A persistent server can continue running an older binary after the executable changes, while preflight sees the newer one. Capabilities remain frozen although OD1 permits vendor upgrades. Compatible adapter-version resumes also lack a rule for the version subsequently persisted and reported. **Smallest fix:** specify actual-instance metadata delivery, capability compatibility checks, and preservation of the session’s adapter-version identity.

24. **important — [OD2, L65](docs/workstreams/rust-foundation/adapters/design.md:65), AD13, §4 L540. The inherited-configuration recommendation is stronger than its implementation/evidence.**  
    Claude plugin hooks remain unverified; Codex MCP suppression is unverified. `--safe-mode` also disables the prompt configuration OD2 recommends inheriting. No demonstrated readback supplies a complete inventory of inherited instruction files/configuration across the three routes. `canonical_cwd` identifies a location, not the configuration contents discovered there. **Smallest fix:** distinguish verified suppression, inherited execution exceptions, and unavailable inventory explicitly; qualify the selected recipe before claiming the split policy is enforced.

25. **minor — [OD3, L66](docs/workstreams/rust-foundation/adapters/design.md:66). The alternative analysis overstates the subreaper conflict and gives an already-satisfied revisit trigger.**  
    Runtime §5.1 prevents claiming continued Host reaping after the anchor’s self-KILL; it does not prohibit subreaping while the anchor lives or establish that adding it necessarily requires pidfds. The stated trigger—survival after normal cancel—already occurred in Codex `c3`. WSL delegation availability is not established by the supplied evidence. **Smallest fix:** describe the narrower forced-cleanup limitation, label deployment assumptions unverified, and base deferral on the demonstrated per-session attribution gap and cost.

26. **important — [§8, L754](docs/workstreams/rust-foundation/adapters/design.md:754). A binary override alone cannot replay vendor protocols through today’s fake agent.**  
    The fake requires `VIA_FAKE_SCENARIO`/`VIA_FAKE_SYNC_DIR`, validates its own `StartRequest`, and handles its own interrupt schema (`via-fake-agent/src/main.rs:146–148,249–276`). It cannot accept Claude user lines or Codex initialize/thread/turn RPCs merely because `EmitRaw` exists. Real adapters’ allow-lists do not pass the fixture variables. Version probes also need distinct handling. **Smallest fix:** explicitly own a bounded vendor replay mode and test-only fixture configuration, including request sequencing and probe responses; preserve the existing fake protocol.

27. **important — [§5.5, L659](docs/workstreams/rust-foundation/adapters/design.md:659). The literal checker misses names it promises to catch and has unsafe exclusions.**  
    Camel splitting makes `OpenCodeAdapter` become `open/code/adapter`, which does not match the table token `opencode`; similarly `OpenAI` does not match `openai`. Column-zero braces can occur inside multiline raw strings, so rustfmt does not guarantee the proposed block boundary. Excluding every `tests.rs`/`tests/` path regardless of compilation context can hide production code. **Smallest fix:** normalize table names consistently with scanned tokens, recognize actual test-module boundaries, and self-test these cases. Describe it as a literal guard, not proof that Core has no harness-shaped behavior.

28. **minor — [§2, L123](docs/workstreams/rust-foundation/adapters/design.md:123). The matrix is incomplete and contains overstated cells.**  
    It lacks explicit entries for C2’s `session.vendor_closed`, `resume.mismatch`, session/late attribution, and the cleanup-observation replacement. Codex’s schema changes were not “additive changes only”: the report records removed plugin definitions/fields. OpenCode’s crash-after-acceptance case demonstrates unresolved execution, not an induced uncertain-submission boundary. Claude “no catalog” should distinguish the absent vendor catalog from its packet’s bundled VIA catalog. **Smallest fix:** add the missing rows and correct those cells.

29. **minor — [§4, L505](docs/workstreams/rust-foundation/adapters/design.md:505). Not every reported drift is actually dispositioned.**  
    `CX drift 7 → VX12` covers `instant_interrupt` but not `write_stdin_approval` becoming stable/on or removal of `guardianv2.thread_context`. OC drift 9’s git-root resolution and the explicitly inferred instruction-file absence are not separately addressed. The new format addendum is also absent. **Smallest fix:** record a specific amendment, adapter action, or reasoned no-change disposition for each item.

30. **important — [§1.5, L103](docs/workstreams/rust-foundation/adapters/design.md:103), S-SPEC. The amendment audit misses affected contract text.**  
    Runtime §2 still says C1 DTOs live in Core, conflicting with the decided placement of adapter-produced DTOs. C1 §3.2 retains the old tested-version/executable-consistency explanation, while AC1 names other sections. Existing vendor acceptance tables still require old version/usage/class behavior, including Codex’s “never sum” snapshot test. Late-event and recovery conflicts are likewise omitted from §1.5. **Smallest fix:** enumerate and amend every affected normative occurrence and test expectation, rather than only the principal mapping rows.

31. **important — [§7, L717](docs/workstreams/rust-foundation/adapters/design.md:717). The slice dependencies and acceptance conditions are not fully executable.**  
    Existing exported Beads make each `.3.3` depend on `.3.4`, but the table presents Claude review/live verification before qualification. New S-CORE/S-LAUNCH dependencies are proposals, not present edges. Owner decisions that change approved behavior have no explicit implementation gate. “All S1 unit tests unchanged” is also incompatible with removing the public fake types imported by those tests. **Smallest fix:** state the actual dependency order and coordinator updates required, gate owner-dependent behavior, and preserve characterization assertions while allowing necessary API migrations.

32. **minor — [OD4, L67](docs/workstreams/rust-foundation/adapters/design.md:67), VO2. New committed evidence changes the defect disposition.**  
    The `3a6cc3e` addendum reports existing upstream issues and an open fix, so another report is unnecessary. It also narrows the pagination failure and proves `format:{type:"text"}` fails too; local `reads-X1_format_text.json` confirms that failure. **Smallest fix:** update OD4 to tracking the existing report/fix, disposition the addendum, and require omission of `format` on supported plain turns.

**Out of scope, noticed**

- C2 A6 gives a five-second auto-decline deadline, while the committed Codex packet §4 gives a one-second default. This contradiction predates the proposed amendments.
- `.repo-context/repo-map.md` still describes the earlier partial S1 checkpoint and known compilation failure, whereas the current handoff records later S1 closure and stable interfaces.

**Could not verify**

- Build/test/live qualification results: none were run under the read-only boundary.
- Claude hook/plugin suppression and effective per-model effort; Codex MCP suppression and live acceptance of its decline bodies; OpenCode skill suppression. These remain explicitly unproven.
- The newer addendum’s GitHub issue/PR status and actual interfaces of the five future products were not independently checked externally.
- Numerical version floors, compatibility declarations, and OD1–OD5 approvals remain unsettled.

The design’s Markdown links resolve. I found no secrets, personal data, or machine-local absolute paths in the design itself.

