# VIA API v1 (contract C1)

Status: draft 3, 2026-09-30; the owner approved the S1 set on 2026-09-26
(see Decisions below). Draft 3 applies the adapter design's amendments
AC1–AC10 ([adapter design](../workstreams/rust-foundation/adapters/design.md)
§3.4, revision 9); leftover detection follows the owner's choice of option A
on 2026-10-01 (adapter design, conflict 4). Public contract between
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
(one conversation, one vendor session, one route for life; a compatible
resume advances its recorded adapter version, C2 §1 rule 2) and **turn** (one prompt → one envelope), addressed `s_7f3/2`. The
caller generates the session's handle; the daemon stores only its hash.

**First-release scope (owner, 2026-09-26):** Claude Code, Codex and OpenCode;
Pi added by owner, 2026-10-03; all methods below are in scope. Each route
truthfully declares native, partial or unsupported vendor behavior. ACP
references describe future coverage, not a release gate. Language, coding standard, testing policy and S1 scope are
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
| `events`, `logs` | same names | no | read-only | durable events (page), evidence locations |
| `models`, `daemon/status`, `daemon/stop` | `via models`, `via daemon …` | no | read-only / idempotent | catalog, daemon state |

| Entity | States |
|---|---|
| Session | `idle`, `active`, `closed` (internal admission gate: `open`, `closing`) |
| Turn | `queued`, `running` (phase `submitting` or `accepted`), `completed`, `failed`, `cancelled`, `unknown` |
| Cancel | outcome `requested`, `acknowledged`, `forced`, `unknown`; cleanup `quiescent`, `uncertain`, `pending` |

| Piece | Key points |
|---|---|
| Identifiers | `s_` + 12 base32; turn `s_…/N`; vendor session id opaque; handle `h_` + 43 base64url, caller-generated, hashed at rest |
| Envelope | state, failure class, stop reason, cancel outcome and cleanup, final text, structured output, denied actions, auto-declined requests, route and versions, usage and cost with per-field scope, event range, evidence locations, `revision` |
| Events | `type` tag, per-session dense `seq`, `turn` nullable for session events, `late` flag |
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
| P11 | Codex owned stdio server key is `config_hash` without bound; `config_hash` covers VIA-controlled launch settings (resolved program path, arguments, passed environment, server cwd, protocol pin), not credentials or binary contents; the observed binary version is reported, not keyed. Every turn sets `sandboxPolicy`; mixed-bound sharing waits for pinned enforcement proof. OpenCode keys include the full effective bound, VIA owner session and durable private namespace; no cross-owner server sharing or live-session migration. Bound-keyed routes refuse bound changes on resume | as reviewed in `docs/specs/vendors/codex.md` §9 and `docs/specs/vendors/opencode.md` §2 |
| P12 | Live recovery after daemon restart is `unknown` for every route in v1; `resumed` only when a route's rejoin is probe-verified on the configured transport (Codex stdio server dies with the daemon, P3) | as written |
| P13 | Version rule: every vendor version is supported; refused only on demonstrated handshake breakage; `untested` warns until the maintainers' check (owner OD1, 2026-09-30, superseding the 2026-09-26 P13 approval) | decided; C2 §5 (adapter design AD7) |

## 1. Scope, transport, versioning

- **Transport (decided, D1).** JSON-RPC 2.0 over the daemon's Unix socket,
  one JSON object per line, UTF-8, line length capped at 1 MiB.
  Requests carry `id` (A31); the daemon sends no notifications. No batches.
  `via serve --stdio` forwards messages unchanged.
  Parse JSON with depth at most 64 and 65,536 nodes per document before
  constructing an unbounded value; reject an excess as the named parse or
  parameter error. The 1 MiB limit includes the line feed; a longer line
  gets `request_too_large` and the connection closes. A larger prompt is
  passed as `prompt_file` (§4). A request `id` is a string, a number or
  `null`, at most 256 bytes encoded; a longer one is `invalid_request`.
  The daemon writes each reply within 10 s of having it ready to write; a
  peer that does not read it in that time is disconnected.
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
  is `version_mismatch`. A mismatched `hello` stops nothing. It returns
  `data: {daemon_version, store_path}` and permits one plain `daemon/stop`,
  which only daemon main's idle predicate accepts. The CLI restarts an idle
  daemon whose Store matches (runtime §6.1), else reports.
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

Never starts a process or server (Q5). `vendor_version` is the last version
seen for the harness and resolved program path, or `null`;
`version_status` is `tested` (in the adapter's `checked` set), `untested`
(not yet checked by the maintainers, warning `vendor_version_untested`) or
`refused` (a cached
handshake-check failure on something VIA relies on). Every vendor version is
supported by default (P13, C2 §5). Errors: `unknown_model`,
`harness_unavailable`, `invalid_params`.

### 3.2 `spawn` — new session and turn 1

CLI: `via spawn --harness H --model M --prompt "…" [--prompt-file F|-] [--instructions F] [--bound B] [--allow-dir D]… [--network] [--cwd D] [--effort E] [--output-schema F] [--wall-ms N] [--idle-ms N] [--max-steps N] [--require V,…] [--allow-untested] [--vendor h.k=v]… [--label L] [--idempotency-key K] [--handle-file F|--handle-stdin] [--background]`

`--prompt-file F` sends `prompt_file` with `F` made absolute; `-` reads
stdin into `prompt`.

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
`unsupported_verb` (`data.verb:"spawn"`, with `harness` and `route`),
`harness_unavailable`, `missing_capability`, `bound_unsupported`,
`admission_refused`, `store_error`. When the daemon already retains its
bound of unresolved turns (runtime contract §8), `spawn` is refused
`admission_refused` if all are in flight, and `store_error` while any retained
turn failed to persist its terminal. Idempotency (P4): same key + same handle
hash + byte-identical params → the stored receipt; different handle or
params → `invalid_params` with `kind: idempotency_conflict`.
`allow_untested` is included in that exact retry identity. It is accepted,
stored and has no effect: every version is supported unless refused for
breakage, which it cannot waive (P13).

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
`queue_full`, `bound_unsupported`, `unsupported_verb`, `admission_refused`,
`harness_unavailable` (including `data.reason: adapter_version` when the
stored adapter version is not compatible with the running adapter).
A compatible resume advances the session's recorded `adapter_version` to the
running adapter's version at that turn's `turn.started` commit.

### 3.4 `steer` — input into the active turn

CLI: `via steer <session> --text "…" [--expect-turn N] [--op-key K]`

Result: `{turn, delivery}`; `delivery` is `injected` or the declared partial
semantics (§4.1). Steer on an idle session is `no_active_turn`; a steer
that arrives while the turn is `submitting` waits for acceptance then
applies, or fails `no_active_turn` if acceptance fails. A successful reply
follows the commit of its `steer.delivered` event; if that commit fails, the
reply is `store_error`, never success. A steer reaches
only the turn it selected: if that turn ends before the input is admitted,
the steer is `turn_mismatch` or `no_active_turn` and no successor receives
it (C2 `SteerInput.turn`). Errors: `unsupported_verb` (`data.verb:"steer"`),
`no_active_turn`, `turn_mismatch`, `invalid_handle`, `admission_refused`
(`data.reason:"control_lane_full"`: the session's control lane is full and
nothing was written, C2 §2), and `steer_failed`: `data.reason` `not_steerable`
with `data.delivery:"none"` (the vendor refused steer in the turn's current
phase), `not_delivered` with `data.delivery:"uncertain"` (writing the input
began but the vendor never acknowledged it; or, for a keyed steer, the
daemon-generated outcome of an intent whose owner ended without recording
one, as above), or `not_recorded` with
`data.delivery` as a success would give it (the vendor took the input, but
VIA could not record its `steer.delivered` event).

**Keyed steer.** With `op_key` (§3), the key's answer is the stored
outcome of its durable intent. The key is looked up after the session and
handle are authenticated, the Store latch is checked (once latched, every
steer is `store_error`, a repeat included) and the params are validated
(an invalid `op_key` is `invalid_params`). The look-up comes before the
capability check (`unsupported_verb`) and every current-turn check:

- the same key with other params, or a key another verb holds, is
  `invalid_params` with `kind2: idempotency_conflict`;
- the same key with byte-identical params replays the stored outcome and
  never writes input again.

The daemon commits the key's intent before the steer is checked or its
input goes to the vendor. It then commits the outcome exactly once:

- a success, in the same transaction as its `steer.delivered` event;
- any refusal of this section, `unsupported_verb` and
  `admission_refused` (`control_lane_full`) included, on its own.

That stored outcome is the key's answer, even when the first caller never
received it. Some cases:

- **Retry after a failed intent write.** The intent write was refused
  before it was enqueued, or is known to have rolled back. The reply was
  `store_error` and no key exists, so a retry is a fresh first attempt.
  An intent write still in flight may yet commit: a repeat then finds an
  intent with no owner and gets the uncertain outcome below.
- **Retry after a lost reply.** The outcome committed but its reply was
  lost. The caller saw `store_error`, and a repeat replays the committed
  outcome.
- **Caller went away.** An intent has one owner at a time: the request
  until the steer is handed to the session's turn, and the daemon's turn
  bookkeeping after. A repeat while an owner remains waits for it and
  gets the outcome it stores.
  - Gone before the intent write was enqueued, or once it is known to
    have rolled back: no key exists; a repeat is a fresh first attempt.
  - Gone while the intent write is in flight: the Store may still commit
    it. If it commits, the intent has no owner and nothing was sent, as
    below.
  - Gone after the intent committed but before the hand-off: the intent
    has no owner and nothing was sent. A repeat (or restart recovery)
    stores the uncertain outcome below; the input is never sent.
  - Gone after the hand-off: a delivery the vendor acknowledged is still
    recorded, the outcome together with its `steer.delivered` event, and
    a repeat gets it. A refusal by the driver, or an input the vendor
    never acknowledged, is recorded by no one: once the steer's turn has
    settled, no delivery can resolve the intent, and a repeat (or restart
    recovery) stores the uncertain outcome below. A repeat before then
    waits.
- **Uncertain outcome.** This is the outcome the daemon itself stores for
  an unresolved intent whose owner ended without recording an outcome:
  `steer_failed` with `data.reason:"not_delivered"`,
  `data.delivery:"uncertain"`. It is distinct from a `not_delivered`
  refusal a driver reported and the daemon recorded (§8.1). Cases:
  - the request ended before the hand-off, or while its intent write was
    in flight and that write then committed;
  - the request ended after the hand-off, and the steer's turn settled
    without a delivery, a refusal the request did not record included;
  - the steer's delivery was consumed but its `steer.delivered` commit
    failed;
  - the steer's session ended without the delivery committing;
  - its outcome could not be recorded, for example a `control_lane_full`
    refusal whose own outcome commit rolled back; the turn may still be
    running and no restart need have happened;
  - the daemon restarted first.

  The message then says only that the delivery outcome was not durably
  recorded. Whether the vendor acknowledged the input is unknown, and the
  input is never sent again.

A stored refusal stays the key's answer; to retry, use a new key.

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
`uncertain` (stopping not provable, whether the stop was acknowledged or
only requested), `pending` (still waiting for
tool completion or the cleanup deadline). `pending` exists only before the
cleanup deadline; a settled result is `quiescent` or `uncertain`. OS
group-absence evidence covers the agent's own group; descendants outside it
that no vendor item tracks are not part of cleanup and are reported in
`leftovers` (§5). For Codex, the matching
`turn/completed:interrupted` acknowledges cancel; the interrupt RPC response
alone does not. With open tools, the turn remains nonterminal while cleanup
is `pending` until `min(acknowledged_at + 60 s, turn.wall_deadline)`. If
the wall budget has expired, settle `uncertain` immediately. On tool
quiescence or that deadline, commit one `cancelled` envelope with cleanup
`quiescent` or `uncertain`. At uncertainty, warn `cancel_cleanup_uncertain`;
a successor may dispatch with `predecessor_cleanup_uncertain`. No
acknowledgement by the order's `force_at` on a shared server gives state and
outcome `unknown`, with no kill; a private process follows §7.6's
private-process force row. Late tool
completion remains late evidence and does not rewrite that envelope. Codex P2/P2b: after
`interrupted` the tool's `sleep 120` ran to the 60 s poll limit;
`command/exec/terminate` does not apply to agent-started tools and
`thread/unsubscribe` does not stop them, so on a shared server a tool
survives until it finishes or the server dies. A queued
turn is dropped: `cancelled`, `acknowledged`, `quiescent`. On an already
terminal turn: `already_terminal: true` with the recorded `cancel` or
`null`. A no-`wait` cancel of a running turn replies `state: running`,
`cleanup: pending`, `settled_at: null`. A cancel that lands during
settlement replies `already_terminal: true`. Idempotent. Errors:
`turn_not_found`, `invalid_handle`, `unsupported_verb`.

### 3.6 `close` — end the session

CLI: `via close <session> [--mode graceful|force] [--deadline-ms N] [--op-key K]`

Sets the admission gate `closing` (new `resume` → `session_closed`),
cancels the active turn, drops queued turns as `cancelled`, closes the
vendor session (`close(mode, deadline)` down to Host, D7), then sets
`closed`. Result `{session_id, state: "closed", cancelled_turns, cleanup,
leftovers}`; `cleanup` is `uncertain` while any process group the session owns
lacks a proof of absence or, on a shared server, while a turn of the session
runs or ended without its own cleanup proved quiescent (whether or not it was
cancelled) and the server group it ran on lacks a proof of absence; otherwise
`quiescent`. `leftovers` (§5) is always present and non-null only when this
close stopped the session's server, where a keyed replay returns the stored
report; otherwise `null`.
Idempotent; a second `close` during closing waits for the first. A close
that finds Core retiring the session's idle lane (C2 §3, idle lanes) takes
that driver close over with its own mode and deadline if it has not
started. Otherwise it waits for the retirement to end: the driver close,
bounded by 3 s, then the lane's drain, which can outlast this close's own
deadline. That driver close belongs to no C1 close, so `leftovers` is
`null`. A close
whose `session.closed` commit is refused a second time because turns of the
session are unfinished replies `admission_refused` ([Task 3 design](../workstreams/rust-foundation/t3/design.md) §4 step 6).

### 3.7 `status`

`via status <session> [--turn N] [--after-step N] [--limit N]`

Params: `session`, `turn?` (default the running turn, else the latest),
`after_step?` (default 0), `limit?` (default 100, max 1000).

```json
{"session_id":"s_7f3k9q2mzr4c","state":"active","admission":"open","harness":"codex","model":"gpt-6-sol",
 "route":"codex-app-server","adapter_version":"0.1.0","vendor_version":"0.159.2","version_status":"untested",
 "inherit":{"hooks":"off","mcp_servers":"unknown","plugins":"unknown","skills":"unknown","agents":"unknown","instruction_files":"unknown"},
 "warnings":[{"code":"vendor_version_untested","message":"…"},{"code":"config_switch_unverified","message":"…","data":{"categories":[…]}}],
 "vendor_session_id":"019…","vendor_identity_verified":true,"cwd":"/work/repo","process":{"alive":true,"cleanup":"quiescent","idle_since":null},
 "active_turn":{"n":2,"state":"running","phase":"accepted","started_at":"…","last_event_seq":57,"cancel":null},
 "progress":{"turn":2,"current_step":4,"phase":"tools","running_tools":["shell"],"tools_overflow":false,"last_activity_at":"…",
             "tokens":{"total":18200,"scope":"vendor_interval"}},
 "steps":{"turn":2,"items":[{"step":1,"started_at":"…","ended_at":"…","tokens":5100}],"next_after":1,"more":true},
 "queue":[{"n":3,"op_key":"k-17","queued_at":"…","effective":{…}}],
 "turns":[{"n":1,"state":"completed","revision":0},{"n":2,"state":"running"},{"n":3,"state":"queued"}],
"label":null,"created_at":"…","updated_at":"…"}
```

`status` describes one turn: the `turn` param, else the running turn, else
the latest. `progress` is an in-memory snapshot of that turn while it runs
in this daemon, read after `steps` without a Store round trip, and `null`
otherwise, including as soon as the turn is terminal. `current_step` is
VIA's count of model steps: 0 before the vendor accepts the turn, 1 after,
and one more each time the model produces output after tool results,
derived the same way for every vendor; it equals the vendor's model calls
only where that vendor's evidence shows it. `running_tools` holds at most
64 names of tools started and not ended, never inputs or outputs;
`tools_overflow` is true when more started since the last step boundary.
`phase` is `tools` while a listed tool runs or `tools_overflow` is true,
else `model`; both reset at each step boundary. `last_activity_at` is the
arrival time of the last vendor message attributed to the turn. `tokens` is
an approximate running total of completed steps, updated once per step,
labelled with the route's token scope (§4.1), or `null` when the route has
no validated source or before the first sample. The envelope's `steps` and
`usage` hold the final figures.

`steps` pages the durable step history of the selected turn, running or
finished: one item per completed step, ordered by `step`. The step in
progress has no item yet, and a step that ended after the Store read
appears on a later call. After a daemon crash the history holds every step
whose row was committed; a step whose row was being committed, and the
step then in progress, are missing, and the agent's transcript has them. A
committed `turn.ended` implies that all the turn's rows are durable, also
after a refused Store write or a stop forced at shutdown, except for a
terminal synthesized by crash recovery or by the failure-resolution batch
after a Store write of uncertain outcome (runtime §7).

`process.alive` is true only on positive evidence that the vendor process
is live; `process.cleanup` is `uncertain` under the same rule as `close`'s
`cleanup` (§3.6), else `quiescent` (T4-A23).

`adapter_version` is the session's recorded adapter version, advanced by a
compatible resume (C2 §1 rule 2). `vendor_version` and `version_status` are
from the handshake of the instance running the described turn (on a
persistent connection, that connection's handshake, even when read for an
earlier turn): recorded when the turn is accepted, and in its terminal
envelope, which also carries it for a turn rejected after the handshake.
Before either, or when no handshake was read, `vendor_version` is null and
`version_status` is `untested` (C2 §5).
`inherit` holds the effective state (`on`, `off` or
`unknown`) of each inherited-configuration category, frozen at spawn
(C2 §6.2). `warnings` repeats the standing warnings:
`vendor_version_untested` while the described turn's `version_status` is
`untested`, and
`config_switch_unverified` with `data.categories` while any effective state
differs from the verified requested state.

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
`wait` returns as soon as the turn's terminal commits; without any change
it also re-reads every 5 s, a safety recheck, until its bound. It and an
`events` long-poll (§3.11) are the blocking reads. A caller that wants progress polls
`status` (§3.7) on another connection, since a connection carries one request
at a time. Closing the connection of a pending `wait` releases only that
waiter.

### 3.10 `list`

`via list [--state S] [--harness H] [--label L] [--since T] [--limit N] [--cursor C]`.
Ordered by creation, newest first. `cursor` is opaque; each session that
existed when the first page was read is examined once and returned if it
matches the filters then; sessions created later are not returned. A page
examines at most 1000 sessions, so it can be short or empty while
`next_cursor` is not `null`. Each summary is `{session_id, state,
admission, harness, model, label, created_at, last_active_at}`;
`last_active_at` is the time of the session's latest durable event, and
`since` matches `last_active_at ≥ since`. Result
`{sessions: [summary], next_cursor}`. The page stops at both requested item
count and 1 MiB encoded bytes. A summary is far smaller than a page, so the
first one always fits; there is no refusal path and never a truncated
success.

### 3.11 `events` — page

`via events <session|turn> [--after SEQ] [--limit N] [--types T,…] [--wait-ms N] [--follow]`

Params: `session` or `turn`, `after?` (default 0), `limit?` (default 200,
max 1000), `types?`, `wait_ms?` (default 0, max 30,000; above is
`invalid_params`). Result `{events, next_after, more: bool,
earliest_seq}`. Semantics:

- The page is a bounded Store scan in `seq` order from `after`, filtered by
  `types` (gaps in `seq` are expected under a filter). It stops at both the
  requested count and 1 MiB encoded bytes. `next_after` is the last scanned
  seq, including filtered-out events; `more` uses the committed head captured
  with the page. An event (its payload at most 256 KiB) is far smaller than
  a page, so the first match always fits; there is no refusal path and never
  a truncated success.
- History pruned by retention: `history_pruned` error carrying
  `earliest_seq` when `after < earliest_seq - 1`. Until retention prunes,
  `earliest_seq` is 1.

- Long-poll: with `wait_ms` above 0, a page with no matching events is
  read again from its `next_after` (which advances past filtered-out
  events) as events commit, and at least every 5 s (a safety recheck); the
  call returns as soon as a page has events. At `wait_ms` it returns the
  last empty page normally, with the latest `next_after`; it is not an
  error. `wait_ms` bounds its Store reads too: if no read completed by
  then, the reply is the empty page at `after` with `more: true`, so the
  caller reads again. `wait_ms: 0` returns the first
  page at once. A session or turn not found is refused as soon as the first
  read completes; if `wait_ms` cuts that read, the reply is the empty page
  with `more: true` above, and the next call reports it. Final
  shutdown ends a long-poll that found nothing `daemon_stopping`, as it ends
  a `wait`. Closing the connection of a pending long-poll releases only
  that call.

There is no follow stream: a caller follows a session by long-polling from
each reply's `next_after` (`via events --follow` does this until Ctrl-C),
polls `status` (§3.7) for progress and uses `wait` (§3.8) for the end of a
turn.

### 3.12 `logs` — evidence locations

`via logs <session|turn>`. Returns where a turn's evidence is, for the
addressed turn or, for a session, its running turn else its latest
submitted turn: `{session_id, turn, vendor_session_id, transcript, folder,
files: [{name, bytes}]}`. `transcript` is the path of the vendor's own
transcript, a hint that follows the vendor's layout, or `null`. `folder`
is the turn's evidence folder in VIA's state directory, or `null` for a
turn never submitted. `files` lists the files there that exist:
`stderr.log` (the agent's stderr),
(absent on a shared-server route, where the agent's stderr belongs to
the server, not to a turn), `undecoded.bin` (the first 64 KiB of a
vendor message VIA could not decode, named by the turn's failure) and
`final_text.txt` and the structured-output file the envelope names
(`structured_output.json`, or a revision's own file; a final text or
structured output too long for the envelope, §5). VIA does
not read or decode them; the caller reads the files. There is no paging.

### 3.13 `models`; 3.14 `daemon/status`, `daemon/stop`

`via models [--harness H]` → `{models: [{model, harness, aliases, source}]}`.
`via daemon status` → `{daemon_version, pid, started_at, sessions: {idle,
active, closing}, servers: [{harness, vendor_version, key, sessions}],
socket_path, store_path, health, store_failure, connections, limits,
storage}`. `limits` holds the effective disk and WAL thresholds; `storage`
holds `free_bytes`, `data_bytes`, `data_measured_at`, `below_free_floor`
and `over_warn_size`.

`servers` lists the live shared servers whose handshake succeeded:
`key` is an opaque 16-hex-digit server key, and `sessions` counts the
sessions holding a lease on it.

`health` reports `healthy`, or `store_failed` once the daemon has latched
a Store failure (runtime §7); it stays `store_failed` until the daemon
exits. `store_failure` is `null`, or reports the latest recorded Store
failure as `{kind, scope, since, count, affected}`. `scope` is `request`,
`turn`, `session` or `daemon`, and `affected` lists at most 16 addresses
plus a count. It carries no prompts, payloads or handles. `connections`
reports `{limit, in_use, held_unproven}`.

`via daemon stop [--drain|--force]` refuses while any session is active
or durably `closing`, unless one of these is given:

- `drain`: refuse new work, run accepted queued turns to completion, then
  stop. Drain closes no session; sessions stay open and resumable after
  restart.
- `force`: close with mode `force` every session that has unfinished
  work when force is accepted (a running turn, or a queued turn including
  one being dispatched or cancelled). Turns end `cancelled` or `unknown`.
  Sessions without such work stay as they were: open, or `closing` for
  restart to finish.

`drain` with `force` is `invalid_params`; after acceptance new work is
refused `daemon_stopping`. The result `{"stopping":true}` only
acknowledges acceptance; it is not evidence that work stopped or the
daemon exited. A session closed by `force` commits `session.closed` with
`reason: "daemon_stop_force"`. Drain runs accepted turns under their
existing deadlines; force and an idle stop then share one final 10 s shutdown deadline. The daemon exits 0
only after positive cleanup, joins and durable records, otherwise 4
(runtime contract §6.2).

## 4. Canonical parameters

| Parameter | Type | Scope | Notes |
|---|---|---|---|
| `harness` | `claude`, `codex`, `opencode`, `acp:<agent>`, `fake` | session | optional if `model` resolves; `fake` is a test double, available only with runtime §11.1's fixture configuration |
| `model` | string | session (P5) | `resolved` reported in the envelope |
| `allow_untested` | bool, default false | session | immutable after spawn; accepted and stored for compatibility; no effect (P13): every version is supported unless refused for breakage, which it cannot waive |
| `effort` | `low`…`max` or vendor value | per turn | unknown values refused; a vendor value that can be judged only against a discovered catalog is refused at submission, `failed(submit_failed)` with `failure.data.field:"effort"`; no vendor turn starts (C2 §5) |
| `instructions` | `{text}` or `{path}` | session | native or `prepended_to_prompt` (partial). `path` is absolute: a regular UTF-8 file of at most 1 MiB that the daemon's user can read, copied when the request is received and frozen as its text; a file that changes during the copy is `invalid_params` naming `instructions`. The path is not stored; the retry identity uses the copy's SHA-256 and length |
| `prompt` | string | per turn | exactly one of `prompt` and `prompt_file` |
| `prompt_file` | absolute path | per turn | a regular UTF-8 file of at most 16 MiB that the daemon's user can read; the daemon copies it when the request is received and refuses it (`invalid_params`, kind2 `prompt_file`) if it changes during the copy. The path is not stored; the retry identity uses the copy's SHA-256 and length |
| `bound` | `{mode: read_only\|workspace_write\|full, extra_write_dirs: [path], network: bool}` | per turn (D5): inherited unless set on `resume` | always never-ask (D3); combinations per §4.2 |
| `cwd` | absolute path | session | must exist |
| `output_schema` | JSON Schema object or `null` | per turn | `null` clears an inherited schema; validated by VIA (Q2, draft 2020-12 only: a `$schema` in any schema position (not inside instance values such as `const`, `enum`, `default` or `examples`) that names another dialect is `invalid_params`; size ≤ 256 KiB; at most 2,048 subschemas and 64 patterns, counted over every schema position whether referenced or not, each pattern within the regular-expression limit, else `invalid_params`; runtime §8). Patterns keep ECMA-262's meaning of class escapes, `.` and `\b`; one VIA cannot compile is `invalid_params` |
| `deadlines` | `{wall_ms?, idle_ms?}` | per turn | Core-owned absolute deadlines (D7); defaults 3 600 000 / 600 000. `idle_ms` is a positive integer; `0` is `invalid_params`. Like `wall_ms`, it is frozen at receipt and inherited (P5) |
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
`revision` counts revisions and `turn.revised` announces them. A revision
replaces the envelope: state, failure, stop reason, steps, cost,
structured output (validated as in Q2) and vendor data come from the late
terminal; a usage aggregate in the late terminal supersedes the stored usage
and its warnings, and without one the stored usage stands; `events` extends to the `turn.revised` event; the stored
`final_text` is kept, since a late terminal carries no text.
The encoded envelope is at most 1 MiB by construction, and no turn fails
for the size of its result. `final_text` is inline up to 256 KiB encoded.
A longer final text is written to `final_text.txt` in the turn's evidence
folder (§3.12): `final_text` is then `null` and `final_text_file` gives
`{path, bytes, truncated}`. The file holds at most 64 MiB; a longer text is
cut there at a character boundary with `truncated: true`. VIA syncs the
file and its folder before committing the envelope that names it. A failed
file step fails the turn `store`. After a failed write, the file is cut to
its last whole character and named with `truncated: true` only if that cut
and both syncs succeed; otherwise the envelope names no file.
`structured_output` is inline up to 32 KiB encoded. A larger value is
written to `structured_output.json` in the same folder: `structured_output`
is then `null` and `structured_output_file` gives `{path, bytes}`. VIA writes the whole file and syncs it and its folder
before committing the envelope or revision that names it, and never changes
a written file. A revision's spill uses a file name of its own
(`structured_output.r<revision>-<nonce>.json`), so a file that an attempt
wrote but no committed envelope names is never a result, is never deleted
and never blocks a later revision. A revision's file is kept even when
its write or sync fails. The write is part of the commit that names the file: if it
fails, that commit fails and resolves under §7.6's Store rule, and a partial
file is never named. Validation (Q2) runs on every present value before it
is stored, whatever the turn's state, including a late revision (§7.6); a
spilled value counts as present for `structured_output_missing`. Validation
work is bounded (runtime §8): a value it cannot finish is treated as invalid
with `reason: validation_limit` (otherwise `reason: invalid`). An invalid value
is kept. On a turn that would complete it fails the turn
`structured_output_invalid` (`failure.data.reason`); on a turn that ends
otherwise the state and failure class stand, and the envelope carries the
warning `structured_output_invalid` (`data.reason`). `denied_actions` and
`auto_declined_requests`
hold the first 1,000 entries each; `denied_actions_total` and
`auto_declined_requests_total` count all. An entry's strings are cut at a
character boundary to keep it within 256 bytes; its `event_seq` cites the
event with the full payload. At receipt a `bound` over 32 KiB, a `vendor`
over 16 KiB, a `cwd` over 4 KiB, or a `model` (as requested or as resolved)
or `effort` over 1 KiB encoded is `invalid_params` naming the member;
`bound.effective` is at most 32 KiB
encoded too. `failure.message` is at most 2 KiB,
cut at a character boundary. `warnings` holds at most one entry per code
(the closed list below), each with VIA's own `message` of at most 1 KiB and
`data` of at most 4 KiB encoded. Adapter-reported warnings are durable
`warning` events (§6) and reach the envelope only as these codes. An
evidence `transcript` hint over 4 KiB encoded is `null`. The daemon refuses
a state directory whose path is over 1 KiB encoded (runtime §6.1), so the
evidence folder and file paths stay under 2 KiB. Numbers are 64-bit integers
or finite doubles. With these caps, the IDs and versions of at most 1 KiB
each (C2 A1) and `leftovers`' 16 processes, every variable-size field is
bounded; their encoded sum stays
below 1 MiB, and a conformance test assembles that maximum.

```json
{"api_version":1,"session_id":"s_7f3k9q2mzr4c","turn":2,"address":"s_7f3k9q2mzr4c/2","revision":0,
 "state":"cancelled","failure":null,"stop_reason":"interrupted","vendor_stop_reason":"interrupted",
 "cancel":{"outcome":"acknowledged","cleanup":"uncertain","requested_at":"…","settled_at":"…"},
 "harness":"codex","model":{"requested":"gpt-6-sol","resolved":"gpt-6-sol"},"effort":{"requested":"high","resolved":"high"},
 "route":"codex-app-server","adapter_version":"0.1.0","vendor_version":"0.157.1","version_status":"tested",
 "vendor_session_id":"0192f…","cwd":"/work/repo",
 "bound":{"requested":{…},"effective":{…},"inherited":true},
 "final_text":"","final_text_file":null,"structured_output":null,"structured_output_file":null,"leftovers":null,
 "denied_actions":[{"kind":"command","target":"curl …","reason":"network disabled","at":"…","event_seq":41}],
 "auto_declined_requests":[{"vendor_method":"item/tool/requestUserInput","summary":"2 questions","blocking":true,"at":"…","event_seq":52}],
 "denied_actions_total":1,"auto_declined_requests_total":1,
 "steps":3,
 "usage":{"input_tokens":18000,"cached_input_tokens":12000,"output_tokens":900,"reasoning_output_tokens":300,"total_tokens":19200,
          "scope":"vendor_interval","provenance":"reported"},
 "cost":{"usd":null,"scope":"turn","provenance":"unavailable"},
 "timestamps":{"queued_at":"…","submitted_at":"…","accepted_at":"…","ended_at":"…"},"duration_ms":48210,
 "exit":null,"events":{"first_seq":23,"last_seq":71,"count":49},
 "evidence":{"folder":"…/evidence/s_7f3k9q2mzr4c/2","transcript":null},
 "vendor_options":{"codex":{}},"warnings":[{"code":"cancel_cleanup_uncertain","message":"…"}],"vendor":{"turn_id":"0192f…"}}
```

| Field | Meaning |
|---|---|
| `state`, `failure` | §7.2; `failure` = `{class, message, vendor_code?, retryable, data?}` (§8.2). `data` is present only for an adapter-side `submit_failed`, where it has `reason` (`"invalid_param"` or `"handshake_refused"`) and, with `invalid_param`, `field` (the C1 parameter name), and for `structured_output_invalid`, where it has `reason` (`"invalid"` or `"validation_limit"`, §5). It is bounded to 256 bytes, never holds vendor text, and follows §8.1's `data.field` naming. `vendor_code` keeps only vendor codes |
| `stop_reason` | `end_turn`, `max_steps`, `budget`, `refusal`, `interrupted`, `deadline`, `error`, `other`; vendor word in `vendor_stop_reason` |
| `cancel` | outcome and cleanup certainty (§3.5, §7.4) |
| `bound` | requested, effective, and whether it was inherited |
| `denied_actions` | actions the vendor's own bound denied (D3): `file_write`, `command`, `network`, `other` |
| `auto_declined_requests` | vendor requests VIA declined (D3) |
| `usage` | `scope` ∈ `turn` (verified per-turn), `session_cumulative`, `vendor_interval` (numbers reported, interval not verified); `provenance` `reported`/`unavailable`. `turn` sums the model calls of the vendor session the turn ran in; delegated sub-agent sessions may be excluded (OpenCode task sessions are) |
| `steps` | the vendor's own count of model steps in the turn (Claude `num_turns`), or `null` when the vendor reports none; VIA's count is only in `status` `progress` (§3.7) |
| `events` | `{first_seq, last_seq, count}` of the turn's durable events (§6.1) |
| `final_text_file` | `{path, bytes, truncated}` when the final text is in `final_text.txt`, else `null` |
| `structured_output_file` | `{path, bytes}` when the structured output is in a file (`structured_output.json`, or a revision's own file, §5), else `null` |
| `denied_actions_total`, `auto_declined_requests_total` | entries of each list, including those past the first 1,000 |
| `cost` | `usd`; `scope` as above; `provenance` `reported`, `estimated`, `unavailable`. Scopes are per field: Claude P5 showed per-result tokens with rising cumulative `total_cost_usd` |
| `exit` | `{code, signal}` for per-session processes that ended in this turn; `null` for server routes |
| `evidence` | the turn's evidence folder and the vendor's transcript hint, as `logs` returns them (§3.12) |
| `warnings` | `instructions_partial`, `vendor_version_untested`, `usage_interval_unverified`, `structured_output_missing`, `structured_output_invalid`, `cancel_cleanup_uncertain`, `predecessor_cleanup_uncertain`, `config_switch_unverified`, `deprecated`, `observations_lost`.  `config_switch_unverified` is one warning per receipt or envelope listing every category whose requested inheritance setting VIA could not apply or could not verify, `data.categories: [{category, requested, effective}]` (C2 §6.2) `observations_lost` (some observations of a shared-server thread were lost: by ingress overflow, an observation stall, a close's deadline or an internal task failure) carries `data: {trigger_turn, generation, first_unqueued, omitted}`, where `omitted` is `null` when the count is unknown or saturated; it is on the envelope of every turn the loss affected, and, when the triggering turn was already terminal, a durable `late` `warning` event on that turn; that envelope is not rewritten. |
| `leftovers` | processes the coding agent started that were observed after its own process exited; the agent's responsibility, never signalled by VIA (C2 §4.2). `{scope: "turn"\|"server", processes: [{pid, comm, started_at}], total, incomplete, best_effort: true}` or `null`. `processes`: at most 16, oldest first (start ticks, then pid). `total`: the matches found, exact when not `incomplete`, a lower bound otherwise; `total` greater than the list length is the only truncation signal. `started_at`: RFC 3339 UTC, boot time (`/proc/stat` `btime`, whole seconds) plus the process's start ticks, so accurate to about 1 s and emitted with second precision. `comm`: the kernel's process name (at most 15 bytes), lossy UTF-8; it is process-controlled, so a process can name itself anything. Always present; non-null only on per-turn-route envelopes and on `server_lost` envelopes (one shared snapshot per lost server); `null` elsewhere, including recovered turns. Produced best effort by Host's report-only scan for VIA's process marker (C2 §4.2, runtime §5) wherever these destinations apply; `null` when no scan ran. `incomplete: true` means the scan could not settle the full set (C2 §4.2); entries mean "observed during the scan", not "alive" |

## 6. Durable events

### 6.1 Envelope and types

```json
{"seq":41,"session_id":"s_7f3k9q2mzr4c","turn":2,"late":false,"at":"…","type":"action.denied",
 "kind":"command","target":"curl …","reason":"network disabled"}
```

`seq`: Core-assigned per session, dense from 1. `turn`: `null` for
session-level events. `late: true`: attributed to a turn already terminal
(vendor turn id mapped by the adapter); such events never change the
envelope except through §7.6.

| Type | Payload | Committed by |
|---|---|---|
| `session.opened` / `session.closed` / `session.reopened` | `route`, confirmed `vendor_session_id`, `vendor_version` / `reason`, `leftovers` (§5; non-null only when the close stopped the server) / `route`, confirmed `vendor_session_id`, `vendor_version`, `reason` | Core |
| `turn.queued` / `turn.submitted` / `turn.started` | `queue_position` / `attempt` / `effective` | Core |
| `turn.ended` | `state`, `failure?`, `stop_reason`, `cancel?` | **Core only** |
| `turn.revised` | `revision`, `from_state`, `state`, `evidence` (`late_terminal`); always `late: true` | Core |
| `action.denied`, `vendor.request_declined` | as envelope lists; `blocking` on declines | Adapter |
| `steer.delivered` | `delivery` | Adapter |
| `cancel.requested` / `cancel.settled` | — / `outcome`, `cleanup` | Core |
| `warning` | `code`, `message`, `data?` (structured, bounded; `config_switch_unverified` carries `data.categories: [{category, requested, effective}]`, §5) | either |
| `process.exited`, `server.lost` | `code`, `signal` / `key` | Core (from Host) |

Events are durable records only: an event exists when crash recovery or the
envelope depends on it. Model text, reasoning, tool calls, usage updates,
file changes and unknown vendor messages are not events; the agent's own
transcript keeps them (`logs`, §3.12), and a running turn's progress is in
`status` (§3.7).

For a delayed-init CLI such as Claude, `session.opened`/`session.reopened`
is committed exactly once per connection generation only after matching
vendor identity confirmation. Core atomically persists the confirmed ID and
verification flag with that event, before any acceptance derived from the
same message. A pre-init startup/resume rejection emits neither event, even
when it echoes the expected UUID. Matching init confirms identity, not turn
acceptance; prompt-associated evidence is still required.

Rust: `#[serde(tag = "type")]` on the serialize side, tags set with
`rename`; a client keeps an unknown type as `Other { type, payload }`.

### 6.2 Ordering

Per session FIFO in `seq`; no promise across sessions (D4). `turn.ended`
is the last non-late event of its turn.

## 7. States

### 7.1 Session

| From | To | Cause |
|---|---|---|
| — | `idle` | receipt committed |
| `idle` | `active` | a turn is dispatched |
| `active` | `idle` | turn resolved, queue empty, cleanup not `pending` |
| `active` | `active` | next queued turn dispatched (§7.3 gate) |
| any | `closed` | `close`; `daemon/stop --force`, for a session with unfinished work at force acceptance |

Vendor-process idle shutdown (Q4, 15 min, per harness) is **not** a state
change: the session stays `idle`, `status.process.alive` becomes `false`,
and the next `resume` reopens the vendor session (`session.reopened`).

### 7.2 Turn

| From | To | Cause |
|---|---|---|
| — | `queued` | accepted, committed |
| `queued` | `running/submitting` | dispatch: Core commits `submitted_at` **before** any vendor I/O (coding-style §7) |
| `running/submitting` | `running/accepted` | vendor acceptance committed (`accepted_at`) |
| `running/submitting` | `failed` (`submit_failed`) | the submission was rejected before acceptance, by the vendor with a definite error or by the adapter before any vendor submission |
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

The new daemon validates the Store and commits recovery before
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
evidence, never signalling authority. Cleanup and recovery perform no scan of
vendor environments and no vendor marker discovery, and there is no
daemon-side numeric TERM/KILL of a saved pid or pgid. The only environment
read is the report-only leftover scan (C2 §4.2): it may read the environment
of a same-uid process started at or after the vendor, through one
`/proc/<pid>` descriptor, solely to match the exact `VIA_PROCESS_MARKER`
entry; nothing from it is kept except the report, and the marker never
authorizes a signal or proves ownership or liveness. If the anchor is absent or unverified, Host does not signal.
Cleanup is `uncertain` unless a same-boot, same-PID-namespace, non-signalling
group query proves `ESRCH` for a persisted Host-created group with full
identity, generation and pgid > 1 (runtime contract §5.2). `Ok`, `EPERM`,
other errors or namespace mismatch do not prove absence. Positive group
absence can make cleanup `quiescent`; it cannot prove vendor terminal state,
protocol acknowledgement, forced outcome or reaping, and the restarted turn
remains `unknown`. Group absence covers only that group; recovered turns
carry `leftovers: null`. A stdio-attached shared server dies with the daemon (P3);
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
| Codex per-thread ingress/C2 stall overflow | running on affected thread generation | promptly resolve every nonterminal submitted turn under preceding disposition precedence, interrupt through reserved control, and block same-thread dispatch until clean reopen; preserve prior terminal envelopes and other threads |
| Observation or message overflow failed the connection | running | `failed(overflow)` |
| Submission rejected definitively | submitting | `failed(submit_failed)` |
| Daemon restart | any | §7.5 |
| Late vendor terminal for an `unknown` turn | unknown | revise to that state, `revision + 1`, `turn.revised`; a caller that already read the result must read it again. The terminal is attributed through the vendor turn ID recorded at acceptance, never by position, so a turn never accepted is not revised. Only a caller `cancel` or `close` that stopped the turn makes an `interrupted` terminal `cancelled`; otherwise (a Core deadline, a Store or protocol stop) it is classified as a vendor terminal (§8.2). A stored cancel outcome `unknown` or `requested` becomes `acknowledged` for an `interrupted` terminal; for any other terminal `unknown` becomes `requested`; `acknowledged` and `forced` stand |

Rows 1–2 cover caller-originated cancels (`cancel`, `close`). A Core-deadline
stop resolves `failed(deadline_*)` after rows 3–4. A `Deadline` coincident
with an order's `force_at` takes the order's cause. Only the resolution
cases in [Task 3 design](../workstreams/rust-foundation/t3/design.md) §7.2 (runtime §7) end `failed(store)`; a failed write of
a spilled `structured_output.json` (§5) is a failure of the commit that names
it and resolves the same way. A revision known not to have committed, after
the applicable retry, is not made and the turn stays `unknown` (a Store
failure scoped to the turn, which does not latch); an uncertain
revision commit latches and is reconciled under runtime §7, never assumed
absent. A natural
terminal whose one retry commits keeps its result, and a dispatcher-owned
queued cancellation whose retry commits stays `cancelled`.

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
| -32009 | `harness_unavailable` | binary missing, handshake check refused (C2 §5, `data.reason: handshake_refused`), server failed to start, or the stored adapter version is not compatible (`data.reason: adapter_version`) |
| -32010 | `unknown_model` | |
| -32011 | `queue_full` | |
| -32012 | `admission_refused` | resource/aggregate result cannot fit a bounded page or response; Store read lane full; disk free space below the floor; a session's control lane full for `steer` (`data.reason:"control_lane_full"`, §3.4) |
| -32013 | `no_active_turn` | |
| -32014 | `turn_mismatch` | |
| -32015 | `turn_not_finished` | |
| -32016 | `wait_timeout` | |
| -32017 | `daemon_stopping` | |
| -32018 | `store_error` | Before a receipt: `data.commit_outcome: not_committed\|unknown` and `retry: same_key_only` when uncertain. Before a receipt, `commit_outcome: not_committed` is also used for a request that was never enqueued because the writer's queue was full. A disconnected writer is `unknown` and latches. `commit_outcome` describes receipts only: a steer whose `steer.delivered` commit fails is `store_error` without data (the input was delivered; §3.4). A keyed steer whose intent or outcome could not be recorded is `store_error` without data. A rolled-back intent leaves no key. A key whose outcome committed but whose reply was lost replays that outcome. For an affected receipted nonterminal turn that cannot be resolved durably: `data.session`, `data.turn`, last-known `data.durable_state`, `data.terminal_persisted:false`. No terminal envelope is invented. |
| -32019 | `history_pruned` | `data.earliest_seq` |
| -32020 | `request_too_large` | request line over 1 MiB; `data.max_bytes`; the connection closes |
| -32021 | `steer_failed` | steer input refused, not acknowledged, or delivered without a record (§3.4): `data.reason` `not_steerable` (nothing was applied, `data.delivery:"none"`), `not_delivered` (writing began, in part or whole, without the vendor's acknowledgement; or, for a keyed steer, the daemon-generated outcome of a durable intent whose owner ended without recording an outcome, so the delivery outcome was not durably recorded, §3.4; whether it was applied is unknown, `data.delivery:"uncertain"`) or `not_recorded` (the vendor took it, `data.delivery` as on success, but no `steer.delivered` event records it) |

A receipt whose commit outcome is `unknown` latches Store failure (runtime
§7). Restart recovery settles it; a keyed retry after restart learns its
receipt. An unkeyed caller must not resend the request.

### 8.2 Turn failure classes (`failure.class`)

| Class | Meaning | Set by |
|---|---|---|
| `deadline_wall`, `deadline_idle` | Core deadline | Core |
| `submit_failed` | the submission was rejected before acceptance, either by the vendor or by the adapter before any vendor submission (a failed handshake check, or a parameter the discovered catalog rejects; C2 §5); `failure.data` names the adapter-side reason | Core (from adapter rejection or observation) |
| `resume_mismatch` | vendor returned a different or fresh session | Adapter observation |
| `vendor_error` | vendor turn failure; `vendor_code` keeps the vendor's code | Adapter |
| `rate_limit`, `auth`, `context_exceeded`, `budget_exceeded` | specific vendor classes | Adapter |
| `server_lost`, `process_exited` | Host-confirmed death | Core |
| `protocol` | malformed known message, or vendor stream contradiction | Adapter |
| `overflow` | this session's observation channel stalled past its limit, the connection's message queue overflowed, or a vendor message exceeded 1 MiB (C2 A1) | Core |
| `structured_output_invalid` | VIA validation failed (Q2) | Core |
| `daemon_restart`, `store` | §7.5; Store write failed after dispatch | Core |

Adapters never commit a class; they report observations and Core commits
(C2 §4). A vendor failure after acceptance is `failed` with the vendor's
class; `submit_failed` is only before acceptance; HTTP 401/403 → `auth`. `max_steps` reached with a normal result is `completed` with
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
- One narrow, owner-approved exception (2026-10-01), separate from the
  OpenCode password exception, which does not cover it: a report-only
  leftover scan (C2 §4.2) may read the environment of a same-uid process
  started at or after the vendor, through one `/proc/<pid>` descriptor,
  solely to match the exact `VIA_PROCESS_MARKER` entry. Its buffer can
  transiently hold credential values; it is compared in memory and dropped,
  nothing from it is kept except the report, and the marker never authorizes
  a signal or proves ownership or liveness.
- Prompts and outputs are private local data; retention is daemon config
  (Proposed 30 days); vendor processes get a per-adapter environment
  allow-list, never the caller's environment.
- `via serve --stdio` gives its parent full C1 access; local callers only.

## 10. Open questions

Confirmed by review (Astra): Q2 VIA validates
structured output; Q3 `wait` = latest turn at acceptance; Q4 15-minute
process shutdown, configurable; Q5 catalog-only `describe`. D9 stays open.
Owner, 2026-09-26: P12 approved as written; P7/P11 are resolved by
the reviewed Codex and Claude vendor packets. Vendor live gates remain open.
Owner, 2026-09-30 (OD1): P13's version rule supersedes the 2026-09-26 P13
approval.

| # | Question | Recommendation / alternatives |
|---|---|---|
| P7 | Codex pending cleanup | reviewed §3.5/§7.3 rule: settle at `min(acknowledged_at + 60 s, wall_deadline)`; do not add `--after-uncertain` |
| P11 | server key vs per-turn bound | reviewed §Decisions rule; Codex key excludes bound and mixed-bound operation awaits enforcement proof; OpenCode key includes full bound, owning VIA session and durable private namespace, refusing bound changes or cross-owner sharing |
| P12 | live recovery gate | `unknown` everywhere in v1; alternative: enable Codex rejoin after the socket-transport probe (D9) |
| P13 | version rule | Version rule: every vendor version is supported; refused only on demonstrated handshake breakage; `untested` warns until the maintainers' check (owner OD1, 2026-09-30, superseding the 2026-09-26 P13 approval) |
| Q6 | Claude steer semantics | resolved `unsupported`; busy input can merge into a running result, so no steer input is written |
| Q7 | Channel sizes (C2 A1 limits) | as written, fixed; disk and WAL thresholds are daemon config (runtime §8) |
