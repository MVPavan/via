# T4-0 design review round 2: GPT-6 Sol (high)

Review of `wt/t4-0` at `bb54709`, verbatim, local links converted to repo paths.

## Verdict: **UNSOUND for implementation**

The revision addresses much of round 1, but it still conflicts with the exact retry and Wire contracts, and its reader cleanup can exceed the existing deadline. This is a design verdict, not a finding that the current code already implements these mechanisms.

### Decisions 1–20

**APPLIED:** 1–8, 13–15, 18–19. **MISSING:** none. The remaining decisions are **PARTIAL**:

| Decision | Design location | Remaining issue |
|---|---|---|
| 9, Wire health | `docs/workstreams/rust-foundation/t4/design.md:947` | The first-failure wake is specified, but `try_reserve` only forwards ready frames; it does not prove the promised “N−1 frames commit as events” before failure. Specify the Core commit ordering or test the narrower raw-evidence guarantee. |
| 10, reader lifetime | `docs/workstreams/rust-foundation/t4/design.md:821` | Abort and join may consume another three seconds *after* the cleanup deadline. Drain, barrier, abort and join need one shared bound. |
| 11, retained memory | `docs/workstreams/rust-foundation/t4/design.md:235`, `docs/workstreams/rust-foundation/t4/design.md:1200` | The form inventory omits separately retained Store read rows, parsed page values and encoded replies; a single 1 MiB page permit does not account for simultaneous copies. |
| 12, reserved capacity | `docs/workstreams/rust-foundation/t4/design.md:351` | The proof acknowledges timed-out requests retain slots, then claims a healthy writer never has more than one occupied slot. Bound abandoned requests within the shutdown window before relying on eight slots. |
| 16, `list` cursor | `docs/workstreams/rust-foundation/t4/design.md:1278`, `docs/workstreams/rust-foundation/t4/design.md:1790` | Phase 2 changes the specified ordering; its stated guarantee also fails if an initially matching session changes out of the filter before phase 2. |
| 17, durable `status` | `docs/workstreams/rust-foundation/t4/design.md:1716` | A `connections.state='open'` row cannot by itself establish that a process is alive. Spawn validation also omits C1’s absolute-path requirement for `cwd`. |
| 20, slices | `docs/workstreams/rust-foundation/t4/design.md:2125` | S2 owns new `crates/via-core/tests/*`; S3 also claims unspecified new Core tests. Assign disjoint filenames. S1b’s frozen spawn fields depend on DTO work assigned to S3; define a compilable S1b interface or move those DTO fields earlier. |

Thus round 1 findings **1, 4, 6 and 7** have substantive mechanisms; **2 and 5** have mechanisms but need the ordering and capacity proof above; **3** remains open on the cleanup deadline.

### Amendments A12–A20

| Amendment | Recommendation | Contract basis |
|---|---|---|
| A12, two-phase `list` | **Reject as written.** Correct the filter guarantee and obtain an explicit change to the cross-page order. | `docs/specs/via-api-v1.md:304` |
| A13, coordination primitives | **Accept.** Explicit cancellation and bounded joins preserve the runtime requirement. | `docs/specs/runtime-contracts.md:1315` |
| **A14**, frozen `params` keys | **Accept**, with the runtime target table updated. The immutable JSON and replacement query provide a durable source for `cwd` and `allow_untested`. | `docs/specs/runtime-contracts.md:704`; `docs/specs/via-api-v1.md:398` |
| **A15**, `ended_seq` only | **Accept.** Existing `queued_seq` supplies the first bound; the proposed terminal transaction and recovery query supply the last. | `docs/specs/runtime-contracts.md:707`; `crates/via-core/src/engine/terminal.rs:79` |
| A16, S1 status values | **Accept the stated defaults and projection.** Resolve the separate `process.alive` defect before implementation. | `docs/specs/via-api-v1.md:274` |
| **A17**, digest identity comparison | **Reject.** Length and SHA-256 are a probabilistic equality check; the contracts require byte-identical params and retained exact identity bytes. Stream-compare on a digest match. | `docs/specs/via-api-v1.md:195`; `docs/specs/runtime-contracts.md:727` |
| **A18**, 256 KiB blob threshold | **Accept.** The contract mandates blobs *above* 1 MiB and does not prohibit an earlier threshold. | `docs/specs/runtime-contracts.md:1029` |
| A19, overflow stop order | **Accept.** It gives the approved stall an independent control path. | `docs/specs/adapter-contract.md:55` |
| **A20**, non-Clone `WireSender` and direct `WireParts` | **Reject as a package.** Returning `WireParts` directly is reasonable, but the runtime contract expressly requires a clonable control handle. Keep a separate unique lifetime owner for the `JoinSet`. | `docs/specs/runtime-contracts.md:265`, `docs/specs/runtime-contracts.md:297` |

### Defects and concrete fixes

- **Blocker — cleanup bound:** `docs/workstreams/rust-foundation/t4/design.md:836` starts a fresh join allowance at the deadline. Use one absolute cleanup deadline for every stage and test a pipe held open through that deadline. `docs/specs/runtime-contracts.md:973`.
- **Blocker — exact replay and Wire interface:** Apply the A17 and A20 rulings above before coding. Add failure-first tests for a large identity replay through the exact-byte path and for simultaneous frame read/control using the contract-shaped handle.
- **Important — page size and memory:** `docs/workstreams/rust-foundation/t4/design.md:1212` caps *event text*, while C1 caps **encoded page bytes**. Count the complete response and charge each simultaneously retained representation; test an event just below 1 MiB whose wrapper pushes the page over the limit. `docs/specs/via-api-v1.md:322`; `docs/specs/runtime-contracts.md:1041`.
- **Important — `list` proof and lifecycle slots:** Repair the A12 guarantee with a filter-change scenario; make `docs/workstreams/rust-foundation/t4/design.md:351` abandoned-request bound numerical and test repeated timeouts before the latch batch.
- **Important — status and spawn:** Replace the `open`-row proxy for `process.alive` with a source that reflects confirmed live process state, and require `cwd` to be absolute as well as existing. `docs/workstreams/rust-foundation/t4/design.md:1712`; `docs/specs/via-api-v1.md:403`.
- **Minor — citation/restatement inventory:** A20’s restatement list omits the existing Route and Wire call sites, including `crates/via-routes/src/runtime.rs:150` and `crates/via-wire/src/runtime.rs:78`. Add them to the amendment’s update list.

**Slice order:** S1a → S1b → (S2 ∥ S3) → S4 is feasible after the S1b DTO dependency is made explicit. S2 and S3 have disjoint named production files, but their new Core test ownership is not yet disjoint. The shared-file sequence in §11 is otherwise serial.

I content-spot-checked more than 15 design/report `file:line` citations against code and specs, including the raw-index scan, spawn’s frozen params, event `turn`, schema targets, Wire signature, and C1 paging limits. I did not verify runtime timing, RSS, compilability, every event-write path or the full citation inventory. No files were edited; no Cargo or `bd` command was run. The review worktree remained clean at `bb54709`.

