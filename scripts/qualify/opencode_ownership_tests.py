"""Run-history ownership and cleanup regressions for OpenCode packet §13."""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock

import opencode_cases as cases
import opencode_driver as runtime
import opencode_safety as safety


def register(driver,path,kind):
    """Register test-created roots when exercising the registry implementation."""
    if hasattr(driver,'ownership'): driver.ownership.register(path,kind)
    return path


def store(path):
    with sqlite3.connect(path/'store.sqlite3') as database:
        database.execute('create table if not exists fixture (value text)')
        database.execute('insert into fixture values (?)',('clean',))


def full_sequence(secret_namespace=None):
    """Real phase orchestration/namespace/start/finish; only external effects are fake."""
    with tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
        proc=mock.Mock(); proc.stat.return_value={'start_ticks':20}; proc.alive.return_value=False
        d=runtime.Driver('release','failpoints','pinned',Path(root)/'evidence',proc=proc)
        def repo(path,*args,**kwargs):
            safety.private_directory(path)
            (path/'AGENTS.md').write_text('private fixture\n')
        def minimal(helpers,*args):
            (helpers/'rg').symlink_to('/usr/bin/rg')
            return str(helpers)+':/usr/bin:/bin',{'checked':True}
        def command(args):
            if args==['daemon','status']: store(d.state); return {'pid':10}
            raise AssertionError('only a fake daemon status is permitted')
        with mock.patch.object(runtime,'ANCHOR_SOCKET_TAIL',0), \
             mock.patch.object(safety,'init_fixture_repo',side_effect=repo), \
             mock.patch.object(safety,'build_minimal_path',side_effect=minimal), \
             mock.patch.object(safety,'verify_binary'), mock.patch.object(safety,'validate_path'), \
             mock.patch.object(safety,'verify_daemon_program'), \
             mock.patch.object(d,'build',return_value='1'*64), mock.patch.object(d,'_refresh_inventory'), \
             mock.patch.object(d,'_locks_owned',return_value=True), \
             mock.patch.object(d,'_lock_free',return_value=True), \
             mock.patch.object(d,'_pgrep_clear',return_value=True), mock.patch.object(d,'via',side_effect=command):
            d.prepare()
            project=register(d,d.evidence/'project','vendor-private'); project.mkdir(mode=0o700)
            d.fixtures['fixture']={'path':project,'config':{}}
            d.start('release',project)
            old=d.namespace; log=old/'log'; log.mkdir()
            (log/'vendor.log').write_bytes(b'private-password' if secret_namespace=='old' else b'fixture-provider-secret')
            with (old/'opencode.db').open('wb') as file: file.truncate(runtime.OBSERVATION_BYTES+1)
            # The old VIA Store must use its backup too, not the per-file cap.
            with sqlite3.connect(d.state/'store.sqlite3') as database:
                database.execute('create table padding (value blob)')
                database.execute('insert into padding values (zeroblob(?))',(runtime.OBSERVATION_BYTES+1,))
            d.secret_forms=[b'fixture-provider-secret']
            proc.read.return_value=safety.PASSWORD_KEY+b'=private-password\0'
            d.vault.read_once(proc,safety.Identity(11,20))
            class Adapter:
                def execute(self,kind,**args):
                    if kind=='namespace': return d._operation(kind,args)
                    if kind=='build_hashes': return {'release':'1'*64,'test-failpoints':'2'*64}
                    if kind in {'phase_begin','phase_guard','phase_build'}: return {}
                    if kind=='daemon_stop_proof': return {'proven':True}
                    if kind=='direct_seed': return {key:True for key in (
                        'exact_namespace_layout','chain_0700','known_connection_shape',
                        'secret_absent_from_integration','direct_child_gone','descendants_gone','pgrep_absent')} | {'model_requests':0}
                    if kind=='credential_refusal':
                        d.start('release',project)
                        log=d.namespace/'log'; log.mkdir()
                        (log/'vendor.log').write_bytes(b'private-password' if secret_namespace=='new' else b'clean')
                        return {'code':'unexpected_credential_state','cached':False,
                                'session_creates':0,'prompt_submits':0,'credential_gets':0}
                    raise AssertionError('unapproved fake phase operation')
            def fake_case(driver,case): case.check('FAKE external qualification predicate',True)
            functions={name:fake_case for name in cases.CASES if name!='credential_shape'}
            phases=[]
            with mock.patch.dict(cases.CASES,functions):
                for phase in cases.PHASES:
                    rows=cases.run_phase(Adapter(),phase.name)
                    if any(row['result']!='pass' for row in rows): raise AssertionError('mock phase failed')
                    phases.append(phase.name)
            return phases,d.finish()


class OwnershipTests(unittest.TestCase):
    def driver(self,root): return runtime.Driver('release','fp','pinned',Path(root)/'evidence')

    def test_full_mocked_phase_sequence_preserves_old_namespace_and_store(self):
        phases,result=full_sequence()
        self.assertEqual(phases,[phase.name for phase in cases.PHASES])
        self.assertEqual(phases[-1],'credentials')
        self.assertTrue(result['stopped']); self.assertTrue(result['secrecy_scan']['secret_absent'])
        self.assertEqual(result['secrecy_scan']['vendor_private_synthetic_files'],1)
        self.assertEqual(result['secrecy_scan']['store_backups'],2)

    def test_password_in_either_retained_namespace_blocks_finish(self):
        for namespace in ('old','new'):
            with self.subTest(namespace=namespace), self.assertRaisesRegex(safety.Blocked,'final secrecy scan failed'):
                full_sequence(namespace)

    def test_version_probe_nonregular_entries_are_vendor_private(self):
        with tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
            d=self.driver(root); probe=d.state/'vendor'/'opencode'/'probe'; probe.mkdir(parents=True)
            target=Path(root)/'target'; target.write_bytes(b'not followed')
            (probe/'link.log').symlink_to(target); os.mkfifo(probe/'stream.log')
            result=d.secrecy_scan()
            self.assertTrue(result['secret_absent']); self.assertEqual(result['metadata_only_files'],2)

    def test_unregistered_regular_evidence_file_blocks(self):
        with tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
            d=self.driver(root); (d.evidence/'unregistered.bin').write_bytes(b'clean')
            with self.assertRaisesRegex(safety.Blocked,'unregistered evidence file'): d.secrecy_scan()

    def test_credential_checks_start_below_registered_root(self):
        with tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
            d=self.driver(root); folder=register(d,d.evidence/'credential-labelled-root','via-owned'); folder.mkdir()
            (folder/'via.log').write_bytes(b'clean')
            self.assertTrue(d.secrecy_scan()['secret_absent'])

    def test_hash_seeds_produce_identical_full_sequence_verdicts(self):
        results=[]
        code="import json,sys; sys.path.insert(0,'scripts/qualify'); from opencode_ownership_tests import full_sequence; print(json.dumps(full_sequence(),sort_keys=True))"
        for seed in ('0','1','7','42'):
            env=dict(os.environ); env['PYTHONHASHSEED']=seed
            result=subprocess.run([sys.executable,'-B','-c',code],env=env,capture_output=True,timeout=30)
            self.assertEqual(result.returncode,0,result.stderr.decode())
            results.append(json.loads(result.stdout))
        self.assertTrue(all(row==results[0] for row in results[1:]))

    def test_via_disappearance_restarts_the_whole_scan_up_to_three_attempts(self):
        for persistent in (False,True):
            with self.subTest(persistent=persistent), tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
                d=self.driver(root); d.state.mkdir(); path=d.state/'via.log'; path.write_bytes(b'clean')
                original=Path.open; calls=[]
                def vanished(candidate,*args,**kwargs):
                    if candidate==path:
                        calls.append(candidate)
                        if persistent or len(calls)==1: raise FileNotFoundError(str(candidate))
                    return original(candidate,*args,**kwargs)
                with mock.patch.object(Path,'open',vanished):
                    if persistent:
                        with self.assertRaisesRegex(safety.Blocked,'VIA scan entry vanished'): d.secrecy_scan()
                        self.assertEqual(len(calls),3)
                    else:
                        self.assertTrue(d.secrecy_scan()['secret_absent']); self.assertEqual(len(calls),2)

    def test_leak_seen_before_vanishing_survives_whole_scan_retry(self):
        for value in (b'private-password',b'h_private_bearer',b'fixture-provider-secret'):
            with self.subTest(value=value), tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
                d=self.driver(root); d.state.mkdir()
                leaking=d.state/'a.log'; leaking.write_bytes(value)
                rotating=d.state/'b.log'; rotating.write_bytes(b'clean')
                proc=mock.Mock(); proc.read.return_value=safety.PASSWORD_KEY+b'=private-password\0'
                d.vault.read_once(proc,safety.Identity(11,20))
                d.handles={'s_fixture':'h_private_bearer'}; d.secret_forms=[b'fixture-provider-secret']
                original=Path.open; calls=[]
                def vanished(path,*args,**kwargs):
                    if path==rotating:
                        calls.append(path)
                        if len(calls)==1:
                            leaking.unlink()
                            raise FileNotFoundError(str(path))
                    return original(path,*args,**kwargs)
                with mock.patch.object(Path,'open',vanished): result=d.secrecy_scan()
                self.assertEqual(result['scan_attempts'],2)
                self.assertFalse(result['secret_absent'])

    def test_new_unstarted_state_does_not_require_a_store(self):
        with tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
            d=self.driver(root); d.state.mkdir(); store(d.state)
            if hasattr(d,'ownership'): d.ownership.daemon_started(d.state)
            else: d._daemon_ran=True  # Baseline RED: the old global latch.
            with mock.patch.object(d,'stop',return_value={'proven':True}), \
                 mock.patch.object(safety,'init_fixture_repo'), mock.patch.object(d,'_refresh_inventory'):
                d._operation('namespace',{})
            self.assertTrue(d.secrecy_scan()['secret_absent'])

    def test_cleanup_keeps_original_failure_ahead_of_secondary_provider_failure(self):
        with tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
            d=self.driver(root); calls=[]
            def close(*args): calls.append(True); raise safety.Blocked('secondary provider failure')
            d.mock_providers={'fixture':types.SimpleNamespace(__exit__=close)}
            with mock.patch.object(d,'stop',side_effect=safety.Blocked('original stop failure')):
                with self.assertRaisesRegex(safety.Blocked,'original stop failure'): d.finish()
            self.assertEqual(calls,[True])

    def test_longest_prefix_is_stable_and_roots_cannot_be_reclassified(self):
        from opencode_ownership import OwnershipRegistry
        with tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
            root=Path(root); ownership=OwnershipRegistry(root/'evidence')
            state=ownership.register_state(root/'evidence'/'first-state')
            namespace=ownership.register(state/'vendor'/'opencode'/'namespace','vendor-private')
            ownership.daemon_started(state)
            ownership.register_state(root/'evidence'/'second-state')
            self.assertEqual(ownership.classify(namespace/'log'/'vendor.log').kind,'vendor-private')
            self.assertEqual(ownership.classify(state/'via.log').kind,'via-owned')
            self.assertTrue(ownership.classify(state).started)
            ownership.register_state(state)
            self.assertTrue(ownership.classify(state).started)
            self.assertIsNone(ownership.classify(ownership.evidence/'unknown.json'))
            with self.assertRaisesRegex(safety.Blocked,'cannot be reclassified'):
                ownership.register(namespace,'via-owned')

    def test_provided_pin_at_evidence_root_is_an_explicit_artifact(self):
        with tempfile.TemporaryDirectory(prefix='via-ownership-') as root:
            evidence=Path(root)/'evidence'; evidence.mkdir(mode=0o700)
            pin=evidence/'opencode'; pin.write_bytes(b'FAKE immutable binary')
            d=runtime.Driver('release','fp',pin,evidence)
            self.assertEqual(d.ownership.classify(pin).kind,'helper')
            self.assertTrue(d.secrecy_scan()['secret_absent'])


if __name__=='__main__': unittest.main()
