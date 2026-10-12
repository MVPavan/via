**SOUND** within the requested scope.

| Finding | Status | Evidence |
|---|---|---|
| r2: Server-route failures incorrectly receive no-launch evidence | Fixed | [adapter-contract.md:208](../../../../../docs/specs/adapter-contract.md#L208) uses route-specific cleanup evidence and requires a complete Host journal for no-launch quiescence. |
| r2: `resume.mismatch` can overwrite a retained terminal result | Fixed | [adapter-contract.md:384](../../../../../docs/specs/adapter-contract.md#L384) defers turn disposition to `TurnEnd`, preserves retained results, and ends the connection through health. |
| Remaining r1 finding 3: acceptance and terminal preservation | Fixed | [adapter-contract.md:257](../../../../../docs/specs/adapter-contract.md#L257) distinguishes all three timing cases; the updated §4 row now agrees. |

**New defects:** None found from these two edits.

**Could not verify:** Runtime implementation of the evidence and mismatch rules, or live vendor behavior; no runtime or vendor checks were performed.

The saved r3 delta matches `git diff docs/specs/`; `git diff --check` passed. Only the two r2→r3 edits were evaluated. No edits, Git mutations, `bd`, vendor CLI, or model runs were performed.