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
exactly seven submissions, at most two concurrently, each with a 120 s VIA wall,
short fixed prompts, low effort, and a 1200 s run deadline. Reservations happen
BEFORE the CLI call; no retries or extra turns. A missing/unknown envelope makes
uncertainty sticky and forbids further submissions. The proxy independently
limits starts per server and rejects other models/policy/effort. This bounds
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
                  GATE the bound change sent between turns. Denied-write tool
                  evidence and absent-file observations are RECORD ONLY: fast
                  code-mode refusals may omit commandExecution items. A present
                  prohibited file still blocks. Retire daemon/server, stored resume
                  with exact thread identity and excludeTurns:true.
  interrupt       GATE accepted A/B on one server; actual tool observed via
                  owned /proc; cancel A through VIA, interrupted terminal and
                  acknowledged cleanup; B remains running after A interrupt,
                  then returns only its answer; A resumes successfully.
  never_ask       GATE never/user reviewer in real outbound and echoed thread
                  settings; no reply grants permission. RECORD ONLY denied-write
                  observations and counts of inducible no-grant request paths.
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

Blocked = shared.Blocked
Unreadable = shared.Unreadable
MODEL = "gpt-6-luna"
# Structural bounds: Codex packet §7 cost unavailable, runtime §8 deadlines.
TURN_LIMIT, ACTIVE_LIMIT, WALL_S, RUN_S = 7, 2, 120, 1200
LINE_BYTES, TRACE_BYTES, PROC_BYTES = 8 * 1024 * 1024, 16 * 1024 * 1024, 256 * 1024
PAGE_LIMIT, POLL_S, PROC_COUNT = 1000, 90, 32768
REQUEST_LIMIT = 4096
# Packet §3 start ordering/runtime §8: qualification's pending ownership deadline.
DEFER_S = 30
# Packet §§4/8: finite configuration inventory and notification-method evidence.
MCP_LIMIT, NOTIFICATION_METHOD_LIMIT = 128, 128
# Runtime §6.1: fixed file proof, read no more than one byte beyond this marker.
WORKSPACE_MARKER = b"allowed"
NO_GRANT_METHODS = ("item/commandExecution/requestApproval", "item/fileChange/requestApproval",
                    "item/permissions/requestApproval", "item/tool/requestUserInput",
                    "mcpServer/elicitation/request", "item/tool/call")
SECRET = re.compile(r"(?i)(?:bearer\s+\S+|sk-[A-Za-z0-9_-]{8,}|"
                    r"(?:access_token|refresh_token|api_key|authorization|auth\.json)"
                    r"\s*[\":=]|/home/|/Users/)")


def secret_free(value):
    """Boolean only; no match or value is printed (runtime §6.1 privacy)."""
    return SECRET.search(json.dumps(value, ensure_ascii=True)) is None


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
    """Packet §7: reservation before submission, sticky uncertainty, no retries."""
    def __init__(self):
        self.used, self.active, self.uncertain = 0, set(), False
        self.deadline = time.monotonic() + RUN_S

    def reserve(self, key, model):
        shared.interrupt_guard()
        require(model == MODEL, "model must be gpt-6-luna")
        require(not self.uncertain and time.monotonic() < self.deadline,
                "spending control: uncertainty or run deadline")
        require(self.used < TURN_LIMIT and len(self.active) < ACTIVE_LIMIT,
                "spending control: turn/concurrency cap")
        require(key not in self.active, "spending control: duplicate reservation")
        self.used += 1
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


class Wire:
    """Paired protocol facts, never raw content (packet §§3–7)."""
    def __init__(self, version, sink=None):
        self.version, self.pending, self.facts, self.starts = version, {}, [], 0
        self.declines, self.server_requests = {}, {}
        self.initialized, self.inventory = False, False
        self.sink = sink
        self.threads, self.turns, self.notifications = set(), set(), {}
        self.deferred, self.deferred_bytes = [], 0

    def fact(self, **value):
        require(len(self.facts) < REQUEST_LIMIT, "protocol evidence bound")
        require(secret_free(value), "secret-free protocol evidence")
        self.facts.append(value)
        if self.sink:
            self.sink(value)
        return value

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
            require(params["model"] == MODEL, "model must be gpt-6-luna")
            require(params["approvalPolicy"] == "never" and params["approvalsReviewer"] == "user",
                    "never-ask/reviewer policy mismatch")
            require(self.inventory, "MCP inventory not verified")
            fact.update(model=MODEL, never=True, reviewer_user=True)
            if method == "turn/start":
                self.starts += 1
                require(self.starts <= TURN_LIMIT and params["effort"] == "low",
                        "spending control: proxy starts/effort")
                require(digest(params["threadId"]) in self.threads, "unowned thread request")
                fact.update(thread=digest(params["threadId"]), schema=digest(params["outputSchema"]),
                            bound=params["sandboxPolicy"]["type"])
            else:
                fact.update(bound=params["sandbox"])
                if method == "thread/resume":
                    require(params["excludeTurns"] is True, "resume history not excluded")
                    fact.update(thread=digest(params["threadId"]), exclude_turns=True)
        elif method == "turn/interrupt":
            require((digest(params["threadId"]), digest(params["turnId"])) in self.turns,
                    "unowned turn interrupt")
            fact.update(thread=digest(params["threadId"]), turn=digest(params["turnId"]))
        if "id" in message:
            require(type(message["id"]) in (str, int), "protocol id shape")
            key = digest(message["id"])
            require(key not in self.pending and len(self.pending) < REQUEST_LIMIT,
                    "protocol duplicate/pending bound")
            fact["request"] = key
            self.pending[key] = fact.copy()
        return self.fact(**fact)

    @staticmethod
    def scope(method, params):
        """Packet §3: notification scope, including errors with a thread identity."""
        scoped_error = method == "error" and "threadId" in params
        if not method.startswith(("thread/", "turn/", "item/")) and not scoped_error:
            return None
        thread = params.get("threadId")
        if method == "thread/started":
            thread = params.get("thread", {}).get("id")
        require(type(thread) is str, "unowned thread notification: missing identity")
        turn = params.get("turnId")
        if method.startswith("turn/"):
            turn = params.get("turn", {}).get("id", turn)
        has_turn = (method.startswith(("turn/", "item/"))
                    or method == "thread/tokenUsage/updated"
                    or scoped_error and "turnId" in params)
        require(not has_turn or type(turn) is str,
                "unowned turn notification; correlation unavailable: missing identity")
        return digest(thread), digest(turn) if has_turn else None

    def owns(self, scope):
        """Packet §3: only paired replies establish owned thread/turn IDs."""
        thread, turn = scope
        return thread in self.threads and (turn is None or (thread, turn) in self.turns)

    def establishing(self, scope):
        """Packet §3 start ordering: match only requests able to establish this scope."""
        thread, turn = scope
        if thread not in self.threads:
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
        require(remaining >= 0, "deferred notification deadline")
        return remaining

    def resolve_deferred(self, replied):
        """Packet §3 start ordering: publish deferred facts only after ownership proof."""
        self.deferred_timeout()
        pending, self.deferred = self.deferred, []
        self.deferred_bytes = 0
        for message, candidates, size, created in pending:
            candidates.discard(replied)
            scope = self.scope(message["method"], message["params"])
            if self.owns(scope):
                self.incoming(message)
            else:
                require(bool(candidates), "unowned deferred notification after establishing reply")
                self.deferred.append((message, candidates, size, created))
                self.deferred_bytes += size

    def incoming(self, message):
        self.deferred_timeout()
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
                require(type(result["thread"]["id"]) is str, "thread identity missing")
                thread = digest(result["thread"]["id"])
                require(method != "thread/resume" or thread == request["thread"],
                        "resume identity mismatch")
                self.threads.add(thread)
                fact.update(thread=thread, never=True, reviewer_user=True)
            elif method == "turn/start":
                require(type(result["turn"]["id"]) is str, "turn/start id missing")
                self.turns.add((request["thread"], digest(result["turn"]["id"])))
                fact.update(thread=request["thread"], turn=digest(result["turn"]["id"]))
            elif method not in ("turn/interrupt", "thread/unsubscribe"):
                raise Blocked("protocol reply method not qualified")
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
            require(scope is None or self.owns(scope),
                    "unowned thread/turn notification; correlation unavailable")
            key = digest(message["id"])
            require(type(message["id"]) in (str, int) and key not in self.server_requests
                    and len(self.server_requests) < REQUEST_LIMIT, "duplicate/server request bound")
            self.server_requests[key] = method
            return self.fact(kind="server_request", method_hash=digest(method))
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
                        turn=digest(params["turn"]["id"]), status=params["turn"]["status"])
        elif method in ("item/started", "item/completed"):
            item = params["item"]
            require(item.get("type") in ("userMessage", "agentMessage", "reasoning", "plan", "commandExecution"),
                    "background agent or unqualified tool item")
            if item["type"] == "commandExecution":
                output = item.get("aggregatedOutput")
                require(output is None or type(output) is str, "tool output type")
                code = item.get("exitCode")
                require(code is None or type(code) is int, "tool exit type")
                fact.update(kind="tool", thread=digest(params["threadId"]),
                            turn=digest(params["turnId"]), finished=method == "item/completed",
                            command_hash=command_digest(item.get("command")),
                            exit_code=code, read_only_error=isinstance(output, str)
                            and "Read-only file system" in output)
        if scope is not None and not self.owns(scope):
            candidates = self.establishing(scope)
            require(bool(candidates), "unowned thread/turn notification; correlation unavailable")
            size = len(json.dumps(message).encode())
            require(len(self.deferred) < REQUEST_LIMIT and self.deferred_bytes + size <= TRACE_BYTES,
                    "deferred notification bound")
            self.deferred.append((copy.deepcopy(message), candidates, size, time.monotonic()))
            self.deferred_bytes += size
            # Only ownership remains unproved. The proxy forwards immediately;
            # no fact is published before a matching establishing reply.
            return None
        if fact["kind"] == "notification":
            key = fact["method_hash"]
            require(key in self.notifications or len(self.notifications) < NOTIFICATION_METHOD_LIMIT,
                    "notification method bound")
            self.notifications[key] = self.notifications.get(key, 0) + 1
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
        wire, write_lock = Wire(control["version"], sink), asyncio.Lock()
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
                    raise Blocked("deferred notification deadline") from None
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
        require(not wire.deferred, "unowned deferred notification: establishing reply unavailable")
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
            self.record_only, self.owner_before, self.owner_after = [], None, None
            self.blocked_reasons = []
            self.metadata_day = datetime.datetime.now(datetime.timezone.utc).date()
        except BaseException:
            shutil.rmtree(self.work)
            raise

    def preflight(self):
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
                   "trace_dir": str(self.trace_dir), "version": self.args.candidate_version}
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
                         "proxy_launcher": shared.sha256(self.launcher)}})

    def via_call(self, directory, label, *args, timeout=60, handle=None):
        """Strict CLI reply schema; handles on stdin, no stdout/stderr persisted."""
        require(shared.sha256(self.via) == self.args.via_sha256, "pinned-hash mismatch")
        if handle is not None:
            args = (*args, "--handle-stdin")
        if args[0] not in ("daemon", "cancel", "close"):
            require(time.monotonic() < self.spend.deadline, "run deadline")
            timeout = min(timeout, max(0.1, self.spend.deadline - time.monotonic()))
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

    def submit(self, label, verb, *args, session=None):
        self.spend.reserve(label, self.model)
        try:
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
            return session
        except BaseException:
            self.spend.uncertain = True
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
            self.envelopes.append(envelope)
            return envelope
        except BaseException:
            self.spend.uncertain = True
            raise

    def start(self, name):
        self.start_phase(name, {"memories": False, "inherit": {"hooks": False}})

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

    def observe_tool(self, session, tag):
        deadline = time.monotonic() + POLL_S
        while time.monotonic() < deadline and not shared.INTERRUPTED:
            _, status, _ = self.via_call(self.evidence, "status", "status", session, "--json")
            if status["progress"] is not None and status["progress"]["running_tools"]:
                entries = os.listdir("/proc")
                require(len(entries) <= PROC_COUNT, "process survey bound")
                for pid in entries:
                    if not pid.isdigit():
                        continue
                    fd, ident = Proc().open(int(pid))
                    if fd is None:
                        continue
                    os.close(fd)
                    # Do not read foreign argv: metadata ancestry first.
                    try:
                        Proc().own(ident, self.daemon)
                    except Blocked:
                        continue
                    raw = Proc().content(ident, "cmdline")
                    if tag.encode() in raw:
                        self.owned.add((ident["pid"], ident["start_ticks"]))
                        save(self.evidence / "tool-executables.json", executable_facts(self.daemon))
                        self.check("owned tool observed", True)
                        return ident
            if status["turns"][-1]["state"] in shared.TERMINAL:
                break
            time.sleep(0.2)
        raise Blocked("tool execution not observable")

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


def turn_facts(run, envelope):
    """Exact C1 ordinal -> paired native turn identity, across server retirement."""
    facts, thread = run.facts(), digest(envelope["vendor_session_id"])
    starts = [fact for fact in facts if fact["kind"] == "reply" and fact["method"] == "turn/start"
              and fact["thread"] == thread]
    require(0 < envelope["turn"] <= len(starts), "turn correlation missing")
    turn = starts[envelope["turn"] - 1]["turn"]
    return [fact for fact in facts if fact.get("thread") == thread and fact.get("turn") == turn]


def workspace_marker(path):
    """Runtime §6.1: bounded marker proof through dirfd, without following links."""
    directory = None
    try:
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        before = os.stat(path.name, dir_fd=directory, follow_symlinks=False)
        require(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid()
                and before.st_nlink == 1, "workspace marker unverifiable")
        fd = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory)
        with os.fdopen(fd, "rb") as file:
            after = os.fstat(file.fileno())
            require((before.st_dev, before.st_ino) == (after.st_dev, after.st_ino)
                    and after.st_nlink == 1, "workspace marker unverifiable")
            return file.read(len(WORKSPACE_MARKER) + 1) == WORKSPACE_MARKER
    except OSError:
        raise Blocked("workspace marker unverifiable") from None
    finally:
        if directory is not None:
            os.close(directory)


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
    nonce = "VIAQUAL" + secrets.token_hex(8)
    schema_a, schema_b, schema_null = (run.work / name for name in ("a.json", "b.json", "null.json"))
    for path, field in ((schema_a, "written"), (schema_b, "blocked")):
        path.write_text(json.dumps({"type": "object", "properties": {field: {"type": "boolean"}},
                                   "required": [field], "additionalProperties": False}))
    schema_null.write_text("null\n")
    session = run.submit("c1", "spawn", *spawn_args(run, ws,
                         f"Remember {nonce} in this conversation, without storing it in a file. "
                         "Run exactly `printf allowed > allowed.txt` once. Report written:true.",
                         schema=schema_a))
    first = run.finish("c1")
    completed(run, first, "c1")
    run.check("schema set", first["structured_output"] == {"written": True})
    run.check("workspace write succeeded", workspace_marker(ws / "allowed.txt"))
    tools = [fact for fact in turn_facts(run, first) if fact["kind"] == "tool" and fact["finished"]]
    run.record_only.append({"observation": "allowed write command visibility",
                            "exact_command_visible": len(tools) == 1
                            and tools[0]["command_hash"] == digest("printf allowed > allowed.txt")})
    run.submit("c2", "resume", session, "--bound", "read_only", "--output-schema", str(schema_b),
               "--wall-ms", str(WALL_S * 1000), "--prompt",
               "Run exactly `printf tightened > denied.txt` once. Do not retry or ask for "
               "permission. Report blocked:true only if the command failed.", session=session)
    second = run.finish("c2")
    completed(run, second, "c2")
    run.check("schema replaced", second["structured_output"] == {"blocked": True})
    thread = digest(second["vendor_session_id"])
    current_starts = [fact for fact in run.facts() if fact["kind"] == "request"
                      and fact["method"] == "turn/start" and fact["thread"] == thread]
    run.check("bound change sent between turns", len(current_starts) == 2
              and current_starts[0]["bound"] == "workspaceWrite"
              and current_starts[1]["bound"] == "readOnly")
    denied_write_observation(run, second, "printf tightened > denied.txt", ws / "denied.txt")
    run.stop_daemon()
    run.facts(final=True)
    run.start("stored-resume")
    run.submit("c3", "resume", session, "--output-schema", str(schema_null), "--wall-ms",
               str(WALL_S * 1000), "--prompt",
               "Use no tools. Reply with only the remembered VIAQUAL code.", session=session)
    third = run.finish("c3")
    completed(run, third, "c3")
    run.check("schema cleared", third["structured_output"] is None and third["final_text"] == nonce)
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
    run.check("schema set and replacement sent", starts[0]["schema"] == digest(json.loads(schema_a.read_text()))
              and starts[1]["schema"] == digest(json.loads(schema_b.read_text())))
    run.check("current bound sent", starts[-1]["bound"] == "readOnly")


def interrupt(run):
    """Packet §§5/8: protocol cancel, surviving tools, interleaved isolation."""
    ws = run.work / "interrupt"
    ws.mkdir(mode=0o700)
    tag = "VIAQUALTOOL" + secrets.token_hex(8)
    command_a = f"python3 -c 'import time; tag=\"{tag}\"; time.sleep(45)'"
    a = run.submit("a1", "spawn", *spawn_args(run, ws,
                   f"Run exactly `{command_a}` in the foreground and wait. Then reply A_END."))
    tool = run.observe_tool(a, tag)
    b = run.submit("b1", "spawn", *spawn_args(run, ws,
                   "Run exactly `sleep 55` in the foreground and wait. Then reply B_ONLY."))
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
    ended = run.finish("a1")
    run.check("actual interrupted acknowledgement", ended["state"] == "cancelled"
              and ended["stop_reason"] == "interrupted" and ended["cancel"] is not None
              and ended["cancel"]["outcome"] == "acknowledged")
    cleanup = ended["cancel"]["cleanup"]
    run.check("cleanup packet disposition", cleanup in ("quiescent", "uncertain"))
    fd, current = Proc().open(tool["pid"], tool["start_ticks"])
    if fd is not None:
        os.close(fd)
    run.check("quiescent tool absence proved", cleanup != "quiescent"
              or current is None or current["state"] == "Z")
    _, status, _ = run.via_call(run.evidence, "b-after-cancel", "status", b, "--json")
    run.check("B continues after A cancellation", status["turns"][-1]["state"] == "running")
    other = run.finish("b1")
    completed(run, other, "b1")
    run.check("B result isolated", other["final_text"].strip() == "B_ONLY"
              and other["vendor_session_id"] != ended["vendor_session_id"])
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
    """Packet §4: reviewer/never gate; refused-write visibility is record-only."""
    ws = run.work / "never-ask"
    ws.mkdir(mode=0o700)
    session = run.submit("n1", "spawn", *spawn_args(run, ws,
                         "Run exactly `printf forbidden > forbidden.txt` once. Do not change "
                         "the command or retry. Request approval if needed. Then reply DONE.",
                         bound="read_only"))
    ended = run.finish("n1")
    completed(run, ended, "n1")
    thread = digest(ended["vendor_session_id"])
    facts = run.facts()
    run.check("never/user reviewer verified", any(fact["kind"] == "reply"
              and fact["method"] == "thread/start" and fact["thread"] == thread
              and fact["never"] and fact["reviewer_user"] for fact in facts))
    denied_write_observation(run, ended, "printf forbidden > forbidden.txt", ws / "forbidden.txt")


def usage(run):
    """Packet §7: whole keyless last samples vs envelopes; no cost estimation."""
    facts = run.facts(final=True)
    for envelope in run.envelopes:
        thread = digest(envelope["vendor_session_id"])
        terminals = [fact for fact in facts if fact["kind"] == "terminal" and fact["thread"] == thread]
        # Match the ordinal retained turn in this stored thread, never sum a
        # thread's cumulative total into a turn. Every start has a paired ID.
        starts = [fact for fact in facts if fact["kind"] == "reply" and fact["method"] == "turn/start"
                  and fact["thread"] == thread]
        require(0 < envelope["turn"] <= len(starts), "usage turn correlation missing")
        turn = starts[envelope["turn"] - 1]["turn"]
        samples = [fact for fact in facts if fact["kind"] == "usage" and fact["thread"] == thread
                   and fact["turn"] == turn]
        run.check("usage correlated terminal", any(fact["turn"] == turn for fact in terminals))
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
    run.check("exact structural turn plan", run.spend.used == TURN_LIMIT)


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
    interrupted = bool(shared.INTERRUPTED)  # final read of deferred flag
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
               "spending_uncertain": run.spend.uncertain if run else True,
               "checks": run.checks if run else [],
               "owner_home_writes": {"policy": "accepted, not minimised",
                   "before": run.owner_before if run else None,
                   "after": run.owner_after if run else None,
                   "new_session_files": len(set(run.owner_after["session_digests"])
                       - set(run.owner_before["session_digests"]))
                       if run and run.owner_before is not None and run.owner_after is not None else None},
               "record_only_scope": ["command visibility and denied-write enforcement",
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
    try:
        save(args.evidence / "summary.json", summary)
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
        for params in ({"threadId": "foreign"}, {"threadId": None}, {"threadId": "t", "turnId": "foreign"}):
            with self.subTest(params=params), self.assertRaisesRegex(Blocked, "unowned"):
                wire.incoming({"method": "error", "params": params})

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

    def test_never_ask_missing_tool_is_record_only(self):
        run = self.run_object()
        ended = fake_envelope()
        facts = [{"kind": "reply", "method": "thread/start", "never": True,
                  "reviewer_user": True, "thread": digest(ended["vendor_session_id"])}]
        with mock.patch.object(run, "submit", return_value="fake"), \
             mock.patch.object(run, "finish", return_value=ended), \
             mock.patch.object(run, "facts", return_value=facts), \
             mock.patch(__name__ + ".turn_facts", return_value=[]):
            never_ask(run)
        self.assertTrue(all(check["pass"] for check in run.checks))
        self.assertFalse(run.record_only[0]["exact_command_refusal_visible"])

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
                        {"kind": "terminal", "thread": digest("b"), "status": "completed"},
                    ]
                    run.handles = {"a": "fake"}
                    run.receipts = {"a1": ({"turn": "turn-a"}, "a")}
                    status = {"vendor_identity_verified": True, "progress": {"running_tools": ["fake"]},
                              "turns": [{"state": "running"}], "cancel": {"outcome": "requested"}}
                    with mock.patch.object(run, "submit", side_effect=["a", "b", "a"]), \
                         mock.patch.object(run, "observe_tool", return_value={"pid": 2, "start_ticks": 20}), \
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

    def conversation_fixture(self, wrong_bound=False, marker_link=False):
        run = self.run_object()
        first, second, third = (fake_envelope() for _ in range(3))
        first.update(structured_output={"written": True})
        second.update(turn=2, structured_output={"blocked": True})
        third.update(turn=3, final_text="VIAQUALfixed", structured_output=None)
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
                else:
                    marker.write_text("allowed")
            return "fake"
        def facts(*args, **kwargs):
            return [{"kind": "request", "method": "thread/resume", "thread": thread,
                     "exclude_turns": True, "bound": "read-only"}] + [
                {"kind": "request", "method": "turn/start", "thread": thread, "bound": bound,
                 "schema": digest(json.loads((run.work / name).read_text()))}
                for name, bound in list(zip(("a.json", "b.json", "null.json"),
                    ("workspaceWrite", "workspaceWrite" if wrong_bound else "readOnly", "readOnly")))[:len(submitted)]]
        with mock.patch.object(run, "submit", side_effect=submit), \
             mock.patch.object(run, "finish", side_effect=[first, second, third]), \
             mock.patch.object(run, "facts", side_effect=facts), \
             mock.patch.object(run, "stop_daemon"), mock.patch.object(run, "start"), \
             mock.patch(__name__ + ".turn_facts", return_value=[]), \
             mock.patch.object(secrets, "token_hex", return_value="fixed"):
            conversation(run)
        return run

    def test_conversation_can_observe_writes_without_tool_items(self):
        run = self.conversation_fixture()
        self.assertTrue(all(check["pass"] for check in run.checks))
        self.assertTrue(any(row["observation"] == "denied write" for row in run.record_only))

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
        for number in range(TURN_LIMIT):
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
        run.daemon = self.process(9, 0, 90)
        self.process(1, 0, 10)
        self.process(2, 1, 20)
        proc = Proc(self.root)
        status = {"progress": {"running_tools": ["fake"]}, "turns": [{"state": "completed"}]}
        with mock.patch(__name__ + ".Proc", return_value=proc), \
             mock.patch.object(proc, "content") as content, \
             mock.patch.object(os, "listdir", return_value=["2"]), \
             mock.patch.object(run, "via_call", return_value=(0, status, None)), \
             self.assertRaisesRegex(Blocked, "not observable"):
            run.observe_tool("fake", "foreign-marker")
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
        with mock.patch.object(run, "facts", return_value=facts):
            usage(run)
            envelope["usage"]["input_tokens"] += 1
            with self.assertRaisesRegex(Blocked, "keyless last sum"):
                usage(run)

    async def proxy_fixture(self, nonempty=False, identity_error=False, pidfd_error=False,
                            termination=False, foreign=False, reused=False, after_signal=False,
                            executable_mismatch=False, duplicate_config=False, config=None,
                            race=None, observed=None, startup=False, forward_pause=False,
                            unsafe_item=False):
        """Entire stdio exchange using fake transports; no process or real /proc."""
        binary = self.root / "fake-codex"
        binary.write_text("synthetic executable")
        parent = 999999 if foreign else os.getpid()
        identity = self.process(2, parent, 20)
        identity["ppid"] = parent
        (self.root / "2/exe").symlink_to(binary)
        traces = self.root / "trace"
        traces.mkdir()
        control = {"codex": str(binary), "codex_hash": shared.sha256(binary),
                   "model": MODEL, "version": "0.160.1", "codex_home": str(self.root),
                   "mcp_names": [], "trace_dir": str(traces)}
        upstream, native = asyncio.StreamReader(), asyncio.StreamReader()
        if executable_mismatch:
            other = self.root / "changed-native"
            other.write_text("untrusted native replacement")
            (self.root / "2/exe").unlink()
            (self.root / "2/exe").symlink_to(other)
        child = mock.Mock(pid=2, returncode=None, stdout=native)
        sent = []

        def write(raw):
            request = decode_line(raw)
            sent.append(request["method"])
            result = {"initialize": {"userAgent": "via/0.160.1"},
                      "config/read": {"config": {"features": {key: False for key in
                          ("apps", "plugins", "memories", "hooks")}, "notify": [],
                          "model_provider": "openai", "profile": None, "model_providers": {},
                          "otel": None, "mcp_servers": {"foreign": {"enabled": True}} if nonempty else {}}},
                      "model/list": {"data": [], "nextCursor": None}}.get(request["method"])
            if request["method"] == "thread/start" and race:
                result = {"model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
                          "thread": {"id": "t"}}
            if request["method"] == "turn/start" and race:
                notifications = (
                    {"method": "turn/started", "params": {"threadId": "t", "turn": {"id": "u"}}},
                    {"method": "turn/completed", "params": {"threadId": "t",
                     "turn": {"id": "u", "status": "completed"}}})
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
                            "id": "u" if race == "match" else "foreign"}}}).encode() + b"\n")
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
                    upstream.feed_data(json.dumps({"id": 10, "method": "thread/start", "params": {
                        "model": MODEL, "approvalPolicy": "never", "approvalsReviewer": "user",
                        "sandbox": "read-only"}}).encode() + b"\n")
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
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
