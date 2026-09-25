# A2A research for VIA

**Checked 2026-09-24.** This report uses the current A2A project specification, governance and release pages, vendor documentation, and project repositories. “Shipped” means there is a released product, package, or usable repository implementation; it does not by itself establish production deployment by customers. **UNVERIFIED** marks claims I could not confirm in the sources checked.

## 1. What A2A is

A2A (Agent2Agent) is an open protocol for agents to discover one another, exchange messages, manage tasks, and return outputs while keeping each agent’s internal tools and implementation opaque to its peers. Google announced it on **2025-04-09**; the Linux Foundation launched the project under its governance on **2025-06-23**. [Google announcement, 2025-04-09](https://developers.googleblog.com/a2a-a-new-era-of-agent-interoperability/), [Linux Foundation launch, 2025-06-23](https://www.linuxfoundation.org/press/linux-foundation-launches-the-agent2agent-protocol-project-to-enable-secure-intelligent-communication-between-ai-agents)

**Owner and maturity.** A2A is hosted by the Linux Foundation and technically overseen by a Technical Steering Committee (TSC). The governance page lists eight represented companies: Google, Microsoft, Cisco, AWS, Salesforce, ServiceNow, SAP, and IBM. The current tagged specification release I found is **v1.0.1**, dated **2026-05-26**; the wire protocol version is **1.0**. Treat v1.0 as the current stable protocol line, with v1.0.1 as a later specification release. This is project maturity, not evidence of external ISO or IETF ratification. [A2A governance](https://github.com/a2aproject/A2A/blob/main/GOVERNANCE.md), [A2A releases](https://github.com/a2aproject/A2A/releases), [versioning rules in the spec](https://a2a-protocol.org/latest/specification/)

**Change history.** The project progressed from v0.2 through v0.3 to v1.0. The Linux Foundation took over project hosting in June 2025; v1.0 was released on **2026-03-12**, followed by v1.0.1 on **2026-05-26**. v1.0’s documented migration from v0.3 includes breaking changes to task-state and role enums, parts’ type representation, HTTP paths, and error formats. Compatibility between versions is therefore a real adapter concern, not just a version-string difference. [A2A release history](https://github.com/a2aproject/A2A/releases), [v0.3-to-v1.0 changes](https://a2a-protocol.org/latest/whats-new-v1/)

### Core concepts and mechanics

| Concept | What it means in A2A |
|---|---|
| **Agent Card and discovery** | An Agent Card advertises an agent’s identity, skills, supported interfaces, capabilities, and authentication requirements. The v1 well-known URL is `/.well-known/agent-card.json`. Clients can also use a registry/catalog or a directly configured card. A public card can be accompanied by a more detailed authenticated card. [Discovery and cards](https://a2a-protocol.org/latest/specification/) |
| **Task and lifecycle** | A task has an `id`, usually a `contextId`, status, messages, and possibly artifacts. v1 states are `SUBMITTED`, `WORKING`, `INPUT_REQUIRED`, `AUTH_REQUIRED`, `COMPLETED`, `FAILED`, `CANCELED`, and `REJECTED` (serialized as `TASK_STATE_*`). The last four are terminal; input and authentication required are interruptions. [Task states and model](https://a2a-protocol.org/latest/specification/) |
| **Messages and parts** | Messages carry user or agent communication and contain one or more parts. Parts can represent text, files, or structured data; they support multimodal exchange. The spec distinguishes communication messages from task outputs. [Messages and artifacts](https://a2a-protocol.org/latest/specification/) |
| **Artifacts and final result** | Artifacts are task outputs, potentially with multiple named parts and metadata. A successful final result is represented by a task in `COMPLETED` state with its output artifact(s); clients can retrieve the task to read them. Messages may convey status or ask for input, but the spec says task outputs should be returned as artifacts. [Messages and artifacts](https://a2a-protocol.org/latest/specification/) |
| **Streaming** | A client may submit with streaming enabled and receive task creation, status changes, and artifact updates over a Server-Sent Events (SSE) stream. It may also subscribe to updates for an ongoing task. [Streaming operations and examples](https://a2a-protocol.org/latest/specification/) |
| **Push notifications** | If supported by the agent, the client can register a webhook for task updates. This requires a reachable callback and appropriate authentication; it is optional capability, not something every agent must offer. [Push notification operations](https://a2a-protocol.org/latest/specification/) |
| **Multi-turn and input required** | An agent can pause a task in `INPUT_REQUIRED`; the client continues by sending another message with the same `taskId` and `contextId`. This supports an explicit wait-for-input interaction. [Multi-turn task semantics](https://a2a-protocol.org/latest/specification/) |
| **Cancel** | A client can request cancellation by task ID. The server attempts to cancel and returns the updated state, but cancellation is not guaranteed. [Cancel Task](https://a2a-protocol.org/latest/specification/) |
| **Authentication** | Agent Cards can declare API-key, HTTP authentication, OAuth 2.0, OpenID Connect, and mutual-TLS schemes. Credentials are acquired out of band and sent on requests. The protocol describes authentication metadata and flows; the service still has to implement authentication, authorization, identity verification, and safe task scoping. [Security objects and authentication](https://a2a-protocol.org/latest/specification/) |
| **Bindings / transports** | The current specification defines JSON-RPC 2.0 over HTTP(S), HTTP+JSON/REST, and gRPC. Streaming is delivered as SSE for HTTP bindings; gRPC has its own streaming binding. The spec requires equivalent behavior when an agent advertises multiple interfaces. [Protocol bindings](https://a2a-protocol.org/latest/specification/) |

The current roadmap also lists **bidirectional streaming** as future work. The existing SSE stream is principally server-to-client updates; do not assume it gives VIA a standard way to inject an instruction into an agent while it is actively generating or executing. [A2A roadmap, last updated 2026-09-15](https://a2a-protocol.org/latest/roadmap/)

## 2. Relationship to MCP and ACP

**MCP and A2A address different boundaries.** MCP standardizes how an agent connects to tools and data. A2A standardizes how agents communicate with other agents. A VIA harness adapter could use MCP for tools available to its local agent and A2A for remote agent services; neither replaces the other. [A2A’s MCP comparison](https://a2a-protocol.org/latest/topics/a2a-and-mcp/), [MCP overview](https://modelcontextprotocol.io/introduction)

**Zed’s Agent Client Protocol is a different ACP.** Zed’s ACP connects coding agents to editors; A2A connects agents to agents. Zed’s docs describe ACP external agents as running their own process and owning their own runtime, auth, and configuration. That is enough to distinguish its purpose here; a full Zed ACP comparison is outside this report’s scope. [Zed Agent Client Protocol](https://zed.dev/acp), [Zed external agents](https://zed.dev/docs/ai/external-agents)

**IBM/BeeAI’s Agent Communication Protocol was merged into A2A.** IBM announced the merger on **2025-08-25**; the ACP repository was archived on **2025-08-27**. BeeAI moved to an A2A server adapter for exposing agents. This is not the same protocol as Zed’s ACP. [IBM/BeeAI merger announcement](https://github.com/orgs/i-am-bee/discussions/5), [archived ACP repository](https://github.com/i-am-bee/acp), [BeeAI framework updates](https://github.com/i-am-bee/beeai-framework)

## 3. Implementations, including coding agents

### Official A2A SDKs

The A2A project lists SDKs for **Python, Go, Java, JavaScript/TypeScript, C#/.NET, and Rust**. SDK package version numbers are independent of protocol version numbers, so a package labeled `2.x` can implement the v1.0 protocol. Versions below are the latest release labels I could confirm on the official repositories by the report date; .NET and Rust need qualification. [Official SDK list](https://a2a-protocol.org/latest/sdk/)

| Language | Release seen | Evidence |
|---|---:|---|
| Python | `1.1.5` | Official [Python SDK releases](https://github.com/a2aproject/a2a-python/releases), checked 2026-09-24 |
| Go | `2.5.0` | Official [Go SDK releases](https://github.com/a2aproject/a2a-go/releases), dated 2026-08-18 |
| Java | `1.3.2.Final` | Official [Java SDK releases](https://github.com/a2aproject/a2a-java/releases), dated 2026-09-08 |
| JavaScript / TypeScript | `1.2.0` | Official [JS SDK releases](https://github.com/a2aproject/a2a-js/releases), dated 2026-09-18 |
| C# / .NET | `1.0.0-preview2` | A prerelease is visible in the [official .NET release history](https://github.com/a2aproject/a2a-dotnet/releases); I found no confirmed stable v1 SDK release |
| Rust | Version **UNVERIFIED** | The [official Rust repository](https://github.com/a2aproject/a2a-rs) contains separate crates and recent component releases; I could not confirm one unified SDK version or its exact v1.0 compatibility level |

There is also an official A2A CLI. Its skill can teach coding harnesses including Claude Code, Cursor, and Codex to invoke the CLI as an A2A client. That is an **external client utility**, not proof those harnesses natively implement A2A. The CLI has released binaries and its own command specification, but its `serve` mode or use by a harness should not be confused with that harness exposing an A2A server. [A2A CLI repository and skill](https://github.com/a2aproject/a2a-cli), [CLI releases](https://github.com/a2aproject/a2a-cli/releases)

### Coding harness evidence

“Native” below means first-party harness support documented by its maintainers. A community wrapper or installed CLI/skill can still be useful, but it is a separate service or client process.

| Harness | A2A status found | Evidence and limits |
|---|---|---|
| **Gemini CLI** | **First-party client support shipped**; first-party server package exists but is labeled experimental. | The CLI docs describe connecting to remote A2A subagents. Google’s repository contains a Gemini CLI A2A server package and explicitly labels its code experimental and under active development. [Remote subagents](https://github.com/google-gemini/gemini-cli/blob/main/docs/core/remote-agents.md), [server package README](https://github.com/google-gemini/gemini-cli/tree/main/packages/a2a-server) |
| **Claude Code** | No native A2A support confirmed. Can use the official A2A CLI skill as a client; community server wrappers exist. | The [official A2A CLI skill](https://github.com/a2aproject/a2a-cli) is harness instructions for driving a separate binary. Community projects include [Claude Code SDK wrapper](https://github.com/ericabouaf/claude-a2a), explicitly not production-ready, and [Claude Code CLI wrapper](https://github.com/jcwatson11/claude-a2a). **Production use UNVERIFIED.** |
| **Codex CLI** | No native A2A support confirmed. Can use the official A2A CLI skill as a client; community wrapper exists. | The [A2A CLI skill](https://github.com/a2aproject/a2a-cli) documents Codex as a target. [coding-agent-a2a](https://github.com/casabre/coding-agent-a2a) advertises a Codex adapter. **Independent production deployment UNVERIFIED.** |
| **Cursor** | No native A2A support confirmed. Can use the official A2A CLI skill as a client; community wrapper exists. | The [A2A CLI skill](https://github.com/a2aproject/a2a-cli) documents Cursor as a target. [coding-agent-a2a](https://github.com/casabre/coding-agent-a2a) advertises a Cursor adapter. **Independent production deployment UNVERIFIED.** |
| **OpenCode** | No native A2A support confirmed; community wrapper exists. | [coding-agent-a2a](https://github.com/casabre/coding-agent-a2a) advertises an OpenCode adapter and an A2A server surface. **Independent production deployment UNVERIFIED.** |
| **Copilot CLI** | Native A2A client or server support **UNVERIFIED**. | The A2A project’s [CLI proposal](https://github.com/a2aproject/A2A/issues/1929) names Copilot CLI as a desired skill target; that is proposal context, not evidence of shipped harness integration. |
| **Devin, Jules, Goose, Amp** | Native A2A client/server support **UNVERIFIED**. | I could not confirm first-party A2A integration documentation or a production A2A endpoint for these harnesses in the primary sources checked. |
| **Other coding harnesses** | Community wrappers exist, but evidence is repo-level rather than production-level. | [coding-agent-a2a](https://github.com/casabre/coding-agent-a2a) also lists Claude Code and Vibe adapters. Its README documents a runnable Docker setup, protocol endpoints, and task tools; this proves an implementation is available, not that it is production-deployed. |

### Implementations beyond coding harnesses

The ecosystem has moved beyond announcement-only status: Microsoft documents A2A v1.0 as **generally available** for Foundry hosted agents; AWS documents A2A support in Bedrock AgentCore Runtime; Google documents deployable Agent Engine and ADK paths. Those are concrete service implementations, but they do not prove broad, independently verified customer production usage, particularly for local coding harnesses. [Microsoft hosted-agent protocols](https://learn.microsoft.com/azure/foundry/agents/concepts/hosted-agents), [Microsoft A2A tool status](https://learn.microsoft.com/en-ca/azure/foundry/agents/how-to/tools/agent-to-agent), [AWS A2A protocol contract](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-a2a-protocol-contract.html), [Google Agent Engine deployment](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/agent-engine/deploy)

**Production-use assessment:** I found evidence of released SDKs and CLIs, GA/runtime support from major vendors, and deployable examples. I did **not** find independently verifiable evidence establishing widespread production use of A2A by coding harnesses such as Claude Code, Codex, Cursor, or OpenCode. Treat vendor claims and tutorials as evidence that products support A2A, not as proof of customer adoption.

## 4. Fit for VIA’s verbs

| VIA verb | A2A mapping | What VIA still needs to decide or implement |
|---|---|---|
| **spawn** | `SendMessage` can start a task; `returnImmediately` can return before it completes. The server provides a task ID. | Map a VIA role/model/workspace request into the agent’s advertised skill and accepted input. A task ID does not provide process isolation or local harness configuration. [Send Message](https://a2a-protocol.org/latest/specification/) |
| **resume** | `GetTask` and `SubscribeToTask` let the caller resume observing a task. An `INPUT_REQUIRED` task can receive a follow-up message using the same task and context IDs. | A2A does not define how to resume an underlying Claude/Codex/etc. session after server restart, or promise the server retains that harness session. VIA would need durable mapping from A2A task/context IDs to adapter session IDs. [Task and multi-turn semantics](https://a2a-protocol.org/latest/specification/) |
| **steer** | A follow-up message can continue an `INPUT_REQUIRED` task. A server may define additional message handling. | General **mid-turn steering is not guaranteed** by the core protocol. Current server-to-client streaming does not provide the missing bidirectional control path; roadmap work on bidirectional streaming and handling messages during working states reinforces that this area is unsettled. [Task semantics](https://a2a-protocol.org/latest/specification/), [A2A roadmap](https://a2a-protocol.org/latest/roadmap/) |
| **cancel** | `CancelTask` is a direct mapping to a cancellation request. | Cancellation is best-effort and depends on the agent being able to stop its execution. VIA must expose actual adapter capability and outcome rather than promise a hard kill. [Cancel Task](https://a2a-protocol.org/latest/specification/) |
| **status** | `GetTask`, `ListTasks`, and `SubscribeToTask` provide status and updates. | Decide whether VIA’s local run ledger is authoritative or whether an A2A server is authoritative; reconcile states and failures across both. [Task operations](https://a2a-protocol.org/latest/specification/) |
| **result** | A completed task carries output artifact(s), retrievable with the task. | Convert A2A artifact parts into VIA’s result envelope, including exit status, session ID, usage/cost, logs, and input/output tree pins if VIA promises those fields. [Artifacts](https://a2a-protocol.org/latest/specification/) |

**What A2A gives VIA:** task/context identifiers, a task lifecycle, typed messages and parts, artifacts, streaming updates, optional webhooks, discovery metadata, version negotiation, and a standardized cancellation request.

**What it does not standardize for a local coding sub-agent:** worktree/workspace creation, filesystem or network sandboxing, permission mode, model/effort selection, usage and cost accounting, stable resume of a harness’s native session, process exit code, logs, or a Git diff. Artifacts can carry files or data, but that is not a standard diff contract or isolation policy. These would remain VIA/runtime responsibilities or require explicitly versioned extensions.

## 5. Pros and cons for VIA

| Approach | Advantages | Costs and risks |
|---|---|---|
| **VIA exposes roles as A2A agents (external interface)** | Makes VIA roles discoverable and callable by other A2A clients; task IDs, artifacts, async updates, and cancel requests come from a shared protocol. Could be useful when an orchestrator or remote service needs to call VIA across a process or network boundary. | Requires an HTTP/gRPC service lifecycle, task persistence, auth, authorization, and a safe mapping from network requests to local workspaces and harness processes. A2A does not supply the VIA-specific execution controls or result envelope. Exposing a local coding workspace through a network endpoint increases the security surface. |
| **A2A as VIA’s internal adapter protocol** | One uniform message/task shape might seem to simplify adapters and permit reuse of SDK clients. | Poor match for VIA’s local process-control boundary: direct CLI invocation is simpler than starting an A2A server for each local harness. A2A task state is not the same as harness session state; mid-turn steering is unresolved; workspace, model, effort, sandbox, usage, exit code, and diffs remain custom. It risks adding HTTP, task stores, protocol versions, and adapter indirection without eliminating the per-harness launch/resume work. |
| **Do not use A2A in the core VIA path** | Keeps VIA focused on its stated purpose: direct harness adapters, a durable local run record, isolation, and a structured result envelope. Matches the CLI-first, one-turn-per-process design in the VIA handoff. | VIA does not get generic agent discovery/interoperability for free. If users later need networked delegation, VIA would have to add an A2A façade or another external interface. |

The protocol is not static: v1.0 introduced breaking model and binding changes; the project’s roadmap continues to list enhancements. Major-v1 reduces wire-format churn risk compared with the v0.x period, but SDK versions and extensions still move independently. [v1.0 migration notes](https://a2a-protocol.org/latest/whats-new-v1/), [A2A releases](https://github.com/a2aproject/A2A/releases), [A2A roadmap](https://a2a-protocol.org/latest/roadmap/)

For a local-only CLI, a local HTTP server per role would be operational overhead. **Inference:** if the service binds beyond loopback or accepts untrusted requests, it also makes the workspace and tools reachable through a network-facing control plane; A2A authentication metadata alone does not provide a sandbox or validate whether a discovered agent should be trusted. The spec calls for authentication and authorization implementation, secure card retrieval, and care with webhook URLs. [A2A security considerations](https://a2a-protocol.org/latest/specification/)

## 6. Recommendation

**Do not use A2A as VIA’s internal adapter protocol for v0.** Keep VIA’s core path as direct, headless harness adapters with VIA-owned task records, isolation, capability declarations, and result normalization. A2A does not remove the harness-specific work that dominates VIA’s cost: launch flags, session mapping, steering semantics, sandboxing, and result extraction.

**Keep an A2A external façade as a later option, not a v0 commitment.** The case for that changes if VIA needs remote agents or other orchestrators to discover and invoke VIA roles. Then VIA could expose selected roles as A2A agents while keeping its internal execution direct; task/artifact mapping would be an adapter at the boundary. The case is supported by current v1 stability, official SDKs, released A2A CLI tooling, and vendor support including Microsoft’s GA endpoint. It is limited by thin verified adoption among coding harnesses, the lack of native support across most harnesses checked, and unresolved mid-turn control semantics.

That recommendation is consistent with the handoff’s separation between **direct local harness execution** and the possibility of a future cross-system control plane. It should be revisited when VIA has a concrete networked caller or when coding harnesses standardize on a protocol that exposes the local execution controls VIA requires.

### Primary sources consulted

- [A2A protocol specification, current v1 line](https://a2a-protocol.org/latest/specification/) — version and behavior checked 2026-09-24.
- [A2A v1.0 migration guide](https://a2a-protocol.org/latest/whats-new-v1/) — changes from v0.3 to v1.0.
- [A2A governance](https://github.com/a2aproject/A2A/blob/main/GOVERNANCE.md) and [release history](https://github.com/a2aproject/A2A/releases) — TSC and tags checked 2026-09-24.
- [Official SDK index](https://a2a-protocol.org/latest/sdk/) and linked SDK repositories — release labels checked 2026-09-24.
- [Google A2A announcement](https://developers.googleblog.com/a2a-a-new-era-of-agent-interoperability/) — 2025-04-09.
- [Linux Foundation A2A project launch](https://www.linuxfoundation.org/press/linux-foundation-launches-the-agent2agent-protocol-project-to-enable-secure-intelligent-communication-between-ai-agents) — 2025-06-23.
- [IBM/BeeAI ACP merger announcement](https://github.com/orgs/i-am-bee/discussions/5) — 2025-08-25; [archived ACP repository](https://github.com/i-am-bee/acp) — archived 2025-08-27.
- [Gemini CLI remote subagent documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/core/remote-agents.md) and [A2A server package](https://github.com/google-gemini/gemini-cli/tree/main/packages/a2a-server) — checked 2026-09-24.
- [Official A2A CLI](https://github.com/a2aproject/a2a-cli) — CLI and harness skill checked 2026-09-24.
- [Microsoft Foundry A2A docs](https://learn.microsoft.com/en-ca/azure/foundry/agents/how-to/tools/agent-to-agent), [AWS AgentCore contract](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-a2a-protocol-contract.html), [Google Agent Engine docs](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/agent-engine/deploy) — checked 2026-09-24.
- [Community coding CLI wrapper](https://github.com/casabre/coding-agent-a2a), [Claude Code SDK wrapper](https://github.com/ericabouaf/claude-a2a), [Claude Code CLI wrapper](https://github.com/jcwatson11/claude-a2a) — repository claims and setup checked 2026-09-24.