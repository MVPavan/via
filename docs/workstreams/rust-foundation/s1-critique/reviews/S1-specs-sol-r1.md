UNSOUND

- **Important — `docs/workstreams/rust-foundation/t2/dispatch-design.md:281`:** A failed receipt still unconditionally “latches,” contradicting A14/A16 and `latch.rs:502–504`, where a known-not-committed receipt failure stays scoped. The same obsolete unconditional-latch rule remains at lines 140–159, 173–174, 221, 396–399 and 442. Appending scoped-case references leaves contradictory instructions. **Smallest fix:** replace those unconditional clauses with the scoped-versus-latched distinction; retain the prescribed resolution write or retry for each site.

- **Minor — `docs/workstreams/rust-foundation/t2/dispatch-design.md:70`:** “It scans no slot” contradicts the amended §2.4 force-set collection and the implementation: `request_stop` calls `unfinished_sessions`, which examines slot state (`stop.rs:128,164–173`). **Smallest fix:** delete that sentence, or qualify it to describe force acceptance’s slot scan under `admission`.

Could not verify: runtime execution and timing guarantees; verification was by document comparison and source inspection, without Rust tests.