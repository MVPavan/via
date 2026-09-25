# Writing useful tests

A test should name a plausible break and exercise the behavior that would reveal
it. Use existing repository tooling and representative boundaries.

- Derive expected values independently from requirements, worked examples or
  trusted fixtures. Do not compute both sides with the implementation under test.
- Assert meaningful results, state or boundary interactions. Outbound call counts,
  arguments and ordering can be the contract; internal call choreography usually is not.
- Prefer real components where economical. Fakes, mocks and partial responses are
  legitimate when they preserve the contract exercised and do not replace the
  behavior being proved. Add integration coverage for material assumptions they omit.
- Choose a stable public boundary when practical; internal tests can earn their
  place for algorithms and invariants. Do not add production methods solely for
  test cleanup or expose internals just to satisfy a test pattern.
- Control time, randomness and shared state when they cause nondeterminism.
  Keep fixtures understandable and expectations visible. A focused parameter table
  often beats duplicated tests or a generic fixture framework.
- A test of exact source text is useful only when that text is the actual contract;
  for a script, execute controlled inputs and assert outputs/side effects instead.

Before accepting substantive tests, identify the wrong branch, missing validation
or side effect they catch. A safe mutation trial can resolve uncertainty when
worth its cost; mental inspection is not a reported executed mutation test.
Regression checks should fail against the original defect and pass after the fix.
Characterization checks instead establish the existing passing baseline.

Preserve tests that catch distinct failures. Update expectations for intentional
behavior changes and explain why; do not weaken checks merely to obtain green.
Scale verification to affected behavior and honor required repository gates.
