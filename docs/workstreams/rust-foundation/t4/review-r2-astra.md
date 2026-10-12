# T4-0 design review round 2: GPT-6 Astra (high)

Review of `wt/t4-0` at `bb54709`, verbatim, local links converted to repo paths.

**UNSOUND for implementation.** The rewrite resolves much of round 1, but control, failure publication, cleanup deadlines and memory accounting still have blocking gaps.

Reviewed clean `wt/t4-0` at `bb54709`. Locations below refer to the revised `docs/workstreams/rust-foundation/t4/design.md`.

**1. Decisions 1–20**

APPLIED:

| Decision | Design location |
|---|---|
| 1 — Blob path | §3.6 |
| 2 — Immediate follow lag | §6.4 |
| 3 — Registration order | §6.3, steps 1–5 |
| 4 — Schema targets | §3.7; subject to A14/A15 rulings |
| 5 — Observation cap | §5.1 |
| 6 — Log ownership | §3.7, §6.1 |
| 7 — Bounded log lookup | §3.4, §6.1 |
| 12 — Reserved Store capacity | §3.1–§3.5 |
| 13 — Lane locking/shutdown | §3.3 |
| 15 — Session counts | §8.2 |
| 18 — Inventory corrections | §3.7, §6.2 |
| 19 — Test requirements | §10.1–§10.3 |

PARTIAL; none wholly MISSING:

| Decision | Remaining gap and location |
|---|---|
| **8 — Stall/control** | §5.3 keeps the outer stop arm alive, but awaits interrupt writes inside that arm without specifying continued stop/deadline polling. Finding 1. |
| **9 — Wire health** | §3.4 and §4.3 notify queued commands on worker death, leaving the in-flight batch uncovered. Finding 2. |
| **10 — Reader lifetime** | §4.1 adds another three seconds after the cleanup deadline and permits return with unjoined readers. Finding 3. |
| **11 — Memory** | §2.4, §6.1 and §6.4 still under-account read/decode/serialization coexistence. Finding 4. |
| **14 — Follower lifetime** | §6.3 and §6.4 contradict each other about permit release; terminal termination lacks the stated deadline. Finding 5. |
| **16 — List cursor** | §6.1/A12’s replacement proof fails when an initially matching session changes filter membership. Finding 6. |
| **17 — Durable status** | §8.4 equates raw-log state with process liveness; blob-backed effective values cause refusal regardless of actual response size. Finding 7. |
| **20 — Slice plan** | §11 names the shared files, but does not establish the prerequisite interfaces needed by S1b before S3. See item 4 below. |

Round-1 blockers **1–4 remain partially unresolved** through findings 1–4. **5 is addressed** by the Store partition and sequential lifecycle issuer. **6’s registration and terminal-at-start gaps are addressed**, with the delayed retirement sweep remaining an explicit limitation. **7 remains partially unresolved** through finding 5.

**2. Amendments A12–A20**

| Amendment | Recommendation |
|---|---|
| **A12 — Two-phase list** | **Reject as written.** Catch-up changes C1’s prescribed order, so this requires an actual amendment; its claimed initial-filter reachability is also false. `docs/specs/via-api-v1.md:304`, finding 6. |
| **A13 — Coordination primitives/tests** | **Accept.** Explicit cancellation and bounded joins preserve the required ownership behavior; this implements settled Q5/Q6. `docs/specs/runtime-contracts.md:81`. |
| **A14 — Frozen params keys** | **Accept.** Immutable keys and the replacement query preserve durable values without duplicate columns. `docs/specs/runtime-contracts.md:704`, `docs/specs/via-api-v1.md:398`. |
| **A15 — `ended_seq` only** | **Accept.** Existing `queued_seq` supplies the other endpoint; atomic terminal writes and recovery reconstruction satisfy the event-bound target. `docs/specs/runtime-contracts.md:694`. |
| **A16 — Status/list definitions** | **Accept the explicitly bounded S1 definitions**, including newest-64 turns, provided those changes are actually incorporated into C1. This does **not** approve finding 7’s liveness inference or blob-based refusal. `docs/specs/via-api-v1.md:274`. |
| **A17 — Digest-only identity equality** | **Reject.** Collision resistance does not preserve the binding byte-identical guarantee. Use length/hash to reject mismatches quickly, then compare matching candidates in bounded chunks through the blob owner. `docs/specs/via-api-v1.md:196`, `docs/specs/runtime-contracts.md:727`. |
| **A18 — 256 KiB blob threshold** | **Accept.** Runtime requires blobs above 1 MiB; it does not prohibit earlier spilling. Retain an exact command-size guard—the “about 1 MiB” argument alone is insufficient. `docs/specs/runtime-contracts.md:1029`. |
| **A19 — Overflow stop order** | **Accept the mechanism**, retaining T3’s cause coalescing. Complete serviceability through pending writes as finding 1 requires. `docs/specs/adapter-contract.md:55`, `docs/workstreams/rust-foundation/t3/design.md:185`. |
| **A20 — Non-Clone sender/direct parts** | **Accept the interface amendment.** One lifetime owner and separately borrowed control/data halves can provide the required concurrency. The claim that cloning necessarily duplicates lifetime ownership is unnecessary; approval depends on fixing cleanup, not that argument. `docs/specs/runtime-contracts.md:265`. |

**3. Remaining/new defects, ranked**

1. **Blocker — Pending stdin/control writes can still hide stop changes.**  
   Design `docs/workstreams/rust-foundation/t4/design.md:1133` awaits `Control::on_wake`; that method awaits `write_frame` using the turn deadline (`crates/via-routes/src/runtime.rs:469`). Existing writes select daemon cancellation and the wall deadline, not updated `force_at` (`crates/via-wire/src/runtime.rs:397`); “health between pieces” does not cover a blocked piece or raw acknowledgement.  
   **Fix:** retain the write as pending state and service stop changes, `force_at`, force and health throughout every write/ack wait, preserving partial-write offsets.

2. **Blocker — Raw-worker death misses its in-flight batch.**  
   Design `docs/workstreams/rust-foundation/t4/design.md:466` publishes `WriterLost` through **queued** commands. If the worker panics after removing a lone stderr append into its batch, unwinding drops its acknowledgement, which the stderr reader does not await. No remaining queued sink need wake Route. T3 expressly classifies raw-thread disappearance or dropped replies as uncertain (`docs/workstreams/rust-foundation/t3/design.md:1888`).  
   **Fix:** a death guard must own and fail both queued and in-flight batch sinks/replies. Test worker death after dequeue with stdout quiet.

3. **Blocker — Cleanup extends its absolute deadline and relinquishes task ownership.**  
   Design `docs/workstreams/rust-foundation/t4/design.md:836` starts another three-second join allowance **at** the supplied deadline, then permits `pending_tasks`. That contradicts the absolute close-and-drain bound (`crates/via-routes/src/lib.rs:287`) and the design’s own “no reader outlives finish” claim.  
   **Fix:** budget drain, barrier, abort and join against one deadline. If joining cannot finish, transfer the owned tasks to shutdown supervision explicitly; do not return by dropping their owner.

4. **Blocker — Encoded-page permits do not bound retained allocations.**  
   Design `docs/workstreams/rust-foundation/t4/design.md:1200` reserves 1 MiB for a page, but then retains decoded events and serialized output; existing Store reads construct `Value`s (`crates/via-store/src/runtime/sql.rs:1607`). `logs` additionally permits 1 MiB plus another unit before conversion. One encoded-byte allowance cannot cover these simultaneous representations. The fixed 40-byte node estimate also lacks a conservative container-capacity proof. `docs/specs/runtime-contracts.md:1041` requires all such allocations to be charged.  
   **Fix:** meter each retained representation before allocation, or preserve encoded events and stream bounded serialization. Count response wrappers/separators too: §6.1 currently counts event text alone against C1’s encoded-page limit.

5. **Important — Subscription teardown has contradictory ownership rules.**  
   Design `docs/workstreams/rust-foundation/t4/design.md:1424` retains subscription permits until notice/deadline; `docs/workstreams/rust-foundation/t4/design.md:1480` releases them at the end decision while notices/state remain queued. Repeated unsubscribe/register cycles can therefore evade the intended bound on retained termination state. `terminal` is also omitted from the deadline triggers, and the connection deadline is set “once” without a reset rule. `docs/specs/runtime-contracts.md:1098`.  
   **Fix:** retain accounting for ending subscriptions until retirement; include terminal endings; define a termination episode that ends after successful notices or socket closure.

6. **Important — A12’s reachability proof ignores mutable filters.**  
   Design `docs/workstreams/rust-foundation/t4/design.md:1294` applies current filters in both phases, while `docs/workstreams/rust-foundation/t4/design.md:1791` promises every initially matching session. An unseen `state=active` session becoming idle disappears from both phases. `docs/specs/via-api-v1.md:306`.  
   **Fix:** preserve snapshot membership/version history, or request an explicit weaker filter-membership guarantee with a correct proof.

7. **Important — Status conflates raw completeness with process liveness.**  
   Design `docs/workstreams/rust-foundation/t4/design.md:1735` reports alive only for an `open` connection, but §3.7 changes that state to `incomplete` on raw failure, potentially before cleanup. Conversely, submission creates `open` before successful launch. `docs/specs/via-api-v1.md:278`.  
   **Fix:** derive liveness from process evidence separately from log completeness. Also assess blob-backed `effective` by actual response size; the 256 KiB storage threshold is not C1’s response limit.

8. **Important — Newly accepted `cwd` is persisted but not applied.**  
   Design `docs/workstreams/rust-foundation/t4/design.md:1712` specifies validation/freezing only. The fake still launches using its configured daemon cwd (`crates/via-adapters/src/fake_config.rs:75`), and the envelope emits `cwd: None` (`crates/via-core/src/engine/terminal.rs:67`).  
   **Fix:** validate an absolute directory, pass the frozen cwd through dispatch to the process specification, and report it consistently. Assign those cross-slice hooks explicitly.

**4. Slice feasibility**

**Existing-file ownership is disjoint for S2/S3 as listed.** The broad “new Core tests” allocations still need explicit filenames.

**The order is not yet implementation-ready.** S1b’s R1 promises frozen spawn fields and blob-aware receipt plumbing, while `SpawnParams` and `parse_bounded` belong to later S3; the current DTO lacks those fields (`crates/via-core/src/api.rs:17`). Specify compilable intermediate interfaces: move shared DTO/blob-input prerequisites into S1b, leaving parsing/dispatch wiring to S3, or defer the dependent R1 work explicitly. Add the cwd handoff above. Then the proposed dependency order is feasible.

No files changed; no Cargo, Beads, tests or benchmarks ran. I did not verify runtime timing/RSS, every writer/caller, parser correctness, or the proposed failpoints. Findings are document and source-level traces.