#!/usr/bin/env python3
"""Fake reply-retention regressions for vendors/opencode.md §13."""
import hashlib
import json
import tempfile
import threading
import time
import http.server
import sys
import unittest
from pathlib import Path
from unittest import mock

from opencode_driver import Driver
from opencode_safety import Blocked, Identity


class ReplyTests(unittest.TestCase):
    def driver(self,root,body):
        d=Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
        d.project=d.evidence/'project'
        d.ensure_vendor=mock.Mock()
        d._http=mock.Mock()
        d._http.request.return_value=(200,json.dumps(body).encode())
        return d

    def retained(self,d):
        files=list(d.evidence.glob('*reply-block.json'))
        self.assertTrue(files,'blocked reply was not retained')
        return json.loads(files[-1].read_text())

    def test_config_endpoint_block_retains_ids_and_origin_classes_without_secrets(self):
        secret='FAKE-private-provider-token'
        handle='h_FAKE-private-handle'
        info={'model':'oclive-mock/fixture-free',
              'agents':{name:{'model':'oclive-mock/fixture-free'} for name in
                        ('via','title','summary','compaction','explore','general','build','plan')},
              'providers':{'oclive-mock':{'settings':{'baseURL':'http://127.0.0.1:1101/v1/'+secret,
                            'apiKey':secret},'headers':{'authorization':handle},
                            'models':{'fixture-free':{'cost':{'input':0,'output':0}}}}}}
        body={'data':[{'type':'document','info':info}]}
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,body);d.mock_origins={'http://127.0.0.1:1101','http://127.0.0.1:1102'}
            d.provider_endpoints={'oclive-mock':'http://127.0.0.1:1102/v1'}
            d.secret_forms.append(secret.encode());d.bearer_forms.add(handle.encode())
            d._served_schema=mock.Mock(return_value={'components':{'schemas':{
                'Config.InfoEncoded':{'properties':{'model':{},'agents':{},'providers':{}}}}}})
            with self.assertRaisesRegex(Blocked,'effective provider endpoints differ from owned fixture'):
                d._auxiliary_bindings('oclive-mock/fixture-free')
            record=self.retained(d);raw=json.dumps(record).encode()
            for forbidden in (secret.encode(),handle.encode(),b'http://',b'headers',b'apiKey'):
                self.assertNotIn(forbidden,raw)
            reply=record['replies'][-1]
            self.assertEqual(reply['http_status'],200)
            self.assertEqual(reply['projection']['data'][0]['info']['providers']['oclive-mock']
                             ['settings']['baseURL'],{'class':'loopback','host_port':'127.0.0.1:1101'})

    def test_cli_schema_block_retains_a_numeric_reply_field_not_error_text(self):
        body={'pid':71,'stopping':'FAKE-vendor-error-text','handle':'h_FAKE-secret'}
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,{});d._binary=Path('FAKE-via')
            d.execute=mock.Mock(return_value=(0,json.dumps(body).encode(),b''))
            with self.assertRaisesRegex(Blocked,'daemon stop.stopping: wrong field type'):
                d.via(['daemon','stop'])
            record=self.retained(d);raw=json.dumps(record).encode()
            self.assertNotIn(b'FAKE-vendor-error-text',raw);self.assertNotIn(b'h_FAKE-secret',raw)
            self.assertEqual(record['replies'][-1]['projection']['pid'],71)

    def test_openapi_hash_block_retains_hash_before_json_decoding(self):
        body={'info':{'version':'2.0.23','description':'FAKE-untrusted-error'},
              'headers':{'token':'FAKE-private-token'}}
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,body)
            d.vendor_identity=Identity(71,123)
            with self.assertRaisesRegex(Blocked,'pinned served OpenAPI differs from packet E7'):
                d._served_schema()
            reply=self.retained(d)['replies'][-1]
            self.assertEqual(reply['sha256'],hashlib.sha256(json.dumps(body).encode()).hexdigest())
            self.assertEqual(reply['projection']['info']['version'],'2.0.23')
            self.assertNotIn('FAKE-untrusted-error',json.dumps(reply))

    def test_invalid_json_retains_only_status_size_and_hash(self):
        raw=b'FAKE-private-invalid-json'
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,{})
            d._http.request.return_value=(200,raw)
            with self.assertRaisesRegex(Blocked,'vendor JSON shape unavailable'):
                d.vendor('GET','/api/config')
            reply=self.retained(d)['replies'][-1]
            self.assertEqual(reply['decode_status'],'invalid-json')
            self.assertEqual(reply['bytes'],len(raw))
            self.assertNotIn(raw.decode(),json.dumps(reply))

    def test_fresh_handle_and_header_values_cannot_be_mirrored_into_ids(self):
        from opencode_reply import reply_projection
        values=('h_FAKE-fresh','FAKE-header-secret','FAKE-password')
        body={'handle':values[0],'headers':{'x-private':values[1]},'password':values[2],
              'data':[{'id':value} for value in values]}
        record=reply_projection(json.dumps(body).encode(),lambda _raw:False)
        self.assertEqual(record['projection']['data'],[{'id':None}]*3)
        for value in values: self.assertNotIn(value,json.dumps(record))

    def test_untrusted_private_unicode_cannot_prevent_reply_retention(self):
        from opencode_reply import reply_projection
        body={'password':'FAKE-invalid-\ud800','id':'ses_FAKE'}
        record=reply_projection(json.dumps(body).encode(),lambda _:False)
        self.assertEqual(record['projection'],{'id':'ses_FAKE'})

    def test_foreground_cli_json_lines_retain_fields_and_redact_across_documents(self):
        from opencode_reply import reply_projection
        handle='h_FAKE-new-foreground'
        raw=json.dumps({'session_id':'s_FAKE','handle':handle}).encode()+b'\n'+ \
            json.dumps({'session_id':handle,'turn':1,'state':'failed'}).encode()
        record=reply_projection(raw,lambda _:False)
        self.assertEqual(record['decode_status'],'json-lines')
        self.assertEqual(record['projection'],[{'session_id':'s_FAKE'},
            {'session_id':None,'turn':1,'state':'failed'}])
        self.assertNotIn(handle,json.dumps(record))

    def test_foreign_or_credentialed_origin_is_never_retained_as_a_url(self):
        from opencode_reply import reply_projection
        for url,classification in (('http://user:FAKE-pass@127.0.0.1:1101/x','credentialed'),
                                   ('https://FAKE-foreign.example/x','non-loopback-or-unverifiable')):
            body={'providers':{'oclive-mock':{'settings':{'baseURL':url}}}}
            record=reply_projection(json.dumps(body).encode(),lambda _raw:False)
            self.assertEqual(record['projection']['providers']['oclive-mock']['settings']['baseURL'],
                             {'class':classification})
            self.assertNotIn(url,json.dumps(record));self.assertNotIn('FAKE-pass',json.dumps(record))

    def test_projection_limit_is_labelled_and_unknown_fields_are_dropped(self):
        from opencode_reply import reply_projection, PROJECTION_ITEMS, PROJECTION_BYTES
        record=reply_projection(json.dumps({'data':[{'id':'ses_FAKE'}]*(PROJECTION_ITEMS+1)}).encode(),
                                lambda _raw:False)
        self.assertTrue(record['truncated']);self.assertIsNone(record['projection'])
        self.assertLessEqual(len(json.dumps(record).encode()),PROJECTION_BYTES)

    def test_retention_failure_preserves_original_block_and_reports_uncertainty(self):
        import opencode_safety as safety
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,{'id':'ses_FAKE'})
            d.vendor('GET','/api/session/ses_FAKE')
            d.reply_evidence.emit=mock.Mock(side_effect=OSError('FAKE-writer-text'))
            error=Blocked('FAKE original reply mismatch')
            with self.assertRaisesRegex(Blocked,'FAKE original reply mismatch'):
                with d.reply_evidence.check(): raise error
            record=safety.blocking_record(error)
            self.assertTrue(record['reply_retention_failed'])
            self.assertEqual(record['reason'],'FAKE original reply mismatch')
            self.assertNotIn('FAKE-writer-text',json.dumps(record))

    def test_case_block_after_a_reply_return_links_recent_projection(self):
        import opencode_cases as cases
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,{'data':{'id':'ses_FAKE'}})
            adapter=cases.DriverAdapter(d)
            def case(_driver,_case):
                d.vendor('GET','/api/session/ses_FAKE/message')
                raise Blocked('FAKE message schema missing')
            record=cases.run_case(adapter,'FAKE-message-check',case)
            self.assertEqual(record['result'],'blocked')
            saved=self.retained(d)
            self.assertEqual(len(record['blocking']['reply_evidence']),1)
            self.assertEqual(saved['association'],'recent-context')
            self.assertEqual(saved['replies'][-1]['projection']['data']['id'],'ses_FAKE')

    def test_integration_and_session_identity_blocks_retain_the_evaluated_reply(self):
        for route,body,reason in (
                ('/api/integration',{'data':[]},'integration check unavailable'),
                ('/api/session/ses_FAKE',{'data':{'id':'ses_FAKE',
                    'model':{'providerID':'FAKE-paid','id':'FAKE-model'}}},
                 'paid or unapproved model identity')):
            with self.subTest(route=route), tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
                d=self.driver(root,{})
                def request(_method,path,*_args,**_kwargs):
                    return (503 if path==route=='/api/integration' else 200,
                            json.dumps(body if path==route else {'data':{'id':'ses_FAKE','model':{'providerID':'oclive-mock','id':'fixture-free'}}}).encode())
                d._http.request.side_effect=request
                d._catalog=mock.Mock(return_value={'oclive-mock/fixture-free':{'free':True}})
                d.vault.names=mock.Mock(return_value=frozenset())
                d._auxiliary_bindings=mock.Mock(return_value=['oclive-mock/fixture-free']*9)
                d.automatic_models_proven=True  # Earlier fixture proof, before this isolated integration refusal.
                with self.assertRaisesRegex(Blocked,reason):
                    d.spending_check(path='/api/session/ses_FAKE/prompt')
                reply=self.retained(d)['replies'][-1]
                self.assertEqual(reply['route'],d._reply_route(route))
                self.assertEqual(reply['projection'],body)

    def test_native_message_schema_block_retains_message_ids(self):
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,{'data':{'id':'msg_FAKE'}})
            with self.assertRaisesRegex(Blocked,'native history shape unavailable'):
                d.transcript('ses_FAKE')
            self.assertEqual(self.retained(d)['replies'][-1]['projection']['data']['id'],'msg_FAKE')

    def test_info_handshake_block_retains_reported_identity_and_version(self):
        from opencode_driver_tests import DriverTests
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d,_record,_pending=DriverTests().vendor_observer(root)
            d.proc.listener.return_value='http://127.0.0.1:1234'
            with mock.patch('opencode_driver.safety.OwnedHTTP') as http:
                http.return_value.request.return_value=(200,b'{"pid":71,"version":"2.0.24"}')
                with self.assertRaisesRegex(Blocked,'owned vendor handshake mismatch'):
                    d._observe_vendor()
            reply=self.retained(d)['replies'][-1]
            self.assertEqual(reply['projection'],{'pid':71,'version':'2.0.24'})

    def test_catalog_price_schema_block_retains_numeric_prices(self):
        from opencode_driver_tests import DriverTests
        body={'data':[{'providerID':'oclive-mock','id':'fixture-free',
                      'cost':[{'input':0,'output':0,'cache':{'read':0}}]}]}
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=DriverTests().catalog_driver(root,body)
            with self.assertRaisesRegex(Blocked,'catalog cost.cache: required field missing'):
                d.spending_check(args=['spawn','--model','oclive-mock/fixture-free'])
            reply=self.retained(d)['replies'][-1]
            self.assertEqual(reply['projection'],body)

    def test_owned_http_redirect_and_response_bound_retain_consumed_reply(self):
        import opencode_safety as safety
        for status,payload,limit,reason in (
                (302,b'{"id":"ses_FAKE"}',1024,'owned API redirect refused'),
                (200,b'{"id":"ses_FAKE"}',4,'HTTP response exceeds bound')):
            class Handler(http.server.BaseHTTPRequestHandler):
                def log_message(self,*_args): pass
                def do_GET(self):
                    self.send_response(status);self.end_headers()
                    self.wfile.write(payload)
            server=http.server.HTTPServer(('127.0.0.1',0),Handler)
            thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
            try:
                with self.subTest(status=status), tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
                    d=self.driver(root,{})
                    d._http=safety.OwnedHTTP('http://127.0.0.1:'+str(server.server_port),
                        Identity(71,123),mock.Mock(),b'FAKE-password',limit=limit)
                    with self.assertRaisesRegex(Blocked,reason): d.vendor('GET','/api/config')
                    reply=self.retained(d)['replies'][-1]
                    self.assertEqual(reply['http_status'],status)
                    self.assertEqual(reply['bytes'],len(payload) if limit==1024 else limit+1)
                    self.assertEqual(reply['complete'],limit==1024)
            finally:
                server.shutdown();server.server_close();thread.join(timeout=5)
                self.assertFalse(thread.is_alive())

    def test_cli_observation_bound_retains_partial_output_before_it_blocks(self):
        import opencode_driver
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,{})
            d._binary=Path(sys.executable);d.execute=opencode_driver.bounded_command
            d.env={'PATH':'/usr/bin:/bin','HOME':root}
            d.project=Path(root)
            with self.assertRaisesRegex(Blocked,'private command observation bound'):
                d.via(['-c','print(\'{"pid":71}\'+" "*(17*1024*1024))'])
            reply=self.retained(d)['replies'][-1]
            self.assertFalse(reply['complete'])
            self.assertGreater(reply['bytes'],opencode_driver.OBSERVATION_BYTES)
            self.assertTrue(reply['truncated'])
            self.assertIsNone(reply['projection'])

    def test_http_deadline_after_headers_retains_status_without_reading_body(self):
        import opencode_safety as safety
        with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
            d=self.driver(root,{})
            d._http=safety.OwnedHTTP('http://127.0.0.1:1234',Identity(71,123),
                                     mock.Mock(),b'FAKE-password')
            with mock.patch.object(safety.http.client,'HTTPConnection') as connection, \
                    mock.patch.object(safety,'_AbsoluteHTTPDeadline') as timer:
                response=connection.return_value.getresponse.return_value
                response.status=200
                timer.return_value.__enter__.return_value.guard.side_effect=Blocked('FAKE HTTP deadline')
                with self.assertRaisesRegex(Blocked,'FAKE HTTP deadline'):
                    d.vendor('GET','/api/info')
            reply=self.retained(d)['replies'][-1]
            self.assertEqual((reply['http_status'],reply['bytes'],reply['complete']),(200,0,False))
            response.read1.assert_not_called()

    def test_bad_sse_event_retains_projection_and_stops_all_threads(self):
        secret='FAKE-sse-private-token'
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self,*_args): pass
            def do_GET(self):
                self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
                body={'type':7,'data':{'sessionID':'ses_FAKE'},'headers':{'token':secret},'message':secret}
                self.wfile.write(b'data: '+json.dumps(body).encode()+b'\n\n');self.wfile.flush()
        server=http.server.HTTPServer(('127.0.0.1',0),Handler)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix='via-ocreply-') as root:
                d=self.driver(root,{})
                d.vendor_identity=Identity(71,123);d.proc=mock.Mock()
                d._http.origin='http://127.0.0.1:'+str(server.server_port)
                d._http.password=b'FAKE-private-password'
                try:
                    d.start_event_capture()
                    end=time.monotonic()+5
                    while d.events_error is None and time.monotonic()<end: time.sleep(.01)
                    self.assertEqual(d.events_error,'Blocked')
                    with self.assertRaisesRegex(Blocked,'native stream observation failed'):
                        d.observe('native_events','ses_FAKE')
                    record=self.retained(d)
                    self.assertEqual(record['replies'][-1]['projection']['type'],'unrecognized')
                    self.assertNotIn(secret,json.dumps(record))
                finally: d.close_event_capture()
                self.assertIsNone(d._event_thread);self.assertIsNone(d._event_watchdog)
        finally:
            server.shutdown();server.server_close();thread.join(timeout=5)
            self.assertFalse(thread.is_alive())
