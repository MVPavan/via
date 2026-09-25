# VIA: Rust or Go? First-principles view

## 1. Recommendation

**Go, with moderate confidence (about 60–65%).** The runtime work VIA must do is supervising processes, moving JSON around and keeping state in SQLite. Go handles that with less machinery. Most facts that favour Rust are either small or depend on a choice the owner hasn't made. None of this is measured yet, so a two-language spike should confirm it before anything is committed (§5).

## 2. The lens applied: what VIA does at runtime

With the product framing removed, a VIA run does six things:

1. **Spawn** a vendor binary in its own process group, from a detached worker process, one worker per run.
2. **Pump** its stdout and stderr: write the raw bytes to a log verbatim, and parse JSONL or JSON-RPC frames into events.
3. **Persist** a launch receipt and run state to SQLite, with several worker processes writing concurrently (WAL mode and `busy_timeout`).
4. **Serve** those events over CLI `--json` and `serve --stdio` JSON-RPC.
5. **Cancel** by signalling the process group, then reap the processes.
6. **Recover** after a crash by reconciling the store with the processes still alive.

Over its life, VIA mostly needs one thing: **adapters that keep up with 10+ vendor CLIs that change often**, pinned and contract-tested per version.

**Binding constraints:**
- **C1. Getting process lifecycle right.** This covers pipes, process groups, reaping, and grandchildren that keep pipes open. Most defects will come from here.
- **C2. A static binary with SQLite inside, cross-compiled.** The owner rated this important.
- **C3. How cheaply adapters can be written and fixed.** This is mostly protocol types and fixtures, written by AI implementers.
- **C4. Tens of concurrent runs.** This is trivial for either language. It does not bind.

**Incidental constraints** (they feel important but don't decide much):
- CPU speed and memory. The workload waits on I/O almost all the time.
- Memory safety. There is no untrusted parsing in a hot loop, and both languages are memory-safe in practice here.
- Binary size.
- Whichever language the harnesses themselves are written in. VIA talks to them over stdio, not by linking their code.

## 3. Deciding factors

### F1. Static binary with SQLite (C2): favours Go, but not strongly

- **Go:** pure-Go SQLite drivers (`modernc.org/sqlite`, or `ncruces/go-sqlite3` via wasm) allow `CGO_ENABLED=0`. That makes `GOOS/GOARCH` cross-builds trivial, including Windows and macOS. **UNVERIFIED in this session:** that current versions of these drivers support WAL correctly with several processes writing at once. The spike must test this.
- **Rust:** `rusqlite` with the `bundled` feature compiles SQLite's C code, so every target needs a C cross-toolchain (musl, `cargo-zigbuild` or `cross`). This is well-trodden but is real CI work, especially for macOS and Windows targets.

### F2. Process lifecycle (C1): roughly equal

- Both languages put a child in its own process group the same way:
  - Go: `SysProcAttr{Setpgid: true}` or `Setsid`
  - Rust: `CommandExt::process_group`, stable since 1.64
- Both kill the group with `kill(-pgid, sig)`. Windows job objects are manual work in both (`x/sys/windows` in Go, the `windows` crate in Rust). The fact that `exec.CommandContext` kills only the direct child doesn't separate them; the earlier council said the same (README §10).
- **Grandchild holding a pipe open.** This is the real trap in both languages.
  - In Go, `cmd.Wait` blocks until pipe copying ends. `Cmd.WaitDelay` (added in Go 1.20, from memory, **UNVERIFIED** here) exists for exactly this case.
  - In Rust with tokio, the same hang shows up as a read that never reaches EOF, or a `select!` branch whose cancellation drops buffered data.
- **The "one worker process per run" rule removes most of Rust's concurrency advantage.** Each worker handles one child: about 3 pipe readers, a DB writer and a signal handler. In Go that is one goroutine per reader plus `context.Context`, a direct match. In Rust it is either tokio (cancellation-safety rules on `select!`, `kill_on_drop`, extra runtime) or plain threads. How hard tokio cancellation is here remains unmeasured, as the brief says.

### F3. Adapter cost and churn (C3): roughly equal, a slight Rust edge for ACP only

- **ACP.** Rust has the official SDK; Go has only `coder/acp-go-sdk`, community-maintained and lagging (v0.13.5, last push 2026-06-05).
  - What VIA would actually use is small. As the client it calls `initialize`, `session/new`, `session/prompt`, `session/cancel` and receives `session/update`. It must also answer the agent's requests: `session/request_permission`, `fs/*` and optionally `terminal/*`.
  - ACP publishes a JSON schema, so Go types can be generated and the hand-written part is a JSON-RPC loop of a few hundred lines. The official SDK therefore saves roughly days, not weeks. That estimate is my inference, **UNVERIFIED**.
  - The gap would matter more if ACP's client-side surface changes quickly. It is one adapter out of many.
- **Codex.** Codex being written in Rust helps only if its protocol types can be used as a crate, which is **not confirmed**; I believe the crates live in a monorepo rather than being published (**UNVERIFIED**). `codex app-server generate-json-schema` works just as well for Go or Rust codegen, so this is a wash.
- **Pi, Claude CLI, Copilot and other vendor RPC.** These are hand-written JSONL/JSON-RPC clients either way; Pi's protocol isn't even JSON-RPC 2.0 (README §10). No language has an advantage.
- **Codegen tools.** Go has `go-jsonschema` and `quicktype`; Rust has `typify` and `schemars`. Both are adequate.

### F4. In-process Python binding (PyO3): the strongest Rust argument, and conditional

- PyO3 is mature, and Go's `c-shared`/gopy route has real problems: the Go runtime's signal handlers, fork safety, and one Go runtime per loaded library. If an in-process binding is ever required, Rust wins clearly.
- But this is still undecided, and the settled architecture works against it. VIA runs one worker process per run, cancels by process group, keeps state durably in SQLite, and uses thin SDKs that spawn the binary.
- An in-process binding would give the foreman a faster way to *submit* and *query*, while the runs themselves still happen in separate processes. The saving is a few milliseconds of process spawn per call, on runs that take minutes.
- **By first principles, the in-process option has almost no runtime value for VIA.** Its only real benefit would be avoiding a subprocess boundary in the foreman's own error handling. I would not pay Rust's costs as an option premium for an undecided feature.

### F5. AI implementers and AI critics: unmeasured, so I don't weigh it

- **Plausible for Go:** a smaller language with one obvious way to do concurrency, so fewer borrow-checker and async-lifetime loops during implementation.
- **Plausible for Rust:** the compiler catches more mistakes before a critic sees the code (exhaustive `match` on protocol enums, `Send`/`Sync`).
- Both claims are **UNVERIFIED**. The spike in §5 is designed to measure exactly this, because this factor could plausibly outweigh everything above.

### F6. Comparable tools (Codex, Herdr, Goose GDK in Rust)

Weak evidence. These are agents or terminal multiplexers, with rendering work, PTYs and in-process LLM loops. That workload differs from a headless supervisor. No Go comparables were checked, so absence there is not evidence either.

### Net

- C2 leans Go.
- C1 and C3 are roughly equal, with a small Rust edge on ACP.
- Rust's decisive argument (F4) depends on a requirement that the settled architecture already makes nearly worthless.
- The largest single uncertainty (F5) is measurable, so it should be measured rather than argued.

## 4. What would change my mind

- **The owner decides in-process Python embedding is a hard requirement.** That switches to Rust with about 75% confidence.
- **The spike shows Rust with fewer lifecycle defects, or similar time to green** for AI implementers. Then Rust's stronger compile-time checks become free, and I'd switch to Rust.
- **The pure-Go SQLite drivers fail the multi-process WAL test.** Then Go needs cgo, which removes its F1 advantage. With F1 gone, ACP and PyO3 tip the result to Rust.
- **ACP's client-side surface churns fast** (for example, several breaking changes per quarter) and the Go community SDK stalls. That strengthens the case for Rust's official SDK.
- **Windows becomes first-class in v0.** Neither language gains or loses much here, but it raises the value of Go's trivial cross-compiling. That reinforces Go.

## 5. Concrete first step: a paired, timeboxed spike (about one day of AI execution)

Build **the same thin slice in both languages** from one shared brief and one shared black-box test suite. The suite is written first, in Python, and drives the binary.

**Slice:**
- `via spawn --json` launches a detached worker (the binary re-executing itself) and writes a launch receipt to SQLite before dispatch.
- The worker runs a **fake agent**, a script that:
  - emits noisy JSONL, including one 10 MB line;
  - spawns a grandchild that inherits stdout and sleeps;
  - optionally ignores SIGTERM.
- The worker writes the raw log verbatim and parsed events to SQLite.
- `via cancel` kills the whole process group, escalating SIGTERM then SIGKILL.
- `via serve --stdio` streams events as JSON-RPC notifications.
- One minimal ACP client handshake against a fake ACP peer: Rust uses the official SDK, Go uses types generated from the ACP schema.

**Adversarial checks (pass/fail):**
1. Cancelling with a grandchild alive leaves zero surviving processes and no hung worker.
2. SIGKILL of the worker mid-write leaves the database recoverable and the run in a well-defined state.
3. 32 concurrent runs produce no `SQLITE_BUSY` failures and no lost events.
4. The raw log is byte-identical to what the fake agent emitted.
5. Static binaries cross-compile for linux amd64/arm64, darwin arm64 and windows amd64 in CI, from one Linux host.

**Metrics per language:**
- implementer wall time and token cost to pass the suite;
- number of AI-critic findings per round, and rounds to convergence;
- adversarial checks passed on the first attempt;
- lines of hand-written code for the ACP piece;
- build complexity (toolchain steps needed for five targets).

**Decision rule:**
- Pick Go if it passes all checks within about 1.3× Rust's defect count.
- Pick Rust if Go needs cgo for SQLite, or Rust shows markedly fewer lifecycle defects at similar time to green.