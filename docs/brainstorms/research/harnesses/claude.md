# Anthropic Claude Code research

**Observed 2026-09-24.** I read the local CLI help and subcommand help without starting a session, logging in, or changing auth. Web sources below were checked the same day. “UNVERIFIED” marks behavior I could not confirm from primary sources.

## 1. Identity

Claude Code is Anthropic’s proprietary coding-agent CLI; it is not open source. Anthropic documents npm installation and a native binary install path. The local installation reports **2.1.281**. The official release page I checked listed **2.1.278**, released September 19, so the local build is ahead of that page’s latest visible release; the public release page does not establish where 2.1.281 came from. The September 18 and 19 releases suggest frequent updates, but don’t establish a fixed cadence. [Setup and install](https://docs.anthropic.com/en/docs/claude-code/getting-started), [Claude Code releases](https://github.com/anthropics/claude-code/releases)

The local CLI help identified the program as `claude 2.1.281 (Claude Code)`. It offers `claude install` and an `install.sh` path for native installation, while Anthropic’s setup page also documents `npm install -g @anthropic-ai/claude-code`. [Setup and install](https://docs.anthropic.com/en/docs/claude-code/getting-started)

## 2. Auth and billing

Claude Code supports Anthropic Console/API-key authentication, Claude Pro/Max subscription login, and enterprise cloud providers including Bedrock and Vertex AI. Anthropic’s API-key path has separate API billing. [Setup and install](https://docs.anthropic.com/en/docs/claude-code/getting-started), [API authentication](https://platform.claude.com/docs/en/manage-claude/authentication)

Anthropic’s current guidance says subscription usage is intended for ordinary use of its own apps, including Claude Code; for third-party tools, its “preferred way” is API-key auth. It also says Anthropic may allow some third-party tools for subscribers who enabled usage credits, and may charge those tools against credits instead of subscription limits. Its SDK overview says third-party developers need prior approval to offer claude.ai login or subscription rate limits. [Claude account login and subscription guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account), [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview)

The Agent SDK usage notice, dated June 16, says Anthropic paused planned billing changes: for now, Agent SDK usage, `claude -p`, and third-party app usage still draw from subscription usage limits. That describes usage accounting; it does **not** explicitly approve every third-party wrapper’s use of subscription auth. [Agent SDK subscription update](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan)

**VIA-specific policy status: UNVERIFIED.** The docs do not clearly say whether a user’s own local program invoking their installed `claude -p` binary is treated as ordinary Claude Code use or third-party software. The safe documented option for software developers is API-key authentication; confirm approval before making subscription auth a VIA promise.

## 3. Headless CLI contract

The documented one-shot command is `claude -p "prompt"`. Anthropic documents piping input through stdin, print-mode controls, output formats, and nonzero exit status on failure. [Run Claude Code programmatically](https://code.claude.com/docs/en/headless)

Local `claude --help` confirmed these relevant flags and values:

- Input/output: `-p, --print`; `--input-format text|stream-json`; `--output-format text|json|stream-json`; `--verbose`; `--include-partial-messages`.
- Sessions: `--session-id <uuid>`; `-r, --resume [value]`; `-c, --continue`; `--fork-session`; `--no-session-persistence`.
- Agent/model: `--model <model>`; `--effort low|medium|high|xhigh|max`; `--system-prompt`; `--append-system-prompt`; their file variants; `--agents <json>`.
- Tools/permissions: `--tools`; `--allowedTools`; `--disallowedTools`; `--permission-mode manual|auto|acceptEdits|bypassPermissions|dontAsk|plan`; `--permission-prompts host|none`; `--dangerously-skip-permissions`; `--restricted`.
- Context/bounds: `--add-dir`; `--settings`; `--setting-sources`; `--strict-mcp-config`; `--mcp-config`; `--json-schema`; `--max-turns`; `--max-budget-usd`; `--bare`.
- `--bg`/`--background` exists, but is rejected in `-p` mode. [Headless CLI reference](https://code.claude.com/docs/en/cli-reference), [print-mode details](https://code.claude.com/docs/en/headless)

**Output and envelope.** `text` is plain text; `json` returns a single JSON result; `stream-json` returns newline-delimited events. The final stream event is `type: "result"` and carries the final response and session metadata. Structured output with `--json-schema` appears in `structured_output`; ordinary result text is in `result`. [Structured output and streaming](https://code.claude.com/docs/en/headless)

The JSON result includes `session_id`, `result`, `is_error`, `usage`, `total_cost_usd`, and per-model usage/cost metadata. Cost is a client-side estimate and can differ from the bill; resumed sessions report cumulative conversation totals. A stream consumer should inspect the terminal result event as well as process exit status. [Result and cost details](https://code.claude.com/docs/en/headless)

A stream includes `system/init` session metadata, assistant/user messages and tool blocks, and—when requested—`stream_event` partial deltas. The `result` event ends the run. `--verbose` is required for `stream-json`; `--include-partial-messages` enables partial chunks. [Streaming reference](https://code.claude.com/docs/en/headless)

Anthropic documents exit code 0 for success and nonzero for failures. Invalid flags are written to stderr before the run; in-run failures such as missing authentication may be emitted as the result on stdout. **Do not assume every error is only on stderr.** [Exit and error handling](https://code.claude.com/docs/en/headless)

Model and effort are selectable with `--model` and `--effort`. Full system-prompt replacement and additive prompt flags are available. Per-session `--agents` JSON can define subagent names, prompts, tools, model, effort, and permission settings. `--add-dir` grants additional directory context. [CLI reference](https://code.claude.com/docs/en/cli-reference), [subagent configuration](https://code.claude.com/docs/en/sub-agents)

Print mode starts with Manual permissions unless configured otherwise. `dontAsk` denies actions that would need a prompt; `acceptEdits` allows filesystem edits; `auto` uses an action classifier; bypass mode disables checks and is explicitly dangerous. `--restricted` narrows tools/settings and filesystem access, but it is not equivalent to OS sandboxing. Claude Code also has sandbox settings that enforce filesystem/network restrictions for shell commands and their children. [Headless permissions](https://code.claude.com/docs/en/headless), [permissions and sandboxing](https://code.claude.com/docs/en/permissions)

Non-interactive mode skips the workspace trust dialog, but without `--bare` it can still load project settings, hooks, MCP configuration and other ambient customizations. Anthropic notes that invalid settings may be silently ignored in print mode. `--bare` avoids many of those inputs, but uses API-key/provider credentials rather than subscription OAuth. [Headless trust and bare mode](https://code.claude.com/docs/en/headless)

Stdin works in print mode and is capped at 10 MB. For larger content, pass a file path. If stdin is unreadable, the CLI warns to stderr and continues with the command-line prompt. The local help describes `--input-format stream-json` as realtime streaming input. [Stdin and streaming](https://code.claude.com/docs/en/headless), [local CLI help](https://code.claude.com/docs/en/cli-usage)

`--max-turns` and `--max-budget-usd` are available bounds; I found no general wall-clock timeout flag in the CLI help or headless reference. A wrapper should impose its own deadline. [CLI bounds](https://code.claude.com/docs/en/cli-reference)

## 4. Session lifecycle in the CLI

Resume a session with `--resume <id>`; `--continue` selects the most recent conversation in the current directory. `--fork-session` creates a new session ID from resumed history. Sessions are persisted locally under `~/.claude/projects/<encoded-cwd>/*.jsonl` (or the configured `CLAUDE_CONFIG_DIR`). [CLI reference](https://code.claude.com/docs/en/cli-reference), [session storage and resume](https://code.claude.com/docs/en/agent-sdk/sessions)

**Invalid resume ID behavior: UNVERIFIED.** I did not find a current official statement saying whether an invalid ID always fails loudly instead of starting a fresh conversation. VIA should check the returned session ID and reject an unexpected new session.

For a running `-p` process, send SIGINT to end the turn. Anthropic says SIGTERM exits with code 143, leaves an in-progress turn unfinished, and records no result for it; the unfinished turn can continue when resumed. [Stopping a print-mode run](https://code.claude.com/docs/en/headless)

The local CLI also has interactive/background-session commands: `claude agents --json`, `attach`, `logs`, and `stop`. Background launch is incompatible with `-p`, so these do not provide a straightforward detached version of the one-shot contract. [CLI reference](https://code.claude.com/docs/en/cli-reference)

**Steering:** `--input-format stream-json` accepts realtime input, but I did not find an official CLI guarantee that this means mid-turn interruption rather than queued follow-up. Treat CLI steering semantics as **UNVERIFIED**. The ACP adapter explicitly advertises a separate steering extension (section 6).

## 5. Other programmatic surfaces

The Python and TypeScript Agent SDKs expose the agent loop directly: structured message objects, tool-permission callbacks, session APIs, interruption, model/permission changes and streamed input. They give an embedding application more control than parsing CLI output, but their auth and subscription use remain subject to the policy above. [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview), [SDK sessions](https://code.claude.com/docs/en/agent-sdk/sessions)

The CLI’s `claude mcp serve` is an MCP server mode, not a general Claude Code JSON-RPC or HTTP session-control server. The local help showed no general local app-server/HTTP control mode. Anthropic does document an experimental HTTP endpoint to fire a **cloud-hosted saved routine**; it returns a session ID but does not stream results or wait for completion. [Local CLI command help](https://code.claude.com/docs/en/cli-usage), [routine API](https://platform.claude.com/docs/en/api/claude-code/routines-fire)

## 6. ACP support

Claude Code itself does not expose ACP as a native CLI mode in its local help. The ACP agent is [`agentclientprotocol/claude-agent-acp`](https://github.com/agentclientprotocol/claude-agent-acp), an ACP wrapper around Anthropic’s Agent SDK. The ACP registry entry observed September 24 lists package version **0.81.2**, npm distribution `@agentclientprotocol/claude-agent-acp`, authors Anthropic, Zed Industries and JetBrains. The current repository `package.json` page showed **0.81.1**, author Zed Industries, Node ≥22, and Apache-2.0; the registry labels 0.81.2 **proprietary**. **License/version conflict: UNVERIFIED for the published 0.81.2 package.** [ACP registry entry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json), [repository package metadata](https://github.com/agentclientprotocol/claude-agent-acp/blob/main/package.json), [repository license](https://github.com/agentclientprotocol/claude-agent-acp/blob/main/LICENSE)

Launch through stdio, for example `npx -y @agentclientprotocol/claude-agent-acp@0.81.2`; the registry provides the package name/version, and the repo package declares the `claude-agent-acp` executable. The repo README describes this as an ACP adapter for the Claude Agent SDK. [ACP registry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json), [adapter README](https://github.com/agentclientprotocol/claude-agent-acp/blob/main/README.md)

**Capabilities in current `main` source:** `loadSession`, resume, close, delete, fork, list, additional directories, session prompt/cancel, image and embedded context, HTTP/SSE MCP, and provider auth/config. It advertises `usage_update` events with context usage and, at turn completion, cost. These are source observations from current `main`, not proof that every capability is present in registry release 0.81.2. [ACP initialization and capabilities](https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/acp-agent.ts)

The adapter advertises `_session/steering`. During a live turn it injects input into the SDK stream; output arrives through `session/update`, not as the steering call’s own result. If idle, the legacy default starts a detached turn and returns `startedNewTurn`; hosts can opt into `promptRequired`, meaning the text was not consumed and should be sent through normal `session/prompt`. The repo recorded a July 2026 issue where detached turns could lack an observable terminal response; current source retains the legacy default for compatibility. [Steering implementation](https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/acp-agent.ts), [issue #903](https://github.com/agentclientprotocol/claude-agent-acp/issues/903)

**`--hide-claude-auth`:** current source says this flag hides the Claude subscription login method and rejects turns that would bill a claude.ai subscription; API-key/Console and supported provider auth remain. Thus, a VIA deployment relying on a user’s subscription should not pass this flag. This is an adapter policy guard, not a way to enable subscription billing. [Flag implementation and comment](https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/hide-claude-auth.ts), [ACP auth methods](https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/acp-agent.ts)

The SDK docs separately say third-party products must not offer claude.ai login or rate limits unless previously approved. Anthropic’s Help Center guidance says it may allow certain third-party tools under usage-credit conditions. That makes ACP subscription use a policy-sensitive case; **whether this specific adapter/use has approval is UNVERIFIED**. [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview), [subscription policy](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account)

ACP supports model, effort and permission-mode configuration through session config options and session setup; the wrapper exposes additional directories and MCP servers. It is not a passthrough for every CLI flag. It presents standardized tool, permission, session and usage events, but abstracts the CLI’s raw JSON transcript and full per-model result fields. [ACP adapter source](https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/acp-agent.ts), [adapter README](https://github.com/agentclientprotocol/claude-agent-acp)

## 7. VIA verb matrix

| VIA verb | Best route | Support | Notes |
|---|---|---|---|
| `spawn` | CLI `-p` | Native | One-shot call with JSON result and exit status. |
| `resume` | CLI `--resume` | Native | Persisted session ID; invalid-ID behavior is **UNVERIFIED**. |
| `steer` | ACP extension | Native / partial | Live injection supported; idle behavior must be negotiated (`promptRequired` recommended). CLI stream input exists, but steer semantics are **UNVERIFIED**. |
| `cancel` | CLI process signal | Native | SIGINT ends turn; SIGTERM leaves it unfinished. ACP `session/cancel` is also supported. |
| `status` | ACP updates + VIA process record | Partial | ACP has live updates and saved-session listing, but no single universal status query in the CLI. |
| `result` | CLI JSON | Native | Final text, session ID, usage/cost metadata, error state and process exit code. ACP provides prompt completion and usage updates. |

CLI flag behavior is documented in Anthropic’s [headless guide](https://code.claude.com/docs/en/headless); ACP lifecycle and steering behavior is declared in the adapter’s [current source](https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/acp-agent.ts).

## 8. Pitfalls for automation

- **Ambient execution:** `-p` skips workspace trust prompts but still loads project hooks and MCP configuration unless `--bare` is used. This can run local integrations in an unattended job. `--bare` avoids them but also bypasses subscription login. [Headless mode](https://code.claude.com/docs/en/headless)
- **Cost accounting:** resumed JSON runs report whole-session totals, and cost is estimated client-side. Don’t interpret cumulative `total_cost_usd` as the latest turn’s spend. [Headless cost notes](https://code.claude.com/docs/en/headless)
- **Output drain:** if a stream consumer reads slowly, Claude Code waits for queued output to drain, up to 30 seconds. Keep the pipe drained and persist raw events. [Streaming reference](https://code.claude.com/docs/en/headless)
- **Hangs:** there is no documented general wall-clock timeout flag. Background workflows/subagents can keep print mode open; their idle wait defaults to 10 minutes. Bound the process externally. [Headless background tasks](https://code.claude.com/docs/en/headless)
- **Stdin:** input is capped at 10 MB; a disconnected or unreadable stdin falls back to the prompt argument with a warning. [Headless stdin](https://code.claude.com/docs/en/headless)
- **ACP steering:** the idle `startedNewTurn` compatibility path can detach output from an owning prompt result. Request `promptRequired` and retry as normal `session/prompt` when no turn is active. [ACP source](https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/acp-agent.ts)
- **Version drift:** Claude Code releases appear frequent, while ACP registry and repository package versions can briefly disagree. Pin adapter versions and contract-test the installed CLI version before relying on exact event fields. [Claude Code releases](https://github.com/anthropics/claude-code/releases), [ACP registry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json), [ACP package metadata](https://github.com/agentclientprotocol/claude-agent-acp/blob/main/package.json)

## 9. Recommendation

Use the **CLI adapter for VIA v0**: invoke the user’s installed `claude -p` process, capture JSON result plus exit status, use an explicit session ID, and persist VIA’s own run/status record. This best matches the handoff’s first scope—spawn, resume and result—and the existing CLI profile. Keep `steer` unsupported in v0 unless VIA uses the ACP wrapper and opts into its `promptRequired` idle contract.

Treat subscription use as a documented policy boundary: `claude -p` is an official programmatic CLI surface and currently draws on subscription usage, but Anthropic’s guidance does not clearly settle whether VIA as a third-party local wrapper is approved to rely on subscription auth. Do not advertise personal-subscription compatibility as guaranteed until Anthropic clarifies that specific case. [VIA handoff](../../../workstreams/handoff.md), [headless CLI](https://code.claude.com/docs/en/headless), [Anthropic subscription guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account), [Agent SDK policy note](https://code.claude.com/docs/en/agent-sdk/overview)

### Sources

- [Claude Code headless/programmatic docs](https://code.claude.com/docs/en/headless)
- [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference)
- [Claude Code SDK sessions](https://code.claude.com/docs/en/agent-sdk/sessions)
- [Claude account subscription guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account)
- [Agent SDK subscription usage update](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan)
- [Agent SDK overview and third-party login note](https://code.claude.com/docs/en/agent-sdk/overview)
- [Claude Code release page](https://github.com/anthropics/claude-code/releases)
- [ACP Claude Agent repository](https://github.com/agentclientprotocol/claude-agent-acp)
- [ACP Claude Agent source](https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/acp-agent.ts)
- [ACP registry release metadata](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json)
- [ACP steering issue #903](https://github.com/agentclientprotocol/claude-agent-acp/issues/903)