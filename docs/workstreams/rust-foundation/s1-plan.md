# S1 plan: skeleton, daemon, Store v1, fake agent

Status: draft, 2026-09-26, committed at the owner's request; the owner's
approval of §2 (failure modes) is still to be confirmed before any worker
starts. Beads `via-jm4.7`. Contracts: `docs/specs/via-api-v1.md` (C1),
`docs/specs/adapter-contract.md` (C2). Testing rules:
`.repo-context/coding-style.md` §10.

## 1. Scope

S1 proves the architecture before any vendor code exists: a thin path
through all six layers and the Store, driven by a fake agent.

- **In:** the `via` binary with the daemon (socket, lock, handshake,
  auto-start, idle exit, `daemon status|stop`); Store schema v1 with
  migrations; C1 methods `hello`, `describe`, `spawn`, `resume`, `steer`,
  `cancel`, `close`, `status`, `wait`, `result`, `list`, `events` (page and
  follow), `logs`, `models`; harness `fake` only; wall and idle deadlines;
  process-tree kill; crash recovery to `unknown`; orphan kill by verified
  identity; raw log per connection and event log per turn.
- **Out:** real vendors (S2+), vendor requests and auto-decline (S3), bound
  enforcement (the fake route declares bound `full` only), version gate
  (S2), server sharing (S3), structured-output validation (S2).
- **Platform:** Linux only in S1 (development runs on WSL2). macOS process
  identity differs and is a later slice.

## 2. Failure modes (owner review)

Method: at every boundary (socket, disk, child process, clock, other
callers) ask what happens if the other side is dead, slow, repeated,
malformed, huge or lying. Each row becomes an end-to-end scenario unless
marked *isolated*. Rows marked **A** are the S1 acceptance set.

### Daemon and socket

| # | What goes wrong | Required behaviour |
|---|---|---|
| F1 | Two CLIs auto-start a daemon at the same moment | Exactly one daemon holds the lock; the other exits; both calls succeed |
| F2 | Daemon killed, stale socket file left | Next CLI starts a new daemon, which takes the lock before replacing the socket |
| F3 | Socket directory unsafe (owner, mode, symlink) | Daemon refuses to start with a clear error; CLI exits 4 |
| F4 | Client and daemon versions differ | `version_mismatch`; CLI restarts an idle daemon, else reports |
| F5 | No `hello`, malformed JSON, unknown field, line over 16 MiB | Request error (oversize closes that connection); the daemon keeps serving others |
| F6 | Idle exit races a new client | Never exits with a session active or a client connected; a late client gets a fresh daemon |
| F7 | `daemon stop` while sessions are active | Refused; `--drain` finishes accepted turns then stops; `--force` closes sessions |

### Store and crashes

| # | What goes wrong | Required behaviour |
|---|---|---|
| F8 | Crash in the middle of `spawn`'s write | Session, turn 1, handle hash and key exist together or not at all |
| F9 **A** | Daemon `kill -9` while a turn runs | Restart marks the turn `unknown`, never re-sends it, cancels turns queued behind it (P6) |
| F10 | Crash after the prompt reached the agent, before acceptance was recorded | Still `unknown`, never re-dispatched: a submission record is written before any agent I/O |
| F11 | Store from a newer VIA, or unreadable | Daemon refuses to start and never rewrites it |
| F12 | Store write fails mid-turn | The turn resolves with a store failure; nothing is lost silently |

### Calls and retries

| # | What goes wrong | Required behaviour |
|---|---|---|
| F13 | `spawn` retried after a lost reply | Same key + handle + params → same receipt, one session; changed params → `idempotency_conflict` |
| F14 | `resume` retried after a lost reply | Same `op_key` → one turn; without a key a retry is a new turn (documented) |
| F15 | Wrong or missing handle on a mutating verb | `invalid_handle`; no state change |
| F16 | Handle leaks | The handle string appears nowhere in logs, trace, events, envelopes or the Store dump (artifact scan) |
| F17 | Ninth queued turn | `admission_refused`; one turn runs at a time; order kept |
| F18 | Verb the route does not support | `unsupported_verb`; no state change |

### Agent processes

| # | What goes wrong | Required behaviour |
|---|---|---|
| F19 **A** | Agent hangs silently | Idle or wall deadline fails the turn; the whole tree, grandchild included, is gone within the grace period |
| F20 | Agent ignores SIGTERM | Escalated to SIGKILL after the grace period |
| F21 | Agent crashes mid-line | Turn `failed` with a vendor class; the partial bytes stay in the raw log |
| F22 | Agent outlives a daemon crash (as Claude did in probe P4) | Restarted daemon kills it only after confirming uid, start time and marker; a reused pid is never signalled (plus an *isolated* identity test) |
| F23 | Secrets in the daemon's environment | The agent sees only allow-listed variables |

Known limitation: a grandchild that calls `setsid` leaves the process group
and escapes the tree kill. Revisit with cgroups in a later slice.

### Streams and backpressure

| # | What goes wrong | Required behaviour |
|---|---|---|
| F24 **A** | Agent floods hundreds of MB | The pipe reader never stops; daemon memory stays bounded; if Core cannot drain for 10 s the turn fails `overflow` (A1) |
| F25 | A follower stops reading | It gets `event_end: lagged`; the turn and other clients are unaffected |
| F26 | Follow starts while events are being written | No gap or duplicate at the replay → live boundary; `seq` dense per session |
| F27 | Invalid UTF-8, split or huge lines | Raw log keeps exact bytes; the framer never panics (plus *isolated* property tests) |
| F28 **A** | Two callers drive two sessions at once | No crosstalk; each session's events stay ordered |

### CLI

| # | What goes wrong | Required behaviour |
|---|---|---|
| F29 | Ctrl-C on foreground `via spawn` | Exit 130; the receipt was already printed; the session keeps running |
| F30 | Client disconnects during `wait` | The turn continues; `result` returns it later |

## 3. How S1 is tested

- End-to-end scenarios run the real `via` binary, daemon and Store against
  the fake agent, each in its own state directory and socket (§10).
- **Fake agent:** a separate test-only binary (`crates/via-fake-agent`, never
  shipped). One process per turn, NDJSON on stdout like a CLI route, driven
  by a scenario script: reply, hang, flood, crash mid-line, ignore SIGTERM,
  spawn a grandchild, dump its environment, report its pids.
- **Failpoints:** named pause and crash points (F8, F10, F12) behind a
  test-only cargo feature, absent from release builds.
- **Isolated, failure-first:** NDJSON framer (property tests), process
  identity check, turn state machine, Store migrations.
- **Artifact:** per run, as §10 specifies (summary, per-scenario evidence,
  sha256 manifest, `REPORT.md`).

## 4. Work plan

Scenarios are written before the code they test and fail first.

| Wave | Work | Workers (GPT-6 Sol high) |
|---|---|---|
| W1 | Internal interfaces for C3 Route, C4 Wire, C5 Host and the Store as Rust types and signatures with doc comments; crate wiring | 1 |
| W1 | Fake agent, E2E harness with artifact, all §2 scenarios (red) | 1, in parallel |
| W2 | Implementation by owned crate: Store; Host; Wire + fake Route; fake Adapter + Core; CLI + daemon | up to 4, disjoint paths |
| W3 | Integrate to green, gate, Astra medium review, fixes, commit | orchestrator + 1 |

Internal contracts (C3–C5, S) are recorded as code, not separate spec
documents; the owner reviews C1 and C2 only. Each worker gets owned paths,
constraints, acceptance scenarios and context pointers, and must not
commit, stage or run `bd`.
