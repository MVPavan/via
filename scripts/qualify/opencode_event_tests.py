"""Offline C1 event/envelope agreement regressions (OpenCode §13; C1 §§5–7)."""
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from opencode_driver import Driver
from opencode_safety import Blocked

SESSION='s_FAKE'


def event(seq,kind='turn.ended',turn=1,**fields):
    return {'seq':seq,'session_id':SESSION,'turn':turn,'type':kind,**fields}


class EventTests(unittest.TestCase):
    def observe(self,rows,turn=1):
        with tempfile.TemporaryDirectory(prefix='via-ocevents-') as folder:
            d=Driver('release','fp','pin',Path(folder)/'evidence')
            d.via=mock.Mock(return_value={'events':rows,'more':False,'next_after':rows[-1]['seq'] if rows else 0})
            return d._operation('via_events',{'session':SESSION,'turn':turn})

    def test_terminal_event_missing_or_nonstring_type_blocks(self):
        from opencode_events import terminal_event
        for kind in (None,1,True,{},[]):
            with self.subTest(kind=kind):
                row=event(1,state='completed')
                if kind is None: row.pop('type')
                else: row['type']=kind
                with self.assertRaisesRegex(Blocked,'^C1 event type unavailable$'):
                    terminal_event([row],SESSION,1)

    def test_c1_turn_ended_starts_at_revision_zero_without_a_revision_member(self):
        observed=self.observe([event(1,'turn.queued'),event(2,state='completed')])
        self.assertTrue(observed['complete']);self.assertEqual(observed['terminal_revision'],0)

    def test_c1_turn_revised_supplies_the_revision(self):
        observed=self.observe([event(1,state='unknown'),event(2,'turn.revised',revision=1,
                                state='completed',from_state='unknown',late=True)])
        self.assertEqual(observed['terminal_revision'],1)

    def test_other_turns_and_late_revisions_do_not_select_the_wrong_envelope(self):
        rows=[event(1,state='unknown'),event(2,turn=2,state='completed'),
              event(3,'turn.revised',revision=1,state='completed',from_state='unknown',late=True)]
        self.assertEqual(self.observe(rows,turn=2)['terminal_revision'],0)

    def test_missing_terminal_never_passes(self):
        with self.assertRaisesRegex(Blocked,'C1 turn terminal event unavailable'):
            self.observe([event(1,'turn.started')])

    def test_non_c1_terminal_alias_never_passes(self):
        with self.assertRaisesRegex(Blocked,'C1 turn terminal event unavailable'):
            self.observe([event(1,'turn.completed',revision=0,state='completed')])

    def test_malformed_or_noncontiguous_revisions_block(self):
        for revision in (None,True,-1,0,2,'1'):
            with self.subTest(revision=revision),self.assertRaisesRegex(Blocked,'C1 turn revision invalid'):
                self.observe([event(1,state='unknown'),event(2,'turn.revised',revision=revision,
                    state='completed',from_state='unknown',late=True)])

    def test_terminal_from_another_session_never_passes(self):
        row=event(1,state='completed');row['session_id']='s_FAKE_foreign'
        with self.assertRaisesRegex(Blocked,'C1 event session differs'):
            self.observe([row])

    def test_duplicate_ended_or_missing_late_marker_blocks(self):
        rows=[[event(1,state='completed'),event(2,state='completed')],
              [event(1,state='unknown'),event(2,'turn.revised',revision=1,
                state='completed',from_state='unknown',late=False)]]
        for events in rows:
            with self.subTest(shape=len(events)),self.assertRaisesRegex(Blocked,'C1 turn terminal sequence invalid'):
                self.observe(events)

    def test_loss_uses_c1_acceptance_and_known_terminal_states(self):
        for state in ('completed','failed','cancelled','unknown'):
            with self.subTest(state=state),tempfile.TemporaryDirectory(prefix='via-ocevents-') as folder:
                d=Driver('release','fp','pin',Path(folder)/'evidence')
                d.via=mock.Mock(return_value={'events':[event(1,'turn.started'),event(2,state=state)],
                                             'more':False,'next_after':2})
                observed=d._operation('loss_observation',{'session':SESSION,'turn_number':1})
                self.assertTrue(observed['accepted_before_loss'])
                self.assertEqual(observed['via_terminal_admitted'],state!='unknown')

    def test_foreign_completed_turn_is_never_reported_uncredited(self):
        with tempfile.TemporaryDirectory(prefix='via-ocevents-') as folder:
            d=Driver('release','fp','pin',Path(folder)/'evidence')
            d._foreign={'id':'msg_FAKE','sid':'ses_FAKE'};d.last_request={'receipt':{}}
            d._vendor_sid=mock.Mock(return_value='ses_FAKE')
            d._await_native=mock.Mock(return_value=[
                {'seq':1,'type':'session.inbox.delivered','data':{'inboxID':'msg_FAKE'}},
                {'seq':2,'type':'session.execution.succeeded'}])
            d.via=mock.Mock(return_value={'events':[event(1,state='completed')],'more':False,'next_after':1})
            observed=d._foreign_observation(SESSION,{'turn_number':1,'expect_owned_absent':True})
            self.assertFalse(observed['foreign_not_credited'])
