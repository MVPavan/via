#!/usr/bin/env python3
"""Maintainer live qualification of Claude Code through the real `via` binary.

Usage:
  scripts/qualify/claude.py --via target/release/via \
      --evidence scratchpad/qualify/claude-<run> [--model haiku] \
      [--budget-usd 0.75] [--claude PATH]
  scripts/qualify/claude.py --self-test

Adapter design item 4 (docs/workstreams/rust-foundation/adapters/design.md,
"Maintainer live check") and packet §9 (docs/specs/vendors/claude-code.md):
opt-in, never in the gate, run on the maintainer's own Claude login. It
starts a private daemon (fresh VIA_STATE_DIR under the evidence directory, a
short runtime directory for the sockets), runs every case through `via`,
asserts it and keeps per-case evidence: receipts, envelopes, events,
status, the recipe of every Claude launch VIA started, and process
snapshots before and after. The evidence package is `summary.json` plus the
per-case directories (packet §9); `summary.json` is written on every exit
path and holds pass, fail, blocked or not_observable per case, the vendor
versions observed and the reported cost.

The run passes, and exits 0, only when every case passes, every daemon it
started has stopped, the reported cost stayed within the cap, and every
envelope reports one vendor version equal to `claude --version`. A pass
adds that version to the adapter's CHECKED set
(crates/via-adapters/src/claude/plan.rs). Infrastructure, authentication
(outside the auth case), quota, budget or runner failure is `blocked`;
a case nobody gives evidence for is `not_observable`; neither passes.

Budget: the cap must be finite and positive. The total reported cost (each
session's latest session-cumulative `usd`) is checked before every submit
and after every turn; reaching it blocks the case and the rest of the run.
Every completed turn must report a positive cost, strictly above the
session's previous completed turn.

Cases (packet §9 live rows):
  recipe_continuity     claude_live_recipe_continuity
  interrupt             claude_live_interrupt
  never_ask             claude_never_ask, live negative
  mcp_switches          claude_live_mcp_resume and the §4 switch table
  usage                 §3 usage scopes against the vendor's own transcript
  private_profile_auth  §4 private profile: HOME is an empty directory
Every launch's argv is checked against the full recipe: model, stream
flags, never-ask pair, tool lists, mode and MCP switches, identity, frozen
instructions (by digest, after the source file was changed), effort,
schema (by digest) and step limit as expected for that turn.

Tool use: VIA records no tool events (C1 §6.1), so tool calls are read
from the vendor's own session transcript of the session VIA created: per
prompt, the number of tool calls and whether any tool input held the
nonce. Only those counts are kept, as for usage numbers.

mcp_switches (owner decision, 2026-10-06): VIA records no init inventory for
Claude (packet §4, via-7c6), so the evidence is Claude's own MCP debug lines,
written by `--debug=mcp --debug-file=<file>` to a file of the runner's. The
runner parses each server's connection message apart from its name, keeps
only `{name: status}` (a name that looks like a URL or host is redacted),
and deletes the raw file on every exit path. Connected servers count only on
a real success line: at least one on the unrestricted spawn and the same on
its resume, none with MCP off; under `--restricted` any set agrees with the
packet's `unknown`. With no server connected on the unrestricted spawn the
case is `not_observable`. `status`, warnings and argv must match the packet
in all three modes.

Declined (owner, 2026-10-06):
- A live SIGTERM-only negative for the interrupt. That a signal death is
  never an acknowledged cancel is VIA's normalizer behaviour, covered by
  the fixture test `claude_cleanup_not_ack` (variant
  `claude_cleanup_not_ack_forced`) in crates/via-core/tests/conformance_claude.rs.
- A manifest, REPORT.md and per-case Store backups: the evidence package
  is `summary.json` plus the per-case directories.
Excluded: claude_live_bounds. Its CLAUDE-BOUND-1 matrix (packet §8) is
Linux and macOS and its bound matrix is still open (via-p98.3.4); `full` is
the only bound this runner exercises.

Phases, each a fresh daemon on the same state directory, since the mode and
the inherited-configuration request are daemon configuration:
  restricted            recipe_continuity, interrupt, never_ask, usage and
                        mcp_switches' restricted launch
  unrestricted          mcp_switches' default-request spawn and resume (the
                        owner's hooks run; accepted)
  unrestricted-mcp-off  mcp_switches' MCP-off launch
  empty-home            private_profile_auth
A daemon whose stop cannot be verified keeps its identity and runtime
directory in the summary, and every later phase is blocked.

Privacy: the runner never prints or records environment values, credential
contents or MCP configuration. It reads no credential file. It records MCP
server names and statuses only, process flags by name (values only for an
allow-list of recipe flags, digests for instructions, schemas and session
IDs), and the presence of VIA's process-marker key, never its value. From
the vendor transcripts of sessions VIA created in this run it reads usage
numbers and tool-call counts only. It reads other processes' command lines
only to discover the tagged tool process; it reads environments, sends
signals or waits on no process VIA did not start, and Claude processes
alive before the run are excluded by pid.
"""

import argparse
import datetime
import glob
import hashlib
import json
import math
import os
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

CATEGORIES = ("agents", "hooks", "instruction_files", "mcp_servers", "plugins", "skills")
MARKER_KEY = b"VIA_PROCESS_MARKER"
TOOLS = "Read,Write,Edit,Glob,Grep,Bash"
# Recipe flags whose value the runner records.
VALUE_FLAGS = {
    "--model", "--effort", "--max-turns", "--permission-mode", "--permission-prompts",
    "--tools", "--allowedTools", "--input-format", "--output-format",
}
# Recipe flags recorded by digest only: instructions, schemas, session IDs.
DIGEST_FLAGS = {"--append-system-prompt", "--json-schema", "--session-id", "--resume"}
CASES = ("recipe_continuity", "interrupt", "never_ask", "mcp_switches", "usage",
         "private_profile_auth")
TERMINAL = {"completed", "failed", "cancelled", "unknown"}
SOCKET_MAX = 107
ANCHOR_SOCKET_TAIL = len("/anchors/") + 32 + len(".sock")
TURN_WAIT_S = 240
QUOTA = re.compile(r"quota|credit|usage limit|rate limit|overloaded", re.IGNORECASE)
EXCLUDED = [{
    "case": "claude_live_bounds",
    "reason": "CLAUDE-BOUND-1 (packet §8) needs Linux and macOS and its bound matrix is "
              "still open (via-p98.3.4); only the `full` bound is exercised here",
}]


class Blocked(Exception):
    """Infrastructure, auth, quota or budget: the case cannot pass or fail."""


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds")


def write_json(path, value):
    path.write_text(json.dumps(value, indent=1, sort_keys=True) + "\n")


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for chunk in iter(lambda: file.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def digest_text(text):
    return hashlib.sha256(text.encode()).hexdigest()


def digest_json(value):
    """The digest of a JSON value as VIA passes a schema: compact, sorted keys."""
    return digest_text(json.dumps(value, sort_keys=True, separators=(",", ":")))


# --- /proc ------------------------------------------------------------------

def proc_stat(pid):
    """`/proc/<pid>/stat` fields the runner needs, or None once gone."""
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    comm = raw[raw.index("(") + 1:raw.rindex(")")]
    fields = raw[raw.rindex(")") + 2:].split()
    return {"pid": pid, "comm": comm, "state": fields[0], "ppid": int(fields[1]),
            "pgid": int(fields[2]), "sid": int(fields[3]), "start_ticks": int(fields[19])}


def all_procs():
    procs = {}
    for name in os.listdir("/proc"):
        if name.isdigit():
            stat = proc_stat(int(name))
            if stat:
                procs[stat["pid"]] = stat
    return procs


def cmdline(pid):
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return []
    return [part.decode("utf-8", "replace") for part in raw.split(b"\0") if part]


def has_marker_key(pid):
    """Whether VIA's marker key is in the environment; the value is never kept."""
    try:
        with open(f"/proc/{pid}/environ", "rb") as file:
            raw = file.read(256 * 1024)
    except OSError:
        return None
    return any(entry.split(b"=", 1)[0] == MARKER_KEY for entry in raw.split(b"\0"))


def alive(pid, start_ticks):
    stat = proc_stat(pid)
    return stat is not None and stat["start_ticks"] == start_ticks and stat["state"] != "Z"


def descendants(root, procs):
    children = {}
    for stat in procs.values():
        children.setdefault(stat["ppid"], []).append(stat["pid"])
    found, stack = set(), [root]
    while stack:
        for child in children.get(stack.pop(), []):
            if child not in found:
                found.add(child)
                stack.append(child)
    return found


def recipe_flags(argv):
    """A launch's flags by name; values for VALUE_FLAGS, digests for DIGEST_FLAGS."""
    flags, values, digests = [], {}, {}
    for index, arg in enumerate(argv[1:], start=1):
        if not arg.startswith("-"):
            continue
        name, eq, attached = arg.partition("=")
        flags.append(name)
        value = attached if eq else (argv[index + 1] if index + 1 < len(argv) else None)
        if name in VALUE_FLAGS:
            values[name] = value
        elif name in DIGEST_FLAGS and value is not None:
            if name == "--json-schema":
                try:
                    digests[name] = digest_json(json.loads(value))
                except ValueError:
                    digests[name] = "unparsable"
            else:
                digests[name] = digest_text(value)
    return {"flags": flags, "values": values, "digests": digests}


# --- MCP debug evidence -----------------------------------------------------------

# Claude's per-server debug message: `MCP server "<name>": <message>`. The
# name holds no quote, and the message is parsed apart from it.
MCP_LINE = re.compile(r'MCP server "(?P<name>[^"\n]*)": (?P<message>.*)$')
HOSTLIKE = re.compile(r"(?:[\w-]+\.)+[A-Za-z]{2,}(?::\d+)?(?:/\S*)?|\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?")


def parse_mcp_debug(text):
    """`{server name: connected | failed | seen}`; only a message that starts
    with `Successfully connected` counts as connected."""
    servers = {}
    for line in text.splitlines():
        match = MCP_LINE.search(line)
        if not match:
            continue
        name, message = match["name"], match["message"]
        if message.startswith("Successfully connected"):
            servers[name] = "connected"
        elif servers.get(name) != "connected":
            failed = message.lower().startswith(("connection failed", "failed")) or (
                " failed" in message.lower())
            servers[name] = "failed" if failed else servers.get(name, "seen")
    return servers


def safe_names(servers):
    """Statuses by name; a name that looks like a URL, host or path is redacted."""
    shown, hidden = {}, 0
    for name in sorted(servers):
        if "://" in name or "@" in name or "/" in name or HOSTLIKE.fullmatch(name):
            hidden += 1
            shown[f"<redacted-name-{hidden}>"] = servers[name]
        else:
            shown[name] = servers[name]
    return shown


def self_test():
    """The MCP parser's and redactor's known cases; returns the failures."""
    failing = 'T [DEBUG] MCP server "Successfully connected" failed to connect: unavailable'
    cases = [
        (parse_mcp_debug(failing), {}),
        (parse_mcp_debug('T [DEBUG] MCP server "a b": Successfully connected (x) in 5ms'),
         {"a b": "connected"}),
        (parse_mcp_debug('T [DEBUG] MCP server "c": Connection failed: Successfully connected'),
         {"c": "failed"}),
        (parse_mcp_debug('T [DEBUG] MCP server "d": Starting connection with timeout'),
         {"d": "seen"}),
        (parse_mcp_debug('T [DEBUG] MCP server "e": Successfully connected\n'
                         'T [DEBUG] MCP server "e": connection closed after 3s (cleanly)'),
         {"e": "connected"}),
        (safe_names({"mcp.example.com": "seen", "https://h/x": "seen", "10.0.0.1:80": "seen",
                     "claude.ai Claude Docs": "connected"}),
         {"<redacted-name-1>": "seen", "<redacted-name-2>": "seen",
          "<redacted-name-3>": "seen", "claude.ai Claude Docs": "connected"}),
    ]
    return [{"got": got, "want": want} for got, want in cases if got != want]


# --- vendor transcript ------------------------------------------------------------

def is_prompt(entry):
    """A user entry that opens a turn: the prompt, not a tool result."""
    message = entry.get("message")
    if entry.get("type") != "user" or entry.get("isMeta") or not isinstance(message, dict):
        return False
    content = message.get("content")
    return not (isinstance(content, list) and any(
        isinstance(part, dict) and part.get("type") == "tool_result" for part in content))


def vendor_turns(path, needle=None):
    """Per prompt: usage summed over model calls (one per message id), the
    number of tool calls and of tool inputs holding `needle`."""
    keys = ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens",
            "output_tokens")
    turns = []
    with open(path) as file:
        for line in file:
            entry = json.loads(line)
            if is_prompt(entry):
                turns.append({"calls": {}, "tools": set(), "needle_inputs": set()})
                continue
            message = entry.get("message")
            if (entry.get("type") != "assistant" or entry.get("isSidechain") or not turns
                    or not isinstance(message, dict)):
                continue
            turn = turns[-1]
            for part in message.get("content") or []:
                if isinstance(part, dict) and part.get("type") == "tool_use":
                    turn["tools"].add(part.get("id"))
                    if needle and needle in json.dumps(part.get("input")):
                        turn["needle_inputs"].add(part.get("id"))
            if isinstance(message.get("usage"), dict):
                usage = {key: message["usage"].get(key) or 0 for key in keys}
                prior = turn["calls"].get(message.get("id"))
                if prior is None or usage["output_tokens"] >= prior["output_tokens"]:
                    turn["calls"][message.get("id")] = usage
    return [{**{key: sum(call[key] for call in turn["calls"].values()) for key in keys},
             "calls": sum(1 for call in turn["calls"].values() if any(call.values())),
             "tool_calls": len(turn["tools"]), "needle_inputs": len(turn["needle_inputs"])}
            for turn in turns]


def vendor_transcript(run, vendor_id):
    """The vendor's transcript of a session VIA created, or None."""
    if not vendor_id:
        return None
    home = Path(run.base_env["HOME"])
    found = glob.glob(str(home / ".claude" / "projects" / "*" / f"{vendor_id}.jsonl"))
    return found[0] if len(found) == 1 else None


# --- launch sampling -------------------------------------------------------------

class LaunchSampler:
    """Polls the daemon's descendants for Claude launches while a turn runs."""

    def __init__(self, run):
        self.run = run
        self.launches = []
        self.seen = set()
        self.stop_flag = threading.Event()
        self.thread = threading.Thread(target=self.loop, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.stop_flag.set()
        self.thread.join(5)

    def loop(self):
        while not self.stop_flag.is_set():
            self.sample()
            time.sleep(0.05)
        self.sample()

    def sample(self):
        if self.run.daemon is None:
            return
        procs = all_procs()
        for pid in descendants(self.run.daemon["pid"], procs):
            stat = procs[pid]
            key = (pid, stat["start_ticks"])
            if key in self.seen or pid in self.run.preexisting_claude:
                continue
            # A launch is a Claude process whose parent is not one: Claude
            # runs helpers (ripgrep) from its own executable.
            if not self.run.is_claude(pid) or self.run.is_claude(stat["ppid"]):
                continue
            argv = cmdline(pid)
            if not argv:
                continue
            self.seen.add(key)
            self.launches.append({"pid": pid, "start_ticks": stat["start_ticks"],
                                  "seen_at": utc_now(), **recipe_flags(argv)})


# --- the runner -------------------------------------------------------------

class Case:
    def __init__(self, run, name):
        self.run = run
        self.name = name
        self.dir = run.evidence / name
        self.dir.mkdir(mode=0o700)
        self.checks = []
        self.result = None
        self.reason = None
        self.sessions = []
        self.identity = {}

    def check(self, name, ok, detail=None):
        self.checks.append({"check": name, "result": "pass" if ok else "fail", "detail": detail})
        return ok

    def not_observable(self, name, reason):
        self.checks.append({"check": name, "result": "not_observable", "detail": reason})

    def settle(self):
        if self.result is None:
            results = {check["result"] for check in self.checks}
            if "fail" in results:
                self.result = "fail"
            elif "not_observable" in results:
                self.result = "not_observable"
            else:
                self.result = "pass" if self.checks else "fail"
        record = {"case": self.name, "result": self.result, "reason": self.reason,
                  "evidence": self.name, "sessions": self.sessions, "checks": self.checks}
        write_json(self.dir / "case.json", record)
        return record


class Run:
    def __init__(self, args):
        self.via = Path(args.via).resolve()
        self.model = args.model
        self.budget = args.budget_usd
        self.claude = args.claude
        self.claude_real = os.path.realpath(self.claude)
        self.evidence = args.evidence
        self.state = self.evidence / "state"
        self.runtime = None
        self.base_env = {key: value for key, value in os.environ.items()
                         if not key.startswith("VIA_")}
        self.env = dict(self.base_env)
        self.daemon = None
        self.daemon_unverified = None
        self.phase = None
        self.daemons = []
        self.costs = {}
        self.completed_cost = {}
        self.versions = []
        self.preexisting_claude = set()
        self.budget_hit = False

    def prepare(self):
        self.runtime = self.short_runtime()
        self.base_env.update(VIA_STATE_DIR=str(self.state), VIA_RUNTIME_DIR=str(self.runtime))
        self.env = dict(self.base_env)
        self.preexisting_claude = self.claude_pids()

    def short_runtime(self):
        candidate = self.evidence / "rt"
        if len(str(candidate)) + ANCHOR_SOCKET_TAIL <= SOCKET_MAX:
            candidate.mkdir(mode=0o700)
            return candidate
        return Path(tempfile.mkdtemp(prefix="vq-", dir="/tmp"))

    def is_claude(self, pid):
        try:
            if os.path.realpath(f"/proc/{pid}/exe") == self.claude_real:
                return True
        except OSError:
            pass
        argv = cmdline(pid)
        return bool(argv) and os.path.basename(argv[0]) == "claude"

    def claude_pids(self):
        return {pid for pid, stat in all_procs().items()
                if stat["comm"] == "claude" or self.is_claude(pid)}

    def spent(self):
        return round(sum(self.costs.values()), 6)

    def budget_guard(self):
        if self.spent() >= self.budget:
            self.budget_hit = True
            raise Blocked(f"budget reached: {self.spent()} USD of {self.budget}")

    # --- via calls ---

    def via_call(self, case_dir, label, *args, timeout=60):
        argv = [str(self.via), *args]
        try:
            proc = subprocess.run(argv, env=self.env, capture_output=True, text=True,
                                  timeout=timeout)
        except subprocess.TimeoutExpired as error:
            raise Blocked(f"`via {args[0]}` did not return within {timeout} s") from error
        (case_dir / f"{label}.json").write_text(proc.stdout)
        if proc.stderr:
            (case_dir / f"{label}.err").write_text(proc.stderr)
        with open(case_dir / "commands.txt", "a") as log:
            shown = ["<prompt>" if index and args[index - 1] == "--prompt" else part
                     for index, part in enumerate(args)]
            log.write(f"{utc_now()} rc={proc.returncode} via {' '.join(shown)}\n")
        if proc.returncode == 4:
            raise Blocked(f"`via {args[0]}`: daemon unreachable (see {label}.err)")
        return proc.returncode, self.parse(proc.stdout), self.parse(proc.stderr)

    @staticmethod
    def parse(text):
        text = text.strip()
        if not text:
            return None
        try:
            return json.loads(text.splitlines()[-1])
        except ValueError:
            return None

    def turn(self, case, label, verb, *args, wait_s=TURN_WAIT_S, expect=()):
        """Spawns or resumes one turn and waits for it; returns (receipt,
        envelope, launches). `expect` names the failure classes the case
        expects; any other auth, quota or step-budget failure blocks."""
        self.budget_guard()
        with LaunchSampler(self) as sampler:
            # `--json` before the arguments: anything after `--` is vendor_args.
            _, receipt, err = self.via_call(case.dir, f"{label}-receipt", verb, "--json",
                                            *args, timeout=60)
            if receipt is None:
                write_json(case.dir / f"{label}-launches.json", sampler.launches)
                return None, err, sampler.launches
            if verb == "spawn":
                self.keep_handle(case, receipt)
            _, envelope, err = self.via_call(case.dir, f"{label}-envelope", "wait",
                                             receipt["turn"], "--timeout-ms",
                                             str(wait_s * 1000), "--json",
                                             timeout=wait_s + 30)
        write_json(case.dir / f"{label}-launches.json", sampler.launches)
        if envelope is None:
            raise Blocked(f"{label}: no envelope within {wait_s} s ({err})")
        self.account(case, label, envelope)
        failure = envelope.get("failure") or {}
        cls = failure.get("class")
        if cls not in expect and (cls in ("rate_limit", "auth", "budget_exceeded") or (
                cls == "vendor_error" and QUOTA.search(failure.get("message") or ""))):
            raise Blocked(f"{label}: vendor {cls} failure")
        return receipt, envelope, sampler.launches

    def keep_handle(self, case, receipt):
        handle = case.dir / f"{receipt['session_id']}.handle"
        handle.write_text(receipt["handle"])
        handle.chmod(0o600)
        case.sessions.append(receipt["session_id"])

    def account(self, case, label, envelope):
        """Records the turn's version and cost; checks a completed turn's
        cost; blocks once the total reaches the cap."""
        self.versions.append({"case": case.name, "turn": label,
                              "vendor_version": envelope.get("vendor_version")})
        cost = envelope.get("cost") or {}
        session = envelope.get("session_id")
        usd = cost.get("usd")
        cumulative = cost.get("scope") == "session_cumulative"
        finite = isinstance(usd, (int, float)) and math.isfinite(usd)
        if finite and cumulative:
            self.costs[session] = max(self.costs.get(session, 0.0), usd)
        if envelope.get("state") == "completed":
            previous = self.completed_cost.get(session, 0.0)
            case.check(f"{label}: cost reported, positive, above the previous turn's",
                       finite and cumulative and usd > 0 and usd > previous,
                       {"usd": usd, "scope": cost.get("scope"), "previous": previous})
            if finite:
                self.completed_cost[session] = usd
        if self.spent() >= self.budget:
            self.budget_hit = True
            raise Blocked(f"budget reached after {label}: {self.spent()} USD of {self.budget}")

    def session_file(self, case, session):
        return f"--handle-file={case.dir / f'{session}.handle'}"

    def status(self, case, label, session, *extra):
        _, status, _ = self.via_call(case.dir, label, "status", session, *extra, "--json")
        return status or {}

    def events(self, case, label, address):
        events, after = [], 0
        while True:
            _, page, _ = self.via_call(case.dir, f"{label}-page", "events", address,
                                       "--after", str(after), "--json")
            if not page:
                break
            events.extend(page.get("events", []))
            if not page.get("more"):
                break
            after = page["next_after"]
        (case.dir / f"{label}-page.json").unlink(missing_ok=True)
        write_json(case.dir / f"{label}.json", events)
        return events

    # --- processes ---

    def snapshot(self, case, label, tags=()):
        """VIA's processes and tagged tool processes; marker key presence only."""
        procs = all_procs()
        ours = descendants(self.daemon["pid"], procs) if self.daemon else set()
        rows, unattributed = [], 0
        for pid, stat in sorted(procs.items()):
            argv = cmdline(pid) if pid in ours or tags else []
            tagged = any(tag in part for tag in tags for part in argv)
            if pid in ours or tagged:
                rows.append({**stat, "claude": self.is_claude(pid), "tagged": tagged,
                             "argv0": os.path.basename(argv[0]) if argv else None,
                             "via_marker_key": has_marker_key(pid)})
            elif stat["comm"] == "claude" and pid not in self.preexisting_claude:
                unattributed += 1
        snap = {"at": utc_now(), "daemon": self.daemon, "processes": rows,
                "unattributed_new_claude": unattributed}
        write_json(case.dir / f"ps-{label}.json", snap)
        return snap

    # --- daemon phases ---

    def start_phase(self, name, claude_config, home=None):
        self.stop_daemon()
        self.state.mkdir(mode=0o700, exist_ok=True)
        config = {"harnesses": {"claude": {"binary": self.claude, **claude_config}}}
        target = self.state / "daemon.json"
        target.write_text(json.dumps(config) + "\n")
        target.chmod(0o600)
        self.env = dict(self.base_env)
        if home is not None:
            self.env["HOME"] = str(home)
        phase_dir = self.evidence / "daemon"
        phase_dir.mkdir(mode=0o700, exist_ok=True)
        write_json(phase_dir / f"{name}-config.json",
                   {"harnesses": {"claude": {"binary": "<claude>", **claude_config}}})
        self.phase = name
        _, status, err = self.via_call(phase_dir, f"{name}-status", "daemon", "status",
                                       "--json", timeout=60)
        if not status or "pid" not in status:
            raise Blocked(f"daemon for phase {name} did not start ({err})")
        stat = proc_stat(status["pid"])
        self.daemon = {"pid": status["pid"], "start_ticks": stat["start_ticks"] if stat else None}
        self.daemons.append({"phase": name, **self.daemon})
        print(f"phase {name}: daemon started", flush=True)

    def stop_daemon(self):
        """Stops the phase's daemon; an unverified stop keeps its identity and
        runtime directory and blocks the rest of the run."""
        if self.daemon_unverified:
            raise Blocked(f"daemon of phase {self.daemon_unverified['phase']} not "
                          "verified stopped")
        if self.daemon is None:
            return
        phase_dir = self.evidence / "daemon"
        record = self.daemons[-1]
        procs = all_procs()
        ours_before = {(pid, procs[pid]["start_ticks"])
                       for pid in descendants(self.daemon["pid"], procs)}
        try:
            _, reply, err = self.via_call(phase_dir, f"{self.phase}-stop", "daemon", "stop",
                                          "--json", timeout=60)
        except Blocked as error:
            reply, err = None, str(error)
        record["stop_reply"] = reply if reply is not None else err
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and alive(self.daemon["pid"], self.daemon["start_ticks"]):
            time.sleep(0.2)
        record["stopped"] = not alive(self.daemon["pid"], self.daemon["start_ticks"])
        record["descendants_left"] = sorted(pid for pid, start in ours_before
                                            if alive(pid, start))
        pgrep = subprocess.run(["pgrep", "-f", str(self.via)], capture_output=True, text=True)
        record["pgrep_via"] = [int(pid) for pid in pgrep.stdout.split()
                               if int(pid) != os.getpid()]
        print(f"phase {self.phase}: daemon stopped={record['stopped']}", flush=True)
        if not record["stopped"] or record["descendants_left"]:
            record["runtime_dir_kept"] = str(self.runtime)
            self.daemon_unverified = record
            raise Blocked(f"daemon of phase {self.phase} not verified stopped")
        self.daemon = None


# --- shared checks -----------------------------------------------------------

def check_launch(case, label, launches, *, mode, session, first, mcp_off=False,
                 effort=None, max_turns=None, schema=None, instructions=None):
    """Exactly one launch, carrying the full recipe as expected for this turn
    (packet §4): None means the flag must be absent."""
    if not case.check(f"{label}: one Claude launch observed", len(launches) == 1,
                      {"launches": len(launches)}):
        return
    launch = launches[0]
    flags, values, digests = launch["flags"], launch["values"], launch["digests"]
    want_values = {"--input-format": "stream-json", "--output-format": "stream-json",
                   "--model": case.run.model, "--permission-mode": "dontAsk",
                   "--permission-prompts": "none", "--tools": TOOLS, "--allowedTools": TOOLS,
                   "--effort": effort, "--max-turns": max_turns}
    wrong = {flag: values.get(flag) for flag, want in want_values.items()
             if values.get(flag) != want}
    presence = {"-p": True, "--verbose": True,
                "--restricted": mode == "restricted", "--strict-mcp-config": mcp_off,
                "--session-id": first, "--resume": not first,
                "--append-system-prompt": instructions is not None,
                "--json-schema": schema is not None}
    wrong.update({flag: f"present={flag in flags}" for flag, want in presence.items()
                  if (flag in flags) != want})
    if instructions is not None and digests.get("--append-system-prompt") != digest_text(
            instructions):
        wrong["--append-system-prompt"] = "digest differs from the frozen text"
    if schema is not None and digests.get("--json-schema") != digest_json(schema):
        wrong["--json-schema"] = "digest differs from the turn's schema"
    identity = digests.get("--session-id" if first else "--resume")
    if first:
        case.identity[session] = identity
    elif identity != case.identity.get(session):
        wrong["--resume"] = "not the session's first identity"
    case.check(f"{label}: argv carries the full recipe", not wrong and identity is not None,
               wrong or None)


def completed(case, label, envelope):
    return case.check(f"{label}: completed", envelope.get("state") == "completed",
                      {"state": envelope.get("state"), "failure": envelope.get("failure")})


# --- cases ---------------------------------------------------------------------

def case_recipe_continuity(run, case):
    """Four launches on one session (packet §9 claude_live_recipe_continuity)."""
    ws = case.dir / "ws"
    ws.mkdir()
    nonce = "QX" + secrets.token_hex(3).upper()
    mark = "MARK" + secrets.token_hex(3).upper()
    changed_mark = "MARK" + secrets.token_hex(3).upper()
    frozen = f"End every reply you write in plain text with a last line that reads exactly: {mark}\n"
    instructions = case.dir / "instructions.txt"
    instructions.write_text(frozen)
    schema_a = {"type": "object", "properties": {"written": {"type": "string"}},
                "required": ["written"], "additionalProperties": False}
    schema_b = {"type": "object", "properties": {"count": {"type": "integer"}},
                "required": ["count"], "additionalProperties": False}
    write_json(case.dir / "schema-a.json", schema_a)
    write_json(case.dir / "schema-b.json", schema_b)
    (case.dir / "schema-null.json").write_text("null\n")
    write_json(case.dir / "nonces.json", {"nonce": nonce, "instruction_mark": mark,
                                          "changed_mark": changed_mark})
    run.snapshot(case, "before")
    receipt, t1, launches = run.turn(
        case, "t1", "spawn", "--harness", "claude", "--model", run.model, "--cwd", str(ws),
        "--instructions", str(instructions), "--effort", "low",
        "--output-schema", str(case.dir / "schema-a.json"), "--wall-ms", "180000",
        "--background", "--prompt",
        f"Remember this code for later in our conversation: {nonce}. It is private: never "
        "write it to a file and never pass it to any tool. Now use the Write tool to create "
        "the file note.txt in the current directory with the content hello. Then give your "
        "structured answer with `written` set to the text you wrote.")
    if receipt is None:
        case.check("t1: spawn accepted", False, t1)
        return
    session = receipt["session_id"]
    hf = run.session_file(case, session)
    # The source changes after spawn: VIA must keep passing the frozen text.
    instructions.write_text(frozen.replace(mark, changed_mark))
    case.check("t1: receipt effort low", receipt["effective"].get("effort") == "low",
               receipt["effective"])
    completed(case, "t1", t1)
    case.check("t1: structured output A", (t1.get("structured_output") or {}).get(
        "written", "").strip() == "hello", t1.get("structured_output"))
    note = ws / "note.txt"
    mode = note.stat().st_mode & 0o777 if note.exists() else None
    case.check("t1: Write tool created note.txt mode 0644 holding hello",
               mode == 0o644 and note.read_text().strip() == "hello",
               {"exists": note.exists(), "mode": oct(mode) if mode is not None else None})
    common = {"mode": "restricted", "session": session, "instructions": frozen}
    check_launch(case, "t1", launches, first=True, effort="low", schema=schema_a, **common)

    receipt2, t2, launches = run.turn(
        case, "t2", "resume", session, hf, "--effort", "medium",
        "--output-schema", str(case.dir / "schema-b.json"),
        "--prompt", "Without using any tools, give your structured answer with `count` set "
                    "to 7.")
    if receipt2 is None:
        case.check("t2: resume accepted", False, t2)
        return
    completed(case, "t2", t2)
    case.check("t2: structured output B replaced A", t2.get("structured_output") == {"count": 7},
               t2.get("structured_output"))
    check_launch(case, "t2", launches, first=False, effort="medium", schema=schema_b, **common)

    receipt3, t3, launches = run.turn(
        case, "t3", "resume", session, hf, "--output-schema", str(case.dir / "schema-null.json"),
        "--max-steps", "1", "--prompt",
        "Use the Bash tool to run the command `echo step-limit-check`, then tell me exactly "
        "what it printed.", expect=("budget_exceeded",))
    if receipt3 is None:
        case.check("t3: resume accepted", False, t3)
        return
    failure = t3.get("failure") or {}
    case.check("t3: step limit gives failed budget_exceeded max_steps error_max_turns",
               t3.get("state") == "failed" and failure.get("class") == "budget_exceeded"
               and t3.get("stop_reason") == "max_steps"
               and failure.get("vendor_code") == "error_max_turns",
               {"state": t3.get("state"), "failure": failure,
                "stop_reason": t3.get("stop_reason"), "steps": t3.get("steps")})
    check_launch(case, "t3", launches, first=False, effort="medium", max_turns="1", **common)

    receipt4, t4, launches = run.turn(
        case, "t4", "resume", session, hf,
        "--prompt", "Answer from memory only, without using any tools: what is the code I "
                    "asked you to remember in my first message? Reply with the code, plus "
                    "anything your instructions require.")
    if receipt4 is None:
        case.check("t4: resume accepted", False, t4)
        return
    completed(case, "t4", t4)
    text = t4.get("final_text") or ""
    case.check("t4: conversation-only nonce recalled", nonce in text, {"final_text": text})
    case.check("t4: frozen instructions in effect, not the changed source",
               mark in text and changed_mark not in text)
    case.check("t4: one step, max_steps 1 inherited", t4.get("steps") == 1
               and receipt4["effective"].get("max_steps") == 1,
               {"steps": t4.get("steps"), "max_steps": receipt4["effective"].get("max_steps")})
    case.check("t4: schema cleared", t4.get("structured_output") is None and not any(
        w.get("code") == "structured_output_missing" for w in t4.get("warnings", [])))
    check_launch(case, "t4", launches, first=False, effort="medium", max_turns="1", **common)
    ids = {e.get("vendor_session_id") for e in (t1, t2, t3, t4)}
    case.check("one vendor session across four launches", len(ids) == 1 and None not in ids,
               {"distinct": len(ids)})
    status = run.status(case, "status", session)
    case.check("vendor identity verified", status.get("vendor_identity_verified") is True)
    run.events(case, "events", session)
    # VIA records no tool events (C1 §6.1): the vendor transcript of this
    # VIA-created session gives the tool calls, as counts only.
    path = vendor_transcript(run, t1.get("vendor_session_id"))
    if path is None:
        case.not_observable("tool calls per turn", "vendor transcript not found")
    else:
        turns = vendor_turns(path, needle=nonce)
        write_json(case.dir / "tool-calls.json",
                   [{"turn": n, "tool_calls": t["tool_calls"],
                     "inputs_with_nonce": t["needle_inputs"]} for n, t in enumerate(turns, 1)])
        case.check("one vendor prompt per VIA turn", len(turns) == 4, {"prompts": len(turns)})
        case.check("no tool input held the nonce in any turn",
                   all(t["needle_inputs"] == 0 for t in turns))
        case.check("t4: no tool call", len(turns) == 4 and turns[3]["tool_calls"] == 0)
    run.snapshot(case, "after")
    case.usage_inputs = [(session, t) for t in (t1, t2, t3, t4)]


def case_interrupt(run, case):
    """A long foreground Bash tool, cancelled (packet §7, §9 claude_live_interrupt)."""
    ws = case.dir / "ws"
    ws.mkdir()
    tag = "via-qualify-" + secrets.token_hex(4)
    run.snapshot(case, "before", tags=(tag,))
    run.budget_guard()
    with LaunchSampler(run) as sampler:
        _, receipt, err = run.via_call(
            case.dir, "t1-receipt", "spawn", "--json", "--harness", "claude", "--model",
            run.model, "--cwd", str(ws), "--wall-ms", "240000", "--background", "--prompt",
            "Use the Bash tool to run exactly this command in the foreground (not in the "
            "background) and wait for it to finish: "
            f"python3 -c 'import time; tag = \"{tag}\"; time.sleep(90)'\nThen reply DONE.")
        if receipt is None:
            case.check("t1: spawn accepted", False, err)
            return
        session = receipt["session_id"]
        run.keep_handle(case, receipt)
        tool, running, deadline = None, False, time.monotonic() + 90
        while time.monotonic() < deadline and not (tool and running):
            status = run.status(case, "status-running", session)
            running = "Bash" in ((status.get("progress") or {}).get("running_tools") or [])
            for pid, stat in all_procs().items():
                argv = cmdline(pid)
                if (argv and os.path.basename(argv[0]).startswith("python")
                        and any(tag in part for part in argv)):
                    tool = stat
            if (status.get("turns") or [{}])[-1].get("state") in TERMINAL:
                break
            time.sleep(0.5)
        case.check("tool observed running (status running_tools Bash, process present)",
                   bool(tool and running), {"running_tools_bash": running, "tool": tool})
        run.snapshot(case, "pre-cancel", tags=(tag,))
        if tool:
            marker = has_marker_key(tool["pid"])
            case.check("VIA process marker key reaches the tool", marker is True,
                       {"key_present": marker})
        _, cancel, _ = run.via_call(case.dir, "cancel", "cancel", session,
                                    run.session_file(case, session), "--wait", "--json",
                                    timeout=150)
        # The decisive observation point: as `cancel --wait` returns.
        tool_alive = bool(tool) and alive(tool["pid"], tool["start_ticks"])
        after = run.snapshot(case, "post-cancel", tags=(tag,))
    write_json(case.dir / "t1-launches.json", sampler.launches)
    in_snapshot = bool(tool) and any(
        row["pid"] == tool["pid"] and row["start_ticks"] == tool["start_ticks"]
        for row in after["processes"])
    case.check("tool pid and start time absent as cancel --wait returns",
               bool(tool) and not tool_alive and not in_snapshot,
               {"alive": tool_alive, "in_post_cancel_snapshot": in_snapshot})
    case.check("Claude launch gone as cancel --wait returns",
               bool(sampler.launches) and not any(
                   alive(l["pid"], l["start_ticks"]) for l in sampler.launches),
               {"launches": len(sampler.launches)})
    check_launch(case, "t1", sampler.launches, mode="restricted", session=session, first=True)
    _, envelope, _ = run.via_call(case.dir, "t1-envelope", "result", receipt["turn"], "--json")
    envelope = envelope or {}
    run.account(case, "t1", envelope)
    cancel_info = envelope.get("cancel") or {}
    case.check("turn cancelled, interrupted", envelope.get("state") == "cancelled"
               and envelope.get("stop_reason") == "interrupted",
               {"state": envelope.get("state"), "stop_reason": envelope.get("stop_reason")})
    case.check("cancel acknowledged", cancel_info.get("outcome") == "acknowledged",
               {"cancel_reply": cancel})
    case.check("cleanup quiescent", cancel_info.get("cleanup") == "quiescent", cancel_info)
    events = run.events(case, "t1-events", receipt["turn"])
    types = [event["type"] for event in events]
    order = [types.index(t) if t in types else None
             for t in ("cancel.requested", "cancel.settled", "turn.ended")]
    case.check("events: cancel.requested, then cancel.settled, then turn.ended",
               None not in order and order == sorted(order), types)
    receipt2, t2, launches = run.turn(case, "t2", "resume", session,
                                      run.session_file(case, session), "--prompt",
                                      "Reply with the single word AFTER. Use no tools.")
    if receipt2 is None:
        case.check("t2: resume accepted", False, t2)
        return
    completed(case, "t2", t2)
    case.check("t2: same session continues", "AFTER" in (t2.get("final_text") or "")
               and t2.get("vendor_session_id") == envelope.get("vendor_session_id"))
    check_launch(case, "t2", launches, mode="restricted", session=session, first=False)
    run.snapshot(case, "after", tags=(tag,))


def case_never_ask(run, case):
    """A Write outside the workspace under --restricted is denied, never asked."""
    ws = case.dir / "ws"
    ws.mkdir()
    outside = case.dir / "outside"
    outside.mkdir()
    target = outside / "denied.txt"
    run.snapshot(case, "before")
    started = time.monotonic()
    receipt, t1, launches = run.turn(
        case, "t1", "spawn", "--harness", "claude", "--model", run.model, "--cwd", str(ws),
        "--wall-ms", "180000", "--background", "--prompt",
        f"Use the Write tool to create the file {target} containing the word no. Do not use "
        "Bash or any other tool for this. If the write is refused, reply REFUSED.")
    if receipt is None:
        case.check("spawn accepted", False, t1)
        return
    elapsed = round(time.monotonic() - started, 3)
    case.check("turn settled without hanging", t1.get("state") in ("completed", "failed")
               and (t1.get("failure") or {}).get("class") not in ("deadline_wall", "deadline_idle"),
               {"state": t1.get("state"), "failure": t1.get("failure"), "elapsed_s": elapsed})
    check_launch(case, "t1", launches, mode="restricted", session=receipt["session_id"],
                 first=True)
    events = run.events(case, "events", receipt["session_id"])
    denials = [e for e in events if e["type"] == "action.denied"]
    entries = t1.get("denied_actions", [])
    # An envelope entry is cut to 256 bytes; its event keeps the full
    # payload (C1 §5).
    case.check("exactly one denial: envelope total, entries and events",
               t1.get("denied_actions_total") == 1 and len(entries) == 1 and len(denials) == 1,
               {"total": t1.get("denied_actions_total"), "entries": len(entries),
                "events": len(denials)})
    case.check("the denial is a file_write of the outside path",
               len(denials) == 1 and denials[0].get("kind") == "file_write"
               and denials[0].get("target") == str(target) and len(entries) == 1
               and entries[0].get("event_seq") == denials[0]["seq"]
               and str(target).startswith(entries[0].get("target") or "\0"))
    case.check("no control request declined (permission prompts none)",
               t1.get("auto_declined_requests_total") == 0,
               t1.get("auto_declined_requests_total"))
    case.check("outside file not created", not target.exists())
    run.snapshot(case, "after")


def mcp_debug_args(case, session_label):
    """Vendor arguments (unreserved, packet §4) that write Claude's MCP debug
    lines to a file of the runner's own; the session's every launch reuses
    them, so each turn's file is read and deleted once the turn ends."""
    folder = case.dir / "mcp-debug"
    folder.mkdir(mode=0o700, exist_ok=True)
    return ["--", "--debug=mcp", f"--debug-file={folder / (session_label + '.live.log')}"]


def purge_mcp_debug(case):
    """Deletes every raw debug file and Claude's link to it."""
    folder = case.dir / "mcp-debug"
    if folder.is_dir():
        for path in folder.iterdir():
            if path.is_symlink() or path.suffix == ".log":
                path.unlink()


def take_mcp_debug(case, session_label, label):
    """Reads one launch's debug file, keeps `{name: status}` and deletes the
    file; returns the connected names (raw, in memory), or None when Claude
    wrote no file."""
    live = case.dir / "mcp-debug" / f"{session_label}.live.log"
    try:
        if not live.exists():
            servers = None
        else:
            servers = parse_mcp_debug(live.read_text(errors="replace"))
    finally:
        purge_mcp_debug(case)
    write_json(case.dir / f"{label}-mcp.json", {
        "source": "Claude MCP debug lines (--debug=mcp --debug-file); raw file deleted",
        "init_inventory": "not recorded by VIA (via-7c6)",
        "servers": None if servers is None else safe_names(servers)})
    if servers is None:
        case.not_observable(f"{label}: MCP debug lines", "Claude wrote no debug file")
        return None
    return sorted(name for name, state in servers.items() if state == "connected")


def mcp_launch(run, case, label, expect_inherit, expect_warning, mode, mcp_off):
    receipt, envelope, launches = run.turn(
        case, label, "spawn", "--harness", "claude", "--model", run.model,
        "--cwd", str(case.dir / "ws"), "--wall-ms", "180000", "--background",
        "--prompt", "Reply with the single word READY. Use no tools.",
        *mcp_debug_args(case, label))
    if receipt is None:
        case.check(f"{label}: spawn accepted", False, envelope)
        return None
    session = receipt["session_id"]
    completed(case, label, envelope)
    check_status(run, case, label, session, expect_inherit, expect_warning)
    check_launch(case, label, launches, mode=mode, session=session, first=True,
                 mcp_off=mcp_off)
    return session


def check_status(run, case, label, session, expect_inherit, expect_warning):
    status = run.status(case, f"{label}-status", session)
    inherit = status.get("inherit")
    case.check(f"{label}: status inherit matches the packet table", inherit == expect_inherit,
               inherit)
    warning = [w for w in status.get("warnings", []) if w.get("code") == "config_switch_unverified"]
    categories = sorted(c["category"] for w in warning for c in w.get("data", {}).get(
        "categories", []))
    case.check(f"{label}: config_switch_unverified categories", categories == expect_warning,
               categories)


def case_mcp_restricted(run, case):
    (case.dir / "ws").mkdir(exist_ok=True)
    run.snapshot(case, "restricted-before")
    expect = {c: "off" for c in CATEGORIES}
    expect["mcp_servers"] = "unknown"
    try:
        session = mcp_launch(run, case, "restricted", expect, sorted(CATEGORIES),
                             "restricted", False)
        if session:
            # Packet §4: under --restricted what loads varied (none, or
            # claude.ai connectors), so `unknown`; any server set agrees.
            take_mcp_debug(case, "restricted", "restricted")
    finally:
        purge_mcp_debug(case)
    run.snapshot(case, "restricted-after")


def case_mcp_unrestricted(run, case):
    (case.dir / "ws").mkdir(exist_ok=True)
    run.snapshot(case, "unrestricted-before")
    expect = {c: "on" for c in CATEGORIES}
    try:
        session = mcp_launch(run, case, "unrestricted", expect, [], "unrestricted", False)
        if not session:
            return
        first = take_mcp_debug(case, "unrestricted", "unrestricted-t1")
        if first is not None and not first:
            case.not_observable("unrestricted-t1: a connected MCP server",
                                "no MCP server connected: with none configured a resume "
                                "that drops servers looks the same")
        elif first:
            case.check("unrestricted-t1: MCP servers connected", True, {"count": len(first)})
        receipt, t2, launches = run.turn(case, "unrestricted-t2", "resume", session,
                                         run.session_file(case, session), "--prompt",
                                         "Reply with the single word AGAIN. Use no tools.")
        if receipt is None:
            case.check("unrestricted-t2: resume accepted", False, t2)
            return
        completed(case, "unrestricted-t2", t2)
        check_launch(case, "unrestricted-t2", launches, mode="unrestricted", session=session,
                     first=False)
        check_status(run, case, "unrestricted-t2", session, expect, [])
        again = take_mcp_debug(case, "unrestricted", "unrestricted-t2")
        if first and again is not None:
            case.check("unrestricted-t2: the resume connects the spawn's MCP servers",
                       again == first, {"spawn": len(first), "resume": len(again),
                                        "same_names": again == first})
    finally:
        purge_mcp_debug(case)
    run.snapshot(case, "unrestricted-after")


def case_mcp_off(run, case):
    run.snapshot(case, "mcp-off-before")
    expect = {c: "on" for c in CATEGORIES}
    expect["mcp_servers"] = "off"
    try:
        session = mcp_launch(run, case, "mcp-off", expect, [], "unrestricted", True)
        if session:
            connected = take_mcp_debug(case, "mcp-off", "mcp-off")
            if connected is not None:
                case.check("mcp-off: no MCP server connected", not connected,
                           {"count": len(connected)})
    finally:
        purge_mcp_debug(case)
    run.snapshot(case, "mcp-off-after")


def case_usage(run, case, inputs):
    """Envelope usage against the vendor's own per-call accounting (packet §3).

    VIA keeps no raw `result` (C1 `logs.transcript` is null on this route),
    so the reference is the vendor's own session transcript: each prompt
    and the usage of each model call after it. One VIA turn is one prompt.
    Cost is checked per completed turn as each turn is accounted.
    """
    if not inputs:
        case.result, case.reason = "blocked", "recipe_continuity produced no turns"
        return
    session, first = inputs[0]
    _, logs, _ = run.via_call(case.dir, "logs", "logs", f"{session}/1", "--json")
    case.check("VIA keeps no vendor transcript (logs.transcript null)",
               (logs or {}).get("transcript") is None, logs)
    path = vendor_transcript(run, first.get("vendor_session_id"))
    if path is None:
        case.not_observable("vendor per-call usage", "vendor transcript not found")
        return
    vendor = vendor_turns(path)
    case.check("one vendor prompt per VIA turn", len(vendor) == len(inputs),
               {"vendor_prompts": len(vendor), "via_turns": len(inputs)})
    rows, cumulative = [], {}
    for (_, envelope), raw in zip(inputs, vendor):
        turn = envelope["turn"]
        raw = {key: raw[key] for key in ("input_tokens", "cache_creation_input_tokens",
                                         "cache_read_input_tokens", "output_tokens", "calls")}
        cumulative = {key: cumulative.get(key, 0) + value for key, value in raw.items()}
        usage = envelope.get("usage") or {}
        expected = {"input_tokens": raw["input_tokens"] + raw["cache_creation_input_tokens"]
                    + raw["cache_read_input_tokens"],
                    "cached_input_tokens": raw["cache_read_input_tokens"],
                    "output_tokens": raw["output_tokens"]}
        got = {key: usage.get(key) for key in expected}
        rows.append({"turn": turn, "state": envelope.get("state"), "vendor_turn": raw,
                     "vendor_cumulative": cumulative, "envelope": got, "expected": expected,
                     "scope": usage.get("scope"), "cost": envelope.get("cost")})
        case.check(f"turn {turn}: usage scope turn", usage.get("scope") == "turn")
        case.check(f"turn {turn}: envelope usage equals the turn's vendor calls",
                   raw["calls"] > 0 and got == expected, {"calls": raw["calls"]})
        if cumulative != raw:
            case.check(f"turn {turn}: envelope usage is not session-cumulative",
                       got["output_tokens"] != cumulative["output_tokens"])
    write_json(case.dir / "usage-compare.json",
               {"source": "the vendor's own session transcript: model-call usage after each "
                          "prompt, one entry per message id; VIA keeps no raw result",
                "rows": rows})


def case_private_profile(run, case):
    """Claude with HOME an empty directory: auth failure, classified `auth`."""
    ws = case.dir / "ws"
    ws.mkdir()
    home = case.dir / "home"
    case.check("scratch HOME has no Claude credential file",
               not (home / ".claude" / ".credentials.json").exists())
    run.snapshot(case, "before")
    receipt, t1, launches = run.turn(
        case, "t1", "spawn", "--harness", "claude", "--model", run.model, "--cwd", str(ws),
        "--wall-ms", "120000", "--background", "--prompt",
        "Reply with the single word READY. Use no tools.", expect=("auth",))
    if receipt is None:
        case.check("spawn accepted", False, t1)
        return
    failure = t1.get("failure") or {}
    case.check("turn failed auth", t1.get("state") == "failed" and failure.get("class") == "auth",
               {"state": t1.get("state"), "failure": failure})
    check_launch(case, "t1", launches, mode="restricted", session=receipt["session_id"],
                 first=True)
    case.check("no credential file created in the scratch HOME",
               not (home / ".claude" / ".credentials.json").exists())
    run.events(case, "events", receipt["session_id"])
    run.snapshot(case, "after")


# --- main -------------------------------------------------------------------

def phase(run, name, config, home, steps):
    """Runs `steps` ([(case, body)]) on a fresh daemon; a phase that cannot
    start, or a run past its budget, blocks them. A settled case is not
    resumed."""
    try:
        if run.budget_hit:
            raise Blocked("budget reached")
        run.start_phase(name, config, home)
    except Blocked as error:
        for case, _ in steps:
            if case.result is None:
                case.result, case.reason = "blocked", str(error)
        return
    for case, body in steps:
        if case.result is None:
            run_case(case, body)


def run_case(case, body):
    try:
        body()
    except Blocked as error:
        case.result, case.reason = "blocked", str(error)
    except Exception as error:  # an unexpected runner error never passes
        case.result, case.reason = "blocked", f"runner error: {type(error).__name__}: {error}"


def positive_usd(text):
    value = float(text)
    if not math.isfinite(value) or value <= 0:
        raise argparse.ArgumentTypeError("must be a finite number above 0")
    return value


def execute(run, cases, info):
    """Discovery, phases and cleanup; every failure lands in `info` or a case."""
    failures = self_test()
    info["parser_self_test"] = "pass" if not failures else failures
    if failures:
        raise Blocked("MCP parser self-test failed")
    info["claude_version_cli"] = subprocess.run(
        [run.claude, "--version"], capture_output=True, text=True, timeout=30).stdout.strip()
    info["via_version"] = subprocess.run(
        [str(run.via), "--version"], capture_output=True, text=True, timeout=30).stdout.strip()
    info["via_sha256"] = sha256(run.via)
    run.prepare()
    print(f"claude --version: {info['claude_version_cli']}; via: {info['via_version']}; "
          f"model {run.model}; budget {run.budget} USD", flush=True)
    recipe, interrupt, never_ask, mcp, usage, auth = cases
    home = auth.dir / "home"
    home.mkdir(mode=0o700)
    phase(run, "restricted", {"restricted": True}, None, [
        (recipe, lambda: case_recipe_continuity(run, recipe)),
        (interrupt, lambda: case_interrupt(run, interrupt)),
        (never_ask, lambda: case_never_ask(run, never_ask)),
        (mcp, lambda: case_mcp_restricted(run, mcp)),
        (usage, lambda: case_usage(run, usage, getattr(recipe, "usage_inputs", []))),
    ])
    phase(run, "unrestricted", {}, None, [(mcp, lambda: case_mcp_unrestricted(run, mcp))])
    phase(run, "unrestricted-mcp-off", {"inherit": {"mcp_servers": False}}, None,
          [(mcp, lambda: case_mcp_off(run, mcp))])
    phase(run, "empty-home", {"restricted": True}, home,
          [(auth, lambda: case_private_profile(run, auth))])


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--via", help="the via binary to qualify through")
    parser.add_argument("--evidence", type=Path, help="a new directory under scratchpad/")
    parser.add_argument("--model", default="haiku")
    parser.add_argument("--budget-usd", type=positive_usd, default=0.75)
    parser.add_argument("--claude", default=shutil.which("claude"),
                        help="Claude Code executable (default: `claude` on PATH)")
    parser.add_argument("--self-test", action="store_true",
                        help="check the MCP debug parser only, then exit")
    args = parser.parse_args()
    if args.self_test:
        failures = self_test()
        print("self-test:", "pass" if not failures else failures)
        return 0 if not failures else 1
    if not args.via or not args.evidence:
        parser.error("--via and --evidence are required")
    args.evidence = args.evidence.resolve()
    if "scratchpad" not in args.evidence.parts:
        parser.error("--evidence must be under a scratchpad/ directory")
    if args.evidence.exists():
        parser.error("--evidence must not exist yet")
    if not args.claude or not os.path.isabs(args.claude):
        parser.error("--claude must be an absolute path (Claude Code not found on PATH)")
    args.evidence.mkdir(parents=True, mode=0o700)

    info = {"runner": "scripts/qualify/claude.py", "started_at": utc_now()}
    run = Run(args)
    cases = [Case(run, name) for name in CASES]
    try:
        execute(run, cases, info)
    except BaseException as error:  # the summary is written on every exit path
        info["runner_error"] = f"{type(error).__name__}: {error}"
    finally:
        try:
            run.stop_daemon()
        except Blocked as error:
            info["final_stop"] = str(error)
        except Exception as error:  # still write the summary
            info["final_stop"] = f"{type(error).__name__}: {error}"
        for case in cases:
            purge_mcp_debug(case)
            if case.result is None and "runner_error" in info:
                case.result, case.reason = "blocked", info["runner_error"]
        if (run.runtime is not None and run.runtime.parent == Path("/tmp")
                and run.daemon_unverified is None and run.daemon is None):
            shutil.rmtree(run.runtime, ignore_errors=True)

    records = [case.settle() for case in cases]
    cli_version = (info.get("claude_version_cli") or "").split(" ")[0] or None
    observed = sorted({v["vendor_version"] for v in run.versions if v["vendor_version"]})
    missing = [v for v in run.versions if not v["vendor_version"]]
    version_ok = bool(cli_version) and observed == [cli_version] and not missing
    stopped = run.daemon_unverified is None and run.daemon is None and all(
        d.get("stopped") and not d.get("descendants_left") for d in run.daemons)
    within_budget = not run.budget_hit and run.spent() < run.budget
    passed = (stopped and version_ok and within_budget and "runner_error" not in info
              and all(record["result"] == "pass" for record in records))
    summary = {
        **info, "ended_at": utc_now(),
        "vendor_versions_seen": observed, "envelopes_without_version": missing,
        "version_check": "pass" if version_ok else "fail",
        "model": run.model, "budget_usd": run.budget, "budget_reached": run.budget_hit,
        "reported_cost_usd": run.spent(), "cost_by_session": run.costs,
        "runtime_dir": (None if run.runtime is None else
                        "rt" if run.runtime.parent == args.evidence else str(run.runtime)),
        "daemons": run.daemons, "daemons_stopped": stopped,
        "daemon_unverified": run.daemon_unverified, "excluded": EXCLUDED,
        "checked_rule": "pass only when every case passes, every daemon stopped, the cost "
                        "stayed under the cap and every envelope reports the one version "
                        "`claude --version` gives; fail, blocked and not_observable never "
                        "pass",
        "result": "pass" if passed else "not_passed",
        "cases": [{"case": r["case"], "result": r["result"], "reason": r["reason"],
                   "evidence": r["evidence"]} for r in records],
    }
    write_json(args.evidence / "summary.json", summary)
    for record in records:
        failing = [c["check"] for c in record["checks"] if c["result"] != "pass"]
        print(f"{record['case']}: {record['result']}"
              + (f" ({record['reason']})" if record["reason"] else "")
              + (f"; not passed: {failing}" if failing else ""), flush=True)
    print(f"versions {observed} (cli {cli_version}); reported cost {run.spent()} USD; "
          f"daemons stopped {stopped}; result {summary['result']}", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
