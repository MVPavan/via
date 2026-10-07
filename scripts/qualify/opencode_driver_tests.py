#!/usr/bin/env python3
"""Fake-only driver checks for vendors/opencode.md §§2–4 and 13."""
import importlib.util
import json
import tempfile
import time
import threading
import types
from unittest import mock
import unittest
from pathlib import Path

from opencode_safety import Blocked, Identity
from opencode_driver import Driver, strict_reply, read_pages, bounded_command, input_id


class DriverTests(unittest.TestCase):
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
                def request(self,method,path):
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
                def verify(self,identity):
                    if identity.start_ticks!=7: raise Blocked('fixture PID reused')
            calls=[]
            envelope={'session_id':'s_fixture','turn':1,'state':'completed','failure':None,
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
                    return 2,b'',b'{"error":{"code":-32602,"message":"refused","data":{"kind":"invalid_params","field":"max_steps"}}}'
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
