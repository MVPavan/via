## 1. Identity

OpenCode is Anomaly’s open source coding agent (the project repository is `anomalyco/opencode`), licensed MIT. The ACP registry lists Anomaly as author and MIT as license. [OpenCode repository](https://github.com/anomalyco/opencode), [ACP registry entry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json) — registry version **1.18.32**, checked 2026-09-24.

The locally installed CLI reports **1.18.21**. Its executable is at `~/.opencode/bin/opencode`; install method is **UNVERIFIED**. The registry and release page show newer **1.18.32**, released 2026-09-21, and a run of recent patch tags. Releases appear frequent; I found no fixed published cadence. [OpenCode releases](https://github.com/anomalyco/opencode/releases) — latest shown 2026-09-21.

I ran read-only help on `opencode`, `opencode run`, `opencode serve`, `opencode acp`, `opencode session`, `opencode stats`, and `opencode auth`. Exact installed flags for the main routes:

- `run`: `--command`, `-c/--continue`, `-s/--session`, `--fork`, `--share`, `-m/--model`, `--agent`, `--format default|json`, `-f/--file`, `--title`, `--attach`, `-p/--password`, `-u/--username`, `--dir`, `--port`, `--variant`, `--thinking`, `-i/--interactive`, `--auto`; global flags include `--print-logs`, `--log-level`, and `--pure`.
- `serve`: `--port` (installed help default `0`), `--hostname` (default `127.0.0.1`), `--mdns`, `--mdns-domain`, `--cors`, and global flags.
- `acp`: the same network flags plus `--cwd` (default current directory), and global flags.

The installed help’s `serve` port default differs from the online server docs’ `4096`; **for this machine, help reports `0`**. [Installed OpenCode help, 1.18.21; online server docs, checked 2026-09-24](https://opencode.ai/docs/server/)

## 2. Auth and billing

OpenCode supports provider API keys and saved provider credentials; its docs say `/connect` stores credentials in `~/.local/share/opencode/auth.json`. It also documents subscription OAuth options for some providers. For OpenAI, the docs say to select “ChatGPT Plus/Pro” during `/connect`; they also list ChatGPT Plus, GitHub Copilot, and GitLab Duo as subscriptions usable in OpenCode. [Providers docs](https://opencode.ai/docs/providers/) — checked 2026-09-24.

Subscription permission depends on the provider. OpenCode’s provider docs state: **“Anthropic explicitly prohibits this”** about plugins that use Claude Pro/Max models with OpenCode; the docs say those plugins have not been bundled since OpenCode 1.3.0. [Providers docs](https://opencode.ai/docs/providers/) — checked 2026-09-24. The docs explicitly present ChatGPT Plus/Pro as supported; I did not independently verify each provider’s terms. Do not assume that one provider’s subscription permission applies to another.

ACP authentication is local-account based: OpenCode advertises “Login with opencode” and directs the user to run `opencode auth login` in a terminal. Its ACP `authenticate` handler accepts that method without initiating a separate login flow. [OpenCode ACP service, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/acp/service.ts)

## 3. Headless CLI contract

The non-interactive path is `opencode run [message..]`. `--format json` emits newline-delimited JSON events. Each line is shaped as `{type, timestamp, sessionID, ...data}`. Observed event types in the versioned source include `text`, `step_start`, `step_finish`, `tool_use`, and `error`. A text event carries `part`; completed text is in `part.text`. A `step_finish` carries `part.reason`, `part.cost`, and `part.tokens` (`input`, `output`, `reasoning`, and cache fields). The session ID is repeated in event envelopes. There is no separate terminal `result` event; consumers need to accumulate text and detect session idle. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts); [message/event schema, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/session/message-v2.ts)

Errors can appear as JSON `error` events and provider/session error objects. The run source sets exit code 1 for prompt/command response errors and observed session errors. Logs go to stderr when `--print-logs` is used. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts)

Model selection is `--model provider/model`; provider-specific reasoning variants use `--variant` (the online docs describe variants as provider-specific). Agent selection is `--agent <name>`. `--agent` selects a configured OpenCode agent/profile; the CLI has no direct `--system` prompt flag. The server API/SDK accepts a `system` field per prompt, and agents can have configured prompts. [CLI docs](https://opencode.ai/docs/cli/), [SDK docs](https://opencode.ai/docs/sdk/), [agent docs](https://opencode.ai/docs/agents/) — checked 2026-09-24.

`--dir` sets the run directory. There is no `--add-dir` flag; external paths are governed by `external_directory` permission rules in config. The CLI has no sandbox flag. `--auto` approves permissions that are not explicitly denied; it is labeled dangerous. `--pure` disables external plugins, not file, shell, or network access. Non-interactive `run` adds `question`, `plan_enter`, and `plan_exit` deny rules, but that is not a general read-only policy. OpenCode’s documented defaults allow most permissions, with `external_directory` and `doom_loop` defaulting to ask. [OpenCode permissions docs](https://opencode.ai/docs/permissions/), [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts)

When stdin is not a TTY, the CLI reads it fully and appends it to the positional prompt with a newline. A pipe left open without EOF can therefore hold startup. There is no end-to-end run timeout flag. Provider request, header, and streamed-chunk timeouts are configurable separately. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts), [config docs](https://opencode.ai/docs/config/) — checked 2026-09-24.

The CLI’s `--format json` means **JSON event stream**, not schema-constrained answer JSON. The SDK supports `format: {type: "json_schema", schema, retryCount}` and returns structured output on the message. [SDK docs](https://opencode.ai/docs/sdk/) — checked 2026-09-24.

I found no first-run folder-trust dialog in the cited headless command documentation or `v1.18.21` run source. Whether other local configuration/plugins can block a first run is **UNVERIFIED**. Provider credentials may need to be set up beforehand via `opencode auth login` or provider environment/config. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts), [auth docs](https://opencode.ai/docs/providers/)

## 4. Session lifecycle in the CLI

`--session <id>` continues that session and sends a new prompt. Invalid IDs fail with “Session not found” and exit 1; they do not silently create a replacement. `--continue` selects the most recent root session, but if none is found the current source falls through to creating a fresh session. `--fork` forks a specified or continued session before sending the prompt. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts)

`opencode session` exposes list and delete commands; there is no CLI `status`, `result`, or `cancel` subcommand in installed help. For result retrieval, use session export or the HTTP API’s session/message endpoints. Without a session ID, `opencode export` prompts for a selection, so automation should pass the ID. Sessions and auth data are stored under the user’s OpenCode data directory; project-specific session/message data is organized by project. [CLI docs](https://opencode.ai/docs/cli/), [storage docs](https://dev.opencode.ai/docs/troubleshooting/) — checked 2026-09-24.

There is no native mid-turn stdin steering after `run` has read its initial prompt. Ctrl-C/process termination is the CLI-level stop path; the documented API and ACP routes provide explicit abort/cancel operations. Exact signal handling and shutdown guarantees are **UNVERIFIED**. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts)

## 5. Other programmatic surfaces

`opencode serve` runs a headless HTTP server with an OpenAPI description and an SDK generated from that API. It exposes session create/list/status/get, messages, asynchronous `prompt_async`, abort, fork, permission replies, event subscriptions, and message retrieval. It provides more direct lifecycle control than `run`; configure HTTP Basic Auth with `OPENCODE_SERVER_PASSWORD` and optionally `OPENCODE_SERVER_USERNAME`. [Server docs](https://opencode.ai/docs/server/) — checked 2026-09-24.

The TypeScript SDK can start/manage a server (`createOpencode`) or connect to an existing one (`createOpencodeClient`). It adds typed API access, stream handling, cancellation via request `AbortSignal`, system prompts, and structured output. [SDK docs](https://opencode.ai/docs/sdk/) — checked 2026-09-24.

OpenCode also documents an in-process `@opencode/sdk` host, which embeds the server without opening an HTTP listener. [SDK overview](https://opencode.ai/v2/docs/build/sdk) — checked 2026-09-24.

`opencode mcp` manages connections to external MCP servers; the docs describe OpenCode as an MCP client. Installed top-level help does not show an MCP server mode that exposes OpenCode as an MCP agent. A plugin-based way to provide one is **UNVERIFIED**. [MCP docs](https://opencode.ai/docs/mcp-servers/), installed CLI help — 1.18.21.

## 6. ACP support

ACP support is **native**, implemented in OpenCode’s own repository; no separate adapter project is required. OpenCode’s docs launch it as `opencode acp`, a JSON-RPC/NDJSON subprocess over stdin/stdout. The ACP registry entry lists Anomaly, MIT, and version **1.18.32** (checked 2026-09-24). [ACP docs](https://opencode.ai/docs/acp/) — last updated 2026-09-24; [OpenCode ACP registry entry](https://raw.githubusercontent.com/agentclientprotocol/registry/main/opencode/agent.json); [ACP registry](https://github.com/agentclientprotocol/registry)

The `v1.18.21` handshake advertises `loadSession`, `list`, `resume`, `close`, and `fork`; the implementation handles ACP `cancel`. It supports prompt content with embedded context and images. OpenCode’s docs say built-in tools, MCP, project rules, agents, and permissions work through ACP, while `/undo` and `/redo` are unsupported. [OpenCode ACP service, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/acp/service.ts), [ACP docs](https://opencode.ai/docs/acp/)

Model, effort, and mode are configurable over ACP through session config options: model selection, a model-supported variant/effort, and an available OpenCode agent/mode. Invalid selections return errors. The ACP usage update reports context tokens used/size and total USD cost; it does not report separate input/output token counts in that update. [OpenCode ACP service, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/acp/service.ts), [usage implementation, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/acp/usage.ts)

ACP supports cancel but does not advertise a mid-turn steering capability. Sending another prompt is a follow-up prompt, not a documented injection into the currently running model turn. Permission/mode config flows through OpenCode’s configured agents and permissions; ACP does not expose a sandbox boundary. [OpenCode ACP service, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/acp/service.ts)

Versioned issue reports document a `opencode acp` process exiting early in **1.4.3** and a JSON CLI run hanging after its answer in **0.17.7**; neither report establishes the same bug in **1.18.21**. Current `acp.ts` waits for stdin to end, but that does not prove every client lifecycle is bug-free. [ACP issue #22795](https://github.com/anomalyco/opencode/issues/22795), [CLI issue #32506](https://github.com/anomalyco/opencode/issues/32506), [ACP command source, tag `v1.18.21`](https://github.com/anomalyco/opencode/blob/v1.18.21/packages/opencode/src/cli/cmd/acp.ts)

## 7. VIA verb matrix

| VIA verb | Best route | Support | Notes |
|---|---|---|---|
| `spawn` | `opencode run --format json` | Native | One prompt per process; captures session ID, events, usage, and exit status. |
| `resume` | `opencode run --session ID --format json` | Native | Continues by ID; invalid ID fails loudly. |
| `steer` | HTTP API/SDK | Partial | Can send another prompt, including asynchronously; documented as a message, not proven mid-turn injection. |
| `cancel` | HTTP API/SDK or ACP | Native | Session abort or ACP cancel; CLI itself has no cancel subcommand. |
| `status` | HTTP API/SDK or ACP | Native | HTTP session status endpoint; ACP supports session listing/lifecycle, but no standalone `status` RPC. |
| `result` | HTTP API/SDK or CLI export | Native | Read stored messages/export by ID; CLI’s run output is a stream to normalize. |

HTTP capabilities and ACP lifecycle methods are documented in the [server API](https://opencode.ai/docs/server/) and implemented in the [ACP service at `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/acp/service.ts).

## 8. Pitfalls for automation

- **No CLI sandbox:** `run` has no write/network confinement flag; `--auto` expands permission. Most permissions default to allow. This is material for VIA’s proposed per-spawn isolation and write-bound guarantees. [CLI help, 1.18.21](https://opencode.ai/docs/cli/), [permissions docs](https://opencode.ai/docs/permissions/)
- **Prompt construction:** positional arguments are shell-parsed; quote prompts safely. Piped stdin is read to EOF and appended to the prompt, which can block if the producer keeps the pipe open. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts)
- **Output/event drift:** consume the versioned event envelope, not formatted terminal text. `--format json` emits event lines and has no terminal result record; final text and token/cost data are distributed across events. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts)
- **Resume fallback:** an invalid explicit session ID fails loudly, but `--continue` with no prior root session creates a new session. [OpenCode `run.ts`, tag `v1.18.21`](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.21/packages/opencode/src/cli/cmd/run.ts)
- **Hangs/timeouts:** no end-to-end timeout flag; an open report for 0.17.7 describes JSON mode waiting indefinitely for an idle event after the answer was stored. Use an outer deadline and test against the pinned version. [Issue #32506](https://github.com/anomalyco/opencode/issues/32506), [config timeouts](https://opencode.ai/docs/config/)
- **Subscription compliance:** provider rules differ. OpenCode documents ChatGPT Plus/Pro use but explicitly warns against Claude Pro/Max through plugins. [Providers docs](https://opencode.ai/docs/providers/)

## 9. Recommendation

For VIA v0’s `spawn` / `resume` / `result`, use **`opencode run --format json`** as the simplest one-shot route. Pin the tested OpenCode version, parse the event stream, persist the returned session ID, and treat exit code plus session/error events as separate signals. If VIA needs status/cancel or structured output, use the HTTP API/SDK; ACP is a viable native lifecycle protocol, but its usage update omits separate input/output token counts.

Keep the current adapter’s refusal when VIA promises bounded writes or a read-only run: OpenCode does not provide a CLI sandbox flag, and `--auto` is permissive. Enable execution only when VIA itself supplies the required isolation and policy boundary. [Existing OpenCode adapter](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/profiles/opencode.py), [OpenCode permissions docs](https://opencode.ai/docs/permissions/), [CLI docs](https://opencode.ai/docs/cli/)

### Sources

Primary sources cited above: [OpenCode CLI docs](https://opencode.ai/docs/cli/), [server docs](https://opencode.ai/docs/server/), [SDK docs](https://opencode.ai/docs/sdk/), [ACP docs](https://opencode.ai/docs/acp/), [permissions docs](https://opencode.ai/docs/permissions/), [provider docs](https://opencode.ai/docs/providers/), [OpenCode `v1.18.21` source](https://github.com/anomalyco/opencode/tree/v1.18.21), [ACP registry](https://github.com/agentclientprotocol/registry) (registry snapshot checked 2026-09-24).