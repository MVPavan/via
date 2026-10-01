**UNSOUND.** Reviewed `0529375..85f05c3` against the supplied sources, including the C2 amendments in `49df5f7`.

Findings below distinguish implementation defects from missing acceptance coverage. Paths are repository-relative.

1. **Important — `crates/via-core/src/engine/drive.rs:1593`; `engine/lane.rs:199` — Confirmed identity is only held in memory.**  
   `IdentityConfirmed` updates the lane and turn record without committing the identity observation or `session.opened`/`session.reopened`. A fresh Engine starts with no confirmed ID, so a resume after daemon restart can accept a different vendor session instead of reporting `resume_mismatch`. Transcript continuity is also lost. C2 requires confirmation to be durable before acceptance; H3 does not waive that requirement.  
   **Smallest fix:** commit confirmation through the session journal, then recover that committed identity when opening a lane after restart.

2. **Important — `crates/via-core/src/engine/drive.rs:1360`; `engine/lane.rs:76` — Core does not consume health independently.**  
   The running loop watches observations, orders, idle expiry and `run_turn`, but never `DriverHealth`. Health is inspected only when selecting/replacing a lane at dispatch. Consequently, `RetirementUncertain`, owned-task failures and `TurnAbandoned` have no independent Core reaction; an abandoned dispatcher also leaves no session monitor to consume its failure.  
   **Smallest fix:** add an owned session health consumer, preserving the first cause and applying C2 disposition without depending on observation delivery or a later dispatch.

3. **Important — `crates/via-core/src/engine/drive.rs:389`; `engine/lane.rs:205` — Failed-lane replacement can wait on its own capacity.**  
   Core reserves a new connection slot before reaching `lane()`, where the failed driver is closed. A persistent lane can retain its slot after `RetirementUncertain`. With all four slots occupied, its successor waits indefinitely and never reaches the close that would release the slot.  
   **Smallest fix:** retire a failed lane before reserving replacement capacity, while preserving cleanup ownership and confirmed identity.

4. **Important — `crates/via-core/src/engine/drive.rs:1351`, `:1618` — Session observations are not serviced correctly.**  
   Only `execute()` drains the receiver; the dispatcher exits on an empty queue. Between-turn denials, declines, identity and vendor-close observations therefore remain queued until another turn, replacement or shutdown. Additionally, `VendorClosed` and `Warning` are discarded even while a turn runs. The fake’s idle-close source makes the missing between-turn path reachable today.  
   **Smallest fix:** implement the specified session drain and session-observation commits, sharing ordered receiver ownership with the running loop.

5. **Important — `crates/via-core/src/engine/drive.rs:1622`, `:1711` — Both `LateTerminal` paths discard the result.**  
   Current and late observations both ignore it. AD4 requires a late terminal to revise an eligible `unknown` turn whose original `TurnEnd` retained no terminal. The comment explicitly acknowledges the missing revision write.  
   **Smallest fix:** implement the guarded revision transaction and `turn.revised` event; preserve turns that already retained a terminal.

6. **Important — `crates/via-core/src/engine/lane.rs:86`, `:109`, `:240` — Unmapped traffic becomes the current turn’s traffic.**  
   `attribute()` maps every unknown ID to `Current`. This includes genuinely unseen IDs, IDs evicted after 64 mappings, and previous IDs lost when replacing a lane. A late denial/decline can consequently enter the successor’s event attribution and envelope. C2 requires genuinely unseen traffic to be session-level and prevents expired ownership from becoming current traffic.  
   **Smallest fix:** distinguish current, late, session-level and expired attribution; preserve the necessary tombstones across replacement and reject/drop expired traffic safely.

7. **Important — `crates/via-core/src/engine/drive.rs:1433`; `engine/stop.rs:389` — Daemon force loses the retained vendor stop reason.**  
   When a `TurnEnd` carries a terminal plus `ForceStopped`, the force handoff drops the terminal. `Retained` keeps several terminal fields but omits its stop reason; forced finalization writes `vendor_stop_reason: null`. This breaks terminal retention when force arrives after decoding the terminal.  
   **Smallest fix:** carry the retained vendor stop reason through forced finalization and add the terminal-then-daemon-force case.

8. **Important — `crates/via-core/src/engine/terminal.rs:163`, `:185`, `:627` — Structured output breaks the envelope size guarantee.**  
   The maximum-envelope helper leaves `structured_output` null. An inline check measured **1,173,668 bytes** after adding 255 KiB of structured output to its maximum envelope. Reducing unsupported fields to the current fake shape and using 251 KiB still produced **1,052,290 bytes**, above Store’s **1,048,576-byte** limit. Debug assembly can panic; release assembly can reach Store with an oversized envelope.  
   **Smallest fix:** resolve the combined field budget within the fixed envelope limit, enforce it before commit, and include structured output in the combined-maximum test. The conflicting bounds need an explicit design disposition.

9. **Important — `crates/via-core/src/engine/terminal.rs:595`, `:614` — Definite vendor rejection loses its vendor code.**  
   `Rejected { reason: VendorError(code, detail), … }` is classified as `submit_failed`, but `failure()` is always called with `None` for `vendor_code`. The supplied code/detail survive only inside debug-formatted error text.  
   **Smallest fix:** preserve the vendor code and bounded detail when normalizing this rejection, alongside its existing evidence.

10. **Important — `crates/via-core/src/engine/progress.rs:334` — Usage component overflow silently reports a fabricated total.**  
    `saturating_add` converts an overflowing sum into `u64::MAX` while retaining reported provenance. StepTracker checks do not protect components such as `input` or `cached_input`; two representable samples can therefore produce an incorrect envelope figure.  
    **Smallest fix:** detect component overflow and propagate an explicit failure or unavailable component, rather than emitting saturation as an exact reported count.

11. **Important — `crates/via-cli/tests/s1_bounds.rs:518` — A migrated assertion was weakened.**  
    For an in-limit model, `passed.is_some() || member == "model"` accepts every error except the narrowly recognised size error. Returning `unknown_model`, `harness_unavailable` or another route refusal would still pass. The approved outcome change requires successful receipt.  
    **Smallest fix:** require no error for the model case; retain the existing route-refusal checks for unsupported fields.

12. **Important — `crates/via-core/src/engine/lane.rs:210`; `engine/recovery.rs:366` — Generic routing/recovery integration remains incomplete.**  
    The lane constructs a hard-coded fake `SessionRef` with an empty adapter version. Core recovery still uses anchor-page reconciliation without invoking per-session `AdapterSet::recover`. These leave the promised lane/recovery migration incomplete; H3’s version-storage workaround does not authorise inventing route identity.  
    **Smallest fix:** recover the frozen session reference and route session recovery through the adapter’s recovery operation using the reconciled Host facts.

13. **Important — `crates/via-core/tests/conformance_core.rs:558`, `:837`, `:931`; `conformance_driver.rs:1652` — Required boundary coverage is incomplete.**  
    Missing cases include:
    - mismatch **after acceptance but before terminal retention**, including typed error and evidence assertions;
    - Core’s **20-second post-acknowledgement cleanup** case beyond `close_by`;
    - persistent wall expiry with an earlier order whose `force_at` passes **before** the wall;
    - independent Core health reaction, including abandonment and retirement failure;
    - genuine end-to-end late durable delivery: test (14) manually supplies the mapping and observations inside Core.

    **Smallest fix:** add bounded tests for these specified boundaries, with independent expected outcomes and cleanup/evidence assertions.

Verification: **53/53 selected tests passed; 687/687 full failpoint tests passed, one skipped.** Formatting, failpoint Clippy, layer checks, guard self-test and diff checks passed. The literal guard reports the expected **97 remaining sites**. Git remained clean; no worktree VIA processes remained.

For **s1_f24**, both current runs passed: measured growth was **32,404 KiB** and **13,676 KiB**. The first is only **364 KiB below the limit**. Source comparison confirms the legacy runtime already had the one-message Route hop; the skipped flood text body is not retained by the new normalizer. Larger observation records, retained session channels and changed task scheduling introduce bounded metadata/allocator effects, but I could not attribute the reported +2–4 MiB mean growth to a specific cause.

**Could not verify:** the RSS regression’s causal explanation, runtime behavior at the missing boundaries above, historical RED execution, or fresh deny/release gates. H4 wiring and legacy runtime/type deletion were inspected without an additional defect found.