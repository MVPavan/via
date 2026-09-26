# routes-decision.md review r1: Claude Fable 5.1 high

Run: `claude -p --model claude-fable-5-1 --effort high`, read-only tools, 2026-09-25 (Claude Code 2.1.282; runtime reported model claude-fable-5-1, 47 turns). Brief: shared with the Astra review. Verbatim output below.

**Verdict: ACCEPT WITH CHANGES.** "No bridged ACP" and "no SDK routes" follow from the evidence; "shared vendor server by default" is under-argued because the doc never evaluates the per-run server alternative, never states the target concurrency, and understates the daemon conflict.

## Findings

**1. [major] The strongest alternative is missing: vendor-server protocol with per-run lifetime.** `access-methods.md:160-164` already names it ("RPC protocol with CLI-style lifetime"). A `codex app-server` spawned per run gives steer, interrupt, `sandboxPolicy`, structured events, zero layers, no supervisor, no blast radius, and no invariant-5 change. Its n=1 cost equals the CLI's (`analysis.md:90-91`; 113 MB vs 125 MB, `runs/full/summary.md:14,21`). What sharing buys is only R2, and R2 is defined "at the concurrency a workflow crew uses" (line 21) without a number. At n=5 the saving for Codex is ~440 MB (`summary.md:15,22`). Only OpenCode has a case that survives at small n (23×, db-lock), and OpenCode is blocked on an external sandbox anyway. Fix: state the crew's target n; add the per-run app-server column to §6; scope "shared" to OpenCode, or justify it for Codex at the stated n.

**2. [major] The daemon conflict is understated.** §8 frames invariant 5 as "one worker process per run", but the sharper half is "No daemon" (`invariants.md:27-28`). R1 says runs outlive the caller; a shared server that later `via` invocations attach to needs discovery (socket), a lease, crash takeover and idle shutdown. That is a VIA-owned long-lived process in all but name. Fix: say explicitly that the shared-server model requires either a supervisor-owned detached server (a daemon by another name) or per-run servers, and put that choice in front of the owner rather than "server lifetime" alone.

**3. [major] CPU claim cherry-picks OpenCode.** Line 77-78 says servers "flatten the start-up CPU burst" and cites only `opencode serve` at 1.5 cores. Codex app-server peaked at 25.2 cores at n=32 (`runs/full/summary.md:24`), and `analysis.md:107` says so ("except codex-server at n=32, 25 cores peak"). Fix: state that only OpenCode's server flattens the burst; Codex's does not.

**4. [major] R3 ("VIA adds no permission layer") is violated by the native ACP route as written.** Under ACP the agent calls `session/request_permission` and VIA must answer, with a deadline, or the turn hangs (`access-methods.md:380-383`, `:422-425`; spec `protocol__v1__prompt-turn.md:255`). Decision 5 covers only flags and server params. Fix: add an explicit ACP answer policy (e.g. answer from the declared bound, never treat answers as the bound, deadline-then-cancel) and note that for Claude stream-json `can_use_tool` only fires if `--permission-prompt-tool stdio` is passed, which VIA must not pass.

**5. [major] "Resubmit" on server loss conflicts with the no-automatic-resend rule.** §8 offers "fail or resubmit". `access-methods.md:407-410` (launch receipt; "never resends an ambiguous prompt automatically") and `analysis.md:118-119` (retry safety after partial work untested) argue that resubmit is unsafe once a tool has run. Fix: constrain resubmit to sessions with no observed tool activity, or drop it.

**6. [minor] Memory table mixes two benchmark runs.** CLI columns come from the full run (Claude 4,440; Codex 3,384) while acpx columns come from the acpx run, whose native baselines were 4,534 and 3,808 (`runs/acpx/summary.md:44,50`). Ratios barely move, but say which run each column uses.

**7. [minor] "~40 control-request types" is 35.** The `SDKControlRequestInner` union at `sdk.d.ts:4280` has 35 members. Write "35".

**8. [minor] Native-ACP R2 "✓ shared per connection" is unmeasured.** acpx spawned one agent per session (`capabilities.md:16-17`), so no multi-session-per-process ACP memory figure exists. Mark as inferred from the spec, not measured.

**9. [minor] "as OpenCode does today" (line 146) has no referent in this repo.** `workflow_interpreter/` does not exist here; `access-methods.md:277-279` cites it as an existing profile elsewhere. Say where it lives.

**10. [minor] Ambiguous cross-references.** "the §6 layer-2 checks" (line 135) means `access-methods.md` §6, but this document has its own §6. "(4)", "(1)", "(3)" on lines 27-28 and "Invariant 5" are numbers in `invariants.md`. "Workflow crew" and "inspector" are undefined. Qualify each reference with the file.

**11. [minor] "Exactly what the Agent SDK drives, minus the host" (line 131) overstates.** The flags match `run.log:2`, but the SDK also changes defaults (line 107-108 admits this) and the bridge passes `settingSources` (`sysprompt/claude.jsonl:3`). Say "the same stream-json control protocol", not "exactly".

**12. [minor] OpenCode `serve` is an HTTP listener.** Bind address, auth secret and CORS (`access-methods.md:349-351`) are not mentioned as a cost. Also untested: VIA's server sharing SQLite with a user's own interactive `opencode`, which is the same cross-process contention that caused "database is locked".

**13. [minor] Per-turn usage sentence omits that session-level `usage_update` is stable** (`announcements__session-usage-stabilized.md`, June 5 2026). Only the per-turn RFD is draft (`rfds__end-turn-token-usage.md`).

## Verified claims

Memory figures (`analysis.md:37-46`, `acpx-analysis.md:27-34`); 5 of 184 db-lock (`analysis.md:67`); bridge doubling and idle trees (`acpx-analysis.md:34-37`); permission bypass and codex-acp "read-only" (`capabilities.md:82-89`); wedged session (`capabilities.md:93-96`); SDK spawn line and process reuse (`run.log:2-10`); SDK 0.3.257 and 215 MB binary (`claude-agent-sdk-linux-x64/`); installed `claude` 2.1.282 (`analysis.md:11`); v2 draft dated July 20 2026, `session/load` and fs/terminal removed (`protocol__v2__migration.md:14,42,53-54`); "MAY" (`prompt-turn.md:255`); system prompt reached Claude only (`sysprompt/claude.jsonl:19` ends with BANANA; codex and opencode replies do not, error logs empty); no system-prompt/structured-output/budget fields in v1 protocol pages; app-server "experimental" (`codex.md:62`, reported from installed help); Anthropic June pause (`claude.md:17`, secondary source).

## Unverified

- Bundled binary is "Claude Code 2.1.257": package metadata gives only SDK 0.3.257; the binary was not run.
- SDK "uses a minimal system prompt" when no preset is set: not stated in `sdk.d.ts`; the demo (`demo.mjs:13`) set the preset explicitly, so the live run did not test it.
- `codex app-server generate-json-schema` exists: could not run the command here.
- Copilot runtime JSON-RPC usable directly: the doc marks it "to verify"; `access-methods.md:221` says "not recommended directly".

## Add or cut

Add: target concurrency for R2; the per-run app-server option; an explicit daemon-or-not statement; ACP permission answer policy; Codex ToS ambiguity (`codex.md:107`) in §9 alongside Anthropic; server discovery portability (Unix sockets, Windows later per invariant 7); a §10 item on Codex thread-level cwd/config isolation within one server. Cut: the "flatten the CPU burst" generalisation; "exactly" on line 131.