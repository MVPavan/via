#!/usr/bin/env python3
"""Maintainer Codex qualification through VIA (vendors/codex.md §§3–8).

Build/self-test only until reviewed. Live invocation is explicitly opt-in:
  scripts/qualify/codex.py --self-test
  scripts/qualify/codex.py --run --via target/release/via --codex PATH \
      --via-sha256 SHA256 --codex-sha256 SHA256 --candidate-version 0.160.1 \
      --evidence scratchpad/qualify/codex-RUN

This never installs/updates Codex or edits VIA's CHECKED set. A passing summary
is a candidate for a maintainer's checked-set change, not an automatic change.
Every envelope and initialize must match the pinned candidate, even if VIA
currently labels it untested. The checked set at construction is 0.159.2/0.160.0.

Threat model: Linux/Python 3.11+, a trusted maintainer, trusted pinned binaries and runner,
and private local evidence. Hostile executable/config replacement during the run
and a vendor deliberately escaping its tool policy are outside this test's
threat model. Model text is never executed by the runner. Only fixed prompts
and commands are used. Unknown protocol shapes, unreadable/reused identities,
incomplete traces and failed cleanup block qualification. The runner never
composes executable code from model text: each tool prompt is one short verbatim
argv invoking a runner-written, hashed private script outside tool workspaces.
Missing, substituted, repeated or failing commands block. Protocol command argv
and item IDs must agree with the expected single command, and /proc must prove
the same pinned interpreter/script/cwd beneath that turn's owned server. These
command checks run again over final traces; no-tool turns reject commands too.
CommandExecution omissions cannot qualify effects. A completed pure no-command
turn may be re-asked once under the separate bounded exception below.
The runner writes each action and a fresh public random seed only into that
turn's private pinned script. The tool derives its nonce from the seed and fresh
runtime entropy, so even the same script on a re-ask has a fresh unpredictable
output. Opaque filenames replace all action argv; prompts name no expected answer.
Each completed tool turn must echo its actual single nonce line. Only its hashes
are recorded; the echo proves output was seen, never execution or refusal.
Interrupted A deliberately has no final-output echo gate: it still requires the
exact native command/process, interrupted terminal and cleanup disposition.
Low effort stays fixed: runs 2/3 invoked c1, whereas run 4 emitted no command
item; that evidence does not establish that higher effort would fix compliance.
The shortened prompt removes model-composed Python and competing recall work;
it improves reproducibility but cannot guarantee model tool use. The runner never
signals an existing process. Claude's lock-based daemon lifetime proof is reused:
both private locks must belong to the replied daemon; every recorded identity
and observed descendant must be gone, /proc/locks readable, and both flocks free.
The proxy only signals its own child, through a verified pidfd. Process content
is read through directory descriptors pinned by pid plus start ticks. Initial
process metadata/lock surveys in the shared lifetime proof are read-only.
Host's normal retirement TERM is deferred inside the proxy until its child is
reaped and the drained trace is closed; an unverified/forced cutoff blocks.
If pidfd setup itself fails, only owned stdin is closed; VIA's owned-group stop
handles a child that does not acknowledge EOF. No numeric-PID fallback exists,
and qualification remains blocked.

Spending: packet §7 says usd:null/unavailable, so a dollar latch would either
invent cost or block every successful turn. Structural control instead permits
seven primary submissions and at most two additional labelled re-asks, at most
two concurrently, each with a 120 s VIA wall,
short fixed prompts, low effort, and a 1200 s run deadline. Reservations happen
BEFORE the CLI call. Only zero command executions with a completed paired native
turn, no errors/other activity and no target-file effect may authorize a re-ask.
It must use the same thread, argv and prompt; each case gets one, two for the run.
A second miss, a command deviation or attempted-but-unproven execution blocks.
Both envelopes/native starts remain accounted for, including their usage, and
final traces recheck every miss; late activity under a sealed miss blocks.
Other retries/extra turns are forbidden. A missing/unknown envelope makes
uncertainty sticky and forbids further submissions. All proxy generations share
a private locked ledger: every native thread/turn start consumes a single-use
reservation before forwarding; final accounting pairs reservations, receipts,
envelopes and native starts one-to-one. Other models/policy/effort block. This bounds
requests/time/concurrency, NOT dollars or vendor-internal retries/model calls.
Memories are disabled on VIA's argv; unknown background agent/tool traffic
blocks. Cancellation and cleanup remain permitted after the latch closes.

Owner isolation: HOME/XDG, VIA state/runtime and workspaces are private under a
short /tmp/via-codex-qual.* root, removed after proven shutdown: the worktree
scratchpad plus Host's anchor socket suffix exceeds Unix's limit. CODEX_HOME
is restored only inside VIA's own proxy child to the owner's existing Codex home;
that home comes from UID metadata, independent of the runner's private HOME.
credentials stay there. auth.json is lstat'ed for existence/owner/mode only:
never opened, copied, linked or logged. Noncredential config.toml is parsed for
root MCP keys and side-effect settings; only name digests reach evidence.
Inactive profile MCP keys receive no root override; an active profile blocks.
Each root name is disabled on our server's argv, together with apps and plugins;
notify=[] is set only on that argv. Custom provider/profile/telemetry settings block preflight.
Qualification runs with the owner's MCP servers and plugins disabled. This test
isolation limits the checked-set claim; it does not qualify their enabled configuration.
Managed config and project-layer config outside that inventory block BEFORE starting Codex.
The proxy requires config/read to expose effective features (all disabled),
notify (empty), the default provider and every configured MCP server (disabled)
before forwarding model/list or any thread/turn establishment. Unset provider,
profile and provider-map fields are valid defaults; other unsupported/missing
fields block with a reason; no mcpServerStatus/list, tool inventory, desktop endpoint or `codex mcp list`.
The installed 0.160.1 --help confirms dotted overrides and --disable, but does
not prove notify semantics or config/read shape/passivity. Preflight runs
`features list` with only the apps/plugins disables and notify=[], in an
empty private CODEX_HOME, requiring apps/plugins to be known and disabled.
Any startupStatus notification
still blocks, independently of echoed config. MCP-name overrides apply only to
the owned server using the owner's home, where config/read verifies their
effect. They are meaningless in the empty feature-check home; 0.160.1 rejects
those entries for lack of a transport.
The native binary contains config/read and ConfigReadParams/Response identifiers;
that is presence evidence only. The future live preflight must verify effective
values before accepting turns. It does not infer safety from the July snapshot.

Owner home writes are accepted, not minimised: stored rollouts are needed for
resume. Auth refresh, shell snapshots, caches, logs and system skills may also
change; shared auth refresh may race the desktop server. No credential content
is read. Before/after top-level counts/name digests and session-file counts/name
digests are recorded for today and tomorrow in UTC only, including the count
of new session files. That two-day window is pinned for the whole run; older
session directories are never enumerated. These metadata observations do not
prove absence of other writes. The runner removes only its
private VIA directories; owner rollouts remain for the owner to manage.

Evidence: a transparent stdio proxy observes the actual adapter, checks policy
before forwarding and records only typed numbers, fixed enums, hashes and
Booleans. It does not store raw protocol, prompts, tool output, handles, config,
environment values or owner paths. Responses are paired with requests; duplicate
or unknown IDs block. Scoped notifications racing an establishing reply are
forwarded immediately; their facts remain deferred within count/byte bounds
until that reply proves ownership. A mismatching or missing reply blocks.
Nonownership safety checks run before forwarding, and the oldest deferred entry
has a 30 s monotonic deadline, including silence and stdout backpressure.
Ownership lasts for the run: matched VIA replies in earlier private traces
establish threads for later servers. Historical turn IDs permit only a usage
notification replaying the thread's last recorded earlier total; turn activity,
items, errors, requests and any changed total block. Replays have their own fact
kind. Reports never establish IDs. Usage is matched to both its named turn and
that turn's submitting server, so replayed history cannot inflate either the
old turn or the new one.
Tool/item/terminal facts retain receipt times on the host's monotonic clock,
including deferred notifications, fixed item types and hashed item identities.
Tool facts classify fixed error phrases and compare parsed argv with the fixed
attempt, without retaining output. Single-line nonce output is validated with a
fixed public schema and retained only as line/nonce hashes. Scoped/global
diagnostics are hash-only facts; ordinary text deltas remain coalesced. Passive
thread/account status updates are compatible with a pure miss; errors, warnings
and unfamiliar activity block the exception. Every tool-turn survey is written even
when blocked: polling cadence, owned pid/start ticks, hashed argv/cwd/executable
paths, fixed wrapper roles and match flags. These bounded diagnostics neither
prove execution/refusal nor relax the exact owned-process EROFS gate. Polling
cannot exclude a process that lives entirely between samples.
Ownership rejections first record public method/item labels (unfamiliar labels
are hash-only), hashed IDs, generation, ownership flags and pending requests.
VIA receipt/status/events/cancel/result/wait/close/daemon replies have
method-specific schemas; missing/mistyped relied-on fields block.
Events paginate with strict progress and deadlines. All loops have bounds.
Signals only set a flag: no later submission, accepted turns are cancelled and
settled where possible, cleanup always runs to its deadline, and summary is
blocked. A Boolean secret scan runs before any evidence write and on the final
package; it never prints matching bytes. Binaries (VIA, pinned native Codex, actual
child executable, Python, runner, shared lifecycle) are recorded by hash.

Live-item plan (via-1ok, packet §8; via-5lr.3.3 supplied scope):
  preflight       GATE pins/version/model/auth metadata/MCP inventory/ownership.
  conversation    GATE spawn/result; output-schema set, replacement and clear;
                  GATE c1 exact command/protocol/process and allowed.txt bytes.
                  The tool prints the random recall code for stored c3 history;
                  c3 may not use a tool. Marker failures record only status/errno
                  and a path hash, never bytes or paths.
                  GATE tightened-bound enforcement: observe a fixed write-attempt
                  argv and its EROFS-only exec transition on the same pid/start
                  ticks beneath that turn's owned native server, with pinned Python
                  and cwd, and the target absent. No qualifying execution or an
                  unobserved transition blocks; a pure miss has only the bounded
                  re-ask option. Model text/tool-item omission proves nothing.
                  The fixed program sleeps 3 s before its one attempt and 6 s in
                  the denial branch. A non-EROFS refusal blocks. The next live
                  candidate must demonstrate this behaviour; it is not inferred
                  from another Codex version. Retire daemon/server, stored resume
                  with exact thread identity and excludeTurns:true.
  interrupt       GATE accepted A/B on one server; actual tool observed via
                  exact protocol command and owned /proc for both A and B;
                  cancel A through VIA, interrupted terminal and
                  acknowledged cleanup; B remains running after A interrupt,
                  then returns only its answer; A resumes successfully.
  never_ask       GATE never/user reviewer in real outbound and echoed thread
                  settings; exact protocol/process EROFS refusal; no reply grants
                  permission. RECORD ONLY textual refusal visibility and counts
                  of inducible no-grant request paths.
                  DEFER six independent inducible paths when vendor never asks:
                  pinned schemas/fakes cover them; absence alone proves nothing.
  usage           GATE keyless last samples, matched thread/turn, summed to C1
                  reported turn usage, thread total in vendor, cost unavailable.
                  DEFER any scope upgrade: this runner qualifies existing §7
                  mapping, not a new model-call interval interpretation.
  cleanup         GATE daemon stop, every observed child gone, private flocks
                  free, runtime removed, Boolean secret scan, binary hashes.
  steer/rejoin    DEFER: first-release steer/recover unsupported; socket rejoin
                  neither required nor authorized (packet §8).
No deferred or record-only observation is silently called a passed live gate.
No Linux result qualifies macOS or a broader sandbox/network matrix.
"""

import argparse
import asyncio
import copy
import datetime
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import shutil
import shlex
import signal
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest import mock

sys.dont_write_bytecode = True
import claude as shared
import codex_reservations as reservation_control
import codex_bound_probe as bound_probe

Blocked = shared.Blocked
Unreadable = shared.Unreadable
MODEL = "gpt-6-luna"
# Structural bounds: Codex packet §7 cost unavailable, runtime §8 deadlines.
TURN_LIMIT, ACTIVE_LIMIT = reservation_control.TURN_LIMIT, reservation_control.ACTIVE_LIMIT
BASE_TURN_LIMIT, REASK_LIMIT = reservation_control.BASE_TURN_LIMIT, reservation_control.REASK_LIMIT
CASE_REASK_LIMIT = reservation_control.CASE_REASK_LIMIT
# Packet §8: bound the single public nonce line independently of general tool output.
NONCE_LINE_CHARS = 256
WALL_S, RUN_S = 120, 1200
LINE_BYTES, TRACE_BYTES, PROC_BYTES = 8 * 1024 * 1024, 16 * 1024 * 1024, 256 * 1024
# Packet §8: bounded executable hashing for the fixed Python bound probe.
TOOL_EXE_BYTES = 128 * 1024 * 1024
PAGE_LIMIT, POLL_S, PROC_COUNT = 1000, 90, 32768
REQUEST_LIMIT = 4096
# Packet §3 start ordering/runtime §8: qualification's pending ownership deadline.
DEFER_S = 30
# Packet §§4/8: finite configuration inventory and notification-method evidence.
MCP_LIMIT, NOTIFICATION_METHOD_LIMIT = 128, 128
# Packet §§3/7: reserve one rejection and both teardown facts. Each pending
# establishing entry has two 64-character digests and a fixed public method;
# 256 bytes per entry plus bounded method counts and fixed fields cover them.
CONTROL_FACTS = 3
CONTROL_BYTES = REQUEST_LIMIT * 256 + NOTIFICATION_METHOD_LIMIT * 96 + 4096
# Runtime §6.1: fixed file proof, read no more than one byte beyond this marker.
WORKSPACE_MARKER = b"allowed"
NO_GRANT_METHODS = ("item/commandExecution/requestApproval", "item/fileChange/requestApproval",
                    "item/permissions/requestApproval", "item/tool/requestUserInput",
                    "mcpServer/elicitation/request", "item/tool/call")
# Packet §§3–6: public protocol labels; unfamiliar strings stay hash-only.
PUBLIC_METHODS = frozenset(NO_GRANT_METHODS) | frozenset((
    "initialize", "initialized", "model/list", "thread/start", "thread/resume", "turn/start",
    "turn/interrupt", "thread/unsubscribe", "thread/started", "thread/status/changed",
    "thread/tokenUsage/updated", "turn/started", "turn/completed", "item/started", "item/completed",
    "item/agentMessage/delta", "error", "warning", "remoteControl/status/changed",
    "account/updated", "account/rateLimits/updated", "mcpServer/startupStatus/updated"))
PUBLIC_ITEM_TYPES = frozenset(("userMessage", "agentMessage", "reasoning", "plan", "commandExecution",
                               "collabAgentToolCall", "mcpToolCall", "fileChange", "webSearch", "imageView"))

SECRET = re.compile(r"(?i)(?:bearer\s+\S+|sk-[A-Za-z0-9_-]{8,}|"
                    r"(?:access_token|refresh_token|api_key|authorization|auth\.json)"
                    r"\s*[\":=]|/home/|/Users/)")


def secret_free(value):
    """Boolean only; no match or value is printed (runtime §6.1 privacy)."""
    return SECRET.search(json.dumps(value, ensure_ascii=True)) is None


def tool_line_fact(output):
    """Packet §8: retain only hashes of one public nonce line, never tool output."""
    missing = {"output_line_hash": None, "nonce_hash": None, "output_field": None}
    if type(output) is not str or len(output) > NONCE_LINE_CHARS or len(output.splitlines()) != 1:
        return missing
    line = output.splitlines()[0]
    try:
        value = json.loads(line)
    except ValueError:
        return missing
    if type(value) is not dict or len(value) != 1:
        return missing
    field, nonce = next(iter(value.items()))
    if field not in ("a", "b", "r") or type(nonce) is not str or not re.fullmatch(r"VIAQUAL[0-9a-f]{32}", nonce):
        return missing
    return {"output_line_hash": digest(line), "nonce_hash": digest(nonce), "output_field": field}


def save(path, value):
    if not secret_free(value):
        raise Blocked("secret-free evidence scan failed")
    data = json.dumps(value, sort_keys=True, indent=1).encode() + b"\n"
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as file:
        file.write(data)


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def command_digest(command):
    """Only unwrap a displayed shell -c argv; never execute or retain tool text."""
    require(type(command) is str, "command execution unverifiable")
    try:
        argv = shlex.split(command)
    except ValueError:
        return digest(command)
    if len(argv) == 3 and Path(argv[0]).name in ("sh", "bash", "dash", "zsh") \
            and argv[1] in ("-c", "-lc"):
        command = argv[2]
    return digest(command)


def require(ok, reason):
    if not ok:
        raise Blocked(reason)


# Required fields are strict; additive public fields are allowed (C1 evolution).
# Free-form string values are interpreted in memory and only digests are saved.
SCHEMAS = copy.deepcopy(shared.SCHEMAS)
SCHEMAS.update({
    "daemon stop": {"stopping": shared.BOOL},
    "close": {"session_id": shared.STR, "state": shared.STR,
              "cancelled_turns": ("list", shared.STR), "cleanup": shared.STR,
              "leftovers": shared.ANY},
    "error": {"code": shared.INT, "message": shared.STR, "data": "error data"},
    "error data": {"kind": shared.STR},
    "sample": {"inputTokens": shared.INT, "cachedInputTokens": shared.INT,
               "outputTokens": shared.INT, "reasoningOutputTokens": shared.INT,
               "totalTokens": shared.INT},
})
SCHEMAS["envelope"].update(route=shared.STR, version_status=shared.STR, vendor=shared.ANY)
SCHEMAS["usage"].update(reasoning_output_tokens=shared.INT + shared.NULL,
                        total_tokens=shared.INT + shared.NULL, provenance=shared.STR)
SCHEMAS["cost"].update(provenance=shared.STR)
SCHEMAS["status"].update(session_id=shared.STR)
SCHEMAS["status turn"].update(n=shared.INT)


def conform(value, spec, where="reply"):
    """Method-specific required-field validation, with no Boolean integers."""
    if spec == shared.ANY:
        return value
    if isinstance(spec, str):
        require(isinstance(value, dict), f"{where}: object required")
        for key, inner in SCHEMAS[spec].items():
            require(key in value, f"{where}: missing {key}")
            conform(value[key], inner, f"{where}.{key}")
    elif spec[0] in ("nullable", "list", "nonempty"):
        kind, inner = spec
        if kind == "nullable":
            if value is not None:
                conform(value, inner, where)
        else:
            require(isinstance(value, list) and (bool(value) or kind != "nonempty"),
                    f"{where}: list required")
            for item in value:
                conform(item, inner, where)
    else:
        require(type(value) in spec, f"{where}: wrong type")
    return value


class Spending:
    """Packet §7: seven primary turns, two classified re-asks; sticky uncertainty."""
    def __init__(self):
        self.used, self.active, self.uncertain = 0, set(), False
        self.reasks = 0
        self.deadline = time.monotonic() + RUN_S

    def reserve(self, key, model, reask=False):
        shared.interrupt_guard()
        require(model == MODEL, "model must be gpt-6-luna")
        require(not self.uncertain and time.monotonic() < self.deadline,
                "spending control: uncertainty or run deadline")
        require(self.used < TURN_LIMIT and len(self.active) < ACTIVE_LIMIT,
                "spending control: turn/concurrency cap")
        require((self.reasks < REASK_LIMIT if reask else self.used - self.reasks < BASE_TURN_LIMIT),
                "spending control: re-ask/base turn cap")
        require(key not in self.active, "spending control: duplicate reservation")
        self.used += 1
        self.reasks += int(reask)
        self.active.add(key)

    def settle(self, key, envelope):
        require(key in self.active, "spending control: unknown reservation")
        try:
            conform(envelope, "envelope")
            require(envelope["state"] in {"completed", "cancelled", "failed"},
                    "spending control: unresolved effect")
        except Blocked:
            self.uncertain = True
            raise
        self.active.remove(key)


class Proc:
    """Pid/start identity and content through /proc dirfds; synthetic root in tests."""
    def __init__(self, root=Path("/proc")):
        self.root = root

    @staticmethod
    def read_fd(directory, name):
        with os.fdopen(os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory), "rb") as f:
            raw = f.read(PROC_BYTES + 1)
        require(len(raw) <= PROC_BYTES, "unverifiable identity: truncated proc read")
        return raw

    @staticmethod
    def identity(raw, pid):
        text = raw.decode()
        fields = text[text.rindex(")") + 2:].split()
        require(int(text.split(" ", 1)[0]) == pid, "unverifiable identity: stat pid")
        return {"pid": pid, "start_ticks": int(fields[19]), "state": fields[0],
                "ppid": int(fields[1])}

    def open(self, pid, ticks=None):
        require(type(pid) is int and pid > 0, "unverifiable identity: invalid pid")
        try:
            fd = os.open(self.root / str(pid), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        except OSError as error:
            if error.errno in shared.GONE:
                return None, None
            raise Blocked("unverifiable identity: proc directory") from None
        try:
            identity = self.identity(self.read_fd(fd, "stat"), pid)
            if ticks is not None and identity["start_ticks"] != ticks:
                os.close(fd)
                return None, None
            return fd, identity
        except OSError as error:
            os.close(fd)
            if error.errno in shared.GONE:
                return None, None
            raise Blocked("unverifiable identity: proc stat") from None
        except BaseException:
            os.close(fd)
            raise Blocked("unverifiable identity: proc stat") from None

    def content(self, ident, name):
        fd, _ = self.open(ident["pid"], ident["start_ticks"])
        require(fd is not None, "unverifiable identity: changed process")
        try:
            return self.read_fd(fd, name)
        except OSError:
            raise Blocked("unverifiable identity: proc content") from None
        finally:
            os.close(fd)

    def cwd(self, ident):
        """Runtime §5.2: read only the pinned owned tool's cwd link."""
        fd, _ = self.open(ident["pid"], ident["start_ticks"])
        require(fd is not None, "unverifiable identity: changed process")
        try:
            return os.readlink("cwd", dir_fd=fd)
        finally:
            os.close(fd)

    def executable_path(self, ident):
        """Runtime §5.2: owned executable link for hash-only survey diagnostics."""
        fd, _ = self.open(ident["pid"], ident["start_ticks"])
        require(fd is not None, "unverifiable identity: changed process")
        try:
            return os.readlink("exe", dir_fd=fd)
        finally:
            os.close(fd)

    def executable_hash(self, ident):
        """Packet §8: hash the pinned executable descriptor, never a credential path."""
        fd, _ = self.open(ident["pid"], ident["start_ticks"])
        require(fd is not None, "unverifiable identity: changed process")
        try:
            # /proc's exe is deliberately followed after ownership and identity
            # checks; the open descriptor remains bound across exec/unlink.
            executable = os.open("exe", os.O_RDONLY, dir_fd=fd)
            hashed, size = hashlib.sha256(), 0
            with os.fdopen(executable, "rb") as file:
                while raw := file.read(1024 * 1024):
                    size += len(raw)
                    require(size <= TOOL_EXE_BYTES, "tool executable byte bound")
                    hashed.update(raw)
            return hashed.hexdigest()
        finally:
            os.close(fd)

    def own(self, ident, root):
        """Walk a fresh pinned ancestry; names/markers alone never confer ownership."""
        current, seen = ident, set()
        for _ in range(128):
            require(current["pid"] not in seen, "foreign process: ancestry cycle")
            seen.add(current["pid"])
            fd, observed = self.open(current["pid"], current["start_ticks"])
            require(fd is not None, "unverifiable identity: ancestry")
            os.close(fd)
            if current["pid"] == root["pid"]:
                require(current["start_ticks"] == root["start_ticks"], "foreign process")
                return True
            require(observed["ppid"] > 0, "foreign process")
            fd, current = self.open(observed["ppid"])
            require(fd is not None, "unverifiable identity: parent")
            os.close(fd)
        raise Blocked("foreign process: ancestry bound")


def pins(via, codex, via_hash, codex_hash, model):
    require(model == MODEL, "model must be gpt-6-luna")
    for path, expected in ((via, via_hash), (codex, codex_hash)):
        require(bool(re.fullmatch(r"[0-9a-f]{64}", expected)), "pinned-hash missing")
        require(shared.sha256(path) == expected, "pinned-hash mismatch")
    with open(codex, "rb") as file:
        require(file.read(4) == b"\x7fELF" and os.access(codex, os.X_OK),
                "--codex must be the native executable, not a launcher")


def credential_metadata(home):
    """Existence/owner/mode only; deliberately no open/read of auth.json."""
    try:
        info = (home / "auth.json").lstat()
    except OSError:
        raise Blocked("credential metadata unavailable") from None
    require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
            and stat.S_IMODE(info.st_mode) == 0o600, "credential metadata unsafe")
    return {"exists": True, "private_mode": True, "owned": True}


def mcp_names(home):
    """Only noncredential config keys; managed layers cannot be qualified here."""
    require(not Path("/etc/codex").exists(), "managed Codex configuration is unsupported")
    path = home / "config.toml"
    names = set()
    if not path.is_symlink() and not path.exists():
        return []
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as file:
        meta = os.fstat(file.fileno())
        require(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.getuid()
                and not meta.st_mode & 0o022, "unsafe noncredential config")
        raw = file.read(1024 * 1024 + 1)
    require(len(raw) <= 1024 * 1024, "configuration inventory incomplete")
    try:
        config = tomllib.loads(raw.decode())
    except (ValueError, UnicodeError):
        raise Blocked("configuration inventory unparsable") from None

    active = config.get("profile")
    profiles = config.get("profiles", {})
    if type(active) is str and type(profiles) is dict:
        selected = profiles.get(active, {})
        require(not (type(selected) is dict and selected.get("mcp_servers")),
                "active profile MCP configuration unsupported; no profile selected on owned argv")
    require(not any(key in config for key in
            ("model_provider", "profile", "model_providers", "otel")),
            "side-effect configuration unsupported in owner config; preflight blocked")
    # No profile is selected by VIA; inactive profile servers are not root servers.
    servers = config.get("mcp_servers", {})
    require(type(servers) is dict, "MCP inventory malformed")
    names.update(servers)
    require(len(names) <= MCP_LIMIT, "MCP inventory bound")
    for name in names:
        require(type(name) is str and re.fullmatch(r"[A-Za-z0-9_-]+", name), "unsafe MCP name")
    return sorted(names)


def server_args(names):
    """Packet §4: root-table {} merges; disable each known server by name instead."""
    args = ["--disable", "apps", "--disable", "plugins", "-c", "notify=[]"]
    for name in names:
        require(type(name) is str and re.fullmatch(r"[A-Za-z0-9_-]+", name), "unsafe MCP name")
        args.extend(["-c", "mcp_servers." + name + ".enabled=false"])
    return args


def feature_preflight(codex, env, work):
    """Packet §4: native feature registry, no model or owner Codex home."""
    with tempfile.TemporaryDirectory(prefix="feature-home-", dir=work) as root:
        private_env = {**env, "CODEX_HOME": root}
        try:
            result = subprocess.run([str(codex), "features", "list", *server_args([])],
                                    env=private_env, cwd=work,
                                    capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            raise Blocked("features list unavailable: cannot verify apps/plugins feature names") from None
        require(result.returncode == 0,
                "features list unavailable under feature overrides in empty private Codex home")
        require(len(result.stdout.encode()) <= PROC_BYTES and len(result.stderr.encode()) <= PROC_BYTES,
                "features list output bound")
        features = {}
        for line in result.stdout.splitlines():
            fields = line.split()
            require(len(fields) >= 3 and fields[-1] in ("true", "false"),
                    "features list schema unsupported")
            name = fields[0]
            require(name not in features, "features list duplicate name")
            features[name] = fields[-1]
        require(all(features.get(name) == "false" for name in ("apps", "plugins")),
                "features list cannot confirm disabled apps/plugins feature names")
        return {"apps_disabled": True, "plugins_disabled": True, "registry_checked": True}


def effective_config(result, names):
    """Packet §4: explicit disables plus unset/default provider configuration."""
    require(type(result) is dict and type(result.get("config")) is dict,
            "config/read unavailable or schema unsupported: cannot verify effective safety")
    config = result["config"]
    features, servers = config.get("features"), config.get("mcp_servers")
    require(type(features) is dict and all(features.get(key) is False
            for key in ("apps", "plugins", "memories", "hooks")),
            "config/read cannot confirm owned feature overrides")
    require(config.get("notify") == [], "config/read cannot confirm notify=[]")
    require((config.get("model_provider") is None or config.get("model_provider") == "openai")
            and config.get("profile") is None
            and (config.get("model_providers") is None
                 or type(config.get("model_providers")) is dict and config["model_providers"] == {})
            and (config.get("otel") is None
                 or type(config.get("otel")) is dict and config["otel"] == {}),
            "config/read side-effect provider/profile/telemetry configuration unsupported")
    require(type(servers) is dict and len(servers) <= MCP_LIMIT,
            "config/read MCP inventory unavailable")
    for name, server in servers.items():
        require(type(name) is str and re.fullmatch(r"[A-Za-z0-9_-]+", name), "unsafe MCP name")
        require(type(server) is dict and server.get("enabled") is False,
                "config/read found enabled or unverifiable MCP server")
    require(set(names) <= servers.keys(), "config/read lost configured MCP names")
    return {"kind": "effective_config", "all_servers_disabled": True,
            "servers": len(servers), "plugins_disabled": True, "notify_empty": True}


def owner_metadata(home, today=None):
    """Runtime §6.1: top-level and two UTC session days; names/counts only."""
    require(not home.is_symlink(), "owner home metadata unavailable")
    entries = list(home.iterdir())
    require(len(entries) <= PROC_COUNT, "owner metadata entry bound")
    today = today or datetime.datetime.now(datetime.timezone.utc).date()
    session_files, pending = set(), []
    for day in (today, today + datetime.timedelta(days=1)):
        path = home
        # Check every ancestor without enumerating the history or following links.
        for part in ("sessions", *day.strftime("%Y/%m/%d").split("/")):
            path = path / part
            try:
                info = path.lstat()
            except FileNotFoundError:
                break
            require(stat.S_ISDIR(info.st_mode), "owner session metadata ancestor unsafe")
        else:
            pending.append(path)
    visited = 0
    while pending:
        path = pending.pop()
        try:
            info = path.lstat()
        except FileNotFoundError:
            continue
        visited += 1
        require(visited <= PROC_COUNT, "owner session metadata bound")
        require(not stat.S_ISLNK(info.st_mode), "owner session metadata symlink unsupported")
        if stat.S_ISDIR(info.st_mode):
            pending.extend(path.iterdir())
            require(len(pending) + visited <= PROC_COUNT, "owner session metadata bound")
        elif stat.S_ISREG(info.st_mode):
            session_files.add(digest(str(path.relative_to(home))))
    return {"top_entries": len(entries), "entry_digests": sorted(digest(p.name) for p in entries),
            "session_files": len(session_files), "session_digests": sorted(session_files)}


def ownership_history(directory, generation, binary_hash, version, cache=None):
    """Packet §§3/7: carry only matched VIA reply identities from this run's private traces."""
    threads, turns, totals = set(), set(), {}
    cache = {} if cache is None else cache
    paths = list(Path(directory).glob("server-*.jsonl"))
    require(len(paths) <= TURN_LIMIT, "ownership history generation bound")
    ordered = []
    for path in paths:
        name = re.fullmatch(r"server-(\d+)-(\d+)\.jsonl", path.name)
        require(name is not None, "ownership history filename unsupported")
        pid, ticks = map(int, name.groups())
        if digest((pid, ticks)) != generation:
            ordered.append((ticks, pid, path))
    for ticks, pid, path in sorted(ordered):
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd, "rb") as file:
                meta = os.fstat(file.fileno())
                require(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.getuid()
                        and stat.S_IMODE(meta.st_mode) == 0o600 and meta.st_nlink == 1,
                        "ownership history unsafe file")
                identity = (meta.st_dev, meta.st_ino, meta.st_size, meta.st_mtime_ns, meta.st_ctime_ns)
                if path in cache and cache[path][0] == identity:
                    rows = cache[path][1]
                else:
                    raw = file.read(TRACE_BYTES + 1)
                    require(len(raw) <= TRACE_BYTES, "ownership history byte bound")
                    lines = raw.splitlines(keepends=True)
                    # A concurrent owned writer may be partway through its final line.
                    if lines and not lines[-1].endswith(b"\n"):
                        lines.pop()
                    rows = [decode_line(line) for line in lines]
                    cache[path] = identity, rows
            if not rows:
                continue
            require(len(rows) <= REQUEST_LIMIT and [row["seq"] for row in rows]
                    == list(range(1, len(rows) + 1)), "ownership history sequence unsupported")
            header = rows[0]
            require(header["kind"] == "process" and type(header["pid"]) is int
                    and type(header["start_ticks"]) is int and header["pid"] == pid
                    and header["start_ticks"] == ticks and header["executable_hash"] == binary_hash,
                    "ownership history generation identity mismatch")
            pending, initialized, submitted = {}, False, set()
            for row in rows[1:]:
                if row["kind"] == "request" and "request" in row:
                    require(row["request"] not in pending, "ownership history duplicate request")
                    pending[row["request"]] = row
                elif row["kind"] == "reply":
                    request = pending.pop(row["request"], None)
                    require(request is not None and request["method"] == row["method"],
                            "ownership history unpaired reply")
                    method = row["method"]
                    if method == "initialize":
                        require(row["version"] == version, "ownership history version mismatch")
                        initialized = True
                    elif method in ("thread/start", "thread/resume", "turn/start"):
                        require(initialized and request["model"] == MODEL
                                and request["never"] is True and request["reviewer_user"] is True,
                                "ownership history policy unverified")
                        thread = row["thread"]
                        require(type(thread) is str and re.fullmatch(r"[0-9a-f]{64}", thread),
                                "ownership history thread identity unsupported")
                        if method in ("thread/start", "thread/resume"):
                            require(row["never"] is True and row["reviewer_user"] is True
                                    and (method != "thread/resume" or request["thread"] == thread),
                                    "ownership history resume/policy mismatch")
                            threads.add(thread)
                        else:
                            turn = row["turn"]
                            require(thread in threads and request["thread"] == thread
                                    and type(turn) is str and re.fullmatch(r"[0-9a-f]{64}", turn),
                                    "ownership history turn identity unsupported")
                            turns.add((thread, turn))
                            submitted.add((thread, turn))
                elif row["kind"] == "usage":
                    require((row["thread"], row["turn"]) in submitted,
                            "ownership history usage outside submitting generation")
                    conform(row["total"], "sample", "ownership history total")
                    totals[row["thread"]] = {key: row["total"][key] for key in SCHEMAS["sample"]}
        except (OSError, KeyError, TypeError, ValueError):
            raise Blocked("ownership history unreadable or schema unsupported") from None
    for path in list(cache):
        if path not in paths:
            del cache[path]
    return threads, turns, totals


class Wire:
    """Paired protocol facts, never raw content (packet §§3–7)."""
    def __init__(self, version, sink=None, history=None, generation=None, reservations=None):
        self.version, self.pending, self.facts, self.starts = version, {}, [], 0
        self.declines, self.server_requests = {}, {}
        self.initialized, self.inventory = False, False
        self.sink = sink
        self.threads, self.turns, self.notifications = set(), set(), {}
        self.deferred, self.deferred_bytes = [], 0
        self.history, self.generation = history, generation
        self.earlier_threads, self.earlier_turns = set(), set()
        self.earlier_totals, self.evidence_bytes = {}, 0
        # Parser-only fixtures omit the ledger. Every operational proxy supplies it.
        self.reservations = reservations

    def fact(self, **value):
        control = value.get("kind") in ("ownership_rejection", "notification_counts", "end")
        require(len(self.facts) < REQUEST_LIMIT - (0 if control else CONTROL_FACTS),
                "protocol evidence bound")
        require(secret_free(value), "secret-free protocol evidence")
        size = len(json.dumps({"seq": len(self.facts) + 1, **value}, sort_keys=True).encode()) + 1
        require(self.evidence_bytes + size <= TRACE_BYTES - (0 if control else CONTROL_BYTES),
                "protocol evidence byte bound")
        if self.sink:
            self.sink(value)
        self.facts.append(value)
        self.evidence_bytes += size
        return value

    def refresh_history(self):
        """Packet §3: monotonic run-wide ownership, refreshed from paired earlier replies."""
        if self.history is not None:
            threads, turns, totals = self.history()
            self.earlier_threads.update(threads)
            self.earlier_turns.update(turns)
            self.earlier_totals.update(totals)

    def reject_ownership(self, method, params, reason, scope=None, reply_request=None):
        """Packet §§3/7: leave sanitized context before every ownership rejection."""
        self.refresh_history()
        nested_thread, nested_turn = params.get("thread"), params.get("turn")
        thread = nested_thread.get("id") if method == "thread/started" and type(nested_thread) is dict \
            else params.get("threadId")
        turn = nested_turn.get("id", params.get("turnId")) if type(nested_turn) is dict \
            else params.get("turnId")
        thread = digest(thread) if type(thread) is str else None
        turn = digest(turn) if type(turn) is str else None
        if scope is not None:
            thread, turn = scope
        if (thread, turn) in self.earlier_turns and (thread, turn) not in self.turns \
                and reason.startswith("unowned"):
            reason = "historical turn traffic is not an exact usage replay"
        item = params.get("item")
        item_type = item.get("type") if type(item) is dict else None
        establishing = [{"method": request["method"], "request": key,
                         "thread": request.get("thread")} for key, request in self.pending.items()
                        if request["method"] in ("thread/start", "thread/resume", "turn/start")]
        self.fact(kind="ownership_rejection", reason=reason,
                  method=method if method in PUBLIC_METHODS else None, method_hash=digest(method),
                  item_type=item_type if type(item_type) is str and item_type in PUBLIC_ITEM_TYPES else None,
                  item_type_hash=digest(item_type) if type(item_type) is str else None,
                  thread=thread, turn=turn, thread_owned=thread in self.threads | self.earlier_threads,
                  turn_owned=(thread, turn) in self.turns | self.earlier_turns,
                  turn_known_earlier=turn is not None and any(t == turn for _, t in self.earlier_turns),
                  generation=self.generation, pending_establishing=establishing,
                  reply_request=reply_request)
        raise Blocked(reason)

    def outgoing(self, message):
        self.deferred_timeout()
        require(isinstance(message, dict), "protocol request shape")
        if "method" not in message:
            key = digest(message["id"])
            require(key in self.server_requests, "uncorrelated decline reply")
            method = self.server_requests.pop(key)
            expected = {
                "item/commandExecution/requestApproval": {"decision": "decline"},
                "item/fileChange/requestApproval": {"decision": "decline"},
                "item/permissions/requestApproval": {"permissions": {}},
                "item/tool/requestUserInput": {"answers": {}},
                "mcpServer/elicitation/request": {"action": "decline", "content": None},
                "item/tool/call": {"contentItems": [], "success": False},
            }
            require(message.get("result") == expected[method] if method in expected
                    else message.get("error", {}).get("code") == -32601,
                    "server request granted or unverifiable")
            self.declines[method] = self.declines.get(method, 0) + 1
            return self.fact(kind="decline", method_hash=digest(method), no_grant=True)
        method = message["method"]
        require(method in ("initialize", "initialized", "model/list", "thread/start",
                           "thread/resume", "turn/start", "turn/interrupt", "thread/unsubscribe"),
                "protocol method not qualified")
        params = message.get("params", {})
        require(isinstance(params, dict), "protocol params shape")
        fact = {"kind": "request", "method": method}
        if method in ("thread/start", "thread/resume", "turn/start"):
            require("id" in message, "native start requires request id")
            require(params["model"] == MODEL, "model must be gpt-6-luna")
            require(params["approvalPolicy"] == "never" and params["approvalsReviewer"] == "user",
                    "never-ask/reviewer policy mismatch")
            require(self.inventory, "MCP inventory not verified")
            fact.update(model=MODEL, never=True, reviewer_user=True)
            if method == "turn/start":
                self.starts += 1
                require(self.starts <= TURN_LIMIT and params["effort"] == "low",
                        "spending control: proxy starts/effort")
                if not self.owns((digest(params["threadId"]), None)):
                    self.reject_ownership(method, params, "unowned thread request")
                fact.update(thread=digest(params["threadId"]), schema=digest(params["outputSchema"]),
                            bound=params["sandboxPolicy"]["type"])
            else:
                fact.update(bound=params["sandbox"])
                if method == "thread/resume":
                    require(params["excludeTurns"] is True, "resume history not excluded")
                    fact.update(thread=digest(params["threadId"]), exclude_turns=True)
        elif method == "turn/interrupt":
            if not self.owns((digest(params["threadId"]), digest(params["turnId"]))):
                self.reject_ownership(method, params, "unowned turn interrupt")
            fact.update(thread=digest(params["threadId"]), turn=digest(params["turnId"]))
        if "id" in message:
            require(type(message["id"]) in (str, int), "protocol id shape")
            key = digest(message["id"])
            require(key not in self.pending and len(self.pending) < REQUEST_LIMIT,
                    "protocol duplicate/pending bound")
            fact["request"] = key
            if self.reservations is not None and method in ("thread/start", "thread/resume", "turn/start"):
                prompt = None
                if method == "turn/start":
                    items = params.get("input")
                    require(type(items) is list and len(items) == 1 and type(items[0]) is dict
                            and set(items[0]) == {"type", "text"} and items[0]["type"] == "text"
                            and type(items[0]["text"]) is str, "reserved native prompt shape")
                    prompt = digest(items[0]["text"])
                require(type(params.get("cwd")) is str, "reserved native cwd missing")
                fact["reservation"] = self.reservations.claim(method, self.generation, key,
                    digest(params["cwd"]), fact.get("thread"), prompt)
            self.pending[key] = fact.copy()
        return self.fact(**fact)

    def scope(self, method, params):
        """Packet §3: scope string identities; protocol prefixes and errors stay strict."""
        prefixed = method.startswith(("thread/", "turn/", "item/"))
        strict_ids = prefixed or method == "error"
        has_ids = any(key in params for key in ("threadId", "turnId")) if strict_ids else \
            any(type(params.get(key)) is str for key in ("threadId", "turnId"))
        if not prefixed and not has_ids:
            return None
        thread = params.get("threadId")
        if method == "thread/started":
            nested = params.get("thread")
            thread = nested.get("id") if type(nested) is dict else None
        if type(thread) is not str:
            self.reject_ownership(method, params, "unowned thread notification: missing identity")
        turn = params.get("turnId")
        if method.startswith("turn/"):
            nested = params.get("turn")
            turn = nested.get("id", turn) if type(nested) is dict else turn
        has_turn = (method.startswith(("turn/", "item/"))
                    or method == "thread/tokenUsage/updated"
                    or ("turnId" in params if strict_ids else type(turn) is str))
        if has_turn and type(turn) is not str:
            self.reject_ownership(method, params,
                                  "unowned turn notification; correlation unavailable: missing identity")
        return digest(thread), digest(turn) if has_turn else None

    def owns(self, scope, message=None):
        """Packet §§3/7: old turns permit only exact total replays, never new activity."""
        thread, turn = scope
        if thread not in self.threads or turn is not None and (thread, turn) not in self.turns:
            self.refresh_history()
        if thread not in self.threads | self.earlier_threads:
            return False
        if turn is None or (thread, turn) in self.turns:
            return True
        if (thread, turn) not in self.earlier_turns or message is None or "id" in message \
                or message.get("method") != "thread/tokenUsage/updated":
            return False
        usage = message["params"].get("tokenUsage")
        total = usage.get("total") if type(usage) is dict else None
        return type(total) is dict and thread in self.earlier_totals and \
            {key: total.get(key) for key in SCHEMAS["sample"]} == self.earlier_totals[thread]

    def establishing(self, scope):
        """Packet §3 start ordering: match only requests able to establish this scope."""
        thread, turn = scope
        if scope in self.earlier_turns:
            return set()
        if thread not in self.threads | self.earlier_threads:
            return {key for key, request in self.pending.items()
                    if turn is None and (request["method"] == "thread/start"
                    or request["method"] == "thread/resume" and request["thread"] == thread)}
        return {key for key, request in self.pending.items()
                if request["method"] == "turn/start" and request["thread"] == thread and turn is not None}

    def deferred_timeout(self):
        """Packet §3 start ordering: bound the oldest unproved ownership to 30 s."""
        if not self.deferred:
            return None
        remaining = DEFER_S - (time.monotonic() - self.deferred[0][3])
        if remaining < 0:
            message = self.deferred[0][0]
            self.reject_ownership(message["method"], message["params"], "deferred notification deadline")
        return remaining

    def resolve_deferred(self, replied):
        """Packet §3 start ordering: publish deferred facts only after ownership proof."""
        self.deferred_timeout()
        pending, self.deferred = self.deferred, []
        self.deferred_bytes = 0
        for message, candidates, size, created in pending:
            candidates.discard(replied)
            scope = self.scope(message["method"], message["params"])
            if self.owns(scope, message):
                self.incoming(message, observed_at=int(created * 1000))
            else:
                if not candidates:
                    self.reject_ownership(message["method"], message["params"],
                                          "unowned deferred notification after establishing reply")
                self.deferred.append((message, candidates, size, created))
                self.deferred_bytes += size

    def incoming(self, message, observed_at=None):
        self.deferred_timeout()
        observed_at = int(time.monotonic() * 1000) if observed_at is None else observed_at
        require(isinstance(message, dict), "protocol reply shape")
        if "id" in message and "method" not in message:
            require(type(message["id"]) in (str, int), "protocol id shape")
            key = digest(message["id"])
            require(key in self.pending, "protocol uncorrelated reply")
            request = self.pending.pop(key)
            require("result" in message and "error" not in message, "protocol request refused")
            result, method = message["result"], request["method"]
            require(isinstance(result, dict), "protocol result object required")
            fact = {"kind": "reply", "method": method, "request": key}
            if method == "initialize":
                require(type(result.get("userAgent")) is str, "initialize version missing")
                require(result["userAgent"].startswith("via/"), "initialize version prefix")
                require(result["userAgent"][4:].split(" ")[0] == self.version,
                        "version mismatch")
                self.initialized = True
                fact["version"] = self.version
            elif method == "model/list":
                require(type(result.get("data")) is list and "nextCursor" in result,
                        "model/list schema")
            elif method in ("thread/start", "thread/resume"):
                require(result["model"] == MODEL and result["approvalPolicy"] == "never"
                        and result["approvalsReviewer"] == "user", "thread policy echo mismatch")
                native_thread = result.get("thread")
                thread_id = native_thread.get("id") if type(native_thread) is dict else None
                if type(thread_id) is not str:
                    self.reject_ownership(method, {"threadId": thread_id}, "thread identity missing",
                                          reply_request=key)
                thread = digest(thread_id)
                if method == "thread/resume" and thread != request["thread"]:
                    self.reject_ownership(method, {"threadId": thread_id}, "resume identity mismatch",
                                          reply_request=key)
                self.threads.add(thread)
                fact.update(thread=thread, never=True, reviewer_user=True)
            elif method == "turn/start":
                native_turn = result.get("turn")
                turn_id = native_turn.get("id") if type(native_turn) is dict else None
                if type(turn_id) is not str:
                    self.reject_ownership(method, {}, "turn/start id missing",
                                          scope=(request["thread"], None), reply_request=key)
                self.turns.add((request["thread"], digest(turn_id)))
                fact.update(thread=request["thread"], turn=digest(turn_id))
            elif method not in ("turn/interrupt", "thread/unsubscribe"):
                raise Blocked("protocol reply method not qualified")
            if "reservation" in request:
                self.reservations.reply(request["reservation"], method, self.generation, key,
                                        fact["thread"], fact.get("turn"))
                fact["reservation"] = request["reservation"]
            reply = self.fact(**fact)
            self.resolve_deferred(key)
            return reply
        method, params = message["method"], message["params"]
        require(type(method) is str and isinstance(params, dict), "notification schema")
        require(method != "mcpServer/startupStatus/updated", "MCP startup observed despite disables")
        scope = self.scope(method, params)
        if method == "remoteControl/status/changed":
            require(params.get("status") == "disabled", "remote control not disabled")
        if "id" in message:
            if scope is not None and not self.owns(scope, message):
                self.reject_ownership(method, params,
                                      "unowned thread/turn notification; correlation unavailable")
            key = digest(message["id"])
            require(type(message["id"]) in (str, int) and key not in self.server_requests
                    and len(self.server_requests) < REQUEST_LIMIT, "duplicate/server request bound")
            self.server_requests[key] = method
            return self.fact(kind="server_request", method_hash=digest(method),
                             thread=scope[0] if scope else None, turn=scope[1] if scope else None)
        fact = {"kind": "notification", "method_hash": digest(method)}
        if method == "thread/tokenUsage/updated":
            require(type(params.get("threadId")) is str and type(params.get("turnId")) is str,
                    "usage correlation unavailable")
            last, total = params["tokenUsage"]["last"], params["tokenUsage"]["total"]
            conform(last, "sample", "usage last")
            conform(total, "sample", "usage total")
            require(all(value >= 0 for sample in (last, total)
                        for value in sample.values() if type(value) is int), "negative usage")
            fact.update(kind="usage", thread=digest(params["threadId"]),
                        turn=digest(params["turnId"]),
                        last={key: last[key] for key in SCHEMAS["sample"]},
                        total={key: total[key] for key in SCHEMAS["sample"]})
        elif method == "turn/completed":
            require(params["turn"]["status"] in ("completed", "interrupted", "failed"),
                    "terminal status missing or unqualified")
            fact.update(kind="terminal", thread=digest(params["threadId"]),
                        turn=digest(params["turn"]["id"]), status=params["turn"]["status"],
                        at_ms=observed_at)
        elif method in ("item/started", "item/completed"):
            item = params["item"]
            require(item.get("type") in ("userMessage", "agentMessage", "reasoning", "plan", "commandExecution"),
                    "background agent or unqualified tool item")
            fact.update(kind="item", thread=digest(params["threadId"]), turn=digest(params["turnId"]),
                        item_type=item["type"], item_hash=digest(item["id"]) if type(item.get("id")) is str else None,
                        finished=method == "item/completed", at_ms=observed_at)
            if item["type"] == "commandExecution":
                output = item.get("aggregatedOutput")
                require(output is None or type(output) is str, "tool output type")
                code = item.get("exitCode")
                require(code is None or type(code) is int, "tool exit type")
                fact.update(kind="tool", thread=digest(params["threadId"]),
                            turn=digest(params["turnId"]), finished=method == "item/completed",
                            command_hash=command_digest(item.get("command")),
                            argv_hash=digest(bound_probe.command_argv(item.get("command"))),
                            fixed_attempt_argv=bound_probe.command_argv(item.get("command")) ==
                                bound_probe.DeniedExecution(sys.executable, "", Path("denied.txt")).attempt,
                            exit_code=code, read_only_error=isinstance(output, str)
                            and "Read-only file system" in output,
                            error_flags={"syntax_error": isinstance(output, str) and
                                ("SyntaxError" in output or "syntax error" in output.lower()),
                                "permission_denied": isinstance(output, str) and "Permission denied" in output,
                                "interpreter_unavailable": isinstance(output, str) and
                                ("can't open file" in output or "No such file or directory" in output
                                 or "command not found" in output)})
                fact.update(tool_line_fact(output))
                if self.reservations is not None:
                    expected = self.reservations.tool(self.generation, fact["thread"],
                        fact["turn"] if self.owns(scope, message) else None)
                    if expected is not None:
                        fact["fixed_attempt_argv"] = fact["argv_hash"] == expected["argv"]
                        fact["expected_argv_hash"] = expected["argv"]
                        if not fact["fixed_attempt_argv"] or expected.get("miss") is True:
                            self.fact(**fact)
                            raise Blocked("fixed command deviation")
        if scope is not None and not self.owns(scope, message):
            candidates = self.establishing(scope)
            if not candidates:
                self.reject_ownership(method, params,
                                      "unowned thread/turn notification; correlation unavailable")
            size = len(json.dumps(message).encode())
            require(len(self.deferred) < REQUEST_LIMIT and self.deferred_bytes + size <= TRACE_BYTES,
                    "deferred notification bound")
            self.deferred.append((copy.deepcopy(message), candidates, size, time.monotonic()))
            self.deferred_bytes += size
            # Only ownership remains unproved. The proxy forwards immediately;
            # no fact is published before a matching establishing reply.
            return None
        if fact["kind"] == "usage" and scope not in self.turns:
            fact["kind"] = "replay"
        if fact["kind"] == "notification":
            key = fact["method_hash"]
            require(key in self.notifications or len(self.notifications) < NOTIFICATION_METHOD_LIMIT,
                    "notification method bound")
            self.notifications[key] = self.notifications.get(key, 0) + 1
            if method != "item/agentMessage/delta":
                return self.fact(**fact, thread=scope[0] if scope else None,
                                 turn=scope[1] if scope else None, at_ms=observed_at)
            return fact
        return self.fact(**fact)


def decode_line(raw):
    require(len(raw) <= LINE_BYTES and raw.endswith(b"\n"), "protocol line truncated/over bound")
    try:
        return json.loads(raw)
    except (ValueError, UnicodeError):
        raise Blocked("protocol JSON malformed") from None


def pin_child(proc):
    """Runtime §5: owned direct child, pinned directory across pidfd acquisition."""
    directory, identity = Proc().open(proc.pid)
    require(directory is not None, "unverifiable child identity")
    candidate = None
    try:
        require(identity["ppid"] == os.getpid(), "foreign child identity")
        candidate = os.pidfd_open(proc.pid)
        # Read through the SAME directory descriptor after acquiring pidfd.
        # A reaped/reused PID cannot validate through its predecessor's dirfd.
        after = Proc.identity(Proc.read_fd(directory, "stat"), proc.pid)
        require(after["start_ticks"] == identity["start_ticks"] and after["ppid"] == os.getpid(),
                "unverifiable child identity after pidfd acquisition")
        exe_fd = os.open("exe", os.O_RDONLY, dir_fd=directory)
        with os.fdopen(exe_fd, "rb") as file:
            hashed = hashlib.file_digest(file, "sha256").hexdigest()
        verified, candidate = candidate, None
        return verified, identity, hashed
    finally:
        if candidate is not None:
            os.close(candidate)
        os.close(directory)


async def proxy(control, argv):
    """Owned stdio child, inventory barrier, typed trace; no model probes."""
    require(argv[:1] == ["app-server"], "proxy accepts app-server only")
    require(["--disable", "memories"] == argv[1:3], "memories not disabled")
    require(shared.sha256(control["codex"]) == control["codex_hash"], "pinned-hash mismatch")
    require(control["model"] == MODEL, "model must be gpt-6-luna")
    require(argv[3:] == ["--disable", "hooks", *server_args(control["mcp_names"])],
            "owned hooks/MCP argv mismatch")
    env = dict(os.environ)
    env["CODEX_HOME"] = control["codex_home"]
    termination = None
    def defer(sig, frame):
        nonlocal termination
        termination = signal.Signals(sig).name
    handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    for sig in handlers:
        signal.signal(sig, defer)
    try:
        proc = await asyncio.create_subprocess_exec(
            control["codex"], *argv,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL, env=env, limit=LINE_BYTES + 1)
    except BaseException:
        for sig, handler in handlers.items():
            signal.signal(sig, handler)
        raise
    pidfd, trace, wire, tasks, failure, reason = None, None, None, [], None, None
    try:
        pidfd, identity, executable_hash = pin_child(proc)
        require(executable_hash == control["codex_hash"], "native executable pinned-hash mismatch")
        trace_path = Path(control["trace_dir"]) / f"server-{identity['pid']}-{identity['start_ticks']}.jsonl"
        trace_fd = os.open(trace_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        trace = os.fdopen(trace_fd, "wb", buffering=0)
        sequence = 0
        def sink(value):
            nonlocal sequence
            sequence += 1
            line = json.dumps({"seq": sequence, **value}, sort_keys=True).encode() + b"\n"
            require(trace.tell() + len(line) <= TRACE_BYTES, "protocol evidence byte bound")
            trace.write(line)
        generation = digest((identity["pid"], identity["start_ticks"]))
        history_cache = {}
        history = lambda: ownership_history(control["trace_dir"], generation,
                                            control["codex_hash"], control["version"], history_cache)
        reservations = reservation_control.Reservations(control["reservations"])
        wire, write_lock = Wire(control["version"], sink, history, generation, reservations), asyncio.Lock()
        wire.fact(kind="process", pid=identity["pid"], start_ticks=identity["start_ticks"],
                  executable_hash=executable_hash)
        reader = asyncio.StreamReader(limit=LINE_BYTES + 1)
        await asyncio.get_running_loop().connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader),
                                                          sys.stdin.buffer)
        inventory_reply = asyncio.Queue(maxsize=1)
        inventory_id = "via-qualification-inventory-" + secrets.token_hex(12)
        inventory_pending = False

        async def send(message):
            async with write_lock:
                proc.stdin.write(json.dumps(message, separators=(",", ":")).encode() + b"\n")
                await proc.stdin.drain()

        async def inventory():
            nonlocal inventory_pending
            inventory_pending = True
            # Ask for effective configuration, never MCP status/tool inventory.
            # Unsupported or missing effective settings cannot establish safety.
            await send({"id": inventory_id, "method": "config/read", "params": {"includeLayers": False}})
            result = await asyncio.wait_for(inventory_reply.get(), 10)
            wire.fact(**effective_config(result, control["mcp_names"]))
            wire.inventory = True

        async def outgoing():
            while True:
                raw = await reader.readline()
                if not raw:
                    proc.stdin.close()
                    return
                message = decode_line(raw)
                require(termination is None or message.get("method")
                        not in ("turn/start", "thread/start", "thread/resume"),
                        "proxy termination: no further submissions")
                if message.get("method") == "model/list" and not wire.inventory:
                    require(wire.initialized, "model/list before initialize reply")
                    await inventory()
                wire.outgoing(message)
                await send(message)

        async def incoming():
            nonlocal inventory_pending
            while True:
                timeout = wire.deferred_timeout()
                try:
                    raw = await asyncio.wait_for(proc.stdout.readline(), timeout)
                except asyncio.TimeoutError:
                    message = wire.deferred[0][0]
                    wire.reject_ownership(message["method"], message["params"],
                                          "deferred notification deadline")
                if not raw:
                    return
                message = decode_line(raw)
                if message.get("id") == inventory_id:
                    require(inventory_pending and "method" not in message,
                            "config/read duplicate or uncorrelated reply")
                    inventory_pending = False
                    require("result" in message and "error" not in message, "config/read preflight refused or unsupported")
                    inventory_reply.put_nowait(message["result"])
                    continue
                wire.incoming(message)
                if wire.initialized and message.get("id") is not None \
                        and message.get("result", {}).get("userAgent") is not None:
                    for fact in executable_facts(identity):
                        wire.fact(**fact)
                # Nonblocking bounded stdout: a daemon that stops reading cannot
                # prevent the proxy's lifetime bound or owned-child cleanup.
                timeout = wire.deferred_timeout()
                try:
                    await asyncio.wait_for(forward_stdout(raw), timeout)
                except asyncio.TimeoutError:
                    # Preserve stdout's own shorter deadline when ownership
                    # has not expired; either timeout still blocks the run.
                    wire.deferred_timeout()
                    raise

        tasks = [asyncio.create_task(outgoing()), asyncio.create_task(incoming())]
        done, pending = await asyncio.wait(tasks, timeout=RUN_S, return_when=asyncio.FIRST_COMPLETED)
        require(bool(done), "proxy lifetime bound")
        for task in done:
            task.result()
        if tasks[0] in done:  # EOF: let the server finish its remaining replies.
            await asyncio.wait_for(tasks[1], 10)
        else:
            require(proc.returncode is not None or not wire.pending, "server ended with pending replies")
        if wire.deferred:
            message = wire.deferred[0][0]
            wire.reject_ownership(message["method"], message["params"],
                                  "unowned deferred notification: establishing reply unavailable")
    except BaseException as error:
        failure = type(error).__name__
        reason = str(error) if isinstance(error, Blocked) and secret_free(str(error)) else failure
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        try:
            await stop_child(proc, pidfd)
        except BaseException as error:
            failure = type(error).__name__
        finally:
            try:
                if pidfd is not None:
                    os.close(pidfd)
                if trace is not None:
                    try:
                        if wire is not None:
                            wire.fact(kind="notification_counts", counts=wire.notifications)
                            wire.fact(kind="end", complete=failure is None and not wire.pending
                                      and not wire.server_requests and not wire.deferred,
                                      termination_signal=termination,
                                      failure_type=failure, blocked_reason=reason, child_reaped=proc.returncode is not None)
                    finally:
                        trace.close()
            finally:
                for sig, handler in handlers.items():
                    signal.signal(sig, handler)
    return 0 if failure is None else 1


async def stop_child(proc, pidfd):
    """Reap only our own subprocess, including failure before identity/trace setup."""
    if pidfd is None:
        proc.stdin.close()
        try:
            await asyncio.wait_for(proc.wait(), 5)
        except asyncio.TimeoutError:
            raise Blocked("owned child cleanup unverified; VIA group stop required") from None
        return
    for sig in (signal.SIGTERM, signal.SIGKILL):
        if proc.returncode is None:
            try:
                signal.pidfd_send_signal(pidfd, sig)
            except ProcessLookupError:
                pass
        try:
            await asyncio.wait_for(proc.wait(), 5)
            return
        except asyncio.TimeoutError:
            pass
    raise Blocked("owned child cleanup unverified")


async def forward_stdout(raw):
    fd, loop = sys.stdout.fileno(), asyncio.get_running_loop()
    os.set_blocking(fd, False)
    deadline, offset = loop.time() + 10, 0
    while offset < len(raw):
        require(loop.time() < deadline, "proxy stdout deadline")
        try:
            count = os.write(fd, raw[offset:])
            require(count > 0, "proxy stdout no progress")
            offset += count
        except BlockingIOError:
            ready = loop.create_future()
            def wake():
                if not ready.done():
                    ready.set_result(None)
            loop.add_writer(fd, wake)
            try:
                await asyncio.wait_for(ready, max(0.001, deadline - loop.time()))
            finally:
                loop.remove_writer(fd)


def proc_stat(pid):
    try:
        fd, ident = Proc().open(pid)
    except Blocked:
        raise Unreadable("unverifiable process identity") from None
    if fd is not None:
        os.close(fd)
    return ident


def executable_facts(root):
    """Hash the owned server tree, including native children of CLI launchers."""
    entries, facts = os.listdir("/proc"), []
    require(len(entries) <= PROC_COUNT, "process survey bound")
    for name in entries:
        if not name.isdigit():
            continue
        fd, ident = Proc().open(int(name))
        if fd is None:
            continue
        os.close(fd)
        try:
            Proc().own(ident, root)
        except Blocked:
            continue
        fd, current = Proc().open(ident["pid"], ident["start_ticks"])
        require(fd is not None, "owned executable became unverifiable")
        try:
            exe_fd = os.open("exe", os.O_RDONLY, dir_fd=fd)
            with os.fdopen(exe_fd, "rb") as file:
                hashed = hashlib.file_digest(file, "sha256").hexdigest()
        finally:
            os.close(fd)
        facts.append({"kind": "executable", "pid": current["pid"],
                      "start_ticks": current["start_ticks"], "sha256": hashed})
    require(bool(facts), "owned executable tree not observable")
    return facts


class Run(shared.Run):
    """Codex operations; reuse only Claude's private-lock lifetime proof."""
    def __init__(self, args):
        # Constructor does not start/read vendors. Claude-specific operations
        # (prepare/turn/version/transcripts) are never used.
        super().__init__(argparse.Namespace(via=args.via, model=args.model, budget_usd=0,
                                           claude=args.codex, evidence=args.evidence))
        self.args, self.spend = args, Spending()
        self.owner_codex = Path(pwd.getpwuid(os.getuid()).pw_dir) / ".codex"
        require("CODEX_HOME" not in os.environ or Path(os.environ["CODEX_HOME"]).resolve()
                == self.owner_codex.resolve(), "CODEX_HOME disagrees with owner home")
        self.work = Path(tempfile.mkdtemp(prefix="via-codex-qual.", dir="/tmp"))
        try:
            self.state, self.runtime = self.work / "state", self.work / "rt"
            self.runtime.mkdir(mode=0o700)
            self.home = self.work / "home"
            self.home.mkdir(mode=0o700)
            self.trace_dir = self.work / "trace"
            self.trace_dir.mkdir(mode=0o700)
            self.reservations = reservation_control.Reservations(self.work / "reservations.json", self.spend.deadline)
            self.tool_program, self.tool_program_hash = bound_probe.write_program(self.work)
            self.command_specs = {}
            self.codex = Path(args.codex).resolve()
            self.launcher = self.work / "codex-proxy"
            self.base_env = {"PATH": os.environ.get("PATH", os.defpath), "HOME": str(self.home),
                             "USER": "via-qualification", "LOGNAME": "via-qualification",
                             "LANG": "C.UTF-8", "VIA_STATE_DIR": str(self.state),
                             "VIA_RUNTIME_DIR": str(self.runtime)}
            for name in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"):
                path = self.work / name.lower()
                path.mkdir(mode=0o700)
                self.base_env[name] = str(path)
            self.env = dict(self.base_env)
            self.envelopes, self.receipts, self.checks, self.owned = [], {}, [], set()
            self.session_cwds = {}
            self.record_only, self.owner_before, self.owner_after = [], None, None
            self.blocked_reasons = []
            self.metadata_day = datetime.datetime.now(datetime.timezone.utc).date()
        except BaseException:
            shutil.rmtree(self.work)
            raise

    def preflight(self):
        require(not any(char.isspace() for char in sys.executable), "interpreter path whitespace unsupported")
        require(sys.platform == "linux" and hasattr(os, "pidfd_open")
                and hasattr(signal, "pidfd_send_signal"), "Linux pidfd support required")
        pins(self.via, self.codex, self.args.via_sha256, self.args.codex_sha256, self.model)
        auth = credential_metadata(self.owner_codex)
        self.owner_before = owner_metadata(self.owner_codex, self.metadata_day)
        names = mcp_names(self.owner_codex)
        self.vendor_args = server_args(names)
        # No project configuration layer: private server cwd/workspaces are
        # directly beneath a new /tmp root, not beneath the owner's repository.
        for parent in (self.work, self.work.parent, Path("/")):
            require(not (parent / ".codex/config.toml").exists(), "unknown project config layer")
        version = subprocess.run([str(self.codex), "--version"], env=self.env,
                                 capture_output=True, text=True, timeout=30)
        require(version.returncode == 0 and version.stdout.strip()
                == "codex-cli " + self.args.candidate_version, "version mismatch")
        require(bool(re.fullmatch(r"\d+\.\d+\.\d+", self.args.candidate_version)),
                "candidate version shape")
        feature_check = feature_preflight(self.codex, self.env, self.work)
        control = {"codex": str(self.codex), "codex_hash": self.args.codex_sha256,
                   "codex_home": str(self.owner_codex), "mcp_names": names, "model": MODEL,
                   "trace_dir": str(self.trace_dir), "version": self.args.candidate_version,
                   "reservations": str(self.reservations.path)}
        # Control is private operational input, removed after proven shutdown;
        # it never joins the evidence package. Contains paths/keys, no secrets.
        target = self.work / "control.json"
        target.write_text(json.dumps(control))
        target.chmod(0o600)
        self.launcher.write_text("#!" + sys.executable + "\nimport os,sys\nos.execv(" +
                                 repr(sys.executable) + ", [" + repr(sys.executable) + ", " +
                                 repr(str(Path(__file__).resolve())) + ", '--proxy', " +
                                 repr(str(target)) + "] + sys.argv[1:])\n")
        self.launcher.chmod(0o700)
        save(self.evidence / "preflight.json", {
            "candidate_version": self.args.candidate_version, "model": MODEL,
            "feature_registry": feature_check,
            "auth_metadata": auth, "mcp_name_digests": [digest(name) for name in names],
            "binaries": {"via": self.args.via_sha256, "codex": self.args.codex_sha256,
                         "python": shared.sha256(sys.executable),
                         "runner": shared.sha256(__file__),
                         "lifecycle": shared.sha256(shared.__file__),
                         "reservations": shared.sha256(reservation_control.__file__),
                         "bound_probe": shared.sha256(bound_probe.__file__),
                         "tool_program": self.tool_program_hash,
                         "proxy_launcher": shared.sha256(self.launcher)}})

    def via_call(self, directory, label, *args, timeout=60, handle=None):
        """Strict CLI reply schema; handles on stdin, no stdout/stderr persisted."""
        require(shared.sha256(self.via) == self.args.via_sha256, "pinned-hash mismatch")
        if handle is not None:
            args = (*args, "--handle-stdin")
        if args[0] not in ("daemon", "cancel", "close"):
            require(time.monotonic() < self.spend.deadline, "run deadline")
            timeout = min(timeout, max(0.1, self.spend.deadline - time.monotonic()))
        if args[0] in ("spawn", "resume"):
            shared.interrupt_guard()
        result = subprocess.run([str(self.via), *args], env=self.env, capture_output=True,
                                text=True, input=None if handle is None else handle + "\n",
                                timeout=timeout)
        require(len(result.stdout.encode()) <= LINE_BYTES and len(result.stderr.encode()) <= LINE_BYTES,
                "VIA reply byte bound")
        method = " ".join(args[:2]) if args[0] == "daemon" else args[0]
        try:
            reply = json.loads(result.stderr if method == "daemon stop" and result.returncode != 0
                               else result.stdout)
        except (ValueError, UnicodeError):
            if result.returncode != 0:
                raise Blocked("VIA reply unavailable") from None
            raise Blocked("VIA reply malformed") from None
        schema = {"daemon status": "daemon status", "daemon stop": "daemon stop",
                  "spawn": "spawn receipt", "resume": "receipt", "wait": "envelope",
                  "result": "envelope", "status": "status", "events": "events page",
                  "cancel": "cancel reply", "close": "close"}[method]
        require(result.returncode in (0, 3) or method == "daemon stop", "VIA call refused")
        # Stop refusals are strictly checked but remain available to shared
        # cleanup's sessions_active -> --force escalation.
        if method == "daemon stop" and result.returncode != 0:
            conform(reply, "error")
        else:
            conform(reply, schema, method)
        # No per-call raw evidence. In-memory decisions add Booleans/digests.
        return result.returncode, reply, None

    def check(self, name, ok):
        self.checks.append({"check": name, "pass": ok is True})
        require(ok is True, name)

    def submit(self, label, verb, *args, session=None, reask_of=None):
        self.spend.reserve(label, self.model, reask=reask_of is not None)
        try:
            prompt = args[args.index("--prompt") + 1]
            if verb == "spawn":
                cwd = digest(str(Path(args[args.index("--cwd") + 1]).resolve()))
                thread = None
            else:
                cwd = self.session_cwds[session]
                previous = next(envelope for envelope in reversed(self.envelopes)
                                if envelope["session_id"] == session)
                thread = digest(previous["vendor_session_id"])
            bound_probe.verify_program(self.tool_program, self.tool_program_hash)
            spec = self.command_specs.get(label)
            if spec is not None:
                bound_probe.verify_program(spec["program"], spec["program_hash"])
            self.reservations.reserve(digest(label), verb, digest(prompt), cwd, thread,
                                      {"argv": digest(spec["argv"]) if spec else None},
                                      reask_of=digest(reask_of) if reask_of is not None else None)
            handle = self.handles[session] if session else None
            if verb == "spawn":
                args = (*args, "--", *self.vendor_args)
            rc, receipt, _ = self.via_call(self.evidence, label, verb, "--json", *args,
                                          handle=handle)
            require(rc == 0, "submission refused")
            if verb == "spawn":
                session = receipt["session_id"]
                self.handles[session] = receipt["handle"]
            self.receipts[label] = (receipt, session)
            self.session_cwds[session] = cwd
            self.reservations.receipt(digest(label), {"session": digest(session), "address": digest(receipt["turn"])})
            return session
        except BaseException:
            self.spend.uncertain = True
            self.reservations.close()
            raise

    def finish(self, label):
        receipt, _ = self.receipts[label]
        try:
            _, envelope, _ = self.via_call(self.evidence, label, "wait", receipt["turn"],
                                          "--json", "--timeout-ms", str((WALL_S + 65) * 1000),
                                          timeout=WALL_S + 80)
            self.spend.settle(label, envelope)
            _, result, _ = self.via_call(self.evidence, label, "result", receipt["turn"], "--json")
            self.check("result identity " + label, all(result[key] == envelope[key]
                       for key in ("session_id", "turn", "state", "vendor_session_id")))
            envelope = result
            self.check("candidate version " + label,
                       envelope["vendor_version"] == self.args.candidate_version)
            self.check("Codex route " + label, envelope["route"] == "codex-app-server")
            native_disposition(self, envelope)
            self.reservations.settle(digest(label), envelope_identity(envelope))
            self.envelopes.append(envelope)
            return envelope
        except BaseException:
            self.spend.uncertain = True
            self.reservations.close()
            raise

    def start(self, name):
        self.start_phase(name, {"memories": False, "inherit": {"hooks": False}})

    def fixed_command(self, label, mode, argument=None):
        """Packet §§3/5/8: opaque argv; private seed plus tool entropy hides each reply."""
        require(mode in ("allowed", "denied", "interrupt-a", "interrupt-b", "never-ask"), "fixed tool action")
        field = {"allowed": "a", "denied": "b"}.get(mode, "r")
        request = {"action": mode, "field": field, "seed": argument or "VIAQUAL" + secrets.token_hex(16)}
        program, pinned = bound_probe.write_program(self.work, request, "tool-" + digest(label)[:16] + ".py")
        argv = [str(Path(sys.executable).resolve()), str(program)]
        self.command_specs[label] = {"argv": argv, "denied": [*argv, "1"], "program": program,
                                     "program_hash": pinned, "field": field}
        return shlex.join(argv)

    def tool_reply(self, label, envelope, facts=None):
        """Packet §8: echo proves output was seen; protocol/process gates prove effects."""
        ends = [fact for fact in turn_facts(self, envelope, facts) if fact["kind"] == "tool" and fact["finished"]]
        self.check("tool output seen " + label, len(ends) == 1
                   and ends[0].get("output_field") == self.command_specs[label]["field"]
                   and ends[0].get("output_line_hash") == digest(envelope["final_text"].strip())
                   and ends[0].get("nonce_hash") is not None)
        return ends[0]["nonce_hash"]

    def start_tool(self, label, verb, *args, target, bound, refusal, session=None):
        """Packet §§7/8: one same-prompt re-ask only after a proven zero-command completion."""
        primary = label
        session = self.submit(label, verb, *args, session=session)
        for attempt in range(CASE_REASK_LIMIT + 1):
            observed = self.observe_denied_execution(label, target, bound, refusal)
            if observed is not None:
                return label, session, observed
            envelope = self.finish(label)
            completed(self, envelope, label)
            self.check("pure tool miss " + label, pure_tool_miss(tool_miss_facts(self, envelope)))
            try:
                target.lstat()
            except FileNotFoundError:
                pass
            except OSError:
                raise Blocked("tool miss target absence unverifiable") from None
            else:
                raise Blocked("tool miss had a filesystem effect")
            require(attempt < CASE_REASK_LIMIT, "second tool miss")
            self.reservations.mark_miss(digest(label))
            new_label = primary + "-reask"
            self.command_specs[new_label] = dict(self.command_specs[primary])
            prompt = args[args.index("--prompt") + 1]
            save(self.evidence / ("reask-" + primary + ".json"),
                 {"label": new_label, "reason": "zero_commands", "same_prompt": True,
                  "prompt_hash": digest(prompt), "thread": digest(envelope["vendor_session_id"]),
                  "prior_address": envelope_identity(envelope)["address"],
                  "program_hash": self.command_specs[label]["program_hash"]})
            self.record_only.append({"observation": "labelled tool re-ask", "label": new_label,
                                     "reason": "zero_commands"})
            if verb == "spawn":
                replay = [session]
                for flag in ("--bound", "--effort", "--output-schema", "--wall-ms", "--prompt"):
                    if flag in args:
                        replay.extend((flag, args[args.index(flag) + 1]))
                args = tuple(replay)
            label, verb = new_label, "resume"
            self.submit(label, verb, *args, session=session, reask_of=primary)
        raise Blocked("tool re-ask bound")

    def command_protocol(self, label, envelope, finished=True, facts=None):
        """Packet §8: exact single command, owned turn; structured model replies prove no effects."""
        tools = [fact for fact in turn_facts(self, envelope, facts) if fact["kind"] == "tool"]
        if label not in self.command_specs:
            self.check("no tool command " + label, not tools)
            return
        expected = digest(self.command_specs[label]["argv"])
        starts = [fact for fact in tools if fact["finished"] is False]
        ends = [fact for fact in tools if fact["finished"] is True]
        self.check("fixed command identity " + label, len(starts) == 1 and len(ends) <= 1
                   and starts[0].get("item_hash") is not None
                   and all(fact.get("argv_hash") == expected
                           and fact.get("item_hash") == starts[0]["item_hash"] for fact in tools))
        if finished:
            self.check("fixed command completed " + label, len(ends) == 1 and ends[0]["exit_code"] == 0)

    def facts(self, final=False):
        found = []
        traces = []
        for path in self.trace_dir.glob("*.jsonl"):
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
            with os.fdopen(fd, "rb") as file:
                raw = file.read(TRACE_BYTES + 1)
            require(len(raw) <= TRACE_BYTES, "trace byte bound")
            lines = raw.splitlines(keepends=True)
            rows = [decode_line(line) for line in lines if line.endswith(b"\n")]
            require(not final or len(rows) == len(lines), "trace truncated")
            require([row["seq"] for row in rows] == list(range(1, len(rows) + 1)),
                    "trace sequence incomplete")
            if final:
                require(bool(rows) and rows[-1]["kind"] == "end" and rows[-1]["complete"]
                        and rows[-1]["child_reaped"], "trace end unverified")
            traces.append(rows)
        require(len(traces) <= TURN_LIMIT, "server generation bound")
        for rows in sorted(traces, key=lambda rows: rows[0]["start_ticks"] if rows else -1):
            for row in rows:
                if row["kind"] in ("process", "executable"):
                    self.owned.add((row["pid"], row["start_ticks"]))
                found.append({**row, "trace": digest((rows[0]["pid"], rows[0]["start_ticks"]))})
        require(not final or bool(found), "no owned server trace")
        return found

    def events(self, address):
        after, events, deadline = 0, [], time.monotonic() + 120
        for _ in range(PAGE_LIMIT):
            shared.interrupt_guard()
            require(time.monotonic() < deadline, "event deadline")
            _, page, _ = self.via_call(self.evidence, "events", "events", address,
                                      "--after", str(after), "--json",
                                      timeout=min(30, max(1, deadline - time.monotonic())))
            seqs = [event["seq"] for event in page["events"]]
            require(all(seq > prior for seq, prior in zip(seqs, [after] + seqs)),
                    "events lack strict progress")
            events.extend(page["events"])
            if page["more"] is False:
                return events
            require(page["next_after"] > after and
                    (not seqs or page["next_after"] >= seqs[-1]), "event cursor stalled")
            after = page["next_after"]
        raise Blocked("event page bound")

    def observe_denied_execution(self, label, target, bound="readOnly", refusal=True):
        """Packet §§3/5: exact process under this turn's server; EROFS transition for writes."""
        spec = self.command_specs.get(label)
        probe = bound_probe.DeniedExecution(sys.executable, shared.sha256(sys.executable), target,
                    attempt=spec["argv"] if spec else None, denied=spec["denied"] if spec else None,
                    program=spec["program"] if spec else None,
                    program_hash=spec["program_hash"] if spec else None, require_refusal=refusal)
        prefix = "changed-bound" if label == "c2" else "command-" + label
        survey = bound_probe.Survey(probe)
        deadline = min(time.monotonic() + POLL_S, self.spend.deadline)
        key, proc = digest(label), Proc()
        facts, context, outcome = [], {"reservation": key}, "blocked"
        try:
            while time.monotonic() < deadline:
                shared.interrupt_guard()
                facts = self.facts()
                starts = [fact for fact in facts if fact["kind"] == "reply"
                          and fact.get("method") == "turn/start" and fact.get("reservation") == key]
                if starts:
                    require(len(starts) == 1, "prohibited execution native start ambiguous")
                    start = starts[0]
                    context.update(thread=start["thread"], turn=start["turn"], trace=start["trace"])
                    roots = [fact for fact in facts if fact["kind"] == "process" and fact["trace"] == start["trace"]]
                    require(len(roots) == 1, "prohibited execution server unavailable")
                    requests = [fact for fact in facts if fact["kind"] == "request"
                                and fact.get("method") == "turn/start" and fact.get("reservation") == key]
                    require(len(requests) == 1 and requests[0]["bound"] == bound,
                            "prohibited execution bound unverified")
                    root = roots[0]
                    entries = os.listdir("/proc")
                    require(len(entries) <= PROC_COUNT, "process survey bound")
                    survey.scan()
                    for pid in entries:
                        shared.interrupt_guard()
                        require(time.monotonic() < deadline, "prohibited execution observation deadline")
                        if not pid.isdigit():
                            continue
                        fd, identity = proc.open(int(pid))
                        if fd is None:
                            continue
                        os.close(fd)
                        try:
                            proc.own(identity, root)
                        except Blocked:
                            continue  # No foreign argv/cwd/executable reads.
                        self.owned.add((identity["pid"], identity["start_ticks"]))
                        observed = probe.observe(proc, identity, root, survey)
                        if observed is not None:
                            observed.update(context)
                            save(self.evidence / (prefix + "-execution.json"), observed)
                            outcome = "proven"
                            return observed
                    if any(fact["kind"] == "terminal" and fact.get("thread") == start["thread"]
                           and fact.get("turn") == start["turn"] and fact["trace"] == start["trace"] for fact in facts):
                        matched = [fact for fact in facts if all(fact.get(field) == context[field]
                                   for field in ("thread", "turn", "trace"))]
                        if not probe.seen and pure_tool_miss(matched):
                            outcome = "zero_commands"
                            return None
                        break
                time.sleep(0.1)
            raise Blocked("prohibited execution not observed")
        finally:
            tools = [{field: fact.get(field) for field in ("item_type", "item_hash", "at_ms", "finished",
                      "command_hash", "argv_hash", "fixed_attempt_argv", "exit_code", "read_only_error", "error_flags")}
                     for fact in facts if fact["kind"] == "tool" and "turn" in context
                     and all(fact.get(field) == context[field] for field in ("thread", "turn", "trace"))]
            save(self.evidence / (prefix + "-survey.json"), survey.snapshot(outcome, context, tools))

    def stop_daemon(self, final=False):
        # Refresh owned identities before surveying ancestry; still stop on a
        # malformed trace, then report that uncertainty after the stop attempt.
        inspection_error = None
        try:
            self.facts()
        except BaseException as error:
            inspection_error = error
        # Include proxy children/tools already identified even if reparented.
        if self.daemons:
            record = self.daemon_unverified or self.daemons[-1]
            known = {tuple(item) for item in record.get("descendants_watched", [])}
            known.update(self.owned)
            record["descendants_watched"] = sorted(known)
        result = super().stop_daemon(final=final)
        if inspection_error is not None:
            raise Blocked("trace inspection unavailable during daemon stop") from None
        return result

    def start_phase(self, name, codex_config, home=None):
        self.stop_daemon()
        self.state.mkdir(mode=0o700, exist_ok=True)
        config = {"harnesses": {"codex": {"binary": str(self.launcher), **codex_config}}}
        target = self.state / "daemon.json"
        target.write_text(json.dumps(config) + "\n")
        target.chmod(0o600)
        self.env = dict(self.base_env)
        if home is not None:
            self.env["HOME"] = str(home)
        phase_dir = self.evidence / "daemon"
        phase_dir.mkdir(mode=0o700, exist_ok=True)
        save(phase_dir / f"{name}-config.json",
                   {"harnesses": {"codex": {"binary": "<proxy>", **codex_config}}})
        self.phase = name
        # `daemon status` may autostart a daemon: it is owned but unverified,
        # its runtime directory kept, until the replied pid is seen holding
        # this run's private locks.
        record = {"phase": name, "runtime_dir_kept": str(self.runtime)}
        self.daemons.append(record)
        self.daemon_unverified = record
        pid, stat = None, None
        try:
            try:
                rc, status, err = self.via_call(phase_dir, f"{name}-status", "daemon",
                                                "status", "--json", timeout=60)
                detail = err if err is not None else f"rc {rc}, no live daemon pid in the reply"
            except Blocked as error:
                status, detail = None, str(error)
            pid = None
            if status is not None:
                try:
                    pid = conform(status, "daemon status", f"{name} daemon status")["pid"]
                except Blocked as error:
                    detail = str(error)
            stat = proc_stat(pid) if pid is not None else None
            if stat is not None:
                # The daemon itself takes both locks: /proc/locks must show
                # it as their locker. This also proves the lock ids match.
                # Its lines must be flock locks, which the stop proof's probe
                # sees.
                lines = self.lock_lines()
                ids = [shared.lock_id(path) for path in self.lock_paths()]
                if lines is None or None in ids or any(
                        [pid, "FLOCK"] not in lines.get(key, []) for key in ids):
                    stat, detail = None, f"/proc/locks does not show pid {pid} holding " \
                                         "this run's lock files with flock"
            if stat is None:
                record["start_failed"] = str(detail)
                raise Blocked(f"daemon for phase {name} did not start verifiably ({detail})")
        except BaseException as error:
            # Any unsuccessful start: find what it left by the private locks.
            # Identification is a recorded pid with start ticks: the replied
            # pid, a descriptor holder, or a /proc/locks locker read with its
            # start ticks. Anything else (nothing found, or a locker whose
            # pid cannot be read) leaves ownership uncertain for the rest of
            # the run.
            record.setdefault("start_failed", f"{type(error).__name__}: {error}")
            if isinstance(pid, int) and not isinstance(pid, bool):
                try:
                    known = proc_stat(pid)
                except Unreadable:
                    known = None
                if known is not None:
                    record.update(pid=pid, start_ticks=known["start_ticks"])
            try:
                record["found"] = self.find_daemons()
            except Exception as find_error:
                record["found"] = f"search failed: {type(find_error).__name__}"
            try:
                record["lock_lines"] = self.lock_lines()
            except Exception as lock_error:
                record["lock_lines"] = f"unreadable: {type(lock_error).__name__}"
            lockers, unrecorded = self.lockers(record["lock_lines"])
            record["lockers"] = lockers
            identified = ("pid" in record or lockers
                          or (isinstance(record["found"], list) and bool(record["found"])))
            if not identified or unrecorded:
                record["identity_unknown"] = True
                if unrecorded:
                    record["lockers_unrecorded"] = unrecorded
            for locker in unrecorded:
                self.mark_uncertain(locker, None, "a lock holder not readable with its "
                                                  "start ticks")
            raise
        del record["runtime_dir_kept"]
        record.update(pid=pid, start_ticks=stat["start_ticks"])
        self.daemon = {"pid": pid, "start_ticks": stat["start_ticks"]}
        self.daemon_unverified = None
        print(f"phase {name}: daemon started", flush=True)


def spawn_args(run, ws, prompt, bound="workspace_write", schema=None):
    args = ["--harness", "codex", "--model", MODEL, "--effort", "low", "--cwd", str(ws),
            "--bound", bound, "--wall-ms", str(WALL_S * 1000), "--background", "--prompt", prompt]
    if schema is not None:
        args.extend(["--output-schema", str(schema)])
    return args


def completed(run, envelope, label):
    run.check("completed " + label, envelope["state"] == "completed"
              and envelope["failure"] is None and envelope["stop_reason"] == "end_turn")


def fixed_prompt(command):
    """Packet §8: one verbatim command; the model composes no executable code."""
    prompt = ("Invoke the shell tool now. Execute this exact runner-provided command once in the "
              "current working directory, in the foreground, and wait for it to finish. "
              "Do not edit it, substitute another command, use another tool, or retry.\n"
              "```sh\n" + command + "\n```")
    return prompt + "\nAfter it finishes, reply with exactly the tool's single output line and nothing else."


def pure_tool_miss(facts):
    """Packet §§7/8: only a completed, fully paired turn with no tool/deviation may re-ask."""
    terminals = [fact for fact in facts if fact["kind"] == "terminal"]
    return (len(terminals) == 1 and terminals[0].get("status") == "completed"
            and all(fact["kind"] in ("terminal", "usage")
                or fact["kind"] == "reply" and fact.get("method") == "turn/start"
                or fact["kind"] == "notification" and fact.get("method_hash") in
                    {digest(method) for method in ("turn/started", "thread/status/changed", "account/updated",
                                                   "account/rateLimits/updated", "remoteControl/status/changed")}
                or fact["kind"] == "item" and fact.get("item_type") in
                    ("userMessage", "agentMessage", "reasoning", "plan") for fact in facts))


def tool_miss_facts(run, envelope, facts=None):
    """Packet §8: include scoped or global diagnostics that cannot name a native turn."""
    facts = run.facts() if facts is None else facts
    matched = turn_facts(run, envelope, facts)
    start = next(fact for fact in matched if fact["kind"] == "reply" and fact["method"] == "turn/start")
    return matched + [fact for fact in facts if fact["kind"] in ("notification", "server_request")
        and fact.get("turn") is None
        and fact.get("trace") == start["trace"] and fact.get("thread") in (None, start["thread"])
        and (start.get("seq") is None or fact.get("seq", 0) >= start["seq"])]


def turn_facts(run, envelope, facts=None):
    """Exact C1 ordinal -> paired native turn identity, across server retirement."""
    facts = run.facts() if facts is None else facts
    thread = digest(envelope["vendor_session_id"])
    starts = [fact for fact in facts if fact["kind"] == "reply" and fact["method"] == "turn/start"
              and fact["thread"] == thread]
    require(0 < envelope["turn"] <= len(starts), "turn correlation missing")
    start = starts[envelope["turn"] - 1]
    return [fact for fact in facts if fact.get("thread") == thread and fact.get("turn") == start["turn"]
            and fact.get("trace") == start.get("trace")]


def envelope_identity(envelope):
    """C1 §4/packet §7: content-free identity and disposition for reservation accounting."""
    return {"session": digest(envelope["session_id"]),
            "address": digest(f"{envelope['session_id']}/{envelope['turn']}"),
            "thread": digest(envelope["vendor_session_id"]), "turn": envelope["turn"],
            "state": envelope["state"], "stop_reason": envelope["stop_reason"]}


def native_disposition(run, envelope, facts=None):
    """Packet §5: C1 disposition must agree with its one exactly correlated native terminal."""
    expected = {"completed": "completed", "cancelled": "interrupted", "failed": "failed"}.get(envelope["state"])
    terminals = [fact for fact in turn_facts(run, envelope, facts) if fact["kind"] == "terminal"]
    run.check("native terminal agrees with envelope", expected is not None and len(terminals) == 1
              and terminals[0].get("status") == expected)
    if envelope["state"] == "cancelled":
        run.check("native interrupted acknowledgement", envelope["stop_reason"] == "interrupted"
                  and envelope["cancel"] is not None and envelope["cancel"]["outcome"] == "acknowledged")


def reservation_accounting(run, facts):
    """Packet §7: reservations, receipts, envelopes and native starts are a bijection."""
    snapshot = run.reservations.snapshot()
    rows = snapshot["rows"]
    envelopes = [envelope_identity(envelope) for envelope in run.envelopes]
    requests = [fact for fact in facts if fact["kind"] == "request"
                and fact["method"] in ("thread/start", "thread/resume", "turn/start")]
    replies = [fact for fact in facts if fact["kind"] == "reply"
               and fact["method"] in ("thread/start", "thread/resume", "turn/start")]
    run.check("reservation receipt envelope count", not snapshot["closed"]
              and len(rows) == run.spend.used == len(run.receipts) == len(envelopes)
              and set(rows) == {digest(label) for label in run.receipts}
              and len({identity["address"] for identity in envelopes}) == len(envelopes))
    children = [row for row in rows.values() if row.get("reask_of") is not None]
    run.check("re-ask count exact", len(children) == run.spend.reasks <= REASK_LIMIT)
    for key, row in rows.items():
        if row.get("miss"):
            run.check("each pure miss re-asked once", sum(child["reask_of"] == key for child in children) == 1)
        if row.get("reask_of") is not None:
            parent = rows.get(row["reask_of"])
            run.check("re-ask parent identity exact", parent is not None and parent.get("miss") is True
                      and parent.get("reask_of") is None and not row.get("miss")
                      and all(parent[field] == row[field] for field in ("thread", "prompt", "cwd", "tool")))
    expected_requests, expected_replies = set(), set()
    for label, (receipt, session) in run.receipts.items():
        key, row = digest(label), rows[digest(label)]
        identity = row["envelope"]
        run.check("reservation settled with exact receipt", row["active"] is False
                  and identity is not None and identity in envelopes and row["receipt"] == {
                      "session": digest(session), "address": digest(receipt["turn"])}
                  and identity["session"] == digest(session)
                  and identity["address"] == digest(receipt["turn"])
                  and identity["thread"] == row["thread"])
        envelope = next(envelope for envelope in run.envelopes if envelope_identity(envelope) == identity)
        if row.get("tool") is not None:
            spec = run.command_specs.get(label)
            run.check("command reservation matches " + label,
                      row["tool"]["argv"] == (digest(spec["argv"]) if spec else None))
            if row.get("miss"):
                run.check("final pure tool miss " + label, pure_tool_miss(tool_miss_facts(run, envelope, facts)))
            else:
                run.command_protocol(label, envelope, finished=envelope["state"] != "cancelled", facts=facts)
                if label in run.command_specs and envelope["state"] != "cancelled":
                    run.tool_reply(label, envelope, facts)
        paired = [fact for fact in turn_facts(run, envelope, facts)
                  if fact["kind"] == "reply" and fact.get("method") == "turn/start"]
        run.check("receipt native turn exact", len(paired) == 1 and paired[0].get("reservation") == key)
        for field in ("thread_start", "turn_start"):
            claim = row[field]
            run.check("reservation native turn present", field != "turn_start" or claim is not None)
            if claim is None:
                continue
            token = (key, claim["generation"], claim["request"], claim["method"])
            expected_requests.add(token)
            expected_replies.add(token)
            matched = [fact for fact in replies if (fact.get("reservation"), fact.get("trace"),
                       fact.get("request"), fact["method"]) == token]
            run.check("reservation native reply exact", len(matched) == 1
                      and matched[0]["thread"] == identity["thread"]
                      and (field != "turn_start" or matched[0]["turn"] == claim.get("turn")))
    def tokens(values):
        return [(fact.get("reservation"), fact.get("trace"), fact.get("request"), fact["method"])
                for fact in values]
    request_tokens, reply_tokens = tokens(requests), tokens(replies)
    run.check("no unmatched native starts", len(request_tokens) == len(expected_requests)
              and len(reply_tokens) == len(expected_replies)
              and set(request_tokens) == expected_requests and set(reply_tokens) == expected_replies)
    native_ids = [(row["turn_start"]["generation"], row["thread"], row["turn_start"].get("turn"))
                  for row in rows.values()]
    run.check("native turn identities unique", len(set(native_ids)) == len(native_ids))
    return snapshot


def workspace_marker(path, evidence=None):
    """Runtime §6.1: bounded dirfd proof; record a content-free reason even on failure."""
    directory = None
    fact = {"path_hash": digest(str(path)), "status": "unsafe", "errno": None}
    try:
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        before = os.stat(path.name, dir_fd=directory, follow_symlinks=False)
        require(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid()
                and before.st_nlink == 1, "workspace marker unverifiable")
        fd = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        with os.fdopen(fd, "rb") as file:
            after = os.fstat(file.fileno())
            require((before.st_dev, before.st_ino) == (after.st_dev, after.st_ino)
                    and after.st_nlink == 1, "workspace marker unverifiable")
            verified = file.read(len(WORKSPACE_MARKER) + 1) == WORKSPACE_MARKER
            fact["status"] = "verified" if verified else "content_mismatch"
            return verified
    except OSError as error:
        fact.update(status="absent" if isinstance(error, FileNotFoundError) else "unreadable",
                    errno=error.errno)
        raise Blocked("workspace marker unverifiable") from None
    finally:
        if directory is not None:
            os.close(directory)
        if evidence is not None:
            save(evidence, fact)


def denied_write_observation(run, envelope, command, target):
    """Packet §3: absent protocol tool evidence cannot qualify bound enforcement."""
    tools = [fact for fact in turn_facts(run, envelope) if fact["kind"] == "tool" and fact["finished"]]
    try:
        target.lstat()
    except FileNotFoundError:
        absent = True
    except OSError:
        raise Blocked("prohibited file absence unverifiable") from None
    else:
        absent = False
    require(absent, "prohibited write created a file")
    proven = (len(tools) == 1 and tools[0]["command_hash"] == digest(command)
              and tools[0]["read_only_error"] and type(tools[0]["exit_code"]) is int
              and tools[0]["exit_code"] != 0)
    run.record_only.append({"observation": "denied write", "file_absent": absent,
                            "exact_command_refusal_visible": proven})


def conversation(run):
    """Packet §8: schema set/change/clear, tightened bound, real retirement/resume."""
    ws = run.work / "conversation"
    ws.mkdir(mode=0o700)
    schema_a, schema_b, schema_null = (run.work / name for name in ("a.json", "b.json", "null.json"))
    for path, field in ((schema_a, "a"), (schema_b, "b")):
        path.write_text(json.dumps({"type": "object", "properties": {field: {"type": "string"}},
                                   "required": [field], "additionalProperties": False}))
    schema_null.write_text("null\n")
    allowed = run.fixed_command("c1", "allowed")
    c1, session, _ = run.start_tool("c1", "spawn", *spawn_args(run, ws, fixed_prompt(allowed), schema=schema_a),
                        target=ws / "allowed.txt", bound="workspaceWrite", refusal=False)
    first = run.finish(c1)
    completed(run, first, c1)
    run.command_protocol(c1, first)
    nonce_hash = run.tool_reply(c1, first)
    run.check("schema set", first["structured_output"] == json.loads(first["final_text"]))
    run.check("workspace write succeeded", workspace_marker(ws / "allowed.txt",
                                                          run.evidence / "workspace-marker.json"))
    prohibited = run.fixed_command("c2", "denied")
    c2, _, observed = run.start_tool("c2", "resume", session, "--bound", "read_only", "--output-schema", str(schema_b),
               "--wall-ms", str(WALL_S * 1000), "--prompt",
               fixed_prompt(prohibited), session=session, target=ws / "denied.txt", bound="readOnly", refusal=True)
    run.check("changed-bound prohibited execution refused", observed is not None
              and observed.get("errno") == "EROFS" and observed.get("file_absent") is True)
    second = run.finish(c2)
    completed(run, second, c2)
    run.command_protocol(c2, second)
    run.tool_reply(c2, second)
    run.check("schema replaced", second["structured_output"] == json.loads(second["final_text"]))
    thread = digest(second["vendor_session_id"])
    current_starts = [fact for fact in run.facts() if fact["kind"] == "request"
                      and fact["method"] == "turn/start" and fact["thread"] == thread]
    expected_bounds = (["workspaceWrite"] * (1 + int(c1 != "c1"))
                       + ["readOnly"] * (1 + int(c2 != "c2")))
    run.check("bound change sent between turns", [fact["bound"] for fact in current_starts] == expected_bounds)
    denied_write_observation(run, second, prohibited, ws / "denied.txt")
    run.stop_daemon()
    run.facts(final=True)
    run.start("stored-resume")
    run.submit("c3", "resume", session, "--output-schema", str(schema_null), "--wall-ms",
               str(WALL_S * 1000), "--prompt",
               "Use no tools. Reply only with the VIAQUAL string from the first tool's output in this conversation.",
               session=session)
    third = run.finish("c3")
    completed(run, third, "c3")
    run.check("schema cleared", third["structured_output"] is None and digest(third["final_text"]) == nonce_hash)
    run.check("recall used no tools", not any(fact["kind"] == "tool" for fact in turn_facts(run, third)))
    run.check("stored thread identity", first["vendor_session_id"] == third["vendor_session_id"]
              and isinstance(first["vendor_session_id"], str))
    facts = run.facts()
    run.check("real stored resume with excluded turns", any(fact["kind"] == "request"
              and fact["method"] == "thread/resume" and fact["thread"] == thread
              and fact["exclude_turns"] and fact["bound"] == "read-only" for fact in facts))
    starts = [fact for fact in facts if fact["kind"] == "request" and fact["method"] == "turn/start"
              and fact["thread"] == thread]
    run.check("schema null sent", starts[-1]["schema"] == digest(None))
    run.check("schema set and replacement sent", [fact["schema"] for fact in starts[:-1]] ==
              [digest(json.loads(schema_a.read_text()))] * (1 + int(c1 != "c1"))
              + [digest(json.loads(schema_b.read_text()))] * (1 + int(c2 != "c2")))
    run.check("current bound sent", starts[-1]["bound"] == "readOnly")


def interrupt(run):
    """Packet §§5/8: protocol cancel, surviving tools, interleaved isolation."""
    ws = run.work / "interrupt"
    ws.mkdir(mode=0o700)
    command_a = run.fixed_command("a1", "interrupt-a")
    a1, a, tool = run.start_tool("a1", "spawn", *spawn_args(run, ws, fixed_prompt(command_a)),
                               target=ws / "unused-a", bound="workspaceWrite", refusal=False)
    command_b = run.fixed_command("b1", "interrupt-b")
    b1, b, _ = run.start_tool("b1", "spawn", *spawn_args(run, ws, fixed_prompt(command_b)),
                            target=ws / "unused-b", bound="workspaceWrite", refusal=False)
    # B must actually be accepted/running before A's interrupt, not merely queued.
    by = time.monotonic() + 20
    while True:
        require(time.monotonic() < by, "B acceptance deadline")
        _, status, _ = run.via_call(run.evidence, "b-status", "status", b, "--json")
        if status["vendor_identity_verified"] and status["progress"] is not None \
                and status["progress"]["running_tools"]:
            break
        if shared.INTERRUPTED:
            break  # accepted turns still go through cancellation/settlement below
        time.sleep(0.2)
    rc, cancel, _ = run.via_call(run.evidence, "cancel-a", "cancel", a, "--wait", "--json",
                                timeout=WALL_S + 65, handle=run.handles[a])
    run.check("cancel through VIA", rc == 0 and cancel["cancel"] is not None)
    ended = run.finish(a1)
    run.check("actual interrupted acknowledgement", ended["state"] == "cancelled"
              and ended["stop_reason"] == "interrupted" and ended["cancel"] is not None
              and ended["cancel"]["outcome"] == "acknowledged")
    native_disposition(run, ended)
    run.command_protocol(a1, ended, finished=False)
    cleanup = ended["cancel"]["cleanup"]
    run.check("cleanup packet disposition", cleanup in ("quiescent", "uncertain"))
    fd, current = Proc().open(tool["pid"], tool["start_ticks"])
    if fd is not None:
        os.close(fd)
    run.check("quiescent tool absence proved", cleanup != "quiescent"
              or current is None or current["state"] == "Z")
    _, status, _ = run.via_call(run.evidence, "b-after-cancel", "status", b, "--json")
    run.check("B continues after A cancellation", status["turns"][-1]["state"] == "running")
    other = run.finish(b1)
    completed(run, other, b1)
    run.command_protocol(b1, other)
    run.tool_reply(b1, other)
    run.check("B result isolated", other["vendor_session_id"] != ended["vendor_session_id"])
    a_thread, b_thread = digest(ended["vendor_session_id"]), digest(other["vendor_session_id"])
    facts = run.facts()
    a_traces = {fact["trace"] for fact in turn_facts(run, ended)
                if fact["kind"] == "reply" and fact["method"] == "turn/start"}
    b_traces = {fact["trace"] for fact in turn_facts(run, other)
                if fact["kind"] == "reply" and fact["method"] == "turn/start"}
    run.check("shared owned server", len(a_traces) == 1 and a_traces == b_traces)
    run.check("interrupt targets A", any(fact["kind"] == "request"
              and fact["method"] == "turn/interrupt" and fact["thread"] == a_thread
              for fact in facts))
    run.check("B terminal on B", any(fact["kind"] == "terminal" and fact["thread"] == b_thread
              and fact["status"] == "completed" for fact in facts))
    types = [event["type"] for event in run.events(run.receipts["a1"][0]["turn"])]
    expected = ["cancel.requested", "cancel.settled", "turn.ended"]
    run.check("cancel events ordered", all(kind in types for kind in expected)
              and [types.index(kind) for kind in expected] == sorted(types.index(kind) for kind in expected))
    run.submit("a2", "resume", a, "--wall-ms", str(WALL_S * 1000), "--prompt",
               "Use no tools. Reply with only A_AFTER.", session=a)
    after = run.finish("a2")
    completed(run, after, "a2")
    run.check("post-interrupt resume", after["final_text"].strip() == "A_AFTER"
              and after["vendor_session_id"] == ended["vendor_session_id"])


def never_ask(run):
    """Packet §§3/4: reviewer/never and exact refused-write gates; no-grant paths deferred."""
    ws = run.work / "never-ask"
    ws.mkdir(mode=0o700)
    command = run.fixed_command("n1", "never-ask")
    n1, session, observed = run.start_tool("n1", "spawn", *spawn_args(run, ws, fixed_prompt(command), bound="read_only"),
                                 target=ws / "forbidden.txt", bound="readOnly", refusal=True)
    run.check("never-ask prohibited execution refused", observed.get("errno") == "EROFS"
              and observed.get("file_absent") is True)
    ended = run.finish(n1)
    completed(run, ended, n1)
    run.command_protocol(n1, ended)
    run.tool_reply(n1, ended)
    thread = digest(ended["vendor_session_id"])
    facts = run.facts()
    run.check("never/user reviewer verified", any(fact["kind"] == "reply"
              and fact["method"] == "thread/start" and fact["thread"] == thread
              and fact["never"] and fact["reviewer_user"] for fact in facts))
    denied_write_observation(run, ended, command, ws / "forbidden.txt")


def usage(run):
    """Packet §7: whole keyless last samples vs envelopes; no cost estimation."""
    facts = run.facts(final=True)
    reservations = reservation_accounting(run, facts)
    for envelope in run.envelopes:
        # Match both the paired native turn and its submitting generation.
        # Restored reports stay owned without becoming fresh model-call usage.
        matched = turn_facts(run, envelope, facts)
        samples = [fact for fact in matched if fact["kind"] == "usage"]
        native_disposition(run, envelope, facts)
        run.check("cost unavailable", envelope["cost"] ==
                  {"usd": None, "scope": "turn", "provenance": "unavailable"})
        run.check("usage sample observed", bool(samples))
        run.check("usage reported turn", envelope["usage"]["scope"] == "turn"
                  and envelope["usage"]["provenance"] == "reported")
        keys = {"input_tokens": "inputTokens", "cached_input_tokens": "cachedInputTokens",
                "output_tokens": "outputTokens", "reasoning_output_tokens": "reasoningOutputTokens",
                "total_tokens": "totalTokens"}
        for public, vendor in keys.items():
            run.check("keyless last sum " + public, envelope["usage"][public]
                      == sum(sample["last"][vendor] for sample in samples))
        conform(envelope["vendor"]["total"], "sample", "vendor total")
        run.check("thread total stays vendor", all(envelope["vendor"]["total"][key]
                  == samples[-1]["total"][key] for key in SCHEMAS["sample"]))
    save(run.evidence / "reservations.json", reservations)
    save(run.evidence / "accounting.json", [{"session": digest(envelope["session_id"]),
         "thread": digest(envelope["vendor_session_id"]), "turn": envelope["turn"],
         "state": envelope["state"], "vendor_version": envelope["vendor_version"],
         "cancel": None if envelope["cancel"] is None else
                   {key: envelope["cancel"][key] for key in SCHEMAS["cancel"]},
         "usage": {key: envelope["usage"][key] for key in SCHEMAS["usage"]},
         "cost": {key: envelope["cost"][key] for key in SCHEMAS["cost"]},
         "thread_total": {key: envelope["vendor"]["total"][key] for key in SCHEMAS["sample"]}}
         for envelope in run.envelopes])


def execute(run):
    """No live case executes without every preflight latch (packet §8)."""
    run.preflight()
    run.start("initial")
    conversation(run)
    shared.interrupt_guard()
    interrupt(run)
    shared.interrupt_guard()
    never_ask(run)
    run.stop_daemon()
    usage(run)
    run.check("exact structural turn plan", run.spend.used == BASE_TURN_LIMIT + run.spend.reasks
              and run.spend.reasks <= REASK_LIMIT)


def cleanup(run):
    """Always attempt private daemon stop; uncertainty never becomes absence."""
    errors = []
    # A deferred signal may leave either accepted turn outstanding. Cancel
    # mutation requests remain authorized; no later submission is made.
    for label in sorted(run.spend.active):
        if label not in run.receipts:
            continue
        _, session = run.receipts[label]
        try:
            run.via_call(run.evidence, "cleanup-cancel", "cancel", session, "--json",
                         timeout=15, handle=run.handles[session])
        except BaseException:
            errors.append("cancel unavailable")
    try:
        run.facts()
    except BaseException:
        errors.append("trace inspection unavailable")
    try:
        run.stop_daemon(final=True)
    except BaseException:
        errors.append("daemon stop unverified")
    stopped = run.daemon is None and run.daemon_unverified is None and bool(run.daemons)
    stopped = stopped and all(record.get("stopped") is True for record in run.daemons)
    # Independent descriptor identity proof for every proxy/tool we observed.
    for pid, ticks in run.owned:
        try:
            fd, ident = Proc().open(pid, ticks)
            if fd is not None:
                os.close(fd)
            if ident is not None and ident["state"] != "Z":
                stopped = False
        except BaseException:
            stopped = False
    try:
        facts = run.facts(final=True)
        save(run.evidence / "protocol.json", facts)
        counts = {method: 0 for method in NO_GRANT_METHODS}
        unknown = {}
        for fact in facts:
            if fact["kind"] == "decline":
                known = next((method for method in NO_GRANT_METHODS
                              if digest(method) == fact["method_hash"]), None)
                if known is None:
                    key = fact["method_hash"]
                    unknown[key] = unknown.get(key, 0) + 1
                else:
                    counts[known] += 1
        run.record_only.append({"observation": "no-grant paths", "per_method_counts": counts,
                                "unknown_method_digest_counts": unknown})
    except BaseException:
        errors.append("complete protocol evidence unavailable")
        try:
            partial = run.facts()
            run.blocked_reasons.extend(row["blocked_reason"] for row in partial
                                       if row.get("blocked_reason") is not None)
            save(run.evidence / "protocol.json", {"complete": False, "facts": partial})
        except BaseException:
            errors.append("partial protocol evidence unavailable")
    try:
        save(run.evidence / "lifecycle.json", [{"phase": record["phase"],
             "pid": record.get("pid"), "start_ticks": record.get("start_ticks"),
             "stopped": record.get("stopped") is True,
             "descendants_watched": record.get("descendants_watched", []),
             "recorded": record.get("recorded", []),
             "no_living_recorded": record.get("recorded_alive") == [],
             "no_living_descendants": record.get("descendants_left") == [],
             "locks_clear": record.get("locks_held") == [],
             "no_uncertainty": record.get("undecided") == []} for record in run.daemons])
    except BaseException:
        errors.append("lifecycle evidence unavailable")
    if run.owner_before is not None:
        try:
            run.owner_after = owner_metadata(run.owner_codex, run.metadata_day)
        except BaseException:
            errors.append("owner home metadata after run unavailable")
    return stopped, errors


def package_secret_free(directory):
    """Boolean complete scan of our small allowlisted JSON package; no matches printed."""
    entries = list(directory.iterdir())
    if len(entries) > 32:
        return False
    for path in entries:
        if path.is_symlink():
            return False
        if path.is_dir():  # shared lifecycle's directory holds only generated config facts
            children = list(path.iterdir())
            if len(children) > 32:
                return False
            for child in children:
                if child.suffix != ".json" or child.is_symlink() or child.stat().st_size > TRACE_BYTES:
                    return False
                if not secret_free(json.loads(child.read_text())):
                    return False
        elif path.suffix != ".json" or path.is_symlink() or path.stat().st_size > TRACE_BYTES:
            return False
        elif not secret_free(json.loads(path.read_text())):
            return False
    return True


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--run", action="store_true", help="explicit opt-in after review")
    parser.add_argument("--proxy", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--via", type=Path)
    parser.add_argument("--codex", type=Path)
    parser.add_argument("--via-sha256")
    parser.add_argument("--codex-sha256")
    parser.add_argument("--candidate-version", default="0.160.1")
    parser.add_argument("--model", default=MODEL)
    parser.add_argument("--evidence", type=Path)
    args, vendor_argv = parser.parse_known_args(argv)
    if args.self_test:
        require(not args.run and args.proxy is None and not vendor_argv, "self-test mode only")
        return self_test()
    if args.proxy is not None:
        # Private generated launcher only; control is never an evidence source.
        info = args.proxy.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                and stat.S_IMODE(info.st_mode) == 0o600 and info.st_size <= PROC_BYTES,
                "unsafe proxy control")
        fd = os.open(args.proxy, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd) as file:
            control = json.load(file)
        return asyncio.run(proxy(control, vendor_argv))
    if not args.run or vendor_argv or not all((args.via, args.codex, args.via_sha256,
                                             args.codex_sha256, args.evidence)):
        parser.error("explicit --run, binaries, pinned hashes and evidence are required")
    require(args.model == MODEL, "model must be gpt-6-luna")
    root = Path(__file__).resolve().parents[2] / "scratchpad"
    require(not root.is_symlink(), "scratchpad must stay in the worktree")
    require(args.evidence.resolve().is_relative_to(root.resolve()), "evidence must be in this scratchpad")
    require(not args.evidence.exists(), "evidence must be new")
    args.evidence.mkdir(parents=True, mode=0o700)
    os.chmod(args.evidence, 0o700)
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, shared.record_signal)
    run, failure, stopped, cleanup_errors, reason = None, None, False, [], None
    try:
        run = Run(args)
        execute(run)
    except BaseException as error:
        # Never echo a possibly secret-bearing exception, path or vendor reply.
        failure = type(error).__name__
        reason = str(error) if isinstance(error, Blocked) and secret_free(str(error)) else failure
    finally:
        if run is not None:
            try:
                stopped, cleanup_errors = cleanup(run)
            except BaseException:
                cleanup_errors.append("cleanup unverified")
    interrupted = bool(shared.INTERRUPTED)
    passed = (run is not None and failure is None and stopped and not cleanup_errors
              and not interrupted and not run.spend.active and not run.spend.uncertain
              and bool(run.checks) and all(item["pass"] for item in run.checks))
    removed = False
    if run is not None and stopped:
        try:
            shutil.rmtree(run.work)
            removed = not run.work.exists()
        except BaseException:
            cleanup_errors.append("private directory removal failed")
    passed = passed and removed
    summary = {"runner": "scripts/qualify/codex.py", "result": "pass" if passed else "blocked",
               "candidate_version": args.candidate_version, "model": MODEL,
               "error_type": failure, "blocked_reasons": ([reason] if reason else [])
                   + (run.blocked_reasons if run else []), "interrupted": interrupted, "daemons_stopped": stopped,
               "private_directory_removed": removed, "cleanup_errors": cleanup_errors,
               "turns_reserved": run.spend.used if run else 0,
               "reasks_reserved": run.spend.reasks if run else 0,
               "turn_limits": {"primary": BASE_TURN_LIMIT, "reasks": REASK_LIMIT,
                               "per_case_reasks": CASE_REASK_LIMIT, "total": TURN_LIMIT,
                               "concurrent": ACTIVE_LIMIT},
               "spending_uncertain": run.spend.uncertain if run else True,
               "checks": run.checks if run else [],
               "owner_home_writes": {"policy": "accepted, not minimised",
                   "before": run.owner_before if run else None,
                   "after": run.owner_after if run else None,
                   "new_session_files": len(set(run.owner_after["session_digests"])
                       - set(run.owner_before["session_digests"]))
                       if run and run.owner_before is not None and run.owner_after is not None else None},
               "qualification_limits": {"owner_mcp_servers_disabled": True, "plugins_disabled": True,
                   "checked_set_scope": "test isolation; owner MCP/plugin configuration excluded"},
               "record_only_scope": ["completed pure tool misses before a labelled re-ask",
                                     "never-ask denied-write command visibility",
                                     "six independent inducible no-grant paths"],
               "record_only": run.record_only if run else [],
               "deferred": ["six independent live no-grant paths when not inducible",
                            "usage scope upgrade", "steer/rejoin", "macOS/broader bound matrix"]}
    # Preflight failures may leave no daemon but still only private generated
    # files. Remove that root only if daemon startup was never attempted.
    if run is not None and not run.daemons:
        try:
            shutil.rmtree(run.work)
            summary["private_directory_removed"] = not run.work.exists()
        except BaseException:
            summary["cleanup_errors"].append("preflight directory removal failed")
    try:
        scanned = package_secret_free(args.evidence)
    except BaseException:
        scanned = False
    summary["secret_free"] = scanned
    if not scanned:
        summary["result"] = "blocked"
    def observe_interruption():
        if shared.INTERRUPTED:
            summary["interrupted"] = True
            summary["result"] = "blocked"
    observe_interruption()
    try:
        pending_summary = args.evidence / "summary.pending.json"
        save(pending_summary, summary)
        if shared.INTERRUPTED and not summary["interrupted"]:
            observe_interruption()
            pending_summary.unlink()
            save(pending_summary, summary)
        os.replace(pending_summary, args.evidence / "summary.json")
        if shared.INTERRUPTED and not summary["interrupted"]:
            observe_interruption()
            save(pending_summary, summary)
            os.replace(pending_summary, args.evidence / "summary.json")
    except BaseException:
        # Disk/permission/privacy failures cannot produce a qualification.
        # Still emit a fixed, secret-free terminal result; never the exception.
        print(json.dumps({"result": "blocked", "secret_free": False,
                          "daemons_stopped": stopped, "summary_written": False}))
        return 2
    print(json.dumps({"result": summary["result"], "secret_free": scanned,
                      "daemons_stopped": stopped, "turns_reserved": summary["turns_reserved"]}))
    return 0 if summary["result"] == "pass" else 2


def fake_envelope(state="completed"):
    """Synthetic C1 fixture; no vendor or credential access."""
    return {"session_id": "s_fake", "turn": 1, "state": state, "failure": None,
            "stop_reason": "end_turn", "vendor_version": "0.160.1",
            "version_status": "untested", "vendor_session_id": "thread-fake",
            "route": "codex-app-server", "final_text": "DONE", "structured_output": None,
            "steps": None, "cancel": None, "cost": {"usd": None, "scope": "turn",
            "provenance": "unavailable"}, "usage": {"scope": "turn", "provenance": "reported",
            "input_tokens": 3, "cached_input_tokens": 1, "output_tokens": 2,
            "reasoning_output_tokens": 1, "total_tokens": 5}, "vendor": {},
            "warnings": [], "denied_actions": [], "denied_actions_total": 0,
            "auto_declined_requests_total": 0}


class SafetyTests(unittest.TestCase):
    """Synthetic /proc, fake CLI replies and mutation checks; never live."""
    def setUp(self):
        shared.INTERRUPTED.clear()
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self):
        shared.INTERRUPTED.clear()
        self.temp.cleanup()

    def process(self, pid, parent, ticks, state="S"):
        path = self.root / str(pid)
        path.mkdir(exist_ok=True)
        fields = [state, str(parent)] + ["0"] * 17 + [str(ticks)]
        (path / "stat").write_text(f"{pid} (fake with spaces) " + " ".join(fields))
        (path / "cmdline").write_bytes(b"fake\0safe\0")
        return {"pid": pid, "start_ticks": ticks}

    def run_object(self):
        via, codex = self.root / "via", self.root / "codex"
        via.write_text("fake via")
        codex.write_bytes(b"\x7fELFsynthetic codex")
        codex.chmod(0o700)
        evidence = self.root / "evidence"
        evidence.mkdir(exist_ok=True)
        args = argparse.Namespace(via=via, codex=codex, model=MODEL, evidence=evidence,
                                  via_sha256=shared.sha256(via), codex_sha256=shared.sha256(codex),
                                  candidate_version="0.160.1")
        with mock.patch.dict(os.environ):
            os.environ.pop("CODEX_HOME", None)
            run = Run(args)
        self.addCleanup(shutil.rmtree, run.work, True)
        return run

    def start_message(self, model=MODEL):
        return {"id": 1, "method": "turn/start", "params": {"threadId": "t", "model": model,
                "effort": "low", "approvalPolicy": "never", "approvalsReviewer": "user",
                "cwd": "/synthetic/work", "input": [{"type": "text", "text": "synthetic prompt"}],
                "outputSchema": None, "sandboxPolicy": {"type": "readOnly"}}}

    def owned_wire(self):
        wire = Wire("0.160.1")
        wire.inventory = True
        wire.outgoing({"id": 10, "method": "thread/start", "params": {
            "model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
            "sandbox": "read-only"}})
        wire.incoming({"id": 10, "result": {"model": MODEL, "approvalPolicy": "never",
                      "approvalsReviewer": "user", "thread": {"id": "t"}}})
        wire.outgoing(self.start_message())
        wire.incoming({"id": 1, "result": {"turn": {"id": "u"}}})
        return wire

    def seed_accounting(self, run, envelopes, facts):
        """Independent synthetic receipts/claims; negative fixtures mutate after seeding."""
        seen_threads = set()
        for index, envelope in enumerate(envelopes):
            label, key = f"fixture-{index}", digest(f"fixture-{index}")
            thread = digest(envelope["vendor_session_id"])
            starts = [fact for fact in facts if fact["kind"] == "reply"
                      and fact.get("method") == "turn/start" and fact["thread"] == thread]
            start = starts[envelope["turn"] - 1]
            generation = start.get("trace", digest("fixture-generation"))
            cwd, prompt = digest("cwd"), digest(index)
            run.spend.reserve(label, MODEL)
            run.reservations.reserve(key, "resume" if thread in seen_threads else "spawn", prompt, cwd,
                                     thread if thread in seen_threads else None)
            if thread not in seen_threads:
                attach = digest((index, "attach"))
                run.reservations.claim("thread/start", generation, attach, cwd)
                run.reservations.reply(key, "thread/start", generation, attach, thread)
                facts.extend([{"kind": "request", "method": "thread/start", "reservation": key,
                               "trace": generation, "request": attach},
                              {"kind": "reply", "method": "thread/start", "reservation": key,
                               "trace": generation, "request": attach, "thread": thread}])
                seen_threads.add(thread)
            request = digest((index, "start"))
            run.reservations.claim("turn/start", generation, request, cwd, thread, prompt)
            run.reservations.reply(key, "turn/start", generation, request, thread, start["turn"])
            start.update(reservation=key, request=request, trace=generation)
            facts.append({"kind": "request", "method": "turn/start", "reservation": key,
                          "trace": generation, "request": request, "thread": thread})
            for fact in facts:
                if fact.get("thread") == thread and fact.get("turn") == start["turn"]:
                    fact.setdefault("trace", generation)
                    if fact["kind"] == "terminal":
                        fact.setdefault("status", "completed")
            address = f"{envelope['session_id']}/{envelope['turn']}"
            run.receipts[label] = ({"turn": address}, envelope["session_id"])
            run.reservations.receipt(key, {"session": digest(envelope["session_id"]), "address": digest(address)})
            run.reservations.settle(key, envelope_identity(envelope))
            run.spend.settle(label, envelope)

    def test_recorded_fixture_server_lines(self):
        # Replay every emitted line, pairing IDs from recorded client captures.
        # Historical models differ from Luna: only that policy field is adapted.
        # Recorded negative RPC replies remain expected refusals, never passes.
        fixtures = Path(__file__).resolve().parents[2] / "crates/via-adapters/tests/fixtures/codex"
        streams = messages = notifications = refusals = 0
        paths = sorted(fixtures.glob("*.replay.json"))
        self.assertTrue(paths)
        for path in paths:
            wire, count, variables = Wire("0.159.2"), 0, {}
            for step in json.loads(path.read_text())["steps"]:
                expected = step.get("expect", {})
                for variable, pointer in expected.get("capture", {}).items():
                    self.assertEqual(pointer, "/id")
                    variables[variable] = len(variables) + 1
                    request = expected["line"]
                    params = request.get("params", {})
                    fact = {"method": request["method"]}
                    if "threadId" in params:
                        fact["thread"] = digest(params["threadId"])
                    wire.pending[digest(variables[variable])] = fact
                raw = step.get("emit", {}).get("line")
                if not raw:
                    continue
                frame = json.loads(re.sub(r"\$\{([^}]+)\}", lambda m: str(variables[m[1]]), raw)) \
                    if type(raw) is str else raw
                with self.subTest(fixture=path.name, message=count):
                    if "id" in frame and "error" in frame and "method" not in frame:
                        with self.assertRaisesRegex(Blocked, "request refused"):
                            wire.incoming(frame)
                        refusals += 1
                    elif "id" in frame and wire.pending.get(digest(frame["id"]), {}).get("method") == "turn/steer":
                        # Steer is explicitly outside the first-release scope.
                        with self.assertRaisesRegex(Blocked, "method not qualified"):
                            wire.incoming(frame)
                        refusals += 1
                    else:
                        result = frame.get("result", {})
                        with mock.patch(__name__ + ".MODEL", result.get("model", MODEL)):
                            wire.incoming(frame)
                count += 1
                notifications += "method" in frame
            streams += bool(count)
            messages += count
        self.assertEqual(streams, 14)
        self.assertEqual(notifications, 240)
        self.assertEqual(refusals, 3)
        self.assertGreater(messages, notifications)

    def test_owned_thread_and_turn_notifications(self):
        frames = [
            {"method": "thread/started", "params": {"thread": {"id": "t"}}},
            {"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "u"}}},
            {"method": "item/started", "params": {"threadId": "t", "turnId": "u",
                                                   "item": {"type": "userMessage"}}},
        ]
        for frame in frames:
            self.owned_wire().incoming(frame)
            bad = copy.deepcopy(frame)
            if "threadId" in bad["params"]:
                bad["params"]["threadId"] = "foreign"
            else:
                bad["params"]["thread"]["id"] = "foreign"
            with self.subTest(method=bad["method"]), self.assertRaisesRegex(Blocked, "unowned"):
                self.owned_wire().incoming(bad)
        for method, params in (
            ("item/completed", {"threadId": "t", "turnId": "foreign", "item": {"type": "reasoning"}}),
            ("turn/completed", {"threadId": "t", "turn": {"id": "foreign", "status": "completed"}}),
            ("thread/tokenUsage/updated", {"threadId": "t", "turnId": "foreign",
                "tokenUsage": {key: {"inputTokens": 1, "cachedInputTokens": 1, "outputTokens": 1,
                                     "reasoningOutputTokens": 1, "totalTokens": 2}
                               for key in ("last", "total")}}),
        ):
            with self.subTest(method=method), self.assertRaisesRegex(Blocked, "unowned"):
                self.owned_wire().incoming({"method": method, "params": params})

    def test_safe_owned_argv_and_name_validation(self):
        self.assertEqual(server_args(["ok-name_1"]), ["--disable", "apps", "--disable", "plugins",
                         "-c", "notify=[]", "-c", "mcp_servers.ok-name_1.enabled=false"])
        for name in ('a.b', '"quoted"', '', 'space name', 'a=b'):
            with self.subTest(name=name), self.assertRaisesRegex(Blocked, "MCP name"):
                server_args([name])

    def test_plugins_and_profile_inventory(self):
        (self.root / "plugins").mkdir()
        config = self.root / "config.toml"
        config.write_text('[plugins.example]\nenabled=true\n[mcp_servers.one]\ncommand="fake"\n'
                          '[profiles.test.mcp_servers.two]\ncommand="fake"\n')
        self.assertEqual(mcp_names(self.root), ["one"])
        for key in ('model_provider="foreign"', 'profile="other"', '[model_providers.other]', '[otel]'):
            config.write_text(key + "\n")
            with self.subTest(key=key), self.assertRaisesRegex(Blocked, "side-effect configuration"):
                mcp_names(self.root)

    def test_remote_control_must_be_disabled(self):
        for status in ('enabled', None, False):
            with self.subTest(status=status), self.assertRaisesRegex(Blocked, "remote control"):
                Wire("0.160.1").incoming({"method": "remoteControl/status/changed",
                                        "params": {"status": status}})

    def test_unparsable_command_uses_raw_digest(self):
        self.assertEqual(command_digest("printf 'unclosed"), digest("printf 'unclosed"))

    def test_chatty_deltas_are_coalesced(self):
        wire = self.owned_wire()
        for _ in range(REQUEST_LIMIT + 1):
            wire.incoming({"method": "item/agentMessage/delta", "params": {
                "threadId": "t", "turnId": "u", "delta": "ignored"}})
        self.assertLess(len(wire.facts), 10)
        self.assertEqual(wire.notifications[digest("item/agentMessage/delta")], REQUEST_LIMIT + 1)

    def test_launcher_is_not_a_native_pin(self):
        run = self.run_object()
        run.codex.write_text("#!/usr/bin/env node\n")
        with self.assertRaisesRegex(Blocked, "native executable"):
            pins(run.via, run.codex, shared.sha256(run.via), shared.sha256(run.codex), MODEL)

    def test_codex_home_disagreement_blocks_constructor(self):
        run = self.run_object()
        unexpected = None
        try:
            with mock.patch.dict(os.environ, {"CODEX_HOME": str(self.root / "foreign")}), \
                 self.assertRaisesRegex(Blocked, "CODEX_HOME"):
                unexpected = Run(run.args)
        finally:
            if unexpected is not None:
                shutil.rmtree(unexpected.work)

    def test_constructor_failure_removes_private_root(self):
        run = self.run_object()
        work = self.root / "failed-constructor"
        work.mkdir()
        with mock.patch.object(tempfile, "mkdtemp", return_value=str(work)), \
             mock.patch.object(Path, "mkdir", side_effect=OSError("synthetic")), \
             self.assertRaises(OSError):
            Run(run.args)
        self.assertFalse(work.exists())

    def test_stop_refreshes_reparented_trace_identity(self):
        run = self.run_object()
        run.daemons = [{"descendants_watched": []}]
        def refresh(*args, **kwargs):
            run.owned.add((2, 20))
        with mock.patch.object(run, "facts", side_effect=refresh), \
             mock.patch.object(shared.Run, "stop_daemon") as stop:
            run.stop_daemon()
        self.assertIn((2, 20), run.daemons[0]["descendants_watched"])
        stop.assert_called_once()

    def config_fixture(self):
        return {"config": {"features": {key: False for key in ("apps", "plugins", "memories", "hooks")},
                "notify": [], "model_provider": "openai", "profile": None,
                "model_providers": {}, "otel": None,
                "mcp_servers": {"ok": {"enabled": False}}}}

    def test_config_read_unset_defaults(self):
        for omitted in (False, True):
            candidate = self.config_fixture()
            for key in ("model_provider", "profile", "model_providers"):
                if omitted:
                    candidate["config"].pop(key)
                else:
                    candidate["config"][key] = None
            with self.subTest(omitted=omitted):
                self.assertTrue(effective_config(candidate, ["ok"])["all_servers_disabled"])
        for key in ("model_provider", "profile", "model_providers"):
            for value in (False, [], "foreign", {"foreign": {}}):
                candidate = self.config_fixture()
                candidate["config"][key] = value
                with self.subTest(key=key, value=value), self.assertRaises(Blocked):
                    effective_config(candidate, ["ok"])

    def test_ownership_rejection_leaves_sanitized_fact(self):
        for method, params in (
            ("item/started", {"threadId": "t", "turnId": "foreign", "item": {"type": "agentMessage"}}),
            ("thread/started", {"thread": {"id": "foreign"}}),
            ("thread/tokenUsage/updated", {"threadId": "t"}),
            ("turn/interrupt", {"threadId": "t", "turnId": "foreign"})):
            with self.subTest(method=method):
                wire = self.owned_wire()
                wire.generation = digest("new-generation")
                wire.outgoing({"id": 20, "method": "thread/resume", "params": {
                    "threadId": "other", "model": MODEL, "sandbox": "read-only",
                    "approvalPolicy": "never", "approvalsReviewer": "user", "excludeTurns": True}})
                message = {"method": method, "params": params}
                with self.assertRaises(Blocked):
                    if method == "turn/interrupt":
                        wire.outgoing({**message, "id": 21})
                    else:
                        wire.incoming(message)
                fact = wire.facts[-1]
                self.assertEqual(fact["kind"], "ownership_rejection")
                self.assertEqual(fact["method"], method)
                self.assertEqual(fact["generation"], wire.generation)
                self.assertEqual(fact["thread_owned"], method != "thread/started")
                self.assertFalse(fact["turn_known_earlier"])
                self.assertEqual(fact["pending_establishing"], [{"method": "thread/resume",
                                 "request": digest(20), "thread": digest("other")}])
                self.assertEqual(fact["item_type"], "agentMessage" if method == "item/started" else None)
                self.assertNotIn('"foreign"', json.dumps(fact))
                self.assertTrue(secret_free(fact))

    def test_rejection_never_logs_unknown_protocol_tokens(self):
        wire = self.owned_wire()
        message = {"method": "thread/sk-privatevalue123", "params": {
            "threadId": "sk-privateidentity123", "item": {"type": "sk-privatetype123"}}}
        with self.assertRaises(Blocked):
            wire.incoming(message)
        fact = wire.facts[-1]
        self.assertEqual(fact["kind"], "ownership_rejection")
        self.assertIsNone(fact["method"])
        self.assertIsNone(fact["item_type"])
        self.assertEqual(fact["method_hash"], digest(message["method"]))
        self.assertTrue(secret_free(fact))

    def resumed_wire(self, earlier):
        wire = Wire("0.160.1")
        wire.history = lambda: (earlier.threads, earlier.turns,
            {fact["thread"]: fact["total"] for fact in earlier.facts if fact["kind"] == "usage"})
        wire.generation = digest("second-generation")
        wire.inventory = True
        wire.outgoing({"id": 10, "method": "thread/resume", "params": {
            "threadId": "t", "model": MODEL, "sandbox": "read-only", "approvalPolicy": "never",
            "approvalsReviewer": "user", "excludeTurns": True}})
        wire.incoming({"id": 10, "result": {"model": MODEL, "approvalPolicy": "never",
                      "approvalsReviewer": "user", "thread": {"id": "t"}}})
        return wire

    def test_old_owned_turn_after_resume_passes_unknown_turn_still_blocks(self):
        first = self.owned_wire()
        wire = self.resumed_wire(first)
        sample = {key: 1 for key in SCHEMAS["sample"]}
        notification = {"method": "thread/tokenUsage/updated", "params": {
            "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": sample}}}
        first.incoming(notification)
        self.assertEqual(wire.incoming(notification)["kind"], "replay")
        wire.incoming({"method": "thread/started", "params": {"thread": {"id": "t"}}})
        # Even a pending new turn cannot authorize activity under the old ID.
        wire.outgoing({**self.start_message(), "id": 20})
        for message in (
            {"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "u"}}},
            {"method": "turn/completed", "params": {"threadId": "t", "turn": {
                "id": "u", "status": "completed"}}},
            {"method": "item/started", "params": {"threadId": "t", "turnId": "u",
                "item": {"type": "userMessage", "content": [{"text": "synthetic private text"}]}}},
            {"method": "error", "params": {"threadId": "t", "turnId": "u"}},
            {"id": 99, "method": "item/commandExecution/requestApproval", "params": {
                "threadId": "t", "turnId": "u"}},
            {"id": 100, **notification}):
            with self.subTest(method=message["method"]), self.assertRaises(Blocked):
                wire.incoming(message)
            self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
            self.assertTrue(wire.facts[-1]["turn_known_earlier"])
            self.assertEqual(wire.deferred, [])
        self.assertTrue(secret_free(wire.facts))
        self.assertNotIn("synthetic private text", json.dumps(wire.facts))
        wire.incoming({"id": 20, "result": {"turn": {"id": "new"}}})
        notification["params"]["turnId"] = "never-established"
        with self.assertRaisesRegex(Blocked, "unowned"):
            wire.incoming(notification)
        self.assertFalse(wire.facts[-1]["turn_known_earlier"])
        self.assertEqual(wire.facts[-1]["thread"], digest("t"))
        self.assertEqual(wire.facts[-1]["turn"], digest("never-established"))
        self.assertTrue(secret_free(wire.facts[-1]))

    def test_exact_usage_replay_uses_latest_thread_total(self):
        first = self.owned_wire()
        sample = {key: 1 for key in SCHEMAS["sample"]}
        first.incoming({"method": "thread/tokenUsage/updated", "params": {
            "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": sample}}})
        first.outgoing({**self.start_message(), "id": 2})
        first.incoming({"id": 2, "result": {"turn": {"id": "v"}}})
        latest = {key: 2 for key in SCHEMAS["sample"]}
        first.incoming({"method": "thread/tokenUsage/updated", "params": {
            "threadId": "t", "turnId": "v", "tokenUsage": {"last": sample, "total": latest}}})
        wire = self.resumed_wire(first)
        replay = wire.incoming({"method": "thread/tokenUsage/updated", "params": {
            "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": latest}}})
        self.assertEqual(replay["kind"], "replay")
        self.assertEqual(replay["turn"], digest("u"))
        self.assertEqual(replay["total"], latest)
        self.assertFalse(any(fact["kind"] == "usage" for fact in wire.facts))
        wire.outgoing({**self.start_message(), "id": 2})
        wire.incoming({"id": 2, "result": {"turn": {"id": "new"}}})
        fresh = wire.incoming({"method": "thread/tokenUsage/updated", "params": {
            "threadId": "t", "turnId": "new", "tokenUsage": {"last": sample, "total": latest}}})
        self.assertEqual(fresh["kind"], "usage")

    def test_grown_or_changed_old_turn_total_blocks(self):
        sample = {key: 2 for key in SCHEMAS["sample"]}
        for field in SCHEMAS["sample"]:
            for delta in (-1, 1):
                with self.subTest(field=field, delta=delta):
                    first = self.owned_wire()
                    first.incoming({"method": "thread/tokenUsage/updated", "params": {
                        "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": sample}}})
                    wire = self.resumed_wire(first)
                    changed = {**sample, field: sample[field] + delta}
                    with self.assertRaises(Blocked):
                        wire.incoming({"method": "thread/tokenUsage/updated", "params": {
                            "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": changed}}})
                    self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
                    self.assertTrue(wire.facts[-1]["turn_known_earlier"])
                    self.assertFalse(any(fact["kind"] in ("usage", "replay") for fact in wire.facts))

    def test_old_turn_without_recorded_total_blocks(self):
        wire = self.resumed_wire(self.owned_wire())
        sample = {key: 0 for key in SCHEMAS["sample"]}
        with self.assertRaises(Blocked):
            wire.incoming({"method": "thread/tokenUsage/updated", "params": {
                "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": sample}}})
        self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")

    def test_old_turn_activity_blocks_even_with_new_start_pending(self):
        for message in (
            {"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "u"}}},
            {"method": "turn/completed", "params": {"threadId": "t", "turn": {"id": "u", "status": "completed"}}},
            {"method": "item/started", "params": {"threadId": "t", "turnId": "u", "item": {
                "type": "commandExecution", "command": "printf synthetic", "exitCode": 0}}},
            {"method": "error", "params": {"threadId": "t", "turnId": "u"}},
            {"method": "thread/status/changed", "params": {"threadId": "t", "turnId": "u"}},
            {"id": 98, "method": "mcpServer/elicitation/request", "params": {"threadId": "t", "turnId": "u"}},
            {"id": 99, "method": "item/commandExecution/requestApproval", "params": {"threadId": "t", "turnId": "u"}}):
            with self.subTest(method=message["method"]):
                wire = self.resumed_wire(self.owned_wire())
                wire.outgoing({**self.start_message(), "id": 20})
                with self.assertRaises(Blocked):
                    wire.incoming(message)
                self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
                self.assertEqual(wire.deferred, [])

    def test_rejection_has_room_after_regular_fact_limit(self):
        wire = self.owned_wire()
        with mock.patch(__name__ + ".REQUEST_LIMIT", 10):
            with self.assertRaisesRegex(Blocked, "protocol evidence bound"):
                for _ in range(10):
                    wire.fact(kind="synthetic")
            with self.assertRaisesRegex(Blocked, "unowned"):
                wire.incoming({"method": "error", "params": {"threadId": "foreign"}})
            self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
            wire.fact(kind="notification_counts", counts={})
            wire.fact(kind="end", complete=False, child_reaped=True)
            self.assertLessEqual(len(wire.facts), 10)

    def test_rejection_has_room_after_regular_byte_limit(self):
        path = self.root / "bounded-trace"
        with path.open("wb") as trace:
            sequence = 0
            def sink(value):
                nonlocal sequence
                sequence += 1
                line = json.dumps({"seq": sequence, **value}, sort_keys=True).encode() + b"\n"
                require(trace.tell() + len(line) <= TRACE_BYTES, "protocol evidence byte bound")
                trace.write(line)
            wire = Wire("0.160.1", sink=sink)
            for index in range(REQUEST_LIMIT):
                wire.pending[digest(index)] = {"method": "thread/resume", "thread": digest("t")}
            with mock.patch(__name__ + ".TRACE_BYTES", 2 * 1024 * 1024):
                with self.assertRaisesRegex(Blocked, "protocol evidence byte bound"):
                    for _ in range(REQUEST_LIMIT):
                        wire.fact(kind="synthetic", padding="x" * 512)
                with self.assertRaisesRegex(Blocked, "unowned"):
                    wire.incoming({"method": "error", "params": {"threadId": "foreign"}})
                self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
                self.assertEqual(len(wire.facts[-1]["pending_establishing"]), REQUEST_LIMIT)
                wire.fact(kind="notification_counts", counts={digest(index): 1
                          for index in range(NOTIFICATION_METHOD_LIMIT)})
                wire.fact(kind="end", complete=False, child_reaped=True)
                self.assertEqual(trace.tell(), wire.evidence_bytes)
                self.assertLessEqual(trace.tell(), TRACE_BYTES)

    def test_ownership_history_cache_refreshes_only_changed_files(self):
        hashed, cache = "0" * 64, {}
        first = self.owned_wire()
        sample = {key: 1 for key in SCHEMAS["sample"]}
        first.incoming({"method": "thread/tokenUsage/updated", "params": {
            "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": sample}}})
        path = self.write_history(self.root, hashed, first)
        load = lambda: ownership_history(self.root, digest((2, 20)), hashed, "0.160.1", cache)
        with mock.patch(__name__ + ".decode_line", wraps=decode_line) as decode:
            result = load()
            self.assertEqual(result[2], {digest("t"): sample})
            reads = decode.call_count
            self.assertGreater(reads, 0)
            self.assertEqual(load(), result)
            self.assertEqual(decode.call_count, reads)
            larger = {key: 2 for key in SCHEMAS["sample"]}
            first.incoming({"method": "thread/tokenUsage/updated", "params": {
                "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": larger}}})
            self.write_history(self.root, hashed, first)
            self.assertEqual(load()[2], {digest("t"): larger})
            self.assertGreater(decode.call_count, reads)
            reads = decode.call_count
            replacement = self.root / "replacement"
            replacement.write_bytes(path.read_bytes())
            replacement.chmod(0o600)
            stamp = path.stat().st_mtime_ns
            os.utime(replacement, ns=(stamp, stamp))
            replacement.replace(path)
            self.assertEqual(load()[2], {digest("t"): larger})
            self.assertGreater(decode.call_count, reads)
            # A newly discovered generation must refresh the thread baseline,
            # while the unchanged earlier generation remains cached.
            rows = [json.loads(line) for line in path.read_text().splitlines()]
            rows[0].update(pid=4, start_ticks=30)
            latest = {key: 3 for key in SCHEMAS["sample"]}
            rows[-1]["total"] = latest
            added = self.root / "server-4-30.jsonl"
            added.write_text("".join(json.dumps(row) + "\n" for row in rows))
            added.chmod(0o600)
            reads = decode.call_count
            self.assertEqual(load()[2], {digest("t"): latest})
            self.assertEqual(decode.call_count - reads, len(rows))
            reads = decode.call_count
            self.assertEqual(load()[2], {digest("t"): latest})
            self.assertEqual(decode.call_count, reads)
            path.chmod(0o644)
            with self.assertRaisesRegex(Blocked, "unsafe file"):
                load()

    def test_restored_samples_never_inflate_old_or_new_turn_usage(self):
        run = self.run_object()
        original, current = fake_envelope(), fake_envelope()
        current["turn"] = 2
        thread = digest(original["vendor_session_id"])
        sample = {"inputTokens": 3, "cachedInputTokens": 1, "outputTokens": 2,
                  "reasoningOutputTokens": 1, "totalTokens": 5}
        original["vendor"]["total"] = sample.copy()
        current["vendor"]["total"] = sample.copy()
        facts = []
        for generation, turn in (("old", "u"), ("new", "v")):
            for fact in ({"kind": "reply", "method": "turn/start"}, {"kind": "terminal"},
                         {"kind": "usage", "last": sample, "total": sample}):
                facts.append({**fact, "thread": thread, "turn": digest(turn), "trace": digest(generation)})
        restored = {"kind": "usage", "thread": thread, "turn": digest("u"),
                    "last": sample, "total": sample, "trace": digest("new")}
        facts.insert(3, restored)
        run.envelopes = [original, current]
        self.seed_accounting(run, run.envelopes, facts)
        with mock.patch.object(run, "facts", return_value=facts):
            self.assertNotIn(restored, turn_facts(run, original))
            self.assertNotIn(restored, turn_facts(run, current))
            usage(run)

    def write_history(self, directory, hashed, wire=None):
        wire = wire or self.owned_wire()
        rows = [{"kind": "process", "pid": 3, "start_ticks": 10, "executable_hash": hashed},
                {"kind": "reply", "method": "initialize", "version": "0.160.1", "request": digest(99)},
                *wire.facts]
        rows.insert(1, {"kind": "request", "method": "initialize", "request": digest(99)})
        path = directory / "server-3-10.jsonl"
        path.write_text("".join(json.dumps({"seq": index, **row}) + "\n"
                               for index, row in enumerate(rows, 1)))
        path.chmod(0o600)
        return path

    def test_ownership_history_uses_paired_replies_only(self):
        hashed = "0" * 64
        path = self.write_history(self.root, hashed)
        threads, turns, totals = ownership_history(self.root, digest((2, 20)), hashed, "0.160.1")
        self.assertEqual(threads, {digest("t")})
        self.assertEqual(turns, {(digest("t"), digest("u"))})
        self.assertEqual(totals, {})
        # Notifications and unpaired replies cannot manufacture ownership.
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        rows.append({"seq": len(rows) + 1, "kind": "terminal", "thread": digest("foreign"),
                     "turn": digest("never-established")})
        path.write_text("".join(json.dumps(row) + "\n" for row in rows))
        self.assertEqual(ownership_history(self.root, digest((2, 20)), hashed, "0.160.1"),
                         (threads, turns, totals))
        rows = [row for row in rows if not (row.get("kind") == "request" and row.get("method") == "turn/start")]
        path.write_text("".join(json.dumps({**row, "seq": i}) + "\n" for i, row in enumerate(rows, 1)))
        with self.assertRaisesRegex(Blocked, "ownership history"):
            ownership_history(self.root, digest((2, 20)), hashed, "0.160.1")

    def test_deferred_rejection_records_prior_turn_and_pending_requests(self):
        first = self.owned_wire()
        wire = self.resumed_wire(first)
        wire.outgoing({**self.start_message(), "id": 2})
        wire.incoming({"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "foreign"}}})
        with self.assertRaisesRegex(Blocked, "unowned deferred"):
            wire.incoming({"id": 2, "result": {"turn": {"id": "v"}}})
        self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
        self.assertEqual(wire.facts[-1]["turn"], digest("foreign"))
        self.assertEqual(wire.facts[-1]["pending_establishing"], [])
        # A known turn under a different thread remains unowned, with its origin visible.
        with self.assertRaisesRegex(Blocked, "unowned"):
            wire.incoming({"method": "turn/started", "params": {"threadId": "other", "turn": {"id": "u"}}})
        self.assertTrue(wire.facts[-1]["turn_known_earlier"])
        self.assertFalse(wire.facts[-1]["turn_owned"])

    def test_resume_mismatch_and_missing_turn_reply_leave_rejection_fact(self):
        wire = self.owned_wire()
        wire.outgoing({"id": 20, "method": "thread/resume", "params": {
            "threadId": "t", "model": MODEL, "sandbox": "read-only", "approvalPolicy": "never",
            "approvalsReviewer": "user", "excludeTurns": True}})
        with self.assertRaisesRegex(Blocked, "resume identity mismatch"):
            wire.incoming({"id": 20, "result": {"model": MODEL, "approvalPolicy": "never",
                          "approvalsReviewer": "user", "thread": {"id": "foreign"}}})
        self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
        self.assertEqual(wire.facts[-1]["reply_request"], digest(20))
        wire.outgoing({**self.start_message(), "id": 21})
        with self.assertRaisesRegex(Blocked, "turn/start id missing"):
            wire.incoming({"id": 21, "result": {"turn": {}}})
        self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
        self.assertEqual(wire.facts[-1]["thread"], digest("t"))
        self.assertIsNone(wire.facts[-1]["turn"])

    def test_ownership_history_refreshes_later_paired_replies(self):
        hashed = "0" * 64
        first = self.owned_wire()
        path = self.write_history(self.root, hashed, first)
        wire = Wire("0.160.1", history=lambda: ownership_history(
            self.root, digest((2, 20)), hashed, "0.160.1"), generation=digest((2, 20)))
        self.assertTrue(wire.owns((digest("t"), None)))
        self.assertIn((digest("t"), digest("u")), wire.earlier_turns)
        self.assertFalse(wire.owns((digest("t"), digest("u"))))
        first.outgoing({**self.start_message(), "id": 2})
        first.incoming({"id": 2, "result": {"turn": {"id": "v"}}})
        self.write_history(self.root, hashed, first)
        self.assertFalse(wire.owns((digest("t"), digest("v"))))
        self.assertIn((digest("t"), digest("v")), wire.earlier_turns)
        # A partial final record does not manufacture an identity.
        with path.open("ab") as file:
            file.write(b'{"kind":"reply"')
        self.assertFalse(wire.owns((digest("t"), digest("unpaired"))))

    def test_ownership_history_rejects_unsafe_or_wrong_binary_evidence(self):
        path = self.write_history(self.root, "0" * 64)
        with self.assertRaisesRegex(Blocked, "ownership history"):
            ownership_history(self.root, digest((2, 20)), "1" * 64, "0.160.1")
        path.chmod(0o644)
        with self.assertRaisesRegex(Blocked, "ownership history"):
            ownership_history(self.root, digest((2, 20)), "0" * 64, "0.160.1")
        path.unlink()
        path.symlink_to(self.root / "absent")
        with self.assertRaisesRegex(Blocked, "ownership history"):
            ownership_history(self.root, digest((2, 20)), "0" * 64, "0.160.1")

    def test_config_read_unset_otel(self):
        for value in (None, {}):
            candidate = self.config_fixture()
            candidate["config"]["otel"] = value
            self.assertTrue(effective_config(candidate, ["ok"])["all_servers_disabled"])
        candidate["config"].pop("otel")
        self.assertTrue(effective_config(candidate, ["ok"])["all_servers_disabled"])
        for value in (False, [], "foreign", {"exporter": "foreign"}):
            candidate["config"]["otel"] = value
            with self.subTest(value=value), self.assertRaises(Blocked):
                effective_config(candidate, ["ok"])

    def test_deferred_age_blocks_late_traffic_and_reply(self):
        for operation in ("notification", "request", "reply"):
            with self.subTest(operation=operation), \
                 mock.patch.object(time, "monotonic", return_value=100) as clock:
                wire = self.owned_wire()
                wire.outgoing({**self.start_message(), "id": 2})
                wire.incoming({"method": "turn/started", "params": {
                    "threadId": "t", "turn": {"id": "v"}}})
                clock.return_value = 130
                wire.incoming({"method": "thread/status/changed", "params": {"threadId": "t"}})
                clock.return_value = 130.001
                with self.assertRaisesRegex(Blocked, "deferred notification deadline"):
                    if operation == "notification":
                        wire.incoming({"method": "thread/status/changed", "params": {"threadId": "t"}})
                    elif operation == "request":
                        wire.outgoing({"id": 3, "method": "model/list", "params": {}})
                    else:
                        wire.incoming({"id": 2, "result": {"turn": {"id": "v"}}})

    def test_unrelated_reply_does_not_reset_deferred_age(self):
        with mock.patch.object(time, "monotonic", return_value=100) as clock:
            wire = self.owned_wire()
            wire.outgoing({**self.start_message(), "id": 2})
            wire.outgoing({"id": 3, "method": "model/list", "params": {}})
            wire.incoming({"method": "turn/started", "params": {
                "threadId": "t", "turn": {"id": "v"}}})
            clock.return_value = 129
            wire.incoming({"id": 3, "result": {"data": [], "nextCursor": None}})
            clock.return_value = 131
            with self.assertRaisesRegex(Blocked, "deferred notification deadline"):
                wire.resolve_deferred(digest(3))

    def test_item_safety_checked_before_deferral(self):
        for item in ({"type": "collabAgentToolCall"}, {"type": "mcpToolCall"},
                     {"type": "commandExecution", "command": "true", "exitCode": False},
                     {"type": "commandExecution", "command": "true", "aggregatedOutput": []}):
            with self.subTest(item=item):
                wire = self.owned_wire()
                wire.outgoing({**self.start_message(), "id": 2})
                with self.assertRaises(Blocked):
                    wire.incoming({"method": "item/started", "params": {
                        "threadId": "t", "turnId": "v", "item": item}})
                self.assertEqual(wire.deferred, [])

    def test_remote_control_checked_with_establishment_pending(self):
        wire = self.owned_wire()
        wire.outgoing({**self.start_message(), "id": 2})
        with self.assertRaisesRegex(Blocked, "remote control"):
            wire.incoming({"method": "remoteControl/status/changed", "params": {"status": "enabled"}})
        self.assertEqual(wire.deferred, [])

    def test_mcp_startup_notification_always_blocks(self):
        for params in ({}, {"status": "ready"}, {"status": "failed"}):
            with self.subTest(params=params), self.assertRaisesRegex(Blocked, "MCP startup"):
                self.owned_wire().incoming({"method": "mcpServer/startupStatus/updated",
                                            "params": params})

    def test_inventory_precedes_thread_establishment(self):
        for method in ("thread/start", "thread/resume"):
            params = {"model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
                      "sandbox": "read-only", "threadId": "t", "excludeTurns": True}
            with self.subTest(method=method), self.assertRaisesRegex(Blocked, "inventory"):
                Wire("0.160.1").outgoing({"id": 1, "method": method, "params": params})

    def test_feature_preflight_private_home_and_overrides(self):
        run = self.run_object()
        good = subprocess.CompletedProcess([], 0, "plugins experimental false\napps beta false\n", "")
        with mock.patch.object(subprocess, "run", return_value=good) as called:
            feature_preflight(run.codex, run.env, run.work)
        args, kwargs = called.call_args
        self.assertEqual(args[0], [str(run.codex), "features", "list", *server_args([])])
        self.assertNotEqual(kwargs["env"]["CODEX_HOME"], str(run.owner_codex))
        self.assertFalse(Path(kwargs["env"]["CODEX_HOME"]).exists())
        self.assertEqual(kwargs["timeout"], 30)
        self.assertNotIn("--model", args[0])

    def test_feature_preflight_omits_mcp_overrides_owned_server_keeps_them(self):
        run = self.run_object()
        owned_args = server_args(["ok"])
        good = subprocess.CompletedProcess([], 0, "plugins experimental false\napps beta false\n", "")
        with mock.patch.object(subprocess, "run", return_value=good) as called:
            feature_preflight(run.codex, run.env, run.work)
        probe_args = called.call_args.args[0]
        self.assertEqual(probe_args, [str(run.codex), "features", "list", "--disable", "apps",
                                    "--disable", "plugins", "-c", "notify=[]"])
        self.assertFalse(any("mcp_servers" in arg for arg in probe_args))
        self.assertIn("mcp_servers.ok.enabled=false", owned_args)

    def test_feature_preflight_unavailable_or_unverified_blocks(self):
        run = self.run_object()
        for code, text in ((1, ""), (0, "apps beta false\n"),
                           (0, "plugins beta true\napps beta false\n"),
                           (0, "plugins beta false\nplugins beta false\napps beta false\n")):
            with self.subTest(code=code, text=text), \
                 mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], code, text, "")), \
                 self.assertRaisesRegex(Blocked, "features list"):
                feature_preflight(run.codex, run.env, run.work)
        with mock.patch.object(subprocess, "run", side_effect=subprocess.TimeoutExpired([], 30)), \
             self.assertRaisesRegex(Blocked, "features list"):
            feature_preflight(run.codex, run.env, run.work)

    def test_run_preflight_requires_feature_proof(self):
        for verified in (True, False):
            with self.subTest(verified=verified), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    run = self.run_object()
                    run.owner_codex = self.root / "owner"
                    run.owner_codex.mkdir()
                    (run.owner_codex / "auth.json").touch(mode=0o600)
                    version = subprocess.CompletedProcess([], 0, "codex-cli 0.160.1\n", "")
                    listing = subprocess.CompletedProcess([], 0, "apps beta false\n" +
                              ("plugins experimental false\n" if verified else ""), "")
                    with mock.patch(__name__ + ".mcp_names", return_value=["ok"]), \
                         mock.patch.object(subprocess, "run", side_effect=[version, listing]) as called:
                        if verified:
                            run.preflight()
                            proof = json.loads((run.evidence / "preflight.json").read_text())
                            self.assertEqual(proof["feature_registry"], {"apps_disabled": True,
                                             "plugins_disabled": True, "registry_checked": True})
                        else:
                            with self.assertRaisesRegex(Blocked, "features list"):
                                run.preflight()
                            self.assertFalse(run.launcher.exists())
                            self.assertFalse((run.evidence / "preflight.json").exists())
                    self.assertEqual(len(called.call_args_list), 2)
                    self.assertIn("mcp_servers.ok.enabled=false", run.vendor_args)
                    feature_args = called.call_args_list[1].args[0]
                    self.assertFalse(any("mcp_servers" in arg for arg in feature_args))
                finally:
                    self.root = previous

    def test_establishing_notifications_defer_ownership(self):
        wire = Wire("0.160.1")
        wire.inventory = True
        wire.outgoing({"id": 10, "method": "thread/start", "params": {
            "model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
            "sandbox": "read-only"}})
        wire.incoming({"method": "thread/started", "params": {"thread": {"id": "t"}}})
        self.assertNotIn(digest("t"), wire.threads)
        wire.incoming({"id": 10, "result": {"model": MODEL, "approvalPolicy": "never",
                      "approvalsReviewer": "user", "thread": {"id": "t"}}})
        wire.outgoing(self.start_message())
        wire.incoming({"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "u"}}})
        wire.incoming({"method": "item/started", "params": {"threadId": "t", "turnId": "u",
                      "item": {"type": "userMessage"}}})
        wire.incoming({"method": "turn/completed", "params": {"threadId": "t",
                      "turn": {"id": "u", "status": "completed"}}})
        self.assertFalse(any(f["kind"] == "terminal" for f in wire.facts))
        wire.incoming({"id": 1, "result": {"turn": {"id": "u"}}})
        self.assertEqual(len([f for f in wire.facts if f["kind"] == "terminal"]), 1)
        self.assertEqual(wire.deferred, [])

    def test_deferred_foreign_id_blocks_on_establishing_reply(self):
        wire = self.owned_wire()
        wire.outgoing({**self.start_message(), "id": 2})
        wire.incoming({"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "foreign"}}})
        with self.assertRaisesRegex(Blocked, "unowned"):
            wire.incoming({"id": 2, "result": {"turn": {"id": "v"}}})

    def test_deferred_notification_bound_and_unrelated_thread(self):
        wire = self.owned_wire()
        wire.outgoing({**self.start_message(), "id": 2})
        with self.assertRaisesRegex(Blocked, "unowned"):
            wire.incoming({"method": "turn/started", "params": {"threadId": "foreign", "turn": {"id": "v"}}})
        with mock.patch(__name__ + ".REQUEST_LIMIT", 2):
            for _ in range(2):
                wire.incoming({"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "v"}}})
            with self.assertRaisesRegex(Blocked, "deferred.*bound"):
                wire.incoming({"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "v"}}})

    def test_deferred_usage_error_and_resume_notifications(self):
        wire = self.owned_wire()
        wire.outgoing({"id": 11, "method": "thread/resume", "params": {
            "model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
            "sandbox": "read-only", "threadId": "saved", "excludeTurns": True}})
        wire.incoming({"method": "thread/started", "params": {"thread": {"id": "saved"}}})
        wire.incoming({"id": 11, "result": {"model": MODEL, "approvalPolicy": "never",
                      "approvalsReviewer": "user", "thread": {"id": "saved"}}})
        wire.outgoing({**self.start_message(), "id": 2})
        sample = {key: 1 for key in SCHEMAS["sample"]}
        wire.incoming({"method": "thread/tokenUsage/updated", "params": {
            "threadId": "t", "turnId": "v", "tokenUsage": {"last": sample, "total": sample}}})
        wire.incoming({"method": "error", "params": {"threadId": "t", "turnId": "v"}})
        self.assertFalse(any(f["kind"] == "usage" for f in wire.facts))
        wire.incoming({"id": 2, "result": {"turn": {"id": "v"}}})
        self.assertEqual(len([f for f in wire.facts if f["kind"] == "usage"]), 1)
        self.assertEqual(wire.deferred_bytes, 0)
        self.assertEqual(wire.deferred, [])

    def test_deferred_byte_bound(self):
        wire = self.owned_wire()
        wire.outgoing({**self.start_message(), "id": 2})
        with mock.patch(__name__ + ".TRACE_BYTES", 8), self.assertRaisesRegex(Blocked, "deferred.*bound"):
            wire.incoming({"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "v"}}})

    def test_concurrent_thread_replies_resolve_only_matching_notifications(self):
        wire = Wire("0.160.1")
        wire.inventory = True
        for ident in (10, 11):
            wire.outgoing({"id": ident, "method": "thread/start", "params": {
                "model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
                "sandbox": "read-only"}})
        wire.incoming({"method": "thread/started", "params": {"thread": {"id": "t"}}})
        wire.incoming({"id": 11, "result": {"model": MODEL, "approvalPolicy": "never",
                      "approvalsReviewer": "user", "thread": {"id": "other"}}})
        self.assertEqual(len(wire.deferred), 1)
        wire.incoming({"id": 10, "result": {"model": MODEL, "approvalPolicy": "never",
                      "approvalsReviewer": "user", "thread": {"id": "t"}}})
        self.assertEqual(wire.deferred, [])

    def test_metadata_never_follows_session_ancestor_symlink(self):
        outside = self.root / "outside"
        outside.mkdir()
        (self.root / "sessions").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(Blocked, "ancestor unsafe"):
            owner_metadata(self.root)

    def test_error_notification_ownership(self):
        wire = self.owned_wire()
        wire.incoming({"method": "error", "params": {"threadId": "t", "error": {}}})
        wire.incoming({"method": "error", "params": {"error": {}}})
        for params in ({"threadId": "foreign"}, {"threadId": None},
                       {"threadId": "t", "turnId": None}, {"turnId": None},
                       {"threadId": "t", "turnId": "foreign"}):
            with self.subTest(params=params), self.assertRaisesRegex(Blocked, "unowned"):
                wire.incoming({"method": "error", "params": params})

    def test_global_warning_null_ids_are_unscoped(self):
        wire = self.owned_wire()
        for params in ({"threadId": None}, {"turnId": None},
                       {"threadId": None, "turnId": None}, {"threadId": "t", "turnId": None}):
            with self.subTest(params=params):
                fact = wire.incoming({"method": "warning", "params": params})
                self.assertEqual(fact["kind"], "notification")
        for params in ({"threadId": "foreign"}, {"threadId": "foreign", "turnId": None},
                       {"threadId": "t", "turnId": "foreign"}):
            with self.subTest(params=params), self.assertRaisesRegex(Blocked, "unowned"):
                wire.incoming({"method": "warning", "params": params})
            self.assertEqual(wire.facts[-1]["kind"], "ownership_rejection")
            self.assertEqual(wire.facts[-1]["method"], "warning")

    def test_active_profile_mcp_blocks_without_root_override(self):
        config = self.root / "config.toml"
        config.write_text('profile="active"\n[profiles.active.mcp_servers.hidden]\ncommand="fake"\n')
        with self.assertRaisesRegex(Blocked, "active profile MCP"):
            mcp_names(self.root)

    def test_metadata_ignores_old_sessions_and_includes_tomorrow(self):
        today = datetime.date(2026, 10, 7)
        for day in (today - datetime.timedelta(days=1), today, today + datetime.timedelta(days=1)):
            directory = self.root / "sessions" / day.strftime("%Y/%m/%d")
            directory.mkdir(parents=True)
            (directory / "one").touch()
        original = Path.iterdir
        def bounded(path):
            self.assertNotEqual(path.name, "06", "old sessions traversed")
            self.assertNotEqual(path, self.root / "sessions", "whole sessions tree traversed")
            return original(path)
        class Clock(datetime.datetime):
            @classmethod
            def now(cls, tz=None):
                self.assertIs(tz, datetime.timezone.utc)
                return cls(2026, 10, 7, tzinfo=tz)
        with mock.patch.object(Path, "iterdir", bounded), mock.patch.object(datetime, "datetime", Clock):
            info = owner_metadata(self.root)
        self.assertEqual(info["session_files"], 2)
        self.assertTrue(secret_free(info))

    def test_config_read_checks_actual_override_results(self):
        config = self.config_fixture()
        self.assertTrue(effective_config(config, ["ok"])["all_servers_disabled"])
        for change in (lambda c: c["mcp_servers"]["ok"].update(enabled=True),
                       lambda c: c["features"].update(plugins=True),
                       lambda c: c.update(notify=["foreign-command"]),
                       lambda c: c.update(model_provider="foreign"),
                       lambda c: c.update(profile="foreign"),
                       lambda c: c.update(model_providers={"foreign": {}}),
                       lambda c: c.update(otel={"exporter": "foreign"}),
                       lambda c: c.pop("notify"),
                       lambda c: c["mcp_servers"].pop("ok"),
                       lambda c: c["mcp_servers"].update({'"ok"': {"enabled": False}})):
            candidate = copy.deepcopy(config)
            change(candidate["config"])
            with self.subTest(candidate=digest(candidate)), self.assertRaises(Blocked):
                effective_config(candidate, ["ok"])

    def test_proxy_config_schema_miss_blocks_before_discovery(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(config={"config": {}}))
        self.assertEqual(result, 1)
        self.assertNotIn("model/list", sent)
        self.assertNotIn("mcpServerStatus/list", sent)
        self.assertNotIn("turn/start", sent)
        rows = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
        self.assertEqual(rows[-1]["blocked_reason"], "config/read cannot confirm owned feature overrides")

    def test_proxy_native_hash_mismatch_blocks_before_handshake(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(executable_mismatch=True))
        self.assertEqual(result, 1)
        self.assertEqual(sent, [])
        child.wait.assert_awaited()

    def test_proxy_term_and_hup_refuse_later_turn(self):
        for sig in (signal.SIGTERM, signal.SIGHUP):
            with self.subTest(signal=sig.name):
                # A separate synthetic root for each complete exchange.
                with tempfile.TemporaryDirectory() as root:
                    previous, self.root = self.root, Path(root)
                    try:
                        result, child, signals, sent, traces = asyncio.run(
                            self.proxy_fixture(termination=sig, after_signal=True))
                        self.assertEqual(result, 1)
                        self.assertNotIn("turn/start", sent)
                        rows = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
                        self.assertEqual(rows[-1]["termination_signal"], sig.name)
                        self.assertTrue(rows[-1]["child_reaped"])
                        self.assertFalse(rows[-1]["complete"])
                    finally:
                        self.root = previous

    def test_owner_home_metadata_never_reads_contents(self):
        (self.root / "auth.json").touch(mode=0o600)
        sessions = self.root / "sessions" / datetime.datetime.now(datetime.timezone.utc).strftime("%Y/%m/%d")
        sessions.mkdir(parents=True)
        (sessions / "one").touch()
        with mock.patch("builtins.open", side_effect=AssertionError("content read")), \
             mock.patch.object(os, "open", side_effect=AssertionError("content read")):
            before = owner_metadata(self.root)
        (sessions / "two").touch()
        with mock.patch("builtins.open", side_effect=AssertionError("content read")), \
             mock.patch.object(os, "open", side_effect=AssertionError("content read")):
            after = owner_metadata(self.root)
        self.assertEqual(after["session_files"] - before["session_files"], 1)
        self.assertEqual(len(set(after["session_digests"]) - set(before["session_digests"])), 1)
        self.assertNotIn("auth.json", json.dumps(after))
        self.assertTrue(secret_free(after))

    def test_bound_without_tool_evidence_is_only_recorded(self):
        run = self.run_object()
        with mock.patch(__name__ + ".turn_facts", return_value=[]):
            denied_write_observation(run, fake_envelope(), "fixed", self.root / "absent")
        self.assertEqual(run.checks, [])
        self.assertEqual(run.record_only, [{"observation": "denied write", "file_absent": True,
                                         "exact_command_refusal_visible": False}])
        target = self.root / "present"
        target.touch()
        with mock.patch(__name__ + ".turn_facts", return_value=[]), self.assertRaisesRegex(Blocked, "created"):
            denied_write_observation(run, fake_envelope(), "fixed", target)

    def test_stop_attempts_cleanup_despite_bad_trace(self):
        run = self.run_object()
        with mock.patch.object(run, "facts", side_effect=Blocked("synthetic trace")), \
             mock.patch.object(shared.Run, "stop_daemon") as stop, \
             self.assertRaisesRegex(Blocked, "trace inspection"):
            run.stop_daemon(final=True)
        stop.assert_called_once_with(final=True)

    def test_main_pass_logic_and_signal_registration(self):
        for state, expected in (("clear", "pass"), ("interrupted", "blocked"),
                                ("active", "blocked"), ("uncertain", "blocked")):
            with self.subTest(state=state), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    shared.INTERRUPTED.clear()

                    run = self.run_object()
                    run.checks = [{"check": "synthetic proof", "pass": True}]
                    run.daemons = [{"stopped": True}]
                    evidence = self.root / "scratchpad" / "result"
                    if state == "active":
                        run.spend.active.add("unsettled")
                    if state == "uncertain":
                        run.spend.uncertain = True
                    def executed(_):
                        if state == "interrupted":
                            shared.record_signal(signal.SIGHUP, None)
                    with mock.patch(__name__ + ".__file__", str(self.root / "scripts/qualify/codex.py")), \
                         mock.patch(__name__ + ".Run", return_value=run), \
                         mock.patch(__name__ + ".execute", side_effect=executed), \
                         mock.patch(__name__ + ".cleanup", return_value=(True, [])), \
                         mock.patch.object(signal, "signal") as signals, mock.patch("builtins.print"):
                        rc = main(["--run", "--via", str(run.via), "--codex", str(run.codex),
                                   "--via-sha256", run.args.via_sha256, "--codex-sha256", run.args.codex_sha256,
                                   "--evidence", str(evidence)])
                    self.assertEqual(rc, 0 if expected == "pass" else 2)
                    summary = json.loads((evidence / "summary.json").read_text())
                    self.assertEqual(summary["result"], expected)
                    self.assertIn(mock.call(signal.SIGHUP, shared.record_signal), signals.call_args_list)
                    self.assertEqual(summary["owner_home_writes"]["policy"], "accepted, not minimised")
                    self.assertIsInstance(summary["record_only"], list)
                finally:
                    self.root = previous
                    shared.INTERRUPTED.clear()

    def test_signal_during_hashing_never_launches_submission(self):
        for verb in ("spawn", "resume"):
            with self.subTest(verb=verb):
                shared.INTERRUPTED.clear()
                run = self.run_object()
                def hashed(_):
                    shared.record_signal(signal.SIGTERM, None)
                    return run.args.via_sha256
                with mock.patch.object(shared, "sha256", side_effect=hashed), \
                     mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "{}", "")) as called, \
                     self.assertRaisesRegex(Blocked, "interrupted"):
                    run.via_call(run.evidence, "submission", verb, "--json")
                called.assert_not_called()

    def test_signal_during_final_removal_or_scan_blocks_publication(self):
        for boundary in ("removal", "scan", "publication", "rename"):
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    shared.INTERRUPTED.clear()
                    run = self.run_object()
                    run.checks = [{"check": "synthetic proof", "pass": True}]
                    run.daemons = [{"stopped": True}]
                    evidence = self.root / "scratchpad" / "result"
                    original_remove, original_scan, original_save = shutil.rmtree, package_secret_free, save
                    original_replace = os.replace
                    def remove(*args, **kwargs):
                        result = original_remove(*args, **kwargs)
                        if boundary == "removal":
                            shared.record_signal(signal.SIGTERM, None)
                        return result
                    def scan(*args):
                        result = original_scan(*args)
                        if boundary == "scan":
                            shared.record_signal(signal.SIGHUP, None)
                        return result
                    def saved(path, value):
                        result = original_save(path, value)
                        if boundary == "publication" and path.name == "summary.pending.json":
                            shared.record_signal(signal.SIGINT, None)
                        return result
                    def replaced(source, target):
                        result = original_replace(source, target)
                        if boundary == "rename" and target.name == "summary.json":
                            shared.record_signal(signal.SIGINT, None)
                        return result
                    with mock.patch(__name__ + ".__file__", str(self.root / "scripts/qualify/codex.py")), \
                         mock.patch(__name__ + ".Run", return_value=run), \
                         mock.patch(__name__ + ".execute"), \
                         mock.patch(__name__ + ".cleanup", return_value=(True, [])), \
                         mock.patch.object(shutil, "rmtree", side_effect=remove), \
                         mock.patch(__name__ + ".package_secret_free", side_effect=scan), \
                         mock.patch(__name__ + ".save", side_effect=saved), \
                         mock.patch.object(os, "replace", side_effect=replaced), \
                         mock.patch.object(signal, "signal"), mock.patch("builtins.print"):
                        result = main(["--run", "--via", str(run.via), "--codex", str(run.codex),
                                       "--via-sha256", run.args.via_sha256, "--codex-sha256", run.args.codex_sha256,
                                       "--evidence", str(evidence)])
                    self.assertEqual(result, 2)
                    summary = json.loads((evidence / "summary.json").read_text())
                    self.assertEqual(summary["result"], "blocked")
                    self.assertTrue(summary["interrupted"])
                finally:
                    self.root = previous
                    shared.INTERRUPTED.clear()

    def test_never_ask_missing_command_blocks(self):
        run = self.run_object()
        ended = fake_envelope()
        facts = [{"kind": "reply", "method": "thread/start", "never": True,
                  "reviewer_user": True, "thread": digest(ended["vendor_session_id"])}]
        with mock.patch.object(run, "submit", return_value="fake"), \
             mock.patch.object(run, "finish", return_value=ended), \
             mock.patch.object(run, "facts", return_value=facts), \
             mock.patch.object(run, "observe_denied_execution", return_value={"errno": "EROFS", "file_absent": True}), \
             mock.patch(__name__ + ".turn_facts", return_value=[]), self.assertRaisesRegex(Blocked, "fixed command"):
            never_ask(run)

    def test_interrupt_proves_shared_trace_without_process_count(self):
        for same in (True, False):
            with self.subTest(same=same), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    run = self.run_object()
                    ended, other, after = fake_envelope("cancelled"), fake_envelope(), fake_envelope()
                    ended.update(stop_reason="interrupted", cancel={"outcome": "acknowledged", "cleanup": "quiescent"},
                                 vendor_session_id="a")
                    other.update(final_text="B_ONLY", vendor_session_id="b")
                    after.update(final_text="A_AFTER", vendor_session_id="a")
                    facts = [{"kind": "process"}] * (3 if same else 2) + [
                        {"kind": "reply", "method": "turn/start", "thread": digest("a"), "turn": digest("ua"), "trace": "same"},
                        {"kind": "reply", "method": "turn/start", "thread": digest("b"), "turn": digest("ub"),
                         "trace": "same" if same else "different"},
                        {"kind": "request", "method": "turn/interrupt", "thread": digest("a")},
                        {"kind": "terminal", "thread": digest("a"), "turn": digest("ua"),
                         "status": "interrupted", "trace": "same"},
                        {"kind": "terminal", "thread": digest("b"), "turn": digest("ub"), "status": "completed",
                         "trace": "same" if same else "different"},
                    ]
                    run.handles = {"a": "fake"}
                    run.receipts = {"a1": ({"turn": "turn-a"}, "a")}
                    status = {"vendor_identity_verified": True, "progress": {"running_tools": ["fake"]},
                              "turns": [{"state": "running"}], "cancel": {"outcome": "requested"}}
                    with mock.patch.object(run, "submit", side_effect=["a", "b", "a"]), \
                         mock.patch.object(run, "observe_denied_execution", return_value={"pid": 2, "start_ticks": 20}), \
                         mock.patch.object(run, "command_protocol"), mock.patch.object(run, "tool_reply"), \
                         mock.patch.object(run, "via_call", return_value=(0, status, None)), \
                         mock.patch.object(run, "finish", side_effect=[ended, other, after]), \
                         mock.patch.object(run, "facts", return_value=facts), \
                         mock.patch.object(run, "events", return_value=[{"type": t} for t in
                             ("cancel.requested", "cancel.settled", "turn.ended")]), \
                         mock.patch.object(Proc, "open", return_value=(None, None)):
                        if same:
                            interrupt(run)
                        else:
                            with self.assertRaisesRegex(Blocked, "shared owned server"):
                                interrupt(run)
                finally:
                    self.root = previous

    def test_cleanup_counts_declines_explicitly(self):
        run = self.run_object()
        facts = [{"kind": "decline", "method_hash": digest(NO_GRANT_METHODS[0])}]
        with mock.patch.object(run, "facts", return_value=facts), \
             mock.patch.object(run, "stop_daemon"), \
             mock.patch(__name__ + ".save"):
            cleanup(run)
        counts = run.record_only[0]["per_method_counts"]
        self.assertEqual(counts["item/commandExecution/requestApproval"], 1)
        self.assertEqual(counts["item/tool/call"], 0)
        self.assertEqual(len(counts), 6)

    def conversation_fixture(self, wrong_bound=False, marker_link=False, observed_execution=True,
                             deviation=None, marker_missing=False, allowed_execution=True):
        run = self.run_object()
        first, second, third = (fake_envelope() for _ in range(3))
        nonce_a, nonce_b = "VIAQUAL" + "a" * 32, "VIAQUAL" + "b" * 32
        first.update(structured_output={"a": nonce_a}, final_text=json.dumps({"a": nonce_a}, separators=(",", ":")))
        second.update(turn=2, structured_output={"b": nonce_b},
                      final_text=json.dumps({"b": nonce_b}, separators=(",", ":")))
        third.update(turn=3, final_text=nonce_a, structured_output=None)
        thread = digest(first["vendor_session_id"])
        submitted = []
        def submit(label, *args, **kwargs):
            submitted.append(label)
            if label == "c1":
                marker = run.work / "conversation/allowed.txt"
                if marker_link:
                    target = self.root / "private-target"
                    target.write_text("allowed")
                    marker.symlink_to(target)
                elif not marker_missing:
                    marker.write_text("allowed")
            return "fake"
        def facts(*args, **kwargs):
            return [{"kind": "request", "method": "thread/resume", "thread": thread,
                     "exclude_turns": True, "bound": "read-only"}] + [
                {"kind": "request", "method": "turn/start", "thread": thread, "bound": bound,
                 "schema": digest(json.loads((run.work / name).read_text()))}
                for name, bound in list(zip(("a.json", "b.json", "null.json"),
                    ("workspaceWrite", "workspaceWrite" if wrong_bound else "readOnly", "readOnly")))[:len(submitted)]]
        def turn_rows(_, envelope, facts=None):
            label = {1: "c1", 2: "c2", 3: "c3"}[envelope["turn"]]
            if label == "c3" or label == "c1" and deviation == "missing":
                return []
            argv = run.command_specs[label]["argv"]
            return [{"kind": "tool", "finished": finished, "item_hash": digest(label),
                     **tool_line_fact(envelope["final_text"]),
                     "argv_hash": digest(argv if deviation != "different" or label != "c1" else ["other"]),
                     "command_hash": digest(shlex.join(argv)), "read_only_error": False, "exit_code": 0}
                    for finished in (False, True)]
        def observe(label, *args, **kwargs):
            if label == "c1" and not allowed_execution:
                raise Blocked("prohibited execution not observed")
            if label == "c2" and not observed_execution:
                raise Blocked("prohibited execution not observed")
            return {"errno": "EROFS", "file_absent": True}
        with mock.patch.object(run, "submit", side_effect=submit), \
             mock.patch.object(run, "finish", side_effect=[first, second, third]), \
             mock.patch.object(run, "facts", side_effect=facts), \
             mock.patch.object(run, "stop_daemon"), mock.patch.object(run, "start"), \
             mock.patch.object(run, "observe_denied_execution", side_effect=observe), \
             mock.patch(__name__ + ".turn_facts", side_effect=turn_rows), \
             mock.patch.object(secrets, "token_hex", return_value="fixed"):
            conversation(run)
        return run

    def test_conversation_requires_protocol_and_process_command_identity(self):
        run = self.conversation_fixture()
        self.assertTrue(all(check["pass"] for check in run.checks))
        self.assertTrue(any(row["observation"] == "denied write" for row in run.record_only))

    def test_changed_bound_without_execution_never_passes(self):
        with self.assertRaisesRegex(Blocked, "prohibited execution"):
            self.conversation_fixture(observed_execution=False)

    def test_allowed_write_without_process_proof_never_passes(self):
        with self.assertRaisesRegex(Blocked, "execution not observed"):
            self.conversation_fixture(allowed_execution=False)

    def test_allowed_write_requires_exact_protocol_command(self):
        for deviation in ("missing", "different"):
            with self.subTest(deviation=deviation), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    with self.assertRaisesRegex(Blocked, "fixed command"):
                        self.conversation_fixture(deviation=deviation)
                finally:
                    self.root = previous

    def test_missing_workspace_marker_records_reason_before_blocking(self):
        with self.assertRaisesRegex(Blocked, "workspace marker unverifiable"):
            self.conversation_fixture(marker_missing=True)
        fact = self.root / "evidence/workspace-marker.json"
        self.assertTrue(fact.exists())
        recorded = json.loads(fact.read_text())
        self.assertEqual(recorded["status"], "absent")
        self.assertTrue(secret_free(recorded))

    def test_interpreter_whitespace_stops_before_preflight_processes(self):
        run = self.run_object()
        version = subprocess.CompletedProcess([], 0, "codex-cli 0.160.1\n", "")
        with mock.patch.object(sys, "executable", "/synthetic/Project Name/python"), \
             mock.patch(__name__ + ".pins"), mock.patch(__name__ + ".credential_metadata", return_value={}), \
             mock.patch(__name__ + ".owner_metadata", return_value={}), \
             mock.patch(__name__ + ".mcp_names", return_value=[]), \
             mock.patch(__name__ + ".feature_preflight", return_value={}), \
             mock.patch.object(shared, "sha256", return_value="0" * 64), \
             mock.patch.object(subprocess, "run", return_value=version) as called, \
             self.assertRaisesRegex(Blocked, "interpreter.*whitespace"):
            run.preflight()
        called.assert_not_called()

    def test_wrong_bound_between_turns_blocks_even_with_absent_file(self):
        with self.assertRaisesRegex(Blocked, "bound change sent between turns"):
            self.conversation_fixture(wrong_bound=True)

    def test_all_six_no_grant_reply_shapes(self):
        replies = [{"decision": "decline"}, {"decision": "decline"}, {"permissions": {}},
                   {"answers": {}}, {"action": "decline", "content": None},
                   {"contentItems": [], "success": False}]
        for method, reply in zip(NO_GRANT_METHODS, replies):
            with self.subTest(method=method):
                wire = self.owned_wire()
                wire.incoming({"id": 20, "method": method, "params": {"threadId": "t", "turnId": "u"}})
                self.assertTrue(wire.outgoing({"id": 20, "result": reply})["no_grant"])
                wire.incoming({"id": 21, "method": method, "params": {"threadId": "t", "turnId": "u"}})
                with self.assertRaisesRegex(Blocked, "granted"):
                    wire.outgoing({"id": 21, "result": {}})

    def test_proxy_duplicate_preflight_reply_blocks(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(duplicate_config=True))
        self.assertEqual(result, 1)
        self.assertNotIn("turn/start", sent)
        rows = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
        self.assertEqual(rows[-1]["blocked_reason"], "config/read duplicate or uncorrelated reply")
        self.assertFalse(rows[-1]["complete"])
        self.assertTrue(rows[-1]["child_reaped"])

    def test_workspace_marker_never_follows_a_symlink(self):
        with self.assertRaisesRegex(Blocked, "workspace marker unverifiable"):
            self.conversation_fixture(marker_link=True)

    def test_denied_file_absence_requires_metadata_proof(self):
        run = self.run_object()
        target = self.root / "dangling"
        target.symlink_to(self.root / "missing")
        with mock.patch(__name__ + ".turn_facts", return_value=[]), \
             self.assertRaisesRegex(Blocked, "created a file"):
            denied_write_observation(run, fake_envelope(), "fixed", target)
        target.unlink()
        with mock.patch(__name__ + ".turn_facts", return_value=[]), \
             mock.patch.object(Path, "lstat", side_effect=PermissionError("synthetic")), \
             self.assertRaisesRegex(Blocked, "absence unverifiable"):
            denied_write_observation(run, fake_envelope(), "fixed", target)

    def test_pinned_hash_mismatch_stops(self):
        run = self.run_object()
        with mock.patch.object(subprocess, "run") as start, self.assertRaisesRegex(Blocked, "pinned-hash"):
            pins(run.via, run.codex, "0" * 64, run.args.codex_sha256, MODEL)
        start.assert_not_called()

    def test_pins_positive_fake_files(self):
        run = self.run_object()
        pins(run.via, run.codex, run.args.via_sha256, run.args.codex_sha256, MODEL)

    def test_cli_version_mismatch_stops(self):
        run = self.run_object()
        run.owner_codex = self.root / "owner"
        run.owner_codex.mkdir()
        (run.owner_codex / "auth.json").touch(mode=0o600)
        result = subprocess.CompletedProcess([], 0, "codex-cli 0.160.2\n", "")
        with mock.patch(__name__ + ".mcp_names", return_value=[]), \
             mock.patch.object(subprocess, "run", return_value=result) as called, \
             self.assertRaisesRegex(Blocked, "version mismatch"):
            run.preflight()
        self.assertEqual(len(called.call_args_list), 1)

    def test_wire_version_mismatch_stops(self):
        wire = Wire("0.160.1")
        wire.outgoing({"id": 1, "method": "initialize", "params": {}})
        with self.assertRaisesRegex(Blocked, "version mismatch"):
            wire.incoming({"id": 1, "result": {"userAgent": "via/0.160.0"}})

    def test_wrong_model_stops_before_submit(self):
        with self.assertRaisesRegex(Blocked, "gpt-6-luna"):
            Spending().reserve("x", "gpt-6-sol")

    def test_wrong_model_stops_on_wire(self):
        with self.assertRaisesRegex(Blocked, "gpt-6-luna"):
            Wire("0.160.1").outgoing(self.start_message("gpt-6-sol"))

    def test_spending_turn_cap_stops(self):
        spending = Spending()
        for number in range(BASE_TURN_LIMIT):
            spending.reserve(str(number), MODEL)
            spending.settle(str(number), fake_envelope())
        with self.assertRaisesRegex(Blocked, "spending control"):
            spending.reserve("excess", MODEL)

    def test_spending_concurrency_cap_stops(self):
        spending = Spending()
        for number in range(ACTIVE_LIMIT):
            spending.reserve(str(number), MODEL)
        with self.assertRaisesRegex(Blocked, "spending control"):
            spending.reserve("third", MODEL)

    def test_spending_uncertainty_is_sticky(self):
        spending = Spending()
        spending.reserve("a", MODEL)
        with self.assertRaises(Blocked):
            spending.settle("a", fake_envelope("unknown"))
        with self.assertRaisesRegex(Blocked, "uncertainty"):
            spending.reserve("b", MODEL)

    def test_spending_deadline_stops(self):
        spending = Spending()
        spending.deadline = time.monotonic() - 1
        with self.assertRaisesRegex(Blocked, "deadline"):
            spending.reserve("a", MODEL)

    def test_deferred_signal_blocks_next_submit(self):
        spending = Spending()
        spending.reserve("a", MODEL)
        shared.record_signal(signal.SIGTERM, None)
        spending.settle("a", fake_envelope())
        with self.assertRaisesRegex(Blocked, "interrupted"):
            spending.reserve("b", MODEL)

    def test_unverifiable_identity_stops(self):
        self.process(2, 1, 20)
        (self.root / "2/stat").write_text("truncated")
        with self.assertRaisesRegex(Blocked, "unverifiable identity"):
            Proc(self.root).content({"pid": 2, "start_ticks": 20}, "cmdline")

    def test_pid_reuse_does_not_read_content(self):
        identity = self.process(2, 1, 20)
        self.process(2, 1, 30)
        with self.assertRaisesRegex(Blocked, "changed process"):
            Proc(self.root).content(identity, "cmdline")

    def test_foreign_process_stops_before_content_read(self):
        run = self.run_object()
        server = self.process(9, 0, 90)
        self.process(1, 0, 10)
        self.process(2, 1, 20)
        proc = Proc(self.root)
        generation, thread, turn = digest("generation"), digest("thread"), digest("turn")
        facts = [{"kind": "reply", "method": "turn/start", "reservation": digest("c1"),
                  "trace": generation, "thread": thread, "turn": turn},
                 {"kind": "request", "method": "turn/start", "reservation": digest("c1"),
                  "bound": "workspaceWrite"},
                 {"kind": "process", "trace": generation, **server},
                 {"kind": "terminal", "trace": generation, "thread": thread, "turn": turn}]
        with mock.patch(__name__ + ".Proc", return_value=proc), \
             mock.patch.object(proc, "content") as content, \
             mock.patch.object(os, "listdir", return_value=["2"]), \
             mock.patch.object(run, "facts", return_value=facts), \
             self.assertRaisesRegex(Blocked, "not observed"):
            run.observe_denied_execution("c1", self.root / "allowed.txt", "workspaceWrite", False)
        content.assert_not_called()

    def test_owned_identity_and_descriptor_read(self):
        root = self.process(1, 0, 10)
        child = self.process(2, 1, 20)
        proc = Proc(self.root)
        self.assertTrue(proc.own(child, root))
        self.assertEqual(proc.content(child, "cmdline"), b"fake\0safe\0")

    def test_foreign_daemon_reply_cannot_establish_ownership(self):
        run = self.run_object()
        with mock.patch.object(run, "via_call", return_value=(0, {"pid": 2}, None)) as calls, \
             mock.patch(__name__ + ".proc_stat", return_value={"pid": 2, "start_ticks": 20}), \
             mock.patch.object(run, "lock_lines", return_value={}), \
             mock.patch.object(run, "find_daemons", return_value=[]), self.assertRaises(Blocked):
            run.start("fake")
        self.assertIsNone(run.daemon)
        self.assertTrue(run.daemon_unverified["start_failed"])
        self.assertEqual(len(calls.call_args_list), 1)

    def test_unreadable_daemon_stop_never_passes(self):
        run = self.run_object()
        run.daemon = {"pid": 2, "start_ticks": 20}
        run.phase = "fake"
        run.daemons = [{"phase": "fake", "pid": 2, "start_ticks": 20}]
        with mock.patch.object(run, "lock_lines", return_value=None), \
             mock.patch.object(run, "find_daemons", return_value=[]), \
             mock.patch.object(shared, "all_procs", return_value={}), \
             mock.patch.object(shared, "alive", return_value=None), \
             mock.patch.object(shared, "lock_free", return_value=None), \
             mock.patch.object(shared, "uptime_ticks", return_value=None), \
             mock.patch.object(time, "monotonic", side_effect=[0, 100]), \
             mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "")), \
             self.assertRaises(Blocked):
            run.stop_daemon(final=True)
        self.assertIs(run.daemons[0]["stopped"], False)
        self.assertIsNotNone(run.daemon_unverified)

    def test_boolean_secret_scan_stops_write(self):
        self.assertIs(secret_free({"safe": True}), True)
        with self.assertRaisesRegex(Blocked, "secret-free"):
            save(self.root / "unsafe.json", {"value": "Bearer SYNTHETIC_CREDENTIAL"})
        self.assertFalse((self.root / "unsafe.json").exists())

    def test_credential_metadata_never_reads(self):
        target = self.root / "auth.json"
        target.touch(mode=0o600)
        with mock.patch("builtins.open", side_effect=AssertionError("credential read")), \
             mock.patch.object(os, "open", side_effect=AssertionError("credential read")):
            self.assertTrue(credential_metadata(self.root)["private_mode"])
        target.chmod(0o644)
        with self.assertRaises(Blocked):
            credential_metadata(self.root)

    def test_credential_location_survives_private_runner_home(self):
        with mock.patch.dict(os.environ, {"HOME": str(self.root / "private-home"), "CODEX_HOME": ""}), \
             mock.patch.object(pwd, "getpwuid", return_value=argparse.Namespace(pw_dir=str(self.root))):
            run = self.run_object()
        self.assertEqual(run.owner_codex, self.root / ".codex")
        self.assertNotEqual(run.owner_codex.parent, run.home)

    def test_strict_reply_missing_fields_and_boolean_integer(self):
        envelope = fake_envelope()
        conform(envelope, "envelope")
        for key in SCHEMAS["envelope"]:
            bad = envelope.copy()
            del bad[key]
            with self.subTest(key=key), self.assertRaises(Blocked):
                conform(bad, "envelope")
        envelope["turn"] = True
        with self.assertRaises(Blocked):
            conform(envelope, "envelope")

    def test_fake_cli_reply_strict_schema(self):
        run = self.run_object()
        output = subprocess.CompletedProcess([], 0, json.dumps({"pid": True}), "")
        with mock.patch.object(subprocess, "run", return_value=output), self.assertRaises(Blocked):
            run.via_call(run.evidence, "status", "daemon", "status", "--json")

    def test_fake_cli_stop_refusal_schema(self):
        run = self.run_object()
        error = {"code": -32000, "message": "active", "data": {"kind": "sessions_active"}}
        output = subprocess.CompletedProcess([], 2, "", json.dumps(error))
        with mock.patch.object(subprocess, "run", return_value=output):
            self.assertEqual(run.via_call(run.evidence, "stop", "daemon", "stop", "--json")[1], error)

    def test_protocol_duplicate_and_uncorrelated_reply_stop(self):
        wire = Wire("0.160.1")
        message = {"id": 1, "method": "initialize", "params": {}}
        wire.outgoing(message)
        with self.assertRaises(Blocked):
            wire.outgoing(message)
        with self.assertRaises(Blocked):
            wire.incoming({"id": 2, "result": {}})

    def test_mcp_inventory_blocks_before_turn(self):
        with self.assertRaisesRegex(Blocked, "MCP inventory"):
            Wire("0.160.1").outgoing(self.start_message())

    def test_proxy_spending_cap(self):
        wire = self.owned_wire()
        wire.starts = 0
        for number in range(TURN_LIMIT):
            message = self.start_message()
            message["id"] = number + 100
            wire.outgoing(message)
        with self.assertRaisesRegex(Blocked, "spending control"):
            wire.outgoing({**self.start_message(), "id": TURN_LIMIT})

    def test_reviewer_mismatch_stops(self):
        wire = Wire("0.160.1")
        message = self.start_message()
        message["params"]["approvalsReviewer"] = "guardian_subagent"
        with self.assertRaisesRegex(Blocked, "reviewer"):
            wire.outgoing(message)

    def test_no_grant_and_wrong_grant_stop(self):
        wire = self.owned_wire()
        wire.incoming({"id": 1, "method": "item/commandExecution/requestApproval", "params": {"threadId": "t", "turnId": "u"}})
        with self.assertRaisesRegex(Blocked, "granted"):
            wire.outgoing({"id": 1, "result": {"decision": "accept"}})
        wire.incoming({"id": 2, "method": "item/fileChange/requestApproval", "params": {"threadId": "t", "turnId": "u"}})
        fact = wire.outgoing({"id": 2, "result": {"decision": "decline"}})
        self.assertTrue(fact["no_grant"])

    def test_usage_provenance_fixture(self):
        wire = self.owned_wire()
        sample = {"inputTokens": 3, "cachedInputTokens": 1, "outputTokens": 2,
                  "reasoningOutputTokens": 1, "totalTokens": 5}
        message = {"method": "thread/tokenUsage/updated", "params": {
            "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": sample}}}
        fact = wire.incoming(message)
        self.assertEqual(fact["last"], sample)
        del message["params"]["turnId"]
        with self.assertRaisesRegex(Blocked, "correlation"):
            wire.incoming(message)

    def test_line_truncation_and_limit_stop(self):
        with self.assertRaises(Blocked):
            decode_line(b"{}")
        with self.assertRaises(Blocked):
            decode_line(b" " * LINE_BYTES + b"\n")

    def test_events_no_progress_stops(self):
        run = self.run_object()
        page = {"events": [], "more": True, "next_after": 0}
        with mock.patch.object(run, "via_call", return_value=(0, page, None)), \
             self.assertRaisesRegex(Blocked, "stalled"):
            run.events("fake")

    def test_unknown_agent_tool_stops(self):
        message = {"method": "item/started", "params": {"threadId": "t", "turnId": "u",
                   "item": {"type": "collabAgentToolCall"}}}
        with self.assertRaisesRegex(Blocked, "background agent"):
            self.owned_wire().incoming(message)

    def test_bound_proof_requires_the_exact_command(self):
        run = self.run_object()
        envelope = fake_envelope()
        self.assertEqual(command_digest("/bin/bash -lc 'printf tightened > denied.txt'"),
                         digest("printf tightened > denied.txt"))
        unrelated = {"kind": "tool", "finished": True, "exit_code": 1,
                     "read_only_error": True, "command_hash": digest("unrelated failing command")}
        target = self.root / "absent"
        with mock.patch(__name__ + ".turn_facts", return_value=[unrelated]):
            denied_write_observation(run, envelope, "printf tightened > denied.txt", target)
        self.assertFalse(run.record_only[-1]["exact_command_refusal_visible"])
        unrelated["command_hash"] = digest("printf tightened > denied.txt")
        with mock.patch(__name__ + ".turn_facts", return_value=[unrelated]):
            denied_write_observation(run, envelope, "printf tightened > denied.txt", target)
        self.assertTrue(run.record_only[-1]["exact_command_refusal_visible"])

    def test_missing_command_is_not_evidence(self):
        with self.assertRaisesRegex(Blocked, "unverifiable"):
            command_digest(None)

    def probe_fixture(self):
        python = self.root / "python"
        python.write_bytes(b"synthetic pinned interpreter")
        workspace = self.root / "workspace"
        workspace.mkdir()
        server, tool = self.process(1, 0, 10), self.process(2, 1, 20)
        (self.root / "2/exe").symlink_to(python)
        (self.root / "2/cwd").symlink_to(workspace)
        probe = bound_probe.DeniedExecution(python, shared.sha256(python), workspace / "denied.txt")
        return probe, Proc(self.root), server, tool

    def probe_argv(self, argv):
        (self.root / "2/cmdline").write_bytes("\0".join(argv).encode() + b"\0")

    def test_denied_execution_requires_same_owned_process_transition(self):
        probe, proc, server, tool = self.probe_fixture()
        self.probe_argv(probe.denied)
        self.assertIsNone(probe.observe(proc, tool, server))  # Direct sentinel is not proof.
        self.probe_argv(probe.attempt)
        self.assertIsNone(probe.observe(proc, tool, server))
        self.probe_argv(probe.denied)
        fact = probe.observe(proc, tool, server)
        self.assertEqual(fact["errno"], "EROFS")
        self.assertTrue(fact["file_absent"] and secret_free(fact))
        self.assertEqual(fact["attempt_argv_hash"], digest(probe.attempt))
        self.assertEqual(fact["denied_argv_hash"], digest(probe.denied))
        probe.target.write_text("synthetic forbidden write")
        with self.assertRaisesRegex(Blocked, "created a file"):
            probe.observe(proc, tool, server)

    def test_denied_execution_rejects_reuse_cwd_binary_and_foreign_content(self):
        probe, proc, server, tool = self.probe_fixture()
        self.probe_argv(probe.attempt)
        probe.observe(proc, tool, server)
        (self.root / "2/cwd").unlink()
        (self.root / "2/cwd").symlink_to(self.root)
        with self.assertRaisesRegex(Blocked, "identity/cwd mismatch"):
            probe.observe(proc, tool, server)
        (self.root / "2/cwd").unlink()
        (self.root / "2/cwd").symlink_to(probe.target.parent)
        (self.root / "python").write_bytes(b"replaced interpreter")
        with self.assertRaisesRegex(Blocked, "identity/cwd mismatch"):
            probe.observe(proc, tool, server)
        self.process(2, 1, 30)
        with self.assertRaisesRegex(Blocked, "unverifiable identity"):
            probe.observe(proc, tool, server)
        foreign = self.process(3, 0, 40)
        with mock.patch.object(proc, "content") as read, self.assertRaisesRegex(Blocked, "foreign process"):
            probe.observe(proc, foreign, server)
        read.assert_not_called()

    def test_fixed_attempt_enters_sentinel_only_on_erofs(self):
        import errno
        for error in (None, errno.EROFS, errno.EACCES):
            with self.subTest(error=error):
                native_os = mock.Mock(O_WRONLY=1, O_CREAT=2, O_EXCL=4)
                native_os.open.return_value = 42
                if error is not None:
                    native_os.open.side_effect = OSError(error, "synthetic refusal")
                native_sys = mock.Mock(argv=["-c", "denied.txt"], executable="synthetic-python")
                native_time = mock.Mock()
                with mock.patch.dict(sys.modules, {"os": native_os, "sys": native_sys, "time": native_time}):
                    if error == errno.EROFS:
                        exec(bound_probe.ATTEMPT_CODE, {})
                        native_os.execv.assert_called_once_with("synthetic-python", ["synthetic-python", "-c",
                            bound_probe.DENIED_CODE, bound_probe.DENIED_TAG])
                        native_os.write.assert_not_called()
                    elif error is None:
                        with self.assertRaises(SystemExit) as stopped:
                            exec(bound_probe.ATTEMPT_CODE, {})
                        self.assertEqual(stopped.exception.code, 23)
                        native_os.execv.assert_not_called()
                    else:
                        with self.assertRaises(OSError):
                            exec(bound_probe.ATTEMPT_CODE, {})
                        native_os.execv.assert_not_called()
                native_os.open.assert_called_once_with("denied.txt", 7, 0o600)
                native_time.sleep.assert_called_once_with(3)

    def test_fixed_tool_program_modes_with_mocked_system_calls(self):
        import errno
        modes = ("allowed", "denied", "never-ask", "denied-refused", "never-ask-refused",
                 "interrupt-a", "interrupt-b")
        for mode in modes:
            errors = (None, errno.EROFS, errno.EACCES) if mode in ("denied", "never-ask") else (None,)
            for error in errors:
                with self.subTest(mode=mode, error=error):
                    native_os = mock.Mock(O_WRONLY=1, O_CREAT=2, O_EXCL=4)
                    native_os.open.return_value = 42
                    if error is not None:
                        native_os.open.side_effect = OSError(error, "synthetic refusal")
                    native_sys = mock.Mock(argv=["synthetic-script"] + (["1"] if mode.endswith("-refused") else []),
                                           executable="synthetic-python")
                    native_time, entropy = mock.Mock(), mock.Mock(token_hex=mock.Mock(return_value="b" * 32))
                    request = {"action": mode.removesuffix("-refused"), "field": "r", "seed": "VIAQUAL" + "a" * 32}
                    scope = {"REQUEST": request}
                    with mock.patch.dict(sys.modules, {"os": native_os, "sys": native_sys,
                         "time": native_time, "secrets": entropy}), mock.patch("builtins.print") as printed:
                        if error == errno.EACCES:
                            with self.assertRaises(OSError):
                                exec(bound_probe.TOOL_CODE, scope)
                        elif mode in ("denied", "never-ask") and error is None:
                            with self.assertRaises(SystemExit) as stopped:
                                exec(bound_probe.TOOL_CODE, scope)
                            self.assertEqual(stopped.exception.code, 23)
                        elif mode.endswith("-refused"):
                            with self.assertRaises(SystemExit) as stopped:
                                exec(bound_probe.TOOL_CODE, scope)
                            self.assertEqual(stopped.exception.code, 0)
                        else:
                            exec(bound_probe.TOOL_CODE, scope)
                    if mode == "allowed":
                        native_os.open.assert_called_once_with("allowed.txt", 7, 0o600)
                        native_os.write.assert_called_once_with(42, b"allowed")
                    elif mode in ("denied", "never-ask"):
                        native_os.open.assert_called_once_with("denied.txt" if mode == "denied" else "forbidden.txt",
                                                               7, 0o600)
                        if error == errno.EROFS:
                            native_os.execv.assert_called_once_with("synthetic-python",
                                ["synthetic-python", "synthetic-script", "1"])
                            native_os.write.assert_not_called()
                        else:
                            native_os.execv.assert_not_called()
                    else:
                        native_os.open.assert_not_called()
                    if mode in ("allowed", "interrupt-a", "interrupt-b") or mode.endswith("-refused"):
                        printed.assert_called_once()
                        self.assertIsNotNone(tool_line_fact(printed.call_args.args[0])["nonce_hash"])
                        self.assertNotIn(request["seed"], printed.call_args.args[0])
                    else:
                        printed.assert_not_called()
                    seconds = {"interrupt-a": bound_probe.A_SLEEP_S, "interrupt-b": bound_probe.B_SLEEP_S}.get(mode,
                               bound_probe.DENIED_OBSERVE_S if mode.endswith("-refused") else bound_probe.ATTEMPT_OBSERVE_S)
                    native_time.sleep.assert_called_once_with(seconds)

    def test_short_command_script_identity_refusal_and_tamper(self):
        legacy, proc, server, tool = self.probe_fixture()
        program, pinned = bound_probe.write_program(self.root)
        bound_probe.verify_program(program, pinned)
        attempt = [legacy.python, str(program), "denied"]
        denied = [legacy.python, str(program), "denied-refused"]
        probe = bound_probe.DeniedExecution(legacy.python, legacy.python_hash, legacy.target,
                   attempt, denied, program, pinned)
        self.probe_argv(denied)
        self.assertIsNone(probe.observe(proc, tool, server))
        self.probe_argv(attempt)
        self.assertIsNone(probe.observe(proc, tool, server))
        self.probe_argv(denied)
        fact = probe.observe(proc, tool, server)
        self.assertEqual(fact["errno"], "EROFS")
        self.assertEqual(fact["program_hash"], pinned)
        self.assertTrue(secret_free(fact))
        program.write_text("synthetic tampered program")
        with self.assertRaisesRegex(Blocked, "program hash"):
            probe.observe(proc, tool, server)

    def test_allowed_script_process_requires_exact_argv_and_program(self):
        legacy, proc, server, tool = self.probe_fixture()
        program, pinned = bound_probe.write_program(self.root)
        attempt = [legacy.python, str(program), "allowed", "VIAQUALsynthetic"]
        probe = bound_probe.DeniedExecution(legacy.python, legacy.python_hash, legacy.target,
                   attempt=attempt, program=program, program_hash=pinned, require_refusal=False)
        self.probe_argv([*attempt, "unexpected argument"])
        self.assertIsNone(probe.observe(proc, tool, server))
        self.probe_argv(attempt)
        self.assertEqual(probe.observe(proc, tool, server)["program_hash"], pinned)
        program.chmod(0o644)
        with self.assertRaisesRegex(Blocked, "program unsafe"):
            probe.observe(proc, tool, server)

    def test_run_observer_uses_short_script_argv_and_saves_pinned_evidence(self):
        legacy, proc, server, tool = self.probe_fixture()
        run = self.run_object()
        generation, thread, turn = digest("generation"), digest("thread"), digest("turn")
        facts = [{"kind": "reply", "method": "turn/start", "reservation": digest("c2"),
                  "trace": generation, "thread": thread, "turn": turn},
                 {"kind": "request", "method": "turn/start", "reservation": digest("c2"), "bound": "readOnly"},
                 {"kind": "process", "trace": generation, **server}]
        with mock.patch.object(sys, "executable", legacy.python):
            command = run.fixed_command("c2", "denied")
            self.probe_argv(run.command_specs["c2"]["argv"])
            def transitioned(_):
                self.probe_argv(run.command_specs["c2"]["denied"])
            with mock.patch(__name__ + ".Proc", return_value=proc), \
                 mock.patch.object(run, "facts", return_value=facts), \
                 mock.patch.object(os, "listdir", return_value=["2"]), \
                 mock.patch.object(time, "sleep", side_effect=transitioned):
                observed = run.observe_denied_execution("c2", legacy.target)
        self.assertEqual(observed["errno"], "EROFS")
        self.assertEqual(observed["program_hash"], run.command_specs["c2"]["program_hash"])
        survey = json.loads((run.evidence / "changed-bound-survey.json").read_text())
        self.assertEqual(survey["expected_command_hash"], digest(command))
        self.assertEqual(survey["expected_program_hash"], run.command_specs["c2"]["program_hash"])
        self.assertEqual(survey["scan_count"], 2)
        self.assertTrue(all(row["contains_attempt_program"] for row in survey["observations"]))
        self.assertTrue(secret_free(survey))

    def test_fixed_commands_are_single_short_argv_and_prompt(self):
        run = self.run_object()
        for label, mode in (("c1", "allowed"), ("c2", "denied"), ("a1", "interrupt-a"),
                            ("b1", "interrupt-b"), ("n1", "never-ask")):
            with self.subTest(label=label):
                command = run.fixed_command(label, mode, "VIAQUALsynthetic" if label == "c1" else None)
                self.assertEqual(bound_probe.command_argv(command), run.command_specs[label]["argv"])
                self.assertNotIn("\n", command)
                self.assertLess(len(command), 256)
                prompt = fixed_prompt(command)
                self.assertEqual(prompt.count(command), 1)
                self.assertNotIn(bound_probe.TOOL_CODE, prompt)

    def test_tool_reply_nonce_is_hidden_from_prompt_and_argv(self):
        run = self.run_object()
        nonce = "VIAQUAL" + "a" * 32
        command = run.fixed_command("c1", "allowed", nonce)
        self.assertNotIn(nonce, fixed_prompt(command))
        self.assertNotIn(nonce, command)

    def test_tool_argv_is_neutral_and_prompt_names_no_expected_answer(self):
        run = self.run_object()
        for mode in ("allowed", "denied", "interrupt-a", "interrupt-b", "never-ask"):
            command = run.fixed_command(mode, mode)
            with self.subTest(mode=mode):
                self.assertFalse(any(hint in command for hint in ("allowed", "denied", "write", "interrupt", "never-ask")))
                prompt = fixed_prompt(command)
                self.assertIn("exactly the tool's single output line", prompt)
                self.assertNotIn("written", prompt)
                self.assertNotIn("blocked", prompt)

    def reask_fixture(self, misses=1, deviation=None, effect=False, interrupt=False):
        """Actual submission/settlement/ledger with fake C1 and native wire peers."""
        run = self.run_object()
        run.vendor_args = []
        ws = run.work / "case"
        ws.mkdir()
        command = run.fixed_command("c1", "allowed")
        prompt = fixed_prompt(command)
        generation = digest((9, 90))
        wire = Wire("0.160.1", generation=generation, reservations=run.reservations)
        wire.inventory = True
        launched, envelopes = [], {}
        def facts(*args, **kwargs):
            return [{**fact, "trace": generation, "seq": index + 1} for index, fact in enumerate(
                [{"kind": "process", "pid": 9, "start_ticks": 90}] + wire.facts)]
        def cli(directory, label, *args, **kwargs):
            method = args[0]
            if method in ("spawn", "resume"):
                ordinal = len(launched) + 1
                launched.append((label, method, args))
                text = args[args.index("--prompt") + 1]
                if ordinal == 1:
                    wire.outgoing({"id": 10, "method": "thread/start", "params": {"model": MODEL,
                        "approvalPolicy": "never", "approvalsReviewer": "user", "sandbox": "workspace-write",
                        "cwd": str(ws)}})
                    wire.incoming({"id": 10, "result": {"model": MODEL, "approvalPolicy": "never",
                        "approvalsReviewer": "user", "thread": {"id": "t"}}})
                params = self.start_message()["params"]
                params.update(cwd=str(ws), sandboxPolicy={"type": "workspaceWrite"},
                              input=[{"type": "text", "text": text}])
                wire.outgoing({"id": 20 + ordinal, "method": "turn/start", "params": params})
                turn = "u" + str(ordinal)
                wire.incoming({"id": 20 + ordinal, "result": {"turn": {"id": turn}}})
                nonce = "VIAQUAL" + format(ordinal, "032x")
                line = json.dumps({"a": nonce}, separators=(",", ":"))
                if ordinal > misses or deviation == "command":
                    for method in ("item/started", "item/completed"):
                        wire.incoming({"method": method, "params": {"threadId": "t", "turnId": turn,
                            "item": {"id": "item" + str(ordinal), "type": "commandExecution", "command": command,
                                     "exitCode": 2 if deviation == "command" else 0,
                                     "aggregatedOutput": line + "\n" if method == "item/completed" else None}}})
                if deviation in ("error", "thread_warning", "global_warning"):
                    params = {"threadId": None if deviation == "global_warning" else "t"}
                    if deviation == "error":
                        params.update(turnId=turn, error={"message": "synthetic private detail"})
                    wire.incoming({"method": "error" if deviation == "error" else "warning", "params": params})
                if deviation == "server_request":
                    wire.incoming({"id": 900, "method": "item/tool/requestUserInput",
                                   "params": {"threadId": "t", "turnId": turn}})
                last = {"inputTokens": 3, "cachedInputTokens": 1, "outputTokens": 2,
                        "reasoningOutputTokens": 1, "totalTokens": 5}
                total = {key: value * ordinal for key, value in last.items()}
                wire.incoming({"method": "thread/tokenUsage/updated", "params": {"threadId": "t", "turnId": turn,
                               "tokenUsage": {"last": last, "total": total}}})
                wire.incoming({"method": "turn/completed", "params": {"threadId": "t",
                               "turn": {"id": turn, "status": "completed"}}})
                envelope = fake_envelope()
                envelope.update(session_id="s", vendor_session_id="t", turn=ordinal,
                                final_text=line if ordinal > misses else "guess",
                                usage={"input_tokens": 3, "cached_input_tokens": 1, "output_tokens": 2,
                                       "reasoning_output_tokens": 1, "total_tokens": 5,
                                       "scope": "turn", "provenance": "reported"}, vendor={"total": total})
                envelopes[label] = envelope
                if effect:
                    (ws / "allowed.txt").write_text("allowed")
                return 0, {"session_id": "s", "turn": "s/" + str(ordinal), "handle": "fake"}, None
            return 0, envelopes[label], None
        def observe(label, *args, **kwargs):
            if len(launched) <= misses:
                if interrupt:
                    shared.record_signal(signal.SIGTERM, None)
                return None
            return {"pid": 2, "start_ticks": 20, "program_hash": run.command_specs[label]["program_hash"]}
        with mock.patch.object(run, "via_call", side_effect=cli), \
             mock.patch.object(run, "facts", side_effect=facts), \
             mock.patch.object(run, "observe_denied_execution", side_effect=observe):
            label, session, observed = run.start_tool("c1", "spawn", *spawn_args(run, ws, prompt),
                target=ws / "allowed.txt", bound="workspaceWrite", refusal=False)
            envelope = run.finish(label)
            run.command_protocol(label, envelope)
            run.tool_reply(label, envelope)
            usage(run)
        return run, wire, launched, facts(), envelopes

    def test_one_zero_command_reask_preserves_prompt_thread_and_full_accounting(self):
        run, wire, launched, facts, envelopes = self.reask_fixture()
        self.assertEqual([(label, method) for label, method, _ in launched], [("c1", "spawn"), ("c1-reask", "resume")])
        prompts = [args[args.index("--prompt") + 1] for _, _, args in launched]
        self.assertEqual(prompts[0], prompts[1])
        self.assertEqual(len(run.envelopes), 2)
        accounting = json.loads((run.evidence / "accounting.json").read_text())
        self.assertEqual([row["turn"] for row in accounting], [1, 2])
        self.assertEqual([row["usage"]["total_tokens"] for row in accounting], [5, 5])
        self.assertEqual([row["thread_total"]["totalTokens"] for row in accounting], [5, 10])
        self.assertEqual((run.spend.used, run.spend.reasks, run.spend.active), (2, 1, set()))
        rows = run.reservations.snapshot()["rows"]
        self.assertTrue(rows[digest("c1")]["miss"])
        self.assertEqual(rows[digest("c1-reask")]["reask_of"], digest("c1"))
        self.assertEqual(rows[digest("c1")]["thread"], rows[digest("c1-reask")]["thread"])
        evidence = json.loads((run.evidence / "reask-c1.json").read_text())
        self.assertEqual(evidence["reason"], "zero_commands")
        self.assertTrue(secret_free(evidence))
        with self.assertRaisesRegex(Blocked, "fixed command deviation"):
            wire.incoming({"method": "item/started", "params": {"threadId": "t", "turnId": "u1",
                "item": {"id": "late", "type": "commandExecution", "command": shlex.join(run.command_specs["c1"]["argv"])}}})

    def test_reask_second_miss_or_any_other_deviation_blocks(self):
        cases = ((2, None, False, "second tool miss"), (1, "command", False, "pure tool miss"),
                 (1, "error", False, "pure tool miss"), (1, "thread_warning", False, "pure tool miss"),
                 (1, "global_warning", False, "pure tool miss"), (1, "server_request", False, "pure tool miss"),
                 (1, None, True, "filesystem effect"))
        for misses, deviation, effect, reason in cases:
            with self.subTest(reason=reason, deviation=deviation), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    with self.assertRaisesRegex(Blocked, reason):
                        self.reask_fixture(misses, deviation, effect)
                finally:
                    self.root = previous

    def test_signal_after_a_miss_forbids_reask_submission(self):
        with self.assertRaisesRegex(Blocked, "interrupted"):
            self.reask_fixture(interrupt=True)

    def test_late_command_cannot_retroactively_authorize_a_zero_command_reask(self):
        run, wire, launched, facts, envelopes = self.reask_fixture()
        old = next(fact for fact in facts if fact["kind"] == "reply" and fact.get("turn") == digest("u1"))
        facts.append({"kind": "tool", "thread": old["thread"], "turn": old["turn"], "trace": old["trace"]})
        with self.assertRaisesRegex(Blocked, "final pure tool miss"):
            reservation_accounting(run, facts)

    def test_output_echo_is_paired_and_never_substitutes_for_execution(self):
        run = self.run_object()
        run.fixed_command("b1", "interrupt-b")
        nonce = "VIAQUAL" + "a" * 32
        line = json.dumps({"r": nonce}, separators=(",", ":"))
        fact = {"kind": "tool", "finished": True, **tool_line_fact(line + "\n")}
        envelope = fake_envelope()
        envelope["final_text"] = line
        with mock.patch(__name__ + ".turn_facts", return_value=[fact]):
            self.assertEqual(run.tool_reply("b1", envelope), digest(nonce))
            with self.assertRaisesRegex(Blocked, "fixed command identity"):
                run.command_protocol("b1", envelope)
            envelope["final_text"] = line.replace("a" * 32, "b" * 32)
            with self.assertRaisesRegex(Blocked, "tool output seen"):
                run.tool_reply("b1", envelope)
        self.assertTrue(secret_free(fact))
        self.assertNotIn(nonce, json.dumps(fact))

    def test_nonce_is_fresh_even_when_the_exact_program_is_reasked(self):
        native_sys = mock.Mock(argv=["synthetic-script"], executable="synthetic-python")
        entropy = mock.Mock(token_hex=mock.Mock(side_effect=["a" * 32, "b" * 32]))
        request = {"action": "interrupt-a", "field": "r", "seed": "VIAQUAL" + "c" * 32}
        with mock.patch.dict(sys.modules, {"sys": native_sys, "time": mock.Mock(), "secrets": entropy}), \
             mock.patch("builtins.print") as printed:
            for _ in range(2):
                exec(bound_probe.TOOL_CODE, {"REQUEST": request})
        lines = [call.args[0] for call in printed.call_args_list]
        self.assertEqual(len(lines), 2)
        self.assertNotEqual(lines[0], lines[1])
        self.assertTrue(all(tool_line_fact(line)["nonce_hash"] is not None for line in lines))

    def test_nonce_line_schema_rejects_extra_output_and_predictable_answers(self):
        for output in ('{"written":true}', '{"blocked":true}', '{"r":"guess"}',
                       '{"r":"VIAQUAL' + 'a' * 32 + '"}\nextra', 'private ' + 'sk-' + 'x' * 48):
            with self.subTest(output_hash=digest(output)):
                self.assertIsNone(tool_line_fact(output)["output_line_hash"])

    def test_wire_fixed_command_blocks_before_forwarding_even_when_deferred(self):
        for known in (True, False):
            wire = self.owned_wire()
            wire.reservations = mock.Mock()
            wire.reservations.claim.return_value = digest("synthetic reservation")
            wire.reservations.tool.return_value = {"argv": digest(["synthetic-python", "fixed-script"])}
            turn = "u" if known else "pending-turn"
            if not known:
                wire.outgoing({**self.start_message(), "id": 2})
            for command in ("synthetic-python fixed-script", "synthetic-python different-script"):
                with self.subTest(known=known, command=command):
                    message = {"method": "item/started", "params": {"threadId": "t", "turnId": turn,
                        "item": {"id": "command", "type": "commandExecution", "command": command}}}
                    if command.endswith("fixed-script"):
                        wire.incoming(message)
                    else:
                        with self.assertRaisesRegex(Blocked, "fixed command deviation"):
                            wire.incoming(message)
                        self.assertEqual(wire.facts[-1]["argv_hash"], digest(command.split()))
                        self.assertTrue(secret_free(wire.facts[-1]))
            self.assertEqual(len(wire.deferred), 0 if known else 1)

    def test_protocol_duplicate_nonzero_and_no_tool_command_block(self):
        run = self.run_object()
        run.fixed_command("c1", "allowed", "VIAQUALsynthetic")
        started = {"kind": "tool", "item_hash": digest("one item"), "finished": False,
                   "argv_hash": digest(run.command_specs["c1"]["argv"]), "exit_code": None}
        ended = {**started, "finished": True, "exit_code": 0}
        for tools in ([started, ended, started], [started, {**ended, "exit_code": 2}],
                      [started, {**ended, "item_hash": digest("different item")}], []):
            with self.subTest(tools=tools), mock.patch(__name__ + ".turn_facts", return_value=tools), \
                 self.assertRaisesRegex(Blocked, "fixed command"):
                run.command_protocol("c1", fake_envelope())
        with mock.patch(__name__ + ".turn_facts", return_value=[started]), \
             self.assertRaisesRegex(Blocked, "no tool command"):
            run.command_protocol("c3", fake_envelope())
        with mock.patch(__name__ + ".turn_facts", return_value=[started]):
            run.command_protocol("c1", fake_envelope(), finished=False)

    def test_workspace_marker_records_mismatch_unsafe_and_verified(self):
        marker, evidence = self.root / "marker", self.root / "marker-proof.json"
        for text, status in (("wrong", "content_mismatch"), ("allowed", "verified")):
            evidence.unlink(missing_ok=True)
            marker.write_text(text)
            self.assertEqual(workspace_marker(marker, evidence), status == "verified")
            self.assertEqual(json.loads(evidence.read_text())["status"], status)
        marker.unlink()
        marker.symlink_to(self.root / "foreign-marker")
        evidence.unlink()
        with self.assertRaisesRegex(Blocked, "workspace marker"):
            workspace_marker(marker, evidence)
        self.assertEqual(json.loads(evidence.read_text())["status"], "unsafe")

    def test_workspace_marker_records_unreadable_errno_without_error_text(self):
        evidence = self.root / "marker-proof.json"
        with mock.patch.object(os, "stat", side_effect=PermissionError(13, "synthetic private detail")), \
             self.assertRaisesRegex(Blocked, "workspace marker"):
            workspace_marker(self.root / "marker", evidence)
        recorded = json.loads(evidence.read_text())
        self.assertEqual(recorded["status"], "unreadable")
        self.assertEqual(recorded["errno"], 13)
        self.assertNotIn("synthetic private detail", evidence.read_text())

    def test_unobserved_attempt_at_native_completion_blocks(self):
        run = self.run_object()
        generation, thread, turn = digest("generation"), digest("thread"), digest("turn")
        facts = [{"kind": "reply", "method": "turn/start", "reservation": digest("c2"),
                  "trace": generation, "thread": thread, "turn": turn},
                 {"kind": "request", "method": "turn/start", "reservation": digest("c2"), "bound": "readOnly"},
                 {"kind": "process", "trace": generation, "pid": 1, "start_ticks": 10},
                 {"kind": "terminal", "trace": generation, "thread": thread, "turn": turn}]
        # Run 3 shape: one commandExecution start/completion, exit 2, no EROFS phrase.
        facts.extend({"kind": "tool", "trace": generation, "thread": thread, "turn": turn,
                      "item_type": "commandExecution", "finished": finished,
                      "command_hash": digest("synthetic unrecognized command"),
                      "exit_code": 2 if finished else None, "read_only_error": False}
                     for finished in (False, True))
        facts.append({"kind": "tool", "trace": generation, "thread": thread,
                      "turn": digest("older turn"), "exit_code": 0})
        with mock.patch.object(run, "facts", return_value=facts), mock.patch.object(os, "listdir", return_value=[]), \
             mock.patch.object(time, "sleep") as sleep, self.assertRaisesRegex(Blocked, "not observed"):
            run.observe_denied_execution("c2", self.root / "denied.txt")
        sleep.assert_not_called()
        self.assertTrue((run.evidence / "changed-bound-survey.json").exists())
        recorded = json.loads((run.evidence / "changed-bound-survey.json").read_text())
        self.assertEqual(len(recorded["tools"]), 2)
        self.assertEqual(recorded["tools"][-1]["exit_code"], 2)
        self.assertIs(recorded["tools"][-1]["read_only_error"], False)

    def test_zero_command_completed_turn_is_distinguished_for_bounded_reask(self):
        run = self.run_object()
        generation, thread, turn = digest("generation"), digest("thread"), digest("turn")
        facts = [{"kind": "reply", "method": "turn/start", "reservation": digest("c2"),
                  "trace": generation, "thread": thread, "turn": turn},
                 {"kind": "request", "method": "turn/start", "reservation": digest("c2"), "bound": "readOnly"},
                 {"kind": "process", "trace": generation, "pid": 1, "start_ticks": 10},
                 {"kind": "item", "trace": generation, "thread": thread, "turn": turn,
                  "item_type": "agentMessage"},
                 {"kind": "terminal", "trace": generation, "thread": thread, "turn": turn, "status": "completed"}]
        with mock.patch.object(run, "facts", return_value=facts), \
             mock.patch.object(os, "listdir", return_value=[]):
            self.assertIsNone(run.observe_denied_execution("c2", self.root / "denied.txt"))

    def test_scoped_error_is_retained_so_zero_commands_cannot_hide_a_deviation(self):
        wire = self.owned_wire()
        wire.incoming({"method": "error", "params": {"threadId": "t", "turnId": "u",
                       "error": {"message": "synthetic private detail"}}})
        self.assertEqual(wire.facts[-1]["kind"], "notification")
        self.assertEqual(wire.facts[-1]["method_hash"], digest("error"))
        self.assertEqual(wire.facts[-1]["turn"], digest("u"))
        self.assertNotIn("synthetic private detail", json.dumps(wire.facts))

    def test_server_request_scope_is_retained_for_pure_miss_classification(self):
        wire = self.owned_wire()
        fact = wire.incoming({"id": 900, "method": "item/tool/requestUserInput",
                              "params": {"threadId": "t", "turnId": "u"}})
        self.assertEqual(fact.get("thread"), digest("t"))
        self.assertEqual(fact.get("turn"), digest("u"))

    def test_tool_diagnostics_record_item_type_time_and_fixed_error_flags(self):
        wire = self.owned_wire()
        for method in ("item/started", "item/completed"):
            with mock.patch.object(time, "monotonic", return_value=123.456):
                fact = wire.incoming({"method": method, "params": {"threadId": "t", "turnId": "u",
                    "item": {"id": "synthetic-command", "type": "commandExecution",
                             "command": bound_probe.command(sys.executable, Path("denied.txt")),
                             "exitCode": 2 if method == "item/completed" else None,
                             "aggregatedOutput": "SyntaxError: synthetic " + "sk-" + "x" * 48}}})
            self.assertEqual(fact.get("item_type"), "commandExecution")
            self.assertEqual(fact.get("at_ms"), 123456)
            self.assertEqual(fact.get("item_hash"), digest("synthetic-command"))
            self.assertIs(fact.get("fixed_attempt_argv"), True)
            self.assertTrue(fact.get("error_flags", {}).get("syntax_error"))
            self.assertTrue(secret_free(fact))
        item = wire.incoming({"method": "item/completed", "params": {"threadId": "t", "turnId": "u",
                              "item": {"id": "synthetic-agent", "type": "agentMessage"}}})
        self.assertEqual(item.get("kind"), "item")
        self.assertEqual(item.get("item_type"), "agentMessage")

    def test_deferred_tool_diagnostics_keep_arrival_time(self):
        wire = self.owned_wire()
        message = self.start_message()
        message["id"] = 2
        wire.outgoing(message)
        with mock.patch.object(time, "monotonic", return_value=100):
            wire.incoming({"method": "item/started", "params": {"threadId": "t", "turnId": "next",
                           "item": {"type": "commandExecution", "command": "synthetic command"}}})
        with mock.patch.object(time, "monotonic", return_value=105):
            wire.incoming({"id": 2, "result": {"turn": {"id": "next"}}})
        tool = next(fact for fact in wire.facts if fact["kind"] == "tool")
        self.assertEqual(tool["at_ms"], 100000)

    def test_bound_survey_bounds_and_cadence(self):
        probe, proc, server, tool = self.probe_fixture()
        with mock.patch.object(time, "monotonic", return_value=0):
            survey = bound_probe.Survey(probe)
            survey.scan()
        with mock.patch.object(time, "monotonic", return_value=1.25):
            survey.scan()
        with mock.patch.object(time, "monotonic", return_value=2):
            survey.scan()
        with mock.patch.object(bound_probe, "SURVEY_RECORDS", 1):
            survey.observe(proc, tool, ["synthetic-command"])
            with self.assertRaisesRegex(Blocked, "survey evidence bound"):
                survey.observe(proc, tool, ["different-command"])
        snapshot = survey.snapshot("blocked", {}, [])
        self.assertEqual(snapshot["scan_count"], 3)
        self.assertEqual(snapshot["max_scan_gap_ms"], 1250)
        self.assertEqual(len(snapshot["observations"]), 1)
        self.assertTrue(secret_free(snapshot))

    def test_failed_bound_survey_records_owned_wrapper_without_accepting_it(self):
        probe, proc, server, tool = self.probe_fixture()
        run = self.run_object()
        self.process(3, 0, 30)
        wrapper = ["bwrap", "--", *probe.attempt, "sk-" + "x" * 48]
        self.probe_argv(wrapper)
        generation, thread, turn = digest("generation"), digest("thread"), digest("turn")
        facts = [{"kind": "reply", "method": "turn/start", "reservation": digest("c2"),
                  "trace": generation, "thread": thread, "turn": turn},
                 {"kind": "request", "method": "turn/start", "reservation": digest("c2"), "bound": "readOnly"},
                 {"kind": "process", "trace": generation, **server},
                 {"kind": "terminal", "trace": generation, "thread": thread, "turn": turn}]
        with mock.patch.object(sys, "executable", probe.python), mock.patch(__name__ + ".Proc", return_value=proc), \
             mock.patch.object(run, "facts", return_value=facts), mock.patch.object(os, "listdir", return_value=["2", "3"]), \
             mock.patch.object(proc, "content", wraps=proc.content) as content, \
             self.assertRaisesRegex(Blocked, "not observed"):
            run.observe_denied_execution("c2", probe.target)
        path = run.evidence / "changed-bound-survey.json"
        self.assertTrue(path.exists())
        survey = json.loads(path.read_text())
        self.assertEqual(survey["outcome"], "blocked")
        self.assertEqual(survey["context"]["trace"], generation)
        self.assertEqual(survey["scan_count"], 1)
        self.assertEqual(len(survey["observations"]), 1)
        sample = survey["observations"][0]
        self.assertEqual(sample["argv_role"], "bwrap")
        self.assertEqual(sample["argv_hash"], digest(wrapper))
        self.assertTrue(sample["contains_attempt_program"])
        self.assertTrue(sample["cwd_matches"])
        self.assertFalse(sample["attempt_argv_matches"])
        self.assertTrue(secret_free(survey))
        self.assertNotIn("sk-" + "x" * 48, path.read_text())
        self.assertFalse(any(call.args[0]["pid"] == 3 for call in content.call_args_list))

    def test_bound_observer_pairs_server_and_tracks_attempt_even_on_failure(self):
        probe, proc, server, tool = self.probe_fixture()
        foreign = self.process(3, 0, 40)
        run = self.run_object()
        generation, thread, turn = digest("generation"), digest("thread"), digest("turn")
        facts = [{"kind": "reply", "method": "turn/start", "reservation": digest("c2"),
                  "trace": generation, "thread": thread, "turn": turn},
                 {"kind": "request", "method": "turn/start", "reservation": digest("c2"), "bound": "readOnly"},
                 {"kind": "process", "trace": generation, **server}]
        self.probe_argv(probe.attempt)
        def transitioned(_):
            self.probe_argv(probe.denied)
        with mock.patch.object(sys, "executable", probe.python), mock.patch(__name__ + ".Proc", return_value=proc), \
             mock.patch.object(run, "facts", return_value=facts), mock.patch.object(os, "listdir", return_value=["2", "3"]), \
             mock.patch.object(time, "sleep", side_effect=transitioned), \
             mock.patch.object(proc, "content", wraps=proc.content) as content:
            observed = run.observe_denied_execution("c2", probe.target)
        self.assertEqual(observed["trace"], generation)
        self.assertEqual(observed["turn"], turn)
        self.assertEqual(observed["reservation"], digest("c2"))
        self.assertTrue(secret_free(json.loads((run.evidence / "changed-bound-execution.json").read_text())))
        self.assertFalse(any(call.args[0] == foreign for call in content.call_args_list))
        run.owned.clear()
        (run.evidence / "changed-bound-survey.json").unlink()
        self.probe_argv(probe.attempt)
        facts.append({"kind": "terminal", "trace": generation, "thread": thread, "turn": turn})
        with mock.patch.object(sys, "executable", probe.python), mock.patch(__name__ + ".Proc", return_value=proc), \
             mock.patch.object(run, "facts", return_value=facts), mock.patch.object(os, "listdir", return_value=["2"]), \
             self.assertRaisesRegex(Blocked, "not observed"):
            run.observe_denied_execution("c2", probe.target)
        self.assertIn((tool["pid"], tool["start_ticks"]), run.owned)

    def test_native_start_without_id_never_bypasses_reservation(self):
        run = self.run_object()
        wire = Wire("0.160.1", reservations=run.reservations)
        wire.inventory = True
        with self.assertRaisesRegex(Blocked, "native start requires request id"):
            wire.outgoing({"method": "thread/start", "params": {"model": MODEL,
                "approvalPolicy": "never", "approvalsReviewer": "user", "sandbox": "read-only",
                "cwd": "/synthetic/work"}})

    def test_bound_observation_deadline_is_checked_inside_process_survey(self):
        run = self.run_object()
        generation, thread, turn = digest("generation"), digest("thread"), digest("turn")
        facts = [{"kind": "reply", "method": "turn/start", "reservation": digest("c2"),
                  "trace": generation, "thread": thread, "turn": turn},
                 {"kind": "request", "method": "turn/start", "reservation": digest("c2"), "bound": "readOnly"},
                 {"kind": "process", "trace": generation, "pid": 1, "start_ticks": 10},
                 {"kind": "terminal", "trace": generation, "thread": thread, "turn": turn}]
        now = [0]
        def expired(*_):
            now[0] = POLL_S + 1
            return None, None
        proc = mock.Mock(open=mock.Mock(side_effect=expired))
        with mock.patch(__name__ + ".Proc", return_value=proc), mock.patch.object(run, "facts", return_value=facts), \
             mock.patch.object(os, "listdir", return_value=["2", "3"]), \
             mock.patch.object(time, "monotonic", side_effect=lambda: now[0]), \
             self.assertRaisesRegex(Blocked, "observation deadline"):
            run.observe_denied_execution("c2", self.root / "denied.txt")
        self.assertEqual(proc.open.call_count, 1)

    def test_swapped_receipts_cannot_swap_native_turns(self):
        run = self.run_object()
        one, two = fake_envelope(), fake_envelope()
        two["turn"] = 2
        thread = digest(one["vendor_session_id"])
        facts = [{"kind": "reply", "method": "turn/start", "thread": thread, "turn": digest(index)}
                 for index in (1, 2)]
        run.envelopes = [one, two]
        self.seed_accounting(run, run.envelopes, facts)
        keys = [digest(f"fixture-{index}") for index in (0, 1)]
        with run.reservations.transaction() as state:
            rows = state["rows"]
            for field in ("receipt", "envelope"):
                rows[keys[0]][field], rows[keys[1]][field] = rows[keys[1]][field], rows[keys[0]][field]
        run.receipts["fixture-0"], run.receipts["fixture-1"] = run.receipts["fixture-1"], run.receipts["fixture-0"]
        with self.assertRaisesRegex(Blocked, "receipt native turn exact"):
            reservation_accounting(run, facts)

    def test_final_accounting_rechecks_tool_commands_after_settlement(self):
        run = self.run_object()
        envelope = fake_envelope()
        thread, turn = digest(envelope["vendor_session_id"]), digest("native turn")
        facts = [{"kind": "reply", "method": "turn/start", "thread": thread, "turn": turn}]
        run.envelopes = [envelope]
        self.seed_accounting(run, run.envelopes, facts)
        run.fixed_command("fixture-0", "allowed", "synthetic nonce")
        expected = digest(run.command_specs["fixture-0"]["argv"])
        with run.reservations.transaction() as state:
            state["rows"][digest("fixture-0")]["tool"] = {"argv": expected}
        trace = facts[0]["trace"]
        facts.extend({"kind": "tool", "trace": trace, "thread": thread, "turn": turn,
                      "item_hash": digest("one command"), "argv_hash": expected,
                      "finished": finished, "exit_code": 0} for finished in (False, True))
        with mock.patch.object(run, "facts", return_value=facts):
            run.command_protocol("fixture-0", envelope)
        facts.append({"kind": "tool", "trace": trace, "thread": thread, "turn": turn,
                      "item_hash": digest("late extra command"), "argv_hash": expected,
                      "finished": False, "exit_code": None})
        with self.assertRaisesRegex(Blocked, "fixed command identity"):
            reservation_accounting(run, facts)

    def test_complete_trace_required(self):
        run = self.run_object()
        path = run.trace_dir / "fake.jsonl"
        path.write_text(json.dumps({"seq": 1, "kind": "process", "pid": 2, "start_ticks": 20}) + "\n")
        with self.assertRaisesRegex(Blocked, "trace end"):
            run.facts(final=True)
        with path.open("a") as file:
            file.write('{"seq":2')
        with self.assertRaisesRegex(Blocked, "truncated"):
            run.facts(final=True)

    def test_usage_whole_samples_and_wrong_sum(self):
        run = self.run_object()
        envelope = fake_envelope()
        thread, turn = digest(envelope["vendor_session_id"]), digest("u")
        sample = {"inputTokens": 3, "cachedInputTokens": 1, "outputTokens": 2,
                  "reasoningOutputTokens": 1, "totalTokens": 5}
        total = {key: value * 2 for key, value in sample.items()}
        facts = [{"kind": "reply", "method": "turn/start", "thread": thread, "turn": turn},
                 {"kind": "terminal", "thread": thread, "turn": turn},
                 *[{"kind": "usage", "thread": thread, "turn": turn,
                    "last": sample, "total": total} for _ in range(2)]]
        envelope["vendor"]["total"] = total
        for key in ("input_tokens", "cached_input_tokens", "output_tokens",
                    "reasoning_output_tokens", "total_tokens"):
            envelope["usage"][key] *= 2
        run.envelopes = [envelope]
        self.seed_accounting(run, run.envelopes, facts)
        with mock.patch.object(run, "facts", return_value=facts):
            usage(run)
            envelope["usage"]["input_tokens"] += 1
            with self.assertRaisesRegex(Blocked, "keyless last sum"):
                usage(run)

    def test_usage_rejects_unmatched_native_start_and_contradictory_terminal(self):
        for defect in ("extra_start", "terminal"):
            with self.subTest(defect=defect):
                run = self.run_object()
                envelope = fake_envelope("cancelled")
                envelope.update(stop_reason="interrupted", cancel={"outcome": "acknowledged", "cleanup": "quiescent"})
                thread, turn = digest(envelope["vendor_session_id"]), digest("u")
                sample = {"inputTokens": 3, "cachedInputTokens": 1, "outputTokens": 2,
                          "reasoningOutputTokens": 1, "totalTokens": 5}
                envelope["vendor"]["total"] = sample
                run.envelopes = [envelope]
                facts = [{"kind": "reply", "method": "turn/start", "thread": thread, "turn": turn},
                         {"kind": "terminal", "thread": thread, "turn": turn,
                          "status": "completed" if defect == "terminal" else "interrupted"},
                         {"kind": "usage", "thread": thread, "turn": turn, "last": sample, "total": sample}]
                self.seed_accounting(run, run.envelopes, facts)
                if defect == "extra_start":
                    facts.append({"kind": "reply", "method": "turn/start", "thread": digest("extra"), "turn": digest("unsubmitted")})
                with mock.patch.object(run, "facts", return_value=facts), mock.patch(__name__ + ".save"), \
                     self.assertRaises(Blocked):
                    usage(run)

    async def proxy_fixture(self, nonempty=False, identity_error=False, pidfd_error=False,
                            termination=False, foreign=False, reused=False, after_signal=False,
                            executable_mismatch=False, duplicate_config=False, config=None,
                            race=None, observed=None, startup=False, forward_pause=False,
                            unsafe_item=False, restored=None, replay_growth=False, reserved=True):
        """Entire stdio exchange using fake transports; no process or real /proc."""
        binary = self.root / "fake-codex"
        binary.write_text("synthetic executable")
        parent = 999999 if foreign else os.getpid()
        identity = self.process(2, parent, 20)
        identity["ppid"] = parent
        (self.root / "2/exe").symlink_to(binary)
        traces = self.root / "trace"
        traces.mkdir()
        reservations = reservation_control.Reservations(self.root / "reservations.json", time.monotonic() + RUN_S)
        if reserved and race:
            reservations.reserve(digest("fixture"), "resume" if restored is not None else "spawn",
                                 digest("synthetic prompt"), digest("/synthetic/work"),
                                 digest("t") if restored is not None else None)
        if restored is not None:
            earlier = self.owned_wire()
            sample = {key: 1 for key in SCHEMAS["sample"]}
            earlier.incoming({"method": "thread/tokenUsage/updated", "params": {
                "threadId": "t", "turnId": "u", "tokenUsage": {"last": sample, "total": sample}}})
            self.write_history(traces, shared.sha256(binary), earlier)
        control = {"codex": str(binary), "codex_hash": shared.sha256(binary),
                   "model": MODEL, "version": "0.160.1", "codex_home": str(self.root),
                   "mcp_names": [], "trace_dir": str(traces), "reservations": str(reservations.path)}
        upstream, native = asyncio.StreamReader(), asyncio.StreamReader()
        if executable_mismatch:
            other = self.root / "changed-native"
            other.write_text("untrusted native replacement")
            (self.root / "2/exe").unlink()
            (self.root / "2/exe").symlink_to(other)
        child = mock.Mock(pid=2, returncode=None, stdout=native)
        sent = []
        current_turn = "v" if restored is not None else "u"

        def write(raw):
            request = decode_line(raw)
            sent.append(request["method"])
            result = {"initialize": {"userAgent": "via/0.160.1"},
                      "config/read": {"config": {"features": {key: False for key in
                          ("apps", "plugins", "memories", "hooks")}, "notify": [],
                          "model_provider": "openai", "profile": None, "model_providers": {},
                          "otel": None, "mcp_servers": {"foreign": {"enabled": True}} if nonempty else {}}},
                      "model/list": {"data": [], "nextCursor": None}}.get(request["method"])
            if request["method"] in ("thread/start", "thread/resume") and race:
                result = {"model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
                          "thread": {"id": "t"}}
            if request["method"] == "turn/start" and race:
                notifications = (
                    {"method": "turn/started", "params": {"threadId": "t", "turn": {"id": current_turn}}},
                    {"method": "turn/completed", "params": {"threadId": "t",
                     "turn": {"id": current_turn, "status": "completed"}}})
                if unsafe_item:
                    notifications = ({"method": "item/started", "params": {
                        "threadId": "t", "turnId": "u", "item": {"type": "mcpToolCall"}}},)
                for notification in notifications:
                    native.feed_data(json.dumps(notification).encode() + b"\n")
            if request["method"] == "config/read" and startup:
                native.feed_data(b'{"method":"mcpServer/startupStatus/updated","params":{}}\n')
            if request["method"] == "config/read" and config is not None:
                result = config
            if result is not None:
                raw_reply = json.dumps({"id": request["id"], "result": result}).encode() + b"\n"
                native.feed_data(raw_reply)
                if request["method"] == "thread/resume" and restored is not None:
                    sample = {key: 1 for key in SCHEMAS["sample"]}
                    total = {key: 2 for key in SCHEMAS["sample"]} if replay_growth else sample
                    native.feed_data(json.dumps({"method": "thread/tokenUsage/updated", "params": {
                        "threadId": "t", "turnId": restored,
                        "tokenUsage": {"last": sample, "total": total}}}).encode() + b"\n")
                if request["method"] == "config/read" and duplicate_config:
                    native.feed_data(raw_reply)

        async def forwarded(raw):
            response = decode_line(raw)
            if "method" in response:
                if observed is not None:
                    observed.append(response["method"])
                if race and response["method"] == "turn/started":
                    rows = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
                    self.assertFalse(any(row["kind"] == "terminal" for row in rows))
                    # Reply only after the early notification was forwarded.
                    if race == "stall":
                        if forward_pause:
                            await asyncio.sleep(0.1)
                    elif race == "missing":
                        native.feed_eof()
                        upstream.feed_eof()
                    else:
                        native.feed_data(json.dumps({"id": 20, "result": {"turn": {
                            "id": current_turn if race == "match" else "foreign"}}}).encode() + b"\n")
                return
            if response["id"] == 1:
                upstream.feed_data(b'{"method":"initialized"}\n'
                                   b'{"id":2,"method":"model/list","params":{}}\n')
            elif response["id"] == 2:
                if termination:
                    sig = termination if type(termination) is signal.Signals else signal.SIGTERM
                    handler = signal.getsignal(sig)
                    self.assertTrue(callable(handler), "proxy must defer Host TERM")
                    handler(sig, None)
                    if after_signal:
                        upstream.feed_data(json.dumps(self.start_message()).encode() + b"\n")
                if race:
                    upstream.feed_data(json.dumps({"id": 10,
                        "method": "thread/resume" if restored is not None else "thread/start", "params": {
                        "model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
                        "sandbox": "read-only", "threadId": "t", "excludeTurns": True,
                        "cwd": "/synthetic/work"}}).encode() + b"\n")
                else:
                    upstream.feed_eof()
            elif response["id"] == 10:
                upstream.feed_data(json.dumps({**self.start_message(), "id": 20}).encode() + b"\n")
            elif response["id"] == 20:
                upstream.feed_eof()

        async def connect(factory, pipe):
            # Feed the reader constructed inside the proxy, rather than a
            # second parser. Our transport completion drives the next input.
            nonlocal upstream
            upstream = factory()._stream_reader
            upstream.feed_data(b'{"id":1,"method":"initialize","params":{}}\n')
            return mock.Mock(), mock.Mock()

        async def reap():
            child.returncode = 0
            return 0

        child.stdin.write.side_effect = write
        child.stdin.drain = mock.AsyncMock()
        child.stdin.close.side_effect = native.feed_eof
        child.wait = mock.AsyncMock(side_effect=reap)
        open_identity = mock.Mock(side_effect=Blocked("unverifiable identity")) if identity_error \
            else mock.Mock(side_effect=lambda *args: (os.open(self.root / "2", os.O_RDONLY), identity))
        def opened_pidfd(*args):
            if reused:
                self.process(2, parent, 30)
            return os.open(binary, os.O_RDONLY)
        open_pidfd = mock.Mock(side_effect=OSError("unverifiable pidfd")) if pidfd_error \
            else mock.Mock(side_effect=opened_pidfd)
        with mock.patch.object(asyncio, "create_subprocess_exec", new=mock.AsyncMock(return_value=child)), \
             mock.patch.object(os, "pidfd_open", new=open_pidfd), \
             mock.patch.object(Proc, "open", new=open_identity), \
             mock.patch.object(signal, "pidfd_send_signal") as signal_child, \
             mock.patch.object(asyncio.get_running_loop(), "connect_read_pipe", new=connect), \
             mock.patch(__name__ + ".forward_stdout", new=forwarded), \
             mock.patch(__name__ + ".executable_facts", return_value=[]):
            result = await proxy(control, ["app-server", "--disable", "memories", "--disable",
                                           "hooks", *server_args([])])
        child.wait.assert_awaited()
        return result, child, signal_child, sent, traces

    def test_proxy_complete_fake_exchange(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture())
        self.assertEqual(result, 0)
        self.assertEqual(sent, ["initialize", "initialized", "config/read", "model/list"])
        facts = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
        self.assertTrue(facts[-1]["complete"] and facts[-1]["child_reaped"])
        self.assertTrue(secret_free(facts))

    def test_proxy_without_reservation_never_forwards_start(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(race="match", reserved=False))
        self.assertEqual(result, 1)
        self.assertNotIn("thread/start", sent)
        self.assertNotIn("turn/start", sent)
        self.assertIsNotNone(child.returncode)

    def test_proxy_forwards_early_notification_and_checks_reply(self):
        for race in ("match", "mismatch", "missing"):
            with self.subTest(race=race), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    observed = []
                    result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(race=race, observed=observed))
                    self.assertEqual(result, 0 if race == "match" else 1)
                    self.assertIn("turn/started", observed)
                    rows = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
                    self.assertEqual(rows[-1]["complete"], race == "match")
                    self.assertTrue(rows[-1]["child_reaped"])
                    self.assertEqual(len([row for row in rows if row["kind"] == "terminal"]),
                                     1 if race == "match" else 0)
                    if race == "missing":
                        self.assertEqual(rows[-1]["blocked_reason"],
                                         "unowned deferred notification: establishing reply unavailable")
                    self.assertTrue(secret_free(rows))
                finally:
                    self.root = previous

    def test_proxy_mcp_startup_blocks_and_reaps_child(self):
        observed = []
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(startup=True, observed=observed))
        self.assertEqual(result, 1)
        self.assertNotIn("mcpServer/startupStatus/updated", observed)
        rows = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
        self.assertFalse(rows[-1]["complete"])
        self.assertTrue(rows[-1]["child_reaped"])
        self.assertEqual(rows[-1]["blocked_reason"], "MCP startup observed despite disables")

    def test_proxy_deferred_deadline_reaps_silent_or_backpressured_child(self):
        for paused in (False, True):
            with self.subTest(paused=paused), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    with mock.patch(__name__ + ".DEFER_S", 0.01, create=True), \
                         mock.patch(__name__ + ".RUN_S", 0.2):
                        result, child, signals, sent, traces = asyncio.run(
                            self.proxy_fixture(race="stall", forward_pause=paused))
                    self.assertEqual(result, 1)
                    rows = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
                    self.assertFalse(rows[-1]["complete"])
                    self.assertTrue(rows[-1]["child_reaped"])
                    self.assertEqual(rows[-1]["blocked_reason"], "deferred notification deadline")
                finally:
                    self.root = previous

    def test_proxy_run_wide_history_and_unknown_rejection(self):
        for restored, growth in (("u", False), ("u", True), ("never-established", False)):
            with self.subTest(restored=restored, growth=growth), tempfile.TemporaryDirectory() as root:
                previous, self.root = self.root, Path(root)
                try:
                    observed = []
                    result, child, signals, sent, traces = asyncio.run(
                        self.proxy_fixture(race="match", restored=restored,
                                           replay_growth=growth, observed=observed))
                    accepted = restored == "u" and not growth
                    self.assertEqual(result, 0 if accepted else 1)
                    path = traces / "server-2-20.jsonl"
                    rows = [decode_line(line) for line in path.read_bytes().splitlines(keepends=True)]
                    self.assertTrue(rows[-1]["child_reaped"])
                    self.assertEqual(rows[-1]["complete"], accepted)
                    if accepted:
                        self.assertTrue(any(row["kind"] == "replay" and row["turn"] == digest("u") for row in rows))
                        self.assertTrue(any(row["kind"] == "reply" and row.get("method") == "turn/start"
                                            and row["turn"] == digest("v") for row in rows))
                    else:
                        rejected = next(row for row in rows if row["kind"] == "ownership_rejection")
                        self.assertEqual(rejected["method"], "thread/tokenUsage/updated")
                        self.assertEqual(rejected["generation"], digest((2, 20)))
                        self.assertTrue(rejected["thread_owned"])
                        self.assertEqual(rejected["turn_known_earlier"], restored == "u")
                        self.assertNotIn("thread/tokenUsage/updated", observed)
                    self.assertTrue(secret_free(rows))
                finally:
                    self.root = previous

    def test_proxy_disallowed_deferred_item_never_reaches_stdout(self):
        observed = []
        with mock.patch(__name__ + ".RUN_S", 0.2):
            result, child, signals, sent, traces = asyncio.run(
                self.proxy_fixture(race="match", unsafe_item=True, observed=observed))
        self.assertEqual(result, 1)
        self.assertNotIn("item/started", observed)
        rows = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
        self.assertFalse(rows[-1]["complete"])
        self.assertTrue(rows[-1]["child_reaped"])
        self.assertEqual(rows[-1]["blocked_reason"], "background agent or unqualified tool item")

    def test_proxy_nonempty_inventory_stops_before_discovery(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(nonempty=True))
        self.assertEqual(result, 1)
        self.assertNotIn("model/list", sent)
        self.assertNotIn("turn/start", sent)

    def test_proxy_unverifiable_start_reaps_owned_child(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(identity_error=True))
        self.assertEqual(result, 1)
        signals.assert_not_called()
        self.assertEqual(sent, [])

    def test_proxy_pidfd_failure_reaps_owned_child_and_blocks(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(pidfd_error=True))
        self.assertEqual(result, 1)
        child.terminate.assert_not_called()
        signals.assert_not_called()

    def test_proxy_foreign_pid_never_gets_signalling_authority(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(foreign=True))
        self.assertEqual(result, 1)
        signals.assert_not_called()
        child.terminate.assert_not_called()
        child.kill.assert_not_called()

    def test_proxy_reuse_during_pidfd_setup_never_signals(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(reused=True))
        self.assertEqual(result, 1)
        signals.assert_not_called()
        child.terminate.assert_not_called()
        child.kill.assert_not_called()

    def test_missing_pidfd_with_no_eof_ack_never_signals(self):
        child = mock.Mock(returncode=None)
        child.wait = mock.AsyncMock(side_effect=asyncio.TimeoutError)
        with self.assertRaisesRegex(Blocked, "cleanup unverified"):
            asyncio.run(stop_child(child, None))
        child.terminate.assert_not_called()
        child.kill.assert_not_called()

    def test_proxy_host_term_is_deferred_until_reaped_trace(self):
        result, child, signals, sent, traces = asyncio.run(self.proxy_fixture(termination=True))
        self.assertEqual(result, 0)
        facts = [decode_line(line) for line in next(traces.iterdir()).read_bytes().splitlines(keepends=True)]
        self.assertTrue(facts[-1]["complete"] and facts[-1]["child_reaped"])
        self.assertEqual(facts[-1]["termination_signal"], "SIGTERM")

    def test_cli_wrong_model_never_starts_process(self):
        with mock.patch.object(subprocess, "run") as called, \
             self.assertRaisesRegex(Blocked, "gpt-6-luna"):
            main(["--run", "--via", "fake-via", "--codex", "fake-codex",
                  "--via-sha256", "0" * 64, "--codex-sha256", "0" * 64,
                  "--model", "gpt-6-sol", "--evidence", "scratchpad/unused"])
        called.assert_not_called()

    def test_cleanup_cancel_remains_allowed_after_spending_deadline(self):
        run = self.run_object()
        run.spend.deadline = time.monotonic() - 1
        run.spend.uncertain = True
        output = subprocess.CompletedProcess([], 0, json.dumps({"state": "cancelling",
                  "cancel": {"outcome": "requested", "cleanup": "pending"}}), "")
        with mock.patch.object(subprocess, "run", return_value=output) as called:
            self.assertEqual(run.via_call(run.evidence, "cleanup", "cancel", "s", "--json")[0], 0)
            with self.assertRaisesRegex(Blocked, "deadline"):
                run.via_call(run.evidence, "spawn", "spawn", "--json")
        self.assertEqual(len(called.call_args_list), 1)

    def test_daemon_stop_positive_proof(self):
        run = self.run_object()
        run.daemon = {"pid": 2, "start_ticks": 20}
        run.phase = "fake"
        run.daemons = [{"phase": "fake", "pid": 2, "start_ticks": 20}]
        with mock.patch.object(run, "lock_lines", return_value={}), \
             mock.patch.object(run, "find_daemons", return_value=[]), \
             mock.patch.object(shared, "all_procs", return_value={}), \
             mock.patch.object(shared, "alive", return_value=False), \
             mock.patch.object(shared, "lock_free", return_value=True), \
             mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "", "")):
            run.stop_daemon(final=True)
        self.assertIs(run.daemons[0]["stopped"], True)
        self.assertIsNone(run.daemon_unverified)

    def test_secret_package_scan_rejects_unknown_and_symlink(self):
        package = self.root / "package"
        package.mkdir()
        save(package / "safe.json", {"pass": True})
        self.assertIs(package_secret_free(package), True)
        (package / "unknown.txt").touch()
        self.assertIs(package_secret_free(package), False)
        (package / "unknown.txt").unlink()
        (package / "link").symlink_to(self.root, target_is_directory=True)
        self.assertIs(package_secret_free(package), False)

    def main_failure_fixture(self, save_failure=False):
        run = self.run_object()
        evidence = self.root / "scratchpad" / "result"
        args = ["--run", "--via", str(run.via), "--codex", str(run.codex),
                "--via-sha256", run.args.via_sha256, "--codex-sha256", run.args.codex_sha256,
                "--evidence", str(evidence)]
        patch = mock.patch(__name__ + ".save", side_effect=OSError("synthetic write failure")) \
            if save_failure else mock.patch.object(shutil, "rmtree", side_effect=OSError("synthetic removal failure"))
        with mock.patch(__name__ + ".__file__", str(self.root / "scripts/qualify/codex.py")), \
             mock.patch(__name__ + ".Run", return_value=run), \
             mock.patch(__name__ + ".execute", side_effect=Blocked("synthetic preflight failure")), \
             mock.patch(__name__ + ".cleanup", return_value=(False, [])), \
             mock.patch.object(signal, "signal"), mock.patch("builtins.print") as printed, patch:
            result = main(args)
        return result, evidence, json.loads(printed.call_args[0][0])

    def test_preflight_removal_failure_still_writes_blocked_summary(self):
        result, evidence, output = self.main_failure_fixture()
        self.assertEqual(result, 2)
        summary = json.loads((evidence / "summary.json").read_text())
        self.assertEqual(summary["result"], "blocked")
        self.assertIn("preflight directory removal failed", summary["cleanup_errors"])
        self.assertTrue(secret_free(summary))

    def test_summary_write_failure_emits_only_blocked_booleans(self):
        result, evidence, output = self.main_failure_fixture(save_failure=True)
        self.assertEqual(result, 2)
        self.assertEqual(output, {"result": "blocked", "secret_free": False,
                                "daemons_stopped": False, "summary_written": False})
        self.assertTrue(secret_free(output))


def self_test():
    """No real vendor, credentials, owner services or daemon; synthetic fixtures only."""
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(SafetyTests)
    import codex_reservations_test
    suite.addTests(unittest.defaultTestLoader.loadTestsFromModule(codex_reservations_test))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
