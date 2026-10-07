#!/usr/bin/env python3
"""Fake/mocked qualification safety checks; vendors/opencode.md §13, OC01/OC12.

No vendor, VIA daemon, model, package download or caller credential file is used.
"""
import base64
import contextlib
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import tarfile
import tempfile
import threading
import time
import types
import http.client
import urllib.parse
import unittest
import urllib.request
from unittest import mock

import opencode_safety as safety


def fake_stat(pid=71, ticks=123, state="S"):
    # Linux stat fields 3..22; starttime is index19 after comm.
    fields = [state, "1", "1", "1", *(["0"] * 15), str(ticks)]
    return f"{pid} (fake vendor) ".encode() + " ".join(fields).encode()


class SafetyTests(unittest.TestCase):
    def test_owned_metadata_requests_share_the_bootstrap_absolute_deadline(self):
        client=safety.OwnedHTTP('http://127.0.0.1:1234',self.identity,self.proc,b'synthetic-password')
        limit=time.monotonic()+1
        client.deadline=limit
        with mock.patch.object(safety,'_bounded_response',return_value=(200,None,b'{}')) as transport:
            client.request('GET','/api/info')
            self.assertLessEqual(transport.call_args.kwargs['deadline'],limit)
        client.deadline=time.monotonic()-1
        with mock.patch.object(safety,'_bounded_response') as transport:
            self.assertBlocked(lambda:client.request('GET','/api/info'),'owned API absolute deadline exhausted')
            transport.assert_not_called()

    def test_no_listener_is_retryable_but_multiple_owned_listeners_block(self):
        fd=self.root/'proc/71/fd'; fd.mkdir()
        net=self.root/'proc/71/net'; net.mkdir()
        tcp=net/'tcp'; tcp.write_text('header\n')
        with self.assertRaises(safety.Blocked) as absent:
            self.proc.listener(self.identity)
        self.assertIsInstance(absent.exception,getattr(safety,'ListenerNotReady',()))
        for inode,port in ((999,'3039'),(998,'303A')):
            (fd/str(inode)).symlink_to('socket:['+str(inode)+']')
            with tcp.open('a') as stream:
                stream.write('0: 0100007F:'+port+' 00000000:0000 0A 0:0 00:0 0 1000 0 '+str(inode)+'\n')
        with self.assertRaises(safety.Blocked) as ambiguous:
            self.proc.listener(self.identity)
        self.assertNotIsInstance(ambiguous.exception,getattr(safety,'ListenerNotReady',()))
        self.assertEqual(str(ambiguous.exception),'owned listener absent or ambiguous')

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="via-oc-safety-")
        self.root = Path(self.temp.name)
        self.identity = safety.Identity(71, 123)
        proc = self.root / "proc" / "71"
        proc.mkdir(parents=True)
        (proc / "stat").write_bytes(fake_stat())
        self.proc = safety.ProcReader(self.root / "proc")

    def tearDown(self):
        for folder, dirs, _files in os.walk(self.root):
            os.chmod(folder, 0o700)
            for name in dirs:
                target = Path(folder) / name
                if not target.is_symlink():
                    target.chmod(0o700)
        self.temp.cleanup()

    def assertBlocked(self, callback, reason):
        with self.assertRaises(safety.Blocked) as blocked:
            callback()
        self.assertEqual(str(blocked.exception), reason)

    def binary(self):
        directory = self.root / "pin"
        directory.mkdir(mode=0o700)
        binary = directory / "opencode"
        binary.write_bytes(b"synthetic pinned binary")
        binary.chmod(0o500)
        directory.chmod(0o500)
        return binary, hashlib.sha256(binary.read_bytes()).hexdigest()

    def spending(self, **changes):
        args = {"integration": {"data": [{"id": "opencode", "connections": []}]},
                "environment_names": safety.VENDOR_ENVIRONMENT,
                "catalog": {safety.MOCK_IDENTITY: {"free": True}},
                "provider": "oclive-mock", "model": "fixture-free",
                "project_providers": {"oclive-mock": "http://127.0.0.1:1234"},
                "cost": None}
        args.update(changes)
        guard = safety.SpendingGuard(owned_mock_origins={"http://127.0.0.1:1234"})
        return guard, args

    def vault(self, password=b"synthetic-password-unique-71"):
        (self.root / "proc" / "71" / "environ").write_bytes(
            b"UNRELATED=never-retained\0OPENCODE_PASSWORD=" + password + b"\0")
        vault = safety.PasswordVault()
        vault.read_once(self.proc, self.identity)
        return vault

    def test_pinned_hash_mismatch_stops(self):
        binary, _sha = self.binary()
        self.assertBlocked(lambda: safety.verify_binary(binary, "0" * 64), 'pinned binary hash mismatch')

    def test_pinned_hash_and_readonly_accept(self):
        binary, sha = self.binary()
        self.assertEqual(safety.verify_binary(binary, sha)["sha256"], sha)

    def test_writable_binary_stops(self):
        binary, sha = self.binary()
        binary.chmod(0o700)
        self.assertBlocked(lambda: safety.verify_binary(binary, sha), 'pinned binary size or read-only mode mismatch')

    def test_program_path_mismatch_stops_before_start(self):
        binary, sha = self.binary()
        self.assertBlocked(lambda: safety.verify_daemon_program(
            {"harnesses": {"opencode": {"binary": "/usr/bin/opencode"}}}, binary, sha), 'daemon program does not resolve to pinned binary')
        self.assertBlocked(lambda: safety.verify_daemon_program({}, binary, sha), 'explicit pinned OpenCode program is required')
        self.assertEqual(safety.verify_daemon_program(
            {"harnesses": {"opencode": {"binary": str(binary)}}}, binary, sha)["sha256"], sha)

    def test_positive_cost_stops(self):
        guard, args = self.spending(cost=0.001)
        self.assertBlocked(lambda: guard.check(**args), 'positive cost hard stop')
        args["cost"] = 0
        self.assertBlocked(lambda: guard.check(**args), 'spending control previously failed')

    def test_paid_identity_stops_with_zero_cost(self):
        guard, args = self.spending(provider="paid", model="premium", cost=0)
        self.assertBlocked(lambda: guard.check(**args), 'paid or unapproved model identity')

    def test_missing_cost_remains_unavailable_and_continues(self):
        guard, args = self.spending()
        self.assertEqual(guard.check(**args)["cost"], None)
        self.assertEqual(guard.check(**args)["cost_available"], False)
        args["cost"] = 0
        self.assertEqual(guard.check(**args)["cost"], 0)

    def test_invalid_cost_stops(self):
        for cost in (True, float("nan"), float("inf"), -1, "0"):
            guard, args = self.spending(cost=cost)
            self.assertBlocked(lambda: guard.check(**args), 'cost evidence invalid')

    def test_each_structural_control_stops(self):
        alterations = ({"integration": {}}, {"integration": {"data": [
            {"id": "x", "connections": [{"type": "credential"}]}]}},
            {"environment_names": safety.VENDOR_ENVIRONMENT | {"HTTP_PROXY"}},
            {"catalog": {safety.MOCK_IDENTITY: {"free": False}}},
            {"project_providers": {"oclive-mock": "https://provider.example.invalid"}},
            {"project_providers": {}}, {"mock_received": False})
        reasons = ("integration credential-free shape unverifiable",
                   "integration is not proven credential-free", "environment allow-list failed",
                   "frozen free model absent from catalog", "endpoint is not unauthenticated IPv4 loopback",
                   "fixture provider endpoint absent", "provider override did not reach mock")
        for alteration, reason in zip(alterations, reasons, strict=True):
            guard, args = self.spending(**alteration)
            self.assertBlocked(lambda: guard.check(**args), reason)

    def test_unsafe_path_stops(self):
        helper = self.root / "helpers"
        helper.mkdir()
        cases = (("/usr/bin::" + str(helper), "unsafe PATH entry"),
                 ("relative:" + str(helper), "unsafe PATH entry"),
                 ("/owner/bin:" + str(helper), "unsafe PATH outside system/helper directories"),
                 ("/usr/bin", "private helper directory absent from PATH"))
        for value, reason in cases:
            self.assertBlocked(lambda: safety.validate_path(value, helper), reason)
        self.assertTrue(safety.validate_path(str(helper) + ":/usr/bin:/bin", helper))

    def test_exact_system_rg_link(self):
        system = self.root / "system"
        system.mkdir()
        rg = system / "rg"
        rg.write_bytes(b"#!/bin/sh\nexit 0\n")
        rg.chmod(0o700)
        helper = self.root / "helper"
        path, proof = safety.build_minimal_path(helper, rg, system_dirs=(str(system),))
        self.assertEqual((helper / "rg").resolve(), rg)
        self.assertTrue(proof["rg_system_directory"])
        self.assertTrue(safety.validate_path(path, helper, system_dirs=(str(system),)))
        self.assertBlocked(lambda: safety.build_minimal_path(
            self.root / "bad-helper", rg, system_dirs=("/usr/bin",)), 'known rg is not a system executable')

    def test_package_appearance_in_every_inventory_root_stops(self):
        roots = [self.root / name for name in ("home", "xdg", "namespace", "fixture.opencode")]
        for root in roots:
            root.mkdir()
        for root in roots:
            for name in ("node_modules", "package.json"):
                inventory = safety.Inventory(roots)
                target = root / name
                target.mkdir() if name == "node_modules" else target.write_text("{}")
                self.assertBlocked(inventory.check, 'package artifact appeared in private roots')
                target.rmdir() if target.is_dir() else target.unlink()

    def test_new_binary_and_binary_magic_stops(self):
        root = self.root / "home"
        root.mkdir()
        for mode, content in ((0o700, b"#!/bin/sh\n"), (0o600, b"\x7fELFsynthetic")):
            inventory = safety.Inventory([root])
            artifact = root / "downloaded"
            artifact.write_bytes(content)
            artifact.chmod(mode)
            self.assertBlocked(inventory.check, 'new or changed binary appeared in private roots')
            artifact.unlink()

    def test_namespace_chain_is_0700_and_digest_exact(self):
        root, env = safety.create_namespace(self.root / "state")
        expected = hashlib.sha256()
        for field in (b"via-opencode-namespace-v1", b"opencode-free-anonymous-v1",
                      b"\x01\x00\x00\x00\x00\x00\x00\x00"):
            expected.update(len(field).to_bytes(8, "little") + field)
        self.assertEqual(root.name, expected.hexdigest()[:16])
        for path in (root.parent.parent, root.parent, root, *(Path(p) for p in env.values())):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)

    def test_namespace_unsafe_existing_chain_stops(self):
        state = self.root / "state"
        state.mkdir(mode=0o700)
        (state / "vendor").mkdir(mode=0o755)
        self.assertBlocked(lambda: safety.create_namespace(state), 'private directory chain is unsafe')

    def test_password_read_once_and_never_written(self):
        password = b"synthetic-password-unique-71"
        vault = self.vault(password)
        with mock.patch.object(self.proc, "read", side_effect=AssertionError("second read")):
            self.assertEqual(vault.read_once(self.proc, self.identity), password)
        destination = self.root / "evidence.json"
        for secret in (password, base64.b64encode(password),
                       base64.b64encode(b"opencode:" + password), password.hex().encode()):
            self.assertBlocked(lambda: vault.safe_write_json(destination,
                                                             {"unsafe": secret.decode()}), 'secret present in evidence; evidence not written')
            self.assertFalse(destination.exists())
        vault.safe_write_json(destination, {"password_absent": True})
        self.assertFalse(vault.leaks(destination.read_bytes()))
        self.assertNotIn(password.decode(), repr(vault))
        self.assertEqual(vault.names(self.identity), {"UNRELATED", "OPENCODE_PASSWORD"})

    def test_password_rotation_boolean_and_duplicate_stops(self):
        vault = self.vault()
        other = self.root / "proc" / "72"
        other.mkdir()
        (other / "stat").write_bytes(fake_stat(72, 124))
        (other / "environ").write_bytes(b"OPENCODE_PASSWORD=distinct-synthetic\0")
        identity = safety.Identity(72, 124)
        vault.read_once(self.proc, identity)
        self.assertIs(vault.changed(self.identity, identity), True)
        self.assertBlocked(lambda: vault.changed(self.identity, self.identity), 'password did not rotate')
        (other / "environ").write_bytes(b"OPENCODE_PASSWORD=a\0OPENCODE_PASSWORD=b\0")
        self.assertBlocked(lambda: safety.PasswordVault().read_once(self.proc, identity), 'environment key duplicated')

    def test_truncated_environ_and_reused_pid_stop(self):
        (self.root / "proc" / "71" / "environ").write_bytes(b"OPENCODE_PASSWORD=not-terminated")
        self.assertBlocked(lambda: safety.PasswordVault().read_once(self.proc, self.identity), 'truncated password observation')
        (self.root / "proc" / "71" / "stat").write_bytes(fake_stat(ticks=999))
        self.assertBlocked(lambda: self.proc.read(self.identity, "environ"), 'owned process identity changed')
        self.assertFalse(self.proc.alive(self.identity))

    def test_unverifiable_evidence_never_passes(self):
        for value in (None, False, 1, "true", [], {}):
            self.assertBlocked(lambda: safety.require_proof(value, "missing evidence"), 'missing evidence')
        self.assertBlocked(lambda: safety.stop_proof(process_states=[None], locks_free=[True, True],
                                                    pgrep_clear=True), 'owned process stop unverified')
        self.assertBlocked(lambda: safety.stop_proof(process_states=[False], locks_free=[True, None],
                                                    pgrep_clear=True), 'private daemon/store lock absence unverified')
        self.assertBlocked(lambda: safety.stop_proof(process_states=[False], locks_free=[True, True],
                                                    pgrep_clear=None), 'targeted pgrep absence unverified')
        self.assertTrue(safety.stop_proof(process_states=[False], locks_free=[True, True],
                                         pgrep_clear=True))

    def test_proc_unreadable_is_not_gone(self):
        (self.root / "proc" / "71" / "stat").write_bytes(b"corrupt")
        self.assertIsNone(self.proc.alive(self.identity))
        self.assertFalse(self.proc.gone(self.identity))

    def test_stop_journal_recovery_only_verified_identity(self):
        journal = self.root / "stopped.json"
        sent = []
        def sender(pid, signum):
            sent.append((pid, signum))
            (self.root / "proc" / "71" / "stat").write_bytes(
                fake_stat(state="T" if signum == signal.SIGSTOP else "S"))
        keeper = safety.StopJournal(journal, self.proc, sender=sender)
        keeper.stop(self.identity)
        self.assertTrue(journal.exists())
        self.assertEqual(json.loads(journal.read_text())["start_ticks"], 123)
        self.assertTrue(keeper.recover())
        self.assertFalse(journal.exists())
        self.assertEqual(sent, [(71, signal.SIGSTOP), (71, signal.SIGCONT)])
        keeper.stop(self.identity)
        (self.root / "proc" / "71" / "stat").write_bytes(fake_stat(ticks=999))
        self.assertTrue(keeper.recover())
        self.assertFalse(journal.exists())
        self.assertEqual(keeper.recovery_fact["proof"], "pid_reused")
        self.assertEqual(len(sent), 3)

    def test_deferred_signal_blocks_admission(self):
        signals = safety.DeferredSignals()
        signals.record(signal.SIGTERM)
        signals.record(signal.SIGINT)
        self.assertEqual(signals.interrupted, "SIGTERM")
        self.assertBlocked(signals.guard, 'qualification interrupted')

    def test_archive_integrity_and_unsafe_member_stop(self):
        self.assertBlocked(lambda: safety.extract_pinned_npm(b"not tar", self.root / "pin"), 'npm archive integrity mismatch')
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode="w:gz") as archive:
            entry = tarfile.TarInfo("../opencode")
            entry.size = 1
            archive.addfile(entry, io.BytesIO(b"x"))
        data = raw.getvalue()
        integrity = base64.b64encode(hashlib.sha512(data).digest()).decode()
        with mock.patch.object(safety, "NPM_SHA512", integrity), \
                mock.patch.object(safety, "NPM_UNPACKED_SIZE", 1):
            self.assertBlocked(lambda: safety.extract_pinned_npm(data, self.root / "pin",
                                                                 integrity=integrity, unpacked_size=1),
                               "unsafe npm archive member")

    def test_archive_verified_binary_readonly_extraction(self):
        binary = b"synthetic executable"
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode="w:gz") as archive:
            entry = tarfile.TarInfo("package/bin/opencode")
            entry.size = len(binary)
            archive.addfile(entry, io.BytesIO(binary))
        data = raw.getvalue()
        integrity = base64.b64encode(hashlib.sha512(data).digest()).decode()
        sha = hashlib.sha256(binary).hexdigest()
        with mock.patch.object(safety, "NPM_SHA512", integrity), \
                mock.patch.object(safety, "NPM_UNPACKED_SIZE", len(binary)):
            path = safety.extract_pinned_npm(data, self.root / "pin", integrity=integrity,
                                            unpacked_size=len(binary), expected_sha=sha)
        self.assertEqual(safety.verify_binary(path, sha)["size"], len(binary))

    def test_download_no_proxy_redirect_or_unapproved_origin(self):
        self.assertBlocked(lambda: safety.official_https_get(
            "https://mirror.example.invalid/package", allowed_hosts={"registry.npmjs.org"}), 'unapproved official package origin')
        self.assertBlocked(lambda: safety._NoRedirect().redirect_request(
            None, None, 302, "redirect", {}, "https://example.invalid"), 'download redirect refused')

    def test_private_git_fixture_commit_and_ancestor_sentinels(self):
        path = self.root / "fixture"
        safety.init_fixture_repo(path, self.root / "git-home", sentinel=True)
        self.assertTrue((path / ".git" / "HEAD").exists())
        self.assertTrue(list((path / ".git" / "refs" / "heads").iterdir()))
        self.assertIn("ANCESTOR_AGENTS", (path / "AGENTS.md").read_text())
        self.assertIn("ANCESTOR_SKILL", (path / ".claude" / "skills" /
                                         "via-sentinel" / "SKILL.md").read_text())
        for directory in (".claude", ".opencode"):
            self.assertIn("ANCESTOR_", (path / directory / "agents" / "via-sentinel.md").read_text())
        self.assertBlocked(lambda: safety.init_fixture_repo(path, self.root / "git-home"), 'fixture Git repository already exists')

    def test_unknown_credential_shape_stops(self):
        guard, args = self.spending(integration={"data": [
            {"id": "x", "connections": [], "credentials": ["unexpected"]}]})
        self.assertBlocked(lambda: guard.check(**args), 'integration is not proven credential-free')

    def test_official_release_redirect_allowlist_and_cap(self):
        handler = safety._OfficialRedirect({"github.com", "release-assets.githubusercontent.com"})
        request = urllib.request.Request("https://github.com/anomalyco/opencode/releases/download/v2.0.22/asset")
        next_request = handler.redirect_request(request, None, 302, "redirect", {},
                                                "https://release-assets.githubusercontent.com/asset")
        self.assertFalse(next_request.has_header("Authorization"))
        self.assertBlocked(lambda: handler.redirect_request(request, None, 302, "redirect", {},
                                                            "https://evil.example.invalid/asset"), 'official release redirect refused')
        self.assertBlocked(lambda: handler.redirect_request(request, None, 302, "redirect", {},
                                                            "http://github.com/asset"), 'official release redirect refused')
        self.assertBlocked(lambda: handler.redirect_request(request, None, 302, "redirect", {},
                                                            "https://github.com/asset"), 'official release redirect refused')
        self.assertBlocked(lambda: safety._OfficialRedirect({"evil.example.invalid"}), 'unapproved release redirect host')

    def test_release_archive_pin_paths_and_readonly(self):
        binary = b"synthetic release executable"
        def archive_bytes(name):
            raw = io.BytesIO()
            with tarfile.open(fileobj=raw, mode="w:gz") as archive:
                entry = tarfile.TarInfo(name)
                entry.size = len(binary)
                archive.addfile(entry, io.BytesIO(binary))
            return raw.getvalue()
        traversal = archive_bytes("../opencode")
        data = archive_bytes("opencode")
        traversal_hash = base64.b64encode(hashlib.sha512(traversal).digest()).decode()
        archive_hash = base64.b64encode(hashlib.sha512(data).digest()).decode()
        self.assertBlocked(lambda: safety.extract_pinned_release(
            traversal, self.root / "badpin", archive_sha512=traversal_hash),
            "unsafe official release archive member")
        self.assertBlocked(lambda: safety.extract_pinned_release(
            data, self.root / "badpin", archive_sha512=archive_hash),
            "official release pinned binary hash mismatch")
        sha = hashlib.sha256(binary).hexdigest()
        pinned = safety.extract_pinned_release(data, self.root / "releasepin",
                                               expected_sha=sha, archive_sha512=archive_hash)
        self.assertEqual(safety.verify_binary(pinned, sha)["size"], len(binary))

    def test_seed_only_post_credential_exception(self):
        client = safety.OwnedHTTP("http://127.0.0.1:1234", self.identity, self.proc,
                                  b"synthetic-password", seeding_mode=True)
        for method, path in (("GET", "/api/credential"), ("GET", "/api/credential/x"),
                             ("DELETE", "/api/credential"), ("POST", "/api/credential/x")):
            self.assertBlocked(lambda: client.request(method, path), 'credential endpoint forbidden')
        default = safety.OwnedHTTP("http://127.0.0.1:1234", self.identity, self.proc,
                                   b"synthetic-password")
        self.assertBlocked(lambda: default.request("POST", "/api/credential"), 'credential endpoint forbidden')


    def test_inventory_credential_names_are_metadata_only(self):
        root = self.root / "credential-inventory"
        root.mkdir()
        names = ("auth.json", ".credentials.json", "credentials", ".npmrc", ".netrc",
                 "bunfig.toml", "opencode.db", "opencode.db-wal", "opencode.db-shm",
                 ".npmrc.backup", ".netrc.old", "auth.json.bak", ".bunfig.toml.bak")
        for name in names:
            (root / name).write_text("synthetic credential input, never inspected")
        original = Path.open
        def guarded(path, *args, **kwargs):
            if path.name in names:
                raise AssertionError("credential content read")
            return original(path, *args, **kwargs)
        with mock.patch.object(Path, "open", guarded):
            inventory = safety.Inventory([root])
            self.assertTrue(inventory.check())
            (root / "auth.json").chmod(0o700)
            self.assertBlocked(inventory.check, 'new or changed binary appeared in private roots')

    def test_uppercase_hex_and_url_escape_variants_never_written(self):
        password = b"private+/=synthetic-secret"
        vault = self.vault(password)
        destination = self.root / "encoded-evidence.json"
        quoted = urllib.parse.quote_from_bytes(password)
        lowered = quoted.replace("%2B", "%2b").replace("%2F", "%2f").replace("%3D", "%3d")
        for encoded in (password.hex().upper(), quoted, lowered):
            self.assertBlocked(lambda: vault.safe_write_json(destination, {"unsafe": encoded}), 'secret present in evidence; evidence not written')
            self.assertFalse(destination.exists())

    def test_all_password_encoding_forms_never_written(self):
        password = b'private"\\+/=synthetic-secret'
        vault = self.vault(password)
        destination = self.root / "all-encoded-evidence.json"
        quoted = urllib.parse.quote_from_bytes(password)
        forms = (password.decode(), base64.b64encode(password).decode(),
                 base64.b64encode(b"opencode:" + password).decode(),
                 password.hex(), password.hex().upper(), quoted,
                 quoted.replace("%2B", "%2b").replace("%5C", "%5c"),
                 json.dumps(password.decode())[1:-1])
        for encoded in forms:
            self.assertIs(vault.leaks(encoded.encode()), True)
            self.assertBlocked(lambda: vault.safe_write_json(destination, {"unsafe": encoded}), 'secret present in evidence; evidence not written')
            self.assertFalse(destination.exists())


    def deadline_probe(self, mode, acquisition):
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass
            def do_GET(self):
                try:
                    if mode == "headers":
                        self.wfile.write(b"HTTP/1.1 200 OK\r\nX-Slow: ")
                        self.wfile.flush()
                        for _index in range(15):
                            self.wfile.write(b"x")
                            self.wfile.flush()
                            time.sleep(0.04)
                        self.wfile.write(b"\r\nContent-Length: 2\r\n\r\n{}")
                    else:
                        self.send_response(200)
                        self.send_header("Content-Length", "15")
                        self.end_headers()
                        for _index in range(15):
                            self.wfile.write(b"x")
                            self.wfile.flush()
                            time.sleep(0.04)
                except (BrokenPipeError, ConnectionResetError):
                    pass
        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        server.timeout = 1
        thread = threading.Thread(target=server.handle_request)
        thread.start()
        try:
            started = time.monotonic()
            if acquisition:
                class fake_https(http.client.HTTPConnection):
                    def __init__(self, _host, _port=None, **kwargs):
                        kwargs.pop("context", None)
                        kwargs.pop("check_hostname", None)
                        super().__init__("127.0.0.1", server.server_port, **kwargs)
                with mock.patch.object(http.client, "HTTPSConnection", fake_https):
                    self.assertBlocked(lambda: safety.official_https_get(
                        "https://registry.npmjs.org/fake", allowed_hosts={"registry.npmjs.org"},
                        deadline_s=0.15), 'HTTP absolute deadline exceeded')
            else:
                client = safety.OwnedHTTP(f"http://127.0.0.1:{server.server_port}",
                                          self.identity, self.proc, b"synthetic-password")
                self.assertBlocked(lambda: client.request("GET", "/api/info", timeout=0.15), 'HTTP absolute deadline exceeded')
            self.assertLess(time.monotonic() - started, 0.40)
        finally:
            server.server_close()
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive(), "fake server helper joined")

    def test_owned_http_trickle_body_absolute_deadline(self):
        self.deadline_probe("body", False)

    def test_owned_http_trickle_headers_absolute_deadline(self):
        self.deadline_probe("headers", False)

    def test_acquisition_trickle_body_absolute_deadline(self):
        self.deadline_probe("body", True)

    def test_acquisition_trickle_headers_absolute_deadline(self):
        self.deadline_probe("headers", True)


    def supervised_fake(self, script, *, deadline=0.15, limit=100):
        original = subprocess.Popen
        launched = []
        home = self.root / "fetch-home"
        def fake_worker(args, *positional, **kwargs):
            if args[-1:] == ["--fetch-worker"]:
                self.assertEqual(set(kwargs["env"]), {"HOME", "PATH", "LANG", *[
                    key for key in safety.PRIVATE_PARTS if key != "HOME"]})
                self.assertTrue(all(str(home) in kwargs["env"][key]
                                    for key in safety.PRIVATE_PARTS))
                child = original([args[0], "-I", "-S", "-c", script], *positional, **kwargs)
                launched.append(child)
                return child
            return original(args, *positional, **kwargs)
        with mock.patch.object(subprocess, "Popen", fake_worker):
            try:
                result = safety.supervised_official_get(
                    "https://registry.npmjs.org/fake", private_home=home,
                    allowed_hosts={"registry.npmjs.org"}, deadline_s=deadline, limit=limit)
            finally:
                self.assertEqual(len(launched), 1)
                self.assertIsNotNone(launched[0].poll(), "helper stopped and reaped")
                self.assertFalse((Path("/proc") / str(launched[0].pid)).exists())
        return result

    def test_fetch_worker_invalid_request_fixed_diagnostics_only(self):
        stdout, stderr = io.BytesIO(), io.BytesIO()
        with mock.patch.object(safety.sys, "stdin", types.SimpleNamespace(buffer=io.BytesIO(b"{}"))), \
                mock.patch.object(safety.sys, "stdout", types.SimpleNamespace(buffer=stdout)), \
                mock.patch.object(safety.sys, "stderr", types.SimpleNamespace(buffer=stderr)):
            self.assertEqual(safety._fetch_worker(), 2)
        self.assertEqual(stdout.getvalue(), b"")
        self.assertEqual(stderr.getvalue(), b"blocked\n")


    def test_supervised_fetch_reaps_blocked_dns_worker(self):
        started = time.monotonic()
        self.assertBlocked(lambda: self.supervised_fake("import time; time.sleep(10)"), 'supervised official fetch absolute deadline exceeded')
        self.assertLess(time.monotonic() - started, 0.60)

    def test_supervised_fetch_fake_bytes_no_network(self):
        self.assertEqual(self.supervised_fake(
            "import sys,time; time.sleep(0.03); sys.stdin.buffer.read(); "
            "sys.stdout.buffer.write(b'fake archive'); sys.stdout.buffer.flush()", deadline=2),
            b"fake archive")

    def test_supervised_fetch_output_limit_stops_and_reaps(self):
        self.assertBlocked(lambda: self.supervised_fake(
            "import sys,time; time.sleep(0.03); sys.stdout.buffer.write(b'x'*101); "
            "sys.stdout.buffer.flush(); time.sleep(10)"), 'official fetch output exceeds bound')

    def test_supervised_fetch_failure_diagnostics_never_propagate(self):
        try:
            self.supervised_fake(
                "import sys,time; time.sleep(0.03); sys.stderr.write('synthetic-private-diagnostic'); "
                "sys.exit(2)")
        except safety.Blocked as error:
            self.assertNotIn("synthetic-private-diagnostic", str(error))
        else:
            self.fail("nonzero helper must block")


    def test_terminal_hup_and_quit_are_deferred_and_restored(self):
        handlers = {signum: object() for signum in
                    (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT)}
        originals = dict(handlers)
        def install(signum, callback):
            previous = handlers[signum]
            handlers[signum] = callback
            return previous
        for signum in (signal.SIGHUP, signal.SIGQUIT):
            deferred = safety.DeferredSignals()
            with mock.patch.object(signal, "signal", install):
                with deferred:
                    self.assertEqual(set(deferred._previous), set(originals))
                    handlers[signum](signum, None)
                    self.assertEqual(deferred.interrupted, signal.Signals(signum).name)
                    self.assertBlocked(deferred.guard, "qualification interrupted")
            self.assertEqual(handlers, originals)

    def test_npm_registry_hash_cannot_replace_reviewed_anchor(self):
        data = b"registry-selected untrusted compressed input"
        integrity = base64.b64encode(hashlib.sha512(data).digest()).decode()
        with mock.patch.object(safety.gzip, "GzipFile", wraps=safety.gzip.GzipFile) as parse:
            self.assertBlocked(lambda: safety.extract_pinned_npm(
                data, self.root / "pin", integrity=integrity),
                "npm metadata does not match reviewed pin")
            parse.assert_not_called()

    def test_npm_registry_size_cannot_replace_reviewed_anchor(self):
        with mock.patch.object(safety.gzip, "GzipFile", wraps=safety.gzip.GzipFile) as parse:
            self.assertBlocked(lambda: safety.extract_pinned_npm(
                b"untrusted compressed input", self.root / "pin", unpacked_size=1),
                "npm metadata does not match reviewed pin")
            parse.assert_not_called()

    def test_release_archive_requires_reviewed_hash_before_gzip(self):
        with mock.patch.object(safety.gzip, "GzipFile", wraps=safety.gzip.GzipFile) as parse:
            self.assertBlocked(lambda: safety.extract_pinned_release(
                b"untrusted release", self.root / "pin"),
                "official release pre-parse hash required")
            parse.assert_not_called()

    def test_credential_aliases_denied_before_transport(self):
        aliases = ("/api//credential", "/api/%63redential", "/api/%2563redential",
                   "/api/%252563redential", "/api/./credential", "/api/x/../credential",
                   "/%61pi/credential", "/api/credential%2Fchild", "/api/credential/")
        for seeding in (False, True):
            client = safety.OwnedHTTP("http://127.0.0.1:1234", self.identity, self.proc,
                                      b"synthetic-password", seeding_mode=seeding)
            with mock.patch.object(safety, "_bounded_response", return_value=(200, None, b"{}")) as transport:
                for path in aliases:
                    self.assertBlocked(lambda: client.request("GET", path), "credential endpoint forbidden")
                transport.assert_not_called()

    def test_ambiguous_noncredential_paths_block_before_transport(self):
        client = safety.OwnedHTTP("http://127.0.0.1:1234", self.identity, self.proc,
                                  b"synthetic-password")
        paths = ("/api//info", "/api/%69nfo", "/api/%GGinfo", "/api/info#fragment",
                 "/api/info%00", "/api/..%5Cinfo", "/api/%25252525252525252569nfo",
                 "/api/\tinfo", "/api/info;parameter", "/api/info#")
        with mock.patch.object(safety, "_bounded_response", return_value=(200, None, b"{}")) as transport:
            for path in paths:
                self.assertBlocked(lambda: client.request("GET", path), "ambiguous API path")
            transport.assert_not_called()

    def test_stale_gone_journal_clears_without_signalling_and_records_fact(self):
        journal = self.root / "stopped.json"
        journal.write_text(json.dumps({"version": 1, **self.identity.report()}))
        journal.chmod(0o600)
        (self.root / "proc" / "71" / "stat").unlink()
        (self.root / "proc" / "71").rmdir()
        sent = []
        keeper = safety.StopJournal(journal, self.proc, sender=lambda *args: sent.append(args))
        self.assertTrue(keeper.recover())
        self.assertFalse(journal.exists())
        self.assertEqual(sent, [])
        self.assertEqual(keeper.recovery_fact, {
            "identity": self.identity.report(), "disposition": "already_gone", "signalled": False})
        self.assertFalse(keeper.recover())
        self.assertIsNone(keeper.recovery_fact)

    def test_stale_unknown_journal_remains_blocked(self):
        journal = self.root / "stopped.json"
        journal.write_text(json.dumps({"version": 1, **self.identity.report()}))
        journal.chmod(0o600)
        sent = []
        keeper = safety.StopJournal(journal, self.proc, sender=lambda *args: sent.append(args))
        (self.root / "proc" / "71" / "stat").write_bytes(b"corrupt")
        self.assertBlocked(keeper.recover, "stopped-daemon journal identity uncertain; retained")
        self.assertTrue(journal.exists())
        self.assertEqual(sent, [])

    def test_reused_pid_proves_old_journal_gone_without_signalling_new_process(self):
        journal = self.root / "stopped.json"
        journal.write_text(json.dumps({"version": 1, **self.identity.report()}))
        journal.chmod(0o600)
        (self.root / "proc" / "71" / "stat").write_bytes(fake_stat(ticks=999))
        sent = []
        keeper = safety.StopJournal(journal, self.proc, sender=lambda *args: sent.append(args))
        self.assertTrue(keeper.recover())
        self.assertFalse(journal.exists())
        self.assertEqual(sent, [])
        self.assertEqual(keeper.recovery_fact, {
            "identity": self.identity.report(), "disposition": "already_gone",
            "signalled": False, "proof": "pid_reused"})
        self.assertTrue(self.proc.alive(safety.Identity(71, 999)))


    def test_block_reason_assertion_rejects_wrong_safety_stop(self):
        def wrong_stop():
            raise safety.Blocked("environment allow-list failed")
        with self.assertRaises(AssertionError):
            self.assertBlocked(wrong_stop, "positive cost hard stop")


    def test_owned_listener_uses_only_verified_fd(self):
        directory = self.root / "proc" / "71"
        (directory / "fd").mkdir()
        (directory / "net").mkdir()
        (directory / "fd" / "7").symlink_to("socket:[7788]")
        (directory / "net" / "tcp").write_text(
            "  sl local_address rem_address st tx tr retr uid timeout inode\n"
            "  0: 0100007F:1234 00000000:0000 0A 00000000:00000000 00:00000000 0 0 0 7788\n"
            "  1: 0100007F:9999 00000000:0000 0A 00000000:00000000 00:00000000 0 0 0 9999\n")
        self.assertEqual(self.proc.listener(self.identity), "http://127.0.0.1:4660")

    def test_owned_http_auth_and_redirect_nonforwarding(self):
        received = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass
            def do_GET(self):
                received.append(self.headers.get("Authorization"))
                if self.path == "/redirect":
                    self.send_response(302)
                    self.send_header("Location", "/never")
                elif self.headers.get("Authorization"):
                    self.send_response(200)
                else:
                    self.send_response(401)
                self.end_headers()
                self.wfile.write(b"{}")
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            client = safety.OwnedHTTP(f"http://127.0.0.1:{server.server_port}",
                                      self.identity, self.proc, b"synthetic-password")
            self.assertEqual(client.request("GET", "/api/info", authenticated=False)[0], 401)
            self.assertEqual(client.request("GET", "/api/info")[0], 200)
            self.assertBlocked(lambda: client.request("GET", "/redirect"), 'owned API redirect refused')
            self.assertEqual(len(received), 3)
            self.assertBlocked(lambda: client.request("GET", "/api/credential"), 'credential endpoint forbidden')
            self.assertBlocked(lambda: client.request("GET", "//elsewhere/path"), 'API path changes owned origin')
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


def test_names():
    return unittest.defaultTestLoader.getTestCaseNames(SafetyTests)


def self_test():
    """Return failed test names, matching claude.py's no-throw self-test interface."""
    result = unittest.TestResult()
    unittest.defaultTestLoader.loadTestsFromTestCase(SafetyTests).run(result)
    return [test.id().rsplit(".", 1)[-1] for test, _detail in (*result.errors, *result.failures)]


if __name__ == "__main__":
    unittest.main(verbosity=2)
