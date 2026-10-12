# T4-0 design review: GPT-6 Sol (high)

Review of `wt/t4-0` at `df9b8d2`, verbatim, local links converted to repo paths.

## Verdict: UNSOUND for implementation

The design inventories every C1 method in §3.1–§3.14, F5 and F24–F27, and every carried item named in `docs/workstreams/rust-foundation/t4/t0.md:36`. That coverage does not resolve the following contract conflicts and missing ownership proofs.

### Ranked findings

1. **Blocker — Valid prompts would be refused.** `docs/workstreams/rust-foundation/t4/design.md:39` defers Store blobs and rejects prompts above about 7 MiB. `docs/specs/runtime-contracts.md:1029` requires bounded blob storage to preserve the 16 MiB C1 request surface. Implement that path in Task 4, or seek an explicit contract amendment that narrows C1. Q9’s proposed refusal is not conformance.

2. **Blocker — Follow replay can retain a stopped reader too long.** `docs/workstreams/rust-foundation/t4/design.md:647` adds a two-second wait *before* deciding lag, followed by another two-second notice deadline. `docs/specs/via-api-v1.md:339` and `docs/specs/runtime-contracts.md:1098` require release within two seconds on outbox exhaustion. The proposed `reserve(room)` before `advance` also makes the “immediate” live lag rule unreachable while the outbox is full. Apply the specified exhaustion rule to replay and live events, with one absolute deadline. T4-A7 is a contract change, not a clarification.

3. **Important — The status and spawn persistence plan relies on a false inventory inference.** `docs/workstreams/rust-foundation/t4/design.md:295` say `cwd` and `allow_untested` are already frozen in session `params`; `crates/via-core/src/engine/receipt.rs:140` stores only `harness` and `model`. The proposed status sources also do not account for the full `docs/specs/via-api-v1.md:274`, including queue and turn details after an idle slot is evicted. Specify durable fields and bounded reads for every status member, then test status after restart and slot eviction.

4. **Important — Schema v6 drops assigned contract work without an adequate replacement.** `docs/workstreams/rust-foundation/t4/design.md:920` explicitly omits event columns, turn event bounds, and the connections table assigned to Task 4 by `docs/specs/runtime-contracts.md:700`. Parsing event JSON in a bounded scan may replace the filter columns, but it does not explain equivalent durable connection state or turn bounds. Record separate, explicit amendments and prove the replacement queries and recovery behavior; otherwise implement the assigned schema.

5. **Important — Raw failure propagation has a quiet-stderr gap.** `docs/workstreams/rust-foundation/t4/design.md:320` lets the raw worker set `incomplete`, while reader health changes only on reader-observed failures. A failed stderr unit with no later stdout frame may have no path to wake Route with its Store failure. Specify an independently serviced failure notification and test a raw sync failure after a lone stderr chunk while stdout is quiet. This is required by `docs/specs/runtime-contracts.md:318`.

6. **Important — Follow registration and raw-log isolation need stronger proofs.** `docs/workstreams/rust-foundation/t4/design.md:849` changes more than the word “actor”: `docs/workstreams/rust-foundation/t4/design.md:591` subscribes **before** the first page, whereas `docs/specs/via-api-v1.md:328` specifies registration after the bounded read. State and amend that ordering explicitly. `docs/workstreams/rust-foundation/t4/design.md:545` claim that logs are isolated “by construction” is also too strong: `crates/via-store/src/runtime/raw.rs:154` validates a referenced file and checksum, not its owning session. Define and test the ownership check, including a cross-session reference.

7. **Minor — Tighten the inventory and failure-first tests.** The `crates/via-core/src/api.rs:1341` confirms every Core event has a nullable `turn`; it need not remain [I]. The `crates/via-core/src/api.rs:1360` emits fixed-format UTC milliseconds for ordinary dates, but Store does not validate every `at`, and wall time can move backwards; string order is not a proven commit order. “Every writer goes through `Head`” is false for the `crates/via-core/src/engine/receipt.rs:120`. Scope the claim to post-spawn commits and enumerate those writers. In `docs/workstreams/rust-foundation/t4/design.md:981`, add a sync-count seam for the group-commit assertion and make the observation-budget test demonstrate admission beyond today’s 64-item channel. No planned test uses a fixed sleep as an ordering assertion.

**Amendment disposition:** A3, A4 and A9 are conformance fixes; A1 and A2 mainly distinguish existing runtime requirements from T3’s `Full` mappings. A10 and A11 are appropriately bounded definitions. A5 materially replaces runtime §4’s `WireParts` interface and needs an explicit independent-control proof. A6 must name the registration-order change. A7 conflicts with the current lag bound. A8 needs the separate replacement proofs above. Thus the restatement lists in §9 are incomplete for A6–A8, even though they correctly identify several T2/T3 sites.

### T4-0 open questions

| Question | Answer |
|---|---|
| Q1 | **Agree.** Keep immediate framed saturation failure; measure real-route bursts before proposing a change. |
| Q2 | **Agree**, with `Shutdown` documented as the sole budget bypass. |
| Q3 | **Agree.** A refused Public read is `admission_refused`. |
| Q4 | **Disagree.** Use the literal exhaustion and two-second release rule for replay and live follow. |
| Q5 | **Agree**, if the seeded generator exercises boundary cases and reports its seed. |
| Q6 | **Agree.** `JoinSet` and `watch` fit; align coding-style wording. |
| Q7 | **Agree.** Use C1’s 3,600,000 ms default. |
| Q8 | **Agree.** Defer durable `output_schema` until a supporting route exists. |
| Q9 | **Disagree.** Implement the runtime-required blob path for valid large prompts. |
| Q10 | **Agree.** Nested deadline `null` is `invalid_params`. |
| Q11 | **Agree conditionally.** Seed from durable Store state and test restart plus closing/closed transitions. |
| Q12 | **Agree.** S1’s unpruned `earliest_seq` is 1. |
| Q13 | **Agree.** Close the excess pre-`hello` socket without bytes. |

I inspected documents and representative code paths only. I did not run `cargo`, `bd`, tests, or memory and throughput measurements; I did not verify every event-write call path or every proposed failpoint.

