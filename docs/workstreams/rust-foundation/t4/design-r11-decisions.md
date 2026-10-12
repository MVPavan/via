# T4-0 design round 11: decisions

Input: `review-r11-sol.md` (Sol high, UNSOUND), reviewing `9af1b86`. Earlier
decisions stand except where changed here. Tag each change `[t4r11.N]`.

1. **Task 4 covers per-turn connections only** (B1–B3, Important 4–5).
   - Stage S1 has only per-turn private connections: the fake, and Claude, per
     `claude-code.md:61` "one process for one VIA turn".
   - Per-session connections exist only for OpenCode's dedicated server. By
     the rationale of the owner's `logs` decision ("Task 4 covers what S1
     has"), the per-session lifecycle moves to the OpenCode adapter task
     (`via-4sw.3.2`, which carries the constraints and Sol's r11 findings).
   - Remove from the design:
     - `TurnWire`/`TurnSender` per-session lending;
     - the per-session connection owner;
     - the raw boundary and barrier;
     - `raw_start`/`raw_end` spans;
     - per-session cancel, close and retirement;
     - the per-session tests and their fixture needs.
   - `logs` for a turn is the turn's own connection or connections. Paging
     uses a cursor per connection.
   - Replace the removed text with a short statement of the constraints the
     OpenCode task must meet:
     - cancel and close keep the server (`opencode.md` §597);
     - P7 settles before the terminal commits;
     - reusable-connection spans, where an unanswered end barrier retires the
       connection;
     - turn handles cannot outlive the turn or close the server;
     - D4 isolation;
     - T3 and C2 amendments for per-session turn failure.
   - Revert any A-amendment text that exists only for per-session behaviour.
     Q-R10-1 moves with it.
2. **Release the request charge before the reply** (Minor 6). A C1 request's
   line and decode charges are released once its DTO is built, before the
   reply charge is acquired, so the pool floor needs no concurrent request
   term. State this in §5.1, and add it to the class-charge test.

## Self-check before committing round 12

- Every round-11 finding is mapped: dissolved by decision 1, or fixed.
- The specs are re-grepped.
- There are no absolute paths and no "frame".
- Report the line count; it should drop.
