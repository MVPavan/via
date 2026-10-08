"""Offline private-run-root and ancestor-sentinel regressions (packet §13)."""
import importlib
import json
import os
import sqlite3
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import opencode_driver as transport
import opencode_safety as safety
import opencode_reply_tests as replies
from opencode_runroot_tests import private_runtime


class IsolationTests(unittest.TestCase):
    def module(self): return importlib.import_module('opencode_runroot')

    def test_every_handoff_uses_the_external_run_root(self):
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            base=Path(folder);work=base/'run';work.mkdir(mode=0o700)
            d=transport.Driver('release','fp','pin',base/'evidence',run_root=work)
            with mock.patch.object(d,'_copy_programs'):
                d.prepare()
            project=d.fixture('FAKE-project',{})
            for provider in d.mock_providers.values(): self.addCleanup(provider.__exit__,None,None,None)
            for path in (d.state,d.runtime,d.helpers,d.home,d.namespace,project,
                         *(Path(d.env[key]) for key in safety.PRIVATE_PARTS),
                         *(Path(value) for value in d.namespace_env.values())):
                self.assertTrue(path.is_relative_to(work),path.name)
            self.assertFalse((d.evidence/'state').exists())

    def test_fresh_root_is_owned_private_and_has_bounded_socket_geometry(self):
        module=self.module()
        with private_runtime() as parent:
            root=module.create_run_root()
            try:
                self.assertEqual(root.parent,parent)
                self.assertEqual(stat.S_IMODE(root.stat().st_mode),0o700)
                self.assertEqual(root.stat().st_uid,os.getuid())
                self.assertLessEqual(len(os.fsencode(root/'runtime'))+transport.ANCHOR_SOCKET_TAIL,
                                     transport.SOCKET_BYTES)
                proof=module.check_ancestors(root)
                self.assertEqual(proof['ancestors'],['runtime-parent','ancestor-1','filesystem-root'])
                self.assertIn('.claude/skills',proof['checked_names'])
            finally: root.rmdir()

    def test_every_discovery_name_blocks_without_reading_its_contents(self):
        module=self.module()
        for name in module.DISCOVERY_NAMES:
            with self.subTest(name=name),tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
                base=Path(folder);root=base/'run';root.mkdir(mode=0o700);root.chmod(0o700)
                path=base/name;path.parent.mkdir(parents=True,exist_ok=True);path.touch()
                with mock.patch.object(Path,'read_bytes',side_effect=AssertionError('must not read')):
                    with self.assertRaisesRegex(safety.Blocked,'ancestor discovery source present'):
                        module.check_ancestors(root)

    def test_readonly_program_copy_is_hashed_and_not_linked(self):
        module=self.module()
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            base=Path(folder);source=base/'source';source.write_bytes(b'FAKE-executable');source.chmod(0o500)
            target=module.copy_program(source,base/'bin'/'via')
            self.assertEqual(safety.sha256(target),safety.sha256(source))
            self.assertNotEqual(target.stat().st_ino,source.stat().st_ino)
            self.assertEqual(stat.S_IMODE(target.stat().st_mode),0o500)
            self.assertEqual(target.stat().st_nlink,1)

    def test_cleanup_requires_process_proof_and_removes_only_exact_fresh_root(self):
        module=self.module()
        with private_runtime():
            root=module.create_run_root()
            try:
                with self.assertRaisesRegex(safety.Blocked,'run root cleanup needs process absence'):
                    module.remove_run_root(root,{'processes_gone':False})
                self.assertTrue(root.exists())
                self.assertTrue(module.remove_run_root(root,{'proven':True,'processes_gone':True,'pgrep_clear':True})['removed'])
                self.assertFalse(root.exists())
            finally:
                if root.exists(): root.rmdir()

    def sentinel(self,folder,*,skills=True,agents=False,sensitive=True):
        d=transport.Driver('release','fp','pin',Path(folder)/'evidence');d.prepare()
        receipts=[{'local_marker':True,'ancestor_agents':agents,'ancestor_skills':skills},
                  {'local_marker':False,'ancestor_agents':sensitive,'ancestor_skills':sensitive},
                  {'local_marker':True,'ancestor_agents':False,'ancestor_skills':False}]
        for receipt in receipts: receipt.update(models=['fixture-free'],received=True)
        def materialize(name,_project,_config):
            provider=mock.Mock();provider.receipt.return_value=receipts[len(d.mock_providers)]
            d.mock_providers[name]=provider;return {}
        d._materialize_fixture=mock.Mock(side_effect=materialize);d._mock_turn=mock.Mock()
        d._last_auxiliary_bindings=True;d.admit_automatic_models=mock.Mock()
        d._instruction_facts=mock.Mock(return_value=[{'source':'native-instruction-state','ancestor_skills':True}])
        return d

    def test_ancestor_skill_crossing_is_record_only_with_native_evidence(self):
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            d=self.sentinel(folder);result=d._repository_sentinel()
            self.assertTrue(result['project_boundary'])
            self.assertTrue(result['ancestor_agents_absent'])
            self.assertNotIn('ancestor_skills_absent',result)
            record=json.loads(next(d.evidence.glob('*ancestor-skills.json')).read_text())
            self.assertEqual(record['disposition'],'record-only')
            self.assertTrue(record['crossed']);self.assertFalse(record['gate'])
            self.assertTrue(record['native_instruction_state'])

    def test_ancestor_instructions_still_block(self):
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            d=self.sentinel(folder,agents=True)
            with self.assertRaisesRegex(safety.Blocked,'vendor repository walk-up crossed private Git boundary'):
                d._repository_sentinel()

    def test_control_must_remain_sensitive_to_both_sentinels(self):
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            d=self.sentinel(folder,sensitive=False)
            with self.assertRaisesRegex(safety.Blocked,'ancestor sentinel control not sensitive'):
                d._repository_sentinel()

    def test_receipt_failure_has_its_own_flag_and_preserves_first_reason(self):
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            d=replies.ReplyTests().driver(folder,{})
            d.start=mock.Mock();d.fixtures={'FAKE':{'path':d.project,'config':{}}}
            d.mock_providers={'FAKE':mock.Mock(requests=0)}
            d.via=mock.Mock(side_effect=safety.Blocked('FAKE first failure'))
            d._record=mock.Mock(side_effect=safety.Blocked('FAKE evidence sink unavailable'))
            with self.assertRaisesRegex(safety.Blocked,'FAKE first failure') as raised:
                d._mock_turn(d.project,'FAKE')
            block=safety.blocking_record(raised.exception)
            self.assertTrue(block.get('mock_receipt_failed'))
            self.assertNotIn('reply_retention_failed',block)

    def test_admission_refreshes_http_deadline_before_any_read(self):
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            d=replies.ReplyTests().driver(folder,{})
            d.phase_deadline=123;d._http.deadline=1
            d.vault.names=mock.Mock(return_value=set())
            def catalog(_identity):
                self.assertEqual(d._http.deadline,123)
                raise safety.Blocked('FAKE stop after first admission read')
            d._catalog=mock.Mock(side_effect=catalog)
            with self.assertRaisesRegex(safety.Blocked,'FAKE stop after first admission read'):
                d.spending_check(args=['spawn','--model',safety.MOCK_IDENTITY])

    def test_sentinel_receipt_labels_possible_rebootstrap_traffic(self):
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            d=replies.ReplyTests().driver(folder,{})
            d.start=mock.Mock();d.fixtures={'FAKE':{'path':d.project,'config':{}}}
            d.mock_providers={'FAKE':mock.Mock(requests=0)}
            def cli(args):
                if args[0]=='spawn': d._bootstrap_count+=1;return {'turn':'s_FAKE/1'}
                return {'state':'completed'}
            d.via=mock.Mock(side_effect=cli);d._mock_turn(d.project,'FAKE')
            record=json.loads(next(d.evidence.glob('*mock-receipt.json')).read_text())
            self.assertEqual(record.get('rebootstraps_during_turn'),1)
            self.assertEqual(record.get('count_scope'),'includes-rebootstrap-traffic')

    def test_native_exports_are_sanitized_before_private_storage_is_removed(self):
        module=self.module()
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            base=Path(folder);work=base/'run';work.mkdir(mode=0o700)
            d=transport.Driver('release','fp','pin',base/'evidence',run_root=work)
            ns=work/'state'/'vendor'/'opencode'/'FAKE';ns.mkdir(parents=True)
            d.ownership.register(ns,'vendor-private')
            database=ns/'opencode.db';marker=module.MARKERS['ancestor_skills']
            secret=b'FAKE-native-private-value';d.secret_forms=[secret]
            with sqlite3.connect(database) as conn:
                conn.execute('create table instruction_blob (hash text,value text)')
                conn.execute('create table instruction_state (session_id text,epoch_start int,through_seq int,initial_values text,current_values text)')
                raw=json.dumps([{'text':marker+' '+secret.decode()}]);digest='a'*64
                conn.execute('insert into instruction_blob values (?,?)',(digest,raw))
                refs=json.dumps({'core/instructions':digest})
                conn.execute('insert into instruction_state values (?,?,?,?,?)',('ses_FAKE',1,3,refs,refs))
                conn.execute('create table session_v2 (id text,directory text,model text,cost real,tokens_input int,tokens_output int)')
                conn.execute('insert into session_v2 values (?,?,?,?,?,?)',('ses_FAKE',str(work),json.dumps({'providerID':'oclive-mock','id':'fixture-free'}),0,2,3))
                conn.execute('create table session_message (type text,seq int,data text)')
                conn.execute('insert into session_message values (?,?,?)',('assistant',1,json.dumps({'content':[{'type':'text','text':secret.decode()}]})))
            (ns/'opencode.log').write_bytes(b'2026-10-08T00:00:00.001 config.updated '+secret)
            sources=module.native_facts(work,d._reply_protected,d.ownership)
            d._record('native-records',{'sources':sources})
            evidence=next(d.evidence.glob('*native-records.json')).read_bytes()
            self.assertNotIn(secret,evidence)
            native=sources[0]['instruction_state'][0]['snapshots'][0]
            self.assertTrue(native['ancestor_skills'])
            self.assertEqual(native['blobs'][0]['native_reference'],digest)
            self.assertNotEqual(native['blobs'][0]['sha256'],digest)
            self.assertTrue(d.secrecy_scan()['secret_absent'])

    def test_ancestor_block_retains_path_classes_and_checked_names(self):
        module=self.module()
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            base=Path(folder);root=base/'run';root.mkdir(mode=0o700);root.chmod(0o700);(base/'AGENTS.md').touch()
            with self.assertRaisesRegex(safety.Blocked,'ancestor discovery source present') as raised:
                module.check_ancestors(root)
            record=raised.exception.ancestor_discovery
            self.assertIn('AGENTS.md',record['checked_names'])
            self.assertEqual(record['ancestors'],['runtime-parent'])
            self.assertFalse(record['clear'])

    def test_cleanup_removes_readonly_program_directory_after_proof(self):
        module=self.module()
        with private_runtime():
            root=module.create_run_root()
            binary=root/'bin';binary.mkdir(mode=0o700)
            (binary/'FAKE').write_bytes(b'FAKE');(binary/'FAKE').chmod(0o500);binary.chmod(0o500)
            try:
                module.remove_run_root(root,{'proven':True,'processes_gone':True,'pgrep_clear':True})
                self.assertFalse(root.exists())
            finally:
                if binary.exists(): binary.chmod(0o700);(binary/'FAKE').unlink();binary.rmdir()
                if root.exists(): root.rmdir()

    def test_private_programs_are_readonly_before_pinned_readback(self):
        with tempfile.TemporaryDirectory(prefix='via-ocisolation-') as folder:
            base=Path(folder);source=base/'source';source.mkdir(mode=0o700)
            binaries=[]
            for name in ('release','failpoints','opencode'):
                path=source/name;path.write_bytes(b'FAKE-'+name.encode());path.chmod(0o500);binaries.append(path)
            source.chmod(0o500)
            work=base/'run';work.mkdir(mode=0o700)
            d=transport.Driver(*binaries,base/'evidence',run_root=work)
            observations=[]
            def verify(path):
                observations.append(Path(path))
                if stat.S_IMODE(Path(path).parent.stat().st_mode)!=0o500:
                    raise safety.Blocked('pinned binary size or read-only mode mismatch')
            try:
                with mock.patch.object(safety,'verify_binary',side_effect=verify):d._copy_programs()
                self.assertEqual(observations,[binaries[2],work/'bin'/'opencode'])
                self.assertEqual(stat.S_IMODE((work/'bin').stat().st_mode),0o500)
                self.assertTrue(all(path.is_relative_to(work) for path in (*d.paths.values(),d.pinned)))
            finally:
                source.chmod(0o700)
                if (work/'bin').exists():(work/'bin').chmod(0o700)
