# W3-F: stop, shutdown and cancel correctness

Model: Opus 5.5 high. Follow `../w1/common.md`; report to
`reports/W3-F.md`. Findings in full: `../w2/sol-reviews/W1-D.md` (1–6) and
`../w2/sol-reviews/W2-E.md` (2). Runs in parallel with W3-G.

Owned: `via-cli` server and daemon main, Host shutdown state, and in
`via-core` stop, shutdown, forced turns and cancel records. W3-G owns Wire
raw appends and Core's observation-commit path; keep any shared hunk in
`via-core/src/engine.rs` small and name it in the report.

1. **Stop reaches daemon main before the reply write.** An accepted stop
   awaits the client write before notifying main, so a client that stops
   reading blocks final shutdown forever. Notify first; bound the write.
2. **No false clean exit.** Drive errors (`Ok(Err(ApiError))`) joined in
   the serving loop are discarded. Keep drive failures and unresolved
   receipted turns in the final disposition; check durable state before
   claiming clean.
3. **No invented acknowledgement.** Force records `acknowledged` when no
   anchor intent exists; C1 §7.4 reserves it for vendor evidence. Represent
   prelaunch cancellation without that claim, derive `forced` from Host
   force evidence (not group absence alone), and add a force-during-launch
   regression.
4. **Drain delivers committed results.** Final shutdown aborts clients as
   soon as drives settle, so a foreground `via spawn` waiting on `wait`
   can lose its reply. Let pending reads deliver committed results within
   the final deadline, then account for clients that remain.
5. **Host keeps failure facts.** A failed task join is counted locally and
   removed, so a later `Host::shutdown` reports zero failures. Keep the
   fact in Host state until final shutdown; test two shutdown calls after
   one failed join.
6. **Cancel lifecycle events.** Forced turns commit only `turn.ended`. Add
   C1 §6 `cancel.requested`, `cancel.settled` and (on force) `session.closed`
   in Core's durable transition, with dense sequence and event-range
   assertions.
7. **Deadline fills `cancel`.** A wall-deadline result has `cancel: null`;
   C1 §7.6 requires the evidenced cancel outcome and cleanup certainty when
   Core's deadline ends a running turn. Assert it end to end.
8. **Force after partial progress (from the W1-D/W2-E merge).** A forced
   turn now commits observations already buffered before `turn.ended`.
   Add an end-to-end test that forces a turn after observations were
   committed (dense seq, spans include them). Decide per C1 whether a turn
   whose Store commit already failed before the force ends `failed(store)`
   rather than `cancelled`, and test it.
