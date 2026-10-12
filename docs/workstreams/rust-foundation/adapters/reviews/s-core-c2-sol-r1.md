**UNSOUND.** The tests pass, but the chunk has contract defects and incomplete planning types. No blocker-severity finding; six important findings and two minor findings follow.

Paths below are relative to `crates/via-adapters/`.

1. **Important — malformed scenarios silently succeed.**  
   [src/config.rs:157](../../../../../crates/via-adapters/src/config.rs#L157) defaults on every JSON parse error, every scalar/null JSON value, and any object without `profile`. Consequently, truncated JSON, non-JSON text and malformed scenario envelopes all configure an apparently healthy default fake. `tests/conformance.rs:482` explicitly requires this incorrect fallback. Runtime §11.1 requires an absolute **JSON fixture** and refusal of malformed configuration; H2 approves the object form and legacy array, not arbitrary input.  
   **Smallest fix:** propagate JSON errors, discriminate supported scenario envelopes, and refuse malformed envelopes. Keep fixture-action interpretation in the fake agent. Replace the raw-text success assertion with refusal assertions.

2. **Important — bootstrap configuration is discarded.**  
   [src/config.rs:91](../../../../../crates/via-adapters/src/config.rs#L91) retains only `FakeProfile`. The validated binary, scenario and sync-directory paths disappear at `load`’s return; the opaque `harnesses` object also disappears. `AdapterSet::new` then receives only the profile. Runtime §11.1 requires the captured paths to become the fake adapter’s explicit launch configuration; C2 §2 makes that configuration Adapter-owned. Chunk 3 cannot launch from this object without changing it or rereading bootstrap inputs.  
   **Smallest fix:** retain an owned validated fixture alongside its profile, and retain `harnesses` as opaque owned JSON.

3. **Important — explicit models are incorrectly catalog-gated.**  
   [src/plan.rs:531](../../../../../crates/via-adapters/src/plan.rs#L531), through `fake/mod.rs:40`, rejects an uncatalogued model even when the caller supplies the harness. Design §5.2 explicitly assigns that case to the vendor; catalog matching is required for **model-only** resolution. `tests/conformance.rs:443` entrenches the opposite behavior. An empty catalog also rejects every explicit model.  
   **Smallest fix:** resolve known aliases, pass an unknown explicit model through unchanged, and reserve `UnknownModel` for unresolved model-only requests.

4. **Important — the new progress observation cannot represent C2 usage.**  
   [src/observation.rs:35](../../../../../crates/via-adapters/src/observation.rs#L35) imports the legacy `ProgressMarks`, whose usage is `Option<(Option<String>, u64)>`. C2 §4 and §5 require `Option<UsageSample>` with independently nullable input, cached-input, output, reasoning-output and total components. The new `UsageSample` exists but cannot travel through `Observation::Progress`.  
   **Smallest fix:** define a new `ProgressMarks` in `observation` using `UsageSample`; preserve the legacy type for unchanged Core.

5. **Important — empty effort can be accepted.**  
   [src/fake/mod.rs:148](../../../../../crates/via-adapters/src/fake/mod.rs#L148) checks only membership in the profile’s effort list. A profile with native effort support and `efforts: [""]` makes both `plan` and `check_turn` accept `effort: ""`. AD18/C2 §5 explicitly requires empty strings to be refused. The existing empty-effort assertion passes only because its fixture omits `""` from the list.  
   **Smallest fix:** reject empty effort independently of membership, shared by both entry points; add this counterexample.

6. **Important — `RoutePlan.server_key` is missing.**  
   [src/plan.rs:394](../../../../../crates/via-adapters/src/plan.rs#L394) omits C2 §2’s `server_key: Option<ServerKey>`. This is an internal planning field needed by the later driver/admission surface, even though the current fake returns no key.  
   **Smallest fix:** add the opaque optional field, initialize it to `None` for the fake, and exclude it from C1 `describe` serialization.

7. **Minor — conformance coverage misses required defect classes.**  
   `crates/via-adapters/tests/conformance.rs:264` (since moved to `crates/via-core/tests/conformance.rs`) exercises only OD2’s fixed requested states. It misses an unverified **on** switch, requested **off** with observed **on**, and requested **off** with neither switch nor observation. Its warning helper selects the first matching warning, so duplicate `config_switch_unverified` warnings would pass. There are also no `check_turn` assertions for schema, step-limit, bound or vendor refusals, and no nonempty-refusal serialization assertions.  
   **Smallest fix:** add a directional state table, assert exactly one warning with the complete categories, and add focused refusal/serialization cases.

8. **Minor — public Boolean contract parameter violates coding style.**  
   [src/capabilities.rs:28](../../../../../crates/via-adapters/src/capabilities.rs#L28) exports `Support::meets(bool)`, contrary to coding-style §3’s prohibition on Boolean contract-function parameters.  
   **Smallest fix:** make this implementation helper `pub(crate)`, or accept a named requirement type.

The three departures:

| Departure | Verdict |
|---|---|
| Public `resolve_model` | Acceptable for this chunk as a pure test seam used by production planning. Its ambiguity test verifies the helper, not multi-adapter `plan` integration. |
| `AdapterSet::new(config)` only | Acceptable staging: chunk 3 owns runtime/resources and driver construction. It must then acquire the C2 signature. |
| New types under `observation`, reusing legacy `ProgressMarks` | Namespace separation is acceptable. Reusing the legacy progress payload is defective: finding 4. |

The five reported behavior differences:

| Difference | Verdict |
|---|---|
| `models.source: bundled` rather than `builtin` | Correct: C2 §2, AD2 and design §5.3 specify `bundled`. |
| Model-only request with no configured adapter → `unknown_model` | Correct: design §5.2 requires `UnknownModel` when no catalog matches. Explicit unavailable harnesses still produce `harness_unavailable`. |
| `RoutePlan.inherit` omitted from JSON | Correct for C1 §3.1; effective inheritance belongs in frozen session parameters/status. |
| Non-JSON scenario → default profile | Defect: finding 1. A valid legacy scenario without `profile` may use defaults; arbitrary malformed input may not. |
| Existing fake-agent `{scripts}` form ignores `profile` | Verified and acceptable: its untagged `Many` variant ignores extra fields. Adapter configuration consumes the profile. |

The four reported deliberate breaks would contradict the inspected assertions: permissive partial requirements, unverified switches returning the request, unconditional version compatibility, and ignored effort membership. That supports those particular tests’ sensitivity, but does not cover the gaps above.

Other inspected behavior is sound: AD12 checks the current version or explicitly listed compatible versions and returns `harness_unavailable` with `reason: adapter_version`; AD13’s implemented effective-state calculation and warning-category construction are correct for valid requests. The capabilities DTO and ordinary RoutePlan JSON match C1. No new harness-specific decision is imposed on Core, and no secrets, personal data or machine-local paths appeared in the diff.

Out of scope, noticed: the existing fake-agent accepts `{scripts}` or a single script object, **not a top-level array**. H2’s legacy-array acceptance at configuration load therefore still needs downstream compatibility work.

Verification: `cargo nextest run --locked --offline -p via-adapters` passed **11/11 tests**; `git diff --check 99e0567..HEAD` passed; Git status remained clean at `6603cf7`. No files were edited, Git state changed, `bd` run, or vendor CLI/model invoked.

Could not verify: the historical RED/mutation runs or reported full-workspace gates. Driver behavior, Core migration and live harness qualification remain outside this chunk and review authorization.