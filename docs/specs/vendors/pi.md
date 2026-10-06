# Pi RPC adapter contract

Status: **design candidate rev2, 2026-10-05, via-jt8.2** (an owner-approved
cut-down that also resolves the first design review: no mechanisms for
unobserved problems; each hypothetical risk is a named live qualification
item, §9). Design plus evidence only, not implementation or
release-conformance evidence. Route: one private `pi --mode rpc` process per
VIA turn, on the private per-turn lifecycle of the
[Claude Code packet](claude-code.md), which this packet references instead of
restating. Its shared-contract amendments were integrated on 2026-10-05
(§10); this document does not override C1, C2 or the runtime contracts.
Revision history and removed material are in
`scratchpad/execution/pi/history.md`.

Authority: [C1](../via-api-v1.md), [C2](../adapter-contract.md),
[runtime contracts](../runtime-contracts.md),
[goal](../../workstreams/rust-foundation/goal.md) and
[invariants](../../../.repo-context/invariants.md) (12: Pi joins the first
release, owner 2026-10-03; 13: thin wrapper). Research input:
[Pi research](../../brainstorms/research/harnesses/pi.md) (pre-1.0;
re-verified here).

Labels: **observed** (probe), **source** (installed Pi `dist/`, or this
repository's code where named), **docs** (installed Pi `docs/`), **live**
(observed on the live model), **proposal**. `E<n>` cites
`scratchpad/execution/pi/evidence.md`, a private, ephemeral evidence packet
with raw transcripts under `probe/`. Never commit those transcripts, session
IDs or local paths.

## 1. Evidence and decisions

Inspected: `@earendil-works/pi-coding-agent`, a Node script run by Node
v22.22.0, WSL2 Linux x86-64. E00–E48 ran on **1.0.0** (2026-10-03). The
installed package now reports **1.0.2**, and its `package.json` predates the
2026-10-04 live runs and probes (E49–E64), so those probably ran on 1.0.2;
the evidence did not record it (L11). Most behaviour was established
against a local scripted fake OpenAI-compatible endpoint in a scratch agent
directory: framing, events, tools, abort, sessions, signals, trust and
context loading.

**Live-model rule (owner, 2026-10-04).** All Pi live testing uses
`openai/gpt-6-luna` through the OpenAI login in VIA's private Pi agent
directory, with `--thinking low`, and no other model. The live runs (seven
model calls, E49–E55) confirmed a real turn, continuity across two
processes, per-call usage and estimated cost, the real 401 error shape, and
abort during a real tool. This revision made no live calls and ran no probes.

| Decision | Resolution | Evidence / gate |
|---|---|---|
| PI-1 route | `pi-rpc`: one private `pi --mode rpc` per VIA turn, `ConnectionKind::PerTurn` (§2) | E01, E11, E14, E22; d1: defensible |
| PI-2 version | `version` from the Pi package's `package.json`, read before launch; no `--version` process (§3) | E45, source `config.js`; coordinator |
| PI-3 identity | Derived expected ID; `--session-id` until identity is confirmed at acceptance, then strict `--session` (§2.2) | E03, E11, E12, E29, E67 |
| PI-4 profile | Private agent directory, logged in once by the owner; validated by an allow-list before every launch (§4.2–4.3) | E37, E49, E58–E61, E64 |
| PI-5 trust | `--no-approve` always | E33 |
| PI-6 effort | `--thinking`; equality check only when effort is requested | E41, E58 |
| PI-7 inventory | Instruction paths from the system patch's `project_context`, per turn, evidence only (§4.7) | E05, E36, E51, E62 |
| PI-8 steer | Unsupported (owner deferral, as Codex) | owner, 2026-10-04 |
| PI-9 interrupt | `Acknowledged` needs the paired abort reply **and** a current-run cancellation marker (§7) | E15–E18, E54 |
| PI-10 output schema | Unsupported | E42 |
| PI-11 bound | `full` with `network:true` only | E35 |
| PI-12 usage | Per-call samples; missing usage is `null`, never 0; cost estimated or unavailable (§5.5) | E24–E26, E52, E63 |
| PI-13 class hints | HTTP status prefix only | E27, E53 |
| PI-14 failure text | Never copy raw vendor text; parsed safe tokens only (§5.4) | E53 |
| PI-15 sizes | Prompt and instructions limits that keep Pi's echo records under the 1 MiB message ceiling (§4.5) | E47, E57 |
| R1 | Refuse the launch while an earlier Pi launch of the session is not proven gone (§7.4) | E13, E66 |
| OD-PI-1 | **A (owner, 2026-10-05):** `failure.data.reason:"uncertain_predecessor"` | C1 §5, C2 `StartRejected` |

## 2. Ownership, route and turn lifecycle

Ownership is as Claude's (Claude §2), with Route owning typed JSONL
parsing, command correlation and the control lane.

**Why one process per turn.** It reuses the reviewed private lifecycle
(Host anchor, own-group cleanup, turn-envelope leftovers, decode fence,
`recover` unsupported). No in-memory vendor state crosses turns: Pi keeps
steering and follow-up queues in memory only (E11), and an idle queue entry
leaks into the next run (E22). Model, effort, tools and instructions are
launch flags, re-applied every turn as in Claude's recipe. RPC runs one
session per process (`new_session` aborts the running run, E14). Startup
costs ~160–240 ms (E01).

| Rejected | Why |
|---|---|
| Persistent `pi --mode rpc` per session | Needs the server machinery and loses the leftover destination; nothing in C1 needs it. Revisit if per-turn startup cost is measured to matter |
| `--mode json` per turn | No abort except signals; exit 0 on failed turns (E09, E31) |
| `--print` | Final text only |
| SDK, `pi-acp` | A Node host breaks the single static binary; the bridge loses usage |

The RPC protocol name does not select process lifetime: on `pi-rpc` it is a
private per-turn process (glossary fix, §10 G7).

| C1 surface | Mapping |
|---|---|
| `describe` | Pure plan; no process or file write; the last version read for this program path, else `null`/`untested` |
| `models` | Bundled catalog, provider-qualified with no bare-ID alias (a bare ID such as `gpt-6-luna` is Codex's, and model-only routing must stay unique); the live list is the handshake's `get_available_models` |
| `spawn`, `resume` | Core receipt, then one launch per turn (§2.1) |
| `steer` | `unsupported_verb` naming `pi-rpc`; nothing written (PI-8) |
| `cancel` | Queued: Core-local. Running: §7 |
| `close` | As Claude: graceful drains permitted work; force requests anchor-owned cleanup |
| `status`, `wait`, `result`, `list`, `events`, `logs` | Core/Store; no vendor readback or transcript scraping |

### 2.1 One turn

`open_session` is logical and performs no vendor I/O. Each `run_turn`,
after Core reserved a slot:

1. **Pre-launch checks**, with no vendor process:
   1. the profile policy (§4.3);
   2. R1 predecessor absence (§7.4);
   3. the version read (§3).
2. **Launch** the §4.1 recipe. Nothing is written before step 3.
3. **Handshake.** Write `get_state`, `get_available_models` and
   `get_commands` at once, each with a VIA `id` (Pi buffers them, E01).
   Check, before any prompt:
   - `sessionId` equals the expected ID, else `Err(ResumeMismatch)`
     (`failed(resume_mismatch)`);
   - `model.provider`/`model.id` equal the resolved model and appear in
     `get_available_models`, else `Rejected{InvalidParam{model}}`. Pi
     fuzzy-matches model names and runs an unknown ID under a known provider
     with only a stderr warning (E30);
   - when effort was requested, `thinkingLevel` equals the mapped level,
     else `Rejected{InvalidParam{effort}}` (PI-6);
   - every `get_commands` entry is `skill:*`; anything else means `-ne` or
     `-np` did not hold, an instance refusal (§3).
4. **Submit** one `{"id":"<v>","type":"prompt","message":PROMPT}`.
   - `success:true, data.disposition:"started"` is acceptance. The driver
     emits `session.vendor_identity_confirmed` (§2.2), then `turn.accepted`
     with `vendor_turn_id` = the VIA request ID (Pi has no turn ID; the
     process is the turn).
   - `success:false` is a definite rejection, for example missing
     credentials, which leaves no session file (E29):
     `Rejected{VendorError}` with no vendor code and VIA-owned text (§5.4).
   - Any other disposition (`handled`, `queued`) is protocol: it cannot occur
     on an idle fresh process under `-ne -np` (E40).
   - A lost reply is the unknown-submission failure: Pi exiting after the
     written prompt with no reply fails the turn `process_exit` with the
     Host-confirmed exit. Nothing is resent.
5. **Observe** (§5) until `agent_settled`. `agent_end` is not the end:
   auto-retry and compaction continue after it (E28).
6. **Close.** After `agent_settled`, and after any admitted abort's reply or
   cutoff (§7), close stdin; Pi exits 0 in ~7 ms (E31). Then S1's close as
   Claude: wait for exit, group cleanup, leftover scan, `TurnEnd`. **Never
   close stdin before `agent_settled`**: EOF aborts the run with no terminal
   and exit 0 (E31). Unlike Claude, EOF here is an unacknowledged stop.

The exit status is never evidence of success (E09, E31). **Startup
failures.** Pi's startup diagnostics go only to stderr (E12,
E30), which runtime §4 gives to `stderr.log` unread. Any exit before every
handshake reply arrived (a bad model, an unusable directory, a strict
continuation with no history, E12) is `Rejected{Protocol}` with the
VIA-owned diagnostic "pi exited before its handshake replies" and the
Host-confirmed exit report: `failed(submit_failed)`, as `SessionGone` and
`Protocol` rejections map today. The vendor's own words stay in
`stderr.log`. An unusable `--session-dir` once made Pi spin at 100 % CPU
until SIGKILL (E30); the turn's deadlines and Host's TERM→KILL bound it, and
the adapter creates the session directory before launch.

### 2.2 Identity and continuation (PI-3)

- **Expected ID, derived.** SHA-256 over `"via pi session " + session_id`,
  laid out as a version-4 UUID, as Claude derives its UUID (E67). Pi accepts
  it (E12, E50). It is a pure function of the durable VIA session ID: no
  storage, and it survives eviction and restart. Changing the derivation
  needs an incompatible `adapter_version`.
- **Per-session directory.** Each VIA session gets its own `--session-dir`
  (§4.4), so Pi's partial-ID matching (E12) cannot select another session.
  Close and turn end never delete the vendor session file.
- **Create or continue: Claude's rule.** Launch with `--session-id ID` while
  `SessionSpec.confirmed_vendor_session_id` is absent, and with strict
  `--session ID` once it is present (E67: Claude chooses `--resume` the same
  way).
- **Pi confirms identity at acceptance, not at the handshake.**
  `get_state` echoes the ID before any prompt (E03), but Pi writes the
  session file only at the first user message (E11), so the handshake proves
  the ID, not history. The confirmation is therefore emitted with the
  `started` reply, carrying the transcript hint (§5.4).
  - A definite rejection (E29: no file) never confirms, so the next turn
    still creates.
  - A lost `started` reply never confirms. The next turn's `--session-id`
    opens the file if Pi persisted the prompt (E12: an existing ID opens),
    or creates one if no history exists.
  - Once confirmed, `--session` never creates: a missing file exits before
    RPC (E12), which is the §2.1 startup rejection. A missing path, by
    contrast, would silently create a session (E12), so VIA never resumes by
    path.

## 3. Capabilities, version and handshake checks

| Surface | Declaration | Basis |
|---|---|---|
| spawn, resume, close | native | E04, E12; live continuity E51 |
| steer | unsupported: "steer is deferred past the first release" | Owner deferral (as Codex). Core refuses by name; `require:["steer…"]` fails `missing_capability`; no `steer` or `follow_up` command is written. E19–E22 and E55 stay historical evidence |
| cancel | native | Reply plus marker (§7); live E54 |
| instructions | native | `--append-system-prompt <file>` (E38) |
| output_schema | unsupported | No schema input (E42) |
| effort | native | `--thinking`; effective level echoed (E41) |
| max_steps | unsupported | No option (E43); non-null refused before I/O |
| bounds | `["full"]`, `network:true` only | E35 (§6) |
| network_control | false | — |
| recover | unsupported | Owned stdio cannot be rejoined |
| usage | tokens `turn`; cost `turn`, `estimated` | E24–E26, E52 |

**Version (C2 §5; owner OD1).** Every Pi version is supported; versions
outside the adapter's `checked` set are `untested` with
`vendor_version_untested`. Pi's RPC carries no version (E45), and
`pi --version` prints `version` from the Pi package's `package.json`
(source `config.js`: `VERSION = pkg.version`). VIA reads the same file
before each launch, so no extra process runs:
- From the resolved entry script's directory (symlinks resolved), walk up
  at most 8 levels to the first directory holding `package.json`; if that
  directory is named `dist` and its parent holds one, use the parent's (Pi's
  `findNodePackageDir`).
- Bounded parse: a regular file of at most 64 KiB, one JSON object, and its
  `version` member a string of at most 64 bytes. Anything else, or no file
  (a Bun-compiled Pi), gives `vendor_version: null` with `untested`. It is
  never a refusal: VIA relies on nothing the version says.
- The string goes into this turn's `InstanceReport`, on every outcome after
  the read, including an exit before the handshake (E12); this is the
  metadata trigger of C2 §5. A package replaced between the read and the
  launch is the accepted race, as for every route.

**Instance refusals** (C2 §5 cache, keyed by program file identity and
recipe digest): a `get_commands` entry other than `skill:*` (§2.1). All
checks precede the prompt, so the turn fails `submit_failed` with
`data.reason:"handshake_refused"`. A model, effort or ID mismatch is a
per-turn rejection, not an instance refusal. A malformed handshake reply is
protocol (§5.1).

## 4. Launch, profile and canonical parameters

### 4.1 Recipe

Argv array, explicit cwd (the session's frozen cwd), never a shell string:

```text
pi --mode rpc
   --model PROVIDER/MODEL_ID                 # exact, provider-qualified
   [--thinking LEVEL]                        # only when effort is set
   --session-dir <vendor_state_dir>/pi/sessions/<via_session_id>
   --session-id ID | --session ID            # §2.2
   --tools read,bash,edit,write              # explicit; never inherit defaultTools
   --no-approve -ne -np                      # no project trust, extensions or prompt templates
   [-ns] [-nc]                               # when inherit.skills / inherit.instruction_files is off
   [--append-system-prompt <vendor_state_dir>/pi/instructions/<via_session_id>]
   --offline
```

**Environment.** As Claude (B7): start empty and pass only `HOME`, `PATH`,
`LANG` and Host's `VIA_PROCESS_MARKER`, plus
`PI_CODING_AGENT_DIR=<vendor_state_dir>/pi/agent`, `PI_OFFLINE=1`,
`PI_SKIP_VERSION_CHECK=1` and `PI_TELEMETRY=0`. No provider key variable,
proxy credential or caller environment. Tool children inherit this
environment, so the marker reaches them (E32). `--offline` and `PI_OFFLINE`
only stop startup catalog and version requests; they do not isolate the
network (E49) and are never presented as doing so.

### 4.2 Private agent directory and credentials (PI-4)

The user's agent directory cannot be made safe with flags: under
`-ne -nc -ns -np --no-approve`, its `APPEND_SYSTEM.md` is still appended and
its `settings.json` still applies, including a `shellCommandPrefix` that
prefixed every bash call (E37). Pi also loads its `SYSTEM.md`, `models.json`
and resource directories (E64), and even `pi --version` touches it (E00).

So VIA uses `<vendor_state_dir>/pi/agent` (a managed 0700 directory,
runtime §6.1). The owner logged Pi in there once
(`PI_CODING_AGENT_DIR=… pi`, then `/login`); Pi wrote its own `auth.json`
(OpenAI, OAuth) and reads it on every turn (E49). VIA never reads, copies or
logs credentials; this follows Claude's precedent (the vendor reads its own
login).

### 4.3 Profile policy (PI-4b)

Directory privacy alone is not a configuration boundary (E37). **Before
every launch** the driver validates the agent directory. A violation is a
`submit_failed` refusal with `data.reason:"handshake_refused"`, whose
VIA-owned message names the rule and the entry or key, never a value. It is
**never cached**: the owner can fix the profile without a binary change. The
message travels as the shared `RouteError::HandshakeRefused`'s optional
VIA-owned `detail` (Claude and Codex pass none), on a failure that launched
nothing. VIA
never writes, repairs or deletes anything in the agent directory.

- **Files.** At most 64 entries, `bin/`'s files counted with the
  directory's own. Every entry checked is owned by the
  daemon's uid, has the expected type, is opened through the directory's
  descriptor without following symlinks (a symlink is refused), and is
  neither group- nor world-writable. `auth.json` also has no group or other
  bits; it is never opened.
- **Entries allowed** (anything else is refused, including `SYSTEM.md`,
  `APPEND_SYSTEM.md`, `models.json`, `AGENTS.md`, `extensions/` and
  `skills/`):

  | Entry | Rule |
  |---|---|
  | `auth.json` | regular file; Pi creates it even without a login (E59) |
  | `models-store.json` | regular file; Pi's catalog cache (E59, E60); not parsed |
  | `settings.json` | **required** regular file, at most 64 KiB, one JSON object within runtime §8's structure limits, holding only the keys below |
  | `bin/` | directory of regular files; Pi's managed tool binaries (E60, `config.js` `getBinDir`) |
  | `sessions/` | directory; the owner's interactive runs; never traversed (VIA passes `--session-dir`) |

- **Settings keys allowed** (any other key is refused, for example
  `shellCommandPrefix`, `packages`, `defaultThinkingLevel` or `retry`):

  | Key | Rule | Why |
  |---|---|---|
  | `cacheWarming` | required, exactly `"off"` | Absent means `"streaming"`: extra model calls on eligible models (E61) |
  | `defaultProvider`, `defaultModel` | string ≤ 1 KiB | Login metadata; `--model` overrides them (E58) |
  | `lastChangelogVersion` | string ≤ 64 B | Vendor bookkeeping |
  | `deviceId` | string ≤ 256 B | Vendor identifier; its value is never recorded |

- **Record.** The turn's evidence folder gets `pi-profile.json` (≤ 4 KiB):
  the policy version, entry names, kinds and modes, the settings key names,
  and one policy digest (SHA-256 over those plus the approved values except
  `deviceId`). A list that would pass 4 KiB is replaced by its count. No
  credential bytes and no device identifier. It records the check the
  launch passed, not the directory after the turn (Pi creates `auth.json`
  and `models-store.json` on its first run, E59), and is written only for a
  turn that launched, best effort: the turn's outcome never depends on it.
- **Profile setting.** The real profile had no `cacheWarming` key (E60);
  `"cacheWarming": "off"` was added to its `settings.json` on 2026-10-05.
  VIA itself never writes it: a profile without it is refused.
- **Accepted limitation.** A same-uid process can change the directory
  between validation and Pi's read; that needs code already running as the
  user. Revisit if vendors ever run under another uid.

### 4.4 VIA-created state

Outside the agent directory: `pi/sessions/<via_session_id>/` (managed 0700;
Pi creates its session files inside) and `pi/instructions/<via_session_id>`
(the frozen instructions, 0600, written atomically before each launch).

### 4.5 Canonical parameters

| Field | Mapping |
|---|---|
| model | `--model provider/id`, exact; checked at the handshake (§2.1). A model without a non-empty provider and ID is refused by `plan` and `check_turn` (`InvalidParam{model}`) before any receipt: Pi could only fuzzy-match it (E30) |
| instructions | Frozen text in the VIA-owned file, passed by absolute path to `--append-system-prompt`. Pi reads an argument that names an existing file as that file (E38), so inline text could silently read a file whose path equals the text |
| prompt, instructions size (PI-15) | `plan`/`check_turn` refuse `ParamSizes.prompt_json` above 524,288 bytes (`InvalidParam{prompt}`) and `instructions_json` above 262,144 bytes (`InvalidParam{instructions}`), before any receipt (C2 §2). Pi repeats the prompt, re-escaped, in its user `message_start`, `message_end` and `agent_end` records, and the creating run's system patch with the instructions in two records (E04, E47, E57). Each record must stay under the 1 MiB vendor-message ceiling (runtime §8); the remaining 256 KiB is headroom for Pi's base prompt (~6 KB, E47), context files and the run's other messages, which PI-15 does not bound. A record over 1 MiB still fails `overflow`, as on Claude (L1) |
| effort (PI-6) | `low`, `medium`, `high`, `xhigh`, `max` and vendor values `off`, `minimal` pass to `--thinking`. Pi clamps silently and only warns on stderr for invalid values (E41), so a requested effort is checked against `get_state.thinkingLevel` (§2.1) and the check's result is cached for `check_turn`. With no effort, `--thinking` is omitted, Pi's default applies (the policy refuses `defaultThinkingLevel`, E58), and the observed level goes to bounded `vendor` data |
| output_schema | unsupported |
| max_steps | unsupported; non-null refused before vendor I/O |
| bound, extra_write_dirs | §6 |
| deadlines | Core-owned. Pi has no turn timeout; its provider idle timeout (300 s) and retry backoff (up to 60 s) can lengthen a turn without events (docs `settings.md`) |
| tools | `--tools read,bash,edit,write`. This is explicit launch configuration, not handshake-proven: the handshake returns no tool list. Where a system patch carries `toolsAdded`/`toolsRemoved` (a creating launch or a changed loadout, E05, E56), an added name outside the list, or a removed name in it, is protocol; a patch lists only changes, so the added names need not be the whole list. Pi accepts unknown names silently (E34), and restores a stored loadout only without `--tools` (E56) |

### 4.6 Inherited configuration (C2 §6.2), both directions

With the private profile and `--no-approve`:

| Category | Requested **on** | Requested **off** |
|---|---|---|
| hooks | `-ne` is unconditional (extensions are unvetted code): effective `off`, verified (E33, E40), warning `config_switch_unverified` | `off`, verified |
| MCP servers | Pi's MCP is an extension, removed by `-ne`: `off`, verified (E40), warning | `off`, verified |
| plugins | Pi packages load as extensions or settings `packages`, removed by `-ne` and the policy: `off`, verified, warning | `off`, verified |
| skills | No `-ns`: `$HOME/.agents/skills` loads; project skills need trust; agent-directory skills are refused by the policy. `on`, verified (E39); inventory from `get_commands` | `-ns`: `off`, verified (E39) |
| agents | Pi 1.0 has no agent loader; subagents are only an example extension, removed by `-ne` (E68). `off`, verified by source, warning | `off`, verified by source |
| instruction files | No `-nc`: `AGENTS.override.md` > `AGENTS.md` > `CLAUDE.md` per directory, cwd and ancestors (E36). `on`, verified; inventory §4.7 | `-nc`: `off`, verified (E36) |

Prompt templates have no C2 category; `-np` always removes them because they
rewrite `/`-prefixed prompts. The live runs used `-ns -nc`; the production
default is L5.

### 4.7 Instruction-file inventory (PI-7)

Pi's `role:"system"` message is a **patch** against the transcript's
current prompt sections (E51, E62): a member carries the section's full new
text, `null` removes it, an absent member is unchanged, and no message means
nothing changed. A process's first run emits one only when something
changed (E04, E51).

- Read the patch from the system `message_end` only, once per turn.
- The paths come from `sections.project_context`, which renders each file
  as `<project_instructions path="P">…</project_instructions>` with no
  escaping (E62), so file contents can hold tag-like text. Accept a listing
  only if every path is absolute, names `AGENTS.override.md`, `AGENTS.md` or
  `CLAUDE.md`, and lies in the cwd or an ancestor, at most one per
  directory; otherwise record `unparsed`.
- `pi-inventory.json` in the turn's evidence folder records one of: the
  paths (`listed`), `none` (`project_context:null`), `not_reported` (no
  `project_context` member, or no patch: Pi reported no change this turn),
  or `unparsed`; plus the `skill:*` names from `get_commands`. At most 32
  paths of 1 KiB and 256 skill names; beyond that, `unparsed`. The record
  is `{"version":1,"instruction_files":{"state":S[,"paths":[…]]},
  "skills":{"state":"listed"|"unparsed","names":[…]}}`, written best effort
  once the handshake passed.
- The inventory is evidence, not public state: C2 §6.2 asks only what a
  route can record. `not_reported` is never presented as empty or
  unchanged. Instruction contents never enter observations, envelopes or
  evidence.

### 4.8 Trust and reserved keys

**PI-5.** Always `--no-approve`: project `.pi/settings.json`, extensions,
skills, `SYSTEM.md`/`APPEND_SYSTEM.md` and MCP never load (E33). Context
files still load; Pi does not trust-gate them.

**Reserved names** (from the pinned `pi --help` of 1.0.2). Long:
`--provider`, `--model`, `--api-key`, `--system-prompt`,
`--append-system-prompt`, `--mode`, `--print`, `--continue`, `--resume`,
`--session`, `--session-id`, `--fork`, `--session-dir`, `--no-session`,
`--name`, `--models`, `--no-tools`, `--no-builtin-tools`, `--tools`,
`--exclude-tools`, `--thinking`, `--extension`, `--no-extensions`,
`--skill`, `--no-skills`, `--prompt-template`, `--no-prompt-templates`,
`--theme`, `--no-context-files`, `--export`, `--list-models`, `--approve`,
`--no-approve`, `--offline`, `--help`, `--version`: every recipe flag, its
negations and opposites, VIA's canonical parameters, and the options that
change the mode, the session, the model, the tools or the loaded resources,
or exit at once. Short: `-p`, `-c`, `-r`, `-n`, `-nt`, `-nbt`, `-t`, `-xt`,
`-e`, `-ne`, `-ns`, `-np`, `-nc`, `-a`, `-na`, `-h`, `-v`.

**Reserved vendor keys.** The vendor-option allow-list is empty. A key
naming a reserved name or short form in any C2 §6.3 normalized spelling, or
a `PI_*` environment name, is `vendor_option_conflict`; any other key is
`invalid_params`.

**Vendor argument passthrough (owner, 2026-10-06; C2 §6.3; adopted).** A
session's frozen `vendor_args` are appended after the last recipe argument
(§4.1, after `--offline`) on every per-turn launch, after a daemon restart
too; they enter the handshake-refusal recipe key and the launch-request
check (`invalid_params` naming `vendor_args`, or `cwd` when the session has
none, past Host's 64 KiB). Matched under C2 §6.3: every reserved long name,
every operand and `--`. Pi matches each option as an exact string, its
short forms multi-letter, so no single-dash element is a cluster of
switches: **every** single-dash element is refused. Pi does not split
`--name=value` (it keeps the element as an unknown flag); the match judges
it by its name all the same, so a reserved name is refused in either
spelling. The unreserved options that take a value are `--use-theme` and
`--tui-mode`, each its next element. Environment names are not arguments
and stay unreachable.

## 5. Typed protocol and normalizer

### 5.1 Typed records

| Record | Required fields (else protocol, C2 rule 6) | Rule |
|---|---|---|
| `response` | `type`, `command`, `success` (bool); `id` for every VIA command; `data` object on success of `get_state`, `get_available_models`, `get_commands`, `prompt`; `error` string on failure | Paired by `id` **and** `command` equal to what VIA sent. Wrong `command`, unknown `id`, a second reply to one `id`, or `command:"parse"` is protocol. Never an observation |
| `get_state.data` | `sessionId`, `sessionFile`, `model.provider`, `model.id`, `thinkingLevel` | §2.1 checks |
| `agent_start`, `turn_start`, `turn_end`, `agent_end`, `agent_settled` | `type` | Lifecycle; `turn_end`/`agent_end` bodies repeat messages and are never re-normalized |
| `message_start`, `message_end` | `message.role` | §5.2; an unknown role is activity |
| `message_end` `role:"assistant"` | Per Pi's documented `AssistantMessage` (docs `message-types.md`): `content` an array whose blocks each have a string `type` (a `text` block also a string `text`); `stopReason` a string; `usage` an object with non-negative numbers `input`, `output`, `cacheRead`, `cacheWrite`, `totalTokens` and `cost.total` (`reasoning` is optional there: absent → `reasoning_output` `null`) | A missing or malformed member is protocol. All-zero numbers are well-formed: they are the §5.5 missing-usage case (`null`), not this one |
| `message_update` | `assistantMessageEvent.type` | §5.2; its cumulative `usage` is ignored |
| `tool_execution_start`, `tool_execution_end` | `toolCallId`, `toolName` | Progress marks; no per-tool state (cleanup is group-based, §7) |
| `extension_ui_request` | `method` | §6 |
| `compaction_*`, `auto_retry_*`, `entry_appended`, `queue_update`, other known types | `type` | Activity; compaction usage per §5.5 |
| unknown `type` | — | Activity; no observation |

- Before the `started` reply, a lifecycle or `message_*` record is protocol
  (E04); pre-prompt compaction records may arrive (E63) and are activity,
  their usage held (§5.5).
- One `agent_settled` per run. A second one, or one with no assistant
  `message_end` since `started`, is protocol: no terminal exists.
- IDs, names, stop reasons and codes are bounded to 1 KiB (C2 A1).
- Control (abort, stop orders) runs on the private lifecycle's independent
  control lane and stays serviceable while observations are saturated, as
  Claude.

### 5.2 Normalized behaviour

| Incoming | Normalized behaviour |
|---|---|
| `message_end` `role:"system"` | Inventory (§4.7) and tool-loadout check (§4.5) only. Holds every loaded context file: never final text, progress or envelope content |
| `message_*` `role:"user"` | Activity (the prompt echo) |
| `message_update` `text_start`/`_delta`, `thinking_start`/`_delta`, `toolcall_start`/`_delta` | One `progress {model:true}` per record, so a long streaming block resets the idle deadline |
| other `message_update` | Activity |
| `message_end` `role:"assistant"` | One usage sample (§5.5); the candidate terminal |
| `tool_execution_start` / `_end` | `progress` `tools_started (toolCallId, toolName)` / `tools_ended (toolCallId)` |
| `message_*` `role:"toolResult"`, `tool_execution_update` | Activity |
| stderr | Written to `stderr.log` by the OS; never read |

Observations keep decode order, and the route uses the private lifecycle's
decode fence, as Claude: idle expiry waits for delivered progress, and the
terminal follows every earlier observation.

### 5.3 Terminal and final text

The terminal is the last assistant `message_end` before `agent_settled`;
auto-retried errors before a later success are superseded (E28).

| `stopReason` | Terminal |
|---|---|
| `stop` | `Completed`, `end_turn`; natural completion wins an abort race |
| `length` | `Completed`, stop reason `budget`, vendor reason kept |
| `aborted`, or `error` with `errorMessage` exactly `"This operation was aborted"` | With VIA's abort and its paired successful reply: `Interrupted` (§7). Without VIA's abort: `Failed`, `vendor_error` |
| `error`, otherwise | `Failed`; class hint below; `detail` per §5.4 |
| `toolUse` | Protocol: no terminating tool exists in this recipe |
| `deferred`, `pending`, anything else | `Failed`, canonical `stop_reason: other`, class hint `vendor_error`, `vendor_stop_reason` kept |

**Final text** is the text blocks of the terminal message when `stopReason`
is `stop` or `length`, sent as completed C2 `final_text` pieces (as
Claude). Earlier assistant text, thinking blocks and the partial text an
abort leaves (E16) are never final text.

**Class hints (PI-13).** `errorMessage` is free text (E27). Its only stable
structure is the HTTP status from Pi's provider layer (E53, source
`formatProviderError`): `<Label> (NNN): <body>` or `NNN: <body>`. Match
only `^NNN: ` or `^[^(:]{1,64} \(NNN\): `. 401/403 → `auth`; 429 →
`rate_limit`; anything else → `vendor_error`. No substring inference, so no
`context_exceeded` until a structured signal exists (L7).

**Denials.** Pi has no permission system; a disabled tool's `Tool bash not
found` (E34) is free text, so no `action.denied` is emitted.

### 5.4 Failure text and raw evidence (PI-14)

This governs every failure path: terminal `detail` and `vendor_code`,
`StartRejected::VendorError` text, command rejections and handshake
diagnostics.
- **Never copy raw vendor text.** The live 401 `errorMessage` echoed a
  masked fragment of the key in use (E53).
- From a provider error VIA keeps the matched HTTP status and, when the rest
  is one complete JSON object ≤ 16 KiB, its top-level `type` and `code`,
  each only if it matches `^[a-z0-9_.-]{1,64}$`. `detail` is VIA-owned text
  built from those (for example `provider error 401 invalid_request_error`);
  `vendor_code` is the accepted `code`, else absent.
- A command rejection (`success:false`) keeps no vendor text or code; its
  message is "pi refused the prompt before starting a run".
- **Raw evidence is Pi's own transcript.** Pi persists failed turns,
  provider error included (E53). `get_state.sessionFile`, resolved against
  the cwd, is the `transcript` hint on `session.vendor_identity_confirmed`
  when it lies inside this session's directory (≤ 4 KiB, C1 §5); otherwise
  there is no hint. VIA adds no raw-traffic log.

### 5.5 Usage and cost (PI-12)

One keyless `UsageSample` per assistant `message_end` (one per model call,
E25): C1 `input` = `input + cacheRead + cacheWrite` (Pi's `input` excludes
cache reads, E24); `cached_input` = `cacheRead`; `output` = `output`;
`reasoning_output` = `reasoning`; `total` = `totalTokens`.

- **Missing usage is `null`, never 0.** Pi reports missing usage as all
  zeros (E26), and aborted or errored calls carry zeros too (E16, E54). A
  sample whose `input`, `output`, `cacheRead` and `cacheWrite` are all 0 is
  sent with every component `null`, so C2's ledger rule makes the turn's
  totals `null` rather than an undercount.
- **Compaction** is a model call too (source E63). A `compaction_end` with
  `result.usage` is one more sample with the same mapping; one without it
  is an all-`null` sample. A sample decoded before the `started` reply is
  held and delivered after `turn.accepted` (C2 §4's early-message retiming,
  as Codex), and dropped on a rejection.
- `message_update` usage is cumulative and never a sample;
  `get_session_stats` is session-cumulative (E25, E52) and is not used.
- **Cost.** The sum of `usage.cost.total` over the turn's samples, `scope:
  turn`, `provenance: estimated`: Pi computes it from catalog prices, so
  under OAuth it is not a billed amount (E24, E52). If any sample is
  all-`null`, the cost is `{usd: null, scope: turn, provenance: unavailable}`; a partial
  sum is never the turn's cost.
- Cache warming adds model calls on eligible models (E61); the profile
  policy requires it off (§4.3).

## 6. Never-ask, declines and bound

**Never-ask.** Pi never asks for tool approval (docs `security.md`). Its
only client requests are extension dialogs (E44, E64), and with `-ne` and no
`-e` nothing can raise one. The driver still answers on the control lane
within A6's 5 s:
- an `extension_ui_request` with an `id`: reply
  `{"type":"extension_ui_response","id":ID,"cancelled":true}`. For a dialog
  (`select`, `confirm`, `input`, `editor`) or an unknown method, emit
  `vendor.request_declined {vendor_method:"extension_ui/<method>",
  summary:<title, ≤ 256 B>, blocking:true}` after the reply is written. Pi
  ignores a reply with no pending request (E64);
- without an `id`: a known fire-and-forget method (`notify`, `setStatus`,
  `setWidget`, `setTitle`, `set_editor_text`) is activity; a dialog or an
  unknown method fails closed as protocol (C2 rule 5), because an
  unanswered dialog blocks the run indefinitely (E44).

**Bound (PI-11).** Pi has no sandbox or permission mode, and its file tools
are not confined to cwd (E35).
- Only `{mode: full, network: true}` is claimable (`bounds:["full"]`,
  `network_control:false`).
- `read_only`, `workspace_write` and every `network:false` are
  `bound_unsupported` naming `pi-rpc`; nonempty `extra_write_dirs` with
  `full` is `invalid_params` (as OpenCode). All are refused before vendor
  I/O.
- `--tools` controls which tools the model is offered (E34); it is not
  containment. A future `read_only` recipe would need a gate like
  CLAUDE-BOUND-1; none is proposed.

VIA claims only: the requested model and effort were applied
(handshake-checked), the tool list was applied as launch configuration,
nothing prompts, project-local executable configuration did not load, and
the profile passed its policy. Never: containment of any kind, tools staying
in cwd, escaped descendants stopped, or network isolation by `--offline`.

## 7. Interrupt, close, cleanup and recovery

### 7.1 Interrupt (PI-9)

1. Write `{"id":"<v>","type":"abort"}` on the control lane. Pi aborts the
   run, SIGKILLs the running tool's group, and replies only after
   `agent_settled` (E15: 105 ms during a tool; E16: 6 ms while streaming;
   E54 live).
2. **`Acknowledged` needs both** the paired abort reply (`command:"abort"`,
   `success:true`) **and** a cancellation marker on the terminal of the run
   that settled after the abort was written: `stopReason:"aborted"`, or
   `"error"` with `errorMessage` exactly `"This operation was aborted"`
   (tool-phase abort, E15, E17, E54).
   - The reply alone is not evidence: an idle abort also succeeds (E18).
   - An arbitrary error, `pending` or an unfamiliar reason is not a marker.
     A 401 racing the abort is `failed(auth)` with cancel outcome
     `requested`.
   - `stop`/`length` keeps `Completed`; the cancel is not acknowledged.
3. **The reply follows settlement (E54).** An abort admitted before
   `agent_settled` keeps the connection open: the driver retains the
   terminal and keeps reading until the reply arrives or the stop order's
   `force_at` (or the wall cleanup cutoff) passes. Only then does it close
   stdin. With no reply, a marker terminal is not retained
   (`terminal: None`) and S1's stop-order row applies (C1 §7.6
   private-process row); other terminals are retained as usual.
4. Then S1's close, as Claude.

**Wall cleanup step:** as Claude, a Host force close with no abort (C2
§4.1). The anchor's TERM reaches Pi's group; Pi's SIGTERM handler kills its
tracked tool groups and exits 143 with no terminal (E31).

### 7.2 Close

Per turn: stdin EOF after `agent_settled` (and after any admitted abort's
reply or cutoff), then exit 0. Force: Host's verified-anchor own-group stop
(TERM, then KILL), as Claude. SIGINT is unhandled in RPC mode and leaves
tool groups running; SIGKILL leaves them too (E31). VIA never sends SIGINT.

### 7.3 Cleanup and recovery

As Claude (C2 §2 Interrupt): `Quiescent` only with `GroupAbsent` for Pi's
own group. Every bash tool runs in its own detached session and group (E31,
source `bash.js`), so group absence proves nothing about tools, as with
Claude's Bash (Claude §7). Pi kills tracked tool groups on abort, EOF,
SIGTERM and SIGHUP (E31). Tools that escape (`setsid`, or any tool after a
SIGKILL of Pi) are leftovers (C2 §4.2), found through the inherited marker
(E32).

**Recover:** unsupported, as Claude: `Unknown`, or `Dead` only with
Host-confirmed death; no resend.

### 7.4 R1: no launch beside an unresolved Pi (G9)

Pi has no session-file lock. Two processes on one session both append, the
history forks, and a later resume follows one branch, with no error (E13).
Only Pi's own process writes the session file, so Pi's own group is the
predicate.
- **Rule.** Before the launch, the driver asks Host, through Route and Wire,
  to prove that **every unresolved earlier Pi launch of this VIA session is
  gone**: every Host anchor record owned by a `Turn` of this session without
  a committed `GroupAbsent` proof, whether held, busy, or not yet re-held
  from the journal after a restart.
- **Bound.** One pass of runtime §5.2's non-signalling probe, within the
  turn's wall deadline. It signals nothing, waits for no vendor process,
  and acquires no vendor or session-file lock or writer; it waits only for
  its Store replies and may commit an absence proof.
- **Not proven** (the Store answered and a record is still without a
  proof: a group still present or busy, a probe denial or namespace
  mismatch, an identity-less record, a recovery record not yet re-held):
  `Err(Rejected{UncertainPredecessor})` with no-launch evidence →
  `failed(submit_failed)`, `failure.data.reason:"uncertain_predecessor"`
  (OD-PI-1).
- **Store failure** (decision C-3, orchestrator, 2026-10-06): a Store read
  that fails or outlives the deadline, or a proof the pass observed but
  could not commit, is the check's `Err`, the turn's Store failure as for
  any Host journal failure, never `uncertain_predecessor`. Nothing is
  launched. A failed, timed-out or late read is `store`, not committed,
  whatever kind the Store gave (a dropped read reply reports
  `UncertainCommit`). An absence proof the pass could not commit is
  Host's journal failure: not committed, or uncertain only when the proof's
  own commit outcome is uncertain. A wall spent before the read is
  `deadline`.
- **Several turns.** If turn A leaves a survivor, turn B refuses without
  launching, and turn C's check still finds A, because the predicate is A's
  group, not B's clean outcome. Once Host proves A's group absent, turns
  proceed.
- **Unchanged:** C1 §7.3 dispatch, earlier envelopes,
  `predecessor_cleanup_uncertain` on turns that launch, and the no-resend
  rule.
- **Mechanism.** Host's `session_predecessors_resolved(session, deadline)`
  (runtime §5.2), through Route and Wire. It runs Host's
  `reprobe_held(deadline, Some(session))`, which proves a session's held
  groups and which Core's close loops (E66), then reads the Store for the
  two cases the pass cannot see: busy groups and records outside the
  in-memory ledger. Not extended to Claude (L10).

## 8. Required fixture tests (none run by this design)

A scripted stdio fake shaped like the evidence, with sanitized fixtures;
hostile profiles only in scratch agent directories. Selection as Claude's:
`cargo nextest run --locked --workspace -E 'test(/^pi_/)'`.

| Test | Decisive assertion |
|---|---|
| `pi_plan_pure` | `describe` starts nothing; unchecked or unreadable version → `untested` with the warning; steer refused by name; `max_steps`, `output_schema`, limited bounds, `network:false`, nonempty `extra_write_dirs`, a prompt over 524,288 or instructions over 262,144 JSON-encoded bytes are refused before receipt |
| `pi_version_read` | The Pi package's `version` from `package.json` through a symlinked entry and the `dist` rule, reported even when Pi exits before the handshake; a missing, oversize, non-object or non-string file → `null`/`untested`, the turn proceeds; no process starts |
| `pi_profile_policy` | Accepted: the allowed set, with and without Pi-created files (E59). Refused by name, no launch and no value in the message: `shellCommandPrefix`, `SYSTEM.md`, `APPEND_SYSTEM.md`, `models.json`, `defaultThinkingLevel`, an unknown key, missing or non-`off` `cacheWarming`, a malformed value, a symlink, wrong owner, a group-writable entry, `auth.json` with group bits, an oversize file, 65 entries. `pi-profile.json` holds no `deviceId` or credential bytes; refusals are not cached |
| `pi_uncertain_predecessor` | A leaves an unproven group; B is refused `uncertain_predecessor` without launching; C is refused too; A proven absent, D launches. The same with A busy, and across a restart with A's record not yet re-held. No resend |
| `pi_identity_continuation` | `--session-id` until confirmed; confirmation only with `started`; a rejected turn 1 (no file) lets turn 2 create; a lost `started` reply keeps `--session-id`; once confirmed, `--session`; a missing file then exits before RPC → `Rejected{Protocol}`, never a fresh session; derived ID stable across eviction and restart |
| `pi_handshake_checks` | Wrong `sessionId` → `resume_mismatch` with no prompt line; model not in the catalog or a clamped requested effort → `InvalidParam` with no prompt; omitted effort skips the check; a non-skill command → refusal, cached; exit before the replies → `Rejected{Protocol}` with stderr unread |
| `pi_protocol_typed` | Wrong `command` for a known `id`, a duplicate reply, missing required fields, an assistant `message_end` without `content` or `usage` or with a non-numeric usage member (all-zero usage stays `null`, not protocol), a lifecycle or message record before `started`, `agent_settled` without an assistant terminal, a second `agent_settled`, `handled`/`queued`: all protocol, no acceptance or resend |
| `pi_acceptance` | Only `started` accepts; `success:false` → `Rejected{VendorError}` with VIA-owned text and no code; lost reply → unknown submission; a pre-acceptance compaction sample arrives after `turn.accepted` |
| `pi_settled_not_agent_end` | Auto-retry with several `agent_end` records gives one terminal at `agent_settled`; stdin stays open until then |
| `pi_terminal_mapping` | Every §5.3 row; only the terminal message is final text; the system message never reaches final text, progress or the envelope |
| `pi_progress_deltas` | One text block streaming longer than `idle_ms` keeps the turn alive; usage snapshots never become samples; idle expiry waits for the decode fence |
| `pi_usage_accounting` | Mixed present and all-zero samples → `null` token components and `unavailable` cost; cache and reasoning counters map as §5.5; compaction with and without usage |
| `pi_abort` | Tool-phase and streaming markers with the paired reply acknowledge; the idle reply alone never does; a 401 racing the abort → `failed(auth)`, `requested`; natural completion keeps `Completed`; a reply after `agent_settled` is awaited; no reply by `force_at` → no acknowledgement |
| `pi_eof_is_stop` | EOF mid-run: exit 0, no terminal, never `Completed` |
| `pi_signals_cleanup` | Force close via TERM kills tool groups; a `setsid` escapee is a leftover; SIGINT is never sent; a spinning startup is bounded by deadlines and KILL |
| `pi_dialog_decline` | A `-e` test extension's `confirm` and an unknown method with an `id` are cancelled within 5 s while observations are saturated; a dialog without an `id` fails closed |
| `pi_inventory_patch` | Add, change and remove the last instruction file (`null` → `none`); unchanged resume → `not_reported`; tag-like file contents → `unparsed`; contents never stored |
| `pi_detail_redaction` | A fixture 401 body with a key-like fragment never reaches `detail`, `vendor_code`, `failure.message`, warnings or evidence notes; an unsafe `code` is dropped; the transcript hint names the session file |
| `pi_record_ceiling` | A prompt at the admitted maximum, all control characters, runs to completion; a fixture record over 1 MiB fails `overflow`, never a short result |

## 9. Live qualification (L-list)

Live items run only on `openai/gpt-6-luna`, `--thinking low`, on VIA's
private profile with `cacheWarming:"off"`.

| Item | Risk and reasoning | Test |
|---|---|---|
| L1 records over 1 MiB | `agent_end` repeats every run message, so a long tool-heavy run, large context files or large tool arguments could exceed the ceiling even with an admitted prompt (E47 inference; not observed). A streaming decoder is a mechanism for an unobserved case | Live runs with many large tool results and a large `AGENTS.md`; record `agent_end` sizes. If real turns overflow, design a bounded decoder then, for every route |
| L2 large prompt delivery | Pi's stdin reader rescans its buffer per chunk (E57: 6 MiB in 269 ms) | Time the admitted maximum prompt |
| L3 accepted but not persisted | If Pi dies after `started` and before writing the user message, the confirmed session has no file and later turns are rejected (explicit, not silent) | Probe whether Pi persists the user message before the `started` reply |
| L4 interrupt races | Provider failure racing abort; natural completion racing abort; a delayed or lost reply after settlement; abort during retry and during compaction | Controlled fake-provider races, then one live tool abort through the adapter |
| L5 production inheritance | Live runs used `-ns -nc`; the default recipe loads skills and instruction files | Default recipe live: skill and instruction inventories recorded, policy passing |
| L6 accounting | Live cache-read and reasoning counters stayed 0 (E52); no `thinking_*` at `low`; auto-compaction and pre-prompt compaction are source-only (E63) | Live, on the private profile's `openai/gpt-6-luna`: a cache-hitting second turn and a reasoning prompt. Forced compaction (mid-run and pre-prompt) on a small context window: controlled fake-provider qualification |
| L7 error shapes | 429, context overflow and other API families' formats are unobserved (E53 is 401 only) | Fixtures for the 429 wrapper; live context overflow; record before adding classes |
| L8 cache warming | Warming on a model with `promptCache` metadata is source-only (E61); moot while the policy requires `off` | Revisit only if the policy changes |
| L9 startup network, macOS | Network without `PI_OFFLINE` was inconclusive (E46); macOS unprobed. Neither blocks the Linux recipe | Network trace; macOS run |
| L10 Claude parity | Whether Claude's private route has the same lazy-persistence or concurrent-writer hazard as PI-3 and R1; not observed, Claude unchanged | Two concurrent Claude writers on one UUID; lost-reply resume |
| L11 version drift | Evidence spans 1.0.0 and, probably, 1.0.2 | Re-run the fixture evidence on the version added to `checked` |
| L12 profile drift | A new Pi may write new agent-directory entries or settings keys, which the policy then refuses | At each pin review, re-run E59 in a scratch directory and update the allow-list |

## 10. Shared-contract amendments (record)

The G1–G10 amendments this packet required were integrated into
[C1](../via-api-v1.md), [C2](../adapter-contract.md) and the
[runtime contracts](../runtime-contracts.md) on 2026-10-05, merged with the
OpenCode 2.x amendments already there, and G7 into the
`.repo-context/CONTEXT.md` glossary; G2 and G5 needed no change. The full amendment
text, the integration notes and the Codex steer rows that still read as
active are in `scratchpad/execution/pi/history.md`.

The owner's private profile carries `"cacheWarming": "off"` (2026-10-05, §4.3).
