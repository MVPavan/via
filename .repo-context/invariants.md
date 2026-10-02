# Invariants

Owner-decided or recorded constraints. Changing one needs the owner; surface
conflicts rather than working around them. Add mechanical checks as the
implementation develops.

## Decided

1. **Subscription/credential rule.** VIA invokes only the vendor's own binary
   or official SDK in its documented headless/programmatic mode, and never
   reads, copies or reuses vendor credentials. One narrow, owner-approved
   exception (2026-10-01): a report-only leftover scan may transiently read
   the environment of same-uid processes started at or after a vendor launch,
   solely to match VIA's exact process marker; nothing else is parsed, kept,
   logged or sent (C2 §4.2; adapter design AD20). The user logs in through the vendor's tool.
   Terms uncertainty does not block development: build the
   adapter properly, disable it if the vendor does not permit the route, and
   record terms status per adapter (not a gate).
   Source: `docs/brainstorms/README.md` §7; `docs/workstreams/handoff.md`.
2. **One route per session.** The route chosen at spawn serves every later
   turn and verb in that session. A session's stored state is used only by an
   adapter version that declares that state compatible: resume or reopen under
   an incompatible adapter version is refused, and a compatible resume
   advances the session's recorded adapter version. Each turn records the
   adapter version that ran it. Source: `docs/brainstorms/README.md` §15
   (D5); owner decision OD5c, 2026-09-30
   (`docs/workstreams/rust-foundation/adapters/design.md` AD12).
3. **Declared verbs, named refusals.** Each adapter declares each verb
   (`native`, `partial` with semantics, `unsupported`); unsupported verbs are
   refused by name. Never fake a verb: process kill is not graceful cancel, a
   follow-up prompt is not steer. Source: `README.md`;
   `docs/brainstorms/access-methods.md` §5 item 8.
4. **One installed VIA executable.** It contains CLI and daemon. Linux
   artifacts are fully statically linked. macOS artifacts statically include
   VIA, Rust dependencies and SQLite, and may dynamically link only the
   Apple-provided system libraries explicitly allowed by
   `docs/specs/platform-packaging.md` §3. No separately installed language
   runtime, VIA helper executable or third-party shared library is required.
   Open-source; installable and usable from any language. CLI plus
   `via serve --stdio`; thin SDKs spawn the binary; no in-process native
   binding. Source: `docs/brainstorms/README.md` §13–§14; P-OWNER-1 in
   `docs/specs/platform-packaging.md` §1.
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
7. **Platforms:** Linux and macOS are the first family, then WSL, native
   Windows later. Linux is the only required current-goal release target;
   macOS artifact production, linkage inspection and native qualification
   are deferred together, not passed. Source: `docs/brainstorms/README.md`
   §14 and `docs/specs/platform-packaging.md` §1.
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

12. **First-release scope:** Claude Code, Codex and OpenCode only. Implement
    the full C1 method surface, including background/wait, cancel and steer;
    each adapter declares actual support and returns named refusals where
    unsupported. ACP agents and additional harnesses are later scope, not
    first-release gates. Source: `docs/brainstorms/README.md` §16.
13. **Thin wrapper: vendor upgrades are the vendor's.** VIA wraps the coding
    agent and nothing more. VIA owns its integration with whatever vendor
    version runs:
    - launching the configured binary;
    - reporting the version it actually speaks, and refusing unsupported
      versions;
    - fixing its adapter when a version changes the protocol VIA reads;
    - keeping VIA's own state consistent (invariant 2 covers VIA's adapter
      versions).

    How the vendor behaves across its own upgrades is out of scope:
    - resuming or migrating threads or sessions an older version made;
    - vendor data compatibility;
    - detecting a binary replaced under a running vendor process, or moving
      live work onto it;
    - vendor regressions.

    Do not add machinery for these. Owner decision, 2026-10-02.

## Contract status and remaining decisions

The S1 contract set, Rust coding standard, testing policy and S1 scope are
approved. C1 and C2 supersede older handoff proposals about the envelope,
background/wait and capabilities. Vendor-dependent decisions remain identified
in their contract tables and are resolved with evidence in the relevant slice;
ACP-specific work is outside the first release. Passthrough and thin SDK
delivery are not part of the first-release acceptance set.
