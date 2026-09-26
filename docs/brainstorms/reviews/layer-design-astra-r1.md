# Layer design review r1: GPT-6 Astra high

Run: `codex exec -s read-only -m gpt-6-astra -c model_reasoning_effort=high`, codex-cli 0.156.1, 89,146 tokens, 2026-09-25. Same brief for all three reviewers (`scratchpad/council/layer-design/brief.md`, local). Verbatim output; absolute local paths made repo-relative.

**SOUND WITH CHANGES** — the responsibility split can support VIA’s premise, but lifecycle, shared-connection ownership and recovery contracts need definition before C1/C2 can be considered stable.

Verified below means inspected documents, local schemas or current official documentation; predicted failures are architectural inferences. No files changed or agents run.

1. **[blocking] C5 promises protocol sessions that a protocol-blind Host cannot provide.**  
   `docs/brainstorms/system-layers.md:26` offers a “shared-server session”; `docs/brainstorms/system-layers.md:142` returns the server’s stdio. Several VIA processes cannot independently read that stream and reliably receive their own responses, notifications and server requests. Session demultiplexing requires L4 knowledge; putting it in L6 contradicts L6’s prohibition on speaking protocols.

   **Fix:** one owner of each vendor connection, hosting L4/L5 and routing session traffic. C5 supplies process identity, endpoints and exit observations—not vendor sessions. Specify how subsequent callers reach the owner through IPC. The daemon choice need not change the logical layers, but **does block finalizing their process placement, IPC and recovery guarantees**.

2. **[blocking] Lease expiry is not safe takeover.**  
   `docs/brainstorms/system-layers.md:261` permits takeover after expiry, but the stored lease has only owner and expiry. A paused owner can resume after its replacement starts, creating two writers or issuing stale cancellation. A replacement also cannot reconstruct a lost stdio connection merely from SQLite.

   **Fix:** atomic lease acquisition with increasing generations, command fencing at the connection owner, process identity beyond PID, and a recovery table distinguishing reconnectable owner, dead connection and unknown submission outcome. Preserve “outcome unknown” separately from ordinary failure; never replay ambiguous submissions. Require the caller handle for every session mutation, including resume and queue changes—not only cancel/steer. Define the handle as bearer authority and document its trust boundary.

3. **[major] Run, turn and session lifetimes are conflated.**  
   `docs/brainstorms/system-layers.md:226` makes a finished turn an idle run, while `docs/brainstorms/system-layers.md:357` permits multiple runs per session. It is unclear when `wait` returns, whether resume changes an existing envelope, or how a completed run becomes resumable.

   **Fix:** make each submitted prompt a run with an immutable terminal envelope; keep session availability and background activity separate. Define C1 queue behavior, C2 acceptance versus completion, strict resume identity, warm versus cold resume, and opaque adapter recovery metadata. Current OMP documentation distinguishes `prompt_result` from `session_settled`; bare `agent_end` is insufficient. Its session switching can also abort current work. [OMP RPC documentation](https://raw.githubusercontent.com/can1357/oh-my-pi/main/docs/rpc.md)

4. **[major] Cancellation has no completion or escalation contract.**  
   `docs/brainstorms/system-layers.md:112` maps cancel directly to interrupt, while `docs/brainstorms/system-layers.md:234` jumps directly to Cancelled. The local `scratchpad/codex-schema/v2/TurnInterruptResponse.json:1` provides no tool-cleanup guarantee. Pi requires queue clearing before abort because queued messages can otherwise continue. [Pi RPC commands](https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/docs/rpc-commands.md)

   **Fix:** L2 owns deadlines, cancellation intent and serialization against enqueue/steer; L3 implements the vendor sequence and declares its completion evidence; L6 performs authorized escalation. Distinguish requested, acknowledged, settled, forced and unknown outcomes. A single run’s timeout must not kill a shared server serving other owners. Configure automatic timeout authority at launch.

5. **[major] Receiving server requests is specified; answering them is not.**  
   `docs/brainstorms/system-layers.md:24` returns server requests, but C2 has no response operation and no layer owns the headless response policy. “Never ask” does not exhaust the problem: the local `scratchpad/codex-schema/ServerRequest.json:1896` includes user input and MCP elicitation. Cursor documents blocking question and plan requests. [Cursor ACP documentation](https://cursor.com/docs/cli/acp)

   **Fix:** L4 owns correlation and response deadlines; L3 supplies protocol-correct decline/cancel/error responses under an explicit unattended policy. Advertise no client filesystem, terminal or hosted-tool capabilities unless intentionally supported. Unknown requests must receive a response, not merely become unknown events. Supporting interactive answers later would require C1/C2 extensions unless included now. This is the earlier review’s unresolved request-handling issue, not a new permission-layer proposal.

6. **[major] Route selection needs a preflight contract before admission.**  
   `docs/brainstorms/system-layers.md:208` checks capabilities before start, but C2 does not define how L2 obtains the selected route’s effective configuration, resource requirements or session capacity. Those depend on version, authentication and existing server configuration.

   **Fix:** add a C2 prepare/describe operation returning effective capabilities, configuration compatibility, resource estimates and an opaque launch plan. Revalidate before dispatch; fallback only before submission. Reject vendor options that override canonical bounds; “partial” version support must never weaken a required permission guarantee.

   Copilot ACP settings can be fixed for every session at server launch. A strict single-server-per-vendor policy therefore needs queue/refusal behavior for incompatible configurations; “vendor + config” silently permits multiple servers. Pi/OMP need exclusive process capacity rather than assumed multiplexing. [Copilot ACP documentation](https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server)

7. **[major] Store placement is right; durability and flow-control contracts are incomplete.**  
   `docs/brainstorms/system-layers.md:292` requires verbatim capture before parsing, while `docs/brainstorms/system-layers.md:368` assigns a raw log per run. Shared-server bytes can interleave runs or lack turn attribution; L5 cannot partition them semantically. Synchronous logging can also stall every session when storage stalls.

   **Fix:** keep Store beside the layers, with separate record and append interfaces. Record connection-scoped raw streams with direction, connection generation and offsets; let normalized events reference them. Define per-run sequence assignment, replay deduplication, terminal-event rules and truncation recovery. Use short SQLite transactions with atomic queue/admission reservations, bounded lock waits and no transaction spanning vendor I/O. Specify bounded buffers, slow-subscriber behavior, disk-full handling and retention; exact unbounded capture cannot coexist with finite resources.

8. **[minor] C3 is a family of contracts, and C4 ownership overlaps.**  
   `docs/brainstorms/layers-and-names.md:65` already admits protocol-specific typed calls. That is sound composition, not a universally interchangeable route interface. L4 owns request pairing, yet C4 also offers requests with correlation IDs.

   **Fix:** retain all six responsibility areas; no additional layer is justified. Describe C3 as shared lifecycle plus protocol-specific interfaces. Put pairing in L4 and framing/transport in L5; clarify that C4 accepts messages for framing. Change “one per vendor” to “one per harness.” Reconcile stale daemon/SDK invariants and Role/Worker glossary entries with the owner’s current decisions, without reopening them.

**Full-harness test of the premise.** The `docs/brainstorms/access-methods.md:211`, interpreted through the newer route decision, yields:

| Harnesses | Architectural fit and limit |
|---|---|
| Claude Code, Codex, OpenCode | Fit after connection ownership, requests and lifecycle fixes; bounded OpenCode runs still require verified native enforcement or refusal. |
| Pi, Oh My Pi | Separate RPC dialects and exclusive process capacity; distinct queue/cancel/settlement semantics. |
| Cursor, Antigravity, Amp | CLI fits with declared missing verbs/data. Cursor SDK API-key authentication must not be assumed equivalent to CLI login. |
| Copilot | CLI/native ACP fit; direct runtime JSON-RPC remains unverified. Server configuration compatibility matters. |
| Gemini CLI, Qwen Code, Grok Build, Devin CLI, Kilo, Factory Droid, Cline, Hermes Agent, Goose | Native ACP fits structurally; resume, permissions, extensions and actual concurrency remain per-harness gates. |
| Muse Code | A protocol-specific L4 implementation fits; schema availability does not prove multiplexing. |
| Kimi CLI | Structurally CLI/ACP-compatible, but explicitly excluded as archived. |

**No harness demonstrates an unavoidable C1/C2 shape change** if unsupported operations are valid outcomes. Conversely, full R1 behavior on every harness is not established. Route independence preserves API shape, not capability equivalence, authentication, resume portability or failure guarantees.

**Prototype first:** two independent callers sharing one owned server; kill/pause its VIA owner around dispatch, race takeover against cancel, and inject faults only after tools demonstrably start. Add Pi/OMP queue-cancel-settlement cases, then blocked requests, slow logging and SQLite contention. The existing benchmark explicitly leaves kill-during-tool cleanup untested (`scratchpad/headless-bench/analysis.md:85`).

Add these contract definitions and conformance cases; cut universal-session assumptions and any claim of transparent route equivalence. Unverified remain live recovery, descendant cleanup, Copilot direct RPC, private vendor-database isolation and permission enforcement across the full matrix.