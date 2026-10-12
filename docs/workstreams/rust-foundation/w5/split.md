# W5-S: split `via-core/src/engine.rs` by responsibility

Model: Opus 5.5 medium. Follow `../w1/common.md`; report to
`reports/W5-S.md`. Behaviour-preserving refactor only: no semantic change,
no renamed public items, no new dependencies, no test weakened.

Goal: later workers can own disjoint files. Split along the
responsibilities that W1–W4 kept colliding on, for example:

- `engine.rs` (or `engine/mod.rs`): `Engine`, `open`, the C1 verb entry
  points, shared small types;
- `engine/drive.rs`: turn driving, `execute`/`observe`, event commits,
  `finish`/`commit_turn_ended`, failure classification;
- `engine/stop.rs`: `request_stop`, `StopMode`, forced turns, final
  `shutdown`, cancel settlement;
- `engine/journal.rs`: already separate; keep it.

Choose the exact boundaries from the code; keep visibility as narrow as
today (`pub(super)`/`pub(crate)` only where a sibling needs it). Move
unit tests with the code they test.

Acceptance: the full gate passes with the same test count (122 passed,
2 ignored at `e24bb2c`); `git diff --stat` shows moves, not rewrites; the
report lists each new file's responsibility and any item whose visibility
had to widen, with the reason.
