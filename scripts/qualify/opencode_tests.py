"""Offline entry/acquisition/verdict regressions for OpenCode packet §13."""

import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import opencode
from opencode_safety import Blocked


class EntryTests(unittest.TestCase):
    def test_plain_invocation_cannot_start_live(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            opencode.arguments([])

    def test_self_test_rejects_live_or_acquisition(self):
        for flag in ("--live", "--acquire"):
            with self.subTest(flag=flag), contextlib.redirect_stderr(io.StringIO()), \
                    self.assertRaises(SystemExit):
                opencode.arguments(["--self-test", flag])

    def test_offline_arguments_need_no_binary(self):
        self.assertTrue(opencode.arguments(["--self-test"]).self_test)

    def test_phase_selection_always_starts_with_preflight(self):
        self.assertEqual(opencode.selected_phases(["anchor"]), ["preflight", "anchor"])

    def test_partial_coverage_never_qualifies(self):
        record = {"case": "preflight", "disposition": "gate", "result": "pass"}
        verdict = opencode.summary_verdict([record], ["preflight"], {"stopped": True}, False)
        self.assertEqual(verdict["result"], "not_passed")

    def test_uncertain_stop_or_signal_never_passes(self):
        phases = opencode.selected_phases(None)
        records = [{"case": name, "disposition": "gate", "result": "pass"}
                   for phase in phases for name in opencode.cases.PHASE_CASES[phase]]
        for cleanup, interrupted in (({}, False), ({"stopped": None}, False),
                                     ({"stopped": True}, True)):
            self.assertEqual(opencode.summary_verdict(records, phases, cleanup, interrupted)
                             ["result"], "not_passed")
        self.assertEqual(opencode.summary_verdict(records, phases, {"stopped": True}, False)
                         ["result"], "pass")

    def test_missing_or_duplicate_case_does_not_pass(self):
        phases = opencode.selected_phases(None)
        records = [{"case": name, "disposition": "gate", "result": "pass"}
                   for phase in phases for name in opencode.cases.PHASE_CASES[phase]]
        for incomplete in (records[:-1], records + [records[0]]):
            self.assertEqual(opencode.summary_verdict(incomplete, phases, {"stopped": True}, False)
                             ["result"], "not_passed")

    def test_record_only_absence_is_not_a_gate_pass(self):
        records = [{"case": "collision", "disposition": "record-only", "result": "not_observable"}]
        self.assertEqual(opencode.summary_verdict(records, ["seams"], {"stopped": True}, False)
                         ["gate_passed"], 0)

    def test_private_acquisition_environment_restored_after_error(self):
        before = dict(os.environ)
        with self.assertRaises(Blocked):
            with opencode.acquisition_home(Path("scratchpad/private-fixture-home")):
                self.assertEqual(set(os.environ), {"HOME", "PATH", "LANG"})
                raise Blocked("fixture")
        self.assertTrue(dict(os.environ) == before, "caller environment was restored")

    def test_manifest_identity_integrity_and_size_required(self):
        for manifest in ({}, {"name": "@opencode/cli-linux-x64", "version": "2.0.24"},
                         {"name": "@opencode/cli-linux-x64", "version": "2.0.22", "dist": {}}):
            with patch.object(opencode, "official_get", return_value=json.dumps(manifest).encode()), \
                    self.assertRaises(Blocked):
                opencode.package_manifest("@opencode/cli-linux-x64")

    def test_acquisition_attempts_same_version_fallbacks_then_blocks(self):
        with tempfile.TemporaryDirectory() as root:
            calls = []
            def unavailable(package):
                calls.append(package)
                raise Blocked("fixture package unavailable")
            with patch.object(opencode, "package_manifest", side_effect=unavailable), \
                    patch.object(opencode, "acquire_release", side_effect=Blocked("fixture")) as release, \
                    self.assertRaises(Blocked):
                opencode.acquire(Path(root))
            self.assertEqual(calls, ["@opencode/cli-linux-x64", "@opencode/cli-linux-x64-baseline"])
            release.assert_called_once()

    def test_release_tag_and_asset_required_before_download(self):
        invalid = ({"tag_name": "v2.0.24", "draft": False, "assets": []},
                   {"tag_name": "v2.0.22", "draft": True, "assets": []},
                   {"tag_name": "v2.0.22", "draft": False, "assets": []})
        for release in invalid:
            with patch.object(opencode, "official_get", return_value=json.dumps(release).encode()) as get, \
                    self.assertRaises(Blocked):
                opencode.acquire_release(Path("unused-fixture"))
            self.assertEqual(get.call_count, 1)

    def test_acquisition_requires_private_context_before_any_network(self):
        with self.assertRaises(Blocked):
            opencode.official_get("https://registry.npmjs.org/fixture", allowed_hosts={"registry.npmjs.org"})

    def test_fake_gate_proof_binds_both_builds_source_tests_and_success(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "crates").mkdir()
            (root / "third_party").mkdir()
            (root / "scripts").mkdir()
            for name in ("Cargo.toml", "Cargo.lock", "scripts/check-release-features.py"):
                (root / name).write_text("fixture build input")
            release, failpoints = root / "release", root / "failpoints"
            release.write_bytes(b"release"); failpoints.write_bytes(b"failpoints")
            release.chmod(0o500); failpoints.chmod(0o500)
            (root / "scratchpad").mkdir(mode=0o700)
            manifest = root / "scratchpad" / "manifest.json"
            valid = {"version": 1, "result": "pass",
                     "rust_source_sha256": opencode.rust_source_hash(root),
                     "via_hashes": {"release": opencode.safety.sha256(release),
                                    "test-failpoints": opencode.safety.sha256(failpoints)},
                     "tests": list(opencode.FAKE_GATE_TESTS),
                     "gates": {name: 0 for name in opencode.FAKE_GATES}}
            for change in ({"version": True}, {"result": "unknown"},
                           {"rust_source_sha256": "0" * 64}, {"via_hashes": {}},
                           {"tests": []}, {"gates": {}},
                           {"gates": {name: False for name in opencode.FAKE_GATES}}):
                with self.subTest(change=tuple(change)):
                    manifest.write_text(json.dumps({**valid, **change}))
                    with self.assertRaises(Blocked):
                        opencode.verify_fake_gates(manifest, release, failpoints, root)
            manifest.write_text(json.dumps(valid))
            proof = opencode.verify_fake_gates(manifest, release, failpoints, root)
            self.assertIs(proof["verified"], True)
            failpoints.chmod(0o700)
            failpoints.write_bytes(b"changed build")
            failpoints.chmod(0o500)
            with self.assertRaises(Blocked):
                opencode.verify_fake_gates(manifest, release, failpoints, root)

    def test_manifest_and_program_overrides_cannot_read_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scratchpad").mkdir(mode=0o700)
            executable = root / "via"
            executable.write_bytes(b"fixture"); executable.chmod(0o500)
            credential = root / "scratchpad" / "auth.json"
            credential.write_bytes(b"synthetic private fixture"); credential.chmod(0o500)
            with patch.object(opencode.safety, "sha256") as hash_file, \
                    self.assertRaises(Blocked):
                opencode.verify_fake_gates(root / "outside.json", executable, executable, root)
            hash_file.assert_not_called()
            with patch.object(opencode.safety, "sha256") as hash_file, \
                    self.assertRaises(Blocked):
                opencode.verify_fake_gates(root / "scratchpad" / "manifest.json",
                                          credential, executable, root)
            hash_file.assert_not_called()

    def test_missing_manifest_blocks_before_acquisition_or_driver_start(self):
        with tempfile.TemporaryDirectory(prefix="oc-entry-", dir=opencode.SCRATCHPAD) as directory:
            root = Path(directory)
            args = SimpleNamespace(self_test=False, phase=["preflight"], evidence=root / "run",
                                   acquire=True, opencode=None, via_release=root / "release",
                                   via_failpoints=root / "failpoints", fake_gate_manifest=root / "missing")
            with patch.object(opencode, "arguments", return_value=args), \
                    patch.object(opencode, "self_test", return_value=([], 1)), \
                    patch.object(opencode, "acquire") as acquire, \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(opencode.main([]), 1)
            acquire.assert_not_called()
            summary = json.loads((args.evidence / "summary.json").read_text())
            self.assertEqual(summary["runner_error"], "Blocked")
            self.assertFalse(summary["daemons_stopped"])

    def test_entry_fake_phase_always_cleans_up_and_writes_sanitized_summary(self):
        import opencode_driver
        with tempfile.TemporaryDirectory(prefix="oc-entry-", dir=opencode.SCRATCHPAD) as directory:
            root = Path(directory)
            calls = []
            class FakeDriver:
                def __init__(self, **_arguments):
                    self.vault = opencode.safety.PasswordVault()
                    self.proc = opencode.safety.ProcReader(root / "synthetic-proc")
                def prepare(self):
                    calls.append("prepare")
                def observe(self, kind):
                    self_kind = kind
                    if self_kind != "build_hashes":
                        raise Blocked("fixture observation unavailable")
                    return {"release": "1" * 64, "test-failpoints": "2" * 64}
                def finish(self):
                    calls.append("finish")
                    return {"stopped": True}
            args = SimpleNamespace(self_test=False, phase=["preflight"], evidence=root / "run",
                                   acquire=False, opencode=root / "fixture-pin",
                                   via_release=root / "release", via_failpoints=root / "failpoints",
                                   fake_gate_manifest=root / "fake-gates")
            record = {"case": "preflight", "disposition": "gate", "result": "pass", "checks": []}
            with patch.object(opencode, "arguments", return_value=args), \
                    patch.object(opencode, "self_test", return_value=([], 1)), \
                    patch.object(opencode, "verify_fake_gates", return_value={"verified": True}), \
                    patch.object(opencode, "RECOVERY_ROOT", root / "recovery"), \
                    patch.object(opencode.safety, "verify_binary"), \
                    patch.object(opencode_driver, "Driver", FakeDriver), \
                    patch.object(opencode.cases, "run_phase", return_value=[record]), \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(opencode.main([]), 1)
            self.assertEqual(calls, ["prepare", "finish"])
            summary = json.loads((args.evidence / "summary.json").read_text())
            self.assertEqual(summary["result"], "not_passed")
            self.assertTrue(summary["daemons_stopped"])

    def test_rejected_password_record_never_reaches_final_summary(self):
        import opencode_driver
        with tempfile.TemporaryDirectory(prefix="oc-entry-", dir=opencode.SCRATCHPAD) as directory:
            root = Path(directory)
            password = b"entry-synthetic-memory-password"
            class FakeDriver:
                def __init__(self, **_arguments):
                    self.vault = opencode.safety.PasswordVault()
                    self.proc = opencode.safety.ProcReader(root / "synthetic-proc")
                    class Proc:
                        def read(self, *_args):
                            return b"OPENCODE_PASSWORD=" + password + b"\0"
                    self.vault.read_once(Proc(), opencode.safety.Identity(44, 7))
                def prepare(self): pass
                def observe(self, _kind): return {}
                def finish(self):
                    # Reproduce the original component's premature clear. The
                    # entry must also exclude a rejected record before cleanup.
                    self.vault.clear()
                    return {"stopped": True}
            args = SimpleNamespace(self_test=False, phase=["preflight"], evidence=root / "run",
                                   acquire=False, opencode=root / "fixture-pin", via_release=root / "release",
                                   via_failpoints=root / "failpoints", fake_gate_manifest=root / "fake-gates")
            record = {"case": "preflight", "disposition": "gate", "result": "pass",
                      "checks": [{"observation": password.decode()}]}
            with patch.object(opencode, "arguments", return_value=args), \
                    patch.object(opencode, "self_test", return_value=([], 1)), \
                    patch.object(opencode, "verify_fake_gates", return_value={"verified": True}), \
                    patch.object(opencode, "RECOVERY_ROOT", root / "recovery"), \
                    patch.object(opencode.safety, "verify_binary"), \
                    patch.object(opencode_driver, "Driver", FakeDriver), \
                    patch.object(opencode.cases, "run_phase", return_value=[record]), \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(opencode.main([]), 1)
            for artifact in args.evidence.glob("*.json"):
                self.assertNotIn(password, artifact.read_bytes())

    def test_final_password_sink_rejects_before_guaranteed_memory_clear(self):
        import opencode_driver
        with tempfile.TemporaryDirectory(prefix="oc-entry-", dir=opencode.SCRATCHPAD) as directory:
            root = Path(directory)
            password, instances = b"final-synthetic-memory-password", []
            class FakeDriver:
                def __init__(self, **_arguments):
                    instances.append(self)
                    self.vault = opencode.safety.PasswordVault()
                    self.proc = opencode.safety.ProcReader(root / "synthetic-proc")
                    class Proc:
                        def read(self, *_args): return b"OPENCODE_PASSWORD=" + password + b"\0"
                    self.vault.read_once(Proc(), opencode.safety.Identity(44, 7))
                def prepare(self): pass
                def observe(self, _kind): return {}
                def finish(self): return {"stopped": True, "unsafe": password.decode()}
                def clear_sensitive(self): self.vault.clear()
            args = SimpleNamespace(self_test=False, phase=["preflight"], evidence=root / "run",
                                   acquire=False, opencode=root / "fixture-pin", via_release=root / "release",
                                   via_failpoints=root / "failpoints", fake_gate_manifest=root / "fake-gates")
            record = {"case": "preflight", "disposition": "gate", "result": "pass", "checks": []}
            output = io.StringIO()
            with patch.object(opencode, "arguments", return_value=args), \
                    patch.object(opencode, "self_test", return_value=([], 1)), \
                    patch.object(opencode, "verify_fake_gates", return_value={"verified": True}), \
                    patch.object(opencode, "RECOVERY_ROOT", root / "recovery"), \
                    patch.object(opencode.safety, "verify_binary"), \
                    patch.object(opencode_driver, "Driver", FakeDriver), \
                    patch.object(opencode.cases, "run_phase", return_value=[record]), \
                    contextlib.redirect_stdout(output):
                self.assertEqual(opencode.main([]), 1)
            self.assertEqual(json.loads(output.getvalue())["result"], "blocked")
            self.assertFalse((args.evidence / "summary.json").exists())
            self.assertFalse(instances[0].vault.leaks(password))
            for artifact in args.evidence.glob("*.json"):
                self.assertNotIn(password, artifact.read_bytes())

    def test_changed_final_gate_evidence_blocks_otherwise_complete_run(self):
        import opencode_driver
        with tempfile.TemporaryDirectory(prefix="oc-entry-", dir=opencode.SCRATCHPAD) as directory:
            root = Path(directory)
            class FakeDriver:
                def __init__(self, **_arguments):
                    self.vault = opencode.safety.PasswordVault()
                    self.proc = opencode.safety.ProcReader(root / "synthetic-proc")
                def prepare(self): pass
                def observe(self, _kind): return {}
                def finish(self): return {"stopped": True}
                def clear_sensitive(self): self.vault.clear()
            args = SimpleNamespace(self_test=False, phase=None, evidence=root / "run",
                                   acquire=False, opencode=root / "fixture-pin",
                                   via_release=root / "release", via_failpoints=root / "failpoints",
                                   fake_gate_manifest=root / "fake-gates")
            def records(_adapter, phase):
                return [{"case": name, "disposition": "gate", "result": "pass", "checks": []}
                        for name in opencode.cases.PHASE_CASES[phase]]
            with patch.object(opencode, "arguments", return_value=args), \
                    patch.object(opencode, "self_test", return_value=([], 1)), \
                    patch.object(opencode, "verify_fake_gates", side_effect=[
                        {"verified": True}, Blocked("build changed")]) as proof, \
                    patch.object(opencode, "RECOVERY_ROOT", root / "recovery"), \
                    patch.object(opencode.safety, "verify_binary"), \
                    patch.object(opencode_driver, "Driver", FakeDriver), \
                    patch.object(opencode.cases, "run_phase", side_effect=records), \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(opencode.main([]), 1)
            self.assertEqual(proof.call_count, 2)
            summary = json.loads((args.evidence / "summary.json").read_bytes())
            self.assertEqual(summary["result"], "blocked")
            self.assertEqual(summary["proof_error"], "Blocked")
            self.assertTrue(summary["coverage_complete"])


if __name__ == "__main__":
    unittest.main()
