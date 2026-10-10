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
