# W1-A report: T1-I1 and T1-I5 (route side)

Branch `claude/w1-a-rust-foundation-fzkahc`, base `639e77f`. Fix commit
`19f8d47`.

## T1-I1: stream drain after terminal

**Failure mode on the frozen tree.** Some earlier fixes were already present.
Wire's `next_frame` reads both pipes concurrently and returns `None` only when
both reach EOF. Stderr goes straight to the raw log in 8 KiB units. The route
already half-closed input after the terminal. What was still broken:

1. After the terminal, the route read **one** more frame and rejected it
   whatever it was (`fake observation after terminal`). A late `tool_ended`,
   which C2 §7 item 6 allows ("Tool completion … may arrive afterward"),
   failed the turn.
2. Because the route stopped at that first frame, a second terminal behind a
   late observation was never read. The turn failed for the wrong reason, and
   the duplicate terminal was never raw-logged.
3. On any route failure, `execute` force-closed and returned without reading
   the pipes again. Bytes buffered in Wire, and any tails still in the pipes,
   never reached the raw log.

**Regressions** (end-to-end through the real `via` daemon and the fake agent,
in `crates/via-cli/tests/route_drain.rs`):

- `route_drain_records_late_tool_end_and_stderr_tail_after_terminal`. Before
  the fix it failed: `state` was `"failed"`, message `fake protocol error …:
  fake observation after terminal`.
- `route_drain_rejects_duplicate_terminal_after_late_observation`. Before the
  fix it failed with the wrong cause (same message, instead of `duplicate fake
  terminal`). After the fix it also asserts that both terminals, the late
  observation and a stdout tail behind the duplicate are in the raw log. A
  mutation check confirmed the tail assertion catches loss: with the
  failure-path drain removed, it fails on `stdout-tail-after-duplicate`.
- `route_drain_survives_stderr_flood_after_terminal` (4 MiB of stderr after
  the terminal, then exit). This **passed before the fix**, so stderr-flood
  handling and the Wire EOF issue were already fixed in the frozen tree. It
  stays as a guard, with an exact byte count for stderr in the raw log.

**Fix.**

- Route (`crates/via-routes/src/runtime.rs`):
  1. Adds a connection-local `Phase` (`Submitted` → `Accepted` → `Terminated`)
     that checks every decoded frame.
  2. After the terminal, the route half-closes input, then loops `next_frame`
     until both pipes reach EOF, all under the turn's absolute deadline.
  3. A late text, tool or unknown frame is admitted. A second terminal, a
     second acceptance or an unsolicited interrupt ack fails protocol.
  4. On any failure, `execute` force-closes and then calls
     `WireConnection::drain_to_eof(deadline)`.
- Wire (`crates/via-wire/src/runtime.rs`): factors the concurrent pipe read
  into `read_either`, which reads one 8 KiB chunk per call and sends stderr
  straight to the raw log. It adds `drain_to_eof`, which:
  1. drops stdin;
  2. stores any buffered partial stdout in 8 KiB raw units (an oversized
     partial frame cannot exceed the raw unit cap);
  3. records both streams unframed until EOF or the deadline.

  Memory stays bounded at one read chunk plus the existing frame cap.

## T1-I5, route side: observations

**Failure mode.** In `drive`, the arm `Text | ToolStarted | ToolEnded |
UnknownNotification if accepted => {}` discarded these messages. Only
acceptance (on a capacity-1 side channel) and the terminal (in the result)
left the route.

**Fix.** `FakeRoute::execute` now takes `mpsc::Sender<RouteMessage>`. Every
decoded message goes on it in decode order with its synced `raw_ref`:
acceptance, text, tool start and end, unknown (kept as
`UnknownNotification {vendor_type, raw_payload, truncated}`), the terminal,
and late observations. The route waits for capacity, bounded by the turn
deadline. A dropped or never-draining consumer becomes `RouteError::Overflow`,
never a silent drop. `RouteAcceptance` is removed.

**Regression.** Unit tests in `runtime.rs`:
`observations_are_admitted_after_acceptance_and_after_terminal` and
`order_violations_fail_the_turn`. They are isolated because a route-level
integration test needs a Store, and `check-layers.py` forbids a via-store
dev-dependency in via-routes. The pre-fix tree had no forwarding path to
test, so there is no failing run for T1-I5; the evidence is the discarding
arm quoted above. The end-to-end suites exercise the new stream, because
acceptance now reaches Core only through it.

## Files changed

- `crates/via-wire/src/runtime.rs`
- `crates/via-routes/src/runtime.rs`, `src/lib.rs`, `Cargo.toml` (tokio
  `sync` feature, previously supplied only by feature unification)
- `crates/via-cli/tests/route_drain.rs` (new)
- **Outside owned paths:** `crates/via-adapters/src/runtime.rs`. This is the
  smallest change that keeps the adapter compiling against the new stream. It
  drains the stream continuously (capacity 64), forwards acceptance to Core
  as before, and ignores everything else.

## Gate (final tree)

- `cargo fmt --all --check`: pass.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: pass.
- `cargo nextest run --locked --workspace`: 57 passed, 0 skipped.
- `cargo deny check`: advisories, bans, licenses and sources ok. There are
  duplicate-crate warnings only.
- `python3 scripts/check-layers.py`: pass.
- `route_drain` was repeated 3 more times: 3/3 passed each run. No fake or
  daemon processes remained afterwards.

## Open or uncertain

- **Observations stop at the adapter.** Core (W1-C's `engine.rs`) has no
  observation channel yet, so the adapter drops text, tool and unknown
  messages after Route has raw-logged them. Wiring them to C1 events needs an
  Adapter → Core observation path. The adapter has no owner in W1.
- **Interpretation of late frames.** I followed C2 §7 item 6: after the
  terminal, text, tool and unknown frames are admitted as late observations,
  and only a repeated terminal, repeated acceptance or unsolicited interrupt
  ack fails. The previous code rejected every post-terminal frame. If the
  owner wants the stricter fake rule, change one match arm in `Phase::advance`.
- **Backpressure before the terminal (C2 A1).** Wire is pull-based. While
  Route waits on a full observation channel, neither pipe is read. Today the
  adapter drains continuously, so this does not happen. A stalled consumer
  would still stop pipe reads, contrary to coding-style §5 ("pipe reads never
  wait for consumers"). Fixing it needs per-pipe reader tasks in Wire, which is
  outside this brief.
- **Failure-path drain.**
  - A drain that hits the deadline or a raw-store error is ignored in favour
    of the original failure. The raw log may then be incomplete without a
    `raw_incomplete` marker, because `RouteError` has no field for it.
  - A process outside the group that holds a pipe open keeps the drain
    running until the turn deadline.
- **Cleanup deadline.** The graceful close after exit still uses a fresh 3 s
  cleanup deadline, unchanged from before. It is a separate C2 deadline, not
  the turn deadline.
- **Existing test coverage.** I added no new fake-agent behaviour. The case
  where stdout ends early while stderr continues has no dedicated test; the
  fake cannot close stdout alone. That path is `next_frame`'s both-EOF
  condition.
