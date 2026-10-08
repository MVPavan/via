"""Offline vocabulary sweep regressions (C1 §§1,3,5–8; runtime §§6,9)."""
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest import mock

import opencode_driver as runtime
import opencode_reply as replies
from opencode_safety import Blocked
from opencode_safety_tests import fake_cli_generation


def envelope():
    """Independent C1 §5 nullable result fixture, without vendor content."""
    return {'session_id':'s_FAKE','turn':1,'revision':0,'state':'completed',
        'failure':None,'stop_reason':None,'vendor_version':'2.0.22',
        'vendor_session_id':'ses_FAKE','final_text':None,'structured_output':None,
        'steps':None,'cancel':None,'cost':{'usd':None,'scope':'turn'},
        'usage':{'scope':'turn','input_tokens':None,'cached_input_tokens':None,'output_tokens':None},
        'warnings':[],'denied_actions':[],'denied_actions_total':0,
        'auto_declined_requests_total':0,'leftovers':None}


class C1Tests(unittest.TestCase):
    def setUp(self):
        self.root=tempfile.TemporaryDirectory(prefix='via-oc-c1-')
        self.addCleanup(self.root.cleanup)
        self.driver=runtime.Driver('release','fp','pin',Path(self.root.name)/'evidence',initialize=False)
        self.driver.handles['s_FAKE']='h_FAKE-memory-only'
        self.driver.last_request={'receipt':{'session_id':'s_FAKE','turn':'s_FAKE/1'}}
        fake_cli_generation(self.driver)
        self.driver._binary=Path('FAKE-via')

    def status(self,state='active',turn='running',phase='submitting',sid=None):
        return {'session_id':'s_FAKE','state':state,'admission':'open',
            'vendor_session_id':sid,'vendor_identity_verified':sid is not None,
            'active_turn':{'n':1,'state':turn,'phase':phase} if turn=='running' else None,
            'turns':[{'n':1,'state':turn}]}

    def test_native_id_waits_for_owned_queued_turn_before_dispatch(self):
        self.driver.via=mock.Mock(side_effect=[self.status('idle','queued'),self.status('active','queued'),
            self.status(),self.status(phase='accepted',sid='ses_FAKE')])
        self.assertEqual(self.driver._vendor_sid('s_FAKE'),'ses_FAKE')
        self.assertEqual(self.driver.via.call_count,4)

    def test_live_pin_waits_for_owned_queued_turn_then_acceptance(self):
        accepted=self.status(phase='accepted',sid='ses_FAKE')
        self.driver.via=mock.Mock(side_effect=[self.status('idle','queued'),
            self.status('active','queued'),self.status(),accepted])
        self.assertEqual(self.driver._live_pin('s_FAKE'),accepted)
        self.assertEqual(self.driver.via.call_count,4)

    def test_queued_pending_never_waives_admission_identity_or_deadline(self):
        for fault in ('closing','closed','other-turn','unknown-turn','deadline'):
            with self.subTest(fault=fault):
                row=self.status('idle','queued')
                if fault=='closing':row['admission']='closing'
                if fault=='closed':row['state']='closed'
                if fault=='other-turn':row['turns'][0]['n']=2
                if fault=='unknown-turn':row['turns'][0]['state']='unknown'
                self.driver.via=mock.Mock(return_value=row)
                self.driver.phase_deadline=time.monotonic()+.025 if fault=='deadline' else None
                with self.assertRaises(Blocked):self.driver._vendor_sid('s_FAKE')
                if fault=='deadline':self.assertGreater(self.driver.via.call_count,1)
                else:self.assertEqual(self.driver.via.call_count,1)

    def test_native_id_pending_blocks_closing_active_session(self):
        row=self.status();row['admission']='closing'
        self.driver.via=mock.Mock(return_value=row)
        self.driver.phase_deadline=time.monotonic()+.025
        with self.assertRaisesRegex(Blocked,'session ended before native creation'):
            self.driver._vendor_sid('s_FAKE')
        self.assertEqual(self.driver.via.call_count,1)

    def test_live_pin_requires_running_turn_not_a_queued_active_alias(self):
        row=self.status();row['active_turn'].update(state='queued',phase=None)
        self.driver.via=mock.Mock(return_value=row)
        self.driver.phase_deadline=time.monotonic()+.025
        with self.assertRaisesRegex(Blocked,'live turn phase unrecognized'):
            self.driver._live_pin('s_FAKE')
        self.assertEqual(self.driver.via.call_count,1)

    def test_cancel_close_and_foreground_transport_cover_c1_settlement_bounds(self):
        self.driver.spending_check=mock.Mock();self.driver._admit_model=mock.Mock()
        self.driver.execute=mock.Mock(side_effect=Blocked('FAKE observed transport budget'))
        commands=(['cancel','s_FAKE','--turn','1','--wait'],['close','s_FAKE'],
                  ['spawn','--harness','opencode','--prompt','FAKE foreground'])
        for command,minimum in zip(commands,(80,50,190)):
            with self.subTest(verb=command[0]):
                self.driver.phase_deadline=None
                with self.assertRaisesRegex(Blocked,'FAKE observed transport budget'):self.driver.via(command)
                self.assertGreaterEqual(self.driver.execute.call_args.kwargs['timeout'],minimum)
                self.driver.phase_deadline=time.monotonic()+.5
                with self.assertRaises(Blocked):self.driver.via(command)
                self.assertLessEqual(self.driver.execute.call_args.kwargs['timeout'],.5)

    def test_close_reply_is_typed_instead_of_unknown_schema(self):
        for cleanup in (True,{},'pending'):
            with self.subTest(cleanup=cleanup):
                row={'session_id':'s_FAKE','state':'closed','cancelled_turns':[],
                     'cleanup':cleanup,'leftovers':None}
                self.driver.execute=mock.Mock(return_value=(0,json.dumps(row).encode(),b''))
                with self.assertRaises(Blocked):self.driver.via(['close','s_FAKE'])

    def test_envelope_revision_and_provenance_are_not_scope_aliases(self):
        for field,value in (('revision',True),('revision',-1),('cost','unavailable'),('usage','unavailable')):
            with self.subTest(field=field):
                row=envelope()
                if field=='revision':row[field]=value
                else:row[field]['scope']=value
                with self.assertRaises(Blocked):runtime.strict_reply(json.dumps(row).encode(),'envelope')

    def test_error_kind_must_match_its_c1_code(self):
        row={'code':-32012,'message':'FAKE refusal','data':{'kind':'invalid_params','field':'max_steps'}}
        with self.assertRaises(Blocked):runtime.strict_reply(json.dumps(row).encode(),'error')

    def test_cancel_outcome_and_cleanup_use_separate_closed_vocabularies(self):
        for schema in ('envelope','cancel reply'):
            for fault in ('outcome','cleanup'):
                with self.subTest(schema=schema,fault=fault):
                    row=envelope() if schema=='envelope' else {
                        'turn':'s_FAKE/1','state':'running','already_terminal':False}
                    row['cancel']={'outcome':'requested','cleanup':'pending'}
                    row['cancel'][fault]='FAKE-unknown'
                    with self.assertRaises(Blocked):
                        runtime.strict_reply(json.dumps(row).encode(),schema)

    def test_closed_projection_retains_c1_events_and_cleanup_vocabulary(self):
        names=('session.reopened','turn.revised','cancel.requested','cancel.settled',
               'vendor.request_declined','steer.delivered','warning','process.exited','server.lost')
        for name in names:
            with self.subTest(event=name):
                row={'type':name,'cancel':{'outcome':'acknowledged','cleanup':'quiescent'},
                     'data':{'kind':'wait_timeout','field':'max_steps'}}
                view=replies.reply_projection(json.dumps(row).encode(),lambda _raw:False)['projection']
                self.assertEqual(view,row)

    def test_future_reply_members_remain_additive_and_nullable(self):
        row=envelope();row['future_additive_field']={'ignored':'FAKE-content'}
        self.assertEqual(runtime.strict_reply(json.dumps(row).encode(),'envelope'),row)

    def test_closed_projection_keeps_used_c1_numbers_and_fixed_failure_labels(self):
        row={'revision':2,'active_turn':{'n':1,'state':'running','phase':'accepted'},
             'progress':{'current_step':2,'phase':'tools'},
             'failure':{'class':'server_lost'},'stop_reason':'end_turn',
             'denied_actions':[{'kind':'command','event_seq':3}],
             'cost':{'usd':None,'scope':'vendor_interval','provenance':'unavailable'},
             'warnings':[{'code':'predecessor_cleanup_uncertain'}]}
        view=replies.reply_projection(json.dumps(row).encode(),lambda _raw:False)['projection']
        self.assertEqual(view,row)
