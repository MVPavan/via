# W1 worker rules (all cloud sessions)

You are one worker in wave W1 of slice S1. A local orchestrator reviews and
merges your branch; you never merge. This repository is public.

## Read first (only what you need)

- `AGENTS.md`; `.repo-context/coding-style.md` (Rust standard; §10 testing);
  `.repo-context/verification.md` (the gate); `.repo-context/invariants.md`.
- `docs/workstreams/rust-foundation/session-handoff.md` (state and findings),
  `docs/workstreams/rust-foundation/checkpoint/task1-review.md` (findings
  T1-I1..I7 in full), `docs/workstreams/rust-foundation/cloud-probe-w0.md`
  (this cloud machine).
- Contracts: `docs/specs/via-api-v1.md` (C1), `docs/specs/adapter-contract.md`
  (C2), `docs/specs/runtime-contracts.md`. Contracts win over current code.

## Setup

- Install the toolchain from `rust-toolchain.toml`; install `cargo-nextest`
  and `cargo-deny` as prebuilt binaries if you can (building cargo-deny from
  source takes ~2 min).
- Export `XDG_RUNTIME_DIR` to a private 0700 directory for tests; never rely
  on PID 1 reaping orphans quickly (see the probe report).

## How to work

1. For each finding you own, **first** write down how it fails, then add the
   smallest regression that fails on the current code for that reason
   (end-to-end through the real `via` binary where the defect crosses a
   process or contract boundary; isolated otherwise). Run it and record the
   failure. Only then fix it.
2. Fix at the owning layer. Edit only your owned paths; if a fix truly needs
   a change elsewhere, make the smallest one and call it out in the report.
3. No speculative features, no unrelated cleanup, no new dependencies unless
   unavoidable (then justify). No new debug RPCs or CLI verbs.
4. Run the full gate (`.repo-context/verification.md`); all of it must pass.

## Git and reporting

- Work on the branch this session was given; commit with clear messages.
  A local worker (subagent in a worktree) never pushes; a cloud session
  pushes only its own branch. Never push to `rust-foundation` or `main`.
- Do not run `bd`. Do not edit `.beads/`. Do not edit other workers' paths.
- Write `docs/workstreams/rust-foundation/w1/reports/<your task id>.md`:
  per finding the failure mode, the regression (and its failing output
  before the fix), the fix, files changed; gate results; anything left open
  or uncertain. Commit it on your branch.
- End with a reply under 1500 characters: branch name, commits, gate result,
  open issues.
- Never print or commit tokens, credentials, environment dumps or private
  paths.
