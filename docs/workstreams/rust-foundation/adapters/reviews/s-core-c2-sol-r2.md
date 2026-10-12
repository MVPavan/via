**SOUND** — all eight findings are fixed within this review’s scope.

Paths below are relative to `crates/via-adapters/`.

| Finding | Status | Reason | File:line |
|---|---|---|---|
| #1 Malformed scenarios defaulted | Fixed | Invalid JSON and unsupported envelopes are refused; both corrected H2 legacy forms remain accepted. | `src/config.rs:204` |
| #2 Bootstrap configuration discarded | Fixed | Configuration owns the three validated paths and opaque `harnesses`; the fake adapter retains its fixture. | `src/config.rs:95`, `src/config.rs:162`, `src/fake/mod.rs:31` |
| #3 Explicit models catalog-gated | Fixed | Known aliases resolve; uncatalogued explicit models pass through, including with an empty catalog. Model-only resolution remains catalog-gated. | `src/fake/mod.rs:44`, `tests/conformance.rs:456` |
| #4 Progress cannot carry C2 usage | Fixed | New progress payload carries `Option<UsageSample>` with independently nullable components. | `src/observation.rs:18` |
| #5 Empty effort accepted | Fixed | Shared validation rejects empty effort independently of catalog membership; both entry points have regression assertions. | `src/fake/mod.rs:161`, `tests/conformance.rs:209` |
| #6 Missing `server_key` | Fixed | Optional opaque key added, initialized to `None`, and excluded from C1 serialization. | `src/plan.rs:422`, `src/fake/mod.rs:128` |
| #7 Conformance gaps | Fixed | Added directional cases, duplicate-warning rejection, per-turn refusal checks and nonempty-refusal serialization assertions. | `tests/conformance.rs:254`, `:489`, `:556` |
| #8 Public Boolean parameter | Fixed | `Support::meets` is now crate-private. | `src/capabilities.rs:28` |

**New defects:** None found in the fix delta.

**Verification:** `cargo nextest run --locked --offline -p via-adapters` passed **15/15 tests**. `git diff --check 6603cf7..HEAD` passed. Git status remained clean at `5984ced`.

**Could not verify:** Historical RED/mutation runs and the author’s reported full-workspace gates. Driver execution, Core migration and live qualification remain outside this fix check.