You are a read-only research explorer. Do not modify any files.

Context: VIA is a CLI that runs a role + prompt on any coding-agent harness headlessly and returns
a structured result (status, final text, session id, exit code, usage), with verbs spawn / resume
/ steer / cancel / status / result. Users run it with their own subscriptions. We need an accurate
survey row for each harness below. Use live web search and PRIMARY sources only (vendor docs,
vendor repos and source, release notes, npm/PyPI pages, the ACP registry at
github.com/agentclientprotocol/registry). Cite a URL for every fact with the version/date seen
(today is 2026-09-24). Mark anything unconfirmed as UNVERIFIED. Never guess.

Already verified from the ACP registry (agent.json), treat as given:
__GIVEN__

HARNESSES: __LIST__
__EXTRA__

For EACH harness give:
1. Identity: exact product name, vendor, repo, license/open source, install, latest version+date,
   the executable name.
2. Tier facts: is the vendor a MODEL PROVIDER whose harness mainly serves its own models (tier 1),
   or is it a multi-provider / bring-your-own-model AGGREGATOR harness (tier 2)? List which model
   providers it can use and whether users can use their own subscriptions (e.g. Claude/ChatGPT
   subscription login) or only API keys / the vendor's own plan.
3. Headless CLI: non-interactive command, output formats (text/json/stream-json), whether it
   reports session id, usage/cost and exit codes, model/effort selection flags, permission/sandbox
   flags, resume/continue by id, any mid-turn input, known blockers (trust dialogs, stdin, TTY).
4. Other programmatic surfaces: SDK, server/RPC mode, MCP server mode.
5. ACP: native or adapter (who maintains), launch command, capabilities (load/resume, cancel,
   steering extensions, usage), maturity (stable/preview/experimental), known bugs.
6. Activity/maturity signals: release cadence, stars, last release; is it actively maintained?
7. One-line VIA verdict: worth an adapter in v0/v1/later/never, and best route (CLI vs ACP vs SDK).

Output: first ONE summary table (columns: harness, vendor, tier 1/2, OSS?, headless CLI + JSON?,
resume?, ACP route + maturity, subscription login?, activity, VIA verdict), then a short section per
harness with citations, then sources. Under ~250 lines.
