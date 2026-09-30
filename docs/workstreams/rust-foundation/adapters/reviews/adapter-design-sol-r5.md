**UNSOUND.** The no-order wall handoff and VO1 race fix are substantially corrected. OD3 still has gaps in reaping ownership, positive cleanup evidence, deadlines and contract consistency.

References are to revision 5 unless another file is named.

**Part 1 — fix check**

| Round-4 item | Status | One-line reason | Location |
|---|---|---|---|
| 1. Core handoff of wall cause | partly | `Deadline` correctly reaches the no-order disposition, but the claim that only `stop_outcome` changes is insufficient for the capped earlier-order/shared-server case. | [L438](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L438), [L471](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L471) |
| 2. Single wall acknowledgement cutoff | fixed | Wall cleanup has one 3 s cutoff; stop orders retain their own `force_at` cutoff. | [L466](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L466) |
| 3. VO1 natural terminals racing cancel | fixed | Natural terminal recognition continues while acknowledgement is pending, with completion and failure race fixtures. | [L999](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L999), [L528](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L528) |

Two problems remain in the replacement wall text:

- **Important — the “only Core change” claim does not cover `by_order`.** Current [terminal.rs L263](../../../../../crates/via-core/src/engine/terminal.rs#L263) calls `stop_outcome`, but the cancel/close branch subsequently constructs its own result: a launched, unforced turn becomes `unknown` with outcome **`requested`** ([L307](../../../../../crates/via-core/src/engine/terminal.rs#L307)). Adding the new argument alone does not implement the promised shared-server row, whose outcome is `unknown`. Specify the additional generic disposition change and assert both state and cancel outcome in the capped-wall fixture at L518–520.
- **Minor — event commit time is misstated.** L444 says Core commits `cancel.requested` “at the wall instant.” [drive.rs L711](../../../../../crates/via-core/src/engine/drive.rs#L711) commits it after the adapter returns; it uses the wall instant as `requested_at`. Correct that distinction.

The underlying S1 chain is verified: Route’s deadline failure → bounded force close → Wire → Host Stop → returned cleanup facts → Core `dispose`. That verification does not establish the new OD3 mechanism.

**Part 2 — new OD3 findings**

1. **Blocker — the absence probe conflicts with ownership of the vendor child.**  
   **Locations:** [AR7 L951](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L951); derived proof claims at AD9 L678–683 and AR2 L947; K16 L63.

   **Defect/evidence:** AR7 promises never to disturb the Tokio-owned handle, then specifies `waitid(ALL, NOHANG)`. A consuming all-child wait can reap the vendor before `Child::try_wait`, losing its status. The literal flags also omit the required state-selection flag. Current [anchor.rs L260](../../../../../crates/via-host/src/anchor.rs#L260) reaps the vendor through that handle. The [Linux wait documentation](https://man7.org/linux/man-pages/man2/waitpid.2.html) distinguishes consuming waits from `WNOWAIT`.

   **Smallest fix:** Keep vendor reaping exclusively through `Child`; reap adopted children by specific PID. Specify a non-consuming final probe using `EXITED | NOHANG | NOWAIT`. Serialize child discovery, signalling and reaping so a PID cannot be reaped between discovery and its signal. Add a vendor-exit interleaving test that preserves the exact exit status.

2. **Important — automatic cleanup has no specified evidence-delivery protocol.**  
   **Locations:** [AR7 L951](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L951); AR2 L947; AD9 L693, L697; AD19 L732–734; S-HOST L1368.

   **Defect/evidence:** Immediate cleanup can reach ECHILD and kill the anchor before Host learns either the vendor exit or the descendant proof. Today exit delivery requires a polled `Status` exchange every 50 ms ([host.rs L1839](../../../../../crates/via-host/src/host.rs#L1839)); `Stopping` reports only the initial live-stop fact ([protocol.rs L116](../../../../../crates/via-host/src/protocol.rs#L116)). Merely adding a reply variant does not define pairing, autonomous delivery, retention or ordering before self-KILL.

   **Smallest fix:** Define one bounded completion exchange/notification carrying vendor exit and cleanup evidence, its receiver and retention rules, and its position before self-KILL. Preserve uncertain outcomes when delivery fails. Test immediate vendor exit and a busy control connection.

3. **Important — cleanup deadlines and the timeout exit are incomplete.**  
   **Locations:** [AR7 L951](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L951); [AD19 L726](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L726), especially server-close cells L733–734.

   **Defect/evidence:** AR7 runs the loop until “the deadline passes,” then performs self-group KILL. This reserves no time for final evidence delivery, anchor exit or group verification. Vendor-exit cleanup has no incoming Stop deadline, so its deadline is undefined. Dispose/EOF waits also have no allocated share of the close budget. Runtime [§5.2 L560](../../../../../docs/specs/runtime-contracts.md#L560) requires escalation and verification within one absolute bound, including KILL by deadline minus 1 s. Self-KILL on timeout can leave escaped children that were never reached; ECHILD-only wording cannot describe that path.

   **Smallest fix:** Define absolute bounds for Stop, EOF and vendor-exit cleanup; budget graceful calls and reserve proof/exit time. Use nonblocking waits. Specify timeout evidence and remaining ownership explicitly, without claiming ECHILD or complete reaping.

4. **Blocker — Codex clean plus completed items is not an exit barrier.**  
   **Locations:** [AD9 L691](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L691); tests L717–718 and L1373; AD19 L733; VX16 L991.

   **Defect/evidence:** The pinned implementation drains its process registry and calls `terminate()`; it does not wait for every process to exit. `terminate()` cancels the token consumed by the completion watcher, which can emit an end event with fallback exit code `-1`. Thus clean success plus `item/completed` does not prove that the sandbox namespace is already empty. Sources: [process manager](https://raw.githubusercontent.com/openai/codex/rust-v0.159.2/codex-rs/core/src/unified_exec/process_manager.rs), [termination implementation](https://raw.githubusercontent.com/openai/codex/rust-v0.159.2/codex-rs/core/src/unified_exec/process.rs), [completion watcher](https://raw.githubusercontent.com/openai/codex/rust-v0.159.2/codex-rs/core/src/unified_exec/async_watcher.rs).

   E1t demonstrates eventual disappearance for its workload; it does not establish the proposed barrier at settlement.

   **Smallest fix:** Keep using clean, but require a qualified termination/namespace-empty barrier for `Quiescent`; otherwise settle `Uncertain`. Add a fixture where clean and synthetic completion arrive before actual process exit.

5. **Important — Codex’s positive claim exceeds the qualified execution surfaces.**  
   **Locations:** [AD9 L691](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L691); K17 L64; AD19 L737–741; VX16–17 L991–992; fixtures L717 and L1373.

   **Defect/evidence:** E1t covers one unified-exec shell tree. Clean calls only `close_unified_exec_processes` ([pinned handler](https://raw.githubusercontent.com/openai/codex/rust-v0.159.2/codex-rs/core/src/session/handlers.rs)). The design itself records unobservable code-mode execution and unqualified hooks/plugins in AD7 L631–636. “The thread ran under bwrap” is neither complete coverage of these surfaces nor a defined observable fact. The table also omits K17’s per-version qualification condition.

   **Smallest fix:** Define the qualified execution/configuration profile and the evidence for actual containment. Require its per-version qualification in the positive predicate. Unqualified surfaces or versions retain `Uncertain`.

6. **Important — “non-shell parts ended” does not prove OpenCode quiescence.**  
   **Locations:** [AD9 L692](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L692); tests L526–527, L719–720 and L1374; propagated through AC2 L933 and conflict 4 L174.

   **Defect/evidence:** A parent `task` part is non-shell, but launches a child session whose tools are outside the parent’s message ledger. This delegation is already verified in [the re-probe L127](../../../../../docs/workstreams/rust-foundation/adapters/reprobe-opencode.md#L127), and the saved task implementation L200 (`scratchpad/execution/adapter-reprobe/opencode/recheck-format/src/oc-1.18.32/packages/opencode/src/tool/task.ts:200`) runs that child session. Custom tools and enabled integrations likewise cannot be classified safe merely by lacking the shell name. LH tested Bash, not this negative classification.

   **Smallest fix:** Use an explicitly qualified allow-list of tools that cannot leave processes, with complete observation continuity. Treat delegation and unknown/custom tools as `Uncertain` unless their descendants are separately proved absent.

7. **Important — the amendment audit leaves contradictory cleanup rules.**  
   **Locations:** [AC2 L933](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L933); conflict 4 L174; audit L1023–1024; AD4 L424–427 and test L505–506.

   **Defect/evidence:** AC2 changes definitions and recovery, but leaves C1 [P7 L67](../../../../../docs/specs/via-api-v1.md#L67), [§7.3 L673](../../../../../docs/specs/via-api-v1.md#L673), and [§7.6 L726](../../../../../docs/specs/via-api-v1.md#L726) with tool-completion-based settlement. The audit explicitly says P7 is unchanged. AD4 still permits return when reported items end even if clean remains outstanding, and its generic 20 s test still promises quiescence without AD9’s qualifications.

   The AR7 audit also omits conflicting reaping text in [coding-style L154](../../../../../.repo-context/coding-style.md#L154), [platform-packaging L284](../../../../../docs/specs/platform-packaging.md#L284), and runtime §10 L1137.

   **Smallest fix:** Amend every affected occurrence to refer to the qualified cleanup proof and explicit settlement rule; retain P7’s absolute time bound. Add the omitted authority documents to S-SPEC.

8. **Important — Codex session-close cleanup is unspecified while another lease lives.**  
   **Locations:** [OD3a L102](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L102); AD9 L688–695; AD19 L733; [gate L1353](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1353).

   **Defect/evidence:** The recipe performs clean then unsubscribe, but AD9 has no row for closing an idle Codex session while its shared server remains alive. Its cancel row requires an interrupted terminal; its server-close row requires anchor cleanup. Neither applies. Missing clean and full-access escapes also leave option (a)’s session-end action incomplete. The owner gate describes a kill loop at Codex session close, although that loop cannot run while another lease lives.

   **Smallest fix:** Add a session-close row, covering all thread terminals, missing/failed clean and full-access uncertainty. State the bounded fallback and retained ownership. Gate all natural run-end cleanup actions, including per-thread clean, rather than describing only the kill loop. This concerns specification, not the pending policy choice.

9. **Important — the daemon-crash test requires evidence the design says is lost.**  
   **Location:** [S-HOST L1368](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1368).

   **Defect/evidence:** “Each” case, including daemon SIGKILL/EOF, must yield a `DescendantsAbsent` reply. AD9 L694 and LM [L219](../../../../../docs/workstreams/rust-foundation/adapters/lifecycle-mechanisms.md#L219) say this reply is lost and recovery is uncertain.

   **Smallest fix:** For daemon crash, assert independent descendant disappearance and `Uncertain` after restart. Require received proof only with a surviving receiver; any alternate receiver must be explicitly designed.

10. **Minor — several S-HOST tests are characterization, not failure-first tests.**  
    **Location:** [L1361–1368](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1361).

    **Defect/evidence:** Today’s own-group cleanup already kills plain/nohup children; `Child::try_wait` already reaps the vendor; anchor-loss uncertainty and no guessed kill already exist. Requiring a nonexistent reply makes these tests red structurally, without detecting their stated behavioral failure. The new tests also omit the decisive consuming-wait race, proof-before-self-KILL ordering and expired-deadline cases.

    **Smallest fix:** Separate preserved characterization from genuine regressions. Make escape survival and cleanup-before-Stop tests fail behaviorally, and add the missing interleavings. Declare prerequisites for bwrap/systemd integration cases.

11. **Minor — S-HOST’s ownership list omits the public evidence type.**  
    **Location:** [L1368](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L1368).

    **Defect/evidence:** `CleanupEvidence` is defined in [via-host/src/lib.rs L196](../../../../../crates/via-host/src/lib.rs#L196), outside the enumerated Host paths. The test files are also absent from ownership.

    **Smallest fix:** Include `lib.rs` and the relevant Host/Wire test paths.

12. **Minor — categorical wording needs three corrections.**

    - **[AD9 L695](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L695):** “Anchor killed … never Quiescent” conflicts with the preserved contained-fake rule when independent `GroupAbsent` evidence succeeds. Scope this row to routes without sufficient independent containment proof.
    - **[AD19 L733](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L733), VX17 L992:** stdin close “stops everything” cites a sandboxed workload; full-access graceful cleanup was not tested, and LH identifies an inferred escape gap. Qualify the vendor claim; retain the Host backstop.
    - **[§2 L232](../../../../../docs/workstreams/rust-foundation/adapters/design.md#L232), AD15 L857–858:** unconditional early-EOF completion conflicts with LH’s active-tool EOF result. State the observed case and bounded fallback.

**Out of scope, noticed:** AD9 L709’s future adopted-child query cannot, by itself, prove absence of descendants still parented beneath a live server. It needs a narrower proof claim before adoption. Anchor-crash coverage remains deferred; I did not challenge that decision or argue for either run-end policy.

**Could not verify:** No implementation, fixture or live qualification was executed. The mechanism probe uses a `/proc` census and blocking waits (anchor.py L24 (`scratchpad/execution/tool-lifecycle/mechanisms/anchor.py:24`)); it does not exercise the specified ECHILD/Tokio protocol. Its vendor-crash case sends Stop after observing survivors, so it does not demonstrate automatic cleanup at vendor exit.

No files were edited, Git state changed, `bd` run, or vendor CLI/model invoked. Final branch, HEAD and Git status matched the initial observations.

