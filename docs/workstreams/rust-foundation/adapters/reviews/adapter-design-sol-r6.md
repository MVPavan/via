**UNSOUND.** The two wall fixes are specified correctly. The redirect removes most obsolete mechanisms, but the leftover report still has gaps in scan identity, recovery, completeness, delivery, and acceptance tests.

References below are to [revision 6](docs/workstreams/rust-foundation/adapters/design.md) unless another file is named. This reviews the specification of the owner’s policy, not the policy choice.

**Part 1 — fix check**

| Round-5 item | Status | Reason | Design line |
|---|---|---|---|
| Part 1 #1: capped wall, `by_order`/`stopped` | fixed | Specifies the additional generic `stopped` change and asserts both `unknown` state and `cancel.outcome: unknown`; matches the branch that currently overrides `stop_outcome`. | [466](docs/workstreams/rust-foundation/adapters/design.md:466), [518](docs/workstreams/rust-foundation/adapters/design.md:518) |
| Part 1 #2: wall commit wording | fixed | Correctly distinguishes the post-return event commit from the wall instant used as `requested_at`; matches `drive.rs:711–730`. | [440](docs/workstreams/rust-foundation/adapters/design.md:440) |
| Part 2 #1–#3, #5–#6: removed mechanisms | fixed | No active rule still requires the kill loop, subreaper, `DescendantsAbsent`, consuming wait, or former positive descendant predicates. References in the rejected alternative are explicitly conditional. | [672](docs/workstreams/rust-foundation/adapters/design.md:672), [718](docs/workstreams/rust-foundation/adapters/design.md:718) |
| Part 2 #4: Codex clean barrier | partly | Default clean and its proof claim are removed, but VX14 still requires adding per-thread clean tests. | [976](docs/workstreams/rust-foundation/adapters/design.md:976), [978](docs/workstreams/rust-foundation/adapters/design.md:978) |
| Part 2 #7: cleanup/audit contradictions | partly | The original P7 contradiction is removed; the new blanket exclusion of out-of-group processes conflicts with the retained server-item rule. | [679](docs/workstreams/rust-foundation/adapters/design.md:679), [685](docs/workstreams/rust-foundation/adapters/design.md:685), [918](docs/workstreams/rust-foundation/adapters/design.md:918) |
| Part 2 #8: Codex session close | fixed | Session close is unsubscribe only; no descendant cleanup is promised while another lease lives. New report-delivery defects are listed below. | [699](docs/workstreams/rust-foundation/adapters/design.md:699) |
| Part 2 #9: daemon-crash test | partly | The impossible reply requirement is gone, but the replacement recovery report lacks specified durable inputs and a complete return path. | [711](docs/workstreams/rust-foundation/adapters/design.md:711), [933](docs/workstreams/rust-foundation/adapters/design.md:933) |
| Part 2 #10: failure-first tests | partly | Positive reporting assertions would fail today; several standalone negative assertions already pass, and some survivor expectations contradict LH. | [723](docs/workstreams/rust-foundation/adapters/design.md:723), [1345](docs/workstreams/rust-foundation/adapters/design.md:1345) |
| Part 2 #11: public type/test ownership | fixed | Host `lib.rs` and Host tests are now included. New propagation-path overlaps remain. | [1352](docs/workstreams/rust-foundation/adapters/design.md:1352) |
| Part 2 #12: categorical wording | partly | Anchor-killed row removed and Codex close qualified; §2/AD15 corrected, but VC7 still says unconditionally that early EOF completes the turn. | [840](docs/workstreams/rust-foundation/adapters/design.md:840), [951](docs/workstreams/rust-foundation/adapters/design.md:951) |

**Part 2 — new redirect findings**

1. **Important — recovery cannot reconstruct the specified start-time prefilter.**  
   **Locations:** AD20 “How” L712; AR2 L933; S-LEFTOVER L1352.

   Only `vendor_marker` is added to persistence. The design does not specify persisting the vendor’s launch/start bound, its clock domain, or its commit order. Today Host generates the vendor marker at [host.rs:2032](crates/via-host/src/host.rs:2032), sends ARM at [1437](crates/via-host/src/host.rs:1437), and records only `vendor_pid` afterward at [1462](crates/via-host/src/host.rs:1462). Recovery can therefore encounter launched work without post-spawn facts.

   **Smallest fix:** Persist the exact marker and a defined boot-relative launch lower bound before ARM can launch the vendor. Specify boot/namespace validation and handling of missing legacy facts. Test daemon death after ARM but before vendor-facts commit. Define tick precision: an older process born in the same kernel tick cannot be excluded by the proposed comparison.

2. **Important — the scan does not bind its reads to one process incarnation or establish liveness.**  
   **Locations:** AD20 L706, L712–713; AR2 L933; tests L723–728.

   The UID/start prefilter can inspect one incarnation, then pathname-based environment or `comm` reads can inspect a reused PID. No retained proc-directory handle, identity recheck, process-state check, or exit-during-scan rule is specified. Thus the report’s “alive” claim has no defined observation beyond marker matching. Linux documents that reads through retained proc descriptors do not switch to a replacement process. [Kernel procfs documentation](https://www.kernel.org/doc/html/latest/filesystems/proc.html).

   **Smallest fix:** Bind metadata/environment reads to the same proc instance, check start identity and eligible UID consistently, exclude dead/zombie entries, and define entries as observed during the scan. Add PID-reuse and exit-between-each-read fixtures. Keep marker attribution separate from liveness evidence.

3. **Important — permission failures and hidden processes can produce a misleading complete empty report.**  
   **Locations:** AD20 “How,” “Shape,” and “Limits” L712–715; AR2 L933; S-LEFTOVER tests L1352.

   `incomplete` is specified for timeout only. Enumeration failure, unreadable metadata/environment, malformed data, and filtering that hides eligible processes have no disposition. Same UID does not eliminate access restrictions: environment access uses ptrace permission checks, and `hidepid=4` can hide non-ptraceable processes. [Kernel implementation](https://raw.githubusercontent.com/torvalds/linux/master/fs/proc/base.c), [procfs mount options](https://www.kernel.org/doc/html/latest/filesystems/proc.html).

   **Smallest fix:** Define which UID is compared; mark coverage failures incomplete; distinguish confirmed disappearance from access failure. Document changed-UID and namespace visibility limits. Test denied reads and filtered enumeration without escalating privileges.

4. **Important — the scan bound is incomplete and may have no usable budget.**  
   **Locations:** AD20 L712; AD19 L698–700; S-LEFTOVER L1352.

   `min(close deadline, 1 s)` mixes an absolute deadline with a duration. It also runs after the Stop result without defining whether that means the initial `Stopping` reply or completed Host close. Current [Host close](crates/via-host/src/host.rs:1944) can spend the remaining deadline proving absence. Neither environment-buffer size nor scanner concurrency/task cancellation is bounded. Sixteen returned entries does not bound scan memory.

   **Smallest fix:** Name the trigger and use an absolute scan deadline such as `min(close_by, scan_started + 1 s)`. Specify the zero-budget result, preserve the existing close bound, and use bounded streaming reads with explicit truncation/cancellation behavior. Test exhausted close budget and oversized environments.

5. **Minor — the public report shape leaves essential semantics undefined.**  
   **Locations:** AD20 L713; AC10 L925; `TurnEnd.leftovers` L303.

   `started_at` has no type, units, clock basis, or namespace interpretation. `total` is undefined when scanning is incomplete. The 17→16 test does not establish whether list truncation sets `incomplete`, or whether `total` remains exact because counting continues. Entry selection and invalid-byte encoding for `comm` are also unspecified.

   **Smallest fix:** Define these fields, distinguish scan incompleteness from list truncation, and state whether incomplete `total` is a lower bound. Define deterministic retention and bounded encoding; include the maximum report in envelope-size validation.

   The no-scan representation itself is clear: **the caller receives `leftovers: null`**, distinct from a scanned empty report.

6. **Important — the privacy assertions conflict with persistence and overstate what metadata guarantees.**  
   **Locations:** AD20 L712–713, L725–726; AR2 L933; S-LEFTOVER L1352.

   “No environment value is ever serialized” cannot hold literally while the environment’s marker value is persisted as `anchors.vendor_marker`. The report also returns `comm`, which is process-controlled metadata rather than a guaranteed secret-free label. Dropping the environment buffer does not by itself verify that errors, debug formatting, or tracing never expose it.

   **Smallest fix:** Explicitly permit only the marker’s internal persistence, keep it distinct from the private anchor marker, and forbid it and raw environment bytes in diagnostics and public output. Scope the guarantee to the fields VIA reads/emits. Use synthetic sentinels to test success, error, and debug paths; disclose or omit potentially sensitive process labels. Preserve [coding-style §8](.repo-context/coding-style.md:214).

7. **Important — “environment-clearing leftover is missed” is not a sound test definition.**  
   **Locations:** AD20 “Limits” L715 and test L724.

   Clearing the language/runtime environment does not necessarily erase the memory exposed through `/proc/<pid>/environ`. Linux reads the recorded environment memory range; glibc `clearenv` changes the environment array, rather than guaranteeing erasure of that original range. [Kernel implementation](https://raw.githubusercontent.com/torvalds/linux/master/fs/proc/base.c), [glibc implementation](https://raw.githubusercontent.com/bminor/glibc/master/stdlib/setenv.c).

   **Smallest fix:** Say “processes whose procfs-visible environment lacks the marker.” Construct the negative fixture by executing the survivor with an environment that omits it, and independently verify that fixture condition.

8. **Important — the return surfaces do not carry all promised reports.**  
   **Locations:** §3.2 L277–280, L288, L303; AD20 L714; AR2 L933; S-LEFTOVER L1352.

   `TurnEnd.leftovers` is expressly limited to per-turn routes, although server-crash turns also need it. The driver’s `CloseReport` and recovery results gain no specified report field. Adding Host `CloseReport.leftovers` and “carry the report” in Wire does not complete Route/Adapter/recovery propagation. Current [Wire close conversion](crates/via-wire/src/connection.rs:341) and [adapter recovery normalization](crates/via-adapters/src/runtime.rs:605) illustrate those separate seams.

   **Smallest fix:** Specify the report on server-loss `TurnEnd`, driver close, Host/Wire/Route recovery, and adapter recovery facts. Define one report per connection generation and its aggregation where a turn has multiple generations.

9. **Important — report ordering, crash fan-out, and close replay are unspecified.**  
   **Locations:** AD20 L714; AC10 L925; S-LEFTOVER L1352.

   “Every `server_lost` turn” needs one completed scan retained before those turns commit, or an explicit unavailable/incomplete result. No barrier prevents one affected turn committing first. For close, the same report must enter `session.closed`, the close result, and keyed/idempotent replay atomically. Today [Store `commit_closed`](crates/via-store/src/runtime/sql.rs:1625) derives its result from durable rows; it does not accept the proposed report.

   **Smallest fix:** Specify scan-before-terminal/close commit ordering, one retained snapshot for shared-server fan-out, and atomic persistence with close/event/operation results. Recovery must collect reporting facts before publishing recovered envelopes. Test stalled delivery, simultaneous affected turns, and crashes on both sides of these commits.

10. **Important — several server exits have no report destination, and “last lease” needs a defined owner.**  
    **Locations:** AD20 L711, L714; AD16 L863–864; AC10 L925; Codex/OpenCode slice rows L1357–1358.

    Idle retirement can release the last lease without a C1 `close`; Codex explicitly permits idle detachment in [its packet](docs/specs/vendors/codex.md:82). An idle-server crash, daemon shutdown between turns, or recovery of an idle server can likewise have no `server_lost` or recovered turn to receive the report. Concurrent final lease release/new acquisition also needs a rule identifying which close owns the retiring generation.

    **Smallest fix:** Define a durable, retrievable session-level destination for exits without an eligible turn/close. Serialize final release and retirement against new leases, and identify the report’s owning close generation. Preserve terminal-envelope immutability.

11. **Important — AD9/AC2’s blanket group exclusion contradicts their server cleanup rule.**  
    **Locations:** AD9 L679, L685–687; AC2 L918.

    The table correctly restores C1 P7, §7.3 and §7.6: reported server tool items remain relevant until completion or the bound. But the following prose excludes *all* processes outside the agent’s group from cleanup. LH establishes that those reported tools can themselves run outside the server group. The table and prose therefore give different answers for an open reported tool.

    The table also lacks the preserved no-launch case, where no group exists and a complete journal permits quiescence; see [C1 §7.4](docs/specs/via-api-v1.md:683).

    **Smallest fix:** Scope the group exclusion to OS group-absence evidence and untracked descendants. Explicitly retain reported-item waiting on server routes and the no-launch rule. With those qualifications, the restored rules match C1.

12. **Important — acceptance tests contain incorrect survivor expectations and unsupported failure-first claims.**  
    **Locations:** AD20 L723–728; §7 L1345; S-LEFTOVER L1352; Claude L1355; Codex L1357.

    The Claude row requires c7’s setsid `sleep` in leftovers, although [LH E1](docs/workstreams/rust-foundation/adapters/lifecycle-harnesses.md:36) shows interrupt killing the setsid descendant and leaving only the double fork. Codex requires background terminals at server close, while [LH E2c](docs/workstreams/rust-foundation/adapters/lifecycle-harnesses.md:66) observed none surviving sandboxed stdin close. These are conditional outcomes, not universal expectations.

    Positive “listed” assertions would fail today because no report exists. Standalone no-signal/no-serialization/missed-process assertions already pass. `systemd-run --user` also needs declared prerequisites.

    **Smallest fix:** Use controlled survivor fixtures; expect empty sandboxed Codex close reports and positive reports only where survivors actually remain. Separate characterization from regressions, declare integration prerequisites, and add the race, denial, boundary, fan-out, replay, and no-scan cases above.

13. **Important — slice dependencies contradict their acceptance tests, and propagation ownership is incomplete.**  
    **Locations:** §7 L1332–1333; S-CORE L1350; S-LEFTOVER L1352; adapter rows L1355–1358.

    S-LEFTOVER blocks only `x.3.3`, but `x.3.2` acceptance already requires leftover assertions to pass. Its paths also overlap S-CORE’s Core fields, Codex’s Host/Wire/Store work, and OpenCode’s Host/Wire/Store work. Some serialization is acknowledged, but the graph does not establish it. S-LEFTOVER omits the Route/Adapter propagation work identified above.

    **Smallest fix:** Either make the report-dependent adapter acceptance wait for S-LEFTOVER, or move those assertions to the later integration stage. Assign the propagation seams and explicitly sequence overlapping writes.

14. **Minor — residual instructions and status wording remain inconsistent.**  
    **Locations:** VC7 L951; VX14 L978; P11 L87; introduction L13–14.

    VC7 still gives unconditional early-EOF completion; VX14 still demands per-thread clean cases despite that feature being future-only. P11 says there is one new failure fact despite AD4 adding `shared` too. “No owner decision is pending” conflicts with the explicit environment-scan acceptance gate at L1341–1343.

    **Smallest fix:** Qualify VC7, remove/defer clean tests, update P11, and distinguish settled OD3 policy from pending acceptance of the chosen scan exception.

15. **Important — §3.7 does not cover the redirect’s complete contract changes.**  
    **Locations:** audit L1007–1017; AC10 L925; AR2 L933; conflict 15 L194–197.

    Beyond the security omissions below, the audit still stops at AD19/AC9 and omits:

    - C2 §4’s AD20 result fact and §2’s close/recovery report surfaces.
    - C1 §3.6 close result, §5 leftover field, and §6 `session.closed` payload.
    - Runtime §3/§4 report propagation, §5 recovery facts, §6 Store `anchors` schema at L679, and the required schema-version treatment.
    - Runtime recovery/shutdown reporting and §8 scan-resource bounds.
    - The complete Core/report acceptance coverage; the S-CORE maximum-envelope fixture does not yet include leftovers.

    **Smallest fix:** Add explicit amendment rows and owners for those locations. Keep conflict 15’s `stopped` behavior and its two-outcome fixture in the integration acceptance list.

**Separately — conflict-4 audit completeness**

**Incomplete.** Without judging the owner’s choice, additional normative text forbids or constrains these reads:

| Omitted occurrence | Required disposition |
|---|---|
| [runtime §5.1 L485–486](docs/specs/runtime-contracts.md:485): never open `/proc/*/environ` or inspect credentials | Scope the approved exception while preserving anchor verification rules. |
| [coding-style L164–165](.repo-context/coding-style.md:164): never perform a vendor-environment scan | Currently absent from the audit’s L173–174 amendment. |
| [coding-style L175–176](.repo-context/coding-style.md:175): never read/copy/log credentials | Must be reconciled with transient buffer reads, not just invariant 1. |
| [C1 §9 L814–823](docs/specs/via-api-v1.md:814): credential prohibition and bounded OpenCode exception | The existing generated-password exception does not authorize reading user/provider credentials. |
| [platform P-I3 L371](docs/specs/platform-packaging.md:371): vendor environment is never read | This test requirement remains contradictory. |
| [Codex packet L205](docs/specs/vendors/codex.md:205): never read/copy credential contents | Applies to the new scan too. |
| [OpenCode packet L223–224](docs/specs/vendors/opencode.md:223), [OC12 L685](docs/specs/vendors/opencode.md:685), [mirrored C1 amendment L740–749](docs/specs/vendors/opencode.md:740) | Reconcile the dump/credential prohibitions and their mirrored wording. |

Also explicitly disposition [platform L269](docs/specs/platform-packaging.md:269) and [L328](docs/specs/platform-packaging.md:328): anchor marker proof must remain control-based, and a persisted marker must never become liveness evidence. Persisting a random vendor marker is not itself credential recovery; using it for attribution requires independent process observation.

I found no additional executable blanket prohibition on environment reads. Test-support collectors already read synthetic process environments; they do not authorize production scanning.

**Out of scope, noticed**

VO11’s generic post-crash `unknown` wording predates this revision and still needs the distinction already made in [OpenCode L613–619](docs/specs/vendors/opencode.md:613): supervised server death versus recovery after daemon death. I did not reopen it as a redirect finding.

**Could not verify**

No new scan or delivery implementation exists to exercise. I did not run tests, vendor CLIs, models, or `bd`; marker inheritance, native permission behavior, scan timing, and failure-first results remain unexecuted. LH/LM provide recorded evidence, not qualification of this proposed scanner.

No files or Git state were changed. Final branch, HEAD, and status matched the initial observations.

