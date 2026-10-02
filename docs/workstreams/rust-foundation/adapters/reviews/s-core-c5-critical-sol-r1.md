**UNSOUND.** Reviewed `40bc0b4..22517cc` and its integration at `8a9774e`. All source locations below refer to `8a9774e`.

1. **Blocker — Schema compilation still has unbounded work.**  
   `third_party/boon/src/ecma.rs:24`, `third_party/boon/src/compiler.rs:527`, `third_party/boon/src/formats.rs:54`. ECMA conversion translates one token, reparses the entire pattern, and repeats. Neither the metaschema budget nor the regex program limit bounds this work; metaschema regex-format checking invokes the same conversion. With VIA’s exact limits, repeated `\d` compiled in **0.98 seconds at 6 KiB**, **3.99 seconds at 12 KiB**, and exceeded an **8-second probe timeout at 24 KiB** in release. These inputs are well below the schema cap. A blocking-step timeout leaves the computation running.  
   **Smallest fix:** bound conversion work before it runs, including metaschema format checks, or replace the repeated rewriting with a bounded single-pass conversion. Add these cases to the regression tests and measurements.

2. **Important — The adapter receives hard-coded inheritance instead of the frozen plan.**  
   `crates/via-core/src/engine/lane.rs:1303`. Core persists `planned.plan.inherit`, reports it through status, but constructs `SessionSpec` with `Inherit::OD2_DEFAULT`. S-LAUNCH configuration can therefore produce a receipt describing one configuration while a real adapter receives another. The fake does not consume this field, masking the integration defect.  
   **Smallest fix:** decode the frozen inheritance into the typed C2 value and pass it to `open_session`. Test the actual `SessionSpec`, including reopening after restart.

3. **Important — Forced finalization spills output before validating it.**  
   `crates/via-core/src/engine/stop.rs:280`, `crates/via-core/src/engine/output.rs:94`. `finalize_forced` spills first; spilling clears `retained.structured_output`. The subsequent `forced_terminal` validation reads only that cleared field. Consequently, an invalid value larger than 32 KiB can be stored without validation or the required warning.  
   **Smallest fix:** classify and validate before spilling. Add a forced-finalization test with invalid spilled output.

4. **Important — Terminal-write failure can erase the structured-output diagnostic.**  
   `crates/via-core/src/engine/batch.rs:139`, `crates/via-core/src/engine/output.rs:56`. Invalid output on a would-be completed turn becomes a `StructuredOutputInvalid` failure without a warning. If terminal persistence fails and shutdown’s resolution batch succeeds, the batch replaces that failure with `Store`, discarding its validation reason. The retained invalid output then has neither the validation failure nor the warning required for a non-completed final state.  
   **Smallest fix:** retain the validation outcome independently and project it after the final Store classification. Test terminal-write failure specifically; an earlier observation-write failure does not cover this path.

5. **Important — Successful steer replies precede durable `steer.delivered`.**  
   `crates/via-core/src/engine/receipt.rs:745`, `crates/via-core/src/engine/drive.rs:2022`. Core returns success immediately after `driver.steer`. The fake resolves that call at `crates/via-routes/src/fake/runtime/lane.rs:391`; Core commits the delivery observation separately afterward. A crash or event-write failure can therefore leave an acknowledged steer without its durable delivery record. This conflicts with runtime §1’s committed-result guarantee.  
   **Smallest fix:** coordinate the steer reply with acknowledgment of the corresponding Core commit. Persistence failure must not produce an ordinary successful reply. This is distinct from deferred keyed-steer support.

6. **Important — Decimal `multipleOf` rejects valid output.**  
   `third_party/boon/src/validator.rs:752`. The implementation tests the fractional part of floating-point division. A probe with schema `{"multipleOf":0.1}` and value `0.3` returned `Invalid`, although the mathematical quotient is three. Core consequently reports `structured_output_invalid` for valid output.  
   **Smallest fix:** use decimal-correct divisibility and add this regression. This defect is inherited from upstream, but becomes a C1 defect through VIA’s adoption.

7. **Important — Numeric bounds lose integer precision.**  
   `third_party/boon/src/validator.rs:715`; the same conversion occurs at `:724`, `:733`, and `:742`. Bounds convert both integers to `f64`. A probe with minimum `9007199254740993` accepted `9007199254740992`. These are distinct, permitted 64-bit JSON integers.  
   **Smallest fix:** compare integer operands exactly and handle mixed numeric representations without rounding away the distinction. Cover all four bound keywords.

8. **Important — `uniqueItems` misses equal zero values on its hash path.**  
   `third_party/boon/src/util.rs:555`, `:604`. Numeric equality treats zero and negative zero as equal, while their hashes use different floating-point bit patterns. Once the array exceeds twenty elements, the hash implementation can miss the duplicate. The probe `[0,-0.0,1,2,…,20]` returned `Valid` under `{"uniqueItems":true}`.  
   **Smallest fix:** normalize zero before hashing and ensure numeric hashing agrees with equality. Test both sides of the twenty-element threshold.

9. **Important — Advertised schema caps exclude unused schema positions.**  
   `third_party/boon/src/compiler.rs:278`, `:437`, `:532`. Limits count only schemas and patterns reached by compilation. Under VIA’s exact configuration, all three probes compiled successfully: **2,050 unused `$defs` schemas**, **65 unused patterns**, and an unused pattern exceeding the regex program limit. The amended C1 describes limits on the schema, rather than only its reachable compiled portion.  
   **Smallest fix:** enforce the limits across schema positions, including unused `$defs`, before admission. Alternatively, explicitly narrow the contract if reachable-only limits are the intended policy.

10. **Important — Recovery silently discards malformed frozen values.**  
    `crates/via-store/src/runtime/sql.rs:2108`, `crates/via-core/src/engine/recovery.rs:565`. Both parsing stages turn decoding failure into absence. Recovery then commits an envelope using `TurnPlan` fallbacks: effort and bound disappear, and the requested model can become the reported resolved model. Corruption is indistinguishable from genuinely absent information.  
    **Smallest fix:** preserve decoding errors through the recovery boundary and handle them explicitly; do not commit replacement frozen metadata. Add malformed JSON and valid-JSON/wrong-shape recovery cases.

11. **Minor — A new test corrupts the ordinary Cargo test process’s working directory.**  
    `crates/via-core/tests/conformance_intake.rs:1155`. The test changes process-wide cwd without restoring it, then deletes that directory with its temporary fixture. Ordinary parallel `cargo test` produced **14 failures out of 21 intake tests**, including incorrect cwd-cap refusals and unavailable-working-directory errors. Nextest’s process isolation makes all 21 pass.  
    **Smallest fix:** run the cwd-changing scenario in a child process. Restoration alone does not remove the concurrent-test race.

12. **Minor — Instructions parsing accepts explicitly null members.**  
    `crates/via-core/src/intake.rs:253`, `:271`. `Option<String>` conflates omission and null, so both `{"text":"x","path":null}` and `{"text":null,"path":"/f"}` pass. These are outside the strict `{text}` or `{path}` shape and C1’s non-nullable-member rule.  
    **Smallest fix:** distinguish presence from null and reject objects containing both members.

13. **Minor — The uncertain steer error falsely says nothing was applied.**  
    `crates/via-core/src/api.rs:1229`. `NotDelivered` correctly carries `delivery:"uncertain"`, but its message says “the steer input was not applied.” That contradicts the amended contract and the coordinator’s explicit uncertainty ruling.  
    **Smallest fix:** use a message that preserves uncertainty for `not_delivered`.

14. **Minor — The amendments leave spawn’s error list incomplete.**  
    `docs/specs/via-api-v1.md:210`. Chunk 5 introduces the required `unsupported_verb` spawn check, but the supplied spec diff does not add that error to §3.2.  
    **Smallest fix:** list `unsupported_verb` and its identifying data.

15. **Minor — The amended runtime inventory still calls implemented frozen values unfinished.**  
    `docs/specs/runtime-contracts.md:777`. The amended implemented-schema table describes frozen instructions, cwd and `allow_untested`, while the unchanged target table still lists them as unimplemented.  
    **Smallest fix:** remove those implemented items from the target row.

The **spec diff’s main semantic changes are coherent**, including `TurnCheck`, turn-bound steer admission, acceptance instance recording, schema v8, and validation warnings on non-completed turns. It needs the corrections above and an explicit decision on reachable-only schema limits. The implementation still conflicts with the rulings requiring bounded compilation, frozen inheritance, uncertainty-preserving steer errors, and validation after final classification.

Receipt parameters, acceptance instance fields and `adapter_version` use the appropriate SQLite transaction boundaries. The normal spill path validates before spilling and names the file after successful writing. The exceptional paths in findings 3–5 prevent an overall transactional-fidelity pass.

Vendoring provenance is sound: the archive checksum matches the record, **33 upstream files are identical**, and modifications occur in exactly the **seven documented files**. The patch is auditable, but its budget audit omits ECMA conversion. The measurement file’s recorded timings remain case-specific evidence; they do not establish complete bounds. Its `diff mismatches=0` line lacks enough corpus detail to reproduce semantic equivalence.

The inspected S1 assertion changes are identified in comments or the supplied design/rulings: typed bound/vendor fixtures, frozen inheritance, `builtin`→`bundled`, and schema v8. I found no additional unexplained S1 expectation change. Historical failure-first execution cannot be established from the current tests alone.

Verification performed: **12 schema tests passed; 77 selected cross-workspace conformance tests passed; focused Core and driver suites passed; release Core build passed; layer and harness-literal guards passed.** The bounded probes reproduced findings 1 and 6–9. Findings involving exceptional transaction paths are source-established rather than newly fault-injected.

I did not rerun the complete default/failpoint/musl gates, prove full upstream semantic equivalence, or exercise real adapters. No vendor CLI, model, `bd`, source edit or Git-state mutation was used. HEAD remained `8a9774e`; final Git status was clean.