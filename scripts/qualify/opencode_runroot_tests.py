"""Review-29 offline ownership, discovery and diagnostic regressions (§13)."""

import contextlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import tempfile
import unittest
from unittest import mock

import opencode_driver as transport
import opencode_runroot as roots
import opencode_safety as safety

PROOF = {'proven': True, 'processes_gone': True, 'pgrep_clear': True}


@contextlib.contextmanager
def private_runtime():
    """FAKE root-owned system ancestors around a real private fixture directory."""
    with tempfile.TemporaryDirectory(prefix='r', dir='/tmp') as folder:
        base = Path(folder)
        base.chmod(0o700)
        real = Path.lstat
        ancestors = set(base.parents)
        absent = {parent / name for parent in ancestors for name in roots.DISCOVERY_NAMES}
        def metadata(path, *args, **kwargs):
            if path in absent:
                raise FileNotFoundError
            info = real(path, *args, **kwargs)
            if path in ancestors:
                values = list(info); values[0] = stat.S_IFDIR | 0o755; values[4] = 0
                return os.stat_result(values)
            return info
        with mock.patch.object(Path, 'lstat', metadata), \
                mock.patch.dict(os.environ, {'XDG_RUNTIME_DIR': str(base)}):
            yield base


class RunRootTests(unittest.TestCase):
    def message_database(self,base):
        database=base/'vendor'/'opencode.db';database.parent.mkdir(parents=True)
        with sqlite3.connect(database) as conn:
            conn.execute('CREATE TABLE session_v2 (id,directory,model,cost,tokens_input,tokens_output)')
            conn.execute('CREATE TABLE session_message (session_id,type,seq,data)')
            conn.execute('CREATE TABLE instruction_state (session_id,epoch_start,through_seq,initial_values,current_values)')
            for sid in ('ses_FAKE_pass','ses_FAKE_block'):
                conn.execute('INSERT INTO session_v2 VALUES (?,?,?,?,?,?)',(sid,str(base),None,0,0,0))
        return database

    def test_native_messages_link_sessions_and_retain_only_closed_diagnostic_fields(self):
        secret='FAKE-private-content-path-token'
        with tempfile.TemporaryDirectory(prefix='via-ocnative-') as folder:
            base=Path(folder);database=self.message_database(base)
            messages=[('ses_FAKE_pass','assistant',1,{
                'finish':'tool-calls','error':None,'tokens':{'input':10,'output':3,'reasoning':2,
                    'cache':{'read':4,'write':0}},'content':[
                    {'type':'reasoning','text':secret},
                    {'type':'tool','name':'shell','state':{'status':'running','input':{'command':secret},'output':secret}},
                    {'type':'text','text':secret}]}),
                ('ses_FAKE_block','assistant',1,{'finish':'error',
                    'error':{'type':'provider.rate-limit','message':secret,'response':{'body':secret}},
                    'tokens':{'input':5,'output':0,'reasoning':0,'cache':{'read':0,'write':0}},
                    'content':[{'type':'tool','name':secret,'state':{'status':'error',
                        'error':{'type':secret},'input':secret,'content':secret}},
                        {'type':secret,'text':secret,'path':secret}]}),
                ('ses_FAKE_block','user',2,{'text':secret,'files':[{'path':secret}]}),
                ('ses_FAKE_block','assistant',3,{'finish':secret,'error':{'name':secret},'content':[
                    {'type':'tool','name':'read','state':{'status':'streaming','input':secret}}]})]
            with sqlite3.connect(database) as conn:
                conn.executemany('INSERT INTO session_message VALUES (?,?,?,?)',[
                    (*row[:3],json.dumps(row[3])) for row in messages])
            ownership=mock.Mock();ownership.classify.return_value.kind='vendor-private'
            ownership.ordered.return_value=[]
            result=roots.native_facts(base,lambda raw:secret.encode() in raw,ownership)[0]['messages']
            self.assertEqual([row.get('sessionID') for row in result],
                             ['ses_FAKE_block']*3+['ses_FAKE_pass'])
            failed=result[0];passed=result[-1]
            self.assertEqual(failed.get('role'),'assistant')
            self.assertEqual(failed.get('error_name'),'provider.rate-limit')
            self.assertEqual(failed.get('finish'),'error')
            self.assertEqual(failed['parts'][0]['tool_name'],'other')
            self.assertEqual(failed['parts'][0]['status'],'error')
            self.assertEqual(failed['parts'][0]['error_name'],'other')
            self.assertEqual(failed['parts'][1]['type'],'other')
            self.assertEqual(passed['parts'][1]['tool_name'],'shell')
            self.assertEqual(passed['parts'][1]['status'],'running')
            self.assertEqual(passed['tokens'],{'input':10,'output':3,'reasoning':2,'cache_read':4,'cache_write':0})
            self.assertEqual(result[2]['parts'][0]['status'],'pending')
            self.assertEqual(result[2]['finish'],'other')
            self.assertEqual([part['type'] for part in result[1]['parts']],['text','file'])
            self.assertTrue(all(part['sessionID']==row['sessionID'] for row in result for part in row['parts']))
            encoded=json.dumps(result).encode()
            self.assertNotIn(secret.encode(),encoded)
            for forbidden in ('text','input','output','path','content','response'):
                for row in result:
                    self.assertNotIn(forbidden,row)
                    self.assertTrue(all(forbidden not in part for part in row['parts']))

    def test_message_snapshot_survives_cleanup_failure_without_raw_content(self):
        with tempfile.TemporaryDirectory(prefix='via-ocnative-') as folder:
            base=Path(folder);work=base/'run';work.mkdir(mode=0o700)
            d=transport.Driver('release','fp','pin',base/'evidence',run_root=work)
            database=self.message_database(work);d.ownership.register(database.parent,'vendor-private')
            with sqlite3.connect(database) as conn:
                conn.execute('INSERT INTO session_message VALUES (?,?,?,?)',
                    ('ses_FAKE_block','assistant',1,json.dumps({'content':[{'type':'text','text':'FAKE-private-text'}]})))
            d.stop=mock.Mock(side_effect=safety.Blocked('FAKE original cleanup failure'))
            with self.assertRaisesRegex(safety.Blocked,'^FAKE original cleanup failure$'):d.finish()
            files=list(d.evidence.glob('*native-messages-before-cleanup.json'))
            self.assertEqual(len(files),1,'case message evidence disappeared with cleanup failure')
            raw=files[0].read_bytes();self.assertNotIn(b'FAKE-private-text',raw)
            self.assertEqual(json.loads(raw)['sources'][0]['messages'][0]['sessionID'],'ses_FAKE_block')

    def test_native_message_unknown_counts_and_protected_enum_values_never_leak(self):
        with tempfile.TemporaryDirectory(prefix='via-ocnative-') as folder:
            base=Path(folder);database=self.message_database(base)
            data={'tokens':{'input':-1,'output':True,'reasoning':float('nan'),
                'cache':{'read':2**70,'write':0}},'content':[{'type':'tool','name':'shell',
                'state':{'status':'FAKE-private-status','input':'FAKE-private-argument'}}]}
            with sqlite3.connect(database) as conn:
                conn.execute('INSERT INTO session_message VALUES (?,?,?,?)',
                    ('ses_FAKE_pass','assistant',1,json.dumps(data)))
            ownership=mock.Mock();ownership.classify.return_value.kind='vendor-private'
            ownership.ordered.return_value=[]
            row=roots.native_facts(base,lambda raw:b'shell' in raw,ownership)[0]['messages'][0]
            self.assertEqual(row['tokens'],{'input':None,'output':None,'reasoning':None,
                                          'cache_read':None,'cache_write':0})
            self.assertEqual(row['parts'][0]['tool_name'],'other')
            self.assertIsNone(row['parts'][0]['status'])
            self.assertNotIn(b'FAKE-private',json.dumps(row).encode())
            for fault in ('identity','sequence','shape','bound'):
                with self.subTest(fault=fault),sqlite3.connect(database) as conn:
                    conn.execute('DELETE FROM session_message')
                    conn.execute('INSERT INTO session_message VALUES (?,?,?,?)',
                        ('bad/path' if fault=='identity' else 'ses_FAKE_pass','assistant',
                         -1 if fault=='sequence' else 1,
                         json.dumps({'content':{} if fault=='shape' else [{},{}]})))
                    conn.commit()
                    with mock.patch.object(roots,'NATIVE_PARTS',1 if fault=='bound' else 65536):
                        with self.assertRaisesRegex(safety.Blocked,'native message'):
                            roots.native_facts(base,lambda raw:False,ownership)

    def test_floor_refusal_is_reported_before_invalid_params_schema_assertions(self):
        with tempfile.TemporaryDirectory(prefix='via-ocfloor-') as folder:
            d=transport.Driver('release','fp','pin',Path(folder)/'evidence')
            d.namespace=d.work/'namespace';d.project=d.work/'project'
            d.start=mock.Mock();d.fixture=mock.Mock(return_value=d.project)
            d._anchor_rows=mock.Mock(return_value=[]);d._tree_identity=mock.Mock(return_value=[])
            d.via=mock.Mock(side_effect=[
                {'capabilities':{'params':{'max_steps':{'support':'unsupported'}}}},
                {'cli_error':{'error':{'code':-32012,'data':{'kind':'admission_refused',
                    'kind2':'disk_free_floor','free_bytes':100,'floor_bytes':200}}},'exit_code':2}])
            with self.assertRaisesRegex(safety.Blocked,
                    '^private VIA state is below its configured disk free floor$'):
                d._operation('fresh_max_steps',{'max_steps':1})

    def test_private_daemon_uses_a_positive_floor_that_fits_the_runtime_filesystem(self):
        with tempfile.TemporaryDirectory(prefix='via-ocfloor-') as folder:
            d=transport.Driver('release','fp','pin',Path(folder)/'evidence')
            d.prepare()
            # FAKE via() below never creates a process; setup's synchronous
            # system commands have exited. Keep its empty short runtime scoped.
            if d.runtime.parent==Path('/tmp'):
                self.addCleanup(d.runtime.rmdir)
            d.build=mock.Mock();d._refresh_inventory=mock.Mock()
            d.via=mock.Mock(return_value={'pid':123})
            d.proc=mock.Mock();d.proc.stat.return_value={'pid':123,'start_ticks':456}
            d._locks_owned=mock.Mock(return_value=True)
            with mock.patch.object(safety,'verify_binary'), \
                    mock.patch.object(safety,'verify_daemon_program'):
                d.start('release',d.namespace)
            import json
            config=json.loads((d.state/'daemon.json').read_text())
            self.assertEqual(config.get('disk',{}).get('free_floor'),1024*1024*1024)

    def test_creation_uses_owned_runtime_parent_and_socket_bound(self):
        with private_runtime() as base:
            root = roots.create_run_root()
            try:
                self.assertEqual(root.parent, base)
                self.assertEqual(root, root.resolve())
                self.assertEqual(stat.S_IMODE(root.lstat().st_mode), 0o700)
                self.assertLessEqual(len(os.fsencode(root / 'runtime')) + 64, 107)
            finally:
                root.rmdir()

    def test_foreign_or_writable_parent_blocks_before_allocation(self):
        for foreign, mode in ((True, 0o755), (False, 0o777), (False, 0o775)):
            with self.subTest(foreign=foreign, mode=mode), private_runtime() as base:
                real = Path.lstat
                parent = base.parent
                def changed(path, *args, **kwargs):
                    info = real(path, *args, **kwargs)
                    if path == parent:
                        values = list(info); values[0] = stat.S_IFDIR | mode
                        values[4] = 49999 if foreign else 0
                        return os.stat_result(values)
                    return info
                with mock.patch.object(Path, 'lstat', changed), \
                        mock.patch.object(roots.tempfile, 'mkdtemp', return_value=str(base / 'allocated')) as allocate:
                    with self.assertRaisesRegex(safety.Blocked, 'ancestor directory unsafe'):
                        roots.create_run_root()
                    allocate.assert_not_called()

    def test_unset_unsafe_or_long_runtime_never_falls_back(self):
        for value, reason in (('', 'runtime directory unavailable'),
                              ('relative', 'runtime directory unsafe'),
                              ('long', 'runtime socket path bound')):
            with self.subTest(value=value), private_runtime() as base:
                parent = base
                if value == 'long':
                    parent = base / ('x' * 100); parent.mkdir(mode=0o700)
                setting = str(parent) if value == 'long' else value
                with mock.patch.dict(os.environ, {'XDG_RUNTIME_DIR': setting}), \
                        mock.patch.object(roots.tempfile, 'mkdtemp', return_value=str(base / 'allocated')) as allocate:
                    with self.assertRaisesRegex(safety.Blocked, reason):
                        roots.create_run_root()
                    allocate.assert_not_called()

    def test_symlink_runtime_parent_blocks_before_allocation(self):
        with private_runtime() as base:
            target = base / 'real'; target.mkdir(mode=0o700)
            alias = base / 'alias'; alias.symlink_to(target, target_is_directory=True)
            with mock.patch.dict(os.environ, {'XDG_RUNTIME_DIR': str(alias)}), \
                    mock.patch.object(roots.tempfile, 'mkdtemp', return_value=str(base / 'allocated')) as allocate:
                with self.assertRaisesRegex(safety.Blocked, 'runtime directory unsafe'):
                    roots.create_run_root()
                allocate.assert_not_called()

    def driver(self, base):
        root = base / 'run'; root.mkdir(mode=0o700)
        d = transport.Driver('release', 'fp', 'pin', base / 'evidence', run_root=root)
        d.project = root / 'project'; d.project.mkdir()
        d.fixtures = {'FAKE': {'path': d.project, 'config': {}}}
        d.daemon = safety.Identity(123, 456); d.phase_kind = 'release'
        d.proc = mock.Mock()
        roots.check_ancestors(root)
        return d

    def test_planted_discovery_source_blocks_next_start_before_admission(self):
        with private_runtime() as base:
            d = self.driver(base); (base / 'AGENTS.md').touch()
            with self.assertRaisesRegex(safety.Blocked, 'ancestor discovery source present'):
                d.start('release', d.project)
            d.proc.verify.assert_not_called()

    def test_weakened_mode_blocks_next_start(self):
        with private_runtime() as base:
            d = self.driver(base); base.chmod(0o770)
            try:
                with self.assertRaisesRegex(safety.Blocked, 'ancestor directory unsafe'):
                    d.start('release', d.project)
                d.proc.verify.assert_not_called()
            finally:
                base.chmod(0o700)

    def test_bootstrap_rechecks_ancestors_before_any_static_or_submit_work(self):
        with private_runtime() as base:
            d = self.driver(base); (base / 'AGENTS.md').touch()
            d.namespace = d.work / 'namespace'
            d._namespace_bootstraps = {d.namespace: ({}, mock.Mock(requests=0))}
            d.fixtures = {}
            d._bootstrap_static = mock.Mock(side_effect=AssertionError('FAKE admission reached'))
            d.via = mock.Mock()
            with self.assertRaisesRegex(safety.Blocked, 'ancestor discovery source present'):
                d._bootstrap_vendor()
            d._bootstrap_static.assert_not_called(); d.via.assert_not_called()

    def test_finish_rechecks_after_cleanup_before_native_reads_and_scan(self):
        with private_runtime() as base:
            d = self.driver(base); (base / 'AGENTS.md').touch()
            d.stop = mock.Mock(return_value=PROOF)
            d.secrecy_scan = mock.Mock(return_value={'secret_absent': True, 'payload_captures': 0})
            with mock.patch.object(safety, 'verify_binary'), \
                    mock.patch.object(roots, 'native_facts', return_value=[]) as native:
                with self.assertRaisesRegex(safety.Blocked, 'ancestor discovery source present'):
                    d.finish()
                d.stop.assert_called_once(); native.assert_not_called()
                d.secrecy_scan.assert_not_called()

    def test_cli_launches_and_probes_recheck_before_execution(self):
        for verb in ('describe','models','spawn','resume','steer'):
            with self.subTest(verb=verb), private_runtime() as base:
                d=self.driver(base);d._binary=Path('FAKE-via')
                d.spending_check=mock.Mock();d._admit_model=mock.Mock()
                d.execute=mock.Mock(side_effect=AssertionError('FAKE launch reached'))
                (base/'AGENTS.md').touch()
                with self.assertRaisesRegex(safety.Blocked,'ancestor discovery source present'):
                    d.via([verb])
                d.execute.assert_not_called();d.spending_check.assert_not_called()

    def test_metadata_attempt_rechecks_after_initial_start(self):
        with private_runtime() as base:
            d=self.driver(base);d.namespace=d.work/'namespace';d.start=mock.Mock()
            def rows():
                (base/'AGENTS.md').touch()
                return []
            d._anchor_rows=mock.Mock(side_effect=rows)
            d.via=mock.Mock(side_effect=AssertionError('FAKE metadata launch reached'))
            with self.assertRaisesRegex(safety.Blocked,'ancestor discovery source present'):
                d._credential_refusal()
            d.via.assert_not_called()

    def test_phase_begin_and_end_bracket_autonomous_host_launches(self):
        from opencode_cases import DriverAdapter
        for stage in ('phase_begin','phase_end'):
            with self.subTest(stage=stage),private_runtime() as base:
                d=self.driver(base);d.build=mock.Mock();d.set_phase_budget=mock.Mock()
                adapter=DriverAdapter(d)
                if stage=='phase_end':
                    adapter.execute('phase_begin',phase='FAKE',seconds=30,
                                    public_turns=0,mock_turns=0,build='release')
                (base/'AGENTS.md').touch()
                with self.assertRaisesRegex(safety.Blocked,'ancestor discovery source present'):
                    adapter.execute(stage,phase='FAKE',seconds=30,
                                    public_turns=0,mock_turns=0,build='release')

    def test_context_is_checked_conservatively(self):
        self.assertIn('CONTEXT.md', roots.DISCOVERY_NAMES)

    def test_absent_root_cleanup_never_creates_a_directory(self):
        root = Path(tempfile.mkdtemp(prefix=roots.RUN_PREFIX, dir='/tmp')); root.rmdir()
        with mock.patch.dict(os.environ, {'XDG_RUNTIME_DIR': '/tmp'}), \
                mock.patch.object(safety, 'private_directory', wraps=safety.private_directory) as create:
            result = roots.remove_run_root(root, PROOF)
            self.assertTrue(result['removed']); create.assert_not_called()
            self.assertFalse(root.exists())

    def test_instruction_diagnostic_errors_are_fixed_blocked_reasons(self):
        with tempfile.TemporaryDirectory(prefix='via-ocnative-') as folder:
            database = Path(folder) / 'opencode.db'; database.write_bytes(b'FAKE-corrupt-database')
            with self.assertRaisesRegex(safety.Blocked, '^native diagnostic unavailable$'):
                roots.instruction_facts(database)
            database.unlink()
            with sqlite3.connect(database) as conn:
                conn.execute('create table instruction_state (session_id text,epoch_start int,through_seq int,initial_values text,current_values text)')
                conn.execute('insert into instruction_state values (?,?,?,?,?)', ('ses_FAKE', 1, 1, 'not-json', 'not-json'))
            with self.assertRaisesRegex(safety.Blocked, '^native diagnostic unavailable$'):
                roots.instruction_facts(database)
            with sqlite3.connect(database) as conn:
                conn.execute('update instruction_state set initial_values=NULL')
            with self.assertRaisesRegex(safety.Blocked, '^native diagnostic unavailable$'):
                roots.instruction_facts(database)

    def test_native_export_normalizes_sqlite_errors(self):
        with tempfile.TemporaryDirectory(prefix='via-ocnative-') as folder:
            base = Path(folder)
            database = base / 'opencode.db'; database.write_bytes(b'FAKE-corrupt-database')
            ownership = mock.Mock(); ownership.classify.return_value.kind = 'vendor-private'
            with self.assertRaisesRegex(safety.Blocked, '^native diagnostic unavailable$'):
                roots.native_facts(base, lambda: (), ownership)

    def test_log_location_walk_is_once_for_all_lines_and_logs(self):
        with tempfile.TemporaryDirectory(prefix='via-ocnative-') as folder:
            base = Path(folder); project = base / 'project'; project.mkdir()
            (project / 'opencode.json').write_text('{}')
            for i in range(3):
                (base / f'log{i}.log').write_text(('config.updated ' + str(project) + '\n') * 2)
            ownership = mock.Mock(); ownership.ordered.return_value = []
            ownership.classify.return_value.kind = 'vendor-private'
            real = Path.rglob; walks = []
            def enumerated(path, pattern):
                if pattern == 'opencode.json': walks.append(pattern)
                return real(path, pattern)
            with mock.patch.object(Path, 'rglob', enumerated):
                result = roots.native_facts(base, lambda: (), ownership)
            self.assertEqual(len(walks), 1)
            self.assertEqual(len(result), 3)
            self.assertTrue(all(row['locations'] == ['project'] for source in result for row in source['log_facts']))

    def test_cleanup_permission_repair_never_swallows_walk_errors(self):
        root=Path(tempfile.mkdtemp(prefix=roots.RUN_PREFIX,dir='/tmp'))
        actual=shutil.rmtree;calls=0
        def removal(path,**kwargs):
            nonlocal calls
            calls+=1
            if calls==1:kwargs['onexc'](None,str(root),PermissionError())
            else:actual(path,**kwargs)
        def unreadable(path,*,onerror=None,**kwargs):
            if onerror:onerror(PermissionError(13,'FAKE unreadable',str(root/'child')))
            return iter(())
        try:
            with mock.patch.dict(os.environ,{'XDG_RUNTIME_DIR':'/tmp'}), \
                    mock.patch.object(roots.shutil,'rmtree',side_effect=removal), \
                    mock.patch.object(roots.os,'walk',side_effect=unreadable):
                with self.assertRaisesRegex(safety.Blocked,'private run root removal unverified'):
                    roots.remove_run_root(root,PROOF)
            self.assertTrue(root.exists(),'unreadable walk must not claim removal')
        finally:
            if root.exists():actual(root)

    def test_readonly_owned_nested_directories_are_removed_after_proof(self):
        root = Path(tempfile.mkdtemp(prefix=roots.RUN_PREFIX, dir='/tmp'))
        nested = root / 'nested'; nested.mkdir()
        (nested / 'FAKE').write_text('FAKE'); nested.chmod(0o500)
        try:
            with mock.patch.dict(os.environ, {'XDG_RUNTIME_DIR': '/tmp'}):
                self.assertTrue(roots.remove_run_root(root, PROOF)['removed'])
            self.assertFalse(root.exists())
        finally:
            if nested.exists(): nested.chmod(0o700)
            if root.exists(): shutil.rmtree(root)
