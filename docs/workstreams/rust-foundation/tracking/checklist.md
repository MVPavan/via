<!-- BD:GENERATED START -->
# Checklist — rust-foundation
_generated from bd @ 2026-09-26T18:21:09Z — DO NOT EDIT (run: BD_RENDER=1 bash <beads-skill-dir>/scripts/bd-render-tracking.sh)_
_Roadmap: [goal.md](../goal.md) · Brainstorm: [roadmap.md](../../../../docs/workstreams/rust-foundation/roadmap.md)_
## [P1] Linux and macOS platform and packaging contract
- [x] `via-pvj.1` Define target packaging and verification contract
- [ ] `via-pvj.2` Implement platform-specific supervision and packaging  (blocked)
- [ ] `via-pvj.3` Verify final Linux release artifacts  (blocked)
- [ ] `via-pvj.4` Produce and qualify deferred macOS release artifact  (blocked)

## [R1] Integrated release candidate and critical review
- [ ] `via-d9o.1` Write complete user help and operating guidance  (blocked)
- [ ] `via-d9o.2` Run complete release verification and evidence audit  (blocked)
- [ ] `via-d9o.3` Critically review the integrated release candidate  (blocked)

## [S1] Rust foundation and first-release design registers
- [x] `via-jm4.1` Apply daemon/Turn/VIA API/Core decisions to invariants, glossary and design docs
- [x] `via-jm4.2` Draft C1 VIA API v1 and C2 Adapter contract spec
- [ ] `via-jm4.3` Vendor behaviour probes for S2/S3 open questions  ← ready
  - 📝 Current per-vendor evidence ownership is mapped in roadmap.md; this task is the aggregation register, not duplicate probe execution.
- [x] `via-jm4.4` Write the VIA Rust standard (.repo-context/rust-style.md) and get it reviewed
  - 📝 Standard written in .repo-context/coding-style.md (not rust-style.md; AGENTS.md already routes code changes there). Reviewed by Astra medium + Sol high (both SOUND WITH CHANGES), revisions applied, config enforced (E2). Open: §10 testing section awaits owner confirmation.
- [x] `via-jm4.5` S0 workspace skeleton, lint/deny config and gate
- [ ] `via-jm4.6` Decide deferred contract decisions in their slice  ← ready
  - 📝 Per-vendor design tasks own decisions; this register closes when their accepted outcomes are integrated.
- [ ] `via-jm4.7` S1: six-layer skeleton, daemon, Store v1, fake agent  🔄 in progress
  - 📝 plan: docs/workstreams/rust-foundation/s1-plan.md. Resume with planning for workflow agreement, then codebase-design/document-review for the bounded design gate and execution/test-driven-development for implementation. Foundation approvals carry forward.  /  Activation turn made progress: owner gate closed, internal design draft complete, Sol design review active, vendor inspections complete and bounded probes active, platform design/review found explicit mac linkage owner choice. Root remains orchestration-only. Dispatch ledger scratchpad/execution/rust-foundation-release/dispatch.json; primary reports remain private. Do not respawn running workers on context reset; inspect native handles first.  /  OWNER-DIRECTED PAUSE 2026-09-26: goal tool is paused; no active worker or authorized redispatch. Prior active-worker notes are historical. Preserve incomplete code and acceptance; resume only after explicit owner instruction. Current handoff: docs/workstreams/rust-foundation/session-handoff.md. Resume with execution and Beads; use Sol high implementation, Astra medium substantial-increment review, Astra high design/Sol high review, and Astra high milestone critique. Closeout task via-jm4.14 does not resume development.
- [x] `via-jm4.8` Reconcile VIA readiness docs and record first-release workflow
- [x] `via-jm4.9` Prepare goal contract and verified Beads release graph
  - 📝 Goal design inputs: direct goal_design_astra_high (gpt-6-astra high), goal_inventory_sol_high (gpt-6-sol high), goal_inventory_astra_medium (gpt-6-astra medium), all completed read-only work. Requested launch configuration and completion verified, not independent backend identity. Sol high reviewed the actual goal; missing unsubscribe added and native-first authority clarified; narrow follow-up confirmed both fixes. Seven epics, 23 current tasks, 40 subtasks mapped, plus preserved S0 history. Resume with execution only after owner approval of goal.md.
- [x] `via-jm4.10` Owner review and activate first-release goal
- [x] `via-jm4.11` Record goal activation and orchestrator-only execution policy
  - 📝 Native worker /root/goal_inventory_sol_high; requested gpt-6-sol/high; owned artifact docs/workstreams/rust-foundation/goal.md. Root orchestrates only. Completion requires primary artifact/check evidence plus configured review.
- [x] `via-jm4.12` Refresh active implementation handoff and goal checkpoint
- [x] `via-jm4.13` Apply owner Linux-first platform and OpenCode decisions
- [ ] `via-jm4.14` Preserve owner-directed pause and Rust foundation checkpoint  🔄 in progress
  - 📝 Checkpoint preservation complete: goal paused, stopped workers recorded, source left unchanged, seven integration findings and reviewed/unintegrated shutdown seam preserved in committed-document candidates. Current fmt/layer/catalog checks pass (catalog advisory warnings); Markdown broken links 0; git diff --check passes. All-target offline cargo check fails E0599 at test lines 134 and 676 (ScenarioError lacks Display); no new runtime tests, implementation fixes or review cycle. Local WIP commit pending; no push authorized.

## [S2] Claude Code adapter and conformance
- [x] `via-p98.1` Claude Code: pinned protocol and behavior evidence
- [x] `via-p98.2` Claude Code: settle adapter contract decisions
- [ ] `via-p98.3` Claude Code: implement and verify adapter  (blocked)

## [S3] Codex adapter and conformance
- [x] `via-5lr.1` Codex: pinned protocol and behavior evidence
- [x] `via-5lr.2` Codex: settle adapter contract decisions
- [ ] `via-5lr.3` Codex: implement and verify adapter  (blocked)

## [S4] Cross-adapter control and recovery hardening
- [ ] `via-gvg.1` Integrate cross-adapter control and ownership behavior  (blocked)
- [ ] `via-gvg.2` Verify overload durability and crash recovery across routes  (blocked)

## [S5] OpenCode adapter and conformance
- [x] `via-4sw.1` OpenCode: pinned protocol and behavior evidence
- [x] `via-4sw.2` OpenCode: settle adapter contract decisions
  - 📝 Dependency refinement: preliminary design may use completed pinned protocol inspection while live probes wait on working provider. Sol design acceptance via-4sw.2.2 now directly depends on live probe gate via-4sw.1.2; implementation and release gates are preserved. Draft must explicitly label unverified model-dependent claims.
- [ ] `via-4sw.3` OpenCode: implement and verify adapter  (blocked)
- [ ] `via-4sw.4` Revisit OpenCode child environment password confinement  (blocked)


<!-- BD:GENERATED END -->

<!-- Human notes below this line are preserved across renders. Everything above is bd-generated; do not hand-edit it. -->
