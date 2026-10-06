#!/usr/bin/env python3
"""Maintainer live qualification of Claude Code through the real `via` binary.

Usage:
  scripts/qualify/claude.py --via target/release/via \
      --evidence scratchpad/qualify/claude-<run> [--model haiku] \
      [--budget-usd 0.75] [--claude PATH]

Adapter design item 4 (docs/workstreams/rust-foundation/adapters/design.md,
"Maintainer live check") and packet §9 (docs/specs/vendors/claude-code.md):
opt-in, never in the gate, run on the maintainer's own Claude login. It
starts a private daemon (fresh VIA_STATE_DIR under the evidence directory, a
short runtime directory for the sockets), runs every case through `via`,
asserts it and keeps per-case evidence: receipts, envelopes, events,
status, the recipe flags of every Claude launch VIA started, and process
snapshots before and after. `summary.json` holds pass, fail, blocked or
not_observable per case, the vendor version and the reported
cost. The run stops at the budget cap; later cases are blocked.

The exit status is 0 only when every case passes and every daemon the run
started has stopped. Infrastructure, authentication (outside the auth
case), quota or budget failure is `blocked`, never a pass; a case VIA gives
no evidence for is `not_observable`, never a pass. A pass adds the vendor
version to the adapter's CHECKED set
(crates/via-adapters/src/claude/plan.rs).

Cases (packet §9 live rows):
  recipe_continuity     claude_live_recipe_continuity
  interrupt             claude_live_interrupt
  never_ask             claude_never_ask, live negative
  mcp_switches          claude_live_mcp_resume and the §4 switch table
  usage                 §3 usage scopes against the vendor's own transcript
  private_profile_auth  §4 private profile: HOME is an empty directory
VIA records no init inventory for Claude (packet §4, via-7c6), so each
mcp_switches inventory check is `not_observable` and the case cannot pass
until VIA records one. Claude's MCP debug lines (`--debug=mcp
--debug-to-stderr`, kept in VIA's `stderr.log`, endpoints redacted) are
supporting evidence: they can fail the case, never pass the inventory.
VIA keeps no raw `result` either, so `usage` compares each envelope with the
vendor's own session transcript (usage numbers only).
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

Privacy: the runner never prints or records environment values, credential
contents or MCP configuration. It reads no credential file. It records MCP
server names and counts only, process flags by name (values only for an
allow-list of recipe flags), and the presence of VIA's process-marker key,
never its value. It reads, sends signals to and waits on no process VIA did
not start; Claude processes alive before the run are excluded by pid.
"""

import argparse
import datetime
import glob
import hashlib
import json
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
# Recipe flags whose value the runner records; every other flag is recorded
# by name only (instructions, schemas and session IDs are never copied).
VALUE_FLAGS = {
    "--model", "--effort", "--max-turns", "--permission-mode", "--permission-prompts",
    "--tools", "--allowedTools", "--input-format", "--output-format",
}
# Vendor arguments that copy Claude's MCP debug lines to its stderr, which
# VIA keeps as the turn's `stderr.log` (C1 §3.12). Unreserved (packet §4).
MCP_DEBUG_ARGS = ["--", "--debug=mcp", "--debug-to-stderr"]
CASES = ("recipe_continuity", "interrupt", "never_ask", "mcp_switches", "usage",
         "private_profile_auth")
TERMINAL = {"completed", "failed", "cancelled", "unknown"}
SOCKET_MAX = 107
ANCHOR_SOCKET_TAIL = len("/anchors/") + 32 + len(".sock")
TURN_WAIT_S = 240
EXCLUDED = [{
    "case": "claude_live_bounds",
    "reason": "CLAUDE-BOUND-1 (packet §8) needs Linux and macOS and its bound matrix is "
              "still open (via-p98.3.4); only the `full` bound is exercised here",
}]


class Blocked(Exception):
    """Infrastructure, auth, quota or budget: the case cannot pass or fail."""


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds")


def parse_time(text):
    return datetime.datetime.fromisoformat(text.replace("Z", "+00:00"))


def write_json(path, value):
    path.write_text(json.dumps(value, indent=1, sort_keys=True) + "\n")


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for chunk in iter(lambda: file.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


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
    """A launch's flags by name; values only for VALUE_FLAGS."""
    flags, values = [], {}
    for index, arg in enumerate(argv[1:], start=1):
        if not arg.startswith("-"):
            continue
        name, eq, attached = arg.partition("=")
        flags.append(name)
        if name in VALUE_FLAGS:
            values[name] = attached if eq else (argv[index + 1] if index + 1 < len(argv) else None)
    return {"flags": flags, "values": values}


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
                  "evidence": str(self.dir.relative_to(self.run.evidence)),
                  "sessions": self.sessions, "checks": self.checks}
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
        self.runtime = self.short_runtime()
        self.base_env = {key: value for key, value in os.environ.items()
                         if not key.startswith("VIA_")}
        self.base_env.update(VIA_STATE_DIR=str(self.state), VIA_RUNTIME_DIR=str(self.runtime))
        self.env = dict(self.base_env)
        self.daemon = None
        self.phase = None
        self.daemons = []
        self.costs = {}
        self.vendor_versions = set()
        self.preexisting_claude = self.claude_pids()
        self.budget_hit = False

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
        out = self.parse(proc.stdout)
        err = self.parse(proc.stderr)
        return proc.returncode, out, err

    @staticmethod
    def parse(text):
        text = text.strip()
        if not text:
            return None
        try:
            return json.loads(text.splitlines()[-1])
        except ValueError:
            return None

    def turn(self, case, label, verb, *args, wait_s=TURN_WAIT_S, expect_auth=False):
        """Spawns or resumes one turn and waits for it; returns (receipt, envelope, launches)."""
        if self.spent() >= self.budget:
            self.budget_hit = True
            raise Blocked(f"budget reached: {self.spent()} USD of {self.budget}")
        with LaunchSampler(self) as sampler:
            # `--json` before the arguments: anything after `--` is vendor_args.
            rc, receipt, err = self.via_call(case.dir, f"{label}-receipt", verb, "--json",
                                             *args, timeout=60)
            if receipt is None:
                write_json(case.dir / f"{label}-launches.json", sampler.launches)
                return None, err, sampler.launches
            if verb == "spawn":
                handle = case.dir / f"{receipt['session_id']}.handle"
                handle.write_text(receipt["handle"])
                handle.chmod(0o600)
                case.sessions.append(receipt["session_id"])
            rc, envelope, err = self.via_call(case.dir, f"{label}-envelope", "wait",
                                              receipt["turn"], "--timeout-ms",
                                              str(wait_s * 1000), "--json",
                                              timeout=wait_s + 30)
        write_json(case.dir / f"{label}-launches.json", sampler.launches)
        if envelope is None:
            raise Blocked(f"{label}: no envelope within {wait_s} s ({err})")
        self.account(envelope)
        failure = envelope.get("failure") or {}
        if failure.get("class") in ("rate_limit",) or (
                failure.get("class") == "auth" and not expect_auth):
            raise Blocked(f"{label}: vendor {failure.get('class')} failure")
        return receipt, envelope, sampler.launches

    def account(self, envelope):
        cost = envelope.get("cost") or {}
        if cost.get("usd") is not None and cost.get("scope") == "session_cumulative":
            session = envelope["session_id"]
            self.costs[session] = max(self.costs.get(session, 0.0), cost["usd"])
        if envelope.get("vendor_version"):
            self.vendor_versions.add(envelope["vendor_version"])

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
        _, status, err = self.via_call(phase_dir, f"{name}-status", "daemon", "status",
                                       "--json", timeout=60)
        if not status or "pid" not in status:
            raise Blocked(f"daemon for phase {name} did not start ({err})")
        stat = proc_stat(status["pid"])
        self.daemon = {"pid": status["pid"], "start_ticks": stat["start_ticks"] if stat else None}
        self.phase = name
        self.daemons.append({"phase": name, **self.daemon})
        print(f"phase {name}: daemon started", flush=True)

    def stop_daemon(self):
        if self.daemon is None:
            return
        phase_dir = self.evidence / "daemon"
        record = self.daemons[-1]
        ours_before = descendants(self.daemon["pid"], all_procs())
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
        procs = all_procs()
        record["descendants_left"] = sorted(pid for pid in ours_before if pid in procs)
        pgrep = subprocess.run(["pgrep", "-f", str(self.via)], capture_output=True, text=True)
        record["pgrep_via"] = [int(pid) for pid in pgrep.stdout.split() if int(pid) != os.getpid()]
        self.daemon = None
        print(f"phase {self.phase}: daemon stopped={record['stopped']}", flush=True)


# --- shared checks -----------------------------------------------------------

def one_launch(case, launches, label):
    case.check(f"{label}: one Claude launch observed", len(launches) == 1,
               {"launches": len(launches)})
    return launches[0] if launches else {"flags": [], "values": {}}


def flag_checks(case, label, launch, present=(), absent=(), values=None):
    flags = launch["flags"]
    for flag in present:
        case.check(f"{label}: argv has {flag}", flag in flags)
    for flag in absent:
        case.check(f"{label}: argv lacks {flag}", flag not in flags)
    for flag, value in (values or {}).items():
        case.check(f"{label}: argv {flag} {value}", launch["values"].get(flag) == value,
                   {"seen": launch["values"].get(flag)})


def completed(case, label, envelope):
    return case.check(f"{label}: completed", envelope.get("state") == "completed",
                      {"state": envelope.get("state"), "failure": envelope.get("failure")})


def mcp_debug_names(case, session, turn):
    """MCP server names in Claude's MCP debug lines on the turn's stderr.log."""
    path = case.run.state / "evidence" / session / str(turn) / "stderr.log"
    if not path.exists():
        return None
    text = path.read_text(errors="replace")
    names = {}
    for line in text.splitlines():
        match = re.search(r'MCP server "([^"]{1,200})"', line) or re.search(
            r'\[MCP\] Server "([^"]{1,200})"', line)
        if not match:
            continue
        state = names.setdefault(match.group(1), "seen")
        if "Successfully connected" in line or "connected with" in line:
            names[match.group(1)] = "connected"
        elif "failed" in line.lower() and state != "connected":
            names[match.group(1)] = "failed"
    # The debug lines name each server's endpoint and ID: configuration the
    # runner must not keep. Only names and states leave this function.
    path.write_text(redact_mcp_debug(text))
    return names


def redact_mcp_debug(text):
    text = re.sub(r"[a-z][a-z0-9+.-]*://\S+", "<redacted-url>", text)
    text = re.sub(r"\bmcpsrv_\w+", "<redacted-id>", text)
    return re.sub(r"(capabilities: ).*", r"\1<redacted>", text)


# --- cases ---------------------------------------------------------------------

def case_recipe_continuity(run, case):
    """Four launches on one session (packet §9 claude_live_recipe_continuity)."""
    ws = case.dir / "ws"
    ws.mkdir()
    nonce = "QX" + secrets.token_hex(3).upper()
    mark = "MARK" + secrets.token_hex(3).upper()
    instructions = case.dir / "instructions.txt"
    instructions.write_text(f"End every reply you write in plain text with a last line "
                            f"that reads exactly: {mark}\n")
    schema_a = case.dir / "schema-a.json"
    write_json(schema_a, {"type": "object", "properties": {"written": {"type": "string"}},
                          "required": ["written"], "additionalProperties": False})
    schema_b = case.dir / "schema-b.json"
    write_json(schema_b, {"type": "object", "properties": {"count": {"type": "integer"}},
                          "required": ["count"], "additionalProperties": False})
    schema_null = case.dir / "schema-null.json"
    schema_null.write_text("null\n")
    write_json(case.dir / "nonces.json", {"nonce": nonce, "instruction_mark": mark})

    run.snapshot(case, "before")
    receipt, t1, launches = run.turn(
        case, "t1", "spawn", "--harness", "claude", "--model", run.model, "--cwd", str(ws),
        "--instructions", str(instructions), "--effort", "low",
        "--output-schema", str(schema_a), "--wall-ms", "180000", "--background",
        "--prompt",
        f"Remember this code for later in our conversation: {nonce}. It is private: never "
        "write it to a file and never pass it to any tool. Now use the Write tool to create "
        "the file note.txt in the current directory with the content hello. Then give your "
        "structured answer with `written` set to the text you wrote.")
    if receipt is None:
        case.check("t1: spawn accepted", False, t1)
        return
    session = receipt["session_id"]
    hf = run.session_file(case, session)
    case.check("t1: receipt effort low", receipt["effective"].get("effort") == "low",
               receipt["effective"])
    completed(case, "t1", t1)
    case.check("t1: effort requested low", (t1.get("effort") or {}).get("requested") == "low",
               t1.get("effort"))
    case.check("t1: structured output A", (t1.get("structured_output") or {}).get("written",
               "").strip() == "hello", t1.get("structured_output"))
    note = ws / "note.txt"
    mode = note.stat().st_mode & 0o777 if note.exists() else None
    case.check("t1: Write tool created note.txt mode 0644", mode == 0o644,
               {"exists": note.exists(), "mode": oct(mode) if mode is not None else None})
    case.check("t1: note.txt holds hello",
               note.exists() and note.read_text().strip() == "hello")
    launch = one_launch(case, launches, "t1")
    flag_checks(case, "t1", launch,
                present=("--session-id", "--restricted", "--append-system-prompt",
                         "--json-schema"),
                absent=("--resume", "--max-turns", "--strict-mcp-config"),
                values={"--effort": "low", "--model": run.model,
                        "--permission-mode": "dontAsk", "--permission-prompts": "none",
                        "--tools": "Read,Write,Edit,Glob,Grep,Bash",
                        "--allowedTools": "Read,Write,Edit,Glob,Grep,Bash"})

    receipt2, t2, launches = run.turn(
        case, "t2", "resume", session, hf, "--effort", "medium",
        "--output-schema", str(schema_b),
        "--prompt", "Without using any tools, give your structured answer with `count` set "
                    "to 7.")
    if receipt2 is None:
        case.check("t2: resume accepted", False, t2)
        return
    completed(case, "t2", t2)
    case.check("t2: structured output B replaced A", t2.get("structured_output") == {"count": 7},
               t2.get("structured_output"))
    launch = one_launch(case, launches, "t2")
    flag_checks(case, "t2", launch, present=("--resume", "--json-schema", "--restricted"),
                absent=("--session-id", "--max-turns"), values={"--effort": "medium"})

    receipt3, t3, launches = run.turn(
        case, "t3", "resume", session, hf, "--output-schema", str(schema_null),
        "--max-steps", "1",
        "--prompt", "Use the Bash tool to run the command `echo step-limit-check`, then tell "
                    "me exactly what it printed.")
    if receipt3 is None:
        case.check("t3: resume accepted", False, t3)
        return
    case.check("t3: receipt max_steps 1", receipt3["effective"].get("max_steps") == 1,
               receipt3["effective"])
    failure = t3.get("failure") or {}
    case.check("t3: step limit gives failed budget_exceeded max_steps",
               t3.get("state") == "failed" and failure.get("class") == "budget_exceeded"
               and t3.get("stop_reason") == "max_steps",
               {"state": t3.get("state"), "failure": failure,
                "stop_reason": t3.get("stop_reason"), "steps": t3.get("steps")})
    case.check("t3: vendor code error_max_turns", failure.get("vendor_code") == "error_max_turns",
               failure.get("vendor_code"))
    launch = one_launch(case, launches, "t3")
    flag_checks(case, "t3", launch, present=("--resume",), absent=("--json-schema",),
                values={"--max-turns": "1"})

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
    case.check("t4: instructions in effect on resume", mark in text)
    case.check("t4: no tool used (steps 1, max_steps 1 inherited)", t4.get("steps") == 1
               and receipt4["effective"].get("max_steps") == 1,
               {"steps": t4.get("steps"), "max_steps": receipt4["effective"].get("max_steps")})
    case.check("t4: schema cleared", t4.get("structured_output") is None and not any(
        w.get("code") == "structured_output_missing" for w in t4.get("warnings", [])))
    leaked = [str(p.relative_to(ws)) for p in ws.rglob("*")
              if p.is_file() and nonce in p.read_text(errors="replace")]
    case.check("nonce never written in the workspace", not leaked, leaked)
    launch = one_launch(case, launches, "t4")
    flag_checks(case, "t4", launch, present=("--resume", "--append-system-prompt"),
                absent=("--json-schema", "--session-id"), values={"--max-turns": "1"})
    ids = {e.get("vendor_session_id") for e in (t1, t2, t3, t4)}
    case.check("one vendor session across four launches", len(ids) == 1 and None not in ids,
               {"distinct": len(ids)})
    status = run.status(case, "status", session)
    case.check("vendor identity verified", status.get("vendor_identity_verified") is True)
    run.events(case, "events", session)
    run.snapshot(case, "after")
    case.usage_inputs = [(session, t) for t in (t1, t2, t3, t4)]


def case_interrupt(run, case):
    """A long foreground Bash tool, cancelled (packet §7, §9 claude_live_interrupt)."""
    ws = case.dir / "ws"
    ws.mkdir()
    tag = "via-qualify-" + secrets.token_hex(4)
    run.snapshot(case, "before", tags=(tag,))
    if run.spent() >= run.budget:
        run.budget_hit = True
        raise Blocked("budget reached")
    with LaunchSampler(run) as sampler:
        _, receipt, err = run.via_call(
            case.dir, "t1-receipt", "spawn", "--harness", "claude", "--model", run.model,
            "--cwd", str(ws), "--wall-ms", "240000", "--background", "--json", "--prompt",
            "Use the Bash tool to run exactly this command in the foreground (not in the "
            "background) and wait for it to finish: "
            f"python3 -c 'import time; tag = \"{tag}\"; time.sleep(90)'\nThen reply DONE.")
        if receipt is None:
            case.check("t1: spawn accepted", False, err)
            return
        session = receipt["session_id"]
        case.sessions.append(session)
        handle = case.dir / f"{session}.handle"
        handle.write_text(receipt["handle"])
        handle.chmod(0o600)
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
        before = run.snapshot(case, "pre-cancel", tags=(tag,))
        if tool:
            marker = has_marker_key(tool["pid"])
            case.check("VIA process marker key reaches the tool", marker is True,
                       {"key_present": marker})
        cancel_at = time.monotonic()
        _, cancel, err = run.via_call(case.dir, "cancel", "cancel", session,
                                      run.session_file(case, session), "--wait", "--json",
                                      timeout=150)
        settled_s = round(time.monotonic() - cancel_at, 3)
        after = run.snapshot(case, "post-cancel", tags=(tag,))
    write_json(case.dir / "t1-launches.json", sampler.launches)
    gone_after = None
    if tool:
        wait_until = time.monotonic() + 3
        while time.monotonic() < wait_until and alive(tool["pid"], tool["start_ticks"]):
            time.sleep(0.1)
        if not alive(tool["pid"], tool["start_ticks"]):
            gone_after = round(time.monotonic() - cancel_at - settled_s, 3)
    run.snapshot(case, "post-cancel-3s", tags=(tag,))
    tool_rows = [row for row in after["processes"]
                 if tool and row["pid"] == tool["pid"] and row["start_ticks"] == tool["start_ticks"]]
    case.check("tool process gone (pid and start time) after cancel",
               bool(tool) and gone_after is not None,
               {"in_post_cancel_snapshot": bool(tool_rows), "gone_s_after_settle": gone_after})
    launch_pids = [(l["pid"], l["start_ticks"]) for l in sampler.launches]
    case.check("Claude launch gone after cancel",
               bool(launch_pids) and not any(alive(*key) for key in launch_pids),
               {"launches": len(launch_pids)})
    _, envelope, _ = run.via_call(case.dir, "t1-envelope", "result", receipt["turn"], "--json")
    envelope = envelope or {}
    run.account(envelope)
    cancel_info = envelope.get("cancel") or {}
    case.check("turn cancelled, interrupted", envelope.get("state") == "cancelled"
               and envelope.get("stop_reason") == "interrupted",
               {"state": envelope.get("state"), "stop_reason": envelope.get("stop_reason")})
    case.check("cancel acknowledged", cancel_info.get("outcome") == "acknowledged",
               {"cancel_reply": cancel, "settled_s": settled_s})
    case.check("cleanup quiescent", cancel_info.get("cleanup") == "quiescent", cancel_info)
    events = run.events(case, "t1-events", receipt["turn"])
    types = [event["type"] for event in events]
    case.check("events cancel.requested then cancel.settled",
               "cancel.requested" in types and "cancel.settled" in types, types)
    receipt2, t2, launches = run.turn(case, "t2", "resume", session,
                                      run.session_file(case, session), "--prompt",
                                      "Reply with the single word AFTER. Use no tools.")
    if receipt2 is None:
        case.check("t2: resume accepted", False, t2)
        return
    completed(case, "t2", t2)
    case.check("t2: same session continues", "AFTER" in (t2.get("final_text") or "")
               and t2.get("vendor_session_id") == envelope.get("vendor_session_id"))
    flag_checks(case, "t2", one_launch(case, launches, "t2"), present=("--resume",))
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
    receipt, t1, _ = run.turn(
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
    # An envelope entry is cut to 256 bytes; its event keeps the full
    # payload (C1 §5).
    denied = [d for d in t1.get("denied_actions", []) if d.get("kind") == "file_write"
              and d.get("target") and str(target).startswith(d["target"])]
    case.check("envelope denial: file_write on the outside path", bool(denied),
               {"denied_actions_total": t1.get("denied_actions_total"),
                "kinds": [d.get("kind") for d in t1.get("denied_actions", [])]})
    case.check("no control request declined (permission prompts none)",
               t1.get("auto_declined_requests_total") == 0,
               t1.get("auto_declined_requests_total"))
    events = run.events(case, "events", receipt["session_id"])
    full = [e for e in events if e["type"] == "action.denied"
            and e.get("kind") == "file_write" and e.get("target") == str(target)]
    case.check("action.denied event: file_write, full outside path",
               bool(full) and any(d.get("event_seq") == full[0]["seq"] for d in denied),
               {"action_denied_events": sum(e["type"] == "action.denied" for e in events)})
    case.check("outside file not created", not target.exists())
    run.snapshot(case, "after")


def mcp_launch(run, case, label, expect_inherit, expect_warning, present, absent, prompt):
    receipt, envelope, launches = run.turn(
        case, label, "spawn", "--harness", "claude", "--model", run.model,
        "--cwd", str(case.dir / "ws"), "--wall-ms", "180000", "--background",
        "--prompt", prompt, *MCP_DEBUG_ARGS)
    if receipt is None:
        case.check(f"{label}: spawn accepted", False, envelope)
        return None, None
    session = receipt["session_id"]
    completed(case, label, envelope)
    status = run.status(case, f"{label}-status", session)
    inherit = status.get("inherit")
    case.check(f"{label}: status inherit matches the packet table", inherit == expect_inherit,
               inherit)
    warning = [w for w in status.get("warnings", []) if w.get("code") == "config_switch_unverified"]
    categories = sorted(c["category"] for w in warning for c in w.get("data", {}).get(
        "categories", []))
    case.check(f"{label}: config_switch_unverified categories", categories == expect_warning,
               categories)
    flag_checks(case, label, one_launch(case, launches, label), present=present, absent=absent)
    return session, envelope


def record_mcp(case, label, session, turn, expect=None, description=None):
    """Records the MCP evidence of one launch; `expect` judges the supporting
    debug lines, which can fail the case but never pass the inventory."""
    names = mcp_debug_names(case, session, turn)
    inventory = {"source": "Claude MCP debug lines in VIA's stderr.log (supporting only)",
                 "count": None if names is None else len(names), "servers": names}
    write_json(case.dir / f"{label}-mcp.json", inventory)
    case.not_observable(f"{label}: init MCP inventory",
                        "VIA records no init inventory: the Claude route keeps none in events, "
                        "status or the evidence folder (packet §4, via-7c6)")
    if names is not None and expect is not None:
        case.check(f"{label}: supporting MCP debug lines: {description}", expect(names),
                   {"count": len(names), "states": sorted(set(names.values()))})
    return names


def case_mcp_restricted(run, case):
    (case.dir / "ws").mkdir(exist_ok=True)
    run.snapshot(case, "restricted-before")
    expect = {c: "off" for c in CATEGORIES}
    expect["mcp_servers"] = "unknown"
    session, _ = mcp_launch(run, case, "restricted", expect, sorted(CATEGORIES),
                            ("--restricted",), ("--strict-mcp-config",),
                            "Reply with the single word READY. Use no tools.")
    if session:
        record_mcp(case, "restricted", session, 1)
    run.snapshot(case, "restricted-after")


def case_mcp_unrestricted(run, case):
    (case.dir / "ws").mkdir(exist_ok=True)
    run.snapshot(case, "unrestricted-before")
    expect = {c: "on" for c in CATEGORIES}
    session, _ = mcp_launch(run, case, "unrestricted-t1", expect, [], (),
                            ("--restricted", "--strict-mcp-config"),
                            "Reply with the single word READY. Use no tools.")
    if not session:
        return
    first = record_mcp(case, "unrestricted-t1", session, 1, bool,
                       "at least one server")
    receipt, t2, launches = run.turn(case, "unrestricted-t2", "resume", session,
                                     run.session_file(case, session), "--prompt",
                                     "Reply with the single word AGAIN. Use no tools.")
    if receipt is None:
        case.check("unrestricted-t2: resume accepted", False, t2)
        return
    completed(case, "unrestricted-t2", t2)
    flag_checks(case, "unrestricted-t2", one_launch(case, launches, "unrestricted-t2"),
                present=("--resume",), absent=("--restricted", "--strict-mcp-config"))
    status = run.status(case, "unrestricted-t2-status", session)
    case.check("unrestricted-t2: status mcp_servers on after resume",
               (status.get("inherit") or {}).get("mcp_servers") == "on", status.get("inherit"))
    record_mcp(case, "unrestricted-t2", session, 2,
               lambda names: bool(first) and set(names) == set(first),
               "the first launch's servers again")
    run.snapshot(case, "unrestricted-after")


def case_mcp_off(run, case):
    run.snapshot(case, "mcp-off-before")
    expect = {c: "on" for c in CATEGORIES}
    expect["mcp_servers"] = "off"
    session, _ = mcp_launch(run, case, "mcp-off", expect, [], ("--strict-mcp-config",),
                            ("--restricted",),
                            "Reply with the single word READY. Use no tools.")
    if session:
        record_mcp(case, "mcp-off", session, 1, lambda names: not names, "no server")
    run.snapshot(case, "mcp-off-after")


def is_prompt(entry):
    """A user entry that opens a turn: the prompt, not a tool result."""
    message = entry.get("message")
    if entry.get("type") != "user" or entry.get("isMeta") or not isinstance(message, dict):
        return False
    content = message.get("content")
    return not (isinstance(content, list) and any(
        isinstance(part, dict) and part.get("type") == "tool_result" for part in content))


def vendor_turn_usage(path):
    """Per-prompt sums of the vendor's per-call usage, one entry per message id."""
    keys = ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens",
            "output_tokens")
    turns = []
    with open(path) as file:
        for line in file:
            entry = json.loads(line)
            if is_prompt(entry):
                turns.append({})
                continue
            message = entry.get("message")
            if (entry.get("type") != "assistant" or entry.get("isSidechain") or not turns
                    or not isinstance(message, dict)
                    or not isinstance(message.get("usage"), dict)):
                continue
            usage = {key: message["usage"].get(key) or 0 for key in keys}
            prior = turns[-1].get(message.get("id"))
            if prior is None or usage["output_tokens"] >= prior["output_tokens"]:
                turns[-1][message.get("id")] = usage
    return [{key: sum(call[key] for call in calls.values()) for key in keys}
            | {"calls": sum(1 for call in calls.values() if any(call.values()))}
            for calls in turns]


def case_usage(run, case, inputs):
    """Envelope usage against the vendor's own per-call accounting (packet §3).

    VIA keeps no raw `result` (C1 `logs.transcript` is null on this route),
    so the reference is the vendor's own session transcript: each prompt
    and the usage of each model call after it. One VIA turn is one prompt.
    """
    if not inputs:
        case.result, case.reason = "blocked", "recipe_continuity produced no turns"
        return
    session, first = inputs[0]
    vendor_id = first.get("vendor_session_id")
    _, logs, _ = run.via_call(case.dir, "logs", "logs", f"{session}/1", "--json")
    case.check("VIA keeps no vendor transcript (logs.transcript null)",
               (logs or {}).get("transcript") is None, logs)
    home = Path(run.base_env["HOME"])
    found = glob.glob(str(home / ".claude" / "projects" / "*" / f"{vendor_id}.jsonl"))
    if len(found) != 1:
        case.not_observable("vendor per-call usage",
                            f"vendor transcript files found: {len(found)}")
        return
    vendor = vendor_turn_usage(found[0])
    case.check("one vendor prompt per VIA turn", len(vendor) == len(inputs),
               {"vendor_prompts": len(vendor), "via_turns": len(inputs)})
    rows, last_cost, cumulative = [], None, {}
    for (_, envelope), raw in zip(inputs, vendor):
        turn = envelope["turn"]
        cost = envelope.get("cost") or {}
        case.check(f"turn {turn}: cost scope session_cumulative",
                   cost.get("scope") == "session_cumulative")
        if last_cost is not None and cost.get("usd") is not None:
            case.check(f"turn {turn}: cost non-decreasing", cost["usd"] >= last_cost,
                       {"previous": last_cost, "cost": cost["usd"]})
        last_cost = cost.get("usd", last_cost)
        cumulative = {key: cumulative.get(key, 0) + value for key, value in raw.items()}
        usage = envelope.get("usage") or {}
        expected = {"input_tokens": raw["input_tokens"] + raw["cache_creation_input_tokens"]
                    + raw["cache_read_input_tokens"],
                    "cached_input_tokens": raw["cache_read_input_tokens"],
                    "output_tokens": raw["output_tokens"]}
        got = {key: usage.get(key) for key in expected}
        rows.append({"turn": turn, "state": envelope.get("state"), "vendor_turn": raw,
                     "vendor_cumulative": cumulative, "envelope": got, "expected": expected,
                     "scope": usage.get("scope"), "cost": cost})
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
        "Reply with the single word READY. Use no tools.", expect_auth=True)
    if receipt is None:
        case.check("spawn accepted", False, t1)
        return
    failure = t1.get("failure") or {}
    case.check("turn failed auth", t1.get("state") == "failed" and failure.get("class") == "auth",
               {"state": t1.get("state"), "failure": failure})
    case.check("one Claude launch observed", len(launches) == 1, len(launches))
    case.check("no credential file created in the scratch HOME",
               not (home / ".claude" / ".credentials.json").exists())
    run.events(case, "events", receipt["session_id"])
    run.snapshot(case, "after")


# --- main -------------------------------------------------------------------

def phase(run, name, config, home, steps):
    """Runs `steps` ([(case, body)]) on a fresh daemon; a phase that cannot
    start blocks them. A case already settled (blocked) is not resumed."""
    try:
        run.start_phase(name, config, home)
    except Blocked as error:
        for case, _ in steps:
            if case.result is None:
                case.result, case.reason = "blocked", str(error)
        return
    for case, body in steps:
        if case.result is None:
            run_case(run, case, body)


def run_case(run, case, body):
    try:
        body()
    except Blocked as error:
        case.result, case.reason = "blocked", str(error)
    except Exception as error:  # an unexpected runner error never passes
        case.result, case.reason = "blocked", f"runner error: {type(error).__name__}: {error}"
    return case


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--via", required=True, help="the via binary to qualify through")
    parser.add_argument("--evidence", required=True, type=Path,
                        help="a new directory under scratchpad/")
    parser.add_argument("--model", default="haiku")
    parser.add_argument("--budget-usd", type=float, default=0.75)
    parser.add_argument("--claude", default=shutil.which("claude"),
                        help="Claude Code executable (default: `claude` on PATH)")
    args = parser.parse_args()
    args.evidence = args.evidence.resolve()
    if "scratchpad" not in args.evidence.parts:
        parser.error("--evidence must be under a scratchpad/ directory")
    if args.evidence.exists():
        parser.error("--evidence must not exist yet")
    if not args.claude or not os.path.isabs(args.claude):
        parser.error("--claude must be an absolute path (Claude Code not found on PATH)")
    args.evidence.mkdir(parents=True, mode=0o700)

    started = utc_now()
    version = subprocess.run([args.claude, "--version"], capture_output=True, text=True,
                             timeout=30).stdout.strip()
    run = Run(args)
    via_version = subprocess.run([str(run.via), "--version"], capture_output=True,
                                 text=True, timeout=30).stdout.strip()
    print(f"claude --version: {version}; via: {via_version}; model {run.model}; "
          f"budget {run.budget} USD", flush=True)
    cases = [Case(run, name) for name in CASES]
    recipe, interrupt, never_ask, mcp, usage, auth = cases
    home = auth.dir / "home"
    home.mkdir(mode=0o700)
    try:
        phase(run, "restricted", {"restricted": True}, None, [
            (recipe, lambda: case_recipe_continuity(run, recipe)),
            (interrupt, lambda: case_interrupt(run, interrupt)),
            (never_ask, lambda: case_never_ask(run, never_ask)),
            (mcp, lambda: case_mcp_restricted(run, mcp)),
            (usage, lambda: case_usage(run, usage, getattr(recipe, "usage_inputs", []))),
        ])
        phase(run, "unrestricted", {}, None,
              [(mcp, lambda: case_mcp_unrestricted(run, mcp))])
        phase(run, "unrestricted-mcp-off", {"inherit": {"mcp_servers": False}}, None,
              [(mcp, lambda: case_mcp_off(run, mcp))])
        phase(run, "empty-home", {"restricted": True}, home,
              [(auth, lambda: case_private_profile(run, auth))])
    finally:
        run.stop_daemon()
        if run.runtime.parent == Path("/tmp"):
            shutil.rmtree(run.runtime, ignore_errors=True)

    records = [case.settle() for case in cases]
    stopped = all(d.get("stopped") and not d.get("descendants_left") for d in run.daemons)
    passed = stopped and all(record["result"] == "pass" for record in records)
    summary = {
        "runner": "scripts/qualify/claude.py",
        "started_at": started, "ended_at": utc_now(),
        "claude_version_cli": version,
        "vendor_versions_seen": sorted(run.vendor_versions),
        "via_version": via_version, "via_sha256": sha256(run.via),
        "model": run.model, "budget_usd": run.budget, "budget_reached": run.budget_hit,
        "reported_cost_usd": run.spent(), "cost_by_session": run.costs,
        "runtime_dir": "rt" if run.runtime.parent == args.evidence else "/tmp (short socket path)",
        "daemons": run.daemons, "daemons_stopped": stopped, "excluded": EXCLUDED,
        "checked_rule": "pass only when every case passes and every daemon stopped; "
                        "fail, blocked and not_observable never pass",
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
    print(f"reported cost {run.spent()} USD; result {summary['result']}", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
