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
- Exact per-copy memory and per-write disk proofs did not converge in design
  review. T4-0 rounds 5–7 grew from 1,803 to 2,295 lines, and each Sol round
  found another uncharged copy or unpredictable SQLite growth. Coarse class
  charges with a named refusal, admission-time disk checks with headroom, and
  measured gates converged in seven rounds. Propose the coarse form, plus a
  measurement task, before designing exact accounting.
- When a review fix needs a new mechanism with its own states (T4-0's
  raw-worker seal: truncation, a `Sealed` answer, recovery repair), first
  restate the requirement narrowly. The seal caused a regression round.
  "`logs` is bounded by the committed `high_water`, and loss is decided by
  offsets" met the same need without new states.
- Codex review sandboxes (`-s read-only`) cannot open the Beads database. Paste
  the relevant Beads notes into the review brief rather than citing
  `bd show`.
- `tokio::sync::Semaphore` is fair. A waiting `acquire`/`acquire_many`
  immediately takes every free permit and receives each released one until it
  is satisfied (tokio 1.53.1 `batch_semaphore.rs:397-445`, `:306-331`);
  `try_acquire` then fails. On a shared memory pool, one waiter starves every
  `try_acquire` caller. Take a whole reservation with `try_acquire_many`, and
  never wait while holding permits.
- Before designing durability for data VIA passes through, check whether the
  producer already keeps it. T4-0 spent rounds 12–15 on raw-log offsets and
  loss tracking. A 2026-09-29 check showed Claude, Codex and OpenCode already
  keep their conversations (transcripts, rollouts, SQLite). Dropping VIA's
  copy removed the task's riskiest mechanism.
- Test harness readiness must never use a command that can start a daemon.
  The Task 4 harness polled `via daemon status`, which auto-starts one. A
  rival daemon then won `daemon.lock`, and about six scenarios failed
  depending on load. Probe the socket directly and match the pid of the
  daemon the test started (`crates/via-cli/tests/support/daemon.rs`
  `serving_pid`).
- Wall-clock bounds in scenario tests fail under a parallel suite without
  any defect. Examples: 181 ms against 100 ms, a 2.7 s WSL realtime-clock
  step, and 115 ms against 100 ms with the test running alone. Prove "does
  not block" by order (the reply arrives while the blocker is still held)
  or by counts (failpoint hits), and record latencies as evidence. Where a
  bound must stay, set it well above normal jitter so only real starvation
  fails it (Task 4 A50, A52).
- A bounded shared pool needs a cap per kind of work, not just an owner per
  task. Once Task 4 owned every blocking step through the Store's 16-slot
  pool, `daemon/status` walks and `logs` checks could fill the pool and
  refuse turn-critical steps. Every CLI command sends `daemon/status`. Give
  optional work its own small permit count inside the shared pool (Task 4
  T4-fix: 2 diagnostic permits).
- Floods in tests must pace on observed consumption. The Wire reader never
  waits for its consumer, so more than 1,024 unconsumed messages overflow by
  design (A47). A test that wrote 1,040 lines in one burst failed 64 of 240
  runs under parallel stress.
- Review copies committed from `scratchpad/` need their links rewritten, not
  just stripped of the absolute prefix. Codex reviews link as
  `(/abs/repo/path:line)`. Removing the prefix leaves repo-root targets with
  `:line` suffixes, which do not resolve from `docs/.../reviews/` and broke
  322 links (`6caffb3`). Link repo files relative to the review's folder
  with `#L<line>`, turn scratchpad targets into plain text (they resolve only on the machine
  that has `scratchpad/`, so the link check misses them), and run the link
  check from `verification.md` before committing.
- Claude Code's `isolation: worktree` creates the worktree from
  `origin/HEAD` (`origin/main`), not from the working branch. On
  `rust-foundation` that is hundreds of commits behind. Workers used to
  fast-forward themselves, but auto mode can deny `merge --ff-only` as
  destructive. Create the worktree yourself at the intended base
  (`git worktree add -b wt/<name> .claude/worktrees/<name> <commit>`) and
  give the worker its path, rather than relying on isolation.
- A Codex review asked to "probe for DoS" or to build hostile inputs against
  a parser or validator can be cut off by the provider's cybersecurity
  filter ("This content was flagged for possible cybersecurity risk"), and
  then it ends with no report. Phrase resource questions as engineering
  limits (are the bounds complete, do the measurements hold), and point the
  reviewer at existing measurement files and tests. If a run is cut off,
  resume the same session with that framing; it finished there (S-CORE
  chunk 5 r2).
- Stress load generators (busy loops used to reproduce flakes under load)
  outlive the worker that started them and load the owner's shared machine.
  Briefs that allow load testing require the worker to stop every
  generator, confirm none remain with `pgrep`, and say so in its report
  (X5 flake hunt, 2026-10-05: 48 loops were still running after hand-back).
- A merge commit and its push belong in one `&&` chain, after staging every
  resolved conflict explicitly. In a script where commands follow one
  another on separate lines, a failed `git commit` (files still unmerged)
  does not stop the `git push` after it, which then publishes the previous
  head (Codex and Claude fix merges, 2026-10-06: the Codex merge went out
  ahead of the Claude merge's commit).
