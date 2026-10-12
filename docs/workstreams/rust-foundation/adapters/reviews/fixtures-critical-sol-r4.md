**UNSOUND.** The original r3 witnesses pass, but four defects remain or were introduced.

At `6bfa066`: **82 active tests passed; 31 conformance cases remain ignored**. All 32 current replay fixtures pass the generic driver, probe verdicts and hygiene scan. Current expectations validate without false positives. Repository files and Git state remained unchanged.

| R3 item | Status and evidence |
|---|---|
| 1 — Launch-qualified markers | **Fixed.** Every marker identifies its launch; repeated-start tests and a six-process concurrent PID/ordinal/ack probe passed. [replay:354](../../../../../crates/via-fake-agent/src/replay.rs#L354). |
| 2 — UsageSample typing | **Fixed.** Nullable string key, named nullable unsigned counters and unknown-member rejection match the Rust type. Positive and negative witnesses pass. [checker:1074](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1074). |
| 3 — Identity-only gates | **Fixed.** Identity-only prefixes validate; acceptance assertions still require the relative order. [checker:1391](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1391). |
| 4 — Hyphen credential values | **Partly fixed.** Both original probes are caught. The replacement exemption retains a false-negative case below. [fixtures:1090](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1090), [fixtures:1114](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1114). |
| 5 — Usage provenance | **Fixed.** Usage excludes `estimated`; cost retains it. [checker:1101](../../../../../crates/via-core/tests/support/conformance_expect.rs#L1101). |
| 6 — Version-probe verdict | **Fixed verdict and obligation.** `probe_exit` checks version output, exit 0 and empty stderr independently of step endings. A new probe-selection defect is below. [probe_exit:55](../../../../../crates/via-core/tests/support/replay_exit.rs#L55), [checker:169](../../../../../crates/via-core/tests/support/conformance_expect.rs#L169). |
| 7 — Slowed causal witness | **Fixed ordering mechanism.** Explicit input acknowledgements replace the delay. New batching deadlock below. [fixtures:434](../../../../../crates/via-fake-agent/tests/fixtures.rs#L434), [fixtures:714](../../../../../crates/via-fake-agent/tests/fixtures.rs#L714). |
| 8 — Early-EOF sleeps | **Fixed.** The witness waits for readiness and published EOF before signalling. [replay test:1227](../../../../../crates/via-fake-agent/tests/replay.rs#L1227). |
| 9 — Lifetimes deviation inspection | **Fixed.** Inspection traverses lifetimes and checks the failure’s lifetime prefix. A synthetic second-lifetime probe produced the required causal failure. [fixtures:1399](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1399). |
| 10 — Directory-entry errors | **Fixed.** Enumeration propagates entry errors. [checker:210](../../../../../crates/via-core/tests/support/conformance_expect.rs#L210). |

The new defects are:

1. **Important — Failed EOF acknowledgements can produce a successful replay.**  
   [input.rs:260](../../../../../crates/via-fake-agent/src/replay/input.rs#L260), [input.rs:205](../../../../../crates/via-fake-agent/src/replay/input.rs#L205).

   On ack failure, the reader leaves the valid EOF queued and appends an error behind it. Consuming EOF caches it; `check_trailing()` then returns without inspecting the error. With the progress path replaced by a directory, a sealed replay exited **0 with empty stderr**, despite failing to write its EOF ack.

   **Smallest fix:** replace the event with an input error when its ack fails, under the same lock, rather than enqueueing the error behind a consumable EOF.

2. **Important — `KNOWN_OPTIONS` cannot prove that a credential value is absent.**  
   [fixtures.rs:1090](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1090), [fixtures.rs:1123](../../../../../crates/via-fake-agent/tests/fixtures.rs#L1123).

   `{"argv":["--password","--verbose"]}` and `{"stderr":"--password --verbose"}` both returned no finding. A credential can itself equal a listed flag string. The global list supplies no parser evidence establishing that the token occupies an option position rather than a value position. The argv scan also cannot recover shell quotation.

   **Smallest fix:** require placeholders at credential-value positions; exempt a missing-value boundary only when the specific recipe/parser establishes it. Remove the unconditional value exemption based on global list membership.

3. **Minor — A valid `version:null` fixture now fails the generic driver.**  
   [fixtures.rs:473](../../../../../crates/via-fake-agent/tests/fixtures.rs#L473).

   Selection tests field presence, so null triggers `--version`. The replay schema accepts null as `Option<String>::None`. A probe of the same fixture gave **direct replay: exit 0**, but **generic driver: “the fixture has no version for a --version probe.”**

   **Smallest fix:** probe only when `version` is a string, preserving null’s absent-version meaning.

4. **Minor — Ack-gated mutation tests can deadlock against the bounded input queue.**  
   [fixtures.rs:653](../../../../../crates/via-fake-agent/tests/fixtures.rs#L653), [fixtures.rs:714](../../../../../crates/via-fake-agent/tests/fixtures.rs#L714).

   The driver hoists every matching request, then waits for acknowledgements of all written lines before releasing the emit gate. With three requests sharing one predecessor, two fill the queue and the third cannot be published until the fake consumes one—after gate release.

   A valid synthetic fixture passed normal replay; its mutation waited for `read 4`, observed only `read 1–3`, and ended at the signal deadline instead of the required causal failure.

   **Smallest fix:** hoist one targeted offending request per mutation run, rather than batching requests beyond the queue’s capacity.

The proposed **UsageSample clarification is SOUND**. Its members and nullable semantics match [observation.rs:182](../../../../../crates/via-adapters/src/observation.rs#L182), and it preserves keyed supersession and keyless addition. C1 exposes the corresponding counters under `_tokens` names. [Proposed C2 text:609](../../../../../docs/specs/adapter-contract.md#L609).

I could not verify private-recording fidelity, real adapter `drive()` implementations and controlled-clock behavior, actual vendor argument parsing, or non-Linux behavior. I did not inject directory-enumeration errors or rerun the full workspace gate. No vendor CLI/model or `bd` command ran.