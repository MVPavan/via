**SOUND** — within the requested revision-8 → revision-9 fix-check scope.

Line references are to revision 9’s [design.md](docs/workstreams/rust-foundation/adapters/design.md).

| Item | Fixed, partly or not | One-line reason | Line |
|---|---|---|---|
| R8 new defect 1 — impossible control assertions | Fixed | Every option-A case requires a non-null report; no-read cases require `incomplete` with no processes. | [748–764](docs/workstreams/rust-foundation/adapters/design.md:748) |
| R8 new defect 2 — eligible-entry failures | Fixed | Required `environ`, `comm`, and final identity/UID reads now set `incomplete` on failure; disappearance remains exempt. | [732](docs/workstreams/rust-foundation/adapters/design.md:732) |
| R8 new defect 3 — persistence ambiguity | Fixed | The step-3 citation is correct; AR2 explicitly distinguishes durable vendor facts from memory-only start ticks carried in `Spawned`. | [731](docs/workstreams/rust-foundation/adapters/design.md:731), [970](docs/workstreams/rust-foundation/adapters/design.md:970) |
| R8 new defect 4 — option-A ownership inventory | Fixed | Conflict 4 now includes S-LEFTOVER’s option-A `anchor.rs` and `protocol.rs` ownership. | [176–178](docs/workstreams/rust-foundation/adapters/design.md:176) |
| R8 partial #10/#12 — failure-first tests | Fixed | Positive control assertions remain where discovery is possible; the contradictory no-read assertions are removed. | [748–764](docs/workstreams/rust-foundation/adapters/design.md:748) |
| R8 partial “New #2” — vendor start bound | Fixed | AR2 supplies the missing contract clarification while preserving capture before `Spawned` and reaping. | [731](docs/workstreams/rust-foundation/adapters/design.md:731), [970](docs/workstreams/rust-foundation/adapters/design.md:970) |
| R8 partial “New #5” — option-A inventory | Fixed | The previously omitted start-bound ownership clause is explicitly inventoried. | [176–178](docs/workstreams/rust-foundation/adapters/design.md:176), [1399](docs/workstreams/rust-foundation/adapters/design.md:1399) |

**New defects introduced by the fixes:** None found.

**Could not verify:** Implementation behavior and failure-first results remain unexercised; the scanner and extended `Spawned` packet are proposed. Detection choice A/B/C remains pending.

HEAD remained `ff39895`; final Git status matched the initial status. No files were edited, and no `bd`, tests, vendor CLI, or model runs were performed.