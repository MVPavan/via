# Cloud and local split (draft for owner decision)

Status: decided by the owner, 2026-09-27 (§5). Facts checked against
official docs on that date; items marked **unverified** need the cloud probe
before we rely on them.

## 1. What already exists

- Two earlier Claude Code cloud sessions worked on VIA: the `.repo-context/`
  guidance set (branch `claude/repo-context`, merged) and the `via-str`
  SQLite WAL spike (branch `spike/sqlite-wal`). Both are idle.
- `.repo-context/learnings.md` (2026-09-25): `git push` works from a cloud
  session once the Claude GitHub App covers the repo, but `bd dolt push` gets
  HTTP 403 from the session's git proxy. Cloud sessions carry Beads changes
  back through the committed `.beads/issues.jsonl` and `interactions.jsonl`;
  Dolt is synced locally.
- No cloud session has worked on `rust-foundation`. `origin/rust-foundation`
  exists at `a366d4f`; local is two commits ahead (the WIP checkpoint and its
  Beads record), which a cloud session would not see until pushed.

## 2. Claude Code cloud sessions: facts

Sources: [cloud environments](https://code.claude.com/docs/en/cloud-environments),
[Claude Code on the web](https://code.claude.com/docs/en/claude-code-on-the-web),
[web quickstart](https://code.claude.com/docs/en/web-quickstart),
[security](https://code.claude.com/docs/en/security).

| Topic | Fact |
|---|---|
| Plans | Pro, Max, Team, and Enterprise premium seats; usage counts against the same limits as other Claude usage |
| Start | claude.ai/code, mobile or desktop app, or `claude --cloud "task"` from the terminal; `claude --teleport` pulls a cloud session back locally |
| Repo | From GitHub (private repos need the Claude GitHub App); each session starts from a fresh clone |
| Machine | Fresh Ubuntu 24.04 x86_64 VM per session: 4 vCPU, 16 GB RAM, 30 GB disk; heavy builds may be stopped |
| Tools | Rust (rustc, cargo), Go, Node, Python, C/C++, Docker, git, `gh` preinstalled; a setup script (runs as root, ~5 min to be cached) can install more |
| Commands | Bash waits 2 min by default, up to 10 min; longer commands move to the background |
| Idle | A session stops after inactivity and its VM is reclaimed; nothing running survives |
| Network | Levels None / Trusted (default) / Custom / Full. Trusted includes crates.io, static.rust-lang.org, npm, PyPI, Docker Hub; other hosts need Custom or Full |
| GitHub | Through a proxy; real GitHub credentials never enter the VM; push only to the session's branch |
| Carried over | Repo `CLAUDE.md`, `.claude/settings.json` hooks, `.claude/skills/`, `.claude/agents/`, `.mcp.json`. **Not** carried: `~/.claude/` (user CLAUDE.md, skills, hooks), local MCP servers |
| Environment variables | Readable by anyone using the environment; the docs say never put secrets there |
| API credentials (Pro/Max) | A key stored on the environment; Anthropic's proxy adds it to requests for the hosts you list **after they leave the VM**. The key never reaches Claude, its commands or the environment |
| Beyond limits | Remote Control (web/mobile driving a session on your own machine) or self-hosted environments |

**Unverified for VIA:** nested virtualization (our kernel 5.15 KVM runner);
user-namespace sandboxing (`bwrap`) inside the VM; whether a `claude` CLI
child process can authenticate inside a cloud session (needed for live
Claude adapter tests); the exact hosts OpenCode's free models need.

## 3. Codex in the cloud

- **Free tier:** ChatGPT Free and Go include Codex only on the web; the
  Codex CLI and IDE extension need Plus or higher
  ([Codex pricing](https://learn.chatgpt.com/docs/pricing)). So a free login
  would not run `codex` in a cloud VM.
- **Our workers are Codex models** (GPT-6 Sol implements, Astra reviews).
  Without Codex in the cloud, cloud sessions would implement with Claude
  models instead: a roster change the owner must approve.

Ways to get Codex into a cloud session, safest first:

| Option | How | Risk |
|---|---|---|
| A. Keep Codex local (recommended) | Cloud does Codex-free work; Codex runs, reviews and live Codex tests stay on this machine | None new |
| B. API credential | OpenAI API key (separate project, spend cap) stored as an environment API credential for `api.openai.com`; Codex runs in API-key mode | Pay-per-token billing, not the ChatGPT plan; **unverified** that Codex works with a proxy-injected key |
| C. Device-code login in the VM | `codex login --device-auth` inside the session | ChatGPT tokens sit in plain text in the VM, readable by the agent; OpenAI says treat `auth.json` like a password ([auth](https://learn.chatgpt.com/docs/auth)). Not recommended |
| Never | Credentials in git, environment variables or the setup script | Exposed to anyone using the environment or the repo |

## 4. Proposed split

| Work (Beads) | Where | Why |
|---|---|---|
| S1 fixes and remaining S1 tasks (`via-jm4.7.*`) | Cloud | Fake agent only; Rust and crates.io work under Trusted |
| OpenCode adapter (`via-4sw.3.*`) | Cloud, if a probe shows the free models are reachable with a Custom allowlist | Free models, no login |
| Claude adapter fixtures and implementation (`via-p98.3.1`–`.3.3`) | Cloud | Recorded fixtures need no login |
| Claude live qualification (`via-p98.3.4`) | Local, unless a cloud probe proves `claude` child auth | Unverified in cloud |
| Codex adapter and live qualification (`via-5lr.3.*`) | Local | Needs Codex login |
| Platform runs on the kernel 5.15 KVM runner (`via-pvj`) | Local | Nested virtualization unverified |
| Beads Dolt sync, merges, pushes of `rust-foundation`, release verification | Local | `bd dolt push` fails in the cloud; merges need the owner |

Coordination rules (proposed):

- One cloud session per Beads task, on its own branch; it pushes only that
  branch. The local orchestrator reviews and merges into `rust-foundation`.
- A cloud session starts from `docs/workstreams/rust-foundation/session-handoff.md`
  and its task's Bead; it records Beads changes in the committed JSONL mirror.
- Before any cloud session starts, the local checkpoint must be pushed
  (owner authorization needed) so the cloud clone sees it.
- The repo is public: nothing private goes into briefs, branches or logs.
  Local-only evidence under `scratchpad/` does not exist in the cloud.

## 5. Owner decisions (2026-09-27)

1. Codex: **option A**, Codex stays local; no Codex credentials in the cloud.
2. Roster: in the cloud, Claude models implement and review; Sol and Astra
   remain available locally.
3. Push `rust-foundation` so cloud sessions start from the checkpoint:
   authorized.
4. Use the owner's promotional cloud credits first (consumed before the
   subscription).
5. Run a short cloud probe first (Rust build and gate, `bwrap`, OpenCode
   free-model hosts, `claude` child auth, nested virtualization) before
   assigning work.

## 6. Roster and models (owner, 2026-09-27)

- The local Claude session is the **orchestrator**: it briefs, starts and
  steers cloud sessions, reviews and merges their branches, runs the gate
  and keeps Beads. Small edits are done directly by the orchestrator or a
  local Opus 5.5 low/medium subagent, without a cloud round trip.
- Cloud sessions (Claude only, no Sonnet):

| Work | Model, effort |
|---|---|
| Quick edits and checks, the W0 probe, fixtures, docs, small fixes | Opus 5.5 low |
| Normal implementation: S1 tasks `.7.6`–`.7.8`, ordinary S1 fixes, Claude and OpenCode adapters | Opus 5.5 medium |
| Shutdown-design integration; hardest S1 findings (T1-I1, T1-I7, other ownership or timing findings) | Opus 5.5 high |
| Final S1 critique (`via-jm4.7.9`) | Fable 5.1 high |

- Local: GPT-6 Sol high implements the Codex adapter.
- Reviews (owner, 2026-09-27): **GPT-6 Sol** reviews the Opus-built work,
  medium for each worker branch, high for a whole slice. **GPT-6 Astra**
  and **Claude Fable 5.1 high** are kept for large bodies of work and
  critical reviews only (for example the final S1 critique and release).
- Default cloud environment: `Via-probe` (Full network, for the probe;
  switch to Custom once OpenCode's hosts are known). Each cloud session
  reports its model and effort first and pushes only its own branch; it
  does not run `bd`, and the orchestrator records Beads changes locally.
- Local control runs in tmux session `via`: `scratchpad/cloud/launch.sh
  <name> <model> <effort> <brief>` starts each cloud session from its own
  window (log `scratchpad/cloud/<name>-launch.log`), and window `watch`
  (`watch-branches.sh`) records new remote branches in
  `scratchpad/cloud/new-branches.log`. Both survive the orchestrator
  session closing; the cloud sessions themselves run on Anthropic's side
  regardless.
- Review loop (owner, 2026-09-27): a worker branch stays unmerged until
  its GPT-6 Sol medium review is SOUND or each remaining finding is
  deferred to a named task. Findings go back to the **same cloud session**
  (`scratchpad/cloud/followup.sh`), which first merges `rust-foundation`,
  fixes, and appends a "Round N" section to its report; Sol re-reviews only
  that round's change. Fresh sessions are for new tasks, a different
  model/effort, or an overlong context. After merging a wave, the
  orchestrator runs the gate; Sol high reviews each completed task or
  slice, which also covers interactions between parallel branches.
- Parallel workers get disjoint files. `via-core/src/engine.rs` is the
  recurring conflict point; split it by responsibility before the next
  parallel wave.
- The owner delegates routine orchestration decisions (scoping, deferring
  findings to planned tasks, fix guidance, escalation); the orchestrator
  decides on evidence and reports what it decided.
