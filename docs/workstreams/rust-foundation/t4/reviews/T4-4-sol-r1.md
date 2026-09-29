**UNSOUND**

## Findings

- **Important — fake usage contradicts the public contract.** `crates/via-core/src/api.rs:988` declares token scope `unavailable`, while `crates/via-core/src/engine/read.rs:233` returns numeric progress tokens with that scope. The terminal envelope still reports usage as unavailable at `crates/via-core/src/engine/terminal.rs:75`. In the test’s 120-token and 50-token steps, `status` reports 170 tokens labelled unavailable, then the final envelope reports no tokens. C1 §3.7 requires `tokens: null` without a validated source; the design calls the fake samples exact by construction. **Smallest fix:** declare the fake’s verified per-turn scope and carry its accumulated total into the terminal envelope.

- **Important — a valid numeric usage sample can prevent `turn.ended` from committing.** `crates/via-core/src/engine/progress.rs:191` accepts and sums `u64` samples, but `crates/via-store/src/runtime/sql.rs:1348` requires each step total to fit `i64`. The specified fake `usage` message permits a non-negative integer: a sample of `9223372036854775808` decodes successfully, then its step-row insert is refused. If it is the open step, the terminal transaction also fails, leaving no committed `turn.ended`; at a boundary, carrying the refused row causes the same terminal failure. **Smallest fix:** reject unrepresentable samples and checked per-step sums as a protocol error before constructing a step row.

## Sol scrutiny

The source paths support the other listed ordering rules: Core owns the sole step reducer and applies `model` before tool starts; it publishes step N+1 before awaiting row N; terminals built from `TurnRecord` carry the open and refused rows in the terminal transaction; uncertain step writes latch; `status` checks both the selected non-terminal turn and `Running.turn`; usage keys supersede or fold as specified, subject to the numeric defect above; and only acceptance or model/tool marks reset idle. I found no concrete weakening in the relevant migrated F24/F27 and failure scenarios. The missing separate RED logs for unit tests are not a plan violation.

## Measurement suggestions

Measure route drain gaps and Wire queue depth under real vendor bursts. The tests’ 100 ms stable-activity pacing can fail under a longer scheduling gap, but does not make an incorrect implementation pass.

## Could not verify

I did not rerun the full Gate G or reproduce the oversized-token case end to end. Focused tests passed **20/20**; `cargo fmt --all --check`, `check-layers.py`, and `git diff --check` passed. Git status remained clean.
