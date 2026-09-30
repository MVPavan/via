UNSOUND

- **Important — `docs/specs/runtime-contracts.md:1129`:** The amended audit row says a known-not-committed write after receipt ends the turn `failed(store)`. This contradicts A14 and §7’s exceptions: a successful natural-terminal retry preserves the vendor result; a successful dispatcher-cancellation retry preserves `cancelled`. Existing tests exercise both. **Smallest fix:** include those exceptions, or replace the blanket disposition with a reference to §7’s per-site rules.

- **Minor — `docs/workstreams/rust-foundation/t2/dispatch-design.md:7`:** The history bullet still states in present tense that runtime §7 latches on the first failed write and replaces failed-write retries. That contradicts A14/A16. Calling it history in the report does not qualify the sentence in this normative document. **Smallest fix:** explicitly mark that historical rule as superseded by A16.

Could not verify: runtime execution, timing guarantees, or test pass status; review used document comparison and source inspection without running Rust tests.