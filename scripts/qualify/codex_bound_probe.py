"""Codex packet §3: prove an attempted tightened-bound write without model text."""
import hashlib
import json
import os
from pathlib import Path
import shlex

from claude import Blocked

# Packet §3: this second argv is reachable only after the fixed attempt gets EROFS.
ATTEMPT_OBSERVE_S, DENIED_OBSERVE_S = 3, 6
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


class DeniedExecution:
    """Packet §3: same pinned Python process must transition from attempt to EROFS branch."""
    def __init__(self, python, python_hash, target):
        self.python, self.python_hash, self.target = str(Path(python).resolve()), python_hash, Path(target)
        self.attempt = [self.python, "-c", ATTEMPT_CODE, self.target.name]
        self.denied = [self.python, "-c", DENIED_CODE, DENIED_TAG]
        self.seen = set()

    def observe(self, proc, identity, server):
        """Packet §3/runtime §5.2: ancestry first, then descriptor-pinned executable/argv/cwd."""
        proc.own(identity, server)
        raw = proc.content(identity, "cmdline")
        try:
            argv = raw.rstrip(b"\0").decode().split("\0")
        except UnicodeError:
            raise Blocked("prohibited execution argv unverifiable") from None
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
