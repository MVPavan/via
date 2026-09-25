# VIA: Rust or Go? The Executor's view

## 1. Recommendation

**Go, with moderate-low confidence (about 60/40).** The confidence ceiling is low for one reason: the biggest executor variable is how fast AI implementers build this, and at what defect rate, in each language. Nobody has measured that. A two-day paired prototype (§5) can settle it, and I would run it before committing, not after.

This lean depends on the Python-foreman question staying open. If the foreman must call VIA in-process through a native binding, **switch to Rust** (see §4).

## 2. The Executor lens applied

The question I asked was what gets v0 working, meaning `via spawn/wait/status/cancel --json` over `claude -p` and `codex exec --json` with SQLite and receipts, and then v1 (vendor RPC plus generic ACP), soonest and with the fewest stuck loops for AI implementers and critics.

v0 is almost entirely **process supervision, line-delimited JSON parsing, SQLite writes and a CLI**. No protocol SDK is on the v0 path, so Rust's main advantage (the official ACP SDK) doesn't matter until v1. v0's risks are lifecycle risks: process groups, a crash between the receipt and dispatch, draining pipes while writing to the DB, and cancelling a grandchild. Those are the areas where the implementation language shapes the size of the bug surface.

**What cuts against my own lean:** at v1, Rust has the stronger protocol ecosystem. ACP's official SDK is Rust, and a Codex app-server protocol crate exists (§3). An executor can't dismiss that, because v1 is roughly half the product.

## 3. Deciding factors

**A. v0 feasibility is roughly equal, and Go has a slight edge in the lifecycle core.**
- **Go:** `os/exec`, `bufio.Scanner`, goroutines per pipe and `context` map directly onto "one worker per run, drain stdout/stderr, cancel". You still set `SysProcAttr{Setpgid: true}` and call `syscall.Kill(-pgid, SIGTERM)` yourself, because `CommandContext` kills only the direct child (verified in the brief). SQLite is `modernc.org/sqlite` (pure Go, keeps `CGO_ENABLED=0` static builds) or `mattn/go-sqlite3` (cgo). *UNVERIFIED this session:* the WAL and concurrency behaviour of `modernc` under tens of writers.
- **Rust:** `tokio::process` plus `process_group(0)` (from `std::os::unix::process::CommandExt`) and `nix::killpg`, with `rusqlite` using its bundled SQLite. Every piece exists. The usual friction is a blocking SQLite call inside async code (`spawn_blocking`), cancel-safety of `select!` arms, and `Send`/lifetime errors across tasks. These are exactly the "how hard is tokio cancellation" uncertainty the brief flags. **UNMEASURED.**
- **Executor judgement:** Go's cancel-and-drain model has fewer ways to go subtly wrong (a dropped future mid-write, for example). Rust's compiler catches more data races. For tens of concurrent runs, the Go approach is enough. This is inference, not measurement.

**B. Static binary: tie, with a small Go edge in cross-compiling.**
- Go: `CGO_ENABLED=0 GOOS=… GOARCH=…` produces a static binary on any host with no extra toolchain, provided the SQLite driver is pure Go.
- Rust: static linux-musl builds are routine but need the musl target, and `cross` or `cargo-zigbuild` for other targets. Bundled SQLite compiles C, so cross-compiling needs a C cross toolchain. This is solvable in both languages, with more CI setup for Rust.

**C. v1 protocol clients favour Rust.**
- ACP: official Rust SDK versus the community `coder/acp-go-sdk` v0.13.5 (last pushed 2026-06-05, from the brief). The Go fallback is generating types from ACP's published JSON schema and writing about 300–600 lines of JSON-RPC-over-stdio client. *Estimate, UNVERIFIED.*
- Codex app-server: a `codex-app-server-protocol` crate exists on crates.io, at v0.63.0 per docs.rs ([crates.io](https://crates.io/crates/codex-app-server-protocol), [docs.rs](https://docs.rs/crate/codex-app-server-protocol/0.63.0/source/)). The search listing names the publisher as "Felipe Rosa". **UNVERIFIED** whether it is an official OpenAI publish, and whether its version tracks the installed `codex` CLI. Pinning adapters to vendor CLI versions (settled decision 4) favours types generated from `codex app-server generate-json-schema` for the installed CLI in *either* language. That shrinks Rust's advantage here.
- Pi RPC is non-JSON-RPC JSONL (see the README), so it's hand-written in both languages.
- Net: Rust saves perhaps one adapter's worth of client code at v1 (ACP). Contract tests are needed per vendor version regardless.

**D. AI implementers and critics: UNMEASURED; this is the swing factor.**
- In principle, Go's small language surface makes AI critic review cheaper (fewer idioms, and `go vet`/`-race` catch concurrency bugs at test time), while Rust makes the compiler an extra critic. Both claims are plausible and neither is measured for this repo's roster. I would not decide on this without the experiment.

**E. Environment fact from this session:** neither `go` nor `cargo`/`rustc` is on PATH here. Day one of either option includes installing a toolchain; this doesn't favour either.

**F. Python in-process binding: Rust only, realistically.** PyO3 plus maturin is mature. Go's `c-shared` approach carries signal-handler and fork-safety issues (from the brief). But the settled architecture already provides a thin Python SDK that spawns the binary, and a JSON-RPC `serve --stdio` makes that SDK nearly free. The in-process need is speculative today.

## 4. What would change my mind (towards Rust)

1. **The owner decides the foreman needs an in-process native binding.** Then Rust, at about 75%. Retrofitting PyO3 onto a Go codebase isn't possible.
2. **The paired prototype (§5) shows Rust at or near parity** in AI build time and critic-found defects for the lifecycle core. Rust's v1 ecosystem advantage then decides it.
3. **The Codex protocol crate turns out to be official and version-locked to the CLI**, and/or `acp-go-sdk` has fallen further behind the ACP spec version VIA targets.
4. **Windows becomes in scope for v1.** Both languages need manual job-object work. I don't know which is easier (UNVERIFIED), so this would trigger a re-check rather than a flip.

What would *raise* confidence in Go: the prototype shows Rust with at least 1.5× wall-clock or at least 2× critic-found lifecycle defects, and ACP-from-schema in Go comes in under a day.

## 5. Concrete first step: a paired two-day prototype

Build the **same throwaway worker core** in both languages, in parallel, each in its own gitignored `scratchpad/` worktree. Use the same AI implementer (the roster's implementer model), the same brief, and the same review cascade. The scope is exactly v0's hard part:

- `via spawn --json <fake-cli>` writes a launch receipt to SQLite (WAL) **before** exec. It spawns one worker per run in a new process group and stores the raw stdout/stderr verbatim. It parses JSONL events into a result envelope.
- `via cancel <id>` kills the process group and must reach a **grandchild** started by the fake CLI.
- `via status/wait --json`.
- Fake peers: a JSONL emitter with noisy, oversized (over 1 MB) lines; one that crashes mid-run; one that forks a grandchild and ignores SIGTERM for 2 s.
- Stretch (day 2): a minimal ACP client against one real ACP agent. In Rust use the official SDK; in Go use `acp-go-sdk` *or* schema-generated types. Record which Go path works.

**Measure:**

| Metric | How |
|---|---|
| AI wall-clock to green | from brief dispatch to all acceptance tests passing |
| Implementer rounds / stuck loops | count of fix rounds and repeated failures with no new evidence |
| Critic-found defects | the same critic prompt against both diffs, classified lifecycle / concurrency / other |
| Correctness under load | 1 / 8 / 32 concurrent runs: no lost receipts, no orphaned processes (`ps` after cancel), no pipe deadlock, no `SQLITE_BUSY` failures |
| Static artifact | `CGO_ENABLED=0` (Go) or the musl target (Rust): `ldd` reports not dynamic; binary size; cross-build to linux-arm64 and darwin-arm64 |
| ACP stretch | hours to a working prompt → result round trip |

**Decision rule:** choose Go if it's faster by at least 1.3× with no more lifecycle defects than Rust and the ACP stretch works. Otherwise choose Rust. The owner's in-process-binding answer overrides the rule in either direction.

This reuses the judge's earlier one-day Python prototype design (README §10), retargeted at the two compiled candidates. The same fake peers carry over, so little design work is wasted.

**Sources**
- [codex-app-server-protocol on crates.io](https://crates.io/crates/codex-app-server-protocol)
- [codex-app-server-protocol 0.63.0 on docs.rs](https://docs.rs/crate/codex-app-server-protocol/0.63.0/source/)
- [codex-app-server-protocol on lib.rs](https://lib.rs/crates/codex-app-server-protocol)
- Repo: `docs/brainstorms/README.md` §9–§12