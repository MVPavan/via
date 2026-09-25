# Localization — finding the failing layer

Load when you cannot yet name the failing component or layer — before ranking
hypotheses (Phase 3), or when probes keep landing in the wrong place (Phase 4).

## Layer tree

Walk the failure down the stack; each branch names its first probe:

```text
Which layer is failing?
├── Entry / API surface   → capture the request+response pair; contract vs actual payload
├── Business logic        → call the suspect function directly with the repro input
├── Data / store          → run the query alone; check schema, integrity, connection config
├── External dependency   → connectivity, version/API changes, rate limits, timeouts
├── Build / tooling / env → dependency versions, config files, env vars, CI vs local
└── The test itself       → wrong assertion, stale fixture, bad setup — a false
                            negative; fix the test, never delete or skip it
```

Failure appeared after changing *unrelated* code → a side effect: suspect
shared state, import-time effects, or test pollution (bisection below).

## Working vs broken comparison

Find the nearest working example of the same pattern in this codebase and
list **every** difference against the broken path — inputs, config,
environment, call order. "That can't matter" is banned; the difference you
dismiss is the usual cause.

## Per-boundary instrumentation

For a multi-component chain (API → service → store; CI → build → deploy),
instrument once, then read:

1. At each component boundary, log what enters and what exits, plus the
   config/env values that should propagate. Tag every line (Phase 4 prefix).
2. Run the loop **once**.
3. Read where the chain breaks — the boundary whose input is right and output
   is wrong names the failing component.
4. Investigate that component only. Leave the tags in place; they come out in
   Phase 6.

## Backward tracing to the origin

An error surfacing deep in the stack is a symptom; the fix belongs at the
source of the bad value.

1. Observe the symptom — where the error surfaced.
2. Find the immediate cause — the code that directly raised it.
3. Ask what called it, and what value it passed.
4. Keep tracing up: where did the bad value *originate*?
5. Fix at that origin — never only where the error appeared.

When the chain is not readable statically, capture it at the dangerous
operation, before it runs:

```python
import os, sys, traceback

print(
    f"[DEBUG-a4f2] about to <operation>: value={value!r} cwd={os.getcwd()}\n"
    + "".join(traceback.format_stack()),
    file=sys.stderr,
)
```

Print to stderr — logger config may swallow debug output under test. Run the
loop, grep the tag, read which caller injected the bad value.

## Commit bisection

When the bug appeared between a known-good and a known-bad state and the loop
is agent-runnable:

```bash
git bisect start
git bisect bad                    # current commit shows the bug
git bisect good <known-good-sha>
git bisect run <focused test command>   # exit 0 = good, 1–124 = bad, 125 = skip
git bisect reset
```

Use the repository's own focused-test command (the test-driven-development
skill's Discover-the-stack rule) — never a guessed default.

## Test-pollution bisection

State from one test breaks another — a test passes alone but fails in the
suite, or files appear where they shouldn't. Run:

```bash
# run from the repo root — the script discovers test files from the cwd
<systematic-debugging-skill-dir>/scripts/find-polluter.sh <pollution-path> <test-glob> <test-command...>
# e.g. <systematic-debugging-skill-dir>/scripts/find-polluter.sh 'scratchpad/leftover.db' 'tests/**/test_*.py' uv run pytest
# the pollution path is an artifact the tests create — never .git, a source root, or a path outside the repo
```

It runs test files one at a time with your runner and halts at the first file
after which the pollution exists. Exit codes: 0 = no polluter found, 1 =
polluter found (named in output), 2 = usage error or pre-existing pollution.
