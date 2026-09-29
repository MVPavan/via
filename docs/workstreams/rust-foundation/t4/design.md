# Task 4 design: events, progress, storage and C1 conformance (round 5)

Status: normative design for Task 4 (Bead `via-jm4.7.8`, step T4-0), round 5.
It replaces rounds 1–4. It is written against the owner-approved
[requirements](requirements.md) (R1–R7), which are normative and override
earlier design assumptions and spec text; every such conflict is a numbered
amendment in §11. The round history, the removed/kept/changed table and the
disposition of the round-4 findings are in [reports/T4-0.md](reports/T4-0.md)
§9. This step writes no code and no tests. Slices are re-planned after the
owner reviews this design; `s1.md`–`s7.md` are superseded.

Sources: requirements R1–R7; C1 (`docs/specs/via-api-v1.md`); C2
(`docs/specs/adapter-contract.md`), A1 as approved; runtime
(`docs/specs/runtime-contracts.md`) §3.1, §4, §6, §7, §8, §9; the vendor
specs in `docs/specs/vendors/`; [dispatch design](../t2/dispatch-design.md);
[T3 design](../t3/design.md) and its A1–A23; `.repo-context/invariants.md`;
`.repo-context/CONTEXT.md` for terms.

Tags:

- **[V]** verified at the cited `file:line` of `wt/t4-0` (base `ea28956`), or
  at the cited dependency source (`serde_json` 1.0.151, `tokio` 1.53.1).
- **[I]** inferred; the implementing slice checks it first.
- **[S §n]** stated by the named vendor spec section.
- **[U]** vendor behaviour the vendor spec has not probed; the vendor slice
  pins it before relying on it.

Every mechanism names one owner: who creates it, who writes it, what ends it.
Every queue is bounded and says what happens when it is full.

## 0. Scope and fixed decisions

Fixed, not reopened:

- Runtime §7 with T3's amendments: a known Store outcome is scoped, an
  uncertain one latches (O1).
- Core owns every absolute deadline.
- C2 A1's numbers as approved: 1024 items and 4 MiB per session; a full
  channel blocks only the normalizer; 10 s without drain fails the turn
  `overflow` and interrupts it.
- No new dependency, no debug RPC, no CLI verb outside C1.
- The requirements R1–R7 (owner-approved 2026-09-29).

What the requirements change, in one paragraph: VIA no longer stores or
streams per-message detail. SQLite keeps lifecycle, control and safety events
plus the envelope (R1). Text, reasoning, tool and usage messages are read only
for a small in-memory progress snapshot (R3) and one `steps` row per model
step (R4); their exact bytes stay in the raw log (R2). Callers poll `status`,
block on `wait`, page `events`, and read raw excerpts with `logs` (R5). There
is no follow stream. Everything that existed to make every event durable,
ordered and followable is removed: follower tasks, the `Head` version wake,
outboxes, termination episodes, `unsubscribe`, `event_end`, the `outbox`
memory class and the stop-order delivery of the stall (A19).

Coordination primitives (A13, kept): `tokio::sync::{watch, Notify,
Semaphore, mpsc, oneshot}` and `JoinSet`. The owner of a task creates its stop
signal, owns its `JoinSet`, and stops, drains and joins it within a bound.
Dropping a handle is never the normal cancellation path. [V] `tokio-util`
and `proptest` are declared (`Cargo.toml:14`, `:28`) and used by no crate.

Out of scope, each with its revisit condition:

| Item | Why | Revisit when |
|---|---|---|
| `describe`/`models` for routes other than `fake` | one route in S1 (`crates/via-core/src/engine/receipt.rs:97` [V]) | the first real route |
| Durable `output_schema` state | the fake refuses it by name (`crates/via-core/src/api.rs:34` DTO member, refusal in `Named::fake`, `:464` [V]) | a route supports it |
| Store operation watchdog (runtime §8) | not a Task 4 item | the S1 close review |
| Blob-backed `effective` | the fake's `Effective` is a few hundred bytes (`api.rs:979` [V]) | a route accepts large `bound`/`vendor` values |
| Retention and the per-session delete | requirement non-goal; `via-jm4.18` | that task; §3.5 gives the keyed delete it uses |
| Runtime §6's 4 GiB logical disk quota | not a Task 4 item, not built today (no quota code: `grep -i quota crates/` finds only the request-refusal kind, `crates/via-store/src/runtime.rs:100` [V]) | owner question Q-R5-6 |
| `logs` on shared or multi-turn connections | S1 has only private per-turn connections | the Codex and OpenCode slices (§4.4, Q-R5-3) |

## 1. Owners, lock order and wakes

| State | Owner | Created by | Written by | Ended by | Bound |
|---|---|---|---|---|---|
| Global memory budget (`MemoryBudget`, one `Semaphore`) | via-store | `Store::open` | each acquirer (§5.1) | last `Arc` after Store threads join | 128 MiB |
| Store request lanes (Latch, Lifecycle, Internal, Public) | `Store`; served by the SQLite thread | `Store::open` | `StoreClient` handles by lane tag | fence, or writer death | §6.1 |
| Raw inbox, raw worker, blob files | Store raw worker | `Store::open` | `RawWriter`, `BlobWriter`, `BlobReader` | fence, then join; death guard | §6.4, §6.6 |
| `ConnectionLatch` | via-wire, per connection | `open_connection` | readers, stdin writer, raw worker via `RawFaultSink` | last holder after `finish` | §7.4 |
| Reader and stdin-writer tasks | `WireMessages` (unique) | `open_connection` | the tasks | `finish` under one deadline, then adoption | §7.6 |
| Message queue | `WireMessages` | `open_connection` | stdout reader | `next_message`, or `finish` | 64 messages, 4 MiB |
| Route → Adapter hop | Adapter `execute` | per drive | Route | end of `execute` | 1 message |
| Observation channel and its byte semaphore | Core drive | per drive | Adapter | end of the drive | 1024 items, 4 MiB |
| Stall timer | Adapter pending delivery | first blocked send | Adapter | acceptance of that item, or 10 s | one per drive |
| Drive reserve | Core drive | dispatch, before submission | — | end of the drive | 16 MiB (§5.3) |
| Step tracker | Core drive (`TurnRecord`) | submission | the drive | the terminal commit | §2.4 |
| Published progress | `Slot` `Running` entry (`crates/via-core/src/engine/queue.rs:274` [V]) | `Running` creation (`:488` [V]) | the session's drive only | `finish_running` (`:593` [V]) | §2.4 |
| Socket admission | daemon accept loop | daemon start | accept loop | daemon end | 32 |
| `started_at` | `Engine` | `Engine::open` | never | never | |
| Open-session tally, closing set | `Engine` (`Sessions`) | `Engine::open`, seeded from Store | receipt and closed-now answers | never | §10.3 |
| Live `Armed` controls | Host ledger (`crates/via-host/src/host.rs:72` [V]) | control verification | `Capacity::armed` | control drop | one per anchor |
| Schema v6 | Store | its owning transaction | that transaction | never in S1 | §6.7 |

**Lock order.** T3 §1 stands: `admission` (async) → `sessions` → slot state;
`Head` as T3 orders it; `stop` alone; no std mutex across an `.await`. An
async owner's lock may briefly take the `Lanes` mutex; the reverse is
forbidden. Code holding `Lanes` or the raw-inbox mutex takes no other lock,
awaits nothing and runs no callback; replies complete after release.
`Sessions` and `Unresolved` are leaf std mutexes, taken in that order by
`Engine::counts()` only. Publishing and reading progress take only the slot
state std mutex, briefly, with no await (§2.4). `MemoryBudget` needs no lock.

**Wakes** (a wake is a hint; the receiver re-reads the owning state):

| Wake | Producer | Consumer |
|---|---|---|
| Force stop or Store latch | `Engine::force_signal()` (`crates/via-core/src/engine/latch.rs:446` [V]) | Route, Adapter |
| Wire failure, EOF or exit | `ConnectionLatch` watch | Route, `next_message`, `finish` |
| Stop order changed, or `force_at` reached | Route's wake (`crates/via-routes/src/runtime.rs:516` [V]) | Route's `select!` |
| Stall or event overflow | the Adapter drops Route's hop receiver | Route, through `Sender::closed()` (§2.3) |
| Raw unit durable | per-unit `oneshot` | `next_message`, stdin writer |

## 2. Events, observations and progress (R1–R3)

### 2.1 Durable events (R1, R2)

Rule (R1): an event is durable when crash recovery or the envelope depends
on it. Core commits exactly these C1 §6 types; nothing else is an event.

| Type | Durable | Source |
|---|---|---|
| `session.opened`, `session.reopened`, `session.closed` | yes | Core |
| `turn.queued`, `turn.submitted`, `turn.started`, `turn.ended`, `turn.revised` | yes | Core |
| `cancel.requested`, `cancel.settled`, `steer.delivered` | yes | Core / Adapter observation |
| `action.denied`, `vendor.request_declined` | yes (the envelope lists cite their `seq`) | Adapter observation |
| `process.exited`, `server.lost`, `raw_log.incomplete`, `warning` | yes | Core / Adapter |
| `assistant.text`, `reasoning.summary` | **no** (R2): a `model` progress mark | raw log only |
| `tool.started`, `tool.ended` | **no**: tool progress marks | raw log only |
| `usage.updated` | **no**: a usage progress mark | raw log only |
| `file.changed` | **no**; not reported (requirement non-goal) | raw log only |
| `vendor.other` | **no**; an unknown message is activity only | raw log only |

Consequences, each stated once here:

- Dense per-session `seq`, `turn`, `late`, `raw_ref` and the ordering rule
  "`turn.ended` is the last non-late event of its turn" are unchanged.
- The envelope is unchanged in content (R6): `events {first_seq, last_seq,
  count}` now ranges over durable events, and `denied_actions[].event_seq`
  and `auto_declined_requests[].event_seq` still cite durable events.
- A turn of the fake has about six events (`turn.queued`, `turn.submitted`,
  `turn.started`, `turn.ended`, plus any cancel events).
- A late tool completion is raw-log evidence only; it never changed a settled
  envelope and still does not.

### 2.2 What a Route decodes (R2)

Route decodes each vendor message into a typed struct that holds only what
R1–R6 need:

- the message type and correlation IDs (vendor turn, thread, session, tool
  call);
- tool names;
- usage numbers;
- acceptance, identity and terminal fields the envelope carries
  (`final_text`, `stop_reason`, `vendor_code`, usage, structured output);
- the payload fields of the durable observations of §2.1 (`action.denied`,
  `vendor.request_declined`, `steer.delivered`, `warning`).

Everything else is skipped with `serde::de::IgnoredAny`, so text chunks,
reasoning, tool inputs and tool outputs are never copied (R2: VIA does not
interpret tool inputs or outputs). Rules, each enforced in Route's decode:

1. **Short fields.** An ID, tool name, type tag, `stop_reason` or
   `vendor_code` is at most `SHORT_FIELD_MAX` = 1 KiB. A known message with a
   longer one is a protocol failure citing its raw ref (C2 §1 rule 6: a
   malformed known message). This is what bounds the terminal extras and the
   failure summary (§6.5); it fixes round-4 finding Astra F2.
2. **Bounded lists.** A `Vec` field has an element cap of
   `LIST_FIELD_MAX` = 256, enforced by a bounded sequence visitor; more is a
   protocol failure. So a decoded message retains at most its length plus
   64 KiB of headers.
3. **Durable payloads** (`action.denied` target, declined summary) keep C2's
   256 KiB encoded cap; over it is a protocol failure with raw evidence.
   Text splitting is gone: no text is an observation any more.
4. **Structure limits first.** Before the typed decode, `json_limits::count`
   (§9.2) enforces depth 64 and 65,536 nodes; an excess is a protocol
   failure.
5. **No peer `Value`, no buffering decode.** No `serde_json::Value` is built
   from vendor or C1 bytes (§9.2). A `Deserialize` type fed peer bytes uses
   none of `#[serde(flatten)]`, `#[serde(untagged)]`, an internally tagged
   enum (`#[serde(tag = …)]`) or an adjacently tagged enum (`tag` +
   `content`): each buffers input into serde's private `Content` tree, an
   uncharged copy. This closes the round-4 decision-1 gap both reviewers
   found. [V] Today these attributes appear on `Serialize`-only types in
   `crates/via-core/src/api.rs` (`:890`, `:1084`, `:1252`, `:1286`, `:1350`),
   on the fake agent's own script input (`crates/via-fake-agent/src/main.rs:32`,
   `:39`), and on the Host anchor protocol (`crates/via-host/src/protocol.rs:76`,
   `:98`), a bounded private channel between VIA's own processes that
   carries no vendor bytes (`:1`). None is fed vendor or C1 bytes; the rule
   names vendor and C1 input only. A grep for these attributes on
   `Deserialize` types in via-routes, via-adapters, via-core and via-cli is an
   acceptance check.

[V] Today the fake route decodes `text` with its text
(`crates/via-routes/src/lib.rs:340-344`), measures tool payloads
(`:381`, called at `:492`, `:508`) and copies up to 16 KiB of an unknown
message (`:518-540`). All three go: `text` keeps only `vendor_turn_id`, tool
messages keep `tool_id` and `name`, and an unknown message keeps only its
type tag (still at most 256 B, `:418` [V]).

### 2.3 The observation channel (C2 A1 after R2)

Core's drive creates the channel per drive (today `mpsc::channel(64)`,
`crates/via-core/src/engine/drive.rs:1280` [V]). After R2 it carries:

| Observation | Core action |
|---|---|
| `turn.accepted`, `session.vendor_identity_confirmed`, `resume.mismatch`, `session.vendor_closed`, `tool.quiescent`, `turn.vendor_terminal` | as C2 §4 today |
| `action.denied`, `vendor.request_declined`, `steer.delivered`, `warning` | commit the event (existing `commit_event`, `drive.rs:1461` [V]) |
| **`progress { at, model, tools_started, tools_ended, usage }`** (new) | fold into the step tracker (§2.4); no Store write, except a step row at a step boundary (§3) |

One vendor message yields at most one `progress` item plus its other
observations. A message with no mark (an unknown type) still yields an empty
`progress`, which only moves `last_activity_at`. A `progress` item carries its
vendor turn ID like every observation; one attributed to a turn that is
already terminal is late and dropped (the raw log holds it).

**Bounds** (C2 A1, unchanged numbers):

- `mpsc::channel(1024)` plus a per-drive `Semaphore` of 4 MiB. An item is
  charged `512 + Σ(64 + len(s))` over its `String` fields, acquired by the
  Adapter before it builds the item and carried in it until Core has handled
  it. Proof: a `String` built from a slice has capacity equal to its length
  and a 24 B header; the fixed struct is under 512 B.
- The Route → Adapter hop shrinks from `mpsc(64)` to `mpsc(1)`
  (`crates/via-adapters/src/runtime.rs:132` [V]). Its memory is part of the
  drive reserve (§5.3), not charged per item. Route blocking on the hop stops
  it calling `next_message`; the Wire queue then fills and fails
  `overflow` (runtime §8 "Route message staging"), which is the contract's
  answer to a burst.

**Stall** (C2 A1: 10 s without drain):

- Owner: the Adapter's pending delivery. The timer is one absolute deadline,
  `first_block + EVENT_STALL` (10 s, a Core constant passed in), restarted
  only when that item is accepted.
- On expiry the Adapter drops the hop receiver. Route selects on
  `hop.closed()` in every wait (§8), so it sees the closure at once, fails
  `RouteError::Overflow` and force-closes the connection (for a private
  process, Host stops the group: the interrupt). Core disposes `Overflow` as
  today (`crates/via-core/src/engine/terminal.rs:97` [V]: `failed`,
  `overflow`).
- This replaces A19's stop order with cause `overflow`. [V] Today the
  Adapter already drops the receiver when a delivery wait fails
  (`crates/via-adapters/src/runtime.rs:146-153`) and Route reports the closed
  hop as `Overflow` (`crates/via-routes/src/runtime.rs:755`); what is new is
  the 10 s bound (today the wait is bounded only by the turn deadline,
  `crates/via-adapters/src/runtime.rs:322`) and the `closed()` arm, so Route need not wait for more vendor
  output. No `StopCause` is added and no exhaustive site changes.

**Event overflow** (C1 §5 accumulation, early): Core counts the payload bytes
of the adapter-sourced durable events it commits for the turn. When the next
one would take the total over `ENVELOPE_MAX` (1 MiB), Core does not commit it
and calls `observed_rx.close()`. The Adapter's next send fails, it drops the
hop, and the turn fails `overflow` by the stall path above. This bounds a
turn's event bytes on disk (§5.4) and fails the turn as early as C1 §5 would
at the terminal.

### 2.4 Progress snapshot (R3)

**Owners.**

- The **step tracker** is per turn state in the drive's `TurnRecord`. The
  drive creates it at submission, writes it for each `progress` item and at
  acceptance and at the terminal, and drops it with the record. It is the
  only place the step rule runs.
- The **published progress** is a copy held in the session `Slot`'s existing
  `Running` entry (`crates/via-core/src/engine/queue.rs:274` [V]), which
  exists exactly while the turn runs (`start_running` `:488`, cleared by
  `finish_running` `:593` [V]). The drive publishes after each change with
  `Slot::publish_progress(turn, &ProgressDelta)` under the slot state mutex
  (a leaf, no await), writing only the changed fields in place: most vendor
  messages change only `last_activity_at`, and the name list is touched only
  on a tool start or end. This carries the value in the existing owner of "the
  running turn", with no new watch or task.
- **Reading.** `status` calls `Engine::slot(session)`
  (`crates/via-core/src/engine.rs:334` [V]), and if the `Running` entry's
  turn matches, copies its `Progress`. It takes no Store round trip and never
  waits on the drive: the mutex is held only for a copy of at most 70 KiB.
  The snapshot ends with the turn (`finish_running`).

**`Progress`** (the published copy, C1 §3.7 `progress` after A26):

| Field | Meaning |
|---|---|
| `turn` | the running turn |
| `current_step` | 0 until acceptance, then the step rule below |
| `phase` | `tools` while any tool is open, else `model` |
| `running_tools` | names of open tracked tools, at most 64 |
| `last_activity_at` | arrival time of the last vendor message (any type) |
| `tokens` | `{total, scope}` of completed steps, or `null` before any sample |

**The step rule** (one reducer for every vendor; the Adapter only classifies
each message into marks):

1. At `turn.accepted`: `current_step = 1`, step start = now,
   `results_since_output = false`.
2. `tools_started` entries `(id, name)`: add to the open set.
3. `tools_ended` ids: remove from the open set; set
   `results_since_output = true`.
4. `model` (the message is model output: text, reasoning or a tool request):
   if `results_since_output`, the current step ends now, `current_step += 1`,
   the new step starts now, and the flag clears.
5. At the terminal: the current step (if `current_step ≥ 1`) ends at the
   terminal time.

So the step count goes up by one exactly when the model produces output after
tool results (R3). A step's end is the next step's start; its row is written
at that instant (§3).

**Tokens** (R3: approximate, once per step, labelled):

- A `usage` mark is `(key?, total)`: an interval sample, never a cumulative
  one. `total` is the vendor's total tokens when reported, else input plus
  output.
- Within a step, samples with the same key replace each other, and different
  keys add (a key is the vendor's message ID when there is one). The step's
  tokens are that sum, or `null` with no sample.
- The published `tokens.total` is the sum over completed steps, updated when
  a step ends. `tokens.scope` is the route's declared
  `capabilities.usage.tokens` (C1 §4.1), so the label is always the route's
  own claim.
- The envelope's `usage` is unchanged: exact where the vendor reports it at
  the terminal (R6), mapped as each vendor spec says.
- The envelope's `steps` becomes the vendor's reported count when the vendor
  reports one (Claude `num_turns`), else the tracker's final
  `current_step`, else `null`.

**Bounds** (per running turn, inside the drive reserve):

- open set: at most `OPEN_TOOLS_MAX` = 64 tracked `(id, name)`, each field at
  most 1 KiB (§2.2 rule 1). A start beyond 64 increments an `untracked`
  count; an end whose id is unknown decrements it if positive. `phase` uses
  tracked plus untracked. So `running_tools` is exact up to 64 concurrent
  tools and a subset beyond.
- samples: at most 16 keys per step; a 17th key adds to a keyless sum.
- published copy: fixed fields plus at most 64 names of at most 1 KiB, so at
  most 70 KiB; the tracker at most 200 KiB.

### 2.5 Vendor mappings of the marks

The reducer is shared; this table is what each Adapter emits. Every row is
[S] from the cited spec unless marked [U].

| Vendor | `model` | `tools_started` | `tools_ended` | `usage` (key) | Envelope `steps`, `usage` |
|---|---|---|---|---|---|
| Fake (runtime §3.1, A33) | `text` | `tool_started {tool_id, name}` | `tool_ended {tool_id}` | new `usage {total_tokens}` message, no key | tracker count; `usage` unavailable (the fake terminal has none) |
| Claude `claude-cli` ([S §5]) | an `assistant` message with text or `tool_use` content | each `tool_use` block `(id, name)` | each `tool_result` block in a `user` message (tool ID) | `assistant.message.usage`, key message ID **[U]**: the spec probes only `result.usage` (§5, §3 row `usage`) | `num_turns` (probe, [S §3]); `result.usage`, scope `turn` |
| Codex `codex-app-server` ([S §5, §7]) | `item/started` or `item/agentMessage/delta` for `agentMessage` or `reasoning` | `item/started` for a tool item (`commandExecution`, `fileChange`; others such as MCP calls **[U]**), name = item type | `item/completed` for that item ID | `thread/tokenUsage/updated` `tokenUsage.last`, no key; scope `vendor_interval` (§7: never `.total` summed) **[U]**: whether one `last` is one model call | tracker count (Codex reports none); usage per §7 unchanged |
| OpenCode `opencode-serve` ([S §4, §5, §7]) | a text or reasoning part of an assistant message correlated to the turn | a tool part entering `running` (call ID, tool name) | a tool part entering `completed` or `error` | the assistant message's token fields, key assistant message ID (§7 one-ledger rule; step-finish duplicates it and is not added) | tracker count; usage per §7 unchanged. Exact event and part type names **[U]** until the vendor slice pins the legacy `message.*` family |

For all three real vendors, a message that is none of these (for example
Claude `system/init`, Codex `thread/status/changed`, OpenCode
`server.heartbeat`) is an empty `progress` item: it moves
`last_activity_at` only. OpenCode's `server.heartbeat` is transport
liveness, not a vendor message of the session [S §5], so it does not move
it. Codex's own tool-completion tracking for P7 (`tool.quiescent`) is
separate, exact and unchanged [S §6]; the snapshot's open set is display
only.

### 2.6 Idle deadline

Runtime §8: idle resets on normalized meaningful progress. [V] Today
`progress()` counts acceptance, `assistant.text`, `tool.started` and
`tool.ended` (`crates/via-core/src/engine/drive.rs:1775-1786`). After R2 it
counts acceptance and any `progress` item with `model`, a started tool or an
ended tool. A usage-only or empty item does not reset it (T3 §5: unknown
messages never did). The timer and its stop order are T3's, unchanged.

## 3. Step rows (R4)

### 3.1 Table

Schema v6 adds:

```sql
CREATE TABLE steps (
  session_id TEXT NOT NULL,
  turn INTEGER NOT NULL,
  step INTEGER NOT NULL CHECK(step >= 1),
  started_ms INTEGER NOT NULL,
  ended_ms INTEGER NOT NULL,
  tokens INTEGER CHECK(tokens IS NULL OR tokens >= 0),
  PRIMARY KEY(session_id, turn, step),
  FOREIGN KEY(session_id, turn) REFERENCES turns(session_id, number)
) WITHOUT ROWID;
```

- Keyed by session first, clustered on the key (`WITHOUT ROWID`), so one
  session's rows are one contiguous key range.
- Times are Unix milliseconds from Core's wall clock; reads render RFC 3339.
  No ordering relies on them (the key orders).
- `tokens` is interpreted under the turn's route `usage.tokens` scope.

### 3.2 Write path and ordering against `turn.ended`

- **Step end.** When the tracker ends step N before the terminal (§2.4 rule
  4), the drive commits `CommitSteps { session, turn, rows: [row N] }` on the
  Internal lane through the single SQLite writer, awaited under
  `while_polling` (§8). No `Head` is taken: a row has no `seq`. The row
  insert is plain `INSERT`; a duplicate is a constraint error (a bug).
- **Batching** (R4 "may be batched"): rows that end while a previous step
  commit is in flight go in the next command together; normally one.
- **The last step** is written **in the terminal transaction**: every
  terminal commit built from the drive's `TurnRecord` (`finish_with`, and
  `finish` for a forced turn, `drive.rs:1035`, `:1000` [V]) carries the open
  step's row with `ended_ms` = the terminal time.
- **Ordering.** The drive awaits each step commit before it handles the next
  observation, and builds the terminal only after draining the channel. So in
  the writer's order every row of turn N precedes, or is in the same
  transaction as, turn N's `turn.ended`. A committed `turn.ended` implies all
  of the turn's rows are durable.
- **Failure.** A step commit follows T3 §7's rules for a turn write, as the
  `assistant.text` commit it replaces did: a known `NotCommitted` records the
  turn's first failure and stops it with cause `store` (`failed(store)`); an
  uncertain outcome latches. Requirement R4 forbids a silent drop, so a
  refused row is never skipped.
- **Cap.** At most `STEP_ROWS_MAX` = 10,000 rows per turn. At the first step
  beyond it, Core commits one `warning {code: "step_history_truncated"}`
  event and writes no more rows; the tracker keeps counting, so `status` and
  the envelope still show the true step.

### 3.3 What survives a crash

- Rows up to the last step whose commit completed. A step that ended while
  its commit was in flight, and the step in progress, are lost; they are
  recoverable only from the raw log (R4).
- Recovery synthesizes the turn's `unknown` terminal (T3) and adds no row.
- The snapshot is memory only and is not recovered.

### 3.4 Reading

`status` reads a page of one turn's rows (§4.2) with
`SELECT step, started_ms, ended_ms, tokens FROM steps WHERE session_id=?1 AND
turn=?2 AND step>?3 ORDER BY step LIMIT ?4`, a primary-key range scan.

### 3.5 Retiring (design only; built by `via-jm4.18`)

`DELETE FROM steps WHERE session_id = ?1` is a single keyed range delete on
the leading key column. The retention task deletes a session's steps, events,
envelopes and raw logs together.

## 4. Caller interface (R5)

Reply size: with request `id` capped at 256 B (A31), every reply's JSON-RPC
wrapper is under 512 B, so each page bound below applies to the `result`
object and needs no per-request wrapper arithmetic. Each reply is written
within `REPLY_WRITE` = 10 s or the connection closes (A32).

### 4.1 `wait` and `result`

- `wait` polls the Store every 20 ms as today (`crates/via-core/src/engine/read.rs:69` [V]) until the
  turn is terminal, `timeout_ms`, or final shutdown. Each poll is
  `terminal_facts` (§6.8), a small read. Only when the turn is terminal does
  the handler acquire its reply permit and read the envelope text with
  `result_text`.
- `result` reads `result_text` once.
- The envelope text is at most `ENVELOPE_MAX` (1 MiB) and is written as
  stored, with no `Value` built. No other change.

### 4.2 `status`

Params (A26): `session`, `turn?` (a turn number, default the running turn,
else the latest), `after_step?` (default 0), `limit?` (default 100, maximum
1000).

The reply is the C1 §3.7 durable members (§10.4), plus:

- `progress`: the published `Progress` (§2.4) when the session's running turn
  is in memory, else `null`;
- `steps`: `{turn, items: [{step, started_at, ended_at, tokens}], next_after,
  more}` for the selected turn, or `null` when the session has no turn.

Sources: one Public Store read (`session_status`, §6.8) for the durable
members and the step page, plus memory for `progress` and `process.alive`.
The reply is built within `STATUS_MAX` = 1 MiB; over it is
`admission_refused`. For the fake it is far below: 64 turn summaries, 8
queued `effective` values of a few hundred bytes, `progress` at most 70 KiB,
steps at most 1000 × 128 B.

### 4.3 `events`

Params: exactly one of `session`, `turn`; `after?` (0), `limit?` (200, max
1000), `types?`. `follow` is gone and refused as an unknown field.

- One read transaction: the window `after < seq ≤ after + 1000`, with `types`
  and `turn` as SQL predicates on the v6 `type` and `turn` columns.
- Returned events stop at `limit` or at `PAGE_MAX` = 1 MiB. A row's borrowed
  length (`ValueRef`, no copy) is checked before it is copied, so an
  over-budget row is never allocated.
- The SQLite thread writes the page's `events` array directly as one JSON
  text (`[`, the stored event texts separated by `,`, `]`); the handler writes
  the reply as three slices: prefix, that array, suffix. One copy exists, and
  it is charged (§5.1).
- `next_after` is the last scanned seq, including filtered rows; `more =
  next_after < head`, with the head read in the same transaction;
  `earliest_seq` is 1 (nothing is pruned; `history_pruned` is checked and
  unreachable).
- A first matching event larger than the page is `admission_refused` (C1
  §3.11). It cannot occur today: an event is at most 256 KiB of payload plus
  Core's fixed fields.

### 4.4 `logs`: raw excerpts, undecoded

Params (A27): exactly one of `session`, `turn`; `cursor?` (opaque, from the
previous page); `limit?` (entries, default 100, maximum 256).

Result: `{entries: [{connection_id, stream, offset, len, text}],
next_cursor}`. `stream` is `stdout`, `stderr` or `stdin`. `text` is the bytes
as lossy UTF-8; VIA does not parse them (R5).

- **Scope.** The connections opened for the addressed turn, or for every turn
  of the session, in creation order, from the v6 `connections` table. Only a
  private connection is served: one owned by one session and opened for one
  turn, which is every connection in S1 (the fake starts one process per
  turn). D4 isolation therefore holds by construction.
- **Cursor.** `r1.<connection_id>.<offset>`, parsed strictly: the next raw
  payload byte to return. A cursor naming a connection outside the addressed
  scope, or an offset outside its sealed or synced range, is
  `invalid_params`.
- **Paging.** From the cursor, the SQLite thread binary-searches the index for
  the unit containing the offset (§6.4), then walks consecutive units. For
  each unit it reads the whole unit (at most 1 MiB, `RAW_UNIT_LIMIT`) and
  checks its SHA-256, then emits the part from the cursor, at a UTF-8 boundary
  when one is within 3 bytes. A page emits at most `LOGS_RAW_MAX` = 128 KiB of
  raw bytes and 256 entries.
- **Bound proof.** One raw byte encodes to at most 6 JSON bytes (a control
  character as `\u00XX`; an invalid byte becomes a 3-byte U+FFFD), and an
  entry's other members are under 256 B. So a page is at most 6 × 128 KiB +
  256 × 256 B = 832 KiB < `PAGE_MAX`, and no unit is ever refused for size: a
  large unit spans several pages.
- **Errors.** A missing or corrupt unit is `store_error`, scoped to the
  request (no latch), as today.
- A running turn's connection is served up to its last synced unit.

### 4.5 Removed

- `events` `follow`, subscriptions, `event` and `event_end` notifications and
  `unsubscribe` (A25). `unsubscribe` is `method_not_found`. The daemon sends
  no notifications.
- Disconnect cleanup is therefore only the in-flight request: a dropped
  `wait` releases only its waiter (C1 §3.8, runtime §9); nothing else is held
  per connection.
- `via events --follow`.

### 4.6 Other methods

| Method | DTO (`deny_unknown_fields`) | Answer |
|---|---|---|
| `describe` | `harness?, model?, bound?, require?, vendor?, cwd?, allow_untested?` | Core, from `Capabilities::fake()` (`api.rs:932` [V]); no process, no write |
| `models` | `harness?` | Core: the fake's one model |
| `list` | `state?, harness?, label?, since?, limit?, cursor?` | Store `list_page` (§6.8; A12) |
| `daemon/status` | none | memory: `started_at`, counts (§10.3) |
| `status`, `events`, `logs` | as above | §4.2–§4.4 |

- New `ApiError` constants: `UNKNOWN_MODEL` (-32010, replacing
  `invalid_params` at `receipt.rs:100` [V]), `HISTORY_PRUNED` (-32019),
  `STORE_QUEUE_FULL` and `MEMORY_BUDGET` (both `admission_refused`).
- CLI (`crates/via-cli/src/main.rs:20-34` [V]): verbs `describe`, `status`,
  `list`, `models`; `status --turn N --after-step N --limit N`; `logs --cursor
  C --limit N`; spawn flags `--prompt-file`, `--instructions`, `--cwd`,
  `--require`, `--allow-untested`, `--label`.
- `serve --stdio`: a byte proxy between stdio and one daemon socket,
  auto-starting the daemon; two copy loops with fixed buffers under one
  `JoinSet`; stdin EOF shuts the socket's write side; ends at socket EOF.
  Parity test: one scripted sequence over the socket and over the proxy gives
  the same replies, including the oversize case.

## 5. Memory and disk bounds (R7)

### 5.1 One global budget, coarse charges

Runtime §8 requires one 128 MiB retained-payload budget, taken as a permit
before allocation. Round 5 keeps one `tokio::sync::Semaphore` of 128 MiB
(`MemoryBudget`, via-store, the lowest crate, `scripts/check-layers.py:20`
[V]) and replaces round 4's ten classes and per-allocation proofs with a few
coarse charges, each an upper bound proved in one line:

| Charge | Amount | Acquired | Released | Refused |
|---|---|---|---|---|
| Store request partition | 8 MiB | `Store::open` | never | open fails |
| Raw unit | `max(len, 512)`, plus the `staging` class (8 MiB per connection, 32 MiB total) | `Payload::stage` before the copy, or by the stdout reader before each append (then `Payload::freeze`) | staging at the raw ack; global when the last `Arc<Payload>` drops | nonblocking: the connection fails `overflow` + `raw_log.incomplete` (A1) |
| Drive reserve | 16 MiB (§5.3) | dispatch, after the connection slot and before the submission commit | end of the drive | waits with the dispatcher's other waits; a queued turn stays queued |
| C1 line | read size, plus the `input` class (32 MiB total), in 64 KiB steps | before each read | handler return | waits to the 5 s partial-line deadline, then closes the connection |
| C1 decode | §5.2 | before each pass | end of pass or handler return | nonblocking: `admission_refused` (`MEMORY_BUDGET`) |
| Reply | `PAGE_MAX + 4 KiB` for pages, `result`, `wait`, `status`; `2 × RAW_UNIT_LIMIT` for `logs`; 64 KiB otherwise | before the Store read | after the write, or the connection's close | nonblocking: `admission_refused` |
| Blob chunk | 64 KiB | before each chunk copy | after the raw worker wrote or read it | waits under the chunk's 2 s bound |
| Dispatched blob prompt | its length, plus `input` | before the load | after the start message is written | nonblocking: the turn fails `overflow` before submission |

The Wire message queue needs no charge of its own: a queued message is the
staged unit's `Arc<Payload>`, whose global charge lasts until the last handle
drops. Its per-connection 64-message / 4 MiB residency is a counter checked
by the reader (nonblocking; full fails `overflow`).

Waits are acyclic: a drive reserve waits only for C1, reply and staging
permits, and none of those waits for a drive. A peer that never reads a reply
holds its permit at most `REPLY_WRITE` (A32).

Not charged, and measured by the RSS gate (§12.4): allocator overhead,
SQLite's own buffers and 8 MiB cache, task stacks, fixed-size structs.

### 5.2 C1 decode charges

Facts from `serde_json` 1.0.151 [V]:

- a string with no escape is returned as a slice of the input
  (`read.rs:512-518`); an escaped one is unescaped into one reused scratch
  `Vec` (`:519-528`), never longer than its raw span;
- a `RawValue` is a slice between two indexes (`read.rs:636-650`); a
  `Box<RawValue>` copies exactly that span;
- recursion limit 128 (`de.rs:63`), above our 64.

Charges for a line of `len` bytes with `nodes` counted nodes (at most
65,536):

| Pass | Charge | Proof |
|---|---|---|
| Counting pass | `2 × len`, transient | scratch at most one string, and `Vec` doubling at most doubles it [I: S checks with an allocator test] |
| Identity pass (keyed calls) | `3 × len`, transient | scratch `2 × len` plus escaped keys at most `len` |
| Typed decode | `2 × len` transient plus `2 × len(params) + 64 × nodes` retained | retained strings and `Box<RawValue>` copies are disjoint spans (at most `len`), a list expanded from a `Box<RawValue>` duplicates at most its span (at most `len` more), and each node adds at most one 24 B header or 16 B box in a `Vec` whose doubling bounds it by 64 B |

Passes run in sequence, each releasing its transient charge before the next.
Peak for one request: `len` (line) + `4 × len` + `64 × nodes`, so at most
5 × 16 MiB + 4 MiB = 84 MiB for a maximal line. This charges the overlapping
representations that round-4 finding Astra F1 found uncharged. One allocator
test at the largest inputs confirms the factors (§12).

### 5.3 The drive reserve

One private connection per drive in S1. The reserve covers, at their
maxima:

| Part | Bound | Proof |
|---|---|---|
| Pipe read buffers | 128 KiB | two 64 KiB buffers |
| Stdin writer | 512 KiB | at most four unacknowledged pieces of at most 96 KiB, one piece being encoded (§7.3) |
| Route decode | 2 MiB | one message at a time, at most 1 MiB; scratch at most `2 × len` |
| Decoded messages in flight | 3.2 MiB | Route's, the hop's one, the Adapter's: 3 × (1 MiB + 64 KiB), §2.2 rules 1–2 |
| Observation channel | 4 MiB | the C2 semaphore (§2.3) |
| Step tracker and published progress | 256 KiB | §2.4 |
| Terminal build | 3 MiB | `final_text` at most 1 MiB, the measured encoded envelope at most 1 MiB, the Store command's copy at most 1 MiB |
| **Total** | **13.1 MiB, reserved as 16 MiB** | |

Four drives reserve 64 MiB. With the 8 MiB partition and 32 MiB of staging
that leaves at least 24 MiB for C1 requests and replies under full load; a
maximal 16 MiB prompt is refused `admission_refused` while three or more
turns run (§14).

### 5.4 The bound table (R7)

| Resource | Bound | Enforced at | At the bound | Proof |
|---|---|---|---|---|
| Vendor stdout message | 1 MiB including LF | stdout reader | `MessageTooLarge`, raw-only drain | runtime §4; reader never buffers more |
| Message queue | 64 messages, 4 MiB per connection | stdout reader | `overflow` | counter |
| Raw staging (memory) | 8 MiB per connection, 32 MiB total | `Payload::stage` / stdout reader | `overflow` + `raw_log.incomplete` | class semaphores |
| Raw log (disk) | per unit 1 MiB (stdout) or 64 KiB (stderr); per connection unbounded except by the turn's wall deadline | reader | — | aggregate: runtime §6 quota, not built (Q-R5-6) |
| Envelope | 1 MiB encoded | `ended_record`, counting writer before allocation | `failed(overflow)` with the bounded summary (§6.5) | measured |
| Turn event bytes (disk) | `ENVELOPE_MAX` of adapter-sourced event payload plus Core's lifecycle events | Core, per commit | channel closed → `overflow` (§2.3) | counter |
| Blob files | one prompt and one identity per spawn/resume, each at most 16 MiB (the C1 line) | handler | — | line cap; unreferenced files swept at start |
| Blob memory | 64 KiB per chunk in flight | handles | chunk wait | one chunk per handle |
| Snapshot | 70 KiB published, 200 KiB tracker, per running turn | reducer | untracked count, keyless sum | §2.4 |
| Step rows | 10,000 per turn, each under 64 B stored | Core | `warning` once, no more rows | counter |
| Observation channel | 1024 items, 4 MiB per drive | Adapter | wait; 10 s → `overflow` | C2 A1 |
| Store requests | 1 Latch + 7 Lifecycle + 64 ordinary slots; 8 MiB bytes | `Lanes::push` | `NotEnqueued` / `admission_refused` | §6.1 |
| Store transaction | 128 events and 1 MiB, plus one envelope of at most 1 MiB | `Lanes::push` | `NotEnqueued` | A30, §6.5 |
| Store replies | `PAGE_MAX` for pages, `ENVELOPE_MAX` for results, `STATUS_MAX`, logs 832 KiB | the SQLite thread checks borrowed lengths before copying | page stops; a single oversize item is `admission_refused` | §4 |
| C1 request | 16 MiB line; 32 MiB input total; decode §5.2 | reader, handler | parse error then close; `admission_refused` | §9 |
| Replies in flight | 32 sockets × one reply, each written within 10 s | handler | connection closed | A32 |
| Global | 128 MiB | every charge above | the charge's own outcome | one semaphore |

## 6. Store

### 6.1 Request lanes [kept: A2]

[V] Today one `sync_channel(128)` carries every request
(`crates/via-store/src/runtime.rs:1021`), served FIFO; Host's
`ProcessJournal` uses the same sender; `Store::drop` sends `Shutdown` with a
blocking `send`.

`Store::open` replaces it with one `Lanes` value (`Mutex<State>` +
`Condvar`) holding four FIFO lanes; every slot and byte is exclusive to its
lane:

| Lane | Members | Slots | Bytes |
|---|---|---|---|
| Latch | the failure-resolution unit only (`crates/via-core/src/engine/batch.rs:85`, `:91`, `:165` [V]) | 1 | `LATCH_BYTES` = `ENVELOPE_MAX + TERMINAL_EXTRAS_MAX + 8 × CANCEL_RECORD_MAX + 512` = 1 MiB + 320 KiB + 512 B |
| Lifecycle | every other Store call of the shutdown pipeline | 7 | 2 MiB |
| Internal | every other commit and every read by Core, Route, Host or recovery | 64 less Public's | the rest: 4 MiB + 704 KiB − 512 B, less Public's |
| Public | reads issued by C1 handlers | at most 32 of the 64 | 4 KiB each |

- Membership is by handle: `StoreClient::public()`, `lifecycle()`,
  `latch()`; default Internal.
- Service order: Latch, then Lifecycle, then Internal and Public round-robin
  (runtime §8's fair lanes).
- A full lane or an exhausted allowance returns `StoreError::NotEnqueued`
  (known outcome, T3 §7.1): a mutation keeps T3's `store_error`
  `not_committed`; a Public read is `admission_refused`
  (`STORE_QUEUE_FULL`) and never latches.
- `Lanes::push` is the only enqueue path; it never blocks. Host's journal
  keeps its `try_send` semantics through it.

### 6.2 Lifecycle and Latch capacity [kept]

[V] `Engine::shutdown` (`crates/via-core/src/engine/stop.rs:444`) issues its
Store requests one at a time, each under `min(FINALIZE_WRITE, remaining)`
(2 s, `latch.rs:46`) or `BATCH_READ` (2 s, `batch.rs:24`), inside the 10 s
`FINAL_SHUTDOWN` (`crates/via-cli/src/server/shutdown.rs:23`).

- The failure-resolution unit awaits each request before the next, so it
  needs one slot and at most `LATCH_BYTES`. Lifecycle requests cannot touch
  Latch bytes. An abandoned Latch request means the writer failed to serve
  one request in 2 s; the next push is `NotEnqueued`, runtime §7's "skipped
  batch".
- At most five Lifecycle requests are abandoned at a full 2 s and one at the
  remainder, so 7 slots cover them plus the one being issued. [I] The
  implementing slice checks every Lifecycle bound is at least
  `min(2 s, remaining)` and that each step checks the deadline first.
- 2 MiB of Lifecycle bytes holds one largest forced terminal (1 MiB + 64 KiB
  + 512 B) plus small requests. When abandoned requests hold the bytes, a
  further push is `NotEnqueued`, reported `not_committed`, and recovery
  resolves the turn at restart (T3 §7).
- `finish` (`drive.rs:1000`) uses the Lifecycle handle; `finish_with`
  (`:1035`) keeps Internal.

### 6.3 Lock discipline, fence and writer death [kept]

- Only queue and counter updates run under the `Lanes` mutex; replies are
  dropped after release. The SQLite thread's wait is a predicate loop, so a
  spurious or early notify is harmless.
- `Shutdown` is an admission fence, not a queued item: `Store::drop` sets
  `fence`, notifies and joins; later pushes are `NotEnqueued`; everything
  accepted before is served in lane order.
- Writer death: the thread body runs under a `DeadGuard` whose `Drop` (also
  on unwind) sets `dead`, takes all lanes and the in-flight item it owns from
  pop to reply, releases the mutex, and fails them `WriterLost`, which
  latches. A push after `dead` is `WriterLost` at once.
- The raw inbox uses the same fenced queue (`FencedQueue<T>`).

### 6.4 Raw inbox, group commit, lookup [kept]

[V] Today `RawWriter::append` `try_send`s into a 64-slot channel and maps
`Full` to `StoreError::Raw("raw queue full")`
(`crates/via-store/src/runtime.rs:522-531`); `raw_loop` serves one unit at a
time and syncs both files per unit (`crates/via-store/src/runtime/raw.rs:25`,
`:120`).

- **Inbox.** A `FencedQueue<RawCommand>` whose depth is bounded by staging
  permits: every `Append` carries an `Arc<Payload>` from `Payload::stage` or `Payload::freeze`
  (`max(len, 512)`), so at most 32 MiB / 512 B = 65,536 appends queue. A
  `Barrier` holds no permit (at most one per connection); a blob command holds
  one 64 KiB chunk permit (one in flight per handle).
- **Fault publication.** via-store declares `trait RawFaultSink { fn
  raw_failed(&self, error: &StoreError); }`; Wire implements it on the
  connection latch. The worker calls it on its own thread, with no lock, and
  it never blocks.
- **Group commit** (runtime §4: 1 MiB or 20 ms). Per touched connection:
  write payloads, `sync_data`, write index entries, `sync_data`, ack. A
  non-`Append` command flushes the batch first. No index entry precedes its
  payload's sync.
- **Death guard** owns the in-flight batch and current command; on exit or
  unwind it sets `dead`, takes the queue, calls each connection's sink once
  with `WriterLost` and fails every reply.
- **Failure scope.** A failed write or sync fails that batch's units of the
  affected connections (each sink once, first error); other connections are
  acked; a failed connection keeps failing fast (`raw.rs:37-38` [V]).
  `StoreError` gains `Clone`.
- **Lookup.** [V] `read_raw_ref` scans the 45-byte index linearly
  (`raw.rs:154-212`). Entries are appended in increasing payload offset, so
  lookup by offset is a binary search (entry `i` at byte `8 + 45 × i`), then
  one payload read checked against length and SHA-256; at most
  `⌈log2(entries)⌉ + 1` reads. Commit-time validation and `logs` (§4.4) use
  it.

### 6.5 Transaction cap, terminal budget and failure summary [A30]

- **`Command::bytes()`** is the exact encoded length of the command's
  variable payload (counting writer over the serialization the SQLite thread
  binds) plus 512 B, over an exhaustive match of `Command`
  (`crates/via-store/src/runtime.rs:710` [V]).
- **Cap** (runtime §8 as amended by A30): at most 128 events and at most
  `TX_PAYLOAD_MAX` = 1 MiB of payload **excluding one terminal envelope**,
  which is itself at most `ENVELOPE_MAX` = 1 MiB (C1 §5, unchanged by R6).
  Anything over is refused `NotEnqueued` before it is queued. A lifecycle
  atomic batch is never split.
- **Terminal budget**, enforced by measurement where each record is built:

  | Constant | Value | Covers |
  |---|---|---|
  | `ENVELOPE_MAX` | 1 MiB | the turn's encoded envelope |
  | `TERMINAL_EXTRAS_MAX` | 64 KiB | `turn.ended` (vendor fields at most 1 KiB, §2.2), an owed `raw_log.incomplete`, the turn row, the connection seal, the final step row |
  | `CANCEL_RECORD_MAX` | 32 KiB | one `queued → cancelled` record (its envelope has no text; `cwd` at most 6 × 4096 escaped) |

  A normal terminal is at most 1 MiB + 64 KiB; the failure-resolution batch
  (one terminal plus at most `FAILURE_BATCH_CANCELLATIONS` = 8,
  `crates/via-store/src/runtime.rs:390` [V]) at most 1 MiB + 320 KiB. Both fit
  their lanes (§6.1).
- **Bounded failure summary** (C1 §5; fixes Astra F2). Over `ENVELOPE_MAX`,
  the turn fails `overflow` and the envelope is the summary: `final_text`
  `""`, `structured_output` `null`, `denied_actions` and
  `auto_declined_requests` `[]` (their durable events remain), every other
  member unchanged. Every remaining member is Core-built, a canonical enum, a
  `cwd` (at most 24 KiB escaped) or a vendor short field (at most 1 KiB,
  §2.2), so the summary is at most 64 KiB; `ended_record` measures it before
  persistence. The raw log is the evidence.
- **Blobs** (runtime §8's floor): prompts and identities over `INLINE_MAX` =
  256 KiB use the blob path, because the retry identity contains the prompt
  (`crates/via-core/src/api.rs:804-858` [V]) and a spawn command carries both.

### 6.6 Blob path on the raw worker [kept; F5 fixed]

- **Owner.** The raw worker is the only thread touching `blobs/`; startup
  verification and the sweep run on the SQLite thread before admission.
  `blobs/` is validated like `raw/` (0700, not a symlink, daemon's uid);
  files `b_<32 hex>.blob`, `create_new`, 0600, `NOFOLLOW`; rows store the
  relative id.
- **Handles.** `BlobWriter::write(chunk ≤ 64 KiB)` (global chunk permit, ack
  under 2 s; timeout is `not_committed` because nothing references it yet);
  `finish() -> BlobRef{id, len, sha256}` syncs the file and `blobs/`;
  `discard`/`Drop` unlink an unfinished file. `StoreClient::discard_blob(BlobRef)`
  unlinks a finished blob after a commit known not to have happened;
  unawaited, and a lost discard is swept. `BlobReader::next_chunk()` returns
  at most 64 KiB, one in flight.
- **Referencing.** A row references a blob only after `finish`, in the same
  transaction; a known non-commit is followed by `discard_blob`; an uncertain
  one leaves it.
- **Exact replay comparison** (C1 byte-identical rule). An inline identity
  is compared byte for byte. A blob identity is compared by length and
  SHA-256 first; on a match Core streams the blob and compares chunks with
  the incoming identity pieces.
  - **Outside `admission`** (fixes Astra F5). The key lookup runs under
    `admission` as today (`receipt.rs:76`, `:202` [V]); a found row is
    committed and immutable (S1 never updates or deletes `spawn_keys` or
    `operations`), so Core releases `admission` before streaming the
    comparison and needs no revalidation.
  - The comparison has one absolute bound, `REPLAY_COMPARE` = 10 s; expiry is
    `store_error` for that request only.
- **Dispatch load.** A blob prompt is loaded after `try_acquire` of `input`
  for its length, with a running SHA-256 and a UTF-8 check; a mismatch fails
  the turn as corrupt evidence; an `input` refusal fails it `overflow` before
  submission. The loaded `String` is moved into the streamed start (§7.3).
- **Recovery.** `verify_blobs()` streams every referenced blob (regular file,
  length, SHA-256); a mismatch is `StoreError::Corrupt("blob")` at the owning
  row. `sweep_blobs()` unlinks unreferenced files. F16's handle-leak scan
  covers `blobs/`.

### 6.7 Schema v6

`SCHEMA_VERSION` becomes 6 (`crates/via-store/src/runtime.rs:24` [V]); older
development Stores are refused untouched, as today (`:34-37` [V]). v6 is
defined once and frozen by a golden DDL test (`s1_store_v6_schema_is_frozen`).
Changes from v5 (`crates/via-store/src/runtime/sql.rs:146-184` [V]):

- **`steps`** (new): §3.1.
- **`sessions`** gains `created_ms`, `updated_ms`, `harness`, `label`,
  `stamp`, `ord INTEGER NOT NULL UNIQUE`, and indexes `(updated_ms DESC, id)`
  and `(stamp)` (A12). `updated_ms` is the `at` of the transaction's
  highest-`seq` event; `stamp = MAX(stamp)+1` in every transaction that
  changes the row; `ord = MAX(ord)+1` once at spawn. Both are strictly
  increasing while no `sessions` row is deleted.
- **Frozen `params`** gains `cwd` (absolute) and `allow_untested` (A14);
  [V] today `receipt.rs:140` freezes only harness and model.
- **`events`** gains `turn INTEGER` (deferred composite FK to `turns`) and
  `type TEXT NOT NULL`, written by `insert_event` from the event JSON
  (`sql.rs:966` [V]), and the index `(session_id, turn, seq)`.
  `connection_id` joins a composite FK `(session_id, connection_id) →
  connections(session_id, id)`, so a raw ref to another session's connection
  cannot commit. `late` and `at` stay inside the JSON: no query reads them
  (A28 narrows runtime §6's target row; round 4's `late`, `at_ms` and two
  partial indexes served only the follower and are dropped).
- **`turns`** gains `ended_seq` with `CHECK((state IN
  ('completed','failed','cancelled','unknown')) = (ended_seq IS NOT NULL))`
  (A15), and `prompt_blob` with `CHECK((prompt IS NULL) <> (prompt_blob IS
  NULL))`.
- **`spawn_keys.identity`, `operations.identity`** become nullable with
  `identity_blob` and the same CHECK.
- **`connections`** (new): `id TEXT PRIMARY KEY`, `session_id`, `turn` (FK),
  `high_water INTEGER` (NULL until sealed), `incomplete INTEGER NOT NULL
  CHECK(incomplete IN (0,1))`, `UNIQUE(session_id, id)`. Files are named from
  the id as today (`raw.rs:159-160` [V]). Created in the submission
  transaction (`SubmissionRecord` gains `connection_id`); `incomplete` set by
  a committed `raw_log.incomplete`; sealed by the terminal transaction from
  the index's last complete entry (durable because `finish` ran its barrier),
  or `incomplete = 1` with the offset it can prove; recovery seals open rows.
- **Time.** `at` strings are parsed strictly to Unix ms; malformed is
  `StoreError::Constraint`. No ordering relies on wall time.

### 6.8 Store reads

| Read | Lane | Returns | Bound |
|---|---|---|---|
| `terminal_facts(session, turn)` | caller's | `Option<{state, cancel}>` via `json_extract(envelope, '$.cancel')`; `None` when not terminal | 4 KiB |
| `result_text(session, turn, budget)` | Public | the stored envelope text | `ENVELOPE_MAX` |
| `events_page` | Public | §4.3 | `PAGE_MAX` |
| `logs_page` | Public | §4.4 | 832 KiB, plus one unit read |
| `list_page` | Public | A12 (§6.9) | `PAGE_MAX` |
| `session_status(session, turn?, after_step, limit)` | Public | §10.4 members and a step page | `STATUS_MAX` |

`terminal_facts` replaces every internal use of the envelope `Value` read,
all of which need only existence, `state` or `cancel` [V]:
`crates/via-core/src/engine/control.rs:48` (state and cancel, `:210-216`),
`journal.rs:550`, `stop.rs:632`, `batch.rs:85`. C1 `result`, `wait` and
`await_terminal` use `result_text`. So no daemon path parses a stored
envelope into a `Value`. The existing `result` read is removed.

### 6.9 `list` paging [kept: A12]

Unchanged from round 4, which both reviewers accepted:

- cursor `l2.<phase>.<v0>.<w0>.<k1>.<k2>`, strict; `l1.` and malformed are
  `invalid_params`;
- the first page fixes `v0 = MAX(stamp)` and `w0 = MAX(ord)`; the population
  is `ord ≤ w0`;
- phase 1: keyset `(updated_ms DESC, id)` over `stamp ≤ v0`; phase 2 (only if
  some population member has `stamp > v0`): `ord` windows of 1000, returning
  `stamp > v0` matches and stopping before an unreturned match;
- limits 50 default, 200 maximum, `PAGE_MAX`; a row's borrowed length is
  checked before its summary is built.

Guarantee and proof: §11.3 (A12).

## 7. Wire [kept, condensed]

[V] Today the consumer is the reader: `next_message` reads 8 KiB chunks and
awaits every raw append inline (`crates/via-wire/src/runtime.rs:451`,
`read_either` `:552`, `record` `:527`); `read_either`'s `select!` is
unbiased (`:555`); `write_message` awaits each piece's ack (`:386`);
`WireHealth` is unused (`crates/via-wire/src/lib.rs:103`).

### 7.1 Shape (runtime §4 as written)

`open_connection` returns `WireConnection`; `into_parts(self) -> WireParts {
sender, messages }`.

- `WireSender: Clone` (control handle, `&self` methods): stdin command
  senders, `Arc<ProcessControl>`, the exit receiver, a latch receiver.
  Methods `write`, `close_input`, `close`, `wait_exit`, `failure()`.
- `WireMessages` (unique, owns the connection's life): the message receiver
  and one `pending` entry, the task `JoinSet`, the stop signal, a `RawWriter`
  for the barrier. Methods `next_message(&mut self)`, `finish(self,
  deadline)`.

### 7.2 Readers

1. A reader reads up to 64 KiB into its fixed buffer.
2. stderr: each chunk is one raw unit (`Payload::stage`, then
   `RawWriter::submit`, not awaited).
3. stdout: a pure `LineSplitter` splits on LF. Before appending `n` bytes
   to the unfinished message, the reader `try_acquire`s `n` of staging plus
   global; at LF the message is frozen without a copy into an `Arc<Payload>`
   (topped up to the 512 B minimum), submitted to raw, counted against the
   64-message / 4 MiB residency, and `try_send` into the queue with its ack.
   Two constructors, so no charged buffer is copied (round-4 Astra F4):
   `Payload::stage(&[u8])` acquires and copies (stderr chunks, stdin
   pieces); `Payload::freeze(Vec<u8>, StagingPermit)` consumes a buffer whose
   bytes were already charged (stdout messages).
4. A line over 1 MiB is `MessageTooLarge`: the prefix stays in raw and the
   reader switches to discard mode. A tail at EOF is one raw unit and the
   in-band end `Unterminated`.
5. A reader never awaits a consumer, the raw worker or the Store.
6. Discard mode (after the first failure): keep reading to EOF; stage 64 KiB
   units while only the budget refused (each refusal marks
   `raw_incomplete`); stop staging once raw is known broken; count discarded
   bytes.

### 7.3 Stdin writer task

- One task per connection in the `JoinSet`, owning `ChildStdin`, with a data
  queue `mpsc(1)` and a control queue `mpsc(8)` (at most 64 KiB, duplicate
  interrupt or close coalesced; runtime §8).
- `WireSender::write(message, deadline)` enqueues and returns a
  `PendingWrite` (a future over the reply `oneshot`, cancel-safe: the task
  owns the write). Route keeps it pinned in its `select!` and services every
  control arm meanwhile (§8).
- The task writes piece by piece, records each written prefix as a stdin raw
  unit, keeps at most four pieces unacknowledged, awaits the remaining acks
  before `Written`, and selects every await on the stop signal and the
  command's deadline. A partial write then deadline closes stdin and answers
  `Indeterminate`; a group kill surfaces as `EPIPE`.
- **Streamed start.** `OutboundMessage::Start{prefix, prompt: String,
  suffix}`: the prompt is cut at char boundaries into 16 KiB slices, each
  escaped into a small buffer (at most 96 KiB per piece). No whole second copy
  exists (runtime §8 allows the fake start beyond 1 MiB). [V] Today
  `FakeStart` builds one byte string (`crates/via-routes/src/lib.rs:47-71`).
- `close_input` is an idempotent control command acknowledged after the
  endpoint drops.

### 7.4 One health state

`ConnectionLatch` (`watch::Sender<LatchState { first: Option<FailureCause>,
raw_incomplete: bool }>`) is the only owner of a connection's failure state.
Written with `send_if_modified` by readers, the stdin writer, `next_message`
(a failed ack) and the raw worker's `RawFaultSink` (including its death
guard). First failure wins; `raw_incomplete` only goes true.
`WireConnection.evidence` is deleted. Every wait selects on the latch.

| Condition | Where | Outcome |
|---|---|---|
| staging refused | reader | `Reader(Overflow)`, `raw_incomplete`; not a Store failure (A1) |
| queue full | stdout reader | `Reader(Overflow)`; the message is in raw |
| message over 1 MiB | stdout reader | `Reader(MessageTooLarge)` |
| raw write or sync fails | raw worker | `Raw(Raw)`, `raw_incomplete`; T3 row 6 |
| raw worker dead | death guard | `Raw(WriterLost)`; Core latches |
| pipe read error / stdin write error | reader / writer | `Reader(Transport)` / `Writer(Io)` |
| EOF | reader | in-band, not a failure |

### 7.5 Consumer rules

`next_message` selects, biased: force, Route's wake, the `pending` entry's
ack (else the queue), the latch. It never reads a pipe; a wake or cancel
loses nothing (the popped entry moves to `pending` before its ack is
awaited). A message reaches Route only after its unit's durable ack, in
stream order. After a latch failure, queued messages are dropped; every
committed event still cites a durable span, and the terminal carries
`raw_log.incomplete` when a unit failed. `read_either` and `drain_to_eof` are
deleted.

### 7.6 `finish`: one deadline, then adoption

`WireMessages::finish(self, deadline) -> FinishReport { raw, adopted }` is
the only normal end:

1. The stop is already ordered (Route's close, Host's kill, or vendor exit);
   readers keep reading the tail.
2. Drain until both readers reach EOF and the writer ends, or
   `deadline − 250 ms`.
3. If anything still runs: mark `raw_incomplete`, set the stop signal,
   `abort_all()` (every await is cancel-safe).
4. Barrier and join together until `deadline`.
5. At `deadline`: an unanswered barrier marks `raw_incomplete`; unjoined
   tasks move into `WireRuntime::adopt(set)`; `finish` returns by
   `deadline`.
6. `WireRuntime::shutdown` (`crates/via-wire/src/runtime.rs:96` [V]) joins
   adopted sets after Host's shutdown and reports stragglers in
   `WireShutdown.pending_tasks`.

Every `run_turn` exit calls `finish` with one absolute deadline (the graceful
close's `close_by`, the force close's cleanup deadline, `failed.close_by`, or
`LAUNCH_DRAIN` for an open failure). `Drop` without `finish` aborts, adopts,
and bumps a test-only `wire::fallback_drops()` counter that every normal test
asserts is zero.

## 8. Serviceability: no wait hides a control

- **Route.** Its loop keeps pinned: the pending start or interrupt
  `PendingWrite`, and the hop `reserve()` future (cancel-safe). It selects,
  biased: (1) daemon force; (2) turn deadline; (3) the latch; (4)
  `hop.closed()` → `Overflow` (§2.3); (5) Route's wake → `Control::on_wake`,
  which at most enqueues one interrupt `PendingWrite` and never awaits a
  write, or returns `Stopped` at `force_at`; (6) a pending write completing;
  (7) the reserve completing, which sends; (8) `next_message`. [V] Today
  `on_wake` awaits `write_message` inline
  (`crates/via-routes/src/runtime.rs:469`), and `forward` selects only on send
  and force (`:738-757`).
- **Adapter.** The pending delivery (charge acquire, stall timer, send) is a
  pinned future polled beside `route` and the hop; `recv` is disabled while
  it is pending, so order holds. When `route` completes first, undelivered
  data turns an `Ok` into `overflow`, as today.
- **Core.** `while_polling(&mut execute, &mut early, fut)` awaits a Store
  commit while it keeps polling the boxed adapter future and stores an early
  result. It wraps every commit in the drive loop: acceptance, events, step
  rows, `observe_order`. [V] Today a commit arm runs to completion without
  polling the adapter (`drive.rs:1292-1320`). Route and the Adapter therefore
  keep servicing orders and the stall timer while Core waits on the Store.

## 9. Connection layer and C1 ingestion (F5)

[V] Today `admit` spawns a task with no socket cap
(`crates/via-cli/src/server/serving.rs:245`); the line reader buffers up to
16 MiB with no budget or deadline, breaks silently on oversize
(`crates/via-cli/src/server/dispatch.rs:45-46`) and builds a whole-request
`Value` (`:48`).

### 9.1 Sockets, input, oversize, replies

- The accept loop owns `Semaphore(32)`; the 33rd peer is closed at once
  without bytes. One request in flight per socket.
- The connection task is sequential: read one line, handle it, write the
  reply, repeat. With no notifications it needs no reader/handler/writer
  split.
- Input: `input` charged in 64 KiB steps before each read; one absolute
  deadline of 5 s from a line's first byte to its LF (budget exhaustion or
  the deadline closes the connection); an idle connection has none.
- A line over 16 MiB (LF included) gets one bounded `parse_error` write (2 s)
  and the connection closes.
- A request `id` over 256 B encoded is `invalid_request` (A31).
- Each reply is written within `REPLY_WRITE` = 10 s from its first byte, else
  the connection closes (A32).
- Seam: `VIA_TEST_PARTIAL_LINE_MS`.

### 9.2 JSON limits (A10) and no peer `Value`

- `json_limits::count` is a `DeserializeSeed` over `serde_json`'s own parser
  (the pattern of `retry_identity`, `api.rs:804-858` [V]) with a depth counter
  and a node count; keys read literally; it builds nothing and fails at depth
  65 or node 65,537. It runs on every C1 line and every vendor message before
  any typed decode.
- No `Value` is built from peer bytes. [V] With the workspace's `raw_value`
  feature (`Cargo.toml:16`), `Value`'s deserializer re-parses a member keyed
  `$serde_json::private::RawValue` (`serde_json-1.0.151/src/value/de.rs:131-134`),
  which could allocate far beyond the counted document.
- Free-form C1 members (`vendor`, `bound`, `effort`, `output_schema`,
  `max_steps`, `instructions`, `require`) are `Box<RawValue>`, inspected only
  by `json_limits::shape` (null, object of empty objects, or other) and
  `string_list` (exact-capacity `Vec<String>`). The adversarial
  private-key document is a unit and C1 test.
- §2.2 rule 5 bans buffering serde attributes on every peer-fed type.
- A C1 excess is `parse_error`; a vendor excess is `RouteError::Protocol`.
- The module lives in via-store (`crates/via-store/src/json_limits.rs`) so
  via-routes and via-core use it with no new edge.

### 9.3 Decode once; stream the identity

Per request line, in order, with §5.2's charges: (1) counting pass; (2)
borrowed envelope `{jsonrpc, id, method, params}` as `&RawValue` (no
allocation, `read.rs:636-650`); (3) `id` checked (string, number or null, at
most 256 B) and copied, `method` decoded; (4) for keyed calls, the identity
pass finds the handle span and forms three borrowed pieces; at most
`INLINE_MAX` becomes one `Vec`, else the pieces stream into a `BlobWriter`;
(5) the typed decode of the method's strict DTO; (6) a prompt over
`INLINE_MAX` streams from the decoded `String` into a `BlobWriter`; (7) drop
and release.

## 10. Spawn members, `cwd`, counts and `status` members [kept]

### 10.1 Spawn members

[V] `SpawnParams` has none of these today, so `deny_unknown_fields` refuses
them. Added: `cwd` (at most 4096 bytes, absolute, an existing directory, else
`invalid_params`); `label` (at most 120 bytes); `allow_untested` (bool,
default false); `instructions` (`Box<RawValue>`; the fake refuses any by
name); `require` (`Box<RawValue>` expanded by `string_list`; each name checked
against `Capabilities::fake()`; the first unmet is refused by name; not an
array of strings is `invalid_params`). Frozen `params` becomes `{harness,
model, cwd, allow_untested}`; an omitted `cwd` freezes the fake's configured
default (the daemon's start directory, `crates/via-adapters/src/fake_config.rs:44`
[V]), so it is always absolute. `label` goes to its v6 column.

### 10.2 `cwd` is applied

`FakeConfig::process_spec` (`fake_config.rs:53`) takes `cwd: &Path` in place
of `self.cwd` (`:75` [V]); `QueuedTurn` gains `cwd` from the session's
`params`; the drive passes it to `execute`; the envelope reports it
(`terminal.rs:67`, `cwd: None` today [V]); `status` reports the same value.
Test: the fake agent reports its working directory (a new `ReportCwd` step),
and the envelope and `status` equal it.

### 10.3 `daemon/status`: `started_at` and counts (A4)

- `started_at` is set once in `Engine::open`.
- `closing` = `Sessions.closing` (replacing `engine.rs:110` [V]); `active` =
  distinct sessions in `Unresolved` **not** in the closing set; `open` =
  `Sessions.open`, seeded once after recovery with `COUNT(*) WHERE state !=
  'closed'`, +1 at spawn receipt, −1 on each closed-now Store answer; `idle`
  = `open − closing − active`, saturating.
- `idle + active + closing = open` while the tally is exact; an uncertain
  close leaves `open` stale until restart.

### 10.4 `status` durable members (A16, A23)

| Member | Source |
|---|---|
| `session_id`, `state`, `admission`, `harness`, `label`, `created_at`, `updated_at` | `sessions` (v6) |
| `model`, `cwd` | `json_extract(params, …)` |
| `route` | `json_extract(receipt, '$.route')` |
| `vendor_session_id`, `vendor_identity_verified` | `null`, `false` (A16) |
| `process.alive` | positive evidence only: the Host ledger holds a live control for an anchor the session owns, in phase `Armed` (`host.rs:104-111` [V]), whose exit watch has **not** reported an exit. The last clause fixes Sol's stale-liveness finding: `track_control` publishes the exit (`host.rs:1319` [V]) while the ledger phase stays `Armed`; `LiveControl` gains a clone of that exit receiver and `Host::live_armed(&ids)` reads it under the ledger mutex. After a restart no control is live, so `false` |
| `process.cleanup` (A23) | `"uncertain"` when any group owned by the session's turns lacks a durable absence proof (`anchors` with `absence_time IS NULL`), else `"quiescent"` |
| `process.idle_since` | `null` (A16) |
| `active_turn` | the `running` turn: `phase` `accepted` if `accepted_at` else `submitting`; `started_at`; `last_event_seq` from `(session_id, turn, seq)`; `cancel` from its first `cancel.requested` event |
| `queue` | at most 8 queued turns `{n, op_key, queued_at, effective}` |
| `turns` | the newest 64 `{n, state}`, `revision: 0` on terminal turns |
| `progress`, `steps` | §4.2 (A26) |

Core composes the reply: the Store snapshot on the Public lane, `progress`
from the slot, and `alive` through the same pass-through chain as
`pending_cleanup` [V]: `crates/via-core/src/engine.rs:406` →
`crates/via-adapters/src/runtime.rs:252` → `crates/via-routes/src/runtime.rs:267`
→ `crates/via-wire/src/runtime.rs:139` → `crates/via-host/src/host.rs:865`.
Each hop gains `live_armed(&ids) -> bool`; the whole chain lands together
(round-4 Astra F4).

### 10.5 Plain conformance

- A3: the fake wall default becomes C1's 3,600,000 ms
  (`crates/via-core/src/api.rs:884` is 30,000 today [V]).
- A9: a nested `null` in `deadlines.*` is `invalid_params` (`api.rs:88`
  `DeadlineParams` reads it as omitted today [V]).

## 11. Amendments

Numbered `T4-A<n>`; `A<n>` in this document. New ones start at A24. Each gives
the change, the exact replacement text, and the restatements a grep found
(`follow`, `subscri`, `event_end`, `outbox`, `notification`, the removed event
types, `event_seq`, `1 MiB envelope`, `Store transaction`). The orchestrator
applies spec edits after review; historical reports and decision files are
not edited.

### 11.1 Status of earlier amendments

| # | Status in round 5 |
|---|---|
| A1 staging overflow is `incomplete`, not a Store failure | kept (§5.1, §7.4) |
| A2 lanes and the 8 MiB partition | kept; byte split updated by A30 (§6.1) |
| A3 fake wall default 3,600,000 ms | kept |
| A4 disjoint session counts | kept (§10.3) |
| A6 "session actor" is `Slot`/`Head` for follow registration | **withdrawn**: follow is removed (A25) |
| A9 nested `null` in `deadlines.*` | kept |
| A10 a JSON node is any value or key | kept (§9.2) |
| A12 `list` two-phase paging | **kept unchanged** (§6.9, proof §11.3) |
| A13 coordination primitives | kept |
| A14 `cwd`, `allow_untested` in frozen `params` | kept |
| A15 `ended_seq`, no recompute | kept |
| A16 S1 definitions of `status`/`list` members | kept; extended by A26 |
| A19 stall as a stop order with cause `overflow` | **withdrawn**: the stall closes Route's hop (A29, §2.3) |
| A21 | withdrawn in round 4 |
| A22 `ENVELOPE_MAX` 704 KiB | **withdrawn**: R6 keeps 1 MiB; replaced by A30 |
| A23 `process.cleanup` | kept; `alive` excludes a reported exit (§10.4) |

### 11.2 New amendments

**T4-A24. The durable event set (R1, R2).** Amends C1 §6, §5, §3.5; C2 §4;
`.repo-context/CONTEXT.md`.

C1 §6 heading: replace "## 6. Canonical event stream" with "## 6. Durable
events". C1 §6.1 type table: delete the rows `assistant.text`,
`reasoning.summary`; `tool.started` / `tool.ended`; `file.changed`;
`usage.updated`; `vendor.other`. The remaining rows are unchanged. Add
directly after the table:

> Events are durable records only: an event exists when crash recovery or the
> envelope depends on it. Model text, reasoning, tool calls, usage updates,
> file changes and unknown vendor messages are not events; their exact bytes
> are in the raw log (`logs`, §3.12), and a running turn's progress is in
> `status` (§3.7).

C1 §6.1 last paragraph: replace "Rust: `#[serde(tag = "type")]`, tags set
with `rename`, unknown types kept as `Other { type, payload }`." with "Rust:
`#[serde(tag = "type")]` on the serialize side, tags set with `rename`; a
client keeps an unknown type as `Other { type, payload }`." (A daemon decodes
no event from a peer; the internally tagged form is never a peer-fed
`Deserialize` in the daemon, §2.2 rule 5.)

C1 §5 field table: the `raw_spans`, `denied_actions` and
`auto_declined_requests` rows are unchanged; their `event_seq` members and
the event `raw_ref`s they rely on name durable events, which still exist.
Add after the `usage` row:

> | `steps` | model steps in the turn: the vendor's count when it reports one, else VIA's count (§3.7 step rule); `null` if the turn never started |
> | `events` | `{first_seq, last_seq, count}` of the turn's durable events (§6.1) |

C1 §3.5 and §7.3: unchanged. A late tool completion is now raw-log evidence
only, which is what "late evidence that does not rewrite the envelope"
already means.

C2 §4 first paragraph: see A29.

`.repo-context/CONTEXT.md`, replace the **Event** definition with:

> VIA's durable record of a lifecycle, control or safety fact in a session,
> such as `turn.started`, `cancel.requested`, `action.denied` or `turn.ended`
> (full list in VIA API §6). It is harness-neutral. Most events belong to one
> turn; a few (`session.opened`, `session.closed`) belong to the session.
> Each event has a dense per-session `seq`. Core commits it to the Store, and
> callers page it with `events`. Model text, tool calls and usage are not
> events: they drive the progress snapshot and step rows, and their bytes
> stay in the raw log.

and in **Vendor message**, replace "An unknown message type becomes a
`vendor.other` event" with "An unknown message type is kept only in the raw
log". In **Step**, append: "VIA counts a step each time the model produces
output after tool results, the same way for every vendor, and records one
`steps` row per step." In **Event**, the replaced text also drops "(page or
follow)" and "One vendor message yields zero or more events". Add, after
**Vendor message**, the requirements' term: "**Observation**: What an
adapter reports from one vendor message. Core turns an observation into a
durable event, a progress update, or envelope accumulation. _Avoid_: event
(reserved for durable records)."

Restatements: C1 summary table row `Events` (keep; still true);
`docs/specs/adapter-contract.md:80` (rule 6), `:273-276`, `:453` (A29);
`docs/specs/runtime-contracts.md:193` (A33); `docs/specs/vendors/claude-code.md:233-244`,
`codex.md:250-265`, `opencode.md:521`, `:527-531` (A33);
`docs/workstreams/rust-foundation/t3/design.md:609-615` (idle progress list:
now "acceptance, and progress items with model output or a tool start or
end") and `:1754` (the `s1_f12_event_not_committed_…` test commits
`assistant.text`; it moves to a step-row commit for the stop half and to
`cancel.requested` for the seq-reuse half); code
`crates/via-core/src/engine/drive.rs:1844-1884` (`event_body`),
`:1775-1786` (`progress`).

**T4-A25. No follow stream (R5).** Amends C1 summary, §1, §3.8, §3.11, §6.2–6.3,
§7.6, §10; runtime §1, §2, §6, §7, §8, §9, §10, §11; S1 plan.

C1 §3.11, replace the whole section with:

> ### 3.11 `events` — page
>
> `via events <session|turn> [--after SEQ] [--limit N] [--types T,…]`
>
> Params: `session` or `turn`, `after?` (default 0), `limit?` (default 200,
> max 1000), `types?`. Result `{events, next_after, more: bool,
> earliest_seq}`. The page is a bounded Store scan in `seq` order from
> `after`, filtered by `types` (gaps in `seq` are expected under a filter). It
> stops at both the requested count and 1 MiB encoded bytes. `next_after` is
> the last scanned seq, including filtered-out events; `more` uses the
> committed head captured with the page. An individual result exceeding the
> response bound is `admission_refused`, never a truncated success. History
> pruned by retention: `history_pruned` error carrying `earliest_seq` when
> `after < earliest_seq - 1`. There is no follow stream: callers poll
> `status` (§3.7) for progress and `wait` (§3.8) for the end of a turn.

C1 §1 Transport, replace "Requests carry `id`; notifications flow daemon →
client only for follow (§3.11). No batches." with "Requests carry `id` (A31);
the daemon sends no notifications. No batches."

C1 summary method table, row `events`, `logs`: replace "canonical events
(page/follow), raw excerpts" with "durable events (page), raw excerpts".

C1 §6.2–6.3 heading and text: replace with "### 6.2 Ordering" and "Per
session FIFO in `seq`; no promise across sessions (D4). `turn.ended` is the
last non-late event of its turn."

C1 §7.6 last row: replace "followers whose subscription ended must poll
`result`" with "a caller that already read the result must read it again".

C1 §10: in "Confirmed by review (Astra): Q1 session-wide follow; Q2 …",
delete "Q1 session-wide follow; ". Row Q7: replace "Outbox and channel sizes
(1000 events; C2 A1 limits)" with "Channel sizes (C2 A1 limits)".

C1 §3.8, append to the first paragraph: "`wait` is the only blocking read;
there is no follow stream. A caller that wants progress polls `status`
(§3.7). Closing the connection of a pending `wait` releases only that
waiter; the turn is unaffected."

Runtime §9, replace the whole section with:

> ## 9. Paging, polling and slow peers
>
> Store is the sole event source. A page is one bounded read transaction
> that returns the page, the scan cursor and the committed head. `next_after`
> records the last scanned seq, not just the last matched event; `more` uses
> the captured head. Filtering can yield an empty page that still advances.
> Page byte limit is 1 MiB. Keep read transactions short (one bounded page).
>
> There is no follow stream and no subscription. Callers poll `status` for
> the in-memory progress snapshot and step history, and use `wait` for a
> turn's end. Cancelling a wait request only releases that waiter; it cannot
> cancel the turn.
>
> A reply is written within 10 s of its first byte, otherwise the socket
> closes (C1 §1). A peer that never reads therefore holds its reply memory for
> at most that long. `logs` returns raw excerpts only from connections that
> belong to the addressed session and turn (C1 §3.12).

Runtime §1: delete the sentence "Live observers consume durable events,
never an independent best-effort copy." and replace limit 2 with "2. A
blocked peer cannot be guaranteed a reply. The daemon closes the socket
after the 10 s reply deadline; the caller retries the read."

Runtime §2 owners table, Core row: replace "seq/state decisions,
subscribers" with "seq/state decisions, progress snapshots".

Runtime §6 write ordering item 5: replace "then wakes waiters/followers"
with "then wakes waiters". §6 checkpoint paragraph: replace "Keep read
transactions short (one bounded page); no follower holds a transaction open
while waiting on a socket." with "Keep read transactions short (one bounded
page); no read holds a transaction open while waiting on a socket."

Runtime §7 table: delete the row "Following affected history".

Runtime §8 table: delete the rows "Subscriber outbox" and "Subscribers".
Replace "AST charges, copies and outboxes" with "AST charges and copies".

Runtime §10 row "C1 §3.11": append "Superseded by T4-A25: follow removed."

Runtime §11 table, replace the row "blocked socket, replay boundary barrier,
unsubscribe barrier | F25/F26 …" with "`core.progress.publish`, crash after
a step commit | F25/F26 (replaced): status answers from memory during a flood
without a Store round trip; step rows survive a crash up to the last
committed step".

S1 plan (`docs/workstreams/rust-foundation/s1-plan.md`): line 19 and line 207
drop "follow", "unsubscribe" and "subscription cleanup"; rows F25 and F26
(`:88-89`) and the F25 note (`:134`) are replaced as proposed in Q-R5-5.

T3 design `§7.6` (`t3/design.md:1414-1417`) and the quoted runtime row
(`:1919`): obsolete; no `event_end` exists.

**T4-A26. `status` returns progress and step history (R3, R4, R5).** Amends
C1 §3.7.

Replace C1 §3.7 with:

> ### 3.7 `status`
>
> `via status <session> [--turn N] [--after-step N] [--limit N]`
>
> Params: `session`, `turn?` (turn number; default the running turn, else
> the latest), `after_step?` (default 0), `limit?` (default 100, max 1000).
>
> ```json
> {"session_id":"s_7f3k9q2mzr4c","state":"active","admission":"open","harness":"codex","model":"gpt-6-sol",
>  "route":"codex-app-server","vendor_session_id":"019…","vendor_identity_verified":true,"cwd":"/work/repo",
>  "process":{"alive":true,"cleanup":"quiescent","idle_since":null},
>  "active_turn":{"n":2,"state":"running","phase":"accepted","started_at":"…","last_event_seq":57,"cancel":null},
>  "progress":{"turn":2,"current_step":4,"phase":"tools","running_tools":["shell"],"last_activity_at":"…",
>              "tokens":{"total":18200,"scope":"vendor_interval"}},
>  "steps":{"turn":2,"items":[{"step":1,"started_at":"…","ended_at":"…","tokens":5100}],"next_after":1,"more":true},
>  "queue":[{"n":3,"op_key":"k-17","queued_at":"…","effective":{…}}],
>  "turns":[{"n":1,"state":"completed","revision":0},{"n":2,"state":"running"},{"n":3,"state":"queued"}],
>  "label":null,"created_at":"…","updated_at":"…"}
> ```
>
> `progress` is an in-memory snapshot of the running turn, read without a
> Store round trip, and `null` when no turn is running in this daemon.
> `current_step` counts model steps: 0 before the vendor accepts the turn, 1
> after, and one more each time the model produces output after tool
> results, derived the same way for every vendor. `phase` is `tools` while a
> tool the vendor started has not ended, else `model`. `running_tools` holds
> at most 64 tool names, never inputs or outputs. `last_activity_at` is the
> arrival time of the last vendor message. `tokens` is an approximate running
> total of completed steps, updated once per step, labelled with the route's
> declared token scope (§4.1), or `null` before the first sample. The snapshot
> ends with the turn; the envelope's `steps` and `usage` hold the final
> figures.
>
> `steps` pages the durable step history of the selected turn, running or
> finished: one item per completed step, ordered by `step`, with the step's
> tokens under the same scope. After a daemon crash the history holds every
> step whose row was committed; the step then in progress is recoverable only
> from the raw log. A turn records at most 10,000 steps; beyond that a
> `warning` with code `step_history_truncated` is committed once and
> `progress.current_step` keeps counting.
>
> `process.alive` is true only on positive evidence that the vendor process
> is live; `process.cleanup` is `uncertain` when any process group of the
> session lacks a proof of absence, else `quiescent` (T4-A23).
> `vendor_session_id` … (the existing paragraph continues unchanged).

C1 §5 warnings row: add `step_history_truncated`.

Restatements: `docs/specs/via-api-v1.md:571` (idle shutdown: `alive`
becomes false; unchanged); A16's list (`:274-297`).

**T4-A27. `logs` returns raw excerpts by byte cursor (R5).** Amends C1 §3.12,
runtime §9.

Replace C1 §3.12 with:

> ### 3.12 `logs` — raw-log excerpts
>
> `via logs <session|turn> [--cursor C] [--limit N]`. Returns the raw bytes
> of the connections opened for the addressed turn (or for every turn of the
> session), in connection order, undecoded:
> `{entries: [{connection_id, stream, offset, len, text}], next_cursor}`.
> `stream` is `stdout`, `stderr` or `stdin`; `text` is the bytes as lossy
> UTF-8, which VIA does not interpret; `offset` and `len` locate them in the
> connection's raw log. `cursor` is opaque; `next_cursor` is `null` at the
> current end. `limit` counts entries (default 100, max 256). A page returns
> at most 128 KiB of raw bytes, so it always fits the 1 MiB bound; a unit
> larger than that spans pages. Never another session's traffic (D4): only
> connections owned by the addressed session are read. Missing or corrupt raw
> evidence is `store_error`.

Runtime §9 last sentence ("`logs` resolves only each selected event's
validated raw reference; never expand to the connection's bounding spans"):
replaced by A25's text. Restatements: C1 §5 `raw_spans` row (unchanged);
`docs/specs/vendors/codex.md:223-224` ("Raw extraction uses individual event
`raw_ref`s, never shared-connection bounding spans"): keep, and add "`logs`
is not served for a shared connection until per-session attribution exists
(T4 design §4.4)".

**T4-A28. `steps` table; event columns.** Amends runtime §6.

Runtime §6 target table: replace the `events` row with "`events`: separate
`turn` and `type` columns (late and time stay in the event JSON) |
`via-jm4.7.8`"; add a row "`steps`: `(session_id, turn, step, started_ms,
ended_ms, tokens)`, primary key `(session_id, turn, step)`, one row per
completed model step, written by the single writer, the last in the terminal
transaction; a session's rows are removed by one keyed delete |
`via-jm4.7.8`". Runtime §6 write-ordering item 4: replace "Adapter emits
observation; Core commits acceptance or events" with "Adapter emits
observation; Core commits acceptance, durable events or a step row, or folds
it into the progress snapshot".

**T4-A29. Observations after R2; the stall closes the hop.** Amends C2
summary A1 row, §1 rule 6, §2, §4, §7; runtime §8; C1 §8.2.

C2 summary table A1 row, replace the decision text with:

> Backpressure: per-session observation channel of 1024 items and 4 MiB; a
> full channel blocks only that session's normalizer; control and sticky
> health travel separately and stay serviceable; Core failing to drain for
> `event_stall_ms` (10 s) fails the turn `overflow`: the adapter closes its
> route hop and the route fails the connection, which interrupts the vendor;
> L5 staging overflow fails the connection (coding-style §5). A known
> observation payload is at most 256 KiB encoded, else protocol failure with
> raw evidence; IDs, names, stop reasons and codes are at most 1 KiB each.
> Unknown messages produce no payload.

C2 §1 rule 6: replace with "6. Unknown vendor notifications are kept only in
the raw log and count as activity; a malformed known message is a `protocol`
observation."

C2 §2 "Independent lanes" bullet: replace "The 1024-item observation queue
also has a 4 MiB budget, and no payload exceeds 256 KiB encoded. Split text
at UTF-8 boundaries while preserving order; another oversize known payload
fails protocol with raw evidence. An unknown payload retains at most 16 KiB
with an explicit truncation marker." with "The 1024-item observation queue
also has a 4 MiB budget; a known payload over 256 KiB encoded fails protocol
with raw evidence."

C2 §4, replace the first paragraph with:

> `Observation` = the C1 event payloads Core commits (`action.denied`,
> `vendor.request_declined`, `steer.delivered`, `warning`), one `progress`
> item per vendor message, plus internal ones Core turns into commits:

and add the row:

> | `progress` | `at`, `model: bool`, `tools_started: [(id, name)]`, `tools_ended: [id]`, `usage?: (key?, total)` | no commit: Core folds it into the running turn's progress snapshot and commits a `steps` row when a step ends (C1 §3.7). `model` marks model output (text, reasoning or a tool request); `usage` is an interval sample, never a cumulative total. A message with no mark sends an empty item, which records activity only |

C2 §7 item 7: replace with "7. Every committed observation resolves its
`raw_ref`, except declared synthesized ones." Item 8: replace
"`vendor.other`" with "raw-log-only activity". Item 12: replace "a stall past
`event_stall_ms` yields an interrupt and `overflow`" with "a stall past
`event_stall_ms` closes the route hop, and the route fails the connection
`overflow`".

Runtime §8, row "C2 observation payload": replace with "256 KiB encoded for
a known payload; IDs, names, stop reasons and codes 1 KiB | Fail protocol
with raw evidence; unknown messages keep no payload". Row "C2 observations":
replace "Core fails `overflow` and interrupts (A1)" with "the adapter closes
the route hop and the route fails the connection `overflow` (A1)".

C1 §8.2 `overflow` row: replace "this session's event channel stalled past
its limit (C2 A1)" with "this session's observation channel stalled past
its limit, or the turn's events or envelope exceeded 1 MiB (C2 A1, §5)".

Withdraws A19: T3's `StopCause` set and T3's text are unchanged (A19 was
never applied). Restatements in code: `crates/via-adapters/src/runtime.rs:312-331` (`deliver`),
`crates/via-routes/src/runtime.rs:738-757` (`forward`).

**T4-A30. The envelope stays 1 MiB; the transaction cap excludes it (R6).**
Amends runtime §8; C1 §5.

- **The conflict.** Runtime §8 caps a transaction at 1 MiB of payload and
  never splits a lifecycle batch. R6 keeps C1 §5's 1 MiB envelope. A terminal
  transaction carries the envelope plus `turn.ended` and owed records, and
  the failure-resolution batch adds up to 8 cancellations, so both cannot
  hold. Round 4 narrowed the envelope (A22); R6 rules that out.
- **The change.** Runtime §8 row "Store transaction", replace with:
  "at most 128 events and 1 MiB payload, not counting the one terminal
  envelope a transaction may carry (itself at most 1 MiB, C1 §5) | Split
  event batches without splitting a lifecycle atomic batch; refuse a larger
  request before it is queued".
- C1 §5, replace "On overflow Core fails the turn with class `overflow`,
  persists a bounded failure summary and leaves the raw log as evidence" with
  "On overflow Core fails the turn with class `overflow` and persists a
  bounded failure summary: `final_text` empty, `structured_output` null,
  `denied_actions` and `auto_declined_requests` empty (their events remain),
  every other member unchanged; the raw log remains the evidence".
- **Proof.** A normal terminal is at most `ENVELOPE_MAX + TERMINAL_EXTRAS_MAX`
  and the failure batch at most `ENVELOPE_MAX + TERMINAL_EXTRAS_MAX + 8 ×
  CANCEL_RECORD_MAX` = 1 MiB + 320 KiB; each summand is measured where it is
  built (§6.5); `LATCH_BYTES` covers the largest (§6.1). The summary is
  bounded because every vendor short field is at most 1 KiB (§2.2).
- **Cost.** Terminal transactions reach 1.31 MiB, and the Latch lane reserves
  that; the ordinary lanes keep 4.7 MiB.
- Restatements: `docs/specs/runtime-contracts.md:1020` (the Store
  transaction row, replaced above), `:1025` (Envelope row: unchanged, 1 MiB); `docs/specs/vendors/claude-code.md:304`,
  `codex.md:263` ("1 MiB envelope": unchanged); round-4 A22 (withdrawn).

**T4-A31. Request `id` at most 256 bytes.** Amends C1 §1.

C1 §1 Transport, add: "A request `id` is a string, a number or `null`, at most
256 bytes encoded; a longer one is `invalid_request`." Reason: every reply's
wrapper is then under 512 B, so each page and result bound is a constant and
no request can make a bounded item unservable (round-4 Sol minor; Astra's
page-refusal case).

**T4-A32. A reply is written within 10 s.** Amends C1 §1; runtime §8.

C1 §1 Transport, add: "The daemon writes each reply within 10 s of its first
byte; a peer that does not read it in that time is disconnected." Runtime §8
row "Socket response serialization": append "| each reply written within
10 s, else the socket closes". Reason: with no follow stream this is the only
per-socket memory a peer can hold; it bounds it in time as well as size, so a
peer that never reads cannot hold reply memory against dispatch (§5.1).

**T4-A33. Fake and vendor observation mappings.** Amends runtime §3.1; the
vendor specs.

Runtime §3.1: replace "An unknown notification tag without `id` follows the
existing bounded `vendor.other` path." with "An unknown notification tag
without `id` is raw-log-only activity." In the same paragraph, replace "§4
message splitting, §8 structure/payload limits, text splitting and raw
durability still apply." with "§4 message splitting, §8 structure/payload
limits and raw durability still apply." In the tool-variant paragraph,
replace "All these fields are strings except optional signed-integer
`exit_code`; tool status is `completed`, `failed` or `cancelled`. They map to
existing C2 observations, not new C1 event types." with:

> All these fields are strings except optional signed-integer `exit_code`;
> tool status is `completed`, `failed` or `cancelled`. VIA reads only `vendor_turn_id` from `text`, and `tool_id` and `name` from
> the tool messages; `text`, `input_summary`, `output_summary`, `status` and
> `exit_code` are optional and not read. A fourth progress tag is
> `{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":120}` (a
> non-negative integer): one interval sample. These map to C2 `progress`
> items (C2 §4), not C1 events.

`docs/specs/vendors/claude-code.md` §5 table, replace the rows:

> | `assistant.message.content` text | `progress` with `model`; final text comes from `result` |
> | assistant `tool_use` | `progress` with `model` and `tools_started (id, name)`; retain the open-item set; no input summary |
> | user `tool_result` | `progress` with `tools_ended (tool ID)`; error/refusal remains error; unmatched IDs are protocol evidence |
> | assistant `message.usage` | `progress` `usage` keyed by message ID (unprobed: the pinned packet probes only `result.usage`) |
> | unknown notification | raw log only; activity; cannot advance lifecycle or the idle timer |

and replace "Never synthesize `file.changed` … the tool event suffices." with
"VIA does not report file changes." and delete "Do not expose private
chain-of-thought as `reasoning.summary`." (no text is exported). §6: replace
"256 KiB known observation (split text only)" with "256 KiB known
observation".

`docs/specs/vendors/codex.md` §5, replace "Normalize agent-message deltas,
completed agent text, tool starts/ends, file changes, reasoning summaries,
usage and terminal statuses using C2. Accumulate final text by item ID;
completed text replaces that item's delta accumulator instead of duplicating
it." with:

> Normalize with C2 `progress` items: `agentMessage` and `reasoning` item
> starts and deltas are `model`; a tool item's `item/started` is
> `tools_started (itemId, item type)` and its `item/completed` is
> `tools_ended`; `thread/tokenUsage/updated` `tokenUsage.last` is a `usage`
> sample. Terminal statuses map as below. For the envelope's final text only,
> accumulate completed `agentMessage` text by item ID; completed text
> replaces that item's delta accumulator instead of duplicating it.

and replace "unknown notifications become `vendor.other` retaining at most
16 KiB with explicit truncation" with "unknown notifications are raw-log-only
activity", and "Large text splits on UTF-8 boundaries;" is deleted.

`docs/specs/vendors/opencode.md` §5, replace "Map text deltas and
authoritative part snapshots without appending the same text twice. Correlate
tools by session/message/part/call IDs; state is pending/running/completed/
error. Keep partial text distinct from final text. Unknown notification
types become bounded `vendor.other`;" with:

> Map assistant text and reasoning parts to C2 `progress` `model`, a tool
> part entering `running` to `tools_started (call ID, tool name)` and one
> entering `completed` or `error` to `tools_ended`, correlated by
> session/message/part/call IDs; each assistant message's token snapshot is
> a `usage` sample keyed by message ID (§7 one ledger). Keep final text from
> the correlated completed assistant only. Unknown notification types are
> raw-log-only activity;

and replace "`session.next.*` events are retained as bounded `vendor.other`,
never a second text/tool/usage emission" with "`session.next.*` events are
raw-log-only, never a second text/tool/usage emission".

### 11.3 Replacement proofs kept from round 4

**A12 (`list`).** Let `S0` be the first page's snapshot, `v0` its maximum
`stamp`, `w0` its maximum `ord`; the population `P = {ord ≤ w0}` is exactly
the sessions at `S0` (`ord` is set once and strictly increases). Any change to
a filter column or `updated_ms` changes the row and raises its `stamp` above
every earlier one. For X in `P`: (a) if X's stamp is still at most `v0` when
phase 1's keyset range covers its key, its key and filter values are those of
`S0`; phase 1's set only shrinks and its members' keys never change; each
page stops before a row it does not return, so the cursor passes every
remaining key once, and X is returned if it matches then. (b) If X changed
first, its stamp exceeds `v0` for good; phase 2 runs because
`EXISTS(ord ≤ w0 AND stamp > v0)`, its cursor examines every `ord` in
`(0, w0]` in exactly one page (a page stopping early sets the cursor before
the first unreturned match), so X is returned if it matches then. (c) If X
changed after phase 1 returned it, phase 2 may return it again. Termination:
phase 1 moves strictly over fixed keys of a shrinking finite set; phase 2's
integer cursor rises by at least one per page to `w0`; so at most `|P|` +
`⌈w0/1000⌉ + |P|` pages. Guarantee (C1 §3.10 as amended): every population
member that matches when the scan reaches it is returned at least once;
sessions created after the first page never are; a member may repeat.

**A14, A15, A23**: as round 4 (§10.4, §6.7): `params` is written once, so
`cwd` and `allow_untested` survive eviction and restart; `ended_seq` is
written with `turn.ended` and a CHECK makes its absence on a terminal turn
impossible; `alive` is positive evidence and `cleanup` uses T3's
close-cleanup vocabulary from the existing ledger query.

## 12. Tests (failure-first)

Each test is written first, fails on the code as found for the stated reason,
and passes once the mechanism lands. Names follow runtime §11: `s1_fNN_`,
`s1_raw_`, `s1_bounds_`, `s1_store_`, `s1_blob_`, `s1_wire_`, `s1_c1_`,
`s1_progress_`.

### 12.1 Rules

1. No fixed sleep is an ordering assertion; a sleep may only give a negative
   check time to fail after a positive barrier proved the state.
2. Every wait polls a condition to an absolute deadline or blocks on a named
   barrier (a failpoint or counter).
3. Time rules run with lowered seams (10 s stall, 5 s partial line, 10 s
   reply write); in-process units use a paused tokio clock.
4. Generated tests use a seeded generator with boundary cases, print the
   seed, and accept `VIA_TEST_SEED` (no `proptest`, A13).
5. Group commit is asserted by counting `sync_data`, never by time.
6. Heavy tests (floods, 16 MiB lines, 32 sockets, 100,000-unit lookups) are
   compiled only under `test-failpoints`; the default suite stays near two
   minutes.
7. Every scenario that starts a daemon runs through `run_scenario` with an
   `Evidence` (`crates/via-cli/tests/support/scenario.rs:111` [V]).

### 12.2 Seams

All `#[cfg(feature = "test-failpoints")]`, added to
`scripts/check-release-features.py`'s `POINTS`:
`Store::raw_sync_count()`, `Store::raw_index_reads()`,
`Store::stall_raw_worker()` (existing, `runtime.rs:1062` [V]),
`raw.sync.fail_persistent` (existing), `raw.worker.panic_after_dequeue`,
`store.writer.before_serve`, `Lanes::high_water(lane)`,
`MemoryBudget::high_water()`, `store.commit.step`, `core.observations.pause`,
`core.progress.publish`, `VIA_TEST_EVENT_STALL_MS`, `VIA_TEST_PARTIAL_LINE_MS`,
`VIA_TEST_REPLY_WRITE_MS`, `wire::fallback_drops()`, `blob.write.fail_after`,
`Store::blob_chunk_reads()`, fake-agent steps `HoldStdin`, `ReportCwd`,
`EchoPromptDigest`, and `/proc/<pid>/status` sampling for daemon and anchors.
The fake agent's `Emit`, `Gate` and `Flood` steps
(`crates/via-fake-agent/src/main.rs:41-62` [V]) already emit arbitrary lines,
including the new `usage` message.

### 12.3 Scenarios

| Test | Proves |
|---|---|
| `s1_progress_step_rule_counts_output_after_tool_results` | fake: text, tool_started, tool_ended, text, text, tool_started, tool_ended, text → `current_step` 3; rows 1–2 committed before the terminal, row 3 in the terminal transaction (asserted by `store.commit.step` barriers) |
| `s1_progress_status_reads_memory_while_store_is_held` | with the SQLite thread held at `store.writer.before_serve`, `status`'s `progress` part is computed with no Store read (a counter of Store reads) |
| `s1_progress_tokens_sum_per_step_and_label_scope` | two `usage` samples in one step replace (no key), steps add; `tokens.scope` equals the fake's declared scope |
| `s1_progress_running_tools_bound_and_phase` | 70 concurrent tool starts: 64 names, `phase` `tools` until all 70 end |
| `s1_progress_step_rows_survive_crash_to_last_commit` | daemon killed after row 2's commit and before row 3: restart shows rows 1–2 and the turn `unknown` |
| `s1_progress_step_history_cap_warns_once` | lowered cap: one `step_history_truncated` warning, no further rows, true `current_step` |
| `s1_progress_step_commit_refused_fails_turn_store` | `store.commit.step` known failure: `failed(store)`, no later rows |
| `s1_store_steps_delete_is_one_keyed_range` | `EXPLAIN QUERY PLAN` of the delete uses the primary key; two sessions interleaved, one deleted, the other intact |
| `s1_c1_status_every_member_after_eviction_and_restart` | durable members equal before and after; `progress` null after restart; `steps` paged |
| `s1_c1_status_alive_false_after_exit_before_control_drop` | Sol's stale-liveness case: exit observed, control still upgradeable → `alive` false |
| `s1_c1_events_page_filters_and_bounds` | window, `types`, `turn`, `next_after` across filtered rows, `more`; the byte bound exercised with a Store-level fixture of large rows (the C2 256 KiB limit is a separate test) |
| `s1_c1_follow_and_unsubscribe_are_refused` | `follow: true` is `invalid_params`; `unsubscribe` is `method_not_found` |
| `s1_c1_logs_pages_raw_bytes_by_cursor_and_isolates_sessions` | a 1 MiB control-character unit spans pages under 1 MiB each; a cursor for another session's connection is `invalid_params` |
| `s1_c1_request_id_over_256_bytes_is_invalid_request` | A31 |
| `s1_c1_reply_not_read_closes_the_socket_and_frees_the_permit` | A32, with `VIA_TEST_REPLY_WRITE_MS` |
| `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` | four turns, 256 MiB stdout flood; daemon peak RSS < 256 MiB, growth < 32 MiB after the first 64 MiB, each anchor ≤ 32 MiB, sum < 384 MiB, `MemoryBudget` high-water ≤ 128 MiB; `daemon/status`, `status` and a `cancel` of another turn answer within 100 ms, including with its interrupt write blocked at `HoldStdin`; the flood turn ends `failed(overflow)` with `raw_log.incomplete` |
| `s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output` | Core held at `core.observations.pause`, vendor silent after filling the channel: `overflow` at the lowered stall with no further vendor byte |
| `s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib` | the C2 bounds |
| `s1_f24_event_bytes_over_1_mib_fail_overflow_early` | Store-level fixture through a test route that emits declined requests: the channel closes and the turn fails `overflow` |
| `s1_bounds_failure_summary_is_bounded_with_1_kib_vendor_fields` | a 1 MiB control-character `final_text` with 1 KiB `stop_reason` and `vendor_code`: summary under 64 KiB; a 1 KiB + 1 field is `protocol` |
| `s1_bounds_decode_allocations_stay_within_charges` | one counting-allocator binary: C1 decode at a maximal line and Route decode at a 1 MiB message stay within §5.2 and §5.3 |
| `s1_f05_…` (oversize, depth and nodes, partial line, 33rd socket) | F5 |
| `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_raw_bytes` | F27, with a seeded splitter test |
| `s1_raw_…`, `s1_store_…`, `s1_wire_…`, `s1_blob_…` | round 4's mechanism tests for kept mechanisms: group commit by count, death guards, lanes and fence, Latch fit with the largest `cwd` (end to end with the longest creatable path; the 4096-byte bound with synthetic records, fixing Sol's impossible fixture), Public saturation at the Store level (not through sockets), binary lookup, finish adoption, torn and mismatched blobs, replay compare outside `admission` with a stalled reader |

## 13. Task 4 scope disposition (`t4/t0.md`)

| Item | Round 5 |
|---|---|
| F5 | kept (§9) |
| F24 | kept (§5, §8, §12.3) |
| F25 follower stops reading | **obsolete**: no follow stream (R5); replacement scenario proposed (Q-R5-5) |
| F26 follow during writes | **obsolete**: same; replacement proposed |
| F27 | kept (§7.2) |
| C1 `describe`, `models`, `list`, `status`, `daemon/status`, `events` page, `logs`, `serve --stdio` | kept (§4, §10) |
| C1 `events` follow, `unsubscribe`, disconnect cleanup of followers | **obsolete** (A25) |
| Per-pipe Wire readers | kept (§7.2) |
| 1024 / 4 MiB observation budget | kept, on the reduced observation set (§2.3) |
| JSON depth and nodes before a `Value` | kept (§9.2) |
| `events`/`logs` paging and turn address in strict DTOs | kept; `logs` by cursor (A27) |
| Raw staging overflow is incomplete, not Store failure | kept (A1) |
| Store requests 64 + 8 reserved | kept (A2) |
| Reserved lane for the latch batch | kept (§6.2) |
| Fake wall default | kept (A3) |
| Remaining spawn CLI options | kept (§10.1) |
| Durable `output_schema` | out of scope (§0) |
| Nested-null `deadlines.*` | kept (A9) |
| `daemon/status` `started_at` and parity | kept (§10.3) |
| Wire `read_either` selection | **obsolete**: deleted with per-pipe readers |
| Blob path, schema v6, 256 KiB rule, memory charges, Store shutdown ordering (round-1 additions) | kept; 256 KiB rule narrowed to durable payloads (A29) |

## 14. Limitations

| Limitation | Revisit when |
|---|---|
| A 16 MiB prompt peaks near 84 MiB of charges; with three or more turns running it is refused `admission_refused` | real routes measure prompt sizes |
| `running_tools` lists at most 64 of more concurrent tools; tokens are approximate and use the route's declared scope | a caller needs exact live figures |
| Claude per-step tokens rely on assistant `message.usage`, which the pinned packet has not probed | the Claude slice probes it; until then Claude `tokens` may be `null` |
| `logs` serves only private per-turn connections | the Codex (shared server) and OpenCode (per-session server) slices add per-turn and per-session attribution |
| No aggregate disk bound: runtime §6's 4 GiB quota is not built; a turn's raw log is bounded only by its wall deadline | Q-R5-6 |
| A step that ended while its row commit was in flight is lost in a crash | never; R4 accepts it |
| The open-session tally is exact only until an uncertain close | Store-failure recovery work |
| `stamp` and `ord` increase strictly only while no `sessions` row is deleted | retention (`via-jm4.18`): they then need a counter row |
| Blob verification at start is linear in referenced blob bytes | retention |
| `revision` is 0; `process.idle_since` is `null` (A16) | late evidence; vendor idle shutdown |
| Allocator overhead and SQLite buffers are uncharged; the RSS gate measures them | the RSS gate fails |
| `list` phase 2 examines every `ord` up to `w0` | session counts make it slow |
| Inferred: `Vec` doubling bounds (§5.2), the §6.2 lifecycle count, `TERMINAL_EXTRAS_MAX` and `CANCEL_RECORD_MAX` at their largest, the drive reserve total (§5.3) | the checking test fails |
