# Layer design review r1: consolidation

Reviewed: `system-layers.md` and `layers-and-names.md` (2026-09-25 drafts).
Reviewers, independent, same brief: GPT-6 Sol high (`layer-design-sol-r1.md`),
GPT-6 Astra high (`layer-design-astra-r1.md`), Claude Fable 5.1 high
(`layer-design-fable-r1.md`). Consolidated by the orchestrator (Claude Opus
5.5); no separate judge. All three verdicts: **SOUND WITH CHANGES**.

All three keep the six layers, the side Store and the C2 boundary. All three
swept the 20-harness matrix and found **no harness that forces a change to
the shape of C1 or C2**, provided "partial" and "refused by name" are valid
outcomes. They also agree this proves the interface premise, not equal
capability: a route change can still change capabilities, bounds, cost and
failure modes, which C1 must report.

## Agreed by all three

| # | Finding | Severity | Fix |
|---|---|---|---|
| A | **Shared-server connection ownership is undefined.** C5 hands out "a session on a shared server" and returns the server's stdio, but a protocol-blind Host cannot split one connection between runs, several VIA processes cannot share one stdio pipe, and if the process holding it dies every run on the server is lost, so takeover is unreachable for exactly the density routes. | blocking | Pick a topology (see "Disagreements"). Either way, C5 supplies processes, endpoints and exit observations, not vendor sessions. |
| B | **Lease expiry is not safe takeover.** A paused owner can wake after its replacement starts (two writers, stale cancel); SQLite alone cannot deliver a command to a live host or rebuild a lost pipe. | blocking (Sol, Astra) / part of A (Fable) | Atomic lease with increasing generation numbers (fencing); process identity beyond PID; durable command IDs with acknowledgement for cancel and steer; separate recovery cases (host alive, host dead with vendor child alive, server lost); keep "outcome unknown" distinct from failure and never resend it. |
| C | **Nobody answers vendor requests.** Codex sends approvals, `requestUserInput`, MCP elicitation, `item/tool/call` and token refresh; ACP sends `request_permission`; `approvalPolicy: never` silences only approvals. An unanswered request hangs the run. | major | L4 correlates and enforces a deadline; L3 answers every request deterministically from the run's declared bound (decline or cancel the turn), unknown requests included, and emits a canonical event. Not a VIA permission layer: it applies the "never ask" bound. Interactive answers later need a narrow C1 verb. |
| D | **A per-run raw log is impossible on a shared connection.** L5 sees one interleaved byte stream; splitting by thread is protocol knowledge (L4). | major | Raw log per connection (endpoint) with direction and offsets; L4 indexes per-run events into it; `via logs R` is a filtered view that never shows another run's traffic. State who assigns event sequence numbers; C2 guarantees per-run order only. |
| E | **Deadlines, cancel escalation and flow control have no owner.** Timeout is only a failure class; cancel jumps straight to Cancelled; Pi stalls if stdout is not drained; cancel during a running tool was never tested. | major | L2 owns deadlines and queue limits; L3 runs the vendor's cancel sequence (Pi: clear queue, abort, wait for settle); L6 escalates by timer; outcomes `requested`, `acknowledged`, `forced`, `unknown`. Never kill a shared server to cancel one run. L5 drains into bounded buffers without blocking pipe reads. |
| F | **C3 is a family, not one contract.** Only open, close and health are common; everything else is per-protocol typed calls. Request-ID pairing belongs in L4, not C4. | minor | Say so. C2 is the substitutable boundary the premise needs. |
| G | **Recorded decisions and glossary are stale.** Invariant 5 ("no daemon, one worker per run") is still listed as decided; handoff property 1 ("one turn per process"); glossary entries for Worker, Role and CLI route. | blocking (Sol) / minor | Mark them "under revision" now, without deciding the daemon question. |

## Agreed by two

- **Run, turn and session identity conflict** (Sol, Astra). The state machine
  resumes an idle *run*, but the records allow many runs per session. Decide:
  each prompt is a new run with an immutable envelope, or a turn within one
  run. Require the caller handle for every mutation (resume, queue, close),
  not only cancel and steer; persist route, adapter version and effective
  bounds with the session so a resume cannot silently change them.
- **Capabilities must be fixed before spawn** (Astra, Fable). A caller that
  needs steer could silently get `codex exec`. Add a preflight on C2 that
  returns the effective route and capabilities (Astra: prepare/describe), and
  a `--require <verbs>` on spawn (Fable); return the chosen route and its
  capabilities in the launch receipt. Fallback only before submission.
- **Server keys and exclusive processes** (Astra, Fable). Copilot fixes
  config per server; Pi and Oh My Pi hold one session per process and need
  a "private per-run server" shape that L6 does not draw yet.

## Raised by one reviewer

Fable:
- Server start needs a claim: two VIA processes can both start a Codex
  server. SERVERS row with a unique key, `starting/ready/stopping` state and
  a lease; an expired VIA-marked server with live threads is adoptable, not
  reaped.
- OpenCode has no native bound, so a shared OpenCode server can serve only
  one bound; key servers by (vendor, version, config, isolation bound). This
  is a VIA-supplied bound, which is still an open owner decision.
- ACP-only harnesses expose no sandbox: declare them bound `full` only.
- `codex exec resume` drops the sandbox flag: re-validate the bound on every
  continue.
- Idle Claude sessions hold a ~145 MB process each; state an idle policy.
- Kill signals must pass through L4 and L5 to reach L6 (`close(mode)` on C3
  and C4), or Host becomes a side component like Store.
- Naming: "session" is overloaded (vendor session, C5, SESSIONS table,
  `via serve`); "Host" names a layer, a run host and a server host.

Astra:
- Oh My Pi distinguishes `prompt_result` from `session_settled`; bare
  `agent_end` is not enough; its session switch can abort current work.
- Cursor's ACP sends blocking question and plan requests.
- Store: short transactions, none spanning vendor I/O; bounded lock waits;
  disk-full handling; retention.
- Over ACP, advertise no client filesystem or terminal capabilities.
- The caller handle is bearer authority; document its trust boundary.

Sol:
- L4 and L5 need not be separate packages until reuse justifies it.

## Disagreement: how to share a vendor server

| Reviewer | Proposal | Consequence |
|---|---|---|
| Astra | One VIA process owns each vendor connection, hosts L4/L5 for all its runs and routes traffic; other VIA processes reach it over IPC | A VIA-owned long-lived process: the daemon question answered "yes" for server routes |
| Fable | Shared servers listen on a socket only (Codex `--listen unix://`, OpenCode HTTP already); each VIA process connects as its own client; the SERVERS row is the discovery record; Codex `thread/resume` rejoins a running thread from another client | No VIA daemon needed; the vendor server is the shared thing. Needs a server-start claim and an idle-stop rule. Multi-client routing untested |
| Sol | Owner chooses: keep "no daemon" with per-run vendor servers, or revise the invariant and specify the shared host | Frames it as a decision, not a design |

Orchestrator view: Fable's option fits both owner positions (shared server,
daemon undecided) and is testable. Verified by the orchestrator: the Codex
schema says "If thread_id identifies a running thread, app-server rejoins
that thread" (`ThreadResumeParams`). Unverified: whether notifications and
server requests reach the right client over a Unix socket. Fable's
prototype 1 settles it.

## Prototype first (merged order)

1. Codex app-server on a Unix socket with two VIA processes: start a run
   from A, kill A during a tool, rejoin from B, then kill the server. Decides
   the sharing topology, C5 and takeover together.
2. Claude control route: interrupt, stream-json input (steer or queue), host
   death during a tool, no `--permission-prompt-tool`.
3. Store under multi-process WAL: lease fencing, server-start claim race,
   renewal under load.
4. Cancel during a running tool on every route class, including Pi and Oh
   My Pi queue-cancel-settle.
5. Blocked vendor requests, output floods, slow Store.

## Owner decisions surfaced

1. Sharing topology: socket-shared vendor server with independent VIA
   clients (Fable), a VIA connection-owner process (Astra), or per-run
   servers.
2. Run identity: new run per prompt, or turns within a run.
3. Mark invariant 5 and handoff property 1 "under revision" now.
4. (Still open from the routes review) whether VIA may wrap a server in an
   external sandbox for OpenCode.
