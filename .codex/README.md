# .codex — the Codex view of this repo's harness

Shared skills live in `.claude/`; repository guidance lives in `.repo-context/`
and `AGENTS.md` (which Codex reads natively). Under `.codex/`:

- `skills/*` — symlinks to `../../.claude/skills/<name>`. Every skill carries
  `agents/openai.yaml` with `policy.allow_implicit_invocation`, the Codex twin
  of `disable-model-invocation`. Not linked: `show-me` (Claude-only), and any
  `harness-*` or `in-progress` skill (root-only).
- `rules/default.rules` — native Codex command policy for high-risk command
  prefixes.
- `agents/*.toml` — hand-maintained Codex twins of `.claude/agents/*.md`.
  Models and effort come from runtime configuration, not these files.
- `config.toml`, `hooks.json`, `hooks/` — Codex wiring and hook scripts (the
  Python generated-edit guard differs from the bash one by payload schema).

Adapted from the DWS harness in the parent `coding-ritual` repo (parent-repo
path). When a skill is added to `.claude/skills/`, add its symlink here.
Running Codex from another agent: `.repo-context/running-codex.md`.
