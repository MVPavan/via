UNSOUND

The round-one fixes address the original examples, but the full sweep still finds outcome loss, cleanup failures accepted as complete evidence, and teardown paths outside the shared deadline. All paths below are relative to `crates/via-cli/tests/`. Findings include pre-existing instances, as requested.

1. **Blocker — restarted daemons silently retain an earlier cleanup report.**  
   **Sites:** `crates/via-cli/tests/support/daemon.rs:284`; restart instances in `s1_daemon_config.rs:448,710,894,965,1330,1487` and `s1_progress.rs:1087,1325`.  
   `create_new(true)` fails once an earlier guard wrote `cleanup.json`; the failure is ignored. Collection subsequently validates the earlier daemon’s reap and anchor inventory. A later daemon can remain unreaped or have unverified cleanup while the scenario accepts the earlier successful report. Startup also truncates the shared trace at `support/daemon.rs:191`.  
   **Smallest fix:** retain a report and trace for every daemon generation, and validate the final cleanup plus every recorded teardown failure.

2. **Blocker — the absence predicate accepts invalid full identities.**  
   **Site:** `crates/via-cli/tests/support/outer_cleanup.rs:127`.  
   Presence of optional fields, `pgid > 1`, and a nonempty generation are insufficient validation. Unlike production’s predicate, this accepts PID zero, start ticks zero, and an empty marker. A direct probe using those values returned `quiescent` and `absence_proven: true` after `ESRCH`. That violates §5.2’s requirement for validated full identity.  
   **Smallest fix:** apply the production identity-validity checks before connecting or probing; invalid identity must remain uncertain.

3. **Blocker — the Store waiver also waives cleanup failure and deadline enforcement.**  
   **Sites:** `crates/via-cli/tests/support/evidenced.rs:439`, `:448`.  
   With `store_expected == false`, any `store_evidence` error becomes `not_collected`, which is accepted as successful cleanup. This can erase an already-unverified anchor result. The `no_store` branch also bypasses the final deadline check. Direct probes produced `outcome: pass`, `evidence_complete: true`, and no cleanup failure for both cases.  
   **Failure scenario:** anchor verification fails, then an evidence query fails; or collection reaches the no-Store branch after teardown expires.  
   **Smallest fix:** separate evidence waivers from cleanup proof, preserve verification results through collection errors, and enforce the shared deadline on every branch.

4. **Blocker — real timeout paths still become ordinary failures.**  
   **Owning sites:** `s1_turn_control.rs:224,288,342,379,435,778,1337`; `s1_lifecycle.rs:65,99,457,553,561,1055`; `s1_store_failure.rs:65,99,423,539,547`; `c1_protocol.rs:164,290`; `route_drain.rs:114,191`.  
   These paths return strings, aggregate transport errors into assertion failures, or panic on timeout. `crates/via-cli/tests/support/evidenced.rs:53` recognizes only an intact `ScenarioError`; the remaining paths consequently record `fail`. The negative-request helpers also explicitly classify timed-out commands as failure at `support/daemon.rs:346` and `s1_prompt_to_result.rs:327`.  
   **Failure scenario:** a command or readiness wait times out and its artifact records an assertion failure instead.  
   **Smallest fix:** preserve typed timeout errors through these owning helpers and aggregators; classify timeout before interpreting a refusal response.

5. **Important — output collection can replace an already-known command outcome.**  
   **Sites:** `support/daemon.rs:304,333`; `s1_prompt_to_result.rs:205,321`; `s1_recovery.rs:126`; `s1_crash_points.rs:126`; `s1_daemon_stop.rs:107`; the timeout self-test at `scenario_runner.rs:81`.  
   Each writes evidence before checking the captured timeout or exit failure. A write failure therefore returns infrastructure failure and loses the known command outcome. Similarly, `crates/via-cli/tests/support/scenario.rs:72` can replace a command timeout with a kill/reap error at `:81`.  
   **Smallest fix:** retain the captured outcome first, then attach output-writing and reap failures separately.

6. **Important — evidence collection can prevent cleanup or erase its result.**  
   **Sites:** `crates/via-cli/tests/support/evidenced.rs:119`, `:405,420,422,426,431,436,442,469–472`.  
   An unsuccessful exit proof skips cleanup reporting entirely. Trace/log/folder collection runs before anchor cleanup and can fail or consume its remaining budget. After verification, backup/query/folder errors can discard the anchor result before `cleanup.json` is written. This contradicts the sweep table’s claim that evidence copies follow recorded cleanup. A failed-exit probe confirmed that no `cleanup.json` was emitted.  
   **Failure scenario:** a trace read fails, or a launched folder is missing while anchor cleanup is also unverified; only the collection error survives.  
   **Smallest fix:** perform and record cleanup independently first, including explicit incomplete records when exit or snapshot acquisition fails; collect evidence afterward and accumulate both failures.

7. **Important — killed stop-command children are not reliably reaped or recorded.**  
   **Sites:** `crates/via-cli/tests/support/evidenced.rs:303`, `s1_crash_points.rs:342`.  
   `run_within` kills after exhausting its deadline, immediately reuses that expired deadline, and ignores the reap result. Its callers receive no failure. `PendingClient::drop` likewise ignores an unsuccessful bounded reap. A normal `sleep` child—not a D-state child—was left as a zombie by the direct probe.  
   **Affected callers:** sandbox stops in `route_drain.rs:214`, `s1_turn_control.rs:128`, `s1_lifecycle.rs:159`, `s1_store_failure.rs:146`; daemon guards at `s1_turn_control.rs:475`, `s1_lifecycle.rs:610`, `s1_store_failure.rs:613`.  
   **Smallest fix:** reserve a bounded reap allowance within the outer deadline and return/record its result.

8. **Important — explicit final-stop helpers still restart teardown budgets.**  
   **Sites:** `s1_turn_control.rs:443,453`; `s1_lifecycle.rs:422,590,412`; `s1_store_failure.rs:575,590,394`.  
   These allow independent command/exit waits, then a fresh ten-second anchor budget; sandbox drop starts another final budget afterward. They are used at scenario exits, not solely for mid-test assertions. A shutdown taking fifteen seconds can finish successfully and leave final collection green.  
   Independent guard deadlines also remain at `support/daemon.rs:242`, `s1_recovery.rs:410`, and `s1_crash_points.rs:477`; retained guards in restart scenarios each start another final cleanup phase.  
   **Smallest fix:** distinguish deliberate intermediate shutdown from final teardown explicitly, and make every final helper/guard join the scenario’s single deadline.

9. **Important — sandbox ordinary-stop attempts can consume the entire ten seconds.**  
   **Sites:** `crates/via-cli/tests/support/evidenced.rs:263`; callbacks at `route_drain.rs:214`, `s1_turn_control.rs:128`, `s1_lifecycle.rs:159`, `s1_store_failure.rs:146`, `c1_protocol.rs:86`.  
   These receive and use all remaining time rather than §11.2’s two-second ordinary-stop cap. A stalled stop consumes the fallback/anchor budget.  
   **Smallest fix:** cap the ordinary-stop phase at `min(2 s, remaining)` and retain the outer deadline for subsequent phases.

10. **Important — blocking reaps remain throughout cleanup and timeout fallbacks.**  
    **Sites:** `s1_turn_control.rs:78`; `s1_lifecycle.rs:64,690`; `s1_store_failure.rs:64`; `route_drain.rs:113`; `c1_protocol.rs:542`; `s1_crash_points.rs:322,449,1037`; `s1_recovery.rs:400`; `s1_daemon_stop.rs:801,846`; `s1_c1_intake.rs:1912`.  
    The collector’s own fixture reaps also block at `evidence_collector.rs:262,290,296,401,432,447,459,521,553`, with blocking joins at `:460,571`.  
    **Failure scenario:** a killed child remains uninterruptible; `Child::wait()` prevents the fallback or review regression from returning within its bound.  
    **Smallest fix:** use deadline-aware polling consistently and preserve an explicit unreaped result. This is broader than the report’s “every reap now polls” claim.

11. **Important — command-runner setup and reap allowance are outside the supplied bound.**  
    **Sites:** `crates/via-cli/tests/support/scenario.rs:59`, `:65,66,75`.  
    Temporary-file creation and spawn precede the deadline; a timed-out command receives another independent second for reap. The teardown guards call this with the outer time remaining, so the helper can exceed that deadline. Slow spawn or filesystem setup is also outside its purported bound.  
    **Smallest fix:** pass an absolute deadline covering setup, execution and reap; supervise potentially blocking setup if the ten-second return guarantee is required.

12. **Important — SQLite snapshot waits ignore the outer deadline.**  
    **Site:** `crates/via-cli/tests/support/outer_cleanup.rs:57`.  
    `snapshot` has no deadline argument and always permits a one-second busy wait. An exclusive-lock probe took **1,001 ms**. Called with only milliseconds left, it overruns teardown before the later check can record failure; row scanning is also unchecked until it finishes.  
    **Smallest fix:** pass the shared deadline, cap SQLite waits by remaining time, and bound/interrupt snapshot processing. Recording an overrun afterward does not bound the operation.

13. **Important — unreadable process metadata is accepted as absence.**  
    **Sites:** `support/evidenced.rs:395`; `s1_turn_control.rs:547`; `s1_vendor_pipeline.rs:279`; `s1_lifecycle.rs:115`; `s1_store_failure.rs:1663`; `s1_recovery.rs:637`; `s1_daemon_stop.rs:368`; `s1_progress.rs:454`.  
    These turn arbitrary read errors—and several turn malformed metadata—into “exited.” A permission/I/O error can make F19/F24 record immediate absence while the process survives; later outer cleanup can remove it and leave the scenario passing. The same issue weakens the daemon exit proof.  
    **Smallest fix:** return a fallible observation; accept disappearance only for genuinely vanished processes, and propagate unreadable/malformed state as uncertainty.

14. **Important — other cleanup waits still accept observations after their deadline.**  
    **Sites:** successful reap observations at `support/evidenced.rs:314`, `support/daemon.rs:229`, `support/scenario.rs:68,77`, `s1_lifecycle.rs:84`, `s1_store_failure.rs:84`, `s1_recovery.rs:483`, `s1_crash_points.rs:549`, `s1_daemon_stop.rs:248,841`, `s1_turn_control.rs:431`, `s1_prompt_to_result.rs:74,91`.  
    Absence waits have the same order at `s1_lifecycle.rs:97`/`:122`, `s1_store_failure.rs:97`, `s1_recovery.rs:660`, `s1_daemon_stop.rs:381`, and `s1_progress.rs:454–460`.  
    **Failure scenario:** polling resumes after expiry and finds exit/absence; success bypasses the deadline check. Some final anchor checks catch this, but explicit shutdown/assertion paths with fresh budgets do not.  
    **Smallest fix:** timestamp after observation and enforce the applicable deadline before returning success, as the revised F19/F24 helpers do.

15. **Important — deadline regressions can pass the new regression tests.**  
    **Sites:** `crates/via-cli/tests/evidence_collector.rs:628`, `:659`; collector deadline tests at `:420,540`.  
    The 300 ms connect test permits a timeout response almost five seconds later. The exchange test checks only `None`, not return time. A scratchpad mutation restoring repeated 300 ms syscall timeouts still passed the exchange test after **1,405 ms**. The collector tests likewise reject late proof without asserting bounded supervisor return.  
    **Smallest fix:** measure each supervised operation separately and assert return within its deadline plus a named, justified scheduling tolerance.

16. **Important — evidenced guards omit mandatory direct-child teardown evidence.**  
    **Site:** `crates/via-cli/tests/support/evidenced.rs:442`.  
    It writes only `{"anchors": ...}`. Guards in `c1_protocol.rs:178`, `s1_turn_control.rs:470`, `s1_lifecycle.rs:605`, and `s1_store_failure.rs:608` retain no structured child reap, stop, kill or phase timing record. Successful collection therefore cannot supply §11.2’s required direct-child reap status; failed exit proof emits no cleanup record at all.  
    **Smallest fix:** accumulate guard records in `Teardown` and include them on every cleanup-report path.

17. **Minor — C1 teardown requests plain stop instead of force-stop.**  
    **Site:** `crates/via-cli/tests/c1_protocol.rs:123`.  
    The request omits `params.force`. If active work exists, plain stop is refused and teardown waits for fallback instead of attempting the prescribed force-stop.  
    **Smallest fix:** send `"params":{"force":true}`.

18. **Minor — pre-ARM records are connected to during recovery cleanup.**  
    **Site:** `crates/via-cli/tests/support/outer_cleanup.rs:140`.  
    The persisted phase is never checked before `challenge`. A valid `identified` row therefore causes a recovery connection despite §5.1 expressly allowing recovery connections only for `arm_intent`.  
    **Smallest fix:** skip control connection for pre-ARM phases and use the permitted independent absence observation.

**Could not verify**

- No native D-state or blocked `/proc/environ` reproduction. The C1 blocking-connect and `/proc/environ` limitations remain accepted exceptions.
- Dropping a live daemon mid-test without restarting can shorten final teardown and cause a false failure. I found no evidence that this concern hides incomplete cleanup.
- Full-workspace acceptance was not established: the permitted scratchpad `TMPDIR` run had **521 passes, 36 Host failures and one Host timeout**. A shorter `/proc` path retry also failed harness validation; those results are not attributed to this chunk.
- Verified: **23 focused tests passed; all 265 CLI failpoint tests passed, one skipped; formatting, test-target Clippy and diff checks passed.** Probes and mutation evidence are in `scratchpad/s1-evidence2-review`.
- Tracked files and Git state remain unchanged at `a45ef81`.
