# VIA

VIA is a cross-harness CLI: one static binary that runs an explicitly
configured coding agent on a prompt and returns a structured result envelope.
Roles, if used, are caller policy. Sources:
`docs/workstreams/handoff.md`, `docs/brainstorms/README.md`,
`docs/brainstorms/access-methods.md`.

First-release harnesses: Claude Code, Codex, OpenCode and Pi. ACP and other
harnesses remain extension concepts, not first-release requirements.

## Harnesses and adapters

**Harness**:
A coding-agent product VIA drives through its vendor's own binary or server (Claude Code, Codex, OpenCode, Pi, …).
_Avoid_: agent (ambiguous), provider, backend

**Adapter**:
L3's per-harness integration: maps VIA operations and parameters to a chosen route, declares capabilities, turns vendor messages into events, and classifies failures. Pinned and contract-tested against specific vendor versions.
_Avoid_: profile, driver, plugin

**Harness process**:
A vendor process VIA started and owns, with its anchor: a per-turn process (CLI route, Pi RPC) or a shared server (Codex app-server, OpenCode serve). Each holds one slot of the daemon's `harness_processes.limit` for its life; a shared server holds one slot however many sessions it serves.
_Avoid_: connection (Wire's connection is the byte stream to one process), agent instance

## Routes

**Route**:
L4's protocol client for reaching a harness: vendor CLI, vendor RPC or native ACP for now. One session keeps one route for its life; fallback is allowed only before submission.
_Avoid_: transport, channel, SDK route (VIA does not use vendor SDKs; see `docs/brainstorms/routes-decision.md`)

**CLI route**:
VIA uses the vendor binary in its documented headless mode and reads structured output and exit status. A route may start a fresh process for a later turn while resuming the same vendor session.

**Vendor RPC route**:
VIA speaks the vendor's own RPC protocol to a process VIA started. The protocol does not decide process lifetime: Codex `app-server` is a shared server; Pi `--mode rpc` runs one private process per turn. VIA never attaches to or stops a process it did not start.
_Avoid_: app-server (one instance of it)

**ACP route**:
Zed's Agent Client Protocol: JSON-RPC over stdio to a native ACP agent subprocess. An extension concept; ACP bridges (`codex-acp` and similar) are not used.
_Avoid_: Agent Communication Protocol (IBM/BeeAI's, merged into A2A)

## Sessions, turns and events

**Session**:
One resumable conversation with one agent. VIA mints its id; it has a
caller-owned handle and its own queue. Its route and adapter version stay fixed.
Its permission bound carries over to every turn unchanged unless the caller
explicitly sets a new one on resume through the handle. VIA revalidates the new
bound against the route and records it per turn; nothing changes it silently.
It maps one-to-one to a vendor session. States: idle, active, closed.
_Avoid_: job, task

**Turn**:
One exchange between VIA's caller and the agent: the caller's prompt goes in, one result envelope comes out, and events record what happened in between. Identified by session id and turn number (`s_7f3/2`). `spawn` starts turn 1; `resume` adds a turn; steer and cancel target the running turn. A session runs at most one turn at a time; later turns wait in its queue.
States: queued, running, completed, failed, cancelled, unknown. Core records the start as `turn.started` when the agent accepts the prompt (the vendor's acceptance message). It records the end as `turn.ended` when the agent's terminal message arrives (Claude Code `result`, Codex `turn/completed`), or when VIA ends the turn itself: cancel, deadline, process exit or overflow.
_Avoid_: step, run

**Step**:
One model step inside a turn. Claude Code's `--max-turns` and `num_turns` count steps; VIA's `max_steps` maps to `--max-turns`. VIA counts a step each time the model produces output after tool results, the same way for every vendor, and records one `steps` row per step; the envelope's `steps` is the vendor's own count.

**Vendor message**:
One message the agent sends VIA as JSON text: an NDJSON line on a CLI's stdout, a JSON-RPC notification from a vendor server, or an SSE event. It is the adapter's input. Wire splits the byte stream into messages and caps their size without decoding them; the route decodes each one into a typed struct. An unknown message type is activity only; a malformed known message is a protocol failure (adapter contract §1).
_Avoid_: frame, vendor event

**Observation**:
What an adapter reports from one vendor message. Core turns an observation into a durable event, a progress update, or envelope accumulation.
_Avoid_: event (reserved for durable records)

**Event**:
VIA's durable record of a lifecycle, control or safety fact in a session, such as `turn.started`, `cancel.requested`, `action.denied` or `turn.ended` (full list in VIA API §6). It is harness-neutral. Most events belong to one turn; a few (`session.opened`, `session.closed`) belong to the session. Each has a dense per-session `seq`. Core commits it to the Store, and callers page it with `events`. Model text, tool calls and usage are not events: they drive the progress snapshot and step rows, and the agent's own transcript keeps them.
_Avoid_: frame, notification, message (reserved for vendor messages)

**Event trace**:
The ordered events of one turn, from `turn.queued` to `turn.ended`, read by `seq`. The session's trace is its turns' traces interleaved with its session events.
_Avoid_: agent trace, log

**Evidence folder**:
One folder per turn under VIA's state directory, holding the agent's stderr, the message VIA failed to decode (if any) and a final text too large for the envelope. VIA keeps no copy of vendor traffic; the agent's own transcript keeps the conversation.

**Run**:
Deliberately not a VIA entity; use session or turn for VIA lifecycle concepts. Ordinary English may still say "run an agent."
_Avoid_: run id, run store

**Vendor session id**:
The harness's own conversation identifier, stored opaque and scoped to adapter and version; used for resume and event demultiplexing.
_Avoid_: VIA session id

**Caller handle**:
Bearer authority for every mutation of its session: resume, queue, steer, cancel and close. Keep it inside the caller/daemon trust boundary; possession authorizes those actions.
_Avoid_: public session id as authority

**Role**:
Caller policy for choosing explicit VIA parameters, not a VIA entity. VIA accepts harness/model, effort, instructions, permission bound, cwd, output schema and namespaced vendor options; a model catalog maps model to harness.

**Envelope**:
The structured result for a turn: session id and turn number, adapter and version, route, vendor session id, model/effort, terminal state, exit code, stop reason, final text, usage, cost with provenance, timestamps, evidence locations, tree pins, denied actions and auto-declined requests.
_Avoid_: result object, output

**Submission intent**:
The durable `turn.submitted` record committed before VIA sends the prompt. If the daemon dies around submission and the outcome cannot be established, the turn ends *unknown* and the prompt is never resent automatically.
_Avoid_: launch receipt (a receipt is the reply to spawn/resume)

**Store**:
VIA's SQLite database, written only by the daemon: sessions (parameters, state, handle hash), turns (prompt, state, envelope), events, step rows, spawn and operation keys for retry replay, and anchors (VIA's process-supervision records). Evidence files live in the evidence folder.
_Avoid_: database, ledger (the parent repo's record store)

**VIA daemon**:
The one process per user that owns agent processes, vendor connections and the Store. It hosts L1's server half plus L2–L6 and the Store; its `main` wires them and handles the single-instance lock, idle exit and signals. It does not constitute another layer.
_Avoid_: host (reserved for L6)

**VIA API**:
The public, versioned C1 JSON-RPC 2.0 contract spoken by clients to the daemon over a user-only Unix socket. The CLI auto-starts the daemon; `via serve --stdio` and thin SDKs are clients too. Mismatched client and daemon versions are refused.
_Avoid_: Run API

**Core**:
L2, responsible for session and turn lifecycle, admission, deadlines, queues, caller authority and envelope assembly.
_Avoid_: Run Core

**Host**:
L6, the only layer that starts and supervises vendor processes and servers. It reports survivors by VIA marker only where C2 §4.2 gives the report a destination (never after daemon-crash recovery) and never stops servers VIA did not start.
_Avoid_: daemon as a synonym

**Wire**:
L5, which moves bytes to and from vendor processes: it drains pipes within bounds, splits output into vendor messages, and creates the turn's evidence folder. It does not interpret vendor messages.

**Layer**:
One of six library responsibility boundaries, L1 Interface through L6 Host. The daemon is the process containing the server half of L1 and L2–L6, not a seventh layer.

**Contract**:
A boundary named for its providing layer: C2 Adapter, C3 Route, C4 Wire, C5 Host and S Store. The public C1 contract is named VIA API.

**Passthrough**:
Raw vendor CLI arguments a session passes unchanged (`via spawn … -- ARGS`,
C1 `vendor_args`), frozen at spawn and appended to every launch; flags the
route reserves are refused, and every result of the session carries the
`vendor_passthrough` warning. First-release scope (owner, 2026-10-06).
_Avoid_: vendor options (the `vendor` parameter's keyed options)

**Cost provenance**:
A cost label: `reported` (vendor's figure), `estimated` (VIA computed), or `unavailable`.

## Verbs

**spawn**:
Create a session and turn 1; `--background` returns their address instead of waiting.

**resume**:
Continue a session with a new turn; a vendor-session-id mismatch or fresh vendor session is an error, never a silent fork.

**steer**:
Inject input into a turn that is still running.
_Avoid_: follow-up (that is resume)

**cancel**:
Stop a running turn through the route's own mechanism.
_Avoid_: kill (process kill is never presented as cancel)

**status**:
Report a session or turn's current state.

**result** / **wait**:
Return a turn's envelope; `wait` blocks until a background turn ends.

**Capability declaration**:
Each adapter's per-verb mark: `native`, `partial` (with stated semantics), or `unsupported`.

**Named refusal**:
The error VIA returns, naming the verb and adapter, when a verb is unsupported or a requested bound cannot be expressed.
_Avoid_: fallback, emulation
