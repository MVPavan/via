# Task 4 design: events, progress, storage and C1 conformance (round 15)

Status: normative design for Task 4 (Bead `via-jm4.7.8`, step T4-0), round 15.
It replaces rounds 1–4. Rounds 6–15 apply the orchestrator's decisions on
Sol's round-5 to round-14 reviews (`design-r5-decisions.md` to
`design-r14-decisions.md`), tagged `[t4r5.N]` to `[t4r14.N]`. The
owner-approved [requirements](requirements.md) (R1–R7, the "Bounding
strategy" and "Thresholds are configuration") are normative and override
earlier design assumptions and spec text; every such conflict is an
amendment in §11. Round 8 replaces exact memory and disk accounting with the owner's
coarse bounds (§5, §6.9) and limits `logs` to private connections (§4.4);
round 9 makes every threshold daemon config (§5.2); round 12 narrows Task 4
to per-turn connections (§4.4). Round history and
decision maps: [reports/T4-0.md](reports/T4-0.md) §9–§19.
This step writes no code and no tests; slices are re-planned after the
owner's review (`s1.md`–`s7.md` are superseded).

Sources: requirements; C1 (`docs/specs/via-api-v1.md`); C2
(`docs/specs/adapter-contract.md`), A1 as approved; runtime
(`docs/specs/runtime-contracts.md`); the vendor specs in
`docs/specs/vendors/`; [dispatch design](../t2/dispatch-design.md); [T3
design](../t3/design.md) and its A1–A23; `.repo-context/invariants.md`;
`.repo-context/CONTEXT.md`. Tags: **[V]** verified at the cited `file:line`
of `wt/t4-0` (base `ea28956`) or dependency source (`serde_json` 1.0.151,
`tokio` 1.53.1); **[I]** inferred, checked first by the implementing slice;
**[S §n]** stated by the vendor spec section; **[U]** not probed by the
vendor spec, pinned by the vendor slice before use. Every mechanism names
its owner (creator, writer, end), and every queue is bounded and says what
happens when it is full.

## 0. Scope and fixed decisions

Fixed, not reopened: runtime §7 with T3's amendments (a known Store outcome
is scoped, an uncertain one latches, O1); Core owns every absolute deadline;
C2 A1's numbers (1024 items and 4 MiB per session, a full channel blocks
only the normalizer, 10 s without drain fails the turn `overflow`); no new
dependency, debug RPC or CLI verb outside C1; the requirements R1–R7 and
the bounding strategy (owner-approved 2026-09-29).

What the requirements change: VIA no longer stores or streams per-message
detail. SQLite keeps lifecycle, control and safety events plus the envelope
(R1). Text, reasoning, tool and usage messages feed only an in-memory
progress snapshot (R3) and one `steps` row per model step (R4); their bytes
stay in the raw log (R2). Callers poll `status`, block on `wait`, page
`events`, and read raw excerpts with `logs` (R5). There is no follow stream,
so follower tasks, the `Head` version wake, outboxes, termination episodes,
`unsubscribe`, `event_end` and the stall's stop-order delivery (A19) are
removed.

Coordination primitives (A13, kept): `tokio::sync::{watch, Notify,
Semaphore, mpsc, oneshot}` and `JoinSet`. A task's owner creates its stop
signal, owns its `JoinSet`, and stops, drains and joins it within a bound;
dropping a handle is never the normal cancellation path. [V] `tokio-util`
and `proptest` are declared (`Cargo.toml:14`, `:28`) and used by no crate.

Out of scope, each with its revisit condition:

| Item | Why | Revisit when |
|---|---|---|
| `describe`/`models` for routes other than `fake` | one route in S1 (`crates/via-core/src/engine/receipt.rs:97` [V]) | the first real route |
| Durable `output_schema` state | the fake refuses it by name (`crates/via-core/src/api.rs:34` DTO member, refusal in `Named::fake`, `:464` [V]) | a route supports it |
| Store operation watchdog (runtime §8) | not a Task 4 item | the S1 close review |
| Blob-backed `effective` | the fake's `Effective` is a few hundred bytes (`api.rs:979` [V]) | a route accepts large `bound`/`vendor` values |
| Retention and the per-session delete | requirement non-goal; `via-jm4.18` | that task; §3.4 gives the keyed delete it uses |
| Reusable connections: per-session and shared servers [t4r7.3, t4r11.1] | S1 has per-turn connections only; their lifecycle, spans and `logs` split, under the constraints in §4.4 | the OpenCode (`via-4sw.3.2`) and Codex adapter tasks |

## 1. Owners, lock order and wakes

| State | Owner | Created by | Written by | Ended by | Bound |
|---|---|---|---|---|---|
| Daemon config (`DaemonConfig`) [t4r8.1] | daemon start | read once before `Store::open` | never | daemon end | §5.2 |
| Global memory pool (`MemoryBudget`, one semaphore; per-class flat charges) [t4r7.1] | via-store | `Store::open` | each class's owner (§5) | last `Arc` after Store threads join | `memory.pool` |
| Store request lanes (Latch, Lifecycle, Internal, Public) | `Store`; served by the SQLite thread | `Store::open` | `StoreClient` handles by lane tag | fence, or writer death | §6.1 |
| Raw inbox, raw worker, blob files | Store raw worker | `Store::open` | `RawWriter`, `BlobWriter`, `BlobReader` | fence, then join; death guard | §6.4, §6.6 |
| `ConnectionLatch` | via-wire, per connection | `open_connection` | readers, stdin writer, raw worker via `RawFaultSink` | last holder after `finish` | §7.4 |
| Reader and stdin-writer tasks | `WireMessages` (unique) | `open_connection` | the tasks | `finish` under one deadline, then adoption | §7.6 |
| Message queue | `WireMessages` | `open_connection` | stdout reader | `next_message`, or `finish` | 64 messages, 4 MiB |
| Route → Adapter hop | Adapter `execute` | per drive | Route | end of `execute` | 1 message |
| Observation channel and its byte semaphore | Core drive | per drive | Adapter | end of the drive | 1024 items, 4 MiB |
| Stall timer | Adapter pending delivery | first blocked send | Adapter | acceptance of that item, or 10 s | one per drive |
| Connection charge [t4r8.5] | `WireMessages`, then its adopted set | before the connection opens | — | `finish` returned and every adopted task joined | `memory.connection` (§5.1) |
| Drive charge | Core drive | dispatch, before submission | — | end of the drive | `memory.drive` (§5.1) |
| Envelope accumulation and meter [t4r6.4] | Core drive (`TurnRecord`) | submission | the drive, per accumulated item | the terminal commit | `ENVELOPE_MAX` (§6.5) |
| Turn activity clock (`TurnActivity`, one `AtomicU64`) [t4r5.11] | Core drive; a clone in the `Running` entry | submission | the Adapter, for each vendor message attributed to the turn | end of the drive | 8 B |
| Step tracker | Core drive (`TurnRecord`) | submission | the drive | the terminal commit | §2.4 |
| Published progress | `Slot` `Running` entry (`crates/via-core/src/engine/queue.rs:274` [V]) | `Running` creation (`:488` [V]) | the session's drive only | `finish_running` (`:593` [V]) | §2.4 |
| Open-session tally, closing set | `Engine` (`Sessions`) | `Engine::open`, seeded from Store | receipt and closed-now answers | never | §10.2 |
| Live `Armed` controls | Host ledger (`crates/via-host/src/host.rs:72` [V]) | control verification | `Capacity::armed` | control drop | one per anchor |
| Disk budgets (SQLite `max_page_count`; the raw worker's byte counter) [t4r7.2] | Store | `Store::open`, from the files on disk | SQLite; the raw worker, per write | never | `disk.*`, `wal.*` (§6.9) |

**Lock order.** T3 §1 stands (`admission` → `sessions` → slot state; `Head`
as T3 orders it; `stop` alone; no std mutex across an `.await`). An async
owner's lock may briefly take the `Lanes` mutex, never the reverse; code
holding `Lanes` or the raw-inbox mutex takes no other lock, awaits nothing
and runs no callback. `Sessions` → `Unresolved` (leaf std mutexes) only in
`Engine::counts()`. Progress takes only the slot state mutex, without await.

**Wakes** are hints; the receiver re-reads the owning state: the force
signal (`Engine::force_signal()`, `crates/via-core/src/engine/latch.rs:446`
[V]), the connection latch (§7.4), Route's wake for a changed stop order or
`force_at` (`crates/via-routes/src/runtime.rs:516` [V]), the closed hop for
a stall (§2.3), and the per-unit durable `oneshot` (§7.5).

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

Not events (R2), raw log only: `assistant.text` and `reasoning.summary` (a
`model` mark), `tool.started` and `tool.ended` (tool marks), `usage.updated`
(a usage mark), `file.changed` (not reported, a requirement non-goal) and
`vendor.other` (an unknown message is activity only).

Unchanged: dense per-session `seq`, `turn`, `late`, `raw_ref` and "`turn.ended`
is the last non-late event of its turn"; the envelope's content (R6), whose
`events {first_seq, last_seq, count}` now ranges over durable events and
whose `denied_actions[].event_seq` and `auto_declined_requests[].event_seq`
still cite durable events. A fake turn has about six events. A late tool
completion is raw-log evidence only, as it never changed a settled envelope.

### 2.2 What a Route decodes (R2)

Route decodes each vendor message into a typed struct holding only what
R1–R6 need: the type and correlation IDs (vendor turn, thread, session, tool
call), tool names, usage numbers, the acceptance, identity and terminal
fields the envelope carries (`stop_reason`, `vendor_code`, usage, structured
output), the text a vendor mapping makes final text (§2.3), and the payloads
of the durable observations of §2.1. Everything else is skipped with
`serde::de::IgnoredAny`, so other text, reasoning, tool inputs and tool
outputs are never copied (R2). Rules, enforced in Route's decode:

1. **Short fields.** An ID, tool name, type tag, `stop_reason` or
   `vendor_code` is at most `SHORT_FIELD_MAX` = 1 KiB; a longer one is a
   protocol failure citing its raw ref (C2 §1 rule 6).
2. **Payloads at most 256 KiB** (C2 A1). A durable payload over it is a
   protocol failure with raw evidence. Final text is split at character
   boundaries into `final_text` pieces sized so that the whole encoded
   observation (key, fields and escaped text) is at most 256 KiB (§2.3)
   [t4r7.6, t4r8.6].
3. **Structure limits first.** `json_limits::scan` (§9.2) enforces depth 64
   and 65,536 nodes before the typed decode; an excess is a protocol failure.
4. **No peer `Value`** (§9.2), and no `#[serde(flatten)]`,
   `#[serde(untagged)]` or internally or adjacently tagged enum on a
   peer-fed type: they buffer input into serde's private tree, which §5's
   charges do not assume [t4r7.1]. [V] Today these attributes appear only on
   `Serialize`-only types in `crates/via-core/src/api.rs`, the fake agent's
   script input (`crates/via-fake-agent/src/main.rs:32`) and the Host anchor
   protocol (`crates/via-host/src/protocol.rs:76`), which carries no vendor
   bytes. A grep of `Deserialize` types in via-routes, via-adapters,
   via-core and via-cli is an acceptance check.

[V] Today the fake route decodes `text` with its text, measures tool
payloads and copies up to 16 KiB of an unknown message
(`crates/via-routes/src/lib.rs:340-344`, `:381`, `:518-540`); after R2 it
keeps `vendor_turn_id`, `tool_id`, `name` and an unknown type tag (at most
256 B, `:418`).

### 2.3 The observation channel (C2 A1 after R2)

Core's drive creates the channel per drive (today `mpsc::channel(64)`,
`crates/via-core/src/engine/drive.rs:1280` [V]). After R2 it carries:

| Observation | Core action |
|---|---|
| `turn.accepted`, `session.vendor_identity_confirmed`, `resume.mismatch`, `session.vendor_closed`, `tool.quiescent`, `turn.vendor_terminal` | as C2 §4 today |
| `action.denied`, `vendor.request_declined`, `steer.delivered`, `warning` | commit the event (existing `commit_event`, `drive.rs:1461` [V]) |
| **`progress { at, model, tools_started, tools_ended, usage }`** (new) | fold into the step tracker (§2.4); no Store write, except a step row at a step boundary (§3) |
| **`final_text { key, text, replace }`** (new) [t4r6.4, t4r7.6] | append to, or replace, that key's final text in the envelope, metered (§6.5). All final text arrives this way, a terminal message's included, before `turn.vendor_terminal`, which carries none. The Adapter cuts a piece at the last character whose escaped encoding keeps the whole observation within 256 KiB (a counting writer over key, fields and text) [t4r8.6]; a longer text is several pieces, the first carrying `replace` |

One vendor message yields at most one `progress` item, only when it carries
a mark (model output, a tool start or end, or a usage sample), plus its other
observations. An unknown or unattributable message produces no observation
[t4r5.11]; it is in the raw log and moves the activity clock only when
attributed. An item for a turn already terminal is late and dropped (the raw
log holds it).

**Bounds** (C2 A1): `mpsc::channel(1024)` plus a per-drive 4 MiB
`Semaphore`; an item counts `512 + Σ(64 + len(s))` over its string fields,
acquired by the Adapter before it builds the item and held until Core has
handled it. The Route → Adapter hop shrinks from `mpsc(64)` to `mpsc(1)`
(`crates/via-adapters/src/runtime.rs:132` [V]). Both are inside the drive
charge (§5). Route blocked on the hop stops calling `next_message`; the
Wire queue then fills and fails `overflow` (runtime §8 "Route message
staging"), the contract's answer to a burst.

**Stall** (C2 A1: 10 s without drain) [t4r5.8]. The Adapter's pending
delivery owns one absolute deadline, `first_block + EVENT_STALL` (10 s, a
Core constant), restarted only when that item is accepted. On expiry the
Adapter drops the session's hop receiver; Route selects on `hop.closed()`
in every wait (§8) and sees it at once:

- **Private route** (one hop per connection: the fake, Claude): Route fails
  `RouteError::Overflow` and force-closes the connection (Host stops its
  private process group: the interrupt) [t4r11.1]. Core
  disposes `Overflow` as today (`crates/via-core/src/engine/terminal.rs:97`
  [V]: `failed`, `overflow`).
- **Shared route** (Codex's shared server, one hop per thread generation):
  Route quarantines that generation exactly as C2 §4 does for an
  ingress-lane overflow, whose last sentence already sends the 10 s timer
  "to the same quarantine". The Codex slice builds this arm.

[V] Today the Adapter drops the receiver on a failed delivery wait and Route
reports the closed hop as `Overflow` (`crates/via-adapters/src/runtime.rs:146-153`,
`crates/via-routes/src/runtime.rs:755`); the 10 s bound and `closed()` arm
are new.

**Envelope overrun** [t4r5.5, t4r6.4]: Core meters each accumulating item as
it handles it (a committed `action.denied` or `vendor.request_declined`
entry, every `final_text` piece; §6.5); there is no separate event-payload
cap. On the item that would take the envelope over `ENVELOPE_MAX`, Core
records the overrun and sends the stop order with cause `overflow` and
`force_at = now` (A34), the path T3 uses for cause `store`. Route acts on it
through its wake; nothing depends on the Adapter sending again.

### 2.4 Progress snapshot (R3)

**Owners.**

- **Step tracker**: per-turn state in the drive's `TurnRecord` (submission
  to terminal); the only place the step rule runs.
- **Published progress**: a copy in the session `Slot`'s `Running` entry
  (`crates/via-core/src/engine/queue.rs:274`, `:488`, `:593` [V]), which
  exists exactly while the turn runs. After each `progress` item the drive
  calls `Slot::publish_progress(turn, &ProgressDelta)` under the slot state
  mutex (a leaf, no await). No new watch or task.
- **Activity clock** [t4r5.11]: one `TurnActivity` (`Arc<AtomicU64>`,
  milliseconds since the drive's base instant) per turn, a clone in the
  `Running` entry and one passed to the Adapter in `execute`, which stores
  the arrival of each vendor message it attributes to the turn, unknown
  types included (`Relaxed`, one writer). It carries no data, so it cannot
  queue or stall.
- **Reading**: `status` calls `Engine::slot(session)`
  (`crates/via-core/src/engine.rs:334` [V]) and, if the `Running` entry's
  turn matches, copies its `Progress` (at most 70 KiB under the mutex): no
  Store round trip, no wait on the drive.

**`Progress`** holds A26's C1 §3.7 `progress` members: `turn`,
`current_step`, `phase` (`tools` while the open set is non-empty or
`tools_overflow` is set, else `model`), `running_tools` (the open set's
names), `tools_overflow` [t4r6.9], `last_activity_at` (any attributed
message, [t4r5.11]) and `tokens` (`{total, scope}` of completed steps).

**The step rule** (one reducer for every vendor; the Adapter only classifies
each message into marks):

1. At `turn.accepted`: `current_step = 1`, step start = now,
   `results_since_output = false`.
2. `model` (model output: text, reasoning or a tool request): if
   `results_since_output`, this is a **step boundary**: the step ends now,
   `current_step += 1`, the next starts now, and the flag, the open set and
   `tools_overflow` are cleared [t4r6.9]. A message's `model` mark is
   applied before its tool starts, so a tool requested in the boundary
   message stays open.
3. `tools_started (id, name)`: added to the open set if it has fewer than 64
   entries and lacks the id; a new id beyond 64 sets `tools_overflow`.
4. `tools_ended` ids: every end, tracked or not, sets
   `results_since_output` [t4r6.9]; a tracked id is removed. After an
   overflow `phase` stays `tools` until the boundary: VIA says it no longer
   knows which tools run instead of counting them.
5. At the terminal the current step (if `current_step ≥ 1`) ends.

So the count rises exactly when the model produces output after tool results
(R3), and an open-set error lasts at most one step. A step's end is the next
step's start; its row is written then (§3).

**Tokens** (R3: approximate, once per step, labelled). No accuracy figure
is claimed; R3's "about 95%" is an owner gate (Q-R5-11), and R3 conformance
is not claimed for Claude, Codex or OpenCode before it and their probes
(§2.5) [t4r5.10, t4r6.14]. A `usage` mark `(key?, total)` is an interval
sample (`total` is the vendor's total, else input plus output). Within a
step, samples with the same key (the vendor's message ID, if any) replace
each other and different keys add; `tokens.total` sums completed steps, and
`tokens.scope` is the route's declared `capabilities.usage.tokens` (C1
§4.1). The envelope's `usage` is unchanged (R6); its `steps` is the vendor's
count when reported (Claude `num_turns`), else the final `current_step`,
else `null`.

**Bounds** (inside the drive charge): at most 64 open `(id, name)` entries
of at most 1 KiB per field (§2.2 rule 1); at most 16 usage keys per step, a
17th adding to a keyless sum; the published copy at most 70 KiB.

### 2.5 Vendor mappings of the marks

The reducer is shared; this is what each Adapter emits, [S] from the cited
spec unless marked [U]. The last column is the probe the vendor slice runs
before `tokens` is claimed for that vendor [t4r5.10].

| Vendor | `model` | `tools_started` | `tools_ended` | `usage` (key) | Envelope `steps`, `usage` | Token evidence and probe |
|---|---|---|---|---|---|---|
| Fake (runtime §3.1, A33) | `text` | `tool_started {tool_id, name}` | `tool_ended {tool_id}` | new `usage {total_tokens}` message, no key | tracker count; `usage` unavailable (the fake terminal has none) | exact by construction; none |
| Claude `claude-cli` ([S §5]) | an `assistant` message with text or `tool_use` content | each `tool_use` block `(id, name)` | each `tool_result` block in a `user` message (tool ID) | `assistant.message.usage`, key message ID **[U]**: the spec probes only `result.usage` (§5, §3 row `usage`) | `num_turns` (probe, [S §3]); `result.usage`, scope `turn` | proven: `result.usage` per result, `total_cost_usd` session-cumulative [S §3, §5]; unprobed: whether each `assistant` message carries `message.usage`, repeated per API call; probe: a multi-step tool turn comparing the per-message-ID sum with `result.usage` |
| Codex `codex-app-server` ([S §5, §7]) | `item/started` or `item/agentMessage/delta` for `agentMessage` or `reasoning` | `item/started` for a tool item (`commandExecution`, `fileChange`; others such as MCP calls **[U]**), name = item type | `item/completed` for that item ID | `thread/tokenUsage/updated` `tokenUsage.last`, no key; scope `vendor_interval` (§7: never `.total` summed) | tracker count (Codex reports none); usage per §7 unchanged | proven: `last` and session-cumulative `total`, replace snapshots [S §7]; unprobed: whether one `last` is one model call; probe: a multi-step turn comparing Σ`last` with the change in `total` |
| OpenCode `opencode-serve` ([S §4, §5, §7]) | a text or reasoning part of an assistant message correlated to the turn | a tool part entering `running` (call ID, tool name) | a tool part entering `completed` or `error` | the assistant message's token fields, key message ID (§7 one ledger; step-finish duplicates it) | tracker count; usage per §7 unchanged | proven: one ledger keyed by message ID [S §7]; unprobed: the exact event and part names (legacy `message.*` family **[U]**), whether one assistant message is one model call; probe: the ledger against the session total |

Until its probe passes, a route declares `usage.tokens` with the spec's scope
label and `tokens` may be `null`; the envelope's `usage` is unaffected.

A real vendor's message that is none of these (Claude `system/init`, Codex
`thread/status/changed`) sends no observation and, if attributed, moves only
the activity clock [t4r5.11]. OpenCode's `server.heartbeat` and Codex's
untagged connection status are attributed to no turn [S OpenCode §5, Codex
§5]. Codex's tool-completion tracking for P7 (`tool.quiescent`) is separate,
exact and unchanged [S §6]; the open set is display only.

### 2.6 Idle deadline

Runtime §8: idle resets on normalized meaningful progress. [V] Today
`progress()` counts acceptance, `assistant.text`, `tool.started` and
`tool.ended` (`crates/via-core/src/engine/drive.rs:1775-1786`). After R2 it
counts acceptance and any `progress` item with `model` or a tool start or
end; usage-only items and unknown messages never reset it (T3 §5). The timer
and its stop order are T3's.

## 3. Step rows (R4)

### 3.1 Table

Schema v6 adds, clustered by key so a session's rows are one contiguous
range:

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

Times are Unix milliseconds from Core's wall clock, rendered RFC 3339; no
ordering relies on them. `tokens` is under the route's `usage.tokens` scope.

### 3.2 Write path and ordering against `turn.ended`

- **Step end.** At a boundary the drive commits `CommitSteps { session,
  turn, rows: [row N] }` on the Internal lane, awaited under `while_polling`
  (§8) before the next observation, so outside the terminal a command
  carries one row. No `Head` is taken (a row has no `seq`).
- **The last step** is written **in the terminal transaction**: every
  terminal built from the drive's `TurnRecord` (`finish_with`, and `finish`
  for a forced turn, `drive.rs:1035`, `:1000` [V]) carries the open step's
  row ended at the terminal time. A forced turn's record travels to final
  shutdown in its `ForcedTurn` (`drive.rs:756` [V]); `hand_off` ends the open
  step, and final shutdown's best-effort terminal (T3 §7.4,
  `t3/design.md:1348`; `forced_terminal`,
  `crates/via-core/src/engine/stop.rs:360` [V]) carries it and any carried
  rows [t4r6.8].
- **No row cap** [t4r5.1]. Every completed step gets a row (under 128 B; a
  step is a whole model call); rows count against the SQLite budget (§6.9).
- **Refused rows ride in the terminal** [t4r5.2]. A step commit follows T3
  §7 for a turn write. A known `NotCommitted` (a budget refusal included)
  records `first_failure` and upgrades the stop order to cause `store`
  (`force_at = now`); the turn then writes only its resolution write (T3
  §7.2), the `failed(store)` terminal, which inserts the refused row, every
  row ended after it (`TurnRecord.carried_rows`) and the open step's row. An
  uncertain outcome latches (T3 §7.4). The stop is forced at once, so only
  items already in flight (the channel's at most 1024, and three more) can
  end further steps: about 1,030 rows of at most 128 B.
- **Ordering and guarantee** [t4r6.8]. The terminal is built only after
  the channel drains, so every row precedes or shares the transaction of
  `turn.ended`; a `turn.ended` built from the `TurnRecord` (natural,
  `failed(store)`, forced at shutdown) implies every row is durable. Only
  the Latch batch after an uncertain outcome (T3 §7.4, no rows) and
  recovery's terminal (§3.3) make no such claim.

### 3.3 What survives a crash

Rows up to the last *committed* step (R4 as clarified [t4r5.15]); a step
whose row commit was in flight, and the step in progress, are recoverable
only from the raw log. Recovery synthesizes the `unknown` terminal (T3) and
adds no row. The snapshot and activity clock are memory only.

### 3.4 Reading and 3.5 retiring

`status` reads a page with `SELECT step, started_ms, ended_ms, tokens FROM
steps WHERE session_id=?1 AND turn=?2 AND step>?3 ORDER BY step LIMIT ?4`, a
primary-key range scan. Retiring (design only; built by `via-jm4.18`):
`DELETE FROM steps WHERE session_id = ?1` is one keyed range delete, done
with the session's events, envelopes and raw logs.

## 4. Caller interface (R5)

With request `id` capped at 256 B (A31), every reply's wrapper is under
512 B, so each bound below applies to the `result` object. Each reply is
written within `REPLY_WRITE` = 10 s or the connection closes (A32).

### 4.1 `wait` and `result`

`wait` polls the Store every 20 ms as today
(`crates/via-core/src/engine/read.rs:69` [V]) with `terminal_facts` (§6.8), a
small read, until the turn is terminal, `timeout_ms`, or final shutdown; only
then does it take its reply charge and read the envelope with `result_text`.
`result` reads `result_text` once. The envelope (at most 1 MiB) is written as
stored; no `Value` is built.

### 4.2 `status`

C1 §3.7 as A26 replaces it: the durable members (§10.3) plus `progress`,
the published `Progress` (§2.4) or `null`, and `steps`, a page of the
selected turn's rows (§3.4). Sources: one Public Store read
(`session_status`, §6.8) and memory for `progress` and `process.alive`. The
reply is built within `STATUS_MAX` = 1 MiB, else `admission_refused`; for
the fake it is far below (64 turn summaries, 8 queued `effective` values,
`progress` at most 70 KiB, 1000 rows of 128 B).

### 4.3 `events`

Params: exactly one of `session`, `turn`; `after?` (0), `limit?` (200, max
1000), `types?`. `follow` is refused as an unknown field (A25); the daemon
sends no notifications, so a dropped connection releases only its in-flight
request (a `wait` releases only its waiter).

- One read transaction over the window `after < seq ≤ after + 1000`, with
  `types` and `turn` as SQL predicates on the v6 columns, stopping at
  `limit` or `PAGE_MAX` = 1 MiB (a row's borrowed length is checked before
  it is copied). The SQLite thread writes the `events` array as one JSON
  text (the stored event texts joined by `,`); the handler writes prefix,
  array, suffix.
- `next_after` is the last scanned seq, filtered rows included; `more =
  next_after < head` (same transaction); `earliest_seq` is 1 (nothing is
  pruned). A first matching event larger than the page is
  `admission_refused` (C1 §3.11); today none is (payloads ≤ 256 KiB).

### 4.4 `logs`: raw excerpts, undecoded [t4r7.3]

C1 §3.12 as A27 replaces it (params, result, the end cursor); `text` is
lossy UTF-8 that VIA does not parse (R5).

- **Per-turn connections only** [t4r7.3, t4r11.1]. Task 4 serves what S1
  has: one private connection per turn (the fake; Claude, "one process for
  one VIA turn", `claude-code.md:61`). A connection belongs wholly to its
  turn (`connections.turn`), so a turn's raw data is its own connection or
  connections, whole; nothing is written per vendor message.
- **Reusable connections** (OpenCode's per-session server, Codex's shared
  server) belong to those adapter tasks (`via-4sw.3.2` for OpenCode)
  [t4r11.1], which must meet: cancel and close keep a
  dedicated server running (`opencode.md:596-599`); P7 settles before the
  terminal commits (`via-api-v1.md:240-245`); a turn's span on a reusable
  connection is bounded by raw barriers, and an unanswered end barrier
  retires the connection; a turn's handles cannot outlive the turn or close
  the server; D4: `logs` never returns bytes to a session other than the
  one they are attributed to, nor unattributed bytes; and T3 and C2
  amendments that separate per-session turn failure from per-turn
  connection failure (`force_at`, the Store latch, daemon shutdown).
- **Scope.** A turn address reads its connections; a session address reads
  the session's connections; both whole, in `connections.ord` order, then
  offset. A session's connections do not overlap in time (a turn's
  connection is sealed before the next turn starts), so only the last can
  be unsealed.
- **Cursor**, one per connection: the sentinel `r1.start` (an omitted
  cursor) or `r1.<connection_id>.<offset>`, the next byte to return, parsed
  strictly. The sentinel resolves to offset 0 of the scope's first
  connection. A supplied cursor is valid only on a connection in scope and
  at most its end (the committed `high_water`, or the durable end while open) [t4r8.7, t4r13.1];
  anything else is `invalid_params`. With nothing new durable,
  `next_cursor` is the request's cursor [t4r5.12].
- **Paging.** The SQLite thread binary-searches the index for the unit
  holding the offset (§6.4), walks consecutive units in scope (to the next
  connection at a sealed end), reads each whole unit (at most 1 MiB,
  `RAW_UNIT_LIMIT`), checks its SHA-256, and emits the part from the cursor,
  at a UTF-8 boundary when one is within 3 bytes. A page emits at most
  `LOGS_RAW_MAX` = 128 KiB of raw bytes and 256 entries: at most 6 × 128 KiB
  + 256 × 256 B = 832 KiB encoded, under `PAGE_MAX`.
- **Errors.** A missing or corrupt unit is `store_error` for the request
  only (no latch).

### 4.5 Other methods

| Method | DTO (`deny_unknown_fields`) | Answer |
|---|---|---|
| `describe` | `harness?, model?, bound?, require?, vendor?, cwd?, allow_untested?` | Core, from `Capabilities::fake()` (`api.rs:932` [V]); no process, no write |
| `models` | `harness?` | Core: the fake's one model |
| `list` | `state?, harness?, label?, since?, limit?, cursor?` | Store `list_page` (§6.10; A12) |
| `daemon/status` | none | memory: `started_at`, counts (§10.2), `limits` (§5.2) [t4r9.7] |
| `status`, `events`, `logs` | as above | §4.2–§4.4 |

- New `ApiError` constants: `UNKNOWN_MODEL` (-32010, replacing
  `invalid_params` at `receipt.rs:100` [V]), `HISTORY_PRUNED` (-32019),
  `STORE_QUEUE_FULL` and `MEMORY_BUDGET` (both `admission_refused`);
  `unsubscribe` is `method_not_found` and `via events --follow` is gone.
- CLI (`crates/via-cli/src/main.rs:20-34` [V]): verbs `describe`, `status`,
  `list`, `models`; `status --turn N --after-step N --limit N`; `logs
  --cursor C --limit N`; spawn flags `--prompt-file`, `--instructions`,
  `--cwd`, `--require`, `--allow-untested`, `--label`.
- `serve --stdio`: a byte proxy between stdio and one daemon socket,
  auto-starting the daemon; two copy loops under one `JoinSet`; stdin EOF
  shuts the socket's write side; ends at socket EOF. Parity test: one
  scripted sequence over the socket and over the proxy gives the same
  replies, the oversize case included.

## 5. Memory and disk bounds (R7) [t4r7.1, t4r8.1]

The owner's bounding strategy replaces exact accounting: one memory pool, a
conservative flat charge per class of retained buffer taken before its owner
allocates, and a named overload when a charge cannot be granted. No maximal
combination is claimed to fit. Every threshold is a daemon config key with a
provisional default (§5.2); `via-d9o.2.3` tunes them. Disk bounds are §6.9.

### 5.1 One pool, flat charges per class

`MemoryBudget` (via-store, the lowest crate, `scripts/check-layers.py:20`
[V]) is one `tokio::sync::Semaphore` of `memory.pool` created by
`Store::open`, with no partitions. An owner acquires its class's charge
before allocating and releases it on drop. Every amount is an assumption
**[I]**, confirmed by the named check.

| Class (key) | Default, held | Cannot be charged | Covers; confirmed by |
|---|---|---|---|
| Store request lanes (`memory.latch_lane`, `memory.lifecycle_lane`, `memory.ordinary_lanes`) | 2 + 2 + 4 MiB, from `Store::open` | open fails | the lane allowances (§6.1); lane high-water test |
| SQLite page cache (`memory.sqlite_cache`) | 8 MiB, from `Store::open` | open fails | `cache_size` on the one long-lived connection, the writer (`crates/via-store/src/runtime/sql.rs:140`, `runtime.rs:1013` [V]); approximate, so the RSS gate confirms it |
| Connection (`memory.connection`) [t4r8.5] | 16 MiB, from before the connection opens until `finish` has returned and every task it adopted has been joined (§7.6) | the dispatcher waits; the turn stays queued | 8 MiB raw staging (runtime §8), the 64-message / 4 MiB Wire queue, pipe buffers, the 1 MiB stdout assembly buffer, stdin pieces, payloads queued for raw; F24's per-connection figures |
| Drive (`memory.drive`) | 8 MiB, from dispatch (before the submission commit) to the drive's end | the dispatcher waits; the turn stays queued | 4 MiB of observations (C2 A1), decoded messages in flight, the envelope accumulation (at most 1 MiB) and its encoding, the step tracker and carried rows |
| C1 request (`memory.request_multiplier`, `memory.request_nodes`) | `request_multiplier × len + min(8 × len, request_nodes) + 64 KiB` for a line of `len` bytes (defaults 4 and 2 MiB), taken as the line is read; line bytes also count against runtime §8's 32 MiB C1 input cap. When the DTO is built, the line and decode share is released and the charge shrinks to the DTO's retained bytes (at most `len`); that remainder is released when the handler has handed the DTO's contents to their owner (a Store command, a blob writer) or dropped them, always before the reply charge is acquired, so a request and its reply are never charged together [t4r11.2] | waits until the 5 s partial-line deadline, then discards the line and replies `admission_refused` (`MEMORY_BUDGET`, `id: null`) | the segments, the line, serde's scratch and the decoded strings, list headers within 65,536 nodes; a counting-allocator test on a 16 MiB escaped prompt and a 65,536-node list |
| Reply or Store page (`memory.reply_small`, `memory.reply_page`, `memory.reply_logs`) [t4r10.3] | 64 KiB for a small reply, `PAGE_MAX + 4 KiB` for a page and 2 MiB for `logs` (832 KiB encoded plus one 1 MiB unit read, §6.8); replies together at most runtime §8's 32 MiB | `admission_refused` (`MEMORY_BUDGET`) | held at most `REPLY_WRITE` (A32); capacity checked in the test binary |
| Codex shared server (`memory.codex_shared`) [t4r9.2] | 16 MiB per shared server, from launch to its end (runtime §8) | the server is not launched | observation and staging lanes, retained tool metadata; not in S1, so the Codex extension's gate confirms it |
| Blob chunk (`memory.blob_chunk`) [t4r10.3]; dispatched blob prompt | 64 KiB, one in flight per handle; the prompt's length until its start is written (§7.3) | waits under the chunk's 2 s bound; the turn stays queued | exact |

With four connections and four drives (`CONNECTION_SLOTS` = 4,
`crates/via-core/src/engine/queue.rs:33` [V]), 112 MiB of the default
128 MiB is held and 16 MiB remains for C1 requests and replies, so
`status`, `cancel` and other small requests are still served. A maximal
16 MiB request is charged about 66 MiB: it is admitted while at most one
turn runs, else refused by name. Waits are acyclic: a dispatcher waits only
for the pool; C1 handlers, replies and blob chunks never wait for a drive.

Not charged, measured by the RSS gate: allocator overhead, SQLite's other
allocations, task stacks, fixed-size structs. **Verification:** F24's RSS
gate, `MemoryBudget::high_water()` at most `memory.pool`, the
counting-allocator test (§12.2) and `via-d9o.2.3`. A class whose measured
peak exceeds its charge gets a larger default.

### 5.2 Daemon config [t4r8.1]

- **File.** `daemon.json` in the state directory (`VIA_STATE_DIR`, default
  `~/.via/state`, `crates/via-cli/src/client.rs:30-37` [V]). JSON, so
  serde_json parses it and no dependency is added. The file is optional:
  absent means every default. It is validated like the Store directory
  (regular file, the daemon's uid, not a symlink, not group- or
  world-writable) and is at most 64 KiB.
- **Read once.** The daemon reads it at start, before `Store::open` and
  before binding the socket; a changed value takes effect at the next daemon
  start.
- **Shape.** One object `{memory, disk, wal}` decoded into strict DTOs
  (`deny_unknown_fields`; serde refuses a duplicate key). Every key is
  optional; values are non-negative integers in bytes (`wal.checkpoint_commits`
  counts commits).
- **Keys and defaults.** `memory`: `pool` 128 MiB, `sqlite_cache` 8 MiB,
  `latch_lane` 2 MiB, `lifecycle_lane` 2 MiB, `ordinary_lanes` 4 MiB,
  `connection` 16 MiB, `drive` 8 MiB, `request_multiplier` 4,
  `request_nodes` 2 MiB, `reply_small` 64 KiB, `reply_page` 1 MiB + 4 KiB,
  `reply_logs` 2 MiB, `blob_chunk` 64 KiB [t4r10.3], `codex_shared` 16 MiB
  [t4r9.2]. `disk`: `sqlite_budget`
  1 GiB, `sqlite_headroom` 64 MiB, `files_budget` 3 GiB, `files_headroom`
  256 MiB. `wal`: `max` 32 MiB, `checkpoint_bytes` 8 MiB,
  `checkpoint_commits` 1000.
- **Validation** [t4r9.2, t4r9.5], each failure naming its key and rule.
  Arithmetic is in `u64` with checked operations; an overflow is a failed
  rule. Ranges: each memory value at most 1 TiB, so a charge counted in KiB
  fits `Semaphore::acquire_many`'s `u32` and the pool fits `MAX_PERMITS`
  (tokio 1.53.1, `Cargo.lock:768-769` [V]); `sqlite_budget − wal.max` at
  most SQLite's 4,294,967,294 pages; `files_budget` at most 2^62 B.
  - Minimums, each from its class (§5.1) and its maximal-input check: lanes
    1.5 MiB each (one terminal, §6.5); `sqlite_cache` 1 MiB [I];
    `connection` 14 MiB (staging, queue and assembly buffers); `drive`
    6 MiB (observations, envelope and encoding); `request_multiplier` 4 and
    `request_nodes` 2 MiB (the four copies and 65,536 list headers);
    `reply_small` 64 KiB (the largest reply that is not a page);
    `reply_page` `PAGE_MAX + 4 KiB`; `reply_logs` 1,860 KiB (§6.8's
    832 KiB and one 1 MiB unit, plus 4 KiB); `blob_chunk` 64 KiB (the fixed
    chunk) [t4r10.3];
    `codex_shared` 5 MiB (one turn's 4 MiB of observations and one 1 MiB
    message).
  - Aggregates: `pool` at least the lanes, the cache, 4 × (`connection` +
    `drive`) and the larger of `reply_page` and `reply_logs` [t4r10.3]
    (a request's charge is released before its reply's, §5.1 [t4r11.2]), and at least the lanes, the cache and a maximal
    16 MiB request's charge; `codex_shared` at most `pool` less the lanes
    and the cache.
  - Disk and WAL: `sqlite_headroom` at least the 16 MiB terminal reserve
    (§6.9); `sqlite_budget` at least `wal.max` + `sqlite_headroom` +
    64 MiB; `files_headroom` at least 32 MiB (four connections' staging) and
    below `files_budget`; `wal.max` at least 4 MiB and above
    `checkpoint_bytes`; `checkpoint_bytes` at least one 4 KiB page, applied
    as whole pages (`wal_autocheckpoint` takes pages; zero disables it);
    `checkpoint_commits` from 1 to 2^32 − 1.
- **Invalid** (unreadable, not JSON, an unknown or duplicate key, a failed
  rule): the daemon refuses to start, writes `via: daemon config invalid:
  <key>: <rule>` to stderr and exits with status 78, before any Store or
  socket state changes; the auto-starting CLI reports that message.
- **Reported.** `daemon/status` returns the effective values (A37)
  [t4r9.7].
- **Fixed, not config:** the C1 limits (line, depth, nodes, pages, envelope,
  `id`, reply deadline), C2 A1's channel and payload limits, runtime §8's
  queue and staging limits, the 4 KiB page size (fixed when the database is
  created) and the blob chunk's size (its charge is `blob_chunk`).

## 6. Store

### 6.1 Request lanes [kept: A2]

[V] Today one `sync_channel(128)` carries every request, served FIFO
(`crates/via-store/src/runtime.rs:1021`); Host's `ProcessJournal` shares the
sender; `Store::drop` sends `Shutdown` with a blocking `send`.

`Store::open` replaces it with one `Lanes` value (`Mutex<State>` +
`Condvar`) holding four FIFO lanes, each with its own slots and bytes:

| Lane | Members | Slots | Bytes |
|---|---|---|---|
| Latch | the failure-resolution unit only (`crates/via-core/src/engine/batch.rs:85`, `:91`, `:165` [V]) | 1 | `latch_lane` |
| Lifecycle | every other Store call of the shutdown pipeline | 7 | `lifecycle_lane` |
| Internal | every other commit and every read by Core, Route, Host or recovery | 64 less Public's | `ordinary_lanes` less Public's |
| Public | reads issued by C1 handlers | at most 32 of the 64 | 4 KiB each |

Membership is by handle (`StoreClient::public()`, `lifecycle()`, `latch()`;
default Internal). Service order: Latch, Lifecycle, then Internal and Public
round-robin (runtime §8's fair lanes). A full lane or allowance returns
`StoreError::NotEnqueued` (known, T3 §7.1): a mutation keeps T3's
`store_error` `not_committed`; a Public read is `admission_refused`
(`STORE_QUEUE_FULL`) and never latches. `Lanes::push` is the only enqueue
path and never blocks; Host's journal keeps its `try_send` semantics.

### 6.2 Lifecycle and Latch capacity [kept]

[V] `Engine::shutdown` (`crates/via-core/src/engine/stop.rs:444`) issues its
Store requests one at a time, each under `min(FINALIZE_WRITE, remaining)` or
`BATCH_READ` (2 s each, `latch.rs:46`, `batch.rs:24`), inside the 10 s
`FINAL_SHUTDOWN` (`crates/via-cli/src/server/shutdown.rs:23`). The
failure-resolution unit awaits each request, so it needs one Latch slot; an
abandoned Latch request makes the next push `NotEnqueued` (runtime §7's
"skipped batch"). At most five Lifecycle requests are abandoned at a full
2 s and one at the remainder, so 7 slots cover them plus the one being
issued ([I]: each Lifecycle bound is at least `min(2 s, remaining)` and each
step checks the deadline first). The allowances (§5.2, 2, 2 and 4 MiB by
default) are flat [I] [t4r7.1]: each holds one terminal (§6.5), and the
Latch-fit test measures the largest failure batch. When abandoned requests
hold the bytes, a push is `NotEnqueued`, reported `not_committed`, and
recovery resolves the turn at restart (T3 §7). `finish` (`drive.rs:1000`)
uses the Lifecycle handle; `finish_with` (`:1035`) keeps Internal.

### 6.3 Lock discipline, fence and writer death [kept]

Only queue and counter updates run under the `Lanes` mutex; replies are
dropped after release, and the SQLite thread waits in a predicate loop.
`Shutdown` is an admission fence, not a queued item: `Store::drop` sets
`fence`, notifies and joins; later pushes are `NotEnqueued`; everything
accepted before is served in lane order. The thread body runs under a
`DeadGuard` whose `Drop` (also on unwind) sets `dead`, takes all lanes and
the in-flight item, releases the mutex and fails them `WriterLost`, which
latches; a later push is `WriterLost` at once. The raw inbox uses the same
fenced queue (`FencedQueue<T>`).

### 6.4 Raw inbox, group commit, lookup [kept]

[V] Today `RawWriter::append` fails on a full 64-slot channel
(`crates/via-store/src/runtime.rs:522-531`) and `raw_loop` syncs per unit
(`crates/via-store/src/runtime/raw.rs:25`, `:120`).

- **Inbox.** A `FencedQueue<RawCommand>` bounded by staging permits: every
  `Append` carries an `Arc<Payload>` from `Payload::stage` (`max(len,
  512)`), so at most 32 MiB / 512 B = 65,536 appends queue; a `Barrier` holds
  no permit (one per connection); a blob command holds one 64 KiB chunk.
- **Fault publication.** via-store declares `trait RawFaultSink { fn
  raw_failed(&self, error: &StoreError); }`; Wire implements it on the
  connection latch; the worker calls it on its own thread, lock-free and
  nonblocking.
- **Group commit** (runtime §4: 1 MiB or 20 ms). The worker first checks its
  byte counter against the raw budget (§6.9) [t4r7.2]; a refused unit fails
  like a failed write. Per touched connection: write payloads, `sync_data`,
  write index entries, `sync_data`, ack. A non-`Append` command flushes the
  batch first; no index entry precedes its payload's sync.
- **Failure.** A failed write or sync fails that batch's units of the
  affected connections (each sink once, first error); others are acked; a
  failed connection keeps failing fast (`raw.rs:37-38` [V]). The death guard
  owns the in-flight batch; on exit or unwind it sets `dead`, takes the
  queue, sends `WriterLost` to each sink once and fails every reply.
- **Offsets** [t4r13.3]. [V] Today the worker assigns offsets at write
  (`crates/via-store/src/runtime/raw.rs:126`, `:146`). Change [t4r14.1]:
  `Payload::stage` takes the unit's staging capacity first; then, under one
  per-connection mutex, `RawWriter` assigns the offset (`enqueued_end`) and
  enqueues with a nonblocking `try_send`, advancing `enqueued_end` only on
  success, so offsets equal the worker's file positions with no gap and
  nothing blocks under the lock. A refused send is a connection failure: a
  full inbox `Raw(Raw)` with `raw_incomplete`, a closed one
  `Raw(WriterLost)` (§7.4). The worker syncs in that order and raises the shared `durable_end`
  before sending acks, so ack observation order cannot affect it.
- **Lookup.** [V] `read_raw_ref` scans the 45-byte index linearly
  (`raw.rs:154-212`). Entries are in increasing payload offset, so lookup is
  a binary search (entry `i` at byte `8 + 45 × i`) then one payload read
  checked against length and SHA-256, at most `⌈log2(entries)⌉ + 1` reads;
  `raw_ref` resolution and `logs` (§4.4) use it.

### 6.5 Transaction cap, envelope meter and failure summary [A30]

- **`Command::bytes()`** is the encoded length of the command's variable
  payload (a counting writer over what the SQLite thread binds) plus 512 B,
  over an exhaustive match of `Command` (`crates/via-store/src/runtime.rs:710`
  [V]).
- **Cap** (runtime §8 as amended by A30): at most 128 events and 1 MiB of
  payload, **excluding one terminal envelope** (at most `ENVELOPE_MAX` =
  1 MiB, C1 §5) **and the turn's carried step rows** (§3.2) [t4r5.2]; over
  it is `NotEnqueued` before queueing, and a lifecycle atomic batch is never
  split. A terminal with its records and rows is about 1.2 MiB, a failure
  batch (one terminal and at most `FAILURE_BATCH_CANCELLATIONS` = 8
  cancellations, `runtime.rs:390` [V]) about 1.3 MiB, both inside their
  lanes' 2 MiB (§6.2).
- **Envelope meter** [t4r5.5, t4r6.4, t4r7.4]. At submission Core measures
  the envelope's base (every member with empty lists and text). It keeps the
  accumulated denied and declined entries and the final-text segments by
  key (a replacement rewrites its key's segment), and meters base plus the
  encoded bytes of every entry, key and segment. There is no key limit:
  many keys end in the same `overflow`. On the item that would exceed
  `ENVELOPE_MAX`, Core records the overrun and orders the stop (§2.3, A34);
  later items only add to their members' totals. At the terminal a counting
  writer measures the full envelope; over `ENVELOPE_MAX`, or after an
  overrun, the turn fails `overflow` and persists the summary.
- **Bounded failure summary** [t4r5.6, t4r6.6, t4r6.7] (C1 §5's overflow
  case, so R6's): each member has A30's budget and is cut as A30 states
  (`final_text` `""`, lists as prefixes of whole entries); `truncation`
  names each cut member with its full size or count. `ended_record` measures
  each member before building it and the whole summary, at most
  `SUMMARY_MAX` = 720 KiB, before persistence.
- **Blobs** (runtime §8's floor): prompts and identities over `INLINE_MAX` =
  256 KiB use the blob path, because the retry identity contains the prompt
  (`crates/via-core/src/api.rs:804-858` [V]) and a spawn command carries
  both.

### 6.6 Blob path on the raw worker [kept; F5 fixed]

- **Owner.** Only the raw worker touches `blobs/` (verification and sweep
  run on the SQLite thread before admission); validated like `raw/`; files
  `b_<32 hex>.blob`, `create_new`, 0600, `NOFOLLOW`; rows store the id.
- **Handles.** `BlobWriter::write(chunk ≤ 64 KiB)` takes the pool's chunk
  charge and the raw budget check (§6.9), acked under 2 s; a timeout or
  refusal is `not_committed` (nothing references it yet). `finish() ->
  BlobRef{id, len, sha256}` syncs the file and `blobs/`; `discard`/`Drop`
  unlink an unfinished file; `StoreClient::discard_blob(BlobRef)` unlinks a
  finished one after a commit known not to have happened (unawaited; a lost
  discard is swept). `BlobReader::next_chunk()` returns at most 64 KiB, one
  in flight. A row references a blob only after `finish`, in the same
  transaction; an uncertain commit leaves the file.
- **Exact replay comparison** (C1 byte-identical rule). An inline identity
  is compared byte for byte; a blob identity by length and SHA-256, then, on
  a match, by streaming the blob against the incoming pieces. The key lookup
  runs under `admission` as today (`receipt.rs:76`, `:202` [V]); a found row
  is committed and immutable (S1 never updates or deletes `spawn_keys` or
  `operations`), so Core releases `admission` before streaming (F5). One
  bound, `REPLAY_COMPARE` = 10 s; expiry is `store_error` for that
  request only.
- **Dispatch load.** The dispatcher waits for the prompt's pool charge
  (§5.1; the turn stays queued), loads the blob into an exact `String` with
  a running SHA-256 and a UTF-8 check (a mismatch fails the turn as corrupt
  evidence), and moves it into the streamed start (§7.3), released once the
  start is written.
- **Recovery.** `verify_blobs()` checks every referenced blob (regular file,
  length, SHA-256; else `Corrupt("blob")` at the row); `sweep_blobs()`
  unlinks unreferenced files; F16's handle-leak scan covers `blobs/`.

### 6.7 Schema v6

`SCHEMA_VERSION` becomes 6 (`crates/via-store/src/runtime.rs:24` [V]); older
development Stores are refused untouched (`:34-37` [V]). v6 is frozen by a
golden DDL test (`s1_store_v6_schema_is_frozen`). Changes from v5
(`crates/via-store/src/runtime/sql.rs:146-184` [V]):

- **`steps`** (new): §3.1.
- **`sessions`** gains `created_ms`, `updated_ms`, `harness`, `label`,
  `stamp`, `ord INTEGER NOT NULL UNIQUE`, and indexes `(updated_ms DESC,
  id)` and `(stamp)` (A12). `updated_ms` is the `at` of the transaction's
  highest-`seq` event; `stamp = MAX(stamp)+1` in every transaction that
  changes the row; `ord = MAX(ord)+1` once at spawn; both strictly increase
  while no `sessions` row is deleted.
- **Frozen `params`** gains `cwd` (absolute) and `allow_untested` (A14;
  today `receipt.rs:140` freezes only harness and model [V]).
- **`events`** gains `turn INTEGER` (deferred composite FK to `turns`),
  `type TEXT NOT NULL` (written by `insert_event` from the event JSON,
  `sql.rs:966` [V]) and the index `(session_id, turn, seq)`;
  `connection_id` gains an FK to `connections(id)`, and `insert_event`
  refuses (`Constraint`) a raw ref to another session's connection. `late`
  and `at` stay inside the JSON: no query reads them (A28).
- **`turns`** gains `ended_seq` with `CHECK((state IN
  ('completed','failed','cancelled','unknown')) = (ended_seq IS NOT NULL))`
  (A15); `prompt_blob` with `CHECK((prompt IS NULL) <> (prompt_blob IS
  NULL))` [t4r11.1].
- **`spawn_keys.identity`, `operations.identity`** become nullable with
  `identity_blob` and the same CHECK.
- **`connections`** (new) [t4r7.3]: `id TEXT PRIMARY KEY`, `ord INTEGER NOT
  NULL UNIQUE` (creation order), `session_id NOT NULL`, `turn NOT NULL` (FK)
  [t4r11.1], `high_water INTEGER` (NULL until sealed),
  `incomplete INTEGER NOT NULL CHECK(incomplete IN (0,1))`. Files are named
  from the id (`raw.rs:159-160` [V]). A per-turn connection is created in
  the submission transaction (`SubmissionRecord` gains `connection_id`);
  `incomplete` is set by a committed `raw_log.incomplete`; the transaction
  that ends the turn commits `high_water` from `FinishReport` or the failed
  open (§7.6) [t4r13.2]; recovery seals open rows from the index's last
  complete entry. Late bytes past `high_water` are not evidence: they stay
  on disk, counted (§6.9), and are never read [t4r13.1].
- **Time.** `at` strings parse strictly to Unix ms (else `Constraint`).

### 6.8 Store reads

| Read | Lane | Returns | Bound |
|---|---|---|---|
| `terminal_facts(session, turn)` | caller's | `Option<{state, cancel}>` via `json_extract(envelope, '$.cancel')`; `None` when not terminal | 4 KiB |
| `result_text(session, turn, budget)` | Public | the stored envelope text | `ENVELOPE_MAX` |
| `events_page` | Public | §4.3 | `PAGE_MAX` |
| `logs_page` | Public | §4.4 | 832 KiB, plus one unit read |
| `list_page` | Public | A12 (§6.10) | `PAGE_MAX` |
| `session_status(session, turn?, after_step, limit)` | Public | §10.3 members and a step page | `STATUS_MAX` |

`terminal_facts` replaces every internal use of the envelope `Value` read,
all of which need only existence, `state` or `cancel` [V]:
`crates/via-core/src/engine/control.rs:48` (`:210-216`), `journal.rs:550`,
`stop.rs:632`, `batch.rs:85`. C1 `result`, `wait` and `await_terminal` use
`result_text`, so no daemon path parses a stored envelope into a `Value`.

### 6.9 Disk budgets [t4r7.2, t4r8.2–4]

Runtime §6's 4 GiB, as configured budgets (§5.2, A36); nothing is reserved
per write. A write may overshoot a budget by at most one bounded
transaction or append (requirements, "Thresholds are configuration").

- **SQLite lines.** The page size is 4 KiB, set when the database is created.
  The ceiling is `max_page_count` = `(sqlite_budget − wal.max) / 4096`. At
  open Store sets it, reads back both pragmas, and refuses to start with
  `store_over_budget` when either differs (a store already above a lowered
  ceiling reads back its larger size) [t4r8.2]. The admission line is the
  ceiling less `sqlite_headroom`, compared with `page_count × page_size`.
- **Two write classes** [t4r8.2]. A terminal or lifecycle write (every
  terminal, including `failed(store)` and forced ones; cancel events;
  session close; queued-turn cancellation; `raw_log.incomplete`; seals;
  recovery's and the Latch batch's writes) may use the headroom, up to the
  ceiling. Every other write (receipts, dispatch's submission, step rows,
  durable events) is refused before `BEGIN` once the database is above the
  admission line, and checked again before `COMMIT` [t4r9.1]: the SQLite
  thread reads `page_count` inside the transaction and, above the line,
  rolls back as below. Either is a known `NotCommitted`
  (`StoreFailureKind::Quota`), so only these writes enter the headroom. A
  receipt is then `store_error` `not_committed`; a queued turn fails
  `store` at dispatch before submission; a running turn fails `store` and
  writes its terminal from the headroom (T3 §7.2, rows ride in it, §3.2).
  The headroom must hold the 16 MiB terminal reserve (four running
  terminals of about 1.3 MiB, 128 queued-turn cancellations of at most
  32 KiB, session closes), which §5.2's validation enforces.
- **`SQLITE_FULL`** at the ceiling: Store issues `ROLLBACK` and, only when it
  succeeds, reports a known `NotCommitted`; if the rollback fails, the
  outcome is uncertain and latches (runtime §7) [t4r8.2].
- **Files** [t4r8.4]. The raw worker's counter is the sum of the actual
  lengths of every file in `raw/` and `blobs/`: payloads, index entries
  (45 B each, `crates/via-store/src/runtime/raw.rs:138-145` [V]), index and file
  headers, and partial writes. It is seeded from the file lengths at open.
  Before each append or chunk the worker computes its complete prospective
  growth (payload, index entry, a new file's header) and refuses it past
  `files_budget`; admission compares the counter with `files_budget −
  files_headroom`. After any failed write it reconciles the counter with
  the affected files' lengths (`fstat`). Blob discards and the sweep lower
  it. A refused append fails its connection like a failed write (§6.4, T3
  §7.2 row 6, `raw_log.incomplete`).
- **Admission** checks both lines at a turn's receipt, at its dispatch and
  before a connection opens. Key and receipt rows are never deleted to make
  room; an actual I/O failure follows runtime §7.
- **WAL** [t4r8.3] (runtime §6's policy, as configured): a passive
  checkpoint after `wal.checkpoint_bytes` of WAL growth
  (`wal_autocheckpoint` in pages) or `wal.checkpoint_commits` commits (a
  counter on the SQLite thread); `journal_size_limit` = `wal.max`. After
  each commit the SQLite thread reads the WAL size; at `wal.max` it admits
  no write, runs `wal_checkpoint(TRUNCATE)`, and fails Store health
  (runtime §7 latch) if the WAL is still at or above `wal.max`. There is no
  prewrite prediction: the commit that crosses `wal.max` completes. The
  policy is the owner's: disk may overshoot by at most one transaction
  [t4r10.1]. SQLite appends each page a transaction dirtied once (a page it
  already wrote is overwritten), 4 KiB plus 24 B each (bundled SQLite
  3.53.2, `sqlite3.c:71575-71599` in `libsqlite3-sys` 0.38.2 [V]); the
  number of pages a maximal transaction dirties is not proved. An
  unverified estimate, assuming a split at every level for every entry, is
  about 73 MiB for a maximal event batch, and more for a terminal carrying
  many step rows [I]. `via-d9o.2.3` measures it, and the WAL's byte bound is
  an owner gate (Q-R9-1).
- **Defaults** [I]: 64 MiB of SQLite headroom leaves 48 MiB beyond the
  terminal reserve; 256 MiB of file headroom lets running turns' output
  finish, and a flood meets the ceiling and fails its own turn. Confirmed by
  `via-d9o.2.3` and `s1_store_disk_budgets_stop_admission_and_fail_visibly`.

### 6.10 `list` paging [kept: A12]

Unchanged from round 4: cursor `l2.<phase>.<v0>.<w0>.<k1>.<k2>`, strict
(`l1.` and malformed are `invalid_params`); the first page fixes `v0 =
MAX(stamp)` and `w0 = MAX(ord)`, and the population is `ord ≤ w0`; phase 1
is a keyset `(updated_ms DESC, id)` over `stamp ≤ v0`; phase 2 (only if a
population member has `stamp > v0`) scans `ord` windows of 1000, returning
`stamp > v0` matches and stopping before an unreturned match; limits 50
default, 200 maximum, `PAGE_MAX`, a row's borrowed length checked before
its summary is built.

**Proof.** Let `S0` be the first page's snapshot, `v0` its maximum
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

## 7. Wire [kept, condensed]

[V] Today `next_message` reads 8 KiB chunks and awaits every raw append
inline (`crates/via-wire/src/runtime.rs:451`, `record` `:527`);
`read_either`'s `select!` is unbiased (`:552`, `:555`); `write_message`
awaits each piece's ack (`:386`); `WireHealth` is unused
(`crates/via-wire/src/lib.rs:103`).

### 7.1 Shape (runtime §4 as written)

`open_connection` returns `WireConnection`; `into_parts(self) -> WireParts {
sender, messages }`. `WireSender: Clone` is the control handle (stdin command
senders, `Arc<ProcessControl>`, the exit and latch receivers; `write`,
`close_input`, `close`, `wait_exit`, `failure()`). `WireMessages` is unique
and owns the connection's life (the message receiver, one `pending` entry,
the task `JoinSet`, the stop signal, a `RawWriter` for the barrier;
`next_message(&mut self)`, `finish(self, deadline)`). Both belong to the
turn [t4r11.1].

### 7.2 Readers

1. A reader reads up to 64 KiB into its fixed buffer and never awaits a
   consumer, the raw worker or the Store.
2. stderr: each chunk is one raw unit (`Payload::stage`, then
   `RawWriter::submit`, not awaited).
3. stdout: a `LineSplitter` splits on LF; an unfinished message goes to the
   connection's 1 MiB assembly buffer. At LF the message (from the read
   buffer when it lies in one read) is copied by `Payload::stage(&[u8])`,
   whose staging count `max(len, 512)` is taken first, nonblocking, from the
   connection's 8 MiB staging allowance (runtime §8, inside the connection
   charge [t4r8.5]). The payload is submitted to raw, counted against the
   64-message / 4 MiB residency, and `try_send` into the queue with its ack.
   `Payload::stage` is the one constructor for stdout, stderr and stdin
   units.
4. A line over 1 MiB is `MessageTooLarge`: the prefix stays in raw and the
   reader switches to discard mode. A tail at EOF is one raw unit and the
   in-band end `Unterminated`.
5. Discard mode (after the first failure): read to EOF; stage 64 KiB units
   while only the budget refused (each refusal marks `raw_incomplete`); stop
   staging once raw is known broken; count discarded bytes.

### 7.3 Stdin writer task

One task per connection in the `JoinSet` owns `ChildStdin`, with a data
queue `mpsc(1)` and a control queue `mpsc(8)` (at most 64 KiB; duplicate
interrupt or close coalesced; runtime §8). `WireSender::write(message,
deadline)` enqueues and returns a `PendingWrite` (a cancel-safe future over
the reply `oneshot`), which Route keeps pinned while it services every
control arm (§8). The task writes piece by piece, records each written
prefix as a stdin raw unit, keeps at most four pieces unacknowledged,
awaits the rest before `Written`, and selects every await on the stop
signal and the deadline; a partial write then deadline closes stdin and
answers `Indeterminate`; a group kill surfaces as `EPIPE`. The streamed
start `OutboundMessage::Start{prefix, prompt: String, suffix}` cuts the
prompt at char boundaries into 16 KiB slices, each escaped into a reused
piece buffer, so no whole second copy exists (runtime §8 allows the fake
start beyond 1 MiB; today `FakeStart` builds one byte string,
`crates/via-routes/src/lib.rs:47-71` [V]). `close_input` is an idempotent
control command acknowledged after the endpoint drops.

### 7.4 One health state

`ConnectionLatch` (`watch::Sender<LatchState { first: Option<FailureCause>,
raw_incomplete: bool }>`) alone owns a connection's failure state, written
with `send_if_modified` by readers, the stdin writer, `next_message` (a
failed ack) and the raw worker's `RawFaultSink` (its death guard included).
First failure wins; `raw_incomplete` only goes true; every wait selects on
it; `WireConnection.evidence` is deleted. Outcomes: staging refused →
`Reader(Overflow)` + `raw_incomplete`, not a Store failure (A1); queue full
→ `Reader(Overflow)` (the message is in raw); a message over 1 MiB →
`Reader(MessageTooLarge)`; a raw write or sync failure → `Raw(Raw)` +
`raw_incomplete` (T3 row 6); raw worker dead → `Raw(WriterLost)`, Core
latches; pipe read or stdin write error → `Reader(Transport)` /
`Writer(Io)`; EOF is in-band, not a failure.

### 7.5 Consumer rules

`next_message` selects, biased: force, Route's wake, the `pending` entry's
ack (else the queue), the latch. It never reads a pipe; a wake or cancel
loses nothing (the popped entry moves to `pending` before its ack is
awaited). A message reaches Route only after its unit's durable ack, in
stream order. After a latch failure queued messages are dropped; every
committed event still cites a durable span, and the terminal carries
`raw_log.incomplete` when a unit failed. `read_either` and `drain_to_eof`
are deleted.

### 7.6 `finish`: one deadline, then adoption

`WireMessages::finish(self, deadline) -> FinishReport { raw, high_water,
adopted }` is
the only normal end: (1) the stop is already ordered (Route's close, Host's
kill, or vendor exit) and readers read the tail; (2) drain until both
readers reach EOF and the writer ends, or `deadline − 250 ms`; (3) if
anything still runs, mark `raw_incomplete`, set the stop signal and
`abort_all()` (every await is cancel-safe); (4) barrier and join together
until `deadline`; (5) `high_water` is then `durable_end` (§6.4), and
`raw_incomplete` is marked iff `enqueued_end > high_water` (bytes never
enqueued keep their own rules) [t4r13.2, t4r13.3]; unjoined tasks move into
`WireRuntime::adopt(set)`, and `finish` returns; (6) `WireRuntime` joins each adopted set as its tasks end,
and `WireRuntime::shutdown` (`crates/via-wire/src/runtime.rs:96` [V]) joins
the rest after Host's shutdown, reporting stragglers in
`WireShutdown.pending_tasks`. The connection charge (§5.1) moves with the
set and is released only when the set is empty [t4r8.5].

Once `open_connection` has returned a connection, every `run_turn` exit
calls `finish` with one absolute deadline (the graceful close's `close_by`,
the force close's cleanup deadline, or `failed.close_by`) [t4r11.1,
t4r12.2]. An open that fails before then has no `WireMessages`: the
existing path cleans up. `WireConnection::open` drops the Host acquisition,
whose anchor stops the group, drains any launched pipes into raw under
`LAUNCH_DRAIN` and returns `WireError::Acquire` with Host's evidence
(`crates/via-wire/src/runtime.rs:348-366`, `drain_pipes` `:626-667` [V]);
Route maps it (`crates/via-routes/src/runtime.rs:148-152`,
`acquire_failure` `:769` [V]). `WireError::Acquire` and `RouteFailure` gain
`high_water` with the same rule [t4r13.2]; the terminal commits it from
either path. `Drop` without
`finish` aborts, adopts, and bumps a test-only `wire::fallback_drops()`
counter that every normal test asserts is zero.

## 8. Serviceability: no wait hides a control

- **Route** keeps pinned the pending start or interrupt `PendingWrite` and
  the hop `reserve()` future (cancel-safe), and selects, biased: (1) daemon
  force; (2) turn deadline; (3) the latch; (4) `hop.closed()` → `Overflow`
  (§2.3); (5) its wake → `Control::on_wake`, which at most enqueues one
  interrupt `PendingWrite`, or returns `Stopped` at `force_at`; (6) a pending
  write completing; (7) the reserve completing, which sends; (8)
  `next_message`. [V] Today `on_wake` awaits `write_message` inline
  (`crates/via-routes/src/runtime.rs:469`) and `forward` selects only on
  send and force (`:738-757`).
- **Adapter.** The pending delivery (charge, stall timer, send) is a pinned
  future polled beside `route` and the hop; `recv` is disabled while it is
  pending, so order holds. If `route` completes first, undelivered data
  turns an `Ok` into `overflow`, as today.
- **Core.** `while_polling(&mut execute, &mut early, fut)` awaits a Store
  commit while polling the boxed adapter future and storing an early result;
  it wraps every commit in the drive loop (acceptance, events, step rows,
  `observe_order`). [V] Today a commit arm runs to completion without
  polling the adapter (`drive.rs:1292-1320`).

## 9. Connection layer and C1 ingestion (F5)

[V] Today `admit` spawns a task with no socket cap
(`crates/via-cli/src/server/serving.rs:245`); the line reader buffers up to
16 MiB with no budget or deadline, breaks silently on oversize
(`crates/via-cli/src/server/dispatch.rs:45-46`) and builds a whole-request
`Value` (`:48`).

### 9.1 Sockets, input, oversize, replies

- The accept loop owns `Semaphore(32)`; the 33rd peer is closed at once
  without bytes. The connection task is sequential (read one line, handle
  it, write the reply), so one request is in flight per socket.
- The line is read in 64 KiB segments while its charge (§5.1) grows, under
  one absolute 5 s deadline from its first byte to its LF (seam
  `VIA_TEST_PARTIAL_LINE_MS`); an idle connection has none.
- A line over 16 MiB (LF included) gets one bounded `parse_error` write
  (2 s), then the connection closes. A request `id` over 256 B is
  `invalid_request` (A31). A reply not written within `REPLY_WRITE` = 10 s
  closes the connection (A32).

### 9.2 JSON limits (A10) and no peer `Value`

- `json_limits::scan` (`crates/via-store/src/json_limits.rs`, in via-store so
  via-routes and via-core need no new edge) runs before any serde pass on
  every C1 line and vendor message: a byte scanner tracking string
  boundaries, bracket depth and a node count (every value and key), failing
  at depth 65 or node 65,537 (`parse_error` for C1,
  `RouteError::Protocol` for a vendor). It stays for a contract reason, not
  accounting: C1 requires the limits before an unbounded value is built,
  and a typed decode would allocate while nesting [t4r7.1]. On a valid
  prefix it and serde_json agree on token boundaries [I]; malformed input is
  refused by whichever finds it first.
- No `Value` is built from peer bytes: with the workspace's `raw_value`
  feature (`Cargo.toml:16`), `Value`'s deserializer re-parses a member keyed
  `$serde_json::private::RawValue`
  (`serde_json-1.0.151/src/value/de.rs:131-134` [V]). Free-form C1 members
  (`vendor`, `bound`, `effort`, `output_schema`, `max_steps`,
  `instructions`, `require`) are `Box<RawValue>`, inspected only by
  `json_limits::shape` (null, object of empty objects, or other) and
  `string_list`; the adversarial private-key document is a unit and C1
  test.

### 9.3 Decode once; stream the identity

Per line: (1) `json_limits::scan`; (2) the borrowed envelope `{jsonrpc, id,
method, params}` as `&RawValue` (`read.rs:636-650`); (3) `id` checked and
copied, `method` decoded; (4) for keyed calls the identity pass forms three
borrowed pieces around the handle span, one `Vec` up to `INLINE_MAX`, else
streamed into a `BlobWriter`; (5) the strict DTO decode; (6) a prompt over
`INLINE_MAX` streams into a `BlobWriter`; (7) drop and release.

## 10. Spawn members, `cwd`, counts and `status` members [kept]

### 10.1 Spawn members and `cwd`

[V] `SpawnParams` has none of these today, so `deny_unknown_fields` refuses
them. Added: `cwd` (at most 4096 bytes, absolute, an existing directory,
else `invalid_params`); `label` (at most 120 bytes, its v6 column);
`allow_untested` (bool, default false); `instructions` (`Box<RawValue>`;
the fake refuses any by name); `require` (`Box<RawValue>` expanded by
`string_list`, each name checked against `Capabilities::fake()`, the first
unmet refused by name, a non-list `invalid_params`). Frozen `params`
becomes `{harness, model, cwd, allow_untested}`; an omitted `cwd` freezes
the fake's configured default (`crates/via-adapters/src/fake_config.rs:44`
[V]), so it is always absolute.

`cwd` is applied: `FakeConfig::process_spec` (`fake_config.rs:53`) takes
`cwd: &Path` in place of `self.cwd` (`:75` [V]); `QueuedTurn` gains `cwd`
from `params`; the drive passes it to `execute`; the envelope
(`terminal.rs:67`, `cwd: None` today [V]) and `status` report it, and a new
fake `ReportCwd` step tests that both equal the agent's directory.

Plain conformance: the fake wall default becomes C1's 3,600,000 ms (A3;
`crates/via-core/src/api.rs:884` is 30,000 today [V]); a nested `null` in
`deadlines.*` is `invalid_params` (A9; `api.rs:88` reads it as omitted [V]).

### 10.2 `daemon/status`: `started_at` and counts (A4)

`started_at` is set once in `Engine::open`. `closing` = `Sessions.closing`
(replacing `engine.rs:110` [V]); `active` = distinct sessions in
`Unresolved` **not** closing; `open` = `Sessions.open`, seeded after
recovery with `COUNT(*) WHERE state != 'closed'`, +1 at spawn receipt, −1
on each closed-now Store answer; `idle` = `open − closing − active`,
saturating. The sum holds while the tally is exact; an uncertain close
leaves `open` stale until restart.

### 10.3 `status` durable members (A16, A23)

| Member | Source |
|---|---|
| `session_id`, `state`, `admission`, `harness`, `label`, `created_at`, `updated_at` | `sessions` (v6) |
| `model`, `cwd` | `json_extract(params, …)` |
| `route` | `json_extract(receipt, '$.route')` |
| `vendor_session_id`, `vendor_identity_verified` | `null`, `false` (A16) |
| `process.alive` | positive evidence only: the Host ledger holds a live control for an anchor the session owns, in phase `Armed` (`host.rs:104-111` [V]), whose exit watch has **not** reported an exit (`track_control` publishes the exit, `host.rs:1319` [V], while the phase stays `Armed`; `LiveControl` gains a clone of that receiver, read by `Host::live_armed(&ids)` under the ledger mutex); after a restart, `false` |
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
Each hop gains `live_armed(&ids) -> bool`; the chain lands together.

## 11. Amendments

Numbered `T4-A<n>` (`A<n>` here); new ones start at A24. Each gives the
change, the exact replacement text, and the restatements a grep found. The
orchestrator applies spec edits after review; historical reports and
decision files are not edited.

### 11.1 Status of earlier amendments

| # | Status in round 8 |
|---|---|
| A2 lanes and their 8 MiB | kept; byte split updated (§6.1, §6.2); one flat charge (§5.1) |
| A6 "session actor" is `Slot`/`Head` for follow registration | **withdrawn**: follow is removed (A25) |
| A10 a JSON node is any value or key | kept (§9.2); A35 withdrawn [t4r7.1] |
| A16 S1 definitions of `status`/`list` members | kept; extended by A26 |
| A19 stall as a stop order with cause `overflow` | **withdrawn**: the stall closes Route's hop (A29, §2.3); A34's cause `overflow` has a different trigger, the envelope overrun [t4r5.5] |
| A21 | withdrawn in round 4 |
| A22 `ENVELOPE_MAX` 704 KiB | **withdrawn**: R6 keeps 1 MiB; replaced by A30 |
| A1, A3, A4, A9, A12, A13, A14, A15, A23 | kept unchanged (A1 §7.4; A4 §10.2; A12 §6.10; A14 §10.1; A15 §6.7; A23 §10.3) |

### 11.2 New amendments

Where a long passage is replaced, it is located by file, line and opening
words; the replacement text is exact.

**T4-A24. The durable event set (R1, R2).** Amends C1 §6, §5; C2 §4;
`.repo-context/CONTEXT.md`.

C1 §6 heading becomes "## 6. Durable events". C1 §6.1 type table: delete the
rows `assistant.text`, `reasoning.summary`; `tool.started` / `tool.ended`;
`file.changed`; `usage.updated`; `vendor.other`. Add after the table:

> Events are durable records only: an event exists when crash recovery or the
> envelope depends on it. Model text, reasoning, tool calls, usage updates,
> file changes and unknown vendor messages are not events; their exact bytes
> are in the raw log (`logs`, §3.12), and a running turn's progress is in
> `status` (§3.7).

C1 §6.1 last paragraph (`via-api-v1.md:550`, "Rust: `#[serde(tag =
"type")]`…"): replace with "Rust: `#[serde(tag = "type")]` on the serialize
side, tags set with `rename`; a client keeps an unknown type as `Other {
type, payload }`." (The daemon decodes no event from a peer, §2.2 rule 4.)

C1 §5 field table: `denied_actions` and `auto_declined_requests` are
unchanged (their `event_seq` still names durable events). The `raw_spans`
row (`:508`) [t4r5.13] becomes:

> | `raw_spans` | bounding spans per connection for the turn, **not** extraction ranges on shared connections; `logs` (§3.12) returns the turn's own raw bytes |

Add after the `usage` row:

> | `steps` | model steps in the turn: the vendor's count when it reports one, else VIA's count (§3.7 step rule); `null` if the turn never started |
> | `events` | `{first_seq, last_seq, count}` of the turn's durable events (§6.1) |

C1 §3.5 and §7.3 are unchanged (a late tool completion is raw-log evidence).

`.repo-context/CONTEXT.md`: replace the **Event** definition with

> VIA's durable record of a lifecycle, control or safety fact in a session,
> such as `turn.started`, `cancel.requested`, `action.denied` or `turn.ended`
> (full list in VIA API §6). It is harness-neutral. Most events belong to one
> turn; a few (`session.opened`, `session.closed`) belong to the session.
> Each event has a dense per-session `seq`. Core commits it to the Store, and
> callers page it with `events`. Model text, tool calls and usage are not
> events: they drive the progress snapshot and step rows, and their bytes
> stay in the raw log.

In **Vendor message**, replace "An unknown message type becomes a
`vendor.other` event" with "An unknown message type is kept only in the raw
log". In **Step**, append "VIA counts a step each time the model produces
output after tool results, the same way for every vendor, and records one
`steps` row per step." After **Vendor message**, add "**Observation**: What
an adapter reports from one vendor message. Core turns an observation into a
durable event, a progress update, or envelope accumulation. _Avoid_: event
(reserved for durable records)."

Restatements: `docs/specs/adapter-contract.md:80`, `:273-276`, `:453` (A29);
`docs/specs/runtime-contracts.md:193` (A33);
`docs/specs/vendors/claude-code.md:233-244`, `codex.md:250-265`, `opencode.md:521`, `:527-531` (A33);
`docs/workstreams/rust-foundation/t3/design.md:609-615` (the idle progress
list becomes "acceptance, and progress items with model output or a tool
start or end") and `:1754` (the `s1_f12_event_not_committed_…` test moves to
a step-row commit for the stop half and `cancel.requested` for the seq-reuse
half); code `crates/via-core/src/engine/drive.rs:1844-1884` (`event_body`),
`:1775-1786` (`progress`).

**T4-A25. No follow stream (R5).** Amends C1 summary, §1, §3.8, §3.11,
§6.2–6.3, §7.6, §10; runtime §1, §2, §4, §6, §7, §8, §9, §10, §11; S1 plan.

C1 §3.11 (`via-api-v1.md:314-360`): the heading becomes "### 3.11 `events`
— page"; delete " [--follow]" from the usage line, "`follow?`, " from the
params and ", subscription?" from the result; delete the bullets on `follow:
true`, session-wide follow, outboxes and `unsubscribe`; keep the page and
`history_pruned` bullets; append "There is no follow stream: callers poll
`status` (§3.7) for progress and `wait` (§3.8) for the end of a turn."

C1, other edits:

- §1 Transport (`:79`): replace "Requests carry `id`; notifications flow
  daemon → client only for follow (§3.11). No batches." with "Requests carry
  `id` (A31); the daemon sends no notifications. No batches."
- Summary table row `events`, `logs` (`:37`): replace "canonical events
  (page/follow), raw excerpts" with "durable events (page), raw excerpts".
- §6.2–6.3 (`:553`): replace heading and text with "### 6.2 Ordering" and
  "Per session FIFO in `seq`; no promise across sessions (D4). `turn.ended`
  is the last non-late event of its turn."
- §7.6 last row (`:661`): replace "followers whose subscription ended must
  poll `result`" with "a caller that already read the result must read it
  again".
- §10 (`:748`): delete "Q1 session-wide follow; ". Row Q7 (`:761`): replace
  "Outbox and channel sizes (1000 events; C2 A1 limits)" with "Channel sizes
  (C2 A1 limits)", and "as written, config-tunable" with "as written, fixed;
  memory and disk thresholds are daemon config (runtime §8)" [t4r9.6].
- §3.8: append to the first paragraph "`wait` is the only blocking read;
  there is no follow stream. A caller that wants progress polls `status`
  (§3.7). Closing the connection of a pending `wait` releases only that
  waiter; the turn is unaffected."

Runtime §9 (`runtime-contracts.md:1080`), replace the whole section with:

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
> at most that long. `logs` returns only the addressed session's or turn's
> own raw bytes (C1 §3.12).

Runtime, other edits:

- §1 (`:28`): delete "Live observers consume durable events, never an
  independent best-effort copy."; limit 2 becomes "2. A blocked peer cannot
  be guaranteed a reply. The daemon closes the socket after the 10 s reply
  deadline; the caller retries the read."
- §2 Core row (`:77`): "seq/state decisions, subscribers" becomes "seq/state
  decisions, progress snapshots".
- §4 (`:318-319`) [t4r6.13]: "Neither waits for Route, Core, SQLite, fsync
  or a follower." becomes "Neither waits for Route, Core, SQLite or fsync."
- §6 item 5 (`:755`): "then wakes waiters/followers" becomes "then wakes
  waiters". §6 (`:771-772`): "no follower holds a transaction open while
  waiting on a socket" becomes "no read holds a transaction open while
  waiting on a socket".
- §7 table: delete the row "Following affected history" (`:941`).
- §8 table: delete the rows "Subscriber outbox" and "Subscribers"; the pool
  sentence is replaced by A36.
- §10 row "C1 §3.11": append "Superseded by T4-A25: follow removed."
- §11 table (`:1314`): replace the row "blocked socket, replay boundary
  barrier, unsubscribe barrier | F25/F26 …" with "`core.progress.publish`,
  crash after a step commit | F25/F26 (replaced): status answers from memory
  during a flood without a Store round trip; step rows survive a crash up to
  the last committed step".

Elsewhere: the S1 plan (`docs/workstreams/rust-foundation/s1-plan.md`) lines
19 and 207 drop "follow", "unsubscribe" and "subscription cleanup", and rows
F25, F26 (`:88-89`) and the F25 note (`:134`) are replaced as proposed in
Q-R5-5. T3 design §7.6 (`t3/design.md:1414-1417`) and its quoted runtime row
(`:1919`) are obsolete; no `event_end` exists. In the C1 method tables of
`docs/specs/vendors/claude-code.md:59` and `opencode.md:461` [t4r5.13],
"`status`, `wait`, `result`, `list`, `events`, `unsubscribe`, `logs`"
becomes "`status`, `wait`, `result`, `list`, `events`, `logs`": only the C1
method goes; OpenCode's own SSE subscription (`opencode.md:455`, "Subscribe
SSE first"; `:460`'s "subscriptions") is unchanged [t4r10.5]. Codex's
vendor method `thread/unsubscribe` (`codex.md:59`, `:87-88`, `:109`, `:232`,
`:395`; `adapter-contract.md:366`; `via-api-v1.md:257`) is a lease release,
not the removed C1 method, and is unchanged.

**T4-A26. `status` returns progress and step history (R3, R4, R5).** Amends
C1 §3.7 (`via-api-v1.md:274`).

After the heading add "`via status <session> [--turn N] [--after-step N]
[--limit N]`" and "Params: `session`, `turn?` (turn number; default the
running turn, else the latest), `after_step?` (default 0), `limit?`
(default 100, max 1000)." In the example, `process` becomes
`{"alive":true,"cleanup":"quiescent","idle_since":null}`, and after
`active_turn` add:

> ```
>  "progress":{"turn":2,"current_step":4,"phase":"tools","running_tools":["shell"],"tools_overflow":false,"last_activity_at":"…",
>              "tokens":{"total":18200,"scope":"vendor_interval"}},
>  "steps":{"turn":2,"items":[{"step":1,"started_at":"…","ended_at":"…","tokens":5100}],"next_after":1,"more":true},
> ```

Before "`vendor_session_id` is nullable" add:

> `progress` is an in-memory snapshot of the running turn, read without a
> Store round trip, and `null` when no turn is running in this daemon.
> `current_step` counts model steps: 0 before the vendor accepts the turn, 1
> after, and one more each time the model produces output after tool
> results, derived the same way for every vendor. `running_tools` holds at
> most 64 names of tools the vendor started and has not ended, never inputs
> or outputs; `tools_overflow` is true when more tools started than that
> since the last step boundary. `phase` is `tools` while a listed tool runs
> or `tools_overflow` is true, else `model`. Both reset at each step
> boundary, so a missed tool end misreports at most one step.
> `last_activity_at` is the arrival time of the last vendor message
> attributed to the turn. `tokens` is an approximate running total of
> completed steps, updated once per step, labelled with the route's declared
> token scope (§4.1), or `null` before the first sample; its accuracy is what
> each route's vendor evidence supports. The snapshot ends with the turn;
> the envelope's `steps` and `usage` hold the final figures.
>
> `steps` pages the durable step history of the selected turn, running or
> finished: one item per completed step, ordered by `step`, with the step's
> tokens under the same scope. After a daemon crash the history holds every
> step whose row was committed; a step whose row was being committed, and the
> step then in progress, are recoverable only from the raw log. Every
> completed step has a row [t4r5.1, t4r6.8]: a committed `turn.ended` implies
> that all the turn's rows are durable, also after a refused Store write or
> a stop forced at shutdown, except for a terminal synthesized by crash
> recovery or by the failure-resolution batch that follows a Store write of
> uncertain outcome (runtime §7).
>
> `process.alive` is true only on positive evidence that the vendor process
> is live; `process.cleanup` is `uncertain` when any process group of the
> session lacks a proof of absence, else `quiescent` (T4-A23).

Restatements: `via-api-v1.md:571` (idle shutdown: `alive` becomes false;
unchanged); A16's list (`:274-297`).

**T4-A27. `logs` returns raw excerpts by byte cursor (R5).** Amends C1 §3.12,
runtime §9. [t4r6.11, t4r7.3, t4r7.6]

Replace C1 §3.12 (`via-api-v1.md:362`) with:

> ### 3.12 `logs` — raw-log excerpts
>
> `via logs <session|turn> [--cursor C] [--limit N]`. Returns raw bytes of
> the addressed turn (or session), undecoded, in connection order then
> offset: `{entries: [{connection_id, stream, offset, len, text}],
> next_cursor}`. `stream` is `stdout`, `stderr` or `stdin`; `text` is the
> bytes as lossy UTF-8, which VIA does not interpret; `offset` and `len`
> locate them in the connection's raw log. A per-turn process's traffic
> belongs to its turn; reusable connections are defined by their adapter's
> section. Bytes are never returned
> to another session (D4). `cursor` is opaque, and an omitted one starts at
> the beginning; `next_cursor` resumes after the last returned byte (before
> any byte is durable, from the beginning), and is `null` only when every
> connection in scope is sealed, its end was reached, and no connection can
> still join the scope (the turn is terminal, or the session is closed).
> `limit` counts entries (default 100, max 256). A page returns at most
> 128 KiB of raw bytes, so it always fits the 1 MiB bound; a unit larger
> than that spans pages. Missing or corrupt raw evidence is `store_error`.

`raw_log.incomplete` keeps its one meaning, raw bytes lost (C1 §7.6,
`codex.md:299-300`); round 7's widening is reverted [t4r7.6]. Enqueued
bytes past the committed `high_water` are lost as evidence, so the terminal
carries `raw_log.incomplete` (§7.6) [t4r13.3]. Runtime §9's last
sentence (`:1119`, "`logs` resolves only each selected event's validated raw
reference…") is replaced by A25's text. Restatements: C1 §5 `raw_spans` row
(A24); `codex.md:223-224`, replace "Raw extraction uses individual event
`raw_ref`s, never shared-connection bounding spans." with "Raw extraction
(`logs`) for the shared server is defined by this adapter's task under D4:
it returns only units attributed to the thread's session and turn, never
another session's or unattributed traffic." [t4r7.3]

**T4-A28. `steps` table; event columns.** Amends runtime §6.

Runtime §6 target table: the `events` row becomes "`events`: separate
`turn` and `type` columns (late and time stay in the event JSON) |
`via-jm4.7.8`"; add "`steps`: `(session_id, turn, step, started_ms,
ended_ms, tokens)`, primary key `(session_id, turn, step)`, one row per
completed model step, written by the single writer, the last in the terminal
transaction; a session's rows are removed by one keyed delete |
`via-jm4.7.8`" and "`connections`: one per turn, sealed with the turn's end
(T4 design §6.7) | `via-jm4.7.8`" [t4r7.3, t4r11.1]. Write-ordering item 4 (`:752`): "Adapter emits
observation; Core commits acceptance or events" becomes "Adapter emits
observation; Core commits acceptance, durable events or a step row, or folds
it into the progress snapshot". Item 6 (`:756`), "Wire seals only after
EOF, raw/index sync and final metadata commit.", becomes "Wire may finish
before the last raw sync; the terminal commits the durable end as
`high_water`, which bounds `logs` and raw references. Later bytes may exist
on disk and are not evidence. A crash before that commit is recovered at
the index's last complete entry; after it, `high_water` stands." [t4r13.4] The quota paragraph is replaced by A36.

**T4-A29. Observations after R2; the stall closes the hop.** Amends C2
summary A1 and `observations` rows, §1 rule 6, §2, §4, §7; runtime §8; C1
§8.2.

C2 summary table A1 row (`adapter-contract.md:55`), replace the decision
text with:

> Backpressure: per-session observation channel of 1024 items and 4 MiB; a
> full channel blocks only that session's normalizer; control and sticky
> health travel separately and stay serviceable; Core failing to drain for
> `event_stall_ms` (10 s) fails the turn `overflow`: the adapter closes the
> session's route hop; a private route fails the connection, which
> interrupts the vendor, and a shared route quarantines that thread
> generation as for an ingress overflow (§4) while other threads continue;
> L5 staging overflow fails the connection (coding-style §5). A known
> observation payload is at most 256 KiB encoded (final text is split into
> pieces), else protocol failure with raw evidence; IDs, names, stop reasons
> and codes are at most 1 KiB each. Unknown and unattributed messages
> produce no observation.

C2 §1 rule 6 (`:80`) becomes "6. Unknown vendor notifications produce no
observation: they are kept only in the raw log and, when the route
attributes them to a turn, update that turn's activity time; a malformed
known message is a `protocol` observation." [t4r5.11]

C2 §2 "Independent lanes" (`:227-231`): replace the sentences from "The
1024-item observation queue also has a 4 MiB budget" to "explicit truncation
marker." with "The 1024-item observation queue also has a 4 MiB budget;
final text is split at character boundaries into `final_text` pieces whose
whole encoded observation is at most 256 KiB [t4r8.6]; another known payload over 256 KiB encoded fails protocol with
raw evidence."

C2 summary row `observations` (`:40`) [t4r6.13]: "C1 events minus Core
fields, plus `turn.vendor_terminal`, `turn.accepted`, `tool.quiescent`"
becomes "the durable C1 event payloads an adapter reports (`action.denied`,
`vendor.request_declined`, `steer.delivered`, `warning`), `progress` and
`final_text`, plus `turn.vendor_terminal`, `turn.accepted`,
`tool.quiescent` and the other internal observations of §4".

C2 §4, replace the first paragraph with:

> `Observation` = the C1 event payloads Core commits (`action.denied`,
> `vendor.request_declined`, `steer.delivered`, `warning`), at most one
> `progress` item per vendor message that carries a progress mark,
> `final_text` pieces, plus internal ones Core turns into commits:

In the `turn.vendor_terminal` row delete "`final_text`, " (all final text
arrives as `final_text` pieces first) [t4r7.6], and add the rows:

> | `progress` | `at`, `model: bool`, `tools_started: [(id, name)]`, `tools_ended: [id]`, `usage?: (key?, total)` | no commit: Core folds it into the running turn's progress snapshot and commits a `steps` row when a step ends (C1 §3.7). `model` marks model output (text, reasoning or a tool request); `usage` is an interval sample, never a cumulative total. A message with no mark sends no item |
> | `final_text` | `key`, `text`, `replace: bool` | no commit: Core appends the text to, or replaces, that key's part of the envelope's final text, metering key and text against the 1 MiB envelope; the item that would exceed it fails the turn `overflow` at once (C1 §5). A piece is cut so that the whole encoded observation, key, fields and escaping included, is at most 256 KiB; a longer text is several pieces, the first carrying `replace` |

C2 §7 [t4r6.13]: item 6's sentence "Tool completion and other evidence may
arrive afterward, …" (`adapter-contract.md:448-450`) becomes "Tool
completion and other evidence may arrive afterward and keep the original turn ID: a tool completion counts for P7
cleanup and stays raw-log evidence of that turn; a durable observation is
committed `late` after Core terminal commit." Item 7 becomes "7. Every
committed observation resolves its `raw_ref`, except declared synthesized
ones." In item 8, "`vendor.other`" becomes "raw-log-only activity". In item
12, "a stall past `event_stall_ms` yields an interrupt and `overflow`"
becomes "a stall past `event_stall_ms` closes the session's route hop: a
private route fails the connection `overflow`; a shared route quarantines
the thread generation (§4)".

Runtime §8 row "C2 observation payload" becomes "256 KiB encoded; final
text is split into pieces; IDs, names, stop reasons and codes 1 KiB | Fail
protocol with raw evidence; unknown messages keep no payload". In row "C2
observations", "Core fails `overflow` and interrupts (A1)" becomes "the
adapter closes the session's route hop; a private route fails the connection
`overflow`, a shared route quarantines the thread generation (A1, C2 §4)".
C1 §8.2 `overflow` row (`via-api-v1.md:711`): "this session's event channel
stalled past its limit (C2 A1)" becomes "this session's observation channel
stalled past its limit, or the turn's envelope accumulation exceeded 1 MiB
(C2 A1, §5)".

C1 §7.6 (`via-api-v1.md:657-658`) [t4r8.9]: "record normalized-event loss
separately from any actual raw gap (C2 §4)" becomes "set `raw_log_incomplete`
only for an actual raw gap (C2 §4)"; "Raw-log or event overflow failed the
connection" becomes "Raw-log or observation overflow failed the
connection"; delete ", while normalized-event loss is separately recorded".

Withdraws A19's stall trigger (A34's cause is the overrun only).
`claude-code.md:305` [t4r5.8]: "At 10 s stalled
observations Core fails overflow and interrupts;" becomes "At 10 s stalled
observations the adapter closes the route hop and the route fails the
connection `overflow`, which interrupts;". Consistent already: `codex.md:276`,
`:310-311`, `opencode.md:550`, `via-api-v1.md:657`. Code:
`crates/via-adapters/src/runtime.rs:312-331` (`deliver`),
`crates/via-routes/src/runtime.rs:738-757` (`forward`).

**T4-A30. The envelope stays 1 MiB; the transaction cap excludes it (R6).**
Amends runtime §8; C1 §5.

Runtime §8 caps a transaction at 1 MiB of payload and never splits a
lifecycle batch; R6 keeps C1 §5's 1 MiB envelope, and a terminal carries the
envelope plus `turn.ended` and owed records, so both cannot hold. Round 4
narrowed the envelope (A22); R6 rules that out.

- Runtime §8 row "Store transaction" becomes "at most 128 events and 1 MiB
  payload, not counting the one terminal envelope a transaction may carry
  (itself at most 1 MiB, C1 §5) or the turn's step rows it carries | Split
  event batches without splitting a lifecycle atomic batch; refuse a larger
  request before it is queued".
- C1 §5 (`via-api-v1.md:471-473`) [t4r5.6]: "On overflow Core fails the turn
  with class `overflow`, …as evidence" becomes "On overflow Core stops the
  turn at once, fails it with class `overflow` and persists a bounded failure summary, in which
  every member has its own budget: `failure.message` up to 2 KiB, cut at a
  character boundary; vendor and route short fields at most 1 KiB each;
  `model.requested` and `effort.requested` up to 6 KiB, else `null`;
  `bound.requested` and `bound.effective` up to 32 KiB each, cutting
  `extra_write_dirs`; `vendor_options` up to 16 KiB, else `{}`; `warnings`
  and `raw_spans` up to 16 KiB each; `final_text` empty; `structured_output`
  `null`; `denied_actions` and `auto_declined_requests` up to 256 KiB each,
  as whole entries. A summary adds `truncation`, a list of `{member, total}`
  naming each member that was cut or emptied and its full size in bytes or
  entries. The summary is at most 720 KiB; the raw log and the durable
  events remain the evidence" [t4r6.6, t4r6.7].
- **Fit.** A terminal is about 1.2 MiB and a failure batch about 1.3 MiB;
  the Latch and Lifecycle lanes allow 2 MiB each (§6.2, §6.5).
- Restatements: `runtime-contracts.md:1020` (replaced above), `:1025`
  (Envelope row, unchanged); `claude-code.md:304`, `codex.md:263` ("1 MiB
  envelope", unchanged); round-4 A22 (withdrawn).

**T4-A31. Request `id` at most 256 bytes.** Amends C1 §1. Add: "A request
`id` is a string, a number or `null`, at most 256 bytes encoded; a longer
one is `invalid_request`." Every reply's wrapper is then under 512 B, so
each page and result bound is a constant.

**T4-A32. A reply is written within 10 s.** Amends C1 §1; runtime §8. C1 §1
Transport, add: "The daemon writes each reply within 10 s of its first byte;
a peer that does not read it in that time is disconnected." Runtime §8 row
"Socket response serialization": append "| each reply written within 10 s,
else the socket closes". With no follow stream this is the only per-socket
memory a peer can hold, now bounded in time as well as size (§5.1).

**T4-A33. Fake and vendor observation mappings.** Amends runtime §3.1; the
vendor specs.

Runtime §3.1: "An unknown notification tag without `id` follows the
existing bounded `vendor.other` path." (`runtime-contracts.md:192-196`)
becomes "An unknown notification tag without `id` is raw-log-only
activity."; "§4 message splitting, §8 structure/payload limits, text
splitting and raw durability still apply." becomes "§4 message splitting,
§8 structure/payload limits and raw durability still apply." In the
tool-variant paragraph (`:225-227`), replace from "They map to existing C2"
to the paragraph's end with:

> VIA reads only `vendor_turn_id` from `text`, and `tool_id` and `name` from
> the tool messages; `text`, `input_summary`, `output_summary`, `status` and
> `exit_code` are optional and not read. A fourth progress tag is
> `{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":120}` (a
> non-negative integer): one interval sample. These map to C2 `progress`
> items (C2 §4), not C1 events.

`claude-code.md` §5 table, replace the rows:

> | `assistant.message.content` text | `progress` with `model`; final text comes from `result`, sent as C2 `final_text` pieces of at most 256 KiB encoded (C2 §4) before the terminal |
> | assistant `tool_use` | `progress` with `model` and `tools_started (id, name)`; retain the open-item set; no input summary |
> | user `tool_result` | `progress` with `tools_ended (tool ID)`; error/refusal remains error; unmatched IDs are protocol evidence |
> | assistant `message.usage` | `progress` `usage` keyed by message ID (unprobed: the pinned packet probes only `result.usage`) |
> | unknown notification | raw log only, no observation; moves the turn's activity time; cannot advance lifecycle or the idle timer |

The sentences from "Never synthesize `file.changed`" to "the tool event
suffices." (`:242-243`) become "VIA does not report file changes."; delete
"Do not expose private chain-of-thought as `reasoning.summary`." (`:244`). §6 (`:302`): "256 KiB
known observation (split text only)" becomes "256 KiB known observation
(final text split into pieces)" [t4r7.6].

`codex.md` §5: the paragraph beginning "Normalize agent-message deltas"
(`:250-253`), up to "instead of duplicating it.", becomes:

> Normalize with C2 `progress` items: `agentMessage` and `reasoning` item
> starts and deltas are `model`; a tool item's `item/started` is
> `tools_started (itemId, item type)` and its `item/completed` is
> `tools_ended`; `thread/tokenUsage/updated` `tokenUsage.last` is a `usage`
> sample. Terminal statuses map as below. For the envelope's final text only,
> send `agentMessage` deltas as C2 `final_text` appends keyed by item ID and
> the completed text as that key's replacement, so completed text never
> duplicates its deltas; each piece's encoded observation is at most 256 KiB
> (C2 §4) [t4r6.4, t4r7.6, t4r8.6].

At `:264-265`, "Large text splits on UTF-8 boundaries; unknown notifications
become `vendor.other` retaining at most 16 KiB with explicit truncation"
becomes "Final text is split into C2 `final_text` pieces of at most 256 KiB
encoded (C2 §4);
unknown notifications are raw-log-only activity".

Codex §5 late detail [t4r5.13]: the sentences from "Thus an already received
or later delivered completion" to "admission to that session is closed."
(`:230-234`) become:

> Thus an already received or later delivered completion after lease
> release is still attributed to its original turn: its raw units carry
> that attribution (`logs`), it still counts for P7 cleanup
> (`tool.quiescent`), and any durable observation it yields
> (`action.denied`, `vendor.request_declined`, `warning`) is committed with
> `late:true`. A tool completion alone is no event (C1 §6.1).
> `thread/unsubscribe` does not promise more vendor notifications. A
> detached session's Core observation sink remains eligible for these late
> observations even though admission to that session is closed.

[t4r6.13] `codex.md:356-357`: "Later tool events remain `late:true`
evidence" becomes "Later tool completions remain raw-log evidence of the
turn". In the `codex_two_threads` row (`:395`), the clause "Deliver an A
completion after uncertain settlement … never session-level/B;" becomes
"Deliver an A tool completion after uncertain
settlement and again after A lease release while B is active: both stay
raw-only evidence attributed to A's original turn (`logs`), count for A's
P7 cleanup, produce no event, and never reach session level or B;".

`opencode.md` §5: from "Map text deltas and authoritative part snapshots"
(`:527-530`) to "Unknown notification types become bounded `vendor.other`;"
becomes:

> Map assistant text and reasoning parts to C2 `progress` `model`, a tool
> part entering `running` to `tools_started (call ID, tool name)` and one
> entering `completed` or `error` to `tools_ended`, correlated by
> session/message/part/call IDs; each assistant message's token snapshot is
> a `usage` sample keyed by message ID (§7 one ledger). Final text is the
> correlated completed assistant's text, sent as a C2 `final_text`
> replacement keyed by message ID, in pieces of at most 256 KiB encoded
> (C2 §4), so the 256 KiB normalized observation maximum holds [t4r6.4,
> t4r7.6, t4r8.6]. Unknown
> notification types are raw-log-only activity;

and at `:521`, "`session.next.*` events are retained as bounded
`vendor.other`, never a second text/tool/usage emission" becomes
"`session.next.*` events are raw-log-only, never a second text/tool/usage
emission".

**T4-A34. Stop cause `overflow` for an envelope overrun** [t4r5.5, t4r6.5,
t4r7.5]. Amends T3 §2 (`t3/design.md:148`, `:165-171`, `:185-187`,
`:252-254`) and its disposition table; C1 §7.6.

- **Cause.** T3 §2's cause list becomes "`cancel`, `close`, `idle_deadline`,
  `store` or `overflow`". Core sends `overflow` only on the item that would
  take the turn's envelope over 1 MiB (§6.5); the stall does not use it
  (A29).
- **Deadlines.** T3's table gains "| `overflow` | now | `min(now + 3 s,
  wall_deadline + 3 s)` |", as for `store`.
- **Coalescing.** T3's rule stands, with causes ranked `store` >
  `overflow` > the rest; `TurnStop::attach`
  (`crates/via-core/src/engine/queue.rs:116-120` [V]) gains the arm.
- **Disposition.** T3 §2 gains, after "Cause `store` overrides every other
  cause's row": "Cause `overflow` overrides every row except cause
  `store`'s: once Core has recorded the overrun, the result is
  `failed(overflow)`, `stop_reason: error`, whatever evidence follows (a
  vendor terminal of any status, a deadline, a forced stop, a process
  exit). Any turn write known not committed, before or after the overrun,
  gives cause `store` precedence; a write of uncertain outcome follows the
  latch rule (§7.4)." T3's table (`t3/design.md:279-280`) gains "cause
  `overflow` (any evidence) | `failed(overflow)`, `stop_reason: error`, the
  bounded summary (A30) | `forced` or `requested`, cleanup by the rule", and
  its "daemon force took over" row adds "for cause `overflow`,
  `failed(overflow)`".
- **C1 §7.6.** Add before the first row: "| Envelope accumulation exceeded
  1 MiB (§5) | running | `failed(overflow)`, `stop_reason: error`, the
  bounded summary; Core stops the turn at once, and no later vendor terminal
  or deadline changes the result. A turn write known not committed, before
  or after, takes precedence (`failed(store)`, §8.2); a write of uncertain
  outcome follows runtime §7's latch rule |".
- **Code.** `StopCause` (`crates/via-routes/src/lib.rs:266` [V]) gains
  `Overflow`, with arms in `crates/via-core/src/engine/terminal.rs:152`,
  `:170`, `:245`, `:263` and `forced_terminal` (`stop.rs:396`) [V].

**T4-A35** is withdrawn [t4r7.1]: the C1 request charge (§5.1) covers
serde's scratch buffer at four times the line, so no key limit is needed;
C1 §1, runtime §8's JSON row and `codex.md:263-264` stay as written.

**T4-A36. Coarse memory and disk bounds** [t4r7.1, t4r7.2, t4r8.1–4,
t4r8.9]. Amends runtime §6 and §8; `codex.md` §5.

Runtime §8 (`runtime-contracts.md:1057-1060`): replace the two sentences
from "The global daemon retained-payload allocation budget" to "acquiring a
global permit is required." with:

> The global daemon retained-payload budget is one pool, 128 MiB by default
> and configurable (daemon config), that includes SQLite's page cache (8 MiB
> by default). Each class of retained buffer (Store lanes, the SQLite cache,
> a connection, a running turn, a C1 request, a reply, a blob chunk, a
> dispatched prompt) takes a conservative flat charge, configurable, from
> the pool before it allocates; per-queue maxima are limits inside a charge,
> not separate permits. The charges are assumptions, confirmed by F24's RSS
> gate and the pool's high-water mark.

Runtime §8, other edits: at `:1041-1044`, the two sentences from "Use
byte-permit wrappers" to "a peer size or item count." become "Each class
charge is an RAII permit held while its buffers live, including data waiting
for sync, decode, channel send, serialization or task join; nothing is
preallocated from an unchecked peer size or item count."; at `:1063-1064`,
"byte-permit high-water <=128 MiB" becomes "pool high-water at most the
configured pool"; at `:1067-1069`, the sentence from "The Codex 16 MiB
permit is a sub-budget" to "retained-payload budget." becomes "The Codex
class (`memory.codex_shared`, 16 MiB by default, configurable) is a
sub-budget for observation/staging lanes and retained tool metadata,
charged from the global pool, not an extra allocation beyond it." [t4r9.6];
row "Store requests" (`:1019`): "8 MiB total" becomes "8 MiB
total by default, configurable"; row "Codex shared Route ingress"
(`:1013`): "16 MiB global permit for" becomes "a `memory.codex_shared`
class charge (16 MiB by default) from the global pool for" [t4r9.6].

Runtime §6 (`:674`): "SQLite cache target 8 MiB" becomes "SQLite cache
target `memory.sqlite_cache` (8 MiB by default, configurable)" [t4r9.6].

Runtime §6 (`:773-777`): replace the sentences from "Configure an initial
4 GiB Store+raw logical quota" to "never shortened by quota pressure." with:

> Disk has two hard budgets, configurable (daemon config), by default 1 GiB
> for SQLite with its WAL (`max_page_count`) and 3 GiB for raw and blob
> files (the sum of their lengths). Turn admission, connection open and
> every ordinary write compare actual sizes with each budget less a
> configured headroom (64 MiB and 256 MiB by default) and refuse by name
> above it; terminal and lifecycle writes may use the headroom, session
> close and queued-turn cancellation included. A write may overshoot a
> budget by at most one bounded transaction or append. A turn that meets a
> hard limit fails with class `store`. The budgets are no guarantee against
> filesystem-full: actual I/O failures still follow §7. Key/receipt
> lifetime is never shortened by budget pressure.

Runtime §6 checkpoint sentence (`:769-771`), from "Checkpoint after 8 MiB WAL
growth" to "if growth cannot be bounded.", becomes "By default and
configurably, checkpoint after 8 MiB of WAL growth or 1000 commits; at a
32 MiB WAL, stop write admission while attempting a truncating checkpoint,
and fail Store health if it cannot bring the WAL below that limit. The
commit that crosses the limit completes, so the WAL overshoots by at most
one transaction." [t4r8.3]

`codex.md`: at `:315`, "Add a 16 MiB global permit pool" becomes "Charge the
`memory.codex_shared` class (16 MiB by default) from the global pool
(runtime §8)"; at `:237`, "charged to the global 16 MiB permit pool" becomes
"charged to that class" [t4r9.6]; at
`:299-300`, "Core records explicit normalized-event loss with the overflow,
and" becomes "Core fails the affected turns `overflow`, and records".
Unchanged and consistent: runtime §4's staging permits (`:320`, inside the
connection charge); C1 §8.2's `store` row.

**T4-A37. Daemon config for thresholds** [t4r8.1]. Amends runtime §6.1,
§8; C1 §3.14.

Runtime §8, add after the pool paragraph: "Memory and disk thresholds (the
pool, class charges, disk budgets and headrooms, WAL limit and checkpoint
triggers) are keys of `daemon.json` in the state directory, with
provisional defaults. The daemon reads it once at start; a change takes
effect at the next start, and an invalid file refuses to start with a named
error. C1, C2 and the other runtime §8 limits are not configurable."
Runtime §6.1 state layout (`runtime-contracts.md:805-812`): add the line
"`daemon.json  optional daemon config: memory, disk and WAL thresholds
(§8)`" after `store.lock` [t4r9.8]. C1 §3.14 (`via-api-v1.md:375-377`):
after "socket_path, store_path, health" add ", limits"; after that sentence
add "`limits` holds the effective memory, disk and WAL thresholds."
[t4r9.7]

## 12. Tests (failure-first)

Each test is written first, fails on the code as found for the stated reason,
and passes once the mechanism lands. Names follow runtime §11 (`s1_fNN_`,
`s1_raw_`, `s1_bounds_`, `s1_store_`, `s1_blob_`, `s1_wire_`, `s1_c1_`,
`s1_progress_`). Coding-style's testing rules apply (synchronization points,
never fixed sleeps as ordering; seeded inputs, here via `VIA_TEST_SEED`, not
`proptest`, A13). Also: time rules run with lowered seams, in-process units
on a paused tokio clock; group commit is asserted by counting `sync_data`;
heavy tests (floods, 16 MiB lines, 32 sockets, 100,000-unit lookups) compile
only under `test-failpoints`, keeping the default suite near two minutes;
every daemon scenario runs through `run_scenario` with an `Evidence`
(`crates/via-cli/tests/support/scenario.rs:111` [V]).

### 12.1 Seams

All under `#[cfg(feature = "test-failpoints")]`, added to
`scripts/check-release-features.py`'s `POINTS`: `Store::raw_sync_count()`,
`Store::raw_index_reads()`, `Store::stall_raw_worker()` and
`raw.sync.fail_persistent` (existing, `runtime.rs:1062` [V]),
`raw.worker.panic_after_dequeue`, `store.writer.before_serve`,
`Lanes::high_water(lane)`, `MemoryBudget::high_water()`,
`store.commit.step`, `core.observations.pause`, `core.progress.publish`,
`Store::read_count()`, `store.read.delay_ms`, `store.rollback.fail`,
`VIA_TEST_EVENT_STALL_MS`,
`VIA_TEST_PARTIAL_LINE_MS`, `VIA_TEST_REPLY_WRITE_MS`,
`wire::fallback_drops()`, `wire.ack.observe_delay` [t4r13.5], `raw.inbox.refuse` [t4r14.1],
`blob.write.fail_after`,
`Store::blob_chunk_reads()`, fake-agent steps `HoldStdin`, `ReportCwd`,
`EchoPromptDigest` (`Emit`, `Gate` and `Flood`,
`crates/via-fake-agent/src/main.rs:41-62` [V], already emit any line), and
`/proc/<pid>/status` sampling. Budgets and charges are lowered through
`daemon.json` (§5.2), not a seam.

### 12.2 Scenarios

| Test | Proves |
|---|---|
| `s1_progress_step_rule_counts_output_after_tool_results` | fake: text, tool_started, tool_ended, text, text, tool_started, tool_ended, text → `current_step` 3; rows 1–2 committed before the terminal, row 3 in the terminal transaction (asserted by `store.commit.step` barriers) |
| `s1_progress_snapshot_adds_no_store_read`; `s1_c1_status_latency_under_bounded_store_delay` [t4r5.14] | `status` on a running turn and on an idle session issue the same number of Store reads (`Store::read_count()`); with `store.read.delay_ms` = 200 it answers within 300 ms while the turn progresses |
| `s1_progress_tokens_sum_per_step_and_label_scope` | two `usage` samples in one step replace (no key), steps add; `tokens.scope` equals the fake's declared scope |
| `s1_progress_tools_overflow_and_untracked_end_count` [t4r6.9] | 70 concurrent tool starts: 64 names and `tools_overflow` true; the end of an untracked tool, then model output, advances `current_step` and writes a row; `phase` stays `tools` until that boundary, which clears both |
| `s1_progress_unknown_messages_send_no_observation` [t4r5.11] | a flood of unknown fake messages sends no C2 item (channel high-water 0) yet moves `last_activity_at` |
| `s1_progress_step_rows_survive_crash_to_last_commit` | daemon killed after row 2's commit and before row 3: restart shows rows 1–2 and the turn `unknown` |
| `s1_progress_step_commit_refused_rows_ride_in_terminal` [t4r5.2] | `store.commit.step` known failure on row 2: `failed(store)`; rows 2, 3 and the open step's row commit in the resolution transaction with `turn.ended`; every step has a row |
| `s1_progress_many_steps_all_have_rows` [t4r5.1]; `s1_progress_forced_shutdown_terminal_carries_open_row` [t4r6.8] | 20,000 fake steps give 20,000 rows; a turn forced by final shutdown in step 3 commits row 3 with its forced `turn.ended` |
| `s1_store_steps_delete_is_one_keyed_range` | `EXPLAIN QUERY PLAN` of the delete uses the primary key; two sessions interleaved, one deleted, the other intact |
| `s1_c1_status_every_member_after_eviction_and_restart` | durable members equal before and after; `progress` null after restart; `steps` paged |
| `s1_c1_status_alive_false_after_exit_before_control_drop` | exit observed, control still upgradeable → `alive` false |
| `s1_c1_events_page_filters_and_bounds`; `s1_c1_follow_and_unsubscribe_are_refused` | window, `types`, `turn`, `next_after` across filtered rows, `more`, the byte bound with a Store-level fixture; `follow: true` is `invalid_params`, `unsubscribe` `method_not_found` |
| `s1_c1_logs_pages_raw_bytes_by_cursor_and_isolates_sessions` [t4r11.1] | a 1 MiB control-character unit spans pages under 1 MiB each; a cursor for another session's connection, for another turn's connection under a turn address, or beyond a connection's end is `invalid_params`; after a crash mid-turn the turn's connection is readable and recovery seals it |
| `s1_c1_logs_end_cursor_resumes_while_running` [t4r5.12, t4r6.11] | a call before any byte is durable returns `r1.start`, and a later call from it returns the first bytes; on a running turn `next_cursor` is non-null at the current end and a later call from it returns only new bytes; after the terminal and seal it is `null` |
| `s1_store_disk_budgets_stop_admission_and_fail_visibly` [t4r7.2, t4r8.2–4, t4r9.1, t4r9.5, t4r10.1] | lowered budgets: above the SQLite admission line a spawn is `store_error` `not_committed`; an ordinary write begun just below the line that would commit above it is rolled back `NotCommitted` and leaves `page_count` at or below the line; a queued turn fails `store` at dispatch, and a running turn's step row is refused and the turn fails `store`, while its terminal, a session close and queued-turn cancellations commit from the headroom; a terminal forced to `SQLITE_FULL` is rolled back and reported `NotCommitted` (a failed rollback latches); a raw flood fails its turn with `raw_log.incomplete` and the counter equals the summed file lengths, index entries and headers included, after a failed write and after restart; a lowered budget below the store's size refuses start (`store_over_budget`); a WAL held above `wal.max` by a reader fails health; the WAL growth of a maximal event batch and of a terminal with many carried rows on a near-ceiling store is recorded for `via-d9o.2.3` (no bound asserted, Q-R9-1) [t4r10.1] |
| `s1_raw_loss_by_offsets_and_logs_stop_at_high_water` [t4r13.5, t4r14.1] | (a) the raw worker stalled (`Store::stall_raw_worker()`) with a stdout append enqueued at `finish`'s deadline: the terminal commits `high_water` = `durable_end` with `raw_log.incomplete`; resumed, the late bytes land past `high_water`, and `logs` and `raw_ref` resolution stop at it; (b) every unit durable when the barrier answers: `high_water` = `enqueued_end`, no flag; (c) acks observed out of order (`wire.ack.observe_delay` holds the stdout task's observation while a later stderr ack arrives): `high_water` and the flag match (a) and (b); (d) a saturated inbox (`raw.inbox.refuse` makes `try_send` report full) [t4r14.1]: the reader does not block, the connection fails `Raw(Raw)` with `raw_log.incomplete`, `enqueued_end` is unchanged, and the accepted units' offsets equal their file positions; (e) a failed open whose drain times out with an append queued: `WireError::Acquire` carries `high_water`, the terminal commits it with the flag; (f) a crash after the terminal commit, before and after the late append lands: restart keeps `high_water`, `logs` stops at it, and the byte counter equals the files' lengths |
| `s1_c1_request_id_over_256_bytes_is_invalid_request`; `s1_c1_reply_not_read_closes_the_socket_and_frees_the_permit` | A31; A32, with `VIA_TEST_REPLY_WRITE_MS` |
| `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` | four turns, 256 MiB stdout flood; daemon peak RSS < 256 MiB, growth < 32 MiB after the first 64 MiB, each anchor ≤ 32 MiB, sum < 384 MiB, `MemoryBudget` high-water ≤ 128 MiB, the SQLite cache's 8 MiB included [t4r7.1]; `daemon/status`, `status` and a `cancel` of another turn answer within 100 ms, including with its interrupt write blocked at `HoldStdin`; the flood turn ends `failed(overflow)` with `raw_log.incomplete` |
| `s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output` | Core held at `core.observations.pause`, vendor silent after filling the channel: `overflow` at the lowered stall with no further vendor byte |
| `s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib` | the C2 bounds |
| `s1_f24_envelope_overrun_stops_the_turn_at_once` [t4r5.5, t4r6.4, t4r6.5] | a test route emits declined requests past 1 MiB and then stays silent, and another streams `final_text` appends past it: the stop order with cause `overflow` is sent on the crossing item; a vendor `completed` terminal that follows still ends `failed(overflow)` |
| `s1_bounds_failure_summary_bounds_every_member` [t4r5.6, t4r6.6, t4r6.7] | a turn with 1 MiB escaped `final_text`, a 64 KiB `failure.message`, 1 KiB vendor fields, a 6 KiB + 1 `model`, 2,000 `extra_write_dirs`, a 64 KiB `vendor` map and 600 KiB of declined requests: `final_text` empty with its length in `truncation`, the message cut to 2 KiB, each member within its budget, lists as prefixes, `truncation` totals exact, the summary at most 720 KiB; a 1 KiB + 1 vendor field is `protocol` |
| `s1_bounds_class_charges_cover_measured_peaks` [t4r7.1, t4r11.2] | one counting-allocator binary: no reallocation on the stdout, C1 line, reply or encode paths at their maxima; C1 decode of a 16 MiB prompt of six-byte escapes and of a 65,536-node list each allocate at most the C1 request charge; a maximal connection and drive allocate at most their charges; a connection whose tasks were adopted keeps its charge until they are joined [t4r8.5]; a `final_text` piece of six-byte escapes with a 1 KiB key encodes to at most 256 KiB [t4r8.6]; a request that cannot be charged is `admission_refused` (`MEMORY_BUDGET`); a request's charge is fully released before its reply charge is acquired, so with the pool at its validated floor and four turns holding their charges a `logs` request gets its 2 MiB reply [t4r11.2] |
| `s1_config_is_read_at_start_validated_and_reported` [t4r8.1, t4r9.2, t4r9.5] | a missing file gives the defaults; lowered values change the pool and budgets only after a restart; an unknown key, a duplicate key, a headroom below the terminal reserve, each key one below its minimum, a value past its range, a sum that overflows `u64`, `checkpoint_bytes` of 0 or 4095 and a group-writable file each refuse start with exit 78 and the named key and rule, touching no Store or socket; `daemon/status` `limits` equals the effective values |
| `s1_f05_…` (oversize, depth and nodes, partial line, 33rd socket) | F5 |
| `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_raw_bytes` | F27, with a seeded splitter test |
| `s1_raw_…`, `s1_store_…`, `s1_wire_…`, `s1_blob_…` | round 4's mechanism tests for kept mechanisms: group commit by count, death guards, lanes and fence, Latch fit with the largest `cwd` (end to end with the longest creatable path; the 4096-byte bound with synthetic records), Public saturation at the Store level (not through sockets), binary lookup, finish adoption, torn and mismatched blobs, replay compare outside `admission` with a stalled reader |

## 13. Task 4 scope disposition (`t4/t0.md`)

Kept: F5 (§9), F24 (§5, §8, §12), F27 (§7.2); the C1 methods, `serve
--stdio` and CLI options (§4, §10); per-pipe Wire readers (§7.2); JSON
limits before a `Value` (§9.2); A1, A2 (§6.1, §6.2), A3, A9; `daemon/status`
(§10.2); the 1024 / 4 MiB observation budget, on the reduced set (§2.3);
strict paging DTOs, `logs` by cursor (A27); blob path, schema v6, Store
shutdown ordering. The 256 KiB rule holds with final text in pieces (A29);
memory is charged per class (§5); runtime §6's quota is built as two disk
budgets (§6.9, A36) [t4r7.2]; thresholds are daemon config (§5.2, A37;
`via-jm4.7.8.1`) [t4r8.1]. Obsolete: F25 and F26 (no follow stream;
replacement scenarios in Q-R5-5), `events` follow, `unsubscribe`, follower
cleanup (A25), Wire `read_either`. Out of scope: durable `output_schema`
(§0).

## 14. Limitations

| Limitation | Revisit when |
|---|---|
| A maximal 16 MiB C1 request is charged about 66 MiB, so it is admitted only while at most one turn runs; otherwise `admission_refused` | real routes measure prompt sizes, or the counting-allocator test allows a smaller charge |
| The class charges (§5.1) and disk headrooms (§6.9) are assumptions; admission near a budget refuses work that would have fit; uncharged allocator and SQLite overhead is left to the RSS gate | a check shows a charge too small, refusals too early, or the RSS gate fails |
| `running_tools` lists at most 64 names plus a flag; a missed tool end misreports `phase` until the next boundary; tokens are approximate under the declared scope, and unprobed for Claude, Codex and OpenCode (§2.5), so `tokens` may be `null`; R3's accuracy is not claimed for them [t4r8.10] | a caller needs exact live figures; each vendor's probe; the owner's accuracy decision (Q-R5-11) |
| Reusable connections (per-session and shared servers) are not designed here; §4.4 lists the constraints their tasks must meet [t4r11.1] | the OpenCode (`via-4sw.3.2`) and Codex tasks |
| A step that ended while its row commit was in flight is lost in a crash | the owner's R4 wording decision (Q-R5-15) [t4r8.10] |
| The open-session tally is exact only until an uncertain close | Store-failure recovery work |
| `stamp` and `ord` increase strictly only while no `sessions` row is deleted; `list` phase 2 examines every `ord` up to `w0`; blob verification at start is linear in blob bytes | retention (`via-jm4.18`), or session counts make it slow |
| `revision` is 0; `process.idle_since` is `null` (A16) | late evidence; vendor idle shutdown |
| Inferred: the §6.2 lifecycle count, the terminal and failure-batch sizes (§6.5), the 128 B step row, and that `json_limits::scan` and serde_json agree on token boundaries | the checking test fails |
