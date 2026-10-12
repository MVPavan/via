# T4-0 design round 5: decisions

Input: `review-r5-sol.md` (Sol high, UNSOUND), reviewing `ba7c8b2`. The
requirements (`requirements.md`) stay normative. Tag each change `[t4r5.N]`.
Keep the design as small as the requirements allow. Do not add a mechanism
where an existing one or a coarse bound suffices.

## Step rows

1. **No per-turn row cap** (Sol B1). Remove `STEP_ROWS_MAX`. Every completed
   step gets a row. Rows count against the disk quota (decision 4), and a
   quota refusal follows the quota's rules. A row is small, and a step is a
   whole model call, so there is no need for a count cap.
2. **Refused rows ride in the terminal** (Sol B2). Rows that are known not
   committed are carried into the terminal transaction, together with the open
   step's row. If the terminal commit also fails, T3's Store-failure rules
   apply, and the design makes no claim that the history is complete. State
   the resulting guarantee exactly: a committed `turn.ended` implies all rows
   of that turn are durable.

## Bounds

3. **Charge capacity, not length** (Sol B3; round-4 F1). Every growable
   buffer is charged at its maximum when it is created, and never grows past
   that maximum. This covers the stdout reader's message buffer and the C1
   line buffer: allocate with the cap, or use fixed charged segments. There is
   no growth-time accounting, and there are no coexisting old and new
   allocations. Show the arithmetic at the maximum connection and turn counts
   against 128 MiB; if it does not fit, lower a count and say which.
4. **Build the disk quota in Task 4** (Sol B4; answers Q-R5-6). Runtime §6
   already specifies the 4 GiB logical quota, and R7 keeps disk bounds.
   Include it and its lifecycle reserve. It covers raw logs, blob files and
   SQLite, including step rows.
5. **Bound the envelope directly** (Sol B7). Drop the extra 1 MiB
   event-payload cap. Envelope accumulation keeps its 1 MiB bound (R6), and
   disk is bounded by decision 4. On an overrun, record the failure and order
   cleanup immediately through turn control. Do not rely on a channel close
   that the adapter may never trigger.
6. **Failure summary: every member bounded** (Sol B8; round-4 F2). Bound each
   retained member independently, including caller-derived ones such as
   `bound` and `vendor_options`. Show the arithmetic from the C1 request
   limits.
   - Do not clear the denied and declined lists. Keep a bounded prefix of each,
     plus the total count and a `truncated` flag.
   - This is C1 §5's existing "bounded failure summary" on overflow, so it is
     R6's overflow case, not an exception to R6.

## Shared routes

7. **`logs` contract for shared and multi-turn connections now** (Sol B5; answers Q-R5-3).
   - When a raw unit is written, it is attributed to `(session, turn)` by the
     route that knows the attribution.
   - `logs` returns only units attributed to the requested session or turn.
     Unattributed shared traffic is never returned (D4 isolation).
   - Schema v6 carries the attribution now, so the vendor slices need no
     migration. The Codex and OpenCode implementations land with their
     adapter slices.
8. **Stall on a shared route** (Sol B6). On a private route, the 10 s stall
   fails the connection. On a shared route, it quarantines the affected thread
   per C2 §4 while other threads continue. Specify both outcomes.

## Progress snapshot

9. **A phase rule that corrects itself** (Sol I9).
   - `phase` is `tools` while the open-tool set is non-empty or its overflow
     count is above zero, and `model` otherwise.
   - At each model output, which is the step boundary, clear the open set and
     the overflow count.
   - `running_tools` holds at most 64 names, plus a `more` count.
   - Ignore ends with an unknown or duplicate id. Do not decrement any
     counter.

   Errors therefore last at most one step.
10. **Token accuracy is an owner decision** (Sol I10). Keep scope-labelled
    tokens. State, per vendor, what the vendor specs prove and what is
    unprobed, and list the probes needed. Make no accuracy claim that the
    evidence does not support. This goes to the owner in the consolidated
    report.
11. **No observation for noise** (Sol I12). Unknown or unattributed vendor
    messages produce no C2 observation. Last activity is a per-turn value
    updated only for attributed messages. Observations are sent only for
    lifecycle, safety and progress marks, and for the envelope's inputs.

## Interface and specs

12. **Resumable `logs` end** (Sol I13). While a connection can still grow,
    `logs` returns a resumable end cursor. `null` is returned only at a sealed
    end.
13. **Complete the amendment audit** (Sol I11). Add replacements for:
    - C1 §5 "raw_refs authoritative";
    - the Claude and OpenCode method-table `unsubscribe` rows;
    - Codex §5 late detail observations.

    Keep Codex's vendor `thread/unsubscribe` distinct from the removed C1
    `unsubscribe`. Grep every spec for each changed rule again.
14. **Status test** (Sol M14). Test that building the progress snapshot adds
    no extra Store read. Separately, test `status` latency under a bounded
    Store delay.

## Author's requirement corrections (report §9.4)

15. The design proceeds on these three clarifications. They go to the owner in
    the consolidated report as requirement text edits.
    - **R2's read list:** also correlation ids, acceptance and identity
      evidence, the denied, declined and steer payloads, and final text.
    - **R4's crash wording:** rows survive up to the last *committed* step.
    - **R6 against the transaction cap:** as A30.

The other open questions stay in the report for the owner: Q-R5-1, 2, 4
(now reduced by decisions 1 and 2), 5, 7, 8, 9 and 10.

## Self-check before committing round 6

- Re-read each changed section against the requirements and these decisions.
- Re-grep the specs for every amended rule.
- Confirm there are no absolute paths and no "frame".
- Report the line count; it should not grow without a stated reason.
