# T4-0 design review round 2: Claude Opus 5.5 (high)

First-principles opinion (effectiveness, simplicity, fit with the project) on `wt/t4-0` at `bb54709`, requested by the owner. The transcript metadata shows effort `high`; the agent's own "low effort" line was a misreport. Verbatim, local paths converted to repo paths.

# T4-0 design, round 2: independent opinion (Claude Opus 5.5, low effort)

Reviewed `wt/t4-0` at `bb54709`: `docs/workstreams/rust-foundation/t4/design.md`
(2,243 lines) and `reports/T4-0.md`, read against decisions 1-20 and the round-1
reviews. The review is read-only. I did not run cargo. The three axes asked for
are effectiveness, simplicity and adherence. I leave a line-by-line defect hunt
to Astra and Sol.

Tags: **[V]** means I read the cited line. **[I]** means inference that I did
not verify in code.

## Verdict

**Targeted revision is needed, not a rethink.** The design is effective on the
contract text it read. It maps every retained memory form, names a single owner
for each mechanism, keeps T3's lock order and adds no new dependency. It also
reuses the existing stop-order path and the existing `Unresolved` owner instead
of inventing parallel ones. Those are the right instincts.

It has three problems:

- It does not plan the **Task 4 exit evidence** that S1 closure depends on.
- Its most novel mechanisms (large-prompt ingestion, the blob thread and the
  `list` catch-up) are more complex than the contracts require. One of them
  does not terminate in ordinary use.
- The slice cut puts Store producers far from their consumers. The riskiest
  work (blob, schema v6 and four read APIs) lands in one Sonnet slice, and the
  blob path is only tested end to end in the last slice.

## 1. Effectiveness: what is closed and what only appears closed

Closed with convincing evidence: C2 A1 (256 KiB rule, 1024/4 MiB, stall
through turn control), runtime §4's reader split and lifetime, lanes 64 + 8,
raw staging overflow, group commit, `logs` isolation and bounded lookup,
`status` durable sources, session counts, F25/F26 protocol and F5. The §2.4
per-form table and the rule "no F24 claim until every row is charged" (§2.2.6,
§10.4) are the right discipline for F24.

These requirements only appear closed:

1. **Task 4's own exit criteria are absent.** `s1-plan.md` §4 Task 4 says:
   "every F1–F30 scenario has an artifact, and the normal default suite meets
   coding-style's speed budget" [V]. The design never mentions artifacts, the
   2-minute budget (coding-style §10) or which gate (default or failpoint) each
   heavy test runs in [V: no match for "artifact" or "speed" in design.md]. The
   evidence gap is real:
   - `Evidence::new` has 23 call sites across roughly 180 CLI tests [V: grep].
   - F15, F16 and F18 have no `s1_fNN_` scenario [V: grep; F18 is noted in
     report §1.2, "not in Task 4's list"].
   - Task 4 is the last task before the S1 close review, so this plan cannot
     be deferred.
   - Two interactions need stating as well. F16's artifact scan ("handle
     appears nowhere … Store dump") must now cover `blobs/`, because retry
     identity blobs are params bytes. The heavy new scenarios (a 256 MiB flood,
     100,000-unit logs, 16 MiB prompts, 32 sockets) each need a gate placement.
2. **F24 is only partly specified.** Runtime §8 requires more than
   daemon RSS:
   - "Anchor RSS is reported separately from the daemon and vendor in F24;
     combined daemon plus four anchors must remain below 384 MiB"
     (`runtime-contracts.md:1050-1056` [V]). S1 spawns real anchors
     (`crates/via-host/src/anchor.rs`, `engine.rs:284` [V]). §10.4 asserts
     daemon RSS only.
   - "F24 must measure control response scheduling within 100 ms"
     (`:1049` [V]). §10.4 measures that with `daemon/status`, which is
     memory-only (§8.2) and so never touches turn control. A cancel
     acknowledgement on the flooding or blocked turn, timed within 100 ms, is
     the real measurement [I: my reading of "control"].
3. **The `list` cursor meets C1's guarantee but does not terminate in normal
   use (§6.1, Q18).** Phase 2 chases `stamp > s_cursor` with no upper bound,
   and every event commit bumps the session's stamp (§3.7). So any running
   turn that streams events reappears on every phase-2 page. A client that
   pages until `next_cursor` is null does not finish while a turn runs.
   Q18 calls this "an update rate that outpaces paging", but a normal turn
   produces exactly that rate. See §2, S1 for a fix that terminates and keeps
   the guarantee.
4. **Follow from `after=0` on a long session will almost always lag.** This
   follows directly from decision 2 (the follower pushes pages at SQLite speed
   into a 1000-event outbox, §6.3 step 4, §6.4). It is settled, so it is not a
   defect. It becomes one if `via events --follow` does not page first and
   resume on `lagged`. §7.3 treats the CLI verbs as "thin". State the CLI's
   catch-up policy: page with `events` until `more=false`, then follow, and
   re-request on `lagged`.

## 2. Simplicity: where the design exceeds the contracts

Each row gives the simpler alternative that still meets the contract, and what
it costs.

| # | Design mechanism | Simpler alternative | Cost |
|---|---|---|---|
| S1 | `list` two-phase catch-up that chases stamps (A12, §6.1) | **(a)** Keep `stamp`, but make phase 2 an **id-ordered, windowed scan** filtered by `stamp > v0`. Scan at most N rows by `id` per page; the cursor is the last id scanned, as in `events_page`. A session updated after the first request has `stamp > v0` permanently and a fixed `id`, so the keyset passes it exactly once. The scan always terminates and Q18 disappears. **(b)** Or amend C1 §3.10 to a snapshot guarantee (updates during paging may be skipped; re-list). That removes `stamp`, its index, the per-transaction `MAX`, and phase 2. | (a) Phase 2 costs `sessions/N` pages even when few rows match. (b) A weaker C1 promise; `list` is a discovery surface, and followers use `events`. |
| S2 | Large-prompt ingestion (§3.6, §7.2): spans of string tokens over 1 MiB are recorded in a hand-written pre-pass; an incremental JSON unescape streams them into 64 KiB blob chunks; a placeholder is spliced into the line before the typed decode; and prompts between 256 KiB and 1 MiB take a second path. | **One path.** Stream the retry identity to its blob from **raw line slices**: `line[..h]`, the hash, `line[h_end..]`. This needs no unescape, because identity bytes are raw params bytes (`retry_identity`, `api.rs:808-858` [V]). Then decode with serde as today, charge the decoded prompt copy to the **global** pool (not the 32 MiB input-buffer class), drop the line, and stage the prompt blob from the `String`. Runtime §8 requires "persisted from the bounded request buffer" and forbids a second copy only for the *outbound* start frame [V `runtime-contracts.md:1029-1040`]. | A transient ~16 MiB second copy for a maximal prompt, inside the 128 MiB global budget. It removes the riskiest custom code in the design: surrogate and escape handling, span bookkeeping, and the ordering of placeholder, identity and decode. |
| S3 | Third Store thread `via-store-blob`, with `sync_channel(4)`, its own fence and death handling, and a `blob` semaphore class (§3.6, §2.3) | Put `BlobChunk` and `BlobFinish` on the existing **raw worker** (`RawInbox`). It already does private-directory writes, `sync_data`, and the §3.3 fence and death protocol. Recovery's blob verification runs inline before admission. In any case, drop the `blob` class semaphore: the design itself says the channel capacity equals the permit count, so only the global charge is needed. | Blob writes share raw-worker throughput. A 16 MiB blob delays raw appends by a few chunk writes, and staging (32 MiB, nonblocking) absorbs that. |
| S4 | `shape.rs`, a hand-written byte scanner for depth, nodes, string bytes and spans (§7.2, §2.4) | A serde `DeserializeSeed`/`Visitor` that counts depth and nodes over `serde_json`'s own lexer, the pattern already used in `retry_identity` (`api.rs:808-858` [V]). `raw_value` is already enabled (`Cargo.toml:16` [V]). With S2 adopted, spans are no longer needed. | [I] `serde_json` may buffer an escaped string in its scratch `Vec` during the pass. Either bound it (the line is already charged) or charge it. The benefit is a smaller correctness surface: no hand-written JSON tokenizer shared by the vendor and C1 paths. |
| S5 | Route's own 10 s wait on `decoded` permits (§2.3), a second 10 s timer beside the Adapter's stall timer (§5.2) | Make the `decoded` wait end only on control arms (stop, force, health and deadline). `decoded` permits are released when the Adapter takes observation permits, so Route waits only while the Adapter is blocked, and then the stall timer is already running and delivers the stop order. That leaves one owner for the "10 s without drain" rule. [I] | None if the reasoning holds. The slice should confirm there is no path where Route waits while the Adapter is not blocked. |
| S6 | Public lane byte cap of 3 MiB (§3.1, §3.5) | Drop it. The design also caps each Public request at 64 KiB, so 32 slots × 64 KiB = 2 MiB, which is below 3 MiB (it says so itself at §3.5). Internal keeps at least 4 MiB automatically. | None; the cap can never be reached. |
| S7 | Galloping search from the previous hit in `logs_page` (§3.4) | Plain binary search. The bound is already met: about 17 index reads per lookup at 100,000 units. | A constant factor on a bounded, rare path. |
| S8 | Recovery recomputes `ended_seq` when it is NULL, plus a test that clears the column (A15, §3.7) | `CHECK(state NOT IN (terminal states) OR ended_seq IS NOT NULL)`. The design itself says the column and its event commit in one transaction, so NULL cannot happen. | None. It deletes speculative code and a test. |
| S9 | `connections.state IN ('open','sealed','incomplete')` together with a nullable `high_water` (§3.7) | Two orthogonal facts: `high_water` (NULL means open) and `incomplete` (0/1). The enum as written makes "incomplete, then sealed at terminal" overwrite one fact with the other. Also note that no S1 reader uses `high_water`; it is built only because the target lists it. | None. |
| S10 | Follower rescan refused: back off from 50 ms to 1 s, then `lagged` after 2 s (§6.3) | End `lagged` on the first refusal. That is C1's documented re-request path, and a refused read means the Public lane is saturated. | More lags under overload. There is no timer state, and the contract's silence is not filled with invented policy. |
| S11 | `WireHealth` as a derived view plus a `health()` accessor (§4.3, A20) | Build no accessor without a reader. `WireHealth` is declared and unused today (`via-wire/src/lib.rs:102` [V]), and the latch plus `subscribe_failure()` serve every consumer the design names. | None. |
| S12 | Two copies of the fence and death queue protocol (`Lanes`, `RawInbox`, §3.3-§3.4) | One small fenced-queue core (`Mutex` + `Condvar` + fence + `dead` + `mem::take` on death), used by both. The design says "the same fence and death protocol", so implement it once. | None. |

Places that are right to be complex, and that I would not simplify:

- The three-part connection task (§6.4). F25 needs a lag notice while the
  handler waits.
- The `while_polling` combinator (§5.3). Core's `select!` really does run
  commits to completion (`drive.rs:1292-1314`, cited [V] by the design).
- The per-session `Head` version (§6.2). I considered a daemon-wide commit
  watch, which would remove the `Lease`, get-or-create, `sweep_needed` and the
  receipt race. It wakes every follower on every commit, and 32 followers
  rescanning would fill the 32-slot Public lane, so the design's choice is
  better.
- The 4-piece unacknowledged stdin window (§4.6). It is needed, because a
  16 MiB start frame would otherwise exceed the 8 MiB per-connection staging
  limit.
- The Lifecycle sufficiency proof (§3.2). I checked that `finish`
  (`drive.rs:1000`) is called only from `stop.rs:299` [V], so the design's
  claim of a single sequential issuer holds.

## 3. Adherence to the project

These parts follow the project well:

- A single owner per state, with create, write and end named (§1 table).
- The existing mechanisms carry the new ones: the stop order for stall,
  `Unresolved` for the active count, `Head` for the wake, and `retire` for
  cleanup.
- No new dependency, no debug RPC, no CLI verb outside C1.
- Test seams follow the existing `test-failpoints` and env-override pattern.
- The no-sleep ordering rule (§10.2) matches coding-style §10.
- Amendments carry restatement lists.

These parts deviate:

- **Existing code and the standard library first.** Three places fall short:
  a custom JSON tokenizer (S4) where the codebase already has a serde-visitor
  span tracker; a custom incremental unescaper (S2); and a third Store thread
  (S3). Coding-style §5 says "other blocking calls use `spawn_blocking`", and
  the raw worker already exists.
- **No speculative features.** These do not meet that rule: galloping (S7),
  the `ended_seq` recompute (S8), the backoff policy (S10), a `health()` view
  with no consumer (S11), and the dead Public byte cap (S6). None is large;
  together they add implementer surface with no test value.
- **Scope.** Building the `sessions` timestamps and label (targets of
  `via-jm4.7.7`, `runtime-contracts.md:704` [V]) is justified, because C1
  `status` and `list` need them. The design takes no other target it does not
  need. Deferring `output_schema` and the Store watchdog is correct and has
  revisit conditions.
- **Settled decisions.** Nothing is reopened without a decision behind it.
  A1, A2, A12 and A19 amend T3 or C1 text, but each traces to a decision.
  A12's C1 amendment is the one I would revisit (§2, S1).
- **Documentation.** The design interleaves round history with the spec: 29
  mentions of "round 1" or "withdrawn" and 105 `[t4r1.N]` tags [V: grep]. For a
  Sonnet implementer, text like "round 1 did X, now Y" invites misreading.
  AGENTS.md says to keep histories behind references. Move history to the
  report, and have the design state only the final mechanism.
- **Layering.** Putting `BytePool` and `shape.rs` in via-store because it is
  the lowest crate is acceptable, but the design should state that reason once
  and forbid Store-specific imports in those modules.

## 4. The five flagged narrowings

| Amendment | Judgement |
|---|---|
| **A14** (`cwd` and `allow_untested` as `params` keys) | **Justified, and not really a narrowing.** The runtime §6 target is "frozen instructions/cwd/allow_untested" on the `sessions` row; `sessions.params` is on that row and immutable. Keep it as a representation note. It is consistent that `harness` and `label` get columns, because `list` filters on them. |
| **A15** (`ended_seq` only; `queued_seq` is the first) | **Justified.** `queued_seq` really is the envelope's `first_seq` (`terminal.rs:82`, cited [V]). Replace the recovery recompute with a CHECK (S8). |
| **A17** (identity compared by length and SHA-256 above 256 KiB) | **Justified.** The exact bytes are still stored in the blob, as runtime §6 requires. Only the comparison method changes, and `handle_hash` already relies on SHA-256. Word it that way so it is not read as weakening "byte-identical". |
| **A18** (`INLINE_MAX` = 256 KiB) | **Justified, but it is not an amendment.** Runtime §8 sets a floor ("over 1 MiB use blobs"), not a ceiling, so a lower threshold conforms. The real reason should be stated: the retry identity *contains* the prompt, so prompt, identity and effective params in one command must fit the 1 MiB transaction cap. Record it as a choice in the contract's silence. |
| **A20** (`open_connection` returns `WireParts`; non-`Clone` `WireSender`; derived `WireHealth`) | **Partly justified.** A non-`Clone` sender is fine: nothing needs a second holder, and a clone would duplicate the `JoinSet` lifetime. Dropping `into_parts` is a needless deviation: keeping it costs one method and removes amendment text. Deriving `WireHealth` from the latch is right in principle, but build it only if something reads it (S11). |

## 5. Slice plan

The root problem is the rule "after S1b, `crates/via-store/**` is closed"
(§11). It forces every Store change into S1a and S1b. S1b then becomes the
kitchen sink:

- schema v6 and `connections`;
- the whole blob path, with its thread, recovery verification and sweep;
- four read APIs, including the `list` two-phase cursor, and `session_status`;
- the Core `Sessions` tally;
- the receipt R1 edit and the close, status, stop and drive hooks.

That is the largest and riskiest slice, given to one Sonnet worker, and it
builds read APIs without their C1 consumers.

Meanwhile the blob path is spread over four slices (S1b store and R1/H1; S2
`write_frame_stream` and `PromptSource`; S3 span extraction; S4 the 16 MiB
end-to-end test). Its first end-to-end test is therefore in the last slice.

A better cut keeps five slices but makes each one vertical:

1. **S1a Store concurrency** (unchanged): lanes, fence, death, `BytePool`,
   `RawInbox`, group commit, barrier, fault sink.
2. **S1b Persistence**: schema v6, `connections`, blob path (on the raw
   worker, S3), receipt staging (R1), dispatch load (H1), the tally.
3. **S2 Wire, Route, Adapter, observation** (as now): streamed start, **and
   the 16 MiB end-to-end spawn test**. Once S2 above is adopted, today's socket
   reader already accepts 16 MiB lines (`dispatch.rs:22` [V]) and serde decodes
   them, so the whole blob path can be tested here.
4. **S3 Read surface** (∥ S2): the **Store read queries** (`events_page`,
   `logs_page`, `list_page`, `session_status`), the C1 arms and DTOs,
   `parse_bounded`, and the **CLI verbs** `describe`, `status`, `list` and
   `models`. S2 edits no via-store file and not `main.rs`, so S3 can own both
   without conflict. Producers and consumers then have one owner and one
   end-to-end test.
5. **S4 Connection layer and follow** (smaller than now): the three-part
   connection task, sockets and input budget, oversize handling,
   `serve --stdio`, follow and `unsubscribe`, and the F24 claim.

For Sonnet workers, give each slice an **extracted, history-free spec** of its
sections rather than a pointer into 2,243 lines. S2 remains the concentration
of concurrency risk: readers, the latch, `finish`, forward arms, pending
delivery, `while_polling`, stall and the envelope bound. If reviews of S1a show
strain, split S2 into (a) the Wire split, readers, latch and `finish`, and
(b) the observation path, stall and `while_polling`. The dependency between
them is linear.

## 6. Highest-value changes, ranked

1. **Plan the Task 4 exit evidence:**
   - an artifact for every scenario F1–F30, including named F15, F16 and F18
     scenarios, with the F16 scan covering `blobs/`;
   - anchor RSS and the combined 384 MiB check in F24;
   - a 100 ms control-response measurement on turn control, not on
     `daemon/status`;
   - a gate placement for each heavy test, with the 2-minute default-suite
     budget.
2. **Make `list` terminate:** use an id-ordered, windowed phase 2 filtered by
   `stamp > v0`, or amend C1 §3.10 to a snapshot guarantee (S1).
3. **Simplify large-prompt ingestion and blob I/O.** Use one decode path with
   the transient copy charged to the global pool, stream the identity from raw
   line slices, and drop span extraction, the placeholder and the incremental
   unescaper. Run blob I/O on the raw worker; drop the blob thread and the
   `blob` class (S2, S3).
4. **Re-cut the slices vertically.** Move the Store read queries and CLI verbs
   into S3, and land the 16 MiB end-to-end test in S2 (§5).
5. **Delete machinery without a present requirement:** Route's own `decoded`
   10 s timer, the Public 3 MiB cap, galloping, the `ended_seq` recompute (use
   a CHECK), the follower backoff, and the `WireHealth` accessor. Keep
   `into_parts` (S5-S8, S10, S11).
6. **Replace the hand-written `shape.rs` tokenizer** with a serde counting
   visitor, the existing pattern. First verify the scratch-buffer behaviour
   (S4).
7. **Publish a clean final spec.** Strip the round history into the report,
   and give each Sonnet slice an extracted brief.

## What I verified and what I did not

- **Verified [V]:**
  - the design and contract lines cited above;
  - `Evidence::new` usage and the F-scenario names by grep;
  - that `WireHealth` is unused;
  - that `finish` has a single call site;
  - `MAX_LINE`;
  - `serde_json` `raw_value`;
  - the `retry_identity` visitor.
- **Inferred [I]:**
  - that the Route `decoded` wait occurs only while the Adapter is blocked;
  - `serde_json`'s scratch-buffer behaviour for the counting visitor;
  - that "control response" in runtime §8 means turn control.
- **Not done:** no cargo runs and no tests. I did not do a defect hunt of the
  concurrency protocols; Astra and Sol cover that.
