**UNSOUND.** Most round-2 fixes are complete, but OpenCode acknowledgement can still depend on delayed reconciliation. Revision 3 also leaves gaps in deadline cancellation, configuration warnings, and the new effort-failure diagnostic.

Compared all 31 rev2→rev3 diff hunks against the relevant contracts and local evidence. No edits, `bd`, vendor CLIs, models, or tests were run. Branch, HEAD, and Git status match the initial observations.

All table references point to revision 3 of `design.md`.

| Round-2 item | Status | Reason and line reference |
|---|---|---|
| 7 | fixed | Confirmed server death, uncertain transport loss, and ambiguous submission now have distinct dispositions. [L427](docs/workstreams/rust-foundation/adapters/design.md:427) |
| 8 | fixed | `Pending` ends at settlement; the contained fake retains `quiescent`, with non-contained cases gated. [L628](docs/workstreams/rust-foundation/adapters/design.md:628) |
| 9 | partly | Cleanup uses the P7 window, but terminal recognition still waits for every assistant to complete; see R3-1. [L895](docs/workstreams/rust-foundation/adapters/design.md:895) |
| 17 | fixed | Invalid and missing output are distinguished, with explicit S-CORE regressions. [L1038](docs/workstreams/rust-foundation/adapters/design.md:1038), [L1259](docs/workstreams/rust-foundation/adapters/design.md:1259) |
| 23 | fixed | Instance provenance sits outside the fallible outcome; failure-path regressions prohibit substituting another instance’s version. [L305](docs/workstreams/rust-foundation/adapters/design.md:305), [L533](docs/workstreams/rust-foundation/adapters/design.md:533) |
| 30 | fixed | The audit includes OpenCode’s version gate and decline deadline, and both C1 P13 rows. [L918](docs/workstreams/rust-foundation/adapters/design.md:918), [L922](docs/workstreams/rust-foundation/adapters/design.md:922) |
| 31 | fixed | OD3-dependent behaviour and specification changes now have an explicit owner gate. [L1238](docs/workstreams/rust-foundation/adapters/design.md:1238) |
| N1 | partly | Tool cleanup is separated, but delayed assistant completion can still postpone acknowledgement beyond `force_at`. [L895](docs/workstreams/rust-foundation/adapters/design.md:895) |
| N2 | fixed | Settled results cannot carry `Pending`; the contradictory fake expectation is corrected. [L628](docs/workstreams/rust-foundation/adapters/design.md:628), [L639](docs/workstreams/rust-foundation/adapters/design.md:639) |
| N3 | fixed | `TurnEnd.instance` preserves the observed version on successful and failed outcomes. [L305](docs/workstreams/rust-foundation/adapters/design.md:305) |
| N4 | fixed | No-terminal server outcomes now follow C1 §7.6’s death/loss distinction. [L427](docs/workstreams/rust-foundation/adapters/design.md:427) |
| N5 | partly | Unsupported OpenCode “on” requests now warn; unverified inheritance without an applied switch remains inconsistently covered. [L725](docs/workstreams/rust-foundation/adapters/design.md:725), [L1120](docs/workstreams/rust-foundation/adapters/design.md:1120) |
| N6 | fixed | Demonstrated incompatibility only, recipe-scoped keys, transient-failure exclusions, and bounded expiry address the poisoning defect. [L545](docs/workstreams/rust-foundation/adapters/design.md:545) |
| N7 | fixed | The free-string schema invalidates my enum-based prescription; discovery now precedes vendor submission, and the C1 change is disclosed. [L803](docs/workstreams/rust-foundation/adapters/design.md:803), [L840](docs/workstreams/rust-foundation/adapters/design.md:840) |
| N8 | fixed | The gate prohibits merging dependent behaviour and blocks each route’s final review; independent plumbing may proceed. [L1241](docs/workstreams/rust-foundation/adapters/design.md:1241) |
| §3.1 rule-2 exception | fixed | Rule 2 is explicitly excepted and assigned to AD12. [L251](docs/workstreams/rust-foundation/adapters/design.md:251) |
| OpenCode 1-second decline rule | fixed | AD17/VO16 replace it with 5 seconds, retaining the remaining-budget cap. [L799](docs/workstreams/rust-foundation/adapters/design.md:799), [L910](docs/workstreams/rust-foundation/adapters/design.md:910) |
| C1 P13 owner-approval row | fixed | AC1 explicitly supersedes the earlier approval and supplies replacement wording. [L833](docs/workstreams/rust-foundation/adapters/design.md:833) |
| Approved invariant-2 wording | fixed | AD12 preserves route identity, refuses incompatible state, advances compatible resumes, and records execution version per turn—consistent with OD5c. [L695](docs/workstreams/rust-foundation/adapters/design.md:695) |

The author’s **N7 premise is verified**: the supplied [schema](scratchpad/execution/adapter-reprobe/codex/schema-0.159.2/codex_app_server_protocol.v2.schemas.json:14233) defines `ReasoningEffort` as a string with `minLength: 1`, without an enum. I withdraw the wire-enum recommendation. The proposed post-discovery rejection is coherent as an explicitly flagged C1 amendment; the schema does not itself force that particular API policy.

The **N6 cache proposal is sound** against the reported defect. Its ten-minute expiry bounds stale refusals, and another recipe remains eligible for a handshake. No additional cache blocker found.

**Defects in revision 3**

1. **Important — R3-1: OpenCode acknowledgement still waits on delayed assistant completion.**  
   **Location:** [VO1, L895](docs/workstreams/rust-foundation/adapters/design.md:895), versus [AD15, L755](docs/workstreams/rust-foundation/adapters/design.md:755).  
   **Defect:** VO1 requires every matching assistant to be completed before recognizing the terminal. AD15 acknowledges cancellation from `MessageAbortedError` plus idle. Those events can precede the final assistant update. Delaying that update beyond `force_at` therefore permits `unknown` despite the earlier acknowledgement evidence.  
   **Evidence:** The saved trace orders [abort error at L788](scratchpad/execution/adapter-reprobe/opencode/runA/sse1.raw:788), [idle at L791](scratchpad/execution/adapter-reprobe/opencode/runA/sse1.raw:791), tool completion at L797, then [completed assistant at L800](scratchpad/execution/adapter-reprobe/opencode/runA/sse1.raw:800). This verifies the ordering; a long delay was not exercised here.  
   **Smallest fix:** Recognize and retain acknowledgement once the correlated abort marker and idle are both observed, independently of assistant completion. Apply P7 thereafter. Add a fixture delaying both final assistant and tool updates past `force_at`.

2. **Important — R3-2: VO1 returns at the wall deadline before completing the prescribed cancellation path.**  
   **Location:** [VO1, L895](docs/workstreams/rust-foundation/adapters/design.md:895).  
   **Defect:** Without an existing cancel, recognition returns without a terminal at the wall deadline, then assigns “Core’s deadline cancel.” But [AD3](docs/workstreams/rust-foundation/adapters/design.md:370) delivers running-turn stops through `TurnCx.stop` while `run_turn` is pending. The new rule does not specify sending `/abort` before return or another consumer that completes the stop afterward. This can settle a timed-out turn while vendor work continues.  
   **Evidence:** [C1 §7.6](docs/specs/via-api-v1.md:730) requires Core to cancel and populate `cancel`; returning a deadline result alone does not perform that operation.  
   **Smallest fix:** At wall expiry, enter the deadline-stop path while the driver remains serviceable, issue the vendor interrupt, then return under the applicable acknowledgement/control/cleanup rules. Add a wall-expiry fixture that verifies `/abort` is sent.

3. **Important — R3-3: Unknown inheritance states do not consistently produce warnings.**  
   **Location:** [AD13, L725](docs/workstreams/rust-foundation/adapters/design.md:725), [AC7, L839](docs/workstreams/rust-foundation/adapters/design.md:839), [§5.4.1, L1117](docs/workstreams/rust-foundation/adapters/design.md:1117).  
   **Defect:** AD13’s trigger covers an impossible request or an applied unverified switch. It omits an unverified inheritance request implemented by leaving vendor defaults alone. AC7 promises warnings whenever the requested setting could not be verified. Codex skills and agents are explicitly “Unverified,” yet the default-warning list mentions only MCP.  
   **Evidence:** [The category table](docs/workstreams/rust-foundation/adapters/design.md:1112) supplies no verified effective state for those categories; the new rule otherwise assigns `unknown`.  
   **Smallest fix:** Warn whenever the requested effective state cannot be verified, including when no switch is applied. Add the corresponding default-warning cases and a regression for requested `on`, no switch, effective `unknown`.

4. **Minor — R3-4: The new effort diagnostic has no declared turn-envelope location.**  
   **Location:** [AD18, L813](docs/workstreams/rust-foundation/adapters/design.md:813), [AC8, L840](docs/workstreams/rust-foundation/adapters/design.md:840).  
   **Defect:** A receipted failure promises `data.field:"effort"`, but AC8 changes only the effort parameter row. It does not define whether this is `failure.data.field` or another envelope field.  
   **Evidence:** [C1’s failure shape](docs/specs/via-api-v1.md:573) contains `{class, message, vendor_code?, retryable}`, and the [current DTO](crates/via-core/src/api.rs:1941) matches it. [C1 §8.2](docs/specs/via-api-v1.md:792) also still describes `submit_failed` exclusively as vendor rejection.  
   **Smallest fix:** Define the bounded diagnostic field in C1 §5, include it in the amendment audit, and extend the class description to cover adapter-side discovery rejection before vendor submission.

**Out of scope, noticed**

None added. Unchanged text was not re-audited.

**Could not verify**

Implementation or regression-test results, live vendor qualification, and actual Beads dependency edges. OD3’s choice was not assessed; only its documented gate and the `Pending` rule were checked.