**Verdict: ACCEPT AFTER CHANGES.** HEAD demonstrates the Task 1 prompt-to-result path: the CLI, fake agent, durable receipt, result, events, and Store evidence are exercised together. I cannot accept the slice yet because force can commit an unsupported terminal disposition, an acquisition failure can underreport raw-evidence uncertainty, and the peer-UID fix lacks a default-gate regression that would catch its original omission.

### Findings

1. **Important — Force can claim a turn was cancelled without evidence that it stopped. Blocks Task 1.** `crates/via-core/src/engine.rs:739` treats every force-abandoned turn as `cancelled`. When Host reports neither a live-vendor stop nor proved absence, the vendor may still be running; C1’s unconfirmed-transport disposition is `unknown`. Separately, `crates/via-core/src/engine.rs:888` changes **positive** Host force evidence into `requested` whenever cleanup is uncertain, although C1 keeps outcome and cleanup certainty separate. Preserve those two facts independently: use `forced` with `uncertain` cleanup when Host proved the stop, and use `unknown` when a launch is possible but neither stop nor terminal is proved. Keep the prelaunch case distinct. Add durable-result regressions for both Host reports.

2. **Important — Post-ARM acquisition errors can leave missing raw bytes unreported and misclassify a deadline. Blocks Task 1.** Host records that ARM may have been sent at `crates/via-host/src/host.rs:445`, but Wire uses that fact only for the *force timeout* branch at `crates/via-wire/src/runtime.rs:174`. If acquisition instead returns a Host error after ARM—for example, a lost spawn reply or acquisition deadline—`crates/via-routes/src/runtime.rs:79` sets `raw_incomplete:false` despite never receiving the pipes. Its `crates/via-routes/src/runtime.rs:370` also turns `HostError::Deadline` into transport loss, yielding `unknown` rather than `failed(deadline_wall)`. Carry the ARM fact and deadline cause through Wire’s error type; settle raw completeness only from evidence the route actually has, and add a controlled post-ARM error regression. This is distinct from the deferred daemon-wide F12 Store-failure controller.

3. **Important — The client peer check is wired correctly, but its default regression would survive removal of that wiring. Blocks Task 1’s requested regression criterion.** `crates/via-cli/src/client.rs:229` checks the peer before protocol traffic, resolving T1-I2 in source. The running `crates/via-cli/src/client.rs:292` calls `verified_peer` directly; it would still pass if `request()` stopped calling it. The end-to-end `crates/via-cli/tests/c1_protocol.rs:388` detects that omission but is ignored by the stated gate. Run it in a suitable privileged or user-namespace environment as acceptance evidence, or add an enabled integration regression with the same before-first-byte assertion.

4. **Important — Drain and force do not close sessions that already have a terminal turn. Defer to `via-jm4.7.7` lifecycle.** `crates/via-core/src/engine.rs:731` commits `session.closed` only alongside a force-abandoned turn. A drained turn is instead left `idle` by `crates/via-store/src/runtime/sql.rs:467`; forcing an already idle daemon likewise closes no session. C1 `docs/specs/via-api-v1.md:552` requires closure after drain and force. The `crates/via-cli/tests/s1_daemon_stop.rs:650` checks the turn and clean exit but not the session row or closing event. Add a durable closure operation for remaining sessions and assert it before claiming a clean lifecycle exit.

### T1-I1–I7 and test evidence

| Finding | Disposition at HEAD |
|---|---|
| I1 stream tails | **Resolved.** Route half-closes, drains both streams, and rejects duplicate terminals; the `crates/via-cli/tests/route_drain.rs:265` exercise each original failure. |
| I2 peer UID | **Code resolved; effective default regression missing** (finding 3). |
| I3 strict requests | **Resolved.** Typed parameters and `crates/via-cli/tests/c1_protocol.rs:212` cover current methods. |
| I4 debug cleanup endpoint | **Resolved.** The production endpoint is absent; the `crates/via-cli/tests/s1_daemon_stop.rs:791` uses an independent Store snapshot, verified control, and absence probe. |
| I5 canonical events/observations | **Resolved for Task 1.** The `crates/via-cli/tests/s1_prompt_to_result.rs:928` and `crates/via-cli/tests/route_drain.rs:365` check committed output. |
| I6 receipt/envelope shape | **Resolved for Task 1** by the same foreground, result, and Store comparison. |
| I7 active force | **Partially resolved.** Force reaches Route and Host, but its final disposition is wrong in finding 1. |

The suite has substantial cross-process evidence. Its weakest additional claim is the `crates/via-core/tests/force_stop.rs:260`: a one-second sleep neither establishes that ARM occurred nor proves the vendor emitted the purported lost line. Replace it with an explicit anchor/vendor barrier when fixing finding 2. The ignored queued-handoff race has been accurately identified as timing dependent; its deterministic version belongs with the deferred failpoint controller.

### Handoff contract text

| Proposed text | Judgment |
|---|---|
| Prelaunch force is `requested`/`quiescent` | **Record as is**, explicitly defining “requested” for a force accepted before any vendor launch. |
| Force after a Store event failure is `failed(store)` with `cancel` filled | **Record as is** as failure precedence; full daemon-wide Store-failure behavior remains `via-jm4.7.7`. |
| Pending `wait` at final shutdown ends `daemon_stopping` | **Record as is** for a result still absent after finalization. |
| `session.closed.reason = daemon_stop_force` | **Record as is** for forced closure; fix the missing idle-session force and drain closures in finding 4. |
| `vendor.other.truncated` | **Record as is**; it is an additive, explicit bounded-payload marker. |
| Post-ARM abandonment marks raw log incomplete because bytes *may* be lost | **Change code or resolve C1 first.** C1 `docs/specs/via-api-v1.md:648` currently reserves `raw_log_incomplete` for actual raw loss; the ARM flag alone proves uncertainty. Retain and drain the pipes where possible, or define a separate uncertainty outcome in the contract before emitting it. |
| 256 unresolved-turn cap refuses with `admission_refused` | **Record as is** for a full set of in-flight turns. Preserve the code’s `store_error` distinction when failed turns occupy that set. |

**Verification limit:** I reviewed source, contracts, and tests read-only at `ec0a225`. I did not rerun the gate; the 116 passed / 2 ignored result is the supplied local result. I ran no `bd` command and changed no files. Git status still shows only the two pre-existing `.beads` modifications.