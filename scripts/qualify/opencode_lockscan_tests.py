"""Amended descriptor scope regressions (OpenCode packet §13 L14)."""
import errno
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import opencode_cases as cases
import opencode_driver as runtime
import opencode_safety as safety


class LockScopeTests(unittest.TestCase):
    def fixture_scan(self, *, started=20, parent=1, alive=True, private=False, comm=b'systemd\n',
                     readable=False, holds_lock=False, missing_parent=False, changed=False):
        directory=tempfile.TemporaryDirectory();self.addCleanup(directory.cleanup)
        root=Path(directory.name);d=runtime.Driver('release','fp','pin',root/'evidence')
        d.namespace=root/'namespace';d.namespace.mkdir();lock=d.namespace/'server.lock';lock.touch()
        row=lock.stat();lockid=f'{safety.os.major(row.st_dev):02x}:{safety.os.minor(row.st_dev):02x}:{row.st_ino}'
        proc=root/'proc'
        for pid in (10,11):(proc/str(pid)/'fdinfo').mkdir(parents=True)
        for pid in (10,11):(proc/str(pid)/'fdinfo/5').touch()
        d.proc=mock.Mock(root=proc)
        anchor=safety.Identity(10,100);candidate=safety.Identity(11,started)
        d.anchor=anchor;d._first_server_ticks=100;d._run_start_ticks=100;d._server_roots={anchor}
        if private:d.helper_ledger[candidate]={'kind':'tool'}
        calls=0
        def stat(pid):
            nonlocal calls
            if pid==11:
                calls+=1
                return {'pid':11,'start_ticks':started+(calls-1 if changed else 0),'ppid':parent}
            if pid==10:return {'pid':10,'start_ticks':100,'ppid':1}
            return None if missing_parent else {'pid':pid,'start_ticks':10,'ppid':1}
        d.proc.stat.side_effect=stat;d.proc.alive.return_value=alive
        def read(identity,name,**_kwargs):
            if name=='comm':return comm
            if identity==anchor or holds_lock:return ('lock: FLOCK '+lockid).encode()
            return b'flags: 01\n'
        d.proc.read.side_effect=read;d._verify=mock.Mock();d._record=mock.Mock()
        listing=Path.iterdir
        def inaccessible(path):
            if path==proc/'11/fdinfo' and not readable:
                raise PermissionError(errno.EACCES,'FAKE hidden',str(path))
            return listing(path)
        return d,anchor,inaccessible

    def test_historical_unreadable_non_descendant_is_recorded_and_excluded(self):
        d,anchor,listing=self.fixture_scan()
        with mock.patch.object(Path,'iterdir',listing):self.assertEqual(d._lock_holders(anchor),[anchor.report()])
        d._record.assert_called_once()
        label,row=d._record.call_args.args
        self.assertEqual(label,'lock-scan-exclusion')
        self.assertEqual((row['pid'],row['start_ticks'],row['comm_class'],row['errno']),
                         (11,20,'systemd','EACCES'))
        self.assertEqual(row['disposition'],'record-only')

    def test_old_unknown_comm_is_classified_without_retaining_its_text(self):
        d,anchor,listing=self.fixture_scan(comm=b'FAKE_PRIVATE_NAME\n')
        with mock.patch.object(Path,'iterdir',listing):self.assertEqual(d._lock_holders(anchor),[anchor.report()])
        self.assertEqual(d._record.call_args.args[1]['comm_class'],'other')
        self.assertNotIn('FAKE_PRIVATE_NAME',json.dumps(d._record.call_args.args[1]))

    def test_new_owned_descendant_unknown_and_changing_processes_still_block(self):
        cases=({'started':100},{'started':101},{'private':True},{'parent':10},
               {'parent':12,'missing_parent':True},{'alive':None},{'changed':True})
        for arguments in cases:
            with self.subTest(arguments=arguments):
                d,anchor,listing=self.fixture_scan(**arguments)
                with mock.patch.object(Path,'iterdir',listing),self.assertRaises(safety.Blocked):
                    d._lock_holders(anchor)
                self.assertFalse(any(call.args[0]=='lock-scan-exclusion' for call in d._record.call_args_list))

    def test_readable_old_process_cannot_hold_the_lock(self):
        d,anchor,listing=self.fixture_scan(readable=True,holds_lock=True)
        with mock.patch.object(Path,'iterdir',listing),self.assertRaisesRegex(cases.QualificationFailure,'escaped anchor'):
            d._lock_holders(anchor)
