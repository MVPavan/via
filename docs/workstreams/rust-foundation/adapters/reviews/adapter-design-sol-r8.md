**UNSOUND.** Scoped to the revision-7 → revision-8 diff and the named fixes. All design-line references below are revision 8.

| Item | Fixed, partly or not | One-line reason | Line |
|---|---|---|---|
| New #1 — final identity/UID check | fixed | Retained-descriptor rereads now check `stat` start ticks/state and `status` real UID; disappearance remains separate. | [730](docs/workstreams/rust-foundation/adapters/design.md:730) |
| New #2 — vendor start-bound race | partly | Capture before `Spawned` fixes the reap race; the claim that memory-only retention needs no contract amendment remains unsupported. | [730](docs/workstreams/rust-foundation/adapters/design.md:730), [1397](docs/workstreams/rust-foundation/adapters/design.md:1397) |
| New #3 — server-loss acceptance | fixed | Supervised loss with in-flight turns requires a non-null shared report; unsubscribe and idle retirement require `null`. | [1397](docs/workstreams/rust-foundation/adapters/design.md:1397) |
| New #4 — environment cap | fixed | Reads allow one lookahead byte; matches count only after EOF within 256 KiB; both boundary sizes are tested. | [730](docs/workstreams/rust-foundation/adapters/design.md:730), [753](docs/workstreams/rust-foundation/adapters/design.md:753) |
| New #5 — option-A inventory | partly | Scan bounds and marker qualifications are covered; newly added option-A owned paths are omitted from the “only” inventory. | [175](docs/workstreams/rust-foundation/adapters/design.md:175), [1397](docs/workstreams/rust-foundation/adapters/design.md:1397) |
| Partial #10/#12 — failure-first tests | partly | Tests (6), (7), (9)–(12) have coherent positive assertions; the blanket control assertion contradicts cases in (8) and (13). | [747](docs/workstreams/rust-foundation/adapters/design.md:747) |
| Partial #3 — pre-eligibility failures | fixed | Failed `stat`/`status` reads explicitly set `incomplete`, except specified disappearance outcomes. | [731](docs/workstreams/rust-foundation/adapters/design.md:731) |
| `hidepid` spelling | fixed | Both `hidepid=4` and `hidepid=ptraceable` are recognized, matching the [kernel documentation](https://www.kernel.org/doc/html/latest/filesystems/proc.html#mount-options). | [731](docs/workstreams/rust-foundation/adapters/design.md:731) |

The final reread uses the correct files and preserves process-instance binding, consistent with [Linux procfs documentation](https://www.kernel.org/doc/html/latest/filesystems/proc.html#process-specific-subdirectories).

**New defects introduced by the fixes**

1. **Important — impossible control-survivor assertions.**  
   **Location:** [747](docs/workstreams/rust-foundation/adapters/design.md:747), [752](docs/workstreams/rust-foundation/adapters/design.md:752), [760](docs/workstreams/rust-foundation/adapters/design.md:760).  
   **Defect/evidence:** Every option-A test must list a marked control survivor. Test (8)’s zero-budget case forbids any `/proc` open; test (13)’s unavailable-bound case forbids candidate environment reads. Neither can discover that control. These assertions would remain red after a correct implementation.  
   **Smallest fix:** Require a non-null, `incomplete` report with no discovered processes in those cases; restrict the listed-control assertion to cases permitting its discovery.

2. **Important — eligible-entry failure coverage regressed.**  
   **Location:** [731](docs/workstreams/rust-foundation/adapters/design.md:731).  
   **Defect/evidence:** Revision 7 covered any denied/unreadable read of an eligible entry. Revision 8 covers only its `environ` read, plus pre-eligibility `stat`/`status` failures. An unreadable `comm` now has no disposition; failures during final identity rereads are also ambiguous once initial eligibility is established. Dropping such entries can conceal failed coverage behind `incomplete: false`.  
   **Smallest fix:** Retain the pre-eligibility rule and restore coverage for every required read of an eligible entry, explicitly including `comm` and final rereads, with the disappearance exception preserved.

3. **Important — the no-amendment claim leaves persistence ambiguous.**  
   **Location:** [730](docs/workstreams/rust-foundation/adapters/design.md:730).  
   **Defect/evidence:** Capture in `spawn_vendor` before [anchor.rs:321](crates/via-host/src/anchor.rs:321) precedes the polling reap at [261](crates/via-host/src/anchor.rs:261). That placement is sound. However, runtime [454](docs/specs/runtime-contracts.md:454) belongs to startup **step 3**; actual step 4 at [458](docs/specs/runtime-contracts.md:458) requires committing vendor child facts. Revision 8 calls start ticks a vendor child fact but forbids persisting them. Platform [195](docs/specs/platform-packaging.md:195) addresses **anchor** metadata and does not resolve this distinction.  
   **Smallest fix:** Add a §3.7 audit entry clarifying that step 4 commits the durable vendor facts while option-A scan start ticks remain memory-only; correct the step reference.

4. **Minor — the replacement inventory omits new option-A ownership text.**  
   **Location:** [175](docs/workstreams/rust-foundation/adapters/design.md:175), [1397](docs/workstreams/rust-foundation/adapters/design.md:1397).  
   **Defect/evidence:** S-LEFTOVER now explicitly owns `anchor.rs` and `protocol.rs` for “start bound in `Spawned`, option A”. Conflict 4’s exhaustive inventory names AD20 rows/tests, AR2 and marker qualifications, but excludes this clause.  
   **Smallest fix:** Include S-LEFTOVER’s option-A start-bound ownership clause in the inventory.

**Could not verify**

The scanner, extended `Spawned` packet and report fields are still proposed. Actual failure-first results, fast-exit capture, UID/state races, deadline cancellation and cap-boundary behavior were not exercised.

No files or Git state changed. No tests, `bd`, vendor CLI or model runs were performed. Final HEAD and status matched the initial observations.