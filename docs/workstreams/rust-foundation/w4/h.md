# W4-H: force race, truthful force evidence, bounded client joins

Model: Opus 5.5 high. Follow `../w1/common.md`; report to
`reports/W4-H.md`. Findings in full: `../w3/sol-review-W3-F.md` 1, 2, 4.
Runs in parallel with W4-I. Owned: `via-cli` server/daemon main, `via-host`
(anchor and Host), Core stop/force. Keep shared hunks small and name them.

1. **Force right after a receipt.** On `Force` the serving loop breaks
   while a receipted spawn may still sit in `drive_rx`, outside the
   `drives` JoinSet; shutdown has no `ForcedTurn` for it, reports it
   unresolved and exits 4. Drain or classify queued drives before final
   shutdown. Regression: end to end, force immediately after the spawn
   receipt; the turn ends `cancelled` with truthful cancel fields.
2. **`forced` needs evidence of a live stop.** Host infers force from an
   empty 50 ms exit poll plus `Reply::Stopping`, which the anchor sends even
   if the vendor already exited (same on the released-control path). Have
   the anchor report whether its cleanup actually stopped a live group,
   and derive `forced` only from that. Regression: vendor exits between
   the last status poll and the stop.
3. **Client joins obey the final deadline.** After `abort_all()` the
   daemon awaits every client join with no remaining timeout. Bound it by
   the same absolute deadline, count what is still pending, and leave the
   rest to process exit.
4. **Force and in-flight output (from the W3 merge).** The biased force
   branch drops observations the adapter has not yet delivered; the
   force-stop e2e test raced it and failed locally (it now waits for a
   durable `assistant.text` before forcing). Decide per C1/C2 what force
   owes bytes already written by the vendor: at least the raw log must be
   complete or explicitly marked incomplete, never silently short. Add a
   regression that forces while output is in flight.
