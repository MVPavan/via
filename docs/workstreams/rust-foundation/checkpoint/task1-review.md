> Preserved at the owner-directed pause, 2026-09-26. Historical snapshot: source line numbers and intermediate dispositions below may predate the frozen code. Refer to `../session-handoff.md` for current status. Artifact filenames without a repository path refer to the original local-only scratchpad directory.

> Store and Host corrections were reviewed as recorded below. T1-I1 through T1-I7 have no final acceptance; later fixes were interrupted. The final-exit design was subsequently reviewed, but remains unintegrated. No new review was run for this checkpoint.

# Task 1 coherent code review — in progress

No combined acceptance verdict yet. Store/Host owning-layer review is underway; moving Core/CLI integration has not been reviewed. Initial types remain separately approved; harness R1 positive cleanup remains an integration obligation.

## Important findings

### T1-S1 — Check raw length before allocation (ADDRESSED)

`crates/via-store/src/runtime/raw.rs:195-204`: matching index/ref length is converted and allocated before the 1 MiB limit is checked. Corrupt persisted data or a forged matching reference can request up to approximately 4 GiB on the sole SQLite reader/writer thread, aborting the daemon instead of returning CorruptEvidence. Move checked conversion and cap before allocation. Regression must use an oversized entry under a controlled allocation budget, never actually request 4 GiB. Root assigned safe reproduction/fix to Store owner.

### T1-H1 — Preserve partial control frames across select cancellation (ADDRESSED)

`crates/via-host/src/anchor.rs:170-175,229-236`, `protocol.rs:154-176`: the armed loop cancels a local-buffer frame reader whenever the 20 ms child-poll/TERM branch wins. Bytes consumed before cancellation are discarded. A valid Status/Stop fragmented across a tick is then malformed, unnecessarily triggering group cleanup or losing control. Persistent bounded framing state must survive selection. Root assigned a deterministic real-anchor fragmentation regression and fix to Host owner.

### T1-H2 — Own and join Host background tasks (CODE FIX ADDRESSED; final-exit policy pending)

`crates/via-host/src/host.rs` start_anchor/acquire_inner discard reaper and status-poller JoinHandles. The poller owns a ProcessControl clone retaining the controller socket. Dropping all returned handles while a vendor remains alive therefore leaves polling and the vendor alive instead of allowing EOF cleanup. No tracked owner can stop/join these tasks before Store/runtime shutdown; aborting the runtime drops the reaper before confirming child reap. Keep explicit supervised task ownership and bounded teardown while retaining reaping responsibility after a deadline. Regression: drop/close ownership while a long-lived vendor runs, prove controller cleanup, reap ownership, and no task survives the declared Host shutdown.

### T1-H3 — Closed exit watch causes a hot loop (ADDRESSED)

`crates/via-host/src/host.rs`, `ProcessControl::close`: when the status task exits on an error without publishing an ExitReport, its watch sender closes with None. Graceful close checks only whether timeout_at returned its outer timeout error; `Ok(Err(RecvError))` causes the while loop to immediately repeat until force_at. This can monopolize a current-thread runtime for the remaining grace budget. Handle both timeout and closed observation explicitly, then attempt cleanup. Regression: closed watch/no report does not starve a concurrent timer or defer forced cleanup.

### T1-H4 — Positive absence persistence escapes the close deadline (ADDRESSED)

`crates/via-host/src/host.rs`, `wait_absence`: after an ESRCH probe, commit_group_absence is awaited without the caller deadline. A queued/busy Store can therefore extend close/recover past its absolute bound. Wrap the receipt wait in the existing deadline, return uncertainty on timeout, and do not retry a mutation whose outcome is unknown. This differs from deferred bounded Store worker shutdown: Host must stop waiting by its own contract. Regression: blocked journal reply cannot extend Host's close deadline.

### T1-S2 — Raw-ref regression has an unrelated reason to fail (ADDRESSED)

`crates/via-store/tests/persistence.rs`, `raw_reference_requires_synced_index_entry`: the forged reference is attached to a completed terminal before any acceptance is recorded. The assertion still fails if raw validation is removed because completed requires acceptance. Establish valid acceptance before the forged terminal or assert exactly CorruptEvidence. This is a missing effective regression, not a claim that current validation is bypassed. Sent to root and Store owner for disposition.

## Evidence and bounds

Store source hashes are pinned in `task1-store-source-hashes.json`. Read Store runtime/raw/SQL/anchor modules and persistence tests; read Host protocol/anchor/native identity/control and native tests. Reviewed resource-bootstrap-seam.md and its scoped Sol re-review: Core owns sole Store, only Wire splits capabilities, no outward operational-handle getters, and current blocking Store Drop must stay off Tokio failure paths. Bounded Store failure shutdown is explicitly deferred, not claimed.

No additional source changes, document changes or Beads mutations were made. Pending: owner regression results/frozen Host hashes; integrated Wire/Route/Adapter/Core/CLI review and actual join gates. Host wait_absence's journal commit currently lacks the caller deadline; disposition pending further scope confirmation.

## Store fixed snapshot

Inspected the final Store fix: checked length precedes payload open/allocation; the new prlimit child regression constrains address space to 128 MiB and requires CorruptEvidence for a u32::MAX index length. Worker reports original code failed with a bounded allocation abort, corrected code passes. The forged-terminal test now first commits valid acceptance and asserts exact CorruptEvidence. Both Store findings are addressed. Worker reports 10 Store tests and clippy passing; fixed hashes pinned in task1-store-fixed-source-hashes.json. No new Store defect found in this delta.

## Host fixed snapshot review

H1/H3/H4 are addressed in source: persistent bounded FrameReader across anchor select branches; graceful wait breaks on closed watch; absence persistence obeys the caller deadline and reports EvidenceStoreFailure without retry. Real fragmentation and blocked-writer regressions exercise the original failures; worker reports 14 Host tests/clippy green outside AF_UNIX-restricted sandbox. Reviewer reran three library tests: initial default sandbox group produced UnverifiedAnchor in the group-presence fixture, then all three passed under `setsid cargo test --locked --offline -p via-host --lib`. This is an environment/group-fixture limitation, not evidence of a cleanup regression.

H2's normal path improves materially: weak polling releases the last controller socket, tracked reapers and shutdown joins replace detached handles, and expired-entry retry retains ownership. It remains open: `host.rs:512-530` removes all JoinHandles from Host into a local vector, then awaits. Cancellation during that await drops pending handles before reinsertion; a later shutdown cannot count or join them. Add cancellation-safe ownership during joins and a deterministic cancellation-inside-join regression. The separately pending policy for final uncertain daemon exit is not resolved or waived by these changes.

Frozen Host source hashes are in task1-host-fixed-source-hashes.json. No combined Task1 approval yet; Core/CLI integration remains unreviewed pending its explicit freeze.

## H2 cancellation fix

Confirmed updated HostTasks retains Arc-owned mutex-protected JoinHandles throughout join awaits. The deterministic regression waits until the join owns the handle mutex, cancels the joiner, verifies registry ownership, then releases and joins the retained task. H2 code correction is addressed. Final uncertain daemon-exit policy remains separately unapproved; no automatic waiver.

## Integration findings — owner fixes pending

- **T1-I1 Important:** Route stops reading after first terminal and waits exit; stdout/stderr tails go unrecorded, duplicate terminals are missed, and a stderr flood can block exit. Wire stdout EOF also returns before stderr EOF. Half-close must precede bounded complete drain/validation of both streams. Owner assigned regression/fix.
- **T1-I2 Important:** client request sends protocol traffic without checking daemon peer UID; only server checks peers. Verify client peer before hello or handles. Root assigned.
- **T1-I3 Important:** C1 envelopes are generic Values without jsonrpc/id/unknown-field validation; current read/status/stop parameters bypass strict DTOs. Enforce current-method request shapes, without implementing future methods. Root assigned.
- **T1-I4 Important architecture/coverage:** production hidden daemon/verify_cleanup RPC and __via_verify_cleanup CLI were not authorized by the reviewed R1 seam, which explicitly prohibits a new debug endpoint. They require a live daemon and cannot prove daemon-first-death cleanup. Root confirms no later authorization and assigned removal plus approved outer snapshot/control/absence path.
- **T1-I5 Important:** successful artifact events contain C2-only turn.accepted and invented turn.terminal instead of C1 turn.started/turn.ended. Route also discards assistant.text/tool/unknown observations. Current events require canonical common fields and tags; future follow API deferral does not authorize incorrect present output. Sent root/source owner for Store/Core coordination.
- **T1-I6 Important (root-known):** receipt/envelope omit required current-method metadata and use incorrect model/raw-spans shapes. Core DTO owner is correcting these; moving engine response code is not yet reviewed.
- **T1-I7 Important:** daemon/stop force only bypasses admission refusal and then waits active drives; it does not force active work to stop. Root disposition requested against current stop-method contract.

Retained earlier successful prompt artifact Qm8zWw: sha256 manifest verifies; SQLite quick_check passes; one completed turn, dense four-event sequence, committed arm_intent and positive absence timestamp. This establishes actual earlier execution, not current contract compliance: events/DTO/debug endpoint defects above remain. Bootstrap ownership search found one production into_wire_parts site in Wire and one Store open in Core, with operational handles private to Wire/Host as required.
