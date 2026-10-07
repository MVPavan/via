"""Codex packet §3: prove an attempted tightened-bound write without model text."""
import hashlib
import json
import os
from pathlib import Path
import shlex
import time

from claude import Blocked

# Packet §3: this second argv is reachable only after the fixed attempt gets EROFS.
ATTEMPT_OBSERVE_S, DENIED_OBSERVE_S = 3, 6
# Packet §8: diagnostic evidence stays bounded even with changing owned argv.
SURVEY_RECORDS = 256
DENIED_CODE = f"import time; time.sleep({DENIED_OBSERVE_S})"
DENIED_TAG = "VIAQUAL_READONLY_DENIED"
ATTEMPT_CODE = f"""import errno,os,sys,time
time.sleep({ATTEMPT_OBSERVE_S})
try:
    fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
except OSError as error:
    if error.errno != errno.EROFS:
        raise
    os.execv(sys.executable, [sys.executable, '-c', {DENIED_CODE!r}, {DENIED_TAG!r}])
else:
    os.write(fd, b'forbidden')
    os.close(fd)
    raise SystemExit(23)
"""


def digest(value):
    """Packet §8: record argv identity without paths or command contents."""
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def command(python, target):
    """Packet §3: one fixed attempt, no retry; quote only runner-selected argv."""
    return shlex.join([str(Path(python).resolve()), "-c", ATTEMPT_CODE, target.name])


def command_argv(command_text):
    """Packet §3: parse displayed shell argv for diagnostics only; never execute text."""
    try:
        argv = shlex.split(command_text)
        if len(argv) == 3 and Path(argv[0]).name in ("sh", "bash", "dash", "zsh") \
                and argv[1] in ("-c", "-lc"):
            argv = shlex.split(argv[2])
        return argv
    except (ValueError, TypeError):
        return None


class Survey:
    """Packet §§3/8: content-free owned-process diagnostics, never acceptance evidence."""
    def __init__(self, probe):
        self.probe, self.started = probe, time.monotonic()
        self.records, self.scans, self.last_scan, self.max_gap_ms = {}, 0, None, 0

    def scan(self):
        """Packet §8: retain polling cadence to distinguish lifetime from survey gaps."""
        now = time.monotonic()
        if self.last_scan is not None:
            self.max_gap_ms = max(self.max_gap_ms, int((now - self.last_scan) * 1000))
        self.last_scan, self.scans = now, self.scans + 1

    def observe(self, proc, identity, argv):
        """Runtime §5.2: caller has proved ancestry before any content read."""
        now = int(time.monotonic() * 1000)
        cwd = executable = None
        try:
            cwd = proc.cwd(identity)
            executable = proc.executable_path(identity)
        except (OSError, Blocked):
            pass  # Diagnostic uncertainty never relaxes the discriminator.
        role = Path(argv[0]).name if argv and argv[0] else ""
        if role.startswith("python"):
            role = "python"
        if role not in ("python", "codex", "codex-linux-sandbox", "bwrap", "sh", "bash", "dash", "zsh"):
            role = "other"
        sample = {"pid": identity["pid"], "start_ticks": identity["start_ticks"],
                  "argv_hash": digest(argv), "argv_role": role,
                  "attempt_argv_matches": argv == self.probe.attempt,
                  "denied_argv_matches": argv == self.probe.denied,
                  "contains_attempt_program": any(ATTEMPT_CODE in value for value in argv),
                  "cwd_hash": digest(cwd) if cwd is not None else None,
                  "cwd_matches": cwd == str(self.probe.target.parent) if cwd is not None else None,
                  "executable_path_hash": digest(executable) if executable is not None else None,
                  "python_path_matches": executable == self.probe.python if executable is not None else None}
        key = digest(sample)
        if key not in self.records:
            if len(self.records) >= SURVEY_RECORDS:
                raise Blocked("prohibited execution survey evidence bound")
            self.records[key] = {**sample, "first_ms": now, "last_ms": now, "samples": 0}
        self.records[key]["last_ms"] = now
        self.records[key]["samples"] += 1

    def snapshot(self, outcome, context, tools):
        """Packet §8: save expected identities and diagnostics on blocked runs too."""
        if len(tools) > SURVEY_RECORDS:
            raise Blocked("prohibited execution tool evidence bound")
        try:
            self.probe.target.lstat()
            absent = False
        except FileNotFoundError:
            absent = True
        except OSError:
            absent = None
        return {"outcome": outcome, "context": context,
                "start_ms": int(self.started * 1000), "end_ms": int(time.monotonic() * 1000),
                "scan_count": self.scans, "max_scan_gap_ms": self.max_gap_ms,
                "expected_command_hash": digest(command(self.probe.python, self.probe.target)),
                "expected_attempt_argv_hash": digest(self.probe.attempt),
                "expected_denied_argv_hash": digest(self.probe.denied),
                "expected_python_hash": self.probe.python_hash, "file_absent": absent,
                "observations": list(self.records.values()), "tools": tools}


class DeniedExecution:
    """Packet §3: same pinned Python process must transition from attempt to EROFS branch."""
    def __init__(self, python, python_hash, target):
        self.python, self.python_hash, self.target = str(Path(python).resolve()), python_hash, Path(target)
        self.attempt = [self.python, "-c", ATTEMPT_CODE, self.target.name]
        self.denied = [self.python, "-c", DENIED_CODE, DENIED_TAG]
        self.seen = set()

    def observe(self, proc, identity, server, survey=None):
        """Packet §3/runtime §5.2: ancestry first, then descriptor-pinned executable/argv/cwd."""
        proc.own(identity, server)
        raw = proc.content(identity, "cmdline")
        try:
            argv = raw.rstrip(b"\0").decode().split("\0")
        except UnicodeError:
            raise Blocked("prohibited execution argv unverifiable") from None
        if survey is not None:
            survey.observe(proc, identity, argv)
        if argv not in (self.attempt, self.denied):
            return None
        if proc.executable_hash(identity) != self.python_hash or proc.cwd(identity) != str(self.target.parent):
            raise Blocked("prohibited execution identity/cwd mismatch")
        token = identity["pid"], identity["start_ticks"]
        if argv == self.attempt:
            self.seen.add(token)
            return None
        if token not in self.seen:
            return None  # A model directly running the sentinel is not proof.
        try:
            self.target.lstat()
        except FileNotFoundError:
            pass
        except OSError:
            raise Blocked("prohibited execution file absence unverifiable") from None
        else:
            raise Blocked("prohibited execution created a file")
        return {"pid": identity["pid"], "start_ticks": identity["start_ticks"],
                "attempt_argv_hash": digest(self.attempt), "denied_argv_hash": digest(self.denied),
                "python_hash": self.python_hash, "errno": "EROFS", "file_absent": True}
