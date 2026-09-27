**Verdict: UNSOUND.** The branch addresses the main shutdown paths, but it can leave an accepted stop pending indefinitely and can report a clean exit while a receipted turn remains unresolved.

### Findings

1. **Important — An accepted stop can wait forever before final shutdown starts.** [server.rs](crates/via-cli/src/server.rs:333) closes admission in `request_stop`, then awaits writing the reply before sending `StopMode` to daemon main. A client that supplies a large JSON-RPC ID and stops reading can block that write; force stops its drives, but the daemon never enters its 10 second final shutdown. Send the stop notification before the client write, and bound the write independently.

2. **Important — Drive errors can become a false clean exit.** [server.rs](crates/via-cli/src/server.rs:144) discards `Ok(Err(ApiError))` from drives joined in the serving loop. For example, a receipted turn whose submission or terminal commit fails can leave no forced-turn entry; a later stop can then satisfy `EngineShutdown::is_clean` and exit 0. Retain drive failures and unresolved receipted turns in the final disposition, with their durable state checked before claiming clean.

3. **Important — Force invents a cancel acknowledgement.** [engine.rs](crates/via-core/src/engine.rs:470) records `outcome: "acknowledged"` when recovery finds no anchor. C1 §7.4 defines acknowledgement as specific vendor evidence; absence of an anchor intent proves no process launch, not vendor acknowledgement. Represent the prelaunch cancellation without that claim, and add a force-during-launch regression. The same code should establish a `forced` outcome from Host force evidence, rather than group absence alone.

4. **Important — Drain can discard a waiting foreground caller.** [server.rs](crates/via-cli/src/server.rs:169) aborts every client as soon as accepted drives settle. A `wait` already serving a foreground `via spawn` can lose its reply even though the result committed. The worker discloses this, but it is a functional gap in the claimed drain behavior. Let pending reads deliver committed results within the final deadline, then account for any clients that remain.

5. **Important — Collected task failures are forgotten by a later shutdown call.** [host.rs](crates/via-host/src/host.rs:682) increments a local failed count and removes the task from the registry. A subsequent `Host::shutdown` can report zero failures and support a clean disposition despite the earlier failed reaper. Retain the failure fact in Host state until final shutdown; test two shutdown calls after one failed join.

6. **Important — The force result is missing required lifecycle events.** [engine.rs](crates/via-core/src/engine.rs:490) commits `turn.ended` but no `cancel.requested`, `cancel.settled`, or `session.closed`. I agree with the worker’s disclosure; it remains a C1 gap. Add those events in Core’s durable transition, with dense sequence and event-range assertions.

### Coverage and open items

The Host report and passive C2/Wire propagation address the original error-only report and ignored join-result paths. The force and drain end-to-end tests would fail on the pre-change binary for the reasons reported. The clean-stop test would fail because the old daemon emitted no summary; it does not exercise an incomplete exit. The new bounded-drop unit test tests its helper, not a real daemon’s exit-4 path. T1-I4’s daemon-first-death test is a positive outer-cleanup gate, **not** a regression that fails on the pre-change code; the worker correctly says so. The CLI `--drain` regression first fails on argument parsing, so it does not independently prove the old strict RPC DTO rejected `drain`.

I agree the missing real exit-4 and failed `anchor.wait()` gates, force-during-launch uncertainty, and lost drain replies remain open. The worker understates the false-clean and stop-notification risks above. I inspected the named ref and contracts only; I did not rerun tests because the shared checkout has unresolved, unrelated conflicts. The worker’s reported gate results are therefore unverified in this review.

