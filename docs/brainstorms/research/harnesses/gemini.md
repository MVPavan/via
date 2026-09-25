# Gemini CLI research for VIA

Research date: 2026-09-24. I checked the installed CLI’s help without submitting a prompt or changing login state. No repository files were changed.

## 1. Identity

- **Vendor:** Google; source repository is `google-gemini/gemini-cli`. The CLI is Apache-2.0 licensed. [Gemini CLI v0.58.0 README](https://github.com/google-gemini/gemini-cli/tree/v0.58.0)
- **Install:** Global npm package `@google/gemini-cli`; the project also documents `npx`, Homebrew, and MacPorts. Locally it is installed globally as `@google/gemini-cli@0.58.0`. [v0.58.0 README](https://github.com/google-gemini/gemini-cli/tree/v0.58.0)
- **Version:** `gemini --version` returned **0.58.0**. As of this research date, the GitHub releases page showed **0.61.0** as the latest stable release and **0.62.0-preview.0** plus nightly builds; the ACP registry listed Gemini CLI **0.61.0**. [GitHub releases, checked 2026-09-24](https://github.com/google-gemini/gemini-cli/releases), [ACP registry, checked 2026-09-24](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json)
- **Cadence:** Google documents nightly releases and weekly preview/stable promotions, normally Tuesday; the release notes describe weekly minor versions and intervening patch releases. [Release process at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/releases.md)

## 2. Auth and billing

- The CLI supports Google-account sign-in to Gemini Code Assist, Gemini Developer API keys, and Vertex AI credentials. Its v0.58.0 docs specifically recommend **API key or Vertex AI for headless mode** when no credential is already cached. [Authentication guide at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/get-started/authentication.mdx)
- **Current consumer subscription constraint:** Google’s June 18, 2026 announcement says Gemini CLI stopped serving free individual, Google AI Pro, and Google AI Ultra accounts; enterprise Gemini Code Assist licenses and API-key authentication remain unaffected. So a personal Google AI subscription is not a viable current Gemini CLI billing route. [Google announcement, 2026-06-18](https://github.com/google-gemini/gemini-cli/discussions/28017)
- **Third-party OAuth constraint:** Google’s current CLI terms page says: “Directly accessing the services powering Gemini CLI … using third-party software, tools, or services … is a violation of applicable terms and policies.” It says this may result in suspension or termination. API-key and Vertex use are covered by their respective service terms. [Google CLI terms and auth methods](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md)
- **VIA implication:** Even though ACP is an official CLI mode, the terms page does not clearly state that a third-party VIA client may use cached Google-account OAuth through ACP. **UNVERIFIED:** whether Google treats a client using the documented ACP interface as an exception. VIA should require API key or Vertex for this adapter unless Google clarifies otherwise.

## 3. Headless CLI contract

**Installed command and flags.** `gemini --help` reported “Defaults to interactive mode” and `-p/--prompt` as non-interactive mode. It also says prompt text is appended to stdin input. The installed 0.58.0 flags are:

```text
-d, --debug
-m, --model
-p, --prompt
-i, --prompt-interactive
    --skip-trust
-w, --worktree
-s, --sandbox
-y, --yolo
    --approval-mode [default|auto_edit|yolo|plan]
    --policy
    --admin-policy
    --acp
    --experimental-acp
    --allowed-mcp-server-names
    --allowed-tools [deprecated]
-e, --extensions
-l, --list-extensions
-r, --resume
    --session-file
    --session-id
    --list-sessions
    --delete-session
    --include-directories
    --screen-reader
-o, --output-format [text|json|stream-json]
    --raw-output
    --accept-raw-output-risk
-v, --version
-h, --help
```

The subcommand help showed `gemini mcp` for managing configured MCP servers, `gemini extensions` for extension management, and `gemini skills` for skill management. Those commands do not launch a prompt. **No `--timeout`, `--effort`, or `--role` flag appears in installed help.**

- **Formats and result data:** `--output-format json` emits one object: `session_id`, `response`, `stats`, optional `error`, and optional `warnings`. The v0.58.0 source type defines `stats` as session metrics. `stream-json` emits JSONL events: `init` (`session_id`, `model`); `message` (`role`, `content`, optional `delta`); `tool_use` (`tool_name`, `tool_id`, `parameters`); `tool_result` (`tool_id`, status, output/error); `error` (severity/message); and `result` (success/error plus aggregate stats). [v0.58.0 output types](https://raw.githubusercontent.com/google-gemini/gemini-cli/v0.58.0/packages/core/src/output/types.ts), [headless docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/headless.md)
- **Usage and cost:** Stats include token counts and latency; streaming stats include total/input/output tokens, cached/input breakdown, duration, tool-call count, and per-model counts. **No monetary cost field is documented in this envelope.** VIA must calculate cost itself if the applicable API pricing and token classes are known. [v0.58.0 output types](https://raw.githubusercontent.com/google-gemini/gemini-cli/v0.58.0/packages/core/src/output/types.ts)
- **Exit codes:** v0.58.0 headless docs list 0 success, 1 general/API error, 42 input error, and 53 turn-limit exceeded. [Headless docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/headless.md)
- **Model and effort:** `-m/--model` selects a model. There is no installed effort flag; the model config supports thinking settings in configuration, so any effort mapping would need version-specific config, not a uniform CLI flag. [v0.58.0 configuration reference](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/reference/configuration.md)
- **Role/system prompt:** `GEMINI_SYSTEM_MD` can replace the built-in system prompt with a Markdown file; `GEMINI.md` files supply hierarchical project instructions. The system-prompt override is a full replacement, so VIA must preserve required built-in instructions if it uses one. A role can also be included in the prompt, but the CLI has no role-selector flag. [v0.58.0 configuration reference](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/reference/configuration.md), [system prompt guide](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/system-prompt.md)
- **Permissions/sandbox:** `--approval-mode` accepts `default`, `auto_edit`, `yolo`, and `plan`; `--yolo` accepts all actions; `--sandbox` enables sandboxing; `--policy` and `--admin-policy` load policy files or directories. Help describes `default` as prompting, which can block automation awaiting approval. [v0.58.0 configuration reference](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/reference/configuration.md)
- **Workspace and trust:** Working directory is the process working directory; `--include-directories` adds workspace dirs (the v0.58 docs describe a maximum of five). `--skip-trust` trusts the current workspace for that session. First-run authentication still requires credentials to be preconfigured for headless use. [Installed 0.58.0 help](https://github.com/google-gemini/gemini-cli), [authentication guide at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/get-started/authentication.mdx)
- **Stdin:** `-p` prompt is appended to stdin content if present. Avoid inherited open stdin in a worker process unless VIA intentionally supplies input. `-i` is interactive and the config docs say it cannot be used with piped stdin. [Installed 0.58.0 help](https://github.com/google-gemini/gemini-cli), [configuration reference at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/reference/configuration.md)
- **Structured output:** `--output-format json` structures the *CLI result envelope*. It does not expose a JSON Schema argument for constraining the model’s answer. The model’s final answer can still be ordinary text. [v0.58.0 headless docs](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/headless.md), [installed 0.58.0 help](https://github.com/google-gemini/gemini-cli)

## 4. Session lifecycle in the CLI

- **Resume:** `--resume <UUID|index|latest>` resumes a saved session; `--resume` alone means latest. `--list-sessions` lists project sessions. History is stored under `~/.gemini/tmp/<project_hash>/chats/` and is project-specific. Combine `--resume <id>` with `-p <new prompt>` for a headless follow-up. [Session management at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/session-management.md)
- **Continue/fork:** `latest` is a resume selector, not a separate “continue” verb. I found no CLI fork-session flag; manual tagged checkpoints are documented, but they are save/resume checkpoints rather than a fork API. [Session management at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/session-management.md)
- **Invalid ID:** **UNVERIFIED** whether an invalid resume ID always fails loudly rather than selecting a fallback; docs explain valid ID forms but do not define the invalid-ID behavior.
- **Cancel:** There is no dedicated headless cancel flag or request channel. VIA can signal/terminate the process, but the exact signal, final JSONL event, and session persistence behavior are **UNVERIFIED** for v0.58.0. ACP has an explicit cancel request.
- **Steer:** No documented way to inject input into a running headless turn. A later `-p` is a separate process/turn. Background/async process supervision is left to VIA; the CLI help does not expose a background job mode. [Headless docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/headless.md)

## 5. Other programmatic surfaces

- **SDK:** Google shipped `@google/gemini-cli-sdk` at v0.58.0. Its README shows an in-process `GeminiCliAgent`, injected instructions, streamed output, and an `AbortController` signal. This avoids parsing a CLI subprocess, but it is a separate programming interface whose session/policy feature parity needs checking before VIA depends on it. [SDK README at v0.58.0](https://raw.githubusercontent.com/google-gemini/gemini-cli/v0.58.0/packages/sdk/README.md)
- **A2A server package:** `packages/a2a-server` is explicitly marked experimental and under active development in v0.58.0. It is an Agent-to-Agent server surface, not ACP or the one-shot headless CLI; the A2A development-tool RFC describes streaming task events and permission interactions. [A2A server README at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/packages/a2a-server/README.md), [A2A development-tool RFC](https://github.com/google-gemini/gemini-cli/blob/main/packages/a2a-server/development-extension-rfc.md)
- **MCP:** Gemini CLI is an MCP client that can connect to configured servers; `gemini mcp` manages those connections. I found no vendor-provided MCP server mode for using Gemini CLI as the server. [ACP mode docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/acp-mode.md), [installed subcommand help](https://github.com/google-gemini/gemini-cli)
- **HTTP/app server:** No general CLI app-server or HTTP server mode was found in the installed help or v0.58.0 docs. A2A is the separate server package.

## 6. ACP support

- **Implementation:** Native in Gemini CLI (`gemini --acp`), not a third-party adapter. The ACP registry lists Google as author, Apache-2.0, and launches `@google/gemini-cli@0.61.0 --acp` as of 2026-09-24. [Gemini CLI ACP docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/acp-mode.md), [ACP registry entry, checked 2026-09-24](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json)
- **Transport and verbs:** JSON-RPC 2.0 over stdio; advertised methods include initialize/authenticate, new/load session, prompt, cancel, set session mode, and unstable set session model. **No session list, close, or delete method is listed.** [ACP mode docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/acp-mode.md)
- **Capabilities/config:** `loadSession` is advertised; the client can provide filesystem/MCP proxy capabilities. Approval mode is configurable through session mode; the model can be changed through the explicitly unstable model method. There is no documented standard ACP effort selector. [ACP mode docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/acp-mode.md)
- **Auth:** ACP authenticates through Gemini CLI’s Google-service auth methods. Use preconfigured API-key or Vertex credentials for unattended VIA runs; do not assume OAuth-backed personal subscriptions are available or permitted through VIA. [Authentication guide at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/get-started/authentication.mdx), [Google CLI terms](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md)
- **Known session issue:** Google issue #27913 reports `session/load` advertised but failing to restore model conversation memory on CLI 0.46.0. The report is historical, and whether it affects 0.58.0 is **UNVERIFIED**. [Issue #27913](https://github.com/google-gemini/gemini-cli/issues/27913)
- **Other lifecycle gap:** Google’s ACP docs enumerate supported operations without a list/close/delete operation; an issue requesting close/delete describes the same gap. [ACP docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/acp-mode.md), [issue #24811](https://github.com/google-gemini/gemini-cli/issues/24811)
- **Lost vs CLI:** ACP gives live structured notifications and protocol cancellation, but the documented surface does not include CLI session listing/deletion or the headless JSON result/stats contract. ACP is therefore better for control, while headless CLI has the stronger documented final envelope. Usage reporting in ACP is **UNVERIFIED** from the v0.58.0 docs.

## 7. VIA verb matrix

| VIA verb | Best route | Support | Notes |
|---|---|---|---|
| `spawn` | Headless CLI (`-p … -o json` or `stream-json`) | **Native** | One process/turn; capture session ID, result, stats, exit code. |
| `resume` | Headless CLI (`-r <id> -p …`) | **Partial** | CLI supports it; invalid-ID behavior is unverified. ACP `loadSession` exists, but has a reported restoration defect on 0.46.0. |
| `steer` | None | **None** | No documented mid-turn input injection in CLI or ACP. |
| `cancel` | ACP (`session/cancel`) | **Native** | ACP explicitly supports cancelling an ongoing prompt. CLI process termination is only partial. |
| `status` | ACP event stream / VIA process state | **Partial** | ACP streams progress; no documented status-query method. Headless status is inferable from process liveness and stream events. |
| `result` | Headless CLI JSON/stream JSON | **Native** | ACP supplies final prompt completion, but no documented CLI-equivalent usage/stats envelope. |

CLI and ACP capabilities are documented in [headless](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/headless.md) and [ACP mode](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/acp-mode.md) references.

## 8. Automation pitfalls

- **OAuth policy and personal accounts:** Personal free/Pro/Ultra access ended for Gemini CLI on June 18, 2026; Google’s terms warn against third-party tools accessing CLI services via Google-account OAuth. [Google announcement](https://github.com/google-gemini/gemini-cli/discussions/28017), [terms](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md)
- **Approval hangs:** `default` mode prompts for tool approval; choose a deliberate noninteractive approval/policy setup or the process may wait for input. [Configuration reference at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/reference/configuration.md)
- **Trust/auth prompts:** `--skip-trust` exists, but it does not authenticate the user. Headless auth requires cached credentials, API key, or Vertex configuration. [Installed help](https://github.com/google-gemini/gemini-cli), [authentication guide at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/get-started/authentication.mdx)
- **Stdin surprises:** `-p` appends to stdin content; ensure VIA sets stdin deliberately. [Installed help](https://github.com/google-gemini/gemini-cli)
- **Output/schema drift:** The event envelope is versioned source code, and Google releases weekly with nightly changes. Pin/test the CLI version and parse known events defensively. [Output types at v0.58.0](https://raw.githubusercontent.com/google-gemini/gemini-cli/v0.58.0/packages/core/src/output/types.ts), [release cadence](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/releases.md)
- **Rate limits:** Google documents API `429 Resource exhausted` as an exceeded request limit; subscription quota and API quota are distinct routes. [Google CLI FAQ](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/faq.md)

## 9. Recommendation

For VIA v0, use the **headless CLI with `--output-format stream-json`**, capture stdout/stderr and the process exit code, and expose only `spawn`, `resume`, and `result`. This best fits VIA’s one-turn-per-process model and returns a documented session ID, final response, token stats, and exit code. Require **API-key or Vertex auth**, not consumer subscription OAuth, because of Google’s current account availability and third-party OAuth terms. Treat `steer` as unsupported; add ACP later if VIA needs long-lived sessions and protocol cancellation, after validating the current `loadSession` behavior and auth policy. [Headless docs at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/headless.md), [Google terms](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md)

Repository context: the VIA handoff says no adapter is built, and `workflow_interpreter/profiles/` has Claude, Codex, and OpenCode profiles but no Gemini profile. The worktree also had pre-existing staged and untracked VIA-related changes; I left them untouched.

### Sources

- [Gemini CLI repository and v0.58.0 source](https://github.com/google-gemini/gemini-cli/tree/v0.58.0) — versioned source/docs.
- [Headless mode](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/headless.md), [session management](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/session-management.md), [configuration](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/reference/configuration.md).
- [Headless output types](https://raw.githubusercontent.com/google-gemini/gemini-cli/v0.58.0/packages/core/src/output/types.ts) — v0.58.0 source.
- [ACP mode](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/cli/acp-mode.md), [ACP registry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json) — registry checked 2026-09-24.
- [Authentication at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/get-started/authentication.mdx), [current CLI terms](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md), [June 18, 2026 account announcement](https://github.com/google-gemini/gemini-cli/discussions/28017).
- [Release process at v0.58.0](https://github.com/google-gemini/gemini-cli/blob/v0.58.0/docs/releases.md), [live releases](https://github.com/google-gemini/gemini-cli/releases) — checked 2026-09-24.