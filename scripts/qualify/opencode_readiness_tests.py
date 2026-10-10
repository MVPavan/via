"""Delayed owned-state regressions for the §13 qualification startup sweep."""
import json
import sqlite3
import tempfile
import time
import threading
from types import SimpleNamespace
import unittest
from pathlib import Path
from unittest import mock

import opencode_driver as transport
from opencode_safety import Blocked, Identity


class ReadinessTests(unittest.TestCase):
    def test_public_helper_waits_past_thirty_seconds_and_mock_does_not(self):
        import opencode_safety as safety
        folder=Path(self.root.name)/'late-helper';folder.mkdir()
        d=self.driver;d._helper_folder=mock.Mock(return_value=folder)
        d._descends_from=mock.Mock(return_value=True);d.phase_deadline=200
        for model in (safety.MOCK_IDENTITY,safety.FREE_IDENTITY):
            with self.subTest(model=model):
                self.clock[0]=0;(folder/'tool.json').unlink(missing_ok=True)
                d.last_request['model']=model
                def advance(seconds):
                    self.clock[0]+=seconds
                    if self.clock[0]>=40:
                        (folder/'tool.json').write_text(json.dumps({
                            'pid':14,'start_ticks':4,'ready':True,'kind':'tool'}))
                with mock.patch.object(transport.time,'sleep',side_effect=advance):
                    if model==safety.MOCK_IDENTITY:
                        with self.assertRaisesRegex(Blocked,'helper barrier not reached'):
                            d._operation('helper_barrier',{'helper':'tool','session':'s_owned'})
                        self.assertAlmostEqual(self.clock[0],30)
                    else:
                        self.assertTrue(d._operation('helper_barrier',{
                            'helper':'tool','session':'s_owned'})['owned'])
                        self.assertGreaterEqual(self.clock[0],40)

    def test_public_helper_bound_still_clips_to_phase_and_checks_identity(self):
        import opencode_safety as safety
        d=self.driver;d.last_request['model']=safety.FREE_IDENTITY
        d.phase_deadline=45;d._helper_folder=mock.Mock(return_value=Path(self.root.name)/'missing')
        with self.assertRaisesRegex(Blocked,'helper barrier not reached'):
            d._operation('helper_barrier',{'helper':'tool','session':'s_owned'})
        self.assertEqual(self.clock[0],45)
        self.clock[0]=0;d.proc.verify.side_effect=Blocked('owned process identity changed')
        with self.assertRaisesRegex(Blocked,'identity changed'):
            d._operation('helper_barrier',{'helper':'tool','session':'s_owned'})
        self.assertEqual(self.clock[0],0)

    def test_public_timeline_is_closed_correlated_and_survives_capture_reset(self):
        import opencode_safety as safety
        d=self.driver;generation=d.vendor_identity
        receipt={'session_id':'s_owned','turn':'s_owned/1'}
        d._timeline_submit(receipt,safety.FREE_IDENTITY,0,generation)
        sid='ses_FAKE_owned';d._timeline_bind('s_owned',sid,turn=1)
        def event(kind,at,**fields):
            return {'type':kind,'_observed_at':at,'data':{'sessionID':sid,**fields}}
        d._timeline_event(event('session.step.started',.5,assistantMessageID='FAKE-before-delivery'),generation)
        d._timeline_event(event('session.inbox.delivered',1,inboxID=transport.input_id('s_owned',1)),generation)
        d._timeline_event(event('session.step.started',2,assistantMessageID='FAKE-private'),Identity(99,9))
        d._timeline_event(event('session.step.started',2,assistantMessageID='FAKE-private'),generation)
        d._timeline_event(event('session.tool.input.started',3,assistantMessageID='FAKE-private',
            id='FAKE-private',name='shell',text='FAKE-private'),generation)
        self.clock[0]=4;d._timeline_ready('s_owned',120)
        d._timeline_event(event('session.execution.interrupted',5,reason='FAKE-private'),generation)
        d.events=[];d.vendor_identity=Identity(23,5)
        d._timeline_submit({'session_id':'s_mock','turn':'s_mock/1'},safety.MOCK_IDENTITY,6,d.vendor_identity)
        d._timeline_flush('model-timelines')
        rows=d._record.call_args.args[1]['turns']
        self.assertEqual(len(rows),1)
        self.assertEqual(rows[0]['elapsed_ms'],{'first_assistant':2000,'first_tool':3000,
            'readiness':4000,'terminal':5000})
        self.assertEqual(rows[0]['sessionID'],sid)
        self.assertEqual(rows[0]['terminal'],'interrupted')
        self.assertNotIn('FAKE-private',json.dumps(rows))

    def test_timeline_replays_buffered_delivery_but_excludes_foreign_work(self):
        import opencode_safety as safety
        d=self.driver;sid='ses_FAKE_owned';generation=d.vendor_identity
        d._timeline_submit({'session_id':'s_owned','turn':'s_owned/1'},safety.FREE_IDENTITY,0,generation)
        def event(kind,at,**fields):
            return {'type':kind,'_observed_at':at,'data':{'sessionID':sid,**fields}}
        d.events=[event('session.inbox.delivered',1,inboxID=transport.input_id('s_owned',1)),
            event('session.step.started',2,assistantMessageID='FAKE-private'),
            event('session.inbox.delivered',3,inboxID='msg_FAKE_foreign'),
            event('session.tool.input.started',4,assistantMessageID='FAKE-private'),
            event('session.execution.succeeded',5)]
        d._timeline_bind('s_owned',sid,turn=1);d._timeline_flush('model-timelines')
        row=d._record.call_args.args[1]['turns'][0]
        self.assertEqual(row['elapsed_ms'],{'first_assistant':2000,'first_tool':None,
            'readiness':None,'terminal':None})
        self.assertTrue(row['delivery_observed'])
        self.assertNotIn('FAKE-private',json.dumps(row))

    def test_cli_spawn_resume_and_wait_register_each_public_turn(self):
        from opencode_c1_tests import envelope
        from opencode_safety_tests import fake_cli_generation
        import opencode_safety as safety
        d=self.driver;fake_cli_generation(d);d._binary=Path('FAKE-via')
        d.last_model=safety.FREE_IDENTITY;d.spending_check=mock.Mock();d._admit_model=mock.Mock()
        d.execute=mock.Mock(return_value=(0,json.dumps({'session_id':'s_FAKE','turn':'s_FAKE/1',
            'handle':'h_FAKE_timing','effective':{'effort':None,'max_steps':None}}).encode(),b''))
        d.via(['spawn','--background','--prompt','FAKE-private'])
        result=envelope();d.execute.return_value=(0,json.dumps(result).encode(),b'')
        d.via(['wait','s_FAKE/1','--timeout-ms','0'])
        d.execute.return_value=(0,json.dumps({'turn':'s_FAKE/2',
            'effective':{'effort':None,'max_steps':None}}).encode(),b'')
        d.via(['resume','s_FAKE','--prompt','FAKE-private'])
        d._timeline_flush('model-timelines')
        rows=d._record.call_args.args[1]['turns']
        self.assertEqual([row['turn'] for row in rows],[1,2])
        self.assertEqual(rows[0]['sessionID'],'ses_FAKE')
        self.assertNotIn('FAKE-private',json.dumps(rows))
        self.assertNotIn('h_FAKE_timing',json.dumps(rows))

    def host_handover(self):
        d=self.driver; d.state.mkdir(mode=0o700,exist_ok=True)
        path=d.state/'store.sqlite3'
        with sqlite3.connect(path) as db:
            db.execute('DROP TABLE IF EXISTS anchors')
            db.execute('CREATE TABLE anchors (generation,pid,start_ticks,vendor_pid,phase,owner_server,absence_time)')
            db.executemany('INSERT INTO anchors VALUES (?,?,?,?,?,?,?)',[
                ('own',12,2,13,'arm_intent','owned-server',None),
                ('next',22,4,23,'arm_intent','next-server',None)])
        if '_host_record' in d.__dict__: del d._host_record
        path.chmod(0o600)
        d._verified_host_record={**self.record,'phase':'arm_intent','state_root':d.state,'vendor':d.vendor_identity}
        d.proc.alive.side_effect=lambda identity: identity not in {Identity(12,2),Identity(13,3)}
        d.proc.stat.side_effect=lambda pid: {'pid':pid,'start_ticks':4 if pid==22 else 5,
            'ppid':11 if pid==22 else 22}
        return path

    def test_owned_dead_predecessor_waits_for_absence_commit(self):
        def commit(seconds):
            self.clock[0]+=seconds
            with sqlite3.connect(path) as db:
                db.execute("UPDATE anchors SET absence_time='proved' WHERE generation='own'")
                db.execute("UPDATE anchors SET pid=22,start_ticks=4,vendor_pid=23,phase='arm_intent' WHERE generation='next'")
        for stage in ('intent','identified','arm_intent'):
            with self.subTest(stage=stage):
                path=self.host_handover()
                with sqlite3.connect(path) as db:
                    db.execute("UPDATE anchors SET pid=?,start_ticks=?,vendor_pid=?,phase=? WHERE generation='next'",
                        (None if stage=='intent' else 22,None if stage=='intent' else 4,
                         23 if stage=='arm_intent' else None,stage))
                with mock.patch.object(transport.time,'sleep',side_effect=commit):
                    result=self.driver._host_record(allow_absent=True)
                self.assertEqual(result['generation'],'next')
                self.assertEqual(result['anchor'],Identity(22,4))
        self.assertGreater(self.clock[0],0)
        self.assertIn(mock.call(self.driver.daemon),self.driver.proc.verify.call_args_list)
        for fault,reason in (('unknown','owned Host generation ambiguous'),
                             ('live','owned Host generation ambiguous'),
                             ('uncertain','owned Host predecessor identity unverifiable'),
                             ('unsafe','owned Host handover Store unsafe'),
                             ('ancestry','owned Host successor ancestry changed')):
            with self.subTest(fault=fault):
                path=self.host_handover()
                if fault=='unknown': self.driver._verified_host_record=None
                elif fault=='live': self.driver.proc.alive.side_effect=None; self.driver.proc.alive.return_value=True
                elif fault=='uncertain': self.driver.proc.alive.side_effect=None; self.driver.proc.alive.return_value=None
                elif fault=='unsafe': path.chmod(0o644)
                elif fault=='ancestry': self.driver.proc.stat.side_effect=lambda pid:{'start_ticks':4,'ppid':999}
                before=self.clock[0]
                with self.assertRaisesRegex(Blocked,reason): self.driver._host_record(allow_absent=True)
                self.assertEqual(self.clock[0],before)

    def test_owned_handover_cannot_extend_phase_deadline(self):
        for state in ('gone','going'):
            with self.subTest(state=state):
                self.host_handover(); self.clock[0]=0; self.driver.phase_deadline=.03
                if state=='going':
                    self.driver._killed_anchor=self.driver.anchor
                    self.driver.proc.alive.side_effect=None; self.driver.proc.alive.return_value=True
                with self.assertRaisesRegex(Blocked,'owned Host predecessor absence deadline'):
                    self.driver._host_record(allow_absent=True)
                self.assertGreaterEqual(self.clock[0],.03)
                self.assertLessEqual(self.clock[0],.04)
        path=self.host_handover(); self.clock[0]=0; self.driver.phase_deadline=.03
        def changed(seconds):
            self.clock[0]+=seconds
            with sqlite3.connect(path) as db:
                db.execute("UPDATE anchors SET generation='other' WHERE generation='next'")
        with mock.patch.object(transport.time,'sleep',side_effect=changed), \
             self.assertRaisesRegex(Blocked,'owned Host handover successor changed'):
            self.driver._host_record(allow_absent=True)

    def test_native_string_session_with_false_verification_blocks_immediately(self):
        self.driver.handles['s_owned']='fake-memory-only-handle'
        self.driver.via=mock.Mock(return_value={'state':'active',
            'vendor_identity_verified':False,'vendor_session_id':'ses_owned'})
        with self.assertRaisesRegex(Blocked,'native session identity unverified'):
            self.driver._vendor_sid('s_owned')
        self.driver.via.assert_called_once()
        self.assertEqual(self.clock[0],0)

    def publication(self,die,killed=False):
        d=self.driver; current=Identity(13,4)
        if die: d.vendor_identity=current
        provider=SimpleNamespace(); threads=[]; errors=[]; verified=threading.Event()
        d._lifecycle_schema={}; d.stop=mock.Mock(); d.start=mock.Mock()
        d.fixture=mock.Mock(return_value=Path(self.root.name))
        d.mock_providers={'l14-1-publication':provider}
        d._mock_request_admit=mock.Mock()
        d._await_owned=mock.Mock(side_effect=lambda read,*args:read())
        def verify(identity):
            if identity!=current: raise Blocked('stale publication identity')
            verified.set()
        d.proc.verify.side_effect=verify; d.proc.alive.return_value=True
        def submit(_):
            def request():
                try: provider.admit_request('fixture-free')
                except BaseException as error: errors.append(error)
            thread=threading.Thread(target=request); threads.append(thread); thread.start()
            self.assertTrue(d._publication_entered.wait(1))
            # The request is already in its hold while ensure_vendor is pending.
            threading.Event().wait(.03)
            return {'session_id':'s_owned','turn':'s_owned/1'}
        def ensure():
            if not die: self.assertEqual(d.proc.verify.call_count,0)
            d.vendor_identity=current
        d.via=mock.Mock(side_effect=submit); d.ensure_vendor=mock.Mock(side_effect=ensure)
        try:
            d._prepare_lifecycle('publication')
            self.assertTrue(verified.wait(1))
            if killed:
                # L14 SIGKILLed this generation's anchor; its dying vendor is
                # neither verifiable nor yet proven gone.
                d._killed_anchor=d.anchor
                d.proc.verify.side_effect=Blocked('FAKE dying vendor')
                d.proc.gone=mock.Mock(return_value=False)
                threads[0].join(1)
                self.assertFalse(threads[0].is_alive())
            if die:
                d.proc.alive.return_value=False
                threads[0].join(1)
                self.assertFalse(threads[0].is_alive())
                self.assertFalse(d._publication_release.is_set())
            self.assertFalse(errors)
        finally:
            d._publication_release.set()
            for thread in threads: thread.join(1)
        self.assertTrue(all(not thread.is_alive() for thread in threads))
        self.assertTrue(all(call.args==(current,) for call in d.proc.verify.call_args_list))

    def test_publication_waits_for_current_generation_verification(self):
        self.publication(False)

    def test_publication_intentional_vendor_death_ends_hold_without_refusal(self):
        self.publication(True)

    def test_publication_hold_ends_without_refusal_once_its_anchor_is_killed(self):
        self.publication(False,killed=True)

    def setUp(self):
        self.root = tempfile.TemporaryDirectory(prefix='via-ocreadiness-')
        self.addCleanup(self.root.cleanup)
        self.driver = transport.Driver('release', 'fp', 'pin',
                                       Path(self.root.name) / 'evidence')
        self.driver.proc = mock.Mock()
        self.driver.daemon = Identity(11, 1)
        self.driver.anchor = Identity(12, 2)
        self.driver.vendor_identity = Identity(13, 3)
        self.record = {'generation': 'own', 'anchor': self.driver.anchor,
                       'vendor_pid': 13, 'server_id': 'owned-server'}
        self.driver._host_record = mock.Mock(return_value=self.record)
        self.driver._record = mock.Mock()
        self.driver.last_request = {'receipt': {'session_id': 's_owned', 'turn': 's_owned/1'}}
        self.clock = [0.0]
        self.monotonic = mock.patch.object(transport.time, 'monotonic',
                                          side_effect=lambda: self.clock[0])
        self.sleep = mock.patch.object(transport.time, 'sleep',
                                    side_effect=lambda seconds: self.clock.__setitem__(
                                        0, self.clock[0] + seconds))
        self.monotonic.start(); self.sleep.start()
        self.addCleanup(self.monotonic.stop); self.addCleanup(self.sleep.stop)

    def events(self, batches):
        batches = iter(batches); last = {'complete': True, 'events': []}
        def observe(kind, *_):
            nonlocal last
            if kind != 'native_events': raise AssertionError('unexpected observation')
            last = next(batches, last)
            return last
        self.driver.observe = mock.Mock(side_effect=observe)

    def delivered(self):
        return {'seq': 1, 'type': 'session.inbox.delivered', 'data': {
            'sessionID': 'ses_owned', 'inboxID': transport.input_id('s_owned', 1)}}

    def terminal(self):
        return {'seq': 2, 'type': 'session.execution.succeeded', 'data': {
            'sessionID': 'ses_owned'}}

    def test_turn_waits_for_delayed_sse_delivery_and_terminal(self):
        self.events([{'events': []}, {'events': [self.delivered()]},
                     {'events': [self.delivered(), self.terminal()]}])
        self.driver._http = mock.Mock()
        self.driver._http.request.return_value = (200, json.dumps({'data': {
            'model': {'providerID': 'oclive-mock', 'id': 'fixture-free'}}}).encode())
        self.driver.provider_endpoints = {'oclive-mock': 'loopback'}
        reply = self.driver._operation('turn_identity', {'session': 's_owned',
                     'envelope': {'vendor_session_id': 'ses_owned', 'turn': 1}})
        self.assertTrue(reply['owned_terminal'])
        self.assertGreaterEqual(self.driver.proc.verify.call_count, 6)

    def test_native_terminal_waits_for_observer_lag(self):
        self.driver._vendor_sid = mock.Mock(return_value='ses_owned')
        self.events([{'events': []}, {'events': [self.delivered(), self.terminal()]}])
        result = self.driver._operation('native_terminal', {'session': 's_owned'})
        self.assertEqual(result['type'], 'session.execution.succeeded')

    def test_accepted_input_waits_for_enqueued_event(self):
        enqueue = {'seq': 1, 'type': 'session.inbox.enqueued', 'data': {'inboxID': 'msg_via_test'}}
        self.events([{'events': []}, {'events': [enqueue]}])
        self.assertEqual(self.driver._observe_accepted_input('ses_owned'), 'msg_via_test')

    def test_observation_wait_hard_stops_when_generation_changes(self):
        self.driver._vendor_sid = mock.Mock(return_value='ses_owned')
        self.events([{'events': []}, {'events': [self.terminal()]}])
        self.driver._host_record.side_effect = [self.record, self.record,
                                                {**self.record, 'generation': 'foreign'}]
        with self.assertRaisesRegex(Blocked, 'generation changed'):
            self.driver._operation('native_terminal', {'session': 's_owned'})

    def test_observation_wait_never_extends_phase_deadline(self):
        self.driver.phase_deadline = .03
        self.driver._vendor_sid = mock.Mock(return_value='ses_owned')
        self.events([{'events': []}])
        with self.assertRaisesRegex(Blocked, 'native terminal absent'):
            self.driver._operation('native_terminal', {'session': 's_owned'})
        self.assertGreaterEqual(self.clock[0], .03)
        self.assertLessEqual(self.clock[0], .04)

    def test_helper_wait_reverifies_parent_and_rejects_foreign_ancestry(self):
        folder = Path(self.root.name) / 'observations'; folder.mkdir()
        self.driver._helper_folder = mock.Mock(return_value=folder)
        self.driver._descends_from = mock.Mock(return_value=False)
        (folder / 'tool.json').write_text(json.dumps({'pid': 14, 'start_ticks': 4,
                                                    'ready': True, 'kind': 'tool'}))
        with self.assertRaisesRegex(Blocked, 'helper.*owned generation'):
            self.driver._operation('helper_barrier', {'helper': 'tool'})
        self.assertNotIn(Identity(14, 4), self.driver.identities)

    def test_helper_timeout_retains_presence_and_not_ready_observations(self):
        for mode in ('absent-folder','absent-file','not-ready','vanished-not-ready'):
            with self.subTest(mode=mode):
                self.clock[0]=0;self.driver.phase_deadline=.03
                folder=Path(self.root.name)/mode
                self.driver._helper_folder=mock.Mock(return_value=folder)
                self.driver._descends_from=mock.Mock(return_value=True)
                if mode!='absent-folder':folder.mkdir()
                readiness=folder/'tool.json'
                if mode in {'not-ready','vanished-not-ready'}:
                    readiness.write_text(json.dumps({'pid':14,'start_ticks':4,'ready':False,'kind':'tool'}))
                def wait(seconds):
                    self.clock[0]+=seconds
                    if mode=='vanished-not-ready':readiness.unlink(missing_ok=True)
                self.driver._record.reset_mock()
                with mock.patch.object(transport.time,'sleep',side_effect=wait):
                    with self.assertRaisesRegex(Blocked,'^helper barrier not reached$'):
                        self.driver._operation('helper_barrier',{'helper':'tool'})
                records=[call.args[1] for call in self.driver._record.call_args_list
                         if call.args[0]=='helper-readiness']
                self.assertEqual(len(records),1,'helper timeout lacks diagnostic evidence')
                self.assertEqual(records[0],{'timed_out':True,'bound_seconds':30,'folder_exists':mode!='absent-folder',
                    'readiness_file_exists':mode=='not-ready',
                    'readiness_seen':mode in {'not-ready','vanished-not-ready'},
                    'not_ready_seen':mode in {'not-ready','vanished-not-ready'},'failure':None})

    def test_helper_timeout_retention_failure_preserves_reason_and_flags_uncertainty(self):
        self.driver.phase_deadline=.03
        self.driver._helper_folder=mock.Mock(return_value=Path(self.root.name)/'missing')
        self.driver._record.side_effect=Blocked('FAKE evidence sink refused')
        with self.assertRaisesRegex(Blocked,'^helper barrier not reached$') as raised:
            self.driver._operation('helper_barrier',{'helper':'tool'})
        from opencode_safety import blocking_record
        self.assertTrue(blocking_record(raised.exception).get('helper_readiness_retention_failed'))

    def test_helper_wait_rejects_changed_parent_before_readiness(self):
        folder = Path(self.root.name) / 'observations'; folder.mkdir()
        self.driver._helper_folder = mock.Mock(return_value=folder)
        self.driver.proc.verify.side_effect = Blocked('owned process identity changed')
        with self.assertRaisesRegex(Blocked, 'identity changed'):
            self.driver._operation('helper_barrier', {'helper': 'tool'})
        self.assertEqual(self.clock[0], 0)

    def test_seam_wait_reverifies_daemon_while_ack_is_absent(self):
        folder = Path(self.root.name) / 'failpoints'; folder.mkdir()
        self.driver.env['VIA_FAILPOINT_DIR'] = str(folder)
        self.driver._armed['point'] = 1
        self.driver.proc.verify.side_effect = Blocked('owned process identity changed')
        with self.assertRaisesRegex(Blocked, 'identity changed'):
            self.driver.seam_wait('point', timeout=.03)
        self.assertEqual(self.clock[0], 0)

    def test_host_partial_row_cannot_hide_ambiguous_generation(self):
        self.driver.state.mkdir()
        with sqlite3.connect(self.driver.state / 'store.sqlite3') as db:
            db.execute('CREATE TABLE anchors (generation,pid,start_ticks,vendor_pid,phase,owner_server,absence_time)')
            db.executemany('INSERT INTO anchors VALUES (?,?,?,?,?,?,?)', [
                ('a', 12, 2, 13, 'arm_intent', 'server-a', None),
                ('b', None, None, None, 'identified', 'server-b', None)])
        del self.driver._host_record
        with self.assertRaisesRegex(Blocked, 'ambiguous'):
            self.driver._host_record(allow_absent=True)

    def test_direct_seed_ambiguity_never_retries(self):
        self.driver.daemon = None
        self.driver.namespace = Path(self.root.name)
        self.driver.namespace_env = {}; self.driver.env = {'PATH': '/usr/bin:/bin'}
        self.driver._locks_free = mock.Mock(return_value=True)
        self.driver._generated_vendor_config = mock.Mock(return_value={})
        self.driver._prepare_l11_mock_provider = mock.Mock()
        self.driver._pgrep_clear = mock.Mock(return_value=True)
        self.driver.vault.read_once = mock.Mock(return_value=b'fixture-password')
        self.driver.proc.stat.return_value = {'pid': 31, 'start_ticks': 3}
        self.driver.proc.gone.return_value = True
        self.driver.proc.listener.side_effect = [Blocked('owned listener absent or ambiguous'),
                                                 'http://127.0.0.1:1234']
        process = mock.Mock(pid=31); process.poll.return_value = None
        with mock.patch.object(transport.safety, 'verify_binary'), \
             mock.patch.object(transport.subprocess, 'Popen', return_value=process), \
             mock.patch.object(transport.safety, 'OwnedHTTP') as http:
            http.return_value.request.return_value = (200, b'{"pid":31,"version":"wrong"}')
            with self.assertRaisesRegex(Blocked, 'owned listener absent or ambiguous'):
                self.driver.direct_seed(self.driver.namespace, {'schema_checked': True})
        self.driver.proc.listener.assert_called_once()

    def test_unverifiable_disappearance_blocks_without_waiting(self):
        self.driver.project = Path(self.root.name)
        self.driver.execute = mock.Mock(return_value=(1, b'', b''))
        self.driver.helper_ledger = {Identity(14, 4): {'kind': 'tool'}}
        self.driver.proc.alive.return_value = None
        self.driver.proc.gone.return_value = False
        with self.assertRaisesRegex(Blocked, 'identity unverifiable'):
            self.driver._operation('helper_stop_proof', {'helper': 'tool'})
        self.assertEqual(self.clock[0], 0)

    def test_native_session_id_waits_for_background_creation(self):
        self.driver.handles['s_owned'] = 'fake-memory-only-handle'
        self.driver.via = mock.Mock(side_effect=[
            {'state': 'active', 'admission': 'open', 'vendor_identity_verified': False, 'vendor_session_id': None},
            {'state': 'active', 'vendor_identity_verified': True, 'vendor_session_id': 'ses_owned'}])
        self.assertEqual(self.driver._vendor_sid('s_owned'), 'ses_owned')
        self.assertEqual(self.driver.via.call_count, 2)

    def test_native_session_id_waits_on_c1_active_session_not_turn_state(self):
        self.driver.handles['s_owned']='fake-memory-only-handle'
        self.driver.via=mock.Mock(side_effect=[
            {'state':'active','admission':'open','active_turn':{'state':'running','phase':'submitting'},
             'vendor_identity_verified':False,'vendor_session_id':None},
            {'state':'active','active_turn':{'state':'running','phase':'accepted'},
             'vendor_identity_verified':True,'vendor_session_id':'ses_owned'}])
        self.assertEqual(self.driver._vendor_sid('s_owned'),'ses_owned')
        self.assertEqual(self.driver.via.call_count,2)
        self.assertGreater(self.clock[0],0)

    def test_memory_sample_counts_c1_active_sessions(self):
        d=self.driver;d.ensure_vendor=mock.Mock()
        d.proc.read.return_value=b'VmRSS: 64 kB\n'
        d.handles={'s_owned':'FAKE-memory-handle','s_idle':'FAKE-memory-handle'}
        d.via=mock.Mock(side_effect=[{'state':'active'},{'state':'idle'}])
        sample=d.observe('memory_sample',{'sessions':['s_owned','s_idle']})
        self.assertEqual(sample['active_sessions'],1)
        self.assertEqual(sample['sessions'],2)

    def test_lifecycle_accepts_c1_active_session_with_accepted_turn(self):
        d=self.driver;d._prepare_lifecycle=mock.Mock();d.ensure_vendor=mock.Mock()
        d._request=mock.Mock(return_value=(200,b'{"pid":13}'))
        d._live_pin=mock.Mock(return_value={'state':'active','admission':'open',
            'vendor_identity_verified':True,'active_turn':{'state':'running','phase':'accepted'}})
        d._publication_entered.set()
        d.observe=mock.Mock(return_value={'events':[]})
        d._lock_holders=mock.Mock(return_value=[d.anchor.report()])
        d.build_hashes={'release':'a'*64};d.phase_kind='release'
        sample=d.lifecycle('publication')
        self.assertTrue(sample['live_pin'])
        self.assertFalse(sample['retirement_inflight'])

    def test_native_session_id_blocks_terminal_before_creation(self):
        self.driver.handles['s_owned'] = 'fake-memory-only-handle'
        self.driver.via = mock.Mock(return_value={
            'state': 'failed', 'vendor_identity_verified': False, 'vendor_session_id': None})
        with self.assertRaisesRegex(Blocked, 'session ended before native creation'):
            self.driver._vendor_sid('s_owned')
        self.driver.via.assert_called_once()

    def test_tool_after_waits_for_atomic_file_update(self):
        project = Path(self.root.name) / 'project'; project.mkdir()
        folder = project / 'observations'; folder.mkdir()
        marker = folder / 'tool.json'
        marker.write_text(json.dumps({'point': 'tool_during', 'pid': 14, 'start_ticks': 4}))
        self.driver._lifecycle_schema = {'paths': {}}
        self.driver.stop = mock.Mock(); self.driver.fixture = mock.Mock(return_value=project)
        self.driver.start = mock.Mock(); self.driver.ensure_vendor = mock.Mock()
        self.driver.via = mock.Mock(return_value={'session_id': 's_owned'})
        self.driver._operation = mock.Mock(return_value={'started': True, 'owned': True})
        self.driver._live_helper = mock.Mock(return_value=project / 'helper')
        self.driver._helper_folder = mock.Mock(return_value=folder)
        self.driver.mock_providers['l14-1-tool_after'] = mock.Mock(script=[])
        self.driver.helper_ledger[Identity(14, 4)] = {'kind': 'tool'}
        self.driver.helper_generations[Identity(14, 4)] = self.driver.vendor_identity
        def sleep(seconds):
            self.clock[0] += seconds
            marker.write_text(json.dumps({'point': 'tool_after', 'pid': 14, 'start_ticks': 4}))
        with mock.patch.object(transport.time, 'sleep', side_effect=sleep):
            self.driver._prepare_lifecycle('tool_after')
        self.assertGreater(self.clock[0], 0)

    def test_direct_seed_verified_absence_waits_under_phase_deadline(self):
        import opencode_safety as safety
        self.driver.daemon = None; self.driver.namespace = Path(self.root.name)
        self.driver.namespace_env = {}; self.driver.env = {'PATH': '/usr/bin:/bin'}
        self.driver._locks_free = mock.Mock(return_value=True)
        self.driver._generated_vendor_config = mock.Mock(return_value={})
        self.driver._prepare_l11_mock_provider = mock.Mock()
        self.driver._pgrep_clear = mock.Mock(return_value=True)
        self.driver.vault.read_once = mock.Mock(return_value=b'fixture-password')
        self.driver.proc.stat.return_value = {'pid': 31, 'start_ticks': 3}
        self.driver.proc.gone.return_value = True
        self.driver.proc.listener.side_effect = safety.ListenerNotReady('not yet')
        self.driver.phase_deadline = .03
        process = mock.Mock(pid=31); process.poll.return_value = None
        with mock.patch.object(safety, 'verify_binary'), \
             mock.patch.object(transport.subprocess, 'Popen', return_value=process):
            with self.assertRaisesRegex(Blocked, 'L11 direct listener startup deadline'):
                self.driver.direct_seed(self.driver.namespace, {'schema_checked': True})
        self.assertLessEqual(self.clock[0], .03)

    def test_bootstrap_store_ambiguity_blocks_instead_of_retrying(self):
        self.driver.namespace = Path(self.root.name)
        self.driver.namespace_project = Path(self.root.name) / 'project'
        config = {}
        hold = mock.Mock(endpoint='http://127.0.0.1:1234/v1', requests=0)
        self.driver._namespace_bootstraps[self.driver.namespace_project] = (config, hold)
        self.driver._bootstrap_static = mock.Mock(return_value={})
        self.driver.via = mock.Mock(side_effect=lambda args: {
            'session_id': 's_owned', 'turn': 's_owned/1'} if args[0]=='spawn' else {})
        self.driver._host_record.side_effect = Blocked('owned Host generation ambiguous')
        with self.assertRaisesRegex(Blocked, 'owned Host generation ambiguous'):
            self.driver._bootstrap_vendor()
        self.assertEqual(self.clock[0], 0)
        self.assertTrue(hold.response_hold.aborted)

    def test_served_wrong_version_and_openapi_do_not_retry(self):
        self.driver.proc.stat.return_value = {'pid': 13, 'ppid': 12, 'start_ticks': 3}
        self.driver.proc.listener.return_value = 'http://127.0.0.1:1234'
        self.driver.vault.read_once = mock.Mock(return_value=b'fixture-password')
        with mock.patch.object(transport.safety, 'OwnedHTTP') as http:
            http.return_value.request.return_value = (200, b'{"pid":13,"version":"2.0.24"}')
            with self.assertRaisesRegex(Blocked, 'handshake mismatch'):
                self.driver._observe_vendor()
            http.return_value.request.assert_called_once()
        self.driver._http = mock.Mock()
        self.driver._http.request.return_value = (200, b'{"paths":{}}')
        with self.assertRaisesRegex(Blocked, 'OpenAPI differs'):
            self.driver._served_schema()
        self.driver._http.request.assert_called_once()

    def test_helper_file_appearance_is_polled_with_owned_ancestry(self):
        folder = Path(self.root.name) / 'observations'; folder.mkdir()
        self.driver._helper_folder = mock.Mock(return_value=folder)
        self.driver._descends_from = mock.Mock(return_value=True)
        def sleep(seconds):
            self.clock[0] += seconds
            (folder / 'tool.json').write_text(json.dumps({'pid': 14, 'start_ticks': 4,
                                                         'ready': True, 'kind': 'tool'}))
        with mock.patch.object(transport.time, 'sleep', side_effect=sleep):
            reply = self.driver._operation('helper_barrier', {'helper': 'tool'})
        self.assertTrue(reply['owned']); self.assertGreater(self.clock[0], 0)
        self.assertIn(Identity(14, 4), self.driver.helper_ledger)

    def test_helper_point_update_rejects_identity_change(self):
        folder = Path(self.root.name) / 'observations'; folder.mkdir()
        self.driver._helper_folder = mock.Mock(return_value=folder)
        self.driver.helper_ledger[Identity(14, 4)] = {'kind': 'tool'}
        self.driver.helper_generations[Identity(14, 4)] = self.driver.vendor_identity
        (folder / 'tool.json').write_text(json.dumps({'pid': 15, 'start_ticks': 5, 'point': 'tool_after'}))
        with self.assertRaisesRegex(Blocked, 'helper after-point identity changed'):
            self.driver._await_helper_point('tool', 'tool_after')

    def test_helper_snapshot_waits_for_each_expected_plugin_process(self):
        project = Path(self.root.name) / 'project'; project.mkdir()
        (project / '.opencode').mkdir()
        self.driver.project = project
        for kind in ('mcp', 'plugin', 'hook'):
            (project / '.opencode' / (kind + '-helper.py')).touch()
        folder = project / 'observations'; folder.mkdir()
        self.driver._helper_folder = mock.Mock(return_value=folder)
        self.driver.proc.alive.return_value = True
        self.driver._descends_from = mock.Mock(return_value=True)
        def publish(kind, pid):
            (folder / (kind + '.json')).write_text(json.dumps({
                'pid': pid, 'start_ticks': pid, 'password_key_absent': True,
                'ready': True, 'kind': kind}))
        publish('mcp', 14)
        def sleep(seconds):
            self.clock[0] += seconds
            publish('plugin', 15); publish('hook', 16)
        with mock.patch.object(transport.time, 'sleep', side_effect=sleep):
            rows = self.driver.observe('helper_observations')['helpers']
        self.assertEqual({row['kind'] for row in rows}, {'mcp', 'plugin', 'hook'})
        self.assertGreater(self.clock[0], 0)

    def test_live_pin_waits_for_via_to_commit_native_acceptance(self):
        self.driver.via = mock.Mock(side_effect=[
            {'state': 'active', 'admission': 'open', 'vendor_identity_verified': True,
             'active_turn': {'state': 'running', 'phase': 'submitting'}},
            {'state': 'active', 'admission': 'open', 'vendor_identity_verified': True,
             'active_turn': {'state': 'running', 'phase': 'accepted'}}])
        self.driver.journal._signal = mock.Mock()
        result = self.driver._operation('server_loss_for_marker', {'session': 's_owned'})
        self.assertTrue(result['vendor_pid_only']); self.assertEqual(self.driver.via.call_count, 2)
        self.driver.journal._signal.assert_called_once()

    def test_duplicate_sse_delivery_is_not_retried(self):
        self.events([{'events': [self.delivered(), {**self.delivered(), 'seq': 2}]}])
        with self.assertRaisesRegex(Blocked, 'ambiguous'):
            self.driver._operation('turn_identity', {'session': 's_owned',
                       'envelope': {'vendor_session_id': 'ses_owned', 'turn': 1}})
        self.assertEqual(self.clock[0], 0)

    def test_native_terminal_never_credits_previous_turn_during_lag(self):
        self.driver._vendor_sid = mock.Mock(return_value='ses_owned')
        self.driver.last_request = {'receipt': {'session_id': 's_owned', 'turn': 's_owned/2'}}
        current = {'seq': 3, 'type': 'session.inbox.delivered', 'data': {
            'sessionID': 'ses_owned', 'inboxID': transport.input_id('s_owned', 2)}}
        current_end = {**self.terminal(), 'seq': 4}
        self.events([{'events': [self.delivered(), self.terminal()]},
                     {'events': [self.delivered(), self.terminal(), current, current_end]}])
        result = self.driver._operation('native_terminal', {'session': 's_owned'})
        self.assertTrue(result['owned']); self.assertGreater(self.clock[0], 0)

    def test_held_response_cancel_waits_for_native_user_interruption(self):
        folder = Path(self.root.name) / 'failpoints'; folder.mkdir()
        self.driver.env['VIA_FAILPOINT_DIR'] = str(folder)
        point = 'wire.http.before_response'
        (folder / (point + '.1.ack')).write_text('{}')
        self.driver.last_cancel = {'reply': {'state': 'cancelled'}, 'held': {point: 1}, 'elapsed_ms': 1}
        self.driver._vendor_sid = mock.Mock(return_value='ses_owned')
        self.driver._fake_control = mock.Mock(return_value={'verified': True})
        self.events([{'events': [self.delivered()]}, {'events': [self.delivered()]},
                     {'events': [self.delivered(), {
            'seq': 2, 'type': 'session.execution.interrupted', 'data': {'reason': 'user'}}]}])
        result = self.driver._operation('cancel_timing', {'session': 's_owned'})
        self.assertTrue(result['native_user_interrupted']); self.assertGreater(self.clock[0], 0)

    def test_helper_snapshot_never_reuses_live_old_generation(self):
        project = Path(self.root.name) / 'project'; project.mkdir()
        folder = project / 'observations'; folder.mkdir()
        self.driver.project = project; self.driver._helper_folder = mock.Mock(return_value=folder)
        identity = Identity(14, 4)
        self.driver.proc.alive.return_value = True
        self.driver.helper_ledger[identity] = {'kind': 'plugin'}
        self.driver.helper_generations[identity] = Identity(50, 50)
        (folder / 'plugin.json').write_text(json.dumps({'pid': 14, 'start_ticks': 4,
                         'password_key_absent': True, 'ready': True, 'kind': 'plugin'}))
        with self.assertRaisesRegex(Blocked, 'generation changed'):
            self.driver.observe('helper_observations')
        self.assertEqual(self.clock[0], 0)

    def test_ensure_vendor_never_bootstraps_when_existing_identity_is_unverifiable(self):
        self.driver._http = mock.Mock()
        self.driver.proc.alive.side_effect=lambda identity: True if identity==self.driver.daemon else None
        self.driver.proc.stat.return_value = {'pid': 13, 'start_ticks': 3, 'ppid': 12}
        self.driver._bootstrap_vendor = mock.Mock()
        with self.assertRaisesRegex(Blocked, 'vendor identity unverifiable'):
            self.driver.ensure_vendor()
        self.driver._bootstrap_vendor.assert_not_called()

    def test_cached_vendor_origin_rechecks_host_binding(self):
        self.driver._http = mock.Mock()
        self.driver.proc.alive.return_value = True
        self.driver._host_record.return_value = {**self.record, 'anchor': Identity(40, 40)}
        with self.assertRaisesRegex(Blocked, 'generation changed'):
            self.driver.ensure_vendor()

    def test_offered_lsp_does_not_require_spawn_in_an_unconfigured_project(self):
        project = Path(self.root.name) / 'project'; project.mkdir()
        (project / '.opencode').mkdir()
        self.driver.project = project; self.driver._lsp_result = {'spawned': True}
        self.driver._helper_snapshot = mock.Mock(return_value=[{'kind': 'tool', 'ready': True}])
        self.driver.phase_deadline = .03
        result = self.driver.observe('helper_observations')
        self.assertEqual(result['helpers'][0]['kind'], 'tool')
