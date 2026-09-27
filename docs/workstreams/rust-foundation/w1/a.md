# W1-A: stream drain after terminal (T1-I1) and route observations (T1-I5, route side)

Model: Opus 5.5 high. Follow `common.md` in this directory.

Owned paths: `crates/via-wire/`, `crates/via-routes/`, `crates/via-fake-agent/`
(only to add fake behaviours your regressions need), and new test files.

1. **T1-I1.** After the fake agent's terminal message the route stops
   reading and waits for exit: stdout/stderr tails go unrecorded, a second
   terminal is missed, a stderr flood can block exit, and Wire's stdout EOF
   returns before stderr EOF. Required: half-close input, then a bounded,
   concurrent drain of both streams to EOF, validating that nothing
   protocol-relevant follows the terminal (duplicate terminal = protocol
   failure), recording both tails in the raw log, all under the turn's
   absolute deadline. A stderr flood must never block exit or grow memory
   without bound (coding-style §5).
2. **T1-I5, route side.** The route discards assistant text, tool and
   unknown observations. Map every fake observation to the C2 observation
   stream (unknown kept as unknown with its raw value), not only
   acceptance/terminal.

Do not touch Core, Store, CLI or Host. Core-side event naming
(`turn.started`/`turn.ended`) belongs to W1-C.
