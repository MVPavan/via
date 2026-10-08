"""Private daemon idle generations and fixed identity diagnostics (§13)."""

import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest import mock

from opencode_driver import Driver
import opencode_safety as safety
import opencode_via_tests as audit
from opencode_safety_tests import fake_stat


class DaemonGenerationTests(unittest.TestCase):
    def cleanup_fixture(self, root):
        d,_=self.fixture(root)
        successor=safety.Identity(125,458)
        home=Path(root)/'private-home';home.mkdir(mode=0o700)
        d.env={'HOME':str(home)}
        d.proc.root=Path(root)/'proc'
        (d.proc.root/str(successor.pid)).mkdir(parents=True)
        d.proc.stat.return_value={'pid':successor.pid,'start_ticks':successor.start_ticks}
        d.proc.read.return_value=b'HOME='+os.fsencode(home)+b'\0'
        d.identities.add(d.daemon)
        live={d.daemon:False,successor:True}
        d.proc.alive.side_effect=lambda identity:live[identity]
        d._lock_free=mock.Mock(side_effect=lambda _path:not live[successor])
        d._lock_rows=mock.Mock(side_effect=lambda:{path:[(successor.pid,'FLOCK')]
            if live[successor] else [] for path in d._lock_paths()})
        d._pgrep_clear=mock.Mock(return_value=True)
        return d,successor,live

    def test_cleanup_registers_unseen_lock_holder_before_waiting_for_exit(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
            d,successor,live=self.cleanup_fixture(root)
            d.pinned=Path(root)/'absent-pin'
            d.secrecy_scan=mock.Mock(return_value={'secret_absent':True,'payload_captures':0})
            with mock.patch('opencode_driver.safety.verify_binary'), \
                    mock.patch('opencode_driver.time.sleep',side_effect=lambda _s:live.update({successor:False})):
                proof=d.finish()
            self.assertTrue(proof['proven'])
            self.assertIn(successor,d.identities)
            self.assertEqual(d.daemon_generations[0]['pid'],125)
            self.assertEqual(d.daemon_generations[0]['start_ticks'],458)
            self.assertEqual(d.daemon_generations[0]['discovery'],'private-locks-and-home')

    def test_cleanup_unseen_holder_with_foreign_home_or_unknown_identity_blocks(self):
        for fault in ('home','identity','lock-kind'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
                d,successor,live=self.cleanup_fixture(root)
                if fault=='home':d.proc.read.return_value=b'HOME=/unowned-fixture\0'
                if fault=='identity':d.proc.stat.return_value=None
                if fault=='lock-kind':d._lock_rows.return_value=None;d._lock_rows.side_effect=lambda:{
                    path:[(successor.pid,'POSIX')] for path in d._lock_paths()}
                with mock.patch('opencode_driver.time.sleep',side_effect=lambda _s:live.update({successor:False})):
                    with self.assertRaisesRegex(safety.Blocked,'private.*(locker|daemon)'):
                        d.stop()

    def test_not_verified_retains_role_and_false_or_unknown(self):
        for value in (False, None):
            with self.subTest(liveness=value):
                proc=safety.ProcReader()
                with mock.patch.object(proc,'alive',return_value=value), \
                        safety.block_context(process_role='daemon'):
                    try: proc.verify(safety.Identity(123,456))
                    except safety.Blocked as error: record=safety.blocking_record(error)
                    else: self.fail('unverified identity passed')
                self.assertEqual(record.get('process'),{'role':'daemon','liveness':value})

    def fixture(self, directory):
        d=Driver('release','fp','pin',Path(directory)/'evidence')
        d.state.mkdir(mode=0o700); d.runtime.mkdir(mode=0o700)
        d.daemon=safety.Identity(123,456); d.phase_kind='release'; d.project=Path(directory)
        d.proc=mock.Mock(); d.proc.alive.return_value=False
        d._locks_free=mock.Mock(return_value=True)
        d._daemon_log_mark={'device':None,'inode':None,'offset':0}
        with sqlite3.connect(d.state/'store.sqlite3') as store:
            store.execute('CREATE TABLE turns(state TEXT,ended_seq INTEGER)')
        (d.state/'store.sqlite3').chmod(0o600)
        summary={'mode':'idle','entry':'entered','disposition':'clean','store':'joined',
            'pending_joins':0,'failed_joins':0,'uncertain_owners':0,'host_failure':None,
            'uncommitted_turns':0,'unresolved_turns':0,'store_failed':False,'blob_tasks':0,
            'unstarted_dispatchers':0,'unclosed_sessions':0,'unjoined_dispatchers':0}
        (d.state/'via.log').write_text(json.dumps({'daemon_shutdown':summary})+'\n')
        (d.state/'via.log').chmod(0o600)
        return d,summary

    def test_cli_reply_cannot_be_served_by_unregistered_successor(self):
        # G1 verified alive before execute; G2 writes the sole clean-idle record.
        # A later log-offset proof must never reinterpret that as G1's clean end.
        for holders in ([(125,'FLOCK')],[(123,'FLOCK'),(125,'FLOCK')]):
            with self.subTest(holders=holders),tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
                d,_=self.fixture(root);d._binary=Path('FAKE-via')
                d.proc.alive.return_value=True
                d._lock_rows=mock.Mock(return_value={path:holders for path in d._lock_paths()})
                def served(*args,**kwargs):
                    d.proc.alive.return_value=False
                    return 0,b'{"pid":125}',b''
                d.execute=mock.Mock(side_effect=served)
                d.start=mock.Mock()
                with self.assertRaisesRegex(safety.Blocked,'CLI daemon generation lock ownership changed'):
                    d.via(['daemon','status'])
                d.execute.assert_called_once();d.start.assert_not_called()

    def test_stopped_daemon_fd_snapshot_skips_closed_entries_and_keeps_identity_checks(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
            d,_=self.fixture(root)
            vendor=safety.Identity(124,457);d.vendor_identity=vendor
            d.proc=safety.ProcReader(Path(root)/'proc')
            for identity in (d.daemon,vendor):
                folder=d.proc.root/str(identity.pid);(folder/'fd').mkdir(parents=True)
                (folder/'stat').write_bytes(fake_stat(
                    pid=identity.pid,ticks=identity.start_ticks,state='T'))
            (d.proc.root/'124/fd/0').symlink_to('pipe:[7788]')
            (d.proc.root/'123/fd/7').symlink_to('pipe:[7788]')
            (d.proc.root/'123/fdinfo').mkdir()
            (d.proc.root/'123/fdinfo/7').write_bytes(b'flags: 01\n')
            readlink=safety.os.readlink
            def gone(entry,*,dir_fd):
                if entry=='8':raise FileNotFoundError(safety.errno.ENOENT,'FAKE closed fd')
                return readlink(entry,dir_fd=dir_fd)
            with mock.patch.object(safety.os,'listdir',return_value=['8','7']), \
                    mock.patch.object(safety.os,'readlink',side_effect=gone):
                self.assertTrue(d._stdin_held(d.daemon,vendor))

    def test_unclean_idle_evidence_never_adopts_a_generation(self):
        for fault in ('locks','open-turn','missing-log','incomplete','non-idle','unknown'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
                d,summary=self.fixture(root)
                if fault=='locks': d._locks_free.return_value=False
                if fault=='open-turn':
                    with sqlite3.connect(d.state/'store.sqlite3') as store:
                        store.execute("INSERT INTO turns VALUES('queued',NULL)")
                if fault=='missing-log': (d.state/'via.log').unlink()
                if fault in {'incomplete','non-idle'}:
                    summary['disposition' if fault=='incomplete' else 'mode']='incomplete' if fault=='incomplete' else 'force'
                    (d.state/'via.log').write_text(json.dumps({'daemon_shutdown':summary})+'\n')
                if fault=='unknown': d.proc.alive.return_value=None
                d.start=mock.Mock()
                with self.assertRaises(safety.Blocked): d._refresh_daemon()
                d.start.assert_not_called()

    def test_old_shutdown_record_cannot_prove_current_generation(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
            d,_summary=self.fixture(root)
            log=d.state/'via.log'; info=log.stat()
            d._daemon_log_mark={'device':info.st_dev,'inode':info.st_ino,'offset':info.st_size}
            with self.assertRaisesRegex(safety.Blocked,'idle exit'):
                d._refresh_daemon()

    def test_one_log_rotation_preserves_current_generation_boundary(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
            d,summary=self.fixture(root)
            log=d.state/'via.log'; info=log.stat()
            d._daemon_log_mark={'device':info.st_dev,'inode':info.st_ino,'offset':info.st_size}
            log.rename(d.state/'via.log.1')
            log.write_text(json.dumps({'daemon_shutdown':summary})+'\n'); log.chmod(0o600)
            d.start=mock.Mock()
            d._refresh_daemon()
            d.start.assert_called_once_with('release',Path(root))

    def test_exit_proof_cannot_hide_unknown_second_liveness(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
            d,_summary=self.fixture(root)
            d.proc.alive.side_effect=[False,None]
            with self.assertRaises(safety.Blocked) as caught: d._refresh_daemon()
            self.assertEqual(safety.blocking_record(caught.exception)['process'],
                             {'role':'daemon','liveness':None})

    def test_retirement_waits_for_daemon_exit_before_bootstrap(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
            d,_summary=self.fixture(root)
            old=d.daemon; vendor=safety.Identity(124,457); successor=safety.Identity(125,458)
            d.vendor_identity=vendor
            live={old:True,vendor:True,successor:True}
            d.proc.alive.side_effect=lambda identity:live[identity]
            d.proc.gone.side_effect=lambda identity:live[identity] is False
            d._vendor_sid=mock.Mock(return_value='ses_fixture')
            d.close_event_capture=mock.Mock()
            def retired(identities,*_args,**_kwargs):
                # Vendor death occurs before the daemon's bounded final shutdown
                # completes, as in run21; no CLI may race that closing interval.
                for identity in identities: live[identity]=False
            d._await_gone=mock.Mock(side_effect=retired)
            d._operation('idle_retirement',{'session':'s_fixture'})
            self.assertIs(d.proc.alive(old),False,'retirement returned with a closing daemon')
            self.assertEqual([call.args[0] for call in d._await_gone.call_args_list],[[vendor],[old]])
            d.start=mock.Mock(side_effect=lambda *_args:setattr(d,'daemon',successor))
            d._host_record=mock.Mock(return_value=None)
            d._bootstrap_vendor=mock.Mock()
            d.ensure_vendor()
            self.assertEqual(d.daemon,successor)
            d._bootstrap_vendor.assert_called_once()


@unittest.skipUnless(audit.AVAILABLE,'optional real-VIA audit: build release, failpoints and via-fake-agent')
class RealDaemonTests(unittest.TestCase):
    def test_cleanup_discovers_real_daemon_autostart_behind_retired_identity(self):
        with audit.ViaFixture('failpoints',auto_start=False) as fixture:
            d=fixture.driver
            execute=d.execute
            def accelerated(argv,**kwargs):
                if argv[0]==str(d.paths['failpoints']):
                    kwargs['env']={**kwargs['env'],'VIA_TEST_IDLE_EXIT_MS':'1200'}
                return execute(argv,**kwargs)
            d.execute=accelerated
            d.start('failpoints',fixture.project)
            old=d.daemon
            d.stop()
            # As in run21, a CLI starts a successor before Driver registers it;
            # cleanup still has only the exited predecessor saved.
            code,out,err=d.execute([str(d.paths['failpoints']),'daemon','status','--json'],
                                  env=d.env,cwd=fixture.project,timeout=10)
            self.assertEqual((code,err),(0,b''))
            successor=json.loads(out)['pid']
            row=d.proc.stat(successor)
            self.assertIsNotNone(row)
            d.daemon=old
            proof=d.finish()
            self.assertTrue(proof['proven'])
            recorded=[r for r in d.daemon_generations if r['pid']==successor]
            self.assertEqual(len(recorded),1)
            self.assertEqual(recorded[0]['start_ticks'],row['start_ticks'])
            self.assertEqual(recorded[0]['discovery'],'private-locks-and-home')
            self.assertIs(d.proc.alive(safety.Identity(successor,row['start_ticks'])),False)

    def test_real_idle_exit_during_retirement_then_bootstrap(self):
        # The existing test-only interval accelerates the designed idle expiry;
        # no client or keepalive runs during retirement. The release stays 60 s.
        with audit.ViaFixture('failpoints',auto_start=False) as fixture:
            d=fixture.driver
            execute=d.execute
            def accelerated(argv,**kwargs):
                if argv[0]==str(d.paths['failpoints']):
                    kwargs['env']={**kwargs['env'],'VIA_TEST_IDLE_EXIT_MS':'1200'}
                return execute(argv,**kwargs)
            d.execute=accelerated
            d.start('failpoints',fixture.project)
            threads,replies=audit.RealViaTests().bootstrap_policy(fixture)
            try: d.ensure_vendor()
            finally:
                for thread in threads: thread.join(5)
            first=d.daemon
            result=d._operation('idle_retirement',{'session':d._bootstrap_session})
            self.assertTrue(result['stop_proven'])
            self.assertIs(d.proc.alive(first),False)
            # A fresh FAKE provider creates the audit's loopback test peer;
            # the scripted vendor itself is not an HTTP model client.
            d._namespace_bootstraps.clear()
            (d.namespace/'opencode.json').unlink()
            replies.clear()
            try: d.ensure_vendor()
            finally:
                for thread in threads: thread.join(5)
            self.assertNotEqual(d.daemon,first)
            self.assertIs(d.proc.alive(d.daemon),True)
            records=[json.loads(path.read_text()) for path in sorted(d.evidence.glob('*daemon-generation*.json'))]
            self.assertEqual([row['generation'] for row in records if row['event']=='started'],[1,2])
            ended=[row for row in records if row['event']=='clean-idle-exit']
            self.assertEqual(len(ended),1)
            self.assertTrue(ended[0]['locks_released'])
            self.assertEqual(ended[0]['open_turns'],0)
            self.assertEqual(ended[0]['shutdown']['mode'],'idle')
