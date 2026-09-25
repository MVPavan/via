## 1. Verdict

**Build VIA as a separately installable Python 3.13+ library and CLI, using `asyncio`.** Use `asyncio.create_subprocess_exec`, `TaskGroup` and bounded stream readers; official `agent-client-protocol` for ACP; official Python vendor SDKs where their controls and observability pass adapter tests; protocol-specific clients for other RPC surfaces; HTTPX for HTTP; Pydantic v2 for contracts; `sqlite3` with WAL and short transactions; `structlog`; `argparse`; and pytest with fake processes and replay fixtures. Distribute wheels through `uv tool` or `pipx`. Start background execution with detached per-run workers, subject to a recovery prototype, and expose both Python and versioned JSON process interfaces. **No custom TypeScript sidecar initially.** Python wins through sufficient verified SDK coverage and a smaller integration change—not because competing languages require rewriting the interpreter. Confidence is moderate: lifecycle correctness remains unmeasured.

## 2. Agreement

All three members support:

- Python, a new adapter contract, and reuse of existing vendor knowledge.
- Official SDKs or documented vendor CLI/RPC/ACP routes; no credential extraction.
- Explicit capability declarations and named refusals for unsupported verbs.
- SQLite run state, separate raw logs, version-pinned adapter tests and live qualification.
- Per-run background workers rather than a mandatory central daemon.
- Deferring TypeScript sidecars until a required capability justifies one.
- Treating Windows cleanup and distribution as Python’s principal weaknesses.

This agreement is well supported, but the reports share repository research; their matching claims are not three independent factual confirmations.

## 3. Disputes

**A. `asyncio` versus AnyIO**

Fable favors AnyIO’s cancel scopes and structured concurrency. Opus and Sol favor stdlib `asyncio`; ACP’s Python SDK explicitly uses asyncio transports. AnyIO is capable, but its documentation warns that its cancellation semantics can trouble code written for asyncio. [ACP Python SDK](https://github.com/agentclientprotocol/python-sdk), [AnyIO cancellation](https://anyio.readthedocs.io/en/stable/cancellation.html)

**Ruling:** choose `asyncio` as VIA’s lifecycle owner. Python already supplies task groups and timeouts. Allow SDK-internal AnyIO, with explicit cleanup tests; neither framework automatically guarantees correct vendor cancellation.

**B. Official SDK versus handwritten Codex RPC**

Opus and Sol allow the Python SDK; Fable prefers raw app-server RPC because the SDK might be synchronous.

**Checked:** OpenAI documents **`AsyncCodex`**, app-server JSON-RPC transport, stable Python releases and a pinned runtime dependency. [Official Codex SDK documentation](https://learn.chatgpt.com/docs/codex-sdk)

**Ruling:** test the official async SDK first. Write a direct client only for demonstrated missing controls, transport visibility or ownership requirements. The suspected async limitation does not support building one.

**C. How much Python code transfers?**

Opus proposes substantial primitive extraction and direct catalog reuse. Fable proposes new supervision but says the ledger and catalog transfer whole. Sol recommends targeted extraction.

**Checked:** the [profile base](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/profiles/_base.py) imports interpreter execution and sandbox types; the [catalog](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/foreman/model_catalog.py#L28) imports foreman configuration and profile constants; the [ledger fence](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/ledger/fence.py) uses `fcntl`; the inspector explicitly declares itself Linux-only.

**Ruling:** Sol’s narrower position is strongest. Transfer parsers, argv knowledge, fixtures and invariants selectively. Build VIA’s own lifecycle and task-independent contracts. Python reduces adaptation cost, but neither wholesale transfer nor a full interpreter port is necessary.

**D. Does a separate VIA database violate ADR 0006?**

Opus says yes; Fable leaves an ADR question; Sol interprets the ADR as governing workflow facts.

**Checked:** the decision establishes `LedgerStore` as the sole implementation of `WorkflowStore`/`WorkflowReads` and removes competing workflow record backends. [ADR 0006](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/docs/adr/0006-ledger-only-record-store.md#L32)

**Ruling:** favor Sol’s interpretation. A separate execution service’s run store is not automatically a competing workflow store. Document ownership and reconciliation before implementation: VIA owns execution observations; the interpreter owns workflow decisions. The older handoff’s proposal to use the existing ledger still needs explicit reconciliation.

**E. How strong are TypeScript’s SDK and SQLite advantages?**

All favor TypeScript’s SDK breadth, but Opus penalizes Node’s SQLite options more heavily; Sol places Node only one scoring point behind Python.

The decisive checks support substantial Python coverage:

| Surface | Verified finding |
|---|---|
| ACP | Official Python, TypeScript, Rust and Kotlin libraries; a Java SDK repository also exists. Go has a community SDK. [ACP organization](https://github.com/agentclientprotocol), [Java](https://github.com/agentclientprotocol/java-sdk), [Go](https://github.com/coder/acp-go-sdk) |
| Codex | Python, including async support, and TypeScript. [Documentation](https://learn.chatgpt.com/docs/codex-sdk) |
| Copilot | Python, TypeScript, Go, .NET, Java and Rust; GA. [Official repository](https://github.com/github/copilot-sdk) |
| Cursor | Python exposes sync and async clients. [Python documentation](https://cursor.com/docs/sdk/python) |
| Antigravity | Official Python SDK with an async runtime interface. [Vendor documentation](https://www.antigravity.google/docs/sdk/overview) |

Current Node documentation labels `node:sqlite` **release candidate**, not stable; maturity must be assessed against the chosen Node version. [Node SQLite documentation](https://nodejs.org/api/sqlite.html)

**Ruling:** TypeScript/Node is a credible runner-up. Python has enough native coverage to avoid an immediate sidecar. Neither a native SQLite dependency nor small subjective score differences establish a decisive disadvantage. No member proves complete lifecycle parity across every harness.

**F. Go/Rust superiority for process supervision and distribution**

Opus and Fable award substantial process-platform advantages to compiled stacks. Sol points out that descendant cleanup remains explicit work.

**Checked:** Go’s default `CommandContext` cancellation calls `Kill` on the command’s process; it does not establish process-tree cleanup. [Go documentation](https://pkg.go.dev/os/exec#CommandContext)

**Ruling:** favor Sol’s qualification. Go has a compelling distribution story, but changing language does not remove Windows Job Object integration, process identity races or escaped descendants. A compiled VIA also still depends on vendor binaries and possibly SDK runtimes.

**G. `argparse` versus Typer**

Opus and Fable cite existing repository conventions; Sol recommends Typer without identifying a needed feature.

**Ruling:** `argparse` initially. This is a minor choice; completion or CLI usability requirements could justify Typer later.

**H. Windows and adapter enablement**

Opus suggests deferring Windows; Fable suggests best-effort support; Sol proposes refusing guarantees that have not been verified. Opus/Fable also recommend disabling Cursor pending terms clarification.

**Ruling:** adopt explicit platform capability guarantees, following Sol. Avoid silently weakening `cancel`. Separately, the [handoff](../../workstreams/handoff.md) already says terms uncertainty does not block development. “Disabled pending uncertainty” is a proposed policy change, not an existing requirement; this adjudication makes no legal determination.

## 4. Errors found

- **Opus: Pi is not JSON-RPC 2.0.** Its documented records use fields such as `type`, `command` and `success`. Fable’s combined “JSON-RPC/JSONL client” also needs this separation made explicit. Share byte transport machinery, not a universal message schema. [Pi protocol](https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/docs/rpc.md)

- **Fable: the foreman does not have to call VIA in-process.** The brief explicitly permits a stable process boundary. Opus likewise overstates other languages’ migration cost by implying thousands of interpreter lines must be ported.

- **Fable: Codex async uncertainty is resolved by current documentation.** This was honestly marked UNVERIFIED, but it cannot remain a reason to prefer raw RPC. `AsyncCodex` is documented.

- **Opus and Fable: Claude CLI ≥2.1.257 is mis-scoped.** The cited README attaches that requirement to the system-prompt snapshot feature, not to the SDK’s general installation prerequisites. [Claude SDK README](https://github.com/anthropics/claude-agent-sdk-python)

- **Fable: “every harness has a route without a Node runtime” is too strong.** Avoiding VIA’s own Node sidecar does not establish that vendor executables or their installation paths need no Node runtime. Opus’s universal reachability claim likewise exceeds the verified adapter evidence.

- **Opus/Fable: direct or whole catalog/ledger reuse is overstated.** Their dependencies are interpreter-specific, and the ledger’s locking implementation is platform-specific.

- **Fable: retaining only final text does not prove bounded memory.** Final text itself can grow without bound. Opus’s line-by-line logging also needs explicit handling of oversized or unterminated records.

- **Unsupported estimates:** Opus’s “about 300 lines” JSON-RPC peer, Fable’s transferable-code fractions, and all three reports’ AI implementation velocity scores lack measurements. They should not decide the stack.

- **Minor packaging error:** Opus describes an existing uv workspace; [pyproject.toml](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/pyproject.toml) declares neither a workspace nor a build backend. Standalone packaging is new work.

## 5. Gaps

None develops these sufficiently:

- **Cross-process admission:** an in-memory semaphore cannot limit independently launched workers. Define global limits, worker claims and ownership fencing.
- **Crash recovery:** receipts precede dispatch, but reconciliation still needs to distinguish never launched, submitted, running and unknowable. Sol correctly preserves `unknown`; the recovery protocol remains unspecified.
- **Control delivery:** cancel/steer need durable request IDs, acknowledgements, ordering and duplicate handling. Database rows plus socket wakeups alone do not establish these semantics.
- **Supervisor containment:** isolate the vendor’s kill target so forced cleanup does not kill the worker before it records the outcome. Address descendants that escape ordinary process groups.
- **Blocking storage:** synchronous SQLite lock waits and filesystem writes can stall the loop that drains pipes and handles cancellation. Specify bounded queues and I/O ownership.
- **Logs and SDK visibility:** define whether “verbatim” means wire bytes or SDK-delivered events. SDKs may hide the underlying stream. Include disk-full behavior, retention, access permissions and bounded result storage.
- **Release compatibility:** adapter pins alone do not prevent automatic vendor upgrades or SDK dependency conflicts. Define tested version ranges, refusal policy and a minimal OS test matrix.
- **Role semantics:** identify how role instructions, model selection and permission policy survive resume across harnesses. The existing catalog is not yet a standalone cross-vendor contract.

## 6. Conditions that would change the verdict

- **Choose TypeScript/Node** if important required verbs repeatedly exist only in TS SDKs, or Python packages fail needed lifecycle tests.
- **Choose Go** if runtime-independent VIA installation and Windows operations become hard first-release requirements, and the selected routes work without extensive sidecars.
- **Consider Rust** if measured resource or systems requirements justify its implementation cost.
- **Reconsider per-run workers** if measured startup overhead, memory or centralized admission needs favor a shared supervisor.

**Cheap prototype: one day, one Python worker, no full adapter framework.** Exercise fake CLI, ACP and Pi-style peers; add one official async SDK smoke test.

Test 1, 8 and 32 concurrent runs; noisy stdout/stderr; oversized records; withheld permissions; database contention; caller and worker crashes; and cancellation of a child with a grandchild. Measure worker memory, drain progress, cancel latency and recovery outcomes. Require no duplicate dispatch after ambiguous crashes, continued control responsiveness, exact retained bytes, and explicit failure when limits are exceeded.

This settles the main uncertainty: **whether the proposed worker architecture preserves lifecycle and resource guarantees under failure**. If it fails, identify the cause before benchmarking a second language.

## 7. Member position summary

- **Claude Opus 5.5:** Python/asyncio, argparse and detached workers; strong operational detail, but overstates reuse and assumes too much common RPC machinery.
- **Claude Fable 5.1:** Python/AnyIO with new supervision; useful boundary analysis, but overstates mandatory in-process integration and misses Codex’s async API.
- **GPT-6 Sol:** Python/asyncio with targeted extraction; strongest qualifications about platform guarantees and adapter readiness, with an unjustified Typer preference.

Read-only adjudication: no files changed, no implementation tests run; existing Git changes were preserved.