# Pi coding agent research

**Source snapshot:** 24 September 2026. This report distinguishes the Pi coding agent from other projects named Pi, and uses the active upstream repository and ACP Registry entry. Because neither `pi` nor `pi-acp` is installed here, I could not run their help commands or verify behavior locally.

## 1. Identity

- **Project:** Pi coding agent, by Mario Zechner / badlogic. The former `github.com/badlogic/pi-mono` URL now redirects to the active `earendil-works/pi` repository; its coding-agent package is `@earendil-works/pi-coding-agent`, with the executable `pi`. This is the intended project, not an unrelated Pi-named tool. ([upstream repository](https://github.com/earendil-works/pi), checked 24 Sep 2026)
- **License:** MIT, open source. ([upstream repository](https://github.com/earendil-works/pi), checked 24 Sep 2026)
- **Install:** `npm install -g @earendil-works/pi-coding-agent`, a standalone release binary, or build from source. ([coding-agent README](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md), checked 24 Sep 2026)
- **Current version:** v0.87.1, released 22 Sep 2026. Recent releases are frequent, but the release list alone does not establish a guaranteed cadence. ([release page](https://github.com/earendil-works/pi/releases), v0.87.1, 22 Sep 2026)
- **Local install:** **UNVERIFIED / not installed.** `command -v pi` and `command -v pi-acp` returned no executable; consequently no installed version or flags are available.

## 2. Auth and billing

Pi supports provider API keys and OAuth login flows; credentials are stored in `~/.pi/agent/auth.json`, and can be supplied through environment variables. The documented subscription logins include ChatGPT Plus/Pro for Codex, Claude Pro/Max, GitHub Copilot, and other providers. ([provider authentication docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/providers.md), checked 24 Sep 2026)

Pi’s current provider docs say:

> “Anthropic subscription auth is active for Claude Pro/Max accounts. Third-party harness usage draws from extra usage and is billed per token, not against Claude plan limits.”

For OpenAI, those docs list ChatGPT Plus or Pro as required for Codex, and say OpenAI officially endorsed Pi through “Codex for OSS.” That does **not** establish that every third-party or commercial Pi workload is covered by the endorsement. ([provider docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/providers.md), checked 24 Sep 2026)

OpenAI’s individual Terms of Use prohibit “Automatically or programmatically extract[ing] data or Output.” OpenAI separately documents sign-in with ChatGPT for the **Codex CLI**. Whether use of Pi’s separate Codex OAuth provider is permitted for every personal-subscription use case is **UNVERIFIED** in the vendor terms and docs reviewed here; VIA should not represent it as blanket authorization. ([OpenAI Terms of Use](https://openai.com/policies/terms-of-use/), checked 24 Sep 2026; [Codex CLI sign-in documentation](https://help.openai.com/en/articles/11381614-api-codex-cli-and-sign-in-with-chatgpt), updated 2026)

## 3. Headless CLI contract

Pi documents four interfaces: interactive terminal UI, print, JSON event stream, and RPC. For one-shot VIA runs, the simplest command is:

```sh
pi --print --model <provider/model> --thinking <level> \
  --append-system-prompt <role-text-or-file> "<prompt>"
```

`--print` writes the final assistant text to stdout and exits. `--mode json` emits JSONL event records. `--mode rpc` is a long-running JSONL control interface; there is no documented `--output-format stream-json` flag. With terminal stdin/stdout, bare `pi` opens the UI; when streams are redirected, Pi uses print mode unless JSON or RPC was selected. In print mode, piped stdin is prepended to the prompt. ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md), checked 24 Sep 2026)

| Need | Where it appears |
|---|---|
| Final text | Print mode stdout; in JSON events, inspect completed assistant `message_end` / `turn_end` messages. |
| Session ID | RPC `get_state` or `get_session_stats`; the CLI JSON event stream is not documented as a complete run envelope. |
| Usage and cost | JSON `message_update.usage` contains input/output/cache/total tokens and cost. RPC `get_session_stats` reports session totals and cost. Streaming usage may remain zero until completion if the provider does not report usage while streaming. |
| Errors | RPC command rejection appears in a response with `success:false`; provider failures after prompt acceptance appear in message/events. Stderr carries diagnostics. |
| Exit code | The process exit code is available to the parent process. A stable mapping from model/provider failure to process exit code is **UNVERIFIED** in the documented contract. |

Sources: [JSON event stream](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/json.md), [RPC protocol](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md), [RPC commands and stats](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc-commands.md), all checked 24 Sep 2026.

- **Model and reasoning:** `--provider`, `--model` (exact or fuzzy ID/name, optionally `provider/id:<thinking>`), and `--thinking` (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; clamped to model support). ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md), checked 24 Sep 2026)
- **Role / system prompt:** `--system-prompt` replaces the default; repeatable `--append-system-prompt` appends text or a file. Pi also loads context files such as `AGENTS.md` and `CLAUDE.md` unless disabled. ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md), checked 24 Sep 2026)
- **Tools and permissions:** `--tools`, `--exclude-tools`, `--no-builtin-tools`, and `--no-tools` let a caller restrict available tools. Pi has no built-in permission popups or sandbox; enabled tools run with the OS permissions of the Pi process. ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md); [security docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/security.md), checked 24 Sep 2026)
- **Working directory / extra paths:** The process working directory controls project configuration, resources, relative paths, and session grouping. `@path` includes files in the initial prompt. The CLI has no documented general “extra directories” sandbox flag. ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md), checked 24 Sep 2026)
- **Trust / first run:** Headless print, JSON, and RPC modes cannot display Pi’s project-trust prompt. With default trust setting `ask`, protected project settings/resources are skipped unless a CLI override or saved/extension decision applies. Use `--approve` or `--no-approve` explicitly for automated runs. ([security docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/security.md), checked 24 Sep 2026)
- **Structured output:** No CLI JSON-schema flag is documented. The upstream example registers a terminating tool with a TypeBox schema, which can yield typed tool arguments, but is an extension pattern—not a general CLI guarantee of schema-constrained assistant text. ([example extension](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/examples/extensions/structured-output.ts), checked 24 Sep 2026)
- **Timeouts:** No CLI timeout option was found in the current CLI reference. VIA should enforce a parent-side deadline and cancellation policy. ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md), checked 24 Sep 2026)

## 4. Session lifecycle in the CLI

Pi persists JSONL sessions under `~/.pi/agent/sessions/`, grouped by working directory; `--session-dir` overrides storage. `--continue` continues the latest session for the current project, `--session <path|id>` opens a chosen session, and `--fork <path|id>` forks it. **Do not use `--session-id` as a strict resume check:** the documented flag creates that ID if absent. Invalid-ID behavior for `--session` is **UNVERIFIED** in the CLI reference. ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md), checked 24 Sep 2026)

The CLI has no documented standalone cancel command. RPC provides `abort` for the current operation, `steer` for an in-progress run, `follow_up` after the run, `get_state`, `get_messages`, and `get_session_stats`. Steering arrives after the active assistant turn completes its tool calls and before the next model call; it is not immediate interruption. RPC can shut down cleanly when the client closes child stdin. Signal-specific shutdown guarantees are **UNVERIFIED**. ([RPC command reference](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc-commands.md); [RPC protocol](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md), checked 24 Sep 2026)

## 5. Other programmatic surfaces

- **SDK:** `@earendil-works/pi-coding-agent` embeds Pi in Node.js/Bun and exposes session creation, prompts, event subscriptions, system-prompt/resource loading, model/runtime configuration, and tools. It is the richest surface, but requires a Node.js/Bun integration. ([SDK docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/sdk.md), checked 24 Sep 2026)
- **RPC:** Long-running child process, JSONL commands/events, state and session queries, prompt steering, abort, and statistics; the best process-isolated surface for VIA. ([RPC docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md), checked 24 Sep 2026)
- **MCP / HTTP / app-server:** Pi’s README says it has no built-in MCP server or app-server mode; its documented process integration is RPC over stdin/stdout. No built-in HTTP server mode was found in the current coding-agent docs. ([coding-agent README](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md), checked 24 Sep 2026)

## 6. ACP support

Pi does **not** expose native ACP in the current coding-agent docs. The ACP Registry entry is **pi ACP**, package `pi-acp@0.0.33`, maintained at [`svkozak/pi-acp`](https://github.com/svkozak/pi-acp) by Sergii Kozak, MIT licensed. It launches as `npx -y pi-acp` or `pi-acp`; it speaks ACP JSON-RPC over stdio and starts `pi --mode rpc`. The registry snapshot has version 0.0.33; the adapter describes itself as MVP-style and expects minor breaking changes. ([ACP Registry entry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json), checked 24 Sep 2026; [adapter README](https://github.com/svkozak/pi-acp/blob/main/README.md), checked 24 Sep 2026; [package metadata](https://github.com/svkozak/pi-acp/blob/main/package.json), checked 24 Sep 2026)

- **Capabilities:** `loadSession` is advertised; session listing and deletion appear as unstable capabilities in the current source. Pi sessions persist in Pi’s session directory, with an adapter mapping file to reattach ACP IDs. The adapter advertises no stable `session/resume` or `session/close` capability in the inspected initialization code; `session/load` is the documented resume/history path. ([adapter source](https://github.com/svkozak/pi-acp/blob/main/src/acp/agent.ts), checked 24 Sep 2026; [adapter README](https://github.com/svkozak/pi-acp/blob/main/README.md), checked 24 Sep 2026)
- **Cancel and steer:** ACP `session/cancel` is implemented. The adapter README exposes `/steering` as a setting/control for Pi’s queue mode, but does not document an ACP mid-turn steer operation; do not equate that setting with VIA steer. ([adapter source](https://github.com/svkozak/pi-acp/blob/main/src/acp/agent.ts); [adapter README](https://github.com/svkozak/pi-acp/blob/main/README.md), checked 24 Sep 2026)
- **Model / effort / mode:** ACP session responses include model/configuration choices; model selection is through the ACP client’s selector, while `/thinking` maps to the client’s mode/thinking selector. The adapter does not supply Pi-style ask/architect/code modes. ([adapter README](https://github.com/svkozak/pi-acp/blob/main/README.md), checked 24 Sep 2026)
- **Auth:** The adapter supports ACP Registry terminal auth, which launches Pi in a terminal for login. VIA should assume credentials are preconfigured; the terminal flow is not headless. ([adapter README](https://github.com/svkozak/pi-acp/blob/main/README.md), checked 24 Sep 2026)
- **Sandbox / filesystem / usage:** The adapter runs Pi locally; it does not delegate filesystem or terminal operations to the ACP client. It reports context-window occupancy with `usage_update`, not the native RPC’s complete token-and-cost statistics. ([adapter README](https://github.com/svkozak/pi-acp/blob/main/README.md), checked 24 Sep 2026)
- **Known issue:** Open issue #82 reports that if the Pi child exits while `pi-acp` remains alive, a subsequent prompt can return a successful empty `end_turn` with no content or error; reported on adapter 0.0.31 / Pi 0.80.7, 16 Jul 2026. Treat as an issue report, not proof that 0.0.33 still has the bug. ([issue #82](https://github.com/svkozak/pi-acp/issues/82), opened 16 Jul 2026)
- **Other known issue:** Open issue #84 reports ACP `session/prompt` hanging after extension slash commands that do not start an agent loop; reported with adapter 0.0.31 / Pi 0.80.8, 19 Jul 2026. ([issue #84](https://github.com/svkozak/pi-acp/issues/84), opened 19 Jul 2026)

## 7. VIA verb matrix

“Native” means directly available from Pi’s documented programmatic surface; “partial” means an adapter or process wrapper can approximate it with caveats.

| VIA verb | Best route | Support | Notes |
|---|---|---:|---|
| `spawn` | RPC | Native | Start `pi --mode rpc`, configure cwd/model/prompt; capture events and process result. |
| `resume` | RPC | Native | Start RPC with `--session <path/id>` or `--continue`; validate ID separately because `--session-id` creates missing sessions. |
| `steer` | RPC | Native | Supported queue semantics: delivered after current assistant turn and its tool calls. |
| `cancel` | RPC | Native | `abort` cancels current operation; hard process termination is VIA-owned. |
| `status` | RPC | Native | `get_state` gives streaming state, model, session ID, message count, and queue counts. |
| `result` | RPC | Native | Read events/messages and `get_session_stats`; final status/exit-code policy still belongs to VIA. |

Sources: [CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md), [RPC protocol](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md), [RPC commands](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc-commands.md), checked 24 Sep 2026. **ACP is partial for this matrix:** it provides spawn, cancel, status-like events, and load-based resume, but does not document the native steer operation or complete usage/cost result.

## 8. Automation pitfalls

- **Output drift:** Print mode gives final text, while JSON and RPC are event protocols. Pin Pi versions and parse the documented event shapes; latest release v0.87.1 fixed invalid `--mode` values that previously could be silently ignored. ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md); [v0.87.1 notes](https://github.com/earendil-works/pi/releases), 22 Sep 2026)
- **Lost / premature result:** In RPC, a successful `prompt` response means accepted or queued, not completed. Wait for `agent_settled`, not merely `agent_end`; queued steering, retries, or compaction may continue afterward. ([RPC docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md); [JSON event docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/json.md), checked 24 Sep 2026)
- **Stdin and framing:** Print mode prepends piped stdin to the prompt. RPC requires strict LF-delimited JSON records; keep reading stdout, or Pi can stall on backpressure. ([CLI docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md); [RPC docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md), checked 24 Sep 2026)
- **Trust and permissions:** Headless runs cannot ask about project trust; untrusted project settings/resources may be skipped unless VIA explicitly chooses `--approve`. Enabled tools otherwise use the process’s OS privileges. ([security docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/security.md), checked 24 Sep 2026)
- **Version churn:** The upstream release page shows frequent 0.8x releases, including v0.87.0 and v0.87.1 in the days before this snapshot. Pin the CLI and contract-test adapters. ([release page](https://github.com/earendil-works/pi/releases), 21–22 Sep 2026)
- **ACP child failure:** The adapter’s reported empty-success-on-dead-child issue could turn a failed turn into a lost prompt; monitor events and enforce completion deadlines. ([issue #82](https://github.com/svkozak/pi-acp/issues/82), 16 Jul 2026)

## 9. Recommendation

Use **Pi RPC directly** for VIA v0. It offers the best fit for the requested verbs: process isolation, JSONL events, session/state/stat queries, true mid-run queued steering, and abort. Spawn it with explicit trust/tool policy, await `agent_settled`, collect `get_session_stats`, and enforce VIA-owned deadlines and process exit handling. Keep ACP as a separately declared partial route for ACP ecosystem integration; its current registered adapter adds a translation layer and loses native steering and full token/cost reporting. ([Pi RPC docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md); [ACP Registry entry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json), checked 24 Sep 2026)