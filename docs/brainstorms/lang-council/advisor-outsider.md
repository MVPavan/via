# VIA: Rust or Go? An outsider's view

## 1. Recommendation

**Lean Go, at about 55–60% confidence. That is barely better than a coin flip, and most of the doubt comes from gaps in the brief, not from either language.**

This lean holds only if the foreman calls VIA through the CLI or a thin SDK, which is what settled decision #1 already implies. If the owner requires the foreman to call VIA in-process through a native binding, switch to **Rust at about 70%**.

## 2. My lens: what a newcomer would stop and ask

Reading the brief, `docs/brainstorms/README.md` and `docs/workstreams/handoff.md` cold, these are the unanswered questions I'd raise before choosing a language:

1. **Who installs VIA, and why does a static binary matter?** The brief calls static distribution "important" but never says for whom. Everything in the repo points to one owner on Linux/WSL2, and the main consumer (the foreman) is Python, so it already needs a Python environment. A static binary pays off if VIA ships to other people or runs inside agent sandboxes and CI images. If it's for the owner's machines, `uv tool install` was enough, and the earlier council had already chosen Python. Please state the audience. It is the premise behind this whole question.
2. **Which platforms?** Windows scope is listed as unknown. That changes the cost of cancel (Job Objects vs process groups) and cross-compiling. macOS isn't mentioned at all. A newcomer would want the target list written down.
3. **The in-process question contradicts itself.** Settled decision #1 says other languages use thin SDKs that spawn the binary. README §1 says "Foreman calls VIA" should mean sharing a library, not shelling out (README.md:33). The owner lists it as undecided. Only one of these can hold. **This is the biggest single factor in Rust vs Go**, and it's open.
4. **What happens to the Python code that already works?** The handoff lists working profiles, inspector, model catalog and ledger (`workflow_interpreter/profiles/`, `inspector/`, `foreman/model_catalog.py`; ADR 0006 "ledger-only record store"). Rust and Go both mean rewriting it. That leaves open questions:
   - Does the foreman keep its own crew path until VIA reaches parity? Then two implementations drift.
   - VIA's SQLite store next to the interpreter's ledger means two writers or two schemas. Who owns what (ADR 0006)?
   
   The brief treats this as settled. An outsider sees a migration cost the brief never mentions.
5. **What do "tens of concurrent runs" and "one worker process per run" add up to?** If every run gets its own OS process, no single process handles much concurrency. Each worker supervises one child, pumps a few pipes and writes to SQLite. That largely removes the concern about how hard tokio cancellation is: a Rust worker can run on blocking threads with no async runtime at all. **The workload is simple in both languages.** The hard parts are the process tree, crash recovery and contract drift, and those are language-neutral.
6. **Who reads this code when AI critics disagree?** If the owner is the final judge, their fluency in each language matters. The brief doesn't say.

## 3. Deciding factors

| Factor | Favours | Evidence / status |
|---|---|---|
| Static binary with SQLite | **Go, slightly** | Go with `CGO_ENABLED=0` and a pure-Go SQLite driver (`modernc.org/sqlite`) cross-compiles with no C toolchain. Rust's `rusqlite` "bundled" compiles SQLite's C source, so musl or cross-target builds need a C cross-compiler (usually `cross` or `cargo-zigbuild`). Both are well-trodden. Go's path has fewer moving parts. I'm confident about this from general knowledge but didn't re-check it this session. |
| Process groups and cancel | Neutral | Brief: Go's `exec.CommandContext` kills only the direct child. Rust's `std` has the same limitation. Both need `setpgid` and `kill(-pgid)`, plus manual Windows Job Objects. Rust has had `CommandExt::process_group` in std since 1.64; Go uses `SysProcAttr{Setpgid:true}`. The earlier council (README §10) reached the same neutral conclusion. |
| ACP adapter | **Rust, weakly at first** | The official SDK and the spec are in Rust (verified this session). Go has only `coder/acp-go-sdk`: v0.13.5, last push 2026-06-05, so about 3.5 months stale. But ACP is a **v1** feature (README §9), and a client needs only a few methods (`session/new`, `prompt`, `update`, `cancel`, `load`). Types can be generated from the published schema. The spec is at v2.0.0-alpha. A Rust SDK tracks v2 for free; Go means rerunning codegen and handwriting behaviour changes. |
| Codex app-server | Neutral in v1 | `generate-json-schema` works for either language. Whether Codex's Rust protocol types can be used as a crate is unconfirmed. Mid-turn steer, the main reason for app-server, is ranked "not very important". |
| Pi RPC | Neutral | It's JSONL and not JSON-RPC 2.0 (README §10), so it needs a handwritten client in either language. |
| Native Python binding | **Rust, strongly, only if required** | PyO3 is mature. Go `c-shared`/gopy problems are real (brief: signal handlers, fork safety, one runtime per library). The foreman forks vendor processes, so fork safety inside a Go-loaded `.so` is a real risk here. It isn't theoretical. |
| AI implementer throughput | Unknown; my prior leans Go | UNVERIFIED and unmeasured. A plausible prior: Go's small surface and fast compiles mean fewer lifetime and async-trait detours per edit loop. Rust's compiler catches more defects before the critic sees them. Neither has been measured on this team. |
| Comparable tools | Rust, weakly | Codex, Herdr and Goose's GDK are Rust. Antigravity CLI (`agy`) is written in Go (README §5). No Go tool like VIA was checked, so this is a gap in the search, not evidence against Go. |
| Copilot SDK | Neutral | It ships official Go and Rust SDKs (README §11 table). |

**How I weigh it.** The settled architecture (binary plus JSON-RPC stdio plus thin generated SDKs) is the design Go is best at: plain subprocesses, JSON, a static cgo-free binary. Rust's clear advantages are the official ACP SDK (needed in v1, and codegen makes up for much of it) and PyO3, which the settled architecture seems to have designed out. If that holds, Go wins by a small margin on the stated priority (static distribution) and on simplicity. If the in-process binding comes back, Rust wins decisively, because Go-in-Python is the one real trap on this list.

## 4. What would change my mind

- **Owner requires in-process foreman calls** → Rust.
- **The v1 ACP adapter needs more than codegen can give.** Examples: bidirectional permission requests, `_session/*` extensions, v2 transport changes. Or a Go ACP spike takes over 2× the Rust equivalent. → Rust.
- **Windows becomes a first-release target.** Neutral on process control, but check Go's pure-Go SQLite on Windows against Rust's MSVC toolchain before deciding.
- **A measured AI build-rate gap** in either direction from the prototype below.
- **The static-binary audience turns out to be the owner's machines only.** Then reopen whether leaving Python was justified at all. The earlier council chose Python, and ignoring the answer would be sunk-cost reasoning.

## 5. First step

**Before any prototype**, the owner answers three questions in one line each:
- Who installs VIA?
- Which operating systems?
- In-process binding: yes or no?

A "no" to the binding question alone may settle this.

**Then a twin-spike prototype, with each language getting the same brief:**
- Build the command `via spawn --background <fake-cli>` → `via wait`/`cancel`.
- It covers:
  - a launch receipt written to SQLite (WAL) before dispatch
  - one detached worker per run
  - a verbatim raw log
  - cancel sent to a process group where the fake CLI spawns a sleeping grandchild
  - one ACP `session/new` + `session/prompt` + `session/cancel` against a fake ACP peer
    - Rust uses the official SDK; Go uses `acp-go-sdk` or schema codegen
- Build it as a static Linux `x86_64` + `aarch64` binary.

**Measure it, with the same implementer model and effort for both languages:**
1. Wall-clock time and number of implementer rounds to green.
2. How many defects the critic finds, and how serious they are.
3. After cancel: surviving processes, zero required.
4. 32 concurrent runs with noisy output: lost log bytes and any `SQLITE_BUSY` failures.
5. Binary size and time to set up the cross-build.
6. Hours to adopt an ACP schema bump (simulate one field change).

The prototype is disposable, so write it in gitignored `scratchpad/`. Pick the winner on metrics 1, 2 and 6. Metrics 3–5 should pass in both languages. If they don't, that failure tells you more than the language choice does.