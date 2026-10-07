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
from types import SimpleNamespace
import unittest
from unittest import mock

from opencode_cases import Case, DriverAdapter, case_anchor
from opencode_driver import Driver
from opencode_safety import Blocked, Identity, ProcReader, StopJournal, PRIVATE_PARTS

# Each role is a plain stdlib Python fake. No daemon/vendor executable is invoked.
FAKE_ROLES = r'''
import ctypes,json,os,signal,subprocess,sys,threading,time
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
    def worker():
        while True: time.sleep(.005)
    threading.Thread(target=worker,daemon=True).start()
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
    def run_staged_case(self, fixture):
        """Use the live case/adapter chain; only fixture acquisition is synthetic."""
        driver = fixture.driver
        adapter = DriverAdapter(driver)
        adapter.end = time.monotonic() + 10
        case = Case("anchor")
        source = driver.observe
        successors = []

        def observed(kind, target=None):
            if kind == "interruption":
                return {"interrupted": False}
            if kind == "lifecycle_capabilities":
                return {"source_schema_proven": True, "points": {"publication": True}}
            if kind == "lifecycle_successor":
                predecessor = Identity(**target["predecessor"])
                self.assertTrue(fixture.proc.gone(predecessor))
                successors.append(predecessor)
                # The acquired generation is synthetic; death and signal replies are real.
                return {"admitted": True, "parallel_servers": 0, "password_changed": True}
            return source(kind, target)

        sample = {"host_launched": True, "reached": True, "live_pin": True,
                  "retirement_inflight": False, "lock_holders": [driver.anchor.report()],
                  "anchor_identity": driver.anchor.report(),
                  "daemon_identity": driver.daemon.report(),
                  "vendor_identity": driver.vendor_identity.report(),
                  "info_pid": driver.vendor_identity.pid, "record_pid": driver.vendor_identity.pid}
        with mock.patch("opencode_cases.L14_POINTS", ("publication",)), \
                mock.patch.object(driver, "lifecycle", return_value=sample), \
                mock.patch.object(driver, "observe", side_effect=observed), \
                mock.patch.object(driver, "signal_barrier", wraps=driver.signal_barrier) as calls:
            try:
                case_anchor(adapter, case)
            finally:
                operations = [call.args[0]["operation"] for call in calls.call_args_list]
                self.assertEqual(operations, ["daemon_sigstop", "anchor_sigkill",
                                              "vendor_death", "daemon_sigcont"])
        self.assertEqual(len(successors), 1)
        self.assertEqual(case.finish()["result"], "pass")
        return case

    def assert_resumed(self, fixture):
        self.assertTrue(fixture.proc.alive(fixture.identities["daemon"]))
        self.assertNotIn(fixture.proc.stat(fixture.identities["daemon"].pid)["state"], {"T", "t"})
        self.assertFalse(fixture.driver.journal.path.exists())

    def test_l14_cooperative_live_case_staged_chain(self):
        with OwnedBarrierFixture("cooperative") as fixture:
            case = self.run_staged_case(fixture)
            self.assertTrue(fixture.proc.gone(fixture.identities["vendor"]))
            self.assertTrue(fixture.proc.gone(fixture.identities["anchor"]))
            self.assert_resumed(fixture)
            self.assertTrue(any("SIGPIPE" in note for note in case.limitations))

    def test_l14_survivor_live_case_staged_chain_resumes_and_fences(self):
        with OwnedBarrierFixture("survivor") as fixture:
            started = time.monotonic()
            with self.assertRaisesRegex(Blocked, "vendor survived"):
                self.run_staged_case(fixture)
            self.assertGreaterEqual(time.monotonic() - started, 1)
            self.assertTrue(fixture.proc.alive(fixture.identities["vendor"]))
            written = fixture.wait_json("stderr-written.json")
            self.assertTrue(written["written"])
            self.assertEqual(written["pid"], fixture.identities["vendor"].pid)
            self.assertTrue(fixture.wait_json("stderr-drained.json")["received"])
            self.assert_resumed(fixture)
            with mock.patch.object(fixture.driver, "ensure_vendor") as launch:
                with self.assertRaisesRegex(Blocked, "predecessor not gone"):
                    fixture.driver.observe("lifecycle_successor", {
                        "predecessor": fixture.identities["vendor"].report()})
                launch.assert_not_called()

    def test_l14_staged_stop_polls_lagging_worker_before_kill(self):
        with OwnedBarrierFixture("cooperative") as fixture:
            taskroot = Path("/proc") / str(fixture.driver.daemon.pid) / "task"
            workers = [int(path.name) for path in taskroot.iterdir()
                       if int(path.name) != fixture.driver.daemon.pid]
            self.assertTrue(workers)
            original = fixture.driver.proc.parse_stat
            lagged = []

            def parsed(raw, pid):
                row = original(raw, pid)
                if pid == workers[0] and row["state"] in {"T", "t"} and not lagged:
                    lagged.append(pid)
                    return {**row, "state": "R"}
                return row

            started = time.monotonic()
            with mock.patch.object(fixture.driver.proc, "parse_stat", side_effect=parsed):
                self.run_staged_case(fixture)
            self.assertLess(time.monotonic() - started, 5)
            self.assertEqual(lagged, [workers[0]])
            self.assert_resumed(fixture)

    def test_l14_staged_unstopped_worker_exhausts_five_second_bound(self):
        with OwnedBarrierFixture("cooperative") as fixture:
            driver = fixture.driver
            taskroot = Path("/proc") / str(driver.daemon.pid) / "task"
            worker = next(int(path.name) for path in taskroot.iterdir()
                          if int(path.name) != driver.daemon.pid)
            original = driver.proc.parse_stat
            ticks = []

            def clock():
                ticks.append(len(ticks))
                return float(ticks[-1])

            def unstopped(raw, pid):
                row = original(raw, pid)
                return {**row, "state": "R"} if pid == worker else row

            try:
                with mock.patch("opencode_driver.time",
                                SimpleNamespace(monotonic=clock, sleep=time.sleep)), \
                        mock.patch.object(driver.proc, "parse_stat", side_effect=unstopped):
                    with self.assertRaisesRegex(Blocked, "thread-group stop deadline exhausted"):
                        driver.signal_barrier({"operation": "daemon_sigstop",
                                               "identity": driver.daemon.report()})
            finally:
                driver.signal_barrier({"operation": "daemon_sigcont", "identity": driver.daemon.report()})
            self.assertEqual(ticks[-1], 5)
            self.assertIsNone(driver._sigkill_at)
            self.assertTrue(fixture.proc.alive(fixture.identities["anchor"]))
            self.assert_resumed(fixture)

    def test_l14_staged_stopped_group_at_deadline_cannot_pass(self):
        with OwnedBarrierFixture("cooperative") as fixture:
            driver = fixture.driver
            observed_threads = []
            original = driver.proc.parse_stat

            def stopped(raw, pid):
                row = original(raw, pid)
                if pid != driver.daemon.pid:
                    observed_threads.append(row["state"])
                return row

            # The leader is really stopped; model the elapsed leader wait/scan
            # reaching the shared deadline before all-thread proof is accepted.
            clock = mock.Mock(side_effect=[0.0, 5.0])
            try:
                with mock.patch("opencode_driver.time",
                                SimpleNamespace(monotonic=clock, sleep=time.sleep)), \
                        mock.patch.object(driver.proc, "parse_stat", side_effect=stopped):
                    with self.assertRaisesRegex(Blocked, "thread-group stop deadline exhausted"):
                        driver.signal_barrier({"operation": "daemon_sigstop",
                                               "identity": driver.daemon.report()})
            finally:
                driver.signal_barrier({"operation": "daemon_sigcont", "identity": driver.daemon.report()})
            self.assertTrue(observed_threads)
            self.assertTrue(all(state in {"T", "t"} for state in observed_threads))
            self.assertIsNone(driver._sigkill_at)
            self.assertTrue(fixture.proc.alive(fixture.identities["anchor"]))
            self.assert_resumed(fixture)

    def test_l14_staged_missing_daemon_row_blocks_without_killing_anchor(self):
        with OwnedBarrierFixture("cooperative") as fixture:
            driver = fixture.driver
            driver.signal_barrier({"operation": "daemon_sigstop", "identity": driver.daemon.report()})
            original = driver.proc.stat
            daemon_reads = []

            def vanished(pid):
                if pid == driver.daemon.pid:
                    daemon_reads.append(pid)
                    # verify(alive) reads twice; the subsequent independent sample vanishes.
                    if len(daemon_reads) == 3:
                        return None
                return original(pid)

            try:
                with mock.patch.object(driver.proc, "stat", side_effect=vanished):
                    with self.assertRaisesRegex(Blocked, "daemon barrier not held"):
                        driver.signal_barrier({"operation": "anchor_sigkill",
                                               "identity": driver.anchor.report(), "group": False})
            finally:
                driver.signal_barrier({"operation": "daemon_sigcont", "identity": driver.daemon.report()})
            self.assertTrue(fixture.proc.alive(fixture.identities["anchor"]))
            self.assert_resumed(fixture)

    def test_l14_prekill_observation_failure_still_resumes_owned_daemon(self):
        with OwnedBarrierFixture("cooperative") as fixture:
            with mock.patch.object(fixture.driver, "_stdin_held", side_effect=Blocked("stdin uncertain")), \
                    mock.patch.object(fixture.driver, "_staged_signal", wraps=fixture.driver._staged_signal) as staged:
                with self.assertRaisesRegex(Blocked, "stdin uncertain"):
                    fixture.driver.signal_barrier(fixture.sample())
                self.assertEqual([call.args[0]["operation"] for call in staged.call_args_list],
                                 ["daemon_sigstop", "daemon_sigcont"])
            self.assertTrue(fixture.proc.alive(fixture.identities["anchor"]))
            self.assertTrue(fixture.proc.alive(fixture.identities["vendor"]))
            self.assert_resumed(fixture)


def test_names():
    return unittest.defaultTestLoader.getTestCaseNames(BarrierTests)


def self_test():
    result = unittest.TestResult()
    unittest.defaultTestLoader.loadTestsFromTestCase(BarrierTests).run(result)
    return [test.id().rsplit(".", 1)[-1] for test, _detail in (*result.errors, *result.failures)]


if __name__ == "__main__":
    unittest.main(verbosity=2)
