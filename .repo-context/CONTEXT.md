# VIA

VIA is a cross-harness CLI: one static binary that runs a role and prompt on
any coding-agent harness and returns a structured result envelope. Sources:
`docs/workstreams/handoff.md`, `docs/brainstorms/README.md`,
`docs/brainstorms/access-methods.md`.

## Harnesses and adapters

**Harness**:
A coding-agent product VIA drives through its vendor's own binary or server (Claude Code, Codex, OpenCode, Pi, …).
_Avoid_: agent (ambiguous), provider, backend

**Tier 1 / Tier 2**:
Tier 1 is a model provider's own harness (Claude Code, Codex, Antigravity); tier 2 is a multi-provider aggregator (OpenCode, Cursor, Pi, …).

**Adapter**:
VIA's per-harness integration over one route, pinned and contract-tested against specific vendor versions, with declared verb capabilities and a recorded terms status.
_Avoid_: profile, driver, plugin

**Terms status**:
The recorded position of a vendor's terms on automated access through an adapter's route; informs disabling an adapter, never gates development.

**Drift**:
A vendor binary, SDK or bridge version outside the versions an adapter was contract-tested against.

## Routes

**Route**:
The access mechanism an adapter uses to reach a harness: CLI, SDK, vendor RPC, or ACP. A managed run uses one route for its whole life.
_Avoid_: transport, channel

**CLI route**:
VIA spawns the vendor binary once per turn in its documented headless mode and reads JSON/JSONL output and the exit code; continuity comes from a vendor session id passed back later.

**SDK route**:
A vendor library called in VIA's process; an integration style, not a wire mechanism, since most SDKs spawn a vendor binary underneath.

**Vendor RPC route**:
A long-lived vendor-specific server VIA spawns or connects to and speaks the vendor's own protocol with (Codex `app-server`, Pi `--mode rpc`).
_Avoid_: app-server (one instance of it)

**ACP route**:
Zed's Agent Client Protocol: JSON-RPC over stdio to an agent subprocess, spoken natively or through a bridge.
_Avoid_: Agent Communication Protocol (IBM/BeeAI's, merged into A2A)

**Bridge**:
A separate program that translates ACP to a harness's own surface (`codex-acp`, `claude-agent-acp`, community `pi-acp`); each bridge adds a layer and a failure domain.

## Runs

**Run**:
One managed execution of a role and prompt on one adapter, identified by a VIA-minted run id and persisted in the run store.
_Avoid_: job, task, session

**Run id**:
VIA's identifier for a run, minted before dispatch and distinct from the vendor session id.

**Vendor session id**:
The harness's own conversation identifier, stored opaque and scoped to adapter and version; used for resume.
_Avoid_: run id

**Role**:
A named binding that selects model, effort and hence harness for a run; the alternative is explicit `--model`/`--effort`.

**Envelope**:
The single structured result for every harness: adapter and version, route, run id, vendor session id, model/effort, terminal state, exit code, stop reason, final text, usage, cost with provenance, timestamps, log path, tree pins.
_Avoid_: result object, output

**Launch receipt**:
The run's intent persisted before dispatch; if the process dies around submission the run is *unknown* and the prompt is never resent automatically.

**Run store**:
The durable run record with idempotent keys, kept in SQLite behind a storage interface.
_Avoid_: database, ledger (the parent repo's record store)

**Worker**:
The one process per run that owns a run's lifecycle; there is no daemon.

**Passthrough**:
A mode that forwards native harness arguments unchanged; its results are marked unstructured.

**Cost provenance**:
A cost label: `reported` (vendor's figure), `estimated` (VIA computed), or `unavailable`.

## Verbs

**spawn**:
Start a new run; `--background` returns a run id instead of waiting.

**resume**:
Continue a run's vendor session with a new turn; a session-id mismatch or fresh session is an error, never a silent fork.

**steer**:
Inject input into a turn that is still running.
_Avoid_: follow-up (that is resume)

**cancel**:
Stop a running turn through the route's own mechanism.
_Avoid_: kill (process kill is never presented as cancel)

**status**:
Report a run's current state.

**result** / **wait**:
Return a run's envelope; `wait` blocks until a background run ends.

**Capability declaration**:
Each adapter's per-verb mark: `native`, `partial` (with stated semantics), or `unsupported`.

**Named refusal**:
The error VIA returns, naming the verb and adapter, when a verb is unsupported or a requested bound cannot be expressed.
_Avoid_: fallback, emulation

## Consumers

**Thin SDK**:
A small per-language library that spawns the VIA binary; the only programmatic interface (no in-process native binding).

**Foreman**:
The parent repo's workflow-interpreter component that is to call VIA to run crews.
