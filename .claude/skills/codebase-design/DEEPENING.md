# Deepening by dependency shape

Use the codebase-design definitions when deciding whether a module can hide
complexity without moving it into callers. Dependency categories suggest options,
not automatic permission to merge modules.

| Dependency | Candidate test/design strategy |
|---|---|
| In-process computation/state | Test meaningful behavior directly; preserve useful ownership boundaries |
| Locally substitutable I/O | Real lightweight instance or faithful fake, with integration coverage for differences |
| Remote owned service | Explicit transport contract; inject a suitable adapter when it isolates a present concern |
| External service | Test the owned boundary with safe fixtures/fakes; verify important provider assumptions separately |

Encapsulation or a present testing need can justify an interface with one adapter.
Internal seams need not become public API. Preserve tests that catch meaningful
failures; remove old tests only when their behavior is covered or intentionally
retired. Interface tests and focused internal algorithm tests can complement each
other. A changed test needs inspection, not automatic deletion or rejection.
