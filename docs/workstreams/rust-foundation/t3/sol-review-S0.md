**Verdict: SOUND (merge).** No findings; no fix required.

1. The moved function bodies, await points, and lock order are unchanged. The `RecoveredGroup` `Drop` implementation and the moved tests retain their behavior; adding `Recovered.cursor` does not change permit release order (`crates/via-core/src/engine/slots.rs:13,54`).
2. The `pub(super)` changes expose items only to the parent module and its siblings, as needed. At the deadline, `save_cursor` adds one clone and brief mutex acquisition before the existing Store query (`crates/via-core/src/engine/recovery.rs:141-155`). Nothing reads the cursor yet, so it does not change recovery decisions.
3. The files match S0’s layout and leave the named engine and server modules available for later slice ownership.

This was a read-only review of the refs and report. I did not run the verification gate, as requested.