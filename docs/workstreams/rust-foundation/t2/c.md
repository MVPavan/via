# T2-C: restart handoff and Task 2 integration

Model: Opus 5.5 high, continuing the T2-B2 cloud session on its branch
(`claude/t2-b2-step-1-3pslux`; merge `origin/rust-foundation` first). Follow
`../w1/common.md`; report to `reports/T2-C.md`. Design:
`dispatch-design.md`; recovery: `crates/via-core/src/engine/recovery.rs`.

T2-A (startup recovery) and T2-B2 (dispatcher, Store-failed latch) are
merged. What neither owns yet: turns that a restarted daemon finds still
`queued` in Store. Today they have no dispatcher, and queued successors of
a recovered `unknown` turn are not cancelled.

## Step 1: design section, then continue

Add a short normative "Restart handoff" section to `dispatch-design.md`,
commit and push it, then implement without waiting (the orchestrator has it
reviewed in parallel and messages you only if something changes). It must
define, per C1 §7.5 and runtime §7 (restart paragraph):

- After recovery commits and before admission, every durable `queued`
  turn is read with a paged, bounded-memory Store read. A turn with an
  `unknown` predecessor, including one recovery just settled, is committed
  `queued → cancelled` (C1 §7.2, P6). Every other one is enqueued in its
  session's slot, counted in `queued`, `active` and unresolved, and its
  dispatcher is started through the start channel. No `unknown` turn is
  resent.
- A Store failure during the handoff fails startup (runtime §7); the
  daemon never admits on a partial handoff.
- What happens when the surviving queued count exceeds the daemon-wide
  bound: all are still counted, and admission refuses new turns until the
  count is back under it.
- The §2.2 "earlier daemon left it" indeterminate case, which this
  resolves.

## Step 2: implementation and tests

Real daemon and SQLite, using `test-failpoints` where timing matters:

1. A crash with a running turn and a queued successor. On restart the
   first is `unknown` / `daemon_restart`, the successor is `cancelled`, and
   neither launches a vendor.
2. A crash after a turn's terminal commit, with a queued successor not yet
   dispatched. On restart the successor is dispatched and completes.
3. Keyed receipt replay. With `store.commit.reply_lost` on a receipt, the
   caller gets `store_error` with `commit_outcome: unknown` and
   `retry: same_key_only`, and the daemon exits 4. After a restart, the
   same keyed request returns the same session and turn, and the turn runs
   exactly once (one launch).
4. Unkeyed lost resume receipt. After a restart the committed queued turn
   runs exactly once.

Each test must fail on the current `rust-foundation` for the stated
reason. Run the gate from `.repo-context/verification.md`, with 5 full
`--features via-cli/test-failpoints` runs and their counts.
