UNSOUND

1. **Important — `crates/via-cli/tests/support/evidenced.rs:252`: unreadable process environments are treated as absence.** Every `environ` read error is skipped, including permission failures for a live sandbox process. After its locks are released, the collector can accept exit and delete the sandbox. Reproduction: a live, same-UID process carrying the runtime marker made its environment unreadable; `stop_within` returned `Ok(())` while it remained alive. **Smallest fix:** distinguish vanished/provably unrelated processes from unreadable potential sandbox processes; return an indeterminate-proof error for the latter and preserve the sandbox.

2. **Important — `crates/via-cli/tests/c1_protocol.rs:80`: the budget still does not bound blocking connection setup.** `Connection::open` calls blocking `UnixStream::connect` before setting socket timeouts. A stalled listener with a full accept queue can therefore exceed 20 seconds. Reproduction using the unchanged proof functions: a 300 ms budget returned after **802 ms**. Late success was correctly rejected, but execution was unbounded during connect. Additionally, per-operation timeouts do not impose an absolute deadline across `write_all`/`read_line`. **Smallest fix:** pass the absolute deadline into nonblocking connection and exchange operations, recomputing remaining time before each wait.

Round 2 finding 3 is fixed: folder validation now runs unconditionally in `store_evidence`. The three new self-tests are meaningful, but they miss unreadable environments and bounded completion of blocking operations. No unrelated-process false match was found in the inspected launch paths.

**Out of scope, noticed**

The pre-existing `crates/via-cli/tests/c1_protocol.rs:151` performs synchronous stop exchanges before starting its exit-wait deadline. The analogous S1 daemon guards also use separate stop/wait budgets. These do not affect this verdict.

**Could not verify**

Full workspace/release gates and macOS qualification were not rerun. All seven collector tests passed with artifacts confined to `target/`; formatting and diff checks passed. Tracked files and Git state remained unchanged.
