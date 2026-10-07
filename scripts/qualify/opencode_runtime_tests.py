#!/usr/bin/env python3
"""Fake and owned loopback runtime regressions for OpenCode packet §13."""
import errno
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import types
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

import opencode_driver as runtime
import opencode_safety as safety


class RuntimeTests(unittest.TestCase):
    def driver(self, root, **kwargs):
        return runtime.Driver('release', 'failpoints', 'pinned', Path(root)/'evidence',
                              initialize=False, **kwargs)

    def test_sse_headers_then_two_seconds_idle_still_deliver_event(self):
        release=threading.Event()
        class Handler(BaseHTTPRequestHandler):
            protocol_version='HTTP/1.1'
            def do_GET(self):
                self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.end_headers()
                release.wait(2.2)
                try:
                    self.wfile.write(b'data: {"type":"session.idle","data":{"sessionID":"ses_fixture"}}\n\n')
                    self.wfile.flush(); release.wait(3)
                except OSError: pass
            def log_message(self,*args): pass
        server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        server.daemon_threads=False
        thread=threading.Thread(target=server.serve_forever,name='owned-idle-sse-mock')
        thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root,proc=mock.Mock())
                d.vendor_identity=safety.Identity(10,20)
                d._http=types.SimpleNamespace(origin='http://127.0.0.1:'+str(server.server_port),password=b'fake-private')
                d.phase_deadline=time.monotonic()+8
                try:
                    d.start_event_capture()
                    deadline=time.monotonic()+4
                    while not d.events and d.events_error is None and time.monotonic()<deadline: time.sleep(.02)
                    self.assertIsNone(d.events_error)
                    self.assertEqual(d.events[0]['type'],'session.idle')
                finally:
                    release.set(); d.close_event_capture()
                    self.assertIsNone(d._event_thread); self.assertIsNone(d._event_watchdog)
        finally:
            release.set(); server.shutdown(); server.server_close(); thread.join(3)
            self.assertFalse(thread.is_alive())

    def test_lock_fd_churn_skipped_but_live_unreadable_directory_blocks(self):
        for mode in ('closed','esrch','unreadable'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                root=Path(root); d=self.driver(root)
                d.namespace=root/'namespace'; d.namespace.mkdir(); lock=d.namespace/'server.lock'; lock.touch()
                row=lock.stat(); lockid=f'{os.major(row.st_dev):02x}:{os.minor(row.st_dev):02x}:{row.st_ino}'
                procroot=root/'proc'; fdinfo=procroot/'10'/'fdinfo'; fdinfo.mkdir(parents=True)
                (fdinfo/'4').touch(); (fdinfo/'5').touch()
                proc=mock.Mock(root=procroot)
                proc.stat.return_value={'pid':10,'start_ticks':20}
                def read(identity,name):
                    if name=='fdinfo/4':
                        code=errno.ESRCH if mode=='esrch' else errno.ENOENT
                        try: raise OSError(code,'closed',name)
                        except OSError as cause: raise safety.Blocked('owned process observation unreadable') from cause
                    return ('lock: FLOCK '+lockid).encode()
                proc.read.side_effect=read; d.proc=proc; anchor=safety.Identity(10,20)
                if mode in {'closed','esrch'}: self.assertEqual(d._lock_holders(anchor),[anchor.report()])
                else:
                    original=Path.iterdir
                    def listing(path):
                        if path==fdinfo: raise PermissionError(errno.EACCES,'unreadable',str(path))
                        return original(path)
                    with mock.patch.object(Path,'iterdir',listing), self.assertRaisesRegex(safety.Blocked,'scan unverifiable'):
                        d._lock_holders(anchor)

    def test_finish_closes_every_provider_after_stop_or_provider_failure(self):
        for failing_provider in (False,True):
            with self.subTest(failing_provider=failing_provider), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); calls=[]
                providers=[]
                for number in range(2):
                    def close(*args,number=number):
                        calls.append(number)
                        if failing_provider and number==0: raise safety.Blocked('mock stop failure')
                    providers.append(types.SimpleNamespace(__exit__=close))
                d.mock_providers=dict(enumerate(providers))
                with mock.patch.object(d,'stop',side_effect=safety.Blocked('owned stop uncertainty')):
                    reason='owned mock provider cleanup unverified' if failing_provider else 'owned stop uncertainty'
                    with self.assertRaisesRegex(safety.Blocked,reason): d.finish()
                self.assertEqual(calls,[0,1]); self.assertFalse(d.mock_providers)

    def test_finish_stops_real_non_daemon_loopback_threads_after_stop_error(self):
        from opencode_cases import LoopbackProvider
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            d=self.driver(root)
            providers=[LoopbackProvider(),LoopbackProvider()]
            for provider in providers: provider.__enter__()
            d.mock_providers=dict(enumerate(providers))
            try:
                self.assertTrue(all(not provider.thread.daemon for provider in providers))
                with mock.patch.object(d,'stop',side_effect=safety.Blocked('private stop uncertainty')):
                    with self.assertRaisesRegex(safety.Blocked,'private stop uncertainty'): d.finish()
                self.assertTrue(all(not provider.thread.is_alive() for provider in providers))
            finally:
                for provider in providers: provider.__exit__(None,None,None)

    def test_finish_releases_publication_before_provider_close_when_stop_fails(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            d=self.driver(root); observed=[]
            def close(*args): observed.append(d._publication_release.is_set())
            d.mock_providers={'publication':types.SimpleNamespace(__exit__=close)}
            with mock.patch.object(d,'stop',side_effect=safety.Blocked('journal recovery uncertainty')):
                with self.assertRaisesRegex(safety.Blocked,'journal recovery uncertainty'): d.finish()
            self.assertEqual(observed,[True])

    def test_wait_supervision_has_margin_over_cli_deadline(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            calls=[]
            def execute(*args,**kwargs):
                calls.append(kwargs); return 4,b'',b''
            d=self.driver(root,execute=execute); d._binary=Path('release')
            with self.assertRaisesRegex(safety.Blocked,'private daemon unreachable'):
                d.via(['wait','s_fixture/1','--timeout-ms','180000'])
            self.assertGreater(calls[0]['timeout'],180)

    def test_bearer_handle_is_registered_for_subsequent_secret_scan(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            reply={'session_id':'s_fixture','turn':'s_fixture/1','handle':'h_fake_bearer',
                   'effective':{'effort':None,'max_steps':None}}
            d=self.driver(root,execute=lambda *args,**kwargs:(0,json.dumps(reply).encode(),b''))
            d._binary=Path('release')
            with mock.patch.object(d,'spending_check'),mock.patch.object(d,'_admit_model'):
                d.via(['spawn','--background','--prompt','fixture'])
            path=d.evidence/'nested'/'output.json'; path.parent.mkdir(); path.write_text('{"echo":"h_fake_bearer"}')
            self.assertIn(b'h_fake_bearer',d.secret_forms)
            self.assertFalse(d.secrecy_scan()['secret_absent'])

    def test_secrecy_scan_covers_logs_nested_output_and_labels_protected_sources(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            root=Path(root); d=self.driver(root); d.secret_forms=[b'fixture-secret']
            d.namespace=root/'namespace'; d.namespace.mkdir(); d.home=root/'home'; d.home.mkdir()
            project=root/'project'; project.mkdir(); config=project/'opencode.json'
            config.write_text('{"synthetic":"fixture-secret"}')
            d.fixtures={'fixture':{'path':project}}
            auth=d.home/'auth.json'; auth.write_text('fixture-secret')
            database=d.namespace/'opencode.db'; database.write_bytes(b'fixture-secret')
            log=d.namespace/'logs'/'vendor.log'; log.parent.mkdir(); log.write_text('clean')
            nested=d.evidence/'nested'/'output.json'; nested.parent.mkdir(); nested.write_text('{}')
            original=Path.read_bytes
            def read(path):
                if path in {auth,database}: raise AssertionError('credential content read')
                return original(path)
            with mock.patch.object(Path,'read_bytes',read):
                report=d.secrecy_scan(); self.assertTrue(report['secret_absent'])
                self.assertGreaterEqual(report['metadata_only_files'],2)
                self.assertEqual(report['synthetic_source_files'],1)
                for path in (log,nested):
                    path.write_text('fixture-secret')
                    self.assertFalse(d.secrecy_scan()['secret_absent']); path.write_text('clean')

    def test_fresh_max_steps_uses_only_mock_selector(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            d=self.driver(root); d.namespace=Path(root)/'namespace'; calls=[]
            def via(args):
                calls.append(args)
                if args[0]=='describe': return {'capabilities':{'params':{'max_steps':{'support':'refused'}}}}
                return {'cli_error':{'error':{'code':-32602,'data':{'kind':'invalid_params','field':'max_steps'}}}}
            with mock.patch.object(d,'fixture',return_value=Path(root)),mock.patch.object(d,'start'), \
                 mock.patch.object(d,'_anchor_rows',return_value=[]),mock.patch.object(d,'via',side_effect=via):
                result=d._fresh_max_steps(1)
            self.assertEqual(result['prompt_submits'],0)
            self.assertTrue(all(args[args.index('--model')+1]==safety.MOCK_IDENTITY for args in calls))

    def test_credentials_directory_contents_are_metadata_only(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            d=self.driver(root); d.home.mkdir()
            directory=d.home/'credentials'; directory.mkdir()
            protected=directory/'entry.json'; protected.write_text('synthetic credential')
            original=Path.read_bytes
            def read(path):
                if path==protected: raise AssertionError('credential directory contents read')
                return original(path)
            with mock.patch.object(Path,'read_bytes',read):
                scan=d.secrecy_scan()
            self.assertEqual(scan['metadata_only_files'],1)

    def test_fixture_synthetic_source_exception_never_allows_password_or_handle(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            root=Path(root); d=self.driver(root)
            project=root/'project'; project.mkdir(); source=project/'opencode.json'
            d.fixtures={'fixture':{'path':project}}; d.secret_forms=[b'synthetic-provider']
            d.handles={'s_fixture':'h_memory_bearer'}
            proc=mock.Mock(); proc.read.return_value=safety.PASSWORD_KEY+b'=private-password\0'
            d.vault.read_once(proc,safety.Identity(10,20))
            for value,clean in (('synthetic-provider',True),('h_memory_bearer',False),('private-password',False)):
                with self.subTest(value=value):
                    source.write_text(json.dumps({'key':value}))
                    self.assertIs(d.secrecy_scan()['secret_absent'],clean)

    def test_shell_requests_do_not_consume_model_spending_or_budget(self):
        for path in ('/api/shell','/api/session/ses_fixture/shell'):
            with self.subTest(path=path), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); d._http=mock.Mock(); d._http.request.return_value=(204,b'')
                with mock.patch.object(d,'ensure_vendor'),mock.patch.object(d,'spending_check') as spend, \
                     mock.patch.object(d,'_admit_model') as admit:
                    self.assertEqual(d.vendor('POST',path,{'command':'true'})['status'],204)
                spend.assert_not_called(); admit.assert_not_called()
