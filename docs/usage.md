# VIA user guide

This guide covers the first release (VIA 0.1.0). The authoritative contract
is the public API, [C1](specs/via-api-v1.md); this guide describes how the
`via` CLI exposes it.

- [Install](#install)
- [Prerequisites](#prerequisites)
- [Concepts](#concepts)
- [The daemon](#the-daemon)
- [Harnesses, routes and models](#harnesses-routes-and-models)
- [Bounds](#bounds)
- [Background and foreground turns](#background-and-foreground-turns)
- [Verbs](#verbs)
- [The result envelope](#the-result-envelope)
- [Exit codes and errors](#exit-codes-and-errors)
- [Warnings](#warnings)
- [Where state lives](#where-state-lives)
- [Configuration (`daemon.json`)](#configuration-daemonjson)
- [Known limitations](#known-limitations)

## Install

Linux x86_64 only. macOS is not part of this release.

Build from source with the toolchain pinned in `rust-toolchain.toml` (rustup
installs it on first use):

```bash
cargo build --release --locked
# binary: target/release/via
```

### Static (musl) binary

The release target is a fully static `x86_64-unknown-linux-musl` binary
([platform packaging](specs/platform-packaging.md) §2–§3). Building it needs
the musl Rust target and `musl-gcc` (Debian/Ubuntu package `musl-tools`),
because the bundled SQLite is compiled with the target's C compiler:

```bash
rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --locked -p via-cli --bin via --target x86_64-unknown-linux-musl
# binary: target/x86_64-unknown-linux-musl/release/via (static-pie, no shared libraries)
```

`scripts/build-release.sh` (run from the repository root) does this build,
checks the binary is static, runs `scripts/check-release-features.py` (no
test failpoints in the release), checks `via --version` against the crate
version, and writes `dist/via-<version>-x86_64-unknown-linux-musl` with a
`.sha256` beside it.

Install by copying the binary into a directory on your `PATH`, for example:

```bash
install -m 755 dist/via-<version>-x86_64-unknown-linux-musl ~/.local/bin/via
via --version
```

One binary contains both the CLI and the daemon.

## Prerequisites

- The harness CLI for each harness you use, installed and **logged in by
  you**: `claude`, `codex`, `opencode` or `pi`. VIA looks the binary up on
  `PATH` (or uses `harnesses.<name>.binary`, see
  [Configuration](#configuration-daemonjson)).
- VIA uses the vendor's own stored login. It never reads, copies or stores
  credentials, and it does not pass API-key environment variables through:
  the daemon starts with a cleared environment holding only `HOME`, `PATH`,
  `LANG`, `USER`, `LOGNAME` and `XDG_RUNTIME_DIR`, captured from the CLI
  that started it.
- `jq` is handy for the examples below; VIA itself does not need it.

## Concepts

- **Session**: one conversation with one vendor session, on one route for its
  life. ID `s_` + 12 characters, e.g. `s_7f3k9q2mzr4c`.
- **Turn**: one prompt and its one result envelope. Address
  `<session>/<n>`, e.g. `s_7f3k9q2mzr4c/2`. Where a verb takes an address, a
  bare session ID means the session's latest turn.
- **Handle**: a secret `h_…` token that authorizes `resume`, `steer`,
  `cancel` and `close` on a session. `via spawn` generates one and prints it
  once, in the spawn receipt (`handle`). The daemon stores only its hash and
  can never show it again, so keep it. Read verbs (`status`, `wait`, `result`,
  `list`, `events`, `logs`) need no handle.

  The CLI reads the handle from, in this order: `--handle-file F`,
  `--handle-stdin`, the `VIA_HANDLE` environment variable, then
  `--handle h_…` (visible to other processes in the argument list, so prefer
  the others). `via spawn` also accepts one of these to use your own handle.

All output is JSON, one object per line: results on stdout, errors on
stderr. The `--json` flag is accepted on every verb; output is JSON with or
without it.

## The daemon

Any CLI verb starts the daemon automatically if none is running. There is one
daemon per OS user, reached over a user-only Unix socket. It exits on its own
after 60 s idle (no client connected, no running or queued work, no pending
cleanup).

```bash
via daemon status        # version, pid, sessions, live servers, health, storage
via daemon stop          # refused while any session is active or closing
via daemon stop --drain  # refuse new work, let accepted turns finish, then stop
via daemon stop --force  # force-close every session with unfinished work
```

`via daemon status` example (abbreviated):

```json
{"daemon_version":"0.1.0","pid":4242,"started_at":"2026-10-10T16:37:58.589Z",
 "sessions":{"idle":0,"active":0,"closing":0},"servers":[],"health":"healthy","store_failure":null,
 "harness_processes":{"limit":8,"in_use":0,"held_unproven":0},
 "socket_path":"…/via/via.sock","store_path":"…/.via/state/store.sqlite3",
 "limits":{…},"storage":{"free_bytes":…,"below_free_floor":false,"over_warn_size":false,…}}
```

`daemon stop` replies `{"stopping":true}` when the stop is accepted; that is
not proof the daemon has exited. `--drain` and `--force` together are
refused. The daemon reads its environment and `daemon.json` once at start: to
pick up a changed `PATH` or config, stop it and let the next command start a
new one.

Two more entry points exist for integrators: `via daemon` runs the daemon in
the foreground, and `via serve --stdio` proxies the C1 JSON-RPC protocol
between stdin/stdout and the daemon socket.

## Harnesses, routes and models

| Harness | `--harness` | Route | `--model` | Versions checked by maintainers |
|---|---|---|---|---|
| Claude Code | `claude` | `claude-cli` | `sonnet` (default), `opus`, `haiku`, or a Claude model ID passed through unchanged | 2.1.285, 2.1.289, 2.1.290 |
| Codex | `codex` | `codex-app-server` | a Codex model name, e.g. `gpt-6-luna`; judged against the catalog the Codex server reports | 0.159.2, 0.160.0, 0.160.1 |
| OpenCode | `opencode` | `opencode-serve` | `provider/id` as OpenCode names it; no default | 2.0.22 (the only version that runs) |
| Pi | `pi` | `pi-rpc` | `provider/id`, e.g. `openai/gpt-6-luna`; a bare `gpt-6-luna` is refused | 1.0.2 |

- `via spawn` requires `--model`. `via describe` accepts `--model` alone and
  picks the harness whose catalog lists it (`--model openai/gpt-6-luna`
  selects Pi).
- `via models` lists the bundled catalogs (Claude, Pi) and, while a Codex
  server is running, the models it reported. OpenCode lists none; use the
  model IDs from your OpenCode installation.
- Any vendor version runs, with the warning `vendor_version_untested` if it
  is not in the checked set, unless a handshake check fails. **Exception:**
  OpenCode runs only its checked version; any other is refused at the
  handshake (`handshake_refused`).

### Capabilities per route

| | Claude | Codex | OpenCode | Pi |
|---|---|---|---|---|
| `spawn`, `resume`, `close` | native | native | native | native |
| `cancel` | partial (`aborts_tools_then_result`) | native | native | native |
| `steer` | unsupported | unsupported | unsupported | unsupported |
| `--instructions`, `--effort` | native | native | native | native |
| `--output-schema` | native | native | unsupported | unsupported |
| `--max-steps` | partial (`agentic_turn_limit`) | unsupported | unsupported | unsupported |
| Bounds | `full` + `--network` | `read_only`, `workspace_write`, `full` | `full` + `--network` | `full` + `--network` |
| Token usage scope | `turn` | `turn` | `turn` | `turn` |
| Cost | reported, cumulative for the session | unavailable | per turn | per turn |

`steer` is deferred past this release on every route; `via steer` is refused
with `unsupported_verb`. To redirect an agent, `cancel` the turn and `resume`
with a new prompt. `via describe` prints the live capability set for a route.

Passing a parameter the route does not support (for example `--max-steps`
on Codex) is refused; `via describe` shows the refusal in advance.

## Bounds

A bound is the sandbox the vendor applies to the agent: `--bound
read_only|workspace_write|full`, plus `--network` to allow network access
(without `--network`, network is off) and `--allow-dir D` (repeatable) for
extra writable directories. `--network` and `--allow-dir` require `--bound`.
The agent always runs non-interactively: approval requests are declined
automatically and reported in the envelope.

| Route | Accepted |
|---|---|
| `codex-app-server` | `read_only` or `workspace_write` without `--network`; `full` only with `--network`. **Always pass `--bound` on a Codex spawn**: a turn with no bound at all is rejected when it starts; later turns inherit it |
| `claude-cli` | `full --network` only |
| `opencode-serve` | `full --network` only, no `--allow-dir` |
| `pi-rpc` | `full --network` only, no `--allow-dir` (Pi has no sandbox) |

`full` means the agent can run any command as your user. Use Codex's limited
bounds when you need containment. A bound set on `resume` applies to that
turn and later ones; an omitted bound is inherited.

## Background and foreground turns

- **Background** (`via spawn --background`): prints the spawn receipt and
  exits 0 at once. Collect the result with `via wait` or `via result`.
- **Foreground** (no `--background`): prints the receipt line, waits until
  the turn ends however long it runs, then prints the envelope line. Ctrl-C
  stops waiting (exit 130) but leaves the turn running.

`via resume` always returns as soon as the turn is accepted (it prints a turn
receipt); wait for it the same way.

Turns in one session run one at a time. A `resume` while a turn runs is
queued (at most 8 per session; `queue_position` in the receipt).

## Verbs

`via <verb> --help` lists every flag. Durations are milliseconds.

### `spawn`: new session and turn 1

```bash
via spawn --harness codex --model gpt-6-luna --bound workspace_write \
  --cwd ./myrepo --effort high --label refactor-auth \
  --prompt-file task.md --wall-ms 1800000 --background > receipt.json
```

Receipt (abbreviated):

```json
{"session_id":"s_7f3k9q2mzr4c","turn":"s_7f3k9q2mzr4c/1","state":"queued",
 "route":"codex-app-server","adapter_version":"1","vendor_version":null,"version_status":"untested",
 "capabilities":{…},"effective":{"model":"gpt-6-luna","effort":"high","bound":{…},
 "deadlines":{"wall_ms":1800000,"idle_ms":600000},"max_steps":null},
 "warnings":[],"handle":"h_…"}
```

Main flags:

- `--harness`, `--model` (required); exactly one of `--prompt TEXT` or
  `--prompt-file F` (`-` reads the prompt from stdin; a file may be up to
  16 MiB).
- Session settings, fixed for the session's life: `--cwd D` (made
  absolute; when omitted, the directory you run `via spawn` in, with
  symlinks resolved), `--instructions F` (a file whose text becomes the
  session's system instructions, up to 1 MiB), `--label L` (up to 120
  characters, filterable in `list`), `--allow-untested` (accepted for
  compatibility; no effect).
- Per-turn settings, inherited by later turns unless changed on `resume`:
  `--bound`/`--network`/`--allow-dir`, `--effort` (`low` … `max`, or a vendor
  value), `--output-schema F` (a file holding a JSON Schema draft 2020-12
  object; a file holding just `null` clears an inherited schema), `--wall-ms` (default
  3,600,000), `--idle-ms` (default 600,000), `--max-steps`.
- `--require steer,cancel` refuses the spawn unless the route supports those
  verbs natively (`verb:partial` also accepts partial support).
- `--idempotency-key K`: a retry with the same key, handle and parameters
  returns the original receipt instead of starting a second session. Pass the
  same handle (for example with `--handle-file`) when you retry.
- `--vendor harness.key=value` (repeatable): vendor options the adapter
  declares; options touching sandbox, permissions, model, cwd or session
  identity are refused.
- `-- ARGS…`: raw arguments appended to the vendor's command line for every
  launch of the session. Unverified by VIA; adds the `vendor_passthrough`
  warning. Refused on OpenCode.

### `resume`: add a turn

```bash
via resume s_7f3k9q2mzr4c --handle-file handle \
  --prompt "Run the tests and fix any failure." --op-key fix-tests-1
```

Prints `{turn, state, queue_position, effective, warnings}`. Accepts the
per-turn flags above; session settings (`--cwd`, `--instructions`, `--label`,
vendor arguments) are refused. `--op-key K` makes the call safe to retry: a
repeat with the same key and parameters returns the stored receipt.

### `steer`: input into the running turn

```bash
via steer s_7f3k9q2mzr4c --handle-file handle --text "Skip the docs folder."
```

Unsupported on every route in this release; it returns `unsupported_verb`
(exit 2).

### `cancel`: stop the running or a queued turn

```bash
via cancel s_7f3k9q2mzr4c --handle-file handle --wait
```

```json
{"turn":"s_7f3k9q2mzr4c/2","state":"cancelled","already_terminal":false,
 "cancel":{"outcome":"acknowledged","cleanup":"quiescent","requested_at":"…","settled_at":"…"}}
```

`--turn N` picks a turn (default: the running one, else the latest).
`--force-after MS` (default 10,000) is how long the vendor gets to stop
before VIA forces it. Without `--wait` the reply comes at once with
`state: running` and `cleanup: pending`. `cleanup: uncertain` means VIA could
not prove the agent's tools stopped. Cancelling an already finished turn
returns `already_terminal: true`.

### `close`: end the session

```bash
via close s_7f3k9q2mzr4c --handle-file handle --mode graceful --deadline-ms 10000
```

```json
{"session_id":"s_7f3k9q2mzr4c","state":"closed","cancelled_turns":["s_7f3k9q2mzr4c/3"],"cleanup":"quiescent","leftovers":null}
```

Cancels the running turn, drops queued turns and closes the vendor session.
`--mode force` stops running work at once. Repeating a close is safe.

### `status`: a session and one turn's progress

```bash
via status s_7f3k9q2mzr4c --turn 2
```

Abbreviated:

```json
{"session_id":"s_7f3k9q2mzr4c","state":"active","harness":"codex","model":"gpt-6-luna",
 "route":"codex-app-server","vendor_version":"0.160.1","version_status":"tested",
 "active_turn":{"n":2,"state":"running","phase":"accepted","started_at":"…","cancel":null},
 "progress":{"turn":2,"current_step":4,"phase":"tools","running_tools":["shell"],"last_activity_at":"…",
             "tokens":{"total":18200,"scope":"turn"}},
 "steps":{"turn":2,"items":[…],"next_after":3,"more":false},
 "queue":[],"turns":[{"n":1,"state":"completed","revision":0},{"n":2,"state":"running"}],
 "warnings":[],"label":"refactor-auth","created_at":"…","updated_at":"…"}
```

Poll `status` to watch a running turn. `--after-step N` and `--limit N`
(default 100, max 1000) page the step history.

### `wait`: block until a turn ends

```bash
via wait s_7f3k9q2mzr4c/2 --timeout-ms 900000
```

Prints the result envelope once the turn is terminal. The default bound is
30,000 ms; at the bound it fails with `wait_timeout` (exit 2) and the turn
keeps running, so just wait again. Exit 3 if the turn ended `failed`,
`cancelled` or `unknown` (the envelope is still printed). Ctrl-C exits 130
without affecting the turn.

### `result`: the envelope, now

```bash
via result s_7f3k9q2mzr4c/2
```

Returns the envelope of a finished turn, or `turn_not_finished` (exit 2).
Exit 3 if the turn ended `failed`, `cancelled` or `unknown`.

### `list`: sessions, newest first

```bash
via list --state idle --harness claude --label refactor-auth --since 2026-10-01T00:00:00Z --limit 20
```

```json
{"sessions":[{"session_id":"s_7f3k9q2mzr4c","state":"idle","admission":"open","harness":"claude",
  "model":"haiku","label":"refactor-auth","created_at":"…","last_active_at":"…"}],"next_cursor":null}
```

Pass `next_cursor` back as `--cursor` for the next page. `--since` matches
`last_active_at` and takes any RFC 3339 timestamp: `2026-10-01T00:00:00Z`,
with fractional seconds (`2026-10-01T00:00:00.000Z`, as VIA prints them) or
with an offset (`2026-10-01T02:00:00+02:00`). Session states are `idle`,
`active` and `closed`.

### `events`: durable lifecycle events

```bash
via events s_7f3k9q2mzr4c --types turn.ended,warning --after 0 --limit 50
via events s_7f3k9q2mzr4c/2 --follow
```

```json
{"events":[{"seq":41,"session_id":"s_7f3k9q2mzr4c","turn":2,"late":false,"at":"…","type":"turn.ended",…}],
 "next_after":41,"more":false,"earliest_seq":1}
```

Events are lifecycle records (`session.opened`, `turn.queued`,
`turn.started`, `turn.ended`, `cancel.requested`, `action.denied`,
`warning`, …), not the model's text or tool calls; those stay in the agent's
own transcript (see `logs`). `--wait-ms N` (max 30,000) long-polls for new
events. `--follow` keeps polling from each page's `next_after` and prints
each non-empty page until Ctrl-C (exit 130).

### `logs`: where the evidence is

```bash
via logs s_7f3k9q2mzr4c/2
```

```json
{"session_id":"s_7f3k9q2mzr4c","turn":"s_7f3k9q2mzr4c/2","vendor_session_id":"019…",
 "transcript":"…/.codex/sessions/…","folder":"…/.via/state/evidence/s_7f3k9q2mzr4c/2",
 "files":[{"name":"stderr.log","bytes":312}]}
```

`transcript` is the vendor's own transcript (a hint, or `null`); `folder`
holds VIA's evidence for the turn: `stderr.log` (absent on shared-server
routes), `undecoded.bin`, and `final_text.txt` / `structured_output.json`
when a result was too large for the envelope. Read the files yourself.

### `describe`: preflight, no side effects

```bash
via describe --harness pi --model openai/gpt-6-luna --bound full --network --require cancel
```

Prints the route plan: `route`, resolved `model`, `capabilities`,
`effective_bound`, `version_status`, `refusals` and `warnings`. It starts no
process and writes nothing. A non-empty `refusals` list tells you why a
`spawn` with the same parameters would be refused, for example:

```json
"refusals":[{"field":"model","kind":"invalid_params","route":"pi-rpc",
  "message":"route pi-rpc takes a provider-qualified model, provider/id"}]
```

### `models`: the model catalog

```bash
via models --harness claude
```

```json
{"models":[{"model":"sonnet","harness":"claude","aliases":[],"source":"bundled"},
           {"model":"opus","harness":"claude","aliases":[],"source":"bundled"},
           {"model":"haiku","harness":"claude","aliases":[],"source":"bundled"}]}
```

## The result envelope

`wait` and `result` print the envelope; a foreground `spawn` prints it as its
second line. Abbreviated:

```json
{"api_version":1,"session_id":"s_7f3k9q2mzr4c","turn":2,"address":"s_7f3k9q2mzr4c/2","revision":0,
 "state":"completed","failure":null,"stop_reason":"end_turn","vendor_stop_reason":"…",
 "harness":"codex","model":{"requested":"gpt-6-luna","resolved":"gpt-6-luna"},
 "route":"codex-app-server","vendor_version":"0.160.1","version_status":"tested",
 "final_text":"All 42 tests pass.","final_text_file":null,
 "structured_output":null,"structured_output_file":null,
 "denied_actions":[],"auto_declined_requests":[],"steps":null,
 "usage":{"input_tokens":18000,"cached_input_tokens":12000,"output_tokens":900,
          "reasoning_output_tokens":300,"total_tokens":19200,"scope":"turn","provenance":"reported"},
 "cost":{"usd":null,"scope":"turn","provenance":"unavailable"},
 "duration_ms":48210,"evidence":{"folder":"…","transcript":"…"},"warnings":[],…}
```

| Field | Meaning |
|---|---|
| `state` | `completed`, `failed`, `cancelled` or `unknown` (VIA cannot tell how the turn ended, e.g. after a daemon restart) |
| `stop_reason` | `end_turn`, `max_steps`, `budget`, `refusal`, `interrupted`, `deadline`, `error` or `other`; the vendor's own word is in `vendor_stop_reason` |
| `failure` | `null`, or `{class, message, vendor_code?, retryable, data?}` when `state` is `failed`; classes below |
| `final_text` | the agent's final answer. Over 256 KiB it is `null` and `final_text_file` gives `{path, bytes, truncated}` |
| `structured_output` | the JSON validated against `--output-schema`, or `null`; large values spill to `structured_output_file` |
| `usage` | token counts; `scope` says what they cover (`turn`, `session_cumulative`, or `vendor_interval`: not verified per turn); `provenance` `reported` or `unavailable` |
| `cost` | `usd` (may be `null`), with its own `scope` and `provenance` (`reported`, `estimated`, `unavailable`). On Claude, `usd` is the session's cumulative cost |
| `warnings` | at most one entry per code, `{code, message, data?}`; see [Warnings](#warnings) |
| `cancel` | cancel outcome and cleanup certainty when the turn was cancelled |
| `denied_actions`, `auto_declined_requests` | actions the vendor's sandbox denied and approval requests VIA declined |
| `revision` | starts at 0; an `unknown` turn can later be revised when late vendor evidence arrives, so re-read it if you care |

Failure classes (`failure.class`): `deadline_wall`, `deadline_idle` (raise
`--wall-ms`/`--idle-ms`), `submit_failed` (rejected before the vendor accepted
it; `failure.data.reason` says why, e.g. `handshake_refused`,
`launch_failed`), `auth` (log in to the vendor CLI again), `rate_limit`,
`context_exceeded`, `budget_exceeded`, `vendor_error`, `resume_mismatch`,
`process_exited` and `server_lost` (check `stderr.log` via `via logs`),
`protocol`, `overflow`, `structured_output_invalid`, `daemon_restart`,
`store`.

## Exit codes and errors

| Exit | Meaning |
|---|---|
| 0 | success: the command printed its result; for `spawn` (foreground), `wait` and `result`, the turn `completed` |
| 2 | request error: a JSON-RPC error object on stderr (`{code, message, data: {kind, …}}`), including CLI argument errors (`invalid_params` with `data.field`) |
| 3 | foreground `spawn`, `wait` or `result`: the turn ended `failed`, `cancelled` or `unknown` (the envelope is still printed) |
| 4 | daemon unreachable: it could not be started or reached (`data.kind: daemon_unreachable`) |
| 130 | interrupted while a foreground `spawn`, `wait` or `events --follow` was waiting; the turn keeps running |

Exit 3 from `wait` and `result` applies from this release; earlier builds
exited 0 for those envelopes. `--help` and `--version` exit 0. Bare `via`
prints help on stderr and exits 2.

Common errors (`data.kind`) and what to do:

| Error | Fix |
|---|---|
| `invalid_params` | read `message` and `data.field`; `data.kind2` is `vendor_option_conflict`, `session_scope_on_resume`, `idempotency_conflict` or `unknown_field` when it applies |
| `unknown_model` | check the model name and form for the harness (`provider/id` for OpenCode and Pi); `via models` |
| `harness_unavailable` | install the vendor CLI and put it on `PATH`, then `via daemon stop` so the daemon restarts with the new `PATH`; or set `harnesses.<name>.binary`. `data.reason: handshake_refused`: the vendor version is not usable (OpenCode: install the checked version) |
| `bound_unsupported` | choose a bound the route accepts ([Bounds](#bounds)) |
| `missing_capability` | a `--require` verb is not supported on that route |
| `unsupported_verb` | the route does not support the verb (all `steer` calls in this release) |
| `invalid_handle` | pass the handle printed in the spawn receipt for this session |
| `session_not_found`, `turn_not_found` | check the ID; `via list` |
| `session_closed` | the session is closed or closing; spawn a new one |
| `queue_full` | 8 turns already queued in the session; wait or cancel some |
| `wait_timeout` | the turn is still running; call `wait` again or raise `--timeout-ms` |
| `turn_not_finished` | `result` on a running turn; use `wait` |
| `admission_refused` | a daemon-wide limit was hit, e.g. free disk space below the floor (default 5 GiB, `disk.free_floor`) or too many unresolved turns; free space or wait |
| `version_mismatch` | a daemon from a different `via` binary is running; `via daemon stop` it (it must be idle) and retry |
| `daemon_stopping` | the daemon is shutting down; retry after it exits |
| `store_error` | VIA could not persist state; see `via daemon status` (`health`, `store_failure`) and `<state>/via.log`. Do not blindly resend an unkeyed `spawn`; retry with the same `--idempotency-key` |
| `daemon_unreachable` (exit 4) | read `message`: it carries the daemon's startup error. Common causes: an invalid `daemon.json` (`daemon config invalid: …`, daemon exit 78); `runtime directory too long` (anchor socket paths must fit 108 bytes, so set a shorter `VIA_RUNTIME_DIR`); a daemon serving another state directory |

## Warnings

Warnings appear in receipts, envelopes and `status`. None of them stops a turn.

| Code | Meaning and action |
|---|---|
| `vendor_version_untested` | the installed vendor version is not in the checked set. It runs; prefer a checked version if you see odd behaviour |
| `config_switch_unverified` | VIA could not apply or verify an inherited-configuration setting (hooks, MCP servers, plugins, skills, agents, instruction files). `data.categories` lists `{category, requested, effective}`; your own vendor configuration may apply |
| `vendor_passthrough` | the session passes raw vendor arguments after `--`; VIA's guarantees depend on them |
| `instructions_partial` | the route prepended `--instructions` to the prompt instead of using a native system prompt |
| `usage_interval_unverified` | token numbers are reported but not proven to cover exactly this turn |
| `structured_output_missing`, `structured_output_invalid` | `--output-schema` was set but the agent returned no value, or one that fails the schema |
| `cancel_cleanup_uncertain` | the turn was cancelled but VIA could not prove its tools stopped; check for leftover processes (`leftovers` in the envelope) |
| `predecessor_cleanup_uncertain` | this turn started after a predecessor whose cleanup was uncertain |
| `observations_lost` | some vendor messages of a shared server were dropped; the envelope may be incomplete; the transcript has the full record |
| `credential_state_unchecked` | VIA could not check the credential state of a VIA-started vendor server on this version |
| `deprecated` | a request used a deprecated API feature |

## Where state lives

| Path | Contents |
|---|---|
| `~/.via/state/` (override: `VIA_STATE_DIR`) | `store.sqlite3` (sessions, turns, events), `daemon.json` (optional config), `via.log` (daemon warnings and errors, rotated at 10 MiB), `evidence/<session>/<turn>/` (per-turn evidence), `vendor/<harness>/` (adapter-private vendor state) |
| `$XDG_RUNTIME_DIR/via/`, else `~/.via/run/` (override: `VIA_RUNTIME_DIR`) | `via.sock`, the daemon lock, private anchor sockets |

Both overrides must be absolute paths without `..`. Directories are created
mode 0700. `via daemon status` prints the resolved `socket_path` and
`store_path`. Vendor transcripts stay where each vendor writes them; `via
logs` points to them.

## Configuration (`daemon.json`)

Optional, in the state directory, read once at daemon start. Unknown keys
are refused (the daemon will not start). Example:

```json
{"harness_processes":{"limit":8},
 "disk":{"free_floor":5368709120},
 "harnesses":{"claude":{"binary":"/opt/claude/bin/claude"}}}
```

- `harness_processes.limit`: how many vendor processes VIA runs at once
  (default 8). A Codex or OpenCode server takes one slot however many
  sessions it serves; each Claude or Pi turn takes one.
- `disk.free_floor`, `disk.warn_size`, and `wal.*`: storage thresholds in
  bytes.
- `harnesses.<name>.binary`: absolute path of the vendor CLI (default: a
  `PATH` lookup).
- `harnesses.<name>.inherit.{hooks,mcp_servers,plugins,skills,agents,instruction_files}`:
  booleans choosing whether the vendor loads your own configuration of that
  kind; `harnesses.claude.restricted` and `harnesses.codex.memories` are
  vendor-specific switches. See [runtime contracts](specs/runtime-contracts.md)
  §8 for defaults.

## Known limitations

- **Linux x86_64 only.** macOS, ARM64 Linux and Windows are not supported in
  this release.
- **No `steer`** on any route; cancel and resume instead.
- **OpenCode change-set overflow.** OpenCode's per-step changed-files event
  can exceed VIA's 1 MiB event limit when one step creates thousands of
  non-ignored files; the turn then fails with `overflow`. Deferred (bead
  `via-ty8`).
- OpenCode runs only version 2.0.22, refuses raw vendor arguments, and
  supports neither `--output-schema` nor `--max-steps`. Pi supports neither
  either.
- After a daemon restart (crash or forced stop), a turn that was running is
  reported `unknown`; VIA never resends it.
