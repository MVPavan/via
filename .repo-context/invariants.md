# Invariants

Owner-decided or recorded constraints. Changing one needs the owner; surface
conflicts rather than working around them. Add mechanical checks as the
implementation develops.

## Decided

1. **Subscription/credential rule.** VIA invokes only the vendor's own binary
   or official SDK in its documented headless/programmatic mode, and never
   reads, copies or reuses vendor credentials; the user logs in through the
   vendor's tool. Terms uncertainty does not block development: build the
   adapter properly, disable it if the vendor does not permit the route, and
   record terms status per adapter (not a gate).
   Source: `docs/brainstorms/README.md` §7; `docs/workstreams/handoff.md`.
2. **One route per session.** The route chosen at spawn serves every later
   turn and verb in that session. The adapter version also stays fixed. Source:
   `docs/brainstorms/README.md` §15 (D5).
3. **Declared verbs, named refusals.** Each adapter declares each verb
   (`native`, `partial` with semantics, `unsupported`); unsupported verbs are
   refused by name. Never fake a verb: process kill is not graceful cancel, a
   follow-up prompt is not steer. Source: `README.md`;
   `docs/brainstorms/access-methods.md` §5 item 8.
4. **Single static binary.** Open-source; installable and usable from any
   language. CLI plus `via serve --stdio`; thin SDKs spawn the binary; no
   in-process native binding. Source: `docs/brainstorms/README.md` §13
   (settled inputs), §14.
5. **One VIA daemon per user.** It owns agent processes, vendor connections and
   the Store as its only writer. The CLI, `via serve --stdio` and thin SDKs are
   clients over a user-only Unix socket; the CLI auto-starts the daemon, which
   exits when idle. One binary includes the `via daemon` subcommand and refuses
   client/daemon version mismatches. Source: `docs/brainstorms/README.md` §15
   (D1).
6. **SQLite behind a small storage interface.** Chosen on requirements
   (embedded, crash-safe, static cross-builds). The daemon is the Store's only
   writer. Revisit Turso when it reaches a stable 1.0.
   Source: `docs/brainstorms/README.md` §15 (D1); §14.
7. **Platforms:** macOS and Linux first, then WSL, native Windows later.
   Source: `docs/brainstorms/README.md` §14.
8. **Language: Rust.** Stable 1.98.1, edition 2024, in the single `via` binary.
   Source: `docs/brainstorms/README.md` §15 (D8).
9. **Vendor SDK routes: not used for now.** Use vendor servers where available,
   otherwise vendor CLIs; native ACP is for breadth. No bridged ACP or acpx
   runtime dependency for now. Revisit under the conditions in
   `docs/brainstorms/routes-decision.md`. Source:
   `docs/brainstorms/README.md` §15 (D8).
10. **Never ask.** A session begins with out-of-bound actions denied, not
    prompted. L3 automatically declines vendor requests under a deadline and
    reports denials and declines in the turn envelope. The bound carries over
    unchanged unless the caller explicitly sets a new one on resume through
    the handle. VIA revalidates it against the route and records it per turn;
    nothing changes it silently. Source: `docs/brainstorms/README.md` §15
    (D3, D5).
11. **Own vendor servers.** VIA starts its own vendor servers and never
    attaches to or stops servers it did not start.
    Source: `docs/brainstorms/README.md` §15 (D8).

## Provisional

- **Remaining handoff proposals** (envelope detail, per-spawn isolation,
  pinned adapters, passthrough marked unstructured, background/wait) and the
  v0 scope ladder await owner confirmation. Source:
  `docs/workstreams/handoff.md`.
