# OpenCode serve adapter contract

Status: **design candidate rev7 for 2.x, 2026-10-06, via-4sw.3** (an
owner-approved cut-down: no mechanisms for unobserved problems; each
hypothetical risk is a named live qualification item, §13). Pinned to
**OpenCode 2.0.22**, SHA-256
`32cf5aa0a69a650e36277e3315d189835ddc79fb9aa1d0aef5025be5af5ad122`.
Scope: one VIA-owned, private, shared `opencode serve --stdio` for all of
VIA (owner, 2026-10-06), reusing the [Codex packet](codex.md)'s
shared-server machinery by reference. Design plus evidence only. Revision
history is in `scratchpad/execution/opencode2/history.md`.

**Revision rev7 (owner decisions, 2026-10-06, bead via-4sw.3).** The first
release runs **one OpenCode server for all of VIA**: one private namespace,
whose project configuration is always on ("disable nothing"); a request to
turn off a category only the project switch could turn off is `unknown`
with `config_switch_unverified`, never a second server (§4.5). VIA's data
stays private and credential-free; the free anonymous provider is the
default, and a project may configure its own provider, whose secrets VIA
never keeps (§4.3). The user's own OpenCode service, user configuration and
data are never touched. The one-live-server fence holds across daemon
restarts as a kernel `flock` on the data root that the server's anchor
takes and the server inherits, not a Store record (§3.2): the
`"opencode:<namespace>:<generation>"` `owner_server` label is withdrawn
(it did not fit `ServerId`; this supersedes decision C-1 of
`scratchpad/execution/c2-gap/report.md`), and no Store schema change is
needed. Because the fence depends on per-version lock retention, only
checked versions run (§12). `vendor_args` stays refused (§2.2). Changed:
§§2.2, 2.3, 3, 4.1–4.5, 5, 6, 7.1, 7.3, 8, 9, 10, 11, 12, 13 (OC01, OC02,
OC02b, OC04, OC07, OC09, OC10, OC11, OC12b, L4, L5, L9, L11, L13), 14 and
15. Revisit items after the release are in §14.

Authority: [C1](../via-api-v1.md), [C2](../adapter-contract.md),
[runtime contracts](../runtime-contracts.md),
[platform contract](../platform-packaging.md),
[goal](../../workstreams/rust-foundation/goal.md) and
[invariants](../../../.repo-context/invariants.md). The C1, C2 and runtime
edits this packet required are integrated into those contracts (section 15).

Labels: **observed** (probe of 2.0.22, 2026-10-04/05), **source** (the
binary's embedded JavaScript), **doc** (the served OpenAPI document),
**proposal**. `E<n>` cites `scratchpad/execution/opencode2/evidence.md`, a
private, ephemeral local evidence packet (not a public fixture dependency).

**Sharing verdict.** The evidence shows no sharing hazard that a recipe
cannot fix. The one cross-session reach found, the code-mode
`opencode_session_move` / `opencode_session_rename` tools, is removed by
session deny rules, proved live (E43). What remains is within the same-user
boundary C1 §2 already excludes (§4.4).

## 1. Pin and evidence boundary

| Item | Value |
|---|---|
| Binary | `opencode` 2.0.22, Bun single-file ELF x86-64, SHA-256 above |
| Served OpenAPI | 3.1.0, SHA-256 `540fdf565da27de9df69b6c3864582344e74ac4ffa225c283b289481d215d241` (E7) |
| Platform | WSL2, Linux 6.6 x86-64; macOS deferred (§13) |
| Model access | No login; the vendor's no-key free catalog (`apiKey:"public"`); live turns used `opencode/mimo-v2.6-flash-free` (E9) |
| Probes | 20 cases, 36 private servers, each with a private `HOME`, every XDG root under the case directory and a fresh password; all stopped and confirmed absent; the owner's background service was never contacted |

## 2. Transport and launch

### 2.1 `serve --stdio`

`opencode serve --stdio` is **not** an API carried on stdin/stdout (E2). It
starts the loopback HTTP server, hands the URL over as one stdout line
`{"url":"http://127.0.0.1:<port>"}`, and lives until stdin ends (E4). It
reads the server password from `OPENCODE_PASSWORD`, then deletes
`OPENCODE_PASSWORD` and `OPENCODE_SERVER_PASSWORD` from its own environment
before serving (E2); tool children did not see it (E35). Without a password
it generates an unprinted one and every request is 401 (E5). OpenCode's own
`--standalone` client launches exactly `serve --stdio --port 0` with a
generated `OPENCODE_PASSWORD` (E3).

**Choice.** HTTP plus SSE on `127.0.0.1` in `--stdio` mode: stdio carries
only the URL handoff and the lifetime, so VIA chooses no port, needs no
listener scan, and closing stdin is the graceful stop (15–164 ms, E4, E6).
Wire needs bounded HTTP/1.1 and SSE splitting (runtime contracts §4). Plain HTTP mode is
rejected: tools would inherit the password.

### 2.2 Launch and handshake

| Step | Rule | Evidence |
|---|---|---|
| Launch | After version admission (below), Host's anchor starts the admitted, already-opened program file (fd-pinned exec) as `serve --stdio --hostname 127.0.0.1 --port 0`, cwd = the namespace directory (§3.2), environment exactly §4.1, stdin a pipe VIA holds open and never writes, stdout a pipe, stderr to the connection evidence folder (runtime §4) | E2, E3 |
| URL handoff | The first stdout line, at most 4 KiB, must be a JSON object whose `url` is `http://127.0.0.1:<1–65535>` with no path, credentials or query. Later stdout bytes are read, counted and discarded | E4 |
| Authentication | HTTP Basic `opencode:<password>` to that exact origin only; no proxies, no redirects, no unauthenticated fallback | E5 |
| Identity and version | `GET /api/info` must be 200 JSON with string `version` and integer `pid`; `pid` must equal the vendor child pid in Host's `Spawned` report, which Host passes up as passive data (runtime §5 `AcquiredProcess.vendor_pid`, through Wire to the route); `version` must equal the version admitted before launch (below), else the server is refused before publication | E5, E58 |
| Credential state | §4.3 | E41 |
| Catalog | `GET /api/model` (no location) repeated 200 ms apart until non-empty, decoded through §4.3's allow-list; cached per server generation with the version for `models`; effort is checked in `run_turn` against a fresh per-location fetch instead (§5) | E10 |
| Events | `GET /api/event` (SSE) opened; `server.connected` received before the server is published | E18 |
| Deadline | 30 s from spawn for all of the above, independent of any turn; a waiting turn's own deadlines end only its wait (Codex §2) | proposal |

No session mutation or prompt is sent before publication.

**Version admission before launch: probe and launch the same opened file
(owner rule, 2026-10-06; runtime §5 "Pinned program").** Only a checked
version (§12) may touch the namespace's data root, so the version is
established before the server starts, from the very file that will run:

1. The route stats the resolved program path and passes its file identity
   (device, inode, size, mtime, ctime) in the launch spec.
2. While Host configures the launch, before any ARM intent, the anchor
   opens that path (read-only, close-on-exec) and refuses the
   configuration unless the open file's identity equals the one passed:
   `ProgramChanged`, nothing launched, the acquisition fails as a transient
   startup failure (the next acquisition stats again).
3. Through that open file (`/proc/self/fd/<n>`), the anchor runs the
   vendor's own `--version`: stdin `/dev/null`, stdout kept up to 256
   bytes, stderr discarded, a 2 s bound (killed and waited for at the
   bound). It runs in a separate probe root,
   `<vendor_state_dir>/opencode/probe/`, with its own private `HOME`, XDG
   directories and `TMPDIR`, `OPENCODE_DISABLE_AUTOUPDATE=1` and no
   password, never the namespace's: `--version` writes a log and creates
   XDG directories (E58), but opens no database.
4. Host compares the trimmed output with the route's admitted outputs
   (`opencode v<v>` for each `v` in `checked`). Any other output, a
   non-zero exit or a timeout refuses the configuration before ARM: no
   server starts, the data-root lock is never taken (the anchor takes it
   only after an admitted probe), and the turn is `submit_failed` with
   `failure.data.reason:"handshake_refused"` (C2 §5), cached by binary
   identity, with a message naming the version found and the `checked`
   set.
5. On ARM the anchor execs the server through the same open file, so
   replacing or unlinking the path after step 2 cannot change what runs
   (observed for 2.0.22: an fd-pinned `--version` printed `v2.0.22` after
   the path was replaced and after it was unlinked, also from a
   close-on-exec descriptor, E58). `argv[0]` is the resolved program path.
6. The handshake's `/api/info.version` must equal the admitted version, as
   a consistency check; a difference is `handshake_refused`.

Every server generation runs its own probe (about 50 ms, E58); nothing is
cached except refusals. Source reading found the server re-executing
itself (`process.execPath`) only for client and service-install commands,
not in `serve`; a server-mode self-spawn would name the path, not the
pinned file (L13 includes it). macOS has no `/proc/self/fd`; its mechanism
is deferred with the platform (L10).

**Vendor argument passthrough: refused in the first release (owner,
2026-10-06; C2 §6.3).** Arguments on the server's argv are per server,
and §3.2's fence allows one live server on the one namespace's data root,
so a session with a different list would need a second server on a data
root that already has one. The route therefore refuses any non-empty `vendor_args` in
`plan` and `check_turn`, before any receipt or vendor I/O, as
`InvalidParam { field: "vendor_args" }`: C1 `invalid_params` naming
`vendor_args`, with no kind2, the same refusal the `fake` harness gives.
`vendor_option_conflict` is not used, because it means "sets what the
route owns", and this refuses every list, reserved or not. The session's
results carry no `vendor_passthrough`, since no session of this route has
the arguments.

Revisit after the release, if OpenCode passthrough is wanted (§14 R3).
How to support it is a future architecture choice that needs an owner
decision; one possibility is a separate server, namespace and data root
per distinct list, which the per-data-root lock (§3.2) would fence
unchanged. Reserved flags would then be extracted from the pinned binary, as
the Claude and Codex packets do: `serve --stdio --port --hostname`,
`--service`, `--standalone`, `--cors`, `--mdns*`, help and version, any
flag that selects configuration, profile, project, data directory or log
destination, every operand, and `--`.

**Failure classes.**

- **Transient startup failure**: a held data-root lock (§3.2), spawn
  error, exit before the URL line, the deadline passing (including an
  empty catalog), a connection or 5xx error, or a `pid` mismatch. The process is retired through Host; the acquiring
  turn fails through C2's server-acquisition failure path; nothing is
  cached. A 1.x binary (no `--stdio`) exits before the URL line and lands
  here.
- **Incompatible handshake**: a version probe outside `checked` (above,
  before launch); a first stdout line that is not JSON, is over 4 KiB, or
  is JSON of the wrong shape or with a non-loopback URL; `/api/info` 200
  with HTML, non-JSON or missing `version` or `pid`; an
  `/api/info.version` different from the admitted version; a first SSE
  event other than `server.connected`; or a required endpoint answering
  404. These are server-level `handshake_refused` (C2
  §5): no server starts (probe) or the server is retired through Host
  before publication, and the acquiring turn is `submit_failed` with
  `failure.data.reason:"handshake_refused"`. The refusal is cached under C2
  §5's key: program path and file identity (device, inode, size, mtime,
  ctime) plus the recipe hash, 10 minutes, so a binary replaced at the same
  path retries at once. An unchecked version's message names the version
  found and the `checked` set. A readback that differs from a value VIA has
  just sent is a session-level refusal with its own key (§5).

### 2.3 Password

VIA generates 256 random bits per server generation, keeps them in daemon
memory and passes them only as `OPENCODE_PASSWORD` in that launch
environment; never in argv, Store, keys, traces, diagnostics or
decode-failure evidence; `Authorization` headers are stripped before any
transport logging. Tests use synthetic secrets and Boolean leak checks.

Under `--stdio` the vendor removes the password from the environment its
children inherit (source E2; observed for the shell tool, E35), so the 1.18
exception (tool children inherit the password) is **withdrawn** (C1
§9). This is not isolation: the server's initial environment
(`/proc/<pid>/environ`) is readable by same-user processes, so a full-bound
tool can recover the password and reach **every session on that shared
server**, which in the first release is every OpenCode session VIA runs
(§3). That sits inside the same-user boundary C1 §2 excludes for
full-bound sessions.

## 3. Shared ownership (A8, P11)

Same as Codex §2 "Shared ownership" for leases, pins, reservations,
publication after the handshake, concurrent equal-key acquisition, one
harness-process slot per live server, idle retirement when the last lease, pin
and reservation are released (stdin close, then S1's hard stop), daemon
stop, and never attaching to a pre-existing vendor server. In the first
release there is one namespace and so **one OpenCode server for all of
VIA** (owner, 2026-10-06): every OpenCode session leases it, whatever its
inherited-configuration request (§4.5), and it holds one harness-process
slot. Differences:

### 3.1 Launch key

```text
namespace   = (provider_profile_id, provider_profile_epoch)
launch_key  = namespace + recipe_hash
recipe_hash = H(domain "via-opencode-serve-v2", adapter_version,
                resolved_program_path, argv, passed environment
                (names and values, excluding OPENCODE_PASSWORD and Host's
                VIA_PROCESS_MARKER; namespace paths written as placeholders),
                generated config digest, server cwd placeholder,
                protocol pin "opencode-api-v2")
```

The first release has exactly one namespace,
`(opencode-free-anonymous-v1, 1)` (§4.1). Project configuration is always
on (§4.5), so it is no longer part of the namespace, and no session setting
selects a namespace. Nothing per session enters `recipe_hash` either
(`vendor_args` is refused, §2.2), so every OpenCode session of a daemon run
has the same launch key. A launch key that differs from the live server's,
should one arise, cannot start a second server: it waits for that server's
retirement under the fence (§3.2).

Not in the key: credentials, binary contents or stats (they key only the
refusal cache), the observed version (reported, not keyed; an upgrade takes
effect at the next launch, Codex parity), the bound, model, effort, agent,
instructions, permission rules and session cwd (all per session, §5),
the session's inherited-configuration request (§4.5), and the password and
port. The bound leaves the key because v2 permission rules
are per session (E7, E15) and the route admits only `full` with
`network:true` (A4). `ServerReport.key` is 16 hex digits of
`H(launch_key)`.

### 3.2 Namespace and its fence

The namespace has one private directory
`<vendor_state_dir>/opencode/<16 hex of H(namespace)>/` (0700, runtime
§6.1) holding `home/`, `config/`, `data/`, `state/`, `cache/`, `runtime/`,
`tmp/`. The vendor database, which also stores any credential (E41), is
`data/opencode/opencode.db` (E2, E38). Every OpenCode session uses it, so
no per-session namespace record is needed. Sessions, messages, settings
and pending inbox items persist in the database across server restarts
(E31, E32, E53). VIA never reads, writes or contacts the user's own
user-level OpenCode configuration, data or service (§4.1; revisit items R1 and R2,
§14).

**One live server, across daemon restarts: the data-root lock.** The
vendor takes no lock: a second server over the same data root served the
same sessions (E32). VIA adds one, held by the kernel, not recorded in the
Store:

1. The namespace directory holds `server.lock` (0600, created on first
   use, never unlinked, as `daemon.lock` and `store.lock` are, runtime
   §6.1).
2. The server's anchor takes `flock(LOCK_EX | LOCK_NB)` on it while Host
   configures the launch, after an admitted version probe (§2.2) and before
   any ARM intent (runtime §5, exclusive
   launch lock). If the lock is held, the anchor refuses the configuration
   and exits: nothing is launched, and the acquisition fails at once
   through C2's server-acquisition failure path with the no-launch
   evidence (C1 `submit_failed`, `failure.data.reason: launch_failed`,
   with the `launch_failed` warning naming the lock step).
3. At ARM the anchor clears close-on-exec on that descriptor and starts
   the server, which inherits it; the anchor keeps its own copy. The
   anchor never calls `LOCK_UN` and never closes its copy: it holds the
   lock until it exits.

An `flock` lock belongs to the open file description. The kernel releases
it when every descriptor of that description is closed (each holder exits
or closes its copy), or earlier when any holder calls `LOCK_UN` on its
copy. The holders are the anchor and the server; children the server
spawns did not inherit it (E57: location and session shells, including
`setsid` escapes). **While the server keeps its inherited descriptor and
never unlocks it** (qualification gate L13, §13), the lock lives at least
as long as the server process, whatever ends it:

| Path | Lock |
|---|---|
| Idle retirement, drain or server loss | the anchor's own-group stop ends both holders; free for the next generation |
| Failed acquisition, before or after ARM | held while either holder lives; no anchor ID, Store row or proof is needed to know it |
| Daemon crash before ARM | only the anchor held it; it exits on controller EOF or its 5 s bootstrap bound; free |
| Daemon crash after ARM | each anchor stops its group on controller EOF (runtime §5.1); a server that survives (an orphan) still holds it, so no second server starts until it exits |
| Anchor killed from outside | the surviving server still holds it; free when the server exits |
| Reboot | no process survives, so no lock does |

This replaces any Store-based fence: no `owner_server` label, startup
anchor cohort, absence-proof gate or schema change is involved. An
OpenCode server's anchor is an ordinary `ProcessOwner::Server` with a
minted `ServerId`, as a Codex server's is. Host's absence proofs still
decide cleanup evidence (§10); they no longer gate a launch.

Within a daemon run the route starts a new server generation only after
Host's retirement of the previous one has returned (§8); turns that arrive
meanwhile wait for it within their acquisition budget. A refused lock
therefore means a process VIA no longer controls holds it: an orphan of a
crashed daemon, or a generation whose retirement left it running. Each
later acquisition tries again; nothing is cached or recorded.

Limits.

- The lock is advisory and same-user: a process that unlinks or replaces
  `server.lock`, or unlocks a descriptor of it, defeats it, inside C1 §2's
  boundary.
- Its hold through the server depends on the server keeping its inherited
  descriptor (E57). Losing it is unsafe only if the anchor is also gone
  (two holders become none while the server still serves); L13 is a
  qualification gate for it, and fixture OC02b (f) shows the boundary.
- A server child that inherits the descriptor through a spawn path not yet
  probed would keep the lock after the server exits: that fails closed
  (`launch_failed` until the child exits), never into two servers; L13
  covers the remaining spawn paths.
- The lock does not touch harness-process capacity. Core reserves a
  `harness_processes` permit for every recovered anchor row it cannot
  prove gone and releases it once an absence proof commits; but an
  identity-less pre-ARM intent or a row from an earlier boot can never get
  that proof, so its permit is never released (a recovery problem for
  every harness, bead via-joc). Enough such rows can exhaust
  `harness_processes.limit`, and a new OpenCode server then waits for
  capacity before it reaches `Configure`, even with the lock free. Stale
  rows never block through the lock itself.

A lease is held while the session's driver is open. Close of one session
detaches it (§6); only Host's lifecycle stops the server.

## 4. Environment, configuration, credentials and isolation

### 4.1 Launch environment (profile `opencode-free-anonymous-v1`, epoch 1)

Start from an empty environment:

| Keys | Value |
|---|---|
| `PATH` | reviewed explicit value |
| `LANG` | fixed `C.UTF-8` |
| `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`, `TMPDIR` | the namespace's private directories (§3.2) |
| `OPENCODE_CONFIG_CONTENT` | the generated configuration (§4.2) |
| `OPENCODE_DISABLE_AUTOUPDATE` | `1` (source flag, E14) |
| `OPENCODE_PASSWORD` | the generated password (§2.3) |
| `VIA_PROCESS_MARKER` | set by Host; tools inherit it (E36), so the leftover scan works with scope `server` |

Never forward provider keys, other `OPENCODE_*` (including `_API_KEY`,
`_AUTH_CONTENT`, `_DB`, `_MODELS_URL|PATH`), proxy credentials or caller
configuration. With no project provider configuration the vendor then uses
its public free route (E9); a project may configure its own provider, which
then applies (§4.3). VIA never
sets `OPENCODE_DISABLE_PROJECT_CONFIG`: project configuration is always on
(§4.5). The
per-session environment endpoint is unused: it replaces the tool
environment wholesale and is lost on restart (E36, E32).

### 4.2 Generated configuration

```json
{"$schema":"https://opencode.ai/config.json","autoupdate":false,"share":"disabled",
 "default_agent":"via",
 "tool_output":{"max_bytes":51200,"max_lines":2000},
 "permission":{"*":"allow","question":"deny"},
 "agent":{"via":{"mode":"primary","description":"VIA",
                 "permission":{"*":"allow","question":"deny"}}}}
```

V1 keys are converted to V2 (E14, E54). `OPENCODE_CONFIG_CONTENT` loads
**last**, after global and project documents (source and `/api/config`
order, E54); a project `default_agent` lost to it live (E54). `tool_output`
pins the vendor's preview defaults (E55); the largest tool event observed
was 55,022 bytes (E18), a measurement, not a guarantee. Keys VIA does not
set (a project's `instructions`, `plugins`, `mcp`, extra agent fields) still
always come from the project (§4.5). No `steps` (§5).

### 4.3 Credentials and provider configuration (owner rules, 2026-10-05 and 2026-10-06)

VIA's private data is credential-free; a project may configure its own
provider.

**Private data.** VIA **never** calls `GET /api/credential`: it returns
secret values (doc, E41). Stored credentials live in the namespace database
(E41), so a file check cannot see them.

- **Fresh namespace** (no `data/opencode/opencode.db`): clean by
  construction. VIA created it private and empty, never writes a credential
  and forwards none (§4.1).
- **Otherwise**, during the handshake: `GET /api/integration` (no
  location). On 2.0.22 its connection entries are typed
  `{type:"credential", id, label, method}` or `{type:"env", name}` with no
  value field (doc), and a stored synthetic credential's value was absent
  from the response (Boolean, E41).
  - Known shape with any connection: refuse startup,
    `unexpected_credential_state` (evidence of a stored credential); the
    acquiring turn fails through the acquisition path naming only
    integration IDs; not cached.
  - Known shape, no connection: proceed.
  - Unknown shape: skip the check, proceed, and add the warning
    `credential_state_unchecked` to every turn on that server generation.
    Only checked versions run (§12), so this needs a checked version's
    response to differ from its known shape.
- Keep no state file.

**Project provider configuration (owner, 2026-10-06).** A session
location's project configuration may define providers and models with
`settings` (including `apiKey`), `headers`, `body`, `options` and endpoints
(doc: `Config.ProviderEncoded`, `Config.ModelEncoded`; `Model.Info` and its
variants carry `settings`, `headers` and `body` in `/api/model`). It is the
user's own project configuration and applies, following "disable nothing";
the free anonymous route is only the default where no project sets a
provider. If the state directory lies inside a project, the server's own
location (§2.2's catalog) sees that project's providers too. VIA never
logs, stores, returns or writes into evidence any provider key, header or
endpoint secret, from any source:

- Every `settings`, `headers`, `body`, `apiKey` and `options` value is
  treated as secret. VIA decodes through two typed allow-lists and skips
  every other field without keeping it (serde `IgnoredAny`): a catalog
  entry of `/api/model` keeps `providerID`, `id`, `name` and
  `variants[].id`; a session model reference (`Model.Ref` in the session
  readback) keeps `providerID`, `id` and `variant`.
- **No payload capture anywhere** (runtime §4 exception): an OpenCode
  connection is opened with payload capture off, so neither Route's
  decode failures nor Wire's automatic oversize and unterminated-message
  captures write `undecoded.bin` or any other copy of HTTP or SSE bytes;
  the failure records only the endpoint or event type, status, length and
  failure kind.
- In production VIA calls only the endpoints §§2, 5–8 and 11 name. It
  never calls `GET /api/config`, `/api/provider*`, `/api/mcp`,
  `/api/plugin` or `/api/credential`; where §4.5 mentions them they are
  qualification probes.
- Vendor error text is never copied into an envelope, log or evidence
  (§7.3); only `error.type` and the status are kept.
- The vendor's stderr goes to the server's evidence folder (runtime §4) as
  on every route; whether pinned OpenCode writes a provider secret there
  is a gate in L4 (§13).

### 4.4 Isolation on a shared server

| Surface | Reach of one session into another | Control |
|---|---|---|
| HTTP API | Everything, with the password | Only the daemon is given it; a same-user tool can read it from `/proc` (§2.3), inside C1 §2's boundary |
| Event stream | One stream carries every session's events (E16) | Demultiplexed by session (§7.1) |
| Model tools | Code-mode `opencode_session_move` / `opencode_session_rename` reach other sessions and move the current one (E37, E42) | **Denied** by session rule (§5); proved live for other-session rename, move and self-move (E43) |
| Tool environment | Shared server environment; no password (E35) | Leftovers have scope `server` (Codex parity) |
| Files and database | Full bound: tools can read the namespace database | Same-user boundary, C1 §2 |
| Saved approvals | Project-scoped `always` approvals apply to every session (E15) | VIA only ever replies `reject` |
| Blast radius | A session's tool can crash or stop the server, which every OpenCode session of VIA shares (§3) | Every leased session sees `server_lost` (§10), as on Codex |

### 4.5 Inherited configuration (C2 §6.2, owner OD2)

Project configuration is one server-level switch, and in the first release
it is **always on** (owner, 2026-10-06, following the release rule
"disable nothing"). The server reads each session location's walk-up
`.opencode`, `opencode.json(c)`, `.claude`, `.agents` and `AGENTS.md`, per
location on one shared server (source E13; observed for two locations,
E54). `OPENCODE_DISABLE_PROJECT_CONFIG=1` would stop all of them loading
(E12, E54), but it is per server: honouring it for one session would need
a second server on the one data root (§3.2), so VIA never sets it.
User-level sources are VIA's private, empty ones; the user's own OpenCode
configuration is never read (§3.2; revisit R1, §14).

`on` keeps C2 §6.2's meaning for every harness: the user's configuration
applies. Here only its project-level part can, so a requested `on` is
`unknown` for every category, instruction files and skills included, with
`config_switch_unverified` (owner, 2026-10-06). Project loading is still
observed (E12, E54); it does not make the state `on`.

A request to turn off a category that only the switch controls is not
applied: the session runs on the same server, its effective state is
`unknown` (C2 §6.2: an `off` VIA cannot apply), and it carries
`config_switch_unverified`. Skills are the one category a session can turn
off, by its own permission rule (§5).

C1 freezes effective states at spawn, so the frozen state is what the recipe
guarantees; later evidence (`/api/mcp`, `/api/plugin`, instruction deltas)
is a diagnostic only.

| Category (OD2 default) | Requested on | Requested off |
|---|---|---|
| instruction files (on) | `unknown`: project `AGENTS.md` per location loads (E12, E54); user-level instructions are private and empty | `unknown`: not applied, project files still load |
| skills (on) | `unknown`: project skills load (E12); user-level skills are private and empty | `off`: session rule `{skill,*,deny}` (§5, E12) |
| agents (on) | `unknown`: project agents load (source), no inventory (`/api/agent` returns `[]`, E11); VIA always runs its own `via` agent | `unknown`: not applied |
| plugins (on) | `unknown`: project plugins load (source) | `unknown`: not applied |
| MCP servers (off) | `unknown`: project MCP loads (source) | `unknown`: not applied; the MCP resource helpers are denied (§5) |
| hooks (off) | `unknown`: OpenCode hooks are plugin hooks | `unknown`: not applied |

Every `unknown` or differing state produces `config_switch_unverified`
(C1 §5). Every spawn therefore warns: with the default request for all six
categories; L4–L5 (G18–G19, §13) qualify what loads, but cannot make a
category `on` while user-level sources stay private. Only skills requested
`off` is verified and drops out of the warning.

## 5. Per-session settings

| Setting | Request and rule | Evidence |
|---|---|---|
| Model identity | `POST /api/session` `model:{providerID,id}`; the frozen identity is `(providerID, id)`; readback must match on every reopen | E7, E16, E32 |
| Effort (variant) | Per-turn state. Effort null ⇔ variant omitted ⇔ readback `"default"` (E46). A turn with a non-null effort fetches `GET /api/model?location[directory]=<canonical cwd>` fresh in `run_turn`, just before the variant step (no cache, no retry), and the requested variant must be in that response's `variants` for the session's model, else `Rejected(InvalidParam{field:"effort"})`, nothing sent. The check is needed because an unknown variant is accepted and stored silently, so the readback cannot reject it (E46); it is per location because project configuration applies per location (E54) and `/api/model` takes a location (doc). The vendor says a snapshot may precede plugin settlement (doc), so a location whose plugins have not settled can transiently refuse a variant they add: a clear `submit_failed` naming `effort` that the caller may retry. When the session model readback's `variant` differs from the request, `POST /api/session/{id}/model {model:{providerID,id,variant?}}` (omitting `variant` clears it), then read back: the readback's `variant` must equal the request (`"default"` for null), else the just-sent rule below (an ignored switch) | E9, E33, E46 |
| Agent | `agent:"via"` at creation; readback `via` (unknown names are accepted silently, E32) | E11 |
| Instructions | `PUT /api/experimental/session/{id}/instructions/entries/via {value}` after creation; readback must equal; null puts nothing. A context block the model sees (E12), not a system-prompt replacement. Vendor limit: **262,144 UTF-8 bytes of the JSON-encoded value** (E45); `plan` refuses more via `ParamSizes.instructions_json`, `InvalidParam{field:"instructions"}` | E12, E32, E45 |
| Permission rules | At creation: `{*,*,allow}`, `{question,*,deny}`, `{opencode_session_move,*,deny}`, `{opencode_session_rename,*,deny}`, `{opencode_list_mcp_resources,*,deny}`, `{opencode_read_mcp_resource,*,deny}`, plus `{skill,*,deny}` when skills are off; readback must equal exactly; never patched later. A tool whose last matching rule denies it is removed from the turn's tool snapshot (E42, E43) | E15, E42, E43 |
| Working directory | `location:{directory:<canonical cwd>}`; readback must equal; `move` is never called | E7, E16, E43 |
| Max steps | No per-turn field → **unsupported** | E7, E11 |
| Output schema | No structured-output field → **unsupported** | E7 |

Fixed at launch: binary, argv, environment, generated configuration,
password and server cwd.

**Readback rules.** A readback that differs from a value VIA has **just
sent** (creation, instruction entry, variant switch) is demonstrated
incompatibility: `handshake_refused` for that turn only; the server stays
published. It is cached under a **session refusal digest**: H(recipe hash,
the values the readbacks compare: model identity, agent, the exact
permission rule list, canonical cwd, the SHA-256 of the instruction text,
variant), with C2 §5's program identity. As Codex's `Echoed` inputs do,
this keeps the refusal off any session whose readback inputs differ: a
skills-off session's permission refusal never blocks a default session. On reopen VIA sent
nothing, so a difference is a session-state fault: `Rejected { reason:
StartRejected::SettingsMismatch { setting } }` with `setting: VendorSetting`
(`Model`, `Agent`, `Permissions`, `Instructions`) → `failed(submit_failed)`,
`failure.data.reason:"settings_mismatch"`, with `failure.data.field` for
`Model` and `Instructions` (C2 §2 `StartRejected`); not cached (C2 §5
refusal cache).

## 6. Operations

| C2 operation | OpenCode 2.0.22 mapping | Evidence |
|---|---|---|
| `describe` / `plan` | Process-free: bundled profile, effort mapping, last version and cached catalog for this program path; refusals per §12; `server_key` from §3.1 | — |
| `models` | Bundled entries plus the cached `GET /api/model` catalog of a live owned server (public fields only) | E9 |
| `check_turn` | Pure: `full,network:true` only; same refusals as `plan`; the prompt admission bound (§9); no location-dependent effort check: a variant is judged in `run_turn` (§5) | E9, E47 |
| `prepare` / `readiness` | `Pinned(Server)` when the OpenCode server is live or launching and not draining, else `NeedsConnection`; readiness changes on publication, drain start (§8) and retirement | C2 §3 |
| First `run_turn` of a connection generation, new session | `POST /api/session {model, agent:"via", location, permissions}`; persist the returned `ses_…` ID via `session.vendor_identity_confirmed`; then the instruction entry and readback. Never send a caller-chosen session ID: the free tier rejects it (403 FreeTierError, E29) | E7, E16, E29 |
| First `run_turn` of a connection generation, reopen | `GET /api/session/{id}`: 404, or a different `id` or `location.directory` → `ResumeMismatch`, never create. Then the settings readback (§5). On the session's first attachment in a server generation, the leftover cleanup of §7.2 | E31, E32, E46 |
| `run_turn` submission | The execution rule (§7.2), the effort step (§5), then one `POST /api/session/{id}/prompt {id:<caller ID>, text}`; outcomes §8 | E17, E30, E50 |
| Acceptance | 200 whose `data.id` is the caller ID and `data.sessionID` the session, or `session.inbox.enqueued{sessionID, inboxID:<caller ID>}`, whichever first; deduplicated | E17, E19, E30 |
| Observations, terminal, final text | §7 | E19, E23 |
| `steer` | **Unsupported in the first release** (owner deferral, as Codex and Pi). On 2.0.22 a busy `delivery:"steer"` prompt is injected at the next step boundary of the running execution with one terminal (E26), so AD10's "separate conversation" reason is stale | E26 |
| Interrupt (stop order; the wall's cleanup step) | §7.4 | E24, E49 |
| `close` (one session) | Cancel queued/active work under C1 (§7.4), then detach at a cutoff in decode order (C2 §3), release the lease; no vendor call; `vendor_closed:false`, `leftovers:null`; never `DELETE` the vendor session | — |
| Idle retirement, daemon stop | Same as Codex | E4, E25 |
| `health` | Sticky first cause: protocol, transport loss, overflow, `ServerLost`, `RetirementUncertain`, Store | C2 §2 |
| `recover` | No live rejoin: `Unknown`, or `Dead` only with Host-confirmed death; no resend (§10) | E31 |

## 7. Execution, routing and terminal

### 7.1 Routing and attribution

One SSE connection per server, opened before publication. Each `data:`
event's envelope is decoded first: `type` and the routing session
(`data.sessionID`, or `data.form.sessionID` for `form.created`, E44).
Events are routed by `(server generation, sessionID)` to the leasing
session's lane, then to a turn by these keys:

| Key | Learned from | Names |
|---|---|---|
| caller input ID | the turn (deterministic, below) | inbox events |
| assistant message ID | `session.step.started` | steps, text, tool events, usage samples, `permission.asked.source.messageID` |
| tool call ID | `session.tool.input.started` or `session.tool.called` | tool completions, `permission.asked.source.id` |
| child session ID | `session.created{parentID}` while the parent's execution is owned by turn T | the child's interactive requests → T |

Executions carry only `sessionID` (E19), so the execution a turn owns is the
one in which `session.inbox.delivered` named its caller ID; that
execution's terminal is the turn's terminal, even if a vendor-originated
input started it or joined it (E52, E53). After a turn settles its keys are
retained as a tombstone; later matching events are that turn's late
observations (`late:true` up to the close cutoff, C2 §3), never the
successor's. Unmatched events are counted diagnostics. Events without a
session (`*.updated`, `server.*`, `project.*`, `vcs.*`) are ignored; child
session events are not the root turn's observations (C1 §5) except
interactive requests (§11).

**Caller message ID.** `msg_via` + 22 base62 characters of
`H("via-opencode-input-v1", VIA session id, turn number)`, echoed as
`inboxID` (E30). It is never sent twice: a repeated ID returns the original
record and starts a spurious empty execution (E30, E56). Being
deterministic, VIA recognises its own leftover inputs without a Store
column (§7.2).

| Event | C2 observation |
|---|---|
| `session.inbox.*`, `session.execution.started` | execution state (§7.2); `inbox.enqueued` of the caller ID is acceptance |
| `session.text.delta`, `session.reasoning.*` | `progress` `model` |
| `session.tool.called` | `tools_started` |
| `session.tool.success`, `session.tool.failed` | `tools_ended`, always; a `tool.failed{error.type:"permission.rejected"}` also yields `action.denied` unless it is a VIA decline (§11) |
| `session.text.ended` | final-text candidate (§7.3) |
| `session.step.ended|failed`, `session.compaction.ended|failed` | usage sample (§12) |
| `session.execution.succeeded|failed|interrupted` | terminal (§7.3) |
| `permission.asked`, `form.created` | decline (§11) |
| other known types (`session.step.*`, `session.tool.input.*`, `session.retry.scheduled`, `session.usage.updated`, `session.instructions.updated`, `session.renamed`, …) | activity only |

**Forward compatibility.** The terminal set is an explicit allow-list of the
three types above; any other type, including an unknown
`session.execution.*` suffix, is activity only. Unknown fields are ignored.
A known type whose required fields do not decode fails that session's
driver `protocol` and drains the generation (§8), since the session's
execution state may have missed it; an undecodable envelope or non-JSON
`data:` fails the server generation `protocol`. Only events that carry a
`seq` are checked: a non-increasing per-session `seq` is `protocol` and a
gap is only a diagnostic (E20); events without one, such as permission
requests, are not checked.

### 7.2 The execution rule (E51, E52, E53)

OpenCode's interrupt and inbox controls address the session, not a turn
(E24, E49); concurrent inputs merge into one execution (E52); a delayed
interrupt stopped a later execution (E51); queued input survives a crash
and wakes into the next execution (E53). One rule prevents all three
hazards. **VIA sends a session's next prompt only when:**

1. every earlier request VIA made for that session (prompt, setup,
   interrupt, inbox cancel, decline) has a complete response or was never
   sent; and
2. the stream has shown the predecessor's execution end (its terminal, or
   `session.inbox.cancelled` of its never-delivered caller ID), or the
   predecessor was proven not accepted (§8: 400, 401, 404, or never sent);
   and no execution of the session is running.

If this does not hold within `min(remaining wall, 30 s)`, the turn is
`Rejected { reason: VendorError(Some(VendorCode("session_busy")), detail) }` →
`failed(submit_failed)`, nothing sent. The rule gates only a successor
prompt. Interrupts, inbox cancels and declines count as requests of rule 1,
so a stop still in flight blocks the successor even after the predecessor
ended naturally (E51); but they are never held behind the session's pending
prompt: they go out at once on their reserved pools (§8, C2 §2).

**Server-scoped session state.** The route's single per-server task that
consumes the stream records, per session and server generation, the facts
the rule needs: the last turn's state (sent, accepted, delivered, ended,
not accepted, never sent), whether an execution is running, and the
session's requests without a complete response. It applies each event
before staging it into the session's lane and keeps the record when a
driver is closed or fails, so a driver reopened on the same generation
(after an idle-lane close or an overflow) reads the same state. It records
everything decoded, including a terminal the lane could not admit, and is
used **only** for the rule above, never to decide a turn's outcome. The
generation's end discards it.

**Leftover cleanup.** When a session is first opened on a server generation
(reopen), before its first dispatch and outside the dispatch decision, VIA
reads `GET /api/session/{id}/inbox` once and cancels every item whose ID is
one of VIA's recomputed caller IDs for the session (a leftover input of an
`unknown` turn, E53): `DELETE …/inbox/{id}`, then wait for its
`inbox.cancelled` (the 204 proves nothing, E48). The cleanup records
nothing and revises no turn. Other items are left to the vendor; foreign
inbox writers are a qualification item (§13).

### 7.3 Terminal and final text

| Vendor terminal of the owned execution | `VendorTerminal` | Evidence |
|---|---|---|
| `succeeded` | `Completed`; stop reason from the last `step.ended.finish`: `stop` → `EndTurn`, `length` → `Budget`, `content-filter` → `Refusal`, else `Other` | E19, E8 |
| `failed{error}` | `Failed`; `vendor_code` = `error.type`, class hint §12; no vendor message text, which can echo project provider configuration (§4.3) | E29, E33 |
| `interrupted{reason:"user"}` after VIA's interrupt for this turn | `Interrupted` (acknowledgement, §7.4) | E24 |
| `interrupted{reason:"shutdown"}` after VIA's successful decline of a **permission** request correlated to this turn by `callID` | `Completed`, `Other`, decline recorded (VO1 b parity; owner Q5) | E27 |
| any other `interrupted`, including `shutdown` after only a form decline | `Failed`, `vendor_error`, `vendor_code:"interrupted:<reason>"` | E28 |
| `session.inbox.cancelled` of the turn's caller ID, never delivered | input-cancellation terminal (§7.4) | E49 |

A cancel racing a natural `succeeded` or `failed` keeps the natural
terminal (C1 §7.6). **Outcome boundary:** a turn's outcome is decided only
by what reaches its driver through its lane, in order; the server-scoped
state (§7.2) never decides it.

**Final text.** On `Completed` only: the `session.text.ended` texts of the
turn's last owned step, in `ordinal` order, sent as `final_text` pieces of
at most 256 KiB encoded (C2 §4); earlier steps' text is progress (E23).

### 7.4 Cancellation

A stop (caller cancel/close order, or the wall's cleanup step) for the
current turn:

| Turn state | Action | Acknowledgement |
|---|---|---|
| prompt not yet sent | withdraw it (§8); nothing reached the vendor | the stop ends the turn as C2 does for withdrawn input (Codex parity) |
| sent, not delivered | `DELETE …/inbox/{caller ID}`; never interrupt first (an interrupt leaves queued input parked, E49) | `inbox.cancelled{ID}` → input-cancellation terminal; `inbox.delivered{ID}` first → interrupt |
| delivered | `POST /api/session/{id}/interrupt` (no `resume`) | `execution.interrupted{reason:"user"}` of the owned execution → `Acknowledged`; `{"interrupted":false}` means nothing was running and the natural terminal decides |

At most one inbox cancel and one interrupt per turn per server generation.

**Input-cancellation terminal.** `session.inbox.cancelled` of the turn's
caller ID with no delivery is native evidence that the input never ran:
`VendorTerminal { status: Interrupted, stop_reason: Other,
vendor_stop_reason: "input_cancelled", vendor_code:
"session.inbox.cancelled" }`, no usage, cleanup `Quiescent`. After a caller
stop it is C1 §7.6 row 1 (`cancelled`, `acknowledged`); after the wall's
cleanup step the deadline failure stands; for an accepted turn that became
`unknown` at its order's `force_at` it revises the turn like a late terminal
(→ `cancelled`). A turn whose acceptance was never recorded is never
revised.

**Cleanup** for delivered turns: every reported tool item ended
(`tool.success` or `tool.failed`). The interrupt kills the tool's own
process group and `tool.failed{aborted}` precedes the terminal (E24, E25),
so cleanup is normally `Quiescent` at acknowledgement; otherwise wait within
`min(ack + tool_grace, wall)` (C2 §4.1), then `Uncertain`. Descendants that
leave the tool's group survive (E25); they are reported where a destination
exists (§10).

**Deadlines.** A caller order without acknowledgement by `force_at` → outcome
`unknown` (C1 §7.6 shared-server force row); never kill the shared server
for it. The wall's cleanup step has S1's 3 s bound; without acknowledgement
the turn stays `failed(deadline_wall)`, cleanup `Uncertain`.

## 8. Request outcomes and drain

Requests run on per-server connection pools, one request per connection:
**decline** 2 (permission reply, form `DELETE`; first priority, so A6's 5 s
holds while other requests wait), **stop** 2 (interrupt, inbox `DELETE`),
**general** 4 (prompts, session create, readbacks, settings, instruction
entries, inbox reads, catalog). Pools never borrow from each other. Only a
successor prompt waits for the session's unanswered requests (§7.2); a stop
or decline never waits for the session's pending prompt.

**Sent.** A request is *sent* once any byte of it was written; it can be
withdrawn only before that (runtime §4's existing rule). Each request has a
response timeout: a decline 5 s, a stop its order's `force_at` or S1's
cleanup bound, others `min(remaining wall, 30 s)`; at the timeout VIA closes
the socket. E56 observed that an interrupt whose header block was never
completed had no effect; this is evidence only, not a mechanism.

| Request | Complete response | Meaning |
|---|---|---|
| prompt | 200 JSON, `data.id` = caller ID, `data.sessionID` = session | accepted |
| prompt | 400 `InvalidRequestError` (payload decode, E56) | proven non-acceptance: `Rejected(Protocol("invalid_request"))` |
| prompt | 404 `SessionNotFoundError` (E56) | proven non-acceptance: `Rejected(SessionGone)` |
| any | 401 | proven non-acceptance of a prompt; the server generation fails `protocol` (the password no longer works) |
| interrupt | 200 `{interrupted}` | settled; acknowledgement only from the stream |
| inbox `DELETE` | 204 | settled; proves nothing about cancellation (E48) |
| permission reply | 204 / 404 | declined / request gone |
| form `DELETE` | 204 / 404 / 409 | cancelled / gone / already settled |
| create, model switch, entry `PUT` | 200 / 204 | settled; the readback decides (§5) |
| non-prompt setup | other complete status | the turn `Rejected(VendorError(Some(<error _tag or "http_<status>">), detail))`, its prompt never sent; the code is the vendor's error type (`_tag`, as `error.type` in §4.3) or the status, and `detail` is generated by VIA ("<request> answered <status>"), never vendor text |

**Everything else drains.** No complete response before the timeout, a
socket failure after a byte was sent, or an inconclusive or malformed
response (a prompt 409, 5xx or other status; a 200 that does not decode or
names another ID; any undecodable body) means VIA can no longer know the
request's effect. The server generation **drains** (Q10, decided):

1. The route publishes the drain as a readiness change: `prepare` gives
   `NeedsConnection`, Core reserves a slot (C2 §3 rule 3), and new turns
   wait for the next generation within their acquisition budget (the
   data-root lock, §3.2, forbids a parallel server).
2. No new prompt or setup request is sent on the generation. A turn pinned
   to it whose prompt was never sent ends with `StartRejected::SessionGone`
   (C2 §3 rule 4, as the driver already returns for a dead pin) →
   `submit_failed`; the caller may retry on the next generation.
3. Stops for turns already sent, and declines, are still sent.
4. Turns already sent continue to their end or to the generation's end.
   The turn whose prompt response was inconclusive resolves by what reached
   its lane first: its terminal stands; acceptance means it continues;
   otherwise C2's unknown-submission failure (`unknown`, never revised
   later). This is not a driver health failure: the stream is intact.
5. When no sent turn runs, the route retires the server through Host (stdin
   close, S1's hard stop, anchor absence proof). That also stops any vendor
   execution no turn owns. The next generation starts fresh (about 0.2 s to
   the URL line, E4); leftover cleanup (§7.2) runs at each session's reopen.

## 9. Bounds

| Resource | Bound | Full |
|---|---|---|
| SSE line and assembled event | 1 MiB | server generation `overflow` |
| Route staging | 1,024 messages / 4 MiB per server | server generation `overflow` |
| Per-session ingress lane | 16 messages / 1 MiB, within staging | lane overflow (below) |
| C2 observations | 1,024 items / 4 MiB per session, 10 s stall | lane overflow |
| Prompt admission (`check_turn`, `plan`) | `json_len(prompt) + json_len(cwd) ≤ 1,048,576 − 8,192` | `invalid_params` naming `prompt` |
| Instruction entry | 262,144 encoded value bytes (§5) | `invalid_params` naming `instructions` |
| HTTP | headers 64 KiB; response bodies 1 MiB (`/api/model` 4 MiB); JSON depth 64, 65,536 nodes | `protocol` |
| Final-text candidates | 4 MiB per turn | turn `overflow` |
| Retained per server | session states 1,024; tombstoned turns 4,096; child sessions 4,096; pending interactive requests 64; requests without a complete response 64 | server generation `overflow` |
| Correlation keys (aggregate) | caller input, assistant message and tool call IDs of every live and tombstoned turn, and child-session IDs, together: 65,536 entries and 8 MiB of retained ID bytes per server | server generation `overflow` |
| Liveness | heartbeats every 15 s (E18); 45 s without a byte | transport loss (§10) |

**Prompt admission (security).** The prompt is echoed whole in the 200
response and in `session.inbox.enqueued` (E47); OpenCode has no cap (3 MiB
accepted). The event's `data` is the JSON-encoded text plus about 424 bytes
(E47). `json_len` is the UTF-8 length of the JSON string encoding, quotes
and escapes included (serde_json and JavaScript escape identically for
valid UTF-8, E45, E47). A prompt that fits can never overflow the shared
stream through its echo, so one session cannot fail every session's
stream. Core fills `ParamSizes.prompt_json` and `cwd_json` (C2 §2).

Retained items are IDs and short fields copied from single events, each
at most 1 KiB (runtime §8's ID and code bound; longer is `protocol`). One
long turn can learn many assistant and tool IDs, so these are bounded in
aggregate, not per turn. Retained memory is therefore at most the 9,344
counted items above times 1 KiB plus the 8 MiB correlation bound, about
17 MiB per server before allocator overhead. Long-run memory is a
qualification item (§13).

**Lane overflow** is exactly Codex's rule (C2 §4): sticky driver overflow
with `ObservationLoss`; a terminal admitted before the overflow is kept;
every other nonterminal turn fails `overflow` (cleanup `Uncertain`, stop
posted as a cleanup intent), including one whose terminal found the lane
full. No drain: the server-scoped state (§7.2) still records the
execution's end, so the session's next turn, on a new driver, dispatches.

## 10. Server death, transport loss and recovery

A terminal admitted to a turn's lane before a loss signal is that turn's
result (C1 §7.6 first matching row); the loss is signalled after everything
already admitted. On SSE EOF or error the route stops dispatch and asks
Host for the process state within S1's cleanup bound (3 s):

| Host evidence | Every leased session's nonterminal turns |
|---|---|
| exit confirmed | `ServerLost` → `failed(server_lost)` |
| process alive | `TransportLost` → `unknown`; the server retires through Host |
| unconfirmed at the bound | `TransportLost` → `unknown`; later confirmation rewrites nothing |

Leftover reports, cleanup certainty from Host's group evidence and the
`server_turns` link are the same as Codex; tool processes in their own
groups can survive a SIGKILL of the server group (E25) and appear in the
shared leftover report (`scope: server`) without by themselves making
cleanup `Uncertain`.

**Restart and recovery.** A replacement starts only when its anchor gets
the data-root lock (§3.2), in the same daemon run and after a restart: an
orphan server a daemon crash left behind holds the lock, so no second
server starts until it exits. Each session is re-verified on its first `run_turn` (§6),
and its leftover inputs are cancelled (§7.2); the password rotates. After a
crash the vendor keeps no outcome: no completion, no idle record, nothing
resumes on its own (E31), queued input stays parked (E53). Daemon-crash
recovery follows P12: submitted or accepted turns become `unknown`; no
resend. Conversation history survives restart (E31).

## 11. Never-ask and declines

Every session on a VIA-owned server is never-ask, whoever owns it. Session
rules (§5) allow everything except `question` and the denied tools. Any
interactive request on the server is declined within
`min(remaining operation budget, 5 s)` from decode (C2 A6) on the decline
pool; never `once` or `always`.

| Request | Decline | Settlement |
|---|---|---|
| `permission.asked{id, sessionID, action, resources, source:{messageID, id:callID}}` | `POST /api/session/{sid}/permission/{rid}/reply {"decision":"reject"}` → 204; 404 means gone, not declined | `permission.replied{reply:"reject"}`; the tool fails and the vendor ends the execution `interrupted{shutdown}` (E27): §7.3's decline row applies only when correlated by `callID` |
| `form.created{form:{id, sessionID, title, …}}` | `DELETE /api/session/{sid}/form/{fid}?message=declined%20by%20VIA` → 204 (E44); 409 means already settled | `form.cancelled` settles the request only; its effect on the execution is unobserved (G20), so the terminal follows §7.3's ordinary rows |

**Attribution:** through the child map's originating turn, else
`source.messageID`, else `source.id` (callID), live or tombstoned; for a
form, the turn owning its session's execution. An unattributed request is
still declined and recorded only as a diagnostic, never credited to the
current turn. A decline not settled within 5 s fails closed:

- attributed to a turn that is still running: that turn is stopped and
  fails `protocol` (C2 rule 8);
- attributed to a turn that has already settled (a tombstone): the
  settled turn's result never changes, and VIA sends **no** interrupt for
  it, since a session-scoped interrupt would stop the successor (§7.2).
  The failure is recorded on the settled turn as a late
  `vendor.request_declined` observation with `summary` saying the decline
  did not settle (`late:true`, up to the close cutoff, C2 §3; after it, a
  counted diagnostic). The decline's effect is unknown and a request left
  pending could wedge the session's execution, so the server generation
  **drains** (§8): the successor and other sent turns finish or meet their
  own deadlines, and retirement then ends the pending request. Drain is
  the existing path for an unknown request effect, keeps unrelated
  sessions' turns, and needs no new state;
- unattributed: the server generation fails `protocol` (C2 rule 8).

**Observations.** `vendor.request_declined {vendor_method:
"permission.asked:<action>" | "form.created", summary, blocking:true}`
(summary bounded, never tool input or values); one decline per vendor
request ID. The declined `callID` suppresses the matching `action.denied`;
`tools_ended` is still emitted. Any other `tool.failed` with
`error.type:"permission.rejected"` (a rule denial, E55) yields
`action.denied` with `kind` from the action (`bash`/`shell` → `command`;
`edit`/`write`/`patch` → `file_write`; `webfetch`/`websearch`/`browser` →
`network`; else `other`).

## 12. Usage, errors, capabilities and version

**Usage.** Per model call inside the owned execution: `tokens` of
`session.step.ended|failed` (keyed by assistant message ID; a repeat
supersedes) and of `session.compaction.ended|failed` (keyed by the
compaction's `inputID`, else its event ID; source E55), summed, provenance
`reported` (E22). Scope `turn`; a turn with any compaction sample reports
tokens and cost as `vendor_interval` with `usage_interval_unverified` until
live compaction is qualified (G21). `input` excludes cache reads;
`cache.read` → cached input, `cache.write` → `vendor`.
`session.usage.updated` is cumulative and counts hidden title calls (E22):
never used. Child sessions are excluded. Cost: the summed `cost`, same
scope, `reported`; never priced from tables. Missing values are
unavailable, not zero.

**Class hints** (`error.type`, `status`; E34):

| Vendor error | Class |
|---|---|
| `provider.auth`, or `status` 401/403 | `auth` (observed 403, E29) |
| `provider.rate-limit` or `status` 429 | `rate_limit` (source) |
| `provider.quota` | `budget_exceeded` (source) |
| `provider.no-route`, other `provider.*`, unknown | `vendor_error` (no-route observed, E33) |

A bad model is accepted at creation and fails after acceptance (E33).
`session.retry.scheduled` is activity, never authority to resubmit.

**Version (owner, 2026-10-06; an exception to C2 §5's OD1 rule).**
`/api/info.version` is reported, and OpenCode runs **only** versions in
`checked = {"2.0.22"}`: versions that passed every gate, L13 included. Any
other version is refused before the server starts: the anchor runs the
opened file's own `--version` before ARM and launches that same file only
if it is checked (§2.2: `handshake_refused`, cached by binary identity, a
message naming the version and the checked set). The handshake's
`/api/info.version` must then match. Claude, Codex and Pi run untested versions with a warning; OpenCode
cannot, because the one-server fence's safety depends on the server keeping
its inherited lock descriptor (§3.2), which is qualified per version (L13),
not visible in a handshake. `allow_untested` does not waive it. `describe`
starts no process and reports the last version seen, `refused` when it is
outside `checked`.

**Declared capability** after its tests pass:

| Surface | Declaration |
|---|---|
| Spawn / resume | native: vendor session create, persistent history, reopen by exact ID (E16, E31) |
| Steer | unsupported in the first release (§6) |
| Cancel | native: inbox cancel before delivery, interrupt after (§7.4) |
| Close | native detach; vendor history kept |
| Recover | unsupported (P12) |
| Instructions | native session instruction entry, at most 262,144 encoded bytes |
| Effort | native only for a variant of the model in the session location's catalog, checked in `run_turn` (§5) |
| Version | only versions in `checked` run; any other is refused (`handshake_refused`) before publication |
| Output schema | unsupported |
| Max steps | `params.max_steps:{support:"unsupported",reason:"No per-turn step limit on opencode-serve 2.0.22"}`; non-null refused by Core preflight before server acquisition or vendor I/O |
| Bound | `full` with `network:true` only; nonempty `extra_write_dirs` → `invalid_params` |
| Prompt size | encoded prompt plus cwd at most 1 MiB − 8 KiB |
| Usage / cost | scope `turn`, or `vendor_interval` with a compaction sample until G21; `reported` |
| Vendor options | allow-list empty; reserved keys C2 §6.1 |
| Vendor arguments | refused in the first release: any non-empty `vendor_args` is `invalid_params` (§2.2) |
| Inherited configuration | project configuration always on; every category `unknown` with `config_switch_unverified`, except skills requested `off` (`off`); §4.5 |

## 13. Acceptance fixtures and live qualification

Failure-first fixtures run real VIA/Core/Host/Wire/Store against a fake
HTTP/SSE vendor replaying sanitized 2.0.22 transcripts; live sets use the
pinned binary and a free model.

| Fixture | Required observation |
|---|---|
| OC01 handshake | URL line not JSON, wrong shape, non-loopback or oversized; `/api/info` HTML, non-JSON, missing fields → incompatible, cached; version admission through real Host anchors and fake vendor binaries: a binary whose server would be fully compatible but whose `--version` says `2.0.23` → refused at configuration, no ARM intent, no server process ever started, the lock never taken, the namespace directory unchanged (no file created or modified), `submit_failed`/`handshake_refused` naming `2.0.23` and the checked set, cached; the same path replaced by a `2.0.22` binary → admitted; the path replaced after the route's stat and before configuration → `ProgramChanged`, nothing launched, a later acquisition re-stats; the path replaced (and, separately, unlinked) between an admitted probe and ARM → the server that runs is the probed file (its `/api/info` and the anchor's exec target identity say so); probe exit non-zero, oversized or over 2 s → refused, the probe process gone; a daemon crash between the probe and ARM → no server started; `/api/info.version` different from the probe → `handshake_refused`; `allow_untested:true` does not waive any of it; URL line missing or exit before it → transient; `pid` mismatch, timeout, empty catalog, 5xx → transient, not cached; binary replaced at the same path clears the cache; wrong password → 401; no proxy or redirect |
| OC02 namespace and credentials | Sessions with different inherited-configuration requests share one process and slot; the default request → every category `unknown` and one `config_switch_unverified` listing all six; instruction files or skills requested on → `unknown`, never `on`; instruction files requested off → same server, `unknown`; skills off → `off` by session rule and absent from the warning; the launch environment never has `OPENCODE_DISABLE_PROJECT_CONFIG`; private roots only; fresh namespace proceeds; known integration shape with a synthetic credential → `unexpected_credential_state`; known shape, none → proceeds; unknown shape → proceeds with `credential_state_unchecked`; Boolean scan: the synthetic value never appears in any byte VIA read; `GET /api/credential` never requested |
| OC03 full path | Live spawn/result, background/wait, events/logs on a free model; close never deletes vendor history |
| OC04 continuity and settings | Two-turn context answer; idle retirement then reopen with one identity read; missing or mismatching ID → `resume_mismatch`, no create; reopen settings mismatch → `SettingsMismatch` → `submit_failed`/`settings_mismatch`, not cached; just-sent readback differing → `handshake_refused`, cached under the session refusal digest: after a skills-off session's permission readback is refused, a default session with otherwise equal inputs and the same binary is admitted and runs, and a second skills-off session is refused from the cache; the server stays published throughout; variant readback: a non-default variant read back as sent; null clears it to `"default"`; a fake vendor that ignores a switch (readback still the previous variant) → `handshake_refused` for that turn, nothing prompted |
| OC05 execution rule | Interrupt still in flight when the predecessor ends naturally blocks the successor (replays E51); predecessor terminal not yet on the stream blocks it; 400/404 and never-sent predecessors release it; a terminal before own delivery is not `protocol`; a vendor-originated execution racing dispatch is not the turn's; a driver reopened on the same generation (idle close, overflow) dispatches from the server-scoped state; leftover VIA input at reopen is cancelled once and revises nothing; `session_busy` after 30 s |
| OC06 routing | Interleaved sessions on one stream; `form.created` routed by `form.sessionID`; child requests credited to the originating turn; late events by input, assistant and tool keys to tombstones, never the successor; unknown types, fields and `session.execution.*` suffixes; malformed known payload → driver `protocol` and drain; non-JSON data; non-increasing seq |
| OC07 never-ask and isolation | Create/readback rules; other-session rename/move and self-move unavailable (live); declines within 5 s with the general pool full and observations saturated; a late request naming a settled turn's IDs credited to it; with T1 settled and T2 running, a late permission request naming T1's tool ID whose decline does not settle in 5 s: T1's envelope unchanged, a late `vendor.request_declined` on T1, no interrupt sent, the generation drains, T2 finishes or meets its own deadline, then the server retires; unattributed declined and credited to none; a child (task) session gets the same deny rules (other-session rename/move, self-move, `question`, `skill` when off) and its permission requests are declined (live); only a callID-correlated permission decline maps `interrupted{shutdown}` to `Completed/Other`; form decline leaves the ordinary rows; `vendor.request_declined` fields; `tools_ended` kept; `permission.rejected` → `action.denied`, deduplicated |
| OC08 control | Cancel before send withdraws; before delivery → inbox cancel → input-cancellation terminal → `cancelled`/`acknowledged`, quiescent; same after wall cleanup keeps `failed(deadline_wall)`; lost cancel race → interrupt; interrupt during a tool → `interrupted{user}`, quiescent; no acknowledgement by `force_at` → `unknown`, no kill, later terminal or input cancellation revises an accepted turn |
| OC09 outcomes, drain and overload | Prompt status table; timeout and socket failure after a byte → drain; inconclusive prompt → turn by lane order (terminal, acceptance, else `unknown`); drain publishes readiness; a pinned unsent turn → `SessionGone`; sent turns finish; retirement through Host; next generation fresh. Lane overflow → `failed(overflow)` as Codex, terminal-overflow case, no drain, successor dispatches; oversize event, full staging and each §9 count → generation `overflow`, including one long turn that learns 65,537 assistant and tool IDs, and the 8 MiB ID-byte bound reached by live and tombstoned turns together; an ID over 1 KiB → `protocol`; prompt admission at both sides of the limit with escaping-heavy text; 45 s silence |
| OC02b data-root lock | Through real Host anchors and a fake vendor that reports its open descriptors: `server.lock` is created 0600 and never unlinked; the anchor holds it from configuration, the vendor holds the inherited descriptor, the vendor's own children do not. A held lock refuses the configuration: no ARM intent commits, no vendor starts, the turn is `submit_failed`/`launch_failed` with the lock step, nothing cached, and a later acquisition succeeds once the holder exits. Each path, refused while a holder lives and admitted after it exits: (a) a post-ARM failed acquisition (handshake failure) whose anchor is then killed with the vendor kept alive; (b) a pre-ARM failure (configuration accepted, ARM never sent), admitted as soon as the anchor exits; (c) a launch or retirement task failure; (d) a real daemon crash after ARM, then restart: the orphan held by a test barrier refuses a new server, and admits one after its anchor's EOF cleanup; (e) an anchor killed from outside with the vendor alive. The anchor never unlocks: after the fake vendor closes its copy, the lock stays held until the anchor exits. The startup window: a fake binary whose `--version` is unchecked and whose server would close its inherited descriptor never reaches ARM, so its anchor and server never hold or drop the lock. (f) Unsafe boundary: a fake vendor that closes its inherited descriptor and keeps serving, then its anchor killed from outside: the lock is free and a second server's configuration succeeds while the first still serves, which is why L13 gates enablement. (g) The same with a fake vendor that closes the descriptor and reopens the file under the same descriptor number: its `/proc/<pid>/fd` target is unchanged and a fresh-open `LOCK_NB` still fails while the anchor lives, yet the L13 method below rejects it (no lock line in its `fdinfo`; after the anchor-only kill the fresh-open `LOCK_NB` succeeds). Store rows never gate a launch through the lock: with spare `harness_processes` capacity, a Store holding an identity-less pre-ARM server intent, a server anchor with another boot ID, and unproven Codex server anchors from earlier runs admits an OpenCode server at once when no lock holder lives; a live Codex server never blocks it. Such rows exhausting recovered capacity is the separate limitation of §3.2 (bead via-joc), not tested here |
| OC10 recovery | Server death during N sessions' turns → `server_lost` with one shared leftover report; a terminal admitted before the loss kept; EOF with process alive → `unknown`; quiescent with `GroupAbsent` even when tool leftovers are listed; daemon crash → `unknown`, no resend; a daemon crash leaving an orphan server, then restart: no second server while the orphan lives (OC02b d); password rotation |
| OC11 parameters and usage | Variant check and switch: two locations on one server whose project configurations give the same model different variants: on every turn, including later turns of each session, each session's effort is accepted or refused in `run_turn` by a fresh fetch of its own location's catalog, before any prompt, and `check_turn` accepts both (no location-dependent preflight); a location whose variant appears only after its first catalog response: the first turn is refused `submit_failed` naming `effort`, a retried turn is accepted; non-default readback, clearing to default and an ignored switch as OC04; instruction size at 262,144/262,145 encoded bytes; output schema and max steps refusals; step-keyed usage excluding `usage.updated`; a compaction sample → `vendor_interval` plus `usage_interval_unverified`; class hints |
| OC12b provider secrets | Synthetic project configurations at two locations set `providers.opencode.settings.apiKey`, a provider header, a model and a variant header and `body`, and a custom endpoint URL with a token; the fake vendor echoes them in `/api/model`, the session model readback, an error event's message and its stderr. Boolean scan: none of the synthetic values appears in any envelope, event, status, `models` or `describe` result, log, diagnostic, Store row or evidence file except the vendor's own stderr capture (whose live behaviour is L4's gate); a `/api/model` body that fails to decode, an HTTP body over its cap, a body truncated before its declared length, an SSE event over 1 MiB and an SSE stream ending mid-event, each carrying the synthetic values in their first bytes: no `undecoded.bin` or other payload copy anywhere, only endpoint or event type, status, length and failure kind; `GET /api/config`, `/api/provider*`, `/api/mcp`, `/api/plugin` and `/api/credential` never requested |
| OC12 password | Password only in the launch environment; absent from tools (Boolean), argv, Store, keys, diagnostics and captures; synthetic secrets only |

**Live qualification items.** Each is a named test in `via-4sw.3.4` (or
later where noted), not a mechanism; until it passes, the behaviour above
stands. Items that gate route enablement (L4, L5, L13) say so.

| Item | Risk | Test |
|---|---|---|
| L1 409 trigger | The doc lists prompt 409 (`PromptConflictError`, `InboxConflictError`, `BusyError` in source); no trigger was found | Drive concurrent prompts, reused IDs and busy sessions; record which yields 409 and confirm drain is the right response |
| L2 mid-write cancel | A stop while a prompt body is being written or its response is pending | Cancel during a large prompt write, and again while the prompt response is still pending; confirm the stop is served at once on its reserved pool, independent of the prompt, that an indeterminate write drains, and that there is no enqueue or a single enqueue |
| L3 foreign inbox writers | A non-VIA item in a session's inbox (vendor `synthetic`, `compaction`, `move`, or another writer) delays or joins a turn | Provoke vendor-originated items and an external writer, including foreign input arriving between VIA's execution-state check and its prompt's delivery; confirm delivery-ownership attribution (§7.1) and that the execution rule holds |
| L4 (G18) hostile project config | Project config overriding `share`, permissions, MCP or plugins despite the generated config; project provider configuration with keys, headers and endpoints | Synthetic project fixtures per key; gates route enablement. With the pinned binary, OC12b's secrecy matrix live: project provider `settings.apiKey` and headers, model and variant headers and `body` values, an endpoint URL carrying a token, and the error paths (a provider error, a decode failure, an oversized and a truncated response): the session uses that provider (it applies), and a Boolean scan finds none of the synthetic values in any VIA output, Store row, log or evidence file, the server's stderr capture included; any occurrence fails qualification until resolved |
| L5 (G19) MCP, plugins, hooks | MCP tool permission names; plugin and hook loading | Project MCP server and plugin fixtures; record the MCP tool permission names and exact OD2 states. Denying MCP tools is a qualification probe only: production adds no MCP tool denial (MCP requested `off` stays `unknown`, §4.5) beyond §5's resource helpers; gates enablement |
| L6 (G20) model-driven forms | A form raised by the model; the execution after `DELETE` | Trigger a form live; record the terminal after decline |
| L7 (G21) live compaction | Compaction events and their usage | Force automatic compaction; verify keys and totals, then allow scope `turn` |
| L8 rate-limit, quota, context shapes | Error shapes unobserved | Trigger each on a free or synthetic provider; confirm class hints |
| L9 long-run memory | Retained state over hours with many sessions; the one server carries every OpenCode session, so §9's per-server counts are daemon-wide | N sessions on one server for a long run, with escaping-heavy and large event fields; measure retained memory and RSS against §9's count bounds |
| L10 macOS | Platform deferred | Run OC01–OC12 on macOS under the platform contract |
| L11 new-version credential shape | A new version's `/api/integration` may change shape or carry values | Before a version joins `checked`, rerun E41's synthetic-credential probe with every other gate (L4, L5, L13) |
| L12 other unobserved | `superseded` and `inactivity` interrupt reasons; agent `steps`; durable replay; `OPENCODE_DISABLE_AUTOUPDATE` behaviour; truncated-body handling (E56) | Probe each at the next pin review |
| L13 (gate) lock retention | The data-root lock (§3.2) is sound only while the server keeps its share of the **locked open file description**, never unlocking it; a descriptor number or `/proc/<pid>/fd` target cannot show that (a close and reopen keeps both, OC02b g), and the anchor's copy masks a fresh-open check while it lives; E57 covered fd 3 from a probe script and the location and session shells only | **Gates route enablement and every pin review.** Each sample is one controlled server generation started through the real Host launch (VIA's anchor), driven to a point in its life (just after publication, during and after each spawn path: the model's tool runner, location and session shells, a project MCP stdio server, LSP and a plugin spawn, and after a long run). The direct test: (1) a same-uid scan of `/proc/*/fdinfo` finds the flock line for `server.lock` only on the anchor's and the server's descriptors (another holder, such as a child, would mask the result: the sample is set aside and the holder reported as an availability finding); (2) `SIGKILL` the anchor's pid alone, never its group and never `SIGTERM` (which starts the anchor's own group stop), with a test barrier (a `test-failpoints` build of VIA, the pinned real OpenCode) holding Host and the route from retiring the generation or closing its stdin; (3) confirm the server is the same process (pid and start ticks) and still answers `/api/info`; (4) a `LOCK_NB` attempt from a fresh open must fail; (5) step 3 again. A sample where step 3 or 5 fails, because the server exited or was signalled, is invalid and rerun, never counted. Then stdin close stops the server; confirm it is gone and the lock is free. Supporting evidence: on Linux the server's `/proc/<pid>/fdinfo/<fd>` shows a `lock:` `FLOCK … WRITE` line only while that descriptor belongs to the locked description (checked locally on Linux 6.6: present on the locking descriptor and its duplicates, absent after a close and reopen under the same number). **Pass:** every valid sample's step 4 fails to lock and every `fdinfo` check shows the line. **Fail:** any valid sample in which the fresh open locks, or any loss of the `fdinfo` line or unlock during the server's life; OpenCode qualification then fails and stays failed until that is resolved. A child found holding the descriptor is not a safety failure (it only blocks launches after the server exits) but is reported as an availability finding with its spawn path |

## 14. Owner questions and revisit items

Decided (owner, 2026-10-06): transient receipt of project provider
credentials is a narrow, owner-approved exception to invariant 1. OpenCode's
`/api/model` and session model replies may carry a project's provider
credentials; VIA receives them transiently, decodes only the allow-listed
model fields (§4.3) and never logs, stores, returns or keeps them.

Decided (coordinator, 2026-10-06): OpenCode vendor error text is dropped;
envelopes and evidence keep only the vendor's error type and the status
(§4.3, §7.3, §8).

Decided (coordinator, 2026-10-06, critical review round 2): version
admission probes and launches the same opened file before ARM (§2.2); an
OpenCode connection captures no payload bytes (§4.3, runtime §4); effort
is checked in `run_turn` against a fresh location catalog, with no
location-dependent `check_turn` preflight (§5).

Decided (owner, 2026-10-06, bead via-4sw.3): one OpenCode server for all of
VIA, one private namespace with project configuration always on; a request
to turn off a category only the project switch controls is `unknown` with
`config_switch_unverified`, never a second server (§3, §4.5); private VIA
data and the free anonymous provider profile; the one-live-server fence
stays across daemon restarts (§3.2); `vendor_args` refused (§2.2). This
supersedes **Q2** below and decision C-1 of the C2 gap report.

Decided (owner, 2026-10-06, critical review Blocker 1): OpenCode runs only
versions in `checked`, those that passed every gate including L13; any
other version is refused (`handshake_refused`; since critical review
round 2, before the server starts, §2.2), even with a compatible handshake. This differs from
C2 §5's warn-and-run rule for the other harnesses because the fence's
safety depends on per-version lock retention (§3.2, §12).

Decided (owner, 2026-10-06, critical review Blocker 2): project provider
configuration applies; it is the user's own project configuration
("disable nothing"), and the free anonymous route is only the default
where a project sets no provider. VIA never logs, stores, returns or puts
in evidence any provider key, header or endpoint secret (§4.3).

Decided (owner, 2026-10-06, review finding 5): `on` keeps C2 §6.2's
meaning for every harness. Because only the project-level part of the
user's configuration applies (user-level sources are private and empty),
instruction files and skills requested `on` are `unknown` with
`config_switch_unverified`; requested `off` is unchanged (instruction
files `unknown`, skills `off` by the deny rule) (§4.5).

Decided (owner, 2026-10-05): **Q2** (superseded 2026-10-06), the project
switch follows instruction files; **Q10**, an
unknown request effect drains and restarts the server generation, stopping
any unowned execution after unrelated turns finish (§8); **Q11**, relaxed
credential check (§4.3).
Earlier questions were accepted as proposed (history file).

**Revisit after the release** (owner, 2026-10-06; not first-release work):

| Item | Revisit |
|---|---|
| R1 user configuration | Use the user's own OpenCode configuration and providers instead of VIA's private, empty user-level sources and the free anonymous profile |
| R2 user service | Attach to the user's own OpenCode service; invariant 11 (own vendor servers) would need the owner first |
| R3 passthrough | OpenCode `vendor_args` (§2.2); the design (for example a server and data root per argument list) is a future architecture choice that needs an owner decision |

## 15. Contract amendments (record)

The amendments this packet proposed to C1 (`via-api-v1.md`), C2
(`adapter-contract.md`) and the runtime contracts (`runtime-contracts.md`)
were integrated into those contracts on 2026-10-05; the contracts are now
authoritative, and the full amendment text is kept in
`scratchpad/execution/opencode2/history.md`.

After the critical review (2026-10-06): C1 P13 (both rows),
`version_status` and `allow_untested`; C2 §5's version rule and effort
validation.

Critical review round 2 (2026-10-06): runtime §4 (`Capture::Off`), §5
(`PinnedProgram`, version probe, ARM text) and §8's payload row; C2 §5's
OpenCode version and effort exceptions; C1 P13 (both rows) and §9; the
invariant 1 exception in `.repo-context/invariants.md`.

Rev7 (2026-10-06) made these edits in place: C1 P11 and its §Decisions
row, and the §9 password note (one server for all of VIA; reach of a
recovered password); C2 A8, the §6.2 process-shape and
inherited-configuration cells, and the §6.3 OpenCode row; runtime §5
(the exclusive launch lock, its `PrivateProcessSpec` field and
`Configure`), §8's OpenCode server row and ownership note (no OpenCode
`owner_server` label).
