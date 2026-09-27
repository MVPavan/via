# W2-E report: turn data path

Branch `claude/w2-e-rust-foundation-1buttv`, based on `56e3f55`. Brief:
[`../e.md`](../e.md). All three items are implemented and regression-tested.
The full gate passes.

## 1. Observations reach Core (T1-I5)

**Failure mode.** Route forwarded text, tool and unknown observations. Adapter
then dropped everything except acceptance, so Core committed only
`turn.queued/submitted/started/ended`. Nothing enforced C2's 256 KiB payload
bound: a 300 KiB `tool_started` completed normally. No test would notice if
Route stopped forwarding, because the Route tests only exercised
`Phase::advance`.

**Regressions.**
- End to end through `via`, in `crates/via-cli/tests/route_drain.rs`:
  - `observations_become_ordered_events_with_exact_raw_refs` checks the
    event types and order, dense `seq`, common fields and payloads. Text of
    280 KB is split into pieces that each encode to at most 256 KiB and
    concatenate back to the original. It also checks the 16 KiB
    unknown-payload prefix with `truncated: true`, and that each event's
    `raw_ref` resolves to exactly the emitted stdout frame. Late
    post-terminal observations are included, and `raw_spans` covers every
    reference.
  - `failure_class_protocol_for_oversized_tool_payload`.
- Route stream: `route_forwards_every_observation_in_order_with_its_raw_ref`
  in `crates/via-core/tests/route_stream.rs`. It runs the real
  Adapter→Route→Wire→Host path with a scripted `/bin/sh` vendor and asserts
  payloads, order and exact raw bytes per reference.
  - **Location.** It lives in via-core because `scripts/check-layers.py`
    allows via-routes only via-wire, so it cannot open a Store. via-core may
    name both via-adapters and via-store.
  - **Anchor.** The anchor is the real `via` binary beside the test build.
    The test binary cannot serve: libtest prints `running 1 test` onto the
    stdout the vendor inherits. This follows the existing precedent of
    `via-fake-agent`: a `-p via-core`-only run needs `via` built first.

**Pre-fix output:**

```
observations_become_ordered_events_with_exact_raw_refs
  left:  ["turn.queued", "turn.submitted", "turn.started", "turn.ended"]
  right: ["turn.queued", …, "assistant.text" ×3, "tool.started", "tool.ended", "vendor.other" ×2, "turn.ended"]
failure_class_protocol_for_oversized_tool_payload
  left: String("completed")  right: "failed"
```

The Route-stream test needs the new observation API, so it cannot compile
on the old code. Instead, I made `forward` stop sending non-acceptance
messages after the fix. Both tests then failed: `left: 1, right: 6`
observations, and the 4-vs-11 event list above.

**Fix.**
- Adapter normalizes every Route message into
  `FakeObservation::{Accepted, Data{observation, raw_ref}}`. It sends them to
  Core in decode order and waits for capacity until the deadline.
- If Core cannot take an observation, Adapter drops the Route receiver.
  Route then fails the turn as `Overflow` and still runs its cleanup.
- Text is split greedily at UTF-8 boundaries by exact `serde_json` escaped
  length. Every piece cites the same frame.
- Route refuses a `tool_started`/`tool_ended` whose encoded fields exceed
  256 KiB. That is a protocol failure, cited to the frame.
- Core commits each observation through the new `StoreClient::commit_event`
  as `assistant.text`, `tool.started`, `tool.ended` or `vendor.other`,
  between `turn.started` and `turn.ended`, with dense `seq` and `late: false`.
  The envelope's `raw_spans` now bound every committed reference.

## 2. No silent evidence gaps

**Failure modes.**
- (a) `next_frame` drained a complete line out of the buffer before checking
  the 1 MiB cap. A line of exactly 1 MiB plus LF was lost from the raw log.
- (b) `drain_to_eof` returned on the first append error, and Route ignored
  the result. Later bytes were neither drained nor marked, and nothing said
  the evidence was incomplete.

**Regressions.**
- (a) `oversized_stdout_line_is_retained_as_raw_evidence`, end to end. It
  asserts that the full 1 MiB line and the tail after it are in the raw log,
  with no `raw_log.incomplete` event or warning, since every byte was kept.
  Pre-fix it failed at `route_drain.rs:456` with "oversized line lost from
  raw log".
- (b) `failing_raw_append_keeps_draining_and_reports_incomplete_evidence`,
  in `route_stream.rs`, isolated. The test drops the Store owner after
  acceptance, so later raw appends fail, then releases the vendor to write
  more stdout and stderr. It asserts cause `Store` and
  `raw_incomplete: true`.
  - Pre-fix, Route mapped the cause to `TransportLost` and had no flag.
  - With the new API and the old drain behaviour reinstated, it failed with
    `RouteFailure { cause: Store, …, raw_incomplete: false }`.
  - The fake vendor cannot fail a raw append through `via`, and S1 has no
    failpoint feature, so an end-to-end case would need one.

**Fix.**
- Wire keeps an oversized line buffered until the failure drain records it
  in 64 KiB units.
- Every raw append goes through `record`, which latches
  `RawEvidence::Incomplete` on failure.
- `drain_to_eof` now returns `RawEvidence` and never aborts early. After a
  failed append it reads and discards until EOF. A read error or the cleanup
  deadline also marks the log incomplete.
- Route carries this as `RouteFailure.raw_incomplete`. Core commits
  `raw_log.incomplete {connection_id}` (C1 §6.1) before `turn.ended` and adds
  the `raw_log_incomplete` warning (C1 §5).
- Route's failure cleanup (force close and drain) now has its own 3 s
  bound. Previously it reused the turn deadline, which is already spent
  after a deadline failure.

## 3. Failure classes

**Failure mode.** Every `AdapterError` became `failure.class = "protocol"`.
Route mapped every Wire error, including Host-confirmed exit and deadline,
to `TransportLost`.

**Regressions.** All are end to end, in `route_drain.rs`.

| Test | Asserts | Pre-fix |
|---|---|---|
| `failure_class_protocol_cites_the_malformed_frame` | `protocol`; `turn.ended.raw_ref` is the malformed frame | `raw_ref` null (panic at `:184`) |
| `failure_class_process_exited_before_acceptance` | `process_exited`, `exit.code` 3, no `turn.started` | `protocol` |
| `failure_class_process_exited_after_acceptance` | `process_exited`, `exit.code` 4 | `protocol` |
| `failure_class_deadline_wall_after_hang` | `deadline_wall`, `stop_reason: deadline` (~31 s) | `protocol`, "transport lost" |
| `failure_class_process_exited_after_completed_terminal` | exit 5 after a completed terminal | passed (guard) |
| `failure_class_vendor_error_keeps_vendor_code` | `vendor_error`, `vendor_code` | passed (guard) |

Every case also checks dense events and that `turn.ended` matches the
envelope. Overflow, Store and transport loss cannot be produced by the fake
agent. The unit test `engine::tests::route_causes_keep_their_c1_disposition`
covers them.

**Fix.**
- Route's `wire_cause` keeps the typed cause:

  | Wire error | Route cause |
  |---|---|
  | `Raw`, `RawStore`, `RawRangeMismatch` | `Store` |
  | `Deadline` | `Deadline`, a new variant |
  | `FrameTooLarge`, `UnterminatedFrame` | `Protocol`, with a named detail |
  | `Overflow` | `Overflow` |
  | I/O, Host, `Transport` | `TransportLost` |

- `RouteFailure {cause, evidence, exit, raw_incomplete}` crosses to Core
  inside `AdapterError::Route`. It includes the Host-confirmed exit, which
  now fills the envelope's `exit`.
- Core maps causes through a `FailureClass` enum, per coding-style §4:

  | Cause | C1 result |
  |---|---|
  | `Protocol` | `protocol` |
  | `ProcessExited` | `process_exited` |
  | `Overflow` | `overflow` |
  | `Store` | `store` |
  | `Deadline` | `deadline_wall` / `deadline` |
  | `TransportLost` | `unknown` with no failure (C1 §7.6) |

- A Core Store failure while committing acceptance or observations no longer
  abandons the running turn. Core stops committing, keeps draining the
  adapter and commits `failed(store)`.
- Wire's `wait_exit` reported a closed exit channel as `Deadline`; it is now
  `Transport`.

## Files and shared hunks

- **Wire:** `crates/via-wire/src/{lib,runtime}.rs`.
- **Route:** `crates/via-routes/src/{lib,runtime}.rs`.
- **Adapter:** `crates/via-adapters/src/{lib,runtime}.rs`, plus
  `Cargo.toml` (`serde_json` dev-dependency).
- **Store event records:** `crates/via-store/src/{lib,runtime}.rs`,
  `runtime/sql.rs` (`EventRecord`, `commit_event`).
- **Core (shared with W1-D):**
  - `crates/via-core/src/engine.rs`: `drive` plus new `execute`, `observe`,
    `commit_event`, `TurnRecord`, `classify`/`failed_terminal`/
    `route_disposition`, `terminal_envelope`'s `raw_spans` argument, and
    tests.
  - `crates/via-core/src/api.rs`: `FailureClass`, the new `EventBody`
    variants, `Warning::RAW_LOG_INCOMPLETE`, `RawSpan::include` replacing
    `bounding`, and a test.
  - I did not touch stop, shutdown or `verify_cleanup`.
- **Tests:**
  - New `crates/via-core/tests/route_stream.rs`, plus `Cargo.toml`
    (`tempfile` and tokio `rt`/`macros` dev-dependencies).
  - `crates/via-cli/tests/route_drain.rs`: new cases, and the helper now
    reads pipes while waiting (it deadlocked above 64 KiB of output).
  - `crates/via-cli/tests/s1_prompt_to_result.rs`: the C1 shape test now
    expects the fixture's `assistant.text` event (five events). This file is
    also likely touched by W1-D (T1-I4).
- **`Cargo.lock`:** two dev-dependency lines, no new packages.

## Gate

Run on this branch with `XDG_RUNTIME_DIR` set to a private 0700 directory.

- `cargo fmt --all --check`: pass.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: pass.
- `cargo nextest run --locked --workspace`: 77 passed, 1 skipped. The skip is
  the pre-existing root-only
  `c1_protocol::c1_client_refuses_daemon_socket_of_another_uid`. Wall time is
  about 31 s, set by the deadline case.
- `cargo deny check`: advisories, bans, licenses and sources ok.
- `python3 scripts/check-layers.py`: pass.

## Open or uncertain

1. **Deadline envelope.** `cancel` stays `null`. C1 §7.6 says `cancel` is
   filled on a Core deadline, but S1 has no cancel. The deadline test takes
   about 31 s because `FAKE_WALL_MS` is fixed and there is no `--wall-ms`.
2. **Cleanup deadline.** The fresh 3 s cleanup bound follows the existing
   success-path precedent and the contract's "cleanup deadline". It is a new
   relative timeout, which coding-style §5 otherwise discourages.
3. **`vendor.other` field.** `vendor.other` adds a `truncated` field. C1 §6.1
   lists only `vendor_type` and `payload`, while C2 A1 requires an explicit
   marker. C1 should record the field.
4. **Contract interpretations:**
   - `FrameTooLarge` and an unterminated EOF frame map to `protocol`.
   - `TransportLost` maps to `unknown`.
   - A Store failure overrides other causes.
   - The 256 KiB tool bound is measured on Route's re-encoded fields.
5. **Durable incompleteness.** Incompleteness is not persisted as a Store
   connection state, because the schema has no `connections` table.
   `raw_log.incomplete` has no end-to-end test until a failpoint exists; the
   isolated test and the event-shape unit test cover it.
6. **Store edge cases:**
   - An *uncertain* Store commit can leave `seq` ambiguous, so the terminal
     commit then fails and the turn stays running. This is pre-existing.
   - `commit_event` validates each `raw_ref` by scanning the index, so many
     observations cost O(n²). This is deferred with the per-session budget.
7. **Adapter normalization.** A normalization failure can only come from
   invalid acceptance identity, which Route already rules out. It would be
   reported as `overflow`.
8. **Deferred (`via-jm4.7.8`):** per-pipe reader tasks and the 4 MiB
   per-session observation budget. Observations still wait under the turn
   deadline, not C2's 10 s no-drain timer.
