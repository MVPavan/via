"""Codex packet §§3/7: private run-wide native submission authority and accounting."""
import contextlib
import fcntl
import json
import os
from pathlib import Path
import stat
import time

from claude import Blocked

# Packet §7/runtime §8: seven base submissions plus two verified no-command re-asks.
BASE_TURN_LIMIT, REASK_LIMIT, ACTIVE_LIMIT, LEDGER_BYTES, LOCK_S = 7, 2, 2, 256 * 1024, 2
# Packet §8 qualification: owner-authorized exception, once per zero-command case.
CASE_REASK_LIMIT = 1
TURN_LIMIT = BASE_TURN_LIMIT + REASK_LIMIT


def require(ok, reason):
    """Packet §7: unavailable control evidence cannot authorize native work."""
    if not ok:
        raise Blocked("reservation control: " + reason)


class Reservations:
    """Packet §7: every proxy consumes the same atomic, single-use reservations."""
    def __init__(self, path, deadline=None):
        self.path = Path(path)
        self.lock = self.path.with_suffix(".lock")
        if deadline is not None:
            fd = os.open(self.lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            os.close(fd)
            fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "w") as file:
                json.dump({"version": 1, "deadline": deadline, "closed": False, "rows": {}}, file)
                file.flush()
                os.fsync(file.fileno())

    @staticmethod
    def private(fd):
        info = os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                and stat.S_IMODE(info.st_mode) == 0o600 and info.st_nlink == 1, "unsafe file")

    @contextlib.contextmanager
    def transaction(self, write=True):
        fd = os.open(self.lock, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            self.private(fd)
            deadline = time.monotonic() + LOCK_S
            while True:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    require(time.monotonic() < deadline, "lock deadline")
                    time.sleep(0.01)
            source = os.open(self.path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(source, "rb") as file:
                self.private(file.fileno())
                raw = file.read(LEDGER_BYTES + 1)
            require(len(raw) <= LEDGER_BYTES, "byte bound")
            try:
                state = json.loads(raw)
                require(state["version"] == 1 and type(state["closed"]) is bool
                        and type(state["rows"]) is dict and len(state["rows"]) <= TURN_LIMIT,
                        "schema or turn bound")
            except (KeyError, TypeError, ValueError):
                raise Blocked("reservation control: unreadable schema") from None
            yield state
            if write:
                raw = json.dumps(state, sort_keys=True).encode()
                require(len(raw) <= LEDGER_BYTES, "byte bound")
                temporary = self.path.with_suffix(".tmp")
                target = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                try:
                    with os.fdopen(target, "wb") as file:
                        file.write(raw)
                        file.flush()
                        os.fsync(file.fileno())
                    os.replace(temporary, self.path)
                finally:
                    temporary.unlink(missing_ok=True)
        except OSError:
            raise Blocked("reservation control: file or lock unverifiable") from None
        finally:
            os.close(fd)

    def reserve(self, key, verb, prompt, cwd, thread=None, tool=None, reask_of=None):
        """Packet §7: admit before CLI launch, never recycle an accepted reservation."""
        with self.transaction() as state:
            rows = state["rows"]
            require(not state["closed"] and time.monotonic() < state["deadline"], "closed or expired")
            require(len(rows) < TURN_LIMIT and sum(row["active"] for row in rows.values()) < ACTIVE_LIMIT,
                    "turn/concurrency cap")
            if reask_of is None:
                require(sum(row["reask_of"] is None for row in rows.values()) < BASE_TURN_LIMIT,
                        "base turn cap")
            else:
                parent = rows.get(reask_of)
                require(sum(row["reask_of"] is not None for row in rows.values()) < REASK_LIMIT,
                        "re-ask cap")
                require(parent is not None and parent["miss"] and parent["reask_of"] is None
                        and not parent["active"] and verb == "resume" and parent["thread"] == thread
                        and parent["prompt"] == prompt and parent["cwd"] == cwd and parent["tool"] == tool
                        and not any(row["reask_of"] == reask_of for row in rows.values()),
                        "re-ask is not a unique same-thread same-prompt tool miss")
            require(key not in rows and verb in ("spawn", "resume"), "duplicate or invalid submission")
            require(verb == "spawn" or thread is not None, "resume identity unavailable")
            rows[key] = {"verb": verb, "prompt": prompt, "cwd": cwd, "thread": thread,
                         "active": True, "thread_start": None, "turn_start": None,
                         "receipt": None, "envelope": None, "tool": tool,
                         "miss": False, "reask_of": reask_of}

    def mark_miss(self, key):
        """Packet §§7/8: only a settled first tool miss may authorize one same-thread re-ask."""
        with self.transaction() as state:
            row = state["rows"][key]
            require(not row["active"] and row["envelope"] is not None
                    and row["envelope"]["state"] == "completed" and row["reask_of"] is None
                    and row["tool"] is not None and row["tool"]["argv"] is not None
                    and not row["miss"], "invalid tool miss")
            row["miss"] = True

    def tool(self, generation, thread, turn=None):
        """Packet §§3/7: expected command digest, including replies still establishing a turn."""
        with self.transaction(write=False) as state:
            rows = [row for row in state["rows"].values() if row["thread"] == thread
                    and row["turn_start"] is not None and row["turn_start"]["generation"] == generation
                    and (row["active"] if turn is None else row["turn_start"].get("turn") == turn)]
            require(len(rows) <= 1, "ambiguous tool reservation")
            if rows and rows[0]["miss"]:
                return {**rows[0]["tool"], "miss": True}
            return rows[0].get("tool") if rows else None

    def close(self):
        """Packet §7: uncertain effects close admission; cleanup stays available."""
        with self.transaction() as state:
            state["closed"] = True

    def claim(self, method, generation, request, cwd, thread=None, prompt=None):
        """Packet §§3/7: consume exactly one compatible reservation before forwarding."""
        with self.transaction() as state:
            require(not state["closed"] and time.monotonic() < state["deadline"], "closed or expired")
            rows = state["rows"]
            require(sum(row["active"] for row in rows.values()) <= ACTIVE_LIMIT, "concurrency cap")
            field = "turn_start" if method == "turn/start" else "thread_start"
            candidates = []
            for key, row in rows.items():
                if not row["active"] or row[field] is not None or row["cwd"] != cwd:
                    continue
                if method == "thread/start" and row["verb"] == "spawn" and row["thread"] is None:
                    candidates.append(key)
                elif method == "thread/resume" and row["verb"] == "resume" and row["thread"] == thread:
                    candidates.append(key)
                elif method == "turn/start" and row["thread"] == thread and row["prompt"] == prompt:
                    candidates.append(key)
            require(len(candidates) == 1, "native start has no unique reservation")
            key = candidates[0]
            rows[key][field] = {"generation": generation, "request": request, "method": method}
            return key

    def reply(self, key, method, generation, request, thread, turn=None):
        """Packet §3: replies bind claims to exact native IDs, never notifications."""
        with self.transaction() as state:
            row = state["rows"][key]
            field = "turn_start" if method == "turn/start" else "thread_start"
            claim = row[field]
            require(claim is not None and claim["generation"] == generation
                    and claim["request"] == request and claim["method"] == method, "unpaired native reply")
            require(row["thread"] in (None, thread), "native thread mismatch")
            require("thread" not in claim, "duplicate native reply")
            row["thread"] = claim["thread"] = thread
            if method == "turn/start":
                require(turn is not None, "native turn missing")
                claim["turn"] = turn

    def receipt(self, key, receipt):
        """C1 §4/packet §7: bind each CLI receipt once, without raw IDs."""
        with self.transaction() as state:
            row = state["rows"][key]
            require(row["receipt"] is None, "duplicate receipt")
            row["receipt"] = receipt

    def settle(self, key, envelope):
        """Packet §7: release concurrency only after both C1 results and native mapping."""
        with self.transaction() as state:
            row = state["rows"][key]
            require(row["active"] and row["receipt"] is not None and row["envelope"] is None,
                    "unknown or duplicate settlement")
            require(row["receipt"]["session"] == envelope["session"]
                    and row["receipt"]["address"] == envelope["address"]
                    and row["thread"] == envelope["thread"], "receipt/envelope identity mismatch")
            row["envelope"], row["active"] = envelope, False

    def snapshot(self):
        """Packet §7: bounded public-field ledger for final one-to-one verification."""
        with self.transaction(write=False) as state:
            return state
