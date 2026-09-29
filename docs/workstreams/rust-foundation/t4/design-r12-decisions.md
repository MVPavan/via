# T4-0 design round 12: decisions

Input: `review-r12-sol.md` (Sol high, UNSOUND: 1 blocker, 1 minor), reviewing
`ba778e4`. Earlier decisions stand. Tag each change `[t4r12.N]`. Targeted
fixes only.

1. **The raw worker owns the seal** (blocker).
   - When `finish` reaches its deadline with the barrier unanswered, Core
     enqueues a `Seal(connection, offset)` command to the raw worker at the
     last proven offset.
   - The raw worker applies it in queue order. It discards every append for
     that connection that would land beyond the seal, and truncates the file
     back to the seal if needed, so no durable byte is ever past a sealed end.
   - Loss evidence is exact: Wire counts this connection's enqueued but
     unacknowledged units. The terminal carries `raw_log.incomplete` only if
     that count is above zero at the seal. Those units are then discarded by
     construction, which is a real loss and matches A27's meaning.
   - If the seal command itself cannot be enqueued or applied, the connection
     latches as a raw-store failure under existing rules.
   - Test: a raw worker that resumes after the `finish` deadline, holding a
     queued append. Afterwards, no durable byte is beyond the seal, `logs` is
     consistent, and `raw_log.incomplete` is set. Add a zero-pending case,
     where the flag is not set.
2. **`finish` is only for opened connections** (minor). The "every `run_turn`
   exit calls `finish`" rule applies once `open_connection` has returned. State
   who cleans up a partly failed open: the existing Host and Wire open-failure
   path, cited by file:line.

## Self-check before committing round 13

- Both findings are mapped.
- A27 is re-grepped for the seal and loss rule.
- There are no absolute paths and no "frame".
- Report the line count.
