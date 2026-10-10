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
from opencode_safety_tests import fake_cli_generation


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
                    reason='owned stop uncertainty'  # Original cleanup failure takes precedence.
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
            fake_cli_generation(d)
            with self.assertRaisesRegex(safety.Blocked,'private daemon unreachable'):
                d.via(['wait','s_fixture/1','--timeout-ms','180000'])
            self.assertGreater(calls[0]['timeout'],180)

    def test_bearer_handle_is_registered_for_subsequent_secret_scan(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            reply={'session_id':'s_fixture','turn':'s_fixture/1','handle':'h_fake_bearer',
                   'effective':{'effort':None,'max_steps':None}}
            d=self.driver(root,execute=lambda *args,**kwargs:(0,json.dumps(reply).encode(),b''))
            fake_cli_generation(d)
            d._binary=Path('release')
            with mock.patch.object(d,'spending_check'),mock.patch.object(d,'_admit_model'):
                d.via(['spawn','--background','--prompt','fixture'])
            path=d.evidence/'nested'/'output.json'
            d.ownership.register(path.parent,'runner-evidence')
            path.parent.mkdir(); path.write_text('{"echo":"h_fake_bearer"}')
            self.assertIn(b'h_fake_bearer',d.secret_forms)
            self.assertFalse(d.secrecy_scan()['secret_absent'])

    def test_secrecy_scan_covers_logs_nested_output_and_labels_protected_sources(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            root=Path(root); d=self.driver(root); d.secret_forms=[b'fixture-secret']
            d.namespace=root/'namespace'; d.namespace.mkdir(); d.home=root/'home'; d.home.mkdir()
            project=root/'project'; project.mkdir(); config=project/'opencode.json'
            for folder in (d.namespace,d.home,project): d.ownership.register(folder,'vendor-private')
            config.write_text('{"synthetic":"fixture-secret"}')
            d.fixtures={'fixture':{'path':project}}
            auth=d.home/'auth.json'; auth.write_text('fixture-secret')
            database=d.namespace/'opencode.db'; database.write_bytes(b'fixture-secret')
            log=d.namespace/'logs'/'vendor.log'; log.parent.mkdir(); log.write_text('clean')
            nested=d.evidence/'nested'/'output.json'; nested.parent.mkdir(); nested.write_text('{}')
            d.ownership.register(nested.parent,'runner-evidence')
            original=Path.open
            def read(path,*args,**kwargs):
                if path in {auth,database}: raise AssertionError('credential content read')
                return original(path,*args,**kwargs)
            with mock.patch.object(Path,'open',read):
                report=d.secrecy_scan(); self.assertTrue(report['secret_absent'])
                self.assertGreaterEqual(report['metadata_only_files'],2)
                self.assertEqual(report['synthetic_source_files'],1)
                for path in (log,nested):
                    path.write_text('fixture-secret')
                    scan=d.secrecy_scan()
                    self.assertIs(scan['secret_absent'],path==log)
                    self.assertEqual(scan['vendor_private_synthetic_files'],int(path==log))
                    path.write_text('clean')

    def hostile_case(self, driver):
        from opencode_cases import HOSTILE_OUTPUT_VERBS, case_hostile_provider, run_case
        replies={'fixture':'fixture', 'turn':{'session_id':'s_fixture',
                 'envelope':{'state':'failed','vendor_session_id':'ses_fixture','final_text':None}, 'vendor_error_observed':True},
                 'mock_receipt':{'received':True,'model_matches':True},
                 'hostile_output_matrix':{'complete':True,'verbs':list(HOSTILE_OUTPUT_VERBS),'event_pages':1}}
        def execute(operation, **_args):
            return driver.secrecy_scan() if operation=='secrecy_scan' else replies[operation]
        return run_case(types.SimpleNamespace(execute=execute), 'hostile_provider', case_hostile_provider)

    def test_synthetic_namespace_log_passes_l4_and_finish(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            d=self.driver(root); d.namespace=d.state/'vendor'/'opencode'/'namespace'
            log=d.namespace/'log'/'vendor.log'; log.parent.mkdir(parents=True)
            log.write_bytes(b'fixture-provider-secret'); d.secret_forms=[b'fixture-provider-secret']
            with self.subTest(proof='l4'):
                self.assertEqual(self.hostile_case(d)['result'],'pass')
            with self.subTest(proof='finish'), mock.patch.object(d,'stop',return_value={'proven':True}), \
                 mock.patch.object(safety,'verify_binary'):
                self.assertTrue(d.finish()['stopped'])
            report=d.secrecy_scan()
            self.assertEqual(report['vendor_private_synthetic_files'],1)

    def test_password_and_handle_in_vendor_log_still_fail_l4_and_finish(self):
        for value in (b'private-password',b'h_private_bearer'):
            with self.subTest(value=value), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); d.namespace=d.state/'vendor'/'opencode'/'namespace'
                log=d.namespace/'log'/'vendor.log'; log.parent.mkdir(parents=True); log.write_bytes(value)
                proc=mock.Mock(); proc.read.return_value=safety.PASSWORD_KEY+b'=private-password\0'
                d.vault.read_once(proc,safety.Identity(10,20))
                d.handles={'s_fixture':'h_private_bearer'}
                d.secret_forms=[b'fixture-provider-secret',b'h_private_bearer']
                self.assertEqual(self.hostile_case(d)['result'],'fail')
                with mock.patch.object(d,'stop',return_value={'proven':True}), \
                     mock.patch.object(safety,'verify_binary'), \
                     self.assertRaisesRegex(safety.Blocked,'final secrecy scan failed'):
                    d.finish()

    def test_prior_bearer_stays_protected_in_vendor_log_after_rotation(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            replies=[{'session_id':'s_fixture','turn':'s_fixture/'+str(number),'handle':handle,
                      'effective':{'effort':None,'max_steps':None}}
                     for number,handle in ((1,'h_old_bearer'),(2,'h_next_bearer'))]
            d=self.driver(root,execute=mock.Mock(side_effect=[(0,json.dumps(reply).encode(),b'') for reply in replies]))
            fake_cli_generation(d)
            d._binary=Path('release')
            with mock.patch.object(d,'spending_check'),mock.patch.object(d,'_admit_model'):
                for _ in replies: d.via(['spawn','--background','--prompt','fixture'])
            d.namespace=d.state/'vendor'/'opencode'/'namespace'
            log=d.namespace/'log'/'vendor.log'; log.parent.mkdir(parents=True); log.write_bytes(b'h_old_bearer')
            self.assertEqual(d.handles['s_fixture'],'h_next_bearer')
            self.assertFalse(d.secrecy_scan()['secret_absent'])
            d.clear_sensitive()
            self.assertFalse(d.bearer_forms)

    def test_synthetic_values_still_fail_each_via_sink(self):
        for sink in ('evidence','phase','summary','stderr','undecoded','store'):
            with self.subTest(sink=sink), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); d.state.mkdir(); d.secret_forms=[b'fixture-provider-secret']
                if sink=='store':
                    with runtime.sqlite3.connect(d.state/'store.sqlite3') as db:
                        db.execute('CREATE TABLE fixture (value TEXT)')
                        db.execute('INSERT INTO fixture VALUES (?)',('fixture-provider-secret',))
                else:
                    path={'evidence':d.evidence/'nested'/'output.json',
                          'phase':d.evidence/'hostile.json','summary':d.evidence/'summary.json',
                          'stderr':d.state/'capture'/'stderr.log',
                          'undecoded':d.state/'capture'/'undecoded.bin'}[sink]
                    if sink=='evidence': d.ownership.register(path.parent,'runner-evidence')
                    elif sink in {'phase','summary'}:
                        d.ownership.register(path,'runner-evidence',directory=False)
                    path.parent.mkdir(parents=True,exist_ok=True); path.write_bytes(b'fixture-provider-secret')
                self.assertFalse(d.secrecy_scan()['secret_absent'])

    def test_vendor_private_synthetic_matches_are_labelled_on_each_private_root(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            d=self.driver(root); d.namespace=d.state/'vendor'/'opencode'/'namespace'
            xdg=d.namespace/'data'; project=d.evidence/'fixtures'/'fixture'
            d.namespace_env={'XDG_DATA_HOME':str(xdg)}; d.fixtures={'fixture':{'path':project}}
            d.ownership.register(project,'vendor-private')
            for folder in (d.namespace,d.home,xdg,project):
                folder.mkdir(parents=True,exist_ok=True); (folder/'vendor.log').write_bytes(b'fixture-provider-secret')
            d.secret_forms=[b'fixture-provider-secret']
            report=d.secrecy_scan()
            self.assertTrue(report['secret_absent'])
            self.assertEqual(report['vendor_private_synthetic_files'],4)

    def test_vendor_nonregular_scan_entries_are_metadata_only_and_never_followed(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            root=Path(root); d=self.driver(root); d.state.mkdir()
            target=root/'target.log'; target.write_bytes(b'private-password')
            directory=root/'target-directory'; directory.mkdir(); (directory/'vendor.log').write_bytes(b'private-password')
            d.namespace=d.state/'vendor'/'opencode'/'namespace'; d.namespace.mkdir(parents=True)
            (d.namespace/'linked.log').symlink_to(target)
            (d.namespace/'linked-logs').symlink_to(directory, target_is_directory=True)
            (d.namespace/'opencode.db').symlink_to(target)
            os.mkfifo(d.namespace/'stream.log')
            proc=mock.Mock(); proc.read.return_value=safety.PASSWORD_KEY+b'=private-password\0'
            d.vault.read_once(proc,safety.Identity(10,20))
            with mock.patch.object(runtime.sqlite3,'connect',side_effect=AssertionError('nonregular store read')):
                report=d.secrecy_scan()
            self.assertTrue(report['secret_absent'])
            self.assertEqual(report['metadata_only_files'],4)

    def test_via_rotated_log_and_store_blob_have_no_suffix_exemption(self):
        for suffix in ('via.log.1','blobs/b_fixture.blob'):
            for value in (b'private-password',b'fixture-provider-secret',b'h_private_bearer'):
                with self.subTest(path=suffix,value=value), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                    d=self.driver(root); path=d.state/suffix; path.parent.mkdir(parents=True); path.write_bytes(value)
                    proc=mock.Mock(); proc.read.return_value=safety.PASSWORD_KEY+b'=private-password\0'
                    d.vault.read_once(proc,safety.Identity(10,20)); d.handles={'s_fixture':'h_private_bearer'}
                    d.secret_forms=[b'fixture-provider-secret',b'h_private_bearer']
                    self.assertFalse(d.secrecy_scan()['secret_absent'])

    def test_missing_or_nonregular_store_after_daemon_run_blocks(self):
        for kind in ('missing','symlink','directory','fifo'):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); d.state.mkdir(); d.ownership.daemon_started(d.state); path=d.state/'store.sqlite3'
                if kind=='symlink':
                    target=Path(root)/'target'; target.write_bytes(b'not a Store'); path.symlink_to(target)
                elif kind=='directory': path.mkdir()
                elif kind=='fifo': os.mkfifo(path)
                with self.assertRaisesRegex(safety.Blocked,'VIA Store'):
                    d.secrecy_scan()

    def test_via_regular_config_database_and_extensionless_files_are_read(self):
        for name in ('output.db','output.sqlite','opencode.json','extensionless'):
            with self.subTest(name=name), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); d.ownership.register(d.evidence/name,'runner-evidence',directory=False)
                (d.evidence/name).write_bytes(b'fixture-provider-secret')
                d.secret_forms=[b'fixture-provider-secret']
                self.assertFalse(d.secrecy_scan()['secret_absent'])

    def test_verified_fake_daemon_start_requires_store_even_after_stop(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            proc=mock.Mock(); proc.stat.return_value={'start_ticks':20}; proc.alive.return_value=False
            d=self.driver(root,proc=proc); d.state.mkdir(); project=d.evidence/'project'; project.mkdir()
            d.fixtures={'fixture':{'path':project,'config':{}}}; d.env={'PATH':'/usr/bin:/bin'}
            with mock.patch.object(d,'build'), mock.patch.object(safety,'verify_binary'), \
                 mock.patch.object(safety,'validate_path'), mock.patch.object(safety,'verify_daemon_program'), \
                 mock.patch.object(d,'_refresh_inventory'), mock.patch.object(d,'_record'), \
                 mock.patch.object(d,'via',return_value={'pid':10}), \
                 mock.patch.object(d,'_locks_owned',return_value=True), \
                 mock.patch.object(d,'_lock_free',return_value=True), \
                 mock.patch.object(d,'_pgrep_clear',return_value=True):
                d.start('release',project); d.stop()
            self.assertIsNone(d.daemon)
            with self.assertRaisesRegex(safety.Blocked,'VIA Store missing after daemon run'):
                d.secrecy_scan()

    def test_nonregular_via_roots_block_before_external_read(self):
        for kind in ('evidence','state','daemon-home','daemon-tmp'):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); target=Path(root)/'outside'; target.mkdir()
                if kind=='evidence': path=d.evidence; path.rmdir()
                elif kind=='state': path=d.state
                else:
                    path=d.evidence/kind
                    d.env={'HOME' if kind=='daemon-home' else 'TMPDIR':str(path)}
                    d.ownership.register(path,'via-owned')
                path.symlink_to(target,target_is_directory=True)
                with self.assertRaisesRegex(safety.Blocked,'VIA scan root'):
                    d.secrecy_scan()

    def test_unlisted_nonregular_via_entries_block(self):
        for kind in ('file-link','directory-link','fifo'):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); target=Path(root)/'outside'; target.mkdir()
                path=d.evidence/'hidden.log'
                if kind=='file-link':
                    target=target/'target'; target.write_bytes(b'not followed'); path.symlink_to(target)
                elif kind=='directory-link': path.symlink_to(target,target_is_directory=True)
                else: os.mkfifo(path)
                with self.assertRaisesRegex(safety.Blocked,'non-regular VIA scan entry'):
                    d.secrecy_scan()

    def test_only_named_operational_nonregular_entries_are_allowed(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            d=self.driver(root); d.helpers.mkdir(); d.runtime.mkdir()
            target=Path(root)/'checked-rg'; target.write_bytes(b'fixture rg')
            (d.helpers/'rg').symlink_to(target); (d.runtime/'daemon.lock').touch()
            anchor=d.runtime/'anchors'/'a_fixture.sock'; anchor.parent.mkdir()
            sockets=[]
            try:
                for path in (d.runtime/'via.sock',anchor):
                    channel=runtime.socket.socket(runtime.socket.AF_UNIX); channel.bind(str(path)); sockets.append(channel)
                self.assertTrue(d.secrecy_scan()['secret_absent'])
                unexpected=d.runtime/'unexpected.sock'
                channel=runtime.socket.socket(runtime.socket.AF_UNIX); channel.bind(str(unexpected)); sockets.append(channel)
                with self.assertRaisesRegex(safety.Blocked,'non-regular VIA scan entry'):
                    d.secrecy_scan()
            finally:
                for channel in sockets: channel.close()

    def test_only_vendor_private_vanishing_entries_are_counted_and_skipped(self):
        for private in (True,False):
            for phase in ('lstat','read','directory'):
                with self.subTest(private=private,phase=phase), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                    d=self.driver(root); d.namespace=d.state/'vendor'/'opencode'/'namespace'
                    folder=(d.namespace if private else d.state)/'log'; folder.mkdir(parents=True)
                    path=folder/('gone' if phase=='directory' else 'gone.log')
                    if phase=='directory': path.mkdir(); (path/'child.log').write_bytes(b'clean')
                    else: path.write_bytes(b'clean')
                    original=Path.open if phase=='read' else Path.lstat
                    def vanished(candidate,*args,**kwargs):
                        if candidate==path: raise FileNotFoundError(errno.ENOENT,'fixture vanished',str(candidate))
                        return original(candidate,*args,**kwargs)
                    with mock.patch.object(Path,'open' if phase=='read' else 'lstat',vanished):
                        if private:
                            scan=d.secrecy_scan(); self.assertTrue(scan['secret_absent'])
                            self.assertEqual(scan['vendor_private_vanished_entries'],1)
                        else:
                            with self.assertRaisesRegex(safety.Blocked,'VIA scan entry vanished'):
                                d.secrecy_scan()

    def test_daemon_home_xdg_and_tmp_roots_are_via_sinks(self):
        for key,part in safety.PRIVATE_PARTS.items():
            with self.subTest(key=key), tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
                d=self.driver(root); folder=d.evidence/('daemon-'+part); folder.mkdir()
                d.ownership.register(folder,'via-owned')
                d.env={key:str(folder)}; (folder/'output.log').write_bytes(b'fixture-provider-secret')
                d.secret_forms=[b'fixture-provider-secret']
                scan=d.secrecy_scan()
                self.assertFalse(scan['secret_absent'])
                self.assertEqual(scan['vendor_private_synthetic_files'],0)

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
            original=Path.open
            def read(path,*args,**kwargs):
                if path==protected: raise AssertionError('credential directory contents read')
                return original(path,*args,**kwargs)
            with mock.patch.object(Path,'open',read):
                scan=d.secrecy_scan()
            self.assertEqual(scan['metadata_only_files'],1)

    def test_fixture_synthetic_source_exception_never_allows_password_or_handle(self):
        with tempfile.TemporaryDirectory(prefix='via-runtime-') as root:
            root=Path(root); d=self.driver(root)
            project=root/'project'; project.mkdir(); source=project/'opencode.json'
            d.ownership.register(project,'vendor-private')
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
