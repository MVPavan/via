# W1-C report: canonical events (T1-I5 Core/Store) and receipt/envelope shapes (T1-I6)

Branch `claude/w1-c-rust-foundation-68ta7d`. Base `639e77f`. All claims below
were checked in this cloud session unless marked otherwise.

## Regression (shared by both findings)

`s1_c1_events_receipt_and_envelope_shapes` in
`crates/via-cli/tests/s1_prompt_to_result.rs` runs end to end through the real
`via` binary and daemon with the fake agent. It spawns in the foreground,
checks that `result` returns the same envelope, reads `events`, and lists
every C1 §3.2 / §4.1 / §5 / §6.1 shape violation. I wrote it and ran it
against the unchanged code first. It failed for the reasons below; this is
an excerpt of the failure output, with session ids shortened:

```text
receipt.route missing / receipt.adapter_version missing / receipt.version_status missing
capabilities.verbs.spawn is not a C1 §4.1 support entry: null   (same for every verb, param, recover)
receipt.effective incomplete: null
envelope.model = "fake"
envelope.effort/vendor_version/vendor_session_id/cwd/bound/structured_output/steps/cancel/vendor_options/vendor missing
envelope.denied_actions/auto_declined_requests/warnings is not a list
envelope.timestamps.{queued_at,submitted_at,accepted_at,ended_at} missing; duration_ms missing
envelope.usage not explicit unavailable: null; envelope.cost = null; envelope.events = null
raw span shape: {"connection_id":"c_…","len":118,"offset":200}
event types ["turn.queued", "turn.submitted", "turn.accepted", "turn.terminal"]
event 1 lacks C1 common fields: {"address":"s_…/1","seq":1,"session_id":"s_…","turn":1,"type":"turn.queued"}
event 2 lacks C1 common fields: {"seq":2,"turn":"s_…/1","type":"turn.submitted"}
event 3 lacks C1 common fields: {"raw_ref":{…},"seq":3,"turn":"s_…/1","type":"turn.accepted"}
event 4 lacks C1 common fields: {"seq":4,"session_id":"s_…","state":"completed","turn":1,"type":"turn.terminal"}
event raw_ref outside envelope raw_spans
```

After the fix it passes.

## T1-I5 (Core/Store side): canonical events

**How it failed.** The Store created the `turn.submitted` and `turn.accepted`
event documents itself. Core created `turn.queued` and an invented
`turn.terminal`. The `turn` field was sometimes an address string and
sometimes a number. `session_id`, `late` and `at` were missing, and
`raw_ref` was missing or not written into the document (the terminal event's
span was kept only in SQLite columns). C2's `turn.accepted` reached C1
readers.

**Fix.**
- Core now builds every event as a typed `Event` (`crates/via-core/src/api.rs`).
  Each event carries the common fields `seq`, `session_id`, `turn` (number),
  `late`, `at` (RFC 3339 UTC, ms) and `raw_ref`, and an `EventBody` tagged
  enum with the C1 names `turn.queued{queue_position}`,
  `turn.submitted{attempt}`, `turn.started{effective}` and
  `turn.ended{state,failure,stop_reason}`. Core assigns `seq` densely
  (C1 §6.1).
- C2 acceptance evidence is still durable: the vendor correlation and
  `accepted_at` go into `turns`, and the raw span goes into the event row.
  The public event for the same transaction is `turn.started`.
- Store (`crates/via-store/src/runtime.rs`, `runtime/sql.rs`) no longer
  creates events. `commit_submission` and `commit_acceptance` now take
  `SubmissionRecord` and `AcceptanceRecord`, each carrying Core's event. One
  `insert_event` helper handles submission, acceptance and terminal events.
  It requires `seq == next_seq`, and requires any `raw_ref` in the document to
  equal the span stored in the columns, so `logs` reads exactly the cited
  bytes. `submitted_at` and `accepted_at` come from the event's `at`, which
  gives one clock. The schema is unchanged.
- New Store test `events_keep_dense_seq_and_cited_raw_span` (in
  `crates/via-store/tests/persistence.rs`) covers the new constraints. These
  constraints are new behaviour, so the test was written alongside the fix
  rather than failure-first.

## T1-I6: receipt and envelope shapes

**How it failed.** See the regression output above. The receipt had no route
plan, capabilities, effective values or warnings. It did carry the non-C1
extras `api_version`, `turn_number`, `address` and `revision`. The envelope
had `model` as a bare string, `raw_spans` as raw refs, a failure `kind`
instead of `class` (`transport_lost` and `vendor_failed` are not C1 classes),
the raw vendor word as `stop_reason`, and no timestamps, usage, cost, event
range or several required keys.

**Fix** (typed `Serialize` DTOs in `api.rs`; `engine.rs` builds them):
- Receipt (§3.2): `session_id`, `turn` (address), `state`, `route`,
  `adapter_version`, `vendor_version`, `version_status`, `capabilities`,
  `effective`, `warnings`. The old extras are gone (contracts win). The CLI
  reads only `turn` and `session_id`.
- Capabilities (§4.1) describe what this build actually does. Only `spawn`
  is `native`. `resume`, `steer`, `cancel`, `close`, every param and
  `recover` are `unsupported` with a reason. `bounds: []`,
  `network_control: false`, and usage/cost are `unavailable`.
- Effective: `model`; `effort`, `bound` and `max_steps` are `null`;
  `deadlines.wall_ms` is 30000, the deadline the engine actually applies (now
  the same constant), and `idle_ms` is `null` because no idle deadline is
  enforced.
- Envelope (§5): `model` and `effort` are `{requested,resolved}`. `failure` is
  `{class,message,vendor_code?,retryable}`, using the classes `submit_failed`,
  `vendor_error`, `process_exited` and `protocol`. `stop_reason` maps to C1's
  closed set, with the raw word kept in `vendor_stop_reason`. `usage` has
  every count `null` with `provenance: unavailable`, and `cost` is
  `{usd:null,scope:turn,provenance:unavailable}`. `timestamps` are
  `queued_at` (read from the durable `turn.queued`) plus `submitted_at`,
  `accepted_at` and `ended_at`, all equal to the matching event `at`.
  `duration_ms` runs from submitted to ended. `events` is
  `{first_seq,last_seq,count}`. `raw_spans` are
  `{connection_id,path:"raw/<c>.raw",first_offset,last_offset}` per
  connection, bounding the turn's event raw refs, with `last_offset`
  exclusive. `cancel`, `cwd`, `vendor_session_id`, `bound` and
  `structured_output` are explicit `null`/empty, and `vendor.turn_id` is the
  accepted vendor turn id.
- `version_status` is `untested`, with a `vendor_version_untested` warning,
  because the fake agent reports no version.

## Existing tests updated

- `s1_f30_wait_disconnect_result_survives` polled for `turn.accepted`; it now
  polls for `turn.started`.
- `crates/via-store/tests/persistence.rs` and `raw_bounds.rs` use the new
  `SubmissionRecord` and `AcceptanceRecord` API (`submission()` and
  `acceptance()` helpers). Their assertions are unchanged.
- `crates/via-host/tests/anchor_process.rs` uses `commit_spawn` only and
  needed no change.

## Files changed

`crates/via-core/src/{api.rs,engine.rs}`, `crates/via-store/src/{lib.rs,runtime.rs,runtime/sql.rs}`,
`crates/via-store/tests/{persistence.rs,raw_bounds.rs}`, `crates/via-cli/tests/s1_prompt_to_result.rs`
(new regression plus the f30 rename). I added the response DTOs at the end of
`api.rs` as `pub(crate)` items and did not touch W1-B's request types or the
`lib.rs` re-exports.

## Gate (checked, toolchain 1.98.1, `XDG_RUNTIME_DIR` private 0700)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | pass |
| `cargo nextest run --locked --workspace` | 55/55 pass (the base had 53 tests; I added 1 e2e, 1 Store and 1 formatter unit test) |
| `cargo deny check` | advisories, bans, licenses and sources ok |
| `python3 scripts/check-layers.py` | exit 0 |

The `test-failpoints` gates do not apply: `via-cli` has no such feature yet.

## Open or uncertain

- **Capability ownership.** C2 makes capabilities an adapter declaration
  (`RoutePlan`), but via-adapters has no capabilities API yet, so Core states
  the fake route plan (`Capabilities::fake`, `RoutePlan::fake`).
  `adapter_version` is Core's `CARGO_PKG_VERSION`, which is valid only
  because all crates share the workspace version. Move both to the adapter
  when C2 `describe` lands.
- **Spec interpretations (inferred).** `raw_spans.last_offset` is exclusive,
  `duration_ms` runs from submitted to ended, `usage`/`cost` capability words
  are `"unavailable"`, and `cwd` is `null` because spawn has no cwd yet.
- **One turn per session.** `turn.queued` is assumed at seq 1 and the event
  range starts at 1. Multi-turn sessions (resume) must track per-turn ranges.
- **Not emitted: `session.opened`.** The fake route confirms no vendor
  identity. Adapter observations (`assistant.text`, tool, unknown) are
  W1-A's route-side part and are not committed as events by Core yet; Core
  still consumes only acceptance and terminal.
- **Unchanged success rule.** A completed vendor turn with uncertain cleanup
  is still `failed` (now class `process_exited`). This matches the previous
  success rule; whether it should instead be `completed` with a cleanup
  warning belongs to W1-D's cleanup and shutdown work.
