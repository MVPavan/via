# T2-B: multi-turn sessions, resume, queues and retries

Model: Opus 5.5 medium. Follow `../w1/common.md`; report to
`reports/T2-B.md`. Runs in parallel with T2-A.

Contracts: C1 `docs/specs/via-api-v1.md` (§3.2 spawn idempotency, `resume`
and `op_key`, §3.3 and §8.1 queue errors, `wait`), runtime
`docs/specs/runtime-contracts.md` (queue bounds: 8/session, 128
daemon-wide). Core currently assumes one turn per session (fixed event
sequence starts); remove that assumption.

1. **Multi-turn sessions and `resume`**, with `op_key` retry safety
   (F14): same `op_key` → one turn; without a key a retry is a new turn.
2. **Per-session queue** (F17): one turn runs at a time, order kept, the
   ninth queued turn is `queue_full`; daemon-wide refusal stays
   `admission_refused`.
3. **Spawn retries** (F13): same key + handle + params → the same receipt
   and one session; changed params → `idempotency_conflict`. Check what
   exists and test it end to end through `via`.
4. **Independent sessions** (F28): two callers drive two sessions at once;
   no crosstalk; each session's events stay dense and ordered.
5. **`wait.timeout_ms`** (C1), currently rejected as `unknown_field`.
6. **Harness gaps** carried on `via-jm4.7.6`: assert 0700 modes of test
   and evidence directories (evidence `raw/` included); an
   `infrastructure_failure` classification test for the scenario runner.

Owned: `via-core` (`engine.rs` verbs, `engine/drive.rs` logic, a new
module for queues if useful), `via-store` sessions/turns/queue, `via-cli`
client/server for `resume` and `wait`, tests. Crash-point tests (F8, F10)
and the failpoint controller belong to T2-A; don't build a second fault
mechanism.
