# VIA — handoff

> VIA exploration started in [MVPavan/coding-ritual](https://github.com/MVPavan/coding-ritual) (historical branch `dws-workflow`, pinned for links at `09cee1b`). The old worktree/submodule setup was removed; work now continues only in this standalone repository. Paths in the historical evidence and reuse sections below are parent-repo references unless explicitly described as local. Current `.repo-context/` and `docs/specs/` references belong to this repository.

Status: owner-paused S1 checkpoint with partial runtime code and tests;
integration acceptance and test compilation fixes remain open. Current epic:
`via-jm4`. After explicit owner resumption, recover from
[the Rust foundation handoff](rust-foundation/session-handoff.md).
Historical exploration began on branch `via`, cut from `dws-workflow` @ fe6a9f9
(2026-09-24), tracked then as `cr-w53s`.

## What VIA is

A single CLI to run an **explicitly configured agent + prompt** on **any coding
harness** and get a
**structured result** back —
so any agent can use sub-agents across harnesses and models, not only within
Claude or within Codex. The first release supports **Claude Code, Codex and
OpenCode**. VIA is also meant to become the **control plane** the
workflow interpreter's foreman calls instead of driving crews itself.

Owner's framing: there will be several top harnesses; some models live only in
some of them; people will use several at once. VIA is the cross-harness,
cross-model single interface, with one pattern for spawn / resume / steer /
cancel / close. Passthrough remains a later extension outside the first release.

First open question from the owner: **is it needed, and how much?** The
assessment below says yes, scoped tightly; the owner confirmed the three-harness
first-release scope on 2026-09-26.

## Assessment so far (2026-09-24 discussion)

Why it is useful:
1. One delegation surface. Today delegation is three different paths: the
   Claude Agent tool, raw `codex exec` / `codex exec resume` commands
   (`.repo-context/running-codex.md`), and the interpreter's graphs — each
   with its own flags, result shape and resume story.
2. Cross-harness, cross-model review is a daily practice already (Opus reviews
   GPT-6 Sol's work and vice versa), done by hand.
3. Model availability dictates the harness; caller policy may map a role to
   explicit model and harness parameters.
4. The foreman needs exactly "run role R on prompt P, return a structured
   result"; sharing one tested path beats two.

Why to keep it tight:
- Cost is per harness, forever: each vendor's headless contract (flags,
  output format, session ids, resume, sandbox) moves with its releases.
- Verbs are not uniform: spawn/resume map almost everywhere; steer differs
  (continuation turn vs mid-turn vs unsupported); cancel differs. VIA must
  declare per-adapter capability, never fake a verb.
- Passthrough is an escape hatch; results through it are unstructured.

## Properties: decided architecture

1. **Decided:** One headless VIA daemon per user owns agent processes, vendor
   connections and the Store. CLI, `via serve --stdio` and thin SDKs speak the
   public C1 VIA API to it over a user-only Unix socket. The CLI auto-starts
   it; it exits when idle and rejects client/daemon version mismatches.
2. **Decided:** One result envelope per turn. The current field definitions
   are in `docs/specs/via-api-v1.md` §5; denials and auto-declines are required.
3. **Decided:** Roles are caller policy. VIA takes explicit harness/model,
   effort, instructions, permission bound, cwd, output schema and namespaced
   vendor options. The model catalog maps model to harness.
4. **Decided:** `spawn`, `resume`, `steer`, `cancel`, `status`, `result` declare
   route-specific capabilities; unsupported verbs get named refusals. A
   session keeps one route and adapter version for life. Route fallback occurs
   only before submission.
5. **Decided:** The daemon alone writes the Store. A session is one resumable
   agent conversation; each prompt and its tool calls produce one turn. A
   launch receipt precedes submission; an unknown outcome is never resent
   automatically. SQLite remains behind a small storage interface.
6. **Decided:** A session starts with out-of-bound actions denied, not asked.
   L3 automatically declines vendor requests under a deadline. Denials and
   declines appear in the turn envelope. The permission bound carries over to
   every turn unchanged unless the caller explicitly sets a new one on resume
   through the handle. VIA revalidates it against the route and records it per
   turn; nothing changes it silently. External sandboxing for vendors without
   a native bound remains open.
7. **Decided:** Adapters are versioned and contract-tested against vendor
   versions. The exact outside-tested-range policy is resolved in S2 under
   C1 P13 / C2 A2. Passthrough is outside first-release acceptance.
8. **Decided:** `spawn --background` returns the session and turn address;
   `via wait` returns that turn's envelope for parallel sub-agents.

Rust 1.98.1, edition 2024, and a single static `via` binary are decided.
Vendor servers are preferred where available, otherwise vendor CLIs; native
ACP adds breadth. SDK and bridged ACP routes are not used for now. VIA starts
its own vendor servers and never attaches to or stops servers it did not start.
See `.repo-context/invariants.md` and `docs/brainstorms/routes-decision.md`.

First-release scope (owner, 2026-09-26): Claude Code, Codex and OpenCode;
the full C1 API, including background/wait, cancel, steer and close, with
truthful per-route capabilities. ACP, additional harnesses, passthrough,
thin SDK delivery and foreman integration are outside this release. See the
current handoff for the implementation sequence and proposed release checks.

## Evidence already gathered

- **Herdr 0.9.1 live probe** (isolated named session, 2026-09-24): starts 24
  agent kinds in terminal panes and forwards native args (Codex came up as
  GPT-6-Sol low in 4 s). But the first prompt was reported submitted with the
  agent idle and was silently lost (second identical prompt worked); results
  are screen text only (no structured result, exit code or session id); Claude
  blocked on its folder-trust dialog; needs a running server; validates
  nothing. Conclusion: good for watching agents, not for delegated work.
- Prior control-plane research, all REJECT as replacements, with borrow
  lists: `docs/research/codebases/comparison-herdr-orca.md` (Herdr, Orca,
  Traycer, T3 Code) and the per-repo docs under `docs/research/codebases/`.
  T3 Code's Claude result edge cases and "Codex resume must fail loud" are
  relevant adapter lessons.

## What already exists to build on

- Crew profiles (per-harness launch + parse): `workflow_interpreter/profiles/`
  (`claude.py`, `codex.py`, `opencode.py`, `registry.py`, `_base.py`).
  FROZEN, do not change: `profiles/codex_appserver*.py`, `contracts/codex.py`,
  `inspector/rpc_session.py`.
- Inspector (contains, launches, records exit, grades): `workflow_interpreter/inspector/`.
- Model catalog (auto-discovered, probed) and role bindings:
  `workflow_interpreter/foreman/model_catalog.py`, `config/roles.example.toml`,
  `config/claude-model-seed.json`; design in `docs/workstreams/model-catalog/design.md`.
- Ledger (parent-repo record): see `docs/workstreams/run-ledger/roadmap.md` and
  `docs/adr/0006-ledger-only-record-store.md`.
- agent-matrix skill (Claude spawn-parameter validation; its catalog
  `docs/research/codebases/subagent-runtimes/agent-matrix-values.yaml` is
  stale since 2026-07-23): `.claude/skills/agent-matrix/SKILL.md`,
  `tools/agent-matrix/agent_matrix.py`.
- Prompting guide for writing VIA's role prompts and the agent-matrix update:
  `harness_lifecycle/prompting-guides/GUIDE.md`.

## Brainstorm and research (2026-09-24)

Discussion record and all research: `docs/brainstorms/README.md`
(ACP, A2A, landscape, 20-harness CLI + ACP survey, Gemini → Antigravity,
subscription-access rule).

**Design rule — subscription access.** VIA invokes only the vendor's own
binary or official SDK in its documented headless/programmatic mode, and never
reads, copies or reuses vendor credentials; the user logs in through the
vendor's own tool. Owner decision: terms uncertainty does not block
development — build adapters properly and disable any a vendor turns out not to
permit. Record terms status per adapter.

## Remaining decisions

1. Decide whether VIA may wrap a vendor server in an external sandbox when
   the vendor has no native bound.
2. Decide whether shared vendor servers connect to the daemon over stdio or
   a Unix socket, which could allow rejoining after a daemon crash.
3. Resolve vendor-dependent C1/C2 decisions with probes in their owning slice.

Language, coding standard, testing policy and S1 scope are approved. The
relationship to agent-matrix and foreman integration are outside this release.

## Constraints carried over

- Stage explicit files only; no push without the owner's word; no machine-local
  absolute paths in committed files; throwaway work in `scratchpad/`.
- Verification commands: `.repo-context/verification.md`.
- Implementer/reviewer roster is the owner's choice per session.
