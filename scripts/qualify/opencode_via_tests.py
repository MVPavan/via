"""Real VIA CLI interaction audit with the scripted OpenCode fake (§13; C1 §3).

No real vendor, external provider network, or caller configuration is used.
Vendor/schema pins are replaced with labelled FAKE evidence. The call matrix
substitutes spending policy; bootstrap tests exercise the real guard with
served facts and synthetic faults. CLI, daemon, adapter, Host and Store are real.
"""

from types import SimpleNamespace
import gc
import sqlite3
import contextlib
import base64
import secrets
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.request
import unittest
from unittest import mock

import opencode_driver as runtime
import opencode_safety as safety

REPO = Path(__file__).resolve().parents[2]
PROGRAMS = {'release': REPO/'target/release/via',
            'failpoints': REPO/'target/debug/via',
            'fake': REPO/'target/debug/via-fake-agent'}
AVAILABLE = all(path.is_file() and os.access(path, os.X_OK) for path in PROGRAMS.values())
AUDIT_SECONDS = 90  # Packet §13: one build's bounded CLI interaction audit.
SES = 'ses_via0001'
CLEANUP_PROOFS=[]


def route(method, path, *responses):
    return {'method': method, 'path': path, 'responses': list(responses)}


def event(kind, **data):
    return {'type': kind, 'data': {'sessionID': '$SESSION', **data},
            'id': 'evt_fixture', 'created': 1}


def scripted_server(cwd):
    """Use only the fake's existing route/response/frame vocabulary (§13 OC03)."""
    rules = [{'action': '*', 'resource': '*', 'effect': 'allow'},
             *({'action': action, 'resource': '*', 'effect': 'deny'} for action in
               ('question', 'opencode_session_move', 'opencode_session_rename',
                'opencode_list_mcp_resources', 'opencode_read_mcp_resource'))]
    model = {'providerID': 'oclive-mock', 'id': 'fixture-free', 'variant': 'default'}
    info = {'data': {'id': SES, 'projectID': 'p', 'agent': 'via', 'model': model,
                     'permissions': rules, 'location': {'directory': str(cwd)},
                     'cost': 0, 'tokens': {}, 'time': {'created': 1, 'updated': 1}}}
    begun = [event('session.inbox.enqueued', inboxID='$INPUT'),
             event('session.execution.started'),
             event('session.inbox.delivered', inboxID='$INPUT'),
             event('session.step.started', assistantMessageID='$INPUT:a', agent='via',
                   model={'providerID': 'oclive-mock', 'id': 'fixture-free'}, started=1)]
    completed = [*begun,
                 event('session.text.delta', assistantMessageID='$INPUT:a', ordinal=0, delta='done'),
                 event('session.text.ended', assistantMessageID='$INPUT:a', ordinal=0, text='done'),
                 event('session.step.ended', assistantMessageID='$INPUT:a', finish='stop', cost=0,
                       tokens={'input': 11, 'output': 7, 'reasoning': 2,
                               'cache': {'read': 3, 'write': 4}}),
                 event('session.execution.succeeded')]
    def prompt(events):
        return {'status': 200, 'json': {'data': {'id': '$INPUT', 'sessionID': '$SESSION'}},
                'emit': events}
    return {'routes': [
        route('GET', '/api/info', {'status': 200, 'json': {'version': '2.0.22', 'pid': '$PID'}}),
        route('GET', '/api/integration', {'status': 200, 'json': {'data': []}}),
        route('GET', '/api/model*', {'status': 200, 'json': {'data': [
            {'providerID': 'oclive-mock', 'id': 'fixture-free', 'name': 'FAKE model',
             'variants': [], 'cost': [{'input': 0, 'output': 0,
                                     'cache': {'read': 0, 'write': 0}}]}]}}),
        {'method': 'GET', 'path': '/api/event',
         'sse': {'events': [{'type': 'server.connected', 'properties': {}}]}},
        route('POST', '/api/session', {'status': 200, 'json': info}),
        route('GET', '/api/session/'+SES, {'status': 200, 'json': info}),
        route('GET', '/api/session/'+SES+'/inbox', {'status': 200, 'json': {'data': []}}),
        route('GET', '/api/experimental/session/'+SES+'/instructions/entries',
              {'status': 200, 'json': {'data': []}}),
        route('POST', '/api/session/'+SES+'/prompt', prompt(completed), prompt(completed), prompt(completed),
              prompt(begun)),
        route('POST', '/api/session/'+SES+'/interrupt',
              {'status': 204, 'emit': [event('session.execution.interrupted', reason='user')]}),
    ]}


class ViaFixture:
    """Own private roots and identities; prove shutdown before deleting them (§13)."""
    def __init__(self, kind, *, auto_start=True):
        self.root = Path(tempfile.mkdtemp(prefix='via-oc-cli-', dir=REPO/'scratchpad'))
        self.kind = kind
        self.auto_start = auto_start
        directory = self.root/'fake-bin'; directory.mkdir(mode=0o700)
        self.program = directory/'opencode'
        shutil.copyfile(PROGRAMS['fake'], self.program); self.program.chmod(0o500)
        self.script = self.program.with_suffix('.opencode.json')
        self.script.write_text('{}'); self.script.chmod(0o600)
        for suffix in ('requests', 'reports', 'versions', 'frames'):
            self.program.with_suffix('.'+suffix).touch(mode=0o600)
        directory.chmod(0o500)
        self.driver = runtime.Driver(PROGRAMS['release'], PROGRAMS['failpoints'],
                                     self.program, self.root/'evidence', initialize=False)
        self.cli_calls=[]
        execute=self.driver.execute
        def observed(argv, **kwargs):
            result=execute(argv, **kwargs)
            if argv[0] in {str(PROGRAMS['release']),str(PROGRAMS['failpoints'])}:
                code,out,err=result
                if code in {0,3} and (not out or err):
                    raise AssertionError('FAKE C1 success streams differ from stdout-only JSON')
                if code==2 and (out or not err):
                    raise AssertionError('FAKE C1 refusal streams differ from stderr-only JSON')
                self.cli_calls.append({'build':self.kind,'verb':argv[1],'exit':code,
                                       'flags':[arg for arg in argv[2:] if arg.startswith('--')]})
            return result
        self.driver.execute=observed
        self.patches = contextlib.ExitStack()
        self.expect_spending_latch = False

    def __enter__(self):
        digest = safety.sha256(self.program)
        verify = safety.verify_binary
        verify_program = safety.verify_daemon_program
        self.patches.enter_context(mock.patch.object(safety, 'verify_binary',
            side_effect=lambda path, *a, **kw: verify(path, expected_sha=digest)))
        self.patches.enter_context(mock.patch.object(safety, 'verify_daemon_program',
            side_effect=lambda config, path, *a, **kw:
                verify_program(config, path, expected_sha=digest)))
        # The fake has one scripted SSE consumer. The route owns it; testing a
        # second native observer is outside this audit of VIA CLI interactions.
        self.patches.enter_context(mock.patch.object(self.driver, 'start_event_capture'))
        try:
            self.driver.prepare()
            self.driver.phase_deadline = time.monotonic()+AUDIT_SECONDS
            self.driver.set_phase_budget(0, 32)
            self.project = self.driver.fixture('cli-audit', {'provider': 'mock'})
            self.script.write_text(json.dumps(scripted_server(self.project)))
            if self.auto_start: self.driver.start(self.kind, self.project)
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def requests(self):
        return [json.loads(line) for line in self.program.with_suffix('.requests').read_text().splitlines()]

    def __exit__(self, *_args):
        try:
            if self.driver.daemon is not None:
                try: proof = self.driver.finish()
                except safety.Blocked as error:
                    # §13: a test that deliberately latched spending control
                    # still requires the same completed stop proof.
                    if not self.expect_spending_latch or self.driver.guard.stopped is not True \
                            or str(error) != 'spending control latched during qualification':
                        raise
                    proof = self.driver.stop_result
                if proof['processes_gone'] is not True:
                    raise AssertionError('FAKE audit processes not proven gone')
            else:
                for provider in self.driver.mock_providers.values():
                    provider.__exit__(None, None, None)
            result = subprocess.run(['/usr/bin/pgrep', '-f', '--', str(self.root)],
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    check=False, timeout=5)
            if result.returncode != 1 or result.stdout or result.stderr:
                raise AssertionError('FAKE real-VIA audit pgrep not clear')
        finally:
            self.patches.close()
        identities_gone=all(self.driver.proc.gone(identity) is True
                            for identity in self.driver.identities)
        runtime_gone=self.driver.runtime.parent!=Path('/tmp') or not self.driver.runtime.exists()
        if not identities_gone or not runtime_gone:
            raise AssertionError('FAKE real-VIA roots retained: stop proof incomplete')
        # No finalizer may remove an unproved process's state. Only now make
        # the test-owned read-only executable directory writable for deletion.
        (self.root/'fake-bin').chmod(0o700)
        shutil.rmtree(self.root)
        cleanup={'build':self.kind,
            'owned_processes':len(self.driver.identities),
            'identities_gone':identities_gone,
            'pgrep_clear':True,'private_root_removed':not self.root.exists(),
            'short_runtime_removed':runtime_gone}
        if not all(cleanup[key] is True for key in ('identities_gone','pgrep_clear',
                   'private_root_removed','short_runtime_removed')):
            raise AssertionError('FAKE real-VIA final cleanup not proved')
        CLEANUP_PROOFS.append(cleanup)


class FixtureRetentionTests(unittest.TestCase):
    def test_unproved_stop_retains_root_after_fixture_collection(self):
        with tempfile.TemporaryDirectory(prefix='via-fake-source-') as directory:
            source=Path(directory)/'fake'; source.touch()
            def fail(): raise RuntimeError('FAKE stop proof unavailable')
            def driver(*_args,**_kwargs):
                return SimpleNamespace(daemon=object(),execute=lambda *_a,**_k:None,finish=fail)
            with mock.patch.dict(PROGRAMS,{'fake':source}), mock.patch.object(runtime,'Driver',driver):
                fixture=ViaFixture('release')
                root=fixture.root
                try:
                    with self.assertRaisesRegex(RuntimeError,'stop proof unavailable'):
                        fixture.__exit__(None,None,None)
                    del fixture
                    gc.collect()
                    self.assertTrue(root.is_dir(),'unproved private root was deleted by finalizer')
                finally:
                    if root.exists():
                        (root/'fake-bin').chmod(0o700)
                        shutil.rmtree(root)


@unittest.skipUnless(AVAILABLE, 'optional real-VIA audit: build release, failpoints and via-fake-agent')
class RealViaTests(unittest.TestCase):
    def test_slow_native_creation_keeps_running_and_session_states_distinct(self):
        """C1 §§3.7,7.2: use the fake's real delay while VIA commits creation."""
        for kind in ('release','failpoints'):
            with self.subTest(build=kind),ViaFixture(kind) as fixture:
                driver=fixture.driver
                script=scripted_server(fixture.project)
                creation=next(row for row in script['routes']
                              if row['method']=='POST' and row['path']=='/api/session')
                release=fixture.root/'FAKE-native-create.release'
                creation['responses'][0]['wait_for_file']=str(release)
                fixture.script.write_text(json.dumps(script))
                def spending(**_kwargs):driver.last_model=safety.MOCK_IDENTITY
                with mock.patch.object(driver,'spending_check',side_effect=spending):
                    receipt=driver.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                        '--cwd',str(fixture.project),'--bound','full','--network',
                        '--background','--prompt','FAKE delayed native creation'])
                    def submitting():
                        row=driver.via(['status',receipt['session_id']])
                        return row if row['active_turn'] is not None else None
                    try:status=driver._await_owned(submitting,'FAKE turn did not dispatch')
                    finally:release.touch(mode=0o600)
                    self.assertEqual(status['state'],'active')
                    self.assertIsNone(status['vendor_session_id'])
                    self.assertEqual(status['active_turn']['state'],'running')
                    self.assertEqual(status['active_turn']['phase'],'submitting')
                    self.assertEqual(driver._vendor_sid(receipt['session_id']),SES)
                    self.assertEqual(driver.via(['wait',receipt['turn'],
                                                '--timeout-ms','10000'])['state'],'completed')

    def test_disk_floor_blocks_preflight_with_retained_fixed_numeric_cause(self):
        with ViaFixture('release',auto_start=False) as fixture:
            driver=fixture.driver
            # A bounded synthetic capacity refusal, on a real private VIA daemon;
            # unsupported max_steps still causes no vendor acquisition/model call.
            with mock.patch.object(runtime,'QUALIFICATION_FREE_FLOOR_BYTES',1<<62):
                with self.assertRaisesRegex(safety.Blocked,
                        '^private VIA state is below its configured disk free floor$'):
                    driver._operation('fresh_max_steps',{'max_steps':1})
            retained=list(driver.evidence.glob('*reply-block.json'))
            self.assertTrue(retained)
            record=json.loads(retained[-1].read_text())
            replies=[row for row in record['replies'] if row['source']=='via-stderr']
            self.assertTrue(replies)
            data=replies[-1]['projection']['data']
            self.assertEqual(data['kind'],'admission_refused')
            self.assertEqual(data['kind2'],'disk_free_floor')
            self.assertEqual(data['floor_bytes'],1<<62)
            self.assertIs(type(data['free_bytes']),int)
            self.assertLess(data['free_bytes'],data['floor_bytes'])
            self.assertNotIn('message',replies[-1]['projection'])
            self.assertEqual(fixture.requests(),[])

    def bootstrap_policy(self, fixture, *, reject=None):
        """FAKE routes supply served facts; a test peer drives the owned mock only."""
        d=fixture.driver
        materialize=d._materialize_fixture
        threads=[]; replies=[]
        def configured(name, project, description):
            config=materialize(name, project, description)
            if name.startswith('bootstrap-'):
                script=scripted_server(project)
                if reject:
                    prompt=next(row for row in script['routes'] if row['method']=='POST'
                                and row['path'].endswith('/prompt'))
                    # Existing fake frames model a pending turn, not a vendor
                    # model/client: completion cannot race the cancel proof.
                    prompt['responses']=[prompt['responses'][-1]]
                script['routes'].append(route('GET','/api/config*',{'status':200,'json':{
                    'data':[{'type':'document','info':config}]}}))
                fixture.script.write_text(json.dumps(script))
                provider=d.mock_providers[name]
                def request():
                    try:
                        req=urllib.request.Request(provider.endpoint+'/chat/completions',
                            data=json.dumps({'model':'fixture-free','messages':[]}).encode(),
                            headers={'Content-Type':'application/json'})
                        opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
                        with opener.open(req,timeout=30) as response:
                            replies.append(response.status)
                    except OSError:
                        replies.append('closed')
                thread=threading.Thread(target=request); threads.append(thread); thread.start()
                by=time.monotonic()+5
                while not provider.received and time.monotonic()<by: time.sleep(.01)
                self.assertTrue(provider.received)
                self.assertEqual(replies,[])
            return config
        fixture.patches.enter_context(mock.patch.object(d,'_materialize_fixture',side_effect=configured))
        # E7 is a real-vendor evidence pin, not an API offered by the scripted
        # fake. Only that schema pin is substituted; served facts remain HTTP.
        fixture.patches.enter_context(mock.patch.object(d,'_served_schema',return_value={
            'components':{'schemas':{'Config.InfoEncoded':{'properties':{
                'model':{},'agents':{},'providers':{}}}}}}))
        if reject:
            check=d.guard.check
            def denied(**kwargs):
                # Synthetic served-fact faults reach the real owning guard;
                # neither the CLI replies nor cancellation path are replaced.
                if reject=='integration': kwargs['integration']={'data':[{'id':'fake','connections':[{}]}]}
                if reject=='environment': kwargs['environment_names']={'UNEXPECTED_FAKE_KEY'}
                if reject=='catalog': kwargs['catalog']={}
                if reject=='provider': kwargs['project_providers']={'oclive-mock':'http://127.0.0.1:1/v1'}
                return check(**kwargs)
            fixture.patches.enter_context(mock.patch.object(d.guard,'check',side_effect=denied))
        return threads,replies

    def test_owned_server_bootstrap_is_held_and_charged_only_to_mock_budget(self):
        for kind in ('release', 'failpoints'):
            with self.subTest(build=kind), ViaFixture(kind) as fixture:
                d=fixture.driver
                threads,replies=self.bootstrap_policy(fixture)
                try:
                    d.ensure_vendor()
                finally:
                    for thread in threads: thread.join(5)
                self.assertFalse(any(thread.is_alive() for thread in threads))
                self.assertEqual(replies,[200])
                self.assertIsNotNone(d.vendor_identity)
                self.assertTrue(d.proc.alive(d.vendor_identity))
                requests=fixture.requests()
                self.assertTrue(any(row['target'].startswith('/api/model') for row in requests))
                self.assertTrue(any(row['target'].endswith('/prompt') for row in requests))
                self.assertEqual(d.phase_public_left,0)
                self.assertEqual(d.phase_mock_left,31)
                records=[json.loads(path.read_text()) for path in d.evidence.glob('*bootstrap*.json')]
                self.assertTrue(any(row.get('charged')=='mock-only' and row.get('gate') is False
                                    and row.get('served_checks') is True for row in records))
                self.assertTrue(d.stop()['pgrep_clear'])

    def test_stop_closes_bootstrap_session_even_after_phase_deadline(self):
        with ViaFixture('release') as fixture:
            d=fixture.driver
            threads,_replies=self.bootstrap_policy(fixture)
            try: d.ensure_vendor()
            finally:
                for thread in threads: thread.join(5)
            session=d._bootstrap_session
            self.assertIsNotNone(session)
            d.phase_deadline=time.monotonic()-1
            d.stop()
            with sqlite3.connect(f'file:{d.state / "store.sqlite3"}?mode=ro',uri=True) as store:
                row=store.execute('SELECT state FROM sessions WHERE id=?',(session,)).fetchone()
            self.assertEqual(row,('closed',))
            verbs=[row['verb'] for row in fixture.cli_calls]
            self.assertIn('close',verbs)
            self.assertLess(verbs.index('close'),len(verbs)-1)

    def test_terminal_cli_exit_mapping_for_result_and_foreground_spawn(self):
        for kind in ('release','failpoints'):
            for state in ('completed','failed','cancelled','unknown'):
                with self.subTest(build=kind,state=state), ViaFixture(kind) as fixture:
                    d=fixture.driver
                    script=scripted_server(fixture.project)
                    prompt=next(row for row in script['routes'] if row['method']=='POST'
                                and row['path'].endswith('/prompt'))
                    response=prompt['responses'][0]
                    if state=='failed':
                        response['emit'][-1]=event('session.execution.failed',error={
                            'type':'provider.no-route','message':'FAKE terminal failure'})
                    elif state=='cancelled':
                        response['emit']=response['emit'][:4]
                    elif state=='unknown':
                        response['emit']=response['emit'][:4]
                        sse=next(row for row in script['routes'] if row['path']=='/api/event')
                        sse['sse']['close_after_ms']=1500
                    prompt['responses']=[response]
                    fixture.script.write_text(json.dumps(script))
                    command=['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                        '--cwd',str(fixture.project),'--bound','full','--network','--prompt','FAKE terminal']
                    handle='h_'+base64.urlsafe_b64encode(secrets.token_bytes(32)).decode().rstrip('=')
                    cancellation=[]
                    def cancel_pending():
                        try:
                            by=time.monotonic()+10
                            while time.monotonic()<by:
                                with sqlite3.connect(f'file:{d.state / "store.sqlite3"}?mode=ro',
                                                     uri=True) as store:
                                    rows=store.execute('SELECT id FROM sessions').fetchall()
                                if rows:
                                    session=rows[0][0]
                                    d.handles[session]=handle
                                    status=d.via(['status',session])
                                    if (status.get('active_turn') or {}).get('phase')=='accepted':
                                        d.via(['cancel',session,'--turn','1','--wait'])
                                        cancellation.append('cancelled')
                                        return
                                time.sleep(.01)
                            cancellation.append('deadline')
                        except BaseException as error:
                            cancellation.append(type(error).__name__)
                    thread=threading.Thread(target=cancel_pending) if state=='cancelled' else None
                    if thread: thread.start()
                    try:
                        rc,out,err=d.execute([str(d._binary),*command,'--handle-stdin','--json'],
                            env=d.env,cwd=d.project,input=(handle+'\n').encode(),timeout=30)
                    finally:
                        if thread: thread.join(12)
                    if thread:
                        self.assertFalse(thread.is_alive())
                        self.assertEqual(cancellation,['cancelled'])
                    lines=out.splitlines()
                    self.assertEqual(len(lines),2)
                    receipt=runtime.strict_reply(lines[0],'spawn receipt')
                    envelope=runtime.strict_reply(lines[1],'envelope')
                    self.assertEqual(envelope['state'],state)
                    expected=0 if state=='completed' else 3
                    self.assertEqual(rc,expected)
                    self.assertEqual(err,b'')
                    d.handles[receipt['session_id']]=receipt['handle']
                    d.secret_forms.append(receipt['handle'].encode())
                    result=d.via(['result',receipt['turn']])
                    self.assertEqual(result['state'],state)
                    self.assertEqual([(row['verb'],row['exit']) for row in fixture.cli_calls
                                      if row['verb'] in {'spawn','result'}],
                                     [('spawn',expected),('result',expected)])

    def test_foreground_driver_keeps_receipt_and_accounts_terminal_envelope(self):
        with ViaFixture('release') as fixture:
            d=fixture.driver
            def spending(**_kwargs): d.last_model=safety.MOCK_IDENTITY
            with mock.patch.object(d,'spending_check',side_effect=spending), \
                 mock.patch.object(d,'account',wraps=d.account) as accounted:
                envelope=d.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                    '--cwd',str(fixture.project),'--bound','full','--network','--prompt','FAKE foreground'])
            self.assertEqual(envelope['state'],'completed')
            self.assertEqual(d.last_request['receipt']['turn'],envelope['session_id']+'/1')
            self.assertIn(envelope['session_id'],d.handles)
            self.assertTrue(any('handle' in row for row in d.owned_replies))
            with mock.patch.object(d,'account',wraps=d.account) as result_accounted:
                result=d.via(['result',envelope['session_id']+'/1'])
            self.assertEqual(result,envelope)
            accounted.assert_called_once_with(envelope)
            result_accounted.assert_called_once_with(envelope)

    def test_bootstrap_does_not_mask_the_later_namespace_fixture_configuration(self):
        with ViaFixture('release') as fixture:
            d=fixture.driver
            threads,_replies=self.bootstrap_policy(fixture)
            try: d.ensure_vendor()
            finally:
                for thread in threads: thread.join(5)
            # Exactly the namespace sentinel's registration/write/start shape.
            # Selecting local config is a runner precondition; the fake's
            # immutable served config is not a vendor reload simulation.
            config=d._materialize_fixture('namespace-boundary',d.namespace,{'provider':'mock'})
            (d.namespace/'opencode.json').write_text(json.dumps(config))
            d.fixtures['namespace-boundary']={'path':d.namespace,'config':config}
            d.start('release',d.namespace)
            with mock.patch.object(d,'_auxiliary_bindings',return_value=[safety.MOCK_IDENTITY]*9):
                d.spending_check(args=['spawn','--model',safety.MOCK_IDENTITY])
            self.assertEqual(d.provider_endpoints,d._validate_provider_config(config))

    def test_failed_bootstrap_spending_check_cancels_without_releasing_response(self):
        faults={'integration':'not proven credential-free','environment':'environment allow-list',
                'catalog':'frozen free model absent','provider':'owned loopback mock'}
        for kind,fault in ((kind,fault) for kind in ('release','failpoints') for fault in faults):
            with self.subTest(build=kind,fault=fault), ViaFixture(kind) as fixture:
                d=fixture.driver
                threads,replies=self.bootstrap_policy(fixture,reject=fault)
                try:
                    with self.assertRaisesRegex(safety.Blocked,faults[fault]):
                        d.ensure_vendor()
                finally:
                    for provider in d.mock_providers.values():
                        if getattr(provider,'response_hold',None): provider.response_hold.abort()
                    for thread in threads: thread.join(5)
                self.assertFalse(any(thread.is_alive() for thread in threads))
                self.assertEqual(replies,['closed'])
                self.assertTrue(any(row['verb']=='cancel' for row in fixture.cli_calls))
                receipt=next(row for row in d.owned_replies if 'handle' in row)
                terminal=d.via(['wait',receipt['turn'],'--timeout-ms','10000'])
                self.assertEqual(terminal['state'],'cancelled')
                provider=d.mock_providers['bootstrap-1']
                self.assertFalse(provider.response_hold.released)
                self.assertTrue(provider.response_hold.aborted)
                self.assertTrue(d.guard.stopped)
                self.assertEqual(d.phase_public_left,0)
                self.assertEqual(d.phase_mock_left,31)
                fixture.expect_spending_latch=True

    def test_models_only_lists_and_unoffered_effort_retires_the_acquisition(self):
        for kind in ('release','failpoints'):
            with self.subTest(build=kind), ViaFixture(kind) as fixture:
                d=fixture.driver
                self.assertEqual(d.via(['models','--harness','opencode'])['models'],[])
                self.assertEqual(d._anchor_rows(),[])
                command=['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                         '--cwd',str(fixture.project),'--bound','full','--network',
                         '--effort',runtime.METADATA_EFFORT,'--background','--prompt','FAKE metadata']
                # Exactly the production L11 pre-prompt refusal exception. No
                # successful/model-capable request uses this test substitution.
                d._metadata_command=command; d._metadata_active=True
                try:
                    receipt=d.via(command)
                    result=d.via(['wait',receipt['turn'],'--timeout-ms','30000'])
                finally:
                    d._metadata_command=None; d._metadata_active=False
                self.assertEqual(result['state'],'failed')
                self.assertEqual(result['failure']['class'],'submit_failed')
                self.assertEqual(result['failure']['data']['field'],'effort')
                self.assertIsNone(result['vendor_session_id'])
                self.assertFalse(any(row['method']=='POST' for row in fixture.requests()))
                by=time.monotonic()+5
                while time.monotonic()<by:
                    if d.via(['models','--harness','opencode'])['models']==[]: break
                    time.sleep(.01)
                else: self.fail('FAKE unleased metadata generation did not retire')

    def test_credential_fence_is_exercised_by_real_via_spawn(self):
        for kind in ('release','failpoints'):
            with self.subTest(build=kind), ViaFixture(kind) as fixture:
                d=fixture.driver
                folder=d.namespace/'data/opencode'; folder.mkdir(parents=True,mode=0o700)
                (folder/'opencode.db').touch(mode=0o600)
                script=scripted_server(fixture.project)
                script['routes'][1]=route('GET','/api/integration', {'status':200,'json':{
                    'data':[{'id':'fixture','connections':[{'type':'credential','id':'fake'}]}]}})
                fixture.script.write_text(json.dumps(script))
                proof=d._credential_refusal()
                self.assertEqual(proof['code'],'unexpected_credential_state')
                self.assertFalse(proof['cached'])
                self.assertFalse(any(row['method']=='POST' for row in fixture.requests()))

    def test_release_cli_call_matrix(self):
        self.call_matrix('release')

    def test_release_cli_call_matrix_awaits_delayed_pending_acceptance(self):
        """A healthy native response may exceed the matrix's former five-second poll."""
        original=scripted_server
        def delayed(cwd):
            script=original(cwd)
            prompt=next(row for row in script['routes']
                        if row['method']=='POST' and row['path'].endswith('/prompt'))
            prompt['responses'][3]['sleep_ms']=5500
            return script
        # Keep the CLI matrix and owning readiness probe real; only the fake's
        # fourth prompt response is delayed within the fixture's phase budget.
        with mock.patch(__name__+'.scripted_server',side_effect=delayed):
            self.call_matrix('release')

    def test_failpoints_cli_call_matrix(self):
        self.call_matrix('failpoints')

    def call_matrix(self, kind):
        with ViaFixture(kind) as fixture:
            d=fixture.driver
            def spending(**_args): d.last_model=safety.MOCK_IDENTITY
            # Vendor policy is tested separately. No CLI method or reply is mocked.
            with mock.patch.object(d, 'spending_check', side_effect=spending):
                description=d.via(['describe','--harness','opencode','--model',safety.MOCK_IDENTITY,
                                   '--cwd',str(fixture.project),'--bound','full','--network'])
                self.assertEqual(description['capabilities']['params']['max_steps']['support'],'unsupported')
                refused=d.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                              '--cwd',str(fixture.project),'--bound','full','--network',
                              '--max-steps','1','--background','--prompt','FAKE preflight'])
                self.assertEqual(refused['exit_code'],2)
                self.assertEqual(refused['cli_error']['error']['data']['field'],'max_steps')
                self.assertEqual(d._anchor_rows(),[])
                spawn=['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                       '--cwd',str(fixture.project),'--bound','full','--network','--background']
                receipt=d.via([*spawn,'--prompt','FAKE reply'*4096])
                envelope=d.via(['wait',receipt['turn'],'--timeout-ms','180000'])
                self.assertEqual(envelope['state'],'completed', {
                    'failure_class': (envelope.get('failure') or {}).get('class'),
                    'failure_data': (envelope.get('failure') or {}).get('data'),
                    'warnings': [row['code'] for row in envelope['warnings']],
                    'stop_reason': envelope['stop_reason'],
                    'requests': [(row['method'],row['target'].split('?')[0])
                                 for row in fixture.requests()]})
                session=receipt['session_id']
                event_proof=d._operation('via_events',{'session':session,'turn':envelope['turn']})
                self.assertTrue(event_proof['complete'])
                self.assertEqual(event_proof['terminal_revision'],envelope['revision'])
                self.assertEqual(d.via(['status',session])['session_id'],session)
                self.assertTrue(d.via(['models','--harness','opencode'])['models'])
                self.assertEqual(d.via(['logs',receipt['turn']])['session_id'],session)
                self.assertTrue(runtime.read_pages(lambda after:
                    d.via(['events',session,'--after',str(after)])))
                self.assertTrue(d.via(['events',receipt['turn'],'--after','0','--limit','256'])['events'])
                resumed=d.via(['resume',session,'--prompt','FAKE resume'*4096,'--effort','default'])
                self.assertEqual(d.via(['wait',resumed['turn'],'--timeout-ms','10000'])['state'],'completed')
                extra=d.via(['resume',session,'--prompt','FAKE small'])
                self.assertEqual(d.via(['wait',extra['turn'],'--timeout-ms','10000'])['state'],'completed')
                pending=d.via(['resume',session,'--prompt','FAKE pending'])
                turn_number=int(pending['turn'].rsplit('/',1)[1])
                def accepted():
                    status=d.via(['status',session])
                    active=status['active_turn']
                    return status if active is not None and active['n']==turn_number \
                        and active['phase']=='accepted' else None
                d._await_owned(accepted,'FAKE pending turn was not accepted')
                unavailable=d.via(['result',pending['turn']])
                self.assertEqual(unavailable['exit_code'],2)
                self.assertEqual(unavailable['cli_error']['error']['data']['kind'],'turn_not_finished')
                timeout=d.via(['wait',pending['turn'],'--timeout-ms','0'])
                self.assertEqual(timeout['exit_code'],2)
                self.assertEqual(timeout['cli_error']['error']['data']['kind'],'wait_timeout')
                number=str(int(pending['turn'].rsplit('/',1)[1]))
                d.via(['cancel',session,'--turn',number])
                settled=d.via(['cancel',session,'--turn',number,'--wait'])
                self.assertEqual(settled['state'],'cancelled', {
                    'cancel':settled['cancel'],
                    'failure':d.via(['wait',pending['turn'],'--timeout-ms','10000']).get('failure'),
                    'routes':[(row['method'],row['target'].split('?')[0]) for row in fixture.requests()]})
                self.assertEqual(d.via(['wait',pending['turn'],'--timeout-ms','10000'])['state'],'cancelled')
                self.assertEqual(d.via(['close',session])['state'],'closed')
                self.assertTrue(d.stop()['pgrep_clear'])
                self.assertEqual({row['verb'] for row in fixture.cli_calls},
                    {'daemon','describe','spawn','wait','status','models','logs','events',
                     'resume','result','cancel','close'})
                self.assertEqual({row['exit'] for row in fixture.cli_calls},{0,2,3})
