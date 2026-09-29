# S1 plan: skeleton, daemon, Store v1, fake agent

Status: scope and failure-first approach approved, 2026-09-26. The incremental
sequence below replaces the original whole-workspace worker waves and is
proposed for agreement before dispatch. Beads `via-jm4.7`.
Contracts: `docs/specs/via-api-v1.md` (C1),
`docs/specs/adapter-contract.md` (C2). Testing rules:
`.repo-context/coding-style.md` §10.

## 1. Scope

S1 proves the architecture before any vendor code exists: a thin path
through all six layers and the Store, driven by a fake agent.

- **In:** the `via` binary with the daemon (socket, lock, handshake,
  auto-start, idle exit, `daemon status|stop`); Store schema v1 with
  migrations; C1 methods `hello`, `describe`, `spawn`, `resume`, `steer`,
  `cancel`, `close`, `status`, `wait`, `result`, `list`, `events` (page),
  `logs`, `models`; harness `fake` only; wall and idle deadlines;
  process-group cleanup; crash recovery to `unknown`; orphan kill by verified
  identity; evidence folder and event log per turn; `serve --stdio` proxy.
- **Out:** real vendors (S2+), vendor requests and auto-decline (S3), bound
  enforcement (the fake route declares bound `full` only), version gate
  (S2), server sharing (S3), structured-output validation (S2).
- **Platform:** Linux only in S1 (development runs on WSL2). macOS process
  identity differs and is a later slice.

## 2. Failure modes

Method: at every boundary (socket, disk, child process, clock, other
callers) ask what happens if the other side is dead, slow, repeated,
malformed, huge or lying. Each row becomes an end-to-end scenario unless
marked *isolated*. All rows are required for S1 completion; **A** marks the
four original headline acceptance scenarios, not an exemption for other rows.

### Daemon and socket

| # | What goes wrong | Required behaviour |
|---|---|---|
| F1 | Two CLIs auto-start a daemon at the same moment | Exactly one daemon holds the lock; the other exits; both calls succeed |
| F2 | Daemon killed, stale socket file left | Next CLI starts a new daemon, which takes the lock before replacing the socket |
| F3 | Socket directory unsafe (owner, mode, symlink) | Daemon refuses to start with a clear error; CLI exits 4 |
| F4 | Client and daemon versions differ | `version_mismatch`; CLI restarts an idle daemon, else reports |
| F5 | No `hello`, malformed JSON, unknown field, line over 1 MiB (T4-A39) | Request error (oversize is `request_too_large` and closes that connection); the daemon keeps serving others |
| F6 | Idle exit races a new client | Never exits with a session active or a client connected; a late client gets a fresh daemon |
| F7 | `daemon stop` while sessions are active | Refused; `--drain` finishes accepted turns then stops; `--force` closes sessions |

### Store and crashes

| # | What goes wrong | Required behaviour |
|---|---|---|
| F8 | Crash in the middle of `spawn`'s write | Session, turn 1, handle hash and key exist together or not at all |
| F9 **A** | Daemon `kill -9` while a turn runs | Restart marks the turn `unknown`, never re-sends it, cancels turns queued behind it (P6) |
| F10 | Crash after the prompt reached the agent, before acceptance was recorded | Still `unknown`, never re-dispatched: a submission record is written before any agent I/O |
| F11 | Store from a newer VIA, or unreadable | Daemon refuses to start and never rewrites it |
| F12 | Store write fails mid-turn | A named Store failure is surfaced; no successful durable write is claimed. The design packet must specify caller-visible behavior, admission/cleanup and restart reconciliation when persisting the failure itself is impossible |

### Calls and retries

| # | What goes wrong | Required behaviour |
|---|---|---|
| F13 | `spawn` retried after a lost reply | Same key + handle + params → same receipt, one session; changed params → `idempotency_conflict` |
| F14 | `resume` retried after a lost reply | Same `op_key` → one turn; without a key a retry is a new turn (documented) |
| F15 | Wrong or missing handle on a mutating verb | `invalid_handle`; no state change |
| F16 | Handle leaks | The handle string appears nowhere in logs, trace, events, envelopes or the Store dump (artifact scan) |
| F17 | Ninth queued turn | `queue_full` (C1 §3.3/§8.1); one turn runs at a time; order kept. Daemon-wide resource refusal is separately `admission_refused` |
| F18 | Verb the route does not support | `unsupported_verb`; no state change |

### Agent processes

| # | What goes wrong | Required behaviour |
|---|---|---|
| F19 **A** | Agent hangs silently | Idle or wall deadline fails the turn; every process remaining in the VIA-owned process group, including an ordinary grandchild, is gone within the grace period |
| F20 | Agent ignores SIGTERM | Escalated to SIGKILL after the grace period |
| F21 | Agent crashes mid-line without a terminal result | Turn `failed(process_exited)` after Host confirms exit (C1 §7.6); the first 64 KiB of the partial line go to `undecoded.bin` (T4-A46) |
| F22 | Agent outlives a daemon crash (as Claude did in probe P4) | Restarted daemon kills it only after confirming uid, start time, group and marker; a reused pid is never signalled (plus an *isolated* identity test) |
| F23 | Secrets in the daemon's environment | The agent sees only allow-listed variables |

Known limitation: a grandchild that calls `setsid` leaves the process group
and escapes the group kill. S1 does not prove arbitrary descendant containment.
Document that boundary; stronger containment needs a separate design decision.

### Streams and backpressure

| # | What goes wrong | Required behaviour |
|---|---|---|
| F24 **A** | Agent floods hundreds of MB | The pipe reader never stops; daemon memory stays bounded; if Core cannot drain for 10 s the turn fails `overflow` (A1) |
| F25 | A client polls `status` during a flood (T4-A25; was: a follower stops reading) | `status` answers from memory; the turn and other clients are unaffected |
| F26 | The daemon crashes after a step commit (T4-A25; was: follow starts while events are written) | Step rows survive up to the last committed step; `seq` dense per session |
| F27 | Invalid UTF-8, split or huge lines | Exact messages or explicit failure, the first 64 KiB kept in `undecoded.bin`; the splitter never panics (plus *isolated* property tests) |
| F28 **A** | Two callers drive two sessions at once | No crosstalk; each session's events stay ordered |

### CLI

| # | What goes wrong | Required behaviour |
|---|---|---|
| F29 | Ctrl-C on foreground `via spawn` | Exit 130; the receipt was already printed; the session keeps running |
| F30 | Client disconnects during `wait` | The turn continues; `result` returns it later |

## 3. How S1 is tested

- End-to-end scenarios run the real `via` binary, daemon and Store against
  the fake agent, each in its own state directory and socket (coding-style §10).
- **Fake agent:** a separate test-only binary (`crates/via-fake-agent`, never
  shipped). One process per turn, NDJSON on stdout like a CLI route, driven
  by a scenario script: reply, hang, flood, crash mid-line, ignore SIGTERM,
  spawn a grandchild, dump its environment, report its pids. This shape does
  not prove persistent-session processes or shared servers; vendor slices add
  those fixtures against their real adapter/route code.
- **Failpoints:** named pause and crash points (F8, F10, F12) behind a
  test-only cargo feature, absent from release builds.
- **Isolated, failure-first:** NDJSON message splitter (property tests), process
  identity check, turn state machine, Store migrations.
- **Artifact:** per run, as coding-style §10 specifies (summary, per-scenario evidence,
  sha256 manifest, `REPORT.md`).

## 4. Work plan

Scenarios are written before the code they test, increment by increment. The
coordinator briefs one Sol-high implementation owner per integrated increment;
parallel workers are optional only for independent paths after interfaces are
stable. Each implementation increment reaches green before the next dependent
one starts. Contract types and test-harness preparation are parallel prerequisites
within Task 1's integration group: its intentional red tests become green at
the prompt-to-result join, rather than being treated as a completed runtime gate.

### Design gate: internal contracts and unresolved failure behavior

Stage: `via-jm4.7`. Astra high designs; Sol high reviews. The packet defines
C3 Route, C4 Wire, C5 Host and Store ownership/signatures, transaction and
raw-log durability boundaries, F12 behavior under persistent storage failure,
and numeric memory, cleanup and slow-client bounds. It specifies observable
test seams and how test-only failpoints are enabled and excluded from release.
Produce the bounded design under `docs/specs/`, with Rust doc
comments reflecting it during implementation. Update C1/C2 only when the
resolved behavior requires it; do not reopen approved decisions gratuitously.

Exit: the Sol review has no unresolved blocking finding; each acceptance
assertion has a measurable outcome and the exact gate invocation is recorded.
Dependent implementation waits for this packet. The following tasks consume it.

### Task 1: working prompt-to-result path

Goal: a real CLI auto-starts the daemon, receives a committed spawn receipt,
drives the fake agent and retrieves a durable result through C1.
Stage: `via-jm4.7`.

Files: own the minimal path through `crates/via-*`, test support in the planned
`crates/via-fake-agent`, integration tests under `crates/via-cli/tests`, and
necessary workspace/layer-check wiring. One worker owns overlapping paths.

Interfaces: consumes approved C1/C2 and the design packet; produces an executable
path and reusable isolated test harness, including evidence artifacts.

Approach: write the successful foreground/background flow and F15, F16, F18,
F30 first, then implement it. No broad stubs for future adapter behavior.

Verification: the Rust gate in `.repo-context/verification.md` passes with
nonempty tests; E2E evidence shows the receipt, result and Store agree. Remove
`NEXTEST_NO_TESTS=pass` from the guidance. Test seams: CLI, socket, fake-agent
script and isolated Store dump. Dependencies: design gate. Risk: test-first
cross-process commit/receipt ordering and handle privacy.

### Task 2: session continuity, queues and retries

Goal: resume preserves a session; duplicate keyed calls do not duplicate work;
queues and independent sessions behave according to C1.
Stage: `via-jm4.7`.

Files: Core, Store, CLI and their integration tests; fake-agent scripts as
needed. Interfaces: consumes Task 1; produces durable queue/idempotency behavior.
Approach: failure-first F8, F10, F13, F14, F17 and F28, including crash points
around submission. Assert canonical error kinds and frozen per-turn parameters.

Verification: Rust gate plus the design packet's failpoint invocation; each
listed scenario proves one intended submission and isolated session histories.
Test seams: request/reply loss, persisted intent and controlled agent acceptance.
Dependencies: Task 1. Risk: never infer non-submission from a timeout.

### Task 3: daemon lifecycle and process failure recovery

Goal: concurrent startup, shutdown, deadlines and restart preserve truthful
turn outcomes and only signal positively identified VIA-owned processes.
Stage: `via-jm4.7`.

Files: CLI daemon wiring, Host, Core, Store and affected Route/Wire code plus
integration tests. Interfaces: consumes Tasks 1–2; produces lifecycle controls
and recovery. Approach: failure-first F1–F4, F6–F7, F9, F11–F12, F19–F23 and
F29. Include cancel/close behavior, queued cancellation, drain, force and verified
identity tests. Test owned-group cleanup within the documented containment limit.

Verification: Rust gate and failpoint scenarios; evidence distinguishes
cancel acknowledgement, cleanup and unknown outcomes. Test seams: process
signals, identities, controlled deadlines and Store failure injection.
Dependencies: Task 2. Risk: tests need an outer supervisor that bounds execution
and cleans up test processes even on failure.

### Task 4: streams, overload and full C1 conformance

Goal: bounded processing survives noisy agents and slow callers, and every S1
C1 method is exercised or explicitly refused according to fake capabilities.
Stage: `via-jm4.7`.

Files: Wire, Route, Adapter, Core, CLI and stream/conformance tests. Interfaces:
consumes Tasks 1–3; produces complete S1 behavior. Approach: failure-first F5
and F24–F27, plus normal event paging, status progress, log isolation, models/describe,
daemon status and `serve --stdio` parity. Assert the design packet's numeric
bounds; cover message-splitter property tests.

Verification: the full Rust and failpoint gates pass, every F1–F30 scenario has
an artifact, and the normal default suite meets coding-style's speed budget.
Test seams: controllable stream rates, blocked readers, memory observation and
persisted sequence numbers. Dependencies: Task 3. Risk: overload must not hide
data loss or block lifecycle control.

### Review and completion

Astra medium reviews each coherent increment above, not each small edit. Sol
high fixes findings; the coordinator checks the integrated result and reruns
affected checks. After Task 4, Astra high performs a fresh-context critical
review of architecture, ownership, failure behavior and evidence. Required
fixes land before S1 closes. Workers do not stage, commit or mutate Beads;
the coordinator records results and respects the separate Git approval boundary.
