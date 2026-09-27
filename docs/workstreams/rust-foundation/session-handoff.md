# Rust foundation — paused checkpoint

## Current state (2026-09-27, resumed)

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
- Contract text still to record (implemented, not yet in specs): prelaunch
  force is `requested`/`quiescent`; force after a Store failure is
  `failed(store)` with `cancel` filled; a `wait` pending at final shutdown
  ends `daemon_stopping`; `session.closed.reason` `daemon_stop_force`;
  `vendor.other.truncated`; force abandoning a post-ARM acquisition marks
  the raw log incomplete conservatively (bytes *may* be lost); the 256
  unresolved-turn cap refuses with `admission_refused`.
- Next: Sol high review of all of Task 1 (`via-jm4.7.5`) → fix → record the
  contract text above → close Task 1. Then split `via-core/src/engine.rs`
  by responsibility; then `.7.6`–`.7.8` (failpoint controller first), then
  `.7.9`. After a reboot, restore tmux session `via` (windows `main`, and
  `watch` running `scratchpad/cloud/watch-branches.sh`).
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
