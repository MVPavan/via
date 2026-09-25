# Codex invocation — guidance for agents running Codex

## Running Codex

Call the **Codex CLI directly**. Never use the codex-adapter plugin
(`codex-run.mjs`, `/codex-*` skills) — it was uninstalled 2026-09-23. The
model and effort come from the user's current roster; ask if undefined.

```bash
# new run — use -s read-only for review/analysis, workspace-write to edit
codex exec -C <dir> -s workspace-write -m <model> -c model_reasoning_effort=<effort> \
  -o <answer-file> "<prompt>" 2> <log-file>
# resume — no -C/-s flags; set sandbox via -c
cd <dir> && codex exec resume <session-id> -m <model> -c sandbox_mode=workspace-write \
  -c model_reasoning_effort=<effort> -o <answer-file> "<prompt>"
```

- The session id is the first `session id:` line on stderr; `-o` writes the
  final answer to a file. Prefer resume over a fresh run for follow-ups.
- `codex exec` silently accepts bad `-c` values — see `.repo-context/learnings.md`.
