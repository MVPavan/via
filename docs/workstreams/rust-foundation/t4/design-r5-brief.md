# T4-0 design round 5 brief: redesign against the owner's requirements

Worker: `implementer-high` (Opus 5.5 high; owner decision 2026-09-29).
Worktree `/data/codes/via-wt/t4-0`, branch `wt/t4-0`. This step writes **no
production code and no tests**, and does not edit `docs/specs/`.

## Why this round exists

Rounds 1–4 designed Task 4 on the assumption that every C1 event is durable,
ordered and followable. Round 4 was still UNSOUND from both reviewers, and
most of its weight sat in that assumption: follower permits, the lag rule, the
Public lane, and reply accounting for follow.

The owner has now approved `t4/requirements.md`, which removes the
assumption. Read it first; it is normative for this round. Where it
conflicts with C1, the runtime contracts, C2 or an earlier design, the
requirements win. Record each such conflict as a proposed amendment.

## Outputs (commit on `wt/t4-0`, then stop)

1. **`t4/design.md`, rewritten as round 5.** It is normative and
   self-contained. Start from round 4 and apply first-principles thinking to
   every mechanism: keep it, simplify it, or remove it, and give a one-line
   reason tied to a requirement or a contract. Simplicity is a goal. Prefer
   existing mechanisms and coarse, provable bounds over new machinery. The
   result should be materially shorter than round 4 (2,570 lines).
2. **`t4/reports/T4-0.md`**: add a round-5 section with:
   - a removed / kept / changed / added table, with reasons;
   - the disposition of every round-4 finding (`t4/review-r4-astra.md`,
     `t4/review-r4-sol.md`): removed with the mechanism, fixed (where), or
     still open;
   - open questions for the owner.
3. **Slice files `t4/s1.md`–`s7.md`:** do not revise them. Put one line at the
   top of each: "Superseded by round 5; re-planned after owner review." Slice
   planning happens after the owner reviews the design.

## What the design must cover

- **Durable events (R1, R2).** Which observations Core commits as events, and
  which it folds into the snapshot or the envelope. Also what the Adapter →
  Core observation budget (C2 A1) becomes once detail observations are no
  longer committed.
- **Progress snapshot (R3).**
  - Its owner, and how `status` reads it without blocking the turn or taking
    a Store round trip.
  - The step rule, the same for every vendor: the step count goes up when the
    model produces output after tool results. Give the mapping for the fake
    route and for Claude Code, Codex and OpenCode, from the vendor specs.
  - Token accumulation with scope labels, about 95% accurate.
  - Its bounds.
- **Step rows (R4).**
  - The `steps` table in schema v6, keyed by session first.
  - Where the step-end write sits in the single writer's ordering relative to
    `turn.ended`.
  - What survives a crash.
  - Retiring a session's rows must be a single keyed delete. Only design for
    that; the retention job itself is `via-jm4.18` and is not built here.
- **Caller interface (R5).**
  - `wait`, `status` (snapshot plus step history, paged if needed), and
    `events` pages with no follow stream.
  - `logs` raw excerpts, undecoded.
  - What happens to `unsubscribe`, `event_end`, follow notifications and
    disconnect cleanup of followers.
- **Remaining bounds (R7).** Keep a single memory and disk bound table, with a
  proof for each. The bounds cover message splitting, the raw log, the
  envelope, blob files, the snapshot, step rows, and Store requests and
  replies. Every other Task 4 scope item in `t4/t0.md` (F5, F24–F27, C1 method
  conformance, the carried `via-jm4.7.8` items) either stays with a design,
  or is marked obsolete with a reason.
- **Amendments.** Draft the exact replacement text for every affected spec
  section:
  - C1 §3.7, §3.8, §3.11, §5 (`event_seq` references, the envelope's `events`
    field) and §6;
  - runtime §9 and anything else that changes;
  - C2, and the three vendor specs' observation mappings.

  Number new amendments from T4-A24. Restate whether T4-A22 (`ENVELOPE_MAX`
  704 KiB), T4-A23 (`process.cleanup`) and A12 (`list` paging) still apply,
  or change.

## Constraints

- Terms follow `.repo-context/CONTEXT.md`: vendor message, observation,
  event, event trace, step, raw log. Never "frame".
- Do not contradict `t2/dispatch-design.md`, `t3/design.md` (A1–A23) or
  `.repo-context/invariants.md` except through a numbered amendment.
- Verify each claim about current code against the code (file:line). Verify
  each claim about a vendor against its spec in `docs/specs/vendors/`, and
  mark anything the spec has not probed.
- Commit with explicit paths (no `git add .` or `-A`, no `--no-verify`). End
  the message with:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
  Do not push or merge.

## Report back

- the commit id(s);
- the design's line count;
- the removed / kept / added summary;
- the new amendments;
- the round-4 finding dispositions;
- open questions;
- anything in the requirements you believe is wrong or unsafe, with the
  reason.
