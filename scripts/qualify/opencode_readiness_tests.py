"""Delayed owned-state regressions for the §13 qualification startup sweep."""
import json
import sqlite3
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

import opencode_driver as transport
from opencode_safety import Blocked, Identity


class ReadinessTests(unittest.TestCase):
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
            {'state': 'running', 'vendor_identity_verified': False, 'vendor_session_id': None},
            {'state': 'running', 'vendor_identity_verified': True, 'vendor_session_id': 'ses_owned'}])
        self.assertEqual(self.driver._vendor_sid('s_owned'), 'ses_owned')
        self.assertEqual(self.driver.via.call_count, 2)

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
        self.driver._helper_fixture = mock.Mock(return_value=project / 'helper')
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
        config = {}
        hold = mock.Mock(endpoint='http://127.0.0.1:1234/v1', requests=0)
        self.driver._namespace_bootstraps[self.driver.namespace] = (config, hold)
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
            {'state': 'running', 'admission': 'open', 'vendor_identity_verified': True,
             'active_turn': {'phase': 'submitting'}},
            {'state': 'running', 'admission': 'open', 'vendor_identity_verified': True,
             'active_turn': {'phase': 'accepted'}}])
        self.driver.journal._signal = mock.Mock()
        result = self.driver._operation('server_loss_for_marker', {'session': 's_owned'})
        self.assertTrue(result['anchor_pid_only']); self.assertEqual(self.driver.via.call_count, 2)
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
        self.driver.proc.alive.return_value = None
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
