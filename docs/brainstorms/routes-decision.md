# Agent routes: direct CLI and vendor server protocols

Status: decided 2026-09-26; review r1 applied.

This decision supersedes the provisional CLI-first and SDK routes in
`docs/brainstorms/access-methods.md` §6
and `.repo-context/invariants.md`. It applies to initial adapters and their
later verbs. The owner decisions in `docs/brainstorms/README.md` §15
also replace invariant 5's no-daemon topology.

## 1. Question and requirements

VIA can reach an agent through its CLI, its own server protocol, native ACP
(Agent Client Protocol), bridged ACP, or an SDK. Several use JSON-RPC, but
their methods and failure semantics differ. Which route should each adapter
use?

- **Lifecycle:** create and resume a session, submit a turn, stop, queue and
  check progress. A session outlives its caller. A caller handle is bearer
  authority for resume, queue, steer, cancel and close; protect it as a secret.
- **Resources:** support concurrent agents with bounded memory, startup cost
  and blast radius. The workflow crew's target concurrency has not been set;
  the measurements below cover 1, 5, 15 and 32 sessions.
- **Interface:** expose declared verbs with named refusals and one result
  envelope. The workflow crew is the caller that dispatches coding agents;
  the inspector is a later caller of the same VIA API.
- **Directness and bounds:** use the vendor's own surface where possible.
  Enforce the declared bound through the vendor's mechanisms when available;
  VIA does not add an approval UI. A route without a verified bound refuses
  that bound. External isolation remains an open decision.
- Keep the single static `via` binary and never read vendor credentials
  (`.repo-context/invariants.md` 1, 3 and 4).

## 2. Route is separate from process lifetime

JSON-RPC supplies request ids, methods and responses, not a common agent API.
Codex app-server has `thread/start`, `turn/start` and `turn/steer`; ACP has
`session/new`, `session/prompt` and `session/cancel`. Claude stream-json has
its own control messages. Codex and ACP may use pipe framing; `opencode serve`
uses HTTP. Framing, request correlation and event parsing belong to each
transport and protocol, with a shared result envelope above them.

| Route | Examples | Process shape |
|---|---|---|
| CLI | `claude -p` stream-json, `codex exec --json` | Usually a vendor process per turn or session |
| Vendor server | `codex app-server`, `opencode serve`, Pi `--mode rpc` | A private server or compatible shared server; Pi needs a process per session |
| Native ACP | Vendor's own ACP mode | Multiplexing depends on the vendor; not measured here |
| Bridged ACP | `claude-agent-acp`, `codex-acp` | Bridge plus underlying agent processes |
| SDK | Official typed library | May wrap a CLI/server or embed an agent; Pi and Oh My Pi are reported in-process cases |

A vendor server could be started per session, kept as a bounded pool, or
shared among compatible sessions. A private Codex app-server retains its
control verbs with CLI-like isolation; sharing yields the measured density
benefit. The owner chose one VIA daemon per user: it owns all vendor processes,
connections and the Store, and starts its own servers. It never attaches to
or stops a server it did not start. Clients reach the daemon through the
user-only C1 socket; they do not own vendor connections. Shared servers are
keyed by vendor, version, config hash and bound. Host owns process lifetime;
Core owns admission and per-session queue limits. Cap sessions per server
against its blast radius. Route fallback is permitted only before submission;
a session retains its route and adapter version through later turns.

There is no per-turn lease or fencing protocol. The daemon takes an OS file
lock; on a crash the OS releases it. After restart, Core reconciles the
Store with vendor processes that Host reports by VIA marker. An uncertain
turn remains `unknown`; VIA never resends an ambiguous prompt automatically.
The client pings and restarts a hung daemon. These choices resolve the
old invariant 5 conflict; the daemon is a process that contains the layers,
not an extra agent route.

## 3. Evidence and its limits

The local WSL2 benchmark used 32 cores, 31 GB, trivial turns and small or
free models. `scratchpad/headless-bench/analysis.md` and
`scratchpad/headless-bench/acpx-analysis.md` hold methods and raw references.
Numbers below are baseline-adjusted, one-second-sampled peak anonymous MB at
32 concurrent sessions. CLI/server figures came from the full benchmark;
acpx figures came from a separate run with different native baselines. The
acpx 32-session conditions had one repetition. These are density evidence,
not resource limits or native Linux/macOS acceptance results.

| Agent | CLI, full | Server, full | acpx one-shot | acpx persistent |
|---|---:|---:|---:|---:|
| Claude | 4,440 | — | 9,304 | 12,624 |
| Codex | 3,384 | 712 | 6,523 | 8,422 |
| OpenCode | 14,948 | 642 | — | 18,749 |

At 32 sessions, Codex's server used about one fifth of CLI memory; OpenCode's
used about one twenty-third. At one session, server cost was similar to CLI.
Only OpenCode's server flattened the measured startup CPU burst: 1.5 peak
cores at 32 sessions versus 36 for its CLI. Codex app-server peaked at 25.2
cores versus 21.9 for Codex CLI. OpenCode CLI saw 5 of 184 concurrent turns
fail with `database is locked`; the tested shared server saw none. Sharing a
server also shares failure: killing it lost all five test sessions.

In OpenCode's abort test, 3 of 16 non-target sessions returned off-target
final replies. Causation was not established. Kill tests did not cover
cleanup during an executing tool. A VIA-owned `opencode serve` may still
contend on SQLite with the user's separate interactive OpenCode. Shared
OpenCode needs paired fault/control probes, active-tool cancellation and
descendant cleanup before enablement.

The acpx topology added a Node client and bridge for Claude/Codex, about
90–120 MB per session in those tests, and kept a process tree per idle
session. VIA would replace the acpx client; this does not prove a universal
2× cost for bridged ACP. A direct VIA-to-bridge route and shared native ACP
were not measured. Tested bridges did lose permission fidelity: Codex and
OpenCode wrote under acpx `--deny-all`, and codex-acp's `read-only` mapped to
workspace-write. A Codex server death behind codex-acp left a session wedged
while status still reported healthy.

Claude Agent SDK 0.3.257 was observed spawning the stream-json Claude CLI
through a Node/Python host. Its bundled agent was verified as Claude Code
2.1.257, about 215 MB, behind the installed 2.1.282 at the time. Its default
prompt differs from `claude -p` according to vendor documentation, but that
default was not tested live. The SDK's request union contained 35 control
request types; that count alone does not prove every operation works directly.
Other SDKs differ: Codex TS wraps the CLI, Codex Python drives app-server,
OpenCode's SDK drives `serve`, while Pi/Oh My Pi embed the agent.

## 4. Decision

1. **Use a vendor server where one exists, otherwise its CLI.** Codex uses
   VIA-owned `codex app-server`; OpenCode uses VIA-owned `opencode serve` once
   its isolation and bound gates pass. Claude uses `claude` with the same
   stream-json control protocol as its SDK, without
   `--permission-prompt-tool stdio`. Pi uses private `--mode rpc`; Muse uses
   `muse serve`. `codex exec` remains a pre-submission fallback. Check each
   other harness against its first-party surface before adding an adapter.
2. **Use native ACP for breadth**, when it is a harness's best first-party
   surface and passes the adapter checks in
   `docs/brainstorms/access-methods.md` §6. Do not use bridged ACP for now.
   acpx is a design reference, not a runtime
   dependency. ACP core lacks role instructions, sandbox policy, structured
   output and steer; do not advertise those verbs without a verified
   extension. Stable session `usage_update` does not imply stable per-turn
   usage; the latter was a draft RFD in the reviewed snapshot.
3. **Do not use SDK routes for now.** A VIA-supervised SDK sidecar could outlive
   its caller; lifecycle alone is not a reason to reject it. The current
   choice favors one binary and direct vendor transport over sidecar packaging
   and version coupling. It accepts protocol maintenance and temporarily
   partial verbs, notably Cursor's SDK-only status and cost. Copilot's direct
   runtime JSON-RPC remains unverified and is not yet an enabled route.
4. **Never turn a permission callback into an approval UI.** Each session
   starts with actions outside its bound denied by vendor configuration, not
   asked. The bound carries over to every turn unchanged unless the caller
   explicitly sets a new one on resume through the handle. Revalidate the
   effective bound against the route on every resume and record it per turn;
   nothing changes it silently.
   L3 automatically declines vendor and ACP requests under a deadline,
   including unknown request types,
   and records the decline in the turn envelope. ACP's optional permission
   consultation does not itself establish whether an agent enforces a bound;
   verify each adapter with a live denial probe. An ACP-only agent without a
   sandbox can declare only `full`. Whether VIA may wrap OpenCode in an
   external sandbox is still open; no bounded OpenCode session is enabled
   until that decision and a passing probe.

## 5. Costs, gates and revisit conditions

VIA maintains protocol clients for vendor surfaces that may change: Claude's
control protocol is undocumented, and Codex app-server is experimental. Pin
tested versions, keep recorded contracts, and run live integration probes;
recorded fixtures alone miss vendor drift. Test Codex per-thread cwd/config
and sandbox isolation, per-session abort, and server-loss reconciliation.
Test OpenCode HTTP bind, authentication and CORS; never expose a shared
listener by accident. Validate on native Linux and macOS before enabling an
adapter. Record route-specific terms and billing status; Codex's headless
terms remain ambiguous. Vendor loss may leave turns `unknown`; it is never
an automatic resend trigger. Whether shared vendor servers attach over stdio
or a Unix socket remains open. Windows socket discovery is later platform work.

Revisit SDKs if a required verb exists only there, a vendor deprecates its
CLI/server, or direct protocol maintenance becomes costlier than a supervised
sidecar. Revisit ACP primacy if stable role, sandbox, usage and steer support
arrives and Claude/Codex expose it natively. Revisit Claude routing if
Anthropic ships a server or documents its control protocol. Recheck terms or
metering changes for headless use. The workflow crew's actual concurrency
and a sanitized public benchmark summary remain to be established; the
measured data is local to `scratchpad/`. The detailed testing policy remains
open, with end-to-end checks as the stated direction.
