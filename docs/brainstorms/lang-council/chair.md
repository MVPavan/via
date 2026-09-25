I've finished checking the facts in the repo and on the web. Here is the ruling.

# Chair's ruling: VIA in Rust or Go

## 1. Recommendation

**Go, about 60% confidence, on three conditions:**
1. The foreman calls VIA through the CLI or thin SDK, not a native binding.
2. A pure-Go SQLite driver passes a test with several processes writing in WAL mode.
3. The owner confirms that "static binary: important" means installing without Python or another runtime is a real requirement.

If condition 1 fails (the owner wants an in-process binding), choose **Rust at about 70–75%**. If condition 2 fails, Go needs cgo, loses its build advantage, and the choice becomes roughly 50/50 leaning Rust. If condition 3 fails because the binary is only for the owner's own machines, the premise is gone and the earlier Python verdict should be revisited, not replaced.

**What the confidence means.** The lean is small. No two-language spike has been run, so a "Go" answer is a default, not a measured result. The spike (§6) should overturn it if it points the other way.

## 2. Where the lenses agree

- **Process-group and grandchild cleanup favours neither language.** Both need `setpgid` plus `kill(-pgid)`, and Windows job objects are manual work in both. The earlier council reached the same ruling (`docs/brainstorms/README.md:245`).
- **The Codex crate is a wash.** Pinning adapters to each vendor CLI version favours generating types from the installed binary (`generate-json-schema`), in either language.
- **PyO3 is the one decisive argument for Rust**, and it depends on the undecided foreman question.
- **Go makes a static cross-compiled binary easier**, provided a pure-Go SQLite driver works. Rust's bundled C SQLite needs a C cross toolchain.
- **AI implementer throughput is unmeasured**, and a paired prototype should settle the question.

## 3. Disagreements and rulings

**D1. How much the native-binding option is worth.** The Expansionist counts it as insurance; the Contrarian and First Principles say it is nearly worthless; the Outsider says it must be decided first.
- **Evidence I checked.** The repo contradicts itself. `README.md:34` says "Foreman calls VIA should mean sharing a library, not shelling out", while settled decision #1 prescribes thin SDKs that spawn the binary. The peer reviewer is right that the README's motive is sharing types and run identity (`contracts/run_identity.py`), not saving spawn milliseconds, so First Principles argued against the wrong motive. Types generated from JSON schema into the Python SDK meet most of that motive without an in-process binding.
- **Ruling.** Only the owner can decide this, and it has the most leverage of any open question. It must be answered before the spike, not priced as an option. I give no weight to the Expansionist's other options (napi-rs, WASM, ACP proxy mode), because nothing currently requires them.

**D2. The official ACP SDK advantage.**
- **Evidence I checked.**
  - ACP's official SDKs are TypeScript, Python, Rust and Kotlin, with a Java repo; there is no Go SDK (`tech-stack-council/judge-gpt-6-astra-high.md:59`). So Rust is *an* official SDK, not *the* only one.
  - docs.rs shows `agent-client-protocol` **v2.2.0, released 2026-09-18**, with separate v1 and v2 connection contexts. The crate has already shipped a major version, and the spec's v2 is in alpha (`README.md:49`). That supports the Contrarian's point that spec churn, not a missing SDK, is the lasting cost. I did not check how often it releases.
  - The crate depends on `futures`, and tokio appears only as a dev-dependency. So it is async but not locked to a runtime. A synchronous Rust design could drive it with a small executor, but this would need a bridge.
  - ACP is a v1 feature, not v0.
- **Ruling.** Rust gets a real but moderate edge at v1, worth about one adapter plus faster tracking of spec changes. It does not decide the question on its own.

**D3. Which language's failure modes are worse.** Go leans: Rust's async cancellation bugs are silent, while Go's goroutine leaks are loud and easy to diagnose. Rust leans: the compiler catches concurrency bugs.
- **Missing from both sides.** The Contrarian's own point D3 is the best argument for Rust and the least rebutted. Go has no sum types, and `encoding/json` treats a missing field and a zero value the same way. Across more than 10 vendor schemas that drift, that risks silent contract bugs.
- **Ruling.** This cannot be decided from priors. Build both failure modes into the spike as planted faults: an unknown variant and a missing-versus-zero field on the Go side, and cancellation in the middle of a write on the Rust side.

**D4. Whether "one worker per run" makes concurrency trivial.** The Outsider and First Principles say yes.
- **Ruling: partly wrong.** The peer reviewer is right. `via serve --stdio`, `wait` over many runs, and crash recovery are long-lived processes that merge events from tens of runs. That is where async design and cancellation matter, so the spike must cover it.

**D5. How to decide if the spike ties.** Executor and First Principles default to Go, the Expansionist defaults to Rust, and the Contrarian decides on correctness.
- **Ruling.** Correctness under faults decides first. If that ties, the owner's stated priority (static distribution) plus the earlier council's recorded Go condition break the tie.
- **One correction.** The earlier judge's full condition is stricter than the README's quotation of it: "Choose Go if runtime-independent installation **and Windows operations** become hard first-release requirements, **and the selected routes work without extensive sidecars**" (`judge-gpt-6-astra-high.md:125`). The owner rated static distribution "important", not a hard requirement, and Windows is still undecided. So that precedent supports Go only weakly. The same judge wrote "Consider Rust if measured resource or systems requirements justify its implementation cost" (line 126), and none have been measured.

## 4. What the council as a whole missed

1. **n=1 is not a measurement.** Every decision rule compares one AI run per language, with thresholds of 1.3×, 1.5× and 2×. The variance between AI runs swamps those thresholds. Use the spike as a feasibility and fault-correctness gate, and only compare throughput with at least 3 runs per language.
2. **The earlier council's scoring was never engaged.**
   - The Opus member scored Rust's AI velocity 2/5 against Go's 4/5, citing borrow and async friction on code that changes weekly (`member-opus-5-5-high.md:32`).
   - The Sol member scored them almost equal: Rust 39, Go 38.
   - These are opinions, but they are the only prior on record, and none of this council's lenses cited them.
3. **Rewriting the Python code is priced nowhere.** About 2.6k lines of profiles and parts of the 14.5k-line inspector (`member-opus-5-5-high.md:31`) have to be ported in *either* language. This does not favour one language, but it is the largest v0 cost and should shape the plan.
4. **The contract-test harness can be language-neutral.** Put fixtures and replay tests in Python, as a black-box suite. Most of the lifetime cost is adapter churn, and a neutral harness keeps that cost independent of the language chosen.
5. **The outside benchmark prior was never looked up.** Multi-language agent coding benchmarks (for example Multi-SWE-bench, which reports Go and Rust separately) were not checked. I did not check them either.
6. **The build environment is unprepared.** Neither `go` nor `cargo` is on PATH (verified by the peer reviewer). Installing a toolchain is step zero either way.

## 5. Decisions the owner must make

These three questions could flip the recommendation. Each needs a one-line answer:
1. **Does the foreman need an in-process binding?** A yes means **Rust**. A no confirms the Go lean, subject to the spike. This also resolves the conflict between `README.md:34` and decision #1.
2. **Who installs VIA?** If the answer is only the owner's machines, revisit whether leaving Python was needed at all.
3. **Is Windows a first-release target?** A yes, together with a hard runtime-free-install requirement, meets the earlier judge's full Go condition.

## 6. Practical next step

**Step 0 (today).** Get the owner's three answers above. Install Go 1.2x and stable Rust in the worktree toolchain.

**Step 1: a paired feasibility spike.** Build it in gitignored `scratchpad/`, one git worktree per language. Use the same brief and the same implementer and critic roster for both. Write the black-box pytest suite first; it is shared by both languages. The slice covers:
- `spawn`, `wait`, `status` and `cancel` with `--json`, including a detached worker that re-executes the binary;
- a SQLite WAL launch receipt written before dispatch, and a verbatim raw log;
- `serve --stdio` merging events from 8 concurrent runs;
- a fake ACP peer. The Rust build uses the official crate v2.2.0; the Go build uses types generated from the pinned schema.

**Pass/fail gates.** Both languages must pass all of these:

| # | Check | Pass |
|---|---|---|
| 1 | Cancel while a grandchild holds stdout open and the child ignores SIGTERM | 0 surviving processes (`ps`); worker exits within 5 s of SIGKILL escalation |
| 2 | 50 × `kill -9` of the worker in the middle of a write | 0 lost receipts; every run in a defined recovery state |
| 3 | 32 concurrent runs, 10 MB line, DB locked for 5 s | 0 `SQLITE_BUSY` failures; raw logs byte-identical (sha256) |
| 4 | Planted unknown ACP variant, and a field that is absent vs zero | Both surfaced, never silently dropped or zeroed |
| 5 | 1,000 spawn/cancel cycles | FD count and goroutine/task count flat (±5%) |
| 6 | Cross-build linux amd64/arm64 + darwin arm64 from one Linux host | Artifacts run; `ldd` shows the Linux builds are static |
| 7 | Go only: `modernc` or `ncruces` SQLite under multi-process WAL | Passes checks 2 and 3 with `CGO_ENABLED=0`. If it fails, Go loses its build advantage and I lean Rust |

**Decision rule.**
1. Pick the language with fewer gate failures after one critic round.
2. If they tie on gates, choose Go, unless the owner answered yes to the in-process binding.
3. Only claim a throughput difference if at least 3 runs per language show more than a 1.5× gap in rounds to green.

## 7. One line per lens

- **Executor:** Go ~60%. v0 is process supervision with no protocol SDK on the critical path, and Rust's ecosystem edge only arrives at v1.
- **Expansionist:** Rust ~65%. The official ACP SDK, PyO3 and similar Rust tools keep more future options open, though it conceded that the wire-only design reduces reuse.
- **Contrarian:** Go ~55%, flipping to Rust ~60% if the binding stays open. It made the strongest pro-Rust point (typed unions versus Go's absent/zero ambiguity).
- **Outsider:** Go 55–60%, but asked first for the premises: who installs VIA, which OS, and binding yes or no. It found the README contradiction on the binding.
- **First Principles:** Go 60–65%. The binding constraints are lifecycle, static SQLite and adapter churn, and only static SQLite separates the languages. It gave the sharpest switch condition (pure-Go SQLite passing WAL).