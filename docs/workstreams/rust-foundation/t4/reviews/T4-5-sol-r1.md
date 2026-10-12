**UNSOUND**

## Findings

- **Important — `crates/via-store/src/blob.rs:487`:** The 10 s prompt-file deadline does not cover `writer()` or `finish()`. If copying reaches EOF near the deadline, `finish()` can complete later and the request can still be admitted. Carry the same absolute deadline through blob creation, writes, and finish; discard a blob that finishes too late.

- **Important — `crates/via-core/src/engine/recovery.rs:469`:** Recovery builds an `unknown` terminal envelope with `cwd: null`, although the session’s frozen `cwd` is stored. The history fallback also sets it to `None` at `crates/via-core/src/engine/recovery.rs:308`. After a daemon crash, `result` can therefore disagree with `status` about the turn’s working directory. Read the frozen session `cwd` when rebuilding these envelopes.

- **Important — `crates/via-cli/src/server/dispatch.rs:99`:** The partial-line timer starts when the handler next calls `read_line`, rather than when the first byte arrives. A client can pipeline a partial line behind a long `wait`; that line can remain open for the wait’s duration plus another 5 s. Timestamp incoming bytes while the prior request is handled, while retaining sequential dispatch and replies.

- **Important — `crates/via-cli/src/client.rs:465`:** If reading stdin fails, the stdio proxy leaves the socket’s write side open. Its daemon-to-stdout task can then wait indefinitely for a daemon that is waiting for more input. Shut down the write side on both EOF and read error, and propagate the read error.

- **Minor — `crates/via-core/src/engine/receipt.rs:145`:** The `cwd` metadata check starts an unowned `spawn_blocking` task, contrary to coding-style §5. A stalled filesystem check can outlive cancellation of its request. Keep the task in an owned, bounded set.

- **Minor — `crates/via-cli/src/client.rs:477`:** On daemon EOF, the proxy drops its `JoinSet` without joining a potentially blocked stdin copy. Process exit prevents a persistent worker, but the shutdown does not meet coding-style §5’s bounded drain rule. Use a cancellable input path or an explicitly bounded owner-controlled shutdown.

- **Minor — `crates/via-fake-agent/Cargo.toml:16`:** `sha2` is a new **direct dependency of the fake-agent crate**, despite already being a workspace package. The plan’s unqualified “no new dependency” rule covers that edge. Have the test hash captured prompt bytes instead, then remove the edge.

## Scrutiny checks

The connection permit is owned by each task and released on exit; oversize requests close without another read; and the reply timer starts before `write_all`. Retry identity substitutes the prompt-file content token. The copy opens with `O_NONBLOCK`, checks metadata before and after, and discards unadopted blobs after admission; file I/O is outside the admission lock, so `close` and stop do not wait on that lock. The exceptions are the two deadline findings above. `serve --stdio` has one `JoinSet` and half-closes on stdin EOF, with the error and shutdown gaps noted above.

The other reported deviations do not establish defects: `await_terminal` needs only terminal facts; Store owns blob steps and the added frozen fields; the CLI instruction and session-scope behavior matches C1; duplicate envelope members are refused; and the replacement tests cover null and fractional IDs, nested-null deadlines, unread replies, and the timing-sensitive `result` poll.

## Measurement suggestions

None.

## Could not verify

I reran the T4-5 selector (**20 passed**), `cargo fmt --all --check`, the layer check, and `git diff --check`; Git remains clean. I did not rerun the full gate reported by the worker or reproduce the slow-filesystem and crash-recovery scenarios above.
