# Coding style

Applies to all Rust code under `crates/` (§1–§11) and to written records
(§12). Configuration enforces what it can: `rust-toolchain.toml`,
`[workspace.lints]` in `Cargo.toml`, `clippy.toml`, `deny.toml` and
`scripts/check-layers.py`. This file holds the rules configuration cannot
check. Gate commands: `.repo-context/verification.md`. Terms:
`.repo-context/CONTEXT.md`. Contracts: `docs/specs/`. When a question is not
answered here, look up the relevant entry in the Microsoft Pragmatic Rust
Guidelines (`https://microsoft.github.io/rust-guidelines/`); do not preload
the set.

## 1. Workspace and layers

- One crate per layer: `via-cli` (L1, the only binary, `via`), `via-core`,
  `via-adapters`, `via-routes`, `via-wire`, `via-host`, and `via-store` on the
  side. Internal path dependencies follow the permitted layer graph in
  `scripts/check-layers.py`; changing the graph is an architecture change
  (owner, and `docs/brainstorms/system-layers.md` in the same change).
- A crate's `pub` items are its contract. Default to private modules and
  `pub(crate)`; re-export the contract from `lib.rs`. A change to C1, C2 or
  the Store contract updates `docs/specs/` in the same change.
- In production code, ownership of OS resources is exclusive:
  - only `via-host` creates vendor processes;
  - only `via-store` opens SQLite;
  - only `via-wire` reads and writes vendor pipes and sockets.
  The one exception: `via-cli`'s client half may start the daemon (itself).
  Test harnesses may spawn VIA and fake vendors and inspect their isolated
  artifacts.
- Module size is a review prompt, not a hard limit: past about 500 lines
  (excluding tests), consider a split; past about 800, put new code in a
  new module. Extract a helper when it names an ownership step, a cleanup
  step or an invariant, even if it is used once.
- No trait with a single implementation unless it is a contract (C2, the C3
  family, Store) or a seam the testing policy needs. Adapters are a closed
  set compiled into VIA: dispatch with an enum, not `Box<dyn …>` plugins.

## 2. Dependencies

- External crates come only from `[workspace.dependencies]`, referenced with
  `workspace = true`. Adding one needs a present requirement that `std` or
  an approved crate cannot meet, a passing `cargo deny check`, and the
  orchestrator's approval recorded in the change report. No git sources.
- Prefer `std`. Example: the daemon's single-instance lock is
  `std::fs::File::try_lock` (stable since Rust 1.89), not a crate.
- Enable only the features a crate uses (no tokio `full`). Build and test
  with `--locked`.

## 3. Types and wire formats

- Use newtypes for identifiers and quantities whose confusion causes
  defects (`SessionId`, `TurnNumber`, `VendorSessionId`, `ConnectionId`,
  byte offsets, deadlines). Ordinary text and unambiguous counts may cross
  contracts as plain types.
- No `bool` or ambiguous `Option` parameters on contract functions; use an
  enum or a parameter struct.
- Match VIA's closed internal enums (state machines, failure classes)
  exhaustively, with no `_` arm, so a new variant forces every site to
  decide. Boundary types that must grow are `#[non_exhaustive]`; Rust then
  requires a wildcard in every other crate, including other VIA crates, and
  that wildcard arm must handle the case explicitly (named error or
  logged fallback), never silently ignore it. `#[non_exhaustive]` gives no
  serde wire compatibility on its own.
- Parse once at the boundary into typed values; inner code never
  re-validates strings.
- **Strict about our input, tolerant of theirs.**
  - C1 requests use `#[serde(deny_unknown_fields)]`.
  - Vendor messages ignore unknown fields. Decode the protocol envelope
    first, then dispatch on the method or type. An unknown **notification**
    becomes a raw-payload event (tag plus bounded payload) and is logged. An
    unknown **request** gets the protocol's decline or error response under
    a deadline and a canonical event (D3); it is never left unanswered.
  - A malformed message of a known type is a protocol error, not an
    unknown-message fallback.
  - `#[serde(other)]` cannot keep a payload; write the payload-preserving
    fallback explicitly.
- Tag and field names on the wire come from the spec, set explicitly with
  `#[serde(rename = "…")]` where they differ from Rust names (C1 event tags
  are dotted, e.g. `action.denied`). Types that mirror a vendor protocol
  keep the vendor's names.
- Vendor input is untrusted: cap frame and line length and buffered bytes;
  never allocate from an unchecked vendor-supplied size.

## 4. Errors

- Library crates: a `thiserror` enum per contract boundary, carrying context
  (session, turn, route, vendor). `anyhow` only in the binary's `main`
  functions (client and daemon).
- Failure classes are enums shared with C1/C2, never strings.
- No `unwrap`, `expect`, `panic!`, `todo!` or `unimplemented!` outside tests.
  Where an invariant guarantees success, use
  `#[expect(clippy::expect_used, reason = "<the invariant>")]`.
- Every lint suppression is `#[expect(…, reason = "…")]`, never `#[allow]`,
  so it fails once it is no longer needed.
- Never discard an error (`let _ =`, `.ok()`) without a comment saying why
  that is safe.

## 5. Async, cancellation, deadlines and backpressure

- Runtime: tokio. Async trait methods are written
  `fn op(&self, …) -> impl Future<Output = T> + Send`; no `async_trait`.
- **Task ownership.** No detached tasks. Every `tokio::spawn` belongs to a
  `JoinSet` or `TaskTracker` owned by the component that shuts it down, and
  every long-lived task receives a `CancellationToken` from that owner.
  Owners observe task failures promptly, keep collecting finished `JoinSet`
  entries, and at shutdown close and await their trackers within a bound,
  reporting any that did not finish. `TaskTracker` does not abort tasks and
  `spawn_blocking` work cannot be aborted: bound how much blocking work is
  admitted and keep ownership until it completes.
- **Channels are bounded;** unbounded channels are not used. At each channel,
  a comment says what happens when it is full (wait, drop and count, or
  fail).
- **Pipe reads never wait for consumers.** One reader task per vendor pipe
  reads continuously. Raw-log staging is bounded in bytes. If lossless
  recording cannot keep up, or a raw-log write fails, the connection fails:
  Host supervises the process, draining continues (bounded) during cleanup,
  and the raw log is marked incomplete. Never drop bytes silently, and never
  report a turn as fully observed after a gap.
- **Deadlines are absolute and monotonic.** Core hands down an absolute wall
  deadline (`Instant`) and a separate idle deadline that resets on
  progress. Nested operations use the remaining time; they never start a
  fresh relative timeout that extends the budget. Wall-clock time is for
  records only; `Instant` is never persisted. Convert durations with checked
  arithmetic.
- **Cancellation boundaries.** At every `select!`, `timeout` or abort point,
  state what partial progress can remain and how it is cleaned up.
  `write_all` and `read_exact` are not cancel-safe: keep framing state and
  write offsets across cancellation, or close the connection before reuse.
  A timeout does not prove that a vendor action or Store write stopped;
  never retry an uncertain mutation automatically.
- Never block the runtime. SQLite runs on the Store's own writer thread,
  fed by a channel; other blocking calls use `spawn_blocking`.
- Never hold a `std::sync::Mutex` guard across `.await`; prefer message
  passing to shared locks.
- **Shutdown order** (daemon and each component): stop admission; settle or
  classify active turns; close vendor input and transports while readers
  keep draining; escalate only VIA-owned processes on deadline; reap
  children; flush raw logs and Store records; join the Store thread;
  release the daemon lock. Never kill a shared server to cancel one turn.

## 6. Processes, environment and the socket

- Start vendor processes with `process-wrap`'s process-group wrapper,
  explicitly (the dependency alone creates no group), with the VIA marker
  variable, an argv array (never `sh -c` or a command string), an explicit
  cwd, and a recorded pid, pgid, uid and process start time. Host keeps the
  child handle and reaps every child it starts. Killing means the whole
  group, with timed escalation.
- **Process identity after a crash.** Before signalling a recovered pid or
  pgid, positively confirm uid, process start time, group and marker. If
  identity is uncertain, report an orphan and do not signal. Implement and
  test discovery separately on Linux and macOS. Never attach to, signal or
  reap a process VIA did not start.
- **Environment.** Build each vendor's environment from an explicit,
  reviewed per-adapter allow-list plus the VIA marker; never pass the
  caller's full environment. Invariant 1: never read, copy or log vendor
  credentials.
- **Socket and state directory.** Validate the state/runtime directory's
  owner, mode (`0700`) and file type, and reject symlinks. Create the socket
  with restrictive permissions (`0600`) from the start. Take the
  single-instance lock before checking or replacing a stale socket; never
  unlink the lock file while in use. Both ends verify the peer uid before
  any protocol traffic; implement and test this on Linux and macOS.
- Only the daemon's `main` installs signal handlers.

## 7. Store

- rusqlite, one writer thread owning the `Connection`. On open: verify WAL
  mode, set the `synchronous` level the Store contract requires, enable
  foreign keys where used, and bound `busy_timeout`. Bound WAL growth with
  checkpoints.
- Transactions are short and never span vendor I/O or an `.await`.
- Schema changes are forward-only migrations keyed by
  `PRAGMA user_version`; each migration and its version bump commit
  atomically. Refuse to open a database with a newer schema than the binary
  supports. Update the spec in the same change.
- **Write order.** Persist a turn's submission intent before submitting to
  the vendor; acknowledge state to a caller only after the commit. Raw-log
  bytes are flushed before any Store row references their offsets; recovery
  handles truncated or unreferenced raw spans.
- Disk-full and I/O failures map to named Store errors, never panics.

## 8. Observability and privacy

- `tracing` with structured fields (`session`, `turn`, `route`, `pid`);
  contract methods carry `#[tracing::instrument(skip_all, fields(…))]`.
- Tracing is the daemon's own diagnostics. Environment values, credentials
  and caller handles never appear in tracing at any level. Prompts and
  vendor payloads never appear in tracing at `info` or above.
- Raw logs and Store contents are private local data and may contain
  sensitive vendor output. Retention and export are explicit. Only
  separately sanitized copies become fixtures or shared reports; originals
  stay local so their offsets remain valid.

## 9. Naming and documentation

- Use glossary terms exactly: Session, Turn, Step, VIA API, Core, Adapter,
  Route, Wire, Host, Store, daemon. "Run" is never a type, field, table or
  module name.
- Follow the Rust API Guidelines naming (`as_`/`to_`/`into_`, no `get_`
  prefix, `iter`/`iter_mut`/`into_iter`).
- Every `pub` item has a doc comment. Each crate's docs say what its layer
  owns and must never do. Comments explain why, not what. No `TODO` without
  a bead id.

## 10. Testing (PROPOSED, pending owner confirmation)

- **Failure modes first.** Each slice's plan lists how it can fail before any
  code is written. Each failure mode maps to a test, and the slice report
  shows the mapping. Tests are never written after the code to mirror it.
- **End-to-end tests are the main mechanism.** They run the real `via`
  binary, a real daemon and a real Store.
- **Fake vendors for the default gate.** Adapters point at scripted stand-in
  binaries (fake `claude`, fake `codex app-server`, …). The real adapter and
  route code runs; only the vendor process is fake. Fakes:
  - validate the requests VIA sends;
  - replay recorded, sanitized transcripts;
  - inject faults: hang, flood, crash, partial frames, vendor requests,
    slowness;
  - advance through explicit synchronization points, never fixed sleeps.
  VIA therefore supports a configurable vendor binary path and state
  directory.
- **Isolated tests only where they are sharper or much cheaper,** written
  failure-first: property tests (`proptest`) for the byte framer; focused
  tests for the session/turn state machines, Store migrations and vendor
  message mapping.
- **Every bug fix** starts with the smallest failing reproduction, written
  before the fix. It is an end-to-end scenario when the defect crosses a
  process or contract boundary.
- **Determinism.** Seeded inputs; controlled time where a component allows
  it, and OS timing tested separately with bounded tolerances. IDs are
  normalized through a consistent mapping that preserves references. Assert
  per-session order and the permitted partial order across sessions; never
  sort away ordering. Snapshot (`insta`) only stable canonical fields.
- **Isolation and supervision.** Each scenario owns its state directory,
  socket, lock and fake-vendor environment. An outer harness enforces the
  scenario deadline, kills and reaps what the scenario left behind, and
  writes the artifact even when the scenario fails. Outcomes are pass,
  fail, timeout or infrastructure failure, reported distinctly.
- **Artifact** per run:
  - `summary.json`: per-scenario outcome; seed; VIA version, git commit and
    dirty-tree status; `Cargo.lock` hash; toolchain; OS and architecture;
    features; fake binary and fixture hashes or vendor versions;
    normalization version;
  - per scenario: envelopes, event logs, raw logs, a consistent Store dump
    (SQLite backup, not a copy of the live file) and the daemon trace;
  - a sha256 manifest, verified on replay, and a short `REPORT.md`.
  The gate fails if a scenario omits its summary, manifest or required
  evidence.
- **Live-vendor sets** are small, per adapter, and a separate gate run
  before a slice merges. They assert structure (states, event types,
  envelope fields), not agent text. Authentication, quota or availability
  failures are infrastructure failures, never passes. Refreshing fixtures
  from live runs needs review before snapshots are accepted.
- **Speed budget.** The default suite stays under about 2 minutes; nextest
  runs scenarios in parallel.

## 11. Before handing work back

- While editing, run `cargo fmt --all`; the gate itself uses `--check`.
  Scope commands with `-p <crate>` while iterating, then run the full gate
  in `.repo-context/verification.md`.
- Never weaken a lint, a `deny.toml` rule or the layer check to pass the
  gate; report the conflict instead. Never hand-edit `Cargo.lock`.
- Report the commands you ran and their results.

## 12. Written records (all languages)

- Match the surrounding Markdown: dense prose, tables for comparisons, short
  sections, no filler.
- Cite repo-relative paths; mark parent-repo paths as such
  (`.repo-context/repo-map.md`).
- Label claims as checked, reported, inferred or UNVERIFIED, as the design
  record does; date vendor facts that change.
- Record decisions in the design record (`docs/`) with owner and date; keep
  `.repo-context/` files as routing and summaries that cite their source.
- Experiments go in `scratchpad/`; production code goes in `crates/`.
