#!/usr/bin/env python3
"""Owned fake-process L14 controls; vendors/opencode.md §13 (no live vendor/VIA).

Real Linux pidfds, parent-death signal and driver SIGSTOP barrier are exercised.
The deliberate survivor is outside the cooperative threat model and only tests
that failed death proof cannot admit a successor. All children use private roots.
"""
import ctypes
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from opencode_driver import Driver
from opencode_safety import Blocked, Identity, ProcReader, StopJournal, PRIVATE_PARTS

# Each role is a plain stdlib Python fake. No daemon/vendor executable is invoked.
FAKE_ROLES = r'''
import ctypes,json,os,signal,subprocess,sys,time
from pathlib import Path
root=Path(sys.argv[2]); role=sys.argv[1]; mode=sys.argv[3]
libc=ctypes.CDLL(None,use_errno=True)
def publish(name,value):
    path=root/name
    temporary=root/(name+'.pending')
    temporary.write_text(json.dumps(value))
    temporary.replace(path)
def identity():
    fields=Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()
    return {'pid':os.getpid(),'start_ticks':int(fields[19])}
if role=='vendor':
    parent=os.getppid()
    wanted=signal.SIGKILL if mode=='cooperative' else 0
    if libc.prctl(1,wanted,0,0,0)!=0: os._exit(60)
    if os.getppid()!=parent: os._exit(61)
    publish('vendor.json',{**identity(),'parent':parent,'pdeathsig':wanted})
    reported=False
    while True:
        if mode=='survivor' and not reported and os.getppid()!=parent:
            # Daemon retains this pipe's reader throughout SIGSTOP: no SIGPIPE.
            os.write(2,b'survivor-stderr-after-anchor\n')
            publish('stderr-written.json',{**identity(),'written':True})
            reported=True
        time.sleep(.005)
elif role=='anchor':
    child=subprocess.Popen([sys.executable,'-I','-S',__file__,'vendor',str(root),mode],
                           stdin=0,stdout=subprocess.DEVNULL,stderr=2,close_fds=True)
    publish('anchor.json',{**identity(),'vendor_pid':child.pid})
    while True: time.sleep(.01)
elif role=='daemon':
    read_end,write_end=os.pipe()
    stderr_read,stderr_write=os.pipe()
    os.set_blocking(stderr_read,False)
    child=subprocess.Popen([sys.executable,'-I','-S',__file__,'anchor',str(root),mode],
                           stdin=read_end,stdout=subprocess.DEVNULL,stderr=stderr_write,
                           close_fds=True)
    os.close(read_end); os.close(stderr_write)
    publish('daemon.json',{**identity(),'anchor_pid':child.pid})
    while True:
        child.poll()
        try:
            data=os.read(stderr_read,4096)
            if data:
                publish('stderr-drained.json',{'received':data==b'survivor-stderr-after-anchor\n'})
        except BlockingIOError: pass
        time.sleep(.005)
else: os._exit(62)
'''


class OwnedBarrierFixture:
    """Own three fake identities and positively stop/reap them before root removal."""

    def __init__(self, mode):
        self.root = Path(tempfile.mkdtemp(prefix="via-oc-barrier-"))
        self.proc = ProcReader()
        self.identities = {}
        self.libc = ctypes.CDLL(None, use_errno=True)
        self.original_subreaper = ctypes.c_int()
        if self.libc.prctl(37, ctypes.byref(self.original_subreaper), 0, 0, 0) != 0 \
                or self.libc.prctl(36, 1, 0, 0, 0) != 0:
            shutil.rmtree(self.root)
            raise Blocked("fake-process subreaper unavailable")
        helper = self.root / "fake_roles.py"
        helper.write_text(FAKE_ROLES)
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
        for key, part in PRIVATE_PARTS.items():
            path = self.root / part
            path.mkdir(mode=0o700)
            env[key] = str(path)
        self.env = env
        self.handle = subprocess.Popen([sys.executable, "-I", "-S", str(helper), "daemon",
                                        str(self.root), mode], env=env, cwd=self.root,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.PIPE)
        self.driver = None
        try:
            initial = self.proc.stat(self.handle.pid)
            if initial is None:
                raise Blocked("fake daemon initial identity unavailable")
            self.identities["daemon"] = Identity(initial["pid"], initial["start_ticks"])
            for role in ("daemon", "anchor", "vendor"):
                row = self.wait_json(role + ".json")
                identity = Identity(row["pid"], row["start_ticks"])
                self.proc.verify(identity)
                self.identities[role] = identity
            daemon = self.proc.stat(self.identities["daemon"].pid)
            anchor = self.proc.stat(self.identities["anchor"].pid)
            vendor = self.proc.stat(self.identities["vendor"].pid)
            if daemon["ppid"] != os.getpid() or anchor["ppid"] != self.identities["daemon"].pid \
                    or vendor["ppid"] != self.identities["anchor"].pid:
                raise Blocked("fake process ownership chain differs")
            self.driver = Driver(helper, helper, helper, self.root / "evidence", initialize=False)
            self.driver.phase_kind = "release"
            self.driver.daemon = self.identities["daemon"]
            self.driver.anchor = self.identities["anchor"]
            self.driver.vendor_identity = self.identities["vendor"]
            self.driver.identities = set(self.identities.values())
        except BaseException:
            self.close()
            raise

    def wait_json(self, name, timeout=5):
        path = self.root / name
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if path.exists():
                raw = path.read_bytes()
                if len(raw) > 4096:
                    raise Blocked("fake readiness evidence exceeds bound")
                return json.loads(raw)
            if self.handle.poll() is not None:
                raise Blocked("fake daemon ended before readiness")
            time.sleep(.005)
        raise Blocked("fake process readiness unobserved")

    def sample(self):
        return {**{key: identity.report() for key, identity in self.identities.items()},
                "reached": True, "live_pin": True, "retirement_inflight": False}

    def close(self):
        try:
            if self.driver is not None and self.driver.journal.path.exists():
                self.driver.journal.recover()
            # Descendants first; signal only positively captured PID/start-tick identities.
            sender = StopJournal(self.root / "cleanup-journal.json", self.proc)
            for role in ("vendor", "anchor", "daemon"):
                identity = self.identities.get(role)
                if identity is None:
                    continue
                if self.proc.alive(identity) is True:
                    sender._signal(identity, signal.SIGKILL)
                elif self.proc.alive(identity) is None:
                    raise Blocked("fake cleanup identity uncertain")
            self.handle.wait(timeout=2)
            deadline = time.monotonic() + 2
            while True:
                for identity in self.identities.values():
                    try:
                        os.waitpid(identity.pid, os.WNOHANG)
                    except ChildProcessError:
                        pass
                rows = [self.proc.stat(identity.pid) for identity in self.identities.values()]
                reaped = all(row is None or row["start_ticks"] != identity.start_ticks
                             for row, identity in zip(rows, self.identities.values()))
                if reaped and all(self.proc.gone(identity) for identity in self.identities.values()):
                    break
                if time.monotonic() >= deadline:
                    raise Blocked("fake fixture stop/reap proof unavailable")
                time.sleep(.005)
            checked = subprocess.run(["/usr/bin/pgrep", "-P", str(os.getpid())], env=self.env,
                                     cwd=self.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     timeout=2, check=False)
            if checked.returncode not in {0, 1} or checked.stderr \
                    or {str(identity.pid).encode() for identity in self.identities.values()} \
                    & set(checked.stdout.split()):
                raise Blocked("fake fixture targeted pgrep absence unverified")
            self.handle.stderr.close()
            if len(self.identities) != 3:
                raise Blocked("fake fixture incomplete child identities; root retained")
            shutil.rmtree(self.root)
        finally:
            if self.libc.prctl(36, self.original_subreaper.value, 0, 0, 0) != 0:
                raise Blocked("fake fixture subreaper restore failed")

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()


class BarrierTests(unittest.TestCase):
    def test_l14_cooperative_parent_death_while_real_daemon_stopped(self):
        with OwnedBarrierFixture("cooperative") as fixture:
            result = fixture.driver.signal_barrier(fixture.sample())
            for fact in ("daemon_stopped", "stop_identity_persisted", "stdin_held",
                         "anchor_pid_only", "vendor_gone_within_1s", "daemon_resumed"):
                self.assertIs(result[fact], True)
            self.assertLess(result["death_elapsed_s"], 1)
            self.assertTrue(fixture.proc.gone(fixture.identities["vendor"]))
            self.assertTrue(fixture.proc.gone(fixture.identities["anchor"]))
            self.assertTrue(fixture.proc.alive(fixture.identities["daemon"]))
            self.assertNotIn(fixture.proc.stat(fixture.identities["daemon"].pid)["state"], {"T", "t"})
            self.assertFalse(fixture.driver.journal.path.exists())
            # Driver retains the real-vendor N5 limit; fake stderr reader never closed.
            self.assertIs(result["stderr_sigpipe_limitation"], True)

    def test_l14_survivor_fails_death_proof_resumes_and_fences_successor(self):
        with OwnedBarrierFixture("survivor") as fixture:
            started = time.monotonic()
            with self.assertRaisesRegex(Blocked, "vendor survived"):
                fixture.driver.signal_barrier(fixture.sample())
            self.assertGreaterEqual(time.monotonic() - started, 1)
            self.assertTrue(fixture.proc.alive(fixture.identities["vendor"]))
            written = fixture.wait_json("stderr-written.json")
            self.assertTrue(written["written"])
            self.assertEqual(written["pid"], fixture.identities["vendor"].pid)
            self.assertTrue(fixture.wait_json("stderr-drained.json")["received"])
            self.assertNotIn(fixture.proc.stat(fixture.identities["daemon"].pid)["state"], {"T", "t"})
            self.assertFalse(fixture.driver.journal.path.exists())
            # Exercise actual admission guard: a real living predecessor blocks before launch.
            with mock.patch.object(fixture.driver, "ensure_vendor") as launch:
                with self.assertRaisesRegex(Blocked, "predecessor not gone"):
                    fixture.driver.observe("lifecycle_successor", {
                        "predecessor": fixture.identities["vendor"].report()})
                launch.assert_not_called()
            evidence = list(fixture.driver.evidence.glob("*-l14.json"))
            self.assertEqual(len(evidence), 1)
            result = json.loads(evidence[0].read_bytes())
            self.assertIs(result["vendor_gone_within_1s"], False)
            self.assertIs(result["daemon_resumed"], True)

    def test_l14_prekill_observation_failure_still_resumes_owned_daemon(self):
        with OwnedBarrierFixture("cooperative") as fixture:
            with mock.patch.object(fixture.driver, "_stdin_held", side_effect=Blocked("stdin uncertain")):
                with self.assertRaisesRegex(Blocked, "stdin uncertain"):
                    fixture.driver.signal_barrier(fixture.sample())
            self.assertTrue(fixture.proc.alive(fixture.identities["anchor"]))
            self.assertTrue(fixture.proc.alive(fixture.identities["vendor"]))
            self.assertNotIn(fixture.proc.stat(fixture.identities["daemon"].pid)["state"], {"T", "t"})
            self.assertFalse(fixture.driver.journal.path.exists())


def test_names():
    return unittest.defaultTestLoader.getTestCaseNames(BarrierTests)


def self_test():
    result = unittest.TestResult()
    unittest.defaultTestLoader.loadTestsFromTestCase(BarrierTests).run(result)
    return [test.id().rsplit(".", 1)[-1] for test, _detail in (*result.errors, *result.failures)]


if __name__ == "__main__":
    unittest.main(verbosity=2)
