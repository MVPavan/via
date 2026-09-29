**UNSOUND**

Both round-1 findings are resolved on the ordinary terminal path: the fake declares `turn` token scope, the envelope reports the accumulated total when sampled, and unrepresentable usage is refused before a step row is built. A distinct forced-shutdown path remains incorrect. The new `Protocol` stop cause is justified for a failure detected by Core; it does not duplicate Route’s decode-failure path.

## Findings

- **Important — `crates/via-core/src/engine/stop.rs:394`: forced shutdown drops a known protocol failure.** If Core refuses a usage sample and daemon force closes Route before its protocol stop completes, `crates/via-core/src/engine/drive.rs:698` carries the turn to `forced_terminal`. That function handles idle and store failures but commits this turn as `cancelled` or `unknown`, despite the refused sample. **Smallest fix:** make `forced_terminal` fail the turn as `protocol` when its tracker recorded an unrepresentable sample, while retaining store failure precedence; cover the forced handoff in a focused test.

## Could not verify

I did not run the runtime gate in this read-only review or reproduce the forced-shutdown race. `git diff --check` passed for both requested diffs, and Git status is clean.
