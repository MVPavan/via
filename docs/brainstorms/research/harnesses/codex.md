## 1. Identity

**Research date: 2026-09-24.** Codex CLI is OpenAI’s local coding-agent CLI. The installed binary reports **`codex-cli 0.156.1`**; I confirmed that with `codex --version`. The public CLI source is Apache-2.0 licensed. OpenAI’s current install docs list the standalone installer, npm, and Homebrew routes; the documented npm package is `@openai/codex`. [Codex source and license](https://github.com/openai/codex), [Codex install docs](https://developers.openai.com/codex/cli)

There is no published fixed release cadence. The release history and recent package bumps show frequent updates: the ACP package changelog records Codex `0.154.0` on Sep 15, `0.155.0`/`0.155.1` on Sep 22, and `0.156.1` on Sep 23. Treat the CLI and its contracts as fast-moving. [ACP adapter changelog](https://github.com/agentclientprotocol/codex-acp/blob/main/CHANGELOG.md), [Codex releases](https://github.com/openai/codex/releases)

## 2. Auth and billing

Codex supports ChatGPT sign-in and API-key authentication. OpenAI says signing in to Codex with ChatGPT uses the ChatGPT plan’s usage and billing; using an API key uses API pricing. [OpenAI plan and Codex billing](https://help.openai.com/en/articles/20001275-chatgpt-work-and-codex), [Codex authentication docs](https://help.openai.com/en/articles/11381614-api-codex-cli-and-sign-in-with-chatgpt)

There is an important terms boundary for VIA. OpenAI documents `codex exec` for repeatable scripting and CI, but the individual-user Terms of Use say: **“Automatically or programmatically extract data or Output”** is prohibited. The terms page does not explain how that clause applies to collecting output through the official Codex CLI. So headless CLI invocation is documented, but whether a VIA wrapper that programmatically processes the result is permitted under an individual ChatGPT plan is **UNVERIFIED** from the sources reviewed. The API route has its own terms; OpenAI’s Services Agreement restricts extracting data “other than as permitted through the Services.” [Codex CLI automation docs](https://developers.openai.com/codex/cli), [Terms of Use](https://openai.com/policies/row-terms-of-use/revisions/2024-10-23/), [OpenAI Services Agreement](https://cdn.openai.com/osa/openai-services-agreement.pdf)

## 3. Headless CLI contract

### Command and output

Use **`codex exec [OPTIONS] [PROMPT]`**. The installed help says that if no prompt is supplied—or `-` is used—the prompt is read from stdin. If stdin is piped and a prompt argument is also supplied, stdin is appended as a `<stdin>` block.

- Default output: final assistant response on stdout; activity goes to stderr.
- `--json`: JSON Lines events on stdout. This is the installed CLI’s structured event mode; its help does not advertise a `--stream-json` flag or a general `--output-format` selector.
- `-o, --output-last-message <FILE>`: also writes the final message to a file.
- `--output-schema <FILE>`: requests a final response matching a JSON Schema. In the event stream, the response is still an `agent_message` item containing text—JSON text when structured output is requested.
- The source event schema defines `thread.started` (with `thread_id`), `turn.started`, `turn.completed` (with usage), `turn.failed`, top-level `error`, and `item.started`/`item.updated`/`item.completed`. Items include `agent_message`, `command_execution`, `file_change`, MCP calls, reasoning, and other activity types. The usage fields include input, cached input, cache-write input, output, and reasoning-output tokens. **No dollar cost is included.** [Local `codex exec --help` and version output](https://github.com/openai/codex/blob/main/codex-rs/exec/src/cli.rs), [event schema source](https://github.com/openai/codex/blob/main/codex-rs/exec/src/exec_events.rs)

The process exit code and terminal event should both be captured. The source defines terminal failure events, but I found no stable, documented table of all `codex exec` exit codes. Don’t infer success solely from a final-looking text response or from an item-level error; an upstream report describes a successful turn and exit 0 that also contained an item-level event-error about dropped events. [Event schema](https://github.com/openai/codex/blob/main/codex-rs/exec/src/exec_events.rs), [reported stream-lag case](https://github.com/openai/codex/issues/19689)

### Model, role, workspace, and permissions

The installed `exec` help exposes `-m/--model`, `-p/--profile`, and repeated `-c/--config key=value`. Reasoning effort is configured through `model_reasoning_effort`, for example `-c 'model_reasoning_effort="high"'`. There is no dedicated `--system-prompt` flag in the help. Codex configuration has an `instructions` field and a `developer_instructions` field; VIA can put a role in the prompt or configure developer instructions, subject to the selected auth/account’s supported settings. Project `AGENTS.md` files are another source of instructions and are ambient workspace context, so isolate or account for them when reproducibility matters. [Installed CLI help](https://github.com/openai/codex/blob/main/codex-rs/exec/src/cli.rs), [Codex config source](https://github.com/openai/codex/blob/main/codex-rs/config/src/config_toml.rs)

The installed launch flags include:

- Workspace: `-C/--cd`, `--skip-git-repo-check`, `--worktree`
- Additional writable directories: repeatable `--add-dir`
- Sandbox: `-s/--sandbox read-only|workspace-write|danger-full-access`
- Approval: `-a/--ask-for-approval on-request|never`, `--approve-for-me`
- Bypass: `--dangerously-bypass-approvals-and-sandbox`
- Config hygiene: `--ignore-user-config`, `--ignore-rules`, `--strict-config`
- Persistence: `--ephemeral`

`exec resume` has a narrower help surface: it exposes `--config`, model, `--output-schema`, `--json`, `-o`, and `--ephemeral`, but does **not** list `-C`, `--sandbox`, or `--add-dir`. It inherits the thread’s workspace and sandbox/config state; do not assume launch flags can be repeated unchanged on resume. The existing VIA CLI profile explicitly handles that difference. [Installed resume help](https://github.com/openai/codex/blob/main/codex-rs/exec/src/cli.rs), [VIA’s read-only Codex profile](<https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/profiles/codex.py>)

**Timeouts:** no `codex exec` wall-clock timeout flag appeared in the installed help. VIA needs an outer watchdog and must record whether termination came from Codex or the watchdog.

**First run and trust:** authenticate before unattended operation. A first login or browser-based ChatGPT auth flow is interactive. Codex also uses project trust/configuration; exact headless behavior for an untrusted project under installed `0.156.1` was not probed here, so treat a trust/auth dialog as a possible blocker until verified in VIA’s launch environment. [Codex quickstart](https://developers.openai.com/codex/cli), [Codex config source](https://github.com/openai/codex/blob/main/codex-rs/config/src/config_toml.rs)

**Stdin:** your supplied observation says `codex exec` hung when stdin was non-TTY and remained open. That is consistent with the installed help’s stdin behavior. VIA should pass the prompt as an argument or explicitly use `-` with a finite, closed input stream; close or redirect inherited stdin rather than leaving an open pipe. This specific hang is **user-observed**, not independently reproduced in this read-only research.

## 4. Session lifecycle in the CLI

`codex exec resume <SESSION_ID> [PROMPT]` resumes by UUID or thread name. `--last` selects the newest recorded session. `codex exec fork <SESSION_ID> [PROMPT]` creates a fork. The TypeScript Codex SDK README says sessions are persisted under `~/.codex/sessions`; `--ephemeral` is intended not to persist session files. [Installed resume/fork help](https://github.com/openai/codex/blob/main/codex-rs/exec/src/cli.rs), [Codex SDK README](https://github.com/openai/codex/blob/main/sdk/typescript/README.md)

**Invalid IDs:** exact `0.156.1` behavior was not locally tested. An upstream issue reports `exec resume` surfacing a missing-thread error, while its proposed improvement would fall back to a new thread. That makes “invalid ID always fails loudly” unsafe to assume without a version-pinned probe; VIA should verify the resumed `thread_id` equals the requested ID. [Upstream invalid-resume issue](https://github.com/openai/codex/issues/22064)

There is no dedicated `codex exec cancel` command. For a foreground invocation, VIA can send a process signal and record the resulting process status, but that is process termination rather than a graceful agent-level cancel contract. `codex queue --thread … --message …` exists, but it queues a message; it is not a documented mid-turn steer API. `exec` itself is foreground; running it under a process supervisor is caller-managed backgrounding. [Installed top-level and queue help](https://github.com/openai/codex/blob/main/codex-rs/exec/src/cli.rs)

## 5. Other programmatic surfaces

| Surface | What it adds | Limits for VIA |
|---|---|---|
| **Codex SDK (`@openai/codex-sdk`)** | TypeScript wrapper around the CLI’s JSONL stream; start/resume threads, run turns, stream events, and request schema-constrained output. | It wraps the CLI rather than exposing a separate hosted service. Use outer process/session supervision for deadlines and cancellation. [SDK README](https://github.com/openai/codex/blob/main/sdk/typescript/README.md) |
| **`codex app-server`** | Long-lived JSON-RPC server over stdio, Unix socket, or WebSocket. Protocol methods include thread start/resume/read/list and turn start/steer/interrupt, with notifications for streaming status, messages, and usage. | Marked experimental in the installed help. It is a more complete control API but adds protocol/version coupling. [App-server docs](https://developers.openai.com/codex/app-server), [app-server source](https://github.com/openai/codex/tree/main/codex-rs/app-server) |
| **`codex mcp`** | Manages external MCP servers used *by* Codex. | The installed `codex --help` lists `mcp`, but not `mcp-server`; `codex mcp-server --help` returned root help. No Codex MCP server mode was advertised by this installed CLI. [Installed CLI help](https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs) |
| **HTTP server** | **UNVERIFIED / not found:** no Codex CLI HTTP agent-server mode appeared in installed help or reviewed docs. App-server supports WebSocket, Unix socket, and stdio. | OpenAI’s API is an HTTP inference surface, not a local Codex CLI session server. |

The separate experimental `codex exec-server` is a remote process-execution service, not the conversation/session API VIA needs. [Installed exec-server help](https://github.com/openai/codex/blob/main/codex-rs/exec-server/src/main.rs)

## 6. ACP support

Codex CLI does **not** advertise native ACP in its own help. The supported route is an adapter: [`agentclientprotocol/codex-acp`](https://github.com/agentclientprotocol/codex-acp), replacing the archived `zed-industries/codex-acp`. The ACP Registry snapshot retrieved on Sep 24 lists **`@agentclientprotocol/codex-acp` 1.13.1**, Apache-2.0, with OpenAI, JetBrains, and Zed as authors. Its changelog dates 1.13.1 to Sep 23 and says it bundles Codex `0.156.1`. The published command is `npx -y @agentclientprotocol/codex-acp`. [ACP Registry entry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json), [adapter README](https://github.com/agentclientprotocol/codex-acp), [changelog](https://github.com/agentclientprotocol/codex-acp/blob/main/CHANGELOG.md), [archived predecessor](https://github.com/zed-industries/codex-acp)

The adapter starts Codex App Server and translates ACP to Codex operations. Its source advertises `loadSession`, session resume/list/close, and a `_meta.steering.supported` extension; the ACP base protocol supplies session prompt/cancel. The adapter also supports session forks and, after capability negotiation, native subagent sessions. This is substantially richer than `exec`, but steering is an extension clients must negotiate rather than an assumption about all ACP agents. [Adapter README](https://github.com/agentclientprotocol/codex-acp), [adapter initialize and method source](https://github.com/agentclientprotocol/codex-acp/blob/main/src/CodexAcpServer.ts)

Model, reasoning effort, fast mode, approval mode, and sandbox mode are configurable through ACP session config options. Additional directories and MCP servers can also be supplied. Auth methods include ChatGPT login, `CODEX_API_KEY`/`OPENAI_API_KEY`, and an opt-in custom OpenAI-compatible gateway. `NO_BROWSER=1` hides browser-based ChatGPT auth. [Adapter README](https://github.com/agentclientprotocol/codex-acp)

Activity is high and the surface is still evolving: recent releases landed Sep 22–23, with frequent Codex dependency bumps. Open issues include usage reporting that may undercount a turn by exposing only the final model request’s usage, a steering idle-race case, and stale history replay after rollback. An auth issue reported expired ChatGPT credentials being surfaced as an internal error instead of `AuthRequired`. Treat these as reported adapter issues, not proof every current run is affected. [Adapter changelog](https://github.com/agentclientprotocol/codex-acp/blob/main/CHANGELOG.md), [usage issue #447](https://github.com/agentclientprotocol/codex-acp/issues/447), [steering issue #440](https://github.com/agentclientprotocol/codex-acp/issues/440), [history issue #355](https://github.com/agentclientprotocol/codex-acp/issues/355), [auth issue #495](https://github.com/agentclientprotocol/codex-acp/issues/495)

Compared with direct CLI use, ACP gives normalized session/update messages and lifecycle calls, but it does not guarantee access to every CLI option or event. In particular, the adapter’s reported usage may not equal the CLI’s per-turn usage, and dollar cost remains unavailable. Pin the adapter package and negotiate capabilities instead of depending on a moving `main` branch. [Adapter source](https://github.com/agentclientprotocol/codex-acp/blob/main/src/CodexAcpServer.ts), [usage issue #447](https://github.com/agentclientprotocol/codex-acp/issues/447)

## 7. VIA verb matrix

| VIA verb | `codex exec` | App Server | ACP adapter | Best v0 route |
|---|---|---|---|---|
| **spawn** | **Native**: `exec --json`; capture first `thread_id`. | **Native**: `thread/start` + `turn/start`. | **Native**: `session/new` + `session/prompt`. | CLI |
| **resume** | **Native**: `exec resume ID`; verify returned ID. | **Native**: `thread/resume` + `turn/start`. | **Native**: `session/load` or `session/resume`. | CLI |
| **steer** | **Partial**: `codex queue` queues input; no documented mid-turn steer. | **Native**: `turn/steer`. | **Partial/extension**: `_session/steering`, advertised by this adapter. | Refuse in CLI v0; consider ACP later |
| **cancel** | **Partial**: terminate the process; no agent-level cancel contract. | **Native**: `turn/interrupt`. | **Native**: `session/cancel`; close is separate. | Refuse in CLI v0; consider ACP later |
| **status** | **Partial**: process state only; no query-by-thread CLI command. | **Native**: thread/turn status notifications and reads. | **Partial**: updates and session listing; not a uniform standalone status call. | Process supervisor in v0 |
| **result** | **Native**: final text, JSONL terminal event, token usage, exit code. | **Native**: turn events/read APIs. | **Native, with usage caveat**: prompt response and streamed updates. | CLI |

The “best v0 route” follows the handoff’s proposed initial scope of spawn/resume/result and builds on the existing CLI profile. [VIA handoff](<../../../workstreams/handoff.md>), [existing profile](<https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/profiles/codex.py>)

## 8. Automation pitfalls

- **Stdin hangs:** the supplied observation and installed help both make open stdin a concern. Close it or pass a bounded prompt input.
- **No enforced timeout:** use a VIA-owned deadline and preserve the event log when terminating.
- **Stream interpretation:** consume JSONL as complete records; require a terminal turn event and process exit. A top-level `error` or `turn.failed` is significant, while item-level error events may be nonfatal.
- **Resume identity:** missing-ID behavior is version-sensitive in upstream reports. Verify returned thread ID and refuse any unexpected fresh thread.
- **Auth/trust prompts:** authenticate and establish workspace trust before unattended operation; exact first-run behavior for this version/environment remains unverified.
- **Event loss/hangs:** upstream reports include a review path that emitted an error then hung without a terminal turn event, and a separate case of dropped app-server events during long runs. They concern older CLI versions, so relevance to `0.156.1` is **UNVERIFIED**, but justify an external timeout and log retention. [Hang report](https://github.com/openai/codex/issues/41984), [event-drop report](https://github.com/openai/codex/issues/38234)
- **Version churn:** the CLI and ACP adapter are updated frequently. Pin versions and parse additive/unknown JSONL events defensively. [ACP changelog](https://github.com/agentclientprotocol/codex-acp/blob/main/CHANGELOG.md)

## 9. Recommendation

For VIA v0, use **`codex exec --json`** for spawn and **`codex exec resume <id> --json`** for resume. It is officially documented for repeatable workflows, produces a structured event stream with the session ID and token usage, and matches the existing VIA CLI profile. Return `cancel`, mid-turn `steer`, and live `status` as named unsupported capabilities on that route; process termination and queued follow-up are not equivalent substitutes.

Keep App Server/ACP as the next route if VIA needs full lifecycle control. App Server exposes turn steering and interruption directly; `codex-acp` maps richer lifecycle and config behavior into ACP, but is experimental-by-dependency and has active usage, auth, history, and steering issues. For personal-plan auth, resolve the Terms of Use ambiguity before making automated result collection a default. [Codex automation docs](https://developers.openai.com/codex/cli), [App Server docs](https://developers.openai.com/codex/app-server), [ACP adapter](https://github.com/agentclientprotocol/codex-acp), [OpenAI Terms of Use](https://openai.com/policies/row-terms-of-use/revisions/2024-10-23/)

### Sources

Primary sources linked above: OpenAI Codex docs and source; OpenAI Help Center and terms; the live [ACP registry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json); and the adapter’s source, README, changelog, and issue tracker. The installed help snapshot was captured locally with `codex-cli 0.156.1`; no model run or paid prompt was used.

Earlier workspace memory recorded `0.154.0`; the local version check and the Sep 23 adapter changelog supersede that value.

