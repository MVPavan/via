"""Lock ownership, seeded connection and actual ask regressions (§13)."""
import errno
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import opencode_cases as cases
import opencode_driver as runtime
import opencode_safety as safety
import opencode_lockscan_tests as locktests
import opencode_cases_tests as casetests


class Round44Tests(unittest.TestCase):
    def test_event_bound_failure_retains_byte_counts_from_the_main_thread(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            error=safety.Blocked('owned event capture bound')
            error.event_bound={'line_bytes':1048577,'frame_bytes':1048692,
                'prior_data_bytes':0,'complete_line':False,'total_bytes':1048692}
            d._stash_event_failure(error,'Blocked')
            self.assertFalse(list(d.evidence.glob('*native-event-bound.json')))
            self.assertIs(d._retain_event_failure(),error)
            self.assertEqual(json.loads(next(d.evidence.glob('*native-event-bound.json')).read_text()),error.event_bound)

    def test_bootstrap_failure_retains_only_terminal_state_and_fixed_class(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.namespace=Path(folder)/'namespace';d.namespace.mkdir()
            d.namespace_project=Path(folder)/'project';d.namespace_project.mkdir()
            provider=mock.Mock(requests=0,admission_blocked=False,model_matches=True)
            d._namespace_bootstraps[d.namespace_project]=({},provider)
            d._bootstrap_static=mock.Mock(return_value={});d._verify=mock.Mock()
            d._http=mock.Mock();d._observe_vendor=mock.Mock();d._host_record=mock.Mock(return_value={'owned':True})
            d._vendor_sid=mock.Mock(return_value='ses_FAKE');d.spending_check=mock.Mock()
            def via(args):
                return {'session_id':'s_FAKE','turn':'s_FAKE/1'} if args[0]=='spawn' else {
                    'state':'unknown','failure':{'class':'overflow','message':'NEVER_RETAIN_FAKE'}} if args[0]=='wait' else {}
            d.via=mock.Mock(side_effect=via)
            with self.assertRaisesRegex(safety.Blocked,'bootstrap mock turn did not complete'):
                d._bootstrap_vendor()
            row=json.loads(next(d.evidence.glob('*bootstrap.json')).read_text())
            self.assertEqual(row.get('terminal_state'),'unknown')
            self.assertEqual(row.get('terminal_failure_class'),'overflow')
            self.assertNotIn('NEVER_RETAIN_FAKE',json.dumps(row))

    def test_large_mock_message_is_projected_without_retaining_its_content(self):
        import sqlite3,json
        from opencode_diagnostic import capture,DIAGNOSTIC_BYTES
        from opencode_ownership import OwnershipRegistry
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);work=root/'private';work.mkdir();evidence=root/'evidence';evidence.mkdir()
            owned=OwnershipRegistry(evidence);owned.register(work,'vendor-private')
            with sqlite3.connect(work/'opencode.db') as db:
                db.execute('CREATE TABLE session_message(session_id TEXT,seq INT,data TEXT)')
                db.execute('INSERT INTO session_message VALUES(?,?,?)',('ses_FAKE',1,
                    json.dumps({'text':'x'*DIAGNOSTIC_BYTES,'error':{'name':'FAKE_Error','message':'FAKE error'}})))
            row=capture(work,owned,{'ses_FAKE':{'model':safety.MOCK_IDENTITY}})
            self.assertEqual(row['sessions'][0]['errors'][0]['name'],'FAKE_Error')
            self.assertLess(len(json.dumps(row)),1024)

    def test_overlapping_inventory_roots_count_unique_entries_and_keep_all_checks(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);child=root/'namespace';child.mkdir()
            (child/'ordinary').write_bytes(b'FAKE')
            (root/'ordinary').write_bytes(b'FAKE')
            inventory=safety.Inventory([child,root,child],max_entries=3)
            (root/'ordinary').write_bytes(b'\x7fELF FAKE')
            with self.assertRaisesRegex(safety.Blocked,'new or changed binary'):
                inventory.check()

    def test_process_vanished_before_uid_filter_is_a_snapshot_absence(self):
        helper=locktests.LockScopeTests();self.addCleanup(helper.doCleanups)
        d,anchor,_=helper.fixture_scan(readable=True)
        original=Path.stat
        def stat(path,*args,**kwargs):
            if path==d.proc.root/'11':raise FileNotFoundError(errno.ENOENT,'FAKE',str(path))
            return original(path,*args,**kwargs)
        with mock.patch.object(Path,'stat',stat):
            self.assertEqual(d._lock_holders(anchor),[anchor.report()])

    def test_pid_reused_during_scan_is_retried_with_new_identity(self):
        helper=locktests.LockScopeTests();self.addCleanup(helper.doCleanups)
        d,anchor,_=helper.fixture_scan(readable=True)
        replaced=safety.Identity(11,21);reads=[]
        oldread=d.proc.read.side_effect;changed=[False]
        def read(identity,name,**kwargs):
            reads.append(identity)
            if identity.pid==11 and identity!=replaced:
                changed[0]=True
                raise safety.Blocked('owned process identity changed')
            return oldread(identity,name,**kwargs)
        d.proc.read.side_effect=read
        d.proc.stat.side_effect=lambda pid:{'pid':pid,'start_ticks':100 if pid==10 else (21 if changed[0] else 20),'ppid':1}
        self.assertEqual(d._lock_holders(anchor),[anchor.report()])
        self.assertIn(replaced,reads)

    def test_seed_connection_is_bound_to_empty_baseline_and_exact_mock_credential(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            empty={'data':[{'id':'oclive-mock','connections':[]}]}
            expected={'data':[{'id':'oclive-mock','connections':[{
                'id':'con_FAKE','type':'credential','label':'VIA synthetic fixture'}]}]}
            d._check_seed_connections(empty,seeded=False)
            d._check_seed_connections(expected,seeded=True)
            for reply,seeded in [(expected,False),(empty,True),
                ({'data':[{'id':'foreign','connections':expected['data'][0]['connections']}]},True),
                ({'data':[{'id':'oclive-mock','connections':[{'id':'con_FAKE','type':'api','label':'VIA synthetic fixture'}]}]},True),
                ({'data':[{'id':'oclive-mock','connections':[{'id':'con_FAKE','type':'credential','label':'other'}]}]},True),
                ({'data':expected['data']*2},True)]:
                with self.subTest(reply=reply,seeded=seeded),self.assertRaises(safety.Blocked):
                    d._check_seed_connections(reply,seeded=seeded)

    def test_never_ask_cannot_pass_without_an_actual_rejected_ask(self):
        d=casetests.CasesTests().never_ask_driver()
        d.replies['permission_observations']['attempts']=[{'callID':'call_FAKE','child_session':'ses_FAKE',
            'deny_rule':True,'asked':False,'disposition':'action.denied'}]
        result=cases.run_case(d,'never_ask',cases.case_never_ask)
        self.assertEqual(result['result'],'fail')
        self.assertTrue(any(row['check']=='actual child ask rejected by VIA' and row['result']=='fail'
                            for row in result['checks']))

    def test_policy_point_set_and_published_ceilings_match(self):
        import opencode_mock_policy as policy
        self.assertEqual(set(policy.ANCHOR_FRESH_POINTS),set(cases.L14_POINTS)-{'long_run','disposal'})
        self.assertEqual(cases.L_DISPOSITIONS['L4'][2],policy.HOSTILE_PHASE_REQUESTS)
        self.assertEqual(cases.L_DISPOSITIONS['L14'][2],policy.ANCHOR_PHASE_REQUESTS)
