You are a read-only research explorer. Do not modify any files.

Context: VIA is a CLI + library that runs a role + prompt on many coding-agent harnesses and returns
a structured result (verbs spawn / resume / steer / cancel / status / result). Users run VIA locally
with their OWN accounts. We are deciding whether VIA adapters should use vendor SDKs instead of (or
in addition to) the headless CLIs. Background: docs/brainstorms/README.md and
docs/brainstorms/access-methods.md (read §1 and §3 only).

Use live web search and PRIMARY sources only: vendor docs, SDK repos and SOURCE CODE, package
registries (PyPI/npm/crates/Go), release notes, vendor terms/help-center pages. Read source code to
answer "what does the SDK actually run underneath". Cite a URL for every fact with version/date seen
(today 2026-09-24). Mark anything unconfirmed as UNVERIFIED. Quote terms verbatim. Never guess.
Locally installed CLIs you may run with --help only: claude 2.1.281, codex 0.156.1, gemini 0.58.0,
opencode 1.18.21. You may inspect installed Python/npm packages if present, but do not install
anything and do not run paid prompts.

SDKS TO COVER: OpenCode SDK (@opencode-ai/sdk); Cline SDK (@cline/sdk); Pi coding-agent SDK (earendil-works/pi, programmatic API); Oh My Pi SDK; Kimi / Moonshot Agent SDK (MoonshotAI/kimi-agent-sdk); Qwen Code SDK (if any); Factory Droid SDK (if any); Amp SDK (if any, e.g. @sourcegraph/amp-sdk); Goose (any library API); Hermes Agent (any library API); Kilo (any SDK)
For each, first establish whether an SDK exists at all. For multi-provider harnesses, the subscription question is per upstream provider: which provider subscriptions (Claude Pro/Max, ChatGPT Plus/Pro, Copilot, SuperGrok, etc.) can be used through that SDK, and what the upstream provider's terms say. Note that Anthropic reportedly prohibits Claude subscription use in third-party harnesses (OpenCode docs) — verify from Anthropic sources.

For EACH SDK answer:
1. Identity: package names per language, repo, license, version + date, maturity label (GA/beta/
   preview), maintainer (vendor or community).
2. Architecture — what runs underneath: does it spawn the vendor CLI binary (which one, bundled or
   system, which protocol: stream-json / app-server JSON-RPC / other), run an in-process agent loop,
   or call a cloud API? Cite the source file.
3. Equivalence to the CLI: is it the SAME agent as the CLI? Compare: built-in tools (read/write/edit/
   bash/search/web), system prompt, project instructions (CLAUDE.md/AGENTS.md), skills, slash
   commands, hooks, subagents, MCP, plugins, permissions/sandbox, sessions (resume/fork, stored where,
   shared with the CLI?), model/effort selection, settings files. What is missing or different by
   default (e.g. settings not loaded unless opted in)? Can it do real coding work in a repo like the CLI?
4. Subscription vs API: which auth methods work (personal subscription login, API key, cloud
   providers, enterprise)? Does the SDK pick up the CLI's existing subscription login? What do the
   vendor's docs/terms say about using a PERSONAL subscription through the SDK — (a) for your own
   local automation, (b) for a third-party product offered to others? Quote them. Note any billing
   difference (e.g. SDK usage billed differently from interactive use, usage credits).
5. Lifecycle for VIA verbs: spawn, resume, steer (mid-turn input), cancel/interrupt, status, result;
   streaming events; usage/cost reporting; structured output; concurrency (many sessions per process?).
6. Languages available and parity between them.
7. Pitfalls: version coupling with the CLI, bundled binaries, hidden child processes, memory, auth
   gotchas, known bugs (cite issues).
8. Verdict for VIA: use SDK, CLI or both for this harness, and why.

Output: first a summary table (SDK | languages | runs underneath | same agent as CLI? | personal
subscription works? | subscription allowed for own use / for 3rd-party product | steer | cancel |
usage | maturity | VIA verdict), then one section per SDK, then sources. Under ~350 lines.
