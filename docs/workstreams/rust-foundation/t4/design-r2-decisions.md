# T4-0 design round 2: decisions

Inputs:
- `review-r2-astra.md` (UNSOUND);
- `review-r2-sol.md` (UNSOUND);
- `review-r2-opus.md` ("targeted revision, not a rethink"; asked for by the owner for effectiveness, simplicity and fit with the project).

Decisions 1–20 of round 1 stand except where changed below. From round 3 on, design-first work is done by Opus 5.5 high (owner, 2026-09-29). Tag each change `[t4r2.N]`.

## Correctness (blockers)

1. **One absolute cleanup deadline** (Astra 3, Sol). Drain, raw barrier, abort and join all count against the single deadline supplied. There is no fresh allowance at or after it. If a join cannot finish by the deadline, ownership of the unfinished tasks passes explicitly to shutdown supervision, which joins them under the daemon's shutdown bound. Never return by dropping their owner. Test: a pipe held open through the deadline.
2. **Pending writes stay serviceable** (Astra 1). A stdin or control write, or a raw acknowledgement wait, is held as pending state. Stop changes, `force_at`, the daemon force and health are serviced for the whole of every write and acknowledgement wait, and partial-write offsets are kept. Test: a blocked stdin write while `force_at` moves earlier.
3. **The raw worker's death guard owns in-flight work** (Astra 2, Sol 9). A guard fails both the queued sinks and replies and the in-flight batch's sinks and replies, and publishes the first classified failure. Test: the worker dies after dequeuing a lone stderr append while stdout is quiet. For the "N−1 frames commit" claim, either specify the Core commit order that achieves it or narrow the claim to what raw evidence guarantees, and test that.
4. **Memory accounting covers every copy that exists at the same time** (Astra 4, Sol). Either charge each retained representation before allocating it (Store rows, decoded events, serialized output), or keep events encoded and stream bounded serialization. Simplest correct option first; say which you chose. The page limit counts C1's full encoded response, including wrappers and separators. Any per-node charge needs a conservative proof, or must come from the counting visitor in decision 12. Test: an event just under 1 MiB whose wrapper pushes the page over the limit.
5. **Subscription teardown** (Astra 5).
   - An ending subscription stays charged until it is retired.
   - `terminal` is one of the deadline triggers.
   - A termination episode ends after its notices succeed or the socket closes, with a defined reset rule.
6. **`list`** (Astra 6, Sol, Opus 4). Use Opus's shape: phase 1 is the C1 keyset order; phase 2 is an id-ordered windowed scan filtered by `stamp > v0`. It always terminates. Record the exact guarantee as a numbered C1 §3.10 amendment:
   - no session that still matches the filter when the scan reaches it is skipped;
   - a session that leaves the filter may be absent;
   - the cross-page order changes as stated.

   The A12 proof must include a filter-change scenario.
7. **`status`** (Astra 7, Sol).
   - `process.alive` comes from process evidence (Host/ledger or anchor phase), never from the raw-log state of `connections`.
   - Size a blob-backed `effective` by the actual response size, not the storage threshold.
   - `cwd` must be absolute as well as existing.
8. **`cwd` is applied, not only stored** (Astra 8). The frozen `cwd` is passed through dispatch into the process spec, and the envelope reports it. Assign each hook to a named slice.
9. **Store lifecycle capacity under timeouts** (Sol). Bound abandoned or timed-out requests numerically within the shutdown window before relying on 8 lifecycle slots, or give the latch batch its own slot. Test: repeated timeouts before the latch batch.

## Amendment rulings

| Amendment | Ruling |
|---|---|
| A12 | Replaced by decision 6's amendment. |
| A13 | Accepted. The orchestrator edits coding-style. |
| A14 | Accepted. `cwd` and `allow_untested` become immutable `params` keys, and the runtime §6 target table is updated. |
| A15 | Accepted, with a CHECK constraint instead of a recovery recompute (Opus). |
| A16 | Accepted as bounded S1 definitions; decision 7 still applies. |
| A17 | **Rejected** (Astra, Sol). Length and hash reject mismatches fast; on a hash match, compare the exact bytes in bounded chunks through the blob owner. |
| A18 | Accepted. Record it as a design choice, not an amendment, since runtime §8 sets a floor. Keep an exact command-size guard, and note that the retry identity contains the prompt, so the two must fit the transaction cap together (Opus). |
| A19 | Accepted. Keep T3's cause coalescing; decision 2 completes it. |
| A20 | **Rejected as a package** (Sol, Opus). Runtime §4 requires a **clonable** control handle, so keep `WireSender: Clone`. The lifetime owner of the reader `JoinSet` is a separate unique owner, not the sender. Returning `WireParts` from `open_connection` is fine; keep `into_parts`. List Route's and Wire's current call sites. |

## Simplification (Opus 5, 6; the owner asked for simplicity)

10. **Large-prompt ingestion:**
    - Decode once with serde, and charge the transient copy to the global pool.
    - Stream the retry identity from raw line slices, with no hand-written unescaper.
    - Run blob I/O on the existing raw worker; there is no third thread.
    - Drop the separate `blob` byte class.
11. **Delete machinery that has no present requirement:**
    - Route's own 10 s `decoded` timer (the Adapter's stall timer owns this);
    - the unreachable Public 3 MiB byte cap;
    - galloping search in `logs_page` (a plain indexed lookup is enough);
    - the `ended_seq` recovery recompute (CHECK instead);
    - the follower's refusal back-off;
    - any `WireHealth` accessor that has no reader.

    If one of these is actually needed, keep it with a one-line present requirement.
12. **JSON limits:** replace the hand-written `shape.rs` tokenizer with a serde counting visitor, the pattern `retry_identity` already uses. First check that the visitor enforces depth and node limits before building a `Value`; if it cannot, say why and keep the smallest tokenizer.

## Exit evidence (Opus 2, 3: effectiveness)

13. **Plan Task 4's own acceptance criteria:**
    - an artifact for every F1–F30 scenario, with named scenarios for F15, F16 and F18;
    - the F16 handle-leak scan also covers `blobs/`;
    - the default-suite speed budget of about 2 minutes, with each heavy test placed in a named gate;
    - F24: daemon RSS and anchor RSS reported separately, and daemon plus four anchors under 384 MiB (runtime §8);
    - the 100 ms control-response check exercises turn control (`cancel`), not only `daemon/status`.

## Slice plan and hand-off

14. **Vertical slices** (Opus 8, Astra, Sol).
    - Re-cut so each slice ends in an end-to-end test.
    - The blob path lands with a 16 MiB end-to-end prompt test in the slice that first uses it.
    - Store read queries and CLI verbs live with the read surface (S3), not a kitchen-sink S1b.
    - The DTO and blob-input prerequisites come before their first user, with compilable interfaces between slices.
    - New Core test files are named and disjoint per slice.
15. **A history-free normative spec and per-slice briefs** (Opus 7).
    - `t4/design.md` becomes the clean normative text: keep the `[t4rN.M]` tags, but no narrative of rounds.
    - Write one brief per slice in `t4/`, each giving the slice's owned files, interfaces, tests and acceptance checks, for the Sonnet implementers.
