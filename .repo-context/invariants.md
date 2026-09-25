# Invariants

Owner-decided or recorded constraints. Changing one needs the owner; surface
conflicts rather than working around them. None is mechanically checkable yet
(no code exists), so `check-invariants` has nothing to run.

## Decided

1. **Subscription/credential rule.** VIA invokes only the vendor's own binary
   or official SDK in its documented headless/programmatic mode, and never
   reads, copies or reuses vendor credentials; the user logs in through the
   vendor's tool. Terms uncertainty does not block development: build the
   adapter properly, disable it if the vendor does not permit the route, and
   record terms status per adapter (not a gate).
   Source: `docs/brainstorms/README.md` §7; `docs/workstreams/handoff.md`.
2. **One route per managed run.** The route chosen at spawn serves every later
   verb on that run. Source: `docs/brainstorms/access-methods.md` §3.
3. **Declared verbs, named refusals.** Each adapter declares each verb
   (`native`, `partial` with semantics, `unsupported`); unsupported verbs are
   refused by name. Never fake a verb: process kill is not graceful cancel, a
   follow-up prompt is not steer. Source: `README.md`;
   `docs/brainstorms/access-methods.md` §5 item 8.
4. **Single static binary.** Open-source; installable and usable from any
   language. CLI plus `via serve --stdio`; thin SDKs spawn the binary; no
   in-process native binding. Source: `docs/brainstorms/README.md` §13
   (settled inputs), §14.
5. **Routes: CLI, vendor RPC, ACP only.** Settled input to the language
   council; it narrows the SDK-layer recommendations in
   `docs/brainstorms/access-methods.md` §6. Source: `docs/brainstorms/README.md` §13.
6. **No daemon.** One worker process per run; CLI and `via serve` processes
   access the store concurrently. Source: `docs/brainstorms/README.md` §14.
7. **SQLite behind a small storage interface.** Chosen on requirements
   (embedded, crash-safe, multi-process, static cross-builds). Revisit Turso
   when it reaches 1.0 with stable multi-process WAL.
   Source: `docs/brainstorms/README.md` §14.
8. **Platforms:** macOS and Linux first, then WSL, native Windows later.
   Source: `docs/brainstorms/README.md` §14.

## Provisional

- **Language: NOT decided.** Go leads at ~60%, conditional on a pure-Go SQLite
  driver passing multi-process WAL with `CGO_ENABLED=0` (prototype gate 7) and
  the other feasibility-spike gates. The prototype plan needs owner discussion
  before it starts. Source: `docs/brainstorms/README.md` §13, §14;
  `docs/brainstorms/lang-council/chair.md`.
- **Handoff properties** (one envelope, role selection, uniform verbs,
  durable run record, per-spawn isolation, pinned adapters, passthrough marked
  unstructured, background/wait) and the v0 scope ladder are proposals pending
  owner confirmation. Source: `docs/workstreams/handoff.md`;
  open decisions in `docs/brainstorms/README.md` §8.
