# VIA — Unified Coding-Agent CLI

**Architecture, Go vs Rust, and implementation stacks**

**Date:** 25 September 2026

Go versus Rust, agent integration options, durable session management, and complete implementation stacks for both languages.

> Recommendation: Rust + Tokio + SQLite, by a narrow margin. Choose Go when implementation speed and maintenance simplicity carry more weight than minimizing VIA’s own overhead.

### What you are building

VIA is a supervisor and common interface for actual coding-agent processes: Claude Code, Codex, OpenCode, GitHub Copilot, Cursor, Grok Build, and future adapters. It is not a model gateway. Each underlying agent keeps its own authentication, configuration, tools, context, and execution loop.

### The decision in one view

| Dimension | Assessment |
| --- | --- |
| Concurrency | Both languages are suitable for dozens of long-lived, predominantly I/O-bound sessions. This is an engineering expectation, not a measured capacity guarantee. |
| Rust’s advantage | Explicit memory ownership, compile-time concurrency safeguards, and an upstream ACP library. [1, 12, 13] |
| Go’s advantage | A more direct implementation model, useful standard-library tooling, and a convenient CGo-free distribution path. [10, 15, 16] |
| Weighted scores | Go: 8.4/10. Rust: 8.6/10. Subjective decision scores, not performance benchmarks. |
| Largest risks | Unbounded output buffers, incomplete process-tree cleanup, blocked approvals, and overstated recovery guarantees. |

### Document basis and status

This document consolidates the preceding analysis and expands the Go stack to the same implementation depth as Rust. Product capabilities and package choices were checked against the primary references listed at the end. Recommendations, proposed commands, and design examples are distinguished from documented agent behavior. No VIA implementation, load test, or agent compatibility test was run.

Reading guide: Sections 1–6 define the problem and language decision; 7–9 describe the shared architecture; 10–13 specify both stacks; 14–15 cover delivery and validation. The reference register provides clickable source links.

## 1. Scope, boundaries, and terminology

### The non-negotiable boundary

```text
Caller / terminal / application
          |
         VIA
          |
Actual coding-agent binary or agent server
          |
Agent-owned authentication, configuration,
tools, context, and model access
```

VIA should own process or connection lifecycle, session routing, protocol translation, event capture, approval delivery, and its ledger. It should not silently replace an agent with an implementation built on a generic model API. Native configuration fidelity must be checked for each integration mode; selecting the same executable does not guarantee identical defaults.

### Two modes, one executable

| Mode | Behavior | Recommended contract |
| --- | --- | --- |
| Pass-through | Launch the agent with native terminal input and output. | Preserve arguments and exit status. Do not promise a normalized transcript or rich session controls. |
| Managed session | Own structured streams or a server connection. | Expose common commands, approvals, lifecycle state, and a durable event history. |

One VIA executable per operating system and architecture is the distribution goal. It does not bundle every agent by default. Agent binaries, their own requirements, and any optional Node.js/Python bridges remain separate dependencies. Copilot’s Go and Rust SDKs do not include the CLI by default. [7]

### Terms used in this document

| Term | Meaning |
| --- | --- |
| ACP | Agent Client Protocol: an agent/client interaction protocol. VIA acts as the client of an ACP-capable agent. [1, 2] |
| JSON-RPC / JSONL | A request/response message convention / newline-delimited JSON framing. They are not interchangeable concepts. |
| MCP | Model Context Protocol: a separate integration surface for capabilities such as tools and resources. Agent-side MCP support alone does not expose control of the agent. [9] |
| Daemon / IPC | A long-running local session owner / inter-process communication between that owner and VIA clients. |
| WAL / RSS / PTY | Write-ahead log / resident process memory / pseudo-terminal. A PTY is for terminal interaction, not a substitute for structured agent events. |

## 2. Communication strategy and agent map

Your four integration choices overlap. A CLI can expose a JSON-RPC server; an SDK can launch that CLI and speak its protocol; ACP can run over stdio. Separate the transport from the capability contract instead of creating four mutually exclusive adapter categories. Copilot’s SDK is a documented example of an SDK talking to the actual CLI server. [7]

> Selection rule: use a supported native structured interface that preserves the required behavior. Prefer ACP for shared functionality; use a native protocol or SDK when it offers necessary controls. Use terminal automation only as an explicit fallback.

| Agent | Documented surface | Preferred VIA path |
| --- | --- | --- |
| Claude Code | claude -p with JSON/streaming JSON; Python and TypeScript Agent SDKs; separate ACP bridge. [3, 4] | Native structured CLI first; add the bridge only when needed and disclose its dependency and behavioral differences. |
| Codex | codex app-server: bidirectional thread/turn messages, events, and approvals. [5] | Native stdio App Server adapter, with version pinning and the maturity caveat in Section 3. |
| OpenCode | opencode acp; HTTP server with OpenAPI and server-sent events. [6] | ACP for the common surface; HTTP/SSE for OpenCode-specific capabilities. |
| GitHub Copilot | Official Go and Rust SDKs; copilot --acp --stdio. [7, 8] | ACP for reuse, or the language SDK for deeper control. Do not implement both initially without a need. |
| Cursor | agent acp with standard ACP and Cursor extensions. [14] | Native ACP plus required extension handlers. Resolve the executable explicitly. |
| Grok Build | grok agent stdio; headless mode with structured output. [17] | Native ACP. Keep headless one-shot mode as a capability-limited option. |

### What “common interface” should mean

Normalize starting a session, sending input, observing events, answering supported permission requests, interrupting a turn, stopping, and resuming when available. Preserve agent-specific extensions. A common API should expose unsupported operations clearly rather than pretending every agent has identical semantics.

For each adapter, record the executable path, agent version, protocol version, selected mode, and capability snapshot. Treat the table above as a documented integration map, not a claim that every installed version has been tested with VIA.

## 3. Agent-specific caveats

### Claude Code: headless mode is not a neutral switch

The documented CLI supports JSON and streaming JSON, but full SDK callbacks are a separate integration surface. The ACP bridge is a separate project, not an Anthropic-native ACP flag. Test its runtime requirements, configuration loading, and credential behavior before offering it as equivalent to direct CLI use. [3, 4]

Do not automatically add --bare. Current documentation says it skips normal configuration discovery and does not read subscription OAuth credentials or the system keychain. Conversely, ordinary -p mode can load repository hooks and MCP configuration without the interactive workspace-trust dialog. VIA needs an explicit workspace-trust policy and an explicit authentication/configuration mode. [3]

### Codex: preserve the documented support caveat

The current App Server page labels the app-server command and WebSocket transport experimental and unsupported for production workloads; it also documents a stable API surface versus opt-in experimental methods. Do not interpret that API distinction as removing the broader caveat. Prototype over stdio, pin a tested CLI version, and make support status visible. [5]

Codex omits the jsonrpc version member on the wire. Its CLI can generate schemas for the installed version. Keep a provider-specific codec rather than assuming an unmodified generic JSON-RPC implementation will work. [5]

### Copilot: SDK status and ACP status are different

The SDK repository states that the SDK is generally available. Copilot ACP documentation still labels ACP public preview. SDKs communicate with the actual CLI server; Go and Rust can use a separately installed executable. These are separate contracts with separate upgrade risks. [7, 8]

### Cursor: requests can block progress

Cursor documents blocking cursor/ask_question and cursor/create_plan extension methods. Your client must answer them; merely collecting text updates can leave the session waiting indefinitely. Unknown blocking methods need a visible, protocol-appropriate failure or user interaction path. [14]

### OpenCode and Grok: preserve the chosen mode

OpenCode provides both ACP and server APIs, so keep their adapters separate behind one capability interface. Grok documents headless and persistent ACP modes; do not equate a saved headless session ID with an uninterrupted running process. [6, 17]

> Compatibility policy: advertise only tested capabilities for a recorded version. Preserve permissions and configuration intentionally. Never auto-enable unrestricted approval flags to avoid implementing approval handling.

## 4. Go versus Rust: concurrency and resources

### Concurrency: effectively a tie at your scale

Go multiplexes goroutines over operating-system threads. Tokio schedules lightweight asynchronous tasks. Neither model inherently requires a dedicated operating-system thread for every waiting session. Blocking operations and subprocess APIs still need careful handling. [10, 11]

For a few dozen sessions that mostly wait for events, I expect both to be adequate. Give each session an owner and a small set of long-lived readers/writers. Hours of elapsed time are not computational work; the important variables are message volume, allocations, backlog, and cleanup. This assessment is not a benchmark.

### Memory: Rust offers stronger control, not an automatic RSS guarantee

Rust’s ownership model does not require a tracing collector. Go’s collector introduces a tunable memory/CPU trade-off. Rust therefore offers more explicit lifetime control, but allocator behavior, dependency choices, and retained data still determine actual resident memory. [12, 18]

```text
Total memory = VIA + all agent-process trees + OS/cache overhead

VIA memory ~= runtime baseline + active metadata
           + bounded queues + parsers + database caches
```

| Illustrative policy | Arithmetic consequence |
| --- | --- |
| 40 sessions × 2 MiB of pending output | 80 MiB in those buffers alone. |
| 40 sessions × 50 MiB of retained transcript | 2,000 MiB in transcript storage alone. |

These are hypothetical budgets, not measured Go or Rust usage. Store full transcripts durably and read them in pages. Agent-process trees may dominate total memory, but measure them separately rather than assuming this will always be true.

Go’s GOMEMLIMIT is a soft bound on runtime-managed memory, not a hard process-RSS cap and not a limit on child agents. Tightening it too far can increase garbage-collection work. [18]

### CPU: a modest potential Rust advantage

VIA decodes messages, routes control traffic, allocates buffers, writes records, and renders output. Its language does not directly accelerate a separate agent’s model inference, compiler, or test runner. Avoid reparsing JSON, unnecessary copies, frequent tiny transactions, and busy polling before attempting low-level scheduler tuning.

> Verdict: concurrency is not the deciding factor. Rust has the stronger memory-control story and lower-GC-overhead potential; the actual whole-system benefit must be measured.

## 5. Go versus Rust: correctness and distribution

### Long-running correctness

Safe Rust rules out important ownership and data-race errors at compile time. It does not eliminate deadlocks, logical races, incomplete state machines, or errors in unsafe/foreign code. Go’s race detector checks executed paths at runtime; it is not a compile-time proof. [13, 19]

| Failure assumption | Go | Rust / Tokio |
| --- | --- | --- |
| Cancellation kills everything | CommandContext normally targets the direct child; it is not complete process-tree supervision. [20] | Dropping Child normally leaves it running. kill_on_drop is not complete tree supervision. [21] |
| Process exit means cleanup is done | Wait, pipe draining, handle closure, and goroutine shutdown still matter. [20] | Explicit wait/reaping, stream draining, and task shutdown still matter. [21] |

Rust is attractive for explicit state transitions and ownership. Neither language implements a correct cross-platform supervisor simply by exposing a spawn function. Operating-system lifecycle correctness remains application work.

### Single executable does not mean identical linkage everywhere

| Distribution requirement | Go | Rust |
| --- | --- | --- |
| No separate language interpreter | Yes; the application includes its runtime. [18] | Yes; native executable. [22] |
| One VIA executable per target | A CGo-free dependency graph is convenient. [16] | Possible with target-specific builds and dependency choices. [22] |
| Fully static on every OS | Not a universal promise. Inspect target artifacts. | Not a universal promise. Linux musl and native dependencies need explicit handling. [22] |
| Embedded SQLite | modernc.org/sqlite avoids CGo. [23] | rusqlite bundled compiles SQLite into the application. [24] |

Build-time dependencies are different from end-user dependencies. Rust’s bundled SQLite needs a suitable C build toolchain during compilation; users do not need a SQLite installation. Go’s pure-Go SQLite approach simplifies the CGo-free build path, but must still be benchmarked on the actual queries. [23, 24]

### Development, maintenance, and diagnostics

My assessment: Go is easier to keep direct and approachable for a small team; Rust adds ownership and async-design complexity in exchange for stronger constraints. Go provides CPU/heap profiles and execution tracing. Rust’s upstream ACP library is an ecosystem advantage; Go’s listed ACP libraries are community-managed. [1, 2, 15]

## 6. Weighted scores, pros, and cons

The rubric below preserves the preceding recommendation. Scores are subjective engineering judgments under the assumption that either language can be maintained competently. They do not represent measured performance, reliability probabilities, or universal language rankings.

| Criterion | Weight | Go / 10 | Rust / 10 |
| --- | --- | --- | --- |
| Long-lived I/O concurrency | 20% | 9 | 9 |
| Memory footprint and control | 15% | 7 | 9 |
| CPU-overhead potential | 10% | 8 | 9 |
| In-process lifecycle and state safety | 15% | 8 | 9 |
| Single-binary, cross-platform packaging | 10% | 9 | 8 |
| ACP and native integration fit | 10% | 8 | 9 |
| Implementation and maintenance simplicity | 15% | 9 | 7 |
| Diagnostics and iteration | 5% | 9 | 8 |
| Weighted total, rounded | 100% | 8.4 | 8.6 |

Method: sum(weight × score). The unrounded totals are 8.35 for Go and 8.55 for Rust. The 0.2-point difference is intentionally small and should not be read as a performance advantage.

### Go: strengths and costs

Strengths: direct concurrency model, useful standard-library building blocks, a convenient CGo-free release path, and lower implementation complexity in my assessment. Costs: garbage-collection trade-offs, runtime rather than compile-time race detection, and a community ACP dependency that needs its own compatibility review. [2, 10, 15, 16, 18, 19]

### Rust: strengths and costs

Strengths: explicit resource ownership, strong concurrency constraints, no tracing-GC requirement, and upstream ACP support. Costs: greater async/ownership design complexity, more involved native cross-compilation, and an ongoing need for explicit process cleanup despite those language guarantees. [1, 12, 13, 21, 22]

### When I would select each

Choose Rust for a long-lived infrastructure tool whose own overhead, resource ownership, and extensibility are central priorities. Choose Go when faster implementation and straightforward maintenance outweigh the narrow resource-control advantage, especially with substantially stronger Go experience.

> Decision: Rust is my default for your stated priorities. Go is a fully credible production choice. Reweighting the rubric toward delivery speed can reasonably reverse the result.

## 7. Shared architecture

```text
via CLI / application client / optional MCP frontend
                         |
                  local authenticated IPC
                         |
                     via daemon
                         |
              session lifecycle + policy
                 /                 \
        protocol adapters        SQLite ledger
                |
       actual agent processes
```

Recommendation: ship one executable with a foreground client mode and a daemon mode. The daemon owns managed sessions, so a terminal can detach without killing them. Start with one daemon rather than one VIA worker per session; introduce workers only when their isolation benefit justifies added overhead and recovery complexity.

### Three different promises

| Promise | Meaning |
| --- | --- |
| Detach and reattach | The terminal disconnects; the daemon and agent are still alive. |
| Resume conversation | A compatible agent process reloads a saved provider session or thread. |
| Recover live execution | In-flight work, approvals, streams, and effects continue correctly after failure. This requires extra provider support and must not be assumed. |

### Keep identities separate

| Identity | Recommended purpose |
| --- | --- |
| via_session_id | Logical conversation or managed session in VIA. |
| provider_session_id | Agent-owned conversation/thread identifier. |
| run_id | One launch or execution attempt within that session. |
| process_identity | PID plus available start/ownership metadata; never rely on PID alone during recovery. |

Codex and OpenCode expose session/thread-oriented server APIs. Sharing one compatible agent server across conversations may reduce repeated process overhead, but changes the failure domain and isolation model. Make that an adapter capability, not a global optimization. Keep authentication, workspace, and configuration boundaries explicit. [5, 6]

### Normalized events without losing provider meaning

Use a versioned envelope containing session ID, run ID, local sequence, timestamp, event kind, and provider payload or a durable reference. Preserve unknown extensions; unsupported operations are explicit errors. Missing usage is unknown, not zero. Only usage with compatible units and scopes should be aggregated.

## 8. Lifecycle and streaming guardrails

### One owner, independent readers, serialized writes

Assign session state to one owner. Read structured stdout and drain stderr independently. Correlate responses by request ID, route incoming requests immediately, and serialize outgoing frames. Do not wait for a final prompt response while ignoring permission requests; some agent requests deliberately block progress. [14]

Keep cancellation and approval handling separate from bulk text processing. Prioritize control traffic before writing frames, but do not assume you can preempt a frame already being written or prevent head-of-line blocking on a shared transport. Limit outbound message sizes and define timeout/error behavior.

### Bound bytes as well as queue entries

A queue of ten messages is not small when those messages contain huge tool outputs. Set limits for frame size, per-session queued bytes, global queued bytes, subscriber backlog, and disk spool. Page historical events from the ledger rather than retaining transcripts in memory.

> Bounded memory, unlimited producer speed, and indefinitely lossless recording cannot all be guaranteed when storage is slower. Choose backpressure, a capped disk spool, or a visibly interrupted session. Never silently drop control events or describe an incomplete ledger as complete.

### Cancellation must have stages

First request a protocol-level turn interruption. Then allow a defined grace period. Escalate to the owned operating-system process group or job when necessary. Finally drain what can be drained, wait/reap, close resources, resolve pending requests, and record the observed terminal state. Keep “interrupt turn” separate from “stop agent process.”

Unix process-group signalling and Windows Job Objects are distinct mechanisms and need distinct implementations. Process groups are not a sandbox; descendants may change groups or otherwise escape simple tree assumptions. Use stronger OS containment only when its guarantees are required and tested. [25, 26]

### Security and failure boundaries

Resolve an explicit executable path, pass argument arrays instead of shell strings, and set working directory/environment per launch. Do not mutate the daemon’s global working directory for a session. Protect local sockets or named pipes by user permissions and authorization. A loopback port by itself is not authorization.

Treat agent output as untrusted data. Redact credential-bearing diagnostics, cap captured output, restrict ledger access, and avoid storing raw environment values. A logging failure must produce a visible degraded state. Do not silently relax the agent’s own permission or sandbox settings.

## 9. SQLite and the durable ledger

Recommendation: begin with SQLite on local storage, WAL mode, one dedicated writer, short transactions, and a bounded write queue. WAL permits concurrent readers and one writer; it does not provide unlimited concurrent writers. Avoid long read transactions that obstruct checkpoint progress. [27]

| Table | Purpose |
| --- | --- |
| sessions | Logical identity, provider, workspace, current state, capability snapshot. |
| runs | Launch attempt, agent/adapter versions, process identity, start/end times, exit status, recovery relationship. |
| commands | Requested action, command ID, intent time, delivery and acknowledgement state. |
| events | Per-session/run ordering, event kind, timestamp, normalized metadata, provider payload reference. |
| approvals | Provider request ID, requested permission, user decision, delivery state. |

Update current state and its corresponding event in the same transaction. Use explicit schema versions, embedded migrations, indexes for event pagination and active runs, and a documented retention/export policy. A single writer is an application ownership rule, not merely a SQLite setting.

### Control durability versus transcript durability

Commit launch intent, approval decisions, and terminal states promptly. Batch ordinary text deltas by a bounded byte/count/time policy. A simple initial choice is synchronous=FULL with batched transcript transactions; even then, uncommitted queued text can be lost. NORMAL has different power-loss durability behavior. State the guarantee and test it. [28]

Record the actual embedded SQLite engine version and pin a maintained build. SQLite documents a WAL-reset fix in 3.51.3 and later, with selected older backports. Driver package version and database-engine version are not interchangeable. [27]

### The external-action atomicity gap

```text
1. Commit command intent
2. Send command to agent
3. Observe acknowledgement
4. Commit observed outcome
```

A crash between steps 2 and 4 can leave delivery uncertain. The database commit and the external action are not one transaction. Model delivery_uncertain/unknown explicitly. Do not automatically replay a potentially side-effecting command unless the provider supports appropriate idempotency or reconciliation.

After daemon restart, reconcile recorded runs against process identity and provider state where available. Ordinary pipes cannot simply be reattached to arbitrary surviving processes. A dead daemon may mean resuming a conversation in a new process, not recovering the old live execution.

> The ledger records what VIA requested and observed. It must not claim exactly-once effects or guaranteed live recovery across arbitrary agents.

## 10. Rust implementation stack

This is the Rust stack I would use. Package capabilities below are documented; selecting and combining them is a design recommendation. Pin tested releases in Cargo.lock and keep optional dependencies out of the initial build.

| Layer | Selected package / API | Role in VIA |
| --- | --- | --- |
| CLI | clap [29] | Subcommands, argument validation, native-argument forwarding. |
| Async runtime | tokio [11, 21] | Process I/O, tasks, timers, signals, and local networking. |
| Cancellation | tokio-util CancellationToken; Tokio task/channel APIs [11, 30] | Session cancellation, tracked task completion, bounded queues. |
| JSON / schemas | serde + serde_json [31] | Typed envelopes, provider-specific messages, preserved raw JSON payloads. |
| ACP | agent-client-protocol [1] | Upstream ACP client implementation, behind a VIA-owned adapter interface. |
| Native protocols | Small VIA codecs over Tokio I/O | Codex stdio and agent-specific structured streams. Keep framing explicit. |
| SQLite | rusqlite with bundled [24] | Embedded SQLite owned by one dedicated database thread. |
| Configuration | toml + Serde [32] | Typed configuration with explicit precedence and validation. |
| Observability | tracing + tracing-subscriber [33] | Session/run spans, structured diagnostics, configurable sinks. |
| Errors | thiserror + anyhow [34] | Typed core/adapter errors; contextual errors at executable boundaries. |
| Local IPC | Tokio UnixStream / named-pipe APIs [35] | One versioned local wire format over Unix sockets or Windows named pipes. |
| OS lifecycle | nix on Unix; windows/windows-sys on Windows [36] | Process-group/job integration behind platform-specific modules. |

### Rust-specific design choices

Use one session owner and message passing for state changes. Track spawned tasks explicitly; do not detach tasks casually. Keep synchronous SQLite work off Tokio’s worker threads. A dedicated database thread provides a clear ownership and batching boundary. [11, 24]

Do not assume every ACP library object is Send or that every release has the same runtime model. Validate the selected crate’s requirements during the first integration prototype. Hide those details within the adapter rather than allowing them to dictate the public VIA API.

> Core shape: Tokio owns asynchronous orchestration; session owners own state; the database thread owns SQLite; platform modules own process containment.

## 11. Rust implementation details and extensions

### Task and state structure

```text
supervisor
  session_owner(run_id)
    stdout_reader -> protocol event router
    stderr_reader -> bounded diagnostic capture
    outbound_writer <- serialized control/messages
    child_waiter -> lifecycle owner

  database_thread <- bounded persistence requests
```

Represent lifecycle states explicitly: starting, ready, busy, waiting_for_input, stopping, exited, failed, and unknown. Keep process state separate from turn state. A provider may be alive while a turn has failed or is waiting for approval.

Avoid a global mutex around sessions or holding locks across slow I/O. Keep rich public error types for unsupported capabilities, malformed frames, overload, authentication failure, process exit, and uncertain delivery. A broad catch-all error string loses operational meaning.

### Optional dependencies: add only for a concrete requirement

| Need | Rust choice | Boundary |
| --- | --- | --- |
| OpenCode HTTP API | reqwest [37] | Add only for the HTTP adapter. Select TLS features deliberately and inspect native linkage. |
| Copilot native SDK | github-copilot-sdk [7] | Alternative to ACP, not an additional mandatory layer. Preserve explicit permission policy. |
| MCP frontend | rmcp, official Rust SDK [38] | Expose VIA actions to MCP clients; it is not the internal agent-control protocol. |
| Terminal UI / PTY | Defer initially | Inherit the native terminal for pass-through. Introduce terminal libraries only for an explicit managed-terminal feature. |
| Heavy observability | Defer initially | Begin with structured logs and local counters; avoid requiring an external telemetry service. |

### Testing and builds

Use Rust unit/integration tests, protocol fixture tests, deterministic fake-agent binaries, formatting/lint checks, and platform-native process tests. Add property/fuzz tests around parsers and lifecycle transitions. These are recommended quality gates, not completed validations.

Build release artifacts for each supported OS/architecture. For a static Linux target, evaluate musl with the full dependency graph, including bundled SQLite and any TLS libraries. Keep the build toolchain separate from the end-user requirement. Inspect linkage rather than inferring it from the language. [22, 24]

Do not optimize away debuggability or choose an allocator based on generic benchmark claims. Measure the real supervisor workload before changing runtime worker counts, allocators, JSON libraries, or database bindings.

## 12. Go implementation stack

This is the Go stack I would use—not merely a translation of the Rust package names. Prefer standard-library components, a small dependency set, and a deliberately CGo-free release graph.

| Layer | Selected package / API | Role in VIA |
| --- | --- | --- |
| CLI | github.com/spf13/cobra [39] | Subcommands, help, completion, validation, and native arguments after --. |
| Concurrency | goroutines, context, channels; golang.org/x/sync/errgroup [10, 40] | Per-session ownership, coordinated errors/cancellation, explicit goroutine completion. |
| Subprocesses | os/exec + os/signal [20, 57] | Start actual binaries with explicit cwd/env, pipe streams, observe exit. |
| JSON / framing | encoding/json/v2 + jsontext + bounded bufio framing [41, 53, 58] | Go 1.27+ baseline for VIA-owned codecs; typed events and preserved raw JSON extensions. |
| ACP | github.com/coder/acp-go-sdk [2, 43] | My first community-library candidate. Pin and test its protocol/version coverage. |
| SQLite | database/sql + modernc.org/sqlite [23, 55] | CGo-free embedded ledger with a dedicated writer owner. |
| Schema / queries | embed + numbered SQL migrations [56] | Keep small explicit SQL statements and migration checks; no ORM initially. |
| Configuration | github.com/pelletier/go-toml/v2 [44] | Typed TOML configuration; defaults, file, environment, flags in explicit order. |
| Logging | log/slog [45] | Structured session/run diagnostics without a required logging framework. |
| Local IPC | net Unix sockets; github.com/Microsoft/go-winio [46] | Versioned local protocol; named pipes with restrictive Windows access controls. |
| OS lifecycle | golang.org/x/sys/unix and /windows [47] | Platform-specific process groups, Windows Job Objects, handles, and signals. |
| Diagnostics | runtime/pprof, runtime/trace, runtime metrics [15] | Measure heap, allocations, CPU, goroutine growth, blocking, and scheduler behavior. |

### Why this stack

Cobra gives the command surface; goroutines handle concurrent waiting; a session owner serializes state; modernc keeps SQLite on the CGo-free path; slog and Go diagnostics avoid extra infrastructure. The ACP dependency is isolated so it can be upgraded or replaced without changing VIA’s public API.

> Do not use net/rpc/jsonrpc as your ACP/Codex transport. It implements JSON-RPC 1.0, not the protocol these integrations require. Use the ACP library or a tested provider-specific codec. [42]

## 13. Go implementation details and tooling

### Session ownership and cancellation

Use a session goroutine as the state owner, with bounded queues and independently drained stdout/stderr. Derive its lifetime from the daemon, not the short-lived CLI request context: a caller disconnect must not cancel a detached session. Use errgroup for task coordination, but make channel ownership and every goroutine’s termination path explicit. [40]

Customize cancellation and process-group/job handling instead of relying only on CommandContext. Arrange process waiting and pipe readers according to the os/exec lifecycle contract; use bounded drain/exit deadlines and handle inherited pipe handles that keep streams open. Shutdown should not wait forever. [20]

### Protocol and buffer safeguards

Go-specific update: for a new Go 1.27+ codebase, prefer encoding/json/v2 and jsontext.Value in VIA-owned codecs. Go now documents stricter v2 defaults. Retain v1 encoding/json at SDK boundaries that require its types, with explicit conversion and fixture tests. Preserve IDs without lossy number conversion. Enforce frame-size limits and check reader errors; bufio.Scanner has a maximum token size. [41, 53, 58]

### SQLite ownership

Use one writer goroutine and a dedicated database connection, with an explicitly bounded reader pool only when needed. database/sql is a pool abstraction; naming one goroutine “writer” does not configure that pool. Set the pool limits, close query rows promptly, and apply per-connection settings consistently. [55]

Keep migrations embedded, record the schema version, and execute migrations before accepting sessions. Pin the SQLite driver and its validated transitive dependencies. modernc documents a specific libc-version compatibility requirement; do not override it casually. Benchmark your query mix rather than assuming all SQLite drivers perform identically. [23]

### Tooling and optional extensions

| Concern | Go choice / policy |
| --- | --- |
| Copilot native SDK | github.com/github/copilot-sdk/go; optional alternative to ACP. [7] |
| OpenCode HTTP | net/http with streaming consumption and explicit timeouts; no web framework required. [54] |
| MCP frontend | github.com/modelcontextprotocol/go-sdk; optional, official SDK. [48] |
| Quality gates | go test, go vet, race-enabled tests, built-in fuzzing, and govulncheck. [19, 49] |
| Release automation | GoReleaser for target builds, archives, and release workflow; build-time tooling only. [50] |
| Initial exclusions | No ORM, global event bus framework, distributed queue, PTY emulation, or remote service requirement. |

Important build distinction: use a CGo-free production build, but run race-enabled tests in a supported CI environment with the race detector’s required toolchain. Do not assume CGO_ENABLED=0 can be used for those tests. [19]

## 14. Interface, packaging, and release policy

### Proposed VIA commands—not existing commands

```text
via claude -- <native Claude arguments>
via start claude --cwd /projects/example
via send <session-id> "Review the authentication changes"
via events <session-id>
via stop <session-id>
via daemon
via doctor
```

Keep pass-through and managed-session semantics visibly distinct. Define interrupt separately from stop. A doctor command should inspect configured executable paths, versions, protocol availability, dependency prerequisites, ledger access, and local IPC—not make billable agent calls unless explicitly requested.

### Build profiles

| Concern | Go | Rust |
| --- | --- | --- |
| Production build | CGO_ENABLED=0 go build for a tested dependency graph. | cargo build --release --locked for a tested target. |
| Linux static goal | Inspect output and dependencies; test in a clean environment. | Evaluate a musl target and verify all native/TLS dependencies. |
| Embedded database | modernc.org/sqlite; no end-user SQLite install. [23] | rusqlite bundled; build-time C tooling, no end-user SQLite install. [24] |
| Dependency record | Commit go.mod and go.sum; pin build tools. | Commit Cargo.lock; pin toolchain and build tools. |
| Native platforms | Build and test Windows, macOS, and Linux separately. | Build and test Windows, macOS, and Linux separately. |

These are build blueprints, not complete copy-paste release scripts. Cross-platform availability of the selected packages does not prove that the actual process-supervision behavior is correct. Test each supported architecture/OS combination. [16, 22]

### Installation and upgrade boundaries

Distribute VIA separately from agents. Do not silently install agents, copy their credentials, or auto-update their binaries. Maintain a compatibility manifest with VIA version, adapter version, agent version/range, protocol version, and capabilities that passed tests.

Record actual executable paths and database-engine versions in diagnostics. Separate build-time tools from runtime prerequisites. Keep a reproducible artifact manifest with checksums and dependency/license notices; add platform signing as part of the release workflow where applicable.

### Suggested module boundaries

Use the same architecture in either language: CLI/frontend; local IPC; supervisor/session state; provider adapters; transport codecs; platform process handling; ledger/migrations; configuration; observability; fake-agent fixtures. Public clients should depend on the VIA contract, not the internal ACP library’s data types.

## 15. Implementation sequence and validation

### Build three adapter shapes before adding every agent

First implement pass-through, capability discovery, and a fake managed agent. Then add one native-server adapter (Codex, with its support caveat), one native-ACP adapter (OpenCode is a useful candidate), and Claude’s structured CLI adapter. This tests materially different integration shapes before multiplying providers. [3, 5, 6]

Add the durable session owner, approval routing, bounded persistence, and restart reconciliation. Then expand to Copilot, Cursor extensions, and Grok. Expose an SDK or MCP frontend only after the common session contract is stable enough to support it.

### Acceptance tests

| Scenario | Required observation |
| --- | --- |
| Hours of idle time | No busy polling, unexplained memory growth, or accumulating tasks/goroutines. |
| Output bursts and large messages | Bounded bytes; explicit oversize-frame policy; continuously drained stderr. |
| Slow UI or storage | Visible backpressure/spool/failure policy; no silent control-event loss. |
| Approval wait and cancellation | Reader remains responsive; interruption and stop have distinct outcomes. |
| Parent/descendant failure | Owned processes are handled; exit/drain waits are bounded; handles are released. |
| Database full or write failure | Ledger degradation is visible; no false durable-success acknowledgement. |
| Daemon crash at delivery boundary | Uncertain commands remain uncertain until reconciled; no blind replay. |
| Agent upgrade | Protocol fixtures and live smoke tests catch changed flags, fields, and approval behavior. |

Benchmark a deterministic fake agent at 1, 10, 40, and 100 sessions as a proposed test ladder, not a capacity claim. Measure VIA RSS/heap, CPU, allocation rate, queued bytes, persistence latency, event latency, and process/thread counts. Measure the complete agent-process trees separately. Run endurance and failure tests on each target OS.

### Existing projects worth studying

acpx offers a common headless ACP-oriented interface with sessions and permissions, but requires Node.js and therefore does not directly meet the proposed standalone VIA packaging goal. AgentAPI wraps actual agents through an HTTP API and an in-memory terminal emulator; its terminal-parsing approach is a different trade-off from native protocol integration. [51, 52]

> Final recommendation: Rust + Tokio + SQLite. The Go stack in Sections 12–13 is a strong alternative, not a fallback of last resort. Bounded memory, process ownership, permission handling, and honest recovery semantics matter more than the narrow language-score difference.

## References 1 / 3

Sources accessed on 25 September 2026. Inline [n] markers refer to the entries below. Sources establish documented behavior and package capabilities; VIA architecture choices and scores are recommendations. Links are live documentation, not immutable snapshots or proof of completed integration tests.

<a id="ref-1"></a>
**[1] ACP: Rust library** — [Source 1](https://agentclientprotocol.com/libraries/rust)

<a id="ref-2"></a>
**[2] ACP: community libraries, including Go** — [Source 1](https://agentclientprotocol.com/libraries/community)

<a id="ref-3"></a>
**[3] Claude Code: run programmatically** — [Source 1](https://code.claude.com/docs/en/headless)

<a id="ref-4"></a>
**[4] Claude Agent ACP bridge repository** — [Source 1](https://github.com/agentclientprotocol/claude-agent-acp)

<a id="ref-5"></a>
**[5] OpenAI: Codex App Server** — [Source 1](https://learn.chatgpt.com/docs/app-server)

<a id="ref-6"></a>
**[6] OpenCode: ACP and server APIs** — [Source 1](https://opencode.ai/docs/acp/); [Source 2](https://opencode.ai/docs/server/)

<a id="ref-7"></a>
**[7] GitHub Copilot SDK: languages, architecture, status** — [Source 1](https://github.com/github/copilot-sdk)

<a id="ref-8"></a>
**[8] GitHub Copilot CLI: ACP server** — [Source 1](https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server)

<a id="ref-9"></a>
**[9] MCP: architecture overview** — [Source 1](https://modelcontextprotocol.io/docs/learn/architecture)

<a id="ref-10"></a>
**[10] Effective Go: concurrency and goroutines** — [Source 1](https://go.dev/doc/effective_go)

<a id="ref-11"></a>
**[11] Tokio: task execution and blocking work** — [Source 1](https://docs.rs/tokio/latest/tokio/task/index.html)

<a id="ref-12"></a>
**[12] Rust Book: ownership** — [Source 1](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html)

<a id="ref-13"></a>
**[13] Rust Book: concurrency** — [Source 1](https://doc.rust-lang.org/book/ch16-00-concurrency.html)

<a id="ref-14"></a>
**[14] Cursor CLI: ACP and extension methods** — [Source 1](https://cursor.com/docs/cli/acp)

<a id="ref-15"></a>
**[15] Go: diagnostics and profiling** — [Source 1](https://go.dev/doc/diagnostics)

<a id="ref-16"></a>
**[16] Go: cgo and cross-compilation behavior** — [Source 1](https://pkg.go.dev/cmd/cgo)

<a id="ref-17"></a>
**[17] Grok Build: headless scripting and ACP** — [Source 1](https://docs.x.ai/build/cli/headless-scripting)

<a id="ref-18"></a>
**[18] Go: garbage collector guide and memory limit** — [Source 1](https://go.dev/doc/gc-guide)

<a id="ref-19"></a>
**[19] Go: race detector and requirements** — [Source 1](https://go.dev/doc/articles/race_detector)

## References 2 / 3

<a id="ref-20"></a>
**[20] Go: os/exec lifecycle and cancellation** — [Source 1](https://pkg.go.dev/os/exec)

<a id="ref-21"></a>
**[21] Tokio: subprocess API and caveats** — [Source 1](https://docs.rs/tokio/latest/tokio/process/index.html)

<a id="ref-22"></a>
**[22] Rust Reference: linkage** — [Source 1](https://doc.rust-lang.org/reference/linkage.html)

<a id="ref-23"></a>
**[23] modernc.org/sqlite: CGo-free driver and constraints** — [Source 1](https://pkg.go.dev/modernc.org/sqlite)

<a id="ref-24"></a>
**[24] rusqlite: bundled SQLite feature** — [Source 1](https://github.com/rusqlite/rusqlite)

<a id="ref-25"></a>
**[25] Linux man-pages: process-group signalling** — [Source 1](https://man7.org/linux/man-pages/man2/kill.2.html)

<a id="ref-26"></a>
**[26] Microsoft: Windows Job Objects** — [Source 1](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)

<a id="ref-27"></a>
**[27] SQLite: WAL, checkpoints, and WAL-reset fix** — [Source 1](https://sqlite.org/wal.html)

<a id="ref-28"></a>
**[28] SQLite: synchronous and other PRAGMA settings** — [Source 1](https://sqlite.org/pragma.html#pragma_synchronous)

<a id="ref-29"></a>
**[29] clap: Rust CLI parser** — [Source 1](https://docs.rs/clap/latest/clap/)

<a id="ref-30"></a>
**[30] tokio-util: CancellationToken** — [Source 1](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html)

<a id="ref-31"></a>
**[31] serde_json: typed and raw JSON** — [Source 1](https://docs.rs/serde_json/latest/serde_json/)

<a id="ref-32"></a>
**[32] toml: Rust TOML support** — [Source 1](https://docs.rs/toml/latest/toml/)

<a id="ref-33"></a>
**[33] tracing: Rust diagnostics ecosystem** — [Source 1](https://docs.rs/tracing/latest/tracing/); [Source 2](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/)

<a id="ref-34"></a>
**[34] Rust error crates: thiserror and anyhow** — [Source 1](https://docs.rs/thiserror/latest/thiserror/); [Source 2](https://docs.rs/anyhow/latest/anyhow/)

<a id="ref-35"></a>
**[35] Tokio: Unix sockets and Windows named pipes** — [Source 1](https://docs.rs/tokio/latest/tokio/net/struct.UnixStream.html); [Source 2](https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/index.html)

<a id="ref-36"></a>
**[36] Rust operating-system bindings: nix and windows-rs** — [Source 1](https://docs.rs/nix/latest/nix/); [Source 2](https://github.com/microsoft/windows-rs)

<a id="ref-37"></a>
**[37] reqwest: Rust HTTP client and feature choices** — [Source 1](https://docs.rs/reqwest/latest/reqwest/)

<a id="ref-38"></a>
**[38] Official MCP Rust SDK** — [Source 1](https://github.com/modelcontextprotocol/rust-sdk)

<a id="ref-39"></a>
**[39] Cobra: Go CLI framework** — [Source 1](https://github.com/spf13/cobra)

## References 3 / 3

<a id="ref-40"></a>
**[40] Go: errgroup coordination** — [Source 1](https://pkg.go.dev/golang.org/x/sync/errgroup)

<a id="ref-41"></a>
**[41] Go: buffered I/O and Scanner limits** — [Source 1](https://pkg.go.dev/bufio)

<a id="ref-42"></a>
**[42] Go: net/rpc/jsonrpc uses JSON-RPC 1.0** — [Source 1](https://pkg.go.dev/net/rpc/jsonrpc)

<a id="ref-43"></a>
**[43] Coder: community ACP Go SDK** — [Source 1](https://github.com/coder/acp-go-sdk)

<a id="ref-44"></a>
**[44] go-toml/v2: typed TOML in Go** — [Source 1](https://github.com/pelletier/go-toml)

<a id="ref-45"></a>
**[45] Go: log/slog structured logging** — [Source 1](https://pkg.go.dev/log/slog)

<a id="ref-46"></a>
**[46] Microsoft go-winio: Windows named pipes** — [Source 1](https://github.com/microsoft/go-winio)

<a id="ref-47"></a>
**[47] Go platform APIs: x/sys/unix and x/sys/windows** — [Source 1](https://pkg.go.dev/golang.org/x/sys/unix); [Source 2](https://pkg.go.dev/golang.org/x/sys/windows)

<a id="ref-48"></a>
**[48] Official MCP Go SDK** — [Source 1](https://github.com/modelcontextprotocol/go-sdk)

<a id="ref-49"></a>
**[49] Go: fuzz testing and vulnerability tooling** — [Source 1](https://go.dev/doc/tutorial/fuzz); [Source 2](https://go.dev/doc/security/vuln/)

<a id="ref-50"></a>
**[50] GoReleaser: getting started** — [Source 1](https://goreleaser.com/getting-started/)

<a id="ref-51"></a>
**[51] acpx: headless ACP CLI and prerequisites** — [Source 1](https://github.com/openclaw/acpx)

<a id="ref-52"></a>
**[52] AgentAPI: HTTP wrapper and terminal-emulation design** — [Source 1](https://github.com/coder/agentapi)

<a id="ref-53"></a>
**[53] Go JSON APIs: v2, raw JSON, and v1 compatibility** — [Source 1](https://pkg.go.dev/encoding/json/v2); [Source 2](https://pkg.go.dev/encoding/json/jsontext); [Source 3](https://pkg.go.dev/encoding/json)

<a id="ref-54"></a>
**[54] Go: HTTP client and streaming response APIs** — [Source 1](https://pkg.go.dev/net/http)

<a id="ref-55"></a>
**[55] Go: database/sql pools and connections** — [Source 1](https://pkg.go.dev/database/sql)

<a id="ref-56"></a>
**[56] Go: embedded build-time files** — [Source 1](https://pkg.go.dev/embed)

<a id="ref-57"></a>
**[57] Go: operating-system signal handling** — [Source 1](https://pkg.go.dev/os/signal)

<a id="ref-58"></a>
**[58] Go 1.27: JSON v2 and compatibility notes** — [Source 1](https://go.dev/doc/go1.27)

### Revalidation before implementation

Pin agent and dependency versions after the compatibility prototype. Recheck maturity labels, authentication/configuration defaults, protocol methods, embedded SQLite version, and operating-system support before release. No performance measurements are claimed: memory examples are arithmetic, scores are subjective, and the benchmark ladder is a proposed test plan.
