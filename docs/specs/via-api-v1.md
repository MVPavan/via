# VIA API v1 (contract C1)

Status: draft 2, 2026-09-26; the owner approved the S1 set on 2026-09-26
(see Decisions below). Public contract between
callers and VIA; implemented by L1 (`via-cli`, server half) over L2
(`via-core`). Inputs: `docs/brainstorms/README.md` §15
(authoritative), review `docs/brainstorms/reviews/contract-specs-astra-r1.md`,
probes P1–P5 (summaries in `docs/workstreams/rust-foundation/session-handoff.md` §6; single runs on cheap models: evidence,
not guarantees), `.repo-context/coding-style.md` §3, §5–§7. Companion:
`docs/specs/adapter-contract.md` (C2). Labels: **decided** = owner
decision; **Proposed** = drafted here, listed in §10.

## Summary for review

**Shape.** JSON-RPC 2.0, newline-delimited, over the daemon's user-only Unix
socket; `via serve --stdio` proxies the same messages. Entities: **session**
(one conversation, one vendor session, one route and adapter version for
life) and **turn** (one prompt → one envelope), addressed `s_7f3/2`. The
caller generates the session's handle; the daemon stores only its hash.

**First-release scope (owner, 2026-09-26):** Claude Code, Codex and OpenCode;
all methods below are in scope. Each route truthfully declares native, partial
or unsupported vendor behavior. ACP references describe future coverage, not
a release gate. Language, coding standard, testing policy and S1 scope are
approved; vendor-dependent decisions remain recorded in the tables below.

| Method | CLI | Handle | Retry-safe by | Result |
|---|---|---|---|---|
| `hello` | (implicit) | no | — | daemon and API version |
| `describe` | `via describe` | no | no side effects | route plan |
| `spawn` | `via spawn` | caller-supplied | `idempotency_key` + same handle | spawn receipt (session, turn 1) |
| `resume` | `via resume` | yes | `op_key` | turn receipt (queued or running) |
| `steer` | `via steer` | yes | `op_key` | delivery kind |
| `cancel` | `via cancel` | yes | idempotent | cancel outcome + cleanup certainty |
| `close` | `via close` | yes | idempotent | session closed |
| `status`, `wait`, `result`, `list` | same names | no | read-only | status, envelope, envelope, page |
| `events`, `logs` | same names | no | read-only | canonical events (page/follow), raw excerpts |
| `models`, `daemon/status`, `daemon/stop` | `via models`, `via daemon …` | no | read-only / idempotent | catalog, daemon state |

| Entity | States |
|---|---|
| Session | `idle`, `active`, `closed` (internal admission gate: `open`, `closing`) |
| Turn | `queued`, `running` (phase `submitting` or `accepted`), `completed`, `failed`, `cancelled`, `unknown` |
| Cancel | outcome `requested`, `acknowledged`, `forced`, `unknown`; cleanup `quiescent`, `uncertain`, `pending` |

| Piece | Key points |
|---|---|
| Identifiers | `s_` + 12 base32; turn `s_…/N`; vendor session id opaque; handle `h_` + 43 base64url, caller-generated, hashed at rest |
| Envelope | state, failure class, stop reason, cancel outcome and cleanup, final text, structured output, denied actions, auto-declined requests, route and versions, usage and cost with per-field scope, event range, raw spans, `revision` |
| Events | `type` tag, per-session dense `seq`, `turn` nullable for session events, `late` flag, optional `raw_ref` |
| Errors | request errors: JSON-RPC `error` with stable `data.kind`; turn failures: `failure.class`. After a receipt, a persistent Store failure that prevents a terminal commit returns `store_error` with `terminal_persisted:false`, never a fabricated terminal envelope |
| Evolution | additive; unknown request fields rejected; every wire enum decodes unknown values into an explicit `unknown(raw)` fallback |

**Decisions** (recommendation first, alternatives in §10). **Owner,
2026-09-26:** P1–P6, P8–P10 and P12 approved as written; `idempotency_key`
and `op_key` stay separate. P7 (S3/S4), P11 (S3/S5) and P13 (S2) are
decided in the slice that needs them, after re-probing.

| # | Decision | Recommendation |
|---|---|---|
| P1 | Foreground `via spawn` prints the receipt line, then the envelope (`--json` = two NDJSON lines); `--background` prints only the receipt | as written |
| P2 | Handle: caller-generated, sent with `spawn`; CLI takes it from `--handle-file`, stdin or `VIA_HANDLE`, never argv by default; CLI generates one when absent and prints it only in the receipt | as written |
| P3 | Read verbs need no handle (same-OS-user boundary) | as written |
| P4 | `idempotency_key` scope: per Store, session lifetime; same key + same handle hash + same params → same receipt; else `invalid_params` | as written |
| P5 | Per-session: `model`, `cwd`, `instructions`; per turn: `effort`, `output_schema`, `deadlines`, `max_steps`, `bound` (D5); omitted = inherit from the latest accepted turn | as written |
| P6 | Queue limit 8; no dispatch while the predecessor is `running`, `cancel.cleanup: pending`, or unresolved; queued turns behind an `unknown` turn are cancelled | as written |
| P7 | Codex interrupted terminal evidence acknowledges cancel, but open tools keep the turn nonterminal with cleanup `pending` until tool quiescence or `min(acknowledged_at + 60 s, turn.wall_deadline)`; settle `quiescent` if proved, otherwise `uncertain` and dispatch with a warning. With no remaining wall budget, settle immediately. Late tool completion does not rewrite the settled envelope | as reviewed in `docs/specs/vendors/codex.md` §9 |
| P8 | Error code table §8.1 | as written |
| P9 | Deprecation: kept for one minor release minimum; removed only in v2 | as written |
| P10 | Socket `$XDG_RUNTIME_DIR/via/via.sock` else `~/.via/run/via.sock`; 0700/0600; peer uid check both ends | as written |
| P11 | Codex owned stdio server key is `(codex, observed_binary_version, config_hash)` without bound; `config_hash` includes VIA-controlled startup/environment configuration, not credentials. Every turn sets `sandboxPolicy`; mixed-bound sharing waits for pinned enforcement proof. OpenCode keys include the full effective bound, VIA owner session and durable private namespace; no cross-owner server sharing or live-session migration. Bound-keyed routes refuse bound changes on resume | as reviewed in `docs/specs/vendors/codex.md` §9 and `docs/specs/vendors/opencode.md` §2 |
| P12 | Live recovery after daemon restart is `unknown` for every route in v1; `resumed` only when a route's rejoin is probe-verified on the configured transport (Codex stdio server dies with the daemon, P3) | as written |
| P13 | Version gate: tested sets or ranges per route (Claude initially exactly `{2.1.283}`); outside them version status is `untested` and bound-bearing spawn/resume are refused unless immutable session policy `allow_untested` is true. Read/cleanup verbs remain available | as reviewed in `docs/specs/vendors/claude-code.md` §10 |

## 1. Scope, transport, versioning

- **Transport (decided, D1).** JSON-RPC 2.0 over the daemon's Unix socket,
  one JSON object per line, UTF-8, line length capped (Proposed 16 MiB).
  Requests carry `id`; notifications flow daemon → client only for follow
  (§3.11). No batches. `via serve --stdio` forwards messages unchanged.
  Parse JSON with depth at most 64 and 65,536 nodes per document before
  constructing an unbounded value; reject an excess as the named parse or
  parameter error. The 16 MiB limit includes the line feed.
- **Socket (P10).** Directory validated (owner, 0700, no symlink); socket
  0600; both ends verify the peer uid (coding-style §6).
- **Local paths.** The daemon and CLI resolve `VIA_STATE_DIR` and
  `VIA_RUNTIME_DIR` once at startup, with defaults, exact layout and locks
  specified by [runtime §6.1](runtime-contracts.md). These are local process
  settings, not C1 fields or vendor environment passthrough. `daemon/status`
  reports the resolved socket and Store paths. The CLI compares the existing
  daemon's Store path with its expected path after hello/status and exits 4 on
  mismatch without stopping that daemon.
- **Handshake (decided).** First request must be `hello`; else
  `handshake_required`.

```json
{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":1,"client_version":"0.1.0","client":"via-cli"}}
{"jsonrpc":"2.0","id":1,"result":{"api_version":1,"daemon_version":"0.1.0","daemon_pid":4242,"deprecations":[]}}
```

- **Version mismatch (decided).** One binary; a differing `client_version`
  is `version_mismatch`. The CLI restarts an idle daemon, else reports.
- **Evolution (decided).** Additive within v1: optional params, result and
  event fields, event types, error kinds, methods. Requests use
  `deny_unknown_fields`. Clients ignore unknown result and event fields.
  Every enum on the wire (event `type`, states, classes, kinds, scopes,
  supports) is decoded with an explicit fallback variant that keeps the raw
  string (Rust: hand-written `Unknown(String)`; `#[serde(other)]` is not
  enough, coding-style §3). Field rules: `?` = optional, omitted when
  absent; nullable fields are always present; everything else required.
- **Deprecation (P9).** Listed in `hello.result.deprecations`
  `{what, since, use_instead}`; use adds a `deprecated` warning.
- **CLI mapping.** One verb per method. `--json` prints NDJSON: for
  foreground `spawn` the receipt line then the envelope line (P1); other
  verbs one line. Exit codes: 0 success; 2 request error; 3 turn ended
  `failed`, `cancelled` or `unknown`; 4 daemon unreachable; 130 foreground
  wait interrupted (the session keeps running; the receipt was printed).
  `via daemon` starts the foreground server; `via daemon status` and
  `via daemon stop` remain client verbs.

## 2. Identifiers and the caller handle

| Id | Form | Minted by |
|---|---|---|
| Session id | `s_` + 12 lowercase Crockford base32 (60 random bits) | Core, before dispatch |
| Turn address | `<session id>/<N>`, N ≥ 1, dense | Core |
| Vendor session id | opaque, scoped to `(harness, adapter_version)` | vendor |
| Caller handle | `h_` + 43 base64url chars (256 random bits) | **caller** (P2) |

**Handle (decided, D5; P2).** Bearer authority for `resume`, `steer`,
`cancel`, `close`. The caller generates it, sends it in `spawn.params.handle`,
and the daemon stores only a SHA-256 hash; the daemon never returns a
handle, so losing the receipt loses nothing the daemon could restore. Read
methods need none (P3). Trust boundary: it stops one same-user caller from
mutating another's session by accident or guessing; it is no defence
against same-user processes that read the caller's memory or files. A
handle never appears in tracing, events or envelopes.

**CLI callers (P2).** Sources in order: `--handle-file F`, `--handle-stdin`,
`VIA_HANDLE`, `--handle <h>` (explicit opt-in to argv exposure). `via spawn`
without a source generates one and prints it in the receipt, the only time
it is shown. The CLI stores nothing.

## 3. Methods

`session` = session id; `turn` = turn address or session id (latest turn at
call acceptance). Timestamps RFC 3339 UTC; durations ms. `op_key`
(optional; 1–64 printable ASCII characters, 0x21–0x7E, so characters equal
bytes) on `resume`, `steer`, `close`: the daemon
keeps `op_key → result` for the session's lifetime and replays it on a
repeat, so a lost response is safe to retry; `status` lists queued turns
with their `op_key` for reconciliation.

### 3.1 `describe` — preflight, no side effects

CLI: `via describe --harness codex --model M --bound full --network [--require steer,cancel] [--allow-untested] [--vendor codex.k=v]`

Params: `harness?`, `model?` (one required), `bound?`, `require?`,
`vendor?`, `cwd?`, `allow_untested?` (default false). Result, a **route plan**; `capabilities` is the DTO of
§4.1:

```json
{"harness":"codex","model":{"requested":"gpt-6-sol","resolved":"gpt-6-sol"},
 "route":"codex-app-server","adapter_version":"0.1.0","vendor_version":"0.157.1",
 "version_status":"tested","capabilities":{…},
 "effective_bound":{"mode":"full","extra_write_dirs":[],"network":true},
 "refusals":[],"warnings":[]}
```

Never starts a process or server (Q5). Errors: `unknown_model`,
`harness_unavailable`, `invalid_params`.

### 3.2 `spawn` — new session and turn 1

CLI: `via spawn --harness H --model M --prompt "…" [--prompt-file F|-] [--instructions F] [--bound B] [--allow-dir D]… [--network] [--cwd D] [--effort E] [--output-schema F] [--wall-ms N] [--idle-ms N] [--max-steps N] [--require V,…] [--allow-untested] [--vendor h.k=v]… [--label L] [--idempotency-key K] [--handle-file F|--handle-stdin] [--background]`

Params: §4 parameters, `handle` (required), `require?`, `label?`,
`idempotency_key?` (the same bound as `op_key`: 1–64 printable ASCII
characters, 0x21–0x7E). The daemon validates, resolves the model, runs the
preflight, commits session + turn 1 (`queued`) + handle hash + key in one
Store transaction, then returns the receipt; dispatch follows.

```json
{"session_id":"s_7f3k9q2mzr4c","turn":"s_7f3k9q2mzr4c/1","state":"queued",
 "route":"codex-app-server","adapter_version":"0.1.0","vendor_version":"0.157.1",
 "version_status":"tested","capabilities":{…},
 "effective":{"model":"gpt-6-sol","effort":"high","bound":{…},"deadlines":{"wall_ms":3600000,"idle_ms":600000},"max_steps":null},
 "warnings":[]}
```

Errors: `invalid_params` (incl. `vendor_option_conflict`), `unknown_model`,
`harness_unavailable`, `missing_capability`, `bound_unsupported`,
`admission_refused`, `store_error`. When the daemon already retains its
bound of unresolved turns (runtime contract §8), `spawn` is refused
`admission_refused` if all are in flight, and `store_error` while any retained
turn failed to persist its terminal. Idempotency (P4): same key + same handle
hash + byte-identical params → the stored receipt; different handle or
params → `invalid_params` with `kind: idempotency_conflict`.
`allow_untested` is included in that exact retry identity. It waives only
the tested-version restriction, never unsupported bounds, protocol validation,
required capabilities, identity continuity, never-ask policy or executable-
version consistency.

### 3.3 `resume` — add a turn

CLI: `via resume <session> --prompt "…" [per-turn flags] [--op-key K]`

Params: `session`, `handle`, `prompt`, per-turn parameters (§4), `op_key?`.
`allow_untested` is inherited session policy; attempting to change it on
resume is `invalid_params`.
Result: `{turn, state: "queued"|"running", queue_position, effective: {…},
warnings}`. Effective values are frozen at acceptance (§7.3). A given
`bound` is re-validated against the route (D7), recorded on this turn, and
inherited by later turns; nothing changes it silently (D5). Errors:
`session_not_found`, `session_closed` (also while closing), `invalid_handle`,
`queue_full`, `bound_unsupported`, `unsupported_verb`, `admission_refused`.

### 3.4 `steer` — input into the active turn

CLI: `via steer <session> --text "…" [--expect-turn N] [--op-key K]`

Result: `{turn, delivery}`; `delivery` is `injected` or the declared partial
semantics (§4.1). Steer on an idle session is `no_active_turn`; a steer
that arrives while the turn is `submitting` waits for acceptance then
applies, or fails `no_active_turn` if acceptance fails. Errors:
`unsupported_verb`, `no_active_turn`, `turn_mismatch`, `invalid_handle`.

### 3.5 `cancel` — stop the active or a queued turn

CLI: `via cancel <session> [--turn N] [--force-after MS] [--wait]`

Params: `session`, `handle`, `turn?`, `force_after_ms?` (Proposed default
10 000), `wait?`. Result:

```json
{"turn":"s_7f3k9q2mzr4c/2","state":"cancelled","already_terminal":false,
 "cancel":{"outcome":"acknowledged","cleanup":"uncertain","requested_at":"…","settled_at":"…"}}
```

`outcome` (§7.4) is protocol acknowledgement only. `cleanup` says whether
side effects are known to have stopped: `quiescent` (private process group
absence positively proved under §7.5, or the vendor reported every tool
item of the turn completed),
`uncertain` (acknowledged but not provable), `pending` (still waiting for
tool completion or the cleanup deadline). For Codex, the matching
`turn/completed:interrupted` acknowledges cancel; the interrupt RPC response
alone does not. With open tools, the turn remains nonterminal while cleanup
is `pending` until `min(acknowledged_at + 60 s, turn.wall_deadline)`. If
the wall budget has expired, settle `uncertain` immediately. On tool
quiescence or that deadline, commit one `cancelled` envelope with cleanup
`quiescent` or `uncertain`. At uncertainty, warn `cancel_cleanup_uncertain`;
a successor may dispatch with `predecessor_cleanup_uncertain`. No
acknowledgement by the control deadline gives outcome `unknown`. Late tool
completion remains late evidence and does not rewrite that envelope. Codex P2/P2b: after
`interrupted` the tool's `sleep 120` ran to the 60 s poll limit;
`command/exec/terminate` does not apply to agent-started tools and
`thread/unsubscribe` does not stop them, so on a shared server a tool
survives until it finishes or the server dies. A queued
turn is dropped: `cancelled`, `acknowledged`, `quiescent`. On an already
terminal turn: `already_terminal: true` with the recorded `cancel` or
`null`. Idempotent. Errors: `turn_not_found`, `invalid_handle`,
`unsupported_verb`.

### 3.6 `close` — end the session

CLI: `via close <session> [--mode graceful|force] [--deadline-ms N] [--op-key K]`

Sets the admission gate `closing` (new `resume` → `session_closed`),
cancels the active turn, drops queued turns as `cancelled`, closes the
vendor session (`close(mode, deadline)` down to Host, D7), then sets
`closed`. Result `{session_id, state: "closed", cancelled_turns, cleanup}`.
Idempotent; a second `close` during closing waits for the first.

### 3.7 `status`

```json
{"session_id":"s_7f3k9q2mzr4c","state":"active","admission":"open","harness":"codex","model":"gpt-6-sol",
 "route":"codex-app-server","vendor_session_id":"019…","vendor_identity_verified":true,"cwd":"/work/repo","process":{"alive":true,"idle_since":null},
 "active_turn":{"n":2,"state":"running","phase":"accepted","started_at":"…","last_event_seq":57,"cancel":null},
 "queue":[{"n":3,"op_key":"k-17","queued_at":"…","effective":{…}}],
 "turns":[{"n":1,"state":"completed","revision":0},{"n":2,"state":"running"},{"n":3,"state":"queued"}],
"label":null,"created_at":"…","updated_at":"…"}
```

`vendor_session_id` is nullable and contains only the last confirmed vendor
ID. `vendor_identity_verified` is false until the current connection
generation is confirmed. A first Claude logical open may return with an
internal expected UUID while the public ID is null/verification false;
reopening may display the historical confirmed ID with verification false.
The VIA session/receipt exists independently of vendor confirmation.

### 3.8 `wait`, 3.9 `result`

`via wait <session|turn> [--timeout-ms N]`; `via result <session|turn>`.
`wait` blocks until the addressed turn is terminal (`wait_timeout` on
expiry); `result` returns the envelope now or `turn_not_finished`.
After a receipt, a persistent Store failure that prevents terminal persistence
returns `store_error` with `session`, `turn`, last-known `durable_state` and
`terminal_persisted:false`. It is a request error for the read, not a terminal
envelope. An already committed, readable terminal result is returned as is.
A `wait` whose result is still absent once final shutdown has committed its
last record ends `daemon_stopping`.

### 3.10 `list`

`via list [--state S] [--harness H] [--label L] [--since T] [--limit N] [--cursor C]`.
Ordered by `(updated_at desc, session_id)`; `cursor` is an opaque keyset
cursor over that order (stable across concurrent updates: a session updated
after the cursor was issued may appear again, never be skipped). Result
`{sessions: [summary], next_cursor}`. The page stops at both requested item
count and 1 MiB encoded bytes; an individual result that cannot fit the
bounded response is refused with `admission_refused`, never truncated.

### 3.11 `events` — page or follow

`via events <session|turn> [--after SEQ] [--limit N] [--follow] [--types T,…]`

Params: `session` or `turn`, `after?` (default 0), `limit?` (default 200,
max 1000), `follow?`, `types?`. Result `{events, next_after, more: bool,
earliest_seq, subscription?}`. Semantics:

- The page is a bounded Store scan in `seq` order from `after`, filtered by
  `types` (gaps in `seq` are expected under a filter). It stops at both the
  requested count and 1 MiB encoded bytes. `next_after` is the last scanned
  seq, including filtered-out events; `more` uses the committed head captured
  with the page. An individual result exceeding the response bound is
  `admission_refused`, never a truncated success.
- `follow: true` serializes cursor registration with commit notifications in
  the session actor after the bounded Store read. The initial page is queued
  before live notifications. The daemon rescans durable `seq > scan_cursor`
  and checks the durable head before waiting for a wake, so notifications are
  hints and the replay → live boundary has no gap. No subscription row is
  stored. It pushes each matching event as
  `{"method":"event","params":{"subscription":"sub_…","event":{…}}}`.
  The scan cursor advances across filtered events; the delivery cursor advances
  only after complete notification writes. A seq is never enqueued twice.
- Session-wide follow (Q1): covers every turn until `session.closed`;
  turn follow ends at that turn's `turn.ended`.
- Each subscription has an outbox of at most 1000 events and 1 MiB; at most
  32 subscriptions exist daemon-wide and 8 per socket, with a 16 MiB total
  outbox budget. On exhaustion, freeze it, discard unsent entries and reserve
  one termination notice outside the data outbox. The daemon attempts
  `{"method":"event_end","params":
  {"subscription","reason":"lagged","resume_after":<seq>}}` and the client
  re-requests from `resume_after`. This cursor is the last fully written
  notification seq or the acknowledged initial-page cursor, not proof that
  the client read the bytes. Clients should retain their last received seq.
  Other `reason` values: `terminal`, `unsubscribed`, `closing`, `store_error`.
  `event_end` is best-effort: finish any started NDJSON frame, attempt the
  notice within one 2 s absolute writer deadline, then close on timeout.
  Subscription and outbox ownership is released within 2 s even for a peer
  that never reads. If several subscriptions fail together, the socket may
  close after the first notice/deadline.
- History pruned by retention: `history_pruned` error carrying
  `earliest_seq` when `after < earliest_seq - 1`.
- `unsubscribe {subscription}` removes unsent entries, finishes any started
  frame within the same 2 s bound, then queues `event_end:unsubscribed` before
  the reply. No event for that subscription is enqueued after the reply.
  Connection close drops its subscriptions immediately in memory. Terminal
  detection follows the scan even when its event type is filtered out.

### 3.12 `logs` — raw-log excerpts

`via logs <session|turn> [--after SEQ] [--limit N]`. Returns the bytes each
addressed event's `raw_ref` points to (lossy UTF-8), in event order:
`{entries: [{seq, direction, connection_id, offset, len, text}], next_after}`.
Never another session's traffic (D4); only referenced spans are read. Pages
stop at both requested item count and 1 MiB encoded bytes. A single entry
too large for the bounded response is `admission_refused`, never silently
truncated; missing or corrupt referenced raw evidence is `store_error`.

### 3.13 `models`; 3.14 `daemon/status`, `daemon/stop`

`via models [--harness H]` → `{models: [{model, harness, aliases, source}]}`.
`via daemon status` → `{daemon_version, pid, started_at, sessions: {idle,
active, closing}, servers: [{harness, vendor_version, key, sessions}],
socket_path, store_path, health}`. `health` reports `healthy` or
`store_failed` with a bounded failure kind and affected IDs from memory,
without prompts, payloads or handles. `via daemon stop [--drain|--force]`: refuses
while sessions are active unless `drain` (gate every session `closing`
for new work, run accepted queued turns to completion, then stop) or
`force` (close every session with mode `force`; turns end `cancelled` or
`unknown`). `drain` with `force` is `invalid_params`; after acceptance new
work is refused `daemon_stopping`. The result `{"stopping":true}` only
acknowledges acceptance; it is not evidence that work stopped or the daemon
exited. A session closed by `force` commits `session.closed` with
`reason: "daemon_stop_force"`. Drain runs accepted turns under their existing deadlines; force and
an idle stop then share one final 10 s shutdown deadline. The daemon exits 0
only after positive cleanup, joins and durable records, otherwise 4
(runtime contract §6.2).

## 4. Canonical parameters

| Parameter | Type | Scope | Notes |
|---|---|---|---|
| `harness` | `claude`, `codex`, `opencode`, `acp:<agent>` | session | optional if `model` resolves |
| `model` | string | session (P5) | `resolved` reported in the envelope |
| `allow_untested` | bool, default false | session | immutable after spawn; describe may request a route plan using it; applies only to tested-version restriction (P13) |
| `effort` | `low`…`max` or vendor value | per turn | unknown values refused |
| `instructions` | `{text}` or `{path}` | session | native or `prepended_to_prompt` (partial) |
| `prompt` | string | per turn | required |
| `bound` | `{mode: read_only\|workspace_write\|full, extra_write_dirs: [path], network: bool}` | per turn (D5): inherited unless set on `resume` | always never-ask (D3); combinations per §4.2 |
| `cwd` | absolute path | session | must exist |
| `output_schema` | JSON Schema object or `null` | per turn | `null` clears an inherited schema; validated by VIA (Q2, draft 2020-12, size ≤ 256 KiB) |
| `deadlines` | `{wall_ms?, idle_ms?}` | per turn | Core-owned absolute deadlines (D7); defaults 3 600 000 / 600 000 |
| `max_steps` | integer or `null` | per turn | steps inside one turn (D5) |
| `require` | `[verb]` / `[verb:partial]` | spawn | preflight |
| `vendor` | `{"<harness>": {k: v}}` | as the adapter declares | passthrough, non-portable; reserved keys refused (§4.2) |
| `label` | string ≤ 120 | session | |

Inheritance (P5): a per-turn parameter omitted on `resume`
inherits from the **latest accepted turn** (queued turns included) at
acceptance, and the frozen effective values are returned in the receipt.
Cancelling a queued turn does not un-freeze its successors. Session-scope
parameters on `resume` are `invalid_params`.

### 4.1 Capabilities DTO (one shape for `describe`, receipts, C2)

```json
{"verbs":{"spawn":{"support":"native"},"resume":{"support":"native"},
          "steer":{"support":"partial","semantics":"merged_into_active_turn"},
          "cancel":{"support":"native"},"close":{"support":"native"}},
 "params":{"instructions":{"support":"native"},"output_schema":{"support":"unsupported","reason":"no schema input on this route"},
           "effort":{"support":"native"},"max_steps":{"support":"native"}},
 "bounds":["read_only","workspace_write","full"],"network_control":true,
 "recover":{"support":"unsupported","reason":"stdio server dies with the daemon"},
 "usage":{"tokens":"vendor_interval","cost":"reported_cumulative"}}
```

`support` ∈ `native`, `partial` (with `semantics`), `unsupported` (with
`reason`). `require` passes only `native` unless written `verb:partial`.
The JSON above illustrates DTO shape; route-specific current qualification
and refusals are governed by §4.2.

### 4.2 Bound combinations and precedence

| Route | `read_only` | `workspace_write` | `full` | `network: false` |
|---|---|---|---|---|
| `codex-app-server` | protocol-mapped, unverified; refuse pending pinned enforcement gate | protocol-mapped, unverified; refuse pending pinned enforcement gate | native with `network:true` | limited-bound network control only after proof; `full` + `network:false` refused |
| `claude-cli` | unqualified; refuse pending CLAUDE-BOUND-1 | unqualified; refuse pending CLAUDE-BOUND-1 | `network:true` eligible candidate, qualified only after exact live recipe continuity test | refused, including limited bounds |
| `opencode-serve` | refused (A4, D9) | refused | only with `network:true` and empty `extra_write_dirs`; nonempty `extra_write_dirs` is `invalid_params` before namespace allocation/I/O | refused |
| ACP | refused (D7) | refused | native | refused |

For pinned OpenCode 1.18.32, `describe` declares optional
`params.max_steps` unsupported with a reason. A non-null effective value is
refused by Core preflight before namespace allocation or vendor I/O as
JSON-RPC `-32602`, `data.kind: "invalid_params"`, naming the field and route.
Null/omitted values follow ordinary inheritance and clearing;
`allow_untested` does not waive this refusal. This does not remove any C1
method from the first-release surface (`vendors/opencode.md` §3).

Rules: an unenforceable combination is `bound_unsupported` naming the
route and the reason. `vendor` options that touch permission, sandbox,
approval, instructions, cwd, model or session identity are refused as
`invalid_params` kind `vendor_option_conflict` (reserved key list per
adapter in C2 §6); canonical parameters always win.
For Codex, limited bounds are omitted from `describe.capabilities.bounds`
until `via-5lr.3.4` verifies their enforcement. Its full-access eligibility
does not qualify mixed-bound sharing. The grouped `codex-cli` route is not
enabled in this first-release table. For Claude, tool permissions differ from
Bash OS sandboxing and all-tool containment. `describe` distinguishes the
temporarily refused limited bounds from the separately pending full recipe;
full is not refused by CLAUDE-BOUND-1 itself. No candidate is a live
qualification claim.

## 5. Result envelope

Immutable once terminal, except `unknown` revised by late evidence (§7.6);
`revision` counts revisions and `turn.revised` announces them.
Accumulation is bounded to 1 MiB encoded per turn, including text and
collections. On overflow Core fails the turn with class `overflow`, persists
a bounded failure summary and leaves the raw log as evidence; no successful
result is silently truncated. A terminal envelope that cannot fit the 16 MiB
socket response limit is a named `admission_refused` read error.

```json
{"api_version":1,"session_id":"s_7f3k9q2mzr4c","turn":2,"address":"s_7f3k9q2mzr4c/2","revision":0,
 "state":"cancelled","failure":null,"stop_reason":"interrupted","vendor_stop_reason":"interrupted",
 "cancel":{"outcome":"acknowledged","cleanup":"uncertain","requested_at":"…","settled_at":"…"},
 "harness":"codex","model":{"requested":"gpt-6-sol","resolved":"gpt-6-sol"},"effort":{"requested":"high","resolved":"high"},
 "route":"codex-app-server","adapter_version":"0.1.0","vendor_version":"0.157.1","version_status":"tested",
 "vendor_session_id":"0192f…","cwd":"/work/repo",
 "bound":{"requested":{…},"effective":{…},"inherited":true},
 "final_text":"","structured_output":null,
 "denied_actions":[{"kind":"command","target":"curl …","reason":"network disabled","at":"…","event_seq":41}],
 "auto_declined_requests":[{"vendor_method":"item/tool/requestUserInput","summary":"2 questions","blocking":true,"at":"…","event_seq":52}],
 "steps":3,
 "usage":{"input_tokens":18000,"cached_input_tokens":12000,"output_tokens":900,"reasoning_output_tokens":300,"total_tokens":19200,
          "scope":"vendor_interval","provenance":"reported"},
 "cost":{"usd":null,"scope":"turn","provenance":"unavailable"},
 "timestamps":{"queued_at":"…","submitted_at":"…","accepted_at":"…","ended_at":"…"},"duration_ms":48210,
 "exit":null,"events":{"first_seq":23,"last_seq":71,"count":49},
 "raw_spans":[{"connection_id":"c_01","path":"raw/c_01.log","first_offset":10240,"last_offset":40960}],
 "vendor_options":{"codex":{}},"warnings":[{"code":"cancel_cleanup_uncertain","message":"…"}],"vendor":{"turn_id":"0192f…"}}
```

| Field | Meaning |
|---|---|
| `state`, `failure` | §7.2; `failure` = `{class, message, vendor_code?, retryable}` (§8.2) |
| `stop_reason` | `end_turn`, `max_steps`, `budget`, `refusal`, `interrupted`, `deadline`, `error`, `other`; vendor word in `vendor_stop_reason` |
| `cancel` | outcome and cleanup certainty (§3.5, §7.4) |
| `bound` | requested, effective, and whether it was inherited |
| `denied_actions` | actions the vendor's own bound denied (D3): `file_write`, `command`, `network`, `other` |
| `auto_declined_requests` | vendor requests VIA declined (D3) |
| `usage` | `scope` ∈ `turn` (verified per-turn), `session_cumulative`, `vendor_interval` (numbers reported, interval not verified); `provenance` `reported`/`unavailable` |
| `cost` | `usd`; `scope` as above; `provenance` `reported`, `estimated`, `unavailable`. Scopes are per field: Claude P5 showed per-result tokens with rising cumulative `total_cost_usd` |
| `exit` | `{code, signal}` for per-session processes that ended in this turn; `null` for server routes |
| `raw_spans` | bounding spans per connection for the turn, **not** extraction ranges on shared connections; event `raw_ref`s are authoritative |
| `warnings` | `instructions_partial`, `vendor_version_untested`, `usage_interval_unverified`, `structured_output_missing`, `cancel_cleanup_uncertain`, `predecessor_cleanup_uncertain`, `raw_log_incomplete`, `deprecated` |

## 6. Canonical event stream

### 6.1 Envelope and types

```json
{"seq":41,"session_id":"s_7f3k9q2mzr4c","turn":2,"late":false,"at":"…","type":"action.denied",
 "raw_ref":{"connection_id":"c_01","offset":31744,"len":412},"kind":"command","target":"curl …","reason":"network disabled"}
```

`seq`: Core-assigned per session, dense from 1. `turn`: `null` for
session-level events. `late: true`: attributed to a turn already terminal
(vendor turn id mapped by the adapter); such events never change the
envelope except through §7.6. `raw_ref` is `null` for synthesized events.

| Type | Payload | Committed by |
|---|---|---|
| `session.opened` / `session.closed` / `session.reopened` | `route`, confirmed `vendor_session_id`, `vendor_version` / `reason` / `route`, confirmed `vendor_session_id`, `vendor_version`, `reason` | Core |
| `turn.queued` / `turn.submitted` / `turn.started` | `queue_position` / `attempt` / `effective` | Core |
| `turn.ended` | `state`, `failure?`, `stop_reason`, `cancel?` | **Core only** |
| `turn.revised` | `revision`, `from_state`, `state`, `evidence` | Core |
| `assistant.text`, `reasoning.summary` | `text`, `final` | Adapter observation |
| `tool.started` / `tool.ended` | `tool_id`, `name`, `input_summary` / `status`, `output_summary`, `exit_code?` | Adapter |
| `file.changed` | `path`, `change`, `diff?` | Adapter |
| `action.denied`, `vendor.request_declined` | as envelope lists; `blocking` on declines | Adapter |
| `steer.delivered` | `delivery` | Adapter |
| `cancel.requested` / `cancel.settled` | — / `outcome`, `cleanup` | Core |
| `usage.updated` | as envelope `usage` | Adapter |
| `warning` | `code`, `message` | either |
| `process.exited`, `server.lost`, `raw_log.incomplete` | `code`, `signal` / `key` / `connection_id` | Core (from Host / Wire) |
| `vendor.other` | `vendor_type`, `payload` (bounded to 16 KiB), `truncated` (`true` when the payload was cut to that bound, C2 A1) | Adapter |

For a delayed-init CLI such as Claude, `session.opened`/`session.reopened`
is committed exactly once per connection generation only after matching
vendor identity confirmation. Core atomically persists the confirmed ID and
verification flag with that event, before any acceptance derived from the
same frame. A pre-init startup/resume rejection emits neither event, even
when it echoes the expected UUID. Matching init confirms identity, not turn
acceptance; prompt-associated evidence is still required.

Rust: `#[serde(tag = "type")]`, tags set with `rename`, unknown types kept
as `Other { type, payload }`.

### 6.2 Ordering; 6.3 following

Per session FIFO in `seq`; no promise across sessions (D4). `turn.ended`
is the last non-late event of its turn. Following: §3.11.

## 7. States

### 7.1 Session

| From | To | Cause |
|---|---|---|
| — | `idle` | receipt committed |
| `idle` | `active` | a turn is dispatched |
| `active` | `idle` | turn resolved, queue empty, cleanup not `pending` |
| `active` | `active` | next queued turn dispatched (§7.3 gate) |
| any | `closed` | `close`, `daemon/stop --force`, drain completed |

Vendor-process idle shutdown (Q4, 15 min, per harness) is **not** a state
change: the session stays `idle`, `status.process.alive` becomes `false`,
and the next `resume` reopens the vendor session (`session.reopened`).

### 7.2 Turn

| From | To | Cause |
|---|---|---|
| — | `queued` | accepted, committed |
| `queued` | `running/submitting` | dispatch: Core commits `submitted_at` **before** any vendor I/O (coding-style §7) |
| `running/submitting` | `running/accepted` | vendor acceptance committed (`accepted_at`) |
| `running/submitting` | `failed` (`submit_failed`) | vendor rejected the submission with a definite error |
| `queued` | `cancelled` | cancel, close, predecessor `unknown` (P6) |
| `running` | `completed` / `failed` / `cancelled` / `unknown` | §7.6 disposition |
| `unknown` | terminal | late evidence (§7.6), never a resend |

### 7.3 Queue and dispatch gate

One FIFO per session, capacity 8 (P6). The next turn dispatches only when
the predecessor is terminal and cleanup is settled: `quiescent`,
`uncertain` under P7, or not applicable. For Codex, vendor interrupted
evidence may acknowledge cancel while open tools keep the turn nonterminal
with `pending` cleanup until `min(acknowledged_at + 60 s,
turn.wall_deadline)`. If the wall budget is exhausted, settle immediately.
Core waits for tracked tool completion, then commits the cancelled terminal
with `quiescent`; otherwise at the deadline it commits `uncertain` and a
`cancel_cleanup_uncertain` warning. The next turn carries
`predecessor_cleanup_uncertain`. A late completion does not rewrite the
settled envelope. Behind an `unknown` predecessor the queue is cancelled. Admission
(daemon-wide budget, D7) is checked at dispatch. Only turns with no
`submitted_at` may ever be dispatched automatically (Astra 3).

### 7.4 Cancel outcomes

`requested` (sent, no ack; also a force accepted before any vendor launch,
where nothing exists to acknowledge; cleanup is then `quiescent` when Host's
journal is complete, since nothing was launched, runtime contract §6.2), `acknowledged` (vendor evidence: Codex
`turn/completed` status `interrupted`; ACP `stopReason: cancelled`; Claude
`control_response success` for `interrupt` followed by a `result` with
`subtype: error_during_execution` and `terminal_reason: aborted_tools`,
P5), `forced` (Host killed a private process group after the deadline),
`unknown` (no acknowledgement by the control deadline on a shared server,
or evidence lost). A later wall-budget expiry preserves an already
acknowledged cancellation under §7.6 precedence. Cleanup
certainty is separate (§3.5).

### 7.5 Crash recovery (D2, P12)

The new daemon validates Store and raw evidence and commits recovery before
admission. Per turn: `queued` with no submission intent stays queued only when
no predecessor is `unknown`; an intent without accepted evidence becomes
`unknown`; an accepted turn becomes `unknown` unless a route's rejoin was
probe-verified on the configured transport. Queued successors of `unknown`
are cancelled. None of these turns is resent automatically. Process exit,
including a `Dead` recovery report, does not prove the vendor took no action
or that submission never happened.

For a private process, Host uses only the persisted full **anchor** identity,
generation and private socket to find and challenge a live anchor. It verifies
the control peer and the anchor's own marker/identity before asking that same
anchor to stop its own group. Vendor child identity is separate process
evidence, never signalling authority. There is no scan of vendor environments,
no vendor marker discovery and no daemon-side numeric TERM/KILL of a saved
pid or pgid. If the anchor is absent or unverified, Host does not signal.
Cleanup is `uncertain` unless a same-boot, same-PID-namespace, non-signalling
group query proves `ESRCH` for a persisted Host-created group with full
identity, generation and pgid > 1 (runtime contract §5.2). `Ok`, `EPERM`,
other errors or namespace mismatch do not prove absence. Positive group
absence can make cleanup `quiescent`; it cannot prove vendor terminal state,
protocol acknowledgement, forced outcome or reaping, and the restarted turn
remains `unknown`. A stdio-attached shared server dies with the daemon (P3);
that does not prove its submitted work had no effect.

### 7.6 Disposition table (evidence → resolution, first matching row wins)

| Evidence | While | Result |
|---|---|---|
| Vendor terminal `interrupted`/`cancelled` after a VIA cancel | running | outcome `acknowledged`; with open tools keep turn nonterminal until P7 cleanup settles, then `cancelled` |
| Vendor terminal error with cancel-specific markers after a VIA cancel (Claude `aborted_tools`) | running | `cancelled`, `acknowledged` |
| Vendor terminal `completed` | running | `completed`; `stop_reason` from vendor |
| Vendor terminal `failed` | running | `failed`, class from vendor code (§8.2) |
| Core deadline | running | Core cancels (§7.4); result `failed`, class `deadline_wall`/`deadline_idle`, `cancel` filled |
| Force deadline, private process | running | Host proved the live vendor stopped: `cancelled`, outcome `forced`, cleanup `quiescent` only after verified group absence, else `uncertain`. No vendor could have launched (start never sent): `cancelled`, outcome `requested`. A vendor may have launched but neither a stop nor a terminal is proved: `unknown`, outcome `requested`. Outcome and cleanup certainty are independent (§7.5). If a turn event already failed to commit, the result is `failed(store)` instead, with `cancel` still filled |
| Force deadline, shared server | running | `unknown`, outcome `unknown` |
| Process exited without terminal result (Host-confirmed) | running | `failed(process_exited)` |
| Server death (Host-confirmed) | running | `failed(server_lost)`; every session on it |
| Transport lost, process alive or unconfirmed | running | `unknown` |
| Codex per-thread ingress/C2 stall overflow | running on affected thread generation | promptly resolve every nonterminal submitted turn under preceding disposition precedence, interrupt through reserved control, and block same-thread dispatch until clean reopen; preserve prior terminal envelopes and other threads; record normalized-event loss separately from any actual raw gap (C2 §4) |
| Raw-log or event overflow failed the connection | running | resolve by applicable server death/process exit evidence; `raw_log_incomplete` only when raw bytes were actually lost, while normalized-event loss is separately recorded |
| Submission rejected definitively | submitting | `failed(submit_failed)` |
| Daemon restart | any | §7.5 |
| Late vendor terminal for an `unknown` turn | unknown | revise to that state, `revision + 1`, `turn.revised`; followers whose subscription ended must poll `result` |

## 8. Errors

### 8.1 Request errors (JSON-RPC `error`, stable `code` and `data.kind`)

```json
{"jsonrpc":"2.0","id":7,"error":{"code":-32006,"message":"steer is unsupported on route claude-cli",
 "data":{"kind":"unsupported_verb","verb":"steer","harness":"claude","route":"claude-cli"}}}
```

| Code | `kind` | When |
|---|---|---|
| -32700 / -32600 / -32601 / -32602 | `parse_error` / `invalid_request` / `method_not_found` / `invalid_params` | JSON-RPC standard; `invalid_params.data.kind2` ∈ `unknown_field`, `session_scope_on_resume`, `vendor_option_conflict`, `idempotency_conflict` |
| -32000 | `handshake_required` | |
| -32001 | `version_mismatch` | |
| -32002 | `invalid_handle` | |
| -32003 | `session_not_found` | |
| -32004 | `session_closed` | closed or closing |
| -32005 | `turn_not_found` | |
| -32006 | `unsupported_verb` | |
| -32007 | `missing_capability` | `require` not met |
| -32008 | `bound_unsupported` | combination or route (§4.2), incl. bound change on a bound-keyed server (P11) |
| -32009 | `harness_unavailable` | binary missing, version refused (P13), server failed to start |
| -32010 | `unknown_model` | |
| -32011 | `queue_full` | |
| -32012 | `admission_refused` | resource/aggregate result cannot fit a bounded page or response |
| -32013 | `no_active_turn` | |
| -32014 | `turn_mismatch` | |
| -32015 | `turn_not_finished` | |
| -32016 | `wait_timeout` | |
| -32017 | `daemon_stopping` | |
| -32018 | `store_error` | Before a receipt: `data.commit_outcome: not_committed\|unknown` and `retry: same_key_only` when uncertain. For an affected receipted nonterminal turn that cannot be resolved durably: `data.session`, `data.turn`, last-known `data.durable_state`, `data.terminal_persisted:false`. No terminal envelope is invented. A persistent raw-reference read failure also uses this kind. |
| -32019 | `history_pruned` | `data.earliest_seq` |

A receipt whose commit outcome is `unknown` is never lost: once Store reads
succeed the daemon reconciles it itself, keyed or not, and a committed turn
runs exactly once; only a keyed caller can safely retry to learn its receipt,
so an unkeyed caller must not resend the request.

### 8.2 Turn failure classes (`failure.class`)

| Class | Meaning | Set by |
|---|---|---|
| `deadline_wall`, `deadline_idle` | Core deadline | Core |
| `submit_failed` | vendor rejected the submission before acceptance | Core (from adapter observation) |
| `resume_mismatch` | vendor returned a different or fresh session | Adapter observation |
| `vendor_error` | vendor turn failure; `vendor_code` keeps the vendor's code | Adapter |
| `rate_limit`, `auth`, `context_exceeded`, `budget_exceeded` | specific vendor classes | Adapter |
| `server_lost`, `process_exited` | Host-confirmed death | Core |
| `protocol` | malformed known message, or vendor stream contradiction | Adapter |
| `overflow` | this session's event channel stalled past its limit (C2 A1) | Core |
| `structured_output_invalid` | VIA validation failed (Q2) | Core |
| `daemon_restart`, `store` | §7.5; Store write failed after dispatch | Core |

Adapters never commit a class; they report observations and Core commits
(C2 §4). `max_steps` reached with a normal result is `completed` with
`stop_reason: max_steps`.

## 9. Security notes

- Socket and state directory per coding-style §6; one daemon per user. The
  public C1 API has no network listener in v1: clients use the user-only Unix
  socket or stdio proxy. A VIA-owned vendor server may use a private,
  authenticated loopback HTTP listener with verified process/listener
  provenance. VIA never attaches to an unrelated vendor listener.
- Handle: caller-generated, hashed at rest, never returned or traced.
- VIA never reads, copies, reuses or logs user/provider credentials. For the
  full-bound OpenCode route, VIA generates a fresh password per owned server,
  retains it in daemon memory and injects it only into that server's launch
  environment; VIA does not persist or emit it in Store, argv, diagnostics or
  transport captures/metadata. As an owner-authorized temporary exception,
  vendor tool children may inherit that generated instance password. Each
  server/password/private namespace belongs to one VIA session; cross-session
  sharing is prohibited. Loopback Basic Auth and listener provenance are
  mandatory. Full-bound sessions do not isolate hostile same-user processes;
  this exception does not authorize access to user/provider credentials.
  Revisit at the next vendor-pin/security review and before changing sharing,
  listener exposure, credential reuse or the advertised trust boundary.
  OC01/OC02/OC12 control tests in `vendors/opencode.md` remain required;
  complete child-environment scrubbing is deferred to `via-4sw.4`.
- Prompts and outputs are private local data; retention is daemon config
  (Proposed 30 days); vendor processes get a per-adapter environment
  allow-list, never the caller's environment.
- `via serve --stdio` gives its parent full C1 access; local callers only.

## 10. Open questions

Confirmed by review (Astra): Q1 session-wide follow; Q2 VIA validates
structured output; Q3 `wait` = latest turn at acceptance; Q4 15-minute
process shutdown, configurable; Q5 catalog-only `describe`. D9 stays open.
Owner, 2026-09-26: P12 approved as written; P7/P11 and P13 are resolved by
the reviewed Codex and Claude vendor packets. Vendor live gates remain open.

| # | Question | Recommendation / alternatives |
|---|---|---|
| P7 | Codex pending cleanup | reviewed §3.5/§7.3 rule: settle at `min(acknowledged_at + 60 s, wall_deadline)`; do not add `--after-uncertain` |
| P11 | server key vs per-turn bound | reviewed §Decisions rule; Codex key excludes bound and mixed-bound operation awaits enforcement proof; OpenCode key includes full bound, owning VIA session and durable private namespace, refusing bound changes or cross-owner sharing |
| P12 | live recovery gate | `unknown` everywhere in v1; alternative: enable Codex rejoin after the socket-transport probe (D9) |
| P13 | version gate | tested sets/ranges + immutable `allow_untested`; Claude initial set exactly `{2.1.283}` |
| Q6 | Claude steer semantics | resolved `unsupported`; busy input can merge into a running result, so no steer input is written |
| Q7 | Outbox and channel sizes (1000 events; C2 A1 limits) | as written, config-tunable |
