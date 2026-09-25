# Workstream mode — unattended multi-phase execution

Enter only through an explicit request to run the remaining phases or `/run-phases`.
Reading this reference is not authorization. Record the authorized roadmap, phase
scope, plan approval coverage and commit policy in the execution ledger.

## Preflight

Capture the starting revision and dirty-file baseline. Reconcile each roadmap
phase ID with exactly one Beads epic title `[<phase-id>]`; report missing or
ambiguous records before execution. Preserve all pre-existing edits.

## Walk

1. Read roadmap order and Beads status/dependencies. Select the first eligible
   unfinished phase; never cross an unmet prerequisite.
2. Execute it through the execution skill's phase loop and close gate. Unattended
   authority covers routine plan elaboration within the agreed roadmap, not new
   behavior, changed safety boundaries or scope expansion.
3. After the stage gate and phase exit pass, regenerate tracking and refresh the
   Beads export where the authorized write scope permits it.
4. Honor the mode's established per-phase commit convention only when the user's
   invocation covers it. An explicit no-commit restriction overrides it. Stage
   named phase-owned paths only; if they include pre-existing edits, prepare a
   scoped result rather than committing unrelated content. No push is implied.
5. Continue eligible phases until the authorized scope is complete. If work is
   unfinished but no phase is eligible, report the blocking dependencies.

## Context and recovery

Stay in the current context while useful. Compact or hand off only when the
runtime supports it and context pressure warrants it; no unconditional slash
command applies across providers. Before recovery, persist scope, decisions,
source pointers, checks, next work and unresolved findings in Beads/ledger.
Afterward re-read the ledger and governing phase, query Beads, and inspect Git.

A failed phase gate, failed exit criterion or material unresolved blocker stops
advancement. Diagnose authorized in-scope failures; do not keep dispatching without
new evidence or bypass the task engine's budget and safety boundaries.
