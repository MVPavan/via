# T3-0 design round 4: orchestrator decisions

Round-3 reviews of `design.md` at `4531dd0`: GPT-6 Astra medium
(`astra-review-design-r3.md`, UNSOUND, 5 majors, 1 minor) and Claude Fable
5.1 high (`fable-review-design-r3.md`, SOUND WITH CHANGES, 2 majors, 4
minors). Neither reviewer found a blocker, and both confirm all 18 round-3
decisions are present. One decision per finding or merged group.

1. **Incomplete keyed close passes the fence** (Astra 1). A keyed close
   whose intent row has no result and no close order goes through the
   closed-session check and the final-shutdown fence (step 4) before step
   5. Only two cases bypass the fence: replaying a committed result, and
   subscribing to an attempt already in progress. Test the keyed retry
   after the final start drain.
2. **Evidence before terminals** (Astra 2, Fable M1). The §6.8 pipeline
   runs in this order:
   1. join dispatchers and collect their handoffs;
   2. run Host reconciliation over the collected turns (`adapter.shutdown`)
      and gather its evidence;
   3. finalize exactly those turns with that evidence (forced terminals, or
      the latch batch);
   4. run the closure pass.

   State the reserve that reconciliation leaves before the final deadline.
   It must cover §7.4's 2 s bounds plus the closure pass; today's 1 s
   `FORCED_COMMIT_RESERVE` is not enough.
3. **Host's early stop is independent of Core and Store** (Astra 3). Runtime
   §7 requires Host to stop private groups within 3 s of failure
   notification without waiting for Store.
   - Host owns an early-stop task, subscribed to the daemon force signal
     (which the latch raises in phase one).
   - On that signal it stops every live group in its ledger through the
     verified live control, without Core, a dispatcher or Route being
     polled.
   - Host is the single owner of each anchor control. Route's stop requests
     go through the same owner, so the two stops cannot conflict and a
     second stop is a no-op.
   - Route and Core still own settlement and evidence. Final reconciliation
     supplies the absence proof.
   - Test: hold one turn's observation Store operation at a seam, latch
     through another turn, and prove the held turn's group is stopped (a
     Host early-stop acknowledgement and process absence) before the Store
     operation is released.
   - This is S1 work (`via-host`).
4. **Lock order for the durable closing set** (Astra 4, Fable m5). Short
   `std` mutexes (the durable closing set, `force_sessions` and similar) may
   be taken under `admission`, never the reverse. They are never held
   across an `.await` and never nested with another `std` lock. Three
   things happen atomically under `admission`: closing publication with
   its start registration, the plain-stop check, and removal after a
   confirmed `Closed`.
5. **Store error classification everywhere** (Astra 5, Fable m3). In §7.1,
   A14 and the tests:
   - for both the writer thread and the raw thread, only `try_send Full` is
     `NotEnqueued`;
   - `Disconnected` and a dropped reply are `WriterLost`, which latches;
   - raw `Full` and raw I/O errors are row 6.
6. **Close waiters always resolve** (Fable M2). Subscribe to the close
   watch under `admission`, then release it, then await. Before
   `slot.stop()`, a dispatcher that exits on force or the latch publishes
   `daemon_stopping` (force) or `store_error` (latch) on the close watch.
   Add both to §1 Wakes.
7. **Final-shutdown entry** (Fable m4). `enter_final_shutdown` runs when the
   daemon stops accepting work: at drain end, idle expiry, force acceptance
   or the latch's phase two. On the latch path, the diagnostic window then
   serves concurrently. The re-check at entry only feeds pipeline step 2
   and never returns to serving.
8. **Plain stop and durable closing** (Fable answer 3). A15 reads: a plain
   stop refuses while any session is active or durably `closing`.
9. **Test witnesses** (Astra 6, Fable m6):
   - add test-only acknowledgements for competing head acquisition and for
     Host-reconciliation entry;
   - place a `daemon.shutdown.entry` seam before `admission` is taken, and
     another after the fence is published;
   - drop "its anchor still answers" as a witness;
   - negative claims use ordering witnesses: a reply timestamp after the
     release acknowledgement, and for idle exit a bound of twice
     `VIA_TEST_IDLE_EXIT_MS` measured from the failed `Closed`'s
     acknowledgement.

Apply all of these, keeping lock order, wakes and interactions for anything
added, and update §11. Append a "Round 4" section to `reports/T3-0.md`
mapping each decision to design lines. S1 starts from the round-4 design
as soon as it lands.
