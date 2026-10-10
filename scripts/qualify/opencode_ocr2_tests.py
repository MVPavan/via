"""Review ocr2 regressions: lock identity, event facts, scan bounds, separation, root policy (§13)."""
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
import urllib.parse
from unittest import mock

import opencode_cases as cases
import opencode_driver as runtime
import opencode_safety as safety
import opencode_lockscan_tests as locktests


def scan_fixture(test, **arguments):
    helper=locktests.LockScopeTests();test.addCleanup(helper.doCleanups)
    return helper.fixture_scan(**arguments)


def locktests_unreadable(name):
    error=safety.Blocked('owned process observation unreadable')
    error.__cause__=PermissionError(13,'FAKE',name)
    return error


def lock_line(d, suffix=''):
    row=(d.namespace/'server.lock').stat()
    return ('lock:\t1: FLOCK  ADVISORY  WRITE 99 %02x:%02x:%d%s 0 EOF'
            % (safety.os.major(row.st_dev),safety.os.minor(row.st_dev),row.st_ino,suffix)).encode()


class LockIdentityTests(unittest.TestCase):
    """B1: the complete parsed device/inode tuple, never a substring."""
    def holders(self, anchor_line, outsider_line):
        d,anchor,_=scan_fixture(self,readable=True)
        def read(identity,name,**_kwargs):
            if name=='comm':return b'systemd\n'
            return anchor_line(d) if identity==anchor else outsider_line(d)
        d.proc.read.side_effect=read
        return d,anchor

    def test_outsider_on_an_inode_with_the_lock_inode_as_prefix_is_not_an_escape(self):
        d,anchor=self.holders(lock_line,lambda d:lock_line(d,'0'))
        self.assertEqual(d._lock_holders(anchor),[anchor.report()])
        self.assertIsNone(d._lock_escape)

    def test_anchor_holding_only_a_prefix_inode_does_not_hold_the_lock(self):
        d,anchor=self.holders(lambda d:lock_line(d,'0'),lambda d:b'flags: 01\n')
        with self.assertRaisesRegex(safety.Blocked,'not held by anchor'):d._lock_holders(anchor)
        self.assertIsNone(d._lock_escape)

    def test_outsider_on_the_exact_tuple_still_escapes(self):
        d,anchor=self.holders(lock_line,lock_line)
        with self.assertRaisesRegex(cases.QualificationFailure,'escaped anchor'):d._lock_holders(anchor)

    def test_unparseable_flock_record_blocks(self):
        d,anchor=self.holders(lock_line,lambda d:b'lock:\t1: FLOCK  ADVISORY  WRITE 99 FAKE 0 EOF')
        with self.assertRaisesRegex(safety.Blocked,'lock descriptor record'):d._lock_holders(anchor)
        self.assertIsNone(d._lock_escape)


class EventFactAllowlistTests(unittest.TestCase):
    """B3: only reviewed schema paths keep their key names."""
    def facts(self, value):
        from opencode_eventbound import event_bound_facts
        return event_bound_facts(json.dumps(value,separators=(',',':')).encode())

    def test_unreviewed_keys_become_wildcards_everywhere(self):
        private='customerPrivateIdentifier'
        for kind in ('session.step.ended','session.inbox.enqueued','vendor.FAKE'):
            with self.subTest(kind=kind):
                facts=self.facts({'type':kind,'data':{'sessionID':'ses_FAKE',private:'x'*(1<<17),
                    'metadata':{private:{private:'y'*1024}}}})
                self.assertNotIn(private,json.dumps(facts))
                self.assertIn('data.*',[row['path'] for row in facts['fixture_fields']])

    def test_reviewed_step_ended_paths_keep_their_names(self):
        facts=self.facts({'type':'session.step.ended','data':{'sessionID':'ses_FAKE',
            'assistantMessageID':'msg_FAKE','finish':'stop','files':['f%05d'%n for n in range(200)],
            'tokens':{'input':1,'cache':{'read':2}}}})
        self.assertIn('data.files',[row['path'] for row in facts['field_bytes']])


class LockScanBoundTests(unittest.TestCase):
    """I4: every observation and recheck is bounded; the scan rechecks before returning."""
    def test_deadline_passing_during_historical_classification_blocks(self):
        d,anchor,listing=scan_fixture(self)
        clock=[1000.0]
        base=d.proc.read.side_effect
        def read(identity,name,**kwargs):
            if name=='comm':clock[0]+=runtime.LOCK_SCAN_SECONDS+1  # Expires during the comm read.
            return base(identity,name,**kwargs)
        d.proc.read.side_effect=read
        proc=d.proc.root
        def ordered(path):
            rows=list(listing(path))
            return sorted(rows,key=lambda row:row.name) if path==proc else rows
        with mock.patch.object(Path,'iterdir',ordered), \
                mock.patch.object(runtime.time,'monotonic',lambda:clock[0]), \
                self.assertRaisesRegex(safety.Blocked,'lock scan deadline'):
            d._lock_holders(anchor)
        self.assertFalse(any(call.args[0]=='lock-scan-exclusion' for call in d._record.call_args_list))

    def expire_during(self, d, owner, name, effect=None):
        clock=[1000.0]
        def observe(*args,**kwargs):
            clock[0]+=runtime.LOCK_SCAN_SECONDS+1
            return effect(*args,**kwargs) if effect else None
        setattr(owner,name,mock.Mock(side_effect=observe))
        return mock.patch.object(runtime.time,'monotonic',lambda:clock[0])

    def test_deadline_passing_during_final_anchor_verification_blocks(self):
        d,anchor,_=scan_fixture(self,readable=True)
        with self.expire_during(d,d,'_verify'),self.assertRaisesRegex(safety.Blocked,'lock scan deadline'):
            d._lock_holders(anchor)

    def test_deadline_passing_during_saved_identity_liveness_blocks(self):
        d,anchor,_=scan_fixture(self,readable=True)
        base=d.proc.read.side_effect
        def read(identity,name,**kwargs):
            if identity.pid==11:raise locktests_unreadable(name)
            return base(identity,name,**kwargs)
        d.proc.read.side_effect=read
        listing=Path.iterdir
        def ordered(path):  # The candidate is enumerated last.
            return sorted(listing(path),key=lambda row:row.name) if path==d.proc.root else listing(path)
        with self.expire_during(d,d.proc,'alive',lambda identity:False), \
                mock.patch.object(Path,'iterdir',ordered), \
                self.assertRaisesRegex(safety.Blocked,'lock scan deadline'):
            d._lock_holders(anchor)

    def test_every_observation_and_recheck_counts_toward_the_work_bound(self):
        from opencode_lockscan import historical_exception
        d,anchor,_=scan_fixture(self)
        observed=[]
        for name in ('stat','read','verify'):
            original=getattr(d.proc,name).side_effect
            def wrapped(*args,_name=name,_original=original,**kwargs):
                observed.append(('proc',_name))
                return _original(*args,**kwargs) if _original else None
            getattr(d.proc,name).side_effect=wrapped
        ticks=[]
        def guard():
            ticks.append(len(observed))
        result=historical_exception(d.proc,safety.Identity(11,20),{anchor},100,100,guard=guard)
        self.assertIsNotNone(result)
        # A guard precedes every observation, and one more follows the last before returning.
        self.assertGreaterEqual(len(ticks),len(observed)+1)
        self.assertEqual(ticks[-1],len(observed))


class ProjectSeparationTests(unittest.TestCase):
    """I5: every registered storage/runtime root and every distinct project, at every start."""
    def driver(self):
        folder=tempfile.TemporaryDirectory();self.addCleanup(folder.cleanup)
        root=Path(folder.name)
        d=runtime.Driver('release','fp','pin',root/'evidence',initialize=False)
        d.work=root/'work';d.state=d.work/'state';d.home=d.work/'home';d.runtime=d.work/'runtime'
        d.helpers=d.work/'helpers';d.namespace=d.work/'namespace';d.env={};d.namespace_env={}
        d.ownership=mock.Mock();d.ownership.ordered.return_value=[]
        d.fixtures={};d.namespace_project=None
        return d

    def test_registered_runtime_root_inside_a_project_blocks(self):
        from opencode_ownership import Root
        d=self.driver();project=d.work/'projects'/'state'
        d.ownership.ordered.return_value=[Root(project/'run','via-owned',runtime=True)]
        with self.assertRaisesRegex(safety.Blocked,'project overlaps'):d._check_project_separation(project)

    def test_start_checks_every_other_distinct_project(self):
        d=self.driver();fixture=d.work/'fixtures'/'one'
        d.fixtures={'one':{'path':fixture,'config':{}}}
        d.namespace_project=fixture/'nested'
        with self.assertRaisesRegex(safety.Blocked,'project overlaps'):
            d._check_project_separation(d.namespace_project)
        with self.assertRaisesRegex(safety.Blocked,'project overlaps'):
            d._check_project_separation(fixture)

    def test_same_project_listed_twice_is_one_project(self):
        d=self.driver();project=d.work/'projects'/'state'
        d.fixtures={'sentinel':{'path':project,'config':{}}};d.namespace_project=project
        d._check_project_separation(project)

    def test_fixture_creation_checks_separation(self):
        d=self.driver();d.fixtures={}
        d.namespace_project=d.work/'fixtures'
        d._directory=mock.Mock(return_value=d.work/'fixtures')
        with self.assertRaisesRegex(safety.Blocked,'project overlaps'):d.fixture('one',{'provider':'mock'})


class RunRootPolicyTests(unittest.TestCase):
    """B2 ruling: the run root is not protected material in mock diagnostic capture."""
    def setUp(self):
        from opencode_ownership import OwnershipRegistry
        folder=tempfile.TemporaryDirectory();self.addCleanup(folder.cleanup)
        root=Path(folder.name);self.work=root/'private';self.work.mkdir()
        evidence=root/'evidence';evidence.mkdir()
        self.owned=OwnershipRegistry(evidence);self.owned.register(self.work,'vendor-private')

    def capture(self,message,replacements=()):
        from opencode_diagnostic import capture
        (self.work/'opencode.db').unlink(missing_ok=True)
        with sqlite3.connect(self.work/'opencode.db') as db:
            db.execute('CREATE TABLE session_message(session_id TEXT,type TEXT,seq,data TEXT)')
            db.execute('INSERT INTO session_message VALUES(?,?,?,?)',('ses_FAKE','assistant',1,
                json.dumps({'error':{'name':'FAKE_Error','message':message}})))
        return capture(self.work,self.owned,{'ses_FAKE':{'model':safety.MOCK_IDENTITY}},
                       replacements=replacements)

    def test_encoded_run_root_forms_are_retained_not_blocked(self):
        root=str(self.work)
        for value in (root.replace('/','\\/'),urllib.parse.quote(urllib.parse.quote(root,safe=''),safe=''),
                      root.upper()):
            with self.subTest(value=value):
                rows=self.capture('FAKE '+value)
                self.assertEqual(len(rows['sessions'][0]['errors']),1)

    def test_exact_run_root_is_still_redacted(self):
        rows=self.capture('FAKE '+str(self.work)+'/opencode.db')
        self.assertNotIn(str(self.work),json.dumps(rows))
        self.assertIn('<private-run-root>',json.dumps(rows))

    def test_encoded_passwords_still_block(self):
        password=b'FAKEpassword0123456789'
        encoded=''.join('%%%02X'%byte for byte in password).replace('%','%25')
        with self.assertRaises(safety.Blocked):self.capture('FAKE '+encoded,(password,))


if __name__ == '__main__':
    unittest.main()
