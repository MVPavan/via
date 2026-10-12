## Verdict: ACCEPT AFTER CHANGES

The final branch has end-to-end daemon/SQLite tests for all six F rows. Recovery, dispatch, and connection-slot accounting fit together in the inspected paths. Acceptance is still short of the Task 2 plan: frozen per-turn parameters are absent, several scenarios do not prove a single agent launch, and schema v3 can modify an existing version-0 database.

### Acceptance evidence

| Scenario | Real daemon and SQLite tests | What the tests establish |
|---|---|---|
| F8 | `s1_f08_crash_inside_spawn_write_leaves_nothing`, `s1_f08_crash_after_spawn_commit_keeps_the_whole_session`, `s1_f08_lost_spawn_reply_leaves_one_whole_undispatched_session` | Atomic absence or presence of the session, turn, key and handle hash; restart handoff; one eventual submission and anchor. |
| F10 | `s1_f10_submission_precedes_agent_io_and_restarts_unknown`, `s1_f10_crash_after_prompt_write_restarts_unknown_without_resend`, `s1_f10_crash_before_acceptance_commit_restarts_unknown` | Durable submission before agent I/O; prompt-read and pre-acceptance crash points; `unknown` on restart without a second launch. The additional uncertain-submission and pre-ARM gate tests cover latch/force interaction. |
| F13 | `s1_f13_spawn_retry_after_lost_reply_replays_one_session` | Same receipt, one session and turn, one durable submission event, and canonical `invalid_params`/`idempotency_conflict`. It does **not** count launches. |
| F14 | `s1_f14_resume_retry_with_op_key_adds_one_turn` | Keyed replay adds one turn; unkeyed calls add new turns; canonical conflict and handle errors; one durable submission event per turn. It does **not** count launches. |
| F17 | `s1_f17_ninth_queued_turn_is_queue_full_and_order_kept` | The ninth queued request gets canonical `queue_full`; accepted turns submit in order, one durable submission event each. It does **not** count launches. Daemon-wide `admission_refused` is established by a Core/SQLite integration test, not this daemon scenario. |
| F28 | `s1_f28_two_callers_drive_two_sessions_without_crosstalk` | Two sessions run concurrently; their event IDs and sequences are separate and ordered. It does **not** count launches or require the expected `assistant.text` events to exist. |

None of the six F rows rests solely on a unit test. The tests establish durable submission records and session isolation where relevant. **None establishes frozen per-turn parameter inheritance or overrides.**

### Cross-branch and contract findings

The inspected startup order is Store open → anchor reconciliation → submitted-turn recovery to `unknown` → queued-turn handoff → request acceptance (`crates/via-cli/src/server.rs:182`). That prevents the T2-A crash points from being resent by T2-B2’s dispatcher. T2-C registers surviving queued turns before serving; T2-D reserves a connection slot before the dispatch grant and submission. The pending Store-failure latch blocks grants immediately, then finalizes under admission. I found no contradictory lock acquisition across those paths: receipt and stop operations take `admission` before session state; the dispatcher releases its event-head lock before taking admission for a latch.

Final shutdown drains requested dispatcher starts, joins drives, obtains Host cleanup and terminal evidence, joins clients and Store under one deadline, and exits 0 only when the accounting is clean (`crates/via-cli/src/server.rs:259`, `crates/via-core/src/engine/stop.rs:68`). Schema v3 has the intended partial index for unproven anchors, and existing v1/v2 stores receive the named refusal. The changed runtime spec’s claim that **every older existing Store** is refused has the exception below. Its “Schema v3” table also describes frozen turn parameters and tables that the current SQL does not contain; it should distinguish the target schema from the implemented interim schema.

### Acceptance blockers

1. **Frozen per-turn parameters are neither implemented nor tested.** `crates/via-core/src/api.rs:31` accepts only prompt and key; every resume receipt constructs the same `Effective::fake` values (`crates/via-core/src/engine.rs:638`), and execution uses a fixed wall deadline (`crates/via-core/src/engine/drive.rs:368`). C1 §3.3/§4 and the Task 2 plan (`docs/workstreams/rust-foundation/s1-plan.md:173`) require accepted per-turn values to be frozen and inherited. **Fix:** persist each accepted turn’s effective values, apply supported overrides and inheritance at receipt commit, drive from those values, and add a daemon/SQLite queued-turn inheritance test. Correct the schema-v3 description to match the resulting implementation.

2. **“No duplicate submission” is incompletely proved for F13/F14/F17/F28.** Their shared history assertion counts `turn.submitted`, not fake-agent starts or anchors (`crates/via-cli/tests/s1_sessions.rs:85`). A second launch of the same turn could leave one submission event and pass these checks. F28’s foreign-text check also passes if text events are absent (`crates/via-cli/tests/s1_sessions.rs:721`). **Fix:** assert exactly one anchor/start per intended turn in all four scenarios, and require each F28 session’s own expected text events.

3. **An existing `user_version=0` database is treated as a new Store.** The version check exempts zero (`crates/via-store/src/runtime.rs:29`); an existing file passes the read-only check, is reopened writable, and may have its journal mode changed before initialization (`crates/via-store/src/runtime.rs:603`, `crates/via-store/src/runtime/sql.rs:58`). This contradicts changed runtime §6’s refusal-without-mutation rule for older existing Stores. **Fix:** initialize version zero only when VIA creates a new database; refuse an existing version-zero file before writable open, with a byte-preservation regression.

### Deferred test work

Assign the fixed-sleep capacity checks (`crates/via-cli/tests/s1_crash_points.rs:2202`) and the ignored force-handoff race (`crates/via-cli/tests/s1_daemon_stop.rs:1178`) to **`via-jm4.7.7`** for deterministic failpoint barriers. The already recorded force closure, re-probe and outstanding Store-read items remain there; Task 4’s broader bounds and Store refusal work remains on **`via-jm4.7.8`**. These are not the blockers above.

This was read-only. I did not run `bd` or the Rust gate; I used the supplied orchestrator gate evidence. `git diff --check 64419c0 3955933` passed. Git status still shows only the pre-existing two modified `.beads` files; this review changed nothing.