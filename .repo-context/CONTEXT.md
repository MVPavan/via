# VIA

VIA is a cross-harness CLI: one static binary that runs an explicitly
configured coding agent on a prompt and returns a structured result envelope.
Roles, if used, are caller policy. Sources:
`docs/workstreams/handoff.md`, `docs/brainstorms/README.md`,
`docs/brainstorms/access-methods.md`.

## Harnesses and adapters

**Harness**:
A coding-agent product VIA drives through its vendor's own binary or server (Claude Code, Codex, OpenCode, Pi, …).
_Avoid_: agent (ambiguous), provider, backend

**Tier 1 / Tier 2**:
Tier 1 is a model provider's own harness (Claude Code, Codex, Antigravity); tier 2 is a multi-provider aggregator (OpenCode, Cursor, Pi, …).

**Adapter**:
L3's per-harness integration: maps canonical operations and parameters to a chosen route, declares capabilities, normalizes events, and classifies failures. Pinned and contract-tested against specific vendor versions, with a recorded terms status.
_Avoid_: profile, driver, plugin

**Terms status**:
The recorded position of a vendor's terms on automated access through an adapter's route; informs disabling an adapter, never gates development.

**Drift**:
A vendor binary, SDK or bridge version outside the versions an adapter was contract-tested against.

## Routes

**Route**:
L4's protocol client for reaching a harness: vendor CLI, vendor RPC or native ACP for now. One session keeps one route for its life; fallback is allowed only before submission.
_Avoid_: transport, channel

**CLI route**:
VIA uses the vendor binary in its documented headless mode and reads structured output and exit status. A route may start a fresh process for a later turn while resuming the same vendor session.

**SDK route**:
A vendor library integration style, not a wire mechanism; most SDKs spawn a vendor binary underneath. VIA does not use SDK routes for now. Revisit conditions are in `docs/brainstorms/routes-decision.md`.

**Vendor RPC route**:
A vendor-specific server started by VIA and spoken to through the vendor's own protocol (Codex `app-server`, Pi `--mode rpc`). VIA never attaches to or stops a server it did not start.
_Avoid_: app-server (one instance of it)

**ACP route**:
Zed's Agent Client Protocol: JSON-RPC over stdio to a native ACP agent subprocess. Bridged ACP is not used for now.
_Avoid_: Agent Communication Protocol (IBM/BeeAI's, merged into A2A)

**Bridge**:
A separate program that translates ACP to a harness's own surface (`codex-acp`, `claude-agent-acp`, community `pi-acp`); each bridge adds a layer and a failure domain.

## Sessions and turns

**Session**:
One resumable conversation with one agent. VIA mints its id; it has a
caller-owned handle and its own queue. Its route and adapter version stay fixed.
Its permission bound carries over to every turn unchanged unless the caller
explicitly sets a new one on resume through the handle. VIA revalidates the new
bound against the route and records it per turn; nothing changes it silently.
It maps one-to-one to a vendor session. States: idle, active, closed.
_Avoid_: job, task

**Turn**:
One caller prompt, the agent's tool calls, and one result envelope. Identified by session id and turn number (`s_7f3/2`). States: queued, running, completed, failed, cancelled, unknown. `spawn` starts turn 1; `resume` adds a turn; steer and cancel target the active turn.
_Avoid_: step

**Step**:
One model step inside a turn. Claude Code's `--max-turns` and `num_turns` count steps; VIA's `max_steps` maps to `--max-turns`.

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
The structured result for a turn: session id and turn number, adapter and version, route, vendor session id, model/effort, terminal state, exit code, stop reason, final text, usage, cost with provenance, timestamps, log reference, tree pins, denied actions and auto-declined requests.
_Avoid_: result object, output

**Launch receipt**:
The turn's intent persisted before dispatch; if the daemon dies around submission and the outcome cannot be established, the turn remains *unknown* and the prompt is never resent automatically.

**Store**:
Durable session, turn, event and process records behind a small SQLite storage interface. The daemon is its only writer; L5's tap writes a raw log per connection, while normalized events are indexed per turn.
_Avoid_: database, ledger (the parent repo's record store)

**Worker**:
An agent process or task supervised by the VIA daemon; it does not own VIA's session lifecycle or Store.
_Avoid_: daemon

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
L6, the only layer that starts and supervises vendor processes and servers. It reports survivors by VIA marker after a crash and never stops servers VIA did not start.
_Avoid_: daemon as a synonym

**Wire**:
L5, responsible for framing, byte transport, bounded pipe draining and the exact-byte raw tap per connection. It does not interpret vendor messages.

**Layer**:
One of six library responsibility boundaries, L1 Interface through L6 Host. The daemon is the process containing the server half of L1 and L2–L6, not a seventh layer.

**Contract**:
A boundary named for its providing layer: C2 Adapter, C3 Route, C4 Wire, C5 Host and S Store. The public C1 contract is named VIA API.

**Passthrough**:
A mode that forwards native harness arguments unchanged; its results are marked unstructured.

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

## Consumers

**Thin SDK**:
A small per-language library that uses the VIA binary as a C1 client; no in-process native binding.

**Foreman**:
The parent repo's workflow-interpreter component that is to call VIA to run crews.
