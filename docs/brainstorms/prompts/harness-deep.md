You are a read-only research explorer. Do not modify any files in the repo.

Context: we are designing VIA, a CLI that runs a role + prompt on any coding-agent harness
headlessly and returns a structured result envelope (status, final text, vendor session id,
exit code, usage/cost, logs), with uniform verbs spawn / resume / steer (inject input into a
running turn) / cancel / status / result. Each adapter declares which verbs it supports and
refuses the rest by name. Users run VIA on their own machine with their own subscriptions.
Background: docs/workstreams/handoff.md. Existing adapters (if any) in
workflow_interpreter/profiles/.

YOUR HARNESS: __NAME__
__EXTRA__

Research this ONE harness thoroughly, covering both its headless CLI and its ACP (Zed's Agent
Client Protocol, agentclientprotocol.com) support. Use live web search and PRIMARY sources only
(vendor docs, the vendor's repo source and release notes, the ACP registry at
github.com/agentclientprotocol/registry). Cite a URL for every factual claim with the version or
date seen. Mark anything unconfirmed as UNVERIFIED. Prefer reading source code over marketing.
If the CLI is installed locally, run its --help / subcommand help (read-only, no prompts that
cost money, no login changes) and report the exact installed version and flags.

Answer in this structure:
1. Identity: vendor, license (open source?), install method, current version and release cadence.
2. Auth and billing: subscription login vs API key vs other; is headless/programmatic use on a
   personal subscription allowed or restricted by the vendor's terms or docs (quote them)?
3. Headless CLI contract: the non-interactive command; output formats (text/json/stream-json
   events) with the event/result schema; where final text, session id, usage/tokens/cost, and
   errors appear; exit codes; model and reasoning-effort selection; system prompt / role /
   agent-profile injection; permission/approval modes and sandbox flags; working dir and extra
   dirs; trust dialogs or first-run prompts that can block headless runs; stdin behavior;
   structured-output (JSON schema) support; timeouts.
4. Session lifecycle in the CLI: resume by id, continue last, fork; where sessions are stored;
   whether resume fails loudly or silently starts fresh when the id is invalid; cancel
   (signals, clean shutdown); any mid-turn input/steering mechanism; background/async modes.
5. Other programmatic surfaces: SDKs, app-server / JSON-RPC server modes, MCP server mode,
   HTTP server mode — what each gives beyond the CLI.
6. ACP support: native or adapter (name repo, maintainer, license, version, activity); launch
   command; advertised capabilities (loadSession, resume, close, list, cancel, steering or other
   extensions, usage reporting); how model/effort/mode/sandbox are configured over ACP; auth
   over ACP; known bugs or open issues affecting spawn/resume/cancel/steer; what is lost vs
   the native CLI.
7. VIA verb matrix: a table of spawn / resume / steer / cancel / status / result, with the best
   route (CLI, ACP, SDK/server) and support level (native / partial / none) for each.
8. Pitfalls for automation (output drift, lost prompts, hangs, stdin, trust dialogs, rate
   limits, version churn) with evidence.
9. Recommendation: which route VIA should use for this harness in v0, and why.

Markdown, tables where useful, sources list at the end. Under ~300 lines.
