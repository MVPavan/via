# VIA session handoff (2026-10-22)

This file is the resume point for a new session on any machine. It replaces
the earlier rust-foundation checkpoint, which is still in git history (before
this commit). Read it in full before doing anything else.

## 1. Resuming on a new machine

1. **Repository.** Clone `https://github.com/MVPavan/via` and use branch `rust-foundation` (pushed at the commit that adds this file). `main` holds the same tree, merged.
2. **Beads.**
   - Bead state syncs through its Dolt remote on the same GitHub repo: run `bd dolt pull`, then `bd prime`.
   - `.beads/*.jsonl` are generated mirrors and stay uncommitted.
3. **VIA itself.** Install the release binary as in the [user guide](../../usage.md#install), or build with `cargo build --release --locked`.
4. **Toolchain.** Rust is pinned in `rust-toolchain.toml` and installed by rustup. Gates are in `.repo-context/verification.md`.
5. **Not in the repo:**
   - the gitignored `scratchpad/`, which holds old evidence, review files, the Sol launcher scripts and release-0.1.x artifacts;
   - the assistant's local memory notes.
   Everything needed to continue is summarised here. The standing rules from those notes are in §6.
6. **Harness logins on the new machine:**
   - Claude Code and Codex: log in normally.
   - Pi: one-time login into VIA's private Pi directory ([Pi setup](../../usage.md#pi-setup)).
   - OpenCode: **version 2** (`curl -fsSL https://opencode.ai/v2/install | bash`). It needs no login.

## 2. Where things stand

### Releases
- **VIA 0.1.1** is released: https://github.com/MVPavan/via/releases/tag/v0.1.1
  - Assets: a static musl Linux x86_64 binary and its `.sha256` (`4d443a3e…`).
  - The release notes were later amended to say OpenCode must be v2.
- **0.1.0** is also published, with `via-overview.mp4`.
- Linux x86_64 only.

### What 0.1.1 changed from 0.1.0 (epic `via-l47`, closed)
- Plain-language `--help` for every command and flag, with examples. The old help leaked internal spec references.
- Readable text and tables when output goes to a terminal. Piped or `--json` output is byte-identical JSON, so the scripts and agents contract is unchanged.
- `via models` discovers live catalogs on demand from VIA's own Codex, OpenCode and Pi servers or probes.
  - It has a 20 s bound.
  - Each harness gets a status in a new `harnesses` array.
  - Claude keeps bundled aliases, and any full Claude model ID works.
- `via status --turn N` shows the turn's steps.

### Since 0.1.1 (on `rust-foundation` and `main`)
- `docs/usage.md` says OpenCode must be v2: its default installer and npm give 1.19.x, which VIA refuses.
- `scripts/container-check/` runs a fresh Ubuntu 24.04 user end to end; see its README. In the container:
  - VIA comes from a GitHub release;
  - the harness CLIs come from their official installers;
  - only logins are mounted.

  Each harness runs live through spawn, resume, a cancel during a shell command, and close; then the check looks for leftover processes.

  Last run, on v0.1.1:
  - all four harnesses pass;
  - nothing was left running;
  - Claude's cancel step is not really exercised, because Haiku backgrounds the command and ends its turn first (documented behaviour).

  Run it with `scripts/container-check/run.sh 0.1.1`.

### Verification at release
- Gates were green on the gated tree: 1668 standard tests, 2187 failpoint tests, clippy, fmt, deny, the layer and harness-literal checks, and `scripts/build-release.sh`.
- A Sol (GPT-6.1 Sol, high) review found 2 important display bugs, both fixed and re-checked SOUND.
- A live `via models` run against real logins took 1.3 s, listed 66 models and left no processes behind.

### Nothing is in flight
No branch, worktree, review or agent is open. The merged worktrees were removed.

## 3. Next: the owner's agenda

1. **The owner has two new topics to raise.** They were not stated before this handoff; ask for them first.
2. **Open question to the owner:** start the "0.1.2 robustness" batch below, run through VIA (§5)?
3. **The ACP route.** It needs a design discussion with the owner before any build (§4, item 5).

## 4. Remaining work (macOS excluded by owner decision)

In the recommended order:

1. **Correctness and safety bugs** (small, part of 0.1.2):
   - `via-8w4` (P1): a provider error can echo part of an API key into the envelope, events or logs. Seen live with a Pi 401. Define a redaction rule: `detail` carries status and provider code only, and raw text goes to private evidence.
   - `via-ty8`: an OpenCode turn that changes many files exceeds the 1 MiB event line and fails.
   - `via-joc`: after a crash or reboot, an unproven anchor can hold one of the 8 harness-process slots forever.
   - `via-2s3`: `failure.message` shows Rust Debug text.
   - `via-7zc`: a wrong OpenCode version is accepted at spawn and fails only in the turn; `describe` doesn't warn.
   - `via-aqk`: `config_switch_unverified` appears on every Pi and OpenCode turn, even with default settings.
   - `via-cjw`: when the stdout pipe closes early, the CLI reports `daemon_unreachable`.
2. **Session retention** (`via-jm4.18`, medium, part of 0.1.2). Nothing in `~/.via/state` is ever deleted. Add retention for closed sessions' evidence, logs and rows, plus a manual prune command. This is urgent now that all subagent work runs through VIA.
3. **Leftover-process reporting** (`via-jm4.28`, `via-daz`, `via-jm4.24`). Cleanup works, but the envelope's `leftovers` is always `null`, and the Claude route never scans.
4. **Test-suite reliability.** One pass over about 15 flaky-test beads: `via-iuj`, `via-yv7`, `via-jm4.20`, `via-jm4.19`, `via-dxd`, `via-fu0`, `via-48v`, `via-pt8`, `via-4at`, `via-f7s`, `via-jm4.16`, `via-9us`, `via-gew`, `via-9gh`. Most fail only under load or when the WSL clock steps backwards; some leak processes when run outside nextest.
5. **The ACP route** (the largest item; no bead yet). One generic route for Zed's Agent Client Protocol (`initialize`, `session/new`, `session/prompt`, `session/cancel`, `session/load`, `session/update`). New ACP agents (`gemini --acp`, `opencode acp`, Goose, Kimi, bridges) would then be configured in `daemon.json` instead of coded.
   - **Known hard parts:**
     - mapping VIA bounds to ACP permission requests (the bridges we tested lost fidelity, so expect `full` only at first);
     - the client duties ACP may ask for (file reads, terminals): provide them or decline cleanly;
     - per-agent differences in usage, cost and resume support;
     - liveness of bridged sessions.
   - **Prior research:**
     - `docs/brainstorms/routes-decision.md` (the bridged-ACP density and fidelity findings);
     - the README Roadmap.
   - **Before designing:** the local ACP docs snapshot is old, so re-check the protocol at https://agentclientprotocol.com first.
   - **Estimate:** about 1–2 weeks of agent work after the design.
6. **More control, after ACP:**
   - Claude limited bounds (`via-mv5`; Claude offers only `full` today);
   - Codex steer (`via-gaz`);
   - cross-harness crash and overload scenarios (`via-gvg.1`, `via-gvg.1.1`, `via-gvg.1.2`, `via-gvg.2`).
7. **Hold until a real problem appears** (measure-later rule): `via-d9o.2.3`, `via-pvj.5`, `via-pvj.6`, `via-q74`, `via-jm4.29`, `via-2el`, `via-pzf`. Also not done: the Linux 5.15 minimum-kernel run.
8. **Tracking housekeeping:** `via-d9o.2` and several epics are stale after two releases; close or re-scope them at the next batch.
   - Epics `in_progress` only because of deferred children: `via-jm4`, `via-4sw`, `via-5lr`, `via-pvj`, `via-gvg`, `via-d9o`.

Smaller open owner decisions:
- **Email in history.** The owner's email is in the commit author fields and in the beads `owner` field. It was already public; removing it would mean a history rewrite.
- **Video end card.** Optionally re-render the overview video with "ACP route" on the end card. The source is `docs/assets/via-overview.html`.

## 5. How work is run (owner decisions)

**Roles:**
- Claude (Opus) coordinates and writes no product code.
- Opus implementers write code: effort medium for ordinary slices, high for ownership, concurrency, lifecycle or recovery work.
- GPT-6.1 Sol at high effort does **all** reviews.

**From 2026-10-12, all subagent work runs through VIA itself (dogfooding):**
- **Implementer:**
  ```bash
  via spawn --harness claude --model opus --effort medium|high --bound full --network \
    --cwd <worktree> --instructions .claude/agents/implementer.md \
    --prompt-file <brief> --background --json
  ```
  Then run `via wait <turn> --timeout-ms …` in a background shell so the harness notifies on completion.
- **Sol review:**
  ```bash
  via spawn --harness codex --model gpt-6.1-sol --effort high --bound read_only ...
  ```
  Fix rounds use `via resume <session>` on the same session. This replaces the older tmux `codex exec` scripts, which live in the gitignored scratchpad.
  - Codex refuses `--network` under a limited bound (documented), so omit it for read-only reviews.
- **Limitation:** the Claude route offers only `--bound full` (`dontAsk`, all tools), so Claude workers can't be read-only. On the first run, check that the owner's hooks still apply.
- **Not yet tried end to end:** this VIA-based workflow.
- **Session cleanup:** `via close` each session after collecting results.

**Review loop:**
1. The first review is fresh and exhaustive: list every instance of each issue.
2. Fix rounds resume the same Sol session and check only "fixed?" and "new defects?".
3. The loop closes when no blocker or important finding remains. Minors never add a round.
4. Large work then gets one from-scratch critical review, whose findings are looped the same way.

**Workflow per change:**
- One worktree and branch per worker under `.claude/worktrees/`.
- The coordinator merges with `--no-ff` into `rust-foundation`, runs the full gate on the merged tree, then merges into `main`.
- Delete merged worktrees promptly: each `target/` reaches 30–60 GB.

**Releases:**
1. Bump `Cargo.toml` `version`, then `cargo update -w --offline`.
2. Run the full gate and `scripts/build-release.sh` (writes `dist/`).
3. Merge into `main`, push, then tag `vX.Y.Z`.
4. `gh release create` with the binary and its `.sha256`.
5. Install from the downloaded release and verify the checksum.
6. Run `scripts/container-check/run.sh X.Y.Z`.

## 6. Standing rules

**Scope:**
- Fix only realistic, observed problems; no hypothetical scenarios. Ship fast.
- Simplest behaviour first, measure later.
- Decide routine orchestration on evidence and report the decision. Ask only about new scope, authority or irreversible actions.

**Public repo:**
- No secrets, credentials or personal data; repo-relative paths only.
- Privacy-scan every diff before a merge, for home paths, user names, key prefixes and email addresses.

**Credentials:**
- Never read, copy or log vendor credential files (Claude, Codex, Pi, including VIA's private Pi `auth.json`, and OpenCode). Existence and mode checks only.
- Never echo environment values.
- Never run `codex mcp list`, and never modify vendor config files.

**Processes VIA didn't start:**
- Don't touch the owner's Codex desktop app or app-server, the T3 service, the owner's own `opencode serve --service`, or other `claude` processes.
- Never run plain `opencode` / `opencode run`, and never read the owner's `~/.config/opencode` or `~/.local/share/opencode`.
- Workers stop every process they start and verify with `pgrep`. This includes their own wait loops: one loop in the 0.1.1 work matched itself in `pgrep` and never ended.

**Live tests:**
- Models: Claude Haiku, Codex `gpt-6-luna`, Pi `openai/gpt-6-luna`, and an OpenCode free model (e.g. `opencode/mimo-v2.6-flash-free`).
- Pi live tests use the owner's default `~/.via` state and its Pi login. Never disturb a daemon the owner started.
- Live steps that need the owner's vendor homes are run by the coordinator; implementers analyse the evidence.
- On a failed live run, read the vendor's session log first.

**Git:**
- Explicit `git add <paths>` only.
- Never `git add .`/`-A`, `--no-verify`, force-push, `reset --hard`, `clean`, `restore` or `checkout --` rewrites. Amend only on request.
- Push only with the owner's authority. Pushing `rust-foundation` and `main` and making releases has been authorised for each release.
- Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`, plus the session link line the runtime supplies.

**Deletions:** a hook blocks recursive `rm` outside `/tmp` for the assistant. Ask the owner to run such deletions. `git worktree remove` is fine for clean merged worktrees.

**Beads:**
- Use `--actor cc:t4-orchestrator` (or the session's own actor).
- Use `bd update --append-notes`, never `--notes`, which replaces the notes.
- Close finished beads promptly with evidence.

## 7. Key references

- `README.md`: the product, quick start and roadmap.
- `docs/usage.md`: the user guide.
- `docs/specs/via-api-v1.md`: the C1 API.
- `docs/specs/adapter-contract.md`: the C2 contract.
- `docs/specs/vendors/*.md`: per-vendor notes.
- `.repo-context/`: repo map, invariants, coding style and verification gates.
- `docs/brainstorms/README.md` §15–§16 and `routes-decision.md`: the design record and route choices.
- `scripts/build-release.sh`, `scripts/container-check/` and `scripts/qualify/`: the live qualification runners.
