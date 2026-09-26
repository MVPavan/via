# routes-decision.md review r1: consolidation

Reviewers, independent, same brief: GPT-6 Astra high
(`routes-decision-astra-r1.md`) and Claude Fable 5.1 high
(`routes-decision-fable-r1.md`). Consolidated by the orchestrator (Claude
Opus 5.5); no separate judge. Both verdicts: **ACCEPT WITH CHANGES**.

## Agreed by both

| # | Finding | Severity | Fix |
|---|---|---|---|
| A | Shared vendor server is under-argued. The per-run server (vendor protocol, CLI-style lifetime; `access-methods.md` §1.4) is missing as an option; target concurrency for R2 is not stated; ownership, discovery, lease/fencing, idle shutdown and config-compatible pooling are unspecified. | major | Add per-run server and bounded pool as options; state the crew's target n; put the topology choice to the owner. |
| B | The conflict is with "No daemon", not only "one worker per run". A shared server that later invocations attach to is a VIA-owned long-lived process. | major | Say so explicitly; the owner chooses daemon-like supervisor vs per-run servers. |
| C | "Fail or resubmit" on server loss contradicts the no-automatic-resend rule once a tool has run. | major | Drop resubmit, or limit it to sessions with no observed tool activity. |
| D | Permission handling is inconsistent with R3. Native ACP requires VIA to answer `session/request_permission`; "MAY" means optional consultation, not absent native enforcement; the OpenCode external sandbox is a VIA-supplied bound. | major | Evaluate enforcement per adapter; add an ACP answer policy (answer from the declared bound, deadline then cancel); decide whether external isolation is in scope. |
| E | CPU claim generalises from OpenCode: Codex app-server peaked at 25.2 cores at n=32 (CLI 21.9). | major/minor | Only OpenCode's server flattens the burst. |
| F | "~40 control-request types" is 35 (`SDKControlRequestInner`). | minor | Write 35. |
| G | "As OpenCode does today" has no referent in this repo; cross-references ("§6", invariant numbers) and terms ("workflow crew", "inspector") are ambiguous. | minor | Qualify each with its file; define terms. |

## Raised by one reviewer

Astra:
- The SDK rejection mixes caller lifetime with supervisor lifetime: a
  VIA-supervised SDK sidecar could outlive the caller. "Adds no capability"
  contradicts the Cursor exception. Reframe as a packaging/directness
  trade-off and state whether R4 forbids sidecars.
- The 2× bridged-ACP cost is acpx's topology (client + bridge, one tree per
  session); VIA would replace the client. Label as measured for acpx only.
- Omitted adverse evidence: OpenCode server abort showed 3 of 16 off-target
  replies (causation not established); kill-during-tool cleanup untested.
  Gate shared OpenCode on paired fault probes.
- Topology table over-uniform (Pi one session per process, OpenCode is HTTP,
  Pi/OMP SDKs in-process). Scope unclear (does server-first replace layer 0?
  fallback only before spawn, per invariant 2). Needs native Linux/macOS
  validation, per-route terms status, and a committed sanitized benchmark
  summary.

Fable:
- The memory table mixes two benchmark runs; name the run per column.
- Claude stream-json: VIA must not pass `--permission-prompt-tool stdio`,
  or `can_use_tool` requests come to VIA.
- "Exactly what the Agent SDK drives" overstates; say "the same stream-json
  control protocol".
- `opencode serve` is an HTTP listener (bind, auth, CORS); VIA's server may
  contend on SQLite with the user's own interactive `opencode`.
- ACP session-level `usage_update` is stable (June 2026); only per-turn usage
  is a draft.
- Add Codex ToS ambiguity, Windows server discovery, and Codex per-thread
  cwd/config isolation to the open items.
- Native-ACP "shared per connection" is inferred from the spec, not measured.

## Disagreement

Fable says "no SDK routes" follows from the evidence; Astra accepts the
conclusion but says the stated reasons overreach. Not a conflict on the
decision, only on how strongly it is argued. Astra's framing is better
supported.

## Claims the reviewers could not verify, now resolved by the orchestrator

- Bundled binary 2.1.257: verified (`claude --version` on the bundled
  binary, 2026-09-25); size 215,469,464 bytes.
- `codex app-server generate-json-schema`: listed by `codex app-server
  --help` (codex-cli 0.156.1) as experimental.
- SDK minimal default system prompt: from Anthropic's "Modifying system
  prompts" docs; not tested live.
- Anthropic's June 2026 pause: from the Claude Help Center article and press;
  secondary sources.
- Copilot runtime JSON-RPC directly: still unverified; `access-methods.md`
  marks it "not recommended directly".

## Owner decisions surfaced

1. Default for Codex (and OpenCode): per-run server, supervisor-owned shared
   server, or bounded pool; and the target concurrency that decides it.
2. Whether "No daemon" is dropped or kept (it decides option 1).
3. Whether R4 forbids SDK sidecars outright, or only by default.
4. Whether VIA may supply an external sandbox (OpenCode) despite "no VIA
   permission layer".
