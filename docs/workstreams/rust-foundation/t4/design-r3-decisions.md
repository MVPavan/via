# T4-0 design round 3: decisions

Inputs: `review-r3-astra.md` (UNSOUND) and `review-r3-sol.md` (UNSOUND),
reviewing `f63252d`. Findings have narrowed each round. Round-1 and round-2
decisions stand except where changed here. Tag each change `[t4r3.N]`.

Before committing round 4, the author runs a self-check (decision 12).

## Memory and JSON

1. **No hidden re-parse** (Astra 1). The workspace enables `serde_json`'s
   `raw_value` feature (`Cargo.toml:16`), and `Value`'s deserializer
   re-parses a `$serde_json::private::RawValue` key's string. So every
   peer-controlled JSON value (vendor frames, C1 request params) is built by
   our own counting visitor, which constructs the `Value` nodes as it counts.
   A visitor that counts and then hands off to `Value::deserialize` is not
   enough. Literal keys are preserved.
   - Adversarial S1 test: an object whose first key is that private name,
     holding an encoded array of more than 65,536 elements.
   - Any capacity proof comes from the enabled dependency code. Allocator
     samples only confirm it; they are not the proof.
2. **Every allocation is charged at its boundary** (Astra 2, Sol 4):
   - the 512 B minimum charge is enforced at the shared raw-append
     boundary, so stderr is charged too, not only stdout;
   - container capacity is metered (for example `Vec<Box<RawValue>>`
     backing);
   - a row's borrowed length is checked before its owned text is allocated,
     so no over-budget lookahead row is allocated;
   - the request envelope's owned `id` and `method` are charged before the
     envelope is decoded;
   - the allocator test measures the actual typed DTOs and Core structures
     against their permits, not a proxy `Value`.

## Store capacity

3. **The Latch has exclusive bytes, and the batch fits the transaction
   cap** (Astra 3, Sol blocker).
   - The Latch slot owns an exclusive byte reserve equal to the maximum
     failure-resolution payload. Lifecycle traffic cannot use it.
   - That whole payload (the terminal plus the maximum queued
     cancellations) stays within runtime §8's 1 MiB transaction cap. Bound
     the terminal's share accordingly; there is no exception to the cap.
   - Boundary test: the largest terminal plus the maximum cancellations,
     after timeouts that leave near-limit lifecycle payloads retained.
4. **Schema v6 is defined once** (Sol blocker). S2's v6 includes the blob
   columns and their CHECK constraints from the start; S2 writes inline
   values and S5 starts using the blob columns. Add a test that reopens a
   Store created by S2 after S5's changes.

## Follow, `list` and status

5. **A terminal ending keeps queued events** (Astra 4). On a normal
   `terminal` end, the matching events already queued are delivered before
   the terminal notice, within the same absolute deadline; the connection
   closes if the deadline passes. Only exhaustion (`lagged`) and
   `unsubscribe` discard unsent data (C1 §3.11). Test: a reading client
   whose writer is briefly behind the scanner.
6. **`list` has a fixed population** (Astra 5, Sol 6).
   - The first page captures an immutable creation watermark (the largest
     session id or creation rowid), and both phases are limited to sessions
     at or below it. Mutable stamps still track updates.
   - The phase-2 scan stops at its first row that was not returned
     (result-count or byte limit), so the cursor never skips past it.
   - Rewrite A12 accordingly, with the proof, the filter-change scenario and
     the restatement list.
   - Tests: more than 200 matching ids in one window, continuous updates,
     and continuous creation.
7. **Session counts are disjoint** (Astra 6). Closing takes precedence:
   `active` counts unresolved sessions that are not closing.
8. **`process.alive` needs positive evidence** (Sol 7). It is true only on
   bounded positive Host or process evidence, for example a live armed
   control in the Host ledger. A recorded `vendor_pid` with no absence proof
   means "unproven", not "alive". Report uncertain cleanup separately from
   liveness, as C1 §3.7 allows. If C1's field cannot express "unknown", say
   so and propose the smallest amendment.

## Contract changes

9. **A21 is dropped** (Astra, Sol). Its premise was wrong: `turn.ended`
   does not carry the envelope. Keep the envelope and event bounds separate,
   measure the full response, and refuse an item that cannot fit with
   `admission_refused` (C1 §3.11). Any envelope-limit change must also
   restate C1 §5.

## Slices

10. **Compilable boundaries** (Astra 7).
    - Add a separate encoded public-result API for the read surface. The
      Store result type that `journal.rs` and `control.rs` consume does not
      change inside S4.
    - The minimal Public admission and refusal plumbing moves into S1,
      which uses separate Public and Internal saturation fixtures. Public
      alone cannot fill 64 ordinary slots, since its cap is 32.
    - Specify S1's compatibility path for existing raw callers until S3
      installs staging permits.
    - S5's prompt-digest echo needs a fake-agent capability with a named
      owner.
    - S2's DTO work names `instructions` and `require` explicitly.
    - S5 defines how a finished blob is discarded after `finish(self)`
      consumes its writer.
11. **The shared slice gate list includes `cargo deny check`** (Sol). The
    orchestrator widens the Task 4 selector in `.repo-context/verification.md`
    when implementation begins.

## Author self-check before committing

12. The author makes one adversarial pass over the whole design and the
    slice briefs:
    - For every bound, is every allocation and path charged, including
      dependency behaviour such as serde features?
    - For every reserve, can another class consume it?
    - For every "always" or "never", what input breaks it?
    - For every slice boundary, does it compile without the next slice?
    - List what the pass found and fixed in the report's round-4 section.
