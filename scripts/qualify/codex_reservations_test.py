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
        with self.assertRaisesRegex(Blocked, "base turn cap"):
            Reservations(self.path).reserve(digest(8), "spawn", digest("prompt"), digest("cwd"))

    def test_pending_tool_uses_active_turn_not_settled_history(self):
        key = self.reserve()
        self.settle(key, 0)
        generation, thread = digest(0), digest((0, "thread"))
        expected = {"argv": digest("short fixed command")}
        next_key = digest(1)
        self.ledger.reserve(next_key, "resume", digest("next prompt"), digest("cwd"), thread, expected)
        self.ledger.claim("turn/start", generation, digest("next request"), digest("cwd"), thread,
                          digest("next prompt"))
        self.assertEqual(self.ledger.tool(generation, thread), expected)

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

    def test_only_two_same_prompt_same_thread_reasks_extend_the_seven_turn_cap(self):
        for index in range(7):
            key = digest(index)
            self.ledger.reserve(key, "spawn", digest("prompt"), digest("cwd"),
                                tool={"argv": digest("fixed command")})
            self.settle(key, index)
        for index in (0, 1):
            self.ledger.mark_miss(digest(index))
            self.ledger.reserve(digest((index, "reask")), "resume", digest("prompt"), digest("cwd"),
                digest((index, "thread")), {"argv": digest("fixed command")}, reask_of=digest(index))
        with self.assertRaises(Blocked):
            self.ledger.reserve(digest("ordinary eighth"), "spawn", digest("prompt"), digest("cwd"))
        self.assertEqual(len(self.ledger.snapshot()["rows"]), 9)

    def test_reasks_require_a_proven_miss_and_identical_submission_identity(self):
        key, tool = digest(0), {"argv": digest("fixed command")}
        self.ledger.reserve(key, "spawn", digest("prompt"), digest("cwd"), tool=tool)
        self.settle(key, 0)
        thread = digest((0, "thread"))
        with self.assertRaisesRegex(Blocked, "unique same-thread same-prompt"):
            self.ledger.reserve(digest("unmarked"), "resume", digest("prompt"), digest("cwd"), thread,
                                tool, reask_of=key)
        self.ledger.mark_miss(key)
        cases = (("spawn", digest("prompt"), digest("cwd"), thread, tool),
                 ("resume", digest("changed"), digest("cwd"), thread, tool),
                 ("resume", digest("prompt"), digest("changed"), thread, tool),
                 ("resume", digest("prompt"), digest("cwd"), digest("foreign"), tool),
                 ("resume", digest("prompt"), digest("cwd"), thread, {"argv": digest("other command")}))
        for index, (verb, prompt, cwd, candidate_thread, candidate_tool) in enumerate(cases):
            with self.subTest(index=index), self.assertRaisesRegex(Blocked, "unique same-thread same-prompt"):
                self.ledger.reserve(digest(index + 10), verb, prompt, cwd, candidate_thread, candidate_tool, reask_of=key)
        self.ledger.reserve(digest("valid"), "resume", digest("prompt"), digest("cwd"), thread, tool, reask_of=key)
        with self.assertRaisesRegex(Blocked, "unique same-thread same-prompt"):
            self.ledger.reserve(digest("duplicate"), "resume", digest("prompt"), digest("cwd"), thread, tool, reask_of=key)
        self.assertEqual(len(self.ledger.snapshot()["rows"]), 2)

    def test_non_tool_turns_and_failed_or_active_turns_cannot_be_marked_as_misses(self):
        key = self.reserve()
        with self.assertRaisesRegex(Blocked, "invalid tool miss"):
            self.ledger.mark_miss(key)

        self.settle(key, 0)
        with self.assertRaisesRegex(Blocked, "invalid tool miss"):
            self.ledger.mark_miss(key)
        with self.ledger.transaction() as state:
            state["rows"][key]["tool"] = {"argv": digest("fixed command")}
            state["rows"][key]["envelope"]["state"] = "failed"
        with self.assertRaisesRegex(Blocked, "invalid tool miss"):
            self.ledger.mark_miss(key)

    def test_reask_cap_is_two_even_when_total_and_concurrency_have_room(self):
        tool = {"argv": digest("fixed command")}
        for index in range(3):
            key = digest(index)
            self.ledger.reserve(key, "spawn", digest("prompt"), digest("cwd"), tool=tool)
            self.settle(key, index)
            self.ledger.mark_miss(key)
        for index in (0, 1):
            key, generation, thread = digest((index, "reask")), digest(index), digest((index, "thread"))
            self.ledger.reserve(key, "resume", digest("prompt"), digest("cwd"), thread, tool, reask_of=digest(index))
            request = digest((index, "reask request"))
            self.ledger.claim("turn/start", generation, request, digest("cwd"), thread, digest("prompt"))
            self.ledger.reply(key, "turn/start", generation, request, thread, digest((index, "new turn")))
            identity = {"session": digest((index, "session")), "address": digest((index, "new address")),
                        "thread": thread, "turn": 2, "state": "completed", "stop_reason": "end_turn"}
            self.ledger.receipt(key, {field: identity[field] for field in ("session", "address")})
            self.ledger.settle(key, identity)
            with self.assertRaisesRegex(Blocked, "invalid tool miss"):
                self.ledger.mark_miss(key)
        self.assertEqual(len(self.ledger.snapshot()["rows"]), 5)
        with self.assertRaisesRegex(Blocked, "re-ask cap"):
            self.ledger.reserve(digest("third"), "resume", digest("prompt"), digest("cwd"),
                                digest((2, "thread")), tool, reask_of=digest(2))

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
