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
- Patch rounds on dispatch/failure-recovery code did not converge (T2-B,
  2026-09-27, `docs/workstreams/rust-foundation/t2/sol-review-T2-B*.md`):
  every round fixed the prior blockers, but each fix added an unowned state
  (orphan set, then a 250 ms polling wait loop) that the next review found
  broken. Design the state machine first and review the design; see
  `docs/workstreams/rust-foundation/cloud-and-local.md` §6 "Convergence".
- Local implementer workers committed trees that fail clippy three times
  despite the brief (T3-S2 `42acc87`, `536b5c9`; T3-S3 `3448112`); once a
  `| tail -1` pipe hid the failing exit status. Dispatch must say: run the
  gate in order, stop at the first failure, never pipe a check so its status
  is lost, never commit a tree that fails fmt or clippy. The orchestrator's
  merged-tree gate catches the result, not the intermediate commit.
- Writing a Codex review brief with an unquoted heredoc (`<<EOF`) executes
  backticked names such as `via cancel` as commands and silently drops them
  from the brief. Use `<<'EOF'`, and check the brief before launch.
- Claude Code subagent transcripts (`<session>/subagents/*.jsonl`, CLI
  2.1.283) log each message's stream-start usage: `output_tokens` is 2–16,
  never the final count; input and cache fields are complete. Main-session
  and `claude -p` transcripts log final usage. Estimate subagent output
  from content size, calibrated on main-session messages, and label it an
  estimate (a local script is in the gitignored `scratchpad/usage/`).
- When a contract bound changes (for example, T3's runtime §7 "stops within
  3 s" becoming "the reply wait ends at force + 3 s"), grep the spec, the
  design (including its test tables and amendments) and the code comments
  for every restatement of the old bound, and fix them in one pass. Record
  the change as a numbered amendment, not as a "clarification". T3 took
  four extra Sol confirmation rounds, each finding one more stale line.
