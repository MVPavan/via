# Contrarian advisor: VIA in Rust or Go?

## 1. Recommendation

**Go, with low confidence (about 55/45).** It is close enough that a two-day prototype in each language should decide it, not this memo. I'd hold this lean only if the owner settles one open question now: the foreman talks to VIA through the CLI or thin SDK, not a native binding (see D1). If the owner wants the native binding kept open, switch to **Rust at about 60%**.

Most of the brief's facts point toward Rust. My lens says to test those facts first, and several of them matter less for this workload than they seem to.

## 2. The contrarian lens, applied

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