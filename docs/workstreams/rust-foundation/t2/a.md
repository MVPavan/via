# T2-A: failpoint controller, F8 and F10

Model: Opus 5.5 high. Follow `../w1/common.md`; report to `reports/T2-A.md`.
Runs in parallel with T2-B.

Contract: `docs/specs/runtime-contracts.md` §11 (feature `test-failpoints`,
the controller, the point/seam table, §11.1 fixture keys) and the failpoint
gate lines in `.repo-context/verification.md`, which currently fail because
no crate defines the feature.

1. **Controller.** Implement the §11 controller exactly: feature
   `test-failpoints`, default off, forwarded by `via-cli` only to owners
   that need it; private per-scenario directory plus token; commands name
   point, occurrence and action `pause`, `crash` or `fail_io`; entry is
   acknowledged so the harness can assert durable state before releasing
   or killing the daemon; no prompt, handle or secret in acknowledgements.
   All code and env parsing inside `#[cfg(feature = "test-failpoints")]`;
   a no-feature release build cannot activate anything (prove it with the
   verification release-build check). Add harness support in
   `crates/via-cli/tests/support/`.
2. **Task 2 points:** `store.spawn.before_commit`, `.after_commit`,
   `core.intent.after_commit`, `wire.prompt.after_write`,
   `core.accept.before_commit`, `store.commit.reply_lost`. Other §11 points
   belong to later tasks; do not add them speculatively.
3. **F8** (`s1_f08_*`): crash in the middle of `spawn`'s write → session,
   turn 1, handle hash and key exist together or not at all; a lost reply
   replays one receipt.
4. **F10** (`s1_f10_*`): crash after the prompt reached the agent, before
   acceptance was recorded → after restart the turn is `unknown`, never
   re-dispatched; a submission record precedes any agent I/O.

Owned: failpoint modules in the crates that host a point, feature wiring in
`Cargo.toml`s, test support, the new tests, verification lines. In
`engine/drive.rs` and Store's commit path, add call sites only; T2-B owns
that logic. Gate: the default gate and every failpoint gate line pass.
