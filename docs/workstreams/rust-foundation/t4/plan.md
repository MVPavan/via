# Task 4 implementation plan

Status: implementation plan for Task 4 (Bead `via-jm4.7.8`), accepted by the
orchestrator 2026-09-29; chunk Beads `via-jm4.7.8.3`–`.10` (T4-7 is
`via-jm4.7.8.1`). It implements [design.md](design.md) (round 18, Sol high SOUND) as
written and adds no hardening beyond it.

Inputs, all at `67e2788` on `rust-foundation`: [design.md](design.md) (normative;
§ numbers below are its sections), [requirements.md](requirements.md) (R1–R8),
[t0.md](t0.md), `.repo-context/verification.md`, `.repo-context/coding-style.md`
§10, `scripts/check-release-features.py`. The round-5 briefs `s1.md`–`s7.md` in
this folder are superseded; do not use them.

Owner constraints:
- Implementer Claude Opus 5.5 medium, one worker per chunk in its own git
  worktree; reviewer GPT-6 Sol high, one review loop per chunk.
- Few, coherent chunks, each independently reviewable and gate-green.
- Simple first, measure later: §16 items belong to `via-d9o.2.3`; the only
  measurements here are those §13 names (F24 RSS, WAL growth record).
- Failure-first where §13 names a test. Removal lands in the chunk that owns
  the replacement, so no chunk leaves dead or half-removed code.
- T4-7 absorbs Beads task `via-jm4.7.8.1` (daemon config).

How to use:
- A worker's brief is its chunk section below plus the design sections it
  lists. Where this plan and the design disagree, the design wins: stop and
  report the conflict.
- Branch `wt/t4-<n>` from `rust-foundation` after every chunk in "Depends on"
  has merged. Write each named test first, record its failing (RED) run and
  then its passing (GREEN) run in `reports/T4-<n>.md` with the gate output.
- Chunks do not edit specs, `.repo-context/` or `docs/brainstorms/`: the
  orchestrator applies the §12 spec text separately, and only Close edits
  `.repo-context/verification.md`. Chunks implement the code side of §12.
- The coordinator merges after Sol returns SOUND, then reruns the full gate on
  `rust-foundation`.

## Rules for every chunk

- **Gate (G)**, from the repo root, all green:

  ```bash
  cargo fmt --all --check
  cargo clippy --locked --workspace --all-targets -- -D warnings
  cargo nextest run --locked --workspace
  cargo deny check
  python3 scripts/check-layers.py
  cargo clippy --locked --workspace --all-targets --features via-cli/test-failpoints -- -D warnings
  cargo nextest run --locked --workspace --features via-cli/test-failpoints
  cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08|09|10|12)_/)'
  cargo build --locked --release -p via-cli --no-default-features
  python3 scripts/check-release-features.py target/release/via
  ```

  plus the chunk's **selector**,
  `cargo nextest run --locked --workspace --features via-cli/test-failpoints -E 'test(/^<selector>_/)'`,
  where `<selector>` is the regex body the chunk gives below; it must select a
  nonempty set and pass. Record test counts.
- **Test placement.** Named tests are top-level functions in integration test
  files, so `^s1_` matches. Daemon scenarios live in `crates/via-cli/tests/`,
  run through `run_scenario` with an `Evidence`
  (`crates/via-cli/tests/support/scenario.rs`) and write their artifact under
  `scratchpad/`. Store- or Wire-level tests (design "at the Store level") live
  in that crate's `tests/` with the same prefix. Heavy tests (floods, maximal
  lines, 32 sockets) compile only under `test-failpoints` (§13).
- **Seams.** New named failpoints join `POINTS` in
  `scripts/check-release-features.py`; new `VIA_TEST_*` overrides join its
  `OVERRIDES`; points of removed code leave `POINTS`. Test-only accessors
  (`Lanes::peak`, `Store::read_count`, `Store::blob_writes`,
  `wire::fallback_drops`) and fake-agent steps are `cfg(feature =
  "test-failpoints")` or test-binary code and are not activation inputs.
- **Existing tests** that a chunk's change invalidates are updated in that
  chunk, never weakened; the report lists each with the reason.
- **Constraints** (§0, §1): no new dependency, debug RPC or CLI verb outside
  C1; A13 primitives only; T3 lock order; no std mutex across an `.await`;
  code holding `Lanes` awaits nothing; progress takes only the slot state
  mutex.
- **Commits**: explicit paths, small reversible commits on the chunk branch.

## Chunks

| ID | Title | Design sections | Depends on | Parallel-safe with | Risk | Size |
|---|---|---|---|---|---|---|
| T4-1 | Schema v6 and the evidence folder (raw log removed) | §6.6, §7.1–§7.5, §4.4, §6.7 `evidence_refs`, §8.6 last ¶, §2.1 raw parts, §6.4 identity bullet | — | none (touches every crate) | deep | L, ≈3 d |
| T4-2 | Store runtime: lanes, cap, internal reads, blob path, JSON limits | §6.1–§6.3, §6.4 bytes/cap/blobs, §6.5, §6.7 `terminal_facts`, §10.2 scanner, §5.3 full-disk bullet | T4-1 | T4-3 | deep | L, ≈3 d |
| T4-3 | Vendor pipeline: Wire tasks, serviceability, observation bounds | §8.1–§8.6, §9, §2.3 Bounds and Stall | T4-1 | T4-2 | deep | L, ≈3 d |
| T4-4 | Observations, progress, steps and `status` | §2.1, §2.2, §2.3 table, §2.4–§2.6, §3, §4.2, §6.7 `session_status`, §11.3 | T4-2, T4-3 | none | deep | L+, ≈3.5–4 d |
| T4-5 | C1 connections, request intake, `wait` and `result` | §4 intro, §4.1, §5.2, §10.1–§10.4, §11.1, §6.7 `result_text` | T4-4 | none | deep | L, ≈3 d |
| T4-6 | Final text, envelope bounds, `events`, `list`; F24 memory gate | §2.3 `final_text` and No envelope overrun, §6.4 envelope and final-text file, §4.3, §4.5, §6.7 pages, §6.8, §5.1 | T4-5 | T4-7 | deep | L, ≈3.5 d |
| T4-7 | Daemon config, `via.log`, disk floor, WAL; `daemon/status`, `describe`, `models` | §5.3, §5.4, §5.5, §7.6, §11.2, §4.6 | T4-5 | T4-6 | standard | L, ≈3 d |
| Close | Task 4 acceptance (coordinator) | §13, §14 | T4-6, T4-7 | — | — | S |

Order: T4-1 → (T4-2 ∥ T4-3) → T4-4 → T4-5 → (T4-6 ∥ T4-7) → Close.

Parallel pairs and their only shared files:
- **T4-2 ∥ T4-3.** `crates/via-core/src/engine/drive.rs`: T4-2 owns `dispatch`
  (blob load), `decide` (`terminal_facts`) and `finish` (Lifecycle handle); T4-3
  owns `execute` and `observe` (channel, `while_polling`). `crates/via-wire/src/lib.rs`:
  T4-2 adds one re-export line. The prompt meets at a `String`: T4-2 loads it,
  T4-3 streams it.
- **T4-6 ∥ T4-7.** `crates/via-store/src/runtime.rs` and `runtime/sql.rs`
  (T4-6 adds commands and reads; T4-7 edits open and the commit path), `drive.rs` (T4-6 accumulation and
  `finish`; T4-7 `dispatch`), `crates/via-core/src/api.rs` (T4-6 DTOs and
  receipt maxima; T4-7 error constants), `crates/via-cli/src/server/dispatch.rs`
  and `crates/via-cli/src/main.rs` (different method arms and verbs).
- The chunk merged second rebases and reruns G. Every other pair is
  sequential: T4-4 and T4-5 share `dispatch.rs`, `api.rs`, `queue.rs`,
  `drive.rs` and `terminal.rs`, and T4-5's `cwd` test reads `status`.

## T4-1 Schema v6 and the evidence folder (raw log removed)

**Goal.** Store is at schema v6 in full, and VIA keeps no copy of vendor
traffic: each submitted turn has an evidence folder with the OS-written
`stderr.log` and, on a decode failure, `undecoded.bin`; `logs` returns where
the evidence is.

**Implements.** §6.6 (every v6 change, golden-frozen); §6.4 last bullet and
§6.5 identity (stored as `identity_len` + `identity_sha256`, compared by both);
§7.1–§7.5; §7.2 `EvidenceRoot` in `RuntimeResources`; §7.3 `keep_undecoded`
for route decode failures and Wire's `MessageTooLarge` and unterminated tail
on the current reader; §7.4 `transcript?` on
`session.vendor_identity_confirmed`, committed with `vendor_session_id`; §4.4
`logs`; §6.7 `evidence_refs`; §2.1 (events carry no `raw_ref`; no
`raw_log.incomplete`); §8.6 last paragraph; C1 `evidence` envelope member.
Code side of A15, A27, A28 (columns), A40 (`evidence`), A41, A44, A46.

v6 write sites populated here with the simplest correct value: `sessions`
`created_ms`, `updated_ms` (the `at` of each transaction's highest-`seq`
event), `harness`, `label` (NULL until T4-5), `ord` from `session_ord`,
`vendor_session_id`, `transcript_hint`; `events.turn`, `events.type`;
`turns.ended_seq` on every terminal path (normal, forced, failure batch,
recovery); `turns.prompt_blob` NULL (T4-2 fills it); `turns.evidence_dir`
with `turn.submitted`; strict `at` → Unix ms. The `steps` table is DDL only;
T4-4 adds its writes and reads.

**Owned paths.** `crates/via-store/src/{lib.rs,runtime.rs,runtime/sql.rs}`,
new evidence module in via-store; delete `crates/via-store/src/runtime/raw.rs`;
`crates/via-wire/src/{lib.rs,runtime.rs}`; `crates/via-host/src/{host.rs,anchor.rs}`
(`PrivateProcessSpec.stderr_path`, `OwnedPipes` without `stderr`);
`crates/via-routes/src/{lib.rs,runtime.rs}` and `crates/via-adapters/src/{lib.rs,runtime.rs}`
(raw references only); `crates/via-core/src/{api.rs,engine.rs,engine/*.rs}`
(raw references, recovery, submission, identity confirmation, `logs`);
`crates/via-cli/src/server/dispatch.rs` (`logs` DTO);
`crates/via-fake-agent/src/main.rs` (`Stderr { bytes }` step); test files
listed below; `scripts/check-release-features.py`.

**Removes.** The raw worker, `RawFactory`, `RawWriter`, `DurableRaw`,
`RawStream`, `RawRef` and `StoreError::Raw`/`StoreFailureKind::Raw`;
`raw_ref` on events, observations, `FakeObservation`, `FakeTerminalEvidence`,
`InterruptReport` and `DriverHealth`; `raw_log.incomplete` and the
`raw_log_incomplete` warning (recovery `crates/via-core/src/engine/recovery.rs:393-522`,
drive `raw_incomplete` and raw terminal `crates/via-core/src/engine/drive.rs:672-760`,
`:1176-1224`, `crates/via-core/src/engine/journal.rs:443-467`,
`crates/via-core/src/engine.rs:176-177`, `:465`); the `connections` table and
`events.connection_id/raw_offset/raw_len` (the identity generation check moves
to memory, keyed by the derived connection ID, `crates/via-core/src/engine/drive.rs:48-58`); `raw/`
validation; envelope `raw_spans`; the raw-excerpt `logs` read (`read_logs`,
`crates/via-store/src/runtime/sql.rs:1614`); Wire `record`, `RawEvidence`, `WireConnection.{raw,evidence,stderr}`,
the stderr arm of `read_either`, `drain_pipes`, `LAUNCH_DRAIN`,
`WireError::Acquire.raw`; `POINTS` `raw.append.fail`,
`raw.sync.fail_persistent`; tests `crates/via-store/tests/raw_bounds.rs`,
`a_message_cannot_point_at_a_different_raw_span`,
`s1_f12_raw_failure_records_incomplete`,
`s1_f12_raw_incomplete_reply_lost_is_written_once`, and the raw parts of
`crates/via-core/tests/{route_stop.rs:366-423,route_stream.rs,force_stop.rs:241-245}`
and `crates/via-cli/tests/evidence_collector.rs:50-61` (the collector copies
evidence folders instead).

**Tests.** `s1_store_v6_schema_is_frozen`; `s1_store_identity_compares_length_and_sha256`
(§13.2 last row); `s1_evidence_stderr_is_written_by_the_os_and_listed`;
`s1_evidence_undecoded_message_is_saved_and_named`;
`s1_evidence_folder_failure_fails_store_before_launch`;
`s1_c1_logs_selects_the_turn_and_never_reads_files`. Update the v5 open test
to "fresh Store is v6; a v5 Store is refused untouched".

**Seams.** Fake step `Stderr { bytes }`. `store.read.corrupt.logs` stays, now on
`evidence_refs`. Remove the two raw points.

**Acceptance.**
- `rg -n 'RawRef|raw_ref|RawFactory|RawWriter|DurableRaw|RawStream|raw_log|raw_incomplete|raw_offset|RawEvidence|LAUNCH_DRAIN|drain_pipes' crates scripts`
  returns nothing (`RawValue`, `EmitRaw` and the fake's `emit_raw` are unrelated).
- Every v6 `CHECK` holds on every write path, shown by the existing lifecycle,
  recovery and failure-batch suites passing on v6.
- A recovered turn's `unknown` envelope equals T3's except for the removed raw
  warning.
- G green; selector `s1_(store_v6|store_identity|evidence|c1_logs)`.

**Out of scope.** Lanes, blobs and the cap (T4-2); the Wire task rewrite
(T4-3; this chunk edits the current reader only); `steps` writes (T4-4);
`label`, frozen `cwd`/`allow_untested` (T4-5); final text file (T4-6);
`via.log` (T4-7); reusable-connection evidence (§0).

**Sol scrutiny.** `ended_seq` equivalence on every terminal path including
recovery and the Latch batch; `updated_ms` set in every event-carrying
transaction; `session_ord` never reused; folder creation before Host
acquisition, `create_new` on `<turn>`, each parent synced once; a folder or
file failure is `failed(store)` with no anchor intent; stderr handed to the
anchor as a file and never read by VIA; the in-memory generation check is
equivalent to the dropped table's; hashed identity keeps C1's byte-identical
retry rule; `logs` stats on the blocking pool and never opens file contents.

## T4-2 Store runtime: lanes, cap, internal reads, blob path, JSON limits

**Goal.** The Store serves four bounded lanes with a fence and a death guard,
refuses oversized commands before queueing, answers every internal envelope
question without parsing a `Value`, and stores large prompts as verified
blobs.

**Implements.** §6.1 (Latch, Lifecycle, Internal, Public; `StoreClient::{public,lifecycle,latch}`;
service order; `NotEnqueued` mapping; Host journal `try_send` kept); §6.2
(`finish` on Lifecycle, `finish_with` Internal, the failure-resolution unit on
Latch, the shutdown pipeline on Lifecycle); §6.3; §6.4 `Command::bytes()` and
the cap (terminal envelope excluded; T4-4 adds carried rows); §6.5 in full,
with its first consumer: an inline `prompt` over `INLINE_MAX` = 256 KiB is
written to a blob before `admission`, adopted as `turns.prompt_blob` or
discarded after release, and loaded at dispatch with SHA-256 and UTF-8 checks;
`verify_blobs`/`sweep_blobs` before admission; §6.7 `terminal_facts`, replacing
every internal envelope `Value` read (in `crates/via-core/src/engine/`:
`control.rs:48`, `journal.rs:550`, `stop.rs:632`, `batch.rs:85`, `drive.rs:557`); §10.2 `json_limits::scan` in
`crates/via-store/src/json_limits.rs`, re-exported by via-wire and via-core,
and run on every C1 line before today's decode (`parse_error`); §5.3 "Full
disk" (`SQLITE_FULL`/`ENOSPC` rolls back as known `NotCommitted`; only a failed
rollback latches); `ApiError` `STORE_QUEUE_FULL`, with every C1 read handler on
the Public handle. Code side of A2, A10 (scanner), A30 (cap).

**Owned paths.** `crates/via-store/src/**` (new `lanes`, `blob`, `json_limits`
modules), `crates/via-store/tests/`; `crates/via-core/src/engine/{batch.rs,stop.rs,control.rs,journal.rs,latch.rs,receipt.rs,read.rs}`,
`drive.rs` (`dispatch`, `decide`, `finish` only), `crates/via-core/src/{api.rs,lib.rs}`;
`crates/via-wire/src/lib.rs` (re-export line); `crates/via-cli/src/server/dispatch.rs`
(scan before decode).

**Removes.** The `sync_channel(128)` request queue and the blocking
`Shutdown` send (`crates/via-store/src/runtime.rs:1021`); every daemon-internal parse of a stored
envelope into a `Value`; inline storage of prompts over 256 KiB.

**Tests.** `s1_store_full_disk_rolls_back_known`. From §13.2's last row,
plan-named: `s1_store_writer_death_fails_every_lane_writer_lost`,
`s1_store_lanes_serve_in_order_and_fence_refuses_later_pushes`,
`s1_store_latch_batch_with_largest_cwd_fits_its_lane` (a 4 KiB `cwd` in the
envelope), `s1_store_public_saturation_refuses_reads_without_latch`,
`s1_blob_prompt_over_inline_max_is_a_verified_blob`,
`s1_blob_torn_and_mismatched_blobs_are_refused_or_swept`. Plan-named for
§10.2's inferred claim: `s1_bounds_json_limits_agree_with_serde_json`
(proptest, seeded; depth 65 and node 65,537 fail).

**Seams.** Points `store.writer.before_serve`, `store.rollback.fail`,
`blob.write.fail_after` (`fail_io` at occurrence *k* fails the *k*-th chunk);
accessors `Lanes::peak(lane)`, `Store::blob_writes()`.

**Acceptance.**
- `rg -n 'sync_channel' crates/via-store/src` finds no request queue.
- No `serde_json::from_*::<Value>` of a stored envelope in the daemon's Core
  paths other than C1 `result`/`wait`/`await_terminal`, which T4-5 moves to
  `result_text`.
- No file I/O while `admission` is held (review the receipt path).
- G green; selector `s1_(store|blob|bounds_json)`.

**Out of scope.** `result_text` and replies written as stored (T4-5);
`prompt_file` and streamed identity (T4-5); `events_page`, `list_page` (T4-6);
`session_status`, `CommitSteps` (T4-4); WAL policy and the disk floor (T4-7);
`json_limits::shape`/`string_list` (T4-5); the vendor use of `scan` (T4-4).

**Sol scrutiny.** Only queue and counter updates under the `Lanes` mutex,
replies dropped after release; `DeadGuard` fails every lane and the in-flight
item `WriterLost` on unwind too; a Public `NotEnqueued` never latches and a
mutation's stays `not_committed`; `Command::bytes` is an exhaustive match over
what the thread binds; a batch is never split; each blob is `create_new`,
0600, `NOFOLLOW`, synced with `blobs/`, referenced only after `finish` in the
same transaction and discarded on every path that does not adopt it; blob I/O
on the blocking pool under 2 s; `scan` tracks string boundaries and escapes
exactly.

## T4-3 Vendor pipeline: Wire tasks, serviceability, observation bounds

**Goal.** No wait on the vendor path hides a control: Wire reads stdout in its
own task into a bounded queue, writes stdin from its own task, keeps one
health latch and ends with one deadline; Route, Adapter and Core each keep
controls serviceable; the observation channel is 1024 items and 4 MiB with a
10 s stall that fails `overflow`.

**Implements.** §8.1 (`WireSender`/`WireMessages`, `into_parts`,
`VendorMessage` as bytes, `keep_undecoded` on `WireSender`); §8.2 (64 KiB
reads, `LineSplitter`, 1 MiB assembly, 64-message/4 MiB queue with
`try_send`, discard mode); §8.3 (stdin task, data `mpsc(1)`, control `mpsc(8)`
coalesced, `PendingWrite`, streamed `OutboundMessage::Start` in 16 KiB
slices); §8.4 `ConnectionLatch`; §8.5; §8.6 `finish(deadline)`, straggler
handoff to `WireRuntime` and `WireShutdown.pending_tasks`, `Drop` fallback
with `fallback_drops()`; every `run_turn` exit calls `finish` once; §9 (Route
biased select with pinned write and `reserve()`; Adapter pinned pending
delivery; Core `while_polling` around every drive-loop commit); §2.3 Bounds
(`mpsc::channel(1024)`, per-drive 4 MiB `Semaphore`, item cost
`512 + Σ(64 + len)`, hop `mpsc(1)`) and Stall (one deadline `first_block +
10 s`, hop receiver dropped, Route's `hop.closed()` arm → `Overflow`). Code
side of A29 (`deliver`, `forward`), A41 (Wire shape).

**Owned paths.** `crates/via-wire/src/**`, `crates/via-wire/tests/`;
`crates/via-routes/src/runtime.rs`, `crates/via-routes/src/lib.rs` (start
encoding only); `crates/via-adapters/src/runtime.rs`;
`crates/via-core/src/engine/drive.rs` (`execute`, `observe`, new
`while_polling`); `crates/via-fake-agent/src/main.rs` (`HoldStdin`).

**Removes.** Inline 8 KiB reads in `next_message`, `read_either`,
`drain_to_eof`, inline `write_message` awaits in `on_wake`, the two-arm
`forward`, `FakeStart`'s single byte string (`crates/via-routes/src/lib.rs:47-71`),
the `mpsc(64)` hop and `channel(64)` observations, unused `WireHealth`
(`crates/via-wire/src/lib.rs:103`).

**Tests.** `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_messages`
(seeded proptest splitter plus a daemon scenario for the saved prefix);
`s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output`;
`s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib`;
plan-named from §13.2's last row `s1_wire_finish_joins_and_hands_off_stragglers`;
plan-named for §9 `s1_wire_route_services_cancel_while_stdin_is_held`. Every
test asserts `fallback_drops() == 0` where it can read it.

**Seams.** Point `core.observations.pause`; override `VIA_TEST_EVENT_STALL_MS`;
accessor `wire::fallback_drops()`; fake step `HoldStdin`.

**Acceptance.**
- `rg -n 'read_either|drain_to_eof|FakeStart|WireHealth' crates` returns nothing.
- The stdout reader awaits no consumer and no Store; every `select!` arm in
  Wire, Route and the Adapter is cancel-safe (report lists each).
- G green; selector `s1_(f27|f24|wire)`.

**Out of scope.** The observation set and decode rules (T4-4); final text
pieces (T4-6); the F24 flood and RSS gate (T4-6); shared-route quarantine
(Codex task).

**Sol scrutiny.** First failure wins in `ConnectionLatch`; queue accounting
counts before `try_send` and releases on receive; discard mode reads to EOF
keeping nothing; a partial write then deadline closes stdin and answers
`Indeterminate`; `finish` drains to `deadline − 250 ms`, aborts, joins until
`deadline` and hands off the rest; the stall deadline restarts only on
acceptance; a permit is held until Core has handled the item;
`while_polling` keeps an early adapter result; how daemon scenarios observe
`fallback_drops` (e.g. the shutdown summary) is stated and checked.

## T4-4 Observations, progress, steps and `status`

**Goal.** Text, reasoning, tool and usage messages stop being events (R2):
they feed an in-memory progress snapshot (R3) and one `steps` row per model
step (R4), and `status` returns one consistent moment of one turn (R5).

**Implements.** §2.1 (Core commits only the durable set); §2.2 (typed decode
with `IgnoredAny`; rules 1–4: 1 KiB short fields, 256 KiB payloads,
`json_limits::scan` on every vendor message, no peer `Value`/`flatten`/
`untagged`/internally or adjacently tagged enum; every failure saved through
`keep_undecoded` before `protocol`; the fake keeps `vendor_turn_id`,
`tool_id`, `name` and a ≤ 256 B type tag); §2.3 table (`progress` item;
adapter events unchanged); §2.4 (step tracker in `TurnRecord`,
`Slot::publish_progress`, `TurnActivity`, tokens, bounds); §2.5 fake row
(including the fake `usage` message); §2.6; §3.1–§3.4 (`CommitSteps` on
Internal under `while_polling`, publish before enqueue, open-step row in every
`TurnRecord` terminal including `finish` and `forced_terminal`, refused rows
carried in the `failed(store)` terminal, cap excludes carried rows); §4.2;
§6.7 `session_status` (debug-asserted `STATUS_MAX`); §11.3 including the
`live_armed` chain Host → Wire → Routes → Adapters → Core; CLI `status --turn
--after-step --limit`; envelope `steps` = vendor count or `null`. Code side of
A16, A23, A24 (`drive.rs:1775-1786`, `:1844-1884`), A26, A28 (ordering), A33
(fake).

**Owned paths.** `crates/via-routes/src/lib.rs` (fake decode),
`crates/via-adapters/src/{lib.rs,runtime.rs}` (observation set, activity
clock); `crates/via-core/src/engine/{drive.rs,queue.rs,terminal.rs,stop.rs,read.rs,status.rs}`
and new `progress` module; `crates/via-core/src/api.rs` (status DTO);
`crates/via-store/src/runtime{.rs,/sql.rs}` (`CommitSteps`, terminal rows,
`session_status`, `read_count`, read delay); `crates/via-store/src/failpoint.rs`;
the `live_armed` hop in `crates/via-{host,wire,routes,adapters}/src/`;
`crates/via-cli/src/{main.rs,server/dispatch.rs}` (`status`).

**Removes.** Production of `assistant.text`, `reasoning.summary`,
`tool.started`, `tool.ended`, `usage.updated`, `file.changed`, `vendor.other`;
the `assistant.text` splitter (`crates/via-adapters/src/runtime.rs` `split_text`;
T4-6 adds the final-text splitter); the fake route's text copy and 16 KiB
unknown copy (`crates/via-routes/src/lib.rs:340-344`, `:518-537`); the old
`progress()` rule. Existing scenarios that synchronize on `assistant.text`
(`s1_recovery.rs`, `s1_sessions.rs`, `s1_prompt_to_result.rs`,
`s1_daemon_stop.rs`, `s1_store_failure.rs`, `crates/via-core/tests/route_stream.rs`,
`journal/tests.rs`) move to fake gates or `status`; `s1_f12_event_not_committed_stops_turn_and_reuses_seq`
uses a step-row commit and `cancel.requested` (A24).

**Tests.** `s1_progress_step_rule_counts_output_after_tool_results`;
`s1_progress_snapshot_adds_no_store_read`; `s1_c1_status_latency_under_bounded_store_delay`;
`s1_c1_status_progress_only_for_the_selected_turn`;
`s1_progress_tokens_sum_per_step_and_label_scope`;
`s1_progress_tools_overflow_and_untracked_end_count`;
`s1_progress_unknown_messages_send_no_observation`;
`s1_progress_step_rows_survive_crash_to_last_commit`;
`s1_progress_step_commit_refused_rows_ride_in_terminal`;
`s1_progress_many_steps_all_have_rows`;
`s1_progress_forced_shutdown_terminal_carries_open_row`;
`s1_store_steps_delete_is_one_keyed_range`;
`s1_c1_status_every_member_after_eviction_and_restart`;
`s1_c1_status_alive_false_after_exit_before_control_drop`. Plan-named for
§2.2 rules 1–3: `s1_progress_decode_limits_fail_protocol_with_message_saved`.

**Seams.** Points `store.commit.step`, `core.progress.publish`,
`core.finish_running.pause`, `store.read.delay_ms`; accessor
`Store::read_count()`. The failpoint controller today supports `pause`,
`crash` and `fail_io` only; this chunk adds the smallest extension daemon
scenarios need, a numeric value on a command (the delay) and a hit count the
harness can read, both absent from release. T4-7 reuses it.

**Acceptance.**
- A grep of `Deserialize` types in via-routes, via-adapters and via-core finds
  no `serde_json::Value`, `flatten`, `untagged`, or internally or adjacently
  tagged enum on a peer-fed type (§2.2 rule 4); the report lists the grep.
  (via-cli's C1 DTOs are T4-5's.)
- `status` makes exactly one Store read; `progress` is taken under the slot
  mutex without await.
- G green (including `s1_f(08|09|10|12)_`); selector `s1_(progress|c1_status|store_steps)`.

**Out of scope.** Final text pieces and envelope lists (T4-6); `cwd` values
in `status` (T4-5 freezes them; until then `json_extract` yields `null`);
per-vendor mappings beyond the fake (§2.5, vendor tasks).

**Sol scrutiny.** The step rule is the one reducer (marks from the Adapter
only); a message's `model` mark applies before its tool starts; row N is
enqueued only after N + 1 is published and awaited before the next
observation; every `TurnRecord` terminal carries the open row, and a
`turn.ended` built from it implies every row is durable; a known
`NotCommitted` upgrades the stop to `store` and carries rows, an uncertain one
latches; `status` shows `progress` only when the selected turn is non-terminal
in that read and `Running.turn` matches; usage keys supersede within a key and
add across keys, the 17th key folds into a keyless sum; idle resets only on
acceptance and `model`/tool marks; migrated tests keep their original
assertions.

## T4-5 C1 connections, request intake, `wait` and `result`

**Goal.** Callers are bounded by construction: 32 sockets, one request at a
time, 1 MiB lines with a named refusal, a 5 s partial-line deadline, a 10 s
reply deadline; large prompts arrive as `prompt_file`; requests are decoded
once without a peer `Value`; `wait` checks once per second and replies are the
stored envelope bytes.

**Implements.** §4 intro (sequential connection task, `REPLY_WRITE` = 10 s,
A31/A32); §4.1 (`result_text` on Public; `wait` via `terminal_facts` at 0 s
then each second; `result`, `wait` and `await_terminal` write the stored text,
no `Value`); §5.2; §10.1 (accept-loop `Semaphore(32)`, 33rd closed without
bytes; `request_too_large` −32020 with `data`, 2 s write bound, then close;
`id` ≤ 256 B); §10.2 C1 side (borrowed `RawValue` envelope, free-form members
as `Box<RawValue>`, `json_limits::shape` and `string_list`); §10.3 (streamed
identity over borrowed pieces; content `sha256:<hex>:<len>` for
`prompt_file`); §10.4; §11.1 (spawn members, frozen `{harness, model, cwd,
allow_untested}`, `label` column, `cwd` applied through
`FakeConfig::process_spec`, `QueuedTurn`, `execute`, envelope and `status`;
A3 wall default 3,600,000 ms; A9 nested `null` is `invalid_params`);
`ApiError::REQUEST_TOO_LARGE`; CLI spawn/resume flags `--prompt-file F|-`,
`--instructions`, `--cwd`, `--require`, `--allow-untested`, `--label`; `serve
--stdio`. Code side of A3, A9, A14, A31, A32, A39.

**Owned paths.** `crates/via-cli/src/{main.rs,client.rs,server.rs,server/serving.rs,server/dispatch.rs}`;
`crates/via-core/src/{api.rs,lib.rs}`, `crates/via-core/src/engine/{receipt.rs,read.rs,queue.rs}`,
`drive.rs` and `terminal.rs` (`cwd` only); `crates/via-adapters/src/fake_config.rs`
and the `execute` signature; `crates/via-store/src/json_limits.rs`
(`shape`, `string_list`), `result_text` in the Store; `crates/via-fake-agent/src/main.rs`
(`ReportCwd`, `EchoPromptDigest`).

**Removes.** `MAX_LINE` = 16 MiB and the silent oversize break
(`crates/via-cli/src/server/dispatch.rs:22`, `:45-46`); the whole-request
`Value` decode (`:48`) and `raw_params` identity over exact bytes; the
uncapped `admit` spawn (`crates/via-cli/src/server/serving.rs:245`); the
20 ms `wait` poll (`crates/via-core/src/engine/read.rs:100`); the
nested-null-as-omitted read (`crates/via-core/src/api.rs:88`); the 30,000 ms
fake wall default (`crates/via-core/src/api.rs:884`); `self.cwd` in `process_spec` and the
envelope's `cwd: None`.

**Tests.** `s1_c1_request_too_large_is_named_then_closes`;
`s1_c1_prompt_file_copies_hashes_and_refuses_changes` (without the
"below a lowered floor" clause, which T4-7 adds); `s1_c1_request_id_over_256_bytes_is_invalid_request`;
`s1_c1_reply_not_read_closes_the_socket`;
`s1_c1_wait_checks_each_second_and_32_waiters_leave_status_served`;
F5, plan-named: `s1_f05_oversize_line_is_refused_and_closed`,
`s1_f05_depth_and_node_limits_are_parse_errors`,
`s1_f05_partial_line_deadline_is_per_connection`,
`s1_f05_33rd_socket_is_closed_without_bytes`. Plan-named for §4.6 and §11.1:
`s1_c1_serve_stdio_matches_the_socket` (one scripted sequence, oversize
included); `s1_c1_cwd_is_frozen_applied_and_reported` (`ReportCwd`, envelope
and `status`); `s1_c1_wall_default_and_nested_null_deadlines`.

**Seams.** Point `prompt_file.copy.pause`; overrides `VIA_TEST_PARTIAL_LINE_MS`,
`VIA_TEST_REPLY_WRITE_MS`; fake steps `ReportCwd`, `EchoPromptDigest`.

**Acceptance.**
- The §2.2 rule-4 grep now covers via-cli and via-core's C1 DTOs; with T4-4's
  it spans all four crates.
- No daemon path parses a stored envelope into a `Value`
  (`rg -n 'result\(|await_terminal' crates/via-core/src` reviewed).
- `prompt_file` I/O and blob writes happen with no lock held; under
  `admission` only lookup, checks and commit run.
- G green; selector `s1_(c1|f05)`.

**Out of scope.** Receipt maxima for `bound`/`vendor`/`model`/`effort` (T4-6,
with their envelope test); `describe`, `models`, `UNKNOWN_MODEL` (T4-7); the
disk floor and WAL refusal at receipt (T4-7); `events`/`list` DTOs (T4-6).

**Sol scrutiny.** The permit is released on every connection exit; the 5 s
deadline starts at a line's first byte and an idle connection has none; after
`request_too_large` nothing more is read; the reply timer starts before the
first write; the streamed identity equals the byte-identical rule's except
that a prompt file contributes its content hash; `prompt_file` opens with
`O_NONBLOCK`, `fstat`s before and after, refuses any change by reason, and
bounds the pass at 10 s; a blob not adopted is discarded after `admission` is
released; `close` and daemon stop never wait for a copy; `serve --stdio` is a
byte proxy with one `JoinSet` and stdin EOF shuts the socket's write side.

## T4-6 Final text, envelope bounds, `events`, `list`; F24 memory gate

**Goal.** The envelope is at most 1 MiB by construction and no turn fails for
a large result (R6); `events` and `list` page bounded (R5); the F24 flood
proves the §5.1 memory account.

**Implements.** §2.3 `final_text` row (completed text only, cut by a counting
writer so the whole observation is ≤ 256 KiB, all pieces before
`turn.vendor_terminal`, which loses `final_text`) and "No envelope overrun";
§6.4 envelope table (`final_text` inline ≤ 256 KiB; lists keep the first
1,000 entries ≤ 256 B each with `*_total`; one warning per code; `failure`
message ≤ 2 KiB; receipt refusals of `bound` > 32 KiB, `vendor` > 16 KiB,
`model`/`effort` > 1 KiB), "Final text file" (`StoreClient::final_text_file`,
appends under `while_polling` with 2 s, 64 MiB cut, `finish()` syncs file then
folder), "A final-text write or sync failure"; §4.3 and §6.7 `events_page`
(one transaction, SQL predicates on `turn`/`type`, `PAGE_MAX`, `next_after`,
`more`, `earliest_seq` 1, `follow` an unknown field); §4.5, §6.7 `list_page`,
§6.8 (`l3.<ord>` cursor, filters, `last_active_at`, CLI `list`); `unsubscribe`
stays `method_not_found`; §5.1 via the F24 gate. Code side of A25, A29
(`final_text`), A30 (envelope), A38, A40 (`final_text_file`, totals), A43.

**Owned paths.** `crates/via-routes/src/{lib.rs,runtime.rs}` (final text
pieces), `crates/via-adapters/src/{lib.rs,runtime.rs}` (the observation and
splitter); `crates/via-core/src/engine/{drive.rs,terminal.rs,read.rs}`
(accumulation, `finish`, envelope, pages), `crates/via-core/src/api.rs`
(DTOs, receipt maxima); `crates/via-store/src/runtime{.rs,/sql.rs}`
(`final_text_file`, `events_page`, `list_page`); `crates/via-cli/src/{main.rs,server/dispatch.rs}`
(`events`, `list`); the F24 test and its `/proc/<pid>/status` sampler.

**Removes.** The first-page-only `events` (`crates/via-core/src/engine/read.rs:126-135`) and
`Store::events(…, 1, 1000)`; `final_text` on the vendor terminal and in
`FakeTerminalEvidence`. An A34 envelope-overrun path was not found at
`67e2788` (`rg -in 'overrun'`); the chunk re-greps and removes any it finds.

**Tests.** `s1_bounds_final_text_spills_to_a_file`;
`s1_bounds_envelope_at_every_member_maximum_fits_1_mib`;
`s1_bounds_final_text_piece_fits_256_kib`; `s1_c1_events_page_filters_and_bounds`;
`s1_c1_follow_and_unsubscribe_are_refused`; `s1_c1_list_creation_order_and_last_active`;
`s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control`.

**Seams.** Points `final_text.write.fail`, `final_text.write.short`,
`final_text.sync.fail`; override `VIA_TEST_FINAL_TEXT_FILE_MAX`; the RSS
sampler (test code).

**Acceptance.**
- F24: peak RSS less the idle baseline ≤ 1.25 × §5.1's sum; growth < 32 MiB
  after the first 64 MiB of a 256 MiB flood; each anchor ≤ 32 MiB; controls
  answer within 100 ms. A failure goes back to design review; no limit is
  enlarged (A43).
- `PAGE_MAX` and `ENVELOPE_MAX` hold by debug assertion, with no refusal path.
- G green; selector `s1_(bounds|c1_events|c1_follow|c1_list|f24)`.

**Out of scope.** `structured_output` (null, §0); retention and
`history_pruned` (`via-jm4.18`); disk and WAL (T4-7).

**Sol scrutiny.** Pieces concatenate to the exact text and each encodes ≤ 256
KiB including escapes; the spill happens at exactly one byte past 256 KiB
encoded, writing the held text then the piece; the file is `create_new`,
0600, `NOFOLLOW` in the turn folder; a short write truncates to the last
complete character, records `bytes` and `truncated`, syncs, and names the
file only if that succeeds; the envelope never names a non-durable file;
list entries are cut at a character boundary and cite the full event; the
events scan checks a row's borrowed length before copying; the `list`
guarantee (every session present at the first page examined once) and
termination; F24's baseline and sampling method.

## T4-7 Daemon config, `via.log`, disk floor, WAL; `daemon/status`, `describe`, `models`

**Goal.** Disk and WAL thresholds are daemon config read once at start (R7,
`via-jm4.7.8.1`); new work stops below a free-space floor or at the WAL limit
while admitted turns finish; the daemon's own lines go to `via.log`;
`daemon/status`, `describe` and `models` answer from memory.

**Implements.** §5.5 (`daemon.json`, strict DTOs, validation, exit 78 before
any Store or socket change, the CLI reports it); §7.6 (`DaemonLog`, rotation
past 10 MiB, startup flag cleared before `main.serve`, shutdown summary as one
line, `session`/`turn` fields on every session or turn `tracing` call); §5.3
(statvfs on the blocking pool before `admission`, applied only after the key
lookup finds no key; queued turn fails `store` at dispatch; `DISK_FREE_FLOOR`;
`storage` with a shared 60 s cached walk); §5.4 (`wal_autocheckpoint`,
commit counter, `journal_size_limit`, WAL length after each commit,
`wal_full` with `TRUNCATE` retry at most once a second, refusal before
`BEGIN` as `Quota`/`wal_full`, no health latch); §11.2 (`started_at`,
`Sessions.open` tally, `idle`/`active`/`closing`); §4.6 `describe`, `models`,
`ApiError::UNKNOWN_MODEL` at `crates/via-core/src/engine/receipt.rs:100`, CLI verbs `describe`,
`models`. Code side of A4, A37, A42, A45.

**Owned paths.** `crates/via-cli/src/{server.rs,server/shutdown.rs,client.rs,main.rs}`,
new `crates/via-cli/src/server/config.rs`; `crates/via-store/src/{runtime.rs,runtime/sql.rs}`
(open parameters, commit path, statvfs); `crates/via-core/src/{engine.rs,api.rs}`,
`crates/via-core/src/engine/{receipt.rs,status.rs,close.rs}`, `drive.rs`
(`dispatch` only); `crates/via-cli/src/server/dispatch.rs` (`daemon/status`,
`describe`, `models` arms).

**Removes.** The stderr `tracing` writer after startup
(`crates/via-cli/src/server.rs:96-99`) and the stderr shutdown summary
(`crates/via-cli/src/server/shutdown.rs:146`); the hard-coded `idle: 0` in `daemon/status`.

**Tests.** `s1_config_is_read_at_start_validated_and_reported`;
`s1_daemon_log_after_startup_and_rotation`; `s1_store_disk_floor_refuses_new_work_only`;
`s1_store_data_size_warning_is_cached`; `s1_store_wal_limit_refuses_only_new_work`
(also records WAL growth while the reader holds, for `via-d9o.2.3`, and checks
`journal_size_limit`); adds the below-floor clause to
`s1_c1_prompt_file_copies_hashes_and_refuses_changes`. Plan-named for §11.2
and §4.6: `s1_c1_daemon_status_counts_describe_and_models`.

**Seams.** Point `store.statvfs.free_bytes` (a value, through T4-4's
extension); counter `core.data_size.walks`; lowered thresholds through
`daemon.json` (§13.1).

**Acceptance.**
- An invalid `daemon.json` leaves the state directory's Store files and the
  socket untouched (checked by the test).
- Lifecycle and terminal writes, cancels, closes, recovery, the Latch batch,
  Host absence records and step rows all commit below the floor and at
  `wal.max`.
- G green; selector `s1_(config|daemon_log|store_disk|store_data|store_wal|c1_daemon_status)`.

**Out of scope.** Retention or cleanup (`via-jm4.18`); any memory setting
(§5.1); measuring defaults (`via-d9o.2.3`).

**Sol scrutiny.** Config is read before `Store::open` and the socket, and
`via.log` opened after both locks and before `Store::open`; a keyed retry of a
stored receipt never meets the floor or `wal_full`; the floor is read before
`admission` is taken; a queued turn below the floor fails rather than waits;
`wal_full` refuses only receipts and dispatch, never an admitted turn's
writes; the `TRUNCATE` retry clears `wal_full` only below `wal.max`; the
data-size walk is shared by concurrent calls and runs on the blocking pool;
the open tally's +1/−1 points match §11.2.

## Close: Task 4 acceptance (coordinator)

After T4-6 and T4-7 merge, on `rust-foundation`:
1. Edit `.repo-context/verification.md`: the Task 4 selector becomes

   ```bash
   cargo nextest run --locked --workspace --features via-cli/test-failpoints -E 'test(/^s1_(f05|f2[47]|bounds|store|blob|wire|c1|progress|evidence|config|daemon_log)_/)'
   ```

   (`--workspace`, because Store- and Wire-level tests live in their crates;
   F25/F26 and `raw` are obsolete). In the same paragraph, `s1_raw_...`
   leaves the evidence-bearing list, "raw and event logs" becomes "evidence
   folders and event logs", and, unless the owner answers the open question
   otherwise, the evidence rule applies to daemon scenarios. Run the
   Markdown link check.
2. Run every command in verification.md's "S1 runtime acceptance" with the
   widened selector; record nonempty counts and durations.
3. Keep the scenario artifacts (summary, sha256 manifest, SQLite backups,
   event logs, evidence folders, report) under `scratchpad/t4/close/`, and
   write `reports/T4-close.md` with the §13 test → result table and the §15
   limitations as they stand.
4. A failing gate reopens the owning chunk; it is not fixed in Close.

## Coverage

### Design sections

| Section | Chunk |
|---|---|
| §0 scope, fixed decisions | all (constraints); out-of-scope rows: none |
| §1 owners, lock order, wakes | each row's chunk: config T4-7; lanes T4-2; blobs T4-2; evidence root and folder T4-1; `final_text.txt` T4-6; `via.log` T4-7; `ConnectionLatch`, reader/writer tasks, queue, hop, channel, stall T4-3; accumulation T4-6; step tracker, activity clock, published progress T4-4; data-size cache, tally T4-7; `Armed` controls T4-4 |
| §2.1 durable events | T4-1 (raw event, `raw_ref`); T4-4 (the rest) |
| §2.2 route decode | T4-4 |
| §2.3 observation channel | T4-3 (Bounds, Stall); T4-4 (table, `progress`); T4-6 (`final_text`, No envelope overrun) |
| §2.4, §2.5, §2.6 | T4-4 |
| §3.1–§3.4 steps | T4-1 (DDL); T4-4 (writes, reads) |
| §4 intro | T4-5 |
| §4.1 `wait`, `result` | T4-5 |
| §4.2 `status` | T4-4 |
| §4.3 `events` | T4-6 |
| §4.4 `logs` | T4-1 |
| §4.5 `list` | T4-6 |
| §4.6 other methods | `describe`, `models`, `daemon/status`, `UNKNOWN_MODEL`, `DISK_FREE_FLOOR` T4-7; `REQUEST_TOO_LARGE`, spawn CLI flags, `serve --stdio` T4-5; `STORE_QUEUE_FULL` T4-2; `status` CLI T4-4; `list` CLI, `unsubscribe` T4-6 |
| §5.1 memory | T4-3, T4-5, T4-6 (holders); T4-6 (F24 gate) |
| §5.2 C1 requests | T4-5 |
| §5.3 disk | T4-2 (Full disk); T4-7 (rest) |
| §5.4 WAL, §5.5 config | T4-7 |
| §6.1–§6.3 lanes, fence, death | T4-2 |
| §6.4 bytes, cap | T4-2 (T4-4 adds carried rows) |
| §6.4 envelope table, final text file, failure | T4-6 (receipt maxima included) |
| §6.4 blobs bullet | T4-1 (identity); T4-2 (blobs) |
| §6.5 blob path | T4-2; `prompt_file` use T4-5 |
| §6.6 schema v6 | T4-1 (frozen params `cwd`/`allow_untested` values T4-5) |
| §6.7 reads | `terminal_facts` T4-2; `result_text` T4-5; `events_page`, `list_page` T4-6; `evidence_refs` T4-1; `session_status` T4-4 |
| §6.8 `list` paging | T4-6 |
| §7.1–§7.5 evidence | T4-1 |
| §7.6 `via.log` | T4-7 |
| §8.1–§8.5 Wire | T4-3 |
| §8.6 `finish` | T4-3; last paragraph (drain removal) T4-1 |
| §9 serviceability | T4-3 |
| §10.1 sockets, lines, replies | T4-5 |
| §10.2 JSON limits | T4-2 (`scan`, re-exports, C1 pre-decode); T4-4 (vendor use); T4-5 (`RawValue` members, `shape`, `string_list`) |
| §10.3, §10.4 | T4-5 |
| §11.1 spawn members, `cwd` | T4-5 |
| §11.2 counts | T4-7 |
| §11.3 `status` members | T4-4 |
| §12 amendments | spec text: orchestrator. Code side: A2 T4-2; A3, A9, A14, A31, A32, A39 T4-5; A4, A37, A42, A45 T4-7 (A42 full disk T4-2); A10 T4-2/T4-4/T4-5; A13 all; A15, A27, A41, A44, A46 T4-1 (A41 Wire shape T4-3); A16, A23, A24, A26, A33 T4-4; A25, A38, A43 T4-6; A28 T4-1 (columns), T4-4 (ordering); A29 T4-3 (stall), T4-4 (set), T4-6 (`final_text`); A30 T4-2 (cap), T4-6 (envelope); A40 T4-1 (`evidence`), T4-4 (`steps`), T4-6 (file, totals) |
| §13.1 seams | the chunk named in each chunk's Seams |
| §13.2 tests | table below |
| §14 disposition | F5 T4-5; F24 T4-3, T4-6; F27 T4-3; F25, F26, follow, `unsubscribe`, `read_either`, raw log: removed in T4-6, T4-3, T4-1 as listed |
| §15 limitations | no code; recorded in Close |
| §16 measurements | `via-d9o.2.3`; only the F24 RSS gate (T4-6) and the WAL growth record (T4-7) run here |

### §13 tests

| Test | Chunk |
|---|---|
| `s1_store_v6_schema_is_frozen` (§6.6) | T4-1 |
| `s1_store_identity_compares_length_and_sha256` (§13.2 last row) | T4-1 |
| `s1_evidence_stderr_is_written_by_the_os_and_listed` | T4-1 |
| `s1_evidence_undecoded_message_is_saved_and_named` | T4-1 |
| `s1_evidence_folder_failure_fails_store_before_launch` | T4-1 |
| `s1_c1_logs_selects_the_turn_and_never_reads_files` | T4-1 |
| `s1_store_full_disk_rolls_back_known` | T4-2 |
| `s1_store_…` death guard, lanes and fence, Latch fit, Public saturation | T4-2 |
| `s1_blob_…` torn and mismatched blobs | T4-2 |
| `s1_wire_…` `finish` joins and hands off stragglers | T4-3 |
| `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_messages` | T4-3 |
| `s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output` | T4-3 |
| `s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib` | T4-3 |
| `s1_progress_step_rule_counts_output_after_tool_results` | T4-4 |
| `s1_progress_snapshot_adds_no_store_read` | T4-4 |
| `s1_c1_status_latency_under_bounded_store_delay` | T4-4 |
| `s1_c1_status_progress_only_for_the_selected_turn` | T4-4 |
| `s1_progress_tokens_sum_per_step_and_label_scope` | T4-4 |
| `s1_progress_tools_overflow_and_untracked_end_count` | T4-4 |
| `s1_progress_unknown_messages_send_no_observation` | T4-4 |
| `s1_progress_step_rows_survive_crash_to_last_commit` | T4-4 |
| `s1_progress_step_commit_refused_rows_ride_in_terminal` | T4-4 |
| `s1_progress_many_steps_all_have_rows` | T4-4 |
| `s1_progress_forced_shutdown_terminal_carries_open_row` | T4-4 |
| `s1_store_steps_delete_is_one_keyed_range` | T4-4 |
| `s1_c1_status_every_member_after_eviction_and_restart` | T4-4 |
| `s1_c1_status_alive_false_after_exit_before_control_drop` | T4-4 |
| `s1_c1_request_too_large_is_named_then_closes` | T4-5 |
| `s1_c1_prompt_file_copies_hashes_and_refuses_changes` | T4-5 (T4-7 adds the below-floor clause) |
| `s1_c1_request_id_over_256_bytes_is_invalid_request` | T4-5 |
| `s1_c1_reply_not_read_closes_the_socket` | T4-5 |
| `s1_c1_wait_checks_each_second_and_32_waiters_leave_status_served` | T4-5 |
| `s1_f05_…` oversize, depth and nodes, partial line, 33rd socket | T4-5 |
| `serve --stdio` parity (§4.6) | T4-5 |
| `ReportCwd` `cwd` test (§11.1) | T4-5 |
| `s1_c1_events_page_filters_and_bounds` | T4-6 |
| `s1_c1_follow_and_unsubscribe_are_refused` | T4-6 |
| `s1_c1_list_creation_order_and_last_active` | T4-6 |
| `s1_bounds_final_text_spills_to_a_file` | T4-6 |
| `s1_bounds_envelope_at_every_member_maximum_fits_1_mib` | T4-6 |
| `s1_bounds_final_text_piece_fits_256_kib` | T4-6 |
| `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` | T4-6 |
| `s1_config_is_read_at_start_validated_and_reported` | T4-7 |
| `s1_daemon_log_after_startup_and_rotation` | T4-7 |
| `s1_store_disk_floor_refuses_new_work_only` | T4-7 |
| `s1_store_data_size_warning_is_cached` | T4-7 |
| `s1_store_wal_limit_refuses_only_new_work` | T4-7 |
| A24 rewrite of `s1_f12_event_not_committed_stops_turn_and_reuses_seq` | T4-4 |
| Void (A46): `s1_f12_raw_failure_records_incomplete` and `s1_f12_raw_incomplete_reply_lost_is_written_once` | T4-1 deletes |

### Requirements

| Req | Chunks |
|---|---|
| R1 durable events | T4-1 (no `raw_log.incomplete`; `turn`/`type` columns), T4-4 (the durable set only) |
| R2 not stored or streamed | T4-4 (events stop, decode keeps only what R1–R6 need), T4-6 (final text pieces) |
| R3 progress snapshot | T4-4 |
| R4 step history | T4-1 (table), T4-4 (rows, crash survival, keyed delete) |
| R5 caller interface | T4-5 (`wait`), T4-4 (`status`), T4-6 (`events`, `list`), T4-1 (`logs`) |
| R6 envelope | T4-6 (final text file, lists and totals, 1 MiB), T4-4 (`steps`), T4-1 (`evidence`), T4-2 (cap excludes the terminal) |
| R7 bounds | memory T4-3, T4-5, T4-6 (F24); requests T4-5; disk T4-2 (full disk), T4-7; WAL, config T4-7; evidence caps T4-1 (`undecoded.bin`), T4-6 (`final_text.txt`) |
| R8 evidence without a raw log | T4-1; `via.log` T4-7 |

## Decisions made here

- **One schema owner.** T4-1 writes all of v6 because dropping the raw
  columns forces the raw removal, whose replacement is the evidence folder;
  later chunks only use columns.
- **`json_limits::scan` lands in T4-2** (its crate) with a real first use, the
  C1 line before today's decode; T4-4 and T4-5 add the other uses.
- **Reads land with their consumers** (§6.7 row by row, above), so no Store
  API waits unused for a later chunk.
- **Controller extension in T4-4.** The design's value and counter seams need
  a numeric command value and a readable hit count; T4-4, the first daemon
  scenario that needs them, adds both.
- **Cross-chunk clause.** The prompt-file test's below-floor clause is added
  by T4-7, which introduces the floor.
- **Close edits verification.md's evidence paragraph** as well as the selector
  (`s1_raw_` and "raw logs" are obsolete). This keeps the owner's evidence
  rule for daemon scenarios and does not extend it to Store- or Wire-level
  tests; the owner may prefer otherwise (see the open question).

## Open questions

None blocks a chunk. One for the owner before Close: verification.md
requires every `s1_store_...` test to emit daemon evidence, but §13.2 places
some `s1_store_`, `s1_wire_` and `s1_blob_` tests "at the Store level"
(in-process). The plan keeps them in their crates and has Close limit the
evidence rule to daemon scenarios; the alternative is running them as
daemon scenarios, which costs more and adds nothing.

Resolved 2026-09-29 (orchestrator, under the owner's standing autonomy
directive and "simple first"): take the default. Store- and Wire-level tests
stay in their crates; the evidence rule covers daemon scenarios.
