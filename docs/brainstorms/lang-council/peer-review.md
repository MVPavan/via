# Peer review: VIA, Rust or Go (responses A–E)

I checked the repo claims in `docs/brainstorms/README.md`. Neither `go` nor `cargo` is on PATH, which confirms A's environment fact. I did not verify any claims on the web.

## Response A: Go, about 60%

- **Strongest point.** It is the only response that separates v0 from v1. No protocol SDK sits on the v0 critical path, so Rust's ecosystem lead does not matter until v1. It also notes that pinning to a vendor CLI version favours generating types from the installed `codex` binary in either language, which shrinks the value of the Codex crate.
- **Weakest claim.** "Retrofitting PyO3 onto a Go codebase isn't possible" overstates the case. Doing so means a rewrite, and the settled `serve --stdio` path already covers most of that need, as A itself says in F. The "300–600 lines" figure for an ACP client is an unsourced estimate. A does label it.
- **Missed.**
  - The prior council's explicit condition for choosing Go (see the cross-set section).
  - `serve --stdio` is a long-lived process that multiplexes many runs, not a single-run worker.
  - A 1.3× speed threshold measured on one run per language is statistical noise.

## Response B: Rust, about 65%

- **Strongest point.** It treats Rust's extra reach as options and prices them honestly. It explicitly concedes that the settled wire-only design reduces the value of reusing libraries. Its prototype prices the PyO3 option instead of assuming it.
- **Weakest claims.**
  - The row for "other embeddings" (napi-rs, WASM, a C ABI) and the ACP proxy/conductor mode have no present requirement. `AGENTS.md` says to avoid speculative features. Counting them as upside weights the result toward Rust with capabilities nobody has asked for.
  - "About 10 harnesses in v1" has no source.
  - The decision rule is lopsided ("Rust unless Go wins by a wide margin"), so Go carries the burden of proof without a stated reason.
- **Missed.**
  - Rust's Codex crate advantage disappears under version pinning (A and C both caught this).
  - The static-binary cost of bundled C SQLite is conceded but not weighed against the owner's "important" rating.
  - ACP also has official SDKs in other languages: the prior council cites an official Python `agent-client-protocol` (README:230). Being "the reference" is a weaker claim than being "the only official SDK".

## Response C: Go, about 55%

- **Strongest point.** D3 is the best pro-Rust argument in the set, and it comes from a Go-leaning response. Go has no sum types and `encoding/json` treats absent fields and zero values the same. Silent contract bugs across more than 10 drifting vendor schemas are exactly the ongoing cost VIA carries. C also turns this into a testable prototype check: a planted unknown variant, and a threshold of about 150 lines of hand-written dispatch per protocol.
- **Weakest claim.** C's fix for Rust's biggest risk is "go synchronous, with a thread per run". That conflicts with using the official ACP Rust SDK, which is async. That is likely but UNVERIFIED; check the `agent-client-protocol` crate's runtime requirements. You cannot adopt both the SDK advantage and the no-tokio mitigation without bridging them.
- **Also weak.** C says a native binding "conflicts with the settled architecture". That goes too far: a binding could submit and query while runs stay in separate worker processes, as E notes.
- **Missed.** Pure-Go SQLite behaviour with several processes writing is raised only as "a dependency choice to test". E raised the same point and made it a switch condition.

## Response D: Go, 55–60%

- **Strongest point.** It surfaces an internal contradiction I verified. `README.md:33` says "Foreman calls VIA" should mean sharing a library, not shelling out, while settled decision #1 prescribes thin SDKs that spawn the binary. D is also the only response that asks who the static binary is for. And it raises the migration and ownership cost of the working Python crew layer and the two-store question under ADR 0006. The prior council flagged that ownership issue too (README §10).
- **Weakest claims.**
  - "The spec is at v2.0.0-alpha" is imprecise. The stable schema is v1.23.0, and v2.0.0-alpha.5 is a prerelease (README:49).
  - Proposing to reopen Python pushes against the brief's settled exclusion. It is fair to flag, but out of scope as a recommendation.
- **Missed.** The same point about the `serve` process as A. D's own claim that each worker "doesn't handle much concurrency" overlooks it.

## Response E: Go, 60–65%

- **Strongest point.**
  - It is the clearest separation of binding from incidental constraints.
  - It gives the most concrete account of ACP's client surface. As the client, VIA must also answer `request_permission`, `fs/*` and `terminal/*`, and several others missed this.
  - Its switch condition is sharp: if pure-Go SQLite fails the multi-process WAL test, Go needs cgo, loses F1, and Rust wins.
- **Weakest claim.** "The in-process option has almost no runtime value" is argued from spawn latency alone. The README's stated motive is sharing types and run identity (`contracts/run_identity.py`, README:28–33), not saving milliseconds. Schema-generated Python types may cover that motive, but E did not argue it.
- **Also weak.** "Windows first-class reinforces Go" contradicts E's own F2, where job objects are manual work in both languages. It is a cross-compile point only.
- **Missed.** C's point about typed unions and absent fields. E's claim that "codegen tools are adequate" in both languages skips how `oneOf` types come out in Go.

## Across the set

**Agreement.** All five responses agree on the following:
- Process-group and grandchild cleanup does not favour either language.
- The Codex crate is a wash, or unconfirmed.
- PyO3 is the one decisive pro-Rust lever, and it depends on the undecided foreman question.
- Go is simpler for a static, cross-compiled binary if a pure-Go SQLite driver works.
- AI implementer throughput is unmeasured.
- A paired prototype should decide.

Four lean Go at 55–65% and one leans Rust at 65%. These are close to one another, and each depends on the same open question.

**Genuine conflicts.**
1. **Value of the native-binding option.** B counts it as insurance worth paying for. C and E say it has almost no value. D says it is contradictory and must be decided first. That is a real disagreement about the owner's intent, which only the owner can resolve.
2. **Which failure mode is worse.** C and E: Rust's async cancellation bugs are silent and Go's leaks are loud. B: Rust's compiler catches concurrency bugs that Go only shows at runtime. C's union-typing point cuts against its own lean. It is the least-rebutted argument for Rust.
3. **Burden of proof in the decision rules.** A and E default to Go if it is roughly as good. B defaults to Rust. C decides on fault-suite correctness first. All are defensible, but they give different answers on a tie.

**Errors shared by several.**
- **An n=1 prototype treated as a measurement.** Every decision rule (1.3×, 1.5×, 2× thresholds, rounds-to-green) compares a single AI run per language. AI implementer variance run-to-run is large, so one paired spike cannot measure defect rate. Use at least 3 independent runs per language, or treat the spike as a feasibility gate, not a throughput measurement.
- **"One worker per run means little concurrency"** (D, E, and partly A). `via serve --stdio`, `wait` over many runs, and crash recovery are long-lived and concurrent, and fan in events from tens of runs. That is where async and cancellation design actually lives, and none of the prototypes scope it as hard.
- **Sync Rust as the risk mitigation** (C, D) without checking whether the official ACP SDK forces an async runtime.
- **Windows treated as undecided but priced anyway.** Several responses give it directional weight while also calling it neutral.

**The most important gap none of them covered.** The prior council recorded its own condition for switching, and README §12 quotes it: "Choose Go if runtime-independent installation … become hard first-release requirements." The owner's "static binary: important" is what triggered this re-decision, and that condition names Go, not Rust. No response engaged with it; D only hints at it. Either Rust must beat that condition on new evidence (the Rust-only official ACP SDK, PyO3), or the prior council's reasoning already favours Go.

Two further points that none made concrete:
- Public multi-language agent coding benchmarks with Go and Rust splits, such as Multi-SWE-bench, give at least a prior on AI implementer performance. It is UNVERIFIED whether their numbers apply here. Everyone called that factor unmeasurable without checking for external data.
- Adapter churn dominates the lifetime cost. The contract-test and replay-fixture harness can be language-neutral: C and E propose a Python black-box suite. The language choice therefore governs less of the ongoing cost than all five imply.

**What a synthesis should carry forward:**
- Get the owner's three answers before any spike: who installs VIA, which operating systems, and whether an in-process binding is needed (D).
- Frame the choice as v0 versus v1 (A).
- Plant union and unknown-variant checks in the spike (C).
- Make the multi-process pure-Go SQLite WAL test the switch condition (E).
- Price PyO3 only if the owner answers yes (B).
- Scope `serve --stdio` fan-in into the spike.
- Run each language more than once.