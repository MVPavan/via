# W1: finish S1's review findings in cloud sessions

Plan: `../cloud-and-local.md` §6. Rules: `common.md`. Wave 1a runs A, B and C
in parallel on separate branches; the orchestrator merges them, runs the
gate, then launches D. Reports land in `reports/`.

| Task | Findings | Model | Owned paths (summary) |
|---|---|---|---|
| A | T1-I1, T1-I5 route side | Opus 5.5 high | via-wire, via-routes, via-fake-agent |
| B | T1-I2, T1-I3 | Opus 5.5 medium | via-cli client + request parsing, core request DTOs |
| C | T1-I5 Core/Store side, T1-I6 | Opus 5.5 medium | via-core engine + response DTOs, via-store events |
| D | shutdown design, T1-I7, T1-I4 | Opus 5.5 high | after A–C merge |
