# W5: prepare S1 Task 2

Task 1 (`via-jm4.7.5`) and its leaves (`.7.4`, `.7.12`, `.7.13`) are closed
at `e24bb2c`. Before Task 2 (`via-jm4.7.6`) runs parallel workers, split
`via-core/src/engine.rs` (1,278 lines, the conflict point of every W1–W4
wave) so workers own disjoint files.

Rules: `../w1/common.md`, except reports go to `reports/<task id>.md` here.
Review loop: `../cloud-and-local.md` §6.

| Task | Scope | Model | Brief |
|---|---|---|---|
| W5-S | Behaviour-preserving split of `engine.rs` | Opus 5.5 medium | `split.md` |
