"""Fake-only review regressions for OpenCode qualification §13 (round 25)."""
import http.client
import http.server
import json
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

import opencode_cases as cases
import opencode_driver as transport
import opencode_driver_tests as driver_tests
import opencode_reply as replies
import opencode_reply_tests as reply_tests
import opencode_safety as safety


class ReviewTests(unittest.TestCase):
    def driver(self,root,body):
        return reply_tests.ReplyTests().driver(root,body)

    def retained(self,driver):
        return reply_tests.ReplyTests().retained(driver)

    def test_preflight_budget_covers_bootstrap_and_three_sentinels_but_not_an_extra_request(self):
        phase=next(row for row in cases.PHASES if row.name=='preflight')
        self.assertEqual(phase.public_turns,0)
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=self.driver(root,{})
            d.set_phase_budget(phase.public_turns,phase.mock_turns)
            # Four turns, each observed to cause one VIA and one auxiliary request.
            for _turn in range(4):
                d._mock_request_admit('fixture-free');d._mock_request_admit('fixture-free')
            with self.assertRaisesRegex(safety.Blocked,'mock model request ceiling exhausted'):
                d._mock_request_admit('fixture-free')
            self.assertTrue(d.guard.stopped);self.assertEqual(d.phase_mock_left,0)

    def test_mock_turn_receipt_is_persisted_before_a_later_failure_and_cleanup_totals_it(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root, cases.LoopbackProvider() as provider:
            d=self.driver(root,{});project=d.project
            d.fixtures={'project-boundary':{'path':project,'config':{}}}
            d.mock_providers={'project-boundary':provider}
            d.start=mock.Mock();d.set_phase_budget(0,8)
            provider.admit_request=d._mock_request_admit
            def cli(args):
                if args[0]=='spawn':
                    parts=provider.endpoint.split(':')
                    for _ in range(2):
                        connection=http.client.HTTPConnection('127.0.0.1',int(parts[-1].split('/')[0]),timeout=2)
                        try:
                            connection.request('POST','/v1/chat/completions',
                                json.dumps({'model':'fixture-free','messages':[]}))
                            response=connection.getresponse();response.read()
                            self.assertEqual(response.status,200)
                        finally: connection.close()
                    return {'session_id':'s_FAKE','turn':'s_FAKE/1'}
                return {'state':'completed'}
            d.via=mock.Mock(side_effect=cli)
            d._mock_turn(project,'FAKE prompt')
            paths=list(d.evidence.glob('*mock-receipt.json'))
            self.assertEqual(len(paths),1,'per-turn received count was not persisted')
            record=json.loads(paths[0].read_text())
            self.assertEqual(record['received_requests'],2)
            self.assertEqual(record['fixture'],'project-boundary')
            d.stop=mock.Mock(return_value={'proven':True,'processes_gone':True})
            d.secrecy_scan=mock.Mock(return_value={'secret_absent':True,'payload_captures':0})
            with mock.patch.object(safety,'verify_binary'):
                cleanup=d.finish()
            self.assertEqual(cleanup.get('mock_traffic',{}).get('received_requests'),2)

    def test_nonstring_origins_are_unverifiable(self):
        for value in (5,True,None,[],{},b'http://127.0.0.1:1234'):
            with self.subTest(value_type=type(value).__name__):
                self.assertEqual(replies.origin_class(value),{'class':'non-loopback-or-unverifiable'})

    def test_numeric_origin_through_real_http_preserves_result_and_original_bound_block(self):
        body={'providers':{'oclive-mock':{'settings':{'baseURL':5}}}}
        raw=json.dumps(body).encode()+b' '*256
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self,*_args): pass
            def do_GET(self):
                self.send_response(200);self.end_headers();self.wfile.write(raw)
        server=http.server.HTTPServer(('127.0.0.1',0),Handler)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            for limit in (4096,len(json.dumps(body).encode())+4):
                with self.subTest(limit=limit),tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
                    d=self.driver(root,{})
                    d._http=safety.OwnedHTTP('http://127.0.0.1:'+str(server.server_port),
                        safety.Identity(71,123),mock.Mock(),b'FAKE-password',limit=limit)
                    if limit==4096:
                        self.assertEqual(d.vendor('GET','/api/config')['body'],body)
                    else:
                        with self.assertRaisesRegex(safety.Blocked,'HTTP response exceeds bound'):
                            d.vendor('GET','/api/config')
                        self.assertFalse(self.retained(d)['replies'][-1]['complete'])
        finally:
            server.shutdown();server.server_close();thread.join(5)
            self.assertFalse(thread.is_alive())

    def test_capture_failure_in_transport_finally_preserves_the_original_block(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=self.driver(root,{})
            d._http=safety.OwnedHTTP('http://127.0.0.1:1234',safety.Identity(71,123),
                                     mock.Mock(),b'FAKE-password')
            with mock.patch.object(safety.http.client,'HTTPConnection') as connection, \
                 mock.patch.object(safety,'_AbsoluteHTTPDeadline') as timer, \
                 mock.patch.object(replies,'reply_projection',side_effect=RuntimeError('FAKE-private-projector-text')):
                connection.return_value.getresponse.return_value.status=200
                timer.return_value.__enter__.return_value.guard.side_effect=safety.Blocked('FAKE original deadline')
                with self.assertRaisesRegex(safety.Blocked,'FAKE original deadline'):
                    d.vendor('GET','/api/info')
            record=self.retained(d)
            self.assertTrue(record['replies'][-1]['projection_failed'])
            self.assertNotIn('FAKE-private-projector-text',json.dumps(record))

    def test_capture_context_failure_is_fixed_evidence_and_does_not_replace_a_result(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=self.driver(root,{'data':[]})
            d.reply_evidence.context=mock.Mock(side_effect=RuntimeError('FAKE-context-text'))
            result=d.vendor('GET','/api/config')
            self.assertEqual(result['status'],200)
            error=safety.Blocked('FAKE later schema block')
            d.reply_evidence.block(error)
            record=self.retained(d)
            self.assertTrue(record['replies'][-1]['projection_failed'])
            self.assertNotIn('FAKE-context-text',json.dumps(record))

    def test_unapproved_cli_identity_latches_before_catalogue_or_server_acquisition(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=driver_tests.DriverTests().catalog_driver(root,{'data':[]})
            d._catalog=mock.Mock(side_effect=safety.Blocked('FAKE catalogue called'))
            with self.assertRaisesRegex(safety.Blocked,'paid or unapproved model identity'):
                d.spending_check(args=['spawn','--model','FAKE-paid/FAKE-model'])
            self.assertTrue(d.guard.stopped)
            d._catalog.assert_not_called();d.ensure_vendor.assert_not_called()
            d._http.request.assert_not_called()

    def test_unapproved_direct_identity_latches_before_any_catalogue_wait(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=self.driver(root,{})
            d._http.request.return_value=(200,b'{"data":{"id":"ses_FAKE","model":{"providerID":"oclive-mock","id":"fixture-free"}}}')
            d._catalog=mock.Mock(side_effect=safety.Blocked('FAKE catalogue called'))
            with self.assertRaisesRegex(safety.Blocked,'paid or unapproved model identity'):
                d.vendor('POST','/api/session/ses_FAKE/prompt',
                    {'model':{'providerID':'FAKE-paid','id':'FAKE-model'}})
            self.assertTrue(d.guard.stopped);d._catalog.assert_not_called()
            d._http.request.assert_not_called();d.ensure_vendor.assert_not_called()

    def test_unapproved_checked_catalogue_identity_never_polls(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=self.driver(root,{'data':[]})
            d._await_owned=mock.Mock(side_effect=safety.Blocked('FAKE poll called'))
            with self.assertRaisesRegex(safety.Blocked,'paid or unapproved model identity'):
                d._catalog('FAKE-paid/FAKE-model')
            self.assertTrue(d.guard.stopped);d._await_owned.assert_not_called()

    def test_catalogue_transport_failure_does_not_relabel_a_previous_poll_as_current(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            other={'data':[{'providerID':'FAKE-other','id':'FAKE-other',
                'cost':[{'input':0,'output':0,'cache':{'read':0,'write':0}}]}]}
            d=self.driver(root,{})
            d._http.request.side_effect=[(200,json.dumps(other).encode()),safety.Blocked('FAKE second read unavailable')]
            with mock.patch.object(transport.time,'sleep'), \
                    self.assertRaisesRegex(safety.Blocked,'FAKE second read unavailable'):
                d._catalog(safety.MOCK_IDENTITY)
            record=json.loads(next(d.evidence.glob('*catalog-block.json')).read_text())
            self.assertIsNone(record['http_status']);self.assertEqual(record['models'],[])

    def test_integration_read_follows_catalogue_readiness(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=driver_tests.DriverTests().catalog_driver(root,{'data':[]})
            seen=[]
            def catalog(_identity):
                seen.append('catalog-ready');return {safety.MOCK_IDENTITY:{'free':True}}
            def request(_method,path,**_kwargs):
                seen.append('integration');return 200,b'{"data":[]}'
            d._catalog=mock.Mock(side_effect=catalog);d._http.request.side_effect=request
            self.assertTrue(d.spending_check(args=['spawn','--model',safety.MOCK_IDENTITY])['structural_spending_controls'])
            self.assertEqual(seen,['catalog-ready','integration'])

    def test_projected_identifiers_are_not_multicomponent_paths(self):
        body={'id':'home/alice/private','model':safety.MOCK_IDENTITY,
              'providers':{'home/alice/private':{},'oclive-mock':{}}}
        record=replies.reply_projection(json.dumps(body).encode(),lambda _:False)
        self.assertIsNone(record['projection']['id'])
        self.assertEqual(record['projection']['model'],safety.MOCK_IDENTITY)
        self.assertEqual(record['projection']['providers'],{'oclive-mock':{}})

    def test_cli_projection_uses_the_allowlisted_verb(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=self.driver(root,{})
            d._binary=Path('FAKE-via');d.execute=mock.Mock(return_value=(0,b'[]',b''))
            with self.assertRaisesRegex(safety.Blocked,'CLI reply object missing'):
                d.via(['FAKE-private-verb'])
            record=self.retained(d)
            self.assertEqual(record['replies'][-1]['route'],'unknown')
            self.assertNotIn('FAKE-private-verb',json.dumps(record))

    def test_reply_block_reasons_use_current_vault_synthetic_and_bearer_protection(self):
        for secret,category in (('FAKE-vault-secret','vault'),('FAKE-synthetic-secret','synthetic'),
                                ('h_FAKE-bearer-secret','bearer')):
            with self.subTest(category=category),tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
                d=self.driver(root,{'id':'ses_FAKE'})
                if category=='vault': d.vault.leaks=mock.Mock(side_effect=lambda raw:secret.encode() in raw)
                elif category=='synthetic': d.secret_forms.append(secret.encode())
                else:d.bearer_forms.add(secret.encode())
                d.vendor('GET','/api/session/ses_FAKE')
                error=safety.Blocked('FAKE reason '+secret)
                d.reply_evidence.block(error)
                record=self.retained(d)
                self.assertEqual(record['blocking']['reason'],'blocking reason matched protected material')
                self.assertNotIn(secret,json.dumps(record))
                self.assertFalse(getattr(error,'reply_retention_failed',False))

    def test_sse_failure_is_written_by_main_thread_only(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self,*_args): pass
            def do_GET(self):
                self.send_response(200);self.end_headers()
                self.wfile.write(b'data: {"type":7,"data":{"sessionID":"ses_FAKE"}}\n\n');self.wfile.flush()
        server=http.server.HTTPServer(('127.0.0.1',0),Handler)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
                d=self.driver(root,{})
                d.vendor_identity=safety.Identity(71,123);d.proc=mock.Mock()
                d._http.origin='http://127.0.0.1:'+str(server.server_port);d._http.password=b'FAKE-password'
                original=d.reply_evidence.emit;writers=[]
                def emit(*args):
                    writers.append(threading.get_ident());return original(*args)
                d.reply_evidence.emit=emit
                try:
                    d.start_event_capture();end=time.monotonic()+5
                    while d.events_error is None and time.monotonic()<end:time.sleep(.01)
                    self.assertIsNotNone(d.events_error)
                    self.assertEqual(writers,[],'SSE observer wrote evidence concurrently')
                    with self.assertRaisesRegex(safety.Blocked,'native stream observation failed'):
                        d.observe('native_events','ses_FAKE')
                    self.assertTrue(writers)
                    self.assertTrue(all(writer==threading.get_ident() for writer in writers))
                    self.retained(d)
                finally:d.close_event_capture()
        finally:
            server.shutdown();server.server_close();thread.join(5)
            self.assertFalse(thread.is_alive())

    def test_reply_captured_before_post_read_identity_check_is_marked_unverified(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreview-') as root:
            d=self.driver(root,{})
            proc=mock.Mock();proc.verify.side_effect=[None,safety.Blocked('FAKE owned identity changed')]
            d._http=safety.OwnedHTTP('http://127.0.0.1:1234',safety.Identity(71,123),proc,b'FAKE-password')
            with mock.patch.object(safety,'_bounded_response') as response:
                def bounded(*_args,**kwargs):
                    kwargs['observe'](200,b'{"id":"ses_FAKE"}',complete=True)
                    return 200,None,b'{"id":"ses_FAKE"}'
                response.side_effect=bounded
                with self.assertRaisesRegex(safety.Blocked,'FAKE owned identity changed'):
                    d.vendor('GET','/api/config')
            self.assertIs(self.retained(d)['replies'][-1].get('identity_verified'),False)
