# T3-0 design round 6: final orchestrator decisions

Round-5 reviews of `design.md` at `bf27db6`, the final narrow design pass:

- GPT-6 Astra medium (`astra-review-design-r5.md`): SOUND WITH CHANGES, 1
  major (a test precondition) and 1 minor.
- Claude Fable 5.1 high (`fable-review-design-r5.md`): SOUND WITH CHANGES,
  2 majors (both S1 Host) and 5 minors.

Neither found a blocker. After this round the design is final. Later
findings are handled in the slices' code reviews. Decisions 1–5 go to S1
directly.

## S1 (via-host, fixtures)

1. **No `Stop` to a pre-ARM anchor** (Fable M1). The anchor's pre-ARM loop
   treats `Stop` as an invalid control and exits 1.
   - A ledger entry has a phase: `verified`, `arming` or `armed`. The early
     stop sends `Stop` only to `armed` entries.
   - The owner's ARM gate reads `stopping` under the ledger mutex, atomic
     with the snapshot. If it is set, the gate refuses with
     `HostError::Stopped` and the EOF drop of row 4.
   - The owner marks the entry `armed` under the ledger mutex immediately
     after `Spawned`, before `commit_vendor_facts`. If `stopping` is set,
     the owner sends `Stop` itself under the original deadline.
   - Any acquisition failure after `stopping` was observed returns
     `HostError::Stopped`.
   - Tests:
     - pause at the snapshot while a turn's ARM completes, and expect
       `host.early_stop.sent`;
     - a pre-ARM variant expects `Stopped { launched: false }` and no
       `Stop` frame.
2. **The early-stop task is not pending cleanup** (Fable M2). The task is
   excluded from the pending-cleanup count and from the idle predicate;
   only the Host shutdown join counts it.
3. **Evidence test keeps the anchor alive** (Astra 1, Fable m3). For the
   positive ordering case, a fixture seam in the anchor defers
   `begin_cleanup` until reconciliation's `Stop` (identified by count) and
   withholds `stopped_live` until then. Route's close then reports
   `uncertain`, and reconciliation supplies both facts before the terminal
   commit. The variant that loses all stop evidence still expects
   `unknown`.
4. **Classification wording** (Fable m4). The unit-test row reads: "the
   SQLite writer's `try_send` `Full` gives `NotEnqueued`".
5. **Witness for concurrent stops** (Fable m5). The fixture anchor holds the
   busy control's `Stop` at a named pause. The test asserts that the other
   group's `host.early_stop.sent` acknowledgement arrives while that pause
   is still held.

## S2

6. **The close-watch race seam** (Fable m6). Name the seam for the pause
   between order check and subscription in §10. State the two constraints
   in the test row: force is already accepted when the caller enters, and
   the racing publication is a force or latch exit.

## S3

7. **Diagnostic window** (Astra 2). §7.4 starts the window at
   `enter_final_shutdown`, triggered by phase one's force signal, without
   waiting for phase two.
8. **Client and Store joins** (Fable m7). They run after the pipeline and
   take whatever time remains. A pipeline that runs past `deadline − 2 s`
   already exits 4. Say so in the budget table.

Apply these to `design.md` and append a "Round 6 (final)" section to
`reports/T3-0.md`. No further design review.
