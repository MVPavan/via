# T4-0 design round 1: decisions

Astra high (`review-r1-astra.md`) and Sol high (`review-r1-sol.md`) both
found the design at `df9b8d2` UNSOUND for implementation. The orchestrator
decided as follows. Each decision names the owning mechanism. Revise
`t4/design.md` and `reports/T4-0.md` to match, then tag each change `[t4r1.N]`.

## Contract conformance

1. **Large prompts: implement the blob path** (Sol 1, Astra 10, Q9).
   Runtime §8 requires Store-owned blob files for Store command payloads
   over 1 MiB: chunks are synced before the atomic row references them,
   recovery checks the checksum and length, and prompt, input-identity and
   large effective-params bytes all use this path. Task 4 implements it.
   Refusing a valid 16 MiB C1 request is not conformance, so there is no
   amendment and Q9's refusal is dropped. Assign the blob path to the
   Store-bounds slice, with failure-first tests: a torn blob, a checksum
   mismatch at recovery, and an unreferenced blob.
2. **Follow lag: the literal contract** (Sol 2, Astra 7, Q4). Replay and
   live follow alike use C1 §3.11 and runtime §9 exhaustion. On a full
   outbox, the subscription ends `lagged`, with one absolute 2 s release
   deadline for the notice and ownership. There is no pre-lag wait. Drop
   T4-A7.
3. **Follow registration order: C1's order** (Sol 6, Astra 6). Do the
   bounded Store read first, then register, then rescan durable
   `seq > scan_cursor` and check the durable head before any wait. The
   `Head` version watch is only a wake hint, never the source of truth.
   T4-A6 keeps only what is truly new and names every other change.
4. **Schema: implement runtime §6's Task 4 targets** (Sol 4). That means
   the `events` columns (FK turn, type, late, time), the `turns`
   event-bound columns and the `connections` table (raw paths, high-water
   offsets, open/sealed/incomplete state). Drop T4-A8. If one column is
   genuinely redundant, write a separate numbered amendment that proves
   its replacement query and its recovery behaviour. Replacing the
   `connections` table requires such a proof of durable connection state.
5. **Observation size: enforce C2 A1's 256 KiB per observation** (Astra
   10). Check the encoded size of every observation. Split text at UTF-8
   boundaries, in order. Any other oversize is a protocol failure, with raw
   evidence. An unknown payload keeps at most 16 KiB, with a truncation
   marker. The 1 MiB vendor-frame cap does not substitute for this.
6. **`logs` isolation: an explicit ownership check** (Sol 6). A raw
   reference is served only when its connection belongs to the requested
   session. Check this through the `connections` table (decision 4), not
   "by construction". Test a cross-session reference.
7. **`logs` lookup work is bounded independently of the response size**
   (Astra 9). Use an indexed or direct validated lookup, not a scan from
   the index start. Test a late reference in a long log while lifecycle
   commits wait.

## Ownership and concurrency

8. **Stall and control: the turn-control path owns stall expiry** (Astra
   1). The 10 s observation stall is delivered through the existing turn
   control, the stop order with `overflow` cause, and does not depend on
   further vendor output. While a delivery is pending, Route keeps polling
   stop changes, `force_at`, the daemon force and sticky health.
   Forwarding never disables the stop arm.
9. **Wire health: one owned first-failure state with a wake** (Astra 2, Sol
   5). Wire keeps the first classified failure (reader, raw worker or
   Store) in one health state and publishes it on a watch that every
   consumer selects on, independently of data capacity. The raw worker's
   failure keeps its classification rather than only setting `incomplete`.
   State how frames that are already durable are handled without delaying
   cleanup. Test a raw sync failure after a lone stderr chunk while stdout
   is quiet.
10. **Reader lifetime: stop, drain, barrier, join** (Astra 3). Readers stay
    alive through group stop and EOF or deadline. Then the raw barrier
    flushes, and the readers are joined. At the deadline: mark the log
    incomplete, abort, and join within the existing shutdown bound. Drop
    is an emergency fallback only, never the normal path. Keep a
    `WireParts`-equivalent control/data split if runtime §4 requires it;
    otherwise T4-A5 must prove that control stays independent.
11. **Memory: acquire before allocating, for every retained
    representation** (Astra 4). Specify which `BytePool` class each
    retained form charges and when: partial stdout frames, decoded JSON
    (charge by AST node count as well as bytes), normalized copies,
    observation items and envelope accumulation. Say where each permit
    transfers or drops. Do not claim F24 conformance until every form is
    charged.
12. **Reserved Store capacity, end to end** (Astra 5, Q2).
    - Lifecycle work gets reserved slots **and** reserved bytes inside the
      global budget.
    - Ordinary commits get protected byte capacity that Public reads
      cannot consume.
    - The reads that failure resolution depends on go through protected
      admission.
    - Prove that 8 lifecycle slots cover the latch batch and concurrent
      lifecycle writes, or give the latch its own slot.
13. **Store lane locking and shutdown** (Astra 8).
    - Lock order: an async owner's lock (for example `Head`) may be held
      while briefly taking the `Lanes` mutex; the reverse and any callback
      under `Lanes` are forbidden.
    - Specify the condvar predicate loop.
    - `Shutdown` is an admission fence: new work is refused, and accepted
      work drains first.
    - Define how writer death is published and how pending replies are
      failed.
14. **Follower lifetime: one synchronized subscription state** (Astra 6,
    7).
    - Registration is an atomic get-or-create through the existing
      slot/`Head` mechanism, so an idle or evicted session works.
    - Releasing a lease triggers a retirement check.
    - Enqueue checks a closed flag and a generation under that state, so a
      stopped follower cannot enqueue again.
    - The serializer fixes `resume_after` only after it finishes any frame
      it has started.
    - One absolute socket-termination deadline applies, and the reader,
      handler and writer stay serviceable concurrently.
    - A follow that starts at or beyond the terminal ends at once, using
      the Store snapshot's terminal metadata.
15. **Session counts: `Unresolved` owns "active"** (Astra 11, Q11).
    Active and unresolved sessions derive from the existing `Unresolved`
    owner, not from slots. The open-session tally changes exactly once per
    transition: spawn opens a session (resume and replay do not), and
    every close path closes it once, including standalone close,
    terminal-bearing close and force closure in `stop.rs`. The tally is
    seeded from the Store at startup. Count snapshots are read coherently.
16. **The `list` cursor** (Astra 12). Page with
    `updated_at < t OR (updated_at = t AND id > id_cursor)`. For
    concurrent updates, either specify a snapshot or version strategy that
    satisfies C1 §3.10's reachability, or propose a numbered amendment
    with the exact guarantee.
17. **`status` from durable fields** (Sol 3). Spawn stores `cwd` and
    `allow_untested`; they are not frozen today (`receipt.rs:140`). Every
    C1 §3.7 member gets a durable source and a bounded read that works
    after slot eviction and after restart, with tests for both.

## Inventory and tests

18. **Correct the inventory** (Sol 7).
    - Events carry a nullable `turn`: this is verified, so drop the [I]
      marker.
    - `at` is formatted as fixed-width UTC, but its order is not a commit
      order, so no design may rely on string order of `at`.
    - "Every writer goes through `Head`" is false for the initial spawn
      event. Scope the claim to post-spawn commits and list those writers.
19. **Tests.**
    - Add a sync-count seam for the group-commit assertion.
    - Show that the observation budget admits items beyond today's
      64-item channel.
    - Use no fixed sleep as an ordering assertion.

## Slice plan

20. **Re-derive the slices** (Astra 11). Assign `stop.rs`, `close.rs`,
    `journal.rs` and every `drive.rs` hook explicitly. Anything that two
    slices both touch is either moved into the earlier slice or run
    serially. The blob path, the schema targets and the reserved
    capacity belong to the Store-bounds slice.

## Open questions settled

| Q | Decision |
|---|---|
| Q1 | Keep the immediate framed overflow. Health and control bypass it (decision 8). |
| Q2 | Decision 12. |
| Q3 | A refused Public read is `admission_refused`. Writer loss and corruption keep T3's classification. |
| Q4 | Decision 2. |
| Q5 | Seeded generators that cover boundary cases and report their seed. No new dependency. |
| Q6 | Use `JoinSet` and `watch`, with explicit cancellation and bounded joins. Align the coding-style wording through an amendment line in the design. The orchestrator edits coding-style later. |
| Q7 | 3 600 000 ms. Tests that need shorter deadlines request them. |
| Q8 | Defer. Every request that needs it is explicitly refused. |
| Q9 | Decision 1. |
| Q10 | Nested `null` is `invalid_params`. |
| Q11 | Decision 15. |
| Q12 | Constant, check only. |
| Q13 | Close without bytes. |
