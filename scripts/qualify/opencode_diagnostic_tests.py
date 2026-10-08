"""Offline mock-only diagnostics and review regressions (vendor packet §13)."""
import contextlib
import io
import json
from pathlib import Path
import sqlite3
import tempfile
import time
import unittest
from unittest import mock

import opencode
import opencode_cases as cases
import opencode_driver as runtime
import opencode_safety as safety
from opencode_c1_tests import envelope
from opencode_safety_tests import fake_cli_generation


class DiagnosticTests(unittest.TestCase):
    def test_diagnostic_success_has_a_distinct_nonzero_main_exit(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory(dir=opencode.SCRATCHPAD) as folder:
            root=Path(folder);work=root/'private';work.mkdir(mode=0o700)
            args=SimpleNamespace(self_test=False,phase=[],evidence=root/'evidence',acquire=False,
                opencode=root/'FAKE-pin',via_release=root/'release',via_failpoints=root/'fp',
                fake_gate_manifest=root/'manifest',diagnostic_mock_case=['usage'])
            class Driver:
                def __init__(self,**_kwargs):
                    self.vault=safety.PasswordVault();self.proc=safety.ProcReader(root/'proc')
                def prepare(self):pass
                def observe(self,*_args):return {}
                def finish(self):return {'proven':True,'stopped':True}
                def clear_sensitive(self):pass
            def phase(_adapter,name,**_kwargs):
                return [{'case':'preflight' if name=='preflight' else 'usage',
                         'disposition':'gate','result':'pass','checks':[]}]
            with mock.patch.object(opencode,'arguments',return_value=args), \
                 mock.patch.object(opencode,'self_test',return_value=([],1)), \
                 mock.patch.object(opencode,'verify_fake_gates',return_value={}), \
                 mock.patch.object(opencode,'system_tools_preflight',return_value={}), \
                 mock.patch.object(opencode,'private_recovery_root',return_value=root), \
                 mock.patch.object(opencode.runroots,'create_run_root',return_value=work), \
                 mock.patch.object(opencode.runroots,'check_ancestors',return_value={}), \
                 mock.patch.object(opencode.runroots,'discovery_strings',return_value={}), \
                 mock.patch.object(opencode.runroots,'remove_run_root',return_value={'removed':True}), \
                 mock.patch.object(safety,'system_git_templates',return_value=({},{})), \
                 mock.patch.object(safety,'verify_binary'),mock.patch.object(runtime,'Driver',Driver), \
                 mock.patch.object(cases,'run_phase',side_effect=phase),contextlib.redirect_stdout(io.StringIO()):
                rc=opencode.main([])
            summary=json.loads((args.evidence/'summary.json').read_text())
            self.assertEqual(summary['result'],'diagnostic_pass')
            self.assertEqual(summary['runner_source'],opencode.runner_source_manifest())
            self.assertEqual(rc,4)

    def test_redaction_blocks_casefolded_and_repeated_url_encoded_root_leftovers(self):
        import urllib.parse
        from opencode_diagnostic import redact
        root=Path('/FAKE/private/run-root')
        for text in (str(root).upper(),urllib.parse.quote(str(root),safe='').lower(),
                     urllib.parse.quote(urllib.parse.quote(str(root),safe=''),safe='')):
            with self.subTest(text=text),self.assertRaisesRegex(safety.Blocked,'run root'):
                redact(text,root)

    def test_mock_log_lines_name_session_or_location_context_association(self):
        from opencode_diagnostic import capture
        from opencode_ownership import OwnershipRegistry
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);work=root/'private';work.mkdir();evidence=root/'evidence';evidence.mkdir()
            owned=OwnershipRegistry(evidence);owned.register(work,'vendor-private')
            project=work/'project'
            (work/'opencode.log').write_text('INFO location services booted directory='+str(project)+'\n'
                'ERROR FAKE location failure\nERROR sessionID=ses_FAKE_enrolled FAKE session failure\n'
                'ERROR sessionID=ses_FAKE_foreign NEVER_RETAIN_FOREIGN\n')
            rows=capture(work,owned,{'ses_FAKE_enrolled':{'model':safety.MOCK_IDENTITY,'project':project}})
            self.assertEqual([row.get('association') for row in rows['log_lines']],
                             ['location-context','session-id'])
            self.assertNotIn('NEVER_RETAIN_FOREIGN',json.dumps(rows))

    def test_url_encoded_run_root_is_redacted(self):
        import urllib.parse
        from opencode_diagnostic import redact
        root=Path('/FAKE/private/run-root')
        text='FAKE '+urllib.parse.quote(str(root),safe='')
        self.assertEqual(redact(text,root),'FAKE <private-run-root>')

    def test_guard_mock_only_blocks_even_a_public_provider_override(self):
        origin='http://127.0.0.1:37777'
        guard=safety.SpendingGuard(owned_mock_origins={origin},public_free=True,mock_only=True)
        with self.assertRaisesRegex(safety.Blocked,'paid or unapproved model identity'):
            guard.check(integration={'data':[]},environment_names=safety.VENDOR_ENVIRONMENT,
                catalog={safety.FREE_IDENTITY:{'free':True}},provider='opencode',
                model='mimo-v2.6-flash-free',project_providers={'opencode':origin},cost=0)
        self.assertTrue(guard.stopped)

    def test_bootstrap_provider_without_fixture_does_not_hide_usage_receipt(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as root:
            d=runtime.Driver('release','fp','pin',Path(root)/'evidence')
            project=Path(root)/'project';helpers=project/'.opencode';helpers.mkdir(parents=True)
            (helpers/'tool-helper.py').write_text('# FAKE helper, never executed\n')
            d.project=project;d.fixtures={'usage-cache':{'path':project,'config':{}}}
            provider=SimpleNamespace(receipt=lambda:{'received':True,'model_matches':True,'requests':3},script=[])
            d.mock_providers={'bootstrap-1':SimpleNamespace(),'usage-cache':provider}
            self.assertEqual(d.observe('mock_receipt')['requests'],3)
            d.observe('long_run_barrier_prepare',{'helper':'tool','barrier_seconds':1800,'project':project})
            self.assertEqual(provider.script[-1]['name'],'shell')
            d.project=Path(root)/'unregistered'
            with self.assertRaisesRegex(safety.Blocked,'owned mock provider absent'):d.observe('mock_receipt')

    def test_exception_sites_retain_code_metadata_only(self):
        from opencode_diagnostic import exception_sites
        try:raise KeyError('FAKE-private-message-and-path')
        except KeyError as error:rows=exception_sites(error)
        self.assertTrue(rows)
        self.assertEqual(rows[-1]['function'],'test_exception_sites_retain_code_metadata_only')
        self.assertEqual(rows[-1]['file'],'scripts/qualify/opencode_diagnostic_tests.py')
        self.assertNotIn('FAKE-private',json.dumps(rows))

    def test_usage_context_leaves_output_headroom_and_compaction_is_separate(self):
        configs=[]
        class Driver:
            def execute(self,operation,**args):
                if operation=='fixture':configs.append(args['config']);return 'FAKE-project'
                raise cases.EvidenceUnavailable('FAKE stop before submit')
        with self.assertRaises(cases.EvidenceUnavailable):cases.case_usage(Driver(),cases.Case('usage'))
        # Usage must leave input headroom above the fixture's 4096 output limit.
        self.assertGreaterEqual(configs[0]['context'],32768)

    def test_diagnostic_reads_flushed_logs_after_proven_stop(self):
        import opencode_diagnostic as diagnostic
        with tempfile.TemporaryDirectory() as root:
            d=runtime.Driver('release','fp','pin',Path(root)/'evidence',diagnostic_mock_only=True)
            d.stop=mock.Mock(return_value={'proven':True,'stopped':True})
            d.secrecy_scan=mock.Mock(return_value={'secret_absent':True,'payload_captures':0})
            records=[];d._record=lambda label,value:records.append((label,value))
            def capture(*_args,**_kwargs):
                return {'log_lines':['FAKE flushed error'] if d.stop.called else []}
            with mock.patch.object(diagnostic,'capture',side_effect=capture),mock.patch.object(safety,'verify_binary'):
                d.finish()
            rows=[value for label,value in records if label=='mock-diagnostic']
            self.assertEqual(rows[-1]['log_lines'],['FAKE flushed error'])

    def test_mode_selects_preflight_and_only_named_mock_cases(self):
        from opencode_diagnostic import selection
        with tempfile.TemporaryDirectory() as root:
            args=opencode.arguments(['--diagnostic-mock-case','usage','--via','release',
                '--via-failpoints','fp','--opencode','pin','--evidence',
                str(opencode.SCRATCHPAD/Path(root).name)])
        self.assertFalse(args.live)
        self.assertEqual(selection(args.diagnostic_mock_case),
                         {'preflight':('preflight',),'ledger':('usage',)})
        with contextlib.redirect_stderr(io.StringIO()),self.assertRaises(SystemExit):
            opencode.arguments(['--diagnostic-mock-case','cancel'])

    def test_mock_only_rejects_public_identity_before_acquisition(self):
        with tempfile.TemporaryDirectory() as root:
            d=runtime.Driver('release','fp','pin',Path(root)/'evidence')
            d.diagnostic_mock_only=True
            d.ensure_vendor=mock.Mock()
            with self.assertRaisesRegex(safety.Blocked,'paid or unapproved model identity'):
                d.spending_check(args=['spawn','--model',safety.FREE_IDENTITY])
            self.assertTrue(d.guard.stopped)
            d.ensure_vendor.assert_not_called()

    def test_raw_capture_requires_mock_session_and_redacts_before_writing(self):
        from opencode_diagnostic import capture
        from opencode_ownership import OwnershipRegistry
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);work=root/'private';work.mkdir()
            evidence=root/'evidence';evidence.mkdir()
            owned=OwnershipRegistry(evidence);owned.register(work,'vendor-private')
            db=work/'opencode.db'
            with sqlite3.connect(db) as conn:
                conn.execute('CREATE TABLE session_message(session_id TEXT,type TEXT,seq INT,data TEXT)')
                for sid in ('ses_FAKE_mock','ses_FAKE_public'):
                    error={'type':'FAKE_UnlistedError','message':'FAKE fixture '+str(work)+' pw_FAKE handle_FAKE'}
                    conn.execute('INSERT INTO session_message VALUES(?,?,?,?)',
                        (sid,'step',1,json.dumps({'error':error,'text':'NEVER_RETAIN_CONTENT'})))
            log=work/'opencode.log'
            log.write_text('ERROR sessionID=ses_FAKE_mock FAKE fixture '+str(work)+' pw_FAKE handle_FAKE\n'
                           'ERROR sessionID=ses_FAKE_public NEVER_PUBLIC_ERROR\n')
            rows=capture(work,owned,{'ses_FAKE_mock':{'model':safety.MOCK_IDENTITY,'case':'usage'}},
                         replacements=(b'pw_FAKE',b'handle_FAKE'))
            raw=json.dumps(rows)
            self.assertIn('FAKE_UnlistedError',raw)
            self.assertNotIn(str(work),raw)
            for forbidden in ('pw_FAKE','handle_FAKE','NEVER_RETAIN_CONTENT','NEVER_PUBLIC_ERROR'):
                self.assertNotIn(forbidden,raw)
            self.assertIn('<private-run-root>',raw)
            self.assertEqual(rows['sessions'][0]['sessionID'],'ses_FAKE_mock')
            # The actual evidence sink and whole ownership scan still apply.
            d=runtime.Driver('release','fp','pin',evidence,ownership=owned)
            d.vault._values[safety.Identity(1,1)]=b'pw_FAKE'
            d.secret_forms=[b'handle_FAKE']
            d._record('mock-diagnostic',rows)
            self.assertFalse(d.secrecy_scan()['secret_absent'])  # Original planted password still gates.
            log.unlink()
            self.assertTrue(d.secrecy_scan()['secret_absent'])
            with self.assertRaisesRegex(safety.Blocked,'mock diagnostic session identity unverifiable'):
                capture(work,owned,{'ses_FAKE_public':{'model':safety.FREE_IDENTITY,'case':'usage'}})

    def test_named_case_selection_has_zero_public_budget(self):
        calls=[]
        class Driver:
            def execute(self,name,**args):
                calls.append((name,args))
                if name=='build_hashes':return {'release':'a'*64,'test-failpoints':'b'*64}
        def fixture(_driver,case):case.check('FAKE predicate',True)
        with mock.patch.dict(cases.CASES,{'usage':fixture,'compaction':lambda *_:self.fail('unselected')}):
            rows=cases.run_phase(Driver(),'ledger',case_names=('usage',),mock_only=True)
        self.assertEqual([row['case'] for row in rows],['usage'])
        begin=next(args for name,args in calls if name=='phase_begin')
        self.assertEqual(begin['public_turns'],0)

    def test_terminal_failure_and_stop_reason_are_closed(self):
        for field in ('failure','stop_reason'):
            value=envelope()
            value[field]={'class':'FAKE_invalid','message':'FAKE'} if field=='failure' else 'FAKE_invalid'
            with self.subTest(field=field),self.assertRaises(safety.Blocked):
                runtime.strict_reply(json.dumps(value).encode(),'envelope')

    def test_close_explicit_deadline_covers_cli_settlement(self):
        with tempfile.TemporaryDirectory() as root:
            d=runtime.Driver('release','fp','pin',Path(root)/'evidence')
            fake_cli_generation(d);d._binary=Path('FAKE-via')
            d.execute=mock.Mock(side_effect=safety.Blocked('FAKE transport stop'))
            with self.assertRaisesRegex(safety.Blocked,'FAKE transport stop'):
                d.via(['close','s_FAKE','--deadline-ms','60000'])
            self.assertEqual(d.execute.call_args.kwargs['timeout'],100)

    def test_response_shape_is_reset_on_reused_handler(self):
        # A scripted reusable handler exercises the same parsed-request hook.
        saved=[]
        with cases.LoopbackProvider(retain_exchanges=True) as provider:
            def initialize(handler,*_args):saved.append(handler)
            with mock.patch('http.server.BaseHTTPRequestHandler.__init__',initialize):
                import socket
                connection=socket.create_connection(provider.server.server_address)
                connection.close()
                end=time.monotonic()+2
                while not saved and time.monotonic()<end:time.sleep(.01)
            self.assertTrue(saved)
            handler=saved[0];handler.connection=mock.Mock();handler.wfile=io.BytesIO()
            handler.send_response=mock.Mock();handler.send_header=mock.Mock();handler.end_headers=mock.Mock()
            for model in ('fixture-free','FAKE-paid'):
                body=json.dumps({'model':model,'messages':[]}).encode()
                handler.headers={'Content-Length':str(len(body))}
                handler.rfile=io.BytesIO(body)
                handler.do_POST()
            rows=provider.exchange_records()
            self.assertEqual(rows[0]['response']['message_role'],'assistant')
            self.assertNotIn('message_role',rows[1]['response'])
            self.assertEqual(rows[1]['response']['status'],403)
