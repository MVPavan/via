"""Mock retry/compaction diagnostic regressions (OpenCode packet §13 L4/L7)."""
import http.client
import json
import unittest

import opencode_cases as cases
import opencode_diagnostic as diagnostic
import opencode_driver as runtime
import opencode_safety as safety
from pathlib import Path
import tempfile
from unittest import mock


class MockCaseTests(unittest.TestCase):
    def test_lock_scan_skips_only_a_pidfd_proven_gone_process_with_a_present_stat(self):
        import errno,os
        for alive in (False,True,None):
            with self.subTest(alive=alive),tempfile.TemporaryDirectory() as folder:
                root=Path(folder);d=runtime.Driver('release','fp','pin',root/'evidence')
                d.namespace=root/'namespace';d.namespace.mkdir();lock=d.namespace/'server.lock';lock.touch()
                row=lock.stat();key=f'{os.major(row.st_dev):02x}:{os.minor(row.st_dev):02x}:{row.st_ino}'
                proc=root/'proc'
                for pid in (10,11):(proc/str(pid)/'fdinfo').mkdir(parents=True)
                (proc/'10/fdinfo/5').touch()
                d.proc=mock.Mock(root=proc)
                d.proc.stat.side_effect=lambda pid:{'pid':pid,'start_ticks':20}
                d.proc.alive.return_value=alive
                d.proc.read.return_value=('lock: FLOCK '+key).encode()
                d._record=mock.Mock();d._verify=mock.Mock();anchor=safety.Identity(10,20)
                listing=Path.iterdir
                def inaccessible(path):
                    if path==proc/'11/fdinfo':raise PermissionError(errno.EACCES,'FAKE hidden',str(path))
                    return listing(path)
                with mock.patch.object(Path,'iterdir',inaccessible):
                    if alive is False:self.assertEqual(d._lock_holders(anchor),[anchor.report()])
                    else:
                        with self.assertRaises(safety.Blocked) as block:d._lock_holders(anchor)
                        self.assertEqual(block.exception.process_liveness,alive)
                        d._record.assert_called_once()
                        self.assertEqual(d._record.call_args.args[1]['pid'],11)

    def test_inventory_block_retains_a_closed_difference_without_local_names(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);records=[]
            inventory=safety.Inventory([root]);inventory.record=records.append
            binary=root/'FAKE-private-name';binary.write_bytes(b'\x7fELF FAKE');binary.chmod(0o700)
            with self.assertRaisesRegex(safety.Blocked,'new or changed binary'):
                inventory.check()
            self.assertEqual(len(records),1)
            row=records[0]['differences'][0]
            self.assertEqual(row['change'],'added')
            self.assertEqual(row['size'],9)
            self.assertEqual(row['name_class'],'other')
            self.assertEqual(len(row['sha256']),64)
            self.assertNotIn(str(root),json.dumps(records))
            self.assertNotIn('FAKE-private-name',json.dumps(records))

    def test_long_run_script_outlasts_sampling_instead_of_using_native_two_minute_default(self):
        with tempfile.TemporaryDirectory() as folder:
            project=Path(folder);(project/'.opencode').mkdir();(project/'.opencode/tool-helper.py').touch()
            d=runtime.Driver('release','fp','pin',project/'evidence');d.project=project
            provider=mock.Mock(script=[])
            d.fixtures={'long-run':{'path':project}};d.mock_providers={'long-run':provider}
            d._operation('long_run_barrier_prepare',{'project':project,'helper':'tool','barrier_seconds':1800})
            self.assertEqual(provider.script[-1]['arguments'].get('timeout'),1800000)
            self.assertLess(cases.PHASES[[p.name for p in cases.PHASES].index('long_run')].seconds,
                            provider.script[-1]['arguments']['timeout']/1000)

    def test_direct_seed_retains_reply_context_when_it_blocks(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.reply_evidence.capture('vendor','/api/info',b'{"pid":31,"version":"2.0.22"}',http_status=503)
            d._check_ancestors=mock.Mock(side_effect=safety.Blocked('FAKE seed block'))
            with self.assertRaisesRegex(safety.Blocked,'FAKE seed block'):
                d.direct_seed('FAKE',{'schema_checked':True})
            records=list(d.evidence.glob('*reply-block.json'))
            self.assertEqual(len(records),1)
            self.assertEqual(json.loads(records[0].read_text())['replies'][0]['http_status'],503)

    def test_direct_seed_info_waits_only_for_verified_matching_startup_503(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            identity=safety.Identity(31,3);d._verify=mock.Mock()
            replies=[(503,b'{"pid":31,"version":"2.0.22"}'),
                     (200,b'{"pid":31,"version":"2.0.22"}')]
            d._request=mock.Mock(side_effect=replies)
            d._direct_seed_info(mock.sentinel.http,identity,runtime.time.monotonic()+1)
            self.assertEqual(d._request.call_count,2)
            self.assertEqual(d._verify.call_count,2)
            for status,raw in [(503,b'{"pid":32,"version":"2.0.22"}'),
                               (403,b'{"pid":31,"version":"2.0.22"}')]:
                d._request=mock.Mock(return_value=(status,raw))
                with self.assertRaisesRegex(safety.Blocked,'L11 direct identity/version mismatch'):
                    d._direct_seed_info(mock.sentinel.http,identity,runtime.time.monotonic()+1)
                d._request.assert_called_once()

    def test_cleanup_runtime_proof_uses_its_own_bound_after_phase_expiry(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);pin=root/'pin';pin.write_bytes(b'FAKE embedded bytes')
            d=runtime.Driver('release','fp',pin,root/'evidence')
            d.phase_deadline=1;d._observation_deadline=1
            d.embedded_runtime._pin_identity=d.embedded_runtime._identity(pin.lstat())
            def stop():
                self.assertEqual(d.embedded_runtime._find_bytes(b'embedded','FAKE'),5)
                return {'proven':True}
            d.stop=mock.Mock(side_effect=stop)
            d.secrecy_scan=mock.Mock(return_value={'secret_absent':True,'payload_captures':0})
            with mock.patch('opencode_driver.safety.verify_binary'):
                self.assertTrue(d.finish()['proven'])
            self.assertLess(d.embedded_runtime.deadline(),10)

    def test_l11_registers_the_mock_in_the_persistent_namespace_without_model_admission(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);d=runtime.Driver('release','fp','pin',root/'evidence')
            d.namespace=root/'namespace';d.namespace.mkdir(mode=0o700)
            d._refresh_inventory=mock.Mock()
            try:
                d._prepare_l11_mock_provider()
                config=json.loads((d.namespace/'opencode.json').read_text())
                self.assertEqual(config['model'],safety.MOCK_IDENTITY)
                provider=config['providers']['oclive-mock']
                self.assertEqual(provider['env'],[])
                self.assertEqual(safety.loopback_origin(provider['settings']['baseURL']),
                    safety.loopback_origin(d.mock_providers['l11-seed'].endpoint))
                with self.assertRaises(safety.Blocked):d._mock_request_admit('fixture-free',fixture='l11-seed')
                self.assertEqual(d.mock_providers['l11-seed'].requests,0)
                d._refresh_inventory.assert_called_once()
            finally:
                for provider in d.mock_providers.values():provider.__exit__(None,None,None)

    def test_long_run_finishes_snapshot_before_the_phase_watchdog_deadline(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            clock=[0.0];d.phase_deadline=90.0;d.vendor_identity=safety.Identity(41,1)
            helper=safety.Identity(42,2);d.helper_ledger={helper:{'kind':'tool'}}
            d.helper_generations={helper:d.vendor_identity};d.proc=mock.Mock()
            d.proc.alive.return_value=True;d.last_request={'receipt':{'session_id':'s_FAKE'}}
            d._l9_sessions=['s_FAKE']
            def observe(kind,*_):
                if clock[0]>=90:raise safety.Blocked('native stream observation failed')
                if kind=='memory_sample':return {'at':clock[0],'sessions':1}
                return {'events':[{'fixture':'x'*50001}]}
            d.observe=mock.Mock(side_effect=observe)
            with mock.patch('opencode_driver.time.monotonic',side_effect=lambda:clock[0]), \
                 mock.patch('opencode_driver.time.sleep',side_effect=lambda t:clock.__setitem__(0,clock[0]+t)):
                result=d._operation('memory_tail',{'deadline':90.0,'interval_seconds':5,
                    'sessions':['s_FAKE']})
            self.assertTrue(result['complete']);self.assertTrue(d._long_run_sample['completed_tail'])
            self.assertLess(clock[0],90)
            self.assertEqual(d.phase_deadline,90)

    def test_quota_mock_serves_the_pinned_classifiers_explicit_quota_code(self):
        with cases.LoopbackProvider('quota') as provider:
            connection=http.client.HTTPConnection(*provider.server.server_address,timeout=3)
            try:
                connection.request('POST','/v1/chat/completions',json.dumps({'model':'fixture-free'}))
                response=connection.getresponse();body=json.loads(response.read())
                self.assertEqual(response.status,400)
                self.assertEqual(body['error'].get('code'),'insufficient_quota')
            finally:connection.close()

    def test_missing_session_control_checks_mock_facts_then_refuses_all_provider_work(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.project=Path(folder);d.provider_endpoints={'oclive-mock':'http://127.0.0.1:1234/v1'}
            d.last_model=safety.MOCK_IDENTITY;d._vendor_sid=mock.Mock(return_value='ses_FAKE')
            d.ensure_vendor=mock.Mock();d._operation=mock.Mock();d.spending_check=mock.Mock()
            def vendor(method,path):
                if method=='DELETE':return {'status':204}
                if path.endswith('/ses_FAKE'):return {'status':404,'body':{}}
                return {'status':200,'body':{'data':[]}}
            d.vendor=mock.Mock(side_effect=vendor)
            def via(command):
                if command[0]=='resume':
                    self.assertTrue(d._metadata_active)
                    self.assertEqual(command,d._metadata_command)
                    return {'turn':'s_FAKE/3'}
                return {'failure':{'class':'resume_mismatch'}}
            d.via=mock.Mock(side_effect=via)
            self.assertEqual(d._missing_vendor_session('s_FAKE'),{'code':'resume_mismatch','creates':0})
            d.spending_check.assert_called_once_with(args=['spawn','--model',safety.MOCK_IDENTITY,
                '--cwd',str(d.project)])
            d.ensure_vendor.assert_called_once()
            self.assertFalse(d._metadata_active);self.assertIsNone(d._metadata_command)
            original=via
            def attempted(command):
                if command[0]=='resume':
                    with self.assertRaisesRegex(safety.Blocked,'model request during metadata acquisition'):
                        d._mock_request_admit('fixture-free')
                return original(command)
            d.via=mock.Mock(side_effect=attempted)
            with self.assertRaisesRegex(safety.Blocked,'missing-ID control attempted model work'):
                d._missing_vendor_session('s_FAKE')
            self.assertFalse(d._metadata_active);self.assertIsNone(d._metadata_command)

    def test_anchor_budget_covers_every_implemented_point_and_cold_capability_probe(self):
        phase=next(row for row in cases.PHASES if row.name=='anchor')
        # Fourteen fresh points, two extra LSP steps and one tool-after step;
        # retained long-run successor, cold probe and fixed margin are separate.
        self.assertEqual(phase.mock_turns,14*6+3+2+6+2)

    def test_near_limit_claim_is_settled_before_restart_without_unknown_predecessor(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.via=mock.Mock(side_effect=[{'outcome':'requested'},
                {'state':'cancelled','cancel':{'cleanup':'quiescent'}}])
            d._settle_near_limit_claim({'turn':'s_FAKE/2'})
            self.assertEqual(d.via.call_args_list,[
                mock.call(['cancel','s_FAKE','--turn','2']),
                mock.call(['wait','s_FAKE/2','--timeout-ms','180000'])])
            d.via=mock.Mock(side_effect=[{}, {'state':'unknown','cancel':{'cleanup':'quiescent'}}])
            with self.assertRaisesRegex(safety.Blocked,'near-limit predecessor not safely settled'):
                d._settle_near_limit_claim({'turn':'s_FAKE/2'})

    def test_indeterminate_write_waits_for_owned_drain_without_stopping_server(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.daemon=safety.Identity(41,1);d.vendor_identity=safety.Identity(43,3);d.anchor=safety.Identity(42,2)
            d._http=object();d.close_event_capture=mock.Mock()
            record={'generation':'FAKE-generation','anchor':d.anchor,'vendor_pid':43,'server_id':'FAKE-server'}
            d._host_record=mock.Mock(side_effect=[record,None])
            d.last_request={'receipt':{'turn':'s_FAKE/2'}}
            d._vendor_sid=mock.Mock(return_value='ses_FAKE')
            d.observe=mock.Mock(side_effect=lambda kind,*_: {'events':[]} if kind=='native_events' else {'FAKE':'target'})
            d._acknowledgements=mock.Mock(return_value=[{'_target':{'FAKE':'target'},'occurrence':1}])
            d.env['VIA_FAILPOINT_DIR']=folder
            d.via=mock.Mock(return_value={'state':'unknown'});d.proc=mock.Mock()
            d.proc.gone.return_value=False
            def gone(*_,**__):d.proc.gone.return_value=True
            d._await_gone=mock.Mock(side_effect=gone)
            result=d._operation('write_observation',{'session':'s_FAKE','turn_number':2,
                'point':'wire.http.body_after_prefix','context':{}})
            self.assertTrue(result['drained'])
            self.assertIsNone(d._http)
            d.close_event_capture.assert_called_once()
            self.assertEqual(d._host_record.call_count,2)
            d._await_gone.assert_called_once()
            self.assertEqual(d.via.call_count,1)

    def test_marker_positive_entry_survives_truthful_incomplete_full_scan(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.proc=mock.Mock();d.vendor_identity=safety.Identity(43,3)
            d._started_at=mock.Mock(return_value='2026-01-01T00:00:00Z')
            d.observe=mock.Mock(return_value={'helpers':[{'kind':'marker','pid':42,
                'start_ticks':2,'pgid':42}]})
            report={'scope':'server','processes':[{'pid':42,'started_at':'2026-01-01T00:00:00Z'}],
                'incomplete':True}
            d.owned_replies=[{'leftovers':report}]
            result=runtime.Driver.observe(d,'marker_helper')
            self.assertTrue(result['marker_matches'])
            self.assertTrue(result['host_leftovers']['incomplete'])
            report['processes']=[]
            with self.assertRaisesRegex(safety.Blocked,'exact-marker helper proof unavailable'):
                runtime.Driver.observe(d,'marker_helper')

    def test_block_projection_retains_closed_leftover_scope_and_incomplete_count(self):
        from opencode_reply import reply_projection
        raw=json.dumps({'leftovers':{'scope':'server','incomplete':True,'total':1,
            'processes':[{'pid':42,'started_at':'PRIVATE','comm':'PRIVATE'}]}}).encode()
        row=reply_projection(raw,lambda _:False)['projection']['leftovers']
        self.assertEqual(row,{'scope':'server','incomplete':True,'total':1,'processes':[{'pid':42}]})

    def test_never_ask_plugin_asks_only_actual_fixture_child_shell(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);d=runtime.Driver('release','fp','pin',root/'evidence')
            project=root/'FAKE-project';project.mkdir();(project/'.opencode').mkdir()
            with mock.patch.object(d,'_helper_fixture',return_value=project/'FAKE-helper.py'), \
                 mock.patch.object(cases,'LoopbackProvider') as provider:
                provider.return_value.endpoint='http://127.0.0.1:12345/v1'
                d._materialize_fixture('never-ask',project,{'provider':'mock','plugin':True,
                    'hook':True,'subagent':'oclive-ask'})
            code=(project/'.opencode/plugins/via-fixture/index.js').read_text()
            self.assertIn("ctx.permission.hook('evaluate'",code)
            self.assertIn("event.agent==='oclive-ask' && event.action==='shell'",code)
            self.assertIn("event.effect='ask'",code)

    def test_marker_loss_kills_only_vendor_leaving_host_control_for_exit_report(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.daemon=safety.Identity(41,1);d.anchor=safety.Identity(42,2)
            d.vendor_identity=safety.Identity(43,3);d._live_pin=mock.Mock()
            d._verify=mock.Mock();d.journal=mock.Mock();d._staged_signal=mock.Mock()
            result=d._operation('server_loss_for_marker',{'session':'s_FAKE'})
            import signal
            self.assertEqual(result,{'vendor_pid_only':True})
            d.journal._signal.assert_called_once_with(d.vendor_identity,signal.SIGKILL,role='vendor')
            d._staged_signal.assert_not_called()
            self.assertIsNone(d._killed_anchor)

    def test_lsp_probe_reads_the_served_top_level_configuration_array(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);d=runtime.Driver('release','fp','pin',root/'evidence')
            project=root/'FAKE-project';project.mkdir()
            expected={'command':['FAKE-python','FAKE-lsp'],'extensions':['.via_ocl_fixture']}
            d.fixtures={'lsp-probe':{'config':{'lsp':{'via-fixture':expected}}}}
            provider=mock.Mock();provider.receipt.return_value={'received':True,'model_matches':True,
                'read_calls':[{'id':'FAKE-call','schema_checked':True}]}
            d.mock_providers={'lsp-probe':provider}
            schema={'components':{'schemas':{'Config.InfoEncoded':{'properties':{'lsp':{}}}}}}
            with mock.patch.object(d,'fixture',return_value=project),mock.patch.object(d,'start'), \
                 mock.patch.object(d,'via',return_value={'session_id':'s_FAKE','turn':'s_FAKE/1'}), \
                 mock.patch.object(d,'ensure_vendor'),mock.patch.object(d,'_operation',return_value={'owned':True,'started':True}), \
                 mock.patch.object(d,'_served_schema',return_value=schema), \
                 mock.patch.object(d,'vendor',return_value={'status':200,'body':[
                    {'type':'document','info':{'lsp':{'via-fixture':expected}}}]}), \
                 mock.patch.object(d,'_vendor_sid',return_value='ses_FAKE'), \
                 mock.patch.object(d,'_await_native',side_effect=safety.Blocked('FAKE verified-read boundary')) as native:
                with self.assertRaisesRegex(safety.Blocked,'FAKE verified-read boundary'):d._lsp_probe()
            native.assert_called_once()

    def test_named_remaining_mock_cases_are_selectable_without_public_phases(self):
        names=('hostile_provider','compaction','config','never_ask','marker','auth',
               'credential_shape','write_cancel','foreign','transport_loss','identity',
               'long_run','anchor','error_shapes')
        selected=diagnostic.selection(names)
        self.assertEqual({name for rows in selected.values() for name in rows},{'preflight',*names})
        self.assertNotIn('free',selected)
        for name in ('free','continuity','cancel'):
            with self.assertRaises(Exception):diagnostic.selection([name])

    def test_compaction_fixture_answers_the_pinned_template_only_for_summary_request(self):
        prompt=('You MUST summarize the conversation above into a structured summary. '
                'You MUST use this format for your response.\n<template>\n'
                '## Objective\n## Work State\n</template>')
        with cases.LoopbackProvider() as provider:
            connection=http.client.HTTPConnection(*provider.server.server_address,timeout=3)
            try:
                body={'model':'fixture-free','messages':[{'role':'user','content':prompt}]}
                connection.request('POST','/chat/completions',json.dumps(body))
                reply=connection.getresponse();self.assertEqual(reply.status,200)
                result=json.loads(reply.read());text=result['choices'][0]['message']['content']
                for heading in ('## Objective','## Requirements','## Decisions','## Work State',
                                '### Completed','### Active','### Blocked','## Next Move',
                                '## Relevant Files','## Important Context'):
                    self.assertIn(heading,text)
                self.assertNotIn('<template>',text)
            finally:connection.close()

    def test_checkpoint_update_uses_the_same_summary_template(self):
        from opencode_mock_policy import compaction_request
        prompt=('Update the existing checkpoint in the conversation above into one consolidated summary. '
                'You MUST use this format for your response.\n<template>\n## Objective\n## Work State')
        self.assertTrue(compaction_request([{'role':'user','content':prompt}]))
        self.assertFalse(compaction_request([{'role':'user','content':'Reply MOCK READY.'}]))

    def test_hostile_phase_covers_eight_policy_attempts_title_bootstrap_and_margin(self):
        # Pinned retry policy: 1+10 primary, two title attempts, two bootstrap,
        # plus a fixed two-request margin for each of the eight providers.
        phase=next(row for row in cases.PHASES if row.name=='hostile')
        self.assertEqual(phase.mock_turns,2*((11+2+2+2)+(1+2+2+2)+(1+2+2+2)+(11+2+2+2)))
        self.assertEqual(phase.public_turns,0)

    def test_one_hostile_provider_cannot_consume_another_provider_budget(self):
        with tempfile.TemporaryDirectory() as folder:
            d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
            d.set_phase_budget(0,136)
            d._mock_provider_left={'FAKE-hostile':17}
            for _ in range(17):d._mock_request_admit('fixture-free',fixture='FAKE-hostile')
            with self.assertRaisesRegex(safety.Blocked,'mock provider request ceiling exhausted'):
                d._mock_request_admit('fixture-free',fixture='FAKE-hostile')
            self.assertTrue(d.guard.stopped)
            self.assertEqual(d.phase_mock_left,119)

    def test_hostile_payloads_reach_distinct_native_decoder_and_size_paths(self):
        for mode in ('malformed','oversized'):
            with self.subTest(mode=mode),cases.LoopbackProvider(mode=mode,secret='FAKE_secret') as provider:
                conn=http.client.HTTPConnection(*provider.server.server_address,timeout=5)
                try:
                    conn.request('POST','/chat/completions',json.dumps({'model':'fixture-free','stream':True}))
                    response=conn.getresponse();body=response.read()
                    self.assertEqual(response.getheader('Content-Type'),'text/event-stream')
                    self.assertTrue(body.startswith(b'data: '))
                    if mode=='malformed':self.assertTrue(body.endswith(b'\n\n'))
                    else:
                        self.assertGreater(len(body),10485760)
                        self.assertNotIn(b'\n',body)
                finally:conn.close()
