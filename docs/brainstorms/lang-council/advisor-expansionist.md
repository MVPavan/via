
# Expansionist advisor: Rust or Go for VIA

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