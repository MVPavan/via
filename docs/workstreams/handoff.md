# VIA — handoff

> Plain-text paths such as `workflow_interpreter/…`, `docs/adr/…`, `docs/research/…`, `.repo-context/…` and `scratchpad/…` refer to the parent repository [MVPavan/coding-ritual](https://github.com/MVPavan/coding-ritual) (branch `dws-workflow`, pinned for links at `09cee1b`), where VIA's exploration started. This repo is a submodule there.

Status: idea approved for exploration, nothing built. Bead: `cr-w53s` (epic).
Branch `via`, cut from `dws-workflow` @ fe6a9f9 (2026-09-24).

## What VIA is

A single CLI to run a **role + prompt** on **any coding harness** (Claude Code,
Codex, OpenCode, Pi, Gemini, Cursor, …) and get a **structured result** back —
so any agent can use sub-agents across harnesses and models, not only within
Claude or within Codex. VIA is also meant to become the **control plane** the
workflow interpreter's foreman calls instead of driving crews itself.

Owner's framing: there will be several top harnesses; some models live only in
some of them; people will use several at once. VIA is the cross-harness,
cross-model single interface, with one pattern for spawn / resume / steer /
end — plus a passthrough mode that forwards native arguments unchanged.

First open question from the owner: **is it needed, and how much?** The
assessment below says yes, scoped tightly, but that decision is still the
owner's to confirm in brainstorming.

## Assessment so far (2026-09-24 discussion)

Why it is useful:
1. One delegation surface. Today delegation is three different paths: the
   Claude Agent tool, raw `codex exec` / `codex exec resume` commands
   (`.repo-context/running-codex.md`), and the interpreter's graphs — each
   with its own flags, result shape and resume story.
2. Cross-harness, cross-model review is a daily practice already (Opus reviews
   GPT-6 Sol's work and vice versa), done by hand.
3. Model availability dictates the harness; a role → model → harness mapping
   hides that.
4. The foreman needs exactly "run role R on prompt P, return a structured
   result"; sharing one tested path beats two.

Why to keep it tight:
- Cost is per harness, forever: each vendor's headless contract (flags,
  output format, session ids, resume, sandbox) moves with its releases.
- Verbs are not uniform: spawn/resume map almost everywhere; steer differs
  (continuation turn vs mid-turn vs unsupported); cancel differs. VIA must
  declare per-adapter capability, never fake a verb.
- Passthrough is an escape hatch; results through it are unstructured.

## Proposed properties (to validate in brainstorming)

1. Headless, one turn per process — no PTY or screen scraping.
2. One result envelope for every harness: status, final text, session id,
   exit code, usage/cost, input/output tree pins, log path.
3. Select by role (`roles.toml` + model catalog) or explicit
   `--model/--effort`; unknown values refused by name.
4. Uniform verbs `spawn`, `resume`, `steer`, `cancel`, `status`, `result`;
   each adapter declares which it supports; unsupported → named refusal.
5. Durable run record with idempotent keys (the existing SQLite ledger).
6. Explicit isolation per spawn: worktree, sandbox, write permission.
7. Adapters pinned and contract-tested per CLI version; drift detected;
   `--passthrough` marks results unstructured.
8. `spawn --background` → run id; `via wait <id>` → envelope (parallel
   sub-agents).

Scope ladder (proposal):
- v0: Claude, Codex, OpenCode; `spawn` / `resume` / `result` as a CLI over the
  existing crews; agent-matrix skill teaches `via spawn --role … --prompt …`.
- v1: background/wait, cancel, steer where supported; foreman calls VIA.
- v2: more harnesses one at a time, each only with a real use.

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
- Ledger (run record): see `docs/workstreams/run-ledger/roadmap.md` and
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

## Open questions for the next session

1. Confirm need and scope (brainstorm): v0 boundaries, which harnesses first.
2. Research: does Zed's Agent Client Protocol (ACP) — or another standard —
   already give structured sessions for the target harnesses? If yes, VIA
   adapters could speak it instead of parsing each CLI. Unverified; check
   current coverage from primary sources.
3. Packaging: a subcommand of the workflow interpreter, or a separate package
   the interpreter depends on?
4. Relation to agent-matrix: replace its spawn guidance with VIA usage, fold
   its catalog into the model catalog, or retire it.
5. Steering semantics per harness, and what `cancel` means for each.
6. Name the result envelope contract and where it lives (`contracts/`).

## Constraints carried over

- Stage explicit files only; no push without the owner's word; no machine-local
  absolute paths in committed files; throwaway work in `scratchpad/`.
- Verification commands: `.repo-context/verification.md`.
- Implementer/reviewer roster is the owner's choice per session.
