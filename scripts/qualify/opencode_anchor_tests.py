"""Anchor-phase runner regressions from diagnostic attempt 5 (§13 L14, cleanup)."""
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import opencode_driver as runtime
import opencode_safety as safety


class InventoryAdmissionTests(unittest.TestCase):
    """A helper the runner writes during a live turn is admitted by exact path and content."""
    def setUp(self):
        folder=tempfile.TemporaryDirectory();self.addCleanup(folder.cleanup)
        self.root=Path(folder.name)/'fixture'/'.opencode';self.root.mkdir(parents=True)
        self.inventory=safety.Inventory([self.root])

    def write(self,name,content=b'#!/usr/bin/python3\nFAKE\n'):
        path=self.root/name;path.write_bytes(content);path.chmod(0o500)
        return path,hashlib.sha256(content).hexdigest()

    def test_exact_helper_is_admitted_and_later_checks_pass(self):
        path,digest=self.write('completed-helper.py')
        self.inventory.admit(path,digest)
        self.assertTrue(self.inventory.check())

    def test_any_other_difference_still_blocks(self):
        path,digest=self.write('completed-helper.py')
        self.write('FAKE-vendor-binary')
        with self.assertRaisesRegex(safety.Blocked,'new or changed binary'):self.inventory.admit(path,digest)

    def test_different_content_or_absent_helper_blocks(self):
        path,_=self.write('completed-helper.py')
        with self.assertRaisesRegex(safety.Blocked,'new or changed binary'):
            self.inventory.admit(path,hashlib.sha256(b'FAKE other').hexdigest())
        inventory=safety.Inventory([self.root])
        with self.assertRaisesRegex(safety.Blocked,'runner helper admission'):
            inventory.admit(self.root/'absent-helper.py',hashlib.sha256(b'').hexdigest())


class LiveHelperTests(unittest.TestCase):
    def test_live_turn_helper_keeps_the_private_inventory_unchanged(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-anchor-') as root:
            d=runtime.Driver('release','fp','pin',Path(root)/'evidence',initialize=False)
            project=Path(root)/'fixtures'/'l14';(project/'.opencode').mkdir(parents=True)
            d.ownership.register(Path(root)/'fixtures','vendor-private')
            d.inventory=safety.Inventory([project/'.opencode'])
            helper=d._live_helper(project,'completed')
            self.assertEqual(helper,project/'.opencode'/'completed-helper.py')
            self.assertTrue(d.inventory.check())


class BootstrapProviderTests(unittest.TestCase):
    """Each vendor acquisition's bootstrap has its own provider (64-connection ceiling)."""
    def driver(self,folder):
        d=runtime.Driver('release','fp','pin',Path(folder)/'evidence')
        d.namespace=Path(folder)/'namespace';d.namespace.mkdir()
        d.namespace_project=Path(folder)/'project';d.namespace_project.mkdir()
        d._bootstrap_static=mock.Mock(return_value={});d._verify=mock.Mock()
        d._http=mock.Mock();d._observe_vendor=mock.Mock();d._host_record=mock.Mock(return_value={'owned':True})
        d._vendor_sid=mock.Mock(return_value='ses_FAKE');d.spending_check=mock.Mock()
        d._write_fixture_config=mock.Mock()
        d._validate_provider_config=lambda config:{'oclive-mock':config['endpoint']}
        self.used=[]
        def materialize(name,project,description):
            provider=mock.Mock(requests=0,admission_blocked=False,model_matches=True,endpoint='FAKE-'+name)
            d.mock_providers[name]=provider
            return {'endpoint':provider.endpoint}
        d._materialize_fixture=mock.Mock(side_effect=materialize)
        def via(args):
            if args[0]=='spawn':return {'session_id':'s_FAKE','turn':'s_FAKE/1'}
            if args[0]=='wait':
                provider=next(row for row in d.mock_providers.values()
                              if row.endpoint==d._bootstrap_config['endpoint'])
                provider.requests+=2;self.used.append(provider.endpoint)
                return {'state':'completed'}
            return {}
        d.via=mock.Mock(side_effect=via)
        return d

    def sentinel(self,d):
        provider=mock.Mock(requests=0,admission_blocked=False,model_matches=True,endpoint='FAKE-sentinel')
        d.mock_providers['namespace-boundary']=provider
        config={'endpoint':provider.endpoint}
        d.fixtures['namespace-boundary']={'path':d.namespace_project,'config':config}
        return config,provider

    def test_bootstraps_after_the_namespace_sentinel_use_fresh_providers(self):
        with tempfile.TemporaryDirectory() as folder:
            d=self.driver(folder);self.sentinel(d)
            for _ in range(3):d._bootstrap_vendor();d._bootstrap_session=None
            self.assertEqual(self.used,['FAKE-bootstrap-1','FAKE-bootstrap-2','FAKE-bootstrap-3'])
            # The registered namespace configuration follows what is on disk.
            self.assertEqual(d.fixtures['namespace-boundary']['config'],{'endpoint':'FAKE-bootstrap-3'})

    def test_sentinel_prepared_provider_serves_only_its_own_acquisition(self):
        with tempfile.TemporaryDirectory() as folder:
            d=self.driver(folder);config,provider=self.sentinel(d)
            d._namespace_bootstraps[d.namespace_project]=(config,provider)
            for _ in range(2):d._bootstrap_vendor();d._bootstrap_session=None
            self.assertEqual(self.used,['FAKE-sentinel','FAKE-bootstrap-2'])

    def test_new_acquisition_config_is_not_a_reload_of_the_retired_generation(self):
        with tempfile.TemporaryDirectory() as folder:
            d=self.driver(folder);del d._write_fixture_config
            d._validate_provider_config=lambda config:{'oclive-mock':'http://127.0.0.1:%d/v1'%config['port']}
            def materialize(name,project,description):
                port=40000+len(d.mock_providers)
                provider=mock.Mock(requests=0,admission_blocked=False,model_matches=True,
                                   endpoint='http://127.0.0.1:%d/v1'%port)
                d.mock_providers[name]=provider
                return {'endpoint':provider.endpoint,'port':port}
            d._materialize_fixture=mock.Mock(side_effect=materialize)
            # The retired generation's identities are still recorded, with its served map.
            d.daemon=safety.Identity(70,1);d.anchor=safety.Identity(71,2);d.vendor_identity=safety.Identity(72,3)
            d._verified_endpoint_maps[d.namespace_project]={'identity':d._config_identity(),'endpoints':{}}
            def retired():
                raise safety.Blocked('owned process is not verified alive')
            d._owned_observation_guard=mock.Mock(return_value=retired)
            d._bootstrap_vendor()
            self.assertEqual(self.used,['http://127.0.0.1:40000/v1'])


class CleanupAfterStopBlockTests(unittest.TestCase):
    def test_block_after_daemon_stop_request_keeps_the_absence_proof(self):
        import opencode_daemon_tests as daemontests
        with tempfile.TemporaryDirectory(prefix='via-oc-daemon-') as root:
            d,_=daemontests.DaemonGenerationTests().fixture(root)
            d.proc.alive.return_value=True;d._lock_free=mock.Mock(return_value=True)
            d._pgrep_clear=mock.Mock(return_value=True)
            def via(args):
                if args==['daemon','stop','--force']:
                    d.proc.alive.return_value=False  # The stop request reached the daemon.
                    raise safety.Blocked('new or changed binary appeared in private roots')
                return {}
            d.via=mock.Mock(side_effect=via)
            with self.assertRaisesRegex(safety.Blocked,'new or changed binary'):d.stop()
            self.assertEqual(d.stop_result,{'complete':True,'proven':True,'processes_gone':True,
                                            'locks_free':True,'pgrep_clear':True})


if __name__ == '__main__':
    unittest.main()
