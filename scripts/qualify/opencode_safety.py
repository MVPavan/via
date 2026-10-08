#!/usr/bin/env python3
"""Owned-process qualification controls; vendors/opencode.md §§2–4, 9, 13.

Only explicit acquisition/start methods have effects. Importing this module never
reads caller configuration, starts a daemon, downloads a binary, or calls a model.
The threat model is claude.py's cooperative same-user model, not a sandbox against
hostile same-user mutation. Every uncertain observation blocks its predicate.
"""

import base64
import errno
import contextlib
import contextvars
import hashlib
import gzip
import http.client
import io
import json
import math
import os
import posixpath
import re
from pathlib import Path, PurePosixPath
import shutil
import select
import selectors
import socket
import threading
import signal
import stat
import subprocess
import sys
import tarfile
import time
import tomllib
import urllib.parse
import urllib.request
from dataclasses import dataclass

PINNED_SHA256 = "32cf5aa0a69a650e36277e3315d189835ddc79fb9aa1d0aef5025be5af5ad122"
# §13: public cancellation names only a system interpreter and a project-relative helper.
CANCEL_HELPER_COMMAND = '/usr/bin/python3 .opencode/tool-helper.py'
# §13: helper diagnostics are closed labels; exception messages/paths are never retained.
HELPER_EXCEPTION_CLASSES = frozenset({'PermissionError','FileNotFoundError','ProcessLookupError',
    'OSError','ValueError','TypeError','RuntimeError','KeyError','IndexError','JSONDecodeError',
    'KeyboardInterrupt','SystemExit','AssertionError','OverflowError','MemoryError','BrokenPipeError',
    'ChildProcessError','NotADirectoryError','IsADirectoryError','TimeoutError','ZeroDivisionError'})
HELPER_FAILURE_STEPS = frozenset({'initialization','fork','session','identity','readiness',
                                'protocol','barrier','release','complete'})
NPM_URL = "https://registry.npmjs.org/@opencode/cli-linux-x64/-/cli-linux-x64-2.0.22.tgz"
# Packet §§2, 13 qualification acquisition: reviewed unpacked archive size.
NPM_UNPACKED_SIZE = 204482252
NPM_SHA512 = ("DlV1qgEDDnVqpTWMPqv7tCHCcXodzZBFaMcxjsiYdY6E5gHH2Q68JfasVksyQ1n"
              "u6m1887WQKGhOsepE+oKyYw==")
# Qualification plan acquisition and packet §9 observation bounds.
ARCHIVE_BYTES = 256 * 1024 * 1024
# Packet §13 embedded-runtime search: bounded file, windows, candidates and time.
EMBEDDED_SEARCH_BYTES = ARCHIVE_BYTES
EMBEDDED_SEARCH_WINDOW = 1024 * 1024
EMBEDDED_SEARCH_SEED = 256
EMBEDDED_SEARCH_CANDIDATES = 1024
EMBEDDED_SEARCH_SECONDS = 30
PROC_BYTES = 256 * 1024
HTTP_BYTES = 16 * 1024 * 1024
# Qualification §13 OC01: bound route normalization work and ambiguity.
API_PATH_BYTES = 8 * 1024
API_DECODE_ROUNDS = 8
PASSWORD_KEY = b"OPENCODE_PASSWORD"
FREE_IDENTITY = "opencode/mimo-v2.6-flash-free"
MOCK_IDENTITY = "oclive-mock/fixture-free"
SYSTEM_PATHS = ("/usr/bin", "/bin", "/usr/sbin", "/sbin")
PRIVATE_PARTS = {"HOME": "home", "XDG_CONFIG_HOME": "config", "XDG_DATA_HOME": "data",
                 "XDG_STATE_HOME": "state", "XDG_CACHE_HOME": "cache",
                 "XDG_RUNTIME_DIR": "runtime", "TMPDIR": "tmp"}
VENDOR_ENVIRONMENT = frozenset({"PATH", "LANG", "OPENCODE_CONFIG_CONTENT",
                              "OPENCODE_DISABLE_AUTOUPDATE", "OPENCODE_PASSWORD",
                              "VIA_PROCESS_MARKER", *PRIVATE_PARTS})


class Blocked(Exception):
    """Required proof unavailable or a qualification safety control failed (§13)."""


class ListenerNotReady(Blocked):
    """§13/§2: a verified child has no listener yet; ownership errors still block."""


_BLOCK_CONTEXT = contextvars.ContextVar('opencode_block_context', default={})
BLOCK_REASON_CHARS = 2048  # Packet §13: bounded runner-authored blocking diagnostics.


@contextlib.contextmanager
def block_context(**fields):
    """Retain runner-selected case/phase/verb at the first failure boundary (§13)."""
    current = {**_BLOCK_CONTEXT.get(), **fields}
    token = _BLOCK_CONTEXT.set(current)
    try:
        yield
    except BaseException as error:
        error.block_context = {**current, **getattr(error, 'block_context', {})}
        raise
    finally:
        _BLOCK_CONTEXT.reset(token)


# §13: exception/traceback diagnostics stay bounded and retain no OS text or paths.
OS_ERROR_CHAIN = 16
OS_ERROR_FRAMES = 64


def os_error_record(error):
    """Retain fixed errno and trusted code step from a bounded cause chain (§13)."""
    seen=set()
    for _ in range(OS_ERROR_CHAIN):
        if error is None or id(error) in seen: return None
        seen.add(id(error))
        if isinstance(error,OSError):
            code=error.errno
            name=errno.errorcode.get(code,'UNAVAILABLE') if type(code) is int else 'UNAVAILABLE'
            record={'errno':name,'step':'unavailable','line':0}
            trace=error.__traceback__
            for _ in range(OS_ERROR_FRAMES):
                if trace is None: break
                frame=trace.tb_frame
                module=frame.f_globals.get('__name__')
                if module in {'opencode_safety','opencode_driver'}:
                    step=frame.f_code.co_qualname.replace('.<locals>.','.')
                    if re.fullmatch(r'[A-Za-z_][A-Za-z_0-9.]{0,255}',step):
                        record.update(step=module+'.'+step,line=trace.tb_lineno)
                trace=trace.tb_next
            return record
        error=error.__cause__ if error.__cause__ is not None else error.__context__
    return None


def blocking_record(error, *, stage=None, vault=None, secret_forms=()):
    """Only runner-authored Blocked text may cross the diagnostic sink (§13)."""
    kinds = {'Blocked', 'EvidenceUnavailable', 'QualificationFailure', 'ValueError',
             'KeyError', 'TypeError', 'OSError', 'RuntimeError', 'AssertionError',
             'KeyboardInterrupt', 'SystemExit', 'InterruptedError'}
    kind = type(error).__name__
    reason = str(error) if type(error) is Blocked else 'unexpected runner exception'
    raw = reason.encode()
    if (vault is not None and vault.leaks(raw)) or any(form and form in raw for form in secret_forms):
        reason = 'blocking reason matched protected material'
    reason = ' '.join(reason.split())[:BLOCK_REASON_CHARS] or 'runner block without a message'
    context = getattr(error, 'block_context', _BLOCK_CONTEXT.get())
    record = {key: context.get(key) for key in ('phase', 'case', 'verb')}
    record.update(kind=kind if kind in kinds else 'UnexpectedException', reason=reason)
    os_failure=os_error_record(error)
    if os_failure is not None: record['os_error']=os_failure
    if hasattr(error,'process_liveness') or 'process_role' in context:
        role=getattr(error,'process_role',context.get('process_role','host'))
        liveness=getattr(error,'process_liveness',None)
        record['process']={'role':role if role in PROCESS_ROLES else 'host',
                           'liveness':liveness if type(liveness) is bool else None}
    if stage is not None:
        record['stage'] = stage
    replies=getattr(error,'reply_evidence',None)
    if type(replies) is list:
        record['reply_evidence']=[name for name in replies if type(name) is str
            and re.fullmatch(r'[0-9]+-reply-block\.json',name)]
    if getattr(error,'reply_retention_failed',False) is True:
        record['reply_retention_failed']=True
    if getattr(error,'mock_receipt_failed',False) is True:
        record['mock_receipt_failed']=True
    if getattr(error,'helper_readiness_retention_failed',False) is True:
        record['helper_readiness_retention_failed']=True
    return record


PROCESS_ROLES = frozenset({'daemon','anchor','vendor','host','helper'})


def process_block(role, value, reason):
    """Fixed role and tri-state liveness accompany process-proof failures (§13)."""
    error=Blocked(reason)
    error.process_role=role if role in PROCESS_ROLES else 'host'
    error.process_liveness=value if type(value) is bool else None
    return error


def require_proof(value, reason):
    """Only literal True proves a predicate; missing evidence never passes (§13)."""
    if value is not True:
        raise Blocked(reason)


@dataclass(frozen=True)
class Identity:
    """A process identity, never merely a PID (packet §3.2)."""
    pid: int
    start_ticks: int

    def __post_init__(self):
        if type(self.pid) is not int or self.pid <= 0 or type(self.start_ticks) is not int \
                or self.start_ticks < 0:
            raise Blocked("invalid process identity")

    def report(self):
        return {"pid": self.pid, "start_ticks": self.start_ticks}


class ProcReader:
    """Stable descriptor reads with PID/start-tick checks; synthetic root supported."""

    def __init__(self, root=Path("/proc")):
        self.root = Path(root)

    @staticmethod
    def parse_stat(raw, pid):
        try:
            fields = raw[raw.rindex(b")") + 2:].split()
            if int(raw.split(b" ", 1)[0]) != pid or fields[0] not in {
                    b"R", b"S", b"D", b"Z", b"T", b"t", b"X", b"x", b"K", b"W", b"P", b"I"}:
                raise ValueError("stat identity or state invalid")
            return {"pid": pid, "state": fields[0].decode("ascii"),
                    "ppid": int(fields[1]), "start_ticks": int(fields[19])}
        except (ValueError, IndexError, UnicodeError) as error:
            raise Blocked("unverifiable process stat") from error

    def stat(self, pid):
        try:
            with (self.root / str(pid) / "stat").open("rb") as stream:
                raw = stream.read(PROC_BYTES + 1)
        except FileNotFoundError:
            return None
        except OSError as error:
            raise Blocked("unreadable process stat") from error
        if len(raw) > PROC_BYTES:
            raise Blocked("process stat exceeds bound")
        return self.parse_stat(raw, pid)

    @contextlib.contextmanager
    def opened(self, identity):
        try:
            fd = os.open(self.root / str(identity.pid), os.O_RDONLY | os.O_DIRECTORY
                         | os.O_NOFOLLOW | os.O_CLOEXEC)
        except OSError as error:
            raise Blocked("owned process identity unavailable") from error
        try:
            self._verify_fd(fd, identity)
            yield fd
            self._verify_fd(fd, identity)
        finally:
            os.close(fd)

    def _verify_fd(self, fd, identity):
        try:
            with os.fdopen(os.open("stat", os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd), "rb") as file:
                raw = file.read(PROC_BYTES + 1)
        except OSError as error:
            raise Blocked("owned identity became unverifiable") from error
        if len(raw) > PROC_BYTES or self.parse_stat(raw, identity.pid)["start_ticks"] \
                != identity.start_ticks:
            raise Blocked("owned process identity changed")

    def read(self, identity, name, limit=PROC_BYTES):
        if name not in {"environ", "cmdline", "status", "stat", "net/tcp"} \
                and not (name.startswith("fdinfo/") and name[7:].isdigit()):
            raise Blocked("unapproved proc read")
        with self.opened(identity) as fd:
            try:
                with os.fdopen(os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd), "rb") as file:
                    raw = file.read(limit + 1)
            except OSError as error:
                raise Blocked("owned process observation unreadable") from error
            if len(raw) > limit:
                raise Blocked("owned process observation exceeds bound")
            return raw

    def alive(self, identity):
        try:
            row = self.stat(identity.pid)
        except Blocked:
            return None
        if row is not None and row["start_ticks"] != identity.start_ticks:
            return False
        if self.root != Path("/proc"):
            return row is not None and row["state"] != "Z"
        # A hidden proc entry or a zombie leader with live threads is not exit proof.
        if not hasattr(os, "pidfd_open"):
            return None
        try:
            pidfd = os.pidfd_open(identity.pid)
        except OSError as error:
            return False if error.errno == errno.ESRCH else None
        try:
            poll = select.poll()
            poll.register(pidfd, select.POLLIN)
            try:
                after = self.stat(identity.pid)
            except Blocked:
                after = None
            if after is None:
                # A reaped proc entry is definitive only if this open pidfd
                # proves exit. A hidden entry with a live pidfd stays unknown.
                return False if poll.poll(0) else None
            if after["start_ticks"] != identity.start_ticks:
                return False
            return not bool(poll.poll(0))
        finally:
            os.close(pidfd)

    def verify(self, identity):
        value=self.alive(identity)
        if value is not True:
            raise process_block(_BLOCK_CONTEXT.get().get('process_role','host'),value,
                                "owned process is not verified alive")

    def gone(self, identity):
        return self.alive(identity) is False

    def listener(self, identity):
        """Discover exactly one IPv4 loopback listener belonging to this vendor."""
        with self.opened(identity) as fd:
            try:
                socket_fd = os.open("fd", os.O_RDONLY | os.O_DIRECTORY, dir_fd=fd)
                try:
                    inodes = set()
                    for entry in os.listdir(socket_fd):
                        try:
                            target = os.readlink(entry, dir_fd=socket_fd)
                        except OSError as error:
                            # §13: a closed descriptor is absent; directory and
                            # identity uncertainty still block the observation.
                            if error.errno in {errno.ENOENT, errno.ESRCH}:
                                continue
                            raise
                        if target.startswith("socket:[") and target.endswith("]"):
                            inodes.add(target[8:-1])
                finally:
                    os.close(socket_fd)
            except OSError as error:
                raise Blocked("owned listener descriptor evidence unavailable") from error
        raw = self.read(identity, "net/tcp")
        candidates = set()
        for line in raw.decode("ascii", "strict").splitlines()[1:]:
            fields = line.split()
            if len(fields) < 10:
                raise Blocked("unverifiable socket metadata")
            if fields[3] == "0A" and fields[9] in inodes:
                address, port = fields[1].split(":")
                if address != "0100007F":
                    raise Blocked("owned listener is not IPv4 loopback")
                candidates.add(int(port, 16))
        if not candidates:
            self.verify(identity)
            raise ListenerNotReady("owned listener absent or ambiguous")
        if len(candidates) != 1 or 0 in candidates:
            raise Blocked("owned listener absent or ambiguous")
        self.verify(identity)
        return f"http://127.0.0.1:{candidates.pop()}"


class PasswordVault:
    """Once-per-generation memory-only password; Boolean leak/rotation proofs (§2.3)."""

    def __init__(self):
        self._values = {}
        self._attempted = set()
        self._names = {}

    def __repr__(self):
        return "PasswordVault(<redacted>)"

    def read_once(self, proc, identity):
        if identity in self._attempted:
            if identity in self._values:
                return self._values[identity]
            raise Blocked("password observation already failed for generation")
        self._attempted.add(identity)
        raw = proc.read(identity, "environ")
        if not raw or not raw.endswith(b"\0"):
            raise Blocked("truncated password observation")
        found = []
        names = set()
        offset = 0
        while offset < len(raw):
            end = raw.index(b"\0", offset)
            separator = raw.find(b"=", offset, end)
            if separator < offset:
                raise Blocked("environment entry key unverifiable")
            key = raw[offset:separator]
            try:
                decoded_key = key.decode("ascii", "strict")
            except UnicodeError as error:
                raise Blocked("environment key is not ASCII") from error
            if decoded_key in names:
                raise Blocked("environment key duplicated")
            names.add(decoded_key)
            if key == PASSWORD_KEY:
                # No other value is sliced, decoded, inspected or retained.
                found.append(raw[separator + 1:end])
            offset = end + 1
        del raw
        if len(found) != 1 or not found[0] or b"\n" in found[0] or b"\r" in found[0]:
            raise Blocked("password observation missing or ambiguous")
        self._values[identity] = found[0]
        self._names[identity] = frozenset(names)
        return found[0]

    def names(self, identity):
        """Environment key names from the same once-only read; never values."""
        if identity not in self._names:
            raise Blocked("environment names unavailable")
        return self._names[identity]

    def changed(self, before, after):
        if before not in self._values or after not in self._values:
            raise Blocked("password rotation evidence unavailable")
        changed = self._values[before] != self._values[after]
        require_proof(changed, "password did not rotate")
        return True

    def leaks(self, evidence):
        if isinstance(evidence, str):
            evidence = evidence.encode()
        if not isinstance(evidence, bytes):
            raise Blocked("secret scan input is not bounded bytes")
        candidates = [evidence]
        try:
            parsed = json.loads(evidence)
        except (ValueError, UnicodeError):
            parsed = None
        except RecursionError as error:
            raise Blocked("secret scan JSON depth exceeds bound") from error
        stack, nodes = [(parsed, 0)], 0
        while stack:
            value, depth = stack.pop()
            nodes += 1
            if nodes > 65536 or depth > 64:
                raise Blocked("secret scan JSON structure exceeds packet bound")
            if isinstance(value, str):
                try:
                    candidates.append(value.encode("utf-8"))
                except UnicodeError as error:
                    raise Blocked("secret scan string encoding unverifiable") from error
            elif isinstance(value, dict):
                stack.extend((item, depth + 1) for pair in value.items() for item in pair)
            elif isinstance(value, list):
                stack.extend((item, depth + 1) for item in value)
        for candidate in candidates:
            decoded_url = urllib.parse.unquote_to_bytes(candidate)
            decoded_form = urllib.parse.unquote_to_bytes(candidate.replace(b"+", b" "))
            for password in self._values.values():
                if password.hex().encode() in candidate.lower():
                    return True
                forms = (password, base64.b64encode(password),
                         base64.b64encode(b"opencode:" + password),
                         urllib.parse.quote_from_bytes(password).encode(),
                         password.hex().encode())
                try:
                    forms += (json.dumps(password.decode("utf-8", "strict"))[1:-1].encode(),)
                except UnicodeError:
                    pass
                if any(form and (form in candidate or form in decoded_url or form in decoded_form)
                       for form in forms):
                    return True
        return False

    def safe_write_json(self, path, value):
        raw = (json.dumps(value, sort_keys=True, indent=1, allow_nan=False) + "\n").encode()
        if self.leaks(raw):
            raise Blocked("secret present in evidence; evidence not written")
        path = Path(path)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)

    def redaction_forms(self):
        """§13 mock-only error export: private memory forms, never written or logged."""
        return tuple(self._values.values())

    def clear(self):
        self._values.clear()
        self._names.clear()
        # Python cannot guarantee zeroization of immutable bytes; never persist them.


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_binary(path, expected_sha=PINNED_SHA256, expected_size=None):
    """Check immutable dedicated binary/directory and pin before any start (§2)."""
    path = Path(path)
    try:
        info = path.lstat()
        parent = path.parent.lstat()
    except OSError as error:
        raise Blocked("pinned binary unavailable") from error
    if not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o500 \
            or not stat.S_ISDIR(parent.st_mode) or stat.S_IMODE(parent.st_mode) != 0o500 \
            or not 0 < info.st_size <= ARCHIVE_BYTES \
            or (expected_size is not None and info.st_size != expected_size):
        raise Blocked("pinned binary size or read-only mode mismatch")
    if sha256(path) != expected_sha:
        raise Blocked("pinned binary hash mismatch")
    after = path.stat()
    if (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns) \
            != (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns):
        raise Blocked("pinned binary changed during verification")
    return {"sha256": expected_sha, "size": info.st_size,
            "device": info.st_dev, "inode": info.st_ino}


def verify_daemon_program(config, pinned, expected_sha=PINNED_SHA256, expected_size=None):
    """Explicit daemon harness program must resolve to the pin before every start."""
    if not isinstance(config, dict):
        try:
            config = tomllib.loads(Path(config).read_text())
        except (OSError, ValueError) as error:
            raise Blocked("private daemon configuration unreadable") from error
    try:
        program = config["harnesses"]["opencode"]["binary"]
    except (KeyError, TypeError) as error:
        raise Blocked("explicit pinned OpenCode program is required") from error
    if not isinstance(program, str) or not Path(program).is_absolute() \
            or Path(program).resolve() != Path(pinned).resolve():
        raise Blocked("daemon program does not resolve to pinned binary")
    return verify_binary(pinned, expected_sha, expected_size)


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise Blocked("download redirect refused")


class _OfficialRedirect(urllib.request.HTTPRedirectHandler):
    """Bounded public GitHub asset redirects; default npm/API downloads refuse them."""

    def __init__(self, hosts):
        super().__init__()
        permitted = {"github.com", "release-assets.githubusercontent.com",
                     "objects.githubusercontent.com"}
        if not set(hosts) <= permitted:
            raise Blocked("unapproved release redirect host")
        self.hosts, self.count = frozenset(hosts), 0

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        self.count += 1
        parts = urllib.parse.urlsplit(newurl)
        if self.count > 3 or parts.scheme != "https" or parts.hostname not in self.hosts \
                or parts.username or parts.password or parts.fragment \
                or parts.port not in {None, 443} or req.has_header("Authorization"):
            raise Blocked("official release redirect refused")
        # No header copy: credentials cannot be forwarded even if a caller regresses.
        return urllib.request.Request(newurl, headers={"User-Agent": "VIA-qualification"})


class _AbsoluteHTTPDeadline:
    """Interrupt headers/body at one monotonic deadline; join its only helper thread."""

    def __init__(self, connection, deadline):
        self.connection, self.deadline = connection, deadline
        self.done, self.expired = threading.Event(), threading.Event()
        self.active_socket = None
        self.thread = threading.Thread(target=self._watch, name="via-oc-http-deadline")

    def __enter__(self):
        self.thread.start()
        return self

    def _watch(self):
        if not self.done.wait(max(0, self.deadline - time.monotonic())):
            self.expired.set()
            self._abort()

    def attach(self):
        self.active_socket = self.connection.sock
        if self.expired.is_set():
            self._abort()
            raise Blocked("HTTP absolute deadline exceeded")

    def _abort(self):
        # Shutdown does not take the buffered-reader lock held by read1/getresponse.
        for active in (self.active_socket, self.connection.sock):
            if active is not None:
                try:
                    active.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def guard(self):
        if self.expired.is_set() or time.monotonic() >= self.deadline:
            raise Blocked("HTTP absolute deadline exceeded")

    def __exit__(self, *_args):
        self.done.set()
        self.thread.join(timeout=1)
        if self.thread.is_alive():
            raise Blocked("HTTP deadline helper did not join")


def _bounded_response(connection, method, path, headers, body, *, deadline, limit, observe=None):
    """Shared bounded transport for owned HTTP and unauthenticated official HTTPS."""
    with _AbsoluteHTTPDeadline(connection, deadline) as timer:
        connection.request(method, path, body=body, headers=headers)
        timer.attach()
        response = connection.getresponse()
        data = bytearray()
        complete=False
        try:
            timer.guard()
            while True:
                timer.guard()
                chunk = response.read1(min(65536, limit + 1 - len(data)))
                data.extend(chunk)
                timer.guard()
                if not chunk:
                    complete=True
                    break
                if len(data) > limit:
                    raise Blocked("HTTP response exceeds bound")
            return response.status, response.getheader("Location"), bytes(data)
        finally:
            # §13: even a rejected or partial owned reply reaches the one
            # diagnostic projector; official package fetches have no observer.
            if observe is not None: observe(response.status,bytes(data),complete=complete)


def official_https_get(url, *, allowed_hosts, deadline_s=120, limit=ARCHIVE_BYTES,
                       redirect_hosts=()):
    """Plain HTTPS without urllib proxy/config; joined watchdog bounds headers/body."""
    deadline = time.monotonic() + deadline_s
    redirect = _OfficialRedirect(redirect_hosts) if redirect_hosts else _NoRedirect()
    current = url
    while True:
        parts = urllib.parse.urlsplit(current)
        if parts.scheme != "https" or parts.hostname not in (set(allowed_hosts) | set(redirect_hosts)) \
                or parts.username or parts.password or parts.fragment \
                or parts.port not in {None, 443}:
            raise Blocked("unapproved official package origin")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Blocked("official package download deadline exceeded")
        connection = http.client.HTTPSConnection(parts.hostname, parts.port or 443,
                                                  timeout=remaining)
        try:
            path = urllib.parse.urlunsplit(("", "", parts.path or "/", parts.query, ""))
            status, location, data = _bounded_response(
                connection, "GET", path, {"User-Agent": "VIA-qualification"}, None,
                deadline=deadline, limit=limit)
            if 300 <= status < 400:
                if not location:
                    raise Blocked("official package redirect missing destination")
                target = urllib.parse.urljoin(current, location)
                request = urllib.request.Request(current)
                redirected = redirect.redirect_request(request, None, status, "redirect", {}, target)
                if redirected is None:
                    raise Blocked("official package redirect refused")
                current = redirected.full_url
                continue
            if status != 200:
                raise Blocked("official package GET did not succeed")
            return data
        except (OSError, ValueError, http.client.HTTPException) as error:
            raise Blocked("official package GET unavailable") from error
        finally:
            connection.close()


def _fetch_private_environment(home):
    home = private_directory(home)
    roots = {key: str(private_directory(home / ("worker-" + part)))
             for key, part in PRIVATE_PARTS.items() if key != "HOME"}
    return {"HOME": str(home), **roots, "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}


def _known_interpreter():
    interpreter = Path(sys.executable).resolve()
    if interpreter.parent not in {Path(part).resolve() for part in SYSTEM_PATHS} \
            or not interpreter.is_file() or not os.access(interpreter, os.X_OK):
        raise Blocked("qualification helper interpreter is not a known system executable")
    return interpreter


def supervised_official_get(url, *, private_home, allowed_hosts, deadline_s=120,
                            limit=ARCHIVE_BYTES, redirect_hosts=()):
    """Own/reap one private stdlib child; parent deadline also bounds DNS/connect."""
    if type(deadline_s) not in {int, float} or not math.isfinite(deadline_s) \
            or not 0 < deadline_s <= 120 or type(limit) is not int or not 0 < limit <= ARCHIVE_BYTES:
        raise Blocked("official fetch bounds invalid")
    request = json.dumps({"url": url, "allowed_hosts": sorted(allowed_hosts),
                          "redirect_hosts": sorted(redirect_hosts),
                          "deadline_s": deadline_s, "limit": limit}, allow_nan=False).encode()
    if len(request) > 4096:
        raise Blocked("official fetch request exceeds bound")
    env = _fetch_private_environment(Path(private_home))
    interpreter = _known_interpreter()
    child = None
    identity = None
    proc = ProcReader()
    selector = selectors.DefaultSelector()
    output, stderr_bytes, sent = bytearray(), 0, 0
    deadline = time.monotonic() + deadline_s
    try:
        child = subprocess.Popen([str(interpreter), "-I", "-S", str(Path(__file__).resolve()),
                                  "--fetch-worker"], env=env, cwd=private_home,
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        row = proc.stat(child.pid)
        if row is None:
            raise Blocked("official fetch helper identity unavailable")
        identity = Identity(child.pid, row["start_ticks"])
        for stream, label, event in ((child.stdin, "stdin", selectors.EVENT_WRITE),
                                     (child.stdout, "stdout", selectors.EVENT_READ),
                                     (child.stderr, "stderr", selectors.EVENT_READ)):
            os.set_blocking(stream.fileno(), False)
            selector.register(stream, event, label)
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise Blocked("supervised official fetch absolute deadline exceeded")
            events = selector.select(min(remaining, 0.05))
            for key, _mask in events:
                if key.data == "stdin":
                    try:
                        count = os.write(key.fd, request[sent:])
                    except BrokenPipeError:
                        count = 0
                    sent += count
                    if sent == len(request) or count == 0:
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
                else:
                    raw = os.read(key.fd, 65536)
                    if not raw:
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
                    elif key.data == "stdout":
                        output.extend(raw)
                        if len(output) > limit:
                            raise Blocked("official fetch output exceeds bound")
                    else:
                        stderr_bytes += len(raw)
                        if stderr_bytes > 4096:
                            raise Blocked("official fetch diagnostics exceed bound")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Blocked("supervised official fetch absolute deadline exceeded")
        child.wait(timeout=remaining)
        if child.returncode != 0 or stderr_bytes:
            raise Blocked("official fetch helper blocked; diagnostics discarded")
        return bytes(output)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise Blocked("supervised official fetch unavailable") from error
    finally:
        selector.close()
        if child is not None:
            if child.poll() is None:
                # The direct Popen handle and pidfd bind this helper, never an owner process.
                try:
                    pidfd = os.pidfd_open(child.pid)
                    try:
                        if identity is not None:
                            proc.verify(identity)
                        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                    finally:
                        os.close(pidfd)
                except ProcessLookupError:
                    pass
                child.wait(timeout=2)
            for stream in (child.stdin, child.stdout, child.stderr):
                if stream is not None:
                    stream.close()
            # Reap plus targeted pgrep prove the helper PID is absent; no by-name signals.
            checked = subprocess.run(["/usr/bin/pgrep", "-P", str(os.getpid())], env=env,
                                     cwd=private_home, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     timeout=2, check=False)
            if checked.returncode not in {0, 1} or checked.stderr \
                    or str(child.pid).encode() in checked.stdout.split() \
                    or (identity is not None and not proc.gone(identity)):
                raise Blocked("official fetch helper stop proof unavailable")


def _fetch_worker():
    """Private child protocol: bounded JSON in, package bytes out, fixed failure only."""
    try:
        raw = sys.stdin.buffer.read(4097)
        if len(raw) > 4096:
            raise Blocked("request bound")
        request = json.loads(raw)
        if set(request) != {"url", "allowed_hosts", "redirect_hosts", "deadline_s", "limit"}:
            raise Blocked("request schema")
        data = official_https_get(**request)
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()
        return 0
    except Exception:
        # Never emit arbitrary exception strings, request URLs or environment values.
        sys.stderr.buffer.write(b"blocked\n")
        return 2


def extract_pinned_npm(archive, directory, *, integrity=NPM_SHA512,
                       unpacked_size=NPM_UNPACKED_SIZE, expected_sha=PINNED_SHA256,
                       binary_name="package/bin/opencode"):
    """Verify SHA-512 before bounded regular-entry extraction, then packet SHA-256."""
    if not isinstance(integrity, str) or integrity.removeprefix("sha512-") != NPM_SHA512 \
            or type(unpacked_size) is not int or unpacked_size != NPM_UNPACKED_SIZE:
        raise Blocked("npm metadata does not match reviewed pin")
    if len(archive) > ARCHIVE_BYTES or base64.b64encode(hashlib.sha512(archive).digest()).decode() \
            != NPM_SHA512:
        raise Blocked("npm archive integrity mismatch")
    seen, total, binary = set(), 0, None
    try:
        with gzip.GzipFile(fileobj=io.BytesIO(archive), mode="rb") as compressed:
            decoded = compressed.read(ARCHIVE_BYTES + 1)
        if len(decoded) > ARCHIVE_BYTES:
            raise Blocked("npm aggregate decompression exceeds bound")
        with tarfile.open(fileobj=io.BytesIO(decoded), mode="r:") as tar:
            for entry in tar:
                path = PurePosixPath(entry.name)
                if entry.name in seen or path.is_absolute() or ".." in path.parts \
                        or "\\" in entry.name or not entry.isfile() \
                        or entry.size < 0 or entry.size > ARCHIVE_BYTES:
                    raise Blocked("unsafe npm archive member")
                seen.add(entry.name)
                total += entry.size
                if total > ARCHIVE_BYTES or len(seen) > 32:
                    raise Blocked("npm decompression bound exceeded")
                stream = tar.extractfile(entry)
                if stream is None:
                    raise Blocked("npm archive member unavailable")
                content = stream.read(entry.size + 1)
                if len(content) != entry.size:
                    raise Blocked("npm archive declared size mismatch")
                if entry.name == binary_name:
                    binary = content
        if total != unpacked_size or binary is None or not binary \
                or hashlib.sha256(binary).hexdigest() != expected_sha:
            raise Blocked("extracted pinned binary size or hash mismatch")
    except (tarfile.TarError, OSError, EOFError) as error:
        raise Blocked("npm archive unverifiable") from error
    directory = Path(directory)
    directory.mkdir(mode=0o700)
    binary_path = directory / "opencode"
    fd = os.open(binary_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o500)
    with os.fdopen(fd, "wb") as file:
        file.write(binary)
    binary_path.chmod(0o500)
    directory.chmod(0o500)
    verify_binary(binary_path, expected_sha, len(binary))
    return binary_path


def extract_pinned_release(archive, directory, *, expected_sha=PINNED_SHA256, archive_sha512=None):
    """Only a separately reviewed pre-parse archive hash admits release extraction."""
    if not archive_sha512:
        raise Blocked("official release pre-parse hash required")
    if not isinstance(archive_sha512, str) or base64.b64encode(
            hashlib.sha512(archive).digest()).decode() != archive_sha512.removeprefix("sha512-"):
        raise Blocked("official release archive integrity mismatch")
    if len(archive) > ARCHIVE_BYTES:
        raise Blocked("official release download exceeds bound")
    seen, total, binary = set(), 0, None
    try:
        with gzip.GzipFile(fileobj=io.BytesIO(archive), mode="rb") as compressed:
            decoded = compressed.read(ARCHIVE_BYTES + 1)
        if len(decoded) > ARCHIVE_BYTES:
            raise Blocked("official release decompression exceeds bound")
        with tarfile.open(fileobj=io.BytesIO(decoded), mode="r:") as tar:
            for entry in tar:
                path = PurePosixPath(entry.name)
                if entry.name in seen or path.is_absolute() or ".." in path.parts \
                        or "\\" in entry.name or not (entry.isfile() or entry.isdir()) \
                        or entry.size < 0 or entry.size > ARCHIVE_BYTES:
                    raise Blocked("unsafe official release archive member")
                seen.add(entry.name)
                total += entry.size
                if total > ARCHIVE_BYTES or len(seen) > 32:
                    raise Blocked("official release extraction bound exceeded")
                if entry.isdir():
                    continue
                stream = tar.extractfile(entry)
                if stream is None:
                    raise Blocked("official release archive member unavailable")
                content = stream.read(entry.size + 1)
                if len(content) != entry.size:
                    raise Blocked("official release declared size mismatch")
                if path.name == "opencode":
                    if binary is not None:
                        raise Blocked("official release binary ambiguous")
                    binary = content
        if not binary or hashlib.sha256(binary).hexdigest() != expected_sha:
            raise Blocked("official release pinned binary hash mismatch")
    except (tarfile.TarError, OSError, EOFError) as error:
        raise Blocked("official release archive unverifiable") from error
    directory = Path(directory)
    directory.mkdir(mode=0o700)
    binary_path = directory / "opencode"
    fd = os.open(binary_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o500)
    with os.fdopen(fd, "wb") as file:
        file.write(binary)
    binary_path.chmod(0o500)
    directory.chmod(0o500)
    verify_binary(binary_path, expected_sha, len(binary))
    return binary_path


def namespace_name():
    digest = hashlib.sha256()
    for field in (b"via-opencode-namespace-v1", b"opencode-free-anonymous-v1",
                  (1).to_bytes(8, "little")):
        digest.update(len(field).to_bytes(8, "little"))
        digest.update(field)
    return digest.hexdigest()[:16]


def private_directory(path):
    """Create one 0700 directory; reject existing symlink or unsafe mode (N1)."""
    path = Path(path)
    try:
        path.mkdir(mode=0o700)
    except FileExistsError:
        pass
    row = path.lstat()
    if not stat.S_ISDIR(row.st_mode) or stat.S_IMODE(row.st_mode) != 0o700 \
            or row.st_uid != os.getuid():
        raise Blocked("private directory chain is unsafe")
    return path


def create_namespace(state):
    """Exact §3.2 namespace and seven roots, including vendor/opencode 0700 chain."""
    state = private_directory(state)
    vendor = private_directory(state / "vendor")
    route = private_directory(vendor / "opencode")
    root = private_directory(route / namespace_name())
    return root, {key: str(private_directory(root / part))
                  for key, part in PRIVATE_PARTS.items()}


def init_fixture_repo(path, home, *, sentinel=False):
    """A new private Git fixture with an initial private-identity commit (N2)."""
    path, home = Path(path), Path(home)
    if (path / ".git").exists():
        raise Blocked("fixture Git repository already exists")
    private_directory(path)
    private_directory(home)
    template = private_directory(home / "git-empty-template")
    env = {"HOME": str(home), "XDG_CONFIG_HOME": str(home), "PATH": "/usr/bin:/bin",
           "LANG": "C.UTF-8", "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
           "GIT_AUTHOR_NAME": "VIA fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
           "GIT_COMMITTER_NAME": "VIA fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid"}
    (path / "AGENTS.md").write_text("VIA_LOCAL_AGENT_SENTINEL\n")
    if sentinel:
        (path / "AGENTS.md").write_text("VIA_ANCESTOR_AGENTS_SENTINEL\n")
        skill = path / ".claude" / "skills" / "via-sentinel"
        skill.mkdir(parents=True, mode=0o700)
        (skill / "SKILL.md").write_text("---\nname: via-sentinel\n"
                                        "description: VIA_ANCESTOR_SKILL_SENTINEL\n---\n"
                                        "VIA_ANCESTOR_SKILL_SENTINEL\n")
        for directory, marker in ((".claude", "VIA_ANCESTOR_CLAUDE_AGENT_SENTINEL"),
                                  (".opencode", "VIA_ANCESTOR_OPENCODE_AGENT_SENTINEL")):
            agents = path / directory / "agents"
            agents.mkdir(parents=True, mode=0o700)
            (agents / "via-sentinel.md").write_text(
                "---\ndescription: local walk-up sentinel\nmode: subagent\n---\n" + marker + "\n")
    for args in (("init", "--template=" + str(template), "."),
                 ("add", "--", "AGENTS.md", *([".claude", ".opencode"] if sentinel else [])),
                 ("-c", "commit.gpgsign=false", "commit", "-m", "fixture root")):
        try:
            result = subprocess.run(["/usr/bin/git", *args], cwd=path, env=env,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    timeout=30, check=False)
        except (OSError, subprocess.TimeoutExpired) as error:
            raise Blocked("private fixture Git initialization unavailable") from error
        if result.returncode:
            raise Blocked("private fixture Git initialization failed")
    return path


def validate_path(value, helper_dir, *, system_dirs=SYSTEM_PATHS):
    """Refuse empty/relative/user-bin PATH entries; exact private helper is allowed."""
    helper = Path(helper_dir).resolve()
    allowed = {Path(part).resolve() for part in system_dirs}
    entries = value.split(os.pathsep)
    if not entries or any(not part or not Path(part).is_absolute() for part in entries):
        raise Blocked("unsafe PATH entry")
    for part in entries:
        resolved = Path(part).resolve()
        if resolved != helper and resolved not in allowed:
            raise Blocked("unsafe PATH outside system/helper directories")
    if helper not in {Path(part).resolve() for part in entries}:
        raise Blocked("private helper directory absent from PATH")
    return True


def build_minimal_path(helper_dir, rg=None, *, system_dirs=SYSTEM_PATHS):
    """Link exactly a checked system rg into the helper directory (N8)."""
    helper = private_directory(helper_dir)
    rg = Path(rg or shutil.which("rg", path=os.pathsep.join(system_dirs)) or "")
    allowed = {Path(part).resolve() for part in system_dirs}
    if not rg.is_absolute() or rg.resolve().parent not in allowed \
            or not rg.is_file() or not os.access(rg, os.X_OK):
        raise Blocked("known rg is not a system executable")
    target = helper / "rg"
    if target.exists() or target.is_symlink():
        if not target.is_symlink() or target.resolve() != rg.resolve():
            raise Blocked("fixture rg link does not match checked system executable")
    else:
        target.symlink_to(rg.resolve())
    value = os.pathsep.join((str(helper), *system_dirs))
    validate_path(value, helper, system_dirs=system_dirs)
    return value, {"rg_sha256": sha256(rg), "rg_system_directory": True}


def credential_metadata_only(name):
    """Credential/config databases are never opened by inventory (qualification privacy)."""
    lowered = name.lower().lstrip(".")
    return (lowered.startswith("auth") or "credential" in lowered
            or lowered.startswith(("npmrc", "netrc", "bunfig.toml"))
            or lowered.startswith("opencode.db")
            or any(extension in lowered for extension in (".sqlite", ".db-wal", ".db-shm")))


class EmbeddedRuntime:
    """§13: Bun extraction bytes must occur in this run's verified pinned executable."""

    def __init__(self, pinned, evidence, record, *, deadline=None):
        self.pinned=Path(pinned)
        self.evidence=Path(os.path.abspath(evidence))
        self.record=record
        self.tmpdirs={}
        self.deadline=deadline
        self._pin_identity=None
        self._offsets={}
        self._recorded=set()

    def register_tmpdir(self, path, role):
        """Register the exact private serve/probe TMPDIR in the owned launch recipe (§13)."""
        path=Path(os.path.abspath(path))
        if role not in {'probe','serve'} or not path.is_relative_to(self.evidence):
            raise Blocked('embedded runtime TMPDIR provenance invalid')
        row=path.lstat()
        if not stat.S_ISDIR(row.st_mode) or row.st_uid!=os.getuid() \
                or stat.S_IMODE(row.st_mode)!=0o700:
            raise Blocked('embedded runtime TMPDIR provenance invalid')
        self.tmpdirs[path]=role

    @staticmethod
    def _identity(row):
        return (row.st_dev,row.st_ino,row.st_size,row.st_mode,row.st_uid,
                row.st_nlink,row.st_mtime_ns,row.st_ctime_ns)

    def _verify_pin(self):
        if self._pin_identity is None:
            before=self.pinned.lstat()
            verify_binary(self.pinned,expected_sha=PINNED_SHA256)
            if self._identity(self.pinned.lstat())!=self._identity(before):
                raise Blocked('pinned binary changed during verification')
            if before.st_size>EMBEDDED_SEARCH_BYTES:
                raise Blocked('embedded runtime search byte bound')
            self._pin_identity=self._identity(before)
        if self._identity(self.pinned.lstat())!=self._pin_identity:
            raise Blocked('pinned binary changed during verification')

    def _find_bytes(self,raw,digest):
        """§13: search bounded windows; cached offsets stay in this instance/run only."""
        end=time.monotonic()+EMBEDDED_SEARCH_SECONDS
        if self.deadline is not None: end=min(end,self.deadline())
        def guard():
            if time.monotonic()>=end: raise Blocked('embedded runtime search deadline')
        guard(); self._verify_pin(); guard()
        fd=os.open(self.pinned,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
        key=(len(raw),digest)
        try:
            if self._identity(os.fstat(fd))!=self._pin_identity:
                raise Blocked('pinned binary changed during verification')
            size=os.fstat(fd).st_size
            offset=self._offsets.get(key)
            if offset is not None:
                matches=os.pread(fd,len(raw),offset)==raw
                guard()
                if not matches: raise Blocked('pinned binary changed during verification')
                return offset
            seed=raw[:EMBEDDED_SEARCH_SEED]
            position=0; candidates=0
            while position<size:
                guard()
                window=os.pread(fd,min(EMBEDDED_SEARCH_WINDOW+len(seed)-1,size-position),position)
                if not window: raise Blocked('embedded runtime search file truncated')
                start=0
                while True:
                    guard()
                    local=window.find(seed,start)
                    if local<0 or local>=EMBEDDED_SEARCH_WINDOW: break
                    candidates+=1
                    if candidates>EMBEDDED_SEARCH_CANDIDATES:
                        raise Blocked('embedded runtime search candidate bound')
                    offset=position+local
                    if offset+len(raw)<=size and os.pread(fd,len(raw),offset)==raw:
                        guard(); self._offsets[key]=offset
                        return offset
                    start=local+1
                position+=EMBEDDED_SEARCH_WINDOW
            guard()
            return None
        finally:
            os.close(fd)
            self._verify_pin()

    def accept(self, path, listed):
        """Match stable uid-owned, single-link bytes; emit provenance, never a gate (§13)."""
        path=Path(os.path.abspath(path))
        root=path.parent
        name=re.fullmatch(r'\.bun-'+str(os.getuid())+r'-[0-9a-fA-F]{16}\.(?:so|node)',path.name)
        if root not in self.tmpdirs or name is None \
                or not stat.S_ISREG(listed.st_mode) or listed.st_uid!=os.getuid() \
                or listed.st_nlink!=1 or not 0<listed.st_size<=EMBEDDED_SEARCH_BYTES:
            return False
        # Reject a changed/symlinked directory chain before opening the candidate.
        parent=path.parent
        while True:
            row=parent.lstat()
            if not stat.S_ISDIR(row.st_mode) or row.st_uid!=os.getuid():
                return False
            if parent==root and stat.S_IMODE(row.st_mode)!=0o700: return False
            if parent==self.evidence: break
            parent=parent.parent
        fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
        with os.fdopen(fd,'rb') as file:
            before=os.fstat(file.fileno())
            if self._identity(before)!=self._identity(listed):
                raise Blocked('embedded runtime file changed during verification')
            raw=file.read(listed.st_size+1)
            after=os.fstat(file.fileno())
        if self._identity(after)!=self._identity(before) \
                or self._identity(path.lstat())!=self._identity(before):
            raise Blocked('embedded runtime file changed during verification')
        if len(raw)!=listed.st_size: return False
        digest=hashlib.sha256(raw).hexdigest()
        offset=self._find_bytes(raw,digest)
        if offset is None: return False
        key=(path,self._identity(before))
        if key not in self._recorded:
            self.record({'label':'embedded runtime extraction',
                'path':path.relative_to(self.evidence).as_posix(),'path_class':'vendor-private',
                'name_class':'Bun extraction','tmpdir_role':self.tmpdirs[root],
                'size':len(raw),'sha256':digest,'offset':offset,'binary_sha256':PINNED_SHA256})
            self._recorded.add(key)
        return True


# §13 qualification inventory: bound the system template manifest and probe.
GIT_TEMPLATE_BYTES=1024*1024
GIT_TEMPLATE_FILES=128
GIT_TEMPLATE_SECONDS=10


def system_git_templates(root):
    """Resolve verified system Git defaults and prove their copied bytes (§13)."""
    created=False
    try:
        git=shutil.which('git',path=os.pathsep.join(SYSTEM_PATHS))
        if git is None: raise ValueError('missing system git')
        git=Path(git).resolve()
        if git.parent not in {Path(part).resolve() for part in SYSTEM_PATHS}:
            raise ValueError('non-system git')
        git_hash=sha256(git)
        root=Path(root)
        root.mkdir(mode=0o700,exist_ok=False); created=True
        env={key:str(private_directory(root/part)) for key,part in PRIVATE_PARTS.items()}
        env.update(PATH='/usr/bin:/bin',LANG='C.UTF-8',GIT_CONFIG_NOSYSTEM='1',
                   GIT_CONFIG_GLOBAL='/dev/null')
        def command(args):
            result=subprocess.run([str(git),*args],env=env,cwd=root,
                stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,
                timeout=GIT_TEMPLATE_SECONDS,check=True)
            if len(result.stdout)>GIT_TEMPLATE_BYTES: raise ValueError('git output bound')
            return result.stdout
        # Newer Git exposes its default directly. Older relocatable builds carry
        # share/git-core/templates, relative to the prefix of their builtin exec path.
        try: template=Path(command(['var','GIT_TEMPLATE_DIR']).decode().strip())
        except subprocess.CalledProcessError:
            exec_path=Path(command(['--exec-path']).decode().strip())
            if not exec_path.is_absolute() or exec_path.name!='git-core' \
                    or b'\x00share/git-core/templates\x00' not in git.read_bytes():
                raise ValueError('unknown compiled template path')
            template=exec_path.parent.parent/'share/git-core/templates'
        if not template.is_absolute(): raise ValueError('relative template path')
        template=template.resolve(strict=True)
        if template.is_relative_to(root): raise ValueError('non-system templates')
        hooks=template/'hooks'
        rows=sorted(hooks.iterdir())
        if not rows or len(rows)>GIT_TEMPLATE_FILES: raise ValueError('template count bound')
        templates={}; manifest={}
        for path in rows:
            row=path.lstat()
            if not path.name.endswith('.sample') or not stat.S_ISREG(row.st_mode) \
                    or row.st_size>GIT_TEMPLATE_BYTES: raise ValueError('unsafe system template')
            raw=path.read_bytes()
            if EmbeddedRuntime._identity(path.lstat())!=EmbeddedRuntime._identity(row) \
                    or len(raw)!=row.st_size: raise ValueError('template changed')
            templates[path.name]=raw
            manifest[path.name]={'size':len(raw),'sha256':hashlib.sha256(raw).hexdigest()}
        probe=root/'default-repository'
        command(['init','--bare',str(probe)])
        copied=probe/'hooks'
        if {p.name for p in copied.iterdir()}!=set(templates) \
                or any((copied/name).read_bytes()!=raw for name,raw in templates.items()) \
                or sha256(git)!=git_hash:
            raise ValueError('default template copy differs')
        return templates,{'git':str(git),'git_sha256':git_hash,'template_dir':str(template),
                          'files':manifest,'default_copy_verified':True}
    except (OSError,ValueError,subprocess.SubprocessError) as error:
        raise Blocked('system Git template directory unresolvable') from error
    finally:
        # All synchronous Git children have been waited/reaped before deletion.
        if created and root.exists():
            shutil.rmtree(root)


class GitTemplateCopies:
    """§13: inert default hook copies only in owned OpenCode snapshot Git dirs."""

    def __init__(self,evidence,templates,record):
        self.evidence=Path(os.path.abspath(evidence))
        self.templates=dict(templates); self.record=record
        self.snapshots=set(); self.recorded=set()

    def register_snapshot(self,path):
        path=Path(os.path.abspath(path))
        if not path.is_relative_to(self.evidence):
            raise Blocked('snapshot template provenance invalid')
        self.snapshots.add(path)

    def accept(self,path,listed):
        path=Path(os.path.abspath(path)); git=path.parent.parent
        roots=sorted(self.snapshots,key=str)
        root=next((root for root in roots if git.is_relative_to(root)),None)
        expected=self.templates.get(path.name)
        if root is None or path.parent.name!='hooks' or not path.name.endswith('.sample') \
                or expected is None or not stat.S_ISREG(listed.st_mode) \
                or listed.st_uid!=os.getuid() or listed.st_nlink!=1 \
                or listed.st_size!=len(expected): return False
        parent=path.parent
        while True:
            row=parent.lstat()
            if not stat.S_ISDIR(row.st_mode) or row.st_uid!=os.getuid(): return False
            if parent==self.evidence: break
            parent=parent.parent
        for name,is_directory in (('HEAD',False),('config',False),('objects',True),('refs',True)):
            try: row=(git/name).lstat()
            except FileNotFoundError: return False
            if row.st_uid!=os.getuid() or (stat.S_ISDIR(row.st_mode) if is_directory
                                         else stat.S_ISREG(row.st_mode)) is not True: return False
        fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
        with os.fdopen(fd,'rb') as file:
            before=os.fstat(file.fileno()); raw=file.read(GIT_TEMPLATE_BYTES+1)
            after=os.fstat(file.fileno())
        identity=EmbeddedRuntime._identity
        if identity(before)!=identity(listed) or identity(after)!=identity(before) \
                or identity(path.lstat())!=identity(before):
            raise Blocked('git template copy changed during verification')
        if raw!=expected: return False
        key=(path,identity(before))
        if key not in self.recorded:
            self.record({'label':'inert git template copy','path_class':'vendor-private',
                'path':path.relative_to(self.evidence).as_posix(),'name':path.name,
                'size':len(raw),'sha256':hashlib.sha256(raw).hexdigest()})
            self.recorded.add(key)
        return True


class Inventory:
    """Detect package artifacts/new binaries in HOME, XDG, fixtures and namespace."""

    def __init__(self, roots, *, max_entries=100000, embedded_runtime=None, git_templates=None):
        self.roots = tuple(Path(root) for root in roots)
        self.max_entries = max_entries
        self.embedded_runtime=embedded_runtime
        self.git_templates=git_templates
        self.before = self._snapshot()

    def _snapshot(self):
        entries, binaries = 0, {}
        for index, root in enumerate(self.roots):
            if not root.is_dir() or root.is_symlink():
                raise Blocked("inventory root unavailable or unsafe")
            def unreadable(error):
                # A gone child cannot hold a current artifact. Missing baseline
                # binaries still differ in check(); the root must remain readable.
                if error.errno in {errno.ENOENT,errno.ESRCH} and error.filename is not None \
                        and Path(error.filename)!=root and Path(error.filename).is_relative_to(root):
                    return
                raise Blocked("private inventory contains unreadable directory") from error
            for folder, dirs, files in os.walk(root, followlinks=False, onerror=unreadable):
                for name in (*dirs, *files):
                    entries += 1
                    if entries > self.max_entries:
                        raise Blocked("private inventory exceeds bound")
                    if name in {"node_modules", "package.json"}:
                        raise Blocked("package artifact appeared in private roots")
                    path = Path(folder) / name
                    try:
                        row = path.lstat()
                        if stat.S_ISLNK(row.st_mode):
                            raise Blocked("inventory contains unreviewed symbolic link")
                        if stat.S_ISREG(row.st_mode):
                            protected = credential_metadata_only(name)
                            if protected:
                                if row.st_mode & 0o111:
                                    binaries[(index, str(path.relative_to(root)))] = (
                                        "metadata-only", row.st_dev, row.st_ino, row.st_size,
                                        row.st_mode, row.st_mtime_ns, row.st_ctime_ns)
                                continue
                            with path.open("rb") as file:
                                prefix = file.read(4)
                            if row.st_mode & 0o111 or prefix == b"\x7fELF" \
                                    or prefix[:2] == b"MZ" or prefix in {b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"}:
                                if self.embedded_runtime is not None and self.embedded_runtime.accept(path,row):
                                    continue
                                if self.git_templates is not None and self.git_templates.accept(path,row):
                                    continue
                                if row.st_size > ARCHIVE_BYTES:
                                    raise Blocked("private binary inventory size exceeds bound")
                                binaries[(index, str(path.relative_to(root)))] = (
                                    row.st_dev, row.st_ino, row.st_size, sha256(path))
                    except OSError as error:
                        if error.errno in {errno.ENOENT,errno.ESRCH} and error.filename is not None \
                                and Path(os.fsdecode(error.filename))==path:
                            # Snapshot absence is safe; baseline loss still fails check().
                            continue
                        raise Blocked("private inventory entry unreadable") from error
        return binaries

    def check(self):
        if self._snapshot() != self.before:
            raise Blocked("new or changed binary appeared in private roots")
        return True


class SpendingGuard:
    """Structural controls before every model-capable request; missing cost is null."""

    def __init__(self, *, allowed_environment=VENDOR_ENVIRONMENT,
                 owned_mock_origins=(), public_free=False, mock_only=False):
        self.allowed_environment = frozenset(allowed_environment)
        self.mock_origins = frozenset(owned_mock_origins)
        for origin in self.mock_origins:
            loopback_origin(origin)
        self.public_free = public_free
        self.mock_only = mock_only
        self.stopped = False

    def check(self, *, integration, environment_names, catalog, provider, model,
              project_providers, cost=None, mock_received=None):
        if self.stopped:
            raise Blocked("spending control previously failed")
        try:
            if not isinstance(integration, dict) or not isinstance(integration.get("data"), list) \
                    or not set(integration) <= {"data", "location"}:
                raise Blocked("integration credential-free shape unverifiable")
            for item in integration["data"]:
                if not isinstance(item, dict) or not isinstance(item.get("id"), str) \
                        or not set(item) <= {"id", "name", "methods", "connections"} \
                        or not isinstance(item.get("connections"), list) \
                        or item["connections"] != []:
                    raise Blocked("integration is not proven credential-free")
            if frozenset(environment_names) != self.allowed_environment:
                raise Blocked("environment allow-list failed")
            identity = f"{provider}/{model}"
            allowed={MOCK_IDENTITY} if self.mock_only else {FREE_IDENTITY, MOCK_IDENTITY}
            if identity not in allowed:
                raise Blocked("paid or unapproved model identity")
            if not isinstance(catalog, dict) or not isinstance(catalog.get(identity), dict) \
                    or catalog[identity].get("free") is not True:
                raise Blocked("frozen free model absent from catalog")
            if not isinstance(project_providers, dict):
                raise Blocked("effective project providers unverifiable")
            for endpoint in project_providers.values():
                if loopback_origin(endpoint) not in self.mock_origins:
                    raise Blocked("project provider does not point to owned loopback mock")
            overridden = provider in project_providers
            if identity == MOCK_IDENTITY and not overridden:
                raise Blocked("fixture provider endpoint absent")
            if not overridden and not (self.public_free and identity == FREE_IDENTITY):
                raise Blocked("public model mode not explicitly admitted")
            if overridden and mock_received is False:
                raise Blocked("provider override did not reach mock")
            if cost is not None:
                if type(cost) not in {int, float} or not math.isfinite(cost) or cost < 0:
                    raise Blocked("cost evidence invalid")
                if cost > 0:
                    raise Blocked("positive cost hard stop")
        except (Blocked, TypeError, ValueError) as error:
            self.stopped = True
            if isinstance(error, Blocked):
                raise
            raise Blocked("structural spending evidence unverifiable") from error
        return {"structural_spending_controls": True,
                "cost": cost, "cost_available": cost is not None}


def loopback_origin(value):
    parts = urllib.parse.urlsplit(value)
    try:
        port = parts.port
    except ValueError as error:
        raise Blocked("invalid owned listener port") from error
    if parts.scheme != "http" or parts.hostname != "127.0.0.1" \
            or not port or parts.username or parts.password:
        raise Blocked("endpoint is not unauthenticated IPv4 loopback")
    return f"http://127.0.0.1:{port}"


def normalized_api_path(path):
    """Repeated decoding guards credential routes; reject ambiguous routes (§13 OC01)."""
    if not isinstance(path, str):
        raise Blocked("ambiguous API path")
    try:
        if len(path.encode("ascii")) > API_PATH_BYTES:
            raise Blocked("ambiguous API path")
    except UnicodeError as error:
        raise Blocked("ambiguous API path") from error
    if not path.startswith("/") or path.startswith("//") or "\r" in path or "\n" in path:
        raise Blocked("API path changes owned origin")
    if any(ord(char) < 32 or ord(char) == 127 for char in path) or "#" in path:
        raise Blocked("ambiguous API path")
    parts = urllib.parse.urlsplit(path)
    if parts.netloc or parts.scheme:
        raise Blocked("API path changes owned origin")
    if parts.fragment:
        raise Blocked("ambiguous API path")
    decoded = parts.path
    for _iteration in range(API_DECODE_ROUNDS):
        if re.search(r"%(?![0-9A-Fa-f]{2})", decoded):
            raise Blocked("ambiguous API path")
        try:
            following = urllib.parse.unquote(decoded, encoding="utf-8", errors="strict")
        except UnicodeError as error:
            raise Blocked("ambiguous API path") from error
        if following == decoded:
            break
        decoded = following
    else:
        raise Blocked("ambiguous API path")
    if any(ord(char) < 32 or ord(char) == 127 for char in decoded) \
            or any(char in decoded for char in ("\\", ";", "?", "#")):
        raise Blocked("ambiguous API path")
    canonical = "/" + posixpath.normpath(decoded).lstrip("/")
    return canonical, parts, decoded != parts.path or canonical != parts.path


class OwnedHTTP:
    """Authenticated requests to one verified generation; no proxy or redirects."""

    def __init__(self, origin, identity, proc, password, *, limit=HTTP_BYTES, seeding_mode=False,
                 deadline=None):
        self.origin = loopback_origin(origin)
        if origin.rstrip("/") != self.origin:
            raise Blocked("owned API origin includes a path")
        self.identity, self.proc, self.password, self.limit = identity, proc, password, limit
        self.seeding_mode = seeding_mode
        self.deadline = deadline

    def request(self, method, path, body=None, *, authenticated=True, timeout=30, observe=None):
        canonical, parsed, ambiguous = normalized_api_path(path)
        credential_path = canonical.rstrip("/")
        if credential_path == "/api/credential" or credential_path.startswith("/api/credential/"):
            if not (self.seeding_mode is True and method == "POST"
                    and path == "/api/credential" and not parsed.query):
                raise Blocked("credential endpoint forbidden")
        if ambiguous:
            raise Blocked("ambiguous API path")
        with block_context(process_role='vendor'):
            self.proc.verify(self.identity)
        parts = urllib.parse.urlsplit(self.origin)
        headers = {"Content-Type": "application/json"}
        if authenticated:
            headers["Authorization"] = "Basic " + base64.b64encode(
                b"opencode:" + self.password).decode("ascii")
        raw = None if body is None else json.dumps(body, allow_nan=False).encode()
        if raw is not None and len(raw) > self.limit:
            raise Blocked("owned API request exceeds bound")
        deadline = time.monotonic() + timeout
        if self.deadline is not None:
            deadline = min(deadline, self.deadline)
            timeout = deadline - time.monotonic()
            if timeout <= 0:
                raise Blocked("owned API absolute deadline exhausted")
        connection = http.client.HTTPConnection(parts.hostname, parts.port, timeout=timeout)
        try:
            status, _location, data = _bounded_response(connection, method, path, headers, raw,
                                                        deadline=deadline, limit=self.limit, observe=observe)
            if 300 <= status < 400:
                raise Blocked("owned API redirect refused")
            with block_context(process_role='vendor'):
                self.proc.verify(self.identity)
            return status, data
        except (OSError, http.client.HTTPException) as error:
            raise Blocked("owned API observation unavailable") from error
        finally:
            connection.close()


class DeferredSignals:
    """Record interruption only; cleanup may resume stopped owned daemons (§13 L14)."""

    def __init__(self):
        self.interrupted = None
        self._previous = {}

    def record(self, signum, _frame=None):
        if self.interrupted is None:
            self.interrupted = signal.Signals(signum).name

    def guard(self):
        if self.interrupted is not None:
            raise Blocked("qualification interrupted")

    def __enter__(self):
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
            self._previous[signum] = signal.signal(signum, self.record)
        return self

    def __exit__(self, *_args):
        for signum, handler in self._previous.items():
            signal.signal(signum, handler)


class StopJournal:
    """Persist a stopped daemon identity before SIGSTOP; verify before recovery (N6)."""

    def __init__(self, path, proc, *, sender=None):
        self.path, self.proc, self.sender = Path(path), proc, sender
        self.recovery_fact = None

    def _signal(self, identity, signum, *, role):
        with block_context(process_role=role): self.proc.verify(identity)
        if self.sender is not None:
            # An injected sender is only for synthetic-proc self-tests.
            self.sender(identity.pid, signum)
            return
        if self.proc.root != Path("/proc") or not hasattr(os, "pidfd_open") \
                or not hasattr(signal, "pidfd_send_signal"):
            raise Blocked("stable owned signal unavailable")
        try:
            pidfd = os.pidfd_open(identity.pid)
            try:
                with block_context(process_role=role): self.proc.verify(identity)
                signal.pidfd_send_signal(pidfd, signum)
            finally:
                os.close(pidfd)
        except OSError as error:
            raise Blocked("stable owned signal failed") from error

    def stop(self, identity):
        with block_context(process_role='daemon'): self.proc.verify(identity)
        if self.path.exists():
            raise Blocked("stopped-daemon recovery journal already exists")
        data = json.dumps({"version": 1, **identity.report()}).encode()
        fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as file:
            file.write(data)
            file.flush()
            os.fsync(file.fileno())
        self._signal(identity, signal.SIGSTOP, role='daemon')
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            row = self.proc.stat(identity.pid)
            if row and row["start_ticks"] == identity.start_ticks and row["state"] in {"T", "t"}:
                return
            time.sleep(0.01)
        raise Blocked("daemon SIGSTOP barrier unverified; journal retained")

    def recover(self):
        self.recovery_fact = None
        if not self.path.exists():
            return False
        try:
            row = self.path.lstat()
            if not stat.S_ISREG(row.st_mode) or stat.S_IMODE(row.st_mode) != 0o600 \
                    or row.st_uid != os.getuid() or row.st_size > 4096:
                raise Blocked("stopped-daemon journal unsafe")
            data = json.loads(self.path.read_bytes())
            if set(data) != {"version", "pid", "start_ticks"} or data["version"] != 1:
                raise Blocked("stopped-daemon journal invalid")
            identity = Identity(data["pid"], data["start_ticks"])
        except (OSError, ValueError, TypeError) as error:
            raise Blocked("stopped-daemon journal unreadable") from error
        proof = None
        try:
            current = self.proc.stat(identity.pid)
            if current is not None and current["start_ticks"] != identity.start_ticks:
                # Different start ticks prove the recorded identity ended; never signal its replacement.
                alive, proof = False, "pid_reused"
            else:
                alive = self.proc.alive(identity)
                if alive is None:
                    raise process_block('daemon',None,"stopped-daemon journal identity uncertain; retained")
                after = self.proc.stat(identity.pid)
                if after is not None and after["start_ticks"] != identity.start_ticks:
                    alive, proof = False, "pid_reused"
        except Blocked as error:
            raise process_block('daemon',None,"stopped-daemon journal identity uncertain; retained") from error
        if alive is False:
            self.path.unlink()
            self.recovery_fact = {"identity": identity.report(),
                                  "disposition": "already_gone", "signalled": False}
            if proof is not None:
                self.recovery_fact["proof"] = proof
            return True
        self._signal(identity, signal.SIGCONT, role='daemon')
        self.path.unlink()
        self.recovery_fact = {"identity": identity.report(), "disposition": "resumed", "signalled": True}
        return True

    def resume(self, identity):
        if self.path.exists():
            self.recover()
        else:
            self._signal(identity, signal.SIGCONT, role='daemon')


def stop_proof(*, process_states, locks_free, uncertain=(), pgrep_clear=None):
    """Every tracked identity gone, both locks free, no uncertainty, targeted pgrep."""
    require_proof(not uncertain, "sticky process uncertainty remains")
    if len(locks_free) != 2 or any(value is not True for value in locks_free):
        raise Blocked("private daemon/store lock absence unverified")
    if any(value is not False for value in process_states):
        raise Blocked("owned process stop unverified")
    require_proof(pgrep_clear, "targeted pgrep absence unverified")
    return True


if __name__ == "__main__":
    if sys.argv[1:] == ["--fetch-worker"]:
        raise SystemExit(_fetch_worker())
    raise SystemExit(2)
