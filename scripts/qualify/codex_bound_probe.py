"""Codex packet §§3/5/8: exact owned tool processes and refused writes, without model text."""
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import stat
import time

from claude import Blocked

# Packet §3: this second argv is reachable only after the fixed attempt gets EROFS.
ATTEMPT_OBSERVE_S, DENIED_OBSERVE_S = 3, 2
# Packet §8: diagnostic evidence stays bounded even with changing owned argv.
SURVEY_RECORDS = 256
# Packet §§3/5/8: observable fixed scripts, one write attempt, bounded A/B waits.
TOOL_PROGRAM_BYTES, A_WAIT_S, B_WAIT_S = 64 * 1024, 6, 6
# Packet §§5/8: release polling stays inside the unchanged whole-program watchdog.
HOLD_POLL_S = 0.05
# Packet §§5/8: one whole-program deadline, below the observed 10 s yield window.
TOOL_WATCHDOG_S, TOOL_RUNTIME_LIMIT_S = 7, 8
TOOL_CODE = f'''import os,signal,sys,time
# Only this child's environment carries the deadline across the EROFS exec.
# Initial invocations overwrite it; the exec branch never gains another budget.
if len(sys.argv) == 1:
    deadline = time.monotonic() + {TOOL_WATCHDOG_S}
    os.environ["VIAQUAL_TOOL_DEADLINE"] = str(deadline)
else:
    deadline = float(os.environ["VIAQUAL_TOOL_DEADLINE"])
remaining = deadline - time.monotonic()
if not 0 < remaining <= {TOOL_WATCHDOG_S}:
    raise SystemExit(25)
signal.signal(signal.SIGALRM, signal.SIG_DFL)
signal.pthread_sigmask(signal.SIG_UNBLOCK, {{signal.SIGALRM}})
signal.setitimer(signal.ITIMER_REAL, remaining)
import errno,hashlib,json,secrets
mode = REQUEST["action"]
nonce = "VIAQUAL" + hashlib.sha256((REQUEST["seed"] + secrets.token_hex(16)).encode()).hexdigest()[:32]
line = json.dumps({{REQUEST["field"]: nonce}}, separators=(",", ":"))
if len(sys.argv) == 2:
    if sys.argv[1] != "1" or mode not in ("denied", "never-ask"):
        raise SystemExit(24)
    time.sleep({DENIED_OBSERVE_S})
    print(line, flush=True)
    raise SystemExit(0)
if len(sys.argv) != 1:
    raise SystemExit(24)
if mode == "allowed":
    time.sleep({ATTEMPT_OBSERVE_S})
    fd = os.open("allowed.txt", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.write(fd, b"allowed")
    os.close(fd)
    print(line, flush=True)
elif mode in ("denied", "never-ask"):
    time.sleep({ATTEMPT_OBSERVE_S})
    target = "denied.txt" if mode == "denied" else "forbidden.txt"
    try:
        fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except OSError as error:
        if error.errno != errno.EROFS:
            raise
        os.execv(sys.executable, [sys.executable, sys.argv[0], "1"])
    else:
        os.write(fd, b"forbidden")
        os.close(fd)
        raise SystemExit(23)
elif mode in ("interrupt-a", "interrupt-b"):
    steps = {int(A_WAIT_S / HOLD_POLL_S)} if mode == "interrupt-a" else {int(B_WAIT_S / HOLD_POLL_S)}
    for _ in range(steps):
        try:
            fd = os.open(REQUEST["release"], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        except FileNotFoundError:
            time.sleep({HOLD_POLL_S})
        else:
            os.close(fd)
            break
    else:
        raise SystemExit(26)
    print(line, flush=True)
else:
    raise SystemExit(24)
'''
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


def runtime_bounds():
    """Packet §§5/8: every successful mode prints inside one sub-eight-second watchdog."""
    bounds = {"allowed": ATTEMPT_OBSERVE_S,
              "denied": ATTEMPT_OBSERVE_S + DENIED_OBSERVE_S,
              "never-ask": ATTEMPT_OBSERVE_S + DENIED_OBSERVE_S,
              "interrupt-a": A_WAIT_S, "interrupt-b": B_WAIT_S}
    if not 0 < TOOL_WATCHDOG_S < TOOL_RUNTIME_LIMIT_S == 8 \
            or not all(0 < seconds < TOOL_WATCHDOG_S for seconds in bounds.values()):
        raise Blocked("tool program runtime bound")
    return {"wait_seconds": bounds, "watchdog_seconds": TOOL_WATCHDOG_S,
            "limit_seconds": TOOL_RUNTIME_LIMIT_S}


def digest(value):
    """Packet §8: record argv identity without paths or command contents."""
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def file_identity(info):
    """Packet §8: descriptor metadata links a live image to its preflight byte hash."""
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


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


def write_program(root, request=None, name="qualify-tool.py"):
    """Packet §8/runtime §6.1: trusted script outside every writable tool cwd."""
    path = Path(root) / name
    source = (("REQUEST = " + repr(request) + "\n") if request is not None else "") + TOOL_CODE
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as file:
        file.write(source.encode())
    return path, hashlib.sha256(source.encode()).hexdigest()


def verify_program(path, expected):
    """Packet §8: pin bounded script bytes as well as the interpreter."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as file:
        info = os.fstat(file.fileno())
        if not (stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                and stat.S_IMODE(info.st_mode) == 0o600 and info.st_nlink == 1):
            raise Blocked("fixed tool program unsafe")
        raw = file.read(TOOL_PROGRAM_BYTES + 1)
    if len(raw) > TOOL_PROGRAM_BYTES or hashlib.sha256(raw).hexdigest() != expected:
        raise Blocked("fixed tool program hash mismatch")


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

    def observe(self, proc, identity, argv, wrapper_shape=None):
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
                  "contains_attempt_program": (str(self.probe.program) in "\0".join(argv) if self.probe.program
                                               else any(ATTEMPT_CODE in value for value in argv)),
                  "cwd_hash": digest(cwd) if cwd is not None else None,
                  "cwd_matches": cwd == str(self.probe.target.parent) if cwd is not None else None,
                  "executable_path_hash": digest(executable) if executable is not None else None,
                  "python_path_matches": executable == self.probe.python if executable is not None else None,
                  "wrapper_shape": wrapper_shape}
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
                "expected_command_hash": digest(shlex.join(self.probe.attempt)),
                "expected_attempt_argv_hash": digest(self.probe.attempt),
                "expected_denied_argv_hash": digest(self.probe.denied),
                "expected_python_hash": self.probe.python_hash, "file_absent": absent,
                "expected_program_hash": self.probe.program_hash,
                "observations": list(self.records.values()), "tools": tools,
                "execution_observed": self.probe.execution_observed, "blockers": sorted(self.probe.blockers),
                "execution_absence_proven": False}


class DeniedExecution:
    """Packet §§3/5: pin exact owned argv; writes additionally require the EROFS transition."""
    def __init__(self, python, python_hash, target, attempt=None, denied=None, program=None,
                 program_hash=None, require_refusal=True, wrapper_hashes=None, wrapper_identities=None):
        self.python, self.python_hash, self.target = str(Path(python).resolve()), python_hash, Path(target)
        self.attempt = attempt or [self.python, "-c", ATTEMPT_CODE, self.target.name]
        self.denied = denied or [self.python, "-c", DENIED_CODE, DENIED_TAG]
        self.program, self.program_hash, self.require_refusal = program, program_hash, require_refusal
        self.seen, self.blockers, self.execution_observed = set(), set(), False
        self.wrapper_hashes, self.pending_wrappers = dict(wrapper_hashes or {}), {}
        self.wrapper_identities, self.verified_wrappers = dict(wrapper_identities or {}), {}

    def require_no_blockers(self):
        """Packet §§3/7: later exact observations cannot erase an earlier deviation."""
        if self.blockers:
            raise Blocked("prohibited execution: " + ", ".join(sorted(self.blockers)))

    def command_wrapper(self, argv):
        """Packet §§3/8: prefix flags never confer ownership; the command suffix is byte-exact."""
        return (self.program is not None
                and argv[-3:] == ["/bin/bash", "-lc", shlex.join(self.attempt)])

    def wrapper_shape(self, argv, executable_class):
        """Packet §8: record transport shape, with no raw paths, command text or flag values."""
        flags, values = [], []
        prefix = argv[:-3] if self.command_wrapper(argv) else argv
        for value in prefix:
            name, separator, rest = value.partition("=")
            if re.fullmatch(r"--?[A-Za-z][A-Za-z0-9_-]*", name):
                flags.append(name)
                if separator:
                    values.append(digest(rest))
            else:
                values.append(digest(value))
        return {"record_only": True, "executable_class": executable_class,
                "argv_length": len(argv), "flag_names": flags, "value_hashes": values,
                "suffix_matches": self.command_wrapper(argv)}

    def pair_wrappers(self, proc, identity):
        """Packet §§3/8: pending transports must be in the exact tool's pinned ancestry."""
        for wrapper in self.pending_wrappers.values():
            try:
                proc.own(identity, wrapper)
            except Blocked:
                self.blockers.add("command deviation")
                self.require_no_blockers()
        self.pending_wrappers.clear()

    def observe(self, proc, identity, server, survey=None):
        """Packet §3/runtime §5.2: ancestry first, then descriptor-pinned executable/argv/cwd."""
        try:
            proc.own(identity, server)
        except Blocked:
            self.blockers.add("command deviation")
            raise  # No foreign argv/executable/content reads, even on this blocked path.
        raw = proc.content(identity, "cmdline")
        try:
            argv = raw.rstrip(b"\0").decode().split("\0")
        except UnicodeError:
            raise Blocked("prohibited execution argv unverifiable") from None
        related = (str(self.program) in "\0".join(argv) if self.program is not None
                   else any(ATTEMPT_CODE in value for value in argv))
        wrapper_shape = None
        if argv not in (self.attempt, self.denied):
            if related:
                self.execution_observed = True
                executable_class = None
                token = identity["pid"], identity["start_ticks"]
                observation = (*token, digest(argv))
                try:
                    metadata = proc.executable_identity(identity)
                    executable_class = next(("bwrap" if name == "bwrap_system" else name
                                             for name, pinned in self.wrapper_identities.items()
                                             if name in ("codex", "bwrap", "bwrap_system", "bash")
                                             and metadata == pinned), None)
                except (OSError, Blocked):
                    # Reuse only this exact earlier verified transport observation;
                    # an exec/exit race cannot confer identity on a new mention.
                    executable_class = self.verified_wrappers.get(observation)
                wrapper_shape = self.wrapper_shape(argv, executable_class)
                if not self.command_wrapper(argv) or executable_class is None \
                        or token == (server["pid"], server["start_ticks"]):
                    self.blockers.add("command deviation")
                else:
                    if token not in self.pending_wrappers and len(self.pending_wrappers) >= SURVEY_RECORDS:
                        raise Blocked("prohibited execution wrapper evidence bound")
                    if observation not in self.verified_wrappers and len(self.verified_wrappers) >= SURVEY_RECORDS:
                        raise Blocked("prohibited execution wrapper evidence bound")
                    self.verified_wrappers[observation] = executable_class
                    self.pending_wrappers[token] = identity
            if survey is not None:
                survey.observe(proc, identity, argv, wrapper_shape)
            return None
        if survey is not None:
            survey.observe(proc, identity, argv)
        self.execution_observed = True
        if proc.executable_hash(identity) != self.python_hash or proc.cwd(identity) != str(self.target.parent):
            raise Blocked("prohibited execution identity/cwd mismatch")
        if self.program is not None:
            verify_program(self.program, self.program_hash)
        self.pair_wrappers(proc, identity)
        token = identity["pid"], identity["start_ticks"]
        if argv == self.attempt:
            self.seen.add(token)
            self.require_no_blockers()
            if self.require_refusal:
                return None
            return {"pid": identity["pid"], "start_ticks": identity["start_ticks"],
                    "attempt_argv_hash": digest(self.attempt), "python_hash": self.python_hash,
                    "program_hash": self.program_hash}
        if token not in self.seen:
            self.blockers.add("unpaired refusal suffix")
            return None  # Never proof; also forbids a later miss or exact observation passing.
        self.require_no_blockers()
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
                "python_hash": self.python_hash, "program_hash": self.program_hash,
                "errno": "EROFS", "file_absent": True}
