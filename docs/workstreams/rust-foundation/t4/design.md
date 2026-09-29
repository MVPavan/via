# Task 4 design: events, progress, storage and C1 conformance (round 16)

Status: normative design for Task 4 (Bead `via-jm4.7.8`, step T4-0), round 16.
Round 15 was SOUND at `778752c`. Round 16 applies the owner's revised
[requirements](requirements.md) (marked "r16") and
[`design-r16-decisions.md`](design-r16-decisions.md), tagged `[t4r16.N]`
(`[t4r16.5.k]` for item k of decision 5, `[t4r16.7.N]` for item N of the
owner's follow-up, decision 7); older tags mark reasoning that
still stands. The requirements (R1–R8) override earlier design and spec
text; every conflict is an amendment in §12. Round history:
[reports/T4-0.md](reports/T4-0.md) §9–§20. This step writes no code and no
tests.

Round 16 removes VIA's raw log (R8: an evidence folder per turn takes its
place) and the memory pool (R7: memory is bounded by construction). Disk
budgets give way to a free-space floor, and the WAL limit refuses writes
instead of latching. The owner's follow-up keeps the simplest behaviour for
problems not yet observed (uncapped agent stderr, a final text too long for
the envelope written to a file, `wait` once per second) and lists each for
measurement (§16).

Sources: requirements; C1 (`docs/specs/via-api-v1.md`); C2
(`docs/specs/adapter-contract.md`) with A1; runtime
(`docs/specs/runtime-contracts.md`); `docs/specs/vendors/`; [T3
design](../t3/design.md); `.repo-context/invariants.md`;
`.repo-context/CONTEXT.md`. Tags: **[V]** verified at the cited `file:line`
of `wt/t4-0` at `9ff3e84` (code and specs unchanged since `ea28956`) or of
dependency source (`serde_json` 1.0.151, `tokio` 1.53.1, `rustix` 1.1.5,
`libsqlite3-sys` 0.38.2); **[S]** stated by the cited spec; **[U]**
unverified vendor behaviour, pinned by the vendor slice before use; **[I]**
inferred, checked first by the implementing slice; **[V, probe]** from the
orchestrator's 2026-09-29 probe of Claude Code 2.1.284, Codex 0.159.0 and
OpenCode 1.18.32 (`design-r16-decisions.md:16-31`). Every mechanism names
its owner, and every queue is bounded and says what happens when it is full.

## 0. Scope and fixed decisions

Fixed: runtime §7 with T3's amendments (a known Store outcome is scoped, an
uncertain one latches); Core owns every absolute deadline; C2 A1 (1024 items
and 4 MiB per session, a full channel blocks only the normalizer, 10 s
without drain fails the turn `overflow`); no new dependency, debug RPC or
CLI verb outside C1; R1–R8.

What the requirements change:
- SQLite keeps lifecycle, control and safety events plus the envelope (R1).
  Text, reasoning, tool and usage messages feed only an in-memory progress
  snapshot (R3) and one `steps` row per model step (R4).
- VIA keeps no copy of vendor traffic (R8). The agent's transcript holds the
  conversation; the turn's evidence folder holds its stderr, a message VIA
  could not decode, and a final text too large for the envelope. The
  daemon's own warnings go to `via.log` [t4r16.7.1, t4r16.7.2].
- Callers poll `status`, block on `wait`, page `events`, and ask `logs` where
  the evidence is (R5). There is no follow stream.
- Every buffer has a fixed maximum and every kind of holder a fixed count
  (R7). Disk has a free-space floor for admission and a size warning.
- Problems not yet observed get the simplest behaviour, and §16 lists each
  for measurement in `via-d9o.2.3` [t4r16.7.9].

Coordination primitives (A13): `tokio::sync::{watch, Notify, Semaphore,
mpsc, oneshot}` and `JoinSet`. A task's owner creates its stop signal, owns
its `JoinSet`, and stops, drains and joins it within a bound.

| Out of scope | Why | Revisit when |
|---|---|---|
| `describe`/`models` beyond `fake` | one route in S1 (`crates/via-core/src/engine/receipt.rs:97` [V]) | the first real route |
| Durable `output_schema` state | the fake refuses it (`crates/via-core/src/api.rs:464` [V]) | a route supports it |
| Store operation watchdog (runtime §8) | not a Task 4 item | the S1 close review |
| Blob-backed `effective` | a few hundred bytes (`api.rs:979` [V]) | a route accepts large values |
| Retention | non-goal; `via-jm4.18` | that task (§3.4, §7.5 give its deletes) |
| Reusable connections (OpenCode's per-session server, Codex's shared server) and their evidence | R8: those tasks | `via-4sw.3.2` and the Codex task |

## 1. Owners, lock order and wakes

| State | Owner | Created by | Written by | Ended by | Bound |
|---|---|---|---|---|---|
| Daemon config | daemon start | read once before `Store::open` | never | daemon end | §5.5 |
| Store lanes (Latch, Lifecycle, Internal, Public) | `Store`; the SQLite thread serves them | `Store::open` | `StoreClient` handles by lane | fence, or writer death | §6.1 |
| A blob file | its `BlobWriter`/`BlobReader` | the handle | the handle, on the blocking pool | handle drop; start-up sweep | §6.5 |
| Evidence root `<state>/evidence` | Store | `Store::open` | — | never | §7 |
| A turn's evidence folder | Wire connection | `open_connection` | the OS (stderr); Wire (undecoded message); Core (final text file) | retention | §7 |
| `final_text.txt` | its `FinalTextFile` handle, held by the drive | the first piece past 256 KiB | the handle, on the blocking pool | `finish` before the terminal | 64 MiB (§6.4) |
| `via.log` | `DaemonLog` (the daemon's `tracing` writer) | daemon start, after both locks | any daemon task, one line at a time | daemon exit | rotated at start past 10 MiB (§7.6) |
| `ConnectionLatch` | Wire, per connection | `open_connection` | reader, stdin writer | last holder after `finish` | §8.4 |
| Reader and stdin-writer tasks | `WireMessages` | `open_connection` | the tasks | `finish` | §8.6 |
| Message queue | `WireMessages` | `open_connection` | stdout reader | `next_message` or `finish` | 64 messages, 4 MiB |
| Route → Adapter hop | Adapter `execute` | per drive | Route | end of `execute` | 1 message |
| Observation channel and byte semaphore | Core drive | per drive | Adapter | end of the drive | 1024 items, 4 MiB |
| Stall timer | Adapter pending delivery | first blocked send | Adapter | that item accepted, or 10 s | one per drive |
| Envelope accumulation, step tracker | Core drive (`TurnRecord`) | submission | the drive | the terminal commit | §6.4, §2.4 |
| Turn activity clock (`AtomicU64`) | Core drive; a clone in `Running` | submission | Adapter, per attributed message | end of the drive | 8 B |
| Published progress | `Slot` `Running` entry (`crates/via-core/src/engine/queue.rs:274` [V]) | `Running` creation (`:488` [V]) | the session's drive | `finish_running` (`:593` [V]) | §2.4 |
| Data-size cache | `Engine` | first `daemon/status` | `daemon/status`, at most once per 60 s | never | §5.3 |
| Open-session tally, closing set | `Engine` (`Sessions`) | `Engine::open`, from Store | receipts, closed-now answers | never | §11.2 |
| Live `Armed` controls | Host ledger (`crates/via-host/src/host.rs:72` [V]) | control verification | `Capacity::armed` | control drop | one per anchor |

**Lock order.** T3 §1 stands (`admission` → `sessions` → slot state; `Head`
as T3 orders it; no std mutex across an `.await`). An async owner's lock may
briefly take the `Lanes` mutex, never the reverse; code holding `Lanes`
takes no other lock, awaits nothing and runs no callback. Progress takes
only the slot state mutex, without await.

**Wakes** are hints; the receiver re-reads the owning state: the force
signal (`crates/via-core/src/engine/latch.rs:446` [V]), the connection latch
(§8.4), Route's wake for a stop order or `force_at`
(`crates/via-routes/src/runtime.rs:516` [V]) and the closed hop (§2.3).

## 2. Events, observations and progress (R1–R3)

### 2.1 Durable events (R1, R2)

An event is durable when crash recovery or the envelope depends on it. Core
commits exactly these C1 §6 types:

| Type | Source |
|---|---|
| `session.opened`, `session.reopened`, `session.closed` | Core |
| `turn.queued`, `turn.submitted`, `turn.started`, `turn.ended`, `turn.revised` | Core |
| `cancel.requested`, `cancel.settled`, `steer.delivered` | Core / Adapter observation |
| `action.denied`, `vendor.request_declined` (the envelope lists cite their `seq`) | Adapter observation |
| `process.exited`, `server.lost`, `warning` | Core / Adapter |

Not events (R2): `assistant.text` and `reasoning.summary` (a `model` mark),
`tool.started` and `tool.ended` (tool marks), `usage.updated` (a usage mark),
`file.changed` (a non-goal) and `vendor.other` (activity only). The raw-log
event is gone, and events carry no `raw_ref` [t4r16.1]. Unchanged: dense
per-session `seq`, `turn`, `late`, "`turn.ended` is the last non-late event
of its turn", and the envelope's `events {first_seq, last_seq, count}` and
`event_seq` citations, now over durable events. A fake turn has about six.

### 2.2 What a Route decodes (R2)

Route decodes each vendor message into a typed struct holding only what
R1–R6 need: type and correlation IDs (vendor turn, thread, session, tool
call), tool names, usage numbers, the acceptance, identity and terminal
fields the envelope carries (`stop_reason`, `vendor_code`, usage, the
vendor's step count, structured output), completed final text (§2.3), and
the payloads of §2.1's adapter events. Everything else is skipped with
`serde::de::IgnoredAny`, so tool inputs and outputs are never copied. Rules:

1. **Short fields.** An ID, tool name, type tag, `stop_reason` or
   `vendor_code` is at most 1 KiB (`SHORT_FIELD_MAX`); longer is `protocol`
   (C2 §1 rule 6; Q-R5-9).
2. **Payloads at most 256 KiB** (C2 A1); longer is `protocol`. Final text is
   split into pieces (§2.3).
3. **Structure limits first.** `json_limits::scan` (§10.2) enforces depth 64
   and 65,536 nodes before the typed decode.
4. **No peer `Value`** (§10.2), and no `#[serde(flatten)]`, `untagged` or
   internally or adjacently tagged enum on a peer-fed type: they buffer
   input into serde's private tree, outside §5.1's sizes. [V] Today these
   appear only on `Serialize`-only types in `crates/via-core/src/api.rs`, the
   fake agent's script (`crates/via-fake-agent/src/main.rs:32`) and the anchor
   protocol (`crates/via-host/src/protocol.rs:76`). A grep of `Deserialize`
   types in via-routes, via-adapters, via-core and via-cli is an acceptance
   check.

Every decode failure (rules 1–4, invalid UTF-8, a malformed known message)
saves the message before Route fails `protocol` (§7.3) [t4r16.1]. [V] Today
the fake route decodes `text` with its text and copies up to 16 KiB of an
unknown message (`crates/via-routes/src/lib.rs:340-344`, `:518-537`); after
R2 it keeps `vendor_turn_id`, `tool_id`, `name` and an unknown type tag (at
most 256 B, `:418`).

### 2.3 The observation channel (C2 A1 after R2)

The drive creates the channel (today `mpsc::channel(64)`,
`crates/via-core/src/engine/drive.rs:1280` [V]). It carries:

| Observation | Core action |
|---|---|
| `turn.accepted`, `session.vendor_identity_confirmed` (gains `transcript?`, §7.4), `resume.mismatch`, `session.vendor_closed`, `tool.quiescent`, `turn.vendor_terminal` | as C2 §4 today |
| `action.denied`, `vendor.request_declined`, `steer.delivered`, `warning` | commit the event (`commit_event`, `drive.rs:1461` [V]) |
| **`progress { at, model, tools_started, tools_ended, usage }`** | fold into the step tracker (§2.4); a Store write only at a step boundary (§3) |
| **`final_text { text }`** [t4r16.5.8] | append to the turn's final text: inline up to 256 KiB, else the turn's `final_text.txt` (§6.4) [t4r16.7.8]. The Adapter sends completed text only, all of it before `turn.vendor_terminal`, which carries none, cut at the last character whose escaped encoding keeps the whole observation within 256 KiB (a counting writer); a longer text is several pieces in order |

Completed text only, because a piece is then never revised: there are no
keys and nothing to overwrite. Cost: a Codex turn that fails mid-message has
no partial final text in its envelope; the agent's rollout has it.

A vendor message yields at most one `progress` item, only when it carries a
mark (model output, a tool start or end, a usage sample), plus its other
observations. An unknown or unattributable message produces no observation
and moves the activity clock only when attributed. An item for a turn
already terminal is late and dropped.

**Bounds** (C2 A1): `mpsc::channel(1024)` plus a per-drive 4 MiB
`Semaphore`; an item counts `512 + Σ(64 + len(s))` over its strings,
acquired by the Adapter before it builds the item and held until Core has
handled it. The Route → Adapter hop shrinks from `mpsc(64)` to `mpsc(1)`
(`crates/via-adapters/src/runtime.rs:132` [V]). Route blocked on the hop
stops calling `next_message`; the Wire queue then fills and fails
`overflow` (runtime §8 "Route message staging").

**Stall** (C2 A1) [t4r5.8]. The Adapter's pending delivery owns one absolute
deadline, `first_block + 10 s`, restarted only when that item is accepted.
On expiry it drops the hop receiver; Route selects on `hop.closed()` in
every wait (§9). A private route (the fake, Claude) fails
`RouteError::Overflow` and force-closes the connection; Core disposes it
`failed(overflow)` as today (`crates/via-core/src/engine/terminal.rs:97`
[V]). A shared route quarantines that thread generation as C2 §4 does for an
ingress overflow (the Codex task). [V] Today the Adapter drops the receiver
on a failed delivery and Route reports the closed hop as `Overflow`
(`crates/via-adapters/src/runtime.rs:146-153`,
`crates/via-routes/src/runtime.rs:755-756`); the 10 s bound and `closed()`
arm are new.

**No envelope overrun** [t4r16.7.8]. The envelope cannot exceed 1 MiB
(§6.4), so accumulation never fails a turn: a long final text goes to a
file and the denied and declined lists keep their first 1,000 entries. The
stall above is the only observation `overflow`.

### 2.4 Progress snapshot (R3)

**Owners.** The **step tracker** is per-turn state in the drive's
`TurnRecord`, the only place the step rule runs. The **published progress**
is a copy in the `Slot`'s `Running` entry, which exists exactly while the
turn runs; after each `progress` item the drive calls
`Slot::publish_progress(turn, &ProgressDelta)` under the slot state mutex.
The **activity clock** (`TurnActivity`, `Arc<AtomicU64>`, milliseconds since
the drive's base instant) has a clone in `Running` and one in the Adapter,
which stores the arrival of each message it attributes to the turn, unknown
types included. **Reading:** `status` calls `Engine::slot(session)`
(`crates/via-core/src/engine.rs:334` [V]) and copies the `Progress` (at most
70 KiB) only if its turn is the turn `status` selected (§4.2); no Store
round trip, no wait on the drive.

**`Progress`** holds A26's `progress` members: `turn`, `current_step`,
`phase` (`tools` while the open set is non-empty or `tools_overflow` is set,
else `model`), `running_tools`, `tools_overflow`, `last_activity_at` and
`tokens` (`{total, scope}` of completed steps).

**The step rule** (one reducer; the Adapter only classifies messages into
marks):

1. At `turn.accepted`: `current_step = 1`, step start = now,
   `results_since_output = false`.
2. `model` (text, reasoning or a tool request): if `results_since_output`,
   this is a **step boundary**: the step ends now, `current_step += 1`, the
   next starts, and the flag, the open set and `tools_overflow` clear. A
   message's `model` mark applies before its tool starts.
3. `tools_started (id, name)`: added to the open set if it has fewer than 64
   entries and lacks the id; a new id beyond 64 sets `tools_overflow`.
4. `tools_ended` ids: every end, tracked or not, sets
   `results_since_output`; a tracked id is removed.
5. At the terminal the current step (if `current_step ≥ 1`) ends.

So the count rises exactly when the model produces output after tool results
(R3), and an open-set error lasts at most one step. A step's end is the next
step's start; its row is written then (§3).

**Per-vendor claims** [t4r16.5.1]. The rule is proven for the fake only.
Whether a vendor's marks count its model calls (a call that only requests
tools, parallel tools, tools run in sequence from one call) is settled by
that vendor's probe against captured fixtures (§2.5), as for tokens. Until
then `current_step` is labelled VIA's count and claims nothing more, and
`tokens` may be `null` (R3).

**Tokens** (R3). A `usage` mark `(key?, total)` is an interval sample
(`total` is the vendor's total, else input plus output). Within a step,
samples with the same key (the vendor's message ID, if any) supersede each
other and different keys add; `tokens.total` sums completed steps;
`tokens.scope` is the route's declared `capabilities.usage.tokens`.

**Envelope `steps`** [t4r16.4]: the vendor's own count (Claude `num_turns`,
`docs/specs/vendors/claude-code.md:114` [S]), or `null` when the vendor
reports none (the fake, Codex and OpenCode as specified). VIA's count is
only in `progress`, so no caller sees two unlabelled numbers under one name
(N1).

**Bounds**: at most 64 open entries of at most 1 KiB per field; at most 16
usage keys per step, a 17th adding to a keyless sum; the copy at most
70 KiB.

### 2.5 Vendor mappings of the marks

[S] from the cited spec unless marked. Each vendor's probe runs before its
step counts and `tokens` are claimed, and always includes a tool-only step
(a call whose only output is a tool request) and a parallel-tool step
[t4r16.5.1].

| Vendor | `model` | `tools_started` | `tools_ended` | `usage` (key) | Envelope `steps` | Probe adds |
|---|---|---|---|---|---|---|
| Fake (runtime §3.1, A33) | `text` | `tool_started {tool_id, name}` | `tool_ended {tool_id}` | new `usage {total_tokens}`, no key | `null` | none: exact by construction |
| Claude ([S §5]) | `assistant` with text or `tool_use` | each `tool_use` `(id, name)` | each `tool_result` in a `user` message | `assistant.message.usage`, key message ID **[U]** (the spec probes only `result.usage`; the transcript has per-message `usage` **[V, probe]**) | `num_turns` | usage sum against `result.usage`; VIA's count against `num_turns`; the largest tool-result and image messages against the 1 MiB cap (§5.1) |
| Codex ([S §5, §7]) | `item/started` or `item/agentMessage/delta` for `agentMessage` or `reasoning` | `item/started` for a tool item (`commandExecution`, `fileChange`; others **[U]**), name = item type | `item/completed` | `tokenUsage.last`, no key; scope `vendor_interval` | `null` | Σ`last` against the change in `total` |
| OpenCode ([S §4, §5, §7]) | a text or reasoning part of the turn's assistant message | a tool part entering `running` | a tool part entering `completed` or `error` | the assistant message's tokens, key message ID | `null` | the ledger against the session total; event and part names **[U]** |

Any other message (Claude `system/init`, Codex `thread/status/changed`) sends
no observation and, if attributed, moves only the activity clock. Codex's
P7 tool tracking (`tool.quiescent`) is separate and exact [S §6].

### 2.6 Idle deadline

Idle resets on normalized meaningful progress (runtime §8). [V] Today
`progress()` counts acceptance, `assistant.text`, `tool.started` and
`tool.ended` (`crates/via-core/src/engine/drive.rs:1775-1786`); after R2 it
counts acceptance and `progress` items with `model` or a tool start or end.
Usage-only items, unknown messages and stderr never reset it.

## 3. Step rows (R4)

### 3.1 Table

Schema v6 adds, clustered by key so a session's rows are contiguous:

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
  (§9) before the next observation. No `Head` is taken (a row has no `seq`).
- **The last step** is written **in the terminal transaction**: every
  terminal built from the `TurnRecord` (`finish_with`, and `finish` for a
  forced turn, `drive.rs:1035`, `:1000` [V]) carries the open step's row. A
  forced turn's record reaches final shutdown in its `ForcedTurn`
  (`drive.rs:756` [V]), whose best-effort terminal (`forced_terminal`,
  `crates/via-core/src/engine/stop.rs:360` [V]) carries it [t4r6.8].
- **No row cap** [t4r5.1]: every completed step gets a row (under 128 B).
- **Refused rows ride in the terminal** [t4r5.2] (Q-R5-4). A known
  `NotCommitted` records
  `first_failure` and upgrades the stop order to cause `store`; the turn's
  `failed(store)` terminal inserts the refused row, every later one
  (`TurnRecord.carried_rows`) and the open step's row. An uncertain outcome
  latches (T3 §7.4). Only items in flight (at most 1024, and three more)
  can end further steps: about 1,030 rows.
- **Guarantee** [t4r6.8]. The terminal is built after the channel drains, so
  every row precedes or shares the transaction of `turn.ended`; a
  `turn.ended` built from the `TurnRecord` implies every row is durable.
  Only the Latch batch after an uncertain outcome and recovery's terminal
  make no such claim.

### 3.3 What survives a crash

Rows up to the last committed step (R4). A step whose row commit was in
flight, and the step in progress, are missing; the agent's transcript has
them. Recovery synthesizes the `unknown` terminal (T3) and adds no row.

### 3.4 Reading and retiring

`status` reads `SELECT step, started_ms, ended_ms, tokens FROM steps WHERE
session_id=?1 AND turn=?2 AND step>?3 ORDER BY step LIMIT ?4`, a
primary-key range scan. Retiring (`via-jm4.18`): `DELETE FROM steps WHERE
session_id = ?1`, one keyed range delete, with the session's events,
envelopes and evidence folders (§7.5).

## 4. Caller interface (R5)

With request `id` capped at 256 B (A31), every reply's wrapper is under
512 B, so each bound applies to the `result` object. A reply not written
within `REPLY_WRITE` = 10 s of being ready to write closes the connection
(A32) [t4r16.5.5].

**Connections** [t4r16.5.4]. A socket carries one request at a time: its
task reads one line, handles it and writes the reply. There are at most 32
sockets; the 33rd is closed at once without bytes (§10.1). A caller blocked
on `wait` polls `status` or sends `cancel` on another socket.

### 4.1 `wait` and `result`

`wait` polls the Store with `terminal_facts` (§6.7) until the turn is
terminal, `timeout_ms` or final shutdown; only then does it read the
envelope with `result_text`. It reads at once, then once per second
[t4r16.7.7]: 32 waiters make 32 reads per second, where today's fixed 20 ms
(`crates/via-core/src/engine/read.rs:100` [V]) makes 1,600. A turn's end
is seen at most 1 s late. The Public lane cannot refuse a `wait`: one request per
socket and 32 sockets give at most 32 Public reads, its slot count (§6.1).
`result` reads `result_text` once. The envelope (at most 1 MiB) is written
as stored; no `Value` is built.

### 4.2 `status` describes one moment [t4r16.5.2]

C1 §3.7 as A26 amends it:
1. One Public Store read (`session_status`, §6.7) selects the turn (the
   `turn` param, else the running turn, else the latest) and returns the
   durable members (§11.3) and a page of that turn's committed step rows.
2. Then, from memory: `progress` is the published `Progress` only if the
   `Running` entry's turn is the selected turn, else `null`; and
   `process.alive`.

So `progress` never describes another turn, and it is at least as new as the
rows: the open step (`current_step`) never has a row, and a step that ended
after the Store read appears on a later call. If the turn ended between the
two reads, `active_turn` may say `running` with `progress: null`. The reply
is at most `STATUS_MAX` = 1 MiB, else `admission_refused`; the fake's is far
below (64 turn summaries, 8 queued `effective` values, `progress` at most
70 KiB, 1000 rows of 128 B).

### 4.3 `events`

Params: exactly one of `session`, `turn`; `after?` (0), `limit?` (200, max
1000), `types?`; `follow` is an unknown field (A25). One read transaction
over `after < seq ≤ after + 1000`, with `types` and `turn` as SQL predicates
on the v6 columns, stopping at `limit` or `PAGE_MAX` = 1 MiB (a row's
borrowed length is checked before it is copied); the SQLite thread writes
the `events` array as one JSON text. `next_after` is the last scanned seq;
`more = next_after < head` (same transaction); `earliest_seq` is 1, since
nothing is pruned: `history_pruned` is the retention task's [t4r16.5.9]. A
first matching event larger than the page is `admission_refused`.

### 4.4 `logs`: where the evidence is [t4r16.1]

Params: exactly one of `session`, `turn`. A turn address selects that turn;
a session address, the running turn, else the latest submitted one. Result:

```json
{"session_id":"s_7f3k9q2mzr4c","turn":2,"vendor_session_id":"…",
 "transcript":"…/.claude/projects/-work-repo/….jsonl",
 "folder":"…/state/evidence/s_7f3k9q2mzr4c/2",
 "files":[{"name":"stderr.log","bytes":2048},{"name":"final_text.txt","bytes":1835008}]}
```

`vendor_session_id` and `transcript` come from the session row (§7.4), both
nullable; `transcript` is a hint VIA never opens. `folder` is the turn's
`evidence_dir` made absolute, or `null` for a turn never submitted. `files`
lists each fixed name of §7.1 that exists, with its size from one `stat`
each on the blocking pool. VIA reads no file contents and needs no paging:
the reply is a few KiB.

### 4.5 `list` [t4r16.4]

`list_page(state?, harness?, label?, since?, limit?, cursor?)` (§6.8).
Sessions come in creation order, newest first. A summary is `{session_id,
state, admission, harness, model, label, created_at, last_active_at}`;
`last_active_at` is the time of the session's latest durable event, and
`since` matches `last_active_at ≥ since`. Limit 50 by default, 200 at most,
and `PAGE_MAX`.

### 4.6 Other methods

| Method | DTO (`deny_unknown_fields`) | Answer |
|---|---|---|
| `describe` | `harness?, model?, bound?, require?, vendor?, cwd?, allow_untested?` | Core, from `Capabilities::fake()` (`api.rs:932` [V]); no process, no write |
| `models` | `harness?` | Core: the fake's one model |
| `daemon/status` | none | memory: `started_at`, counts (§11.2), `limits` (§5.5), `storage` (§5.3) |

- New `ApiError` constants: `UNKNOWN_MODEL` (-32010, for the
  `invalid_params` at `receipt.rs:100` [V]), `REQUEST_TOO_LARGE` (-32020,
  §10.1), and `admission_refused` kinds `STORE_QUEUE_FULL` (§6.1) and
  `DISK_FREE_FLOOR` (§5.3). `unsubscribe` is `method_not_found`.
- CLI (`crates/via-cli/src/main.rs:20-34` [V]): verbs `describe`, `status`,
  `list`, `models`; `status --turn N --after-step N --limit N`; spawn and
  resume flags `--prompt-file F|-`, `--instructions`, `--cwd`, `--require`,
  `--allow-untested`, `--label`. `--prompt-file F` sends `prompt_file` with
  `F` made absolute; `-` reads stdin into `prompt`. `via events --follow` is
  gone.
- `serve --stdio`: a byte proxy between stdio and one daemon socket,
  auto-starting the daemon; two copy loops under one `JoinSet`; stdin EOF
  shuts the socket's write side. Parity test: one scripted sequence over the
  socket and over the proxy gives the same replies, oversize included.

## 5. Bounds (R7) [t4r16.2, t4r16.3]

### 5.1 Memory by construction

There is no memory pool, counter or memory setting. Every buffer has a fixed
maximum and every kind of holder a fixed count; this table is the whole
account. Sizes are estimates **[I]**; the F24 RSS gate (§13.2) measures the
sum and allows 1.25 × it (Q-R16-1, accepted [t4r16.7.4]).

| Holder | Count, fixed by | Largest buffers each | Each | Total |
|---|---|---|---|---|
| C1 socket | 32: the accept loop's `Semaphore(32)` (§10.1), one request at a time | the line (1 MiB); its decode: serde's scratch (≤ the longest string), decoded strings and `Box<RawValue>` copies (≤ the line), list headers (65,536 nodes × 24 B, doubled for `Vec` growth); the reply, built after those are dropped (≤ 1 MiB + 512 B); one 64 KiB blob chunk while copying a prompt file or comparing a replay | 6 MiB | 192 MiB |
| Wire connection | 4: `CONNECTION_SLOTS` (`crates/via-core/src/engine/queue.rs:33` [V]) | read buffer 64 KiB; assembly buffer, one message (1 MiB); queue, 64 messages and 4 MiB; the message Route holds (1 MiB); stdin piece buffer (96 KiB) and control queue (64 KiB); undecoded-message write (64 KiB) | 6.3 MiB | 25 MiB |
| Running turn (Route, Adapter, Core drive) | 4: one per connection slot | hop (1 MiB) and decoded struct (≤ 1 MiB); observations 4 MiB (C2 A1); envelope accumulation (inline final text 256 KiB, lists 500 KiB) and its terminal encoding (1 MiB); step tracker, copy and carried rows (270 KiB); the dispatched prompt (≤ 16 MiB, §10.4) until its start is written | 24.1 MiB | 96.3 MiB |
| Store | 1 | lanes 8 MiB (§6.1); the command or page in hand (≤ 2.3 MiB, §6.4); page cache 8 MiB (`cache_size=-8192`, `crates/via-store/src/runtime/sql.rs:140` [V]) | 18.3 MiB | 18.3 MiB |
| **Sum** | | | | **≈ 332 MiB [I]** |

The sum has every holder at its maximum at once: 32 hostile maximal
requests, four 16 MiB prompts, four flooding turns, full lanes. The gate's
baseline covers what the table omits: the binary, tokio's and SQLite's other
allocations, task stacks, fixed-size structs.

**The per-message cap** [t4r16.1]. A vendor stdout message is at most
`MAX_STDOUT_MESSAGE_BYTES` = 1 MiB (`crates/via-wire/src/lib.rs:11` [V];
runtime §8), and it stays 1 MiB: every Wire bound above is sized from it. A
Claude `user` message with a large tool result or an image may exceed it
**[U]**; the Claude probe (§2.5) measures the largest. Raising the cap to
N MiB is one constant plus a queue residency of at least N MiB; each extra
MiB adds about 16 MiB to the sum (3 MiB per connection, 1 MiB per running
turn), so 8 MiB would add about 112 MiB. A message over the cap fails its
turn with its first 64 KiB saved (§7.3), never a silent cut.

**Q-R5-8 is moot**: a request is at most 1 MiB, and no charge exists.

### 5.2 C1 requests [t4r16.2]

- **Line cap.** A request line is at most 1 MiB, LF included. A longer line
  gets `request_too_large` and the connection closes (§10.1). 32 sockets ×
  1 MiB is runtime §8's 32 MiB of C1 input, so no input counter remains.
- **Prompt file.** `spawn` and `resume` take exactly one of `prompt` and
  `prompt_file`, an absolute path to a regular UTF-8 file the daemon's user
  can read, at most `PROMPT_MAX` = 16 MiB (today's largest prompt, the
  16 MiB line). VIA copies it into a blob (§10.4).
- **Retry identity.** For a prompt file the identity carries the copy's
  `"sha256:<64 hex>:<len>"` in place of the path (§10.3): the same content
  matches, changed content is `idempotency_conflict`.

### 5.3 Disk: a free-space floor and a size warning [t4r16.3]

There are no size budgets, headrooms, page ceilings or byte counters.

- **Admission.** At a `spawn` or `resume` receipt (before a prompt-file
  copy) and at a queued turn's dispatch, Core reads the free space of the
  state directory's filesystem, `f_bavail × f_frsize` from
  `rustix::fs::statvfs` (`rustix-1.1.5/src/fs/abs.rs:288` [V]; via-store
  enables `rustix`'s `fs` feature, `crates/via-store/Cargo.toml:16` [V]), on
  the blocking pool. Below `disk.free_floor` (5 GiB):
  - a receipt is `admission_refused`, kind `disk_free_floor`, with
    `data.free_bytes` and `data.floor_bytes`, before any write;
  - a queued turn fails `store` at dispatch before submission, its
    `failure.message` naming the floor. It fails rather than waits because
    nothing would wake a waiting dispatcher when space returns.
- **Everything else proceeds.** Only admission checks the floor. Running
  turns keep writing their rows and events, and **lifecycle and terminal
  writes** proceed: every terminal (carried rows included), cancel events,
  session close, queued-turn cancellation, recovery's writes, the Latch
  batch, Host's absence records and a running turn's step rows
  [t4r16.7.6].
- **Full disk.** An actual `SQLITE_FULL` or `ENOSPC` rolls back and is a
  known `NotCommitted`; only a failed rollback is uncertain and latches
  (runtime §7). A failed blob or evidence write is scoped to its request or
  turn (§6.5, §7.5).
- **Warning.** `daemon/status` gains `storage {free_bytes, data_bytes,
  data_measured_at, below_free_floor, over_warn_size}`. `data_bytes` sums
  the apparent lengths (`metadata().len()`) of `store.sqlite3`, its `-wal`
  and `-shm` files and every file under `blobs/` and `evidence/`. Rule: a
  `daemon/status` that finds the cached value absent or older than 60 s
  recomputes it with one directory walk on the blocking pool, shared by
  concurrent calls. `over_warn_size` is `data_bytes > disk.warn_size`
  (2 GiB). Apparent length needs no filesystem-specific block accounting
  and matches what retention deletes. Cleanup is the retention task's.
- **Q-R8-3 is moot**: a store already below the floor starts, serves reads
  and refuses new work.

### 5.4 WAL [t4r16.3]

The checkpoint policy is runtime §6's, as configured: a passive checkpoint
after `wal.checkpoint_bytes` of growth (`wal_autocheckpoint`, in 4 KiB
pages) or `wal.checkpoint_commits` commits (a counter on the SQLite thread).
`journal_size_limit` = `wal.checkpoint_bytes`, so a WAL that resets is cut
below `wal.max` and its file length measures growth since the reset **[I]**
(SQLite's documented `journal_size_limit`; the WAL test checks it).

**At the limit** (critical-review change 5, N5): after each commit the
SQLite thread reads the WAL file's length; at or above `wal.max` it sets
`wal_full` and runs `wal_checkpoint(TRUNCATE)`. While `wal_full` is set:
- an ordinary write is refused before `BEGIN` as a known `NotCommitted`
  (`StoreFailureKind::Quota`, kind `wal_full`): a receipt gets `store_error`
  `not_committed`; a running turn's other durable event fails the turn
  `store` (§3.2), whose terminal commits;
- lifecycle and terminal writes (§5.3), a running turn's step rows included
  [t4r16.7.6], still commit;
- a write arriving at least 1 s after the last attempt first retries
  `TRUNCATE`; below `wal.max`, `wal_full` clears.

There is no Store health latch: an oversized WAL is a known, recoverable
state, so an external `sqlite3` reader holding a snapshot delays ordinary
writes and nothing else. The commit that crosses `wal.max` completes, so
the WAL may exceed it by one transaction. SQLite writes each page a
transaction dirties once, 4 KiB plus 24 B (`sqlite3.c:71575-71599` in
`libsqlite3-sys` 0.38.2 [V]); the pages a maximal transaction dirties are
not proved, and `via-d9o.2.3` measures them (Q-R9-1, a gate).

### 5.5 Daemon config [t4r8.1, t4r16.3]

- **File.** `daemon.json` in the state directory (`VIA_STATE_DIR`, default
  `~/.via/state`, `crates/via-cli/src/client.rs:30-37` [V]), parsed by
  serde_json; absent means every default. Validated like the Store
  directory (regular file, the daemon's uid, not a symlink, not group- or
  world-writable), at most 64 KiB. Read once, at start, before `Store::open`
  and the socket; a change takes effect at the next start.
- **Shape.** `{disk: {free_floor, warn_size}, wal: {max, checkpoint_bytes,
  checkpoint_commits}}`, strict DTOs (`deny_unknown_fields`; serde refuses a
  duplicate key). Every key is optional; values are non-negative integers
  in bytes, except the commit count. Defaults: 5 GiB, 2 GiB, 32 MiB, 8 MiB,
  1000.
- **Validation**, each failure naming its key and rule: every value at most
  2^62; `wal.max` at least 4 MiB and above `checkpoint_bytes`;
  `checkpoint_bytes` at least one 4 KiB page, applied as whole pages;
  `checkpoint_commits` from 1 to 2^32 − 1; `disk.free_floor` 0 turns the
  floor off.
- **Invalid** (unreadable, not JSON, unknown or duplicate key, failed rule):
  the daemon writes `via: daemon config invalid: <key>: <rule>` to stderr
  and exits 78 (Q-R8-2) before any Store or socket change; the
  auto-starting CLI reports it.
- **Reported**: `daemon/status` `limits` holds the five effective values
  (A37, Q-R8-4).
- **Fixed, not config:** every C1 limit, C2 A1, runtime §8's queues, the
  per-message cap, the undecoded-message prefix, the envelope bounds and
  final-text file cap (§6.4), the `via.log` rotation size, the Store lanes,
  the page size.

## 6. Store

### 6.1 Request lanes [A2]

[V] Today one `sync_channel(128)` carries every request, FIFO
(`crates/via-store/src/runtime.rs:1021`); Host's `ProcessJournal` shares the
sender; `Store::drop` sends `Shutdown` with a blocking `send`. `Store::open`
swaps it for one `Lanes` value (`Mutex<State>` + `Condvar`) of four FIFO
lanes, each with its own slots and bytes:

| Lane | Members | Slots | Bytes |
|---|---|---|---|
| Latch | the failure-resolution unit (`crates/via-core/src/engine/batch.rs:85`, `:91`, `:165` [V]) | 1 | 2 MiB |
| Lifecycle | every other call of the shutdown pipeline | 7 | 2 MiB |
| Internal | every other commit and read by Core, Route, Host or recovery | 64 less Public's | 4 MiB less Public's |
| Public | reads issued by C1 handlers | at most 32 of the 64 | 4 KiB each |

The allowances are fixed (runtime §8's 8 MiB, split by A2); Latch and
Lifecycle each hold one terminal (§6.4). Membership is by handle
(`StoreClient::public()`, `lifecycle()`, `latch()`; default Internal).
Service order: Latch, Lifecycle, then Internal and Public round-robin. A
full lane returns `StoreError::NotEnqueued` (known, T3 §7.1): a mutation
keeps T3's `store_error` `not_committed`; a Public read is
`admission_refused` (`STORE_QUEUE_FULL`) and never latches. `Lanes::push`
never blocks; Host's journal keeps its `try_send` semantics.

### 6.2 Lifecycle and Latch capacity

[V] `Engine::shutdown` (`crates/via-core/src/engine/stop.rs:444`) issues its
Store requests one at a time, each under `min(FINALIZE_WRITE, remaining)` or
`BATCH_READ` (2 s each, `latch.rs:46`, `batch.rs:24`), inside the 10 s
`FINAL_SHUTDOWN` (`crates/via-cli/src/server/shutdown.rs:23`). The
failure-resolution unit awaits each request, so one Latch slot suffices; an
abandoned Latch request makes the next push `NotEnqueued` (runtime §7's
"skipped batch"). At most five Lifecycle requests are abandoned at a full
2 s and one at the remainder, so 7 slots cover them plus the one being
issued [I]. When abandoned requests hold the bytes, a push is `NotEnqueued`,
reported `not_committed`, and recovery resolves the turn at restart.
`finish` (`drive.rs:1000`) uses the Lifecycle handle; `finish_with`
(`:1035`) keeps Internal.

### 6.3 Lock discipline, fence and writer death

Only queue and counter updates run under the `Lanes` mutex; replies are
dropped after release. `Shutdown` is an admission fence: `Store::drop` sets
`fence`, notifies and joins; later pushes are `NotEnqueued`; everything
accepted before is served in lane order. The thread body runs under a
`DeadGuard` whose `Drop` (also on unwind) sets `dead`, takes all lanes and
the in-flight item and fails them `WriterLost`, which latches.

### 6.4 Transaction cap, envelope bound and blobs [A30]

- **`Command::bytes()`** is the encoded length of the variable payload (a
  counting writer over what the SQLite thread binds) plus 512 B, over an
  exhaustive match of `Command` (`crates/via-store/src/runtime.rs:710` [V]).
- **Cap** (runtime §8 as A30 amends it): at most 128 events and 1 MiB of
  payload, **excluding one terminal envelope** (at most `ENVELOPE_MAX` =
  1 MiB, C1 §5) **and the turn's carried step rows**; over it is
  `NotEnqueued` before queueing, and a lifecycle batch is never split. A
  terminal with its records and rows is about 1.2 MiB, a failure batch (one
  terminal and at most 8 cancellations, `runtime.rs:390` [V]) about 1.3 MiB,
  both inside their lanes' 2 MiB (Q-R5-1).
- **Envelope at most 1 MiB by construction** [t4r16.7.8]. Every member has
  a fixed maximum, so no turn fails for a large result and there is no
  overrun path or failure summary. The encoded maxima [I]:

  | Member | Maximum | How |
  |---|---|---|
  | `final_text` | 256 KiB (`FINAL_TEXT_INLINE`) | a longer text goes to the file below; `final_text` is then `null` |
  | `denied_actions`, `auto_declined_requests` | 1,000 entries of at most 256 B each: 500 KiB together | the first 1,000 are kept and `denied_actions_total`, `auto_declined_requests_total` count all; an entry's strings are cut at a character boundary to fit, and the event it cites by `event_seq` keeps the full payload |
  | `bound.requested`, `bound.effective` | 32 KiB each | the request's `bound` is refused over 32 KiB at receipt (`invalid_params`, naming it); `effective` is derived from it [I] |
  | `vendor_options` | 16 KiB | the request's `vendor` is refused over 16 KiB at receipt |
  | `model`, `effort` (requested, resolved) | 1 KiB each | refused over 1 KiB at receipt |
  | `warnings` | one entry per code, message at most 1 KiB: 9 KiB | a repeated code keeps its first message |
  | `failure`, `vendor`, `cwd`, `evidence`, `final_text_file` | 2 KiB message; 1 KiB per short field (C2 §1 rule 6); paths 4 KiB each | as today |
  | `structured_output` | `null` | the fake refuses `output_schema` (§0); the task that adds it bounds it the same way |
  | every other member (IDs, states, usage, cost, times, events) | 8 KiB | fixed shapes |

  The sum is about 884 KiB, under `ENVELOPE_MAX` = 1 MiB. The threshold
  256 KiB is what the other members leave with a margin; it also equals one
  C2 `final_text` piece, so a one-piece text is always inline. A test builds
  an envelope with every member at its maximum (§13.2).
- **Final text file** [t4r16.7.8]. Core keeps the text inline while its
  escaped encoding is at most 256 KiB. The piece that would pass it makes
  Core create `final_text.txt` in the turn's evidence folder through
  `StoreClient::final_text_file(session, turn) -> FinalTextFile` (Store owns
  the evidence root, §7.2; `create_new`, 0600, `NOFOLLOW`), write the held
  text and the piece, and drop the buffer; later pieces are appended. Each
  append runs on the blocking pool under 2 s, awaited under `while_polling`
  (§9). The file holds plain UTF-8 text. Past `FINAL_TEXT_FILE_MAX` = 64 MiB
  the piece is cut at a character boundary and later pieces are dropped; a
  write error likewise stops appending. Either sets `truncated`, and the
  turn does not fail. `finish()` syncs the file before the terminal is
  built. The envelope then carries `final_text: null` and `final_text_file
  {path, bytes, truncated}`; otherwise `final_text_file` is `null`.
- **Blobs** (runtime §8): prompts and identities over `INLINE_MAX` = 256 KiB
  use the blob path, because the retry identity contains the prompt
  (`crates/via-core/src/api.rs:804-858` [V]) and a spawn carries both.

### 6.5 Blob path [owner changed]

With no raw worker [t4r16.1], each blob file's handle is its only owner and
does its own I/O on the blocking pool (`spawn_blocking`, coding-style §5).
- **Files.** `blobs/b_<32 hex>.blob`, `create_new`, 0600, `NOFOLLOW`;
  `blobs/` is validated at `Store::open`; rows store the id.
- **Handles.** `BlobWriter::write(chunk ≤ 64 KiB)` under a 2 s bound; a
  failure is `not_committed` for the request (nothing references the file
  yet). `finish() -> BlobRef{id, len, sha256}` syncs the file and `blobs/`.
  `discard`/`Drop` unlink an unfinished file; `StoreClient::discard_blob`
  unlinks a finished one after a commit known not to have happened (a lost
  discard is swept). `BlobReader::next_chunk()` returns at most 64 KiB. A
  row references a blob only after `finish`, in the same transaction.
- **Exact replay comparison** (C1 byte-identical rule). An inline identity
  is compared byte for byte; a blob identity by length and SHA-256, then by
  streaming the blob against the incoming pieces. The key lookup runs under
  `admission` (`receipt.rs:76`, `:202` [V]); a found row is immutable, so
  Core releases `admission` before streaming (F5), under `REPLAY_COMPARE` =
  10 s (expiry is `store_error` for that request).
- **Dispatch load.** The dispatcher loads the prompt blob into an exact
  `String` with a running SHA-256 and UTF-8 check (a mismatch fails the turn
  as corrupt evidence) and moves it into the streamed start (§8.3), which
  drops it once written. Four slots bound this to four prompts (§5.1).
- **Recovery.** `verify_blobs()` checks every referenced blob (regular file,
  length, SHA-256; else `Corrupt("blob")`); `sweep_blobs()` unlinks
  unreferenced files; both run on the SQLite thread before admission.

### 6.6 Schema v6

`SCHEMA_VERSION` becomes 6 (`crates/via-store/src/runtime.rs:24` [V]); older
development Stores are refused untouched (`:34-37` [V]); a golden DDL test
(`s1_store_v6_schema_is_frozen`) freezes v6. Changes from v5
(`crates/via-store/src/runtime/sql.rs:146-184` [V]):

- **`steps`** (new): §3.1.
- **`sessions`** gains `created_ms`, `updated_ms` (the `at` of the
  transaction's highest-`seq` event: `last_active_at`), `harness`, `label`,
  `ord INTEGER NOT NULL UNIQUE` (`MAX(ord)+1` once at spawn; its index serves
  `list`), and nullable `vendor_session_id` and `transcript_hint` (§7.4).
- **Frozen `params`** gains `cwd` and `allow_untested` (A14; today
  `receipt.rs:140` freezes only harness and model [V]).
- **`events`** gains `turn INTEGER` (deferred composite FK to `turns`),
  `type TEXT NOT NULL` (from the event JSON in `insert_event`, `sql.rs:966`
  [V]) and the index `(session_id, turn, seq)`; it loses `connection_id`,
  `raw_offset` and `raw_len` [t4r16.1].
- **`turns`** gains `ended_seq` with `CHECK((state IN
  ('completed','failed','cancelled','unknown')) = (ended_seq IS NOT NULL))`
  (A15); `prompt_blob` with `CHECK((prompt IS NULL) <> (prompt_blob IS
  NULL))`; and `evidence_dir TEXT`, relative to the state directory, written
  with `turn.submitted` [t4r16.1].
- **`spawn_keys.identity`, `operations.identity`** become nullable with
  `identity_blob` and the same CHECK.
- **No `connections` table** [t4r16.1]: its columns served only the raw log.
  A per-turn connection's ID derives from session and turn
  (`crates/via-core/src/engine/drive.rs:48-58` [V]), identity confirmation's
  generation check is in memory, and evidence is per turn. The OpenCode and
  Codex tasks add what reusable connections need.
- **Time.** `at` strings parse strictly to Unix ms (else `Constraint`).

### 6.7 Store reads

| Read | Lane | Returns | Bound |
|---|---|---|---|
| `terminal_facts(session, turn)` | caller's | `Option<{state, cancel}>` via `json_extract` | 4 KiB |
| `result_text(session, turn)` | Public | the stored envelope text | `ENVELOPE_MAX` |
| `events_page` | Public | §4.3 | `PAGE_MAX` |
| `evidence_refs(session, turn?)` | Public | the selected turn, its `evidence_dir`, `vendor_session_id`, `transcript_hint` | 12 KiB |
| `list_page` | Public | §6.8 | `PAGE_MAX` |
| `session_status(session, turn?, after_step, limit)` | Public | §11.3 members and a step page | `STATUS_MAX` |

`terminal_facts` serves every internal use of the envelope `Value` read,
all of which need only existence, `state` or `cancel` [V]:
`crates/via-core/src/engine/control.rs:48` (`:210-216`), `journal.rs:550`,
`stop.rs:632`, `batch.rs:85`. C1 `result`, `wait` and `await_terminal` use
`result_text`, so no daemon path parses a stored envelope into a `Value`.

### 6.8 `list` paging [t4r16.4]

Cursor `l3.<ord>`, parsed strictly; `l1.`, `l2.` or malformed is
`invalid_params`. A page runs `SELECT … FROM sessions WHERE ord < ?1 ORDER
BY ord DESC LIMIT 1000` (no cursor: no `ord` bound) and applies `state`,
`harness`, `label` and `since` to each row in order, stopping at `limit`
matches, at `PAGE_MAX` (a row's borrowed length checked before its summary
is built) or after the 1000 rows. `next_cursor` is `l3.<ord>` of the last row
examined, or `null` once the oldest session was examined.

Guarantee: `ord` is set once and strictly increases, so every session that
existed at the first page is examined exactly once and returned if it
matches the filters then; a later session is never returned. Termination:
the cursor strictly decreases. A filter that matches little gives short or
empty pages with a non-null cursor. N2 is decided this way (A38).

## 7. Evidence folder (R8) [t4r16.1]

VIA reads the vendor stream live, keeps what R1–R6 need and discards the
rest. The agent keeps the conversation **[V, probe]**: Claude writes prompts,
replies, tool calls, tool results and per-message `usage` to
`~/.claude/projects/<escaped cwd>/<session>.jsonl`; Codex writes a rollout
under `~/.codex/sessions/YYYY/MM/DD/`; OpenCode writes its database and
`opencode.log`. What they do not keep: Claude's transcript omits the
stream's `system/init` and `result` and its stderr; Codex sends diagnostics
to stderr only; OpenCode's bad-model error appeared only on its stream. The
evidence folder holds that remainder, and a final text too large for the
envelope (§6.4).

### 7.1 Layout

```text
<state>/evidence/<session_id>/<turn>/   0700, one per submitted turn
  stderr.log         the agent's stderr, written by the operating system
  undecoded.bin      the first 64 KiB of a message VIA could not decode
  final_text.txt     the final text, when longer than 256 KiB (§6.4)
```

There is no vendor debug file in Task 4 [t4r16.7.2]; the Claude task may
add one on evidence.

Per session, then per turn: retention retires a session with one directory
removal, and a turn's folder is its own. Fixed names let `logs` list files
without reading the directory. Session IDs and turn numbers are validated
internal IDs, never caller paths (runtime §6.1). `turns.evidence_dir` holds
`evidence/<session_id>/<turn>`, committed with `turn.submitted`; `logs` and
the envelope make it absolute.

### 7.2 Owners and creation

- **Store** validates or creates `<state>/evidence` (0700, not a symlink,
  the daemon's uid) at `Store::open`, as it does `raw/` today
  (`crates/via-store/src/runtime.rs:991-997` [V]), and hands Wire an
  `EvidenceRoot` in `RuntimeResources` in place of `RawFactory`.
  `EvidenceRoot::path(session, turn)` computes a path with no I/O.
- **Wire** creates `<session_id>` if missing and `<turn>` exclusively (a turn
  launches once), on the blocking pool, in `open_connection` before Host's
  acquisition.
- **Host** opens `stderr.log` (`create_new`, 0600, `NOFOLLOW`) from the new
  `PrivateProcessSpec.stderr_path` and gives it to the anchor as stderr, in
  place of a pipe (`crates/via-host/src/host.rs:1164-1166` [V]). The vendor
  inherits it (`crates/via-host/src/anchor.rs:287-289` [V]) before the anchor
  detaches its own streams (`anchor.rs:449` [V]). The operating system
  writes the file; no VIA task reads it. `OwnedPipes` loses `stderr`
  (`host.rs:408-415` [V]).
- A folder or file that cannot be created fails the open before ARM, so
  nothing launched: `RouteError::Store`, disposed `failed(store)` like any
  VIA storage failure (`terminal.rs:98` [V]).

### 7.3 The undecoded message

When Route cannot decode a message (§2.2), it calls
`WireSender::keep_undecoded(&bytes)`. Wire writes the first 64 KiB to
`undecoded.bin` (`create_new`, so the first failure wins; one write on the
blocking pool under 2 s) and returns the path or the error. Route then fails
`protocol` with a `failure.message` such as "undecodable vendor message:
1,234,567 bytes; first 65,536 in <folder>/undecoded.bin" (or "…; not saved:
<error>"). Wire does the same itself for a message over the cap
(`MessageTooLarge`, from its assembly buffer) and an unterminated tail at
EOF. 64 KiB bounds VIA's write; the stated length keeps the size known.

### 7.4 Vendor session and transcript [t4r16.7.3]

- `session.vendor_identity_confirmed` gains `transcript?`: the route's
  absolute path hint for the vendor transcript, at most 4 KiB. Core commits
  it with `vendor_session_id` in the transaction that confirms identity
  (C2 §4). Both are nullable; the fake reports neither. The layout is the
  vendor's, so each vendor task confirms it **[U]**; VIA never parses or
  deletes the file.

### 7.5 Bounds, failure and retirement

- **stderr is uncapped** [t4r16.7.5]. The operating system writes
  `stderr.log`; VIA adds no size check, timer, failure or tail. A process
  that escaped the group (runtime §1 limit 3) can keep writing after the
  turn. The free-space floor (§5.3) stops new work if the disk fills.
  Both are limitations (§15), and `via-d9o.2.3` measures stderr sizes
  (§16).
- **VIA's own files** are bounded: `undecoded.bin` at 64 KiB (§7.3),
  `final_text.txt` at 64 MiB (§6.4).
- **Failures.** The OS's writes are the vendor's concern (a vendor that dies
  of `ENOSPC` is `process_exited`). VIA's undecoded write is best effort and
  reported in the failure message. Recovery does nothing: a folder is
  complete as written.
- **Retirement** (`via-jm4.18`): `remove_dir_all` of
  `evidence/<session_id>`, with the session's rows.

### 7.6 Daemon log `via.log` [t4r16.7.1]

The daemon's own warnings and errors (its `tracing` output) go to
`<state>/via.log`. It is diagnostic, not a C1 contract, and `logs` does not
list it.
- **Why.** [V] Today the subscriber writes to stderr
  (`crates/via-cli/src/server.rs:96-99`), and so does the shutdown summary
  (`crates/via-cli/src/server/shutdown.rs:146`). An auto-started daemon's
  stderr is a pipe the CLI reads only while it starts
  (`crates/via-cli/src/client.rs:133`, `:250`), so later lines are lost.
- **Opening.** After both locks are held (a State mutation, runtime §6.1),
  before `Store::open`: a `via.log` longer than 10 MiB is renamed to
  `via.log.1`, replacing it; then `via.log` is opened for append (0600,
  `NOFOLLOW`). There is no other size bound (§16). An open failure is a
  startup failure, reported on stderr.
- **Writer.** `DaemonLog` (a `Mutex` over the file and a `startup` flag)
  is the subscriber's `MakeWriter`. During startup a line goes to stderr and,
  once open, to `via.log`; just before serving (`server.rs:196`, before
  `main.serve`) the flag clears and stderr is no longer written. The CLI can
  still report a failed start. Lines are rare, so each is one synchronous
  `write_all` under the mutex; a write error is ignored.
- **Content.** Every `tracing` call about a session or turn carries
  `session` and `turn` fields. The shutdown summary (runtime §7) is written
  through `DaemonLog` as one line.

## 8. Wire

[V] Today `next_message` reads 8 KiB chunks and awaits every raw append
inline (`crates/via-wire/src/runtime.rs:451`, `record` `:527`); `read_either`
selects over both pipes (`:546`); `write_message` awaits each piece's raw ack
(`:386`); `WireHealth` is unused (`crates/via-wire/src/lib.rs:103`).

### 8.1 Shape (runtime §4)

`open_connection(connection_id, evidence, spec, deadline)` returns
`WireConnection`; `into_parts(self) -> WireParts { sender, messages }`.
`WireSender: Clone` is the control handle (stdin command senders,
`Arc<ProcessControl>`, exit and latch receivers; `write`, `close_input`,
`close`, `wait_exit`, `failure()`, `keep_undecoded`). `WireMessages` is
unique and owns the connection's life (the message receiver, the task
`JoinSet`, the stop signal; `next_message(&mut self)`, `finish(self,
deadline)`). Both belong to the turn. `VendorMessage` is its bytes alone.

### 8.2 The stdout reader

One task reads stdout up to 64 KiB into its fixed buffer and never awaits a
consumer or the Store (runtime §4, coding-style §5). A `LineSplitter` splits
on LF; an unfinished message goes to the 1 MiB assembly buffer; at LF the
message is counted against the 64-message / 4 MiB queue and sent with
`try_send`. A message over 1 MiB is `MessageTooLarge` (prefix saved, §7.3);
a full queue is `Reader(Overflow)`. Either switches the reader to discard
mode: read to EOF, count discarded bytes, keep nothing, so the vendor never
blocks on a full pipe while it is stopped. A tail at EOF is the in-band end
`Unterminated`.

### 8.3 Stdin writer task

One task owns `ChildStdin`, with a data queue `mpsc(1)` and a control queue
`mpsc(8)` (at most 64 KiB; duplicate interrupt or close coalesced).
`WireSender::write(message, deadline)` enqueues and returns a cancel-safe
`PendingWrite`, which Route keeps pinned while it services every control
arm (§9). The task writes piece by piece and selects every await on the stop
signal and the deadline; a partial write then deadline closes stdin and
answers `Indeterminate`; a group kill surfaces as `EPIPE`. The streamed start
`OutboundMessage::Start{prefix, prompt: String, suffix}` cuts the prompt at
character boundaries into 16 KiB slices escaped into a reused piece buffer,
so no second whole copy exists (today `FakeStart` builds one byte string,
`crates/via-routes/src/lib.rs:47-71` [V]). `close_input` is idempotent and
acknowledged after the endpoint drops.

### 8.4 One health state

`ConnectionLatch` (`watch::Sender<LatchState { first: Option<FailureCause>
}>`) alone owns a connection's failure state, written with
`send_if_modified` by the reader, the stdin writer and `next_message`; first
failure wins; every wait selects on it. Outcomes: queue full →
`Reader(Overflow)`; message over 1 MiB → `Reader(MessageTooLarge)`; pipe
read or stdin write
error → `Reader(Transport)` / `Writer(Io)`; EOF is in-band.
`WireConnection.evidence` is deleted.

### 8.5 Consumer rules

`next_message` selects, biased: force, Route's wake, the queue, the latch. It
never reads a pipe; a wake or cancel loses nothing. After a latch failure
queued messages are dropped. `read_either` and `drain_to_eof` are deleted.

### 8.6 `finish`: one deadline

`WireMessages::finish(self, deadline)` is the only normal end: (1) the stop
is already ordered (Route's close, Host's kill, or vendor exit); (2) drain
until stdout EOF and the writer ends, or `deadline − 250 ms`; (3) set the
stop signal and `abort_all()` (every await is cancel-safe); (4) join until
`deadline`; (5) a task still unjoined moves to `WireRuntime`, which joins it
as it ends and reports it at shutdown in `WireShutdown.pending_tasks`
(`crates/via-wire/src/runtime.rs:775` [V]): an owner keeps tasks that miss
its join bound (coding-style §5). None is expected. `Drop` without `finish`
aborts, hands the set over the same way, and bumps a test-only
`wire::fallback_drops()` counter that every normal test asserts is zero.

Every `run_turn` exit after `open_connection` returned calls `finish` with
one absolute deadline (the graceful close's `close_by`, the force close's
cleanup deadline, or `failed.close_by`). An open that fails before then
drops any launched pipes: stdout is not kept (R8) and stderr is already in
its file, so `drain_pipes` and `LAUNCH_DRAIN`
(`crates/via-wire/src/runtime.rs:27`, `:626-667` [V]) are deleted and
`WireError::Acquire` loses `raw`. Route maps the error as today
(`crates/via-routes/src/runtime.rs:148-152`, `acquire_failure` `:769` [V]).

## 9. Serviceability: no wait hides a control

- **Route** keeps pinned the pending start or interrupt `PendingWrite` and
  the hop `reserve()` future, and selects, biased: (1) daemon force; (2)
  turn deadline; (3) the latch; (4) `hop.closed()` → `Overflow`; (5) its
  wake → `Control::on_wake`, which at most enqueues one interrupt or returns
  `Stopped` at `force_at`; (6) a pending write completing; (7) the reserve
  completing, which sends; (8) `next_message`. [V] Today `on_wake` awaits
  `write_message` inline (`crates/via-routes/src/runtime.rs:469`) and
  `forward` selects only on send and force (`:738-757`).
- **Adapter.** The pending delivery (item bytes, stall timer, send) is a
  pinned future polled beside `route` and the hop; `recv` is disabled while
  it is pending. If `route` completes first, undelivered data turns an `Ok`
  into `overflow`, as today.
- **Core.** `while_polling(&mut execute, &mut early, fut)` awaits a Store
  commit while polling the adapter future and storing an early result; it
  wraps every commit in the drive loop. [V] Today a commit arm runs to
  completion without polling the adapter (`drive.rs:1292-1320`).

## 10. Connection layer and C1 ingestion (F5)

[V] Today `admit` spawns a task with no socket cap
(`crates/via-cli/src/server/serving.rs:245`); the line reader buffers up to
`MAX_LINE` = 16 MiB (`crates/via-cli/src/server/dispatch.rs:22`) with no
deadline, breaks silently on oversize (`:45-46`) and builds a whole-request
`Value` (`:48`).

### 10.1 Sockets, lines, replies

- The accept loop owns `Semaphore(32)`; the 33rd peer is closed without
  bytes. The connection task is sequential (§4).
- A line is read into one buffer of at most 1 MiB under one 5 s deadline
  from its first byte to its LF (runtime §8, F5; seam
  `VIA_TEST_PARTIAL_LINE_MS`); an idle connection has none.
- A line over 1 MiB, LF included, gets one `request_too_large` error
  (-32020, `id: null`, `data {max_bytes: 1048576, use: "prompt_file"}`)
  under a 2 s write bound, then the connection closes without reading the
  rest [t4r16.2]. The named kind tells a program to use `prompt_file`,
  which `parse_error` would not.
- A request `id` over 256 B is `invalid_request` (A31). A reply not written
  within 10 s of being ready closes the connection (A32) [t4r16.5.5]: the
  timer starts before the first write, so a peer that never reads cannot
  stall it.

### 10.2 JSON limits (A10) and no peer `Value`

- `json_limits::scan` (`crates/via-store/src/json_limits.rs`) runs before any
  serde pass on every C1 line and vendor message: a byte scanner tracking
  string boundaries, depth and a node count (every value and key), failing
  at depth 65 or node 65,537 (`parse_error` for C1, `RouteError::Protocol`
  for a vendor). It exists because C1 requires the limits before an
  unbounded value is built. On a valid prefix it and serde_json agree on
  token boundaries [I].
- **Placement** [t4r16.5.3, N4]. It stays in via-store, the lowest crate
  (`scripts/check-layers.py:11-20` [V]). Cargo edges are not transitive and
  via-routes depends only on via-wire, via-cli only on via-core, so via-wire
  re-exports it for Routes and via-core for the C1 reader, as each already
  re-exports via-store items (`crates/via-wire/src/lib.rs:8`,
  `crates/via-core/src/lib.rs:8`, `:91` [V]). The layer graph is unchanged.
- No `Value` is built from peer bytes: with the workspace's `raw_value`
  feature (`Cargo.toml:16`), `Value`'s deserializer re-parses a member keyed
  `$serde_json::private::RawValue`
  (`serde_json-1.0.151/src/value/de.rs:131-134` [V]). Free-form C1 members
  (`vendor`, `bound`, `effort`, `output_schema`, `max_steps`,
  `instructions`, `require`) are `Box<RawValue>`, inspected only by
  `json_limits::shape` and `string_list`.

### 10.3 Decode once; stream the identity

Per line: (1) `json_limits::scan`; (2) the borrowed envelope `{jsonrpc, id,
method, params}` as `&RawValue`, borrowed from the line
(`serde_json-1.0.151/src/read.rs:637-652` [V]); (3) `id` checked and
copied, `method` decoded; (4) for keyed calls the identity pass forms the
borrowed pieces around the handle span and, for `prompt_file`, around its
value, which becomes the copy's `"sha256:<64 hex>:<len>"` (§10.4): one `Vec`
up to `INLINE_MAX`, else streamed into a `BlobWriter`; (5) the strict DTO
decode; (6) a `prompt` over `INLINE_MAX` streams into a `BlobWriter`; (7)
drop the line and decode buffers.

### 10.4 Prompt file [t4r16.2]

For a `spawn` or `resume` with `prompt_file`, after the DTO decode and the
floor check (§5.3), before the key lookup:

1. The path must be absolute and at most 4096 bytes. The handler opens it
   read-only with `O_NONBLOCK` (a FIFO cannot block the open), on the
   blocking pool, then `fstat`s the handle: a regular file of at most
   16 MiB.
2. It copies the file into a `BlobWriter` in 64 KiB chunks with a running
   SHA-256 and a streaming UTF-8 check (a sequence split across chunks
   carries over), under one 10 s bound for the whole copy.
3. After EOF it `fstat`s again. The copy is refused if the bytes read differ
   from the first size or the size, `mtime` or `ctime` changed. So a file
   that changed during the copy is refused, never stored torn; the blob is
   self-consistent in any case, since it holds exactly the bytes hashed.
4. A failure discards the blob and answers `invalid_params`, kind2
   `prompt_file`, `reason` one of `not_absolute`, `unreadable`,
   `not_regular`, `too_large`, `not_utf8`, `changed`, `timeout`.
5. The prompt is the blob (`turns.prompt_blob`). A keyed retry copies again
   and compares identities; on a match the new blob is discarded and the
   stored receipt returned.

The daemon runs as the caller's user behind a user-only socket, so reading a
file that user can read grants nothing new; the path is not stored.

## 11. Spawn members, `cwd`, counts and `status` members

### 11.1 Spawn members and `cwd`

[V] `SpawnParams` has none of these today, so `deny_unknown_fields` refuses
them. Added: `prompt_file` (§10.4); `cwd` (at most 4096 bytes, absolute, an
existing directory, else `invalid_params`); `label` (at most 120 bytes);
`allow_untested` (bool, default false); `instructions` (`Box<RawValue>`; the
fake refuses any by name); `require` (`Box<RawValue>` expanded by
`string_list`, each name checked against `Capabilities::fake()`, the first
unmet refused by name). Frozen `params` becomes `{harness, model, cwd,
allow_untested}`; an omitted `cwd` freezes the fake's configured default
(`crates/via-adapters/src/fake_config.rs:44` [V]).

`cwd` is applied: `FakeConfig::process_spec` (`fake_config.rs:53`) takes
`cwd: &Path` in place of `self.cwd` (`:75` [V]); `QueuedTurn` gains `cwd`;
the drive passes it to `execute`; the envelope (`terminal.rs:67`, `cwd:
None` today [V]) and `status` report it; a fake `ReportCwd` step tests both.
The fake wall default becomes C1's 3,600,000 ms (A3; `api.rs:884` is 30,000
today [V]); a nested `null` in `deadlines.*` is `invalid_params` (A9;
`api.rs:88` reads it as omitted [V]).

### 11.2 `daemon/status`: `started_at` and counts (A4)

`started_at` is set once in `Engine::open`. `closing` = `Sessions.closing`
(at `engine.rs:110` today [V]); `active` = distinct sessions in `Unresolved`
not closing; `open` = `Sessions.open`, seeded after recovery with
`COUNT(*) WHERE state != 'closed'`, +1 at spawn receipt, −1 on each
closed-now Store answer; `idle` = `open − closing − active`, saturating. An
uncertain close leaves `open` stale until restart.

### 11.3 `status` durable members (A16, A23)

| Member | Source |
|---|---|
| `session_id`, `state`, `admission`, `harness`, `label`, `created_at`, `updated_at` | `sessions` (v6); `updated_at` is the list's `last_active_at` |
| `model`, `cwd` | `json_extract(params, …)` |
| `route` | `json_extract(receipt, '$.route')` |
| `vendor_session_id` | `sessions.vendor_session_id` (`null` for the fake) |
| `vendor_identity_verified` | `false` unless the current connection generation confirmed it; always `false` for the fake (A16) |
| `process.alive` | positive evidence only: the Host ledger holds a live control for an anchor the session owns, phase `Armed` (`host.rs:104-111` [V]), whose exit watch has not reported an exit (`track_control`, `host.rs:1319` [V]; `LiveControl` gains a clone of that receiver, read by `Host::live_armed(&ids)`); after a restart, `false` |
| `process.cleanup` (A23) | `"uncertain"` when any group of the session's turns lacks a durable absence proof, else `"quiescent"` |
| `process.idle_since` | `null` (A16) |
| `active_turn` | the `running` turn: `phase` `accepted` if `accepted_at` else `submitting`; `started_at`; `last_event_seq` from `(session_id, turn, seq)`; `cancel` from its first `cancel.requested` |
| `queue` | at most 8 queued turns `{n, op_key, queued_at, effective}` |
| `turns` | the newest 64 `{n, state}`, `revision: 0` on terminal turns |
| `progress`, `steps` | §4.2 (A26) |

`alive` travels the pass-through chain `pending_cleanup` uses [V]:
`crates/via-core/src/engine.rs:406` → `crates/via-adapters/src/runtime.rs:252`
→ `crates/via-routes/src/runtime.rs:267` → `crates/via-wire/src/runtime.rs:139`
→ `crates/via-host/src/host.rs:865`; each hop gains `live_armed(&ids) ->
bool`.

## 12. Amendments

Numbered `T4-A<n>`. Each lists its edits, one per row, located by file, line
and opening words; quoted text is exact. The orchestrator applies spec
edits after review; historical reports and decision files are not edited.
Paths: C1 = `docs/specs/via-api-v1.md`, C2 = `docs/specs/adapter-contract.md`,
RT = `docs/specs/runtime-contracts.md`, vendor specs in `docs/specs/vendors/`.

### 12.1 Status of earlier amendments

| # | Round 16 |
|---|---|
| A1 raw staging overflow | **withdrawn** [t4r16.1] |
| A2 Store lanes | kept; allowances fixed (§6.1) |
| A3, A4, A9, A10, A13, A14, A15, A23 | kept (A4 §11.2; A10 §10.2; A14 §11.1; A15 §6.6; A23 §11.3) |
| A6, A19, A21, A22, A35 | withdrawn earlier |
| A12 two-phase `list` | **withdrawn** [t4r16.4]; A38 |
| A16 S1 `status`/`list` definitions | kept, except `vendor_session_id` (its v6 column) and the list summary (A38) |
| A24–A33 | kept, revised below |
| A34 stop cause `overflow` | **withdrawn** [t4r16.5.7]; there is no envelope overrun (§2.3) [t4r16.7.8] |
| A36 memory pool and disk budgets | **withdrawn** [t4r16.2, t4r16.3]; A42, A43 |
| A37 daemon config | revised: five keys |

### 12.2 Amendments

**T4-A24. Durable events (R1, R2).**

| Location | Edit |
|---|---|
| C1 §6 heading | becomes "## 6. Durable events" |
| C1 §6.1 table | delete the rows `assistant.text`, `reasoning.summary`; `tool.started` / `tool.ended`; `file.changed`; `usage.updated`; `vendor.other`; in the `process.exited` row (`:539`) delete `raw_log.incomplete` and `connection_id` |
| C1 §6.1 example (`:517`), text (`:523`) | delete the `raw_ref` member and "`raw_ref` is `null` for synthesized events." [t4r16.1] |
| C1 §6.1 after the table | add the paragraph below |
| C1 §6.1 last paragraph (`:550`) | becomes "Rust: `#[serde(tag = "type")]` on the serialize side, tags set with `rename`; a client keeps an unknown type as `Other { type, payload }`." |
| C1 summary (`:50`) | delete "optional `raw_ref`" |
| `.repo-context/CONTEXT.md` **Event** (`:62-64`) | the definition below |
| CONTEXT **Vendor message** (`:59`) | "An unknown message type becomes a `vendor.other` event" becomes "An unknown message type is activity only"; add after it "**Observation**: What an adapter reports from one vendor message. Core turns an observation into a durable event, a progress update, or envelope accumulation. _Avoid_: event (reserved for durable records)." |
| CONTEXT **Step** (`:56`) | append "VIA counts a step each time the model produces output after tool results, the same way for every vendor, and records one `steps` row per step; the envelope's `steps` is the vendor's own count." |
| Restatements | C2 `:80`, `:273-276`, `:453` (A29); RT `:193` (A33); `claude-code.md:233-244`, `codex.md:250-265`, `opencode.md:521`, `:527-531` (A33); `docs/workstreams/rust-foundation/t3/design.md:609-615` (idle progress becomes "acceptance, and progress items with model output or a tool start or end") and `:1754` (the `s1_f12_event_not_committed_…` test uses a step-row commit and `cancel.requested`); code `crates/via-core/src/engine/drive.rs:1844-1884`, `:1775-1786` |

> Events are durable records only: an event exists when crash recovery or the
> envelope depends on it. Model text, reasoning, tool calls, usage updates,
> file changes and unknown vendor messages are not events; the agent's own
> transcript keeps them (`logs`, §3.12), and a running turn's progress is in
> `status` (§3.7).

> **Event**: VIA's durable record of a lifecycle, control or safety fact in a
> session, such as `turn.started`, `cancel.requested`, `action.denied` or
> `turn.ended` (full list in VIA API §6). It is harness-neutral. Most events
> belong to one turn; a few (`session.opened`, `session.closed`) belong to
> the session. Each has a dense per-session `seq`. Core commits it to the
> Store, and callers page it with `events`. Model text, tool calls and usage
> are not events: they drive the progress snapshot and step rows, and the
> agent's own transcript keeps them.

**T4-A25. No follow stream (R5).**

| Location | Edit |
|---|---|
| C1 §3.11 (`:314-360`) | heading "### 3.11 `events` — page"; delete " [--follow]", "`follow?`, ", ", subscription?" and the bullets on `follow: true`, session-wide follow, outboxes and `unsubscribe`; keep the page and `history_pruned` bullets (retention produces `history_pruned`; until then `earliest_seq` is 1); append "There is no follow stream: callers poll `status` (§3.7) for progress and `wait` (§3.8) for the end of a turn." |
| C1 §1 (`:79`) | "Requests carry `id`; notifications flow daemon → client only for follow (§3.11). No batches." becomes "Requests carry `id` (A31); the daemon sends no notifications. No batches." |
| C1 summary (`:37`) | "canonical events (page/follow), raw excerpts" becomes "durable events (page), evidence locations" |
| C1 §6.2–6.3 (`:553`) | "### 6.2 Ordering": "Per session FIFO in `seq`; no promise across sessions (D4). `turn.ended` is the last non-late event of its turn." |
| C1 §7.6 last row (`:661`) | "followers whose subscription ended must poll `result`" becomes "a caller that already read the result must read it again" |
| C1 §10 (`:748`, `:761`) | delete "Q1 session-wide follow; "; Q7 "Outbox and channel sizes (1000 events; C2 A1 limits)" becomes "Channel sizes (C2 A1 limits)" and "as written, config-tunable" becomes "as written, fixed; disk and WAL thresholds are daemon config (runtime §8)" |
| C1 §3.8 | append "`wait` is the only blocking read; it checks at once, then once per second. A caller that wants progress polls `status` (§3.7) on another connection, since a connection carries one request at a time. Closing the connection of a pending `wait` releases only that waiter." |
| RT §9 (`:1080-1120`) | the text below |
| RT §1 (`:28`, `:35-37`) | delete "Live observers consume durable events, never an independent best-effort copy."; limit 2 becomes "2. A blocked peer cannot be guaranteed a reply. The daemon closes the socket after the 10 s reply deadline; the caller retries the read." |
| RT §2 Core row (`:77`) | "subscribers" becomes "progress snapshots" |
| RT §6 (`:755`, `:771-772`) | "wakes waiters/followers" becomes "wakes waiters"; "no follower holds" becomes "no read holds" |
| RT §7 (`:941`), §8, §10, §11 (`:1314`) | delete the row "Following affected history" and the rows "Subscriber outbox", "Subscribers"; §10 row "C1 §3.11" appends "Superseded by T4-A25: follow removed."; §11 row "blocked socket, replay boundary barrier, unsubscribe barrier" becomes "`core.progress.publish`, crash after a step commit \| F25/F26 (superseded): status answers from memory during a flood; step rows survive a crash up to the last committed step" (Q-R5-5) |
| Elsewhere | `docs/workstreams/rust-foundation/s1-plan.md` lines 19 and 207 drop "follow", "unsubscribe" and "subscription cleanup"; F25, F26 (`:88-89`) and the F25 note (`:134`) follow Q-R5-5; `t3/design.md:1414-1417`, `:1919` are obsolete; in `claude-code.md:59` and `opencode.md:461` the C1 method `unsubscribe` is deleted from the list. Vendor methods stay: OpenCode's SSE subscription (`opencode.md:455`, `:460`), Codex's `thread/unsubscribe` (`codex.md:59`, `:87-88`, `:109`, `:232`, `:395`; C2 `:366`; C1 `:257`) |

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
> turn's end; `wait` checks the Store at once, then once per second.
> Cancelling a wait request only releases that waiter.
>
> A reply is written within 10 s of being ready to write, otherwise the
> socket closes (C1 §1), so a peer that never reads holds its reply buffer
> for at most that long. `logs` returns only the addressed turn's evidence
> locations (C1 §3.12).

**T4-A26. `status` returns progress and step history (R3, R4, R5).** C1 §3.7
(`:274`): after the heading add "`via status <session> [--turn N]
[--after-step N] [--limit N]`" and "Params: `session`, `turn?` (default the
running turn, else the latest), `after_step?` (default 0), `limit?` (default
100, max 1000)." In the example `process` becomes
`{"alive":true,"cleanup":"quiescent","idle_since":null}`, and after
`active_turn` add:

> ```
>  "progress":{"turn":2,"current_step":4,"phase":"tools","running_tools":["shell"],"tools_overflow":false,"last_activity_at":"…",
>              "tokens":{"total":18200,"scope":"vendor_interval"}},
>  "steps":{"turn":2,"items":[{"step":1,"started_at":"…","ended_at":"…","tokens":5100}],"next_after":1,"more":true},
> ```

Before "`vendor_session_id` is nullable" add:

> `status` describes one turn: the `turn` param, else the running turn, else
> the latest. `progress` is an in-memory snapshot of that turn while it runs
> in this daemon, read after `steps` without a Store round trip, and `null`
> otherwise. `current_step` is VIA's count of model steps: 0 before the
> vendor accepts the turn, 1 after, and one more each time the model
> produces output after tool results, derived the same way for every vendor;
> it equals the vendor's model calls only where that vendor's evidence shows
> it. `running_tools` holds at most 64 names of tools started and not ended,
> never inputs or outputs; `tools_overflow` is true when more started since
> the last step boundary. `phase` is `tools` while a listed tool runs or
> `tools_overflow` is true, else `model`; both reset at each step boundary.
> `last_activity_at` is the arrival time of the last vendor message
> attributed to the turn. `tokens` is an approximate running total of
> completed steps, updated once per step, labelled with the route's token
> scope (§4.1), or `null` when the route has no validated source or before
> the first sample. The envelope's `steps` and `usage` hold the final
> figures.
>
> `steps` pages the durable step history of the selected turn, running or
> finished: one item per completed step, ordered by `step`. The step in
> progress has no item yet, and a step that ended after the Store read
> appears on a later call. After a daemon crash the history holds every step
> whose row was committed; a step whose row was being committed, and the
> step then in progress, are missing, and the agent's transcript has them. A
> committed `turn.ended` implies that all the turn's rows are durable, also
> after a refused Store write or a stop forced at shutdown, except for a
> terminal synthesized by crash recovery or by the failure-resolution batch
> after a Store write of uncertain outcome (runtime §7).
>
> `process.alive` is true only on positive evidence that the vendor process
> is live; `process.cleanup` is `uncertain` when any process group of the
> session lacks a proof of absence, else `quiescent` (T4-A23).

Restatements: C1 `:571` (unchanged); A16's list (`:274-297`).

**T4-A27. `logs` returns the evidence locations (R5, R8)** [t4r16.1]. C1
§3.12 (`:362-370`) becomes the text below. Restatements: C1 `:37` (A25);
`codex.md:223-224`, "Raw extraction uses individual event `raw_ref`s, never
shared-connection bounding spans." becomes "Evidence for the shared server
(`logs`) is defined by this adapter's task under D4: it never returns
another session's evidence."

> ### 3.12 `logs` — evidence locations
>
> `via logs <session|turn>`. Returns where a turn's evidence is, for the
> addressed turn or, for a session, its running turn else its latest
> submitted turn: `{session_id, turn, vendor_session_id, transcript, folder,
> files: [{name, bytes}]}`. `transcript` is the path of the vendor's own
> transcript, a hint that follows the vendor's layout, or `null`. `folder`
> is the turn's evidence folder in VIA's state directory, or `null` for a
> turn never submitted. `files` lists the files there that exist:
> `stderr.log` (the agent's stderr), `undecoded.bin` (the first 64 KiB of a
> vendor message VIA could not decode, named by the turn's failure) and
> `final_text.txt` (a final text too long for the envelope, §5). VIA does
> not read or decode them; the caller reads the files. There is no paging.
> [t4r16.7.2, t4r16.7.8]

**T4-A28. `steps`, event and evidence columns; write ordering.**

| Location | Edit |
|---|---|
| RT §6 target table (`:702-712`) | `events` row (`:709`) becomes "`events`: separate `turn` and `type` columns (late and time stay in the event JSON); no raw reference columns \| `via-jm4.7.8`"; `sessions` vendor row (`:705`) becomes "`sessions`: vendor session ID and transcript hint \| columns `via-jm4.7.8`, values the first vendor adapter slice"; delete the `connections` row (`:710`) [t4r16.1]; add "`steps`: `(session_id, turn, step, started_ms, ended_ms, tokens)`, primary key `(session_id, turn, step)`, one row per completed model step, the last in the terminal transaction; a session's rows go by one keyed delete \| `via-jm4.7.8`" and "`turns.evidence_dir` (runtime §4) \| `via-jm4.7.8`" |
| RT §6 v4 table (`:687-698`) | rewritten as v6 (A14); its `events` row (`:697`) loses the raw columns |
| RT §6 write ordering items 3–6 (`:747-757`) | the text below |
| RT §6 (`:759-767`) | "No SQLite transaction waits on a raw sync…" through "…after detecting corruption." becomes "No SQLite transaction waits on vendor I/O." |

> 3. Host commits anchor intent/generation, starts anchor, commits its
>    verified identity, configures, commits `ArmIntent`, then sends ARM once.
>    Anchor spawns vendor in its inherited group with the turn's stderr file
>    and detaches fd 0/1/2 before its acknowledgement; Host records vendor
>    facts. Wire starts its stdout reader and writes the prompt. Vendor
>    acceptance is independent evidence.
> 4. Adapter emits an observation; Core commits acceptance, durable events
>    or a step row, or folds it into the progress snapshot. The two
>    acceptance paths deduplicate by correlation token.
> 5. Core commits terminal event/envelope, then wakes waiters. Cleanup gate
>    separately controls next dispatch.
> 6. Shutdown performs final Store flush/checkpoint and joins before
>    releasing daemon lock.

**T4-A29. Observations after R2; the stall closes the hop.**

| Location | Edit |
|---|---|
| C2 summary A1 (`:55`) | the decision text below |
| C2 §1 rule 6 (`:80`) | "6. Unknown vendor notifications produce no observation: when the route attributes them to a turn, they update that turn's activity time; a malformed known message is a `protocol` observation." |
| C2 §2 lanes (`:227-231`) | from "The 1024-item observation queue also has a 4 MiB budget" to "explicit truncation marker." becomes "The 1024-item observation queue also has a 4 MiB budget; final text is sent as completed `final_text` pieces whose whole encoded observation is at most 256 KiB; another known payload over 256 KiB encoded fails protocol." |
| C2 summary `observations` (`:40`) | "C1 events minus Core fields, plus `turn.vendor_terminal`, `turn.accepted`, `tool.quiescent`" becomes "the durable C1 event payloads an adapter reports (`action.denied`, `vendor.request_declined`, `steer.delivered`, `warning`), `progress` and `final_text`, plus `turn.vendor_terminal`, `turn.accepted`, `tool.quiescent` and the other internal observations of §4" |
| C2 §4 first paragraph (`:273-276`) | "`Observation` = the C1 event payloads Core commits (`action.denied`, `vendor.request_declined`, `steer.delivered`, `warning`), at most one `progress` item per vendor message that carries a progress mark, `final_text` pieces, plus internal ones Core turns into commits:" |
| C2 §4 table | `turn.vendor_terminal` loses "`final_text`, "; `session.vendor_identity_confirmed` gains `transcript?`, committed with the ID; add the two rows below |
| C2 §7 items 6, 8, 12 (`:448-450`, …) | item 6's "Tool completion and other evidence may arrive afterward, …" becomes "Tool completion and other evidence may arrive afterward and keep the original turn ID: a tool completion counts for P7 cleanup; a durable observation is committed `late` after Core terminal commit."; item 8's "`vendor.other`" becomes "activity only"; item 12's "a stall past `event_stall_ms` yields an interrupt and `overflow`" becomes "a stall past `event_stall_ms` closes the session's route hop: a private route fails the connection `overflow`; a shared route quarantines the thread generation (§4)" |
| RT §8 rows | "C2 observation payload" becomes "256 KiB encoded; final text sent in pieces; IDs, names, stop reasons and codes 1 KiB \| Fail protocol, the message saved to the evidence folder; unknown messages keep no payload"; in "C2 observations", "Core fails `overflow` and interrupts (A1)" becomes "the adapter closes the session's route hop; a private route fails the connection `overflow`, a shared route quarantines the thread generation (A1, C2 §4)" |
| C1 §8.2 `overflow` (`:711`) | "this session's event channel stalled past its limit (C2 A1)" becomes "this session's observation channel stalled past its limit, the connection's message queue overflowed, or a vendor message exceeded 1 MiB (C2 A1)" [t4r16.7.5, t4r16.7.8] |
| C1 §7.6 (`:657-658`) | Codex row: delete "record normalized-event loss separately from any actual raw gap (C2 §4)"; the row "Raw-log or event overflow failed the connection" becomes "\| Observation or message overflow failed the connection \| running \| `failed(overflow)` \|" |
| `claude-code.md:305` | "At 10 s stalled observations Core fails overflow and interrupts;" becomes "At 10 s stalled observations the adapter closes the route hop and the route fails the connection `overflow`, which interrupts;" (consistent already: `codex.md:276`, `:310-311`, `opencode.md:550`) |
| Code | `crates/via-adapters/src/runtime.rs:312-331` (`deliver`), `crates/via-routes/src/runtime.rs:738-757` (`forward`) |

> Backpressure: per-session observation channel of 1024 items and 4 MiB; a
> full channel blocks only that session's normalizer; control and sticky
> health travel separately and stay serviceable; Core failing to drain for
> `event_stall_ms` (10 s) fails the turn `overflow`: the adapter closes the
> session's route hop; a private route fails the connection, which
> interrupts the vendor, and a shared route quarantines that thread
> generation as for an ingress overflow (§4) while other threads continue;
> Wire message-queue overflow fails the connection (coding-style §5). A
> known observation payload is at most 256 KiB encoded (final text is sent
> in pieces), else protocol failure; IDs, names, stop reasons and codes are
> at most 1 KiB each. Unknown and unattributed messages produce no
> observation.

> | `progress` | `at`, `model: bool`, `tools_started: [(id, name)]`, `tools_ended: [id]`, `usage?: (key?, total)` | no commit: Core folds it into the running turn's progress snapshot and commits a `steps` row when a step ends (C1 §3.7). `model` marks model output (text, reasoning or a tool request); `usage` is an interval sample, never a cumulative total. A message with no mark sends no item |
> | `final_text` | `text` | no commit: Core appends the text to the turn's final text, inline up to 256 KiB encoded, else in the turn's `final_text.txt` (C1 §5). The adapter sends completed text only, cut so that the whole encoded observation, escaping included, is at most 256 KiB |

**T4-A30. The envelope is 1 MiB by construction; the transaction cap excludes it (R6).**

| Location | Edit |
|---|---|
| RT §8 "Store transaction" (`:1020`) | "at most 128 events and 1 MiB payload, not counting the one terminal envelope a transaction may carry (itself at most 1 MiB, C1 §5) or the turn's step rows it carries \| Split event batches without splitting a lifecycle atomic batch; refuse a larger request before it is queued" |
| C1 §5 (`:470-474`) | from "Accumulation is bounded to 1 MiB" to "`admission_refused` read error." becomes the text below [t4r16.7.8] |
| RT §8 "Envelope accumulation" (`:1025`) | becomes "\| Envelope \| 1 MiB by construction (C1 §5): final text over 256 KiB goes to a file of at most 64 MiB; the denied and declined lists keep 1,000 entries each \| Never fails the turn \|" |
| Consistent | `claude-code.md:304`, `codex.md:263` ("1 MiB envelope") |

> The encoded envelope is at most 1 MiB by construction, and no turn fails
> for the size of its result. `final_text` is inline up to 256 KiB encoded.
> A longer final text is written to `final_text.txt` in the turn's evidence
> folder (§3.12): `final_text` is then `null` and `final_text_file` gives
> `{path, bytes, truncated}`. The file holds at most 64 MiB; a longer text is
> cut there at a character boundary with `truncated: true`, as is a text
> whose file write failed. `denied_actions` and `auto_declined_requests`
> hold the first 1,000 entries each; `denied_actions_total` and
> `auto_declined_requests_total` count all. An entry's strings are cut at a
> character boundary to keep it within 256 bytes; its `event_seq` cites the
> event with the full payload. At receipt a `bound` over 32 KiB, a `vendor`
> over 16 KiB, or a `model` or `effort` over 1 KiB encoded is
> `invalid_params` naming the member. `failure.message` is at most 2 KiB,
> cut at a character boundary.

**T4-A31. Request `id` at most 256 bytes** (Q-R5-2). C1 §1, add: "A request
`id` is a string, a number or `null`, at most 256 bytes encoded; a longer
one is `invalid_request`."

**T4-A32. A reply is written within 10 s of being ready** [t4r16.5.5]
(Q-R5-10). C1 §1 Transport, add: "The daemon writes each reply within 10 s
of having it ready to write; a peer that does not read it in that time is
disconnected." RT §8 "Socket response serialization": append "\| each reply
written within 10 s of being ready, else the socket closes".

**T4-A33. Fake and vendor observation mappings.**

| Location | Edit |
|---|---|
| RT §3.1 (`:192-196`) | "An unknown notification tag without `id` follows the existing bounded `vendor.other` path." becomes "An unknown notification tag without `id` is activity only."; "§4 message splitting, §8 structure/payload limits, text splitting and raw durability still apply." becomes "§4 message splitting and §8 structure/payload limits still apply." |
| RT §3.1 (`:225-227`) | from "They map to existing C2" to the paragraph's end becomes "VIA reads only `vendor_turn_id` from `text`, and `tool_id` and `name` from the tool messages; the other fields are optional and not read. A fourth progress tag is `{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":120}` (a non-negative integer): one interval sample. These map to C2 `progress` items (C2 §4), not C1 events." |
| `claude-code.md` §5 table (`:233-240`) | text: "`progress` with `model`; final text comes from `result`, sent as completed C2 `final_text` pieces of at most 256 KiB encoded before the terminal"; `tool_use`: "`progress` with `model` and `tools_started (id, name)`; retain the open-item set"; `tool_result`: "`progress` with `tools_ended (tool ID)`; error/refusal remains error; unmatched IDs are protocol evidence"; `message.usage`: "`progress` `usage` keyed by message ID (unprobed)"; unknown: "no observation; moves the turn's activity time; cannot advance lifecycle or the idle timer"; stderr (`:240`): "written by the operating system to the turn's evidence folder; never read, parsed or used to reset idle" |
| `claude-code.md:242-244`, `:302` | "Never synthesize `file.changed`" … "the tool event suffices." becomes "VIA does not report file changes."; delete "Do not expose private chain-of-thought as `reasoning.summary`."; "256 KiB known observation (split text only)" becomes "256 KiB known observation (final text in pieces)" |
| `codex.md:250-253` | the paragraph from "Normalize agent-message deltas" to "instead of duplicating it." becomes "Normalize with C2 `progress` items: `agentMessage` and `reasoning` item starts and deltas are `model`; a tool item's `item/started` is `tools_started (itemId, item type)` and its `item/completed` is `tools_ended`; `thread/tokenUsage/updated` `tokenUsage.last` is a `usage` sample. Terminal statuses map as below. For the envelope's final text, send each completed `agentMessage` text as C2 `final_text` pieces in order (C2 §4); deltas are never final text." |
| `codex.md:264-265` | "Large text splits on UTF-8 boundaries; unknown notifications become `vendor.other` retaining at most 16 KiB with explicit truncation" becomes "Final text is sent as C2 `final_text` pieces of at most 256 KiB encoded; unknown notifications are activity only" |
| `codex.md:230-234` | from "Thus an already received or later delivered completion" to "admission to that session is closed." becomes "Thus an already received or later delivered completion after lease release is still attributed to its original turn: it still counts for P7 cleanup (`tool.quiescent`), and any durable observation it yields (`action.denied`, `vendor.request_declined`, `warning`) is committed with `late:true`. A tool completion alone is no event (C1 §6.1). `thread/unsubscribe` does not promise more vendor notifications. A detached session's Core observation sink remains eligible for these late observations even though admission to that session is closed." |
| `codex.md:356-357` | "Later tool events remain `late:true` evidence" becomes "Later tool completions count for P7 cleanup only" |
| `opencode.md:527-530` | from "Map text deltas and authoritative part snapshots" to "Unknown notification types become bounded `vendor.other`;" becomes "Map assistant text and reasoning parts to C2 `progress` `model`, a tool part entering `running` to `tools_started (call ID, tool name)` and one entering `completed` or `error` to `tools_ended`, correlated by session/message/part/call IDs; each assistant message's token snapshot is a `usage` sample keyed by message ID (§7 one ledger). Final text is the correlated completed assistant's text, sent as completed C2 `final_text` pieces of at most 256 KiB encoded. Unknown notification types are activity only;" |
| `opencode.md:521` | "`session.next.*` events are retained as bounded `vendor.other`, never a second text/tool/usage emission" becomes "`session.next.*` events are activity only, never a second text/tool/usage emission" |
| Left to the vendor tasks (R8) | Codex's raw text (`codex.md:247`, `:261`, `:278`, `:295-306`, `:385`, `:395`) and OpenCode's (`opencode.md:146`, `:564-575`) |

**T4-A37. Daemon config** [t4r16.3].

| Location | Edit |
|---|---|
| RT §8, after A43's paragraph | "The disk free-space floor, the data-size warning, the WAL limit and its checkpoint triggers are keys of `daemon.json` in the state directory, with provisional defaults. The daemon reads it once at start; a change takes effect at the next start, and an invalid file refuses to start with a named error. C1, C2, memory and the other runtime §8 limits are not configurable." |
| RT §6.1 layout (`:805-812`) | after `store.lock` add "`daemon.json  optional daemon config: disk floor and warning, WAL (§8)`" |
| C1 §3.14 (`:375-377`) | after "socket_path, store_path, health" add ", limits, storage"; add "`limits` holds the effective disk and WAL thresholds; `storage` holds `free_bytes`, `data_bytes`, `data_measured_at`, `below_free_floor` and `over_warn_size`." |

**T4-A38. `list` in creation order** [t4r16.4]. Withdraws A12. C1 §3.10
(`:307-309`): "Ordered by `(updated_at desc, session_id)`; … never be
skipped)." becomes "Ordered by creation, newest first. `cursor` is opaque;
each session that existed when the first page was read is examined once and
returned if it matches the filters then; sessions created later are not
returned. A page examines at most 1000 sessions, so it can be short or empty
while `next_cursor` is not `null`. Each summary is `{session_id, state,
admission, harness, model, label, created_at, last_active_at}`;
`last_active_at` is the time of the session's latest durable event, and
`since` matches `last_active_at ≥ since`." No restatement in runtime, t2 or
t3.

**T4-A39. Request lines at most 1 MiB; `prompt_file`** [t4r16.2].

| Location | Edit |
|---|---|
| C1 §1 (`:78`, `:83`) | "line length capped (Proposed 16 MiB)" becomes "line length capped at 1 MiB"; "The 16 MiB limit includes the line feed." becomes "The 1 MiB limit includes the line feed; a longer line gets `request_too_large` and the connection closes. A larger prompt is passed as `prompt_file` (§4)." |
| C1 §4 (`:401`) | the `prompt` row's note becomes "exactly one of `prompt` and `prompt_file`"; add "\| `prompt_file` \| absolute path \| per turn \| a regular UTF-8 file of at most 16 MiB that the daemon's user can read; the daemon copies it when the request is received and refuses it (`invalid_params`, kind2 `prompt_file`) if it changes during the copy. The path is not stored; the retry identity uses the copy's SHA-256 and length \|" |
| C1 §3.2 (`:175`) | `[--prompt-file F|-]` stays; add "`--prompt-file F` sends `prompt_file` with `F` made absolute; `-` reads stdin into `prompt`." |
| C1 §8.1 | add "\| -32020 \| `request_too_large` \| request line over 1 MiB; `data.max_bytes`; the connection closes \|"; the `admission_refused` row (`:687`) gains "Store read lane full; disk free space below the floor" |
| C1 §5 (`:473-474`) | "A terminal envelope that cannot fit the 16 MiB socket response limit …" goes with A30's text |
| RT §8 rows | "C1 line" becomes "1 MiB including LF \| `request_too_large`, then close"; "Global C1 input buffers" becomes "32 MiB by construction (32 sockets × 1 MiB) \| 5 s partial-request deadline prevents monopolization"; "Socket response serialization" becomes "1 MiB per response (a page, `status` or an envelope) plus 512 B" |
| RT §8 blob paragraph (`:1029-1039`) | "preserving the 16 MiB public request limit" becomes "for inline prompts over 256 KiB and for prompt files"; "Load only the dispatched prompt into the global 32 MiB input budget." becomes "Load only the dispatched prompt, one per running turn."; "C1's 16 MiB plus bounded JSON wrapper expansion" becomes "the 16 MiB prompt plus bounded JSON wrapper expansion" |
| Restatement | C1 §3.11's 16 MiB outbox budget (`:340`) goes with A25 |

**T4-A40. Envelope `steps` and `evidence`** [t4r16.4, t4r16.1].

| Location | Edit |
|---|---|
| C1 §5 example (`:493`) | `raw_spans` becomes `"evidence":{"folder":"…/evidence/s_7f3k9q2mzr4c/2","transcript":null}`; after `"final_text":""` add `"final_text_file":null`; after the two lists add `"denied_actions_total":1,"auto_declined_requests_total":1` [t4r16.7.8] |
| C1 §5 table (`:508-509`) | the `raw_spans` row becomes "\| `evidence` \| the turn's evidence folder and the vendor's transcript hint, as `logs` returns them (§3.12) \|"; delete `raw_log_incomplete` from `warnings`; after `usage` add "\| `steps` \| the vendor's own count of model steps in the turn (Claude `num_turns`), or `null` when the vendor reports none; VIA's count is only in `status` `progress` (§3.7) \|", "\| `events` \| `{first_seq, last_seq, count}` of the turn's durable events (§6.1) \|", "\| `final_text_file` \| `{path, bytes, truncated}` when the final text is in `final_text.txt`, else `null` \|" and "\| `denied_actions_total`, `auto_declined_requests_total` \| entries of each list, including those past the first 1,000 \|" |
| C1 summary (`:49`); CONTEXT **Envelope** (`:89`) | "raw spans" becomes "evidence locations"; "log reference" becomes "evidence locations" |

**T4-A41. Runtime §4 without a raw log** [t4r16.1].

| Location | Edit |
|---|---|
| RT §4 heading and sketch (`:259-270`) | "## 4. C4: message splitting, evidence folder and transport"; `WireRuntime { evidence: EvidenceRoot, host: Host }`; `VendorMessage { pub bytes: BoundedBytes }`; `Failed { cause: WireFailure }`; `open_connection` gains `evidence: EvidenceFolder` |
| RT §4 (`:299-303`, `:315`) | "obtains a `RawWriter` from its private factory" becomes "creates the turn's evidence folder"; delete "RawWriter, RawFactory" from both lists and "seal raw output" |
| RT §4 (`:318-366`) | from "One task drains stdout and one drains stderr." to the section's end becomes the text below |
| RT §1 (`:14`, `:26-27`) | "SQLite and connection raw logs" becomes "SQLite and per-turn evidence folders"; "An event/result means its transaction committed and every referenced raw range was synced first. It does not promise that all traffic survived a crash." becomes "An event/result means its transaction committed." |
| RT §2 (`:80`, `:82`, `:88`, `:94`) | Wire "byte message splitting and raw staging \| Vendor messages with durable raw evidence" becomes "byte message splitting, evidence files \| Vendor messages"; Store "raw writer/thread" becomes "evidence root"; `RawFactory` becomes `EvidenceRoot`; "operational raw/journal access" becomes "operational journal access" |
| RT §6 sketch (`:607`, `:619-622`, `:636`, `:644-646`) | `RuntimeResources { evidence: EvidenceRoot, journal: ProcessJournal }`; `into_wire_parts(self) -> (EvidenceRoot, ProcessJournal)`; delete `RawFactory` and its `open`; the raw-factory text follows |
| RT §6.1 (`:810-811`, `:820-823`) | the two `raw/` lines become "`evidence/<session-id>/<turn>/  stderr.log, undecoded.bin, final_text.txt`"; "raw/index format and durability" becomes "the evidence folder"; "raw/blob" becomes "evidence/blob" |
| RT §6.2, §7 (`:867-869`, `:882`, `:891`, `:900`, `:930-931`, `:972`, `:980`) | delete the raw-log drain, raw completeness, raw sync, raw thread and raw append-failure clauses |
| RT §11 (`:1310-1315`, `:1319`, `:1323`, `:1338`) | delete the seams `raw.sync.fail_persistent`, `raw.before_sync`, `raw.index.torn_tail` and "raw loss explicit"; the splitter property becomes "exact messages or explicit failure"; drop `s1_raw_` and "raw logs" |
| CONTEXT **Raw log** (`:70-71`) | becomes "**Evidence folder**: One folder per turn under VIA's state directory, holding the agent's stderr, the message VIA failed to decode (if any) and a final text too large for the envelope. VIA keeps no copy of vendor traffic; the agent's own transcript keeps the conversation." |
| CONTEXT **Store** (`:97`), **Wire** (`:117`) | "events (the JSON body plus a pointer into the raw log)" becomes "events, step rows"; "Raw vendor bytes stay in raw log files." becomes "Evidence files live in the evidence folder."; "and writes the raw log per connection" becomes "and creates the turn's evidence folder" |
| `.repo-context/coding-style.md` (`:118-123`, `:143`, `:205`, `:217`, `:274`) | "Raw-log staging is bounded … after a gap." becomes "The message queue is bounded in messages and bytes. If the consumer cannot keep up, the connection fails: Host supervises the process and the reader keeps draining, discarding, during cleanup. Never report a turn as fully observed after a lost message."; "flush raw logs and Store records" becomes "flush Store records"; the others say evidence folders |
| D4 (`docs/brainstorms/README.md:422`) | a historical record, not edited; R8 records its reversal |
| Void | T3 §7.2 row 6 (raw failure); `claude-code.md:239`, `:298`, `:303`, `:306`, `:400-401`, `:419` drop their raw clauses; Codex and OpenCode raw text is their tasks' (R8) |

> One task drains stdout. It never waits for Route, Core or SQLite. It
> splits bytes into vendor messages at LF, at most 1 MiB each including LF,
> and queues each with a nonblocking send; a full queue fails the connection
> `overflow`. At the cap without LF it fails `MessageTooLarge`. After a
> failure it reads to EOF in 64 KiB units and discards them, counting the
> bytes, so the vendor never blocks on a full pipe. The splitter retains
> split UTF-8 without interpreting it; only Route decodes UTF-8/JSON. EOF
> with an unfinished message is the in-band end `Unterminated`.
>
> VIA keeps no copy of vendor traffic (T4 requirements R8). Each submitted
> turn has an evidence folder, `<state>/evidence/<session_id>/<turn>/`. The
> vendor's stderr is the file `stderr.log` there: Host opens it and gives
> it to the anchor as stderr, the vendor inherits it, and the operating
> system writes it; no VIA task reads it. When Route cannot decode a
> message, and when a message exceeds 1 MiB or ends unterminated, Wire
> writes its first 64 KiB to `undecoded.bin`, and the turn's failure names
> the file and the message's length. A final text too long for the
> envelope is written there as `final_text.txt` (C1 §5). The vendor's stderr
> is not capped. The vendor's own transcript keeps the
> conversation; SQLite keeps its path as a hint with the vendor session ID.

**T4-A42. Disk: free-space floor, size warning, WAL refusal** [t4r16.3].
Withdraws A36's §6 text. RT §6 (`:769-777`), from "Checkpoint after 8 MiB
WAL growth" to "never shortened by quota pressure.", becomes:

> Checkpoint after 8 MiB of WAL growth or 1000 commits, both configurable.
> At a 32 MiB WAL (configurable) refuse ordinary writes by name as known not
> committed and retry a truncating checkpoint at most once a second, while
> lifecycle and terminal writes, and running turns' step rows, still commit
> [t4r16.7.6]; this is not a Store health
> failure. The commit that crosses the limit completes, so the WAL may exceed
> it by one transaction. Keep read transactions short (one bounded page); no
> read holds a transaction open while waiting on a socket. No automatic
> retention/pruning in S1. There are no disk size budgets. New work (a
> receipt, a queued turn's dispatch) is refused by name while free space on
> the state directory's filesystem is below a configurable floor, 5 GiB by
> default; running turns, lifecycle and terminal writes proceed.
> `daemon/status` warns when VIA's data exceeds a configurable size, 2 GiB
> by default. An actual `SQLITE_FULL` or `ENOSPC` rolls back as known not
> committed; only a failed rollback is uncertain (§7). Key/receipt lifetime
> is never shortened by disk pressure.

**T4-A43. Memory bounded by construction** [t4r16.2]. Withdraws A36's §8
text.

| Location | Edit |
|---|---|
| RT §8 (`:1041-1044`) | from "Use byte-permit wrappers" to "a peer size or item count." becomes "Every buffer has a fixed maximum and every kind of holder a fixed count; nothing is preallocated from an unchecked peer size or item count." |
| RT §8 (`:1057-1066`) | from "The global daemon retained-payload allocation budget is 128 MiB" to "not silently enlarging the limit." becomes the text below |
| RT §8 (`:1052-1056`, `:1067-1070`) | keep each anchor's 32 MiB and delete "combined daemon plus four anchors must remain below 384 MiB"; "The Codex 16 MiB permit is a sub-budget …" to "retain the 256 MiB RSS target." becomes "The Codex shared server's lanes and tool metadata are fixed buffers counted per server by the Codex task, which measures 32 loaded leases and four active turns against the RSS gate." |
| RT §8 rows | delete "Raw staging" (`:1011`); "Codex shared Route ingress" (`:1013`) "16 MiB global permit for Codex lanes/tool metadata" becomes "fixed per-server buffers (the Codex task)"; "OpenCode HTTP/SSE transport metadata" (`:1014`) "Existing bounded Wire raw/message-splitting and global retained-payload permits" becomes "Existing bounded Wire message splitting"; "Store requests" (`:1019`) "raw/Host cleanup never await this queue" becomes "Host cleanup never awaits this queue"; "Vendor stdout message" (`:1009`) becomes "Fail connection; the first 64 KiB saved as evidence" |
| RT §8 (`:1074`); vendor specs | "redacted raw capture and bounded staging" becomes "bounded staging"; `codex.md:237`, `:261`, `:315` and `claude-code.md:303` drop the 16 MiB permit and raw staging |

> There is no memory pool, byte counter or memory setting. The daemon's
> worst case is the sum over holders of each holder's fixed buffers times
> its fixed count (C1 sockets, connections, running turns, the Store), about
> 332 MiB estimated. F24 drives every holder to its maximum at once and
> records RSS at 10 ms intervals; the daemon's peak RSS less its idle
> baseline must stay within that sum plus a 25% margin for allocator
> overhead and CI variance, and growth must stay below 32 MiB after the
> first 64 MiB of a 256 MiB flood. RSS is an empirical gate, not a
> mathematical bound. Failure of either assertion requires correction or
> explicit design review, not silently enlarging the limit.

**T4-A44. C2 without `raw_ref`** [t4r16.1].

| Location | Edit |
|---|---|
| C2 §4 (`:287-288`) | "Each observation carries `raw_ref: Option<RawRef>` and `at: Instant` (Core records wall time)." becomes "Each observation carries `at: Instant` (Core records wall time)." |
| C2 §7 item 7 (`:452`) | becomes "7. A decode failure saves the message to the turn's evidence folder before the route fails `protocol`." |
| C2 §2 (`:90-110`, `:156`, `:159`) | `RawFactory` becomes `EvidenceRoot`; "creates each connection's raw writer" becomes "creates each turn's evidence folder"; delete "RawWriter, RawFactory", "raw-handle" and "raw/terminal commits" to "terminal commits"; `InterruptReport` and `DriverHealth` lose `evidence: Option<RawRef>` |
| C2 §3 (`:263`), summary (`:46`) | "raw log append" and "raw tap" become "evidence files" |
| Left to the Codex task (R8) | C2 §4 `:299`, `:307-308` |
| Code | `crates/via-adapters/src/lib.rs:120`, `:142`, `:179`, `:230`, `:245`; `crates/via-routes/src/lib.rs:193`, `:306`, `:547`; `crates/via-core/src/api.rs:1205-1214`, `:1329`; `crates/via-core/src/engine/drive.rs:1212-1221`, `:1429-1454`, `:1630-1688` |

**T4-A45. Daemon log `via.log`** [t4r16.7.1].

| Location | Edit |
|---|---|
| RT §6.1 layout (`:805-812`) | after `store.lock` add "`via.log                    daemon warnings and errors; via.log.1 after rotation at start past 10 MiB`" |
| RT §7 (`:914-915`) | "one bounded JSON line on the daemon's stderr" becomes "one bounded JSON line in `via.log` (§6.1); the daemon writes stderr only while it starts, so an auto-starting CLI can report a failed start" |

## 13. Tests (failure-first)

Each test is written first, fails on the code as found for the stated reason,
and passes once the mechanism lands. Names follow runtime §11 (`s1_fNN_`,
`s1_bounds_`, `s1_store_`, `s1_blob_`, `s1_wire_`, `s1_c1_`, `s1_progress_`,
`s1_evidence_`). Coding-style's testing rules apply (synchronization points,
no fixed sleeps as ordering; seeded inputs via `VIA_TEST_SEED`). Time rules
run with lowered seams, in-process units on a paused clock; heavy tests
(floods, maximal lines, 32 sockets) compile only under `test-failpoints`;
every daemon scenario runs through `run_scenario` with an `Evidence`
(`crates/via-cli/tests/support/scenario.rs:111` [V]).

### 13.1 Seams

Under `#[cfg(feature = "test-failpoints")]`, added to
`scripts/check-release-features.py`'s `POINTS`: `store.writer.before_serve`,
`Lanes::peak(lane)`, `store.commit.step`, `core.observations.pause`,
`core.progress.publish`, `Store::read_count()`, `store.read.delay_ms`,
`store.rollback.fail`, `store.statvfs.free_bytes`, `core.data_size.walks`,
`prompt_file.copy.pause`, `blob.write.fail_after`,
`Store::blob_chunk_reads()`, `VIA_TEST_EVENT_STALL_MS`,
`VIA_TEST_PARTIAL_LINE_MS`, `VIA_TEST_REPLY_WRITE_MS`,
`VIA_TEST_FINAL_TEXT_FILE_MAX`, `final_text.write.fail`,
`wire::fallback_drops()`, fake-agent steps `HoldStdin`, `ReportCwd`,
`EchoPromptDigest`, `Stderr { bytes }` (`Emit`, `Gate` and `Flood`,
`crates/via-fake-agent/src/main.rs:41-62` [V], already emit any line), and
`/proc/<pid>/status` sampling. Disk and WAL thresholds are lowered through
`daemon.json`.

### 13.2 Scenarios

| Test | Proves |
|---|---|
| `s1_progress_step_rule_counts_output_after_tool_results` | fake: text, tool_started, tool_ended, text, text, tool_started, tool_ended, text → `current_step` 3; rows 1–2 before the terminal, row 3 in it (`store.commit.step` barriers); envelope `steps` `null` |
| `s1_progress_snapshot_adds_no_store_read`; `s1_c1_status_latency_under_bounded_store_delay` | `status` on a running turn and on an idle session make the same number of Store reads; with `store.read.delay_ms` = 200 it answers within 300 ms while the turn progresses (Q-R5-5) |
| `s1_c1_status_progress_only_for_the_selected_turn` [t4r16.5.2] | with turn 2 running, `status --turn 1` has `progress: null` and turn 1's rows; `status` has turn 2's `progress`, whose `current_step` has no row; a step ended after the Store read (`core.progress.publish` held) appears on the next call |
| `s1_progress_tokens_sum_per_step_and_label_scope` | two keyless samples in one step supersede, steps add; `tokens.scope` is the fake's declared scope |
| `s1_progress_tools_overflow_and_untracked_end_count` | 70 concurrent tool starts: 64 names and `tools_overflow`; an untracked end then model output advances `current_step` and writes a row |
| `s1_progress_unknown_messages_send_no_observation` | a flood of unknown messages sends no C2 item yet moves `last_activity_at` |
| `s1_progress_step_rows_survive_crash_to_last_commit` | killed after row 2's commit, before row 3: restart shows rows 1–2, turn `unknown` |
| `s1_progress_step_commit_refused_rows_ride_in_terminal` | known failure on row 2: `failed(store)`; rows 2, 3 and the open row commit with `turn.ended` |
| `s1_progress_many_steps_all_have_rows` [t4r16.5.6]; `s1_progress_forced_shutdown_terminal_carries_open_row` | 2,000 fake steps give 2,000 rows, more than a page; a turn forced by final shutdown in step 3 commits row 3 with its `turn.ended` |
| `s1_store_steps_delete_is_one_keyed_range` | `EXPLAIN QUERY PLAN` uses the primary key; of two interleaved sessions one is deleted, the other intact |
| `s1_c1_status_every_member_after_eviction_and_restart`; `s1_c1_status_alive_false_after_exit_before_control_drop` | durable members equal before and after; `progress` null after restart; exit observed with the control upgradeable → `alive` false |
| `s1_c1_events_page_filters_and_bounds`; `s1_c1_follow_and_unsubscribe_are_refused` | window, `types`, `turn`, `next_after` across filtered rows, `more`, the byte bound, no `raw_ref`; `follow: true` is `invalid_params`, `unsubscribe` `method_not_found` |
| `s1_c1_wait_checks_each_second_and_32_waiters_leave_status_served` [t4r16.7.7] | a waiter's reads on a paused clock are at 0 s, 1 s, 2 s, …; the end is seen within 1 s; 31 sockets waiting and one polling `status` all answer; a 33rd socket is closed without bytes |
| `s1_evidence_stderr_is_written_by_the_os_and_listed` [t4r16.1] | the fake writes 1 MiB to stderr: `stderr.log` holds exactly those bytes, idle was not reset; `logs` lists it with its size, `folder` absolute, `transcript` and `vendor_session_id` `null`; the envelope's `evidence` equals it |
| `s1_evidence_undecoded_message_is_saved_and_named` | a malformed known message of 200 KiB, and a 2 MiB line: `failed(protocol)` and `failed(overflow)`, each `undecoded.bin` holds the first 64 KiB, and `failure.message` names the file (and the length, when known) |
| `s1_evidence_folder_failure_fails_store_before_launch` | a pre-created `<turn>` folder: `failed(store)`, no anchor intent, no process |
| `s1_c1_logs_selects_the_turn_and_never_reads_files` | a session address selects the running turn, else the latest submitted; a queued turn has `folder: null`; an unreadable file still lists with its size |
| `s1_store_disk_floor_refuses_new_work_only` [t4r16.3] | free space below a lowered floor: `spawn` and `resume` are `admission_refused` `disk_free_floor` with no write; a queued turn fails `store` at dispatch; a running turn ends normally with its rows; a close and a queued-turn cancel commit; `below_free_floor` shows; a daemon started below the floor serves reads |
| `s1_store_data_size_warning_is_cached` | lowered `warn_size` gives `over_warn_size`; `data_bytes` equals the summed apparent lengths; a second call within 60 s walks nothing |
| `s1_store_full_disk_rolls_back_known` | `SQLITE_FULL` on an ordinary commit and on a terminal: rolled back, `NotCommitted`, no latch; `store.rollback.fail` latches |
| `s1_store_wal_limit_refuses_without_latch` [t4r16.3, t4r16.7.6] | lowered `wal.max` and an external reader holding a snapshot: a spawn is `store_error` `not_committed` `wal_full`; a running turn's step rows commit and it ends normally; a running turn's `warning` event is refused and fails it `store` while its terminal commits; a close commits; health stays healthy; the reader closes, a write after 1 s retries `TRUNCATE` and writes resume; WAL growth of a maximal batch and of a terminal with many rows is recorded for `via-d9o.2.3` (Q-R9-1) |
| `s1_c1_request_too_large_is_named_then_closes` [t4r16.2] | 1 MiB + 1 bytes: `request_too_large`, then close; exactly 1 MiB is served; a partial line times out alone |
| `s1_c1_prompt_file_copies_hashes_and_refuses_changes` | a 3 MiB file: the prompt is a blob with a matching SHA-256 (`EchoPromptDigest`); a keyed retry with the same content returns the stored receipt, changed content is `idempotency_conflict`; an append during `prompt_file.copy.pause` is `changed`, no blob left; a FIFO, a directory, a relative path, 16 MiB + 1 bytes and invalid UTF-8 are each refused by reason |
| `s1_c1_list_creation_order_and_last_active` [t4r16.4] | 250 sessions page newest first with no repeats while states change; a session created mid-scan never appears; `last_active_at` is the latest event's time and `since` filters on it; `l2.` is `invalid_params`; a filter matching one old session gives empty pages with a cursor, then it |
| `s1_c1_request_id_over_256_bytes_is_invalid_request`; `s1_c1_reply_not_read_closes_the_socket` | A31; A32, including a peer that never reads the first byte [t4r16.5.5] |
| `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` [t4r16.2, t4r16.5.6] | every §5.1 holder at its maximum at once (four turns with 16 MiB prompts flooding maximal messages and filling observations; 32 sockets sending maximal lines with 65,536-node lists and reading pages): peak RSS less the idle baseline ≤ 1.25 × the §5.1 sum; growth < 32 MiB after the first 64 MiB of a 256 MiB flood; each anchor ≤ 32 MiB; `daemon/status`, `status` and `cancel` of another turn answer within 100 ms, also with its interrupt blocked at `HoldStdin`; the flood turn ends `failed(overflow)` |
| `s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output` | Core held at `core.observations.pause`, vendor silent: `overflow` at the lowered stall |
| `s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib` | the C2 bounds |
| `s1_bounds_final_text_spills_to_a_file` [t4r16.7.8] | a text of exactly 256 KiB encoded is inline; one more byte puts the exact text in `final_text.txt`, with `final_text: null` and `final_text_file {path, bytes, truncated: false}`; past a lowered file cap the file ends at a character boundary with `truncated: true`; `final_text.write.fail` also gives `truncated: true`; every such turn ends `completed` |
| `s1_bounds_envelope_at_every_member_maximum_fits_1_mib` [t4r16.7.8] | every §6.4 member at its maximum, with 1,500 denials and 1,500 declines whose targets are 64 KiB: the envelope encodes within 1 MiB; each list holds 1,000 entries of at most 256 B citing their events, and each total is 1,500; a `bound` of 32 KiB + 1, a `vendor` of 16 KiB + 1 and a `model` of 1 KiB + 1 are `invalid_params` at receipt; a 1 KiB + 1 vendor short field is `protocol` |
| `s1_bounds_final_text_piece_fits_256_kib` | a text of six-byte escapes is cut into pieces each at most 256 KiB encoded that concatenate to the text |
| `s1_config_is_read_at_start_validated_and_reported` [t4r16.3] | no file gives the defaults; lowered values apply only after a restart; an unknown key (a `memory` object included), a duplicate key, `wal.max` ≤ `checkpoint_bytes`, `checkpoint_bytes` 0 or 4095, a value past 2^62 and a group-writable file each exit 78 naming key and rule, touching no Store or socket; `limits` equals the effective values |
| `s1_daemon_log_after_startup_and_rotation` [t4r16.7.1] | an invalid `daemon.json` is reported by the auto-starting CLI from stderr; after a start that recovered a turn, the recovery warning is in `via.log` with its `session` and `turn`; a later warning is in `via.log` and not on stderr; the shutdown summary is the last line of `via.log`; a `via.log` of 10 MiB + 1 byte becomes `via.log.1` at the next start |
| `s1_f05_…` (oversize, depth and nodes, partial line, 33rd socket) | F5 |
| `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_messages` | F27, seeded splitter: every complete message reaches Route byte-exact; a huge line fails `MessageTooLarge` with its prefix saved |
| `s1_store_…`, `s1_wire_…`, `s1_blob_…` | death guard, lanes and fence, Latch fit with the largest `cwd`, Public saturation at the Store level, `finish` joins and hands off stragglers, torn and mismatched blobs, replay compare outside `admission` with a stalled reader |

## 14. Task 4 scope disposition (`t4/t0.md`)

Kept: F5 (§10), F24 (§5.1, §9, §13), F27 (§8.2); the C1 methods, `serve
--stdio` and CLI options (§4, §11); the per-pipe Wire reader (§8.2); JSON
limits before a `Value` (§10.2); A2, A3, A9; `daemon/status` (§11.2); the
1024 / 4 MiB observation budget on the reduced set (§2.3); strict paging
DTOs; blob path, schema v6, Store shutdown ordering. Memory is bounded by
construction (§5.1); runtime §6's quota becomes a free-space floor (§5.3,
A42); thresholds are daemon config (§5.5, A37; `via-jm4.7.8.1`). Obsolete:
F25, F26 (Q-R5-5), `events` follow, `unsubscribe`, follower cleanup (A25),
Wire `read_either`, the raw log and its `logs` excerpts (R8). Out of scope:
durable `output_schema` (§0).

## 15. Limitations

| Limitation | Revisit when |
|---|---|
| Memory has no enforced ceiling; §5.1's ≈ 332 MiB is an estimate the RSS gate measures, most of it 32 hostile maximal requests | the gate fails, or routes need more sockets or larger messages |
| A vendor message over 1 MiB fails its turn; a Claude image or large tool result may be one **[U]** | the Claude probe; raising the cap costs as §5.1 states |
| Agent stderr is uncapped; a vendor, or a process that escaped its group, can fill the disk, and the floor then stops only new work [t4r16.7.5] | measured stderr sizes (§16) |
| A final text over 64 MiB is cut in its file; list entries past the first 1,000 are only in the events | measured sizes (§16) |
| `via.log` grows without bound between daemon starts | its measured size (§16) |
| Below the floor a queued turn fails `store` rather than waits | callers need queued work to survive a full disk |
| At `wal.max` a running turn's step rows commit, but its other durable events are refused and fail it `store` | `via-d9o.2.3` measures the WAL policy |
| `data_bytes` costs one directory walk per minute, linear in evidence files | retention (`via-jm4.18`), or the walk is slow |
| Step counts and tokens are VIA's and unproven for Claude, Codex and OpenCode until their probes (§2.5), so `tokens` may be `null`; `running_tools` lists at most 64 names | each vendor's probe; the owner's accuracy decision (Q-R5-11) |
| Transcript paths follow each vendor's internal layout; a deleted transcript loses the conversation (R8) | each vendor task |
| Reusable connections and their evidence are not designed here | `via-4sw.3.2` and the Codex task |
| The open-session tally is exact only until an uncertain close; `ord` increases strictly only while no session row is deleted; blob verification at start is linear in blob bytes | Store-failure recovery work; retention |
| `revision` is 0; `process.idle_since` is `null` (A16) | late evidence; vendor idle shutdown |
| Inferred: the §6.2 lifecycle count, the terminal and failure-batch sizes and the envelope maxima, `bound.effective` included (§6.4), the 128 B step row, the §5.1 sizes, `journal_size_limit` behaviour, and that `json_limits::scan` and serde_json agree on token boundaries | the checking test fails |

## 16. Measurement list (`via-d9o.2.3`) [t4r16.7.9]

Each case below got the simplest behaviour because it has not been
observed. `via-d9o.2.3` measures it end to end with every adapter, and
hardening follows the data.

| Measure | Against | Section |
|---|---|---|
| Daemon RSS, peak less idle baseline | 1.25 × the §5.1 sum (Q-R16-1, accepted) | §5.1, §13.2 |
| Agent `stderr.log` sizes | uncapped | §7.5 |
| The largest vendor message | the 1 MiB per-message cap | §5.1 |
| Final text sizes; how often `final_text.txt` is used and truncated | 256 KiB inline, 64 MiB file | §6.4 |
| Denied and declined list lengths | the first 1,000 entries | §6.4 |
| Disk growth per turn and session; WAL growth of the largest transactions | the free-space floor, the size warning, `wal.max` plus one transaction (Q-R9-1) | §5.3, §5.4 |
| Behaviour at the floor: refused receipts, queued turns failed at dispatch | the §5.3 rules | §5.3 |
| Step counts and tokens per vendor, with tool-only and parallel-tool steps | the vendor's count and usage | §2.4, §2.5 |
| `via.log` size between daemon starts | rotation at start past 10 MiB | §7.6 |
