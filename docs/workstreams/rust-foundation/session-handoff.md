# Rust foundation: session handoff (2026-09-26)

Read this first to resume. It is self-contained for any harness or model;
repository rules still come from `AGENTS.md`. Tracking: Beads epic
`via-jm4` (`bd show via-jm4`). Written by the orchestrator (Claude Opus 5.5)
at the end of the session that moved VIA work out of the coding-ritual
submodule.

## 1. Where things are

- **Repo:** this checkout (the original VIA clone, now the only place VIA work
  continues). Branch **`rust-foundation`**, based on `main` (824d064). Nothing
  from this branch is pushed or merged; pushing and merging need the owner.
- Commits on the branch: design decisions (f2e3614), specs draft 2 (9dbeac4),
  Rust S0 workspace and coding standard (be5787d), Codex harness (d158962),
  Beads (ed4b020, a7173f8), plus this handoff.
- Other branches: `spike/sqlite-wal` (Go SQLite spike, report pending merge;
  moot for the language choice, still evidence for SQLite WAL) and
  `claude/repo-context` (merged content). Leave both alone.
- **Stale copy:** the parent `coding-ritual` repo has a worktree
  (`coding-ritual-via`, parent-repo path) with VIA as a submodule at `via/`.
  It holds the same commits plus its own Beads database. Do not work there.
- Gitignored scratch material lives in `scratchpad/` (map in §8).

## 2. Goal and phase

VIA is one Rust binary: a per-user daemon plus a CLI that spawns and controls
coding agents (Claude Code, Codex, OpenCode, ACP agents) through one stable
API. Current phase: **foundation**: decisions recorded, contracts drafted, S0
workspace built. **No feature code exists yet, by design:** the owner reviews
the contract specs before implementation starts.

## 3. Decisions in force

Recorded in `docs/brainstorms/README.md` §15, `.repo-context/invariants.md`
and `.repo-context/CONTEXT.md` (glossary). In short:

| # | Decision |
|---|---|
| D1 | One **VIA daemon** per user owns every agent process, vendor connection and the Store (its only writer). CLI, `via serve --stdio` and thin SDKs are clients over a user-only Unix socket. Auto-start, idle exit, version handshake, one binary (`via daemon …`). The daemon is a process, not a layer |
| D2 | No leases. OS file lock (`std::fs::File::try_lock`); after a crash Core decides each in-flight turn (resumed / unknown / failed); unknown is never re-sent |
| D3 | "Never ask": out-of-bound actions are denied, not prompted. Vendor requests get an automatic decline under a deadline; denials are listed in the envelope; the caller resumes, optionally with a new bound |
| D4 | Raw log per connection (exact bytes) plus event log per turn |
| D5 | **Session** (one conversation; keeps route and adapter version for life; the bound carries over per turn unless the caller sets a new one on resume, recorded per turn) and **Turn** (one prompt → one envelope, `s_7f3/2`). "Run" is not an entity. Claude's `--max-turns` counts **steps** |
| D6 | Names: L1 Interface (client + server halves), L2 **Core**, L3 Adapters, L4 Routes, L5 Wire, L6 Host, side Store; contract C1 **VIA API** (public, v1), C2 Adapter, C3 Route (a family), C4 Wire, C5 Host, S Store. Crates `via-cli`, `via-core`, `via-adapters`, `via-routes`, `via-wire`, `via-host`, `via-store` |
| D7 | Accepted layer-review findings: Core owns deadlines, queues, admission; adapters run vendor cancel sequences; never kill a shared server to cancel one turn; `close(mode, deadline)` passes down; `spawn --require`; three Host process shapes; ACP-only harnesses bound `full` only |
| D8 | **Rust** 1.98.1, edition 2024. Routes: vendor server where one exists, else vendor CLI, native ACP for breadth; no SDK routes or bridged ACP for now. VIA starts its own servers and never attaches to or stops others. Roles are caller policy |
| D9 | Open: external sandbox (e.g. `bwrap`) for OpenCode; vendor servers on stdio vs Unix socket |

## 4. Done, and how it was checked

| Work | Where | Checked by |
|---|---|---|
| Decisions applied to invariants, glossary, handoff properties, layer/names, routes decision, layer design v2 | `.repo-context/`, `docs/brainstorms/`, `docs/workstreams/handoff.md` | Consistency pass; 0 broken links; Mermaid validated |
| C1 VIA API v1 + C2 Adapter contract, **draft 2** | `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md` | Drafted by Claude Fable 5.1 high; reviewed by GPT-6 Astra medium (SOUND WITH CHANGES, 4 blocking + 15 major); every finding fixed or turned into an owner decision |
| Rust coding standard | `.repo-context/coding-style.md` (routed from `AGENTS.md` "Code changes") | Reviewed by Astra medium and GPT-6 Sol high (both SOUND WITH CHANGES); revisions applied |
| S0 workspace: 7 crates, `via` binary, workspace lints, `clippy.toml`, `deny.toml`, nextest config, `scripts/check-layers.py` | repo root, `crates/` | Gate in `.repo-context/verification.md` passes (fmt, clippy `-D warnings`, nextest, cargo-deny, layer check); `via --version` works |
| Codex harness modelled on the DWS layout | `.codex/` | Hooks fire in a `codex exec` smoke test; see §7 for gaps |
| Vendor probes P1–P5, P2b | `scratchpad/probes/` | Run once each on cheap models (evidence, not guarantees): §6 |

## 5. Waiting on the owner (next session starts here)

1. **Spec approval.** Summaries sit at the top of both spec files. Decisions
   P1–P13 (C1) and A1–A8 (C2), all recommended "as written". The one real
   choice: **P7**, after a Codex cancel whose tool keeps running, either
   wait up to 60 s and then dispatch the next turn with a warning
   (recommended) or block until the caller acts.
2. **Testing policy.** `.repo-context/coding-style.md` §10 is marked PROPOSED.
   Owner's direction: never write unit tests after the code; end-to-end
   tests as the main mechanism with a verifiable, repeatable artifact; if
   something must be tested in isolation, list its failure modes first. Both
   reviewers objected to a blanket "E2E only". The reconciled proposal
   allows small failure-first tests where sharper or cheaper (byte framer,
   state machines, migrations, message mapping), and a smallest failing
   reproduction before every bug fix. Confirm or amend, then remove the
   PROPOSED label and `NEXTEST_NO_TESTS=pass` once tests land.
3. **Merge/push** of `rust-foundation`, and the `spike/sqlite-wal` report
   merge: owner's call.

## 6. Probe evidence (Codex 0.156.1, Claude Code 2.1.283)

| Probe | Observed | Consequence |
|---|---|---|
| P1 Codex app-server, `approvalPolicy: never` + read-only | 0 server requests in two turns; write blocked; turns completed | Never-ask works; auto-decline stays as a safety net |
| P2/P2b Codex `turn/interrupt` during `sleep 120` | Turn `interrupted`; the tool's `sleep` still alive after 60 s; `command/exec/terminate` (-32600, only for client-started commands) and `thread/unsubscribe` do not stop it; tool runs under `bwrap --die-with-parent` in its own process group | Cancel = acknowledged, cleanup uncertain (spec P7) |
| P3 Codex app-server stdin closed mid-tool | Server exits, tool dies | A daemon crash takes stdio-attached Codex turns with it (spec P12/A7; D9) |
| P4 Claude parent SIGKILLed mid-tool | Claude survived as an orphan; its tool died. Claude refuses a bare `sleep 120` on its own | Recovery must find and kill orphans by verified identity (coding-style §6) |
| P5 Claude stream-json input while busy | Second message merged into the running turn; `control_request` interrupt succeeded and killed the tool (`error_during_execution` / `aborted_tools`); init advertises `interrupt_receipt_v1` | Core holds queued prompts; Claude steer unsupported; interrupt supported (spec A3) |

## 7. Known issues and follow-ups

- `via-bki`: in the Codex smoke test the model did not report the Beads
  SessionStart context in VIA (it did in DWS), and neither repo's model
  listed `.codex/skills`.
- The spec status lines cite `scratchpad/rust-foundation/decisions.md` and a
  scratchpad review path; both are gitignored. Re-point them to
  `docs/brainstorms/README.md` §15 and copy the reviews into
  `docs/brainstorms/reviews/` when the specs are finalised.
- `scratchpad/mermaid-check/` needs `npm install` (node_modules not copied).
- Beads issues open under `via-jm4`: `.2` (spec, awaiting owner), `.3`
  (probes; remaining B1–B8 vendor questions listed in C2), `.4` (standard;
  testing section awaiting owner). `via-str` (Go spike) is obsolete for the
  language decision; the owner decides whether to close it.
- Toolchain installed on this machine: Rust 1.98.1 via rustup (rustfmt,
  clippy), cargo-nextest 0.9.146, cargo-deny 0.20.2.

## 8. Scratchpad map (gitignored, local only)

| Path | Contents |
|---|---|
| `scratchpad/rust-foundation/decisions.md` | The D1–D9 list given to every worker |
| `scratchpad/rust-foundation/briefs/` | Worker briefs (A–G, E2, F2, R-*) |
| `scratchpad/rust-foundation/out/` | Worker reports and the review outputs (`R-astra.md`, `R-sol.md`, `R-spec-astra.md`) |
| `scratchpad/probes/` | Probe scripts, README, raw outputs under `out/` (private: contain vendor streams) |
| `scratchpad/codex-schema/` | `codex app-server generate-json-schema` output for 0.156.1 |
| `scratchpad/headless-bench/`, `scratchpad/acpx-eval/` | Resource benchmarks (native vs acpx), analysis reports |
| `scratchpad/council/`, `scratchpad/from-coding-ritual/` | Earlier review councils, language council, SDK demo logs |

## 9. How work is run

- **Roles** (owner's roster for this workstream): the orchestrator (Claude)
  briefs, reviews and commits; **GPT-6 Sol high** implements code; **GPT-6
  Astra medium** reviews substantial work only; the spec was written by
  **Claude Fable 5.1 high**. The owner reviews specs before code.
- Codex workers: follow `.repo-context/running-codex.md` (call `codex exec`
  directly; `< /dev/null`). For Rust builds inside the Codex sandbox, add
  `-c sandbox_workspace_write.network_access=true` and
  `-c 'sandbox_workspace_write.writable_roots=["<HOME>/.cargo/registry","<HOME>/.cargo/git","<HOME>/.cargo/advisory-dbs"]'`
  (the rest of `~/.cargo` is read-only there). Brief pattern: a shared rules
  file plus a per-task brief listing owned paths, acceptance checks and
  "do not commit, stage or run bd".
- Workers must not edit files owned by another concurrent worker; the
  orchestrator integrates and commits with explicit paths.
- Owner preferences: discuss before prototypes; show review briefs before
  sending when asked; share content inline as tables or narrow text (the
  owner often reads from a remote browser where Mermaid does not render);
  inline text options rather than pop-up questions; keep reports local.

## 10. Next steps, in order

1. Get the owner's answers on §5 items 1–2; apply them (spec edits by the
   spec author or orchestrator; standard §10 by the orchestrator).
2. Plan **S1**: the six-layer skeleton plus daemon (socket, lock, handshake,
   auto-start), Store schema v1, and a fake agent, with **failure modes
   listed first**, each mapped to an end-to-end scenario. S1 acceptance from
   the plan: two callers at once; `kill -9` the daemon mid-turn and the
   restarted daemon marks the turn `unknown`; a hanging agent hits its
   deadline and its tree is killed; a flooding agent never stalls the reader.
3. Dispatch S1 to Sol high workers with disjoint owned paths; Astra medium
   reviews the slice; run the gate; commit.
4. Then S2 Claude CLI, S3 Codex app-server, S4 cancel/deadlines/recovery,
   S5 OpenCode, S6 ACP, S7 Claude control route.
