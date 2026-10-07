"""Fake-backed reservation proofs for Codex packet §§3/7; no vendor processes."""
import hashlib
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest

from claude import Blocked
from codex_reservations import Reservations


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


class ReservationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "reservations.json"
        self.ledger = Reservations(self.path, time.monotonic() + 60)

    def reserve(self, index=0):
        key = digest(index)
        self.ledger.reserve(key, "spawn", digest("prompt"), digest("cwd"))
        return key

    def settle(self, key, index):
        generation, thread, turn = digest(index), digest((index, "thread")), digest((index, "turn"))
        request = digest((index, "attach"))
        self.assertEqual(Reservations(self.path).claim("thread/start", generation, request, digest("cwd")), key)
        self.ledger.reply(key, "thread/start", generation, request, thread)
        request = digest((index, "start"))
        self.assertEqual(Reservations(self.path).claim("turn/start", generation, request, digest("cwd"), thread,
                                                     digest("prompt")), key)
        self.ledger.reply(key, "turn/start", generation, request, thread, turn)
        identity = {"session": digest((index, "session")), "address": digest((index, "address")),
                    "thread": thread, "turn": 1, "state": "completed", "stop_reason": "end_turn"}
        self.ledger.receipt(key, {name: identity[name] for name in ("session", "address")})
        self.ledger.settle(key, identity)

    def test_global_turn_cap_survives_new_proxy_instances(self):
        for index in range(7):
            key = self.reserve(index)
            self.settle(key, index)
        with self.assertRaisesRegex(Blocked, "turn/concurrency cap"):
            Reservations(self.path).reserve(digest(8), "spawn", digest("prompt"), digest("cwd"))

    def test_concurrent_reservation_cap_is_atomic(self):
        admitted, refused = [], []
        barrier = threading.Barrier(3)
        def worker(index):
            barrier.wait(timeout=5)
            try:
                Reservations(self.path).reserve(digest(index), "spawn", digest(index), digest("cwd"))
                admitted.append(index)
            except Blocked:
                refused.append(index)
        threads = [threading.Thread(target=worker, args=(index,)) for index in range(3)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive())
        self.assertEqual(len(admitted), 2)
        self.assertEqual(len(refused), 1)

    def test_native_claim_is_single_use_across_generations(self):
        key = self.reserve()
        one, two = Reservations(self.path), Reservations(self.path)
        self.assertEqual(one.claim("thread/start", digest(1), digest("attach"), digest("cwd")), key)
        with self.assertRaisesRegex(Blocked, "unique reservation"):
            two.claim("thread/start", digest(2), digest("extra"), digest("cwd"))
        one.reply(key, "thread/start", digest(1), digest("attach"), digest("thread"))
        with self.assertRaises(Blocked):
            two.claim("turn/start", digest(2), digest("wrong"), digest("cwd"), digest("thread"), digest("foreign prompt"))
        self.assertEqual(one.claim("turn/start", digest(1), digest("start"), digest("cwd"),
                                   digest("thread"), digest("prompt")), key)
        with self.assertRaises(Blocked):
            two.claim("turn/start", digest(2), digest("extra"), digest("cwd"), digest("thread"), digest("prompt"))

    def test_unknown_receipts_settlement_and_closed_admission_block(self):
        key = self.reserve()
        with self.assertRaises(Blocked):
            self.ledger.settle(key, {"session": digest("s"), "address": digest("a"), "thread": digest("t")})
        self.ledger.close()
        with self.assertRaises(Blocked):
            Reservations(self.path).claim("thread/start", digest(1), digest(2), digest("cwd"))
        with self.assertRaises(Blocked):
            self.reserve(1)

    def test_unsafe_ledger_and_lock_never_grant(self):
        for path in (self.path, self.ledger.lock):
            with self.subTest(path=path.name):
                path.chmod(0o644)
                with self.assertRaisesRegex(Blocked, "unsafe file"):
                    self.reserve()
                path.chmod(0o600)
