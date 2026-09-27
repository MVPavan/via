# T2-E: frozen per-turn parameters and Task 2 acceptance fixes

Model: Opus 5.5 high, continuing the T2-B2…T2-D cloud session on its
branch (`claude/t2-b2-step-1-3pslux`; merge `origin/rust-foundation`
first). Follow `../w1/common.md`; report to `reports/T2-E.md`.

The Sol high slice review of Task 2 (`task2-sol-high-review.md`) returned
ACCEPT AFTER CHANGES with three blockers. This task clears them. The
decisions below are the orchestrator's. If one contradicts a contract, stop
and say so in the report.

## 1. Frozen per-turn parameters (C1 §3.1–§3.3, §4, P5)

Only S1's fake route is in scope; its capabilities are `Capabilities::fake`
in `crates/via-core/src/api.rs`.

- **Accepted parameters.** `spawn` (for turn 1) and `resume` accept C1 §4's
  per-turn parameters: `effort`, `bound`, `output_schema`, `deadlines`
  `{wall_ms?, idle_ms?}`, `max_steps` and `vendor`. The CLI gains the
  matching per-turn flags where C1's CLI lines name them.
- **Validation against the fake route.** These use the canonical errors
  and name the field and the route:
  - `deadlines.wall_ms` is supported.
  - A non-null `effort`, `output_schema` or `max_steps` is refused with
    `invalid_params`. Null or omitted values follow inheritance, and
    `output_schema: null` clears.
  - Any `bound` is `bound_unsupported`, because the fake declares no
    bounds.
  - Nonempty `vendor` options are `invalid_params`.
  - A non-null `deadlines.idle_ms` is `invalid_params`, because Core
    enforces no idle deadline yet. Implementing it and then accepting it
    goes to `via-jm4.7.7`.
  - Session-scope parameters on `resume` are `invalid_params` with
    `kind2: session_scope_on_resume`, and so is `allow_untested`.
- **Inheritance (P5).** An omitted per-turn parameter inherits from the
  latest accepted (receipted) turn of the session, whatever that turn's
  later state. It is resolved under `admission` inside the receipt
  transaction, so the value is consistent with the queue. Cancelling a
  queued turn does not change its successors' frozen values.
- **Frozen and durable.** Each turn's effective values are stored in its
  Store row at receipt commit and returned in the receipt's `effective`. A
  keyed replay returns the identical `effective`. A keyed retry whose
  per-turn parameters differ from the stored request is
  `idempotency_conflict`.
- **Driven from the frozen values.** The dispatcher and the Core deadline
  use the turn's frozen `wall_ms`, not `FAKE_WALL_MS`. That constant stays
  only as the default when nothing is given or inherited: C1's default is
  3 600 000 ms, but the fake route may keep its current test default if
  §4's default is not practical for tests; state which default applies
  and why.
- **Schema.** Bump to v4 under runtime §6's pre-release rule. Correct
  runtime §6's schema description so that it matches the implemented
  tables and columns exactly. Mark any target-only table or column as not
  implemented and name its owning task.

Tests (real daemon and SQLite):

- **Queued inheritance.** Spawn with `wall_ms` A. While turn 1 runs,
  resume turn 2 with `wall_ms` B (queued), then resume turn 3 with no
  parameters (queued). The receipts show A, B and B, and Store rows and
  envelopes show the frozen values.
- **Frozen deadline enforced.** A short `wall_ms` on one turn gives
  `deadline_wall` for that turn only.
- **Refusals.** Each refusal above has its canonical kind and names the
  field.
- **Keyed retry.** A keyed retry returns the identical `effective`, and
  differing parameters give `idempotency_conflict`.

## 2. One launch per intended turn (F13, F14, F17, F28)

The shared history assertion in `crates/via-cli/tests/s1_sessions.rs`
counts `turn.submitted`. Also assert exactly one anchor (fake-agent start)
per intended turn in all four scenarios. F28 must require each session's
own expected `assistant.text` events to be present, not only the absence
of the other session's text.

## 3. Existing version-0 Store file

`user_version = 0` is initialized only when VIA creates a new database. An
existing file at version 0 is refused before any writable open, with the
same named "recreate the dev Store" error. Add a regression checking that
the file's bytes are unchanged.

## Deferred (orchestrator records on `via-jm4.7.7`)

Replace the fixed-sleep capacity checks in `s1_crash_points.rs` and the
ignored force-handoff race in `s1_daemon_stop.rs` with deterministic
failpoint barriers. Core idle deadline.

## Gate

Run the gate from `.repo-context/verification.md`, with 5 full
`--features via-cli/test-failpoints` runs and their counts. Each new test
must fail on the current `rust-foundation` for its stated reason.
