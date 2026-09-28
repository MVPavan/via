# T2-E report: frozen per-turn parameters and Task 2 acceptance fixes

Branch `claude/t2-e-rust-foundation-u1wszb`, started from
`origin/rust-foundation` at `374b490`. This task clears the three blockers
in `../task2-sol-high-review.md`. No decision in `../e.md` contradicted a
contract. Two contract readings are recorded under "Contract readings"
below.

## 1. Frozen per-turn parameters (blocker 1)

**Failure mode.** `spawn` and `resume` accepted only the prompt and the key.
Any C1 §4 per-turn member was refused as `unknown_field`. Every receipt
built the same `Effective::fake` values, `turn.started` repeated them, and
every turn ran under the fixed `FAKE_WALL_MS`. Nothing was frozen, inherited
or stored per turn.

**Regressions.** All run against the real daemon and SQLite, in
`crates/via-cli/tests/s1_sessions.rs`:

- `s1_params_queued_turns_inherit_frozen_per_turn_values`. Spawn with
  `wall_ms` A = 20 000. While turn 1 is held, turn 2 is resumed with the CLI
  flag `--wall-ms` B = 25 000 (queued), and turn 3 is resumed raw with no
  parameters (queued). The receipts show A, B, B. The `turns.effective`
  rows show the same values while turns 2 and 3 are still queued, and again
  after they run. Each turn's `turn.started.effective` matches, and each
  envelope's `effort`/`bound` is the frozen null.
- `s1_params_frozen_wall_deadline_applies_to_its_turn_only`. Turn 1 hangs
  under `wall_ms` 1500. It ends `failed`/`deadline_wall` with
  `duration_ms` < 10 s, well under the 30 s default. Turn 2 sets 60 000,
  holds at a gate for 2 s (longer than turn 1's whole budget) and completes.
- `s1_params_unsupported_values_are_refused_by_name`. Covers spawn and
  resume refusals for `effort`, `output_schema`, `max_steps`
  (`invalid_params`), `bound` (`bound_unsupported`), nonempty `vendor` and
  `deadlines.idle_ms` (`invalid_params`). Each refusal has `data.field`,
  `data.route: "fake"`, and a message that names the field. Resume with
  `harness`, `model`, `allow_untested`, `instructions`, `cwd`, `require` or
  `label` is refused as `invalid_params` with
  `kind2: session_scope_on_resume` and `data.field`. No refusal commits a
  turn. Null or empty values (`effort`, `output_schema`, `max_steps`,
  `vendor: {}`, `deadlines: {wall_ms: null, idle_ms: null}`) are accepted on
  both spawn and resume.
- `s1_params_keyed_retry_replays_identical_effective`. A keyed spawn retry
  and an `op_key` resume retry (made before and after the turn ended) return
  the identical receipt, `effective` included. A changed `wall_ms`, or an
  omitted `deadlines`, under the same key is `idempotency_conflict`. An
  unkeyed turn 3 inherits the keyed turn 2's value.
- Isolated tests:
  `via-store::persistence::frozen_turn_values_are_stored_and_the_latest_turn_supplies_inheritance`
  checks that a queued turn cancelled afterwards still supplies the latest
  frozen values. `via-core api::tests::fake_per_turn_edge_values` covers
  `vendor` with empty per-harness objects, `bound: null` and `wall_ms: 0`.

**Before the fix** (`374b490`), the four daemon tests fail because the
params are refused:

```
s1_params_queued_turns_inherit_…    fail: spawn: expected a result: {"error":{"code":-32602,"data":{"kind":"invalid_params","kind2":"unknown_field"},…}}
s1_params_frozen_wall_deadline_…    fail: spawn: expected a result: {… "kind2":"unknown_field" …}
s1_params_keyed_retry_…             fail: spawn: expected a result: {… "kind2":"unknown_field" …}
s1_params_unsupported_values_…      fail: spawn effort: expected invalid_params/None naming effort: {… "kind2":"unknown_field" …}
Summary: 9 tests run: 5 passed, 4 failed
```

The two isolated tests reference the new `effective` fields, so on the
base they fail to compile rather than fail at run time.

**Fix.**

- `crates/via-core/src/api.rs`: `SpawnParams` and `ResumeParams` accept
  `effort`, `bound`, `output_schema`, `deadlines {wall_ms?, idle_ms?}`,
  `max_steps` and `vendor`. A present `null` is kept distinct from an
  omitted member. `ResumeParams` accepts the session-scope members only in
  order to refuse them. `PerTurn::fake_overrides` validates against the
  fake route's `Capabilities::fake`. `ApiError` gains
  `named: Option<&'static Named>` (field and optional route), which is
  written to `data.field` and `data.route`. There is a new
  `BOUND_UNSUPPORTED` (-32008). `wall_ms: 0` is `invalid_params`.
  `Effective` is now `Deserialize` and has `inherit` and `wall`.
- `crates/via-core/src/engine.rs`: spawn freezes turn 1's values, from the
  given ones or the defaults. Resume first refuses session-scope members and
  validates, then, under `admission`, inherits omitted values from
  `SessionSnapshot::latest_effective`. That is the latest accepted turn,
  whatever its state. Both receipts and Store records carry the same
  `effective`.
- `crates/via-core/src/engine/drive.rs`: submission reads the turn's frozen
  row. An unreadable row is a Store failure, and nothing is sent. The Core
  wall deadline and `turn.started.effective` come from that row.
 
- `crates/via-store/src/runtime.rs` and `runtime/sql.rs`: schema v4 adds
  `turns.effective TEXT NOT NULL`, which is written once in the receipt
  transaction. `SpawnRecord` and `ResumeRecord` carry `effective`.
  `SessionSnapshot.latest_effective` and `QueuedTurn.effective` read it.
- `crates/via-cli/src/main.rs`: `spawn` and `resume` gain the C1 per-turn
  flags `--bound B [--allow-dir D]… [--network]`, `--effort E`,
  `--output-schema F` (a JSON file, `null` clears), `--wall-ms N`,
  `--idle-ms N`, `--max-steps N` and repeatable `--vendor h.k=v`. The daemon
  does all validation.
- `docs/specs/runtime-contracts.md` §6: the schema text is rewritten as the
  exact v4 tables and columns. Target-only tables and columns are listed as
  not implemented, each with its owner (`via-jm4.7.7`, `via-jm4.7.8`, the
  first vendor slice, or "none in S1" for retention metadata). The text
  also states the v0 rule from §3 below.

**Default wall deadline.** The fake route keeps 30 000 ms (`FAKE_WALL_MS`)
when a turn neither sets nor inherits a value. C1 §4's default of
3 600 000 ms is not practical for tests: existing scenarios such as
`route_drain::failure_class_deadline_wall_after_hang`, and any hung fake turn
in a test that sets no deadline, would then run for an hour instead of 30 s.
The receipt reports the effective 30 000.

**Inheritance timing.** The latest turn's values are read under `admission`,
which every receipt commit holds. Store's check inside the receipt
transaction that the new turn is the session's next confirms that the turn
read is still the latest. The read is not itself a SQL statement inside that
transaction. With admission serializing all receipts, the result is the
same.

## 2. One launch per intended turn (blocker 2)

`check_history` in `s1_sessions.rs` now takes the sandbox. For each session
it requires exactly one committed Host anchor per intended turn and no
others: `anchors.owner_session` count equals the number of turns, and each
`owner_turn` count equals 1. This applies in F13, F14, F17 and F28, and in
the new tests. F28 now requires each session's own `assistant.text` for each
of its turns (`a1 reply`, `a2 reply`, `b1 reply`, `b2 reply`), attributed to
the right turn. It still also checks that the other session's text is
absent.

These assertions pass on the base. No duplicate launch exists there, so
these are stronger checks, not bug reproductions. Sensitivity was checked by
a temporary mutation that inserted a second anchor row for F14's turn 2. It
failed with `fail: s_…: 5 agent launches for 4 turns`, and the mutation was
reverted.

## 3. Existing version-0 Store (blocker 3)

**Failure mode.** `check_schema_version` exempted 0. An existing file at
version 0 passed the read-only check, was reopened writable, had its journal
mode switched to WAL, and received the full schema.

**Regression.**
`via-store::persistence::an_existing_version_zero_store_is_refused_without_mutation`
covers two existing files at version 0: an empty file, and a SQLite file
holding a foreign table. It requires a refusal naming `schema v0` and
`recreate`, unchanged bytes, and no `-wal` file. Before the fix, both
opened, and each was turned into a VIA Store:

```
panicked at crates/via-store/tests/persistence.rs:514:13:
an existing version-0 Store opened (foreign tables: false)
(and with the case order swapped: … (foreign tables: true))
```

**Fix** (`crates/via-store/src/runtime.rs` and `runtime/sql.rs`).
`Store::open` records whether the file existed. It creates a new file only
with `create_new` (O_CREAT|O_EXCL, mode 0600, never through a symlink), then
opens it writable without `SQLITE_OPEN_CREATE`.
`check_schema_version(version, created)` refuses any version below 4 unless
it is 0 in a file this open created, using the same named
"recreate the dev Store" error. `configure` repeats that check before its
first mutation, the journal-mode switch. The v1/v2 refusal test now covers
v1 to v3.

**Limitation.** A crash after the exclusive create but before the schema
commit leaves an empty file. The next open refuses it with the recreate
instruction instead of initializing it. This is deliberate under the
"only when VIA creates" rule. Revisit it if unattended recovery from that
window is needed.

## Contract readings

- C1 §5's envelope has no `deadlines` field, and adding one would contradict
  the contract. `wall_ms` therefore appears in the receipt, the Store row and
  `turn.started.effective`. The envelope's `model`, `effort` and `bound` are
  the frozen values, which are null on the fake route.
- `output_schema` is not in C1's `effective` example. On the fake route it is
  always null, and `null` clears, so no column or field was added for it.
  A route that supports it will need both.

## Shared hunks

The shared files are `via-core/src/engine/drive.rs` and Store's
spawn/submission commit path. The changed hunks are:

- `drive.rs`: `Submission.effective`, the frozen-row read in
  `commit_submission`, `wall_deadline`, and `effective` threaded through
  `execute`, `observe` and `accept`.
- `runtime/sql.rs`: the `turns.effective` column in `commit_spawn` and
  `commit_resume`, plus `read_snapshot`/`read_queued_turn`.

No failpoint call site moved. Test fixtures that build Store records
directly gained the new `effective` field: `via-store` tests,
`via-host/tests/anchor_process.rs`, `via-core/tests/route_stream.rs` and
`engine/journal/tests.rs`.

## Gate (cloud session, Rust 1.98.1, cargo-nextest 0.9.146, cargo-deny 0.20.2)

| Command | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | pass |
| `cargo nextest run --locked --workspace` | 180 passed, 2 skipped |
| `cargo deny check` | pass |
| `python3 scripts/check-layers.py` | pass |
| `cargo clippy … --features via-cli/test-failpoints -- -D warnings` | pass |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints`, 5 runs | 210 passed, 2 skipped each time (43.9 s, 43.7 s, 43.6 s, 42.9 s, 43.7 s) |
| `… -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 17 passed |
| `… -E 'test(/^s1_(f2[4567]\|raw\|bounds\|store)_/)'` | exit 4, "no tests to run" (Task 4 set, expected, not a pass) |
| `cargo build --locked --release -p via-cli --no-default-features` + `check-release-features.py` | pass: no `test-failpoints`, 11 points ignored, 0 of 15 markers |
| Ignored peer-UID check (run as root) | 1 passed |
| Skill catalog check / Markdown links | 0 FAIL / 0 broken |

## Open

- The deferred items stay on `via-jm4.7.7` as the brief says:
  deterministic failpoint barriers for the fixed-sleep capacity checks and
  the ignored force-handoff race, and the Core idle deadline. Once the idle
  deadline exists, `deadlines.idle_ms` should be accepted.
- `s1_params_frozen_wall_deadline_applies_to_its_turn_only` holds turn 2 for
  a fixed 2 s. That is a lower bound, not a race: the test passes only if
  turn 2's own 60 s deadline applies. It adds about 2 s to the suite.
- `bound: null` is refused as `bound_unsupported` ("any bound"). If the
  orchestrator reads null as "omitted", that is a one-line change in
  `fake_overrides`.
