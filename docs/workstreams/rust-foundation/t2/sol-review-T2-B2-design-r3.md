1. **Mostly incorporated.** The latch, removal of orphan retries, force closure rule, and bounded start handoff are stated. The remaining gaps are at the Store-failure boundary, so the note does not yet fully establish runtime §7 behavior.

2. **Two unsafe paths remain:**

   - An uncertain `resume` receipt may have committed a queued turn, but the design deliberately does not register it in memory. After the latch, the existing forced-turn shutdown path can commit `session.closed` with the running turn’s terminal (`crates/via-core/src/engine/stop.rs:196`). The note does not establish how closure proves that the possible queued turn was disposed of.
   - A grant can precede the latch, and Route’s force check can precede it too. During acquisition, Wire continues polling Host after force; Host can still send ARM (`crates/via-wire/src/runtime.rs:205`, `crates/via-host/src/host.rs:492`). The stated watch check does not fence a vendor launch after the latch.

   Confirmed receipts and failed terminals remain counted under the stated rules; the latch makes exit 4 mandatory. I found no omitted **Core** state-write category in the note’s list.

3. **SOUND WITH CHANGES.**

   - In Store-failed mode, never commit `session.closed` unless Store positively proves every turn, including any uncertain receipt, has a durable disposition. The simplest rule is to omit `session.closed` from Store-failed forced terminals and exit 4.
   - Serialize launch authorization with the latch at the last pre-ARM gate: if the latch wins, Host must not send ARM; if ARM wins, treat the launch as in flight and clean it up.