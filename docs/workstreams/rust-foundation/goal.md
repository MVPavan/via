# VIA first-release goal

Status: **PAUSED by owner, 2026-09-26; goal tool paused; release incomplete**.
Do not resume implementation, dispatch workers or start reviews until the owner
explicitly resumes. The owner authorized preservation, Beads updates, a handoff
and a local WIP commit only; no push. See [the current handoff](session-handoff.md).

## Goal-tool objective

> Execute `docs/workstreams/rust-foundation/goal.md` using its Beads dependency
> graph and `docs/workstreams/rust-foundation/roadmap.md`. Deliver a locally
> verified VIA first-release candidate for Claude Code, Codex, OpenCode and Pi,
> with the complete C1 API and truthful adapter capabilities. Continue through
> design, implementation, integration, verification, review and necessary fixes
> until every finish criterion below is evidenced. Use the specified models,
> review cadence and safe parallelism. Preserve unrelated work. Do not mark
> the goal complete because a budget/reset approaches, because code compiles,
> or because an external gate is unavailable. Publishing and Git commits,
> pushes and merges require separate authorization.

The coordinator reads this whole document on activation or resumption. No numeric token
budget is inferred from the owner's estimate of remaining usage. The live goal
tool's budget, status and continuation rules remain authoritative.

## Starting point and authority

- Rust 1.98.1/edition 2024, the six-layer architecture plus Store, the S1 C1/C2
  contract set, coding standard and failure-first testing policy are approved.
  Reuse those decisions; do not repeatedly seek approval for them.
- S0 contains seven scaffold crates and `via --version`; no runtime feature
  implementation or runtime tests exist at goal preparation.
- First-release harnesses are exactly **Claude Code, Codex, OpenCode and Pi**.
  ACP, other harnesses, passthrough, thin SDK delivery and foreman integration
  are outside this goal. Extra routes are added only when required for the
  in-scope behavior; there is no separate ACP/Claude-extra-route finish gate.
- Governing sources: `AGENTS.md`, `.repo-context/invariants.md`,
  `.repo-context/coding-style.md`, `.repo-context/verification.md`,
  `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md`, and S1's
  `docs/workstreams/rust-foundation/s1-plan.md`.
- Work remains in this repository on `rust-foundation`. The owner pushed the
  pre-goal branch; preserve all pre-existing edits and other worktrees/branches.
- The owner's goal activation authorizes the scoped implementation, tests, focused conformance
  probes, local artifacts and fixes. It does not authorize publishing, messages
  to others, credential extraction, destructive cleanup or unrelated changes.
  New throwaway prototypes still require owner discussion. Lack of a provider
  login, platform runner or required permission is an explicit unmet gate;
  advance independent work and request only the missing input/authority.

Current checkpoint (2026-09-26): reviewed common types, vendor designs and
protocol evidence are preserved. S1 has partial runtime code and real fake-agent
CLI/daemon/Store tests. Store and Host scoped fixes passed their recorded reviews;
Task 1 integration remains unaccepted after interrupted fixes. The frozen tree
fails all-target compilation at two `ScenarioError::to_string()` calls in the CLI
test harness. Historical green tests do not certify this checkpoint. All workers
are stopped. No vendor adapter or release artifact is complete.

Actual Linux kernel 5.15 baseline infrastructure is established, but no VIA
release artifact has been qualified there. Linux remains the only required
release target; macOS artifact, linkage inspection and native qualification are
deferred together under `via-pvj.4`. OpenCode no-login free-model evidence is
recorded; its VIA qualification and temporary password-exception controls remain
open. These dispositions do not reduce the full-C1, four-harness finish criteria.
The anonymous/private OpenCode profile has no ambient saved-login fallback;
non-null `max_steps` is truthfully unsupported at the pinned vendor version.
See the handoff for precise evidence, outstanding review findings and next work
only after explicit resumption.

## Product surface

Implement C1 `hello`, `describe`, `models`, `spawn`, `resume`, `steer`, `cancel`,
`close`, `status`, `wait`, `result`, `list`, `events`, `unsubscribe`, `logs`, `daemon/status`
and `daemon/stop`; expose the documented CLI and `via serve --stdio` proxy.
Foreground and background operation are both required.

Every harness must demonstrate spawn, conversation-preserving resume and result.
Implement every supported control verb. Each route explicitly declares native,
partial (with precise semantics), or unsupported capabilities. A named refusal
for a genuinely unsupported vendor verb is required behavior, not an excuse to
leave supported verbs unimplemented. Never replace steer with a new turn or
present process termination as proof of graceful cancellation. `--require` and
bound validation obey C1. Usage/cost and cleanup certainty retain their provenance.

## Finish criteria — all required

| Gate | Evidence required to finish |
|---|---|
| Functional API | Automated C1 coverage through CLI/socket/stdio, including receipts, foreground/background, errors, handles, retries, queueing, controls and read APIs; follow/unsubscribe and disconnect release subscriptions without further event delivery; no placeholder paths |
| Four real adapters | Pinned-version fake conformance and small live sets for Claude Code, Codex, OpenCode and Pi; real conversation continuity and supported controls; OpenCode successful result and continuity use a free model, and Pi uses `gpt-6-luna` through the OpenAI login (owner, 2026-10-04: Pi's OpenCode login is unavailable), with no other model/provider substitution absent a new owner decision; capability matrix matches evidence |
| Lifecycle correctness | S1 F1–F30 plus vendor-specific failure tests pass; no automatic resend after uncertain submission, no cross-session traffic leak, verified process identity, truthful cleanup/unknown outcomes |
| Bounded operation | Measured memory/buffer/deadline limits from the reviewed design; noisy vendors and slow consumers do not silently lose data or stall lifecycle control; exact logs or explicit incompleteness |
| Durable state | Migration, crash, receipt/intent ordering and persistent Store-failure scenarios pass, including the inability to persist a failure result; restart never invents evidence |
| Platforms and packaging | Reviewed Linux target/linkage/install contract; produced fully static `x86_64-unknown-linux-musl` artifact; actual kernel 5.15 baseline and current Linux execution; platform socket/process/install tests and release failpoint exclusion. Retain macOS design/compatibility guidance; macOS artifact production, linkage inspection and native qualification are deferred follow-up gates, not current finish gates |
| Verification | Full prescribed fmt/clippy/nextest/deny/layer gates; explicit failpoint coverage; nonempty default suite around the coding-standard two-minute budget; small live gates and current-goal Linux native/artifact gates pass; infrastructure failures never count as passes |
| Evidence and docs | Per-scenario summaries, raw/event logs, consistent Store backups, hashes/manifests and reports as coding-style §10 requires; sanitized shared fixtures; Linux artifact/native evidence and explicit deferred/unverified macOS status; CLI help and usable login/spawn/resume/background/handle/bounds/cancel/recovery instructions |
| Independent review | Sol-high review of material designs; Astra-medium review of substantial implementation increments; Astra-high critiques at S1 completion and the integrated release candidate; every blocker fixed and verified |
| Tracking and handoff | All in-scope leaf issues meet acceptance and close with evidence; parent tasks/epics reconcile; macOS production/inspection/native acceptance is separately linked under deferred `via-pvj.4`; final diff/status and local release report identify checks, versions, artifacts and accepted limitations |

Linux and macOS remain the decided platform family. P-OWNER-1 accepts the
narrow macOS system-library linkage exception in invariant #4, but Linux is
the only required target for this goal. Its `x86_64-unknown-linux-musl`
artifact must be fully static and run on the actual kernel 5.15 baseline and
current Linux configuration, with all required platform and vendor gates.
The macOS ARM64 artifact, linkage inspection and native qualification are
deferred together; design and allowlist guidance remain in
`docs/specs/platform-packaging.md`. No Linux, WSL or cross-build result proves
macOS compatibility.

Process-group supervision does not prove containment of descendants that escape
the group; that limitation must be explicit. Cancellation acknowledgement does
not prove tool quiescence. Optional live rejoin is not needed for v1: approved
`unknown` recovery is sufficient when evidenced and never resent.

Unavailable required Linux runners, fixtures or free-model/provider access
leave their corresponding current-goal gates incomplete. No-login free-tier
OpenCode access has been evidenced, but VIA's free-model adapter qualification
remains open. Missing macOS artifacts/runners are deferred under `via-pvj.4`
and do not block this goal. Never count infrastructure failure or deferred work
as a pass; all remaining current-goal gates must hold before completion.
Follow the goal tool's actual blocked-status rules.

## People, models and reviews

| Responsibility | Executor | Required output / review |
|---|---|---|
| Coordination | Active root agent, orchestration only | Beads, briefs, delegated work, integration, conflict resolution, evidence and finding disposition |
| Material specification or architecture | GPT-6 Astra, high | Bounded design grounded in current code/specs/probes; reviewed by GPT-6 Sol, high, before dependent code |
| Code and tests | GPT-6 Sol, high | Failure-first implementation with owned paths; reviewed by GPT-6 Astra, medium, at a coherent substantial increment |
| Major critical review | GPT-6 Astra, high | Fresh-context critique after S1 and at release candidate; review product guarantees, architecture and evidence, not just style |

Small fixes and mechanical changes stay with the active increment. Do not ask
for Astra-medium review on every edit or Astra-high critique on every task.
Review findings return to the responsible implementer/designer; re-review the
finding and affected behavior. Required fixes are part of the goal. The design
author's confidence is not an independent critical review: use a fresh reviewer
context and primary evidence.

The root delegates design, implementation and reviews to the roster above;
it coordinates work and verifies outcomes without taking an implementation
slice. Workers return summaries of at most 180 words with artifact pointers.
The root reads large evidence only when a finding, failure or integration
decision needs it.

Skills: execution and planning for tracked slices; codebase-design and
document-review for material contracts; test-driven-development for risky
behavior; code-review for substantial code; systematic-debugging for unclear
failures; security when trust-boundary changes need it. First-principles thinking
guides whether the implementation actually fulfills the product guarantees.
No council or broad redesign is required for routine work.

## Spawning and capacity

Direct spawning was exercised during goal preparation on 2026-09-26:

| Requested configuration | Bounded work completed |
|---|---|
| `gpt-6-astra`, `high` | Goal/dependency/finish-criteria design |
| `gpt-6-sol`, `high` | C1/vendor unknowns and scheduling inventory |
| `gpt-6-astra`, `medium` | Current source and verification infrastructure inventory |

These were real completed read-only child tasks, not just catalog checks.
Launch configuration and completion are verified; this is not independent
attestation of the serving backend's model identity. No CLI fallback was needed.
Their results are inputs to this plan, not runtime implementation acceptance.

The owner explicitly requested native spawning first in the goal-document
discussion; this supersedes the prior CLI-only rule and is reflected in
`.repo-context/running-codex.md`. Use native `spawn_agent` first, with explicit
model/effort and a bounded brief.
If it fails or cannot represent the requested model/effort, use direct
`codex exec` following `.repo-context/running-codex.md`, retaining session ID,
requested settings, exit status and output artifact. Do not silently substitute
models or route through a different plugin. Do not retry uncertain mutations.

The session currently exposes seven concurrent-agent slots including the root.
Use up to six children when six independent useful tasks exist; this is capacity,
not a quota to fill. Recheck availability on resumption. Each brief contains:
outcome, owned paths, dependencies, source pointers, model/effort, acceptance
commands, output artifact and explicit preservation of other workers' changes.
Workers do not stage/commit/push or mutate Beads. The coordinator collects every
result, inspects evidence and owns integration. Keep public artifacts free of
private streams and credentials.

At goal preparation, the owner estimated about 60% usage remaining before a
reset in a couple of hours. Treat that as a historical scheduling preference,
not measured current capacity or a hard
deadline. Prefer useful ready work and high-risk evidence; keep independent
workers busy without manufacturing tasks, weakening checks or duplicating work.
Do not idle waiting for the reset when authorized work is ready. Check worker
metadata at meaningful checkpoints; inspect detailed logs on state changes,
failure or completion rather than continuously polling.

## Dependency and parallelism rules

1. The owner reviewed and activated this goal; feature dispatch follows the
   Beads dependency graph and the authority boundaries above.
2. With activation, S1 internal design, three vendor evidence packets and the
   platform/package design can start in parallel. A vendor code task waits for
   its probe/design/review; a platform implementation waits for its contract.
3. S1 design → Sol design review → contract types and test-harness preparation
   (disjoint paths in parallel) → prompt-to-result → queues/retries → lifecycle
   and recovery → streams/full C1 → S1 critical review. Each substantial code
   increment includes Astra-medium review/fixes before dependent work advances.
4. After verified S1 and the corresponding vendor design reviews, Claude,
   Codex and OpenCode adapter work can proceed in parallel on separate adapter,
   route and fixture modules. Shared registry/types/manifests/Core/Host/Store
   have one assigned writer at a time; the coordinator schedules those joins.
5. Cross-adapter control/recovery hardening follows completed adapters;
   platform implementation and packaging proceed independently where their
   actual prerequisites allow. Documentation follows stable user behavior.
6. Final integration joins all four adapters, control hardening, platform
   checks, live evidence and docs before the final Astra-high critical review.

This graph replaces the old mandatory serial S2 → S3 → S4 → S5 proposal.
Slice numbers identify ownership; dependency edges, not numbering, determine
dispatch. The roadmap maps these slices to actual Beads IDs.

## Tracking, resumption and closeout

The preparation baseline mapped seven epics, 23 tasks and 40 subtasks; completed
S0 history remains in the existing foundation epic in addition to these units.
New separately ownable work may add tasks or subtasks as the goal proceeds.
The full acceptance/ownership map is [roadmap.md](roadmap.md).

| Epic | Scope |
|---|---|
| `via-jm4` | S1 foundation, existing history and cross-slice design/evidence registers |
| `via-p98` | Claude Code adapter |
| `via-5lr` | Codex adapter |
| `via-4sw` | OpenCode adapter |
| `via-jt8` | Pi adapter and conformance |
| `via-gvg` | Cross-adapter controls and recovery |
| `via-pvj` | Linux/macOS and packaging |
| `via-d9o` | Integrated release verification and final critique |

`via-jm4.10` records the owner's approval and activation. The first design leaf
is `via-jm4.7.1`; vendor evidence and platform design may start alongside it
under the dependency graph.

Use **epic → task → subtask**, at most three levels. Beads is the state source;
the roadmap is the design/acceptance map, not a manually maintained status board.
Reuse existing foundation records. Add subtasks for separately ownable work,
not each shell command or each tiny fix. Do not close parents while required
children or acceptance evidence remain incomplete.

Before dispatch, claim the ready leaf and include its ID in the brief. Record
completion artifacts, checks, findings and next dependency in that issue.
Before context/usage reset, persist worker/session identifiers, exact progress,
current changes, verified checks, unresolved blockers and next ready tasks;
export `.beads/issues.jsonl` and update the session handoff. Never infer that a
worker stopped or succeeded merely because the coordinator lost context.

On resumption read this goal, the roadmap, current handoff, `bd prime`, active
issues and any surviving worker state. Continue the same goal and approvals.
No repeated broad research, recovery round or expanded scope just to spend
remaining capacity. At completion, produce a concise evidence-linked local
release report. Commit/push/merge/publication remain separately authorized.
