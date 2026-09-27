# Learnings

Verified, likely-to-recur patterns from work in this repo: tool quirks, failed
approaches, and gotchas that would cost the next agent time. Add an entry only
when it is verified and likely to recur; state the pattern, the evidence
(repo-relative path, command, or version), and the fix. Design decisions belong
in the design record (`docs/`), not here.

- `codex exec -c` silently accepted invalid keys or values on CLI 0.144.1,
  including a bogus effort value. Validate safety-critical overrides before
  dispatch; prefer native `-s` for plain `exec`. `exec resume` and `exec review`
  did not accept `-s`; verify current CLI behavior before changing a wrapper.
- `codex exec` launched without a terminal waits on stdin ("Reading additional
  input from stdin..."); redirect `< /dev/null`.
- Claude Code cloud sessions (2026-09-25): `git push` to this repo works once
  the Claude GitHub App covers it, but `bd dolt push` gets HTTP 403 from the
  session's git proxy. Cloud sessions carry Beads changes back through the
  committed `.beads/issues.jsonl` / `interactions.jsonl`; sync Dolt locally.
- `bd init` inside a checkout nested in another beads repo can bootstrap the
  outer repo's issue database. Initialize from a standalone clone.
- `claude --cloud` (CLI 2.1.283, 2026-09-27) decides clone vs upload by asking
  claude.ai whether the Claude GitHub App is installed on the repo. If the
  answer is empty ("status is null" in `--debug-file` output) it uploads a
  bundle: the session has no remote and the git proxy refuses pushes, so
  `--teleport` cannot bring it back. `/web-setup` did not change this; opening
  claude.ai → Connectors → GitHub Integration → "Check repository status" for
  the repo did, after which the CLI logged "GitHub app is installed" and the
  session cloned. `--cloud` also needs a TTY (run it in tmux).
