#!/usr/bin/env python3
"""Fake-only driver checks for vendors/opencode.md §§2–4 and 13."""
import importlib.util
import json
import os
import subprocess
import tempfile
import time
import threading
import types
from unittest import mock
import unittest
from pathlib import Path

from opencode_safety import Blocked, Identity
from opencode_driver import Driver, strict_reply, read_pages, bounded_command, input_id
from opencode_safety_tests import fake_cli_generation


class DriverTests(unittest.TestCase):
    def test_release_thread_failure_cannot_be_overwritten_by_protocol_eof(self):
        import opencode_safety as safety
        with tempfile.TemporaryDirectory(prefix='via-oc-helper-') as folder:
            base=Path(folder);project=base/'project';project.mkdir(mode=0o700)
            (project/'.opencode').mkdir(mode=0o700)
            d=Driver('release','fp','pin',base/'evidence',initialize=False)
            helper=d._helper_fixture(project,'lsp')
            env={key:str(safety.private_directory(base/part)) for key,part in safety.PRIVATE_PARTS.items()}
            env.update(PATH='/usr/bin:/bin',LANG='C.UTF-8')
            script="import pathlib,runpy\noriginal=pathlib.Path.exists\ndef failure(path):\n if path.name=='lsp.release':raise PermissionError(13,'FAKE-private-message','/FAKE-private-path')\n return original(path)\npathlib.Path.exists=failure\nrunpy.run_path("+repr(str(helper))+",run_name='__main__')"
            process=subprocess.Popen(['/usr/bin/python3','-c',script],cwd=project,env=env,
                stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            observation=d._helper_folder(project)/'lsp.json';row=None
            try:
                end=time.monotonic()+2
                while time.monotonic()<end and process.poll() is None:
                    if observation.exists():
                        row=json.loads(observation.read_text())
                        if row.get('ready') is False:break
                    time.sleep(.01)
            finally:
                try:process.communicate(timeout=5)
                except subprocess.TimeoutExpired:process.kill();process.communicate(timeout=5)
            self.assertIsNotNone(row)
            self.assertIs(row.get('ready'),False)
            self.assertEqual(row['failure'],{'exception_class':'PermissionError','errno':'EACCES','step':'release'})
            self.assertEqual(json.loads(observation.read_text()),row)
            self.assertNotIn('FAKE-private',json.dumps(row))

    def test_every_helper_atomically_records_closed_failure_before_readiness(self):
        import opencode_safety as safety
        for kind in ('tool','marker','lsp','mcp','plugin','hook'):
            with self.subTest(kind=kind),tempfile.TemporaryDirectory(prefix='via-oc-helper-') as folder:
                base=Path(folder);project=base/'project';project.mkdir(mode=0o700)
                (project/'.opencode').mkdir(mode=0o700)
                d=Driver('release','fp','pin',base/'evidence',initialize=False)
                helper=d._helper_fixture(project,kind,separate_group=True)
                env={key:str(safety.private_directory(base/part)) for key,part in safety.PRIVATE_PARTS.items()}
                env.update(PATH='/usr/bin:/bin',LANG='C.UTF-8')
                script="import os,runpy\ndef fail(*args):\n raise PermissionError(13,'FAKE-private-message','/FAKE-private-path')\nos.getsid=fail\nos.fork=fail\nrunpy.run_path("+repr(str(helper))+",run_name='__main__')"
                result=subprocess.run(['/usr/bin/python3','-c',script],cwd=project,env=env,
                                      capture_output=True,timeout=5,check=False)
                self.assertNotEqual(result.returncode,0)
                observation=d._helper_folder(project)/(kind+'.json')
                self.assertTrue(observation.exists(),'exception disappeared before readiness')
                row=json.loads(observation.read_text())
                self.assertEqual(row,{'kind':kind,'ready':False,'failure':{
                    'exception_class':'PermissionError','errno':'EACCES',
                    'step':'fork' if kind=='marker' else 'session'}})
                self.assertNotIn('FAKE-private',json.dumps(row))
                self.assertEqual(list(observation.parent.glob('*.tmp')),[])

    def test_helper_timeout_retains_failure_without_exposing_extra_fields(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-helper-') as folder:
            base=Path(folder);d=Driver('release','fp','pin',base/'evidence',initialize=False)
            observations=base/'observations';observations.mkdir()
            d.project=base;d._helper_folder=mock.Mock(return_value=observations)
            row={'kind':'tool','ready':False,'failure':{'exception_class':'PermissionError',
                'errno':'EPERM','step':'session','message':'FAKE-private-message','path':'/FAKE-private-path'}}
            (observations/'tool.json').write_text(json.dumps(row))
            d._verify=mock.Mock(side_effect=AssertionError('failed helper must not become a ready identity'))
            def timeout(read,reason,**_kwargs):
                self.assertIsNone(read());raise Blocked(reason)
            d._await_owned=mock.Mock(side_effect=timeout)
            with self.assertRaisesRegex(Blocked,'^helper barrier not reached$'):
                d._operation('helper_barrier',{'helper':'tool'})
            retained=json.loads(next(d.evidence.glob('*helper-readiness.json')).read_text())
            self.assertEqual(retained.get('failure'),{'ready':False,
                'exception_class':'PermissionError','errno':'EPERM','step':'session'})
            self.assertNotIn('FAKE-private',json.dumps(retained))
            self.assertTrue(retained['not_ready_seen'])
            self.assertEqual(d.identities,set())

    def test_helper_snapshot_retains_a_closed_failure_before_blocking(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-helper-') as folder:
            base=Path(folder);d=Driver('release','fp','pin',base/'evidence',initialize=False)
            observations=base/'observations';observations.mkdir()
            d.project=base;d._helper_folder=mock.Mock(return_value=observations)
            (observations/'lsp.json').write_text(json.dumps({'kind':'lsp','ready':False,
                'failure':{'exception_class':'FAKE-private-class','errno':'FAKE-private-errno',
                           'step':'FAKE-private-step','message':'FAKE-private-message'}}))
            with self.assertRaisesRegex(Blocked,'^helper reported failure$'):d._helper_snapshot()
            retained=json.loads(next(d.evidence.glob('*helper-failure.json')).read_text())
            self.assertEqual(retained,{'ready':False,'exception_class':'other','errno':None,'step':'other'})

    def test_helper_readiness_survives_an_already_detached_shell(self):
        """FAKE the pinned Shell.create detached launch with the real fixture helper (§13)."""
        import opencode_safety as safety
        for detached in (True,False):
            with self.subTest(detached=detached), tempfile.TemporaryDirectory(prefix='via-oc-helper-') as folder:
                base=Path(folder);project=base/'project';project.mkdir(mode=0o700)
                (project/'.opencode').mkdir(mode=0o700)
                d=Driver('release','fp','pin',base/'evidence',initialize=False)
                d._helper_fixture(project,'tool',separate_group=True)
                observation=d._helper_folder(project)/'tool.json'
                release=observation.with_suffix('.release')
                env={key:str(safety.private_directory(base/part))
                     for key,part in safety.PRIVATE_PARTS.items()}
                env.update(PATH='/usr/bin:/bin',LANG='C.UTF-8',VIA_PROCESS_MARKER='FAKE-helper')
                process=subprocess.Popen(['/bin/bash','-c',safety.CANCEL_HELPER_COMMAND],
                    cwd=project,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                    start_new_session=detached)
                identity=None;ready=False
                try:
                    end=time.monotonic()+2
                    while not observation.exists() and process.poll() is None and time.monotonic()<end:
                        time.sleep(.01)
                    ready=observation.exists()
                    if ready:
                        row=json.loads(observation.read_text())
                        identity=Identity(row['pid'],row['start_ticks'])
                        self.assertEqual(identity.pid,process.pid)
                        self.assertEqual(row['pgid'],identity.pid)
                        self.assertEqual(os.getsid(identity.pid),identity.pid)
                        self.assertIs(d.proc.alive(identity),True)
                        self.assertTrue(row['ready'])
                finally:
                    release.touch(mode=0o600)
                    try:_,stderr=process.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill();process.communicate(timeout=5)
                        stderr=b''
                self.assertTrue(ready,'helper readiness unavailable: setsid EPERM'
                                if b'PermissionError: [Errno 1]' in stderr else 'helper readiness unavailable')
                self.assertEqual(process.returncode,0)
                self.assertIs(d.proc.alive(identity),False)

    def test_cancel_diagnostics_register_only_the_verified_cancellation_session(self):
        for diagnostic in (False,True):
            with self.subTest(diagnostic=diagnostic), tempfile.TemporaryDirectory() as folder:
                d=Driver('release','fp','pin',Path(folder)/'evidence',initialize=False)
                d.project=Path(folder)
                d._helper_folder=mock.Mock(return_value=Path(folder))
                d._vendor_sid=mock.Mock(return_value='ses_FAKE_cancel')
                d._await_owned=mock.Mock(return_value={'started':True,'owned':True})
                d._operation('helper_barrier',{'helper':'tool','session':'s_FAKE',
                                             'cancel_diagnostic':diagnostic})
                self.assertEqual(getattr(d,'_cancel_diagnostic_session',None),
                                 'ses_FAKE_cancel' if diagnostic else None)
                self.assertEqual(d._vendor_sid.call_count,int(diagnostic))

    def catalog_driver(self,root,body,status=200):
        """FAKE per-location API replies; the real spending guard remains active."""
        import opencode_safety as safety
        d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
        d.project=d.evidence/'project-boundary'; d.project.mkdir()
        endpoint='http://127.0.0.1:1234'
        d.provider_endpoints={'oclive-mock':endpoint}
        d.guard.mock_origins=frozenset({endpoint})
        d.fixtures={'test':{'path':d.project,'config':{}}}
        d.ensure_vendor=mock.Mock(); d._validate_provider_config=mock.Mock(return_value=d.provider_endpoints)
        d._auxiliary_bindings=mock.Mock(return_value={'model':safety.MOCK_IDENTITY})
        d.vault.names=mock.Mock(return_value=safety.VENDOR_ENVIRONMENT)
        d._http=mock.Mock()
        raw=body if isinstance(body,bytes) else json.dumps(body).encode()
        def request(_method,path,**_kwargs):
            return (200,b'{"data":[]}') if path=='/api/integration' else (status,raw)
        d._http.request.side_effect=request
        return d

    def test_blocked_catalog_retains_checked_mock_location_and_price_evidence(self):
        for cost in ([],[{'input':1,'output':0,'cache':{'read':0,'write':0}}]):
            with self.subTest(cost=cost), tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
                models=[{'providerID':'oclive-mock','id':'fixture-free','cost':cost}]
                d=self.catalog_driver(root,{'data':models})
                with mock.patch('opencode_driver.time.sleep') as sleep:
                    with self.assertRaisesRegex(Blocked,'frozen free model absent from catalog'):
                        d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
                    sleep.assert_not_called()
                paths=list(d.evidence.glob('*catalog-block.json'))
                self.assertEqual(len(paths),1,'blocked catalogue evidence was lost')
                observed=json.loads(paths[0].read_text())
                self.assertEqual(observed['checked_model'],{'providerID':'oclive-mock','id':'fixture-free',
                    'status':'not-explicit-zero'})
                self.assertEqual(observed['location'],'project-boundary')
                self.assertEqual((observed['route'],observed['http_status']),('/api/model',200))
                self.assertEqual(observed['models'][0]['cost'],models[0]['cost'])
                self.assertTrue(d.guard.stopped)
                self.assertTrue(all(call.args[0]=='GET' for call in d._http.request.call_args_list))

    def test_invalid_catalog_retains_only_allowed_fields_and_redacts_identifiers(self):
        secret='FAKE-catalog-protected-password'
        body={'data':[{'providerID':'oclive-mock','id':'fixture-free',
            'cost':[{'input':0,'output':0,'cache':{'read':0,'write':secret},'headers':{'secret':secret}}],
            'status':'deprecated','settings':{'apiKey':secret},'headers':{'token':secret},
            'url':'http://user:'+secret+'@example.invalid/'},
            {'providerID':secret,'id':'http://user:'+secret+'@example.invalid/','cost':[],
             'status':secret}]}
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=self.catalog_driver(root,body); d.secret_forms=[secret.encode()]
            with self.assertRaisesRegex(Blocked,'catalog cost.cache.write: wrong field type'):
                d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
            paths=list(d.evidence.glob('*catalog-block.json'))
            self.assertEqual(len(paths),1,'schema block lost its sanitized catalogue')
            raw=paths[0].read_bytes();self.assertNotIn(secret.encode(),raw)
            for forbidden in (b'http://',b'headers',b'apiKey',b'settings',b'url'):
                self.assertNotIn(forbidden,raw)
            observed=json.loads(raw)
            self.assertEqual(observed['models'][0]['status'],'deprecated')
            self.assertEqual(observed['models'][0]['cost'][0]['cache']['write'],None)
            self.assertIsNone(observed['models'][1]['providerID'])
            self.assertIsNone(observed['models'][1]['id'])
            self.assertEqual(observed['models'][1]['status'],'unrecognized')

    def test_unavailable_catalog_retains_status_without_vendor_error_text(self):
        for status,body,reason in ((503,{'error':'FAKE-PRIVATE-vendor-text'},'location catalog unavailable'),
                                   (200,b'FAKE-PRIVATE-not-json','vendor JSON shape unavailable')):
            with self.subTest(status=status), tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
                d=self.catalog_driver(root,body,status=status)
                with self.assertRaisesRegex(Blocked,reason):
                    d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
                paths=list(d.evidence.glob('*catalog-block.json'))
                self.assertEqual(len(paths),1,'unavailable catalogue status was lost')
                raw=paths[0].read_bytes();self.assertNotIn(b'FAKE-PRIVATE',raw)
                observed=json.loads(raw);self.assertEqual(observed['http_status'],status)
                self.assertEqual(observed['checked_model']['status'],'unverifiable')
                self.assertEqual(observed['models'],[])

    def test_explicit_zero_prices_need_no_vendor_free_key_but_missing_price_blocks(self):
        for cache in ({'read':0,'write':0},{'read':0}):
            with self.subTest(cache=cache), tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
                body={'data':[{'providerID':'oclive-mock','id':'fixture-free',
                               'cost':[{'input':0,'output':0,'cache':cache}]}]}
                d=self.catalog_driver(root,body)
                if 'write' not in cache:
                    with self.assertRaisesRegex(Blocked,'catalog cost.cache: required field missing'):
                        d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
                else:
                    result=d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
                    self.assertTrue(result['structural_spending_controls'])
                    self.assertFalse(list(d.evidence.glob('*catalog-block.json')))

    def test_catalog_identifier_projection_excludes_password_handle_and_synthetic_forms(self):
        secrets=('FAKE-private-password','h_FAKE-private-handle','FAKE-private-synthetic')
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            body={'data':[{'providerID':value,'id':value,'cost':[]} for value in secrets]}
            d=self.catalog_driver(root,body)
            d.vault._values[Identity(71,123)]=secrets[0].encode()
            d.bearer_forms.add(secrets[1].encode()); d.secret_forms.append(secrets[2].encode())
            d.phase_deadline=time.monotonic()+.02
            with self.assertRaisesRegex(Blocked,'owned location catalog readiness deadline'):
                d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
            paths=list(d.evidence.glob('*catalog-block.json'))
            self.assertEqual(len(paths),1,'protected IDs must be redacted, not lose the record')
            raw=paths[0].read_bytes()
            for value in secrets: self.assertNotIn(value.encode(),raw)
            rows=json.loads(raw)['models']
            self.assertTrue(all(row['providerID'] is None and row['id'] is None for row in rows))

    def test_empty_location_catalog_waits_for_explicit_zero_prices(self):
        empty={'data':[]}; ready={'data':[{'providerID':'oclive-mock','id':'fixture-free',
            'cost':[{'input':0,'output':0,'cache':{'read':0,'write':0}}]}]}
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=self.catalog_driver(root,empty); replies=[empty,ready]
            d.daemon=Identity(10,1);d.anchor=Identity(11,2);d.vendor_identity=Identity(12,3)
            d.proc=mock.Mock();d._host_record=mock.Mock(return_value={
                'generation':'FAKE-owned','server_id':'FAKE-server','anchor':d.anchor,'vendor_pid':12})
            def request(_method,path,**_kwargs):
                return (200,b'{"data":[]}') if path=='/api/integration' else (200,json.dumps(replies.pop(0)).encode())
            d._http.request.side_effect=request
            with mock.patch('opencode_driver.time.sleep'):
                result=d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
            self.assertTrue(result['structural_spending_controls'])
            self.assertEqual(replies,[])
            for identity in (d.daemon,d.anchor,d.vendor_identity):
                self.assertGreaterEqual(d.proc.verify.call_args_list.count(mock.call(identity)),4)
            self.assertFalse(list(d.evidence.glob('*catalog-block.json')))

    def test_staged_location_catalog_waits_for_checked_identity(self):
        other={'data':[{'providerID':'FAKE-other','id':'FAKE-other',
            'cost':[{'input':0,'output':0,'cache':{'read':0,'write':0}}]}]}
        ready={'data':[{'providerID':'oclive-mock','id':'fixture-free',
            'cost':[{'input':0,'output':0,'cache':{'read':0,'write':0}}]}]}
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=self.catalog_driver(root,other);replies=[{'data':[]},other,ready]
            d.daemon=Identity(10,1);d.anchor=Identity(11,2);d.vendor_identity=Identity(12,3)
            d.proc=mock.Mock();d._host_record=mock.Mock(return_value={
                'generation':'FAKE-owned','server_id':'FAKE-server','anchor':d.anchor,'vendor_pid':12})
            def request(_method,path,**_kwargs):
                return (200,b'{"data":[]}') if path=='/api/integration' else (200,json.dumps(replies.pop(0)).encode())
            d._http.request.side_effect=request
            with mock.patch('opencode_driver.time.sleep') as sleep:
                result=d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
            self.assertTrue(result['structural_spending_controls']);self.assertEqual(replies,[])
            self.assertEqual(sleep.call_args_list,[mock.call(.2),mock.call(.2)])
            for identity in (d.daemon,d.anchor,d.vendor_identity):
                self.assertGreaterEqual(d.proc.verify.call_args_list.count(mock.call(identity)),6)
            self.assertFalse(list(d.evidence.glob('*catalog-block.json')))

    def test_staged_location_catalog_absence_deadline_retains_last_nonempty_reply(self):
        other={'data':[{'providerID':'FAKE-other','id':'FAKE-other',
            'cost':[{'input':0,'output':0,'cache':{'read':0,'write':0}}]}]}
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=self.catalog_driver(root,other);clock=[0.0];d.phase_deadline=.25
            with mock.patch('opencode_driver.time.monotonic',side_effect=lambda:clock[0]), \
                 mock.patch('opencode_driver.time.sleep',side_effect=lambda seconds:clock.__setitem__(0,clock[0]+seconds)):
                with self.assertRaisesRegex(Blocked,'owned location catalog readiness deadline'):
                    d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
            self.assertAlmostEqual(clock[0],.25)
            paths=list(d.evidence.glob('*catalog-block.json'));self.assertEqual(len(paths),1)
            value=json.loads(paths[0].read_text())
            self.assertEqual(value['checked_model']['status'],'absent')
            self.assertEqual(value['models'][0]['providerID'],'FAKE-other')
            self.assertEqual(value['http_status'],200)

    def test_empty_location_catalog_generation_change_blocks_before_retry(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=self.catalog_driver(root,{'data':[]})
            d.daemon=Identity(10,1);d.anchor=Identity(11,2);d.vendor_identity=Identity(12,3)
            d.proc=mock.Mock();row={'generation':'FAKE-owned','server_id':'FAKE-server',
                                  'anchor':d.anchor,'vendor_pid':12}
            d._host_record=mock.Mock(side_effect=lambda:dict(row))
            with mock.patch('opencode_driver.time.sleep',side_effect=lambda _s:row.update(generation='FAKE-changed')):
                with self.assertRaisesRegex(Blocked,'owned observation generation changed'):
                    d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
            self.assertEqual(d._http.request.call_count,1)  # Failed readiness never reads integration.
            paths=list(d.evidence.glob('*catalog-block.json'));self.assertEqual(len(paths),1)
            self.assertEqual(json.loads(paths[0].read_text())['models'],[])

    def test_empty_location_catalog_has_one_absolute_deadline_and_retains_last_reply(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=self.catalog_driver(root,{'data':[]});clock=[0.0];d.phase_deadline=.25
            with mock.patch('opencode_driver.time.monotonic',side_effect=lambda:clock[0]), \
                 mock.patch('opencode_driver.time.sleep',side_effect=lambda seconds:clock.__setitem__(0,clock[0]+seconds)):
                with self.assertRaisesRegex(Blocked,'owned location catalog readiness deadline'):
                    d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
            self.assertAlmostEqual(clock[0],.25)
            paths=list(d.evidence.glob('*catalog-block.json'));self.assertEqual(len(paths),1)
            value=json.loads(paths[0].read_text());self.assertEqual(value['http_status'],200)
            self.assertEqual(value['models'],[])
            self.assertTrue(all(call.kwargs['timeout']<=.25 for call in d._http.request.call_args_list
                                if call.args[1].startswith('/api/model?')))

    def test_marker_server_loss_records_only_successfully_killed_anchor(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.daemon=Identity(69,120); d.anchor=Identity(70,122)
            d._live_pin=mock.Mock(); d.proc=mock.Mock(); d.journal=mock.Mock()
            d.journal._signal.side_effect=Blocked('FAKE signal failed')
            with self.assertRaisesRegex(Blocked,'FAKE signal failed'):
                d._operation('server_loss_for_marker',{'session':'fixture-session'})
            self.assertIsNone(d._killed_anchor)
            d.journal._signal.side_effect=None
            result=d._operation('server_loss_for_marker',{'session':'fixture-session'})
            self.assertEqual(result,{'anchor_pid_only':True})
            self.assertEqual(d._killed_anchor,d.anchor)
            d.proc.verify.assert_called_with(d.anchor)
            import signal
            d.journal._signal.assert_called_with(d.anchor,signal.SIGKILL,role='anchor')

    def test_info_startup_503_waits_for_matching_owned_server(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d,record,_pending=self.vendor_observer(root)
            d.proc.listener.return_value='http://127.0.0.1:1234'
            with mock.patch('opencode_driver.safety.OwnedHTTP') as http, \
                 mock.patch('opencode_driver.time.sleep'):
                http.return_value.request.side_effect=[
                    (503,b'{"pid":71,"version":"2.0.22"}'),
                    (200,b'{"pid":71,"version":"2.0.22"}')]
                d._observe_vendor()
            self.assertEqual(http.return_value.request.call_count,2)
            d.vault.read_once.assert_called_once()
            self.assertEqual(d.vendor_identity,Identity(71,123))
            self.assertEqual(d._verified_host_record['generation'],record['generation'])
            observed=[call.args[1] for call in d._record.call_args_list
                      if call.args[0]=='vendor-info']
            self.assertEqual([row['status'] for row in observed],[503,200])

    def test_info_pending_refuses_changed_generation(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d,record,_pending=self.vendor_observer(root)
            d.proc.listener.return_value='http://127.0.0.1:1234'
            with mock.patch('opencode_driver.safety.OwnedHTTP') as http, \
                 mock.patch('opencode_driver.time.sleep') as sleep:
                http.return_value.request.return_value=(503,b'{"pid":71,"version":"2.0.22"}')
                sleep.side_effect=lambda _seconds:setattr(d._host_record,'return_value',
                                                         {**record,'generation':'changed'})
                with self.assertRaisesRegex(Blocked,'owned vendor info generation changed'):
                    d._observe_vendor()
            self.assertEqual(http.return_value.request.call_count,1)
            self.assertIsNone(d.vendor_identity)

    def test_info_pending_deadline_is_not_reset(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d,_record,_pending=self.vendor_observer(root)
            d.proc.listener.return_value='http://127.0.0.1:1234'
            with mock.patch('opencode_driver.safety.OwnedHTTP') as http, \
                 mock.patch('opencode_driver.time.sleep') as sleep:
                http.return_value.request.return_value=(503,b'{"pid":71,"version":"2.0.22"}')
                sleep.side_effect=lambda _seconds:setattr(d,'_bootstrap_deadline',time.monotonic()-1)
                with self.assertRaisesRegex(Blocked,'owned vendor info startup deadline'):
                    d._observe_vendor()
            self.assertEqual(http.return_value.request.call_count,1)
            self.assertIsNone(d.vendor_identity)

    def test_info_retry_never_accepts_ambiguous_or_incompatible_reply(self):
        replies=[(503,b'{"pid":72,"version":"2.0.22"}'),
                 (503,b'{"pid":71,"version":"2.0.24"}'),
                 (500,b'{"pid":71,"version":"2.0.22"}'),
                 (503,b'{"version":"2.0.22"}')]
        for reply in replies:
            with self.subTest(reply=reply), tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
                d,_record,_pending=self.vendor_observer(root)
                d.proc.listener.return_value='http://127.0.0.1:1234'
                with mock.patch('opencode_driver.safety.OwnedHTTP') as http, \
                     mock.patch('opencode_driver.time.sleep') as sleep:
                    http.return_value.request.return_value=reply
                    with self.assertRaises(Blocked): d._observe_vendor()
                    sleep.assert_not_called()
                self.assertEqual(http.return_value.request.call_count,1)
                self.assertIsNone(d.vendor_identity)

    def vendor_observer(self, root):
        import opencode_safety as safety
        d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
        record={'anchor':Identity(70,122),'vendor_pid':71,'generation':'owned-generation',
                'phase':'arm_intent','server_id':'owned-server'}
        d._host_record=mock.Mock(return_value=record)
        d.proc=mock.Mock()
        d.proc.stat.return_value={'pid':71,'start_ticks':123,'ppid':70}
        d.vault.read_once=mock.Mock(return_value=b'synthetic-password')
        d._record=mock.Mock(); d.start_event_capture=mock.Mock()
        d._bootstrap_active=True; d._bootstrap_deadline=time.monotonic()+1
        pending=getattr(safety,'ListenerNotReady',Blocked)
        return d,record,pending

    def test_owned_vendor_waits_for_listener_without_rereading_password(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d,_record,pending=self.vendor_observer(root)
            d.proc.listener.side_effect=[pending('owned listener absent or ambiguous'),
                                         'http://127.0.0.1:1234',
                                         'http://127.0.0.1:1234']
            with mock.patch('opencode_driver.safety.OwnedHTTP') as http, \
                 mock.patch('opencode_driver.time.sleep'):
                http.return_value.request.return_value=(200,b'{"pid":71,"version":"2.0.22"}')
                d._observe_vendor()
            self.assertEqual(d.proc.listener.call_count,3)
            d.vault.read_once.assert_called_once()
            self.assertEqual(d.vendor_identity,Identity(71,123))
            self.assertEqual(http.call_args.kwargs['deadline'],d._bootstrap_deadline)
            self.assertTrue(any(call.args[0]=='vendor-listener' and call.args[1]['ready']
                                for call in d._record.call_args_list))

    def test_owned_listener_never_resets_bootstrap_deadline(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d,_record,_pending=self.vendor_observer(root)
            d._bootstrap_deadline=time.monotonic()-1
            d.proc.listener.return_value='http://127.0.0.1:1234'
            with mock.patch('opencode_driver.safety.OwnedHTTP') as http:
                http.return_value.request.return_value=(200,b'{"pid":71,"version":"2.0.22"}')
                with self.assertRaisesRegex(Blocked,'owned listener startup deadline'):
                    d._observe_vendor()
                http.assert_not_called()
            d.proc.listener.assert_not_called()

    def test_ambiguous_listener_is_a_hard_stop_without_retry(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d,_record,_pending=self.vendor_observer(root)
            d.proc.listener.side_effect=Blocked('owned listener absent or ambiguous')
            with mock.patch('opencode_driver.safety.OwnedHTTP') as http:
                with self.assertRaisesRegex(Blocked,'owned listener absent or ambiguous'):
                    d._observe_vendor()
                http.assert_not_called()
            d.proc.listener.assert_called_once()

    def test_listener_wait_refuses_a_changed_host_generation(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d,record,pending=self.vendor_observer(root)
            changed={**record,'generation':'other-generation'}
            d._host_record.side_effect=[record,record,changed]
            d.proc.listener.side_effect=[pending('owned listener absent or ambiguous'),
                                         'http://127.0.0.1:1234']
            with mock.patch('opencode_driver.safety.OwnedHTTP') as http, \
                 mock.patch('opencode_driver.time.sleep'):
                with self.assertRaisesRegex(Blocked,'owned listener generation changed while starting'):
                    d._observe_vendor()
                http.assert_not_called()
            d.proc.listener.assert_called_once()

    def test_stopped_guard_refuses_bootstrap_before_any_submit_or_fixture(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.guard.stopped=True
            d._materialize_fixture=mock.Mock(side_effect=Blocked(
                'FAKE fixture reached after sticky stop'))
            d.via=mock.Mock()
            with self.assertRaisesRegex(Blocked,'spending control previously failed'):
                d._bootstrap_vendor()
            d._materialize_fixture.assert_not_called()
            d.via.assert_not_called()
            self.assertEqual(d._bootstrap_count,0)

    def test_bootstrap_cli_cannot_reset_the_absolute_deadline(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            fake_cli_generation(d)
            d._binary=Path('fake-via'); d._bootstrap_active=True
            d._bootstrap_deadline=time.monotonic()-1
            d.execute=mock.Mock(return_value=(0,b'{"models":[]}',b''))
            with self.assertRaisesRegex(Blocked,'bootstrap CLI deadline'):
                d.via(['models','--harness','opencode'])
            d.execute.assert_not_called()

    def test_bootstrap_lease_handoff_requires_a_current_open_successor(self):
        for state in ('closed','idle','active'):
            current=state!='closed'
            with self.subTest(state=state), tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
                d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
                fake_cli_generation(d)
                d._binary=Path('fake-via'); d._bootstrap_session='s_bootstrap'
                d.handles={'s_bootstrap':'test-boot-handle','s_other':'test-other-handle'}
                envelope={'session_id':'s_other','vendor_session_id':'ses_old','state':'completed'}
                status={'session_id':'s_other','vendor_session_id':'ses_old',
                        'vendor_identity_verified':current,'admission':'open' if current else 'closed',
                        'state':state}
                calls=[]
                def execute(argv,**kwargs):
                    calls.append(argv[1])
                    reply={'wait':envelope,'status':status,'close':{'state':'closed'}}[argv[1]]
                    return 0,json.dumps(reply).encode(),b''
                d.execute=execute; d.account=mock.Mock(); d._record=mock.Mock()
                # This unit exercises handoff, not the schema already covered
                # by the real CLI audit. No fake response substitutes live proof.
                with mock.patch('opencode_driver.strict_reply',side_effect=lambda raw,_schema:json.loads(raw)):
                    d.via(['wait','s_other/1','--timeout-ms','0'])
                self.assertEqual('close' in calls,current)
                self.assertEqual(d._bootstrap_session,None if current else 's_bootstrap')

    def test_existing_owned_server_is_observed_without_another_bootstrap(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.daemon=Identity(10,1); d.proc=mock.Mock()
            d._host_record=mock.Mock(return_value={'vendor_pid':12,'anchor':Identity(11,2)})
            d.proc.stat.return_value={'pid':12,'start_ticks':3,'ppid':11}
            d.proc.alive.return_value=True
            d._observe_vendor=mock.Mock()
            d._bootstrap_vendor=mock.Mock(side_effect=AssertionError('second bootstrap attempted'))
            d.ensure_vendor()
            d._observe_vendor.assert_called_once_with()
            d._bootstrap_vendor.assert_not_called()

    def test_bootstrap_static_checks_precede_any_via_spawn(self):
        for changed,reason in (('environment','environment allow-list'),
                               ('model','frozen mock model'),
                               ('credential','credential-free shape'),
                               ('endpoint','owned loopback mock'),
                               ('disk','configuration drift')):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
                d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
                d.prepare(); d.phase_kind='release'; d.set_phase_budget(0,32)
                d._binary=Path('unused'); d.execute=mock.Mock()
                materialize=d._materialize_fixture
                def mutated(name,project,description):
                    config=materialize(name,project,description)
                    if changed=='environment': d.env['UNEXPECTED_TEST_KEY']='fixture'
                    if changed=='model': config['model']='paid/fixture'
                    if changed=='credential': config['providers']['oclive-mock']['env']=['FAKE_CREDENTIAL']
                    if changed=='endpoint': config['providers']['oclive-mock']['settings']['baseURL']='http://127.0.0.1:1/v1'
                    return config
                original=d._bootstrap_static
                def drift(config):
                    if changed=='disk': (d.namespace/'opencode.json').write_text('{}')
                    return original(config)
                try:
                    with mock.patch.object(d,'_materialize_fixture',side_effect=mutated), \
                         mock.patch.object(d,'_bootstrap_static',side_effect=drift):
                        with self.assertRaisesRegex(Blocked,reason): d._bootstrap_vendor()
                    d.execute.assert_not_called()
                    provider=d.mock_providers['bootstrap-1']
                    self.assertFalse(provider.response_hold.released)
                    self.assertTrue(provider.response_hold.aborted)
                    self.assertEqual(d.phase_mock_left,32)
                    self.assertEqual(d.phase_public_left,0)
                finally:
                    for provider in d.mock_providers.values(): provider.__exit__(None,None,None)
                    if d.runtime.parent==Path('/tmp'): __import__('shutil').rmtree(d.runtime)

    def test_hostile_fixture_covers_variant_endpoint_and_permission_slots(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); d=Driver('release','fp','pin',root/'evidence',initialize=False)
            project=root/'project'; (project/'.opencode').mkdir(parents=True)
            try:
                config=d._materialize_fixture('hostile',project,{'provider':'mock','hostile_keys':True})
                provider=config['providers']['oclive-mock']; model=provider['models']['fixture-free']
                secret=d.mock_providers['hostile'].secret
                self.assertIn(secret,provider['settings']['baseURL'])
                self.assertEqual(model['variants'][0]['headers']['x-variant-fixture'],secret)
                self.assertEqual(model['variants'][0]['body']['via-fixture'],secret)
                self.assertEqual(config['share'],'auto')
                self.assertEqual(config['permissions'],[{'action':'*','resource':'*','effect':'ask'}])
                self.assertEqual(config['agents']['via']['permissions'],config['permissions'])
                self.assertEqual(d._validate_provider_config(config)['oclive-mock'],provider['settings']['baseURL'])
            finally:
                for provider in d.mock_providers.values(): provider.__exit__(None,None,None)

    def test_lifecycle_successor_releases_generation_helper_before_admission(self):
        for survivor,kind in ((False,'tool'),(True,'tool'),(False,'location_shell')):
            with self.subTest(survivor=survivor,kind=kind), tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
                d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
                old,new,helper=Identity(11,1),Identity(12,2),Identity(13,3)
                folder=Path(root)/'helper-channel'; folder.mkdir()
                d.vendor_identity=old; d.identities={old,helper}
                d.server_identities={old}; d.helper_origins={helper:folder}
                d.helper_ledger={helper:{'kind':kind}}; d.helper_generations={helper:old}
                d.proc=mock.Mock()
                def alive(identity):
                    if identity==old: return False
                    if identity==helper: return survivor or not (folder/(kind+'.release')).exists()
                    return True
                d.proc.alive.side_effect=alive; d.proc.gone.side_effect=lambda identity:alive(identity) is False
                def acquire():
                    self.assertTrue((folder/(kind+'.release')).exists())
                    self.assertFalse(alive(helper))
                    d.vendor_identity=new; d.server_identities.add(new)
                d.vault.changed=mock.Mock(return_value=True)
                with mock.patch.object(d,'ensure_vendor',side_effect=acquire) as ensure, \
                        mock.patch.object(d,'close_event_capture'), \
                        mock.patch.object(d,'_host_record',return_value={'phase':'launched'}), \
                        mock.patch('opencode_driver.HELPER_STOP_SECONDS',.02,create=True):
                    if survivor:
                        with self.assertRaises(Blocked):
                            d.observe('lifecycle_successor',{'predecessor':{'pid':old.pid,'start_ticks':old.start_ticks}})
                        ensure.assert_not_called()
                    else:
                        reply=d.observe('lifecycle_successor',{'predecessor':{'pid':old.pid,'start_ticks':old.start_ticks}})
                        self.assertTrue(reply['admitted']); self.assertEqual(reply['parallel_servers'],0)

    def test_strict_reply_rejects_banner_missing_and_boolean_pid(self):
        for raw in (b'banner\n{"pid":4}', b'{}', b'{"pid":true}'):
            with self.subTest(raw=raw), self.assertRaises(Blocked):
                strict_reply(raw, 'daemon status')
        self.assertEqual(strict_reply(b'{"pid":4}', 'daemon status'), {'pid':4})

    def test_cli_error_schema_is_the_bare_c1_stderr_object(self):
        """C1 §1 CLI stderr removes the JSON-RPC error wrapper."""
        value={'code':-32602,'message':'refused',
               'data':{'kind':'invalid_params','field':'max_steps'}}
        self.assertEqual(strict_reply(json.dumps(value).encode(),'error'),value)
        for malformed in ({'error':value},{**value,'code':True},
                          {'code':-32602,'message':'refused','data':{}}):
            with self.subTest(malformed=malformed), self.assertRaises(Blocked):
                strict_reply(json.dumps(malformed).encode(),'error')

    def test_pagination_missing_cursor_never_passes(self):
        for page in ({'events':[], 'more':True, 'next_after':0},
                     {'events':[], 'more':False},
                     {'events':[], 'more':True, 'next_after':True}):
            with self.subTest(page=page), self.assertRaises(Blocked):
                read_pages(lambda _:page)
        self.assertEqual(read_pages(lambda _: {'events':[], 'more':False, 'next_after':0}), [])

    def test_cli_handle_is_stdin_only_and_evidence_redacted(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root)
            calls=[]
            def command(argv, **kw):
                calls.append((argv,kw))
                return (0,b'{"turn":"ses_private/1","state":"running","already_terminal":false,"cancel":null}',b'')
            driver=Driver(root/'release',root/'fp',root/'pin',root/'evidence',
                          execute=command,initialize=False)
            fake_cli_generation(driver)
            driver._binary=root/'release'
            driver.env={'HOME':str(root)}
            driver.handles['ses_private']='h_memory_only'
            reply=driver.via(['cancel','ses_private'])
            self.assertEqual(reply['state'],'running')
            self.assertNotIn('h_memory_only',calls[0][0])
            self.assertEqual(calls[0][1]['input'],b'h_memory_only\n')
            self.assertNotIn(b'h_memory_only',b''.join(p.read_bytes() for p in driver.evidence.glob('*.json')))

    def test_api_credential_get_is_never_sent(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            with self.assertRaises(Blocked):
                d.vendor('GET','/api/credential')

    def test_unknown_observation_blocks(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            with self.assertRaises(Blocked):
                d.observe('fabricated_success')

    def test_build_hashes_both_binaries_without_start(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root)
            for name in ('release','fp'):
                (root/name).write_bytes(name.encode()); (root/name).chmod(0o500)
            d=Driver(root/'release',root/'fp',root/'pin',root/'evidence',initialize=False)
            self.assertNotEqual(d.build('release'),d.build('failpoints'))
            self.assertEqual(set(d.build_hashes),{'release','failpoints'})
            with self.assertRaises(Blocked): d.build('unknown')

    def test_qualified_via_hash_drift_blocks_before_command(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); binary=root/'release'
            binary.write_bytes(b'fixture'); binary.chmod(0o500)
            d=Driver(binary,binary,root/'pin',root/'evidence',initialize=False)
            digest=d.build('release')
            d.fake_gate_manifest={'verified':True,'via_builds':{'release':digest,'failpoints':digest}}
            binary.chmod(0o700); binary.write_bytes(b'changed fixture')
            with mock.patch.object(d,'execute') as execute,self.assertRaises(Blocked): d.build('release')
            execute.assert_not_called()

    def test_real_schema_collection_types_are_checked(self):
        self.assertEqual(strict_reply(b'{"models":[]}', 'models'), {'models':[]})
        with self.assertRaises(Blocked): strict_reply(b'{"models":{}}', 'models')
        with self.assertRaises(Blocked): strict_reply(b'{}', 'unregistered-contract')

    def test_password_evidence_is_boolean_only_and_write_refused(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            identity=Identity(99,5)
            class Proc:
                def read(self,*_): return b'OPENCODE_PASSWORD=synthetic-memory-password\0'
            d.vault.read_once(Proc(),identity)
            with self.assertRaises(Blocked): d._record('secret',{'value':'synthetic-memory-password'})
            self.assertEqual(list(d.evidence.glob('*.json')),[])
            d._record('boolean',{'password_absent':True})
            self.assertNotIn(b'synthetic-memory-password',next(d.evidence.glob('*.json')).read_bytes())

    def test_positive_cost_sets_sticky_stop(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            with self.assertRaises(Blocked): d.account({'cost':{'usd':0.01},'vendor_version':'2.0.22'})
            self.assertTrue(d.guard.stopped)
            with self.assertRaises(Blocked): d._admit_model('opencode/mimo-v2.6-flash-free')

    def test_all_mock_requests_consume_shared_budget(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.set_phase_budget(0,2)
            d._mock_request_admit('fixture-free'); d._mock_request_admit('fixture-free')
            with self.assertRaises(Blocked): d._mock_request_admit('fixture-free')
            self.assertTrue(d.guard.stopped)

    def test_large_stdin_backpressure_has_absolute_deadline(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            start=time.monotonic()
            with self.assertRaises(Blocked):
                bounded_command(['/usr/bin/python3','-c','import time; time.sleep(5)'],
                                env={'HOME':root,'PATH':'/usr/bin:/bin'},input=b'x'*1024*1024,timeout=.1)
            self.assertLess(time.monotonic()-start,2)

    def test_exact_input_id_is_deterministic_and_turn_scoped(self):
        first=input_id('s_fixture',1)
        self.assertTrue(first.startswith('msg_via'))
        self.assertEqual(len(first),29)
        self.assertEqual(first,input_id('s_fixture',1))
        self.assertNotEqual(first,input_id('s_fixture',2))
        with self.assertRaises(Blocked): input_id('s_fixture',True)

    def test_config_switch_restarts_only_owned_daemon(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.daemon=Identity(123,9)
            with mock.patch.object(d,'stop') as stop:
                result=d.observe('set_inherit',{'settings':{'skills':'off'}})
            stop.assert_called_once()
            self.assertEqual(d.inherit_settings,{'skills':False})
            self.assertTrue(result['daemon_restart_required'])

    def test_large_resume_has_private_prompt_file_and_only_handle_stdin(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); prompt='p'*65536; observed=[]
            def command(argv,**kwargs):
                path=Path(argv[argv.index('--prompt-file')+1])
                observed.append(path)
                self.assertEqual(path.read_text(),prompt)
                self.assertEqual(path.stat().st_mode&0o777,0o600)
                self.assertEqual(kwargs['input'],b'h_memory_only\n')
                return 0,b'{"turn":"s_fixture/2","effective":{"effort":null,"max_steps":null}}',b''
            d=Driver('release','fp','pin',root/'evidence',execute=command,initialize=False)
            fake_cli_generation(d)
            d._binary=root/'release'; d.handles['s_fixture']='h_memory_only'
            with mock.patch.object(d,'spending_check'),mock.patch.object(d,'_admit_model'):
                d.via(['resume','s_fixture','--prompt',prompt])
            self.assertFalse(observed[0].exists())

    def test_direct_paid_session_and_requested_override_never_post(self):
        import opencode_safety as safety
        for actual,requested in [('paid/model',None),(safety.FREE_IDENTITY,'paid/model')]:
            with self.subTest(actual=actual,requested=requested),tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
                calls=[]
                class HTTP:
                    def request(self,method,path,body=None):
                        calls.append((method,path))
                        if path=='/api/integration': return 200,b'{"data":[]}'
                        provider,model=actual.split('/')
                        return 200,json.dumps({'data':{'id':'ses_fixture','model':{'providerID':provider,'id':model}}}).encode()
                d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
                d._http=HTTP()
                body={'id':'msg_fixture','text':'fixture'}
                if requested: body['model']={'providerID':'paid','id':'model'}
                with mock.patch.object(d,'ensure_vendor'),self.assertRaises(Blocked):
                    d.vendor('POST','/api/session/ses_fixture/prompt',body)
                self.assertTrue(d.guard.stopped)
                self.assertFalse(any(method=='POST' for method,_ in calls))

    def test_direct_admission_expired_after_metadata_never_posts(self):
        import opencode_safety as safety
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d._http=mock.Mock(); d.phase_public_left=1
            def preflight(**_):
                d.last_model=safety.FREE_IDENTITY; d.phase_deadline=time.monotonic()-1
            with mock.patch.object(d,'ensure_vendor'),mock.patch.object(d,'spending_check',side_effect=preflight),self.assertRaises(Blocked):
                d.vendor('POST','/api/session/ses_fixture/prompt',{'text':'fixture'})
            d._http.request.assert_not_called()

    def test_sse_trickling_header_deadline_shutdown_and_threads_join(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            stopped=threading.Event()
            class Socket:
                def shutdown(self,_): stopped.set()
            class Connection:
                sock=Socket()
                def connect(self): pass
                def request(self,*_,**__): pass
                def getresponse(self):
                    while not stopped.wait(.01): pass  # Headers never finish.
                    raise OSError('closed synthetic transport')
                def close(self): stopped.set()
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.vendor_identity=Identity(19,2); d.proc=mock.Mock()
            d._http=types.SimpleNamespace(origin='http://127.0.0.1:123',password=b'fixture')
            started=time.monotonic()
            with mock.patch('opencode_driver.http.client.HTTPConnection',return_value=Connection()), \
                 mock.patch('opencode_driver.SSE_HANDSHAKE_SECONDS',.05),self.assertRaises(Blocked):
                d.start_event_capture()
            self.assertLess(time.monotonic()-started,1)
            self.assertTrue(stopped.is_set())
            self.assertIsNone(d._event_watchdog)
            self.assertIsNone(d._event_thread)

    def test_finish_keeps_password_guard_until_final_sink_clear(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            class Proc:
                def read(self,*_): return b'OPENCODE_PASSWORD=memory-private-fixture\0'
            d.vault.read_once(Proc(),Identity(7,5))
            with mock.patch.object(d,'stop',return_value={'proven':True}), \
                 mock.patch('opencode_driver.safety.verify_binary'), \
                 mock.patch.object(d,'secrecy_scan',return_value={'secret_absent':True,'payload_captures':0}):
                d.finish()
            with self.assertRaises(Blocked): d._record('final',{'value':'memory-private-fixture'})
            d.clear_sensitive()
            self.assertFalse(d.vault.leaks(b'memory-private-fixture'))

    def test_long_run_lifecycle_reuses_held_vendor_without_new_turn(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.vendor_identity=Identity(19,2); helper=Identity(20,3); d.proc=mock.Mock()
            d.proc.alive.return_value=True; d._lifecycle_schema={'paths':{}}
            d._long_run_sample={'vendor':d.vendor_identity,'completed_tail':True,'helper':helper.report()}
            with mock.patch.object(d,'stop') as stop,mock.patch.object(d,'via') as via:
                d._prepare_lifecycle('long_run')
            stop.assert_not_called(); via.assert_not_called(); d.proc.verify.assert_called_once_with(helper)
            d._long_run_sample['vendor']=Identity(19,99)
            with self.assertRaises(Blocked): d._prepare_lifecycle('long_run')

    def test_actual_driver_adapter_helper_shapes_and_pinned_local_recipes(self):
        """Synthetic proc plus real local fixture materialization; no vendor or CLI."""
        from opencode_cases import DriverAdapter
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); project=root/'project'; project.mkdir(mode=0o700)
            (project/'.opencode').mkdir(mode=0o700)
            (project/'AGENTS.md').write_text('private fixture')
            d=Driver('release','fp','pin',root/'evidence',initialize=False)
            d.project=project; d.vendor_identity=Identity(19,2); d.proc=mock.Mock()
            d.proc.alive.return_value=True; d.inventory=mock.Mock()
            d.inventory.check.return_value=True
            try:
                config=d._materialize_fixture('never-ask',project,{'provider':'mock','mcp':True,
                       'plugin':True,'hook':True,'fake_lsp':True,'subagent':'oclive-ask'})
                self.assertEqual(config['mcp']['servers']['via-fixture']['type'],'local')
                self.assertEqual(config['agents']['oclive-ask']['permissions'][0]['action'],'shell')
                source=(Path(config['plugins'][0])/'index.js').read_text()
                self.assertIn("async setup(ctx)",source)
                self.assertIn("ctx.tool.hook('execute.before'",source)
                self.assertNotIn('package.json',source)
                self.assertEqual([row['name'] for row in d.mock_providers['never-ask'].script],['subagent','shell'])
                for number,kind in enumerate(('mcp','plugin','hook','lsp'),21):
                    compile((project/'.opencode'/f'{kind}-helper.py').read_text(),'<private helper>','exec')
                    (d._helper_folder(project)/(kind+'.json')).write_text(json.dumps(
                        {'pid':number,'start_ticks':3,'kind':kind,'password_key_absent':True,'ready':True}))
                with mock.patch.object(d,'_descends_from',return_value=True):
                    raw=DriverAdapter(d).execute('helper_observations')
                self.assertEqual(raw,{'mcp_started':True,'plugin_started':True,'hook_started':True,
                    'fake_lsp_started':True,'package_inventory_clean':True,'binary_inventory_clean':True})
            finally:
                for provider in d.mock_providers.values(): provider.__exit__(None,None,None)

    def test_partial_cancel_uses_typed_live_progress_and_bound_fake_pool_proof(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); point='wire.http.body_after_prefix'; name='qualification_body_prefix_cancel_preserves_offset_and_stop_pool'
            reply={'turn':'s_fixture/2','state':'running','already_terminal':False,
                   'cancel':{'outcome':'requested','cleanup':'pending'}}
            d=Driver('release','fp','pin',root/'evidence',execute=lambda *_,**__: (0,json.dumps(reply).encode(),b''),initialize=False)
            d._binary=root/'release'; d.env={'VIA_FAILPOINT_DIR':str(root)}; d.handles['s_fixture']='memory-handle'
            d.daemon=Identity(7,9); d._armed[point]=1; d._event_thread=object()
            d.proc=mock.Mock(); d.proc.alive.return_value=True
            d._lock_rows=mock.Mock(return_value={path:[(7,'FLOCK')] for path in d._lock_paths()})
            d.owned_replies=[{'session_id':'s_fixture','vendor_session_id':'ses_fixture'}]
            d.build_hashes={'release':'a'*64,'failpoints':'b'*64}
            d.fake_gate_manifest={'verified':True,'via_builds':dict(d.build_hashes),'source_sha256':'c'*64,'tests':[name]}
            (root/(point+'.1.ack')).write_text(json.dumps({'point':point,'occurrence':1,'pid':7,'action':'pause'}))
            d.via(['cancel','s_fixture'])
            result=d.observe('cancel_timing',{'session':'s_fixture'})
            self.assertTrue(result['cancel_returned']); self.assertTrue(result['prompt_held'])
            self.assertFalse(result['native_user_interrupted'])
            self.assertEqual(result['sourcebound_pool_test']['test_name'],name)
            d.fake_gate_manifest['tests']=[]
            with self.assertRaises(Blocked): d.observe('cancel_timing',{'session':'s_fixture'})

    def test_claimed_near_limit_queue_parking_crash_and_successor_fake(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); project=root/'project'; project.mkdir(); calls=[]; inbox=[]; crashed=False
            d=Driver('release','fp','pin',root/'evidence',initialize=False)
            d.daemon=Identity(7,9); d.proc=mock.Mock(); d.proc.gone.return_value=True
            d.proc.alive.return_value=False
            expected=input_id('s_fixture',2)
            schema={'paths':{'/api/session/{sessionID}/prompt':{'post':{'requestBody':{'content':{'application/json':{'schema':
                    {'type':'object','properties':{'id':{'type':'string'},'text':{'type':'string'},'delivery':{'type':'string','enum':['queue']}},
                     'required':['id','text'],'additionalProperties':False}}}}}}}}
            def via(args):
                nonlocal crashed
                calls.append(('via',args[0]))
                if args[0]=='status': return {'active_turn':{'n':2,'phase':'submitting'}}
                if args[0]=='resume':
                    if '--prompt' in args and len(args[args.index('--prompt')+1])>1000:
                        prompt=args[args.index('--prompt')+1]
                        self.assertEqual(len(json.dumps(prompt).encode())+len(json.dumps(str(project.resolve())).encode()),1024*1024-8192)
                        return {'turn':'s_fixture/2'}
                    inbox.remove(expected); return {'turn':'s_fixture/3'}
                if args[0]=='wait': return {'state':'unknown' if args[1].endswith('/2') else 'completed'}
                raise AssertionError(args)
            def vendor(method,path,body=None):
                calls.append((method,path))
                if method=='POST' and path.endswith('/prompt'):
                    if body.get('delivery')=='queue': inbox.append(body['id'])
                    return {'status':200,'body':{'data':{'id':body['id'],'sessionID':'ses_fixture'}}}
                if method=='POST': return {'status':200,'body':{'interrupted':True}}
                return {'status':200,'body':{'data':[{'id':value} for value in inbox]}}
            def observe(kind,target):
                if kind=='seam_target': return {'generation':'v_owned','session':'s_fixture','request':expected}
                return {'events':[{'type':'session.inbox.cancelled','data':{'inboxID':expected}}]}
            with mock.patch.object(d,'fixture',return_value=project),mock.patch.object(d,'start'), \
                 mock.patch.object(d,'_mock_turn',return_value=({'session_id':'s_fixture'}, {'vendor_session_id':'ses_fixture','turn':1})), \
                 mock.patch.object(d,'via',side_effect=via),mock.patch.object(d,'vendor',side_effect=vendor), \
                 mock.patch.object(d,'observe',side_effect=observe),mock.patch.object(d,'seam'),mock.patch.object(d,'seam_wait'), \
                 mock.patch.object(d,'_operation',return_value={'owned':True}),mock.patch.object(d,'_served_schema',return_value=schema), \
                 mock.patch.object(d,'_vendor_sid',return_value='ses_fixture'),mock.patch.object(d.journal,'_signal') as kill, \
                 mock.patch.object(d,'stop') as stop,mock.patch.object(d,'ensure_vendor'):
                result=d._near_limit_inbox({'count':2,'prompt_bytes':1024*1024-8192})
            self.assertTrue(result['successor_completed']); self.assertEqual(result['owned_deletes'],1)
            self.assertEqual(result['foreign_deletes'],0); self.assertIn('synthetic',result['seed_source'])
            self.assertEqual(len(inbox),1); kill.assert_called_once(); self.assertEqual(stop.call_count,2)

    def test_marker_release_releases_parent_and_child_channels(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); d=Driver('release','fp','pin',root/'evidence',initialize=False)
            d.project=root; channel=root/'channel'; channel.mkdir(); d.helper_channels[str(root)]=channel
            d.observe('helper_release',{'helper':'marker'})
            self.assertTrue((channel/'marker.release').exists())
            self.assertTrue((channel/'marker-parent.release').exists())

    def test_auxiliary_config_readback_empty_agent_catalog_is_not_required(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.project=Path(root); d.provider_endpoints={}
            identity='opencode/mimo-v2.6-flash-free'
            info={'model':identity,'agents':{name:{'model':identity} for name in
                  ('via','title','summary','compaction','explore','general','build','plan')}}
            class HTTP:
                def request(self,method,path,*,timeout=None):
                    if '/api/agent' in path: raise AssertionError('empty inventory cannot prove config')
                    return 200,json.dumps([{'type':'document','info':info}]).encode()
            d._http=HTTP()
            schema={'components':{'schemas':{'Config.InfoEncoded':{'properties':
                    {'model':{},'agents':{},'providers':{}}}}}}
            with mock.patch.object(d,'_served_schema',return_value=schema):
                self.assertEqual(len(d._auxiliary_bindings(identity)),9)
                info['agents']['title']['model']='paid/model'
                with self.assertRaises(Blocked): d._auxiliary_bindings(identity)
            self.assertTrue(d.guard.stopped)

    def test_release_original_pause_after_second_occurrence_guard(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); d=Driver('release','fp','pin',root/'evidence',initialize=False)
            d.daemon=Identity(7,9); d.env={'VIA_FAILPOINT_DIR':str(root)}
            point='opencode.identity.before_lookup'
            d._armed[point]=2; d._armed_history={(point,1),(point,2)}
            (root/(point+'.1.ack')).write_text(json.dumps({'pid':7,'action':'pause','occurrence':1}))
            d.seam_release(point,1)
            self.assertTrue((root/(point+'.1.release')).exists())
            self.assertFalse((root/(point+'.2.release')).exists())

    def test_old_seam_acknowledgement_cannot_be_rebound_to_new_target(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root); point='routes.opencode.before_event_read'
            d=Driver('release','fp','pin',root/'evidence',initialize=False)
            d.phase_kind='failpoints'; d.daemon=Identity(7,9); d._token='private-fixture'
            d.env={'VIA_FAILPOINT_DIR':str(root)}
            (root/(point+'.1.ack')).write_text('{}')
            with mock.patch.object(d,'_host_record',return_value={'server_id':'v_owned'}),self.assertRaises(Blocked):
                d.seam(point,1,'pause',target={'generation':'v_owned'})
            self.assertFalse((root/(point+'.json')).exists())

    def test_lifecycle_materializes_fresh_barrier_and_uses_release_build(self):
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            d.daemon=Identity(1,1); order=[]
            d._lifecycle_schema={'paths':{}}
            with mock.patch.object(d,'stop',side_effect=lambda:order.append('stop')), \
                 mock.patch.object(d,'fixture',return_value=Path(root)) as fixture, \
                 mock.patch.object(d,'start',side_effect=lambda kind,p:order.append(kind)), \
                 mock.patch.object(d,'via',return_value={'session_id':'s_fixture','turn':'s_fixture/1'}), \
                 mock.patch.object(d,'ensure_vendor'), \
                 mock.patch.object(d,'_operation',return_value={'started':True,'owned':True}):
                d._prepare_lifecycle('tool_during')
            self.assertEqual(order,['stop','release'])
            self.assertEqual(fixture.call_args.args[1]['helper'],'tool')

    def test_fake_cli_preflight_and_free_reply_pipeline(self):
        """Exercise actual driver config/start/reply methods with synthetic locks and CLI."""
        import opencode_safety as safety
        with tempfile.TemporaryDirectory(prefix='via-ocdriver-') as root:
            root=Path(root)
            for name in ('release','fp'):
                (root/name).write_bytes(b'fixture'); (root/name).chmod(0o500)
            bindir=root/'pinned'; bindir.mkdir(mode=0o700)
            pinned=bindir/'opencode'; pinned.write_bytes(b'pinned fixture'); pinned.chmod(0o500)
            digest=safety.sha256(pinned); bindir.chmod(0o500)
            procroot=root/'proc'; procroot.mkdir()
            class Proc:
                def __init__(self): self.root=procroot
                def stat(self,pid): return {'pid':pid,'start_ticks':7,'state':'S','ppid':1}
                def alive(self,identity): return identity.start_ticks==7
                def verify(self,identity):
                    if identity.start_ticks!=7: raise Blocked('fixture PID reused')
            calls=[]
            envelope={'session_id':'s_fixture','turn':1,'revision':0,'state':'completed','failure':None,
                      'stop_reason':'end_turn','vendor_version':'2.0.22','vendor_session_id':'ses_fixture',
                      'final_text':'VIA FREE READY','structured_output':None,'steps':1,'cancel':None,
                      'cost':{'usd':0,'scope':'turn'},'usage':{'scope':'turn','input_tokens':1,
                      'cached_input_tokens':0,'output_tokens':1},'warnings':[],'denied_actions':[],
                      'denied_actions_total':0,'auto_declined_requests_total':0}
            def command(argv,**kwargs):
                calls.append((argv,kwargs))
                if argv[1:3]==['daemon','status']: return 0,b'{"pid":123}',b''
                if argv[1]=='describe':
                    return 0,json.dumps({'harness':'opencode','capabilities':{'params':{
                        'max_steps':{'support':'unsupported'}}}}).encode(),b''
                if argv[1]=='spawn' and '--max-steps' in argv:
                    return 2,b'',b'{"code":-32602,"message":"refused","data":{"kind":"invalid_params","field":"max_steps"}}'
                if argv[1]=='spawn':
                    return 0,b'{"session_id":"s_fixture","turn":"s_fixture/1","handle":"h_memory","effective":{"effort":null,"max_steps":null}}',b''
                if argv[1]=='wait': return 0,json.dumps(envelope).encode(),b''
                raise AssertionError('unapproved fake command')
            d=Driver(root/'release',root/'fp',pinned,root/'evidence',proc=Proc(),execute=command)
            for name in ('state','runtime','helpers','home'):
                getattr(d,name).mkdir(mode=0o700)
            d.namespace=d.state/'namespace'; d.namespace.mkdir(mode=0o700)
            project=d.evidence/'project'; project.mkdir(mode=0o700)
            (project/'.opencode').mkdir(mode=0o700)
            d.fixtures['fixture']={'path':project,'config':{}}
            d.env={'HOME':str(d.home),'PATH':str(d.helpers)+':/usr/bin:/bin',
                   'LANG':'C.UTF-8','VIA_STATE_DIR':str(d.state),'VIA_RUNTIME_DIR':str(d.runtime)}
            locks=[]
            for path in d._lock_paths():
                path.touch(); info=path.stat()
                locks.append('1: FLOCK ADVISORY WRITE 123 '+
                             f'{__import__("os").major(info.st_dev):02x}:{__import__("os").minor(info.st_dev):02x}:{info.st_ino} 0 EOF')
            (procroot/'locks').write_text('\n'.join(locks))
            original=safety.verify_binary
            with mock.patch.object(safety,'verify_binary',side_effect=lambda path,*a,**kw:original(path,digest)), \
                    mock.patch.object(d,'fixture',return_value=project), \
                    mock.patch.object(d,'_anchor_rows',return_value=[]):
                proof=d.observe('fresh_max_steps',{'max_steps':1})
            self.assertEqual(proof['class'],'invalid_params')
            self.assertTrue(proof['watch_before'])
            self.assertFalse(any(argv[1]=='models' for argv,_ in calls))
            d.set_phase_budget(1,0)
            with mock.patch.object(d,'spending_check',side_effect=lambda **kw:setattr(d,'last_model',safety.FREE_IDENTITY)):
                receipt=d.via(['spawn','--harness','opencode','--model',safety.FREE_IDENTITY,
                               '--background','--prompt','VIA FREE READY'])
                result=d.via(['wait',receipt['turn']])
            self.assertEqual(result['final_text'],'VIA FREE READY')
            self.assertEqual(d.phase_public_left,0)
            self.assertNotIn(b'h_memory',b''.join(p.read_bytes() for p in d.evidence.glob('*.json')))


if __name__=='__main__':
    unittest.main()
