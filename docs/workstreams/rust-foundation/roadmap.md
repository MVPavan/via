# VIA first-release roadmap

Status: **owner-paused, 2026-09-26**; no dispatch until explicit resumption.
Current checkpoint and interrupted fixes: [session-handoff.md](session-handoff.md).
Goal contract: [goal.md](goal.md).
Beads is the state source; tables here define scope, ownership and acceptance.
Owner-approval issue `via-jm4.10` closed before feature dispatch.

## Phase graph

| Phase | Epic | Goal | Prerequisite for implementation | Risk |
|---|---|---|---|---|
| S1 | `via-jm4` | Verified CLI/daemon/Store spine and failure semantics | Internal design and Sol review; increments serialized | deep |
| S2 | `via-p98` | Claude Code adapter | Verified S1 plus Claude design review | deep |
| S3 | `via-5lr` | Codex adapter | Verified S1 plus Codex design review | deep |
| S5 | `via-4sw` | OpenCode adapter | Verified S1 plus OpenCode design review | deep |
| S6 | `via-jt8` | Pi adapter (owner, 2026-10-03), built side by side with S5 | Verified S1 plus Pi evidence, vendor packet and design review | deep |
| S4 | `via-gvg` | Cross-adapter control/recovery hardening | Four reviewed adapters | deep |
| P1 | `via-pvj` | Linux platform and packaging; macOS design retained, artifact/native gates deferred under `via-pvj.4` | Platform design review plus S1 for runtime changes | deep |
| R1 | `via-d9o` | Verified local release candidate | Four adapters, S4, required Linux platform gates, docs and evidence | deep |

These are eight top-level epics, not one four-level release hierarchy. Existing
foundation records are retained. `via-jm4.3` and `.6` aggregate evidence and
decision outcomes owned by the new vendor tasks; they do not commission duplicate
work. `via-jm4.1`, `.2`, `.4` and `.5` retain completed S0 history. `via-jm4.8`
tracks the documentation reconciliation; `.9` tracks this goal/graph preparation.
Unrelated Beads, including old spikes and harness tooling, are not goal blockers.

After approval, five independent entries became available: internal design,
Claude evidence, Codex evidence, OpenCode evidence and platform design. Each
vendor evidence packet feeds its own Astra-high design and Sol-high review.
Shared C1/C2 edits go through the coordinator rather than competing writers.
After verified S1, three adapter paths can run concurrently. Platform-specific
source changes serialize with other owners of shared Host/CLI files; packaging
scripts can proceed independently. Default and live release checks run on one
fixed integrated state, isolating state directories and outputs.

Substantial implementation leaves include Astra-medium review and required fixes
in their acceptance. They are not closed at first green. Astra-high critical
reviews are separate S1 and release-candidate gates. Implementation workers
are Sol high; design authors are Astra high and design reviewers Sol high.

## S1 foundation and design registers

Goal: complete `via-jm4.7` through its tracked subtasks below. Governing specs:
C1/C2 plus the internal design packet produced by `via-jm4.7.1` and reviewed
by `.2`. Detailed behavior: [s1-plan.md](s1-plan.md). All subtasks have concrete
acceptance in Beads. Test focus: F1–F30, controlled failure injection, durable
state, identity, bounded streams and full C1 including unsubscribe.

| Subtask | Work | Executor | Depends on |
|---|---|---|---|
| `via-jm4.7.1` | Design internal contracts and failure semantics | GPT-6 Astra high | `via-jm4.10` |
| `via-jm4.7.2` | Review and reconcile internal design packet | GPT-6 Sol high | `via-jm4.7.1` |
| `via-jm4.7.3` | Implement reviewed internal contract types | GPT-6 Sol high | `via-jm4.7.2` |
| `via-jm4.7.4` | Build supervised fake-agent test and evidence harness | GPT-6 Sol high | `via-jm4.7.2` |
| `via-jm4.7.5` | Implement prompt-to-result vertical slice | GPT-6 Sol high | `via-jm4.7.3`, `.7.11`, `.7.12`, `.7.13` |
| `via-jm4.7.6` | Implement session continuity queues and retry safety | GPT-6 Sol high | `via-jm4.7.4`, `via-jm4.7.5` |
| `via-jm4.7.7` | Implement daemon lifecycle deadlines and recovery | GPT-6 Sol high | `via-jm4.7.6` |
| `via-jm4.7.8` | Complete bounded streams and C1 conformance | GPT-6 Sol high | `via-jm4.7.7` |
| `via-jm4.7.9` | Critically review and verify completed S1 | GPT-6 Astra high; Sol high fixes | `via-jm4.7.8` |

The integrated common-contract sync `.7.11` is closed. Store `.7.12` and Host
`.7.13` are separately owned Task 1 leaves, each depending on `.7.3`; both
remain unfinished pending combined acceptance. Harness `.7.4` and spine `.7.5`
join before `.7.6`; the harness is not a start dependency that creates a
cleanup-evidence cycle. Live Beads retains auxiliary design/reconciliation leaves.

Exit (S1): all F1–F30 and functional checks pass, Astra-medium findings fixed, then fresh Astra-high critical review and fixes complete. The larger existing foundation epic closes after its cross-slice registers also resolve.

## [S2] Claude Code adapter and conformance — `via-p98`

Goal: Claude Code implements all supported C1 operations; declared capabilities match fake and live evidence; reviews and required fixes pass.

Governing sources: [goal.md](goal.md), `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md`, plus the reviewed slice design where required.

| Task / subtask | Work | Owned scope | Verify |
|---|---|---|---|
| `via-p98.1` | Claude Code: pinned protocol and behavior evidence | Claude Code docs/schema and isolated scratchpad probes: B1/B7/B8; A2/A3/A6 and P13; stream-json continuity, bounds, interrupt and resumed schema/step semantics | Versioned evidence resolves each applicable question or records a conservative supported refusal; no unapproved prototype. |
| `via-p98.1.1` | Claude Code: inspect pinned official protocol and prior probes | private claude evidence packet and probe plan | Primary-source protocol/version map, exact focused probe cases and unresolved questions recorded. |
| `via-p98.1.2` | Claude Code: run isolated conformance probes | isolated private claude probe scripts/state/output | Applicable B1/B7/B8; A2/A3/A6 and P13; stream-json continuity, bounds, interrupt and resumed schema/step semantics observations have raw evidence/version and cleanup; auth/quota failure stays a blocker, never a pass. |
| `via-p98.2` | Claude Code: settle adapter contract decisions | Claude Code contract packet and coordinated C1/C2 updates | Material choices reviewed and reconciled; capability/bounds/cancel/recovery/version matrix testable. |
| `via-p98.2.1` | Claude Code: design evidence-backed adapter contract | planned docs/specs/vendors/claude.md; shared specs through coordinator | Design resolves applicable decision IDs without redesigning approved foundation; supported/partial/refused operations and tests explicit. |
| `via-p98.2.2` | Claude Code: review contract and resolve findings | Claude Code design review and coordinated contract integration | No unresolved blocking design issue; C1/C2 and acceptance matrix agree. |
| `via-p98.3` | Claude Code: implement and verify adapter | planned crates/via-adapters/src/claude, crates/via-routes/src/claude, vendor fixtures; shared files coordinator-owned | Real adapter/route passes fake conformance and live supported operations, fixes reviewed; no integration regression. |
| `via-p98.3.1` | Claude Code: write failing real-route conformance fixtures | claude sanitized fixtures and adapter integration tests | Tests validate outgoing protocol, inject faults and distinguish native/partial/unsupported outcomes; demonstrate intended failures. |
| `via-p98.3.2` | Claude Code: implement supported protocol and lifecycle | claude adapter/route modules; coordinator serializes shared joins | Versioned mapping, never-ask, bounds, resume, events, usage and controls satisfy reviewed contract; fake suite green. |
| `via-p98.3.3` | Claude Code: review fixes and live adapter verification | claude evidence, focused fixes and integration | Substantial review blockers fixed; pinned live spawn/resume/result and supported controls pass; gate/evidence captured. |

Exit (S2): Claude Code implements all supported C1 operations; declared capabilities match fake and live evidence; reviews and required fixes pass. Close only when required descendants and evidence are complete.
Test focus: persistent conversation identity, permission-denial behavior, interrupt and resumed parameters. Risk: deep.

## [S3] Codex adapter and conformance — `via-5lr`

Goal: Codex implements all supported C1 operations; declared capabilities match fake and live evidence; reviews and required fixes pass.

Governing sources: [goal.md](goal.md), `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md`, plus the reviewed slice design where required.

| Task / subtask | Work | Owned scope | Verify |
|---|---|---|---|
| `via-5lr.1` | Codex: pinned protocol and behavior evidence | Codex docs/schema and isolated scratchpad probes: B2/B3/B6/B7; A7/A8 and P7/P11; owned app-server, never-ask decline, steer, tool quiescence and shared-server isolation; optional socket rejoin excluded | Versioned evidence resolves each applicable question or records a conservative supported refusal; no unapproved prototype. |
| `via-5lr.1.1` | Codex: inspect pinned official protocol and prior probes | private codex evidence packet and probe plan | Primary-source protocol/version map, exact focused probe cases and unresolved questions recorded. |
| `via-5lr.1.2` | Codex: run isolated conformance probes | isolated private codex probe scripts/state/output | Applicable B2/B3/B6/B7; A7/A8 and P7/P11; owned app-server, never-ask decline, steer, tool quiescence and shared-server isolation; optional socket rejoin excluded observations have raw evidence/version and cleanup; auth/quota failure stays a blocker, never a pass. |
| `via-5lr.2` | Codex: settle adapter contract decisions | Codex contract packet and coordinated C1/C2 updates | Material choices reviewed and reconciled; capability/bounds/cancel/recovery/version matrix testable. |
| `via-5lr.2.1` | Codex: design evidence-backed adapter contract | planned docs/specs/vendors/codex.md; shared specs through coordinator | Design resolves applicable decision IDs without redesigning approved foundation; supported/partial/refused operations and tests explicit. |
| `via-5lr.2.2` | Codex: review contract and resolve findings | Codex design review and coordinated contract integration | No unresolved blocking design issue; C1/C2 and acceptance matrix agree. |
| `via-5lr.3` | Codex: implement and verify adapter | planned crates/via-adapters/src/codex, crates/via-routes/src/codex, vendor fixtures; shared files coordinator-owned | Real adapter/route passes fake conformance and live supported operations, fixes reviewed; no integration regression. |
| `via-5lr.3.1` | Codex: write failing real-route conformance fixtures | codex sanitized fixtures and adapter integration tests | Tests validate outgoing protocol, inject faults and distinguish native/partial/unsupported outcomes; demonstrate intended failures. |
| `via-5lr.3.2` | Codex: implement supported protocol and lifecycle | codex adapter/route modules; coordinator serializes shared joins | Versioned mapping, never-ask, bounds, resume, events, usage and controls satisfy reviewed contract; fake suite green. |
| `via-5lr.3.3` | Codex: review fixes and live adapter verification | codex evidence, focused fixes and integration | Substantial review blockers fixed; pinned live spawn/resume/result and supported controls pass; gate/evidence captured. |

Exit (S3): Codex implements all supported C1 operations; declared capabilities match fake and live evidence; reviews and required fixes pass. Close only when required descendants and evidence are complete.
Test focus: shared-server isolation, steer, never-ask decline, cancellation versus tool cleanup. Risk: deep.

## [S5] OpenCode adapter and conformance — `via-4sw`

Goal: OpenCode implements all supported C1 operations; declared capabilities match fake and live evidence; reviews and required fixes pass.

Governing sources: [goal.md](goal.md), `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md`, plus the reviewed slice design where required.

| Task / subtask | Work | Owned scope | Verify |
|---|---|---|---|
| `via-4sw.1` | OpenCode: pinned protocol and behavior evidence | OpenCode docs/schema and isolated scratchpad probes: B4/B7; A4/A8 and P11; pinned OpenAPI/SSE, private owned server/database, full-bound semantics and network refusal | Versioned evidence resolves each applicable question or records a conservative supported refusal; no unapproved prototype. |
| `via-4sw.1.1` | OpenCode: inspect pinned official protocol and prior probes | private opencode evidence packet and probe plan | Primary-source protocol/version map, exact focused probe cases and unresolved questions recorded. |
| `via-4sw.1.2` | OpenCode: run isolated conformance probes | isolated private opencode probe scripts/state/output | Applicable B4/B7; A4/A8 and P11; pinned OpenAPI/SSE, private owned server/database, full-bound semantics and network refusal observations have raw evidence/version and cleanup; auth/quota failure stays a blocker, never a pass. |
| `via-4sw.2` | OpenCode: settle adapter contract decisions | OpenCode contract packet and coordinated C1/C2 updates | Material choices reviewed and reconciled; capability/bounds/cancel/recovery/version matrix testable. |
| `via-4sw.2.1` | OpenCode: design evidence-backed adapter contract | planned docs/specs/vendors/opencode.md; shared specs through coordinator | Design resolves applicable decision IDs without redesigning approved foundation; supported/partial/refused operations and tests explicit. |
| `via-4sw.2.2` | OpenCode: review contract and resolve findings | OpenCode design review and coordinated contract integration | No unresolved blocking design issue; C1/C2 and acceptance matrix agree. |
| `via-4sw.3` | OpenCode: implement and verify adapter | planned crates/via-adapters/src/opencode, crates/via-routes/src/opencode, vendor fixtures; shared files coordinator-owned | Real adapter/route passes fake conformance and live supported operations, fixes reviewed; no integration regression. |
| `via-4sw.3.1` | OpenCode: write failing real-route conformance fixtures | opencode sanitized fixtures and adapter integration tests | Tests validate outgoing protocol, inject faults and distinguish native/partial/unsupported outcomes; demonstrate intended failures. |
| `via-4sw.3.2` | OpenCode: implement supported protocol and lifecycle | opencode adapter/route modules; coordinator serializes shared joins | Versioned mapping, never-ask, bounds, resume, events, usage and controls satisfy reviewed contract; fake suite green. |
| `via-4sw.3.3` | OpenCode: review fixes and live adapter verification | opencode evidence, focused fixes and integration | Substantial review blockers fixed; pinned live spawn/resume/result and supported controls pass; gate/evidence captured. |

Exit (S5): OpenCode implements all supported C1 operations; declared capabilities match fake and live evidence; reviews and required fixes pass. Close only when required descendants and evidence are complete.
Test focus: pinned OpenAPI/SSE, server/database ownership, bounds and network refusal. Risk: deep.

## [S6] Pi adapter and conformance — `via-jt8`

Goal: Pi implements all supported C1 operations; declared capabilities match fake and live evidence; reviews and required fixes pass. Added by owner, 2026-10-03, and built side by side with S5 so C2 gaps found by either are fixed once.

Governing sources: [goal.md](goal.md), `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md`, plus `docs/specs/vendors/pi.md` once reviewed.

| Task / subtask | Work | Owned scope | Verify |
|---|---|---|---|
| `via-jt8.1` | Pi: pinned protocol and behavior evidence | isolated scratchpad probes of the installed `pi` | Versioned raw evidence for RPC/JSON modes, sessions, abort/steer, usage, errors, shutdown, trust, tools and isolation; model-dependent probes use `gpt-6-luna`. |
| `via-jt8.2` | Pi: settle adapter contract decisions | `docs/specs/vendors/pi.md`; shared specs through coordinator | Route, bounds, structured output, resume, controls and cleanup decided and reviewed; C2 gaps reconciled with S5. |
| `via-jt8.3` | Pi: implement and verify adapter | planned crates/via-adapters/src/pi, crates/via-routes/src/pi, vendor fixtures; shared files coordinator-owned | Real adapter/route passes fake conformance and live supported operations, fixes reviewed; no integration regression. |
| `via-jt8.3.1`–`.3.4` | fixtures, implementation, review and live verification, `gpt-6-luna` live qualification | as `via-4sw.3.1`–`.3.4` | as `via-4sw.3.1`–`.3.4`, with live checks on `gpt-6-luna` (owner, 2026-10-04). |

Exit (S6): as S5, for Pi. Risk: deep.

## [S4] Cross-adapter control and recovery hardening — `via-gvg`

Goal: Isolation, cancel/cleanup, server death, resource bounds and uncertainty work across real process shapes with independent review.

Governing sources: [goal.md](goal.md), `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md`, plus the reviewed slice design where required.

| Task / subtask | Work | Owned scope | Verify |
|---|---|---|---|
| `via-gvg.1` | Integrate cross-adapter control and ownership behavior | Core/Host/Store/shared registries; exclusive writer | Shared-server operations never kill another session; per-route cleanup and admission semantics agree; review findings fixed. |
| `via-gvg.1.1` | Write cross-session server and failure scenarios | cross-adapter E2E tests | Failing tests cover server death, surviving tools, uncertain cleanup and unrelated active sessions. |
| `via-gvg.1.2` | Fix lifecycle integration and verify control isolation | shared runtime modules and fault scenarios | All cross-session control scenarios pass; no unsafe resend or fabricated quiescence; substantial review findings fixed. |
| `via-gvg.2` | Verify overload durability and crash recovery across routes | bounded stress/fault tests and necessary fixes | Measured bounds, crash/Store-failure and transport evidence hold for persistent and shared processes; no regressions. |

Exit (S4): Isolation, cancel/cleanup, server death, resource bounds and uncertainty work across real process shapes with independent review. Close only when required descendants and evidence are complete.
Test focus: cross-session interference, uncertain outcomes, server death, overload and persistent Store failure. Risk: deep.

## [P1] Linux and macOS platform and packaging contract — `via-pvj`

Goal: Reviewed Linux target/static-linkage contract is satisfied by an installed, tested fully static artifact on actual baseline/current Linux; no failpoints ship. macOS design remains, with artifact production, inspection and native qualification deferred together under `via-pvj.4`.

Governing sources: [goal.md](goal.md), `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md`, plus the reviewed slice design where required.

| Task / subtask | Work | Owned scope | Verify |
|---|---|---|---|
| `via-pvj.1` | Define target packaging and verification contract | planned platform/packaging spec and runner availability | Target triples/minimum OS/linkage/system-library rules/install layout and actual runners identified; invariant changes escalated explicitly. |
| `via-pvj.1.1` | Design platform and artifact acceptance matrix | platform/packaging spec; current environment inventory | Resolve single-static-binary meaning per platform without silent relaxation; identify required runtime proof and unavailable runners. |
| `via-pvj.1.2` | Review target contract and resolve infrastructure needs | platform contract review; coordinator requests missing access | No unresolved design blocker; owner decisions on invariant changes recorded if needed; runner access or explicit blocker evidenced. |
| `via-pvj.2` | Implement Linux supervision and packaging | Linux Host/CLI modules and packaging scripts; serialize shared writers | Linux socket peer/identity behavior and packaging meet reviewed design; substantial review findings fixed. |
| `via-pvj.2.1` | Implement and test Linux socket and process identity | Linux Host/CLI helpers and tests | Positive/negative process identity and socket checks pass on actual target OS. |
| `via-pvj.2.2` | Build installable Linux artifact and verify release features | packaging scripts; coordinator owns shared Cargo changes | Fully static artifact builds with locked dependencies; inspected linkage and install/version smoke agree with contract; test failpoints absent. |
| `via-pvj.3` | Verify final Linux artifact on baseline/current target | target matrix and final artifact outputs | Actual final binary installs/runs on kernel 5.15 baseline and current Linux, passes platform smoke; hash matches tested artifact; missing runner leaves this open. |
| `via-pvj.4` | Deferred macOS artifact and native qualification | linked follow-up, outside current-goal finish | Produce/inspect ARM64 macOS 13 artifact under approved system-library allowlist; verify install, baseline/current native socket/process/failpoint and three-adapter live behavior before claiming macOS compatibility. Deferral is not a pass. |

Exit (P1): Reviewed Linux target/static-linkage contract is satisfied by the installed tested fully static artifact on kernel 5.15 baseline/current Linux; no failpoints ship. Deferred `via-pvj.4` is linked, not a current-goal blocker or accepted macOS artifact. Close only when required descendants and evidence are complete.
Test focus: socket peers, process identities, tested OS targets, install/linkage and absence of test failpoints. Risk: deep.

## [R1] Integrated release candidate and critical review — `via-d9o`

Goal: Every goal finish criterion has current primary evidence; final Astra-high critique blockers fixed; all required Beads descendants closed.

Governing sources: [goal.md](goal.md), `docs/specs/via-api-v1.md`, `docs/specs/adapter-contract.md`, plus the reviewed slice design where required.

| Task / subtask | Work | Owned scope | Verify |
|---|---|---|---|
| `via-d9o.1` | Write complete user help and operating guidance | README, usage guide, CLI help; implementation author coordinates source edits | Examples cover login, three harnesses, verbs, handles, retries, bounds, background, cancellation and recovery; examples run successfully. |
| `via-d9o.2` | Run complete release verification and evidence audit | all gates and private release artifacts | All goal gates pass on final integrated state; live or platform infrastructure failures remain unmet; required artifacts validate. |
| `via-d9o.2.1` | Verify deterministic full suite and artifact integrity | full prescribed Rust/failpoint/doc/layer checks and evidence validation | Nonempty suite, all F1-F30 and vendor faults, feature-isolation checks and artifact integrity pass on final state. |
| `via-d9o.2.2` | Verify four-adapter live release matrix | isolated pinned live cases and versions | Each harness passes required live behavior on release artifacts; quota/auth/unavailability never counted as pass. |
| `via-d9o.3` | Critically review the integrated release candidate | architecture, product guarantees, final evidence | Independent critique recorded and every blocker repaired and verified; no unreviewed material deviation. |
| `via-d9o.3.1` | Perform final architecture and evidence critique | read-only fresh-context integrated critique | Review addresses all goal guarantees, platform/static claims, unsupported verbs, process containment and evidence quality. |
| `via-d9o.3.2` | Resolve final findings and close goal evidence | necessary fixes, final checks, Beads and release report | Critical findings dispositioned and fixes rechecked; close all other required leaf work, then this leaf and satisfied ancestors; export tracking and produce the local handoff without unauthorized Git/publication action. |

Exit (R1): Every goal finish criterion has current primary evidence; final Astra-high critique blockers fixed; all required Beads descendants closed. Close only when required descendants and evidence are complete.
Test focus: all deterministic/live gates on the final state, artifact integrity, usage examples and independent critique. Risk: deep.

## Dependency contract

Each leaf carries explicit prerequisites in Beads. Parent-child links express
aggregation, not permission to skip child gates. The source graph was seeded
without cycles; verify with `bd dep cycles`. Inspect ready **leaves**, not merely
open epics. No future task becomes implementation-ready just because an unrelated
worker is idle. Vendor implementation awaits `via-jm4.7.9` and its own design
review; hardening joins the three vendor verification leaves; final critique
joins deterministic and live final-state evidence. Platform final verification
joins packaging, platform source checks and integrated hardening.

The coordinator alone changes tracking. Replan dependencies when a demonstrated
interface need changes the graph; do not silently weaken acceptance. Add a
subtask for separately owned new work within scope. Escalate scope or invariant
changes to the owner while continuing independent authorized tasks.
