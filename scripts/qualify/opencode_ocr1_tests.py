"""Review ocr1 regressions: lock evidence, diagnostic capture, spending latch, layout (§13)."""
import errno
import json
from pathlib import Path
import sqlite3
import tempfile
import time
import unittest
import urllib.parse
from unittest import mock

import opencode_cases as cases
import opencode_driver as runtime
import opencode_safety as safety
import opencode_lockscan_tests as locktests


def lock_fixture(test, **arguments):
    helper=locktests.LockScopeTests();test.addCleanup(helper.doCleanups)
    d,anchor,listing=helper.fixture_scan(**arguments)
    (d.proc.root/'11'/'fdinfo'/'6').touch()
    return d,anchor,listing


def unreadable(name):
    error=safety.Blocked('owned process observation unreadable')
    error.__cause__=PermissionError(errno.EACCES,'FAKE',name)
    return error


class LockEvidenceTests(unittest.TestCase):
    def foreign_then_unreadable(self,d,anchor,*,after=lambda:None,replacement=None):
        base=d.proc.read.side_effect;reads={'count':0}
        def read(identity,name,**kwargs):
            if identity.pid==11 and name.startswith('fdinfo/') and identity!=replacement:
                reads['count']+=1
                if reads['count']==1:return base(anchor,name,**kwargs)  # A verified FLOCK line.
                after();raise unreadable(name)
            return base(identity,name,**kwargs)
        d.proc.read.side_effect=read

    def test_observed_foreign_lock_survives_historical_exclusion(self):
        d,anchor,_=lock_fixture(self,readable=True)
        self.foreign_then_unreadable(d,anchor)
        with self.assertRaisesRegex(cases.QualificationFailure,'escaped anchor'):d._lock_holders(anchor)
        with self.assertRaisesRegex(cases.QualificationFailure,'escaped anchor'):d._lock_holders(anchor)

    def test_observed_foreign_lock_survives_later_exit(self):
        d,anchor,_=lock_fixture(self,readable=True,started=150)
        self.foreign_then_unreadable(d,anchor,after=lambda:setattr(d.proc.alive,'return_value',False))
        with self.assertRaisesRegex(cases.QualificationFailure,'escaped anchor'):d._lock_holders(anchor)

    def test_observed_foreign_lock_survives_pid_reuse_rescan(self):
        d,anchor,_=lock_fixture(self,readable=True,started=150)
        replaced=safety.Identity(11,151);changed=[False]
        self.foreign_then_unreadable(d,anchor,after=lambda:changed.__setitem__(0,True),replacement=replaced)
        d.proc.stat.side_effect=lambda pid:{'pid':pid,'ppid':1,
            'start_ticks':100 if pid==10 else 151 if changed[0] else 150}
        with self.assertRaisesRegex(cases.QualificationFailure,'escaped anchor'):d._lock_holders(anchor)

    def test_replacement_seen_only_by_liveness_is_rescanned(self):
        d,anchor,_=lock_fixture(self,readable=True,started=150)
        original=safety.Identity(11,150);replaced=safety.Identity(11,151)
        calls={'stat':0};scanned=[]
        def stat(pid):
            if pid==10:return {'pid':10,'start_ticks':100,'ppid':1}
            calls['stat']+=1
            return {'pid':11,'start_ticks':150 if calls['stat']<=2 else 151,'ppid':1}
        d.proc.stat.side_effect=stat
        d.proc.alive.side_effect=lambda identity:identity!=original
        base=d.proc.read.side_effect
        def read(identity,name,**kwargs):
            if identity==original:raise unreadable(name)
            if identity==replaced:scanned.append(name);return base(anchor,name,**kwargs)
            return base(identity,name,**kwargs)
        d.proc.read.side_effect=read
        with self.assertRaisesRegex(cases.QualificationFailure,'escaped anchor'):d._lock_holders(anchor)
        self.assertTrue(scanned)

    def test_scan_checks_deadline_interruption_and_work_bound(self):
        d,anchor,_=lock_fixture(self,readable=True)
        d.phase_deadline=time.monotonic()-1
        with self.assertRaisesRegex(safety.Blocked,'lock scan deadline'):d._lock_holders(anchor)
        d,anchor,_=lock_fixture(self,readable=True)
        d.signals=mock.Mock();d.signals.guard.side_effect=safety.Blocked('FAKE deferred interruption')
        with self.assertRaisesRegex(safety.Blocked,'FAKE deferred interruption'):d._lock_holders(anchor)
        d,anchor,_=lock_fixture(self,readable=True)
        with mock.patch.object(runtime,'LOCK_SCAN_WORK',2), \
                self.assertRaisesRegex(safety.Blocked,'lock scan work bound'):
            d._lock_holders(anchor)


class CaptureTests(unittest.TestCase):
    def setUp(self):
        from opencode_ownership import OwnershipRegistry
        folder=tempfile.TemporaryDirectory();self.addCleanup(folder.cleanup)
        root=Path(folder.name);self.work=root/'private';self.work.mkdir()
        evidence=root/'evidence';evidence.mkdir()
        self.owned=OwnershipRegistry(evidence);self.owned.register(self.work,'vendor-private')
        self.sessions={'ses_FAKE':{'model':safety.MOCK_IDENTITY}}

    def database(self,*rows):
        with sqlite3.connect(self.work/'opencode.db') as db:
            db.execute('CREATE TABLE session_message(session_id TEXT,type TEXT,seq,data TEXT)')
            for seq,data in rows:
                db.execute('INSERT INTO session_message VALUES(?,?,?,?)',('ses_FAKE','assistant',seq,
                    data if type(data) is str else json.dumps(data)))

    def capture(self,replacements=()):
        from opencode_diagnostic import capture
        return capture(self.work,self.owned,self.sessions,replacements=replacements)

    def test_encoded_passwords_and_handles_never_reach_the_record(self):
        # The run root is not protected material here (ocr2 ruling; opencode_ocr2_tests).
        password=b'FAKEpassword0123456789';handle=b'FAKEhandle0123456789'
        def twice(value):  # Every byte percent-encoded, then the percent signs encoded again.
            return ''.join('%%%02X'%byte for byte in value).replace('%','%25')
        encoded=(twice(password),twice(handle))
        for value in encoded:
            for place in ('message','name'):
                with self.subTest(value=value,place=place):
                    (self.work/'opencode.db').unlink(missing_ok=True)
                    self.database((1,{'error':{'name':'FAKE_Error','message':'FAKE',place:'FAKE '+value}}))
                    with self.assertRaises(safety.Blocked):self.capture((password,handle))
            with self.subTest(value=value,place='log'):
                (self.work/'opencode.db').unlink(missing_ok=True)
                (self.work/'opencode.log').write_text('ERROR sessionID=ses_FAKE FAKE '+value+'\n')
                with self.assertRaises(safety.Blocked):self.capture((password,handle))
                (self.work/'opencode.log').unlink()

    def test_seq_must_be_a_bounded_integer(self):
        self.database(('FAKE_RAW_SEQ_TEXT',{'error':{'name':'FAKE_Error','message':'FAKE'}}))
        with self.assertRaisesRegex(safety.Blocked,'sequence'):self.capture()

    def test_log_source_is_a_hashed_identifier(self):
        name=urllib.parse.quote(str(self.work),safe='')+'.log'
        (self.work/name).write_text('ERROR sessionID=ses_FAKE harmless\n')
        rows=self.capture()
        self.assertEqual(len(rows['log_lines']),1)
        self.assertRegex(rows['log_lines'][0]['source'],r'^vendor-log-[0-9a-f]{16}$')

    def test_only_documented_native_error_locations_are_exported(self):
        self.database((1,{'error':{'name':'FAKE_MessageError','message':'FAKE message failure'},
            'content':[{'type':'tool','state':{'status':'error','error':{'name':'FAKE_ToolError','message':'FAKE tool'},
                'input':{'error':{'name':'NEVER_ARGUMENT_NAME','message':'NEVER_ARGUMENT_TEXT'}},
                'output':{'error':{'message':'NEVER_OUTPUT_TEXT'}}}},
                {'type':'text','error':{'message':'NEVER_TEXT_PART'}}]}))
        rows=self.capture();raw=json.dumps(rows)
        self.assertEqual(sorted(row['name'] for row in rows['sessions'][0]['errors']),
                         ['FAKE_MessageError','FAKE_ToolError'])
        for forbidden in ('NEVER_ARGUMENT','NEVER_OUTPUT','NEVER_TEXT_PART'):self.assertNotIn(forbidden,raw)

    def test_location_context_requires_exact_directory_equality(self):
        project=self.work/'project';self.sessions['ses_FAKE']['project']=project
        (self.work/'opencode.log').write_text(
            'INFO message="location services booted" directory='+str(project)+'-other\n'
            'ERROR NEVER_OTHER_LOCATION failure\n'
            'INFO message="location services booted" directory='+str(project)+' workspaceID=w\n'
            'ERROR FAKE own location failure\n')
        rows=self.capture()
        self.assertEqual([row['association'] for row in rows['log_lines']],['location-context'])
        self.assertNotIn('NEVER_OTHER_LOCATION',json.dumps(rows))

    def test_oversized_message_is_never_materialized(self):
        import opencode_diagnostic as diagnostic
        self.database((1,{'text':'x'*4096,'error':{'name':'FAKE_Error','message':'FAKE'}}))
        seen=[]
        real=sqlite3.connect
        class Cursor:
            def __init__(self,cursor):self.cursor=cursor
            def _see(self,rows):
                seen.extend(len(value) for row in rows for value in row if type(value) is str);return rows
            def fetchmany(self,*args):return self._see(self.cursor.fetchmany(*args))
            def fetchall(self):return self._see(self.cursor.fetchall())
            def fetchone(self):
                row=self.cursor.fetchone();self._see([row] if row else []);return row
            def __iter__(self):
                for row in self.cursor:self._see([row]);yield row
        class Connection:
            def __init__(self,*args,**kwargs):self.conn=real(*args,**kwargs)
            def __enter__(self):return self
            def __exit__(self,*args):return self.conn.__exit__(*args)
            def execute(self,*args):return Cursor(self.conn.execute(*args))
        with mock.patch.object(diagnostic.sqlite3,'connect',Connection), \
                mock.patch('opencode_runroot.NATIVE_BYTES',1024),self.assertRaisesRegex(safety.Blocked,'message bound'):
            self.capture()
        self.assertLess(max(seen,default=0),1024)

    def test_extracted_errors_have_an_aggregate_bound(self):
        part={'type':'tool','state':{'status':'error','error':{'name':'FAKE_ToolError','message':'FAKE'}}}
        self.database((1,{'content':[part]*2000}))
        with self.assertRaisesRegex(safety.Blocked,'error bound'):self.capture()

    def test_log_growth_after_size_check_is_not_read_unbounded(self):
        import opencode_runroot as runroot
        from opencode_diagnostic import DIAGNOSTIC_BYTES
        log=self.work/'opencode.log';log.write_text('INFO start\n')
        original=runroot._regular
        def grown(path):
            info=original(path)
            if path==log:
                with log.open('a') as stream:
                    stream.write(('x'*1023+'\n')*(16*DIAGNOSTIC_BYTES//1024))
                    stream.write('ERROR sessionID=ses_FAKE GROWN_AFTER_CHECK\n')
            return info
        with mock.patch.object(runroot,'_regular',grown):
            try:rows=self.capture()
            except safety.Blocked:return
        self.assertNotIn('GROWN_AFTER_CHECK',json.dumps(rows))

    def test_discovery_has_an_entry_bound(self):
        import opencode_diagnostic as diagnostic
        for index in range(8):(self.work/f'entry-{index}').mkdir()
        with mock.patch.object(diagnostic,'DIAGNOSTIC_ENTRIES',4), \
                self.assertRaisesRegex(safety.Blocked,'discovery bound'):
            self.capture()


class IntegrationSecrecyTests(unittest.TestCase):
    def test_unicode_escaped_synthetic_credential_is_found_in_decoded_values(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            secret='a1b2c3d4e5f60718293a4b5c6d7e8f90';d.secret_forms=[secret.encode()]
            escaped=''.join('\\u%04x'%ord(char) for char in secret)
            raw=('{"data":[{"id":"oclive-mock","connections":[{"value":"'+escaped+'"}]}]}').encode()
            self.assertNotIn(secret.encode(),raw)
            with self.assertRaisesRegex(safety.Blocked,'synthetic secret'):
                d._check_integration_secrecy(raw,json.loads(raw))
            d._check_integration_secrecy(b'{"data":[]}',{'data':[]})


class SpendingLatchTests(unittest.TestCase):
    def finish(self,d):
        with mock.patch.object(d,'stop',return_value={'proven':True}), \
             mock.patch('opencode_driver.safety.verify_binary'), \
             mock.patch.object(d,'secrecy_scan',return_value={'secret_absent':True,'payload_captures':0}):
            return d.finish()

    def test_stopped_guard_or_refused_provider_blocks_cleanup_after_closing_providers(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.guard.stopped=True
            with self.assertRaisesRegex(safety.Blocked,'spending control'):self.finish(d)
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            provider=mock.MagicMock();provider.receipt.return_value={'requests':1,'admitted_requests':0,
                'refused_requests':1,'unreleased_responses':0,'connection_limit_reached':False,
                'connection_limit':8,'admission_blocked':True}
            d.mock_providers={'FAKE':provider}
            with self.assertRaisesRegex(safety.Blocked,'spending control'):self.finish(d)
            provider.__exit__.assert_called_once()


class EventRetentionTests(unittest.TestCase):
    def test_failed_retention_is_idempotent_and_never_skips_daemon_stop(self):
        import opencode_daemon_tests as daemontests
        with tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
            d,_=daemontests.DaemonGenerationTests().fixture(root)
            d.proc.alive.return_value=True;d._lock_free=mock.Mock(return_value=True)
            d._pgrep_clear=mock.Mock(return_value=True)
            def via(args):
                if args==['daemon','stop','--force']:d.proc.alive.return_value=False
                return {}
            d.via=mock.Mock(side_effect=via)
            error=safety.Blocked('owned event capture bound');error.event_bound={'line_bytes':1}
            d._stash_event_failure(error,'Blocked')
            written=[];original=d._record
            def record(label,value):
                if label=='native-event-bound':
                    written.append(label);raise OSError(errno.ENOSPC,'FAKE no space')
                return original(label,value)
            d._record=record
            with self.assertRaisesRegex(safety.Blocked,'event failure retention'):d.stop()
            d.via.assert_any_call(['daemon','stop','--force'])
            self.assertIs(d._retain_event_failure(),error)
            self.assertEqual(written,['native-event-bound'])


class LayoutTests(unittest.TestCase):
    def driver(self,folder):
        d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
        for directory in (d.home,d.state,d.helpers):safety.private_directory(directory)
        d.namespace,d.namespace_env=d._create_namespace()
        return d

    def test_namespace_project_is_separate_from_every_private_storage_root(self):
        with tempfile.TemporaryDirectory() as folder:
            d=self.driver(folder)
            project=d._create_namespace_project()
            self.assertEqual(project,d.namespace_project)
            self.assertTrue((project/'.git').is_dir())
            for root in (d.state,d.namespace,d.home,d.runtime,*map(Path,d.namespace_env.values())):
                self.assertFalse(project.is_relative_to(root) or root.is_relative_to(project),root)
            for overlap in (d.namespace,d.namespace/'project',Path(d.namespace_env['HOME'])/'p',
                            d.state/'project',d.home/'project',d.work):
                with self.subTest(overlap=overlap),self.assertRaisesRegex(safety.Blocked,'private storage'):
                    d._check_project_separation(overlap)

    def test_namespace_is_never_a_turn_cwd(self):
        with tempfile.TemporaryDirectory() as folder:
            d=self.driver(folder);d._create_namespace_project()
            d.build=mock.Mock(side_effect=AssertionError('FAKE build reached'))
            with self.assertRaisesRegex(safety.Blocked,'outside initialized private fixtures'):
                d.start('release',d.namespace)
            provider=mock.Mock(requests=0,admission_blocked=False,model_matches=True)
            d._namespace_bootstraps[d.namespace_project]=({},provider)
            d._bootstrap_static=mock.Mock(return_value={});d._verify=mock.Mock()
            d._http=mock.Mock();d._observe_vendor=mock.Mock();d._host_record=mock.Mock(return_value={'owned':True})
            d._vendor_sid=mock.Mock(return_value='ses_FAKE');d.spending_check=mock.Mock()
            commands=[]
            def via(args):
                commands.append(args)
                if args[0]=='spawn':return {'session_id':'s_FAKE','turn':'s_FAKE/1'}
                return {'state':'completed'} if args[0]=='wait' else {}
            d.via=mock.Mock(side_effect=via)
            with self.assertRaises(safety.Blocked):d._bootstrap_vendor()
            self.assertEqual(d.mock_providers,{})  # The cached namespace-project provider was reused.
            spawn=next(args for args in commands if args[0]=='spawn')
            self.assertEqual(spawn[spawn.index('--cwd')+1],str(d.namespace_project))


if __name__=='__main__':
    unittest.main()
