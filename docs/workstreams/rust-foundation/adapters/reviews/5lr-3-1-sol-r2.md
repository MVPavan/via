**UNSOUND at b1a9d2d** — the prior findings are resolved within this slice, but the new checker permits false-green schema validation. The later well-formedness commit is excluded.

All line references below are for **b1a9d2d**. `F/` means `crates/via-adapters/tests/fixtures/codex/`; `S` means `crates/via-core/tests/support/conformance_expect.rs`.

| Finding | Status | Reason | File:line |
|---|---|---|---|
| 1. Hygiene checker | **other slice** | Claude-owned; excluded from this review. | `crates/via-fake-agent/tests/fixtures.rs:333` (r1 reference) |
| 2. Wall terminal/usage omitted | **fixed** | Interrupted terminal and usage are asserted alongside `deadline`. | `F/c3_wall_interrupt.expect.json:39` |
| 3. Effort-check ordering | **fixed** | Coordinator adopted checking before thread creation; expectation cites that ruling. | `F/c7_effort_catalog.expect.json:58` |
| 4. Launches/close unchecked | **fixed** | Checker compares launches and each stated close report, including cleanup. Close mode correctly remains an input. | `S:240`, `S:268` |
| 5. P7 settlement unchecked | **fixed** | Both interrupted cases assert `at_p7_bound`; driver obligations require controlled time and pending-before-bound evidence. | `S:13`; `F/c3_interrupt_uncertain.expect.json:56`; `F/c4_two_sessions.expect.json:67` |
| 6. Recording fidelity | **fixed** | Private recordings confirm c7/c8’s restored `systemError → error → turn/completed` order and c1’s absence of the invented async delta. | `F/c7_bad_model.replay.json:151`; `F/c8_auth.replay.json:156`; `F/c1_commentary_usage.replay.json:269` |
| 7. Provenance | **fixed** | All 14 launched pairs declare the borrowed catalog; effort case declares the borrowed successful exchange and usage. | `F/c7_effort_catalog.expect.json:2` |
| 8. Fidelity claim overstated | **fixed** | Hand-back now accurately calls the local driver a load-and-exit smoke check. Integrated fidelity remains pending. | `codex-fix-r1-handback.md:19` |

**New defect — Important: validation and `ideal()` permit false-green expectations.**

At [S:146](../../../../../crates/via-core/tests/support/conformance_expect.rs#L146), validation does not enforce observation field types or the schema’s accepted-count rule:

- `accepted: true` with `observation_counts: {"turn.accepted": 0}` passes validation. `ideal()` independently copies acceptance and generates zero observations, so `check(expect, ideal(expect))` passes this prohibited combination.
- A string-valued `observations_include` or `observations_order` becomes an empty list through `strings()` at `S:226`, silently removing the assertion.

These are static control-flow witnesses. All **21 committed turns** satisfy the relevant consistency rules, but the new validator cannot reliably guard future schema conversions. Require type/consistency validation and independent negative witnesses.

For well-formed expectations, `check()` compares the stated output fields, including nested terminal data, launches, closes, steer results and observation constraints. Its vocabulary and API are otherwise suitable for Claude adoption. `cleanup_settles` appropriately delegates timing measurement to the future driver.

**Concern verdicts**

- **Close cleanup `uncertain`: sound.** c3 and c4’s A retain unfinished reported tools; unsubscribe proves detachment, not quiescence. This follows Codex §3 Close and §6 cleanup.
- **Steer `after: tool_started`: sound.** The schema shows `accepted` as an example without defining a closed steer-timing enum. Waiting for tool start preserves the recorded scenario.

**Could not verify**

Exact-commit test execution was unavailable after concurrent checker edits. Nextest exercised newer working-tree code: **3 passed** by default; **16 expected adapter-stub failures** with ignored cases enabled. The supplied gate log reports success. Integrated fidelity, real driver timing and Claude adoption remain unverified.

I edited no files or Git state and ran no `bd`, vendor CLI or model. The branch advanced externally to `68257d8`; final Git status was clean.