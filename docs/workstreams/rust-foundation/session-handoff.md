# Rust foundation — paused checkpoint

## Current state (2026-09-28, resumed)

The owner resumed work under a cloud/local split:
[cloud-and-local.md](cloud-and-local.md) §5–§6 holds the decisions, roster,
models, review rule and tmux control. Everything below "Status: PAUSED" is
the historical checkpoint; its findings table is updated by this section.

- W0 cloud probe: [cloud-probe-w0.md](cloud-probe-w0.md).
- W1 ([w1/README.md](w1/README.md)): A (T1-I1, T1-I5 route side),
  B (T1-I2, T1-I3), C (T1-I5 Core side, T1-I6) merged into
  `rust-foundation` at `4f9d6e3`; gate green, 64 tests. Worker reports in
  `w1/reports/`.
- GPT-6 Sol medium reviewed each W1 merge: A unsound, B and C sound with
  changes ([w2/sol-reviews/](w2/sol-reviews/)); three small fixes landed
  locally (`9f00443`).
- W2 ([w2/README.md](w2/README.md)): W1-D (shutdown seam, T1-I7, T1-I4,
  stop drain) and W2-E (T1-I5 end to end, evidence gaps, failure classes)
  merged; gate green, 86 tests. Sol medium: W1-D unsound, W2-E sound with
  changes.
- W3 ([w3/README.md](w3/README.md)): W3-F (stop/shutdown/cancel) and W3-G
  (raw waits, uncertain commits) merged; one unified unresolved-turn
  tracker; force-stop e2e test now waits for a durable observation before
  forcing (it raced locally). Gate green, 98 tests. Sol medium: W3-F
  unsound, W3-G sound with changes (`w3/sol-review-*.md`).
- W4 ([w4/README.md](w4/README.md)): W4-H and W4-I went three review
  rounds in their own cloud sessions (Sol reviews `w4/sol-review-*`) and
  merged after round 3 (W4-I SOUND; W4-H SOUND WITH CHANGES, no blocker).
  Gate green: 116 passed, 2 skipped (root-only peer test; the
  scheduling-dependent queued-handoff force test, ignored until
  `test-failpoints` exists). Deferred test/Store items are on `via-jm4.7.7`.
- **S1 Task 1 closed** (`via-jm4.7.5`, with `.7.4` harness, `.7.12` Store,
  `.7.13` Host) at `e24bb2c`: Sol high slice review
  ([task1-sol-high-review.md](task1-sol-high-review.md)) → T1-close fixes →
  Sol SOUND (`w4/sol-review-T1-close-r2.md`). Gate: 122 passed, 2 ignored
  (root-only peer test, run as root in the cloud; scheduling-dependent
  race). Approved contract text is in C1/runtime (`4b0b192`, `c00cb2b`).
  Deferred items are on `via-jm4.7.6` (harness gaps) and `via-jm4.7.7`
  (session closure on stop, shutdown phase budget, failpoint-deterministic
  timing tests, Store-failure lifecycle).
- W5-S split `via-core/src/engine.rs` into `engine.rs` (API entry points,
  shared types), `engine/drive.rs`, `engine/stop.rs`, `engine/terminal.rs`
  (Sol SOUND; merged `0f3650f`; 122 passed, 2 ignored).
- Now: S1 Task 2 (`via-jm4.7.6`, [t2/README.md](t2/README.md)).
  T2-A (failpoints, F8/F10, startup recovery) merged at `836b6ce` (Sol r5
  SOUND). T2-B (Opus 5.5 medium) stopped after three UNSOUND rounds;
  T2-B2 (Opus 5.5 high, session `session_011RDNGQPpgG3RwXuw349DaK`)
  redesigned dispatch design-first ([t2/dispatch-design.md](t2/dispatch-design.md))
  and merged at `19c4c71` after three design checks and four code rounds
  (`t2/sol-review-T2-B2*.md`). Per runtime §7 the first failed or
  uncertain Core state write latches Store failure (two-phase: pending
  refuses grants and admission at once, finalized under `admission`),
  force-stops and exits 4; there is no in-daemon orphan reconciler.
  Gate at `19c4c71`: 167 passed / 2 skipped; `test-failpoints` 185 / 2
  (3 runs); F08–F12 line 17 passed; release check passes. T2-C (restart
  handoff of surviving queued turns, cancellation behind `unknown`, keyed
  replay after restart; [t2/c.md](t2/c.md)) merged at `81d5dd7` (Sol r2
  SOUND); gate 170 / 2, failpoints 193 / 2. T2-D ([t2/d.md](t2/d.md):
  runtime §8's 4-slot connection limit, slots owned per process group
  until proved absent, recovered and unread unproven groups held; Store
  schema v3) merged at `3955933` (Sol r3 SOUND); gate 173 / 2,
  failpoints 203 / 2 (3 runs), F08–F12 17. Sol high slice review of Task
  2 ([t2/task2-sol-high-review.md](t2/task2-sol-high-review.md)): ACCEPT
  AFTER CHANGES. T2-E ([t2/e.md](t2/e.md): frozen per-turn parameters,
  schema v4, one launch per turn in F13/F14/F17/F28, version-0 Store
  refusal; fresh Opus 5.5 high session `session_01YHMezY6BkxmEWfYAi52b9g`)
  merged at `f24a91d` after the Sol high re-review
  ([t2/task2-sol-high-review-r2.md](t2/task2-sol-high-review-r2.md),
  ACCEPT AFTER CHANGES; dispositions in [t2/README.md](t2/README.md)) and
  Sol medium on round 1 ([t2/task2-sol-review-r3.md](t2/task2-sol-review-r3.md),
  SOUND); gate 180 / 2, failpoints 210 / 2, F08–F12 17. **Task 2
  (`.7.6`) is closed.** Now Task 3 (`.7.7`, [t3/README.md](t3/README.md)):
  design-first T3-0 ([t3/t0.md](t3/t0.md), Opus 5.5 high,
  `session_01JvsKhcyrWwPXLVcSKKEB1J`, branch
  `claude/t3-0-gap-inventory-design-wcxo29`) writes the gap inventory,
  normative design and slice plan. Design reviews (owner): GPT-6 Astra
  medium and Fable 5.1 high; round 1 decisions in
  [t3/design-r1-decisions.md](t3/design-r1-decisions.md). Owner decisions
  ([t3/owner-decisions.md](t3/owner-decisions.md)): Store failures scoped
  when the outcome is known, latch only on unknown outcome (amends runtime
  §7); drain keeps sessions resumable; force closes only sessions with
  unfinished work. Design rounds 2–4 reviewed by Astra medium and Fable
  high, decisions in `t3/design-r{3,4}-decisions.md`; design at `1a814d6`
  final after round 6 and merged at `68aa7b6` ([t3/design.md](t3/design.md)).
  S0 (file split) merged at `08fffce`. S1 (Store/Host/Wire/Route
  primitives, [t3/s1.md](t3/s1.md)) came from the last cloud worker at
  `8d2a7b6`. Three local `implementer-high` fix rounds followed
  ([t3/s1-r1-decisions.md](t3/s1-r1-decisions.md) 1–11). Sol medium
  reviews: `t3/sol-review-S1-{store,host,r1,r2,r3}.md`, the last SOUND. S1
  merged at `4a1c11c`, with the design edits in `8eb4ed0`. Gate: 213 / 2,
  failpoints 264 / 2 three times, F08–F12 17, release check clean. S2
  (turn control, [t3/s2.md](t3/s2.md)) came from a local `implementer-high`
  subagent with two fix rounds ([t3/s2-r1-decisions.md](t3/s2-r1-decisions.md)
  1–5); Sol medium reviews `t3/sol-review-S2-{core,close,r1,r2}.md`, the
  last SOUND. S2 merged at `dd2a81d`. Gate: 231 / 2, failpoints 295 / 2
  three times, F08–F12 17, release check clean. S4 (recovery
  evidence, [t3/s4.md](t3/s4.md)) merged at `3ca53e4` after two local fix
  rounds ([t3/s4-r1-decisions.md](t3/s4-r1-decisions.md) 1–8; Sol
  `t3/sol-review-S4{,-r2}.md`, the last SOUND). S3 (daemon
  lifecycle, [t3/s3.md](t3/s3.md)) merged at `d69e6d0` after two local fix
  rounds ([t3/s3-r1-decisions.md](t3/s3-r1-decisions.md) 1–11; Sol
  `t3/sol-review-S3-{shutdown,serving,r1,r2}.md`; the last finding deferred
  to `via-pvj.2`). Gate: 263 / 1, failpoints 351 / 1 three times, F08–F12 19, release check clean. S5 (Store failures,
  [t3/s5.md](t3/s5.md)) merged at `9efbab8` after four local fix rounds
  ([t3/s5-r1-decisions.md](t3/s5-r1-decisions.md) 1–16; Sol
  `t3/sol-review-S5-*.md`, the last SOUND). Gate: 286 / 1, failpoints 413 / 1 twice, then one intermittent failure of `s1_f12_host_early_stop_independent_of_store` under full-suite load (B ended `process_exited`, not the force row; 0 of 25 in isolation), fixed on `wt/t3-force-row` before push. Sol high reviewed the whole of Task 3
  (`t3/sol-review-T3-{control,failure,conformance}.md`; decisions 1–10 in
  [t3/t3-review-decisions.md](t3/t3-review-decisions.md)). Three Sonnet 5.5
  workers fixed rows 1–9 plus the force-row race; merged at `554a18b` (X),
  `3636b7a` (Y) and `e56efcc` (force row and decision 5), each after Sol
  medium with no open code finding (`t3/sol-review-T3-fix-{X,Y}.md`,
  `t3/sol-review-T3-force-row.md`). Runtime §7 clarified for the Host stop
  bound (`08b8fab`, `1174004`). Gate at `3a6f173`: 304 / 1, failpoints
  445 / 1 three times, F08–F12 58, release check clean. Intermittent tests
  tracked as `via-jm4.15` and `via-jm4.16`. A Sol high re-check of the fix
  delta accepted Task 3 after the runtime §7 Host bound was recorded as
  amendment A23 (`t3/sol-review-T3-recheck.md`, five rounds). **Task 3
  (`via-jm4.7.7`) is closed.** Now Task 4 (`.7.8`, claimed): design-first T4-0
  ([t4/t0.md](t4/t0.md)) by `implementer-sonnet-xhigh` in worktree
  `via-wt/t4-0` (branch `wt/t4-0`); then Astra high and Sol high design
  reviews, then implementation slices. T4-0 design rounds 1–4 are done
  (round 4 on `wt/t4-0` at `d75705d`, still UNSOUND from both reviewers;
  round-4 reviews in `scratchpad/reviews/t4/`, not yet committed). On
  2026-09-29 the owner approved [t4/requirements.md](t4/requirements.md)
  (`8d557bf`): callers are programs, only lifecycle and safety events are
  durable, and a progress snapshot plus per-step rows replace the detail event
  stream. Rounds 5–15 (Opus 5.5 high on `wt/t4-0`) redesigned Task 4 against these
  requirements, and Sol high found the design SOUND at `778752c`
  (`t4/review-r15-sol.md`). Along the way:
  - the owner approved coarse memory and disk bounds (round 8);
  - thresholds became daemon config read at start (round 9, `via-jm4.7.8.1`);
  - end-to-end measurement was deferred to `via-d9o.2.3`;
  - the per-session and shared-server lifecycle moved to `via-4sw.3.2` and
    `via-5lr.3.2`.

  Fable 5.1 high and Astra high then gave critical reviews, and both said
  "ready after named small changes". The consolidated report is
  [t4/critical-review.md](t4/critical-review.md), with the reviews in
  `t4/critical-review-{fable,astra}.md`. It lists eight required changes,
  including memory-pool liveness: tokio's fair semaphore lets a waiting
  acquire take every free permit. It also has recommended simplifications
  (drop A34 for a receiver drop; completed final text only) and owner
  questions, including the new N1–N5. The owner then revised the
  requirements (r16 marks in [t4/requirements.md](t4/requirements.md)):
  - no VIA raw log: the agents keep their own transcripts, and VIA keeps a
    per-turn evidence folder instead (D4 revised);
  - no memory pool, since memory is bounded by construction;
  - 1 MiB request lines, with a prompt file for larger prompts;
  - a disk free-space floor instead of budgets;
  - the vendor's own `steps` in the envelope;
  - `list` in creation order, with `last_active_at` on each row.

  Rounds 16–18 (Opus 5.5 high) applied the revisions and the owner's follow-up
  (design-r16-decisions.md §7: simple first, measure later; `via.log`; a large
  final text goes to a file; `wait` checks once per second). Sol high found
  round 16 UNSOUND (7 findings) and round 17 UNSOUND (3), and round 18 SOUND
  (`t4/review-r18-sol.md`). The design ([t4/design.md](t4/design.md), 1,775
  lines, amendments A24–A46) is merged into `rust-foundation` (`f42c52c`).
  Assumptions not yet observed are listed in design §16 and measured with
  every adapter in `via-d9o.2.3`. On 2026-09-29 the owner authorized
  implementation: each chunk by `implementer` (Opus 5.5 medium) and reviewed
  by Sol high until SOUND; Astra high and Fable 5.1 high critical reviews
  once, on the whole epic after all chunks merge. Spec amendments (T4-A,
  `via-jm4.7.8.2`) are merged (`ee5b8d6`, Sol r3 SOUND,
  `t4/reviews/T4-A-sol-r*.md`). The plan is [t4/plan.md](t4/plan.md)
  (`76001d0`; the old `t4/s1.md`–`s7.md` are superseded): T4-1 → (T4-2 ∥ T4-3)
  → T4-4 → T4-5 → (T4-6 ∥ T4-7) → Close → critic, Beads `via-jm4.7.8.3`–`.10`
  with T4-7 = `.7.8.1`. All chunks are merged and closed after Sol high
  SOUND (`t4/reviews/`, reports in `t4/reports/`): T4-1 (`795ddcb`), T4-2
  (`9574169`), T4-3 (`77415b9`), T4-4 (`7f5c6ed`), T4-5 (`1011484`),
  T4-flake bug `.7.8.11` (`00b9464`, `b201e59`: harness readiness never
  auto-starts a daemon; floods pace on observed consumption), T4-7
  (`8deb767`), T4-6 (`4508181`). Amendments made during implementation:
  A47 (Wire queue 1,024 messages), A48 (pipelined partial-line deadline),
  A49 (first page item always fits), A50 ("does not block" proven by
  order). Close (`.7.8.9`, `7790912`) widened the Task 4 selector in
  `.repo-context/verification.md` and wrote
  [t4/reports/T4-close.md](t4/reports/T4-close.md): gate 330 / 1,
  failpoints 521 / 1, F08–F12 56, Task 4 selector 77 (10 of 10 runs).
  The critic round (`.7.8.10`) followed. Astra high and Fable 5.1 high
  (`t4/reviews/T4-critic-*.md`) both said UNSOUND. T4-fix (`.7.8.12`, merge
  `0e0e37d`, Sol high r3 SOUND) fixed seven confirmed findings: a data walk
  that overruns is cached; diagnostics hold at most 2 of the 16 blob slots;
  evidence steps are owned; a keyed replay no longer needs its `cwd`; a FIFO
  config or log no longer hangs startup; `wait` keeps its cadence; latencies
  are proven by order. It added A51 (what F24 fills) and A52 (F24's 1 s
  control bound). Merged gate: failpoints 529 (3 of 3 runs), Task 4 selector
  85 (10 of 10). **Task 4 epic `via-jm4.7.8` is closed**
  ([t4/reports/T4-close.md](t4/reports/T4-close.md) §7–§8). Follow-ups:
  `via-jm4.19` (test runs leak host anchors; the 15 orphans left by Task 3
  worktrees were stopped on 2026-09-30 at the owner's request),
  `via-jm4.15`, `.16`, `.18`, `via-d9o.2`, `via-d9o.2.3`. Ledger: `scratchpad/execution/t4-impl/progress.md`; worker logs under
  `scratchpad/t4/<chunk>/`. Retention is deferred to `via-jm4.18`. On 2026-09-29 the owner renamed
  "frame" to vendor message/event/event trace (`.repo-context/CONTEXT.md`;
  specs and crates in `8e2bb3e`..`370903d`, `via-jm4.17` closed). The T4
  design drafts on `wt/t4-0` still say "frame"; round 5 adopts the new terms. From
  2026-09-28 new implementation slices use Sonnet 5.5
  (`implementer-sonnet` high, `implementer-sonnet-xhigh` for hard work;
  owner decision); from 2026-09-29 design-first steps go to Opus 5.5 high
  (`implementer-high`), compared with Opus in
  [model-observations.md](model-observations.md). **From 2026-09-28 no new cloud work: Claude workers are local
  subagents (`implementer`, `implementer-high`, `fable-reviewer`), Codex
  reviews in tmux** ([cloud-and-local.md](cloud-and-local.md) §7). After
  Task 3: Task 4 (`.7.8`), then the final critique (`.7.9`). After a reboot, restore tmux session
  `via` (window `main`; Sol reviews open their own windows). The cloud
  sessions are archived, so no branch watcher runs.
- **Final S1 critique (`via-jm4.7.9`, closed 2026-09-30).** Reviews and
  critiques run on Codex `gpt-6.1-sol` at high effort (owner); fixes by
  Opus 5.5 medium. Round 1 at `7370e0e`: **S1 NOT ACCEPTABLE** although
  every gate passed ([s1-critique/reviews/S1-critic-r1.md](s1-critique/reviews/S1-critic-r1.md));
  13 findings fixed in S1-specs `4d210df`, S1-core `af036af`, S1-io
  `4eb02c2` and S1-contract `78d1f9b`. Round 2, from scratch at `c226b1f`:
  NOT ACCEPTABLE, 5 findings ([S1-critic-r2.md](s1-critique/reviews/S1-critic-r2.md)),
  fixed in S1-runtime2 `edaca6b` (corruption latches on acceptance and
  terminal writes; decoded terminals and final text survive late and
  forced paths; T3 design rules tagged `[s1c.r2]`) and S1-evidence2
  `f304c93` (outcome kept beside evidence failures; one 10 s teardown
  deadline; F19/F24 bounds). Reports in `s1-critique/reports/`, reviews in
  `s1-critique/reviews/`. **Merged gate at `3b3e980`:** default 366 / 1,
  failpoints 582 / 1 (3 runs), F08–F12 61, Task 4 selector 92 (10 of 10),
  every daemon scenario pass and complete (the non-pass summaries are
  harness self-tests that must fail), no leftover processes. **Owner review
  rule (2026-09-30, revised):** a chunk's first review finds every issue in
  its code and finding classes at once; later rounds check only fixes and
  fix-introduced defects; issues outside the chunk become beads; each large
  piece of work ends with one independent critique from scratch
  ([cloud-and-local.md](cloud-and-local.md)). Follow-ups: `via-jm4.20`
  (pre-existing load flakes), `via-jm4.21` (Host journal corruption reported
  as `commit_uncertain`), `via-jm4.19` (via-cli anchor/daemon orphan class).
  Recorded harness limitations: the C1 guard's blocking connect,
  `/proc/<pid>/environ` reads, spawn and SQLite row scan outside the
  teardown bound, untested D-state reaps. Ledger:
  `scratchpad/execution/s1-critic/progress.md`.
- **Stable shared interfaces at S1 close (`3b3e980`).** S2, S3, S5 and P1
  build on these:
  - Contracts: C1 [via-api-v1.md](../../specs/via-api-v1.md), C2
    [adapter-contract.md](../../specs/adapter-contract.md) and
    [runtime-contracts.md](../../specs/runtime-contracts.md), with Task 3
    amendments A1–A23 applied, plus the T3 design rules tagged `[s1c.r2]`.
  - Crate boundaries: the six layers checked by `scripts/check-layers.py`.
    The critique changed public signatures only in
    `CancelParams.handle: Option<String>` (C1 F15 precedence),
    `StoreClient::evidence_path(relative)` (replaces `evidence()`),
    `HostTasks::tracked()`, and `via_wire::failpoint` (test builds only).
  - Result rules an adapter keeps: a decoded `completed` stays `completed`,
    with exit and cleanup as evidence (C1 §7.6 row 3); the daemon force
    gives `ForceStopped` with Host's evidence; only a real delivery failure
    is `Overflow`; SQLite corruption latches as `corrupt_store`.
- **Adapter interface design (`via-jm4.22`, closed 2026-09-30).** The owner
  asked for template-like adapters behind C2 ("the language of VIA"),
  designed from the real harnesses. Live re-probes:
  [adapters/reprobe-*.md](adapters/); tool-process lifecycle research:
  [adapters/lifecycle-harnesses.md](adapters/lifecycle-harnesses.md) and
  [adapters/lifecycle-mechanisms.md](adapters/lifecycle-mechanisms.md).
  The design [adapters/design.md](adapters/design.md), revision 9, reached
  Sol SOUND after nine rounds ([adapters/reviews/](adapters/reviews/)).
  Owner decisions are in §1 of the design:
  - new vendor versions get a cheap live check;
  - personal-setup categories are config switches (hooks and MCP off);
  - adding an adapter means a VIA rebuild; ACP comes later;
  - invariant 2 is reworded (AD12);
  - experimental vendor features are allowed;
  - OD3: agents own their processes. VIA soft-stops through the vendor,
    hard-stops only the agent's own group, and reports leftovers (AD20)
    without killing them.

  **Pending owner choice:** conflict 4 on leftover detection. A: a
  report-only marker scan that transiently reads same-uid environments,
  touching invariant 1. B: anchor-subreaper detection. C: no report this
  release. Recommended: A. It is decided in S-SPEC (`via-jm4.25`).
  Slices: `via-jm4.25` S-SPEC → `via-jm4.26` S-CORE → each `x.3.2`;
  `via-jm4.27` S-LAUNCH → each `x.3.2`; each `x.3.3` → `via-jm4.28`
  S-LEFTOVER → `via-gvg.1`/`via-d9o.1`. Unreported leftover cases:
  `via-jm4.24`. Live checks use Claude Haiku, Codex `gpt-6-luna`
  low/medium and an OpenCode free model. Working files:
  `scratchpad/execution/adapter-design/`.
- Process rules and roster: [cloud-and-local.md](cloud-and-local.md) §6
  (review loop in the author's session; merge only after review; S3 Codex
  adapter by a local Opus 5.5 medium session).

Status: **PAUSED BY OWNER, 2026-09-26. Release and S1 acceptance are incomplete.**
The goal tool is paused. Do not resume the goal, implementation, worker dispatch,
reviews or retries until the owner explicitly instructs resumption. The limits
reset is not authorization to continue. The owner authorized this preservation,
Beads refresh, handoff and local WIP commit; no push.

## Recovery and authority

Work remains in this repository on branch `rust-foundation`. The checkpoint
commit containing this document preserves the source as stopped, including
incomplete fixes; its parent is `a366d4f`. Use Git history to identify the
checkpoint hash. No source was repaired during closeout. Unrelated changes in
`.beads/interactions.jsonl` are intentionally excluded from the checkpoint.

Start with this document, [goal.md](goal.md), [roadmap.md](roadmap.md), and live
Beads. Beads is authoritative for task status/dependencies; generated tracking
pages are views, not separate task lists. In-progress issues at this checkpoint
mean unfinished work, **not live workers**. Closeout is `via-jm4.14`.

Approved architecture, Rust 1.98.1/edition 2024, coding standards and testing
policy carry forward. Governing contracts are `docs/specs/via-api-v1.md`,
`docs/specs/adapter-contract.md`, `docs/specs/runtime-contracts.md` and
`docs/specs/platform-packaging.md`; verification is in `.repo-context/verification.md`.
Do not mistake the pending shutdown proposal below for integrated shared specs.

## Scope and owner decisions

- First release: **Claude Code, Codex and OpenCode**, complete C1 through CLI
  and `via serve --stdio`: hello, describe, models, spawn, resume, steer, cancel,
  close, status, wait, result, list, events, unsubscribe, logs, daemon/status,
  daemon/stop. Capabilities must honestly distinguish supported, partial and
  unsupported behavior. No ACP, extra harnesses, passthrough, SDK or foreman delivery.
- Linux is the current required target: fully static `x86_64-unknown-linux-musl`,
  actual kernel 5.15 baseline and current Linux execution. The macOS system-library
  packaging exception is approved; artifact production, linkage inspection and
  native qualification are deferred together under `via-pvj.4`.
- OpenCode uses free models only. No-login access was demonstrated; no paid
  substitute is authorized. The chosen profile is anonymous/private with no
  ambient saved-login fallback. The owner delegated password handling to unblock:
  temporary generated local-server password inheritance is accepted with one VIA
  session per server, BasicAuth, ownership checks and no VIA secret logging.
  Same-user hostile memory isolation is not claimed. Hardening is `via-4sw.4`;
  required exception controls still belong to current adapter qualification.
- No owner answer is pending for those decisions. Missing implementation or proof
  is not an unanswered design question.

## What is preserved

| Area / Beads | Actual state at stop |
|---|---|
| Common S1 design/types, `via-jm4.7.1`–`.3`, `.7.11` | Reviewed design integrated into shared contracts; types approved by Astra medium. Closed evidence remains recorded in Beads. |
| Fake agent and evidence harness, `via-jm4.7.4` | Fake scenarios, evidence collector/runner and real CLI/daemon/SQLite tests exist. Earlier auto-start, prompt-to-result and F30 disconnect executions passed. Later cleanup/integration changes are interrupted and unaccepted. |
| Vertical slice, `via-jm4.7.5` | Partial Core/CLI/Adapter/Route/Wire runtime exists. Earlier end-to-end execution is real, but seven integration findings remain without final acceptance. Current all-target compilation fails in tests. |
| Store, `via-jm4.7.12` | Receipt, submission, acceptance, terminal, raw persistence and anchor journal implemented. Oversized raw allocation and ineffective forged-reference regression corrected and reviewed. Last scoped report: 10 tests and clippy passed. Combined acceptance remains open. |
| Host, `via-jm4.7.13` | Real anchor/ARM, identity, private control, EOF cleanup and tracked process ownership implemented. Framing cancellation, closed-watch loop, journal deadline and cancellation-safe join ownership fixes reviewed. Last scoped report: 15 tests and clippy passed. Final shutdown/report policy is pending implementation. |
| Claude, `via-p98.1/.2` | Pinned evidence and reviewed design integrated into `docs/specs/vendors/claude-code.md`. Vendor adapter implementation and required live qualification `.3.4` remain open. |
| Codex, `via-5lr.1/.2` | Pinned evidence and reviewed design integrated into `docs/specs/vendors/codex.md`. Vendor adapter implementation and required live qualification `.3.4` remain open. |
| OpenCode, `via-4sw.1/.2` | Pinned free-model evidence and reviewed design integrated into `docs/specs/vendors/opencode.md`. Vendor adapter implementation and required live qualification `.3.4` remain open. |
| Platform, `via-pvj.3.1` | Actual KVM Ubuntu 22.04 x86_64 kernel 5.15.0-1106-kvm runner established; task closed. No VIA release artifact has been tested there. Platform implementation/qualification remains open. |
| Later hardening/release, `via-gvg`, `via-d9o` | Not complete; release finish criteria in goal.md remain unchanged. |

The production route currently exercised is fake. Native vendor probes are
protocol evidence, not implemented VIA adapters or release qualification.

## Existing review findings and interrupted fixes

The [preserved Task 1 review](checkpoint/task1-review.md) records owning-layer
fixes and the integration findings. Its intermediate wording and source line
numbers are historical; **no combined Task 1 acceptance exists**.

| Finding | Work still requiring completion and verification |
|---|---|
| T1-I1 | Half-close input, then bounded concurrent stdout/stderr drain and terminal validation; do not lose tails/duplicate terminals or hang on stderr floods. Wire EOF must preserve both streams. |
| T1-I2 | Client checks daemon peer UID before sending hello or any handles. Server-side checking alone is insufficient. |
| T1-I3 | Strict current C1 request envelopes/parameters: jsonrpc, request ID/type, unknown-field rejection and actual DTO validation. |
| T1-I4 | Remove unauthorized cleanup debug RPC/CLI and prove independent cleanup after daemon-first death through the approved outer snapshot/private-control seam. Debug strings were absent in the last narrow inspection, but Core::verify_cleanup remains and the replacement harness strategy is unreviewed. Reopening Core/Store is not automatically an approved independent read-only proof. |
| T1-I5 | Emit C1 turn.started/turn.ended and required common event fields while retaining durable C2 evidence; preserve assistant/tool/unknown observations. turn.terminal remained in the last source inspection. |
| T1-I6 | Complete honest required receipt/envelope capabilities, effective settings, timestamps, usage/cost, model and raw-span shapes; use explicit unavailable/null semantics where appropriate. |
| T1-I7 | Force stop actually enters forced shutdown immediately; bypassing admission refusal then waiting normal drives does not implement force. |

Some source edits toward these fixes may already be present. Inspect the frozen
diff against each finding after resumption; do not reapply changes blindly or
mark a finding closed from a worker heartbeat, old tests or a string search.

Astra high produced a [shutdown ownership correction](checkpoint/shutdown-ownership-seam.md)
and Sol high returned [PASS on the design](checkpoint/shutdown-ownership-sol-review.md).
**It is preserved but not integrated into shared specs or accepted implementation.**
It distinguishes live-daemon caller timeout/cancelled shutdown (retain joins and
capacity ownership) from final daemon exit. The stop receipt is acceptance only;
drain keeps existing work deadlines; force enters final shutdown immediately.
Final shutdown has one total 10-second deadline (F12 starts at first Store
failure), clean exit 0 requires positive cleanup/joins/durability, and only daemon
main may select truthful incomplete exit 4. Preserve partial recovery, pending
and failed joins and wait errors; never infer reaping/quiescence from adoption,
abort or dropped handles. Blocking Store Drop stays off Tokio workers. No new
supervisor, RPC or status fields. Positive F19–F22/P-I2 gates remain mandatory.

## Verification: current versus historical

Checkpoint preservation checks, 2026-09-26:

- `cargo fmt --all --check`: **PASS**.
- `cargo check --locked --offline --workspace --all-targets`: **FAIL**, E0599 at
  `crates/via-cli/tests/s1_prompt_to_result.rs:134` and `:676`.
  `ScenarioError` lacks Display for `error.to_string()`. Left untouched under
  the owner's stop instruction; this checkpoint is deliberately WIP.
- `python3 scripts/check-layers.py`: **PASS**.
- `python3 .claude/scripts/skill-catalog.py --check`: **PASS**, advisory stale
  allowlist warnings only.
- Final Markdown link, diff and staging checks are recorded in closeout Bead
  `via-jm4.14`. No new runtime acceptance suite or review cycle was started.

Historical earlier snapshots only: workspace fmt/clippy/nextest **51/51**,
cargo-deny and layers passed before later integration fixes. Store last scoped
10 tests/clippy, Host last scoped 15 tests/clippy, fake-agent 12 tests and
collector/runner 4 tests passed at their respective freezes. Three real CLI
scenarios passed at an earlier integration snapshot. Review still found the
contract defects above. These are **not** a green certification of this commit,
full S1 F1–F30 acceptance, or release acceptance.

## Vendor and Linux evidence to retain

| Pin | Evidence and remaining boundary |
|---|---|
| Claude Code 2.1.283 | Six conformance cases passed, three partial; resume/schema/interrupt evidence. Required bounds and VIA live qualification remain `via-p98.3.4`. |
| Codex 0.157.1 | Native steer and persistent resume evidenced. Tool survived interrupt, so cleanup remains uncertain. Denied-write read-only proof and VIA live qualification remain `via-5lr.3.4`. |
| OpenCode 1.18.32 | Official `opencode/mimo-v2.6-flash-free` worked with private HOME/all XDG including DATA, private DB, no login/payment or spoofed headers. Free conversation probe: 12 pass, 2 unproven; active tool cancellation: 9 pass, child absent before shutdown and owned group/listener cleanup. Earlier free403/paid timeout reports are historical, not current access blockers. |

OpenCode single-step assistant usage repeats step-finish usage; do not double
count. Multi-step accounting/cost/billing scope remains unproved. `via-4sw.3.4`
still owns usage/B7/controls/exact permissions/temporary-exception controls and
hostile-profile/restart qualification through VIA. Non-null max_steps is
truthfully unsupported at this pin and must fail before I/O with `-32602`,
`data.kind: invalid_params`; that does not waive the C1 verb surface.

The Linux baseline uses a signed official image, pinned SHA256
`be270d5d6d81673914a63e838dd80fa35c571a95c4401a0e538dd15a20715721`.
Task containers and overlays were cleaned; the image/runner are retained locally.
A baseline boot is infrastructure proof, not a VIA artifact pass.

Local-only evidence lives under `scratchpad/execution/rust-foundation-release/`:
`s1-review/` source hashes and reports, `s1-design/` prior ownership seams,
`opencode-evidence/free-tier-report.md`, vendor probes, CLI scenario manifests,
raw/event logs, Store snapshots and platform artifacts. These paths are
intentionally gitignored and may not exist on another machine. The committed
vendor specs and checkpoint review/design snapshots preserve conclusions;
recover or reproduce raw evidence before relying on it for a new acceptance.
Do not publish credentials, raw private transcripts or machine-local paths.

## Worker ownership and next action after explicit resume

No active child worker remains. `/root/s1_spine` and `/root/s1_host` stopped on
usage limits with partial edits preserved. Store, integration reviewer, internal
design and platform reviewer had finished their assigned turns. Do not restart
any worker during this pause.

Resume method: `execution` and Beads; risk-appropriate failure-first regressions,
then the established substantial-increment review workflow. Material designs:
Astra high designs, Sol high reviews. Code: Sol high implements, Astra medium
reviews substantial increments. Astra high critiques completed S1 and the release
candidate, not each small edit. Native named-model agents were available in this
run; explicit model/effort and native-first routing were owner-authorized, with
Codex CLI fallback only if unavailable. Recheck availability when resumed.

After explicit resumption, recover the current diff and `via-jm4.7.4/.5/.12/.13`.
First restore test compilation through the owning implementation worker, then
integrate the reviewed shutdown correction and complete T1-I1–I7 with bounded
ownership: Host owns Host source/tests; Store owns Store source/tests; spine owns
Core/CLI/Adapter/Route/Wire, manifests and shared integration. Coordinate event
mapping with Store and cleanup evidence with the harness; no overlapping writes.
Do not redispatch completed research or duplicate existing changes.

`.7.4` and `.7.5` are one Task 1 integration group; `.7.6` waits for both.
`.7.5` also waits for Store `.7.12` and Host `.7.13`; avoid inventing a circular
harness-cleanup dependency. Run the prescribed current-tree gates and obtain
Astra-medium combined review only once the substantial increment is ready.
Continue the remaining S1 acceptance and milestone critique before adapter
implementation. Independent later vendor implementation can parallelize after
its prerequisites, with shared files assigned to one owner. Final finish means
all goal.md gates evidenced, not merely a build, partial feature or usage reset.
