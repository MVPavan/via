**UNSOUND**

1. **Important — [adapter-contract.md:208](../../../../../docs/specs/adapter-contract.md#L208): rejection evidence has no place in the declared type.**  
   The amendment says `Rejected(StartRejected)` carries exit, cleanup and journal facts, but `StartRejected` contains none; `TurnEnd.outcome` carries `TurnEvidence` only on `Ok`. The merged code likewise has `TurnError::Rejected(StartRejected)` ([observation.rs:313](../../../../../crates/via-adapters/src/observation.rs#L313)). This leaves the cleanup gate without a concrete representation for Claude’s launched, pre-init rejection. Coordinator decisions explicitly require the sketch to change.  
   **Smallest fix:** declare `Rejected { reason: StartRejected, evidence: TurnEvidence }`, define no-launch evidence, and update the affected constructor sketches.

2. **Important — [adapter-contract.md:256](../../../../../docs/specs/adapter-contract.md#L256): resume mismatch still lacks a specified `TurnEnd.outcome`.**  
   Specifying `terminal:null` and an observation does not specify the mandatory outcome. `DriverFailure::ResumeMismatch` belongs to health and cannot fill that field. The fixture explicitly leaves the error kind unasserted; ruling C6 asks for that missing sentence. Merged code returns a Route `TurnFailure` carrying `TurnCause::ResumeMismatch` and cleanup evidence ([fake/driver.rs:745](../../../../../crates/via-adapters/src/fake/driver.rs#L745)).  
   **Smallest fix:** explicitly name the typed mismatch outcome, including its evidence, and distinguish it from `Rejected`/`submit_failed`.

3. **Important — [adapter-contract.md:255](../../../../../docs/specs/adapter-contract.md#L255): “the turn is never accepted” is too broad.**  
   This follows “Every init/result ID is checked.” A matching init and prompt-associated assistant event can already establish acceptance before a later result returns a mismatched ID. Acceptance cannot then be undone. The supplied mismatch fixture instead changes both init and result IDs, establishing only the pre-acceptance case. The unconditional `terminal:null` also conflicts with §4.1 if a valid terminal was already retained before subsequent contradictory traffic.  
   **Smallest fix:** scope never-accepted/null-terminal behavior to mismatch detected before acceptance; preserve earlier acceptance and valid retained evidence when mismatch occurs later.

4. **Important — [adapter-contract.md:209](../../../../../docs/specs/adapter-contract.md#L209): the uncertain-cleanup health trigger differs from the merged code.**  
   All `DriverFailure` variants are represented, but “a retirement or cleanup that ends uncertain” broadens `RetirementUncertain` beyond its implementation and omits journal uncertainty. The code triggers it only for a **launched persistent helper’s retirement**, when group cleanup is unproven **or** `journal_uncertain` is true ([fake/driver.rs:432](../../../../../crates/via-adapters/src/fake/driver.rs#L432), [fake/driver.rs:500](../../../../../crates/via-adapters/src/fake/driver.rs#L500)). Ordinary settled turn cleanup uncertainty is not universally a driver-health failure. First-cause latching and publication independent of observation delivery otherwise agree with the code.  
   **Smallest fix:** name `RetirementUncertain` and state those exact conditions.

5. **Minor — [codex.md:263](../../../../../docs/specs/vendors/codex.md#L263): inventory qualification is contradictory and overly categorical.**  
   The new paragraph records observed instruction-file inventory, while the same table still calls that inventory **unverified** at line 261. “Other categories have no inventory” also conflicts with the table’s schema-backed, unverified `mcpServerStatus/list` inventory. Fixtures establish reported AGENTS.md paths, not completeness or universal absence of other inventory mechanisms.  
   **Smallest fix:** update the instruction-files row to acknowledge observed reported paths while retaining switch/completeness qualification; say no other inventory was qualified by these fixtures.

6. **Minor — [codex.md:154](../../../../../docs/specs/vendors/codex.md#L154): bounded pagination has no exhaustion policy.**  
   “Follows it to the end within” finite budgets does not specify behavior when a non-null cursor remains at the limit. An implementer could cache a partial catalog and incorrectly reject models or efforts. All supplied responses have `nextCursor:null`, so they do not resolve this ambiguity.  
   **Smallest fix:** require an explicit discovery failure on exhaustion and forbid publishing a partial catalog as complete.

The userAgent rule matches every recorded response exactly. Claude’s error-result text mapping and usage mapping agree with the fixtures and rulings; for example, c9a maps `18 + 2348 + 18849 = 21215` input tokens, with `18849` cached tokens. Codex effort ordering also agrees: c7’s thread creation belongs to its later valid `low` turn.

**Could not verify:** live vendor behavior, multi-page pagination, or complete configuration inventory. No `docs/adr/` directory exists in this checkout. The saved diff matches the live spec diff; `git diff --check` passed. No edits, Git mutations, `bd`, vendor CLI, or model runs were performed.