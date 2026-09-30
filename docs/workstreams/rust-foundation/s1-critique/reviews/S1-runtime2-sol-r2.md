**UNSOUND**

1. **Important — Core replaces a decoded completion with `failed(process_exited)`.**  
   `crates/via-core/src/engine/terminal.rs:344` rejects `Completed` unless the exit code is zero; `crates/via-core/src/engine/terminal.rs:348` rejects it when cleanup is uncertain. Both contradict C1 §7.6’s completed-terminal precedence and T3 §2’s late-terminal rule.

   **Scenario:** the vendor emits `completed` with `"done"` but remains alive until wall expiry. Route preserves the terminal and force-closes the vendor; Core records `failed(process_exited)` because cleanup produced SIGTERM. An isolated daemon probe reproduced exactly this result. The same defect affects completed terminals followed by nonzero exit, missing exit evidence, or uncertain cleanup.

   **Smallest fix:** derive the result from the decoded terminal; retain exit and cleanup as independent evidence. Add Core-level regressions for both classifier guards.

2. **Blocker — terminal-write failures do not latch until reconciliation finishes.**  
   `crates/via-core/src/engine/journal.rs:546` awaits `terminal_facts` after an uncertain or corrupt terminal commit. Core invokes the failure hook only afterward at `crates/via-core/src/engine/drive.rs:1099`. The queued-cancellation path has the same ordering at `crates/via-core/src/engine/drive.rs:938`.

   **Scenario:** the terminal commit loses its reply and read-back stalls. Health remains healthy, so admission and dispatch remain enabled despite known write uncertainty. A failpoint daemon probe verified `health: healthy` and no Store failure while reconciliation was paused.

   **Instances:** natural terminal commits, resolution commits, dispatcher/caller queued-cancellation commits, and forced-terminal commits using this shared helper.

   **Smallest fix:** publish the typed failure through the latch before awaiting reconciliation. Bound reconciliation while preserving any committed result subsequently found. Runtime §7 requires admission and dispatch to stop immediately.

3. **Important — terminal-commit corruption becomes `commit_uncertain`, losing `corrupt_store`.**  
   `crates/via-core/src/engine/journal.rs:546` handles corruption and uncertain writes together. Successful read-back retains only `Durable.uncertain`; unsuccessful read-back becomes `RECEIPT_UNKNOWN`. `crates/via-core/src/engine/journal.rs:610` reconstructs that API error as `WriteOutcome::Uncertain`.

   **Scenario:** either the initial terminal write or its retry reports SQLite corruption. Whether read-back finds the terminal or not, Core can report the latch as `commit_uncertain`, contrary to T3 §7.1’s corruption classification.

   **Instances:** all shared-helper callers listed in finding 2, including the queued-cancellation classification at `crates/via-core/src/engine/drive.rs:941` and ordinary terminal classification at `crates/via-core/src/engine/drive.rs:1099`.

   **Smallest fix:** retain the original typed Store outcome separately from receipt uncertainty, through both successful and unsuccessful reconciliation. Classify from that outcome rather than reconstructing it from the API error.

4. **Important — Host-journal corruption also loses its type before reaching Core.**  
   `crates/via-host/src/host.rs:2231` discards `StoreFailureKind` and reduces the result to `HostError::Journal { uncertain }`. Route maps that boolean to ordinary uncertainty at `crates/via-routes/src/runtime.rs:952`; Core consequently selects `WriteOutcome::Uncertain` at `crates/via-core/src/engine/resolve.rs:314`.

   **Scenario:** an anchor-intent, identity, ARM-intent, vendor-facts, or group-absence write reports corruption. The daemon latches, but its reported failure kind is `commit_uncertain`.

   **Every affected site:**
   - Acquisition journal writes: `crates/via-host/src/host.rs:1251`, `crates/via-host/src/host.rs:1322`, `crates/via-host/src/host.rs:1410`, and `crates/via-host/src/host.rs:1464`.
   - Group-absence proof through the same conversion: `crates/via-host/src/host.rs:2188`, including failed acquisition and ordinary/forced close.
   - Re-probe’s independent conversion: `crates/via-host/src/host.rs:1219`.
   - Cleanup-report reduction: `crates/via-host/src/host.rs:939` and `crates/via-host/src/host.rs:1983`.
   - Core consumers: `crates/via-core/src/engine/resolve.rs:314`, `crates/via-core/src/engine/resolve.rs:327`, `crates/via-core/src/engine/resolve.rs:333`, and `crates/via-core/src/engine/reprobe.rs:159`.

   **Smallest fix:** carry the typed Store kind through Host errors and cleanup reports, Route, Adapter results, and Core classification. These instances belong inside the revised class-A scope.

5. **Important — daemon force becomes `Overflow`, with cleanup evidence discarded.**  
   `crates/via-adapters/src/runtime.rs:250` lets the force watch skip the post-Route drain, including when that drain would be empty. A successful Route result then becomes `Overflow` at `crates/via-adapters/src/runtime.rs:259`, with `cleanup: None` and `forced: false`.

   **Scenario:** force arrives during late terminal delivery or finalization. Route supplies terminal evidence, but Adapter manufactures an overflow; Core follows the ordinary failure path instead of forced shutdown. T3 §2 rule 4 requires `ForceStopped`. The problem is the wrong force disposition, rather than the absence of `completed`.

   **Instances:** Route returns success without reapplying force after `messages.finish` at `crates/via-routes/src/runtime.rs:150`, late cleanup at `crates/via-routes/src/runtime.rs:164`, and normal graceful close at `crates/via-routes/src/runtime.rs:359`.

   **Smallest fix:** preserve decoded data through bounded delivery, then apply force as `ForceStopped`, retaining Host cleanup, forced, and journal-failure evidence. Reserve `Overflow` for actual delivery failure.

6. **Blocker — forced handoff discards final text already received by Core.**  
   `crates/via-core/src/engine/drive.rs:676` hands off a forced turn before `settle_final_text` at `crates/via-core/src/engine/drive.rs:706`. The handoff at `crates/via-core/src/engine/drive.rs:735` carries no final-text state. Final shutdown constructs empty text at `crates/via-core/src/engine/stop.rs:389`.

   **Scenario:** Core receives FinalText `"done"`, then daemon force takes over. A daemon probe paused after delivery to Core, raised force, and inspected the durable result: `cancelled`, forced/quiescent, but `final_text: ""`.

   **Instances:** every forced handoff with accumulated final text, including spilled-text state and both ordinary and Store-failure shutdown resolution.

   **Smallest fix:** settle and carry final-text state through `ForcedTurn` into shutdown’s terminal construction. Preserve text-write failure classification. Force precedence does not require erasing already received output.

7. **Important — late delivery postpones forced cleanup and can consume two cleanup allowances.**  
   `crates/via-routes/src/runtime.rs:162` waits up to three seconds for delivery before initiating force-close. If delivery fails, `crates/via-routes/src/runtime.rs:189` creates a fresh cleanup deadline because the delivery failure carries no `close_by`.

   **Scenario:** the hop remains blocked after wall expiry. Vendor work continues during the delivery allowance, followed by another allowance for cleanup: potentially three seconds plus three seconds. The negative regression explicitly checks that the vendor remains alive 2.5 seconds after expiry.

   **Instances:** successful late delivery delays cleanup; failed late delivery additionally resets its budget.

   **Smallest fix:** begin forced cleanup immediately at wall expiry while delivering retained data concurrently, using one absolute deadline for delivery, close, and finish. Runtime §5.2 forbids extending vendor work or granting a fresh cleanup budget.

8. **Important — late delivery exhaustion is reported as a work deadline.**  
   `crates/via-routes/src/runtime.rs:540` returns `RouteError::Deadline` after an already decoded terminal cannot reach the hop. Core consequently reports a deadline failure; `crates/via-core/tests/route_stop.rs:443` explicitly expects that result.

   **Scenario:** the vendor completed, but bounded terminal delivery exhausts its allowance. This is delivery failure, not failure to complete vendor work. T3 §2 expressly prohibits returning `Deadline` for an already decoded terminal.

   **Smallest fix:** classify exhausted delivery as `Overflow` or an explicit delivery failure; apply `ForceStopped` when daemon force governs. Preserve the original absolute cleanup deadline.

9. **Minor — late-path regressions lack deterministic control ordering and latch coverage.**  
   `crates/via-core/tests/route_stop.rs:393` uses 500 ms gaps without acknowledging entry into late delivery or force observation. It accepts any error result and asserts completion only conditionally. The negative test’s probe at `crates/via-core/tests/route_stop.rs:425`, scheduled at `crates/via-core/tests/route_stop.rs:529`, also depends on a 500 ms margin. Neither regression sets the connection latch during late delivery.

   **Scenario:** scheduling delays invalidate the intended ordering, producing intermittent failures or allowing the positive test to pass without exercising force during late delivery.

   **Smallest fix:** acknowledge terminal decoding and late-delivery entry, independently acknowledge force/latch activation, then release delivery. Assert the exact disposition, cleanup evidence, and final text.

**Could not verify**

Physical corruption during terminal and Host-journal writes was assessed from source paths; those specific corruption cases were not executed. A dedicated late-delivery latch regression and repeated scheduling-stress runs were not available.

Executed checks passed: 119 component library tests, all 10 `route_stop` tests, the corrupt-acceptance regression, formatting, layer checks, and diff checks. Three isolated daemon probes reproduced findings 1, 2, and 6. Full workspace, Clippy, dependency, and release gates were not rerun.

Tracked files and Git state remain unchanged on `wt/s1-runtime2` at `b2eade0`.