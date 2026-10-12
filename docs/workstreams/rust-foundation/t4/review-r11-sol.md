# UNSOUND

The round-11 design resolves the WAL wording, configurable charges, span-test ordering, and both minor round-10 findings. Its per-session turn lifecycle still conflicts with the stop and cleanup contracts and can misattribute raw output after a boundary timeout.

## Blockers

1. `docs/workstreams/rust-foundation/t4/design.md:1046` — **A cancelled turn can be committed before P7 cleanup settles.** The path waits for terminal evidence, takes `raw_end`, commits the terminal, and drops the turn borrow. P7 requires an acknowledged interruption with open tools to remain *nonterminal* until quiescence or its cleanup deadline (`docs/specs/via-api-v1.md:245`). Committing first can release the next turn too early and lose the path for later tool evidence. **Smallest fix:** keep the turn and its observation path alive through P7 settlement; then take the boundary and commit a terminal with settled cleanup. Test an acknowledged interruption with a tool still open.

2. `docs/workstreams/rust-foundation/t4/design.md:1049` — **An unanswered `raw_end` barrier does not safely end a reusable turn span.** Units queued ahead of that barrier may become durable later, beyond the “proven durable offset” recorded as `raw_end`. They can then appear as session-level or successor-turn bytes. A timeout alone also does not prove the actual raw gap required for `raw_log.incomplete` (`docs/workstreams/rust-foundation/t4/design.md:1446`). **Smallest fix:** treat an unanswered end barrier as fatal to that connection: retire it through the owner, settle or discard outstanding units with honest loss evidence, and prohibit another turn on it.

3. `docs/workstreams/rust-foundation/t4/design.md:1481` — **The per-session exception is absent from the proposed contract amendments.** A29 still says a private route fails its *connection* on observation stall, while `docs/workstreams/rust-foundation/t3/design.md:239` calls `Close(Force)` at `force_at`, and `docs/specs/adapter-contract.md:235` directs that operation to stop a private group. OpenCode’s dedicated per-session server is private, yet `docs/specs/vendors/opencode.md:597` requires turn cancel and force close to retain it. An implementation following T3/C2 can still kill the server despite the new `TurnSender` API. **Smallest fix:** amend T3, C2 and A29 to distinguish per-turn connection failure from per-session turn failure, including `force_at`, Store latch, and whole-daemon shutdown.

## Important

4. `docs/workstreams/rust-foundation/t4/design.md:951` — **The proposed types block direct `close` calls, but do not yet bind writes to a turn’s lifetime.** `TurnSender` is described as an owned result of `WireSender::for_turn()`, with no stated lifetime, revocation, or rule for pending writes. A retained sender could enqueue an old turn’s command after the `TurnWire` borrow ends. The compile-fail test checks only missing close methods. **Smallest fix:** make the sender lifetime-bound to `TurnWire`, keep its fields private, and settle or revoke pending writes before releasing the borrow; test a late-write attempt.

5. `docs/workstreams/rust-foundation/t4/design.md:1875` — **The proposed per-session test cannot execute as written.** It closes the session with `force` and then runs turn 3 on that session, although close sets the admission gate to `closing` (`docs/specs/via-api-v1.md:264`). The existing fake agent also reads one start and exits; it rejects a second start (`crates/via-fake-agent/src/main.rs:128`, `crates/via-fake-agent/src/main.rs:245`). **Smallest fix:** run the successor-turn check before close, assert admission refusal after close, and specify a multi-turn test fixture or an explicit fake-agent extension.

## Minor

6. `docs/workstreams/rust-foundation/t4/design.md:601` — **The validated pool floor does not by itself reserve a request alongside the largest reply.** With default class values, lanes, cache, four connections and four drives take 112 MiB; the 2 MiB logs reply makes the stated 114 MiB floor. A C1 request has its own positive charge. If that charge is still held when the reply is acquired, a configuration accepted at the floor cannot serve that reply. **Smallest fix:** require release of the request charge before reply acquisition, or include the maximum concurrently held request charge in the floor and test that configuration.

## Round-10 disposition and amendments

| Round-10 finding | Round-11 status |
|---|---|
| B1 WAL figure | **Resolved as an explicit gate.** `docs/workstreams/rust-foundation/t4/design.md:879` calls the byte figure unverified and retains one-transaction overshoot. |
| B2 per-session force close | **Partly resolved.** `docs/workstreams/rust-foundation/t4/design.md:940` removes direct close methods from the turn handle; blockers 1–3 remain. |
| Important 3 class charges | **Resolved in the design.** `docs/workstreams/rust-foundation/t4/design.md:535` add configurable page, logs and blob-chunk charges with minimums. The pool-floor gap is minor finding 6. |
| Important 4 span test | **Resolved in the design.** `docs/workstreams/rust-foundation/t4/design.md:1874` lets pre-barrier units become durable, holds only an unenqueued unit across the barrier, and stalls the worker afterward. |
| Minor 5 OpenCode `unsubscribe` | **Resolved.** `docs/workstreams/rust-foundation/t4/design.md:1359` covers the C1 row and preserves vendor SSE subscription; the row was already named before round 11. |
| Minor 6 “already staged” | **Resolved.** `docs/workstreams/rust-foundation/t4/design.md:450` now says “already enqueued.” |

For **A24–A37**, A29 and A34 remain incomplete for the per-session stop path in blocker 3. A27 needs the timeout and loss rule in blocker 2. A25’s OpenCode row is covered; A35 is explicitly withdrawn. I found no further missing restatement in the reviewed targets of A24–A28, A30–A33, or A36–A37.

## Could not verify

- Implementation or test behavior: this was read-only, and no runtime checks were run.
- Measured RSS, class-charge peaks, WAL growth and its byte bound.
- Vendor token accuracy, the R4 crash wording gate, idle retirement, and the report’s open owner questions, including Q-R10-1.

