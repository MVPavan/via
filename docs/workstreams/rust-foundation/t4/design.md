# Task 4 design: events, progress, storage and C1 conformance (round 7)

Status: normative design for Task 4 (Bead `via-jm4.7.8`, step T4-0), round 7.
It replaces rounds 1–4. Rounds 6 and 7 apply the orchestrator's decisions on
Sol's round-5 and round-6 reviews (`design-r5-decisions.md`,
`design-r6-decisions.md`); each change is tagged `[t4r5.N]` or `[t4r6.N]`
with the decision number. It is written against the owner-approved
[requirements](requirements.md) (R1–R7), which are normative and override
earlier design assumptions and spec text; every such conflict is a numbered
amendment in §11. The round history and per-decision maps are in
[reports/T4-0.md](reports/T4-0.md) §9–§11. This step writes no code and no
tests. Slices are re-planned after the owner reviews this design;
`s1.md`–`s7.md` are superseded.

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
| Route-side attribution of shared and multi-turn connections | Task 4 builds the schema, the rule and the `logs` read (§4.4, §6.7) [t4r5.7]; the Codex and OpenCode Routes that write their attribution are not in S1 | their adapter slices, with no migration |

## 1. Owners, lock order and wakes

| State | Owner | Created by | Written by | Ended by | Bound |
|---|---|---|---|---|---|
| Global memory budget (`MemoryBudget`, five partition semaphores summing to 128 MiB, SQLite's cache one of them [t4r6.2]) | via-store | `Store::open` | each acquirer (§5.2) | last `Arc` after Store threads join | 128 MiB |
| Store request lanes (Latch, Lifecycle, Internal, Public) | `Store`; served by the SQLite thread | `Store::open` | `StoreClient` handles by lane tag | fence, or writer death | §6.1 |
| Raw inbox, raw worker, blob files | Store raw worker | `Store::open` | `RawWriter`, `BlobWriter`, `BlobReader` | fence, then join; death guard | §6.4, §6.6 |
| `ConnectionLatch` | via-wire, per connection | `open_connection` | readers, stdin writer, raw worker via `RawFaultSink` | last holder after `finish` | §7.4 |
| Reader and stdin-writer tasks | `WireMessages` (unique) | `open_connection` | the tasks | `finish` under one deadline, then adoption | §7.6 |
| Message queue | `WireMessages` | `open_connection` | stdout reader | `next_message`, or `finish` | 64 messages, 4 MiB |
| Route → Adapter hop | Adapter `execute` | per drive | Route | end of `execute` | 1 message |
| Observation channel and its byte semaphore | Core drive | per drive | Adapter | end of the drive | 1024 items, 4 MiB |
| Stall timer | Adapter pending delivery | first blocked send | Adapter | acceptance of that item, or 10 s | one per drive |
| Drive reserve | Core drive | dispatch, before submission | — | end of the drive | 11.5 MiB (§5.4) |
| Envelope arena and meter [t4r6.4] | Core drive (`TurnRecord`) | submission | the drive, per accumulated item | the terminal commit | `ENVELOPE_MAX` (§6.5) |
| Turn activity clock (`TurnActivity`, one `AtomicU64`) [t4r5.11] | Core drive; a clone in the `Running` entry | submission | the Adapter, for each vendor message attributed to the turn | end of the drive | 8 B |
| Step tracker | Core drive (`TurnRecord`) | submission | the drive | the terminal commit | §2.4 |
| Published progress | `Slot` `Running` entry (`crates/via-core/src/engine/queue.rs:274` [V]) | `Running` creation (`:488` [V]) | the session's drive only | `finish_running` (`:593` [V]) | §2.4 |
| Socket admission | daemon accept loop | daemon start | accept loop | daemon end | 32 |
| `started_at` | `Engine` | `Engine::open` | never | never | |
| Open-session tally, closing set | `Engine` (`Sessions`) | `Engine::open`, seeded from Store | receipt and closed-now answers | never | §10.3 |
| Live `Armed` controls | Host ledger (`crates/via-host/src/host.rs:72` [V]) | control verification | `Capacity::armed` | control drop | one per anchor |
| Disk ledger (`DiskLedger`, one mutex-guarded ledger) [t4r6.3] | Store | `Store::open`, from the files on disk | the SQLite thread and the raw worker: reserve before each write, settle after | never | 4 GiB, 16 MiB lifecycle reserve, turn holds (§6.9) |
| Raw attribution spans [t4r5.7] | Store (`raw_spans` table); the pending batch is the Route's | the connection's Route | the SQLite thread, by a validated `CommitSpans` [t4r6.12] | never in S1 | 128 spans in memory (§4.4) |
| Schema v6 | Store | its owning transaction | that transaction | never in S1 | §6.7 |

**Lock order.** T3 §1 stands: `admission` (async) → `sessions` → slot state;
`Head` as T3 orders it; `stop` alone; no std mutex across an `.await`. An
async owner's lock may briefly take the `Lanes` mutex; the reverse is
forbidden. Code holding `Lanes` or the raw-inbox mutex takes no other lock,
awaits nothing and runs no callback; replies complete after release.
`Sessions` and `Unresolved` are leaf std mutexes, taken in that order by
`Engine::counts()` only. Publishing and reading progress take only the slot
state std mutex, briefly, with no await (§2.4). `MemoryBudget` needs no lock;
`DiskLedger`'s mutex is a leaf held only for arithmetic [t4r6.3].

**Wakes** (a wake is a hint; the receiver re-reads the owning state):

| Wake | Producer | Consumer |
|---|---|---|
| Force stop or Store latch | `Engine::force_signal()` (`crates/via-core/src/engine/latch.rs:446` [V]) | Route, Adapter |
| Wire failure, EOF or exit | `ConnectionLatch` watch | Route, `next_message`, `finish` |
| Stop order changed, or `force_at` reached | Route's wake (`crates/via-routes/src/runtime.rs:516` [V]) | Route's `select!` |
| Stall | the Adapter drops the stalled session's hop receiver | Route, through `Sender::closed()` (§2.3) |
| Envelope overrun | Core's stop order, cause `overflow` (A34) | Route's wake, as any stop order |
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
  (`final_text`, `stop_reason`, `vendor_code`, usage, structured output),
  including text a vendor mapping accumulates into final text (§2.3
  `final_text`) [t4r6.4];
- the payload fields of the durable observations of §2.1 (`action.denied`,
  `vendor.request_declined`, `steer.delivered`, `warning`).

Everything else is skipped with `serde::de::IgnoredAny`, so other text,
reasoning, tool inputs and tool outputs are never copied (R2: VIA does not
interpret tool inputs or outputs). Rules, each enforced in Route's decode:

1. **Short fields.** An ID, tool name, type tag, `stop_reason` or
   `vendor_code` is at most `SHORT_FIELD_MAX` = 1 KiB. A known message with a
   longer one is a protocol failure citing its raw ref (C2 §1 rule 6: a
   malformed known message). This bounds the terminal extras and the
   vendor-derived members of the failure summary (§6.5).
2. **Bounded lists.** A `Vec` field has an element cap of
   `LIST_FIELD_MAX` = 256, enforced by a bounded sequence visitor that
   allocates the `Vec` with capacity 256 before the first element, so it never
   grows [t4r5.3]; more is a protocol failure. So a decoded message retains
   at most its length plus 64 KiB of headers.
3. **Durable payloads** (`action.denied` target, declined summary) keep C2's
   256 KiB encoded cap; over it is a protocol failure with raw evidence.
   Text splitting is gone: a `final_text` piece or terminal text is bounded
   by its vendor message (1 MiB) and metered by the envelope [t4r6.4].
4. **Structure limits first.** Before the typed decode, `json_limits::scan`
   (§9.2) enforces depth 64, 65,536 nodes and at most 4 KiB for an object key
   that contains an escape; an excess is a protocol failure. The scan
   allocates nothing.
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
6. **No serde scratch for values** [t4r5.3]. A peer-fed string field is a
   `JsonStr<'a>` (a borrowed `&'a RawValue` checked to be a JSON string), never
   a `String`. `JsonStr::to_string_exact` first counts the decoded length (one
   pass over the escapes, no allocation), takes the charge for exactly that
   many bytes, then allocates a `String` of that capacity and unescapes into
   it: capacity equals decoded length equals charge [t4r6.1]. serde_json
   touches its scratch buffer only for escaped object keys (at most 4 KiB by
   rule 4; `serde_json-1.0.151/src/de.rs:2215-2221`, MapKey `parse_str`) and
   for `ignore_value`'s nesting stack, one byte per level (at most 64 after
   rule 4; `de.rs:1102-1103`) [V].

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
| **`final_text { key, text, replace }`** (new) [t4r6.4] | append to, or replace, that key's final text in the envelope arena, metered (§6.5); sent only by a route whose final text spans vendor messages (Codex, OpenCode). A `turn.vendor_terminal` whose `final_text` is present replaces the whole text, metered the same way |

One vendor message yields at most one `progress` item, and only when it
carries a mark (model output, a tool start or end, or a usage sample), plus
its other observations. An unknown message, or one that the Route cannot
attribute to a turn, produces no observation at all [t4r5.11]: it is in the
raw log, and it moves the turn's activity clock only when attributed
(§2.4). A `progress` item carries its vendor turn ID like every
observation; one attributed to a turn that is already terminal is late and
dropped (the raw log holds it).

**Bounds** (C2 A1, unchanged numbers):

- `mpsc::channel(1024)` plus a per-drive `Semaphore` of 4 MiB. An item is
  charged `512 + Σ(64 + len(s))` over its `String` fields, acquired by the
  Adapter before it builds the item and carried in it until Core has handled
  it. Proof: every `String` in an item is built by `to_string_exact` (§2.2
  rule 6) or from a slice, so its capacity equals its decoded length, plus a
  24 B header; the fixed struct is under 512 B [t4r6.1].
- The Route → Adapter hop shrinks from `mpsc(64)` to `mpsc(1)`
  (`crates/via-adapters/src/runtime.rs:132` [V]). Its memory is part of the
  drive reserve (§5.4), not charged per item. Route blocking on the hop stops
  it calling `next_message`; the Wire queue then fills and fails
  `overflow` (runtime §8 "Route message staging"), which is the contract's
  answer to a burst.

**Stall** (C2 A1: 10 s without drain) [t4r5.8]:

- Owner: the Adapter's pending delivery. The timer is one absolute deadline,
  `first_block + EVENT_STALL` (10 s, a Core constant passed in), restarted
  only when that item is accepted.
- On expiry the Adapter drops the stalled session's hop receiver. Route
  selects on `hop.closed()` in every wait (§8), so it sees the closure at
  once. What it then does depends on what the hop fed:
  - **Private route** (one hop per connection; the fake, Claude, OpenCode's
    per-session server): Route fails `RouteError::Overflow` and force-closes
    the connection (for a private process, Host stops the group: the
    interrupt). Core disposes `Overflow` as today
    (`crates/via-core/src/engine/terminal.rs:97` [V]: `failed`, `overflow`).
  - **Shared route** (Codex's shared stdio server: one hop per thread
    generation): Route quarantines that thread generation exactly as C2 §4
    specifies for an ingress-lane overflow, whose last sentence already makes
    the 10 s timer lead "to the same quarantine": sticky overflow health for
    that generation, its data still read and raw-logged but no longer
    forwarded, Core resolving every nonterminal turn of that generation
    `failed(overflow)` and interrupting through reserved control, same-thread
    dispatch closed until a clean reopen. Other threads and reserved control
    continue. S1 has no shared route; the Codex slice builds this arm.
- [V] Today the Adapter already drops the receiver when a delivery wait
  fails (`crates/via-adapters/src/runtime.rs:146-153`) and Route reports the
  closed hop as `Overflow` (`crates/via-routes/src/runtime.rs:755`); what is
  new is the 10 s bound (today the wait is bounded only by the turn
  deadline, `crates/via-adapters/src/runtime.rs:322`) and the `closed()` arm,
  so Route need not wait for more vendor output.

**Envelope overrun** [t4r5.5, t4r6.4]: the envelope's accumulation is
bounded directly (§6.5); there is no separate event-payload cap. Core meters
every accumulating item as it handles it: a committed `action.denied` or
`vendor.request_declined` entry and every `final_text` append or
replacement. On the item that would take the measured envelope over
`ENVELOPE_MAX`, Core records the overrun and sends the turn's stop order with
cause `overflow` and `force_at = now` (A34), the path T3 uses for cause
`store`. Route acts on it through its wake; nothing depends on the Adapter
sending again. Disk use by the events themselves is bounded by the quota
(§6.9).

### 2.4 Progress snapshot (R3)

**Owners.**

- The **step tracker** is per turn state in the drive's `TurnRecord`. The
  drive creates it at submission, writes it for each `progress` item and at
  acceptance and at the terminal, and drops it with the record. It is the
  only place the step rule runs.
- The **published progress** is a copy held in the session `Slot`'s existing
  `Running` entry (`crates/via-core/src/engine/queue.rs:274` [V]), which
  exists exactly while the turn runs (`start_running` `:488`, cleared by
  `finish_running` `:593` [V]). The drive publishes after each `progress`
  item with `Slot::publish_progress(turn, &ProgressDelta)` under the slot
  state mutex (a leaf, no await), writing only the changed fields in place;
  the name list is touched only on a tool start or end or a step boundary.
  This carries the value in the existing owner of "the running turn", with
  no new watch or task.
- **Activity clock** [t4r5.11]. `last_activity_at` changes on every
  attributed vendor message, including unknown types, which send no
  observation. The drive creates one `TurnActivity` (an `Arc<AtomicU64>`
  holding milliseconds since the drive's base instant) per turn, puts a clone
  in the `Running` entry, and passes one to the Adapter in `execute`. The
  Adapter stores the arrival instant of each vendor message it attributes to
  the turn (`Relaxed`; one writer); `status` loads it and converts with Core's
  clock. It ends with the drive. It carries no data, so it cannot queue or
  stall.
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
| `phase` | `tools` while the open set is non-empty or `tools_overflow` is true, else `model` [t4r6.9] |
| `running_tools` | names in the open set, at most 64 |
| `tools_overflow` | true when a tool started beyond the 64 tracked since the last step boundary [t4r6.9] |
| `last_activity_at` | arrival time of the last vendor message attributed to the turn (any type) [t4r5.11] |
| `tokens` | `{total, scope}` of completed steps, or `null` before any sample |

**The step rule** (one reducer for every vendor; the Adapter only classifies
each message into marks):

1. At `turn.accepted`: `current_step = 1`, step start = now,
   `results_since_output = false`.
2. `model` (the message is model output: text, reasoning or a tool request):
   if `results_since_output`, this is a **step boundary**: the current step
   ends now, `current_step += 1`, the new step starts now, the flag clears,
   and the open set and `tools_overflow` are cleared [t4r6.9]. A
   message's `model` mark is applied before its tool starts, so a tool
   requested in the boundary message stays open.
3. `tools_started` entries `(id, name)`: add to the open set if it has fewer
   than 64 entries and the id is not already in it; a new id beyond the 64
   sets `tools_overflow = true` and is not tracked.
4. `tools_ended` ids: every end sets `results_since_output = true`, tracked
   or not, so an untracked tool's result still makes the next model output a
   step boundary [t4r6.9]; an id in the open set is also removed. After an
   overflow `phase` stays `tools` until that boundary: VIA no longer knows
   which tools run, and says so instead of counting them.
5. At the terminal: the current step (if `current_step ≥ 1`) ends at the
   terminal time.

Errors in the open set therefore last at most one step: a missed end keeps a
name listed and `phase` at `tools` until the model's next output after tool
results, which clears both.

So the step count goes up by one exactly when the model produces output after
tool results (R3). A step's end is the next step's start; its row is written
at that instant (§3).

**Tokens** (R3: approximate, once per step, labelled). The design claims no
accuracy figure: what each vendor's evidence supports, and the probes still
needed, are in §2.5. R3's "about 95%" is an owner gate (Q-R5-11): R3
conformance is not claimed for Claude, Codex or OpenCode before it
[t4r5.10, t4r6.14].

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

- open set: at most `OPEN_TOOLS_MAX` = 64 `(id, name)` entries, each field at
  most 1 KiB (§2.2 rule 1), in a `Vec` allocated with capacity 64 at
  submission [t4r5.3]; `tools_overflow` is one flag.
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
Claude `system/init`, Codex `thread/status/changed`) sends no observation; if
the Route attributes it to the turn it moves the activity clock only
[t4r5.11]. OpenCode's `server.heartbeat` and Codex's untagged connection
status are not attributed to any turn [S OpenCode §5, Codex §5], so they
move nothing. Codex's own tool-completion tracking for P7 (`tool.quiescent`)
is separate, exact and unchanged [S §6]; the snapshot's open set is display
only.

**Token evidence per vendor** [t4r5.10]. What the specs prove, and the probe
each vendor slice must run before `tokens` may be claimed for it:

| Vendor | Proven by the spec | Unprobed | Probe needed |
|---|---|---|---|
| Fake | the fixture's `usage` numbers are exact by construction | — | none |
| Claude | `result.usage` per result, scope `turn`; `total_cost_usd` session-cumulative [S §3, §5] | whether each `assistant` message carries `message.usage`, and whether repeated assistant messages for one API call repeat it | a multi-step tool turn on the pinned version, comparing the sum of per-message-ID samples with `result.usage` |
| Codex | `thread/tokenUsage/updated` carries `last` and `total`; `total` is session-cumulative; replace snapshots, never sum repeats [S §7] | whether one `last` covers exactly one model call, so that summing `last` per step matches the delta of `total` | a multi-step turn comparing Σ`last` with the change in `total` |
| OpenCode | each assistant message has token fields; step-finish duplicates them; one ledger keyed by message ID [S §7] | the exact event and part names; whether one assistant message is one model call | the legacy `message.*` event family on the pinned version, comparing the ledger with the session total |

Until a vendor's probe passes, its route declares `usage.tokens` with the
spec's scope label and `tokens` may be `null`; the envelope's `usage` is
unaffected.

### 2.6 Idle deadline

Runtime §8: idle resets on normalized meaningful progress. [V] Today
`progress()` counts acceptance, `assistant.text`, `tool.started` and
`tool.ended` (`crates/via-core/src/engine/drive.rs:1775-1786`). After R2 it
counts acceptance and any `progress` item with `model`, a started tool or an
ended tool. A usage-only item does not reset it, and unknown messages send
nothing (T3 §5: they never reset it). The timer and its stop order are T3's, unchanged.

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

- **Step end.** When the tracker ends step N at a step boundary (§2.4 rule
  2), the drive commits `CommitSteps { session, turn, rows: [row N] }` on the
  Internal lane through the single SQLite writer, awaited under
  `while_polling` (§8). No `Head` is taken: a row has no `seq`. The row
  insert is plain `INSERT`; a duplicate is a constraint error (a bug).
- **Batching** (R4 "may be batched"): the drive awaits each step commit
  before it handles the next observation (below), so outside the terminal a
  command carries one row.
- **The last step** is written **in the terminal transaction**: every
  terminal commit built from the drive's `TurnRecord` (`finish_with`, and
  `finish` for a forced turn, `drive.rs:1035`, `:1000` [V]) carries the open
  step's row with `ended_ms` = the terminal time. A forced turn's record
  travels to final shutdown in its `ForcedTurn` (`drive.rs:756` [V]):
  `hand_off` ends the open step at the hand-off, and final shutdown's single
  best-effort terminal (T3 §7.4, `t3/design.md:1348`) is built from that
  record (`forced_terminal`, `crates/via-core/src/engine/stop.rs:360` [V])
  and committed by `finish`, so it carries the open row and any carried rows
  [t4r6.8].
- **No row cap** [t4r5.1]. Every completed step gets a row. A row is under
  128 B stored and a step is a whole model call; rows count against the disk
  quota (§6.9), and a quota refusal is a known not-committed outcome handled
  by the next bullet.
- **Refused rows ride in the terminal** [t4r5.2]. A step commit follows
  T3 §7 for a turn write. A known `NotCommitted` (including a quota refusal)
  records the turn's `first_failure` and upgrades its stop order to cause
  `store` (`force_at = now`), and the turn then writes nothing except its one
  resolution write (T3 §7.2). The drive keeps the refused row, and every row
  ended after it, in `TurnRecord.carried_rows`, and the resolution write — the
  `failed(store)` terminal transaction — inserts them together with the open
  step's row. An uncertain outcome latches (T3 §7.4).
- **Carried-row bound.** After the refusal the stop is forced at once, so the
  drive handles at most the items already in the channel (1024), the hop's
  one, the Adapter's one in hand and Route's one decoded message, each ending
  at most one step: at most 1 + 1027 + 1 (open) = 1,029 rows, each at most
  128 B in the command, so `TERMINAL_ROWS_MAX` = 132 KiB (`Vec` allocated with
  capacity 1,029 at the refusal [t4r5.3]). The bound is structural; a debug
  assertion checks it.
- **Ordering.** The drive awaits each step commit before it handles the next
  observation, and builds the terminal only after draining the channel. So
  in the writer's order every row of turn N precedes, or is in the same
  transaction as, turn N's `turn.ended`.
- **Guarantee, exactly** [t4r6.8]. A `turn.ended` built from the
  `TurnRecord` (the natural terminal, the `failed(store)` write after a
  known refusal, final shutdown's forced terminal) commits with every row
  not yet durable, so every row of the turn is durable. Only two terminals
  make no such claim, because an earlier row's outcome is unknowable: the
  Latch batch after an uncertain outcome (T3 §7.4), which carries no rows,
  and recovery's (§3.3). A failed terminal write follows T3's rules.

### 3.3 What survives a crash

- Rows up to the last *committed* step (R4 as clarified [t4r5.15]). A step
  that ended while its row commit was in flight, and the step in progress,
  are lost; they are recoverable only from the raw log.
- Recovery synthesizes the turn's `unknown` terminal (T3) and adds no row.
- The snapshot and the activity clock are memory only and are not recovered.

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
  text (`[`, the stored event texts separated by `,`, `]`) into a buffer
  allocated once at the reply's charged capacity [t4r5.3]; the handler writes
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
next_cursor, incomplete}`. `stream` is `stdout`, `stderr` or `stdin`. `text`
is the bytes as lossy UTF-8; VIA does not parse them (R5). `incomplete` is
true when the addressed turn, or any turn of the addressed session, recorded
`raw_log.incomplete` (an indexed read, §6.7) [t4r6.10].

- **Attribution** [t4r5.7]. Every raw unit is attributed to a session and
  turn, to a session only, or to nothing, by the Route that knows the
  attribution, and `logs` returns only units attributed to the addressed
  session or turn. Unattributed traffic is never returned (D4 isolation).
  Schema v6 records it now (§6.7), by connection kind:

  | `connections.kind` | Routes | Attribution of a unit |
  |---|---|---|
  | `turn` | the fake, Claude (one process per turn) | implicit: the connection's `(session_id, turn)`; no rows |
  | `session` | OpenCode (one server per session) | implicit session; the turn from a `raw_spans` row covering the unit, else session-level |
  | `shared` | Codex shared stdio server | only a `raw_spans` row covering the unit; stderr and uncovered units are unattributed |

  A `raw_spans` row `(connection_id, start_offset, end_offset, session_id,
  turn?)` covers whole consecutive units of one attribution. The Route
  learns a stdout unit's attribution when it decodes the message (after the
  unit is durable) and a stdin unit's when it writes it. It extends a
  run-length span in memory, appends closed spans to a batch (an up-front
  `Vec` of 64), commits it as one Internal-lane `CommitSpans` when 64 are
  pending, 1 s after the first, or at the seal, and keeps it until the
  outcome is known; a second batch of 64 fills meanwhile [t4r6.10]. With
  both full, Route awaits the commit as a pinned future, servicing controls
  (§8) but reading no message; a Store that stays slow makes the Wire queue
  fail the connection `overflow`. No known span is dropped. A known refusal
  (quota, `NotEnqueued` or validation, §6.7) fails the connection as a
  raw-log failure (`raw_incomplete` on its latch, §7.4), so each turn on it
  records `raw_log.incomplete`; an uncertain outcome latches (O1). Memory:
  128 spans of at most 128 B. S1 has only `turn` connections; the Codex and
  OpenCode slices build the Route side with no migration.
- **Scope.** For a turn address: the `turn` connections of that turn, then
  the spans of `session` and `shared` connections whose `turn` matches. For
  a session address: the session's `turn` and `session` connections whole,
  then the spans of `shared` connections whose `session_id` matches. Order:
  connection creation order (`connections.ord`), then offset.
- **Cursor** [t4r6.11]. The sentinel `r1.start` (what an omitted cursor
  means) or `r1.<connection_id>.<offset>`, the next raw byte to return;
  parsed strictly. From the sentinel the walk starts at offset 0 of the
  first connection in scope by `ord`. A cursor naming a connection outside
  the scope, or an offset beyond its durable range, is `invalid_params`.
- **Paging.** From the cursor, the SQLite thread binary-searches the index for
  the unit containing the offset (§6.4), then walks consecutive units in
  scope. For each unit it reads the whole unit (at most 1 MiB,
  `RAW_UNIT_LIMIT`) and checks its SHA-256, then emits the part from the
  cursor, at a UTF-8 boundary when one is within 3 bytes. A page emits at
  most `LOGS_RAW_MAX` = 128 KiB of raw bytes and 256 entries. The walk passes
  to the next connection only after a sealed end; it stops at the durable
  end of an unsealed one.
- **End cursor** [t4r5.12]. `next_cursor` is the position after the last
  byte returned, or the request's cursor (`r1.start` included) when nothing
  new is durable, so a polling caller resumes from it. It is `null` only at
  a sealed end: every connection in scope is sealed, the page reached the
  end of the last one, and no connection can still join the scope (a turn
  address whose turn is terminal; a session address whose session is
  closed).
- **Bound proof.** One raw byte encodes to at most 6 JSON bytes (a control
  character as `\u00XX`; an invalid byte becomes a 3-byte U+FFFD), and an
  entry's other members are under 256 B. So a page is at most 6 × 128 KiB +
  256 × 256 B = 832 KiB < `PAGE_MAX`, and no unit is ever refused for size: a
  large unit spans several pages.
- **Errors.** A missing or corrupt unit is `store_error`, scoped to the
  request (no latch), as today.

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
| `list` | `state?, harness?, label?, since?, limit?, cursor?` | Store `list_page` (§6.10; A12) |
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

### 5.1 Charge capacity, never length [t4r5.3]

Every buffer that holds peer or payload bytes takes one of two forms, and
its charge is taken before it is allocated:

- **Up-front capacity**: allocated once at its maximum, charged at that
  maximum, never grown. Writes go through a bounded writer that refuses a
  byte past capacity; refusal is the buffer's stated outcome, never a
  reallocation.
- **Exact size**: a `Box<[u8]>`, a `String` from `JsonStr::to_string_exact`
  (capacity = the counted decoded length, §2.2 rule 6 [t4r6.1]) or a slice,
  or a `Vec` allocated with the counted length and filled to it, charged at
  that size before it is allocated.

No `Vec` on these paths grows by `push` or `extend` past its first capacity,
so no old and new allocations of one buffer ever coexist. Two distinct fixed
allocations may coexist briefly (a C1 line's segments and its assembled copy;
the stdout assembly buffer and a staged message), and both are charged.
serde_json's own scratch is bounded by §2.2 rules 4 and 6 (escaped keys at
most 4 KiB, nesting at most 64): with its doubling growth its peak, old plus
new, is at most 3 × 4 KiB + 64 B, charged as a fixed 16 KiB per decode.

| Buffer | Form | Charge | Charged to |
|---|---|---|---|
| Pipe read buffers (stdout, stderr) | fixed arrays | 2 × 64 KiB | drive reserve |
| Stdout message assembly | up-front capacity, `MAX_STDOUT_MESSAGE_BYTES` | 1 MiB per connection | drive reserve |
| Staged raw unit | exact `Box<[u8]>`, copied from the read or assembly buffer | `max(len, 512)` | staging |
| Stdin pieces | up-front capacity | 5 × 96 KiB | drive reserve |
| Decoded vendor message | exact strings (disjoint spans of the message), lists of capacity 256 | `len + 64 KiB` | drive reserve |
| Observation items | exact strings | `512 + Σ(64 + len)` | the C2 semaphore, inside the drive reserve |
| Event and envelope encodings | exact, after a counting pass | measured | drive reserve |
| Envelope arena [t4r6.4] | up-front capacity `ENVELOPE_MAX` | 1 MiB | drive reserve |
| Open-tool set, carried step rows | up-front capacity 64 and 1,029 | §2.4, §3.2 | drive reserve |
| C1 line | fixed charged segments of 64 KiB; segment list of capacity 256 | 64 KiB per segment | dynamic |
| C1 assembled line (more than one segment) | exact | `len` | dynamic |
| C1 decoded members | exact strings and counted lists (disjoint spans of the line) | `len + 24 × nodes` | dynamic |
| Reply and Store page buffer | up-front capacity: `PAGE_MAX + 4 KiB`, or 2 MiB for `logs` (a 1 MiB unit read plus the 832 KiB page), or 64 KiB | as stated | dynamic |
| Blob chunk | exact, at most 64 KiB | 64 KiB | dynamic |
| Dispatched blob prompt | exact `String` | its length | dynamic |

### 5.2 One budget, five partitions

Runtime §8 requires one 128 MiB retained-payload budget that includes
SQLite's 8 MiB cache (`docs/specs/runtime-contracts.md:1057`) [t4r6.2].
`MemoryBudget` (via-store, the lowest crate, `scripts/check-layers.py:20`
[V]) carves it at open into five `tokio::sync::Semaphore` partitions whose
sizes sum to 128 MiB, so no combination of charges can exceed it:

| Partition | Size | Holders | Acquired | Refused or waits |
|---|---|---|---|---|
| Store | 8 MiB | the request lanes (§6.1) | `Store::open` | open fails |
| SQLite cache [t4r6.2] | 8 MiB | the page cache of the one long-lived connection, the writer's (`cache_size=-8192`, `crates/via-store/src/runtime/sql.rs:140`; opened at `crates/via-store/src/runtime.rs:1013` [V]) | `Store::open`, before the connection opens; held until it closes | open fails |
| Staging | 32 MiB, at most 8 MiB per connection (runtime §8) | staged raw units | before the copy, nonblocking | the connection fails `overflow` + `raw_log.incomplete` (A1) |
| Drives | 46 MiB = `CONNECTION_SLOTS` (4, `crates/via-core/src/engine/queue.rs:33` [V]) × 11.5 MiB | one drive reserve per running turn (§5.4) | by the dispatcher after the connection slot, before the submission commit | waits with the dispatcher's other waits; the turn stays queued |
| Dynamic | 34 MiB [t4r6.2] | C1 lines and decodes, replies, blob chunks, dispatched blob prompts | before each allocation | C1 line segments wait to the 5 s partial-line deadline, then close the connection; a dispatched prompt waits in the dispatcher; blob chunks wait under the chunk's 2 s bound; decodes and replies are nonblocking `admission_refused` (`MEMORY_BUDGET`) |

Waits are acyclic: a dispatcher waits only for drive and dynamic permits;
C1 handlers, replies and blob chunks never wait for a drive, and a reply's
permit is held at most `REPLY_WRITE` (A32).

The Wire message queue needs no charge of its own: a queued message is the
staged unit, charged until its last handle drops; its per-connection
64-message / 4 MiB residency is a counter checked by the reader
(nonblocking; full fails `overflow`).

Not charged, and measured by the RSS gate (§12): allocator overhead,
SQLite's allocations other than its page cache, task stacks, fixed-size
structs. (The read-only probe at open, `runtime.rs:974`, is dropped before
the writer connection opens.)

### 5.3 A C1 request at its maximum

A line of `len` bytes (at most 16 MiB) with `nodes` scanned nodes (at most
65,536):

| Step | Held | Proof |
|---|---|---|
| Read | `len` in segments | 64 KiB segments charged before each read |
| Assemble (more than one segment) | `2 × len`, then `len` | the exact copy is charged before allocation; the segments are released after it |
| `json_limits::scan` | 0 | a byte scanner; it allocates nothing (§9.2) |
| Envelope and identity passes | 16 KiB | borrowed `&RawValue` pieces (`read.rs:636-650` [V]); scratch for keys only |
| Typed decode | `len + 24 × nodes + 16 KiB` more | every retained string is an exact copy, at its decoded length, of a disjoint span of the line (§2.2 rule 6); each node adds at most one 24 B value (a `String` or `Vec` header, a 16 B `Box<RawValue>`, a scalar) in a list or struct of exact capacity |
| Identity and prompt blobs | 64 KiB more per handle | pieces stream from the line and the prompt `String` |

Peak: `2 × len + 24 × nodes + 16 KiB + 128 KiB` = 32 MiB + 1.5 MiB +
144 KiB ≈ 33.7 MiB for a maximal request. It fits the 34 MiB dynamic
partition when no other dynamic charge is held, at the maximum drive and
connection counts [t4r6.2]. Serde-internal overheads other than scratch are
fixed-size and uncharged, as above.

### 5.4 The drive reserve

One private connection per drive in S1. The reserve covers, at their
maxima:

| Part | Bound | Proof |
|---|---|---|
| Pipe read buffers | 128 KiB | two 64 KiB arrays |
| Stdout assembly buffer | 1 MiB | up-front capacity |
| Stdin pieces | 480 KiB | at most four unacknowledged pieces of 96 KiB plus one being encoded (§7.3) |
| Decoded messages in flight | 3.2 MiB + 16 KiB | Route's, the hop's one, the Adapter's: 3 × (1 MiB + 64 KiB), §2.2 rules 1–2 and 6; one 16 KiB scratch |
| Observation channel | 4 MiB | the C2 semaphore (§2.3) |
| Step tracker, published progress, carried rows | 384 KiB | §2.4 (open set 128 KiB, published copy 70 KiB, samples), §3.2 (132 KiB) |
| Event encoding | 260 KiB | one event at a time: at most 256 KiB payload plus Core's fields |
| Envelope arena and terminal build [t4r6.4] | 2 MiB | the arena, 1 MiB up front, holds every accumulated member encoded (§6.5); the exact encoded envelope, at most 1 MiB, moved into the Store command |
| Span batches | 16 KiB | §4.4 |
| **Total** | **11,716 KiB ≈ 11.44 MiB, reserved as 11.5 MiB** | |

Arithmetic at the maximum counts (4 connection slots, so 4 drives and 4
private connections; 32 sockets) [t4r6.2]: Store 8 + SQLite cache 8 +
staging 32 + drives 4 × 11.5 = 46 + dynamic 34 = 128 MiB. Against round 6,
the SQLite cache enters the budget, the drive reserve falls from 13 MiB
(one arena replaces separate list and text buffers) and the dynamic
partition falls from 36 MiB; no admission count is lowered. A shared
connection (Codex) or per-session server (OpenCode) charges its own fixed
buffers when its slice lands, and that slice restates this sum.

### 5.5 The bound table (R7)

| Resource | Bound | Enforced at | At the bound | Proof |
|---|---|---|---|---|
| Vendor stdout message | 1 MiB including LF | stdout reader | `MessageTooLarge`, raw-only drain | the assembly buffer's capacity |
| Message queue | 64 messages, 4 MiB per connection | stdout reader | `overflow` | counter |
| Raw staging (memory) | 8 MiB per connection, 32 MiB total | staging partition | `overflow` + `raw_log.incomplete` | semaphores |
| Raw log (disk) | per unit 1 MiB (stdout) or 64 KiB (stderr); in aggregate the disk quota | reader; raw worker | quota: the connection fails, `raw_log.incomplete` | §6.9 |
| Envelope | 1 MiB encoded, accumulation measured as it grows | Core, per list entry and per final-text append [t4r6.4] | stop order `overflow` at that item; the bounded failure summary | §2.3, §6.5 |
| Blob files | one prompt and one identity per spawn/resume, each at most 16 MiB; in aggregate the quota | handler; raw worker | quota: `store_error` `not_committed` | §6.6, §6.9 |
| Blob memory | 64 KiB per chunk in flight | dynamic partition | chunk wait | one chunk per handle |
| Snapshot | 70 KiB published, 256 KiB tracker, per running turn | reducer | `tools_overflow` [t4r6.9]; keyless sum | §2.4 |
| Step rows | one per completed step, under 128 B each; in aggregate the quota | Store | quota: carried into the terminal (§3.2) | §6.9 |
| Attribution spans | two batches of 64 per connection | Route | Route awaits the commit; a refused batch fails the connection with `raw_log.incomplete` [t4r6.10] | §4.4 |
| SQLite and all files | 4 GiB logical, 16 MiB of it for lifecycle writes | `DiskLedger`, reserved before each write [t4r6.3] | `NotCommitted(Quota)` | §6.9 |
| Observation channel | 1024 items, 4 MiB per drive | Adapter | wait; 10 s → `overflow` or thread quarantine | C2 A1, §2.3 |
| Store requests | 1 Latch + 7 Lifecycle + 64 ordinary slots; 8 MiB bytes | `Lanes::push` | `NotEnqueued` / `admission_refused` | §6.1 |
| Store transaction | 128 events and 1 MiB, plus one envelope of at most 1 MiB and the carried rows | `Lanes::push` | `NotEnqueued` | A30, §6.5 |
| Store replies | `PAGE_MAX` for pages, `ENVELOPE_MAX` for results, `STATUS_MAX`, logs 832 KiB | the SQLite thread checks borrowed lengths before copying into the up-front buffer | page stops; a single oversize item is `admission_refused` | §4 |
| C1 request | 16 MiB line; peak §5.3 | reader, handler | parse error then close; `admission_refused` | §5.3, §9 |
| Replies in flight | 32 sockets × one reply, each written within 10 s | handler | connection closed | A32 |
| Global memory | 128 MiB, SQLite's cache included | the five partitions | each partition's outcome | §5.2 |

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
  + 132 KiB of carried rows + 512 B) plus small requests. When abandoned requests hold the bytes, a
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
  permits: every `Append` carries an `Arc<Payload>` from `Payload::stage`
  (`max(len, 512)`), so at most 32 MiB / 512 B = 65,536 appends queue. A
  `Barrier` holds no permit (at most one per connection); a blob command holds
  one 64 KiB chunk permit (one in flight per handle).
- **Fault publication.** via-store declares `trait RawFaultSink { fn
  raw_failed(&self, error: &StoreError); }`; Wire implements it on the
  connection latch. The worker calls it on its own thread, with no lock, and
  it never blocks.
- **Group commit** (runtime §4: 1 MiB or 20 ms). Before writing a batch the
  worker reserves its payload and index bytes in the disk ledger and settles
  them after the sync (§6.9) [t4r6.3]; a refused unit fails like a failed
  write (below). Per touched connection:
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
  which is itself at most `ENVELOPE_MAX` = 1 MiB (C1 §5, unchanged by R6),
  **and the turn's carried step rows** (at most `TERMINAL_ROWS_MAX`,
  §3.2) [t4r5.2]. Anything over is refused `NotEnqueued` before it is
  queued. A lifecycle atomic batch is never split.
- **Terminal budget**, enforced by measurement where each record is built:

  | Constant | Value | Covers |
  |---|---|---|
  | `ENVELOPE_MAX` | 1 MiB | the turn's encoded envelope |
  | `TERMINAL_EXTRAS_MAX` | 64 KiB | `turn.ended` (vendor fields at most 1 KiB, §2.2), an owed `raw_log.incomplete`, the turn row, the connection seal, the open step's row |
  | `TERMINAL_ROWS_MAX` | 132 KiB | the refused and later step rows (§3.2) |
  | `CANCEL_RECORD_MAX` | 32 KiB | one `queued → cancelled` record (its envelope has no text; `cwd` at most 6 × 4096 escaped) |

  A drive's terminal is at most 1 MiB + 196 KiB; the failure-resolution batch
  (one terminal with no carried rows plus at most
  `FAILURE_BATCH_CANCELLATIONS` = 8, `crates/via-store/src/runtime.rs:390`
  [V]) at most 1 MiB + 320 KiB. Both fit their lanes (§6.1, §6.2).
- **Accumulation, bounded directly** [t4r5.5, t4r6.4]. At submission Core
  measures the envelope's base (every member with empty lists and text) and
  allocates the turn's arena, one up-front `ENVELOPE_MAX` buffer holding the
  accumulated members encoded: denied and declined entries, and final-text
  segments by key (at most 64 keys; more is `protocol`). A replacement, or
  an append to a key whose segment is not last, moves later bytes with
  `copy_within`. The meter is base plus arena bytes; on the item that would
  exceed `ENVELOPE_MAX`, Core records the overrun and orders the stop with
  cause `overflow` at once (§2.3, A34), and later items only add to their
  members' totals. At the terminal a counting writer measures the full
  envelope before any allocation; over `ENVELOPE_MAX` (terminal members add
  to the base), or after an overrun, the turn fails `overflow` and persists
  the summary.
- **Bounded failure summary** [t4r5.6] (C1 §5's existing overflow case, so
  R6's, not an exception to it). Every retained member has its own budget,
  and a member over it is cut as stated; `truncation` lists each cut member
  with its full size or count:

  | Members | Source and C1 limit | Budget (encoded) | Over budget |
  |---|---|---|---|
  | `api_version`, `session_id`, `turn`, `address`, `revision`, `state`, `stop_reason`, `cancel`, `harness`, `route`, `adapter_version`, `version_status`, `steps`, `usage`, `cost`, `timestamps`, `duration_ms`, `exit`, `events`, `failure.{class, retryable}` | Core-built, fixed shape: enums, numbers, timestamps and IDs of at most 64 B | 6 KiB together | cannot occur: fixed shape |
  | `failure.message` [t4r6.7] | Core-built text that may quote a vendor or Store error | 2 KiB | longest prefix, at a char boundary, whose encoding fits |
  | `vendor_stop_reason`, `failure.vendor_code`, `vendor_version`, `vendor_session_id`, `vendor.turn_id`, `model.resolved`, `effort.resolved` | vendor or route short fields, at most 1 KiB raw each (§2.2 rule 1), so 6 KiB escaped | 42 KiB together | cannot occur |
  | `model.requested`, `effort.requested` | caller strings; C1 caps only the 16 MiB line | 6 KiB each | `null` |
  | `cwd` | caller, at most 4096 bytes (§10.1) | 24 KiB | cannot occur |
  | `bound.requested`, `bound.effective` | caller or route; `extra_write_dirs` is an unbounded list under the line cap | 32 KiB each | `extra_write_dirs` cut to the longest prefix that fits |
  | `vendor_options` | caller `vendor` map under the line cap | 16 KiB | `{}` |
  | `warnings`, `raw_spans` | Core-built lists | 16 KiB each | longest prefix that fits |
  | `final_text` [t4r6.6] | vendor, at most 1 MiB | 0 | `""`; the raw log has the text |
  | `structured_output` | vendor | — | `null` |
  | `denied_actions`, `auto_declined_requests` | vendor, entries at most 256 KiB each | 256 KiB each | longest prefix of whole entries that fits; not cleared [t4r5.6] |
  | `truncation` (summary only) | Core: at most 13 entries `{member, total}` | 2 KiB | — |
  | keys and punctuation | — | 4 KiB | — |

  Sum: 6 + 2 + 42 + 12 + 24 + 64 + 16 + 32 + 0 + 512 + 2 + 4 = 716 KiB, so
  `SUMMARY_MAX` = 720 KiB < `ENVELOPE_MAX`. `ended_record` measures each
  group with the counting writer before allocating it, cuts a member over
  its budget as stated, recording `{member, total}` (for `final_text`, its
  encoded length), and measures the whole summary before persistence
  [t4r6.7]. The raw log and the durable events remain the full evidence.
- **Blobs** (runtime §8's floor): prompts and identities over `INLINE_MAX` =
  256 KiB use the blob path, because the retry identity contains the prompt
  (`crates/via-core/src/api.rs:804-858` [V]) and a spawn command carries both.

### 6.6 Blob path on the raw worker [kept; F5 fixed]

- **Owner.** The raw worker is the only thread touching `blobs/`; startup
  verification and the sweep run on the SQLite thread before admission.
  `blobs/` is validated like `raw/` (0700, not a symlink, daemon's uid);
  files `b_<32 hex>.blob`, `create_new`, 0600, `NOFOLLOW`; rows store the
  relative id.
- **Handles.** `BlobWriter::write(chunk ≤ 64 KiB)` (dynamic-partition chunk
  permit and a disk-ledger reservation (§6.9), ack under 2 s; timeout or quota
  refusal is `not_committed` because nothing references it yet);
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
- **Dispatch load.** The dispatcher waits for a dynamic-partition permit of
  the prompt's length (§5.2; beside its other waits, so the turn stays
  queued), then loads the blob into an exact `String` with a running SHA-256
  and a UTF-8 check; a mismatch fails the turn as corrupt evidence. The loaded
  `String` is moved into the streamed start (§7.3) and released once the
  start is written.
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
  (`sql.rs:966` [V]), and the indexes `(session_id, turn, seq)` and
  `(session_id, type)` (the latter for `logs`' `incomplete`, §4.4).
  `connection_id` gains an FK to `connections(id)`, and `insert_event`
  refuses (`Constraint`) a raw ref to a `turn` or `session` connection of
  another session. `late` and `at` stay inside the JSON: no query reads them
  (A28 narrows runtime §6's target row; round 4's `late`, `at_ms` and two
  partial indexes served only the follower and are dropped).
- **`turns`** gains `ended_seq` with `CHECK((state IN
  ('completed','failed','cancelled','unknown')) = (ended_seq IS NOT NULL))`
  (A15), and `prompt_blob` with `CHECK((prompt IS NULL) <> (prompt_blob IS
  NULL))`.
- **`spawn_keys.identity`, `operations.identity`** become nullable with
  `identity_blob` and the same CHECK.
- **`connections`** (new): `id TEXT PRIMARY KEY`, `ord INTEGER NOT NULL
  UNIQUE` (creation order), `kind TEXT NOT NULL CHECK(kind IN
  ('turn','session','shared'))` [t4r5.7], `session_id` and `turn` (FK),
  with `CHECK((kind='turn') = (turn IS NOT NULL) AND (kind='shared') =
  (session_id IS NULL))`, `high_water INTEGER` (NULL until sealed), and
  `incomplete INTEGER NOT NULL CHECK(incomplete IN (0,1))` [t4r6.10]. Files
  are named from the id as today (`raw.rs:159-160` [V]). Created in the
  submission transaction (`SubmissionRecord` gains `connection_id`);
  `incomplete` set by a committed `raw_log.incomplete`; sealed by the
  terminal transaction from the index's last complete entry (durable because
  `finish` ran its barrier), or `incomplete = 1` with the offset it can
  prove; recovery seals open rows.
- **`raw_spans`** (new) [t4r5.7]: `connection_id` (FK), `start_offset`,
  `end_offset CHECK(end_offset > start_offset)`, `session_id NOT NULL` (FK),
  `turn` (NULL for a session-level span), `PRIMARY KEY(connection_id,
  start_offset)`, `WITHOUT ROWID`, and the index `(session_id, turn,
  connection_id, start_offset)`, which also makes a session's spans one keyed
  delete for retention. Written only by `CommitSpans` (§4.4); S1 writes none.
  **Validation** [t4r6.12]: before its transaction commits, `CommitSpans`
  checks that the spans are disjoint from each other and from the
  connection's stored spans (the neighbouring key rows); that each offset
  is a raw unit boundary within the durable range (§6.4 lookup); and
  ownership: the connection is `session` kind with the span's session, or
  `shared` kind with a turn of the span's session submitted on it, and a
  span's `turn` is such a turn. Any failure rolls the batch back as
  `StoreError::Constraint("raw_spans")`, a known refusal (§4.4).
- **Time.** `at` strings are parsed strictly to Unix ms; malformed is
  `StoreError::Constraint`. No ordering relies on wall time.

### 6.8 Store reads

| Read | Lane | Returns | Bound |
|---|---|---|---|
| `terminal_facts(session, turn)` | caller's | `Option<{state, cancel}>` via `json_extract(envelope, '$.cancel')`; `None` when not terminal | 4 KiB |
| `result_text(session, turn, budget)` | Public | the stored envelope text | `ENVELOPE_MAX` |
| `events_page` | Public | §4.3 | `PAGE_MAX` |
| `logs_page` | Public | §4.4 | 832 KiB, plus one unit read |
| `list_page` | Public | A12 (§6.10) | `PAGE_MAX` |
| `session_status(session, turn?, after_step, limit)` | Public | §10.4 members and a step page | `STATUS_MAX` |

`terminal_facts` replaces every internal use of the envelope `Value` read,
all of which need only existence, `state` or `cancel` [V]:
`crates/via-core/src/engine/control.rs:48` (state and cancel, `:210-216`),
`journal.rs:550`, `stop.rs:632`, `batch.rs:85`. C1 `result`, `wait` and
`await_terminal` use `result_text`. So no daemon path parses a stored
envelope into a `Value`. The existing `result` read is removed.

### 6.9 Disk quota [t4r5.4, t4r6.3]

Runtime §6 specifies it; R7 keeps disk bounds; Task 4 builds it.

- **Limit.** `QUOTA` = 4 GiB logical over the database, its WAL, `raw/` and
  `blobs/`: ordinary writes up to `QUOTA − LIFECYCLE_RESERVE` (16 MiB),
  lifecycle writes up to `QUOTA`. Seam `VIA_TEST_QUOTA_BYTES`.
- **Owner.** One `DiskLedger` in `Store`, a std `Mutex<{used, reserved,
  holds}>` locked only for arithmetic by the SQLite thread and the raw
  worker. `Store::open` seeds `used` from the files (`page_count ×
  page_size`, `raw/`, `blobs/`) plus a fixed `WAL_CHARGE` = 40 MiB for the
  WAL (runtime §6 stops admission at 32 MiB of WAL; one transaction adds
  under 8 MiB; the writer is the only connection, so checkpoints complete).
- **Reserve, write, settle.** A write first calls `reserve(class, bound)`,
  which succeeds only if `used + reserved + bound ≤ limit(class)`; after the
  write, `settle` replaces the bound with the measured growth. A raw batch's
  bound is its payloads plus 45 B per index entry, a blob chunk's its
  length, both exact. A SQLite command's is `sqlite_bound = 2 ×
  Command::bytes() + (cells + 16) × 4 KiB` [I], `cells` counting the rows
  and index entries it writes: a stored byte takes at most two at SQLite's
  worst b-tree fill, a cell splits at most one page, and 16 pages cover
  interior pages and the header. After the commit the SQLite thread measures
  `page_count × page_size` and the WAL; growth beyond the bound or a WAL
  beyond `WAL_CHARGE` is recorded as measured, logged as a `warning`, and
  fails a debug assertion. A blob discard or the startup sweep subtracts
  the file; nothing else frees space in S1 (retention is `via-jm4.18`).
- **Turn holds.** A terminal with 1,029 carried rows has `sqlite_bound` ≈
  2 × 1,220 KiB + 1,109 × 4 KiB ≈ 6.7 MiB, and four exceed 16 MiB, so no
  shared reserve can cover running turns. A turn's submission therefore
  reserves, besides its own bound, `TURN_DISK` = 8 MiB (that bound plus
  `cancel.requested`, `cancel.settled` and `raw_log.incomplete`, 104 KiB
  each) and keeps it as a hold for the turn (at most `CONNECTION_SLOTS` =
  4). Those writes and the turn's terminal (by its drive, final shutdown or
  the Latch batch) draw on the hold unchecked; the terminal's settle
  releases it. Holds are memory; a restart reseeds from the files.
- **Classes.** Lifecycle: the writes on a hold, connection seals, the Latch
  failure-resolution batch and recovery's synthesized terminals. All else is
  ordinary, including a session close and a queued turn's cancellation
  outside the Latch batch.
- **At the quota.** A refusal is `NotCommitted(StoreFailureKind::Quota)`
  (`crates/via-store/src/runtime.rs:100` [V]), a known outcome (T3 §7.1): a
  raw unit or batch fails its connection like a failed raw write (§6.4),
  owing `raw_log.incomplete` (on the hold), and stream payload stops
  (runtime §6); an event or step row follows T3 §7.2, the rows riding in the
  terminal (§3.2); a span batch fails its connection (§4.4); a spawn,
  resume, submission, close, queued cancellation or blob write is
  `store_error` `not_committed` and changes nothing. Key and receipt rows
  are never deleted to make room (runtime §6).
- **Reserve sufficiency** [t4r6.3]. Once ordinary writes stop no turn is
  admitted, so the lifecycle writes without a hold are fixed: the Latch
  batch's queued cancellations, one batch per running turn of at most
  `SESSION_QUEUE_LIMIT` = 8 records (`runtime.rs:28` [V]), 4 ×
  `sqlite_bound(8 × 32 KiB + 512 B, 64 cells)` = 4 × 833 KiB; and recovery's
  terminals for at most 4 running turns, 4 × `sqlite_bound(96 KiB, 32
  cells)` = 4 × 384 KiB. Total 4.8 MiB; the other 11 MiB absorbs measured
  overshoot. Beyond it a lifecycle write is refused and T3's rules apply.
  An actual I/O failure still follows runtime §7.

### 6.10 `list` paging [kept: A12]

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
3. stdout [t4r5.3]: a pure `LineSplitter` splits on LF. An unfinished
   message is appended to the connection's assembly buffer, allocated once
   at `MAX_STDOUT_MESSAGE_BYTES` capacity when the connection opens (drive
   reserve) and never grown. At LF the message — from the read buffer
   directly when it lies wholly in one read, else from the assembly buffer —
   is copied by `Payload::stage(&[u8])` into an exact `Box<[u8]>` whose
   staging charge `max(len, 512)` is taken before the copy (nonblocking);
   the assembly buffer is then cleared, keeping its capacity. The payload is
   submitted to raw, counted against the 64-message / 4 MiB residency, and
   `try_send` into the queue with its ack. `Payload::stage` is the one
   constructor for stdout, stderr and stdin units, so the round-4
   `freeze` interface question (Astra F4) no longer arises.
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
  escaped into one of five piece buffers of 96 KiB capacity allocated when
  the connection opens (drive reserve) [t4r5.3]. No whole second copy
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
- Input [t4r5.3]: the line is read into fixed 64 KiB segments, each charged
  to the dynamic partition before it is allocated, listed in a `Vec` of
  capacity 256; one absolute deadline of 5 s from a line's first byte to its
  LF (a segment wait past it closes the connection); an idle connection has
  none. A one-segment line is decoded in place; a longer one is copied into
  an exact `Box<[u8]>` charged before allocation, and the segments are
  released (§5.3).
- A line over 16 MiB (LF included) gets one bounded `parse_error` write (2 s)
  and the connection closes.
- A request `id` over 256 B encoded is `invalid_request` (A31).
- Each reply is written within `REPLY_WRITE` = 10 s from its first byte, else
  the connection closes (A32).
- Seam: `VIA_TEST_PARTIAL_LINE_MS`.

### 9.2 JSON limits (A10) and no peer `Value`

- `json_limits::scan` [t4r5.3] is a byte scanner, not a serde pass, so it
  allocates nothing: it tracks string boundaries and escapes, bracket depth
  and a node count (every value and key), and fails at depth 65, node 65,537
  or an escaped key longer than 4 KiB. It runs on every C1 line and every
  vendor message before any serde pass. On any valid JSON prefix it and
  serde_json agree on token boundaries, so serde never sees a structure the
  scan did not bound; malformed input is refused by whichever finds it first.
- `JsonStr<'a>` and `to_string_exact` (§2.2 rule 6) are the only way a
  peer-fed string becomes a `String`.
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

Per request line, in order, with §5.3's charges: (1) `json_limits::scan`; (2)
borrowed envelope `{jsonrpc, id, method, params}` as `&RawValue` (no
allocation, `read.rs:636-650`); (3) `id` checked (string, number or null, at
most 256 B) and copied, `method` decoded; (4) for keyed calls, the identity
pass finds the handle span and forms three borrowed pieces; at most
`INLINE_MAX` becomes one `Vec`, else the pieces stream into a `BlobWriter`;
(5) the typed decode of the method's strict DTO, its strings through
`JsonStr`; (6) a prompt over `INLINE_MAX` streams from the decoded `String`
into a `BlobWriter`; (7) drop and release.

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

| # | Status in round 7 |
|---|---|
| A1 staging overflow is `incomplete`, not a Store failure | kept (§5.1, §7.4) |
| A2 lanes and the 8 MiB partition | kept; byte split updated by A30 (§6.1) |
| A3 fake wall default 3,600,000 ms | kept |
| A4 disjoint session counts | kept (§10.3) |
| A6 "session actor" is `Slot`/`Head` for follow registration | **withdrawn**: follow is removed (A25) |
| A9 nested `null` in `deadlines.*` | kept |
| A10 a JSON node is any value or key | kept (§9.2); extended by A35 |
| A12 `list` two-phase paging | **kept unchanged** (§6.10, proof §11.3) |
| A13 coordination primitives | kept |
| A14 `cwd`, `allow_untested` in frozen `params` | kept |
| A15 `ended_seq`, no recompute | kept |
| A16 S1 definitions of `status`/`list` members | kept; extended by A26 |
| A19 stall as a stop order with cause `overflow` | **withdrawn**: the stall closes Route's hop (A29, §2.3). A34 adds cause `overflow` for a different trigger, the envelope overrun [t4r5.5] |
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

C1 §5 field table: the `denied_actions` and `auto_declined_requests` rows
are unchanged; their `event_seq` members name durable events, which still
exist. Replace the `raw_spans` row [t4r5.13] ("bounding spans per
connection for the turn, **not** extraction ranges on shared connections;
event `raw_ref`s are authoritative") with:

> | `raw_spans` | bounding spans per connection for the turn, **not** extraction ranges on shared connections; `logs` (§3.12) returns only the raw units attributed to the session or turn |

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
> at most that long. `logs` returns only the raw units attributed to the
> addressed session or turn; unattributed shared traffic is never returned
> (C1 §3.12).

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

Runtime §4 (`docs/specs/runtime-contracts.md:318-319`) [t4r6.13]: replace
"Neither waits for Route, Core, SQLite, fsync or a follower." with "Neither
waits for Route, Core, SQLite or fsync."

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

Vendor specs [t4r5.13]: in the C1 method tables of
`docs/specs/vendors/claude-code.md:59` and `docs/specs/vendors/opencode.md:461`,
replace "`status`, `wait`, `result`, `list`, `events`, `unsubscribe`, `logs`"
with "`status`, `wait`, `result`, `list`, `events`, `logs`". Codex's vendor
method `thread/unsubscribe` (`docs/specs/vendors/codex.md:59`, `:87-88`,
`:109`, `:232`, `:395`; `docs/specs/adapter-contract.md:366`;
`docs/specs/via-api-v1.md:257`) is a lease release on the vendor server, not
the removed C1 method, and is unchanged.

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
>  "progress":{"turn":2,"current_step":4,"phase":"tools","running_tools":["shell"],"tools_overflow":false,"last_activity_at":"…",
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
> each route's vendor evidence supports. The snapshot
> ends with the turn; the envelope's `steps` and `usage` hold the final
> figures.
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
> `vendor_session_id` … (the existing paragraph continues unchanged).

Restatements: `docs/specs/via-api-v1.md:571` (idle shutdown: `alive`
becomes false; unchanged); A16's list (`:274-297`).

**T4-A27. `logs` returns raw excerpts by byte cursor (R5).** Amends C1 §3.12,
runtime §9. [t4r6.10, t4r6.11]

Replace C1 §3.12 with:

> ### 3.12 `logs` — raw-log excerpts
>
> `via logs <session|turn> [--cursor C] [--limit N]`. Returns raw bytes
> attributed to the addressed turn (or session), undecoded, in connection
> order then offset:
> `{entries: [{connection_id, stream, offset, len, text}], next_cursor,
> incomplete}`.
> `stream` is `stdout`, `stderr` or `stdin`; `text` is the bytes as lossy
> UTF-8, which VIA does not interpret; `offset` and `len` locate them in the
> connection's raw log. Each raw unit is attributed when it is written, by
> the route that knows it, to a session and turn, a session, or nothing; a
> per-turn process's traffic belongs to its turn, and a shared server's
> traffic only through the route's attribution. Unattributed traffic is never
> returned (D4). `cursor` is opaque, and an omitted one starts at the
> beginning; `next_cursor` resumes after the last returned byte (before any
> byte is durable, from the beginning), and is `null` only when every
> connection in scope is sealed, its end was reached, and no connection can
> still join the scope (the turn is terminal, or the session is closed).
> `incomplete` is true when the turn, or a turn of the session, recorded
> `raw_log.incomplete`, which includes raw bytes whose attribution could not
> be recorded. `limit` counts entries (default
> 100, max 256). A page returns at most 128 KiB of raw bytes, so it always
> fits the 1 MiB bound; a unit larger than that spans pages. Missing or
> corrupt raw evidence is `store_error`.

Runtime §9 last sentence ("`logs` resolves only each selected event's
validated raw reference; never expand to the connection's bounding spans"):
replaced by A25's text. Restatements: C1 §5 `raw_spans` row (A24);
`docs/specs/vendors/codex.md:223-224`, replace "Raw extraction uses
individual event `raw_ref`s, never shared-connection bounding spans." with
"Raw extraction (`logs`) returns only units the Route attributed to the
thread's session and turn, never shared-connection bounding spans;
untagged connection traffic stays unattributed." [t4r5.7, t4r5.13]

**T4-A28. `steps` table; event columns.** Amends runtime §6.

Runtime §6 target table: replace the `events` row with "`events`: separate
`turn` and `type` columns (late and time stay in the event JSON) |
`via-jm4.7.8`"; add a row "`steps`: `(session_id, turn, step, started_ms,
ended_ms, tokens)`, primary key `(session_id, turn, step)`, one row per
completed model step, written by the single writer, the last in the terminal
transaction; a session's rows are removed by one keyed delete |
`via-jm4.7.8`"; add a row "`connections` with `kind` (`turn`, `session`,
`shared`) and `raw_spans` attributing raw units of `session` and `shared`
connections to a session and turn (T4 design §4.4) | `via-jm4.7.8`"
[t4r5.7]. The 4 GiB quota paragraph is unchanged: Task 4 builds it as
written [t4r5.4]. Runtime §6 write-ordering item 4: replace "Adapter emits
observation; Core commits acceptance or events" with "Adapter emits
observation; Core commits acceptance, durable events or a step row, or folds
it into the progress snapshot".

**T4-A29. Observations after R2; the stall closes the hop.** Amends C2
summary A1 and `observations` rows, §1 rule 6, §2, §4, §7; runtime §8; C1
§8.2.

C2 summary table A1 row, replace the decision text with:

> Backpressure: per-session observation channel of 1024 items and 4 MiB; a
> full channel blocks only that session's normalizer; control and sticky
> health travel separately and stay serviceable; Core failing to drain for
> `event_stall_ms` (10 s) fails the turn `overflow`: the adapter closes the
> session's route hop; a private route fails the connection, which
> interrupts the vendor, and a shared route quarantines that thread
> generation as for an ingress overflow (§4) while other threads continue;
> L5 staging overflow fails the connection (coding-style §5). A durable
> event payload is at most 256 KiB encoded, else protocol failure with
> raw evidence; IDs, names, stop reasons and codes are at most 1 KiB each.
> Unknown and unattributed messages produce no observation.

C2 §1 rule 6: replace with "6. Unknown vendor notifications produce no
observation: they are kept only in the raw log and, when the route
attributes them to a turn, update that turn's activity time; a malformed
known message is a `protocol` observation." [t4r5.11]

C2 §2 "Independent lanes" bullet: replace "The 1024-item observation queue
also has a 4 MiB budget, and no payload exceeds 256 KiB encoded. Split text
at UTF-8 boundaries while preserving order; another oversize known payload
fails protocol with raw evidence. An unknown payload retains at most 16 KiB
with an explicit truncation marker." with "The 1024-item observation queue
also has a 4 MiB budget; a durable event payload over 256 KiB encoded fails
protocol with raw evidence."

C2 summary table (`docs/specs/adapter-contract.md:40`) [t4r6.13], row
`observations`: replace "C1 events minus Core fields, plus
`turn.vendor_terminal`, `turn.accepted`, `tool.quiescent`" with "the durable
C1 event payloads an adapter reports (`action.denied`,
`vendor.request_declined`, `steer.delivered`, `warning`), `progress` and
`final_text`, plus `turn.vendor_terminal`, `turn.accepted`,
`tool.quiescent` and the other internal observations of §4".

C2 §4, replace the first paragraph with:

> `Observation` = the C1 event payloads Core commits (`action.denied`,
> `vendor.request_declined`, `steer.delivered`, `warning`), at most one
> `progress` item per vendor message that carries a progress mark,
> `final_text` pieces, plus internal ones Core turns into commits:

in the `turn.vendor_terminal` row replace "`final_text`" with "`final_text?`
(when present, it replaces any text accumulated from `final_text`)", and add
the rows:

> | `progress` | `at`, `model: bool`, `tools_started: [(id, name)]`, `tools_ended: [id]`, `usage?: (key?, total)` | no commit: Core folds it into the running turn's progress snapshot and commits a `steps` row when a step ends (C1 §3.7). `model` marks model output (text, reasoning or a tool request); `usage` is an interval sample, never a cumulative total. A message with no mark sends no item |
> | `final_text` | `key`, `text`, `replace: bool` | no commit: Core appends the text to, or replaces, that key's part of the envelope's final text, metering it against the 1 MiB envelope; the item that would exceed it fails the turn `overflow` at once (C1 §5). Sent only when a vendor's final text spans messages |

C2 §7 item 6 [t4r6.13]: replace "Tool completion and other evidence may
arrive afterward, retain the original turn ID and become `late` after Core
terminal commit." with "Tool completion and other evidence may arrive
afterward and keep the original turn ID: a tool completion counts for P7
cleanup and stays raw-log evidence of that turn; a durable observation is
committed `late` after Core terminal commit." Item 7: replace with "7.
Every committed observation resolves its `raw_ref`, except declared
synthesized ones." Item 8: replace "`vendor.other`" with "raw-log-only
activity". Item 12: replace "a stall past
`event_stall_ms` yields an interrupt and `overflow`" with "a stall past
`event_stall_ms` closes the session's route hop: a private route fails the
connection `overflow`; a shared route quarantines the thread generation
(§4)".

Runtime §8, row "C2 observation payload": replace with "256 KiB encoded for
a durable event payload; IDs, names, stop reasons and codes 1 KiB | Fail
protocol with raw evidence; unknown messages keep no payload". Row "C2
observations": replace "Core fails `overflow` and interrupts (A1)" with "the
adapter closes the session's route hop; a private route fails the connection
`overflow`, a shared route quarantines the thread generation (A1, C2 §4)".

C1 §8.2 `overflow` row: replace "this session's event channel stalled past
its limit (C2 A1)" with "this session's observation channel stalled past
its limit, or the turn's envelope accumulation exceeded 1 MiB (C2 A1, §5)".

Withdraws A19's stall trigger: the stall never becomes a stop order. A34
adds `StopCause::Overflow` for the envelope overrun only. Spec restatement
[t4r5.8]: `docs/specs/vendors/claude-code.md:305`, replace "At 10 s stalled
observations Core fails overflow and interrupts;" with "At 10 s stalled
observations the adapter closes the route hop and the route fails the
connection `overflow`, which interrupts;". Unchanged and consistent:
`docs/specs/vendors/codex.md:276`, `:310-311` (stall → thread quarantine),
`docs/specs/vendors/opencode.md:550`, `docs/specs/via-api-v1.md:657`.
Restatements in code: `crates/via-adapters/src/runtime.rs:312-331` (`deliver`),
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
  envelope a transaction may carry (itself at most 1 MiB, C1 §5) or the
  turn's step rows it carries | Split event batches without splitting a
  lifecycle atomic batch; refuse a larger request before it is queued".
- C1 §5 [t4r5.6], replace "On overflow Core fails the turn with class
  `overflow`, persists a bounded failure summary and leaves the raw log as
  evidence" with "On overflow Core stops the turn at once, fails it with
  class `overflow` and persists a bounded failure summary, in which every
  member has its own budget: `failure.message` up to 2 KiB, cut at a
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
- **Proof.** A drive's terminal is at most `ENVELOPE_MAX +
  TERMINAL_EXTRAS_MAX + TERMINAL_ROWS_MAX` = 1 MiB + 196 KiB and the failure
  batch at most `ENVELOPE_MAX + TERMINAL_EXTRAS_MAX + 8 × CANCEL_RECORD_MAX`
  = 1 MiB + 320 KiB; each summand is measured where it is built (§6.5);
  `LATCH_BYTES` covers the largest (§6.1). The summary's arithmetic is in
  §6.5.
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
peer that never reads cannot hold reply memory against dispatch (§5.2).

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
> | unknown notification | raw log only, no observation; moves the turn's activity time; cannot advance lifecycle or the idle timer |

and replace "Never synthesize `file.changed` … the tool event suffices." with
"VIA does not report file changes." and delete "Do not expose private
chain-of-thought as `reasoning.summary`." (no text is exported). §6: replace
"256 KiB known observation (split text only)" with "256 KiB durable event
payload".

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
> send `agentMessage` deltas as C2 `final_text` appends keyed by item ID and
> the completed text as that key's replacement, so completed text never
> duplicates its deltas [t4r6.4].

and replace "unknown notifications become `vendor.other` retaining at most
16 KiB with explicit truncation" with "unknown notifications are raw-log-only
activity", and "Large text splits on UTF-8 boundaries;" is deleted.

Codex §5 late detail [t4r5.13] (`docs/specs/vendors/codex.md:230-234`):
replace "Thus an already received or later delivered completion after lease
release still reaches its original turn with `late:true`; unsubscribe does
not promise more vendor notifications. A detached session's Core event sink
remains eligible for these late observations even though admission to that
session is closed." with:

> Thus an already received or later delivered completion after lease
> release is still attributed to its original turn: its raw units carry
> that attribution (`logs`), it still counts for P7 cleanup
> (`tool.quiescent`), and any durable observation it yields
> (`action.denied`, `vendor.request_declined`, `warning`) is committed with
> `late:true`. A tool completion alone is no event (C1 §6.1). `thread/unsubscribe`
> does not promise more vendor notifications. A detached session's Core
> observation sink remains eligible for these late observations even though
> admission to that session is closed.

and [t4r6.13] in `docs/specs/vendors/codex.md:356-357` replace "Later tool
events remain `late:true` evidence" with "Later tool completions remain
raw-log evidence of the turn", and in the `codex_two_threads` row (`:395`)
replace "Deliver an A completion after uncertain settlement and
again after A lease release while B is active: both retain A's original
TurnNo and late:true, never session-level/B;" with "Deliver an A tool
completion after uncertain settlement and again after A lease release while
B is active: both stay raw-only evidence attributed to A's original turn
(`logs`), count for A's P7 cleanup, produce no event, and never reach
session level or B;".

`docs/specs/vendors/opencode.md` §5, replace "Map text deltas and
authoritative part snapshots without appending the same text twice. Correlate
tools by session/message/part/call IDs; state is
pending/running/completed/error. Keep partial text distinct from final text. Unknown notification
types become bounded `vendor.other`;" with:

> Map assistant text and reasoning parts to C2 `progress` `model`, a tool
> part entering `running` to `tools_started (call ID, tool name)` and one
> entering `completed` or `error` to `tools_ended`, correlated by
> session/message/part/call IDs; each assistant message's token snapshot is
> a `usage` sample keyed by message ID (§7 one ledger). Final text is the
> correlated completed assistant's text, sent as a C2 `final_text`
> replacement keyed by message ID [t4r6.4]. Unknown notification types are
> raw-log-only activity;

and replace "`session.next.*` events are retained as bounded `vendor.other`,
never a second text/tool/usage emission" with "`session.next.*` events are
raw-log-only, never a second text/tool/usage emission".

**T4-A34. Stop cause `overflow` for an envelope overrun** [t4r5.5,
t4r6.5]. Amends T3 §2 (`t3/design.md:148`, `:165-171`, `:185-187`,
`:252-254`) and its disposition table; C1 §7.6.

- **Cause.** T3 §2's cause list becomes "`cancel`, `close`, `idle_deadline`,
  `store` or `overflow`". Core sends `overflow` only on the item that would
  take the turn's envelope over 1 MiB (§6.5); the stall does not use it
  (A29).
- **Deadlines.** T3's table gains the row "| `overflow` | now | `min(now +
  3 s, wall_deadline + 3 s)` |", as for `store`.
- **Coalescing.** T3's rule stands (the first `requested_at` stays, the
  earlier `force_at` and `close_by` win, no second interrupt), with causes
  ranked `store` > `overflow` > the rest: `overflow` replaces `cancel`,
  `close` or `idle_deadline`, never `store`; `store` replaces `overflow`.
  Code: `TurnStop::attach` (`crates/via-core/src/engine/queue.rs:116-120`
  [V]) gains the `overflow` arm beside `store`'s.
- **Disposition.** T3 §2 gains, after "Cause `store` overrides every other
  cause's row": "Cause `overflow` overrides every row except cause
  `store`'s: once Core has recorded the overrun, the result is
  `failed(overflow)`, `stop_reason: error`, whatever evidence follows (a
  vendor terminal of any status, a deadline, a forced stop, a process
  exit)." T3's table (`t3/design.md:279-280`) gains "cause `overflow` (any
  evidence) | `failed(overflow)`, `stop_reason: error`, the bounded summary
  (A30) | `forced` or `requested`, cleanup by the rule", and its "daemon
  force took over" row adds "for cause `overflow`, `failed(overflow)`".
- **C1 §7.6.** Add before the first row: "| Envelope accumulation exceeded
  1 MiB (§5) | running | `failed(overflow)`, `stop_reason: error`, the
  bounded summary; Core stops the turn at once, and no later vendor terminal
  or deadline changes the result. Only a turn write that already failed
  (`failed(store)`, §8.2) takes precedence |".
- **Code.** `StopCause` (`crates/via-routes/src/lib.rs:266` [V]) gains
  `Overflow`; the matches and checks on the cause in
  `crates/via-core/src/engine/terminal.rs:152`, `:170`, `:245`, `:263` [V]
  gain an arm, and `forced_terminal` (`crates/via-core/src/engine/stop.rs:396`
  [V], beside the idle branch) fails it `overflow`. Restatements:
  `t3/design.md:65` (stop order owner row, unchanged); C1 §8.2 `overflow`
  row (A29).

**T4-A35. An escaped JSON object key is at most 4 KiB** [t4r5.3]. Extends
A10; amends C1 §1 structure limits and runtime §8's JSON row. It is a new
caller and vendor limit, not required by R1–R7, and awaits the owner
(Q-R5-12) [t4r6.15].

C1 §1, after "Parse JSON with depth at most 64 and 65,536 nodes per
document before constructing an unbounded value; reject an excess as the
named parse or parameter error.", add "An object key that contains an
escape sequence is at most 4 KiB; a longer one is the same error."
Runtime §8 row "JSON structure" (`docs/specs/runtime-contracts.md:1008`):
replace "depth 64, 65,536 nodes per document" with "depth 64, 65,536 nodes
per document, an escaped object key at most 4 KiB". Reason: serde_json copies an
escaped key into its growing scratch buffer before any visitor sees it
(`serde_json-1.0.151/src/de.rs:2215-2221` [V]); this cap is what bounds that
buffer (§2.2 rule 6, §5.1). Unescaped keys of any length are unaffected.
Restatement: `docs/specs/vendors/codex.md:263-264`, replace "JSON depth 64
and 65,536 nodes." with "JSON depth 64, 65,536 nodes and escaped object keys
of at most 4 KiB." In the same sentence, replace "256 KiB observation
payload" with "256 KiB durable event payload" [t4r6.13].

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
`core.progress.publish`, `Store::read_count()`, `store.read.delay_ms`,
`VIA_TEST_QUOTA_BYTES`, `VIA_TEST_EVENT_STALL_MS`, `VIA_TEST_PARTIAL_LINE_MS`,
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
| `s1_progress_snapshot_adds_no_store_read` [t4r5.14] | `status` on a running turn and on an idle session issue the same number of Store reads (`Store::read_count()`): building `progress` adds none |
| `s1_c1_status_latency_under_bounded_store_delay` [t4r5.14] | with `store.read.delay_ms` = 200, `status` on a running turn answers within 200 ms plus 100 ms, while the turn keeps making progress |
| `s1_progress_tokens_sum_per_step_and_label_scope` | two `usage` samples in one step replace (no key), steps add; `tokens.scope` equals the fake's declared scope |
| `s1_progress_tools_overflow_and_untracked_end_count` [t4r6.9] | 70 concurrent tool starts: 64 names and `tools_overflow` true; the end of an untracked tool, then model output, advances `current_step` and writes a row; `phase` stays `tools` until that boundary, which clears both |
| `s1_progress_unknown_messages_send_no_observation` [t4r5.11] | a flood of unknown fake messages sends no C2 item (channel high-water 0) yet moves `last_activity_at` |
| `s1_progress_step_rows_survive_crash_to_last_commit` | daemon killed after row 2's commit and before row 3: restart shows rows 1–2 and the turn `unknown` |
| `s1_progress_step_commit_refused_rows_ride_in_terminal` [t4r5.2] | `store.commit.step` known failure on row 2: `failed(store)`; rows 2, 3 and the open step's row commit in the resolution transaction with `turn.ended`; every step has a row |
| `s1_progress_many_steps_all_have_rows` [t4r5.1] | 20,000 fake steps: 20,000 rows, no warning |
| `s1_progress_forced_shutdown_terminal_carries_open_row` [t4r6.8] | a turn forced by final shutdown in step 3: its forced terminal commits row 3 with `turn.ended`; rows 1–3 all present |
| `s1_store_steps_delete_is_one_keyed_range` | `EXPLAIN QUERY PLAN` of the delete uses the primary key; two sessions interleaved, one deleted, the other intact |
| `s1_c1_status_every_member_after_eviction_and_restart` | durable members equal before and after; `progress` null after restart; `steps` paged |
| `s1_c1_status_alive_false_after_exit_before_control_drop` | Sol's stale-liveness case: exit observed, control still upgradeable → `alive` false |
| `s1_c1_events_page_filters_and_bounds` | window, `types`, `turn`, `next_after` across filtered rows, `more`; the byte bound exercised with a Store-level fixture of large rows (the C2 256 KiB limit is a separate test) |
| `s1_c1_follow_and_unsubscribe_are_refused` | `follow: true` is `invalid_params`; `unsubscribe` is `method_not_found` |
| `s1_c1_logs_pages_raw_bytes_by_cursor_and_isolates_sessions` | a 1 MiB control-character unit spans pages under 1 MiB each; a cursor for another session's connection is `invalid_params` |
| `s1_c1_logs_end_cursor_resumes_while_running` [t4r5.12, t4r6.11] | a call before any byte is durable returns `r1.start`, and a later call from it returns the first bytes; on a running turn `next_cursor` is non-null at the current end and a later call from it returns only new bytes; after the terminal and seal it is `null` |
| `s1_store_logs_serve_only_attributed_spans` [t4r5.7] | Store fixture with a `shared` connection carrying spans of two sessions and uncovered units: each session gets only its spans; uncovered units never appear |
| `s1_store_commit_spans_rejects_overlap_boundary_and_owner` [t4r6.12] | overlapping spans, a span ending inside a unit, and a span naming another session's turn each reject the whole batch with `Constraint("raw_spans")`; nothing is stored |
| `s1_raw_span_batch_waits_and_refusal_is_visible` [t4r6.10] | with the Store stalled, a span-batch unit test fills both batches and waits rather than dropping a span; a refused batch fails the connection, and `logs` then reports `incomplete: true` |
| `s1_store_quota_refuses_ordinary_and_keeps_lifecycle` [t4r5.4, t4r6.3] | lowered quota: concurrent raw and SQLite writers never take `used + reserved` past the limit; a raw unit past `QUOTA − LIFECYCLE_RESERVE` fails the connection with `raw_log.incomplete`, the turn's terminal still commits from its hold, a new spawn is `store_error` `not_committed`; the seeded ledger equals the files after restart |
| `s1_store_quota_sqlite_bound_covers_measured_growth` [t4r6.3] | heavy: every SQLite command of the scenario suite settles with measured growth at most its `sqlite_bound` |
| `s1_c1_request_id_over_256_bytes_is_invalid_request` | A31 |
| `s1_c1_reply_not_read_closes_the_socket_and_frees_the_permit` | A32, with `VIA_TEST_REPLY_WRITE_MS` |
| `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` | four turns, 256 MiB stdout flood; daemon peak RSS < 256 MiB, growth < 32 MiB after the first 64 MiB, each anchor ≤ 32 MiB, sum < 384 MiB, `MemoryBudget` high-water ≤ 128 MiB with the SQLite cache's 8 MiB held from open [t4r6.2]; `daemon/status`, `status` and a `cancel` of another turn answer within 100 ms, including with its interrupt write blocked at `HoldStdin`; the flood turn ends `failed(overflow)` with `raw_log.incomplete` |
| `s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output` | Core held at `core.observations.pause`, vendor silent after filling the channel: `overflow` at the lowered stall with no further vendor byte |
| `s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib` | the C2 bounds |
| `s1_f24_envelope_overrun_stops_the_turn_at_once` [t4r5.5, t4r6.4, t4r6.5] | a test route emits declined requests past 1 MiB and then stays silent, and another streams `final_text` appends past it: the stop order with cause `overflow` is sent on the crossing item; a vendor `completed` terminal that follows still ends `failed(overflow)` |
| `s1_bounds_failure_summary_bounds_every_member` [t4r5.6, t4r6.6, t4r6.7] | a turn with 1 MiB escaped `final_text`, a 64 KiB `failure.message`, 1 KiB vendor fields, a 6 KiB + 1 `model`, 2,000 `extra_write_dirs`, a 64 KiB `vendor` map and 600 KiB of declined requests: `final_text` empty with its length in `truncation`, the message cut to 2 KiB, each member within its budget, lists as prefixes, `truncation` totals exact, the summary at most 720 KiB; a 1 KiB + 1 vendor field is `protocol` |
| `s1_bounds_buffers_never_grow` [t4r5.3, t4r6.1] | one counting-allocator binary: no reallocation on the stdout, C1 line, decode, reply or encode paths at their maxima; a 1 MiB vendor string of six-byte escapes allocates and charges exactly its decoded length; C1 decode of a maximal escaped 16 MiB prompt stays within §5.3's 33.7 MiB; Route decode of a 1 MiB message within §5.4 |
| `s1_bounds_escaped_key_over_4_kib_is_refused` [t4r5.3] | C1 `parse_error` and vendor `protocol`; a 1 MiB unescaped key is accepted by the scan |
| `s1_f05_…` (oversize, depth and nodes, partial line, 33rd socket) | F5 |
| `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_raw_bytes` | F27, with a seeded splitter test |
| `s1_raw_…`, `s1_store_…`, `s1_wire_…`, `s1_blob_…` | round 4's mechanism tests for kept mechanisms: group commit by count, death guards, lanes and fence, Latch fit with the largest `cwd` (end to end with the longest creatable path; the 4096-byte bound with synthetic records, fixing Sol's impossible fixture), Public saturation at the Store level (not through sockets), binary lookup, finish adoption, torn and mismatched blobs, replay compare outside `admission` with a stalled reader |

## 13. Task 4 scope disposition (`t4/t0.md`)

| Item | Round 7 |
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
| Blob path, schema v6, 256 KiB rule, memory charges, Store shutdown ordering (round-1 additions) | kept; 256 KiB rule narrowed to durable payloads (A29); memory charged by capacity (§5) |
| Runtime §6 disk quota | built in Task 4 (§6.9) [t4r5.4] |

## 14. Limitations

| Limitation | Revisit when |
|---|---|
| A maximal 16 MiB C1 request needs about 33.7 MiB of the 34 MiB dynamic partition, so it is admitted only when little other C1 or reply memory is held; otherwise `admission_refused` | real routes measure prompt sizes |
| `running_tools` lists at most 64 names plus an overflow flag; a missed tool end misreports `phase` until the next step boundary; tokens are approximate under the route's declared scope | a caller needs exact live figures |
| Per-step tokens are unprobed for Claude, Codex and OpenCode (§2.5); until each probe passes, `tokens` may be `null` | each vendor slice's probe; the owner's accuracy decision (Q-R5-11) |
| The Route side of `session` and `shared` attribution is specified, not built; under a slow Store its span batches hold back the connection | the Codex and OpenCode slices |
| `sqlite_bound` (§6.9) is inferred; a larger measured growth is recorded and warned, and the reserve's 11 MiB of slack absorbs it | the bound test fails |
| A step that ended while its row commit was in flight is lost in a crash | never; R4 accepts it |
| The open-session tally is exact only until an uncertain close | Store-failure recovery work |
| `stamp` and `ord` increase strictly only while no `sessions` row is deleted | retention (`via-jm4.18`): they then need a counter row |
| Blob verification at start is linear in referenced blob bytes | retention |
| `revision` is 0; `process.idle_since` is `null` (A16) | late evidence; vendor idle shutdown |
| Allocator overhead and SQLite's allocations other than its page cache are uncharged; the RSS gate measures them | the RSS gate fails |
| `list` phase 2 examines every `ord` up to `w0` | session counts make it slow |
| Inferred: the §6.2 lifecycle count, `TERMINAL_EXTRAS_MAX`, `CANCEL_RECORD_MAX` and the 128 B step row at their largest, the drive reserve total (§5.4), the 24 B per node in §5.3, `sqlite_bound`, and that `json_limits::scan` and serde_json agree on token boundaries | the checking test fails |
