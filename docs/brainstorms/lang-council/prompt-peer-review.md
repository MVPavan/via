You are the PEER REVIEWER in a perspective council. Read-only: do not modify any files. Five advisors answered the brief below independently, each through a different lens. Their responses are anonymised as A–E in random order. Review them as a set.

For EACH response: its strongest point, its weakest or unsupported claim (check facts you doubt with web search or the repo), and what it missed. Then across all five: where they agree, where they genuinely conflict, errors shared by several, and the most important gap none covered. Do not pick a winner by vote count. Markdown, ≤ 120 lines.

===== BRIEF =====
# Perspective council brief — VIA: Rust or Go?

You are one advisor in a perspective council. Other advisors answer the same brief through
different lenses; you will not see their answers. Work read-only: do not modify, create or delete
any file. You may read files in this repository and use web search to check facts.

## Question

Should VIA be built in **Rust** or **Go**? (Python is excluded by the static-binary priority.)

## What VIA is

A tool that runs a role + prompt on many coding-agent harnesses (Claude Code, Codex, OpenCode,
Copilot, Pi, Gemini, Cline, Kilo, Droid, …) and returns a structured result envelope. Background:
`docs/brainstorms/README.md` (§9–§12) and `docs/brainstorms/access-methods.md`.

## Decisions already settled (treat as facts, not up for debate)

1. **Architecture.** One static binary is the whole product, with two first-class interfaces:
   the CLI (`via spawn / wait / status / cancel --json`) and `via serve --stdio` (JSON-RPC 2.0
   with streamed events). Other languages use thin SDKs (Python first) that spawn the binary,
   with types generated from VIA's JSON schema — the Codex / Copilot SDK pattern. Optional MCP
   front end later.
2. **Harness routes.** Headless CLI (v0: `claude -p`, `codex exec --json`); vendor RPC in v1
   (Codex app-server JSON-RPC, Pi RPC JSONL, others); one generic ACP (Agent Client Protocol,
   JSON-RPC over stdio) adapter for breadth. No in-process vendor SDKs.
3. **Verbs.** spawn, resume, steer, cancel, status, result, plus background runs and wait. Each
   adapter declares supported verbs and refuses the rest by name.
4. **Required properties.** Durable SQLite run store; a launch receipt written before dispatch;
   VIA run id separate from the vendor session id; one worker process per run; raw logs kept
   verbatim; cancel that reaches the whole process group; adapters pinned and contract-tested per
   vendor CLI version; many concurrent runs (tens).
5. **Design rule.** VIA runs only vendor binaries in their documented headless modes and never
   reuses vendor credentials.

## Owner priorities

- Static binary distribution: **important**.
- Mid-turn steering: **not very important**.
- Whether the Python workflow-interpreter foreman calls VIA in-process (native binding) or via
  the CLI / thin SDK: **undecided**.

## Facts verified in this session

- ACP's official SDK and the spec repository are Rust. Go has only a community SDK,
  `coder/acp-go-sdk` (v0.13.5, last pushed 2026-06-05). ACP publishes a JSON schema from which
  Go types could be generated.
- Codex is written in Rust; `codex app-server generate-json-schema` exists. Whether Codex's Rust
  protocol types are usable directly as a crate is NOT confirmed.
- Comparable Rust tools: Codex, Herdr (terminal agent multiplexer), Goose's GDK. Comparable Go
  tools: none checked.
- Native Python binding: PyO3 (Rust) is mature. Go via cgo `c-shared` or gopy has known issues:
  Go runtime signal handlers, fork safety, one Go runtime per loaded library.
- Go's `exec.CommandContext` kills only the direct child. Process-tree cleanup and Windows job
  objects are manual work in both languages.
- Most code will be written by AI implementers (Claude and GPT models) and reviewed by AI critics.

## Uncertainties — mark, do not assume

Build speed and defect rate of AI implementers in each language (unmeasured); how hard tokio
cancellation is for this workload in practice; how far the Go ACP ecosystem lags; Windows scope.

## Output (markdown, ≤ 150 lines)

1. **Recommendation** (Rust or Go) with a calibrated confidence level.
2. **Your lens applied explicitly** (see below).
3. **Deciding factors** with evidence and citations; mark UNVERIFIED claims.
4. **What would change your mind.**
5. **A concrete first step** — e.g. a small prototype and how to measure it.

Be evidence-led. Do not perform your lens at the expense of accuracy: if the evidence cuts
against the lens's natural direction, say so.

## Your lens
(Each advisor also received one lens instruction, withheld here.)

===== RESPONSE A =====

## 1. Recommendation

**Go, with moderate-low confidence (about 60/40).** The confidence ceiling is low for one reason: the biggest executor variable is how fast AI implementers build this, and at what defect rate, in each language. Nobody has measured that. A two-day paired prototype (§5) can settle it, and I would run it before committing, not after.

This lean depends on the Python-foreman question staying open. If the foreman must call VIA in-process through a native binding, **switch to Rust** (see §4).


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

===== RESPONSE B =====


## 1. Recommendation

**Rust, confidence ~65%.** Most of VIA's possible growth sits in Rust: the reference protocol code (ACP), the source language of the most important harness (Codex), a mature way to embed in Python (PyO3), and a group of similar tools. Go's advantage is real but lies elsewhere. It gets to a working v0 faster and more cheaply, and v0 (`claude -p`, `codex exec --json`, SQLite, process groups) uses almost none of Rust's extra reach.

## 2. My lens applied: which upside does each language unlock?

| Growth path | Rust | Go |
|---|---|---|
| **ACP breadth adapter** (about 10 harnesses in v1) | Official SDK: [agentclientprotocol/rust-sdk](https://github.com/agentclientprotocol/rust-sdk), which also powers Zed's external-agent support. It ships client, agent and proxy crates plus a "conductor" for proxy chains ([mdbook](https://agentclientprotocol.github.io/rust-sdk/)). | Community `coder/acp-go-sdk` (v0.13.5, last pushed 2026-06-05), or types generated from the JSON schema. |
| **VIA as an ACP *agent* or *proxy*, not only a client** | The official SDK already models proxies and conductors, so this would be a small step. | You would build it yourself. |
| **Codex app-server types** | A `codex-app-server-protocol` crate exists on crates.io ([crates.io](https://crates.io/crates/codex-app-server-protocol)), but it appears to be **republished by a third party**, not OpenAI ([lib.rs](https://lib.rs/crates/codex-app-server-protocol)). A git dependency on `openai/codex` is the likely first-party route. **UNVERIFIED** whether it builds cleanly outside the Codex workspace. | Generate the types with `codex app-server generate-json-schema`. This works fine, and it is what Rust would also fall back to. |
| **Python foreman calling VIA in-process** | PyO3 + maturin: mature, and ships abi3 wheels. It keeps this undecided option open cheaply. | cgo `c-shared` has problems with signals, fork and a single runtime per library. In practice the choice is limited to CLI/SDK. |
| **Other embeddings** (Node through napi-rs, WASM for schema validation, a C ABI) | All realistic. | Weak. |
| **Community fit and code to borrow** | Codex, Herdr and Goose GDK are all Rust, so patterns and contributors can move between them. | No comparable Go tools were checked. **UNVERIFIED** either way. |
| **Copilot SDK** | A Rust SDK exists (README §11). | A Go SDK exists. This is a wash. |
| **Performance headroom** | Not decisive. With tens of runs, both languages are bottlenecked on the vendor CLIs. | Same. |

**Where the evidence cuts against simply picking the richer ecosystem:**
- **VIA's settled design reduces the value of library reuse.** It talks over the wire, has no in-process vendor SDKs, and generates types from a schema. Any ecosystem advantage therefore mostly shrinks to "we get typed ACP/Codex structs for free." Go can generate the same types from the published schemas.
- **The Python-embedding upside is only an option.** The owner hasn't decided whether the foreman calls VIA in-process. The thin-SDK pattern Codex and Copilot use is already the settled architecture, and it works equally well from Go. So PyO3 is worth something, but only as insurance.
- **Neither language removes process-tree cleanup work** (verified this session and in the earlier council ruling). Rust doesn't win here.

## 3. Deciding factors

1. **ACP is VIA's breadth strategy, and its reference implementation is Rust (verified).** When the spec changes, the official SDK changes with it. Go's SDK is maintained by one company and lags by an amount nobody has measured (**UNVERIFIED**, listed as an open uncertainty). Every harness routed through ACP inherits that lag.
2. **Rust keeps more future surfaces open.** It supports a PyO3 module, an ACP proxy mode (VIA inserted between an editor and an agent) and Node bindings. None of these is required today, and each is cheap in Rust but expensive or impractical in Go. My lens values keeping options open, but I'm counting them as *options*, not requirements.
3. **Go's real counter-upside is speed of getting started, and I'm scoring it honestly:**
   - Goroutines plus `os/exec` fit a "one worker process per run, stream stdout, wait, kill" design very closely.
   - A pure-Go static binary with `modernc.org/sqlite` needs no cgo. Rust with `rusqlite` bundles C SQLite, so a musl static build needs a C toolchain. Both work, but Go is simpler.
   - Faster compiles mean cheaper iteration loops for AI implementers.
4. **AI implementer productivity is unmeasured (UNVERIFIED).** My inference: Go code from LLMs compiles first time more often. Rust's borrow checker and async lifetimes cost extra fix rounds, but they catch concurrency bugs that Go only exposes at runtime. For a durable run store with cancellation, that trade may favour Rust once defects are counted, not just build speed.
5. **Tokio cancellation risk (UNVERIFIED).** Dropping a future mid-write can leave the run store or a receipt half-updated. This is manageable with `CancellationToken` and explicit shutdown, but it is the main way Rust could fail here.

## 4. What would change my mind (towards Go)

- **The owner rules out in-process embedding for good**, and there are no plans for a proxy or agent mode. Most of Rust's option value then disappears. Go becomes roughly 55/45 favoured on build speed.
- **An ACP compatibility check goes well for Go:** `coder/acp-go-sdk` or schema-generated Go types handle the current ACP schema, including the latest session and permission messages, against 3 or more real agents with little adaptation code.
- **The prototype below shows Rust costing ≥2× the AI fix rounds with no fewer defects.**
- **Windows becomes a hard first-release target.** Neither language wins outright, but Go's single-toolchain cross-compile is simpler.

## 5. Concrete first step: a paired spike (roughly half a day of AI time per language)

Build the **same thin slice twice**, in Rust (tokio + rusqlite bundled + official ACP crate) and in Go (`os/exec` + modernc sqlite + acp-go-sdk):

1. `via spawn --json` writes a launch receipt to SQLite, then starts a worker in a new process group. The worker runs a **fake harness that spawns a grandchild**, prints noisy output, then hangs.
2. `via cancel` kills the whole group. Check with `ps` that no orphans remain.
3. An ACP client session against one real ACP agent (e.g. OpenCode or Goose) runs initialize → prompt → cancel.
4. 32 concurrent runs, with the database under contention.
5. **Rust only:** a 20-line PyO3 `via.spawn()` wrapper, to price the embedding option.

**Measure:**
- Implementer fix rounds until the gates pass.
- Critic findings split into blocking and minor.
- Orphan count after cancel (target: 0).
- Number of receipt/state inconsistencies after 50 `kill -9` injections.
- Static binary size and whether it runs on a clean Alpine container.
- Lines of adapter code needed for ACP in each language.

Pick Rust unless Go wins on fix rounds by a wide margin *and* its ACP result shows no schema gaps.

Sources:
- [agentclientprotocol/rust-sdk (GitHub)](https://github.com/agentclientprotocol/rust-sdk)
- [ACP Rust SDK mdbook](https://agentclientprotocol.github.io/rust-sdk/)
- [agent_client_protocol on docs.rs](https://docs.rs/agent-client-protocol)
- [codex-app-server-protocol on crates.io](https://crates.io/crates/codex-app-server-protocol)
- [codex-app-server-protocol on lib.rs](https://lib.rs/crates/codex-app-server-protocol)
- Repo: `docs/brainstorms/README.md` §9–§12

===== RESPONSE C =====

## 1. Recommendation

**Go, with low confidence (about 55/45).** It is close enough that a two-day prototype in each language should decide it, not this memo. I'd hold this lean only if the owner settles one open question now: the foreman talks to VIA through the CLI or thin SDK, not a native binding (see D1). If the owner wants the native binding kept open, switch to **Rust at about 60%**.

Most of the brief's facts point toward Rust. My lens says to test those facts first, and several of them matter less for this workload than they seem to.


VIA's hard parts are:
- supervising tens of subprocesses
- draining pipes without deadlock
- cancelling whole process groups
- writing launch receipts and SQLite state crash-safely
- keeping adapters stable while vendor CLIs change

Its easy parts are CPU work and memory safety, which is where Rust is strongest. Model latency dominates everything else, so neither language wins on speed. The question is which language's failure modes sit on VIA's critical path. For Rust, the risk sits in lifecycle and cancellation code. For Go, it sits in protocol typing and a thinner ACP ecosystem.

## 3. Deciding factors

### D1. The native Python binding is the weakest pro-Rust argument (strong)

PyO3 is Rust's only advantage that Go cannot match. But putting VIA in the foreman's process conflicts with the settled architecture:
- one worker process per run
- durable receipts
- process-group cancel
- the SDK pattern of spawning the binary (Codex, Copilot)

An in-process VIA brings a tokio runtime, signal handling and child reaping into the foreman. That creates the same class of fork, signal and reaping hazards the brief lists against Go's cgo, though usually less severe. A Rust panic across FFI, or a fork from Python while tokio is running, becomes a foreman crash. This is my inference, not measured. **If the thin SDK is chosen, as the architecture already implies, Rust's biggest unique advantage disappears.** The brief marks this "undecided"; I'd decide it now.

### D2. "The official ACP SDK is Rust" matters less than it looks (moderate)

- ACP is JSON-RPC 2.0 over stdio, and a JSON schema is published. The protocol client is small; the hard parts (lifecycle, permissions, cancel scope) belong to VIA whichever language it uses.
- Over 6–18 months the bigger risk is spec churn, not SDK absence. Pinning the official crate means following its breaking changes. I have not checked its release history, so this is **UNVERIFIED**. Generating types from a pinned schema version works in both languages and suits VIA's per-vendor contract testing better.
- The Go risk is real: `coder/acp-go-sdk` is community-maintained, pre-1.0, and hasn't been pushed since 2026-06-05. **Don't depend on it.** Generate from the schema and treat the SDK as reference code only.

### D3. Rust's structural advantage: typed protocol unions (moderate, the best pro-Rust point)

VIA handles many vendor event streams with tagged unions, optional versus absent fields, and version drift. Serde's tagged enums and `Option<T>` model this cleanly. Go has no sum types, and `encoding/json` treats "absent" and "zero" the same unless every field is a pointer. Schema-generated Go types for `oneOf` unions come out as `json.RawMessage` with hand-written dispatch.

That is where silent contract bugs will hide in Go around month 12, when a vendor adds a variant. The cost is ongoing, not one-time. The mitigation is strict decode tests per pinned vendor version, which VIA needs anyway. Of everything here, this is what most makes me doubt Go.

### D4. Rust's failure mode: async cancellation on the critical path (moderate, uncertain)

Dropped futures in `select!` are the problem. It isn't hard to write correctly; it's hard to *review*. These are my predictions for months 6–18:
- a future dropped mid-way through a receipt write or log flush
- blocking `rusqlite` calls stalling the tokio executor unless every one goes through `spawn_blocking` or a dedicated DB thread
- `Child` handles lost on an error path, because `kill_on_drop` kills only the direct child, not the group
- AI implementers working around the borrow checker with `Arc<Mutex<_>>` everywhere, which reintroduces lock-ordering bugs the compiler won't catch

The brief lists "how hard tokio cancellation is in practice" as unmeasured, and I agree. That is exactly why it shouldn't be assumed cheap. A mitigation that makes this mostly go away: a synchronous design with one OS thread per run and a DB writer thread. Tens of runs doesn't need async.

### D5. Go's failure mode: goroutine and pipe leaks (moderate, well understood)

- `exec.CommandContext` kills only the direct child (verified in the brief). Go 1.20 added `Cmd.Cancel` and `Cmd.WaitDelay`, which let you send the kill to the group (`Setpgid` plus `kill(-pgid)`) and stop `Wait` hanging when a grandchild keeps the pipe open. Both languages need this work: Rust has `process_group()` in std since 1.64. **This is not a real differentiator.**
- Goroutine leaks from undrained stdout/stderr are common but easy to find with `goleak` and `pprof`. Go's failures here are loud and diagnosable. Rust's cancellation bugs tend to be silent.

### D6. Static binaries have a hidden SQLite cost in both languages (moderate)

- **Go:** `mattn/go-sqlite3` needs cgo, which breaks easy cross-compilation. Go stays static only with a pure-Go driver: `modernc.org/sqlite` (transpiled, slower, **UNVERIFIED** by how much) or `ncruces/go-sqlite3` (wasm). Either is fine at tens of runs, but it is a dependency choice to test, including WAL behaviour and busy-timeout semantics.
- **Rust:** `rusqlite` with the bundled feature compiles C SQLite. Static musl builds for Linux, macOS and Windows need a C cross toolchain (`cross` or `cargo-zigbuild`), so the release pipeline is heavier.
- Neither language gives a fully static macOS binary; both link libSystem. So "static binary" really means "no runtime to install", and both deliver that.

### D7. AI implementer throughput is unmeasured, and the evidence cuts both ways (speculation)

- **For Go:** faster compiles make the fix-and-verify loop tighter, and Go's plain style is easier for AI critics to review.
- **For Rust:** the compiler catches a class of defects that would otherwise depend on reviewers. With AI reviewers, that could matter more than it does for human teams.

No data either way. **Don't let either side claim this.**

### D8. Codex types as a Rust crate: don't count it (strong)

The brief marks this unconfirmed. My understanding is that Codex's protocol crates live in its monorepo and are not a stable published API (**UNVERIFIED**). Depending on them through a git pin would tie VIA to Codex's internal refactors. Use `generate-json-schema` in either language, which makes this a wash.

### D9. Comparable tools: survivorship, not evidence (weak)

Codex, Herdr and Goose being in Rust shows Rust can build this; it says nothing about cost or defect rate. "No Go tools checked" means none were looked for, not that none exist. Discount the whole factor.

### Weakest assumptions in the brief, ranked

1. The native binding is still an option (D1).
2. The official ACP SDK is a durable advantage (D2).
3. "Tens of runs" needs async at all (D4). With threads, Rust's biggest risk shrinks.
4. The Codex crate is reusable (D8).

## 4. What would change my mind

**Toward Rust:**
- The owner wants an in-process foreman binding.
- The Go prototype's union and absent-field decoding needs more than about 150 lines of hand-written dispatch per protocol, or misses a planted variant change.
- The ACP schema turns out to contain protocol features that generated Go types can't represent cleanly.
- The Rust prototype, written synchronously or with disciplined tokio, passes the cancel and crash suite on its first critic round.

**Toward Go, more strongly:**
- The Rust prototype needs more than one fix round for cancellation or blocking-DB bugs.
- It needs `unsafe` beyond Windows job objects.
- AI implementer turnaround is clearly slower (wall time to a green suite).
- The ACP crate turns out to have frequent breaking releases.

**Either way:** Windows becoming a first-class target adds job-object work in both languages. It is not a differentiator, but it does raise the value of whichever prototype handled it more cleanly.

## 5. Concrete first step: a paired bake-off

Build the same slice in both languages. Give each to the same AI implementer and critic roster, with the same brief and the same test harness. The harness should be language-neutral: a Python pytest suite driving the binary.

**Slice:**
- `via spawn / wait / status / cancel --json`
- SQLite WAL run store with the receipt written before dispatch
- one worker process per run
- a fake CLI peer that emits noisy JSONL and oversized records
- a fake ACP peer generated from the pinned schema, not the SDK, that emits an unknown union variant

**Measure:**
1. **Correctness under faults:** run at 1, 8 and 32 concurrent runs. Inject these faults:
   - cancel a child that has a grandchild holding the pipe open
   - `kill -9` the worker mid-write
   - lock the DB for 5 seconds
   - send an unknown event variant

   Pass means no orphaned processes (check `ps`), no lost receipts, raw logs byte-exact, and the unknown variant surfaced rather than dropped.
2. **Effort:** implementer and critic rounds to green, wall time, and lines of hand-written protocol code.
3. **Release:** cross-compile static artefacts for linux-amd64, linux-arm64 and darwin-arm64 in CI; measure setup effort and binary size.
4. **Leaks:** run 1,000 spawn/cancel cycles, then compare goroutine or task counts and file-descriptor counts.

**Decision rule:** pick the language with fewer fault-suite failures after one critic round. If they tie, pick the one with fewer fix rounds. Treat measured cancel and crash correctness as decisive, ahead of every factor in section 3.

===== RESPONSE D =====

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

===== RESPONSE E =====

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
