# Task 3 owner decisions (2026-09-28)

The owner chose the orchestrator's recommendation on all three questions.
The analysis is summarised under each decision.

## O1. Store failure policy: scope clean failures; latch only on unknown state

A Store write failure no longer stops the daemon by itself. Everything
shares one SQLite file, one writer thread and one disk. Any daemon exit
loses every running agent, because agents talk over pipes the daemon owns.
Today, one failed write anywhere therefore ends every running agent's work.

A failure whose durable outcome is **known**, meaning SQLite rolled the
transaction back, is scoped to the request or turn that made it. A failure
whose outcome is **unknown** keeps runtime §7's latch (force stop, exit 4,
restart recovery). No in-daemon repair or reconciler is added.

A degraded daemon mode for persistent clean failures (for example, a full
disk) is deferred until after S1. Revisit it if testing shows full disks or
transient I/O errors restarting the daemon.

Decisions on design §7.2 (D1–D13):

- **D1, known not committed.** Only that request or turn fails; nothing
  latches.
  - A receipt (`spawn`/`resume`) gets `store_error` with
    `commit_outcome: not_committed`.
  - Submission intent or anchor intent: no agent I/O happened. The turn
    ends `failed(store)`, and its successors dispatch normally.
  - Acceptance, an event or `cancel.requested` on a running turn: the turn
    is stopped (a stop order with cause `store`) and ends `failed(store)`
    with its cleanup evidence.
  - A natural terminal: retried once with the same content.
  - **Escalation:** the one resolution write after a turn's first clean
    failure (its `failed(store)` terminal, or the terminal retry) latches
    if it fails in any way. A persistently full disk therefore still ends
    in the latch.
  - A sequence number whose event did not commit is not consumed. Specify
    the head handling.
- **D2, uncertain outcome.** As today: latch, and restart settles it. Store
  keeps mapping errors from the commit step to `Uncertain`; do not
  reclassify them without SQLite evidence. Split `Unavailable`: a request
  that was never enqueued is known not committed (D1); a writer lost after
  enqueue is uncertain (latch).
- **D3, refusals.** After a scoped failure, only the affected turn is
  refused or failed. The session continues, and no other mutation or grant
  is refused. After the latch: as today, daemon-wide.
- **D4, failure resolution.** For a scoped failure, resolution is the
  turn's ordinary terminal write (`failed(store)` and cleanup evidence).
  For the latch, use the design's original §7.4 batch from `42993bc`: the
  2 s re-read, one batch, and the affected session's queued turns
  cancelled (A11). A full channel counts as a skipped batch.
- **D5.** The 5 s diagnostic window and 10 s bound apply to the latch path
  only (original §7.3).
- **D6, status.** `health` is `healthy` until the latch and `store_failed`
  after it. The additive `store_failure` field (A9) reports the latest
  failure: `{kind, scope: request|turn|session|daemon, since, count,
  affected (first 16 addresses and a count)}`. It has no prompts, payloads
  or handles.
- **D7.** `event_end: store_error` goes to Task 4.
- **D8, reads and corrupt rows.**
  - A dispatcher whose reads fail continuously for 10 s fails its head
    turn `failed(store)` without agent I/O; if that write fails, it
    latches.
  - An application-level corrupt row (a present but unparseable frozen
    value) fails that turn `failed(store)`, both live and in the restart
    handoff. It is not a startup refusal.
  - SQLite-level corruption (`SQLITE_CORRUPT`/`NOTADB`) latches, and at
    startup it is F11's refusal.
- **D9, startup.** A write failure in startup recovery or the restart
  handoff still fails startup; no dispatch runs from an uncommitted
  recovery view. The D8 corrupt-row case is the exception.
- **D10, Host journal writes.**
  - Anchor intent not committed: no process; the turn fails (D1).
  - Anchor identified, ARM intent or vendor facts not committed: stop the
    group using the in-memory identity and prove absence within a bound,
    and the turn fails `failed(store)`. If absence is not proven, the slot
    stays held and re-probe owns it.
  - A group-absence write not committed: the slot stays held, and
    re-probe retries the write.
  - Uncertain outcomes: latch (D2).
- **D11, raw writes.** A raw append or sync failure fails that connection
  and turn (`failed(store)`, plus a durable incomplete record). If that
  record cannot commit, it latches (runtime §7, first paragraph).
- **D12, close-bearing commits.**
  - `Closing` not committed: reply `store_error`; no state.
  - `Closed` not committed: `closing` stays durable, the reply is
    `store_error`, and a later `close` retries.
  - After the latch: as today.
- **D13.** After the latch, `cancel` and `close` return `store_error`; the
  latch's force stop performs the cleanup.

Contract amendment: runtime §7's first paragraph and its table change to
this split. The design gives the exact replacement text as an amendment.

## O2. `daemon stop --drain` does not close sessions

Drain finishes accepted turns and stops the daemon. Sessions stay open and
resumable after restart. Drain's "closing" is the daemon-lifetime
`daemon_stopping` refusal (the design's A5 recommendation). C1 §7.1 drops
"drain completed" as a cause of `closed`.

## O3. `daemon stop --force` closes only interrupted sessions

Force closes (`session.closed`, `reason: "daemon_stop_force"`) only
sessions that have a running, claimed, cancelling or queued turn when
force is accepted. Idle sessions stay resumable. The carried item "force
closes already-idle sessions" is dropped, along with §6.3's idle-session
pass. Amend C1 §3.14 and §7.1 to "every session with unfinished work".
