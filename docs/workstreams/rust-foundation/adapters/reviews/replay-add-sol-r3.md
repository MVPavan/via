**SOUND** for `bc694c9..67d2a09`. Both remaining findings are fixed; no new defects found.

| Finding | Status | Evidence |
|---|---|---|
| r2 stamp-before-send race | **Fixed** | [input.rs:235](../../../../../crates/via-fake-agent/src/replay/input.rs#L235) acquires the mutex before stamping and publishing. [input.rs:128](../../../../../crates/via-fake-agent/src/replay/input.rs#L128) decides timeout under that same mutex, taking queued events first. |
| Finding 5 remainder | **Fixed** | [input.rs:304](../../../../../crates/via-fake-agent/src/replay/input.rs#L304), [327](../../../../../crates/via-fake-agent/src/replay/input.rs#L327), and [350](../../../../../crates/via-fake-agent/src/replay/input.rs#L350) cover early/equal/late arrivals against both caps and the previous-completion anchor. |

The lock/condvar protocol is sound by source inspection:

- Both waits check their predicates under the mutex and loop after waking, including spurious wakes. Condvar waits atomically release the mutex.
- Publication and removal use `notify_all`; notifications after unlocking cannot lose a required wakeup because waiters recheck the protected predicate.
- The reader waits for room **before** reading. With one producer and a consumer that only removes events, queued plus reader-held events remain bounded at two.
- Blocking reads hold no queue lock. EOF or error is published before the reader exits; EOF is cached, and errors propagate to replay failure.

The move preserves the 1 MiB line limit, bounded read, oversized/partial-input errors, JSON errors, spawn-failure propagation, run cap, ordered EOF and trailing-input checks. Fixture, capture, step, output and diagnostic limits are unchanged. The old channel-disconnection diagnostic disappears; normal terminal paths still terminate correctly.

The tests would fail on the claimed timing regressions: removing the arrival guard accepts the late cases; anchoring to `Instant::now()` accepts the deliberately late anchored line; checking expiry before queue contents fails the published-event and queued-arrival tests. These tests pin decision behavior; atomic publication itself is established by the locking code.

**New defects:** none; severity and smallest fix are not applicable.

**Verified:** `cargo nextest run --locked -p via-fake-agent` — **50 passed, 0 skipped**. Scoped diff check passed. Git remained clean on `wt/replay-add`, tip `67d2a09`.

**Could not verify:** forced scheduling/spurious-wakeup interleavings, injected stdin errors, thread exhaustion or unexpected reader panic. A reader panic now falls back to deadline expiry rather than channel-disconnection detection. I did not rerun temporary mutations or the broader gate under the read-only scope. No source edits, Git changes, Beads, vendor CLI or model runs.