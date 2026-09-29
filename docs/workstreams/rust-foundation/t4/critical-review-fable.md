# Critical review of VIA Task 4 design (round 15, `wt/t4-0` @ 778752c)

Files (repo-relative on `wt/t4-0`; cited below by short name):
- D = `docs/workstreams/rust-foundation/t4/design.md`
- R = `docs/workstreams/rust-foundation/t4/requirements.md`
- T = `docs/workstreams/rust-foundation/t4/reports/T4-0.md`
- C1 = `docs/specs/via-api-v1.md`; RT = `docs/specs/runtime-contracts.md`
- Code under `crates/`

Legend: [V] verified at the cited line; [I] inference.

## 1. Effectiveness

The design delivers R1–R7 as written. The durable set (D:111-117) matches R1 (R:25-31) exactly; the step rule (D:257-274) is one reducer fed by vendor marks, which is the only way R3's "derived the same way for every vendor" (R:52) can hold; the `steps` row and table (D:331-341) match R4 (R:62-64); `wait`/`status`/`events`/`logs` (D:401-479) match R5. [V]

What a program caller still misses:

1. **No link from a step to its raw bytes.** Step rows carry times only (D:331-341); raw units carry no time and `logs` pages only by connection byte offset (D:464-470). A post-mortem of "step 7" must page the whole connection at 128 KiB per call (D:475-477). [V] Fix: the `progress` item that closes a step already comes from a message with a `raw_ref` (adapters carry it: `crates/via-adapters/src/runtime.rs:335` [V]); add `raw_end INTEGER` to the row. Cost: one column beyond R4's shape, an owner change to R4.
2. **Two step counts.** Envelope `steps` is the vendor's count when reported (Claude `num_turns`), else VIA's (D:284-286, A24 at D:1229); `progress.current_step` is always VIA's. Claude's `num_turns` is known to disagree with limits (`docs/specs/vendors/claude-code.md:114`, "observed `num_turns:2` when limit was 1" [V]). A caller comparing the two will see a mismatch with no label. Fix: envelope `steps: {count, source}` or a second member; cost: one C1 field.
3. **`wait` is a 20 ms Store poll per waiter** (D:403-405; today `crates/via-core/src/engine/read.rs:99` [V]) on the Public lane, which has 32 slots and refuses when full with `admission_refused` (D:626-633). An orchestrator with 40 background turns and 40 `wait`s can be refused for polling, and every poll is a read serialized on the single SQLite thread that also fsyncs each commit (`synchronous=FULL`, `crates/via-store/src/runtime/sql.rs:140` [V]). [I] Fix: a `Notify` in the existing `Slot` fired at `finish_running` (`crates/via-core/src/engine/queue.rs:593` [V]) plus one read; or at least make `wait` treat `NotEnqueued` as "retry next tick", never a caller error.
4. Snapshot has no `step_started_at`; derivable from the last row's `ended_at` (D:273-274) but only with a second call. Minor.

## 2. Simplicity

The document is 1,907 lines for a task whose requirements are 148 lines. About half is not R1–R7 work but the t0 inventory (C1 conformance, F5 sockets, blob path, `list` paging, spawn members, schema v6 columns: D:1068-1176, D:897-926). That is legitimate scope per `t0.md:36-52`, but it makes one design carry two tasks. Cuts and merges, each with cost:

- **A34 `StopCause::Overflow` (D:1692-1725; T3 amendments in four places).** Core can instead drop its observation receiver on the crossing item; `deliver` then returns `Err` (`crates/via-adapters/src/runtime.rs:326-328` [V]), the Adapter drops the hop (`:146-153` [V]), Route fails `Overflow`, Core disposes `failed(overflow)` (`crates/via-core/src/engine/terminal.rs:97` [V]). Same path as the stall. Cost: force-close instead of a 3 s graceful stop for an already-broken turn; no later vendor terminal is observed at all, which is what A34's precedence rule wants anyway. Removes a T3 amendment, a C1 §7.6 row and a coalescing rank.
- **Keyed, replaceable `final_text` segments (D:178, D:720-729, Codex at D:1633-1641).** Send final text once, at completion, per vendor (Codex `item/completed` text; Claude `result`; OpenCode completed message). Cost: a Codex turn that fails mid-message loses partial final text from the envelope (it is in the raw log). Removes `replace`, key segments and the "many keys" overflow case.
- **`while_polling` combinator (D:1062-1066).** Spawning the adapter future into the drive's `JoinSet` (A13 primitives, D:50-53) is the standard tokio answer and keeps the Adapter polled during every commit. Cost: one more owned task per drive. [I]
- **Config validation minimums (D:576-598).** They encode the design's internal arithmetic; every class change must be mirrored in a rule. Keep ranges, the pool aggregate and the terminal reserve; drop per-class minimums, which the counting-allocator test already checks. Cost: an operator can set a charge too small and see refusals rather than a start error.
- **Speculative keys/constants:** `memory.codex_shared` in S1 config (D:529, no Codex in S1), `HISTORY_PRUNED` (D:492; `earliest_seq` is always 1, D:435). Cut until their tasks. Cost: none now.
- **Merge reply classes** `reply_small/reply_page/reply_logs` (D:528) into one 2 MiB `reply`. Cost: at the floor, fewer concurrent large replies.

Kept rightly: `connections` table (OpenCode will need it), lanes (A2), blob path (R7), JSON pre-scan (A10 contract).

## 3. Adherence

- **Layer drift, disclosed but real.** `MemoryBudget` (D:515), `json_limits` (D:1091-1093, "so via-routes and via-core need no new edge") and `RawFaultSink` (D:677-679) live in via-store because it is the lowest crate (`scripts/check-layers.py:11-20` [V]), not because Store owns them. RT §2 (RT:53-59) says Store defines storage DTOs and durable IDs. This turns via-store into a utility crate; an owner call is needed between accepting that and adding a leaf `via-bounds` crate to `ALLOWED` (no external dependency, but a graph change).
- **WAL health latch.** At `wal.max`, a WAL that cannot be truncated "fails Store health (runtime §7 latch)" (D:879-882), and the test expects a reader to trigger it (D:1866). The latch is O1 for uncertain outcomes (D:33-34); an oversized WAL is a known, recoverable state, and a developer opening the DB with `sqlite3` would latch every turn `store`. The contract text is inherited (RT:769-771 [V]), so this is a flag, not a defect: recommend a runtime §6 amendment making it a Quota refusal of ordinary writes with retried checkpoints.
- Owner decisions are honoured: coarse bounds (D:505-544), config read at start (D:554-556), one-transaction overshoot (D:832-834, D:883-884), `logs` per-turn only (D:443-447). [V] Invariants 5, 6, 10, 11 untouched. Terms updated by A24 (D:1234-1252). No silent narrowing beyond disclosed items (D:1897-1907); `tokens` will be `null` for every real vendor until probes (D:305-306), which the owner should read as "R3 ships for the fake only".

## 4. Risk and slice order

1. **Raw offsets/`high_water`** (D:690-699, D:1016-1030): four rounds to converge, per-connection mutex plus `try_send`, `durable_end` raised before acks. Highest concurrency risk; build first with test D:1867.
2. **Store `Lanes` rewrite** replacing `sync_channel(128)` (`crates/via-store/src/runtime.rs:1021` [V]) with Mutex+Condvar, fence, `DeadGuard` on unwind (D:618-664). Crash-recovery critical; second.
3. **Disk budget inside the transaction** (`page_count` before `COMMIT`, `SQLITE_FULL` rollback, D:842-860): SQLite behaviours not yet probed on the bundled 3.53.2; third, with D:1866.
4. **Route/Adapter/Core select discipline** (D:1049-1066) and the stall; fourth.
5. Vendor assumptions [U] (D:301-303): Claude per-message `usage`, Codex `last` = one call, OpenCode part names. Retire in vendor slices, not here.
6. **Test feasibility:** 20,000-step test (D:1859) is 20,000 fsyncs under `synchronous=FULL`, minutes on WSL [I]; move under `test-failpoints` or cut to 2,000. "No reallocation on paths at their maxima" (D:1874) is brittle across allocator/serde versions; assert "at most the charge" only. RSS gate (D:1869) needs CI variance margins.

## 5. Owner questions (latest list, T:1409-1411)

| Q | Recommend | Reason |
|---|---|---|
| Q-R5-1 (terminal excluded from 1 MiB cap) | Accept | R6 fixes the envelope at 1 MiB; 1.3 MiB terminals fit the 2 MiB lanes (D:717-719) |
| Q-R5-2 (`id` ≤ 256 B) | Accept | Makes every page bound a constant |
| Q-R5-4 (refused row fails `store`, rows ride in terminal) | Accept | R4 forbids silent loss; the alternative is a lost row |
| Q-R5-5 (replace F25/F26) | Accept | Follow is gone; the replacements test what R3/R4 promise |
| Q-R5-7 (fake `usage` message) | Accept | Only way to test R3 tokens without a vendor |
| Q-R5-8 (16 MiB request ≈ 66 MiB charge) | Accept, note limitation | Real prompts are far smaller; revisit with measurements (D:1899) |
| Q-R5-9 (short field > 1 KiB is `protocol`) | Accept | Truncating vendor evidence misreports it |
| Q-R5-10 (reply within 10 s) | Accept | Bounds the only per-socket memory a peer can hold |
| Q-R5-11 (tokens gate) | Accept, and state it in R3 | Accuracy is unprobed for all three vendors; be explicit that R3 ships for the fake |
| Q-R5-13 (A34 `StopCause::Overflow`) | Decline; use receiver drop (§2 above) | Existing Overflow path gives the same result with no T3 amendment |
| Q-R5-14 (summary budgets, `truncation`) | Accept | Bounded, exact totals; needed once 1 MiB is fixed |
| Q-R5-15 (R2/R4/R6 text edits) | Accept all three | Each is a factual correction (T:622-639) |
| Q-R6-1 (`final_text` pieces ≤ 256 KiB) | Accept the pieces; drop `replace`/keys | Simplification in §2 |
| Q-R7-1 (1 GiB/3 GiB, 64/256 MiB headroom, 32 MiB WAL) | Accept as provisional | They are config; `via-d9o.2.3` tunes |
| Q-R7-2 (class charges; connection 16 + drive 8) | Accept as provisional | Same |
| Q-R7-3 (SQLite cache inside the pool) | Confirm | One pool is the owner's strategy; RSS gate sees the cache anyway |
| Q-R8-1 (key set and minimums) | Accept keys; trim minimums | Per §2, keep ranges, pool aggregate, terminal reserve |
| Q-R8-2 (exit 78, message) | Accept | EX_CONFIG is conventional |
| Q-R8-3 (over-ceiling store refuses start) | Accept refuse-start | Read-only mode adds a state; the operator raises the budget |
| Q-R8-4 (`limits` in `daemon/status`) | Accept | Programs need the effective values to reason about refusals |
| Q-R9-1 (WAL overshoot, ~73 MiB unproved) | Accept as gate; also amend the health-latch rule (§3) | Measurement belongs to `via-d9o.2.3`; the latch is the real risk |
| Q-R9-2 (`codex_shared` not in the floor) | Defer to the Codex task | Not in S1 |

## 6. Verdict

**Ready after named small changes:** (a) `wait` must not be refusable by lane pressure (Notify or retry); (b) decide the via-store utility-crate question; (c) WAL over-limit as refusal, not latch (runtime §6 amendment); (d) drop A34 in favour of receiver drop and drop keyed/replace final text; (e) heavy tests behind `test-failpoints`, allocator test asserts "≤ charge". Optional for R4: `raw_end` on step rows. Then plan slices in the order of §4, and split the t0 conformance half (D:1068-1176, D:897-926) from the R1–R7 half.

## Not checked

No `cargo` run; no vendor probes; Sol's r15 review is not in this worktree (only up to r14). I did not re-verify every [V] in the design, only the ~20 cited above; I did not audit the A24–A37 amendment restatement lists against the specs, the `list` proof (D:908-926), or the §6.2 lifecycle slot count.
