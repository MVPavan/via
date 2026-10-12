**UNSOUND.** Most r1 infrastructure defects were fixed, but causal exceptions, gate timing, validation and privacy still leave gaps.

Verified at `29073bc`: **70 active tests passed**. All **31 ignored conformance cases** fail only with “adapter not implemented.” The current fixtures pass hygiene scanning. Git state remained unchanged.

“Routed” below means the vendor table assigns the assertion to a future named test; it does not mean that test exists or passes.

| R1 | Status and evidence |
|---|---|
| F1 | **Fixed.** Documentary omissions cannot override assertions: [checker:600](../../../../../crates/via-core/tests/support/conformance_expect.rs#L600). |
| F2 | **Partly fixed.** Vacuous baseline and unresolved `start_after` witnesses are rejected. Nested payload and gate validation remains incomplete: [checker:901](../../../../../crates/via-core/tests/support/conformance_expect.rs#L901); findings below. |
| F3 | **Fixed.** Opaque terminal JSON compares exactly: [checker:1223](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1223). |
| F4 | **Fixed.** Final-text pieces compare by concatenation: [checker:1246](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1246). |
| F5 | **Fixed vocabulary and stronger recorded pins.** Payload-bearing observations are supported: [checker:76](../../../../../crates/via-core/tests/support/conformance_expect.rs#L76). Claude denial/decline mapping remains explicitly unasserted under the coordinator’s ruling. |
| F6 | **Fixed vocabulary; survivor assertion soundly routed.** Exit, journal uncertainty, group evidence and health are expressible. `claude_cleanup_not_ack` explicitly requires survivor and provenance assertions: [Claude spec:468](../../../../../docs/specs/vendors/claude-code.md#L468). The crash-evidence documentation needs correction below. |
| F7 | **Fixed interface; pure-operation coverage routed soundly.** Launch checkpoints and write evidence compare through one checker: [checker:1373](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1373). `claude_preflight_pure_version` explicitly checks process/file purity. |
| F8 | **Vocabulary fixed; routing incomplete — Important.** Claude effort, instructions, steps and reserved options have explicit named rows. Codex reserved/bound behavior has `codex_bound_gate`, but no row explicitly proves non-null frozen instruction forwarding: [Codex spec:495](../../../../../docs/specs/vendors/codex.md#L495). Smallest fix: extend that row to require instruction bytes on fresh and resumed sessions. |
| F9 | **Fixed.** Resume values are literal pins rather than self-echoing captures: [Claude suite:25](../../../../../crates/via-core/tests/conformance_claude.rs#L25). Codex resume requests also pin thread IDs. |
| F10 | **Soundly routed.** `claude_identity_resume` explicitly requires the three-child identity chain: [Claude spec:461](../../../../../docs/specs/vendors/claude-code.md#L461). Multi-lifetime replay is present; its concurrent ordinal test passes. |
| F11 | **Fixed.** Clearing asserts null structured output: [c1c expectation:57](../../../../../crates/via-adapters/tests/fixtures/claude/c1c.expect.json#L57). |
| F12 | **Fixed.** Claude amounts/provenance and Codex unavailable cost are pinned: [c1a expectation:70](../../../../../crates/via-adapters/tests/fixtures/claude/c1a.expect.json#L70). |
| F13 | **Recorded pins fixed; untested-version coverage soundly routed.** `claude_preflight_pure_version` and `codex_pin_handshake` explicitly cover outside-`checked` versions: [Claude spec:458](../../../../../docs/specs/vendors/claude-code.md#L458), [Codex spec:490](../../../../../docs/specs/vendors/codex.md#L490). Excluding Core-derived warning codes is consistent with the stated boundary. |
| F14 | **Soundly routed.** Six response bodies, unknown/auth requests, congestion and failed writes are explicit in `codex_never_ask`: [Codex spec:494](../../../../../docs/specs/vendors/codex.md#L494). |
| F15 | **Gate vocabulary added; per-harness assertions soundly routed.** `claude_interrupt_pairing`, `claude_normalizer_accounting`, `codex_start_order` and `codex_control_races` cover the relevant distinctions: [Claude spec:467](../../../../../docs/specs/vendors/claude-code.md#L467), [Codex spec:498](../../../../../docs/specs/vendors/codex.md#L498). Gate mechanics need fixes below. |
| F16 | **Soundly routed.** `codex_cleanup_60s` explicitly includes early tool completion, remaining-wall limits, immutable late completion and successor warnings: [Codex spec:497](../../../../../docs/specs/vendors/codex.md#L497). |
| F17 | **Partly fixed.** c6 now specifies a 40-second controlled advance, but its gate does not establish that the handshake timeout is armed: [c6 expectation:52](../../../../../crates/via-adapters/tests/fixtures/codex/c6_cold_initialize.expect.json#L52). |
| F18 | **Routing incomplete — Important.** Lost/partial starts and congestion are assigned. Codex catalog pagination and contradictory/malformed terminal notifications lack explicit decisive named assertions: [Codex spec:491](../../../../../docs/specs/vendors/codex.md#L491). Smallest fix: add these assertions to named vendor-table rows and x.3.2 acceptance. |
| F19 | **Partly fixed.** Default causal arrival rejects the original witness; `pipelined` removes every causal floor: [replay:659](../../../../../crates/via-fake-agent/src/replay.rs#L659). Confirmed false green below. |
| F20 | **Fixed as an infrastructure policy.** Measurement is documented, c11b allows 5,250 ms, and the actual five-second obligation remains with controlled-time adapter tests: [replay:36](../../../../../crates/via-fake-agent/src/replay.rs#L36). |
| F21 | **Partly fixed.** Original credential/home-path probes are detected; identity values in prefixed text still escape scanning: [fixtures:839](../../../../../crates/via-fake-agent/tests/fixtures.rs#L839). |
| F22 | **Fixed recursion and documented source boundary.** [fixtures:102](../../../../../crates/via-fake-agent/tests/fixtures.rs#L102). Recursive symlink handling introduces a robustness defect below. |
| F23 | **Fixed.** The supervisor covers version probing and blocked stdin writes: [fixtures:224](../../../../../crates/via-fake-agent/tests/fixtures.rs#L224). |
| F24 | **Fixed fixture sealing.** Non-crash endings use EOF, with declared exceptions. The future `drive()` verdict obligation remains insufficiently explicit: [checker:149](../../../../../crates/via-core/tests/support/conformance_expect.rs#L149). |

The new defects and residual defects exposed by these fixes are:

1. **Important — Pipelined exceptions erase necessary causality.**  
   [replay.rs:659](../../../../../crates/via-fake-agent/src/replay.rs#L659), [c11 replay:120](../../../../../crates/via-adapters/tests/fixtures/codex/c11_failed_command.replay.json#L120).  
   The exception correctly permits `turn/start` before the unrelated `thread/started` notification, but also permits it before the `thread/start` reply. A probe wrote both requests together before reading that reply; the **actual c11 fixture exited 0**.  
   **Smallest fix:** allow an explicit causal predecessor, such as `after_emit: 8`, instead of disabling the floor altogether.

   All 15 exceptions have internally plausible independence reasons: thread-start replies versus subsequent notifications; c4’s B start versus A’s later traffic; Claude interrupt versus a later rate-limit event. Their defect is the missing earlier floor, not those independence explanations.

2. **Important — The cold-initialize gate can advance before the timeout exists.**  
   [c6 expectation:52](../../../../../crates/via-adapters/tests/fixtures/codex/c6_cold_initialize.expect.json#L52), [checker:119](../../../../../crates/via-core/tests/support/conformance_expect.rs#L119).  
   Its predicate—unaccepted, no terminal/error, no identity—is already true before `run_turn` is polled. The documentation expressly permits holding turn futures unpolled. `SigCgt` proves handler installation, not consumption of `initialize` or timeout registration. A fixed five-second timeout armed after the advance can therefore pass.  
   **Smallest fix:** establish positive handshake readiness, poll the adapter through its pending handshake wait, then advance and process expired timers before snapshotting and releasing the gate.

3. **Important — `SIGNAL_SKEW` accepts premature EOF.**  
   [replay.rs:688](../../../../../crates/via-fake-agent/src/replay.rs#L688).  
   Closing stdin **20 ms before SIGUSR1** produced exit 0. Subtracting 50 ms exchanges the prior race for a deliberate false-green interval; watcher delays exceeding that interval can still produce false reds.  
   **Smallest fix:** synchronize signal consumption and EOF ordering through an explicit acknowledgement/fence rather than treating timestamp proximity as ordering proof.

4. **Minor — Signal arrivals after the deadline are accepted.**  
   [signals.rs:87](../../../../../crates/via-fake-agent/src/replay/signals.rs#L87).  
   `take()` returns queued arrivals before checking their timestamps against the deadline. A probe supplied an arrival one second after its deadline and received `Ok`. The watchdog can race this successful return.  
   **Smallest fix:** reject `arrived > deadline`, independently of watchdog execution.

5. **Important — Nested validation remains incomplete.** These independent malformed witnesses all returned `validate == Ok`:

   | File:line | Accepted malformed value | Smallest fix |
   |---|---|---|
   | [checker:727](../../../../../crates/via-core/tests/support/conformance_expect.rs#L727) | `describe.params` with boolean model, numeric cwd and string `require` | Validate each parameter’s C1 type. |
   | [checker:873](../../../../../crates/via-core/tests/support/conformance_expect.rs#L873) | Progress with `"model":"yes"` and `"tools_started":123` | Validate observation payload types, tuples and enum values. |
   | [checker:930](../../../../../crates/via-core/tests/support/conformance_expect.rs#L930) | Cost with `"usd":"free"` and boolean scope | Validate cost amount, scope and provenance together. |
   | [checker:965](../../../../../crates/via-core/tests/support/conformance_expect.rs#L965) | Exit with string code and boolean signal | Validate exit member types; likewise the currently unchecked usage and stop-fact members. |

6. **Important — Gate expectations bypass enum validation.**  
   [checker:834](../../../../../crates/via-core/tests/support/conformance_expect.rs#L834).  
   A gate containing `error:"nonsense"` and `terminal.status:"banana"` validates. Gates call `expected_types`, while enum rules run only on final expectations.  
   **Smallest fix:** share enum validation between gates and final expectations, explicitly allowing `pending` cleanup only in intermediate snapshots.

7. **Important — The new close ordering conflicts with c4.**  
   [checker:140](../../../../../crates/via-core/tests/support/conformance_expect.rs#L140), [c4 replay:324](../../../../../crates/via-adapters/tests/fixtures/codex/c4_two_sessions.replay.json#L324).  
   The obligations put closes after the turns. c4 instead blocks on A’s unsubscribe before emitting B’s completion. Waiting for all turns before closing sessions deadlocks a correct adapter.  
   **Smallest fix:** specify closing each session after its own final turn settles, including while other sessions remain active.

8. **Important — Replay completion needs an explicit expected-exit assertion.**  
   [checker:149](../../../../../crates/via-core/tests/support/conformance_expect.rs#L149).  
   The obligation identifies exit 3 as failure but does not explicitly require the fixture’s expected exit code and stderr. A signal-killed shared server can miss unsubscribe/EOF steps without exiting 3; turn `exit:null` cannot detect this. The generic fidelity driver already performs the stronger comparison.  
   **Smallest fix:** require that comparison in both conformance drivers, rejecting undeclared signal exits and incomplete replay endings.

9. **Important — Identity values in prefixed text escape hygiene scanning.**  
   [fixtures.rs:849](../../../../../crates/via-fake-agent/tests/fixtures.rs#L849), [fixtures.rs:915](../../../../../crates/via-fake-agent/tests/fixtures.rs#L915).  
   Both `{"stderr":"WARN {\"username\":\"alice\"}"}` and `{"stderr":"username=alice"}` returned no finding. Identity checking applies only to parsed object keys; the prefixed-text assignment scanner recognizes credentials only. Bare credential arguments also escape it.  
   **Smallest fix:** scan identity assignments in decoded text and embedded prefixed JSON; recognize credential CLI arguments such as `--password VALUE`.

10. **Minor — Recursive scanning follows directory symlinks without cycle detection.**  
    [fixtures.rs:109](../../../../../crates/via-fake-agent/tests/fixtures.rs#L109).  
    A symlink to an ancestor causes unbounded traversal; an external target also escapes the intended fixture boundary.  
    **Smallest fix:** reject fixture symlinks using `symlink_metadata`, or enforce containment and track visited directories.

11. **Minor — Documentation allows a null baseline that validation rejects.**  
    [checker:52](../../../../../crates/via-core/tests/support/conformance_expect.rs#L52), [checker:903](../../../../../crates/via-core/tests/support/conformance_expect.rs#L903).  
    The documented “null allowed” baseline includes `accepted`; validation requires a boolean.  
    **Smallest fix:** document `accepted` separately as boolean-or-reasoned-omission.

12. **Minor — Two provenance descriptions still claim captured resume values.**  
    [c1a replay:2](../../../../../crates/via-adapters/tests/fixtures/claude/c1a.replay.json#L2), [c9a replay:2](../../../../../crates/via-adapters/tests/fixtures/claude/c9a.replay.json#L2).  
    Their successor fixtures now pin literals.  
    **Smallest fix:** update those two descriptions.

The C2 clarification is sound about **server-turn `exit:None`** and live-server cleanup using reported tool items. That rule need not delay ordinary completed/failed terminals; §4.1’s P7 wait remains specific to interrupted terminals.

**Important — Its crash sentence overstates cleanup certainty.** Proposed diff:10 (`scratchpad/execution/s1-critic/spec-c2-server-evidence.diff:10`), repeated by [checker:112](../../../../../crates/via-core/tests/support/conformance_expect.rs#L112). A server crash does not itself establish `GroupAbsent`; a surviving group member or denied probe requires uncertainty under C2 §2 and C1 §3.5. **Smallest fix:** say cleanup derives from Host group evidence, is quiescent only with positive `GroupAbsent` proof, and otherwise is uncertain. Do not prescribe `group_absent:true` for every crash.

I could not verify private recording fidelity, executable adapter assertions, or the updated Bead acceptance metadata. No vendor CLI/model ran. Concurrent lifetime selection passed its current Linux test; the gate and driver findings concern obligations whose implementations are still absent.