# T4-0 design review round 3: GPT-6 Sol (high)

Review of `wt/t4-0` at `f63252d`, verbatim, local links converted to repo paths.

## Verdict: UNSOUND for implementation

The rewrite closes most of the round-2 design gaps, but the S5 schema change and a Store transaction cap conflict need correction before implementation. I treated `docs/workstreams/rust-foundation/t4/review-r2-astra.md:1` as my prior review.

### Round-2 decisions 1–15

**APPLIED:** 1–3, 5, 8–13, 15. **MISSING:** none. These remain **PARTIAL**:

| Decision | Remaining gap |
|---|---|
| **4 — memory accounting** | The design charges simultaneous page texts and replies, but its typed-DTO charge rests on an unproved “no more than a `Value`” claim; the proposed allocator test measures `Value`, while the Core `build` check measures node count. Test actual DTO and Core allocations against their permits. `docs/workstreams/rust-foundation/t4/design.md:266`, `docs/workstreams/rust-foundation/t4/s1.md:161`, `docs/workstreams/rust-foundation/t4/s3.md:145` |
| **6 — `list`** | Phase 2 can skip matching rows if a 1000-ID scan advances its cursor beyond the 200-result or byte limit. Its proof also assumes finitely many new sessions, short of the decision’s unconditional termination requirement. Stop the scan at the first unreturned row and bound the ID range at phase-2 entry. `docs/workstreams/rust-foundation/t4/design.md:1260`, `docs/workstreams/rust-foundation/t4/design.md:1737` |
| **7 — `status`** | `vendor_pid` with no recorded absence proves a launch and an *unproven* absence, not current liveness. Use bounded positive Host/process evidence for `process.alive`; keep uncertain cleanup distinct. `docs/workstreams/rust-foundation/t4/design.md:1625`, `docs/workstreams/rust-foundation/t3/design.md:469` |
| **14 — slice boundaries** | S2 establishes schema v6 without blob columns; S5 adds those columns under the same version, while older development stores are refused and no migration exists. Put the blob columns and CHECKs in S2’s v6 schema, with S2 writing inline values. `docs/workstreams/rust-foundation/t4/s2.md:15`, `docs/workstreams/rust-foundation/t4/s5.md:24`, `docs/workstreams/rust-foundation/t4/design.md:632` |

Of my eight round-2 findings, **pending-write control, raw-worker death, cleanup ownership, subscription teardown, and applied `cwd` are closed in the design**. Memory accounting, `list`, and durable `status` remain open as above. These are design dispositions, not runtime results. `docs/workstreams/rust-foundation/t4/review-r2-astra.md:57`, `docs/workstreams/rust-foundation/t4/design.md:835`, `docs/workstreams/rust-foundation/t4/design.md:469`, `docs/workstreams/rust-foundation/t4/design.md:937`, `docs/workstreams/rust-foundation/t4/design.md:1383`, `docs/workstreams/rust-foundation/t4/design.md:1571`

### Amendments and new defects

- **A12 — reject as written.** A numbered C1 amendment is necessary because phase 2 changes the specified cross-page keyset order. The revised filter-change guarantee is reasonable, but the cursor and termination defects above defeat its proof. Add failure-first tests with over 200 matching IDs in one window and continuous session creation. `docs/specs/via-api-v1.md:304`, `docs/workstreams/rust-foundation/t4/s4.md:119`
- **A21 — reject as stated.** A smaller terminal bound is permissible, but “always fits” is false for sufficiently large legal request IDs; the design itself acknowledges this. C1 already specifies `admission_refused` for an event that cannot fit a page. Restate A21 as a cap that fits **when the measured wrapper leaves room**, and add the omitted C1 §5 envelope-limit restatement. `docs/workstreams/rust-foundation/t4/design.md:1186`, `docs/specs/via-api-v1.md:322`, `docs/specs/via-api-v1.md:470`, `docs/workstreams/rust-foundation/t4/design.md:1896`
- **Blocker — unlisted transaction-cap exception.** The failure-resolution allowance `EVENT_MAX + 64 KiB` exceeds 1 MiB by 48 KiB. Runtime §8 caps a Store transaction at 1 MiB, including lifecycle atomic batches. Bound the whole batch within 1 MiB or propose an explicit amendment and restatement. Add a boundary test for the largest terminal plus eight cancellations. `docs/workstreams/rust-foundation/t4/design.md:516`, `docs/specs/runtime-contracts.md:1020`
- **Blocker — same-version schema change.** The S2→S5 defect above needs a reopen test using a Store created by S2. The current S5 fresh-Store test would miss it. `docs/workstreams/rust-foundation/t4/s5.md:119`
- **Important — exit gate omission.** S1’s inherited acceptance command list omits `cargo deny check`, although repository verification requires it. Add it to the shared slice command list; S7’s final gate already calls for every verification command. `docs/workstreams/rust-foundation/t4/s1.md:183`, `.repo-context/verification.md:12`, `docs/workstreams/rust-foundation/t4/s7.md:102`

I found no other newly numbered amendment. The A21 restatement search covered C1, C2, runtime, T2 and T3; its material omission is C1 §5.

### Slice briefs and exit evidence

**S3 and S4 have disjoint named file ownership**, including their new Core test files. Their declared interfaces are plausibly compilable at the parallel boundary; I did not compile them. Each brief names a daemon end-to-end test, and each provides owned files, interfaces and acceptance checks. S2→S5 is the persistence boundary that fails despite that source-level split. `docs/workstreams/rust-foundation/t4/s3.md:24`, `docs/workstreams/rust-foundation/t4/s4.md:23`, `docs/workstreams/rust-foundation/t4/design.md:2052`

Decision 13’s exit plan now explicitly requires F1–F30 artifacts, named F15/F16/F18 scenarios, a `blobs/` handle scan, a measured roughly two-minute default suite, separate daemon and anchor RSS with the 384 MiB sum, and a **turn `cancel`** response within 100 ms even during a blocked write. These are planned checks, not achieved evidence. `docs/workstreams/rust-foundation/t4/design.md:1972`, `docs/workstreams/rust-foundation/t4/design.md:2007`, `docs/workstreams/rust-foundation/t4/s7.md:50`

I spot-checked more than 15 file-and-line locations against the worktree code and binding documents. I did **not** run Cargo, Beads, tests, benchmarks, RSS measurements, or a compiler check. I made no edits; `wt/t4-0` was clean at `f63252d`.