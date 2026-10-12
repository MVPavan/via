# UNSOUND

Round 6 resolves the row-count cap and specifies the shared-route stall outcome. It still does not prove the memory and disk bounds, complete step history, or reliable raw-log paging required by R1–R7.

## Round-5 findings

| Round-5 finding | Status | Round-6 design |
|---|---|---|
| 1. Step-row cap | **Resolved** | `docs/workstreams/rust-foundation/t4/design.md:477` removes it. |
| 2. Refused rows | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:481` carries them into the drive’s terminal, but the guarantee and other terminal paths remain inconsistent. |
| 3. Memory capacity | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:687` charges major buffers up front, but undercharges decoded strings and excludes SQLite’s contracted cache. |
| 4. Disk quota | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:1093` adds a quota whose concurrent checks and SQLite growth do not enforce its limit or reserve. |
| 5. Shared `logs` | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:598` defines attribution, but permits silently unservable units and leaves an initial polling cursor undefined. |
| 6. Shared-route stall | **Resolved at design level** | `docs/workstreams/rust-foundation/t4/design.md:252` specifies thread quarantine while other threads continue. |
| 7. Envelope overrun | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:281` orders an immediate stop for denied/declined accumulation; streamed final text and stop-order precedence remain open. |
| 8. Failure summary | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:957` budgets most members, but its fixed-field group has no enforced over-budget action. |

## Blockers

1. **Decoded strings can exceed their observation charge.** `docs/workstreams/rust-foundation/t4/design.md:201` allocates a `String` at the *raw JSON span* length; `docs/workstreams/rust-foundation/t4/design.md:239` charges its *decoded* length and claims capacity equals length. Escaped text can make those values very different, allowing the 4 MiB observation queue to retain more than its charge. **Smallest fix:** charge actual capacity, or count decoded bytes before allocating exactly that capacity.

2. **The 128 MiB partition sum omits a contracted resident allocation.** `docs/workstreams/rust-foundation/t4/design.md:725` allocates all 128 MiB to four partitions, then `docs/workstreams/rust-foundation/t4/design.md:748` excludes SQLite’s 8 MiB cache. `docs/specs/runtime-contracts.md:1057` includes that cache in the global budget; an RSS test cannot substitute for its permit. **Smallest fix:** reserve the cache within 128 MiB and redo the maximum-count arithmetic, lowering an admission count if needed.

3. **The disk quota and lifecycle reserve are not enforced.** `docs/workstreams/rust-foundation/t4/design.md:1100` gives separate raw and SQLite writers atomic counters but no atomic *total* reservation: both can pass a check against the same free space. `docs/workstreams/rust-foundation/t4/design.md:1104` also checks SQLite `Command::bytes()` before writing, although page and WAL growth are measured only after commit. The acknowledged overshoot at `docs/workstreams/rust-foundation/t4/design.md:2147` can consume space reserved for terminals. **Smallest fix:** serialize or atomically reserve total quota across both writers, using a proved upper bound for SQLite growth before commit; reconcile the charge afterward. Prove the 16 MiB reserve against those charges.

4. **Accumulating final text can outrun the envelope and drive bounds before Core sees it.** `docs/workstreams/rust-foundation/t4/design.md:948` checks denied and declined entries as they arrive, but measures final text only at terminal. The Codex mapping accumulates agent-message text by item ID (`docs/workstreams/rust-foundation/t4/design.md:1923`); successive bounded vendor messages can therefore build more than the 1 MiB text allowance assumed in `docs/workstreams/rust-foundation/t4/design.md:785`. **Smallest fix:** meter every text append against encoded envelope capacity and order the overflow stop at that append.

5. **`logs` may silently lose known raw evidence.** `docs/workstreams/rust-foundation/t4/design.md:614` delays span commits and drops a closed span under pressure. The units remain on disk but can never be returned by `logs`; `spans_incomplete` is absent from its result. This defeats R5’s post-mortem read without telling the caller. **Smallest fix:** retain and batch bounded attribution until committed, or make attribution failure an explicit connection/raw-log failure visible to callers. Do not silently drop a known span.

6. **A34 is not integrated with stop-order precedence.** `docs/workstreams/rust-foundation/t4/design.md:1970` adds `overflow` and says it replaces cancel/close/idle, while `docs/workstreams/rust-foundation/t3/design.md:165` has no `close_by` rule for it and `docs/workstreams/rust-foundation/t3/design.md:185` retains the first cause except `store`. C1’s first-match disposition table can still select a later vendor completion before overflow (`docs/specs/via-api-v1.md:643`). **Smallest fix:** specify `close_by`, cause upgrade and disposition precedence together in A34 and amend C1 §7.6. Using the existing stop-order mechanism with one added cause is otherwise reasonable.

7. **The overflow-tool rule can omit a model step.** `docs/workstreams/rust-foundation/t4/design.md:345` does not retain IDs beyond 64; `docs/workstreams/rust-foundation/t4/design.md:348` ignores their completions. If an untracked tool’s result is the one followed by model output, `results_since_output` remains false: `current_step` does not advance and no row is written. `running_tools_more` can also keep `phase=tools` after all tools finish. **Smallest fix:** retain bounded correlation sufficient to recognize those completions, or explicitly fail progress tracking at the limit; an unqualified “running now” claim cannot use an uncorrectable unknown count.

8. **The step-row guarantee excludes a normal terminal path.** `docs/workstreams/rust-foundation/t4/design.md:500` limits completeness to drive-written terminals. `docs/workstreams/rust-foundation/t3/design.md:1348` also writes forced terminals in final shutdown, and round 6 does not say how their open step row is included. The exception for recovery or an uncertain write is legitimate; the omitted forced path is not. **Smallest fix:** carry the available `TurnRecord` row into final shutdown’s forced terminal, and state the uncertainty exception only where an earlier row outcome is unknowable.

## Important

- **A resumable cursor is undefined before the first raw byte.** `docs/workstreams/rust-foundation/t4/design.md:638` returns the request cursor when nothing is durable. An initial request has no cursor, yet `null` is reserved for a sealed end. **Smallest fix:** define an initial sentinel cursor and its transition to the first connection.

- **Span validity is not enforced at the Store boundary.** `docs/workstreams/rust-foundation/t4/design.md:1066` keys spans by start offset but specifies no non-overlap, raw-unit-boundary, or `(session_id, turn)` ownership validation. Overlapping rows can expose the same shared raw bytes to two sessions. **Smallest fix:** validate those conditions in `CommitSpans` before the transaction commits.

- **The failure-summary proof relies on a test instead of a bound.** `docs/workstreams/rust-foundation/t4/design.md:964` allocates 8 KiB to core fields including `failure.message` and says an excess “cannot occur (measured in tests).” That member has no stated cap or excess rule, so “every member bounded” is unproved. **Smallest fix:** cap the encoded message within this group, record its truncation, and measure the group before allocation.

- **The amendment re-grep missed restatements.** A29 leaves C2’s summary saying observations are “C1 events minus Core fields” (`docs/specs/adapter-contract.md:40`), although `progress` is now an observation and several former events are not. A33’s Codex late-detail replacement leaves the `codex_two_threads` test expecting a late tool completion to retain `late:true` as an observation (`docs/specs/vendors/codex.md:395`). **Smallest fix:** amend both lines to name progress and raw-only late tool completion accurately. A34 must also amend C1 §7.6 as above.

- **The A26 wording contradicts §3.2.** `docs/workstreams/rust-foundation/t4/design.md:1692` says a terminal “by Store-failure resolution” makes no completeness claim; `docs/workstreams/rust-foundation/t4/design.md:481` explicitly includes the drive’s known-failure resolution in its guarantee. **Smallest fix:** distinguish that drive transaction from the later Latch batch after uncertainty.

- **Token accuracy remains an owner decision.** `docs/workstreams/rust-foundation/t4/design.md:415` identifies the missing vendor probes honestly. It does not yet establish R3’s approximately 95% accuracy for Claude, Codex or OpenCode. **Smallest fix:** retain Q-R5-11 as an explicit owner gate before claiming R3 conformance for those routes.

## Deviations and simplicity

The two buffer forms are justified: 32 sockets cannot each reserve a 16 MiB line. The byte scanner and borrowed `JsonStr` are plausible ways to avoid serde’s hidden copies, but `JsonStr` must be charged by capacity. A35’s 4 KiB escaped-key restriction is **not required by R1–R7**; it is a new caller/vendor limit awaiting the stated owner decision. A simpler contract is possible only if the decoder avoids passing arbitrary escaped keys through serde’s growing scratch, so the current restriction is defensible once approved.

The narrower row guarantee is necessary for crash recovery and uncertain commits, but its final-shutdown gap must be closed. A 128 KiB `final_text` prefix is optional: an empty string plus truncation metadata would make the overflow summary simpler. The `SpanWriter` drop rule is unsound; a bounded batch with explicit failure is simpler than a silent-loss flag. One synchronized disk-quota ledger is also simpler than three independently checked atomic counters.

**Could not verify:** implementation or compilation, allocator/RSS peaks, SQLite page-growth bounds, crash and failpoint behavior, or live vendor step/token mappings. This was a read-only design review; no files changed, and the worktree remained clean at `62f17c5`.

