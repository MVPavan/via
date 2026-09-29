UNSOUND

The redesign removes the follower and outbox machinery, so the round-4 permit-transfer finding is resolved. The round-4 memory and bounded-summary findings remain open, and several new paths do not meet R1–R7.

### Blockers

1. `docs/workstreams/rust-foundation/t4/design.md:419` — Stopping step-row writes at 10,000 silently narrows R4’s “one row when a step ends.” The warning does not preserve that history. **Smallest fix:** write every completed step under an aggregate disk quota, or obtain an explicit change to R4.

2. `docs/workstreams/rust-foundation/t4/design.md:409` — The claim that committed `turn.ended` implies every step row is false after a known `CommitSteps` refusal: the design stops the turn as `failed(store)`, while T3 permits a terminal resolution write without the refused row. **Smallest fix:** carry known-uncommitted rows into that terminal transaction; if they cannot commit, do not claim a complete durable step history.

3. `docs/workstreams/rust-foundation/t4/design.md:583` — The 128 MiB proof charges appended byte lengths, but the growing stdout and C1 `Vec` buffers can retain capacity beyond their lengths and briefly hold old and new allocations during growth. Those coexisting copies are absent from §5.4. Round-4 F1 is therefore not fixed. **Smallest fix:** charge capacity before growth, including the old allocation until freed, or use fixed charged segments.

4. `docs/workstreams/rust-foundation/t4/design.md:657` — R7 retains disk bounds, but the design explicitly leaves raw logs without an aggregate bound; a wall deadline does not bound output bytes. Runtime §6 already specifies a 4 GiB logical quota. **Smallest fix:** include the quota and its lifecycle reserve in Task 4, covering raw and blob writes.

5. `docs/workstreams/rust-foundation/t4/design.md:513` — `logs` is restricted to private, per-turn connections. That leaves the approved R5 method undefined for first-release Codex shared connections and OpenCode multi-turn connections. Deferring attribution to later slices does not define the contract those slices must implement. **Smallest fix:** specify bounded session/turn attribution for raw units now, excluding unowned shared traffic.

6. `docs/workstreams/rust-foundation/t4/design.md:238` — The 10-second stall makes Route fail its “connection,” but `docs/specs/adapter-contract.md:311` requires Codex’s shared route to quarantine the affected thread while other threads continue. The shared-server outcome is unspecified. **Smallest fix:** define a per-thread stall/overflow path for shared routes and retain connection failure for private routes.

7. `docs/workstreams/rust-foundation/t4/design.md:252` — The extra 1 MiB *event-payload* cap can fail a turn whose envelope fits, narrowing R1/R6. Closing `observed_rx` also does not guarantee failure when the adapter sends nothing else. **Smallest fix:** bound actual envelope accumulation and disk use separately; on an overrun, record the failure and order cleanup immediately.

8. `docs/workstreams/rust-foundation/t4/design.md:798` — The “under 64 KiB” failure-summary proof overlooks unchanged caller/vendor-derived members such as `bound` and `vendor_options` in `docs/specs/via-api-v1.md:483`. Clearing denied and declined lists also conflicts with R6’s stated envelope content. Round-4 F2 is not proved fixed. **Smallest fix:** construct and measure an independently bounded summary for *every* retained member, and resolve any R6 exception explicitly.

### Important

9. `docs/workstreams/rust-foundation/t4/design.md:330` — Beyond 64 open tools, `running_tools` becomes a subset and an unmatched or duplicate end can decrement `untracked`, making `phase` falsely report `model`. R3 asks for tools running now. **Smallest fix:** keep bounded correlation that preserves phase, or fail explicitly at the limit and seek approval for that limit.

10. `docs/workstreams/rust-foundation/t4/design.md:312` — R3’s roughly 95% live-token accuracy is unsupported: Claude per-message usage is unprobed, Codex’s `last` interval is unverified, and the design permits `null` after steps. The author’s §9.4 concern is valid, but labelling scope does not satisfy R3. **Smallest fix:** qualify the per-step samples against vendor evidence or obtain an explicit accuracy/availability exception.

11. `docs/workstreams/rust-foundation/t4/design.md:1466` — The amendment audit misses stale current-spec text: C1 still calls event `raw_ref`s authoritative for extraction in `docs/specs/via-api-v1.md:508`; the `docs/specs/vendors/claude-code.md:59` and `docs/specs/vendors/opencode.md:461` C1 method tables still offer `unsubscribe`; `docs/specs/vendors/codex.md:226` still describes late detail observations reaching an event sink. **Smallest fix:** add precise replacements for these restatements, distinguishing Codex’s vendor `thread/unsubscribe` from removed C1 `unsubscribe`.

12. `docs/workstreams/rust-foundation/t4/design.md:213` — An empty C2 observation for every unknown message recreates a per-message queue that R2 no longer needs and can stall on noise. An untagged shared-server message also has no turn ID with which to update a turn snapshot. **Smallest fix:** update a bounded last-activity value only for attributed messages; send C2 observations only for lifecycle, safety, or meaningful progress marks.

13. `docs/workstreams/rust-foundation/t4/design.md:535` — Returning `next_cursor: null` at a *running* connection’s current end leaves a polling caller no opaque position from which to request later bytes. **Smallest fix:** return a resumable end cursor while the connection can grow; use `null` only at a sealed end.

### Minor

14. `docs/workstreams/rust-foundation/t4/design.md:1737` — The proposed “status reads memory while Store is held” scenario cannot receive a full `status` reply: §4.2 requires a Public Store read for durable members and steps. **Smallest fix:** test that progress construction adds no *extra* Store read, and separately test status latency under a bounded Store delay.

The author is right that R2 omits necessary correlation and safety fields, R4’s crash wording exceeds what an in-flight commit guarantees, and R6 conflicts with the old transaction cap. Those observations call for explicit requirement or spec amendments; they do not authorize the R4 cap or R5/R7 deferrals.

**Could not verify:** proposed code compiles; allocator and RSS ceilings; terminal-size measurements; live vendor step and usage mappings; crash/failpoint behavior. This was a read-only review. The worktree remained clean at `ba7c8b2`.

