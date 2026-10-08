#!/usr/bin/env python3
"""Fake and nonforwarding mock qualification checks (OpenCode packet §13)."""

import copy
import http.client
import json
import socket
import threading
import time
import tempfile
from pathlib import Path
from unittest import mock
import unittest
from types import SimpleNamespace

import opencode_cases as cases
import opencode_driver as transport


def sample_event(seq, kind, data, session="ses_owned", **extra):
    return {"seq": seq, "id": "evt_" + str(seq), "type": kind,
            "sessionID": session, "data": data, **extra}


def tokens(count):
    return {"input": count, "output": count + 1, "reasoning": 0,
            "cache": {"read": 7, "write": 3}}


def ledger_events():
    return [
        sample_event(1, "session.step.ended", {"assistantMessageID": "title", "tokens": tokens(999)}),
        sample_event(2, "session.inbox.delivered", {"inboxID": "input_owned"}),
        sample_event(3, "session.step.ended", {"assistantMessageID": "a", "tokens": tokens(2)}),
        sample_event(4, "session.step.ended", {"assistantMessageID": "a", "tokens": tokens(4)}),
        sample_event(5, "session.step.ended", {"assistantMessageID": "child", "tokens": tokens(999)},
                     session="ses_child"),
        sample_event(6, "session.usage.updated", {"tokens": tokens(999)}),
        sample_event(7, "session.step.ended", {"assistantMessageID": "b", "tokens": tokens(6)}),
        sample_event(8, "session.execution.succeeded", {}),
    ]


class FakeDriver:
    def __init__(self, replies=None):
        self.replies = replies or {}
        self.calls = []

    def execute(self, operation, **args):
        self.calls.append((operation, args))
        if operation not in self.replies:
            raise cases.EvidenceUnavailable("fake observation missing: " + operation)
        value = self.replies[operation]
        return value(args) if callable(value) else copy.deepcopy(value)


class CasesTests(unittest.TestCase):
    def test_identity_case_acquires_generation_before_arming_retired_host(self):
        retired=False; calls=[]
        first={'session_id':'s_owned','envelope':{'vendor_session_id':'ses_owned'}}
        def execute(operation,**args):
            nonlocal retired
            calls.append(operation)
            if operation=='idle_retirement': retired=True
            elif operation=='owned_server_identity': retired=False
            elif operation=='seam_arm':
                if retired: raise transport.Blocked('owned Host generation absent')
            elif operation=='start_turn': return {'session_id':'s_owned'}
            elif operation=='seam_wait': return {'occurrence':1}
            elif operation=='wait_turn': return {'state':'completed'}
            elif operation=='seam_observation': return {'acknowledged':[1]}
            elif operation=='identity_duplicate_control': return {'second_occurrence_failed':True}
            elif operation=='missing_vendor_session': return {'code':'resume_mismatch','creates':0}
            return {}
        driver=SimpleNamespace(execute=execute)
        with mock.patch.object(cases,'fixture',return_value='fixture'), \
             mock.patch.object(cases,'turn',return_value=first), \
             mock.patch.object(cases,'completed'):
            cases.case_identity(driver,cases.Case('identity'))
        self.assertLess(calls.index('idle_retirement'),calls.index('owned_server_identity'))
        self.assertLess(calls.index('owned_server_identity'),calls.index('seam_arm'))

    def test_record_only_block_stops_every_later_case(self):
        driver=FakeDriver({'build_hashes':{'release':'a'*64,'test-failpoints':'b'*64},
                           'phase_begin':None,'phase_end':None,'phase_guard':None,'phase_build':None})
        later=mock.Mock()
        def blocked(_driver,_case): raise cases.safety.Blocked('FAKE record-only safety stop')
        with mock.patch.dict(cases.PHASE_CASES,{'ledger':('compaction','usage')}), \
             mock.patch.dict(cases.CASES,{'compaction':blocked,'usage':later}):
            records=cases.run_phase(driver,'ledger')
        later.assert_not_called()
        self.assertEqual(len(records),1)
        self.assertEqual(records[0]['disposition'],'record-only')
        self.assertEqual(records[0]['blocking']['reason'],'FAKE record-only safety stop')

    def hostile_driver(self):
        return FakeDriver({"fixture": "fixture", "turn": {"session_id": "s_hostile",
                              "envelope": {"state": "failed", "vendor_session_id": "ses_owned",
                                           "final_text": None}, "vendor_error_observed": True},
                           "mock_receipt": {"received": True, "model_matches": True},
                           "hostile_output_matrix": {"complete": True,
                              "verbs": list(cases.HOSTILE_OUTPUT_VERBS), "event_pages": 2},
                           "secrecy_scan": {"complete": True, "secret_absent": True, "payload_captures": 0}})

    def test_l4_every_fixture_covers_output_matrix_and_selected_variant(self):
        driver = self.hostile_driver()
        result = cases.run_case(driver, "hostile_provider", cases.case_hostile_provider)
        self.assertEqual(result["result"], "pass")
        self.assertEqual(sum(name == "hostile_output_matrix" for name, _ in driver.calls), 8)
        self.assertTrue(all(args["params"]["effort"] == "fixture-secret"
                            for name, args in driver.calls if name == "turn"))
        names = [name for name, _ in driver.calls]
        self.assertLess(names.index("hostile_output_matrix"), names.index("secrecy_scan"))

    def test_l4_missing_or_incomplete_output_proof_never_passes(self):
        driver = self.hostile_driver()
        del driver.replies["hostile_output_matrix"]
        self.assertEqual(cases.run_case(driver, "hostile_provider", cases.case_hostile_provider)["result"],
                         "not_observable")
        driver = self.hostile_driver()
        driver.replies["hostile_output_matrix"]["verbs"].remove("describe")
        self.assertEqual(cases.run_case(driver, "hostile_provider", cases.case_hostile_provider)["result"], "fail")

    def permission_sample(self):
        return {"complete": True, "child_rules": [{"session_id": "ses_child", "permissions": [
            {"action": name, "resource": "*", "effect": effect} for name, effect in (
            ("*", "allow"), ("question", "deny"), ("opencode_session_move", "deny"),
            ("opencode_session_rename", "deny"), ("opencode_list_mcp_resources", "deny"),
            ("opencode_read_mcp_resource", "deny"), ("skill", "deny"))]}],
            "native_events": [{"type": "session.tool.failed", "data": {"sessionID": "ses_child",
                "id": "call_denied", "name": "question", "error": {"type": "permission.rejected"}}}],
            "events": [{"type": "action.denied", "kind": "other", "target": "question"}]}

    def test_permission_uses_actual_child_denial_and_readback(self):
        result = cases.permission_attempts(self.permission_sample())
        self.assertEqual(result["attempts"][0]["callID"], "call_denied")
        self.assertTrue(result["attempts"][0]["deny_rule"])
        self.assertFalse(result["attempts"][0]["asked"])
        self.assertEqual(result["unhandled_asks"], 0)

    def test_permission_input_started_can_precede_the_tool_name(self):
        raw = self.permission_sample()
        raw["native_events"].insert(0, {"type": "session.tool.input.started", "data": {
            "sessionID": "ses_child", "id": "call_denied", "assistantMessageID": "assistant_owned"}})
        self.assertEqual(cases.permission_attempts(raw)["attempts"][0]["callID"], "call_denied")

    def test_permission_silence_or_native_only_never_passes(self):
        raw = self.permission_sample()
        raw["child_rules"] = []
        with self.assertRaises(cases.EvidenceUnavailable):
            cases.permission_attempts(raw)
        raw = self.permission_sample()
        raw["events"] = []
        with self.assertRaises(cases.EvidenceUnavailable):
            cases.permission_attempts(raw)

    def test_permission_conflicting_extra_rules_cannot_prove_precedence(self):
        raw = self.permission_sample()
        raw["child_rules"][0]["permissions"].append({"action": "question", "resource": "*", "effect": "allow"})
        self.assertFalse(cases.permission_attempts(raw)["attempts"][0]["deny_rule"])

    def test_permission_ask_requires_reject_and_actual_settlement(self):
        raw = self.permission_sample()
        raw["native_events"].extend([
            {"type": "permission.asked", "data": {"id": "req1", "sessionID": "ses_child",
                                                    "source": {"id": "call_denied"}}},
            {"type": "permission.replied", "data": {"requestID": "req1", "reply": "reject"}}])
        raw["events"] = [{"type": "vendor.request_declined", "vendor_method": "permission.asked:question"}]
        result = cases.permission_attempts(raw)
        self.assertEqual(result["unhandled_asks"], 0)
        self.assertTrue(result["attempts"][0]["settled"])
        self.assertIsNone(result["attempts"][0]["decline_ms"])
        raw["native_events"].pop()
        self.assertEqual(cases.permission_attempts(raw)["unhandled_asks"], 1)

    def test_empty_or_unverifiable_checks_never_pass(self):
        self.assertEqual(cases.Case("empty").finish()["result"], "not_observable")
        result = cases.run_case(FakeDriver(), "auth", cases.case_auth)
        self.assertEqual(result["result"], "not_observable")

    def test_missing_auth_evidence_never_passes(self):
        result = cases.run_case(FakeDriver({"auth_checks": {"pid_matches": True}}),
                                "auth", cases.case_auth)
        self.assertEqual(result["result"], "not_observable")

    def test_password_leak_is_a_failure(self):
        raw = {"pid_matches": True, "absent_status": 401, "wrong_status": 401,
               "correct_status": 200, "memory_only": True, "evidence_scan_complete": True,
               "evidence_password_absent": False, "tool_password_key_absent": True,
               "password_changed": True}
        result = cases.run_case(FakeDriver({"auth_checks": raw}), "auth", cases.case_auth)
        self.assertEqual(result["result"], "fail")
        self.assertIn("evidence_password_absent", result["reason"])

    def test_memory_only_password_predicates_complete(self):
        raw = {"pid_matches": True, "absent_status": 401, "wrong_status": 401,
               "correct_status": 200, "memory_only": True, "evidence_scan_complete": True,
               "evidence_password_absent": True, "tool_password_key_absent": True,
               "password_changed": True}
        result = cases.run_case(FakeDriver({"auth_checks": raw}), "auth", cases.case_auth)
        self.assertEqual(result["result"], "pass")
        self.assertEqual(len(result["checks"]), 9)

    def test_usage_final_samples_supersede_child_title_cumulative_excluded(self):
        ledger = cases.usage_ledger(ledger_events(), "ses_owned", "execution_owned", "input_owned")
        self.assertEqual(ledger["steps"], 2)
        self.assertEqual(ledger["totals"]["input_tokens"], 10)
        self.assertEqual(ledger["totals"]["cached_input_tokens"], 14)
        self.assertEqual(ledger["totals"]["cache_write_tokens"], 6)
        self.assertEqual(ledger["scope"], "turn")

    def test_usage_missing_samples_not_zero(self):
        events = ledger_events()
        del events[6]["data"]["tokens"]["input"]
        with self.assertRaises(cases.EvidenceUnavailable):
            cases.usage_ledger(events, "ses_owned", "execution_owned", "input_owned")

    def test_usage_requires_owned_delivery_and_terminal(self):
        for events in (ledger_events()[2:], ledger_events()[:-1]):
            with self.assertRaises(cases.EvidenceUnavailable):
                cases.usage_ledger(events, "ses_owned", "execution_owned", "input_owned")

    def test_foreign_join_prevents_usage_claim(self):
        events = ledger_events()
        events.insert(3, sample_event(3, "session.inbox.delivered", {"inboxID": "foreign"}))
        with self.assertRaises(cases.EvidenceUnavailable):
            cases.usage_ledger(events, "ses_owned", "execution_owned", "input_owned")

    def test_compaction_fallback_key_distinct_and_interval_scope(self):
        events = ledger_events()
        events.insert(-1, sample_event(8, "session.compaction.ended", {"tokens": tokens(5)}))
        ledger = cases.usage_ledger(events, "ses_owned", "execution_owned", "input_owned")
        self.assertEqual(ledger["compactions"], 1)
        self.assertEqual(ledger["totals"]["input_tokens"], 15)
        self.assertEqual(ledger["scope"], "vendor_interval")

    def test_record_only_absence_not_pass(self):
        driver = FakeDriver({"bounded_probe": {"attempts": 3, "observations": []}})
        result = cases.run_case(driver, "forms", cases.CASES["forms"], "record-only")
        self.assertEqual(result["result"], "deferred")
        self.assertEqual(result["disposition"], "deferred-with-reason")
        self.assertEqual(driver.calls, [])

    def lsp_proof(self, spawned=False):
        return {"complete": True, "spawned": spawned, "config_checked": True,
                "mock_received": True, "read_attempted": True, "readiness_checked": True,
                "readiness_seconds": .1, "pin_owned": True,
                "disposition": "offered" if spawned else "lsp: not offered by pinned 2.0.22 (read trigger; 5 s readiness window)"}

    def never_ask_driver(self, spawned=False):
        result = self.write_driver().replies["turn"]
        return FakeDriver({"set_inherit": None, "fixture": "fixture", "turn": result,
                           "lsp_probe": self.lsp_proof(spawned),
                           "permission_observations": {"attempts": [{"callID": "call_denied",
                               "child_session": "ses_child", "deny_rule": True, "asked": False,
                               "disposition": "action.denied"}], "unhandled_asks": 0},
                           "helper_observations": {"mcp_started": True, "plugin_started": True,
                               "hook_started": True, "fake_lsp_started": spawned,
                               "package_inventory_clean": True, "binary_inventory_clean": True}})

    def test_negative_lsp_limitation_names_read_trigger_and_window(self):
        case=cases.Case("lsp", "gate")
        self.assertFalse(cases.checked_lsp_probe(case,self.lsp_proof(False)))
        self.assertEqual(case.limitations,["lsp: not offered by pinned 2.0.22 (read trigger; 5 s readiness window)"])

    def test_l5_negative_configured_read_probe_is_an_explicit_limitation(self):
        result = cases.run_case(self.never_ask_driver(), "never_ask", cases.case_never_ask)
        self.assertEqual(result["result"], "pass")
        self.assertIn("lsp: not offered by pinned 2.0.22 (read trigger; 5 s readiness window)", result["limitations"])

    def test_l5_no_actual_read_cannot_claim_lsp_not_offered(self):
        driver = self.never_ask_driver()
        driver.replies["lsp_probe"]["read_attempted"] = False
        result = cases.run_case(driver, "never_ask", cases.case_never_ask)
        self.assertEqual(result["result"], "not_observable")
        self.assertNotIn("lsp: not offered by pinned 2.0.22 (read trigger; 5 s readiness window)", result["limitations"])

    def test_l5_positive_probe_requires_current_fixture_lsp_spawn(self):
        driver = self.never_ask_driver(spawned=True)
        driver.replies["helper_observations"]["fake_lsp_started"] = False
        result = cases.run_case(driver, "never_ask", cases.case_never_ask)
        self.assertEqual(result["result"], "fail")

    def test_native_cursor_reordering_blocks(self):
        rows = ledger_events()
        rows[2]["seq"] = 1
        driver = FakeDriver({"native_events": {"complete": True, "events": rows}})
        with self.assertRaises(cases.EvidenceUnavailable):
            cases.native_events(driver, "ses_owned")

    def recall_events(self):
        return [sample_event(1, "session.inbox.delivered", {"inboxID": "input_owned"}),
                sample_event(2, "session.step.started", {"assistantMessageID": "a"}),
                sample_event(3, "session.text.ended", {"assistantMessageID": "a", "ordinal": 1,
                                                       "text": "SECOND"}),
                sample_event(4, "session.text.ended", {"assistantMessageID": "a", "ordinal": 0,
                                                       "text": "FIRST"}),
                sample_event(5, "session.step.ended", {"assistantMessageID": "a"}),
                sample_event(6, "session.execution.succeeded", {})]

    def test_no_retrieval_requires_complete_native_interval(self):
        observation = cases.owned_text_and_tools(self.recall_events(), "ses_owned", "input_owned")
        self.assertEqual(observation["final_text"], "FIRSTSECOND")
        self.assertEqual(observation["tool_calls"], [])
        with self.assertRaises(cases.EvidenceUnavailable):
            cases.owned_text_and_tools(self.recall_events()[:-1], "ses_owned", "input_owned")

    def test_tool_retrieval_observable_not_narration(self):
        events = self.recall_events()
        events.insert(2, sample_event(3, "session.tool.called", {"id": "call_real", "name": "read"}))
        observation = cases.owned_text_and_tools(events, "ses_owned", "input_owned")
        self.assertEqual(observation["tool_calls"], ["call_real"])

    def test_unknown_tool_shape_never_proves_no_retrieval(self):
        events = self.recall_events()
        events.insert(2, sample_event(3, "session.tool.future", {"id": "call_unknown"}))
        with self.assertRaises(cases.EvidenceUnavailable):
            cases.owned_text_and_tools(events, "ses_owned", "input_owned")

    def test_all_packet_rows_dispositioned(self):
        self.assertEqual(set(cases.OC_DISPOSITIONS), {"OC01", "OC02", "OC02b", "OC03", "OC04",
                         "OC05", "OC06", "OC07", "OC08", "OC09", "OC10", "OC11", "OC12", "OC12b"})
        self.assertEqual(set(cases.L_DISPOSITIONS), {"L" + str(n) for n in range(1, 13)} | {"L14"})
        self.assertEqual(cases.L_DISPOSITIONS["L10"][0], "deferred-with-reason")

    def test_per_phase_build_and_admission_ceilings(self):
        phases = {row.name: row for row in cases.PHASES}
        self.assertEqual(phases["seams"].build, "test-failpoints")
        self.assertEqual(phases["anchor"].build, "release")
        self.assertEqual(phases["long_run"].public_turns, 0)
        self.assertEqual(phases["long_run"].mock_turns, 32)
        self.assertEqual(phases["anchor"].seconds, 7200)
        self.assertEqual(phases["isolation"].public_turns + phases["hostile"].public_turns
                         + phases["helpers"].public_turns, 6)
        self.assertEqual(phases["credentials"].mock_turns, 0)
        self.assertNotIn("usage", cases.PHASE_CASES["free"])
        self.assertEqual(cases.PHASE_CASES["ledger"], ("usage", "compaction"))
        # Eight B7 turns plus eight title calls; eight hostile fixtures each
        # with up to three provider attempts plus one title call. Further
        # automatic calls hit the hard callback cap, never a budget increase.
        self.assertLessEqual(8 + 8, phases["isolation"].mock_turns)
        self.assertLessEqual(8 * (3 + 1), phases["hostile"].mock_turns)
        # Ledger two-step+title, plus three bounded compaction controls with
        # four model calls and one title each fit the isolated mock budget.
        self.assertLessEqual(3 + 3 * (4 + 1), phases["ledger"].mock_turns)
        self.assertLessEqual(8 * 2 + 8 + 8, phases["long_run"].mock_turns)

    def test_positive_cost_and_paid_identity_fail(self):
        result = {"envelope": {"state": "completed", "vendor_version": "2.0.22",
                               "vendor_session_id": "ses_owned", "final_text": "ok",
                               "cost": {"usd": 0}}, "mode": "public-free",
                  "model": "opencode/mimo-v2.6-flash-free", "owned_terminal": True}
        for mutate in (lambda row: row["envelope"]["cost"].update(usd=0.001),
                       lambda row: row.update(model="paid/model")):
            copy_result = copy.deepcopy(result)
            mutate(copy_result)
            with self.assertRaises(cases.QualificationFailure):
                cases.completed(cases.Case("free"), copy_result, public=True)

    def test_unavailable_cost_does_not_fabricate_zero(self):
        result = {"envelope": {"state": "completed", "vendor_version": "2.0.22",
                               "vendor_session_id": "ses_owned", "final_text": "ok",
                               "cost": {"usd": None}}, "mode": "public-free",
                  "model": "opencode/mimo-v2.6-flash-free", "owned_terminal": True}
        case = cases.Case("free")
        cases.completed(case, result, public=True)
        self.assertEqual(case.finish()["result"], "pass")
        self.assertFalse(any(row["check"] == "reported cost is zero" for row in case.checks))

    def test_armed_pause_released_on_evidence_failure(self):
        driver = FakeDriver({"seam_arm": None, "seam_release": None})
        with self.assertRaises(cases.EvidenceUnavailable):
            with cases.armed(driver, cases.SEAMS["partial"]):
                raise cases.EvidenceUnavailable("unreadable")
        self.assertEqual([name for name, _ in driver.calls], ["seam_arm", "seam_release"])

    def write_driver(self, held=True, manifest=True, full_interrupt=True):
        state = {"mode": "partial"}
        def make_fixture(args):
            state["mode"] = args["name"].removeprefix("write-")
            return "fixture"
        def seam_wait(_args):
            return {"owned": True, "written": 1024 if state["mode"] == "partial" else 8192,
                    "body_length": 8192}
        result = {"session_id": "s1", "turn": "s1/1", "model": "oclive-mock/fixture-free",
                  "mode": "mock", "owned_terminal": True,
                  "envelope": {"state": "completed", "vendor_version": "2.0.22", "turn": 1,
                               "vendor_session_id": "ses_owned", "final_text": "SETUP", "cost": {"usd": 0}}}
        return FakeDriver({"fixture": make_fixture, "turn": result,
                           "start_turn": {"session_id": "s1", "turn": "s1/2"},
                           "seam_arm": None, "seam_release": None, "seam_wait": seam_wait,
                           "helper_barrier": {"owned": True, "started": True}, "helper_release": None,
                           "cancel": lambda _args: {"cancel_returned": True, "cancel_ms": 12,
                               "prompt_held": held, "native_user_interrupted": full_interrupt
                               if state["mode"] == "response" else False,
                               "sourcebound_pool_test": {"verified": manifest,
                                   "test_name": "qualification_body_prefix_cancel_preserves_offset_and_stop_pool",
                                   "source_sha256": "a" * 64,
                                   "via_builds": {"release": "b" * 64, "failpoints": "c" * 64}}},
                           "write_observation": {"enqueues": 0, "submits": 1,
                                                  "indeterminate": True, "drained": True}})

    def test_l2_partial_live_progress_and_fake_pool_are_distinct(self):
        driver = self.write_driver()
        result = cases.run_case(driver, "write_cancel", cases.case_write_cancel)
        self.assertEqual(result["result"], "pass")
        self.assertTrue(any("FAKE" in row["check"] for row in result["checks"]))
        self.assertTrue(any("native Stop response is unavailable" in row for row in result["limitations"]))
        observed = [args for name, args in driver.calls if name == "write_observation"]
        self.assertTrue(all(args["turn_number"] == 2 and args["context"]["vendor_session"] == "ses_owned"
                            for args in observed))

    def test_l2_released_prompt_or_unbound_pool_control_fails(self):
        for driver in (self.write_driver(held=False), self.write_driver(manifest=False)):
            self.assertEqual(cases.run_case(driver, "write_cancel", cases.case_write_cancel)["result"], "fail")

    def test_l2_fullbody_requires_actual_native_interrupt(self):
        driver = self.write_driver(full_interrupt=False)
        self.assertEqual(cases.run_case(driver, "write_cancel", cases.case_write_cancel)["result"], "fail")
        self.assertTrue(any(name == "helper_release" for name, _ in driver.calls))

    def long_run_driver(self, retained=None):
        created = []
        def model_turn(args):
            sid = args.get("session")
            if sid is None:
                sid = "s" + str(len(created) + 1)
                created.append(sid)
            return {"session_id": sid, "turn": sid + "/1", "model": "oclive-mock/fixture-free",
                    "mode": "mock", "owned_terminal": True, "envelope": {
                        "state": "completed", "vendor_version": "2.0.22", "vendor_session_id": "ses_" + sid,
                        "final_text": "MOCK READY", "cost": {"usd": 0}}}
        return FakeDriver({"fixture": "fixture", "turn": model_turn,
                           "phase_deadline": {"monotonic": time.monotonic() + 60},
                           "long_run_barrier_prepare": None,
                           "start_turn": {"session_id": "s8", "turn": "s8/2"},
                           "helper_barrier": {"started": True, "owned": True},
                           "memory_sample": {"rss_bytes": 65536, "active_sessions": 0,
                                             "retained_keys": retained, "retained_bytes": None,
                                             "within_packet_bounds": None},
                           "memory_tail": {"complete": True, "final_sample": True,
                                           "large_fields_seen": True, "public_requests": 0,
                                           "duration_seconds": 0.5}})

    def test_l9_counts_real_receipts_and_records_retained_bounds_unavailable(self):
        driver = self.long_run_driver()
        result = cases.run_case(driver, "long_run", cases.case_long_run, "record-only")
        self.assertEqual(result["result"], "pass")
        self.assertEqual(sum(name in {"turn", "start_turn"} for name, _ in driver.calls), 16)
        tail = next(args for name, args in driver.calls if name == "memory_tail")
        self.assertEqual(len(set(tail["sessions"])), 8)
        self.assertTrue(any("unavailable" in text for text in result["limitations"]))
        self.assertFalse(any("retained keys" in row["check"] or "byte bounds" in row["check"]
                             for row in result["checks"]))
        names = [name for name, _ in driver.calls]
        self.assertLess(names.index("start_turn"), names.index("helper_barrier"))
        self.assertLess(names.index("helper_barrier"), names.index("memory_tail"))
        self.assertFalse(any(name == "wait_turn" for name in names))

    def test_l9_estimated_retained_keys_are_not_live_evidence(self):
        result = cases.run_case(self.long_run_driver(retained=12), "long_run", cases.case_long_run, "record-only")
        self.assertEqual(result["result"], "not_observable")

    def foreign_driver(self, foreign_active=True):
        result = {"session_id": "s1", "turn": "s1/1", "model": "oclive-mock/fixture-free",
                  "mode": "mock", "owned_terminal": True,
                  "envelope": {"state": "completed", "vendor_version": "2.0.22", "turn": 1,
                               "vendor_session_id": "ses_owned", "final_text": "READY",
                               "cost": {"usd": 0}}}
        return FakeDriver({"fixture": "fixture", "turn": result,
                           "start_turn": {"session_id": "s1", "turn": "s1/2"},
                           "seam_arm": None, "seam_wait": {"owned": True}, "seam_release": None,
                           "foreign_prompt": None, "helper_barrier": {"started": True, "owned": True},
                           "foreign_fence_observation": {"foreign_active": foreign_active,
                              "owned_not_submitted": True, "foreign_not_credited": True,
                              "owned_not_terminal": True, "successor_fenced": True},
                           "cancel_nowait": {"state": "running"}, "helper_release": None,
                           "wait_turn": {"state": "cancelled"},
                           "foreign_observation": {"foreign_not_credited": True,
                              "foreign_not_cancelled": True, "owned_not_submitted": True},
                           "near_limit_inbox": {"successor_completed": True,
                                                "owned_deletes": 1, "foreign_deletes": 0}})

    def test_foreign_stays_held_through_fence_and_cancel_intent(self):
        driver = self.foreign_driver()
        result = cases.run_case(driver, "foreign", cases.case_foreign)
        self.assertEqual(result["result"], "pass")
        names = [name for name, _ in driver.calls]
        self.assertLess(names.index("seam_release"), names.index("foreign_fence_observation"))
        self.assertLess(names.index("foreign_fence_observation"), names.index("cancel_nowait"))
        self.assertLess(names.index("cancel_nowait"), names.index("helper_release"))
        self.assertLess(names.index("helper_release"), names.index("wait_turn"))
        observation = next(args for name, args in driver.calls if name == "foreign_observation")
        self.assertTrue(observation["expect_owned_absent"])

    def test_unheld_foreign_cannot_pass_and_helper_is_released(self):
        driver = self.foreign_driver(False)
        result = cases.run_case(driver, "foreign", cases.case_foreign)
        self.assertEqual(result["result"], "fail")
        self.assertEqual(driver.calls[-1][0], "helper_release")

    def anchor_driver(self, *, gone=True, certain=True, stdin=True):
        daemon, anchor, vendor = ({"pid": 101, "start_ticks": 1},
                                   {"pid": 102, "start_ticks": 2},
                                   {"pid": 103, "start_ticks": 3})
        return FakeDriver({
            "lifecycle_capabilities": {"source_schema_proven": True,
                                       "points": {name: True for name in cases.L14_POINTS}},
            "phase_guard": None,
            "lifecycle_prepare": {"host_launched": True, "reached": True, "live_pin": True,
                                  "retirement_inflight": False, "lock_holders": [anchor],
                                  "anchor_identity": anchor, "daemon_identity": daemon,
                                  "vendor_identity": vendor, "info_pid": 103, "record_pid": 103},
            "daemon_sigstop": {"persisted": True, "all_threads_stopped": True,
                               "stdin_writer_open": stdin},
            "anchor_sigkill": {"pid_only": True, "monotonic": 1.0},
            "vendor_death": {"gone": gone, "elapsed_seconds": 0.25, "certain": certain,
                             "daemon_stayed_stopped": True},
            "daemon_sigcont": {"identity_verified": True},
            "lifecycle_successor": {"admitted": True, "parallel_servers": 0,
                                    "password_changed": True},
            "limitation": None,
        })

    def test_l14_positive_all_life_points_and_finally_resume(self):
        driver = self.anchor_driver()
        result = cases.run_case(driver, "anchor", cases.case_anchor)
        self.assertEqual(result["result"], "pass")
        self.assertEqual(sum(name == "daemon_sigcont" for name, _ in driver.calls), 16)
        self.assertTrue(all(args["group"] is False for name, args in driver.calls
                            if name == "anchor_sigkill"))
        self.assertEqual(next(args["point"] for name, args in driver.calls if name == "lifecycle_prepare"),
                         "long_run")

    def test_l14_conditional_lsp_and_optional_skips_are_labelled(self):
        driver = self.anchor_driver()
        capabilities = driver.replies["lifecycle_capabilities"]
        for point in ("lsp_during", "lsp_after", "reload", "disposal"):
            capabilities["points"][point] = False
        capabilities["lsp_probe"] = self.lsp_proof()
        capabilities["notes"] = {"reload": "reload: not offered by served schema",
                                 "disposal": "disposal: not offered by served schema"}
        capabilities["optional_offered"] = {"reload": False, "disposal": False}
        result = cases.run_case(driver, "anchor", cases.case_anchor)
        self.assertEqual(result["result"], "pass")
        self.assertEqual(sum(name == "daemon_sigcont" for name, _ in driver.calls), 12)
        self.assertIn("lsp: not offered by pinned 2.0.22 (read trigger; 5 s readiness window)", result["limitations"])
        self.assertTrue(any("reload:" in note for note in result["limitations"]))
        self.assertTrue(any("disposal:" in note for note in result["limitations"]))

    def test_l14_offered_but_unsampled_disposal_cannot_pass(self):
        driver = self.anchor_driver()
        offered = driver.replies["lifecycle_capabilities"]
        offered["points"]["disposal"] = False
        offered["notes"] = {"disposal": "disposal: deferred; no reviewed held-pin recipe"}
        offered["optional_offered"] = {"disposal": True}
        result = cases.run_case(driver, "anchor", cases.case_anchor)
        self.assertEqual(result["result"], "not_observable")
        self.assertTrue(any("disposal: deferred" in note for note in result["limitations"]))

    def test_l14_survivor_fails_and_daemon_resumes(self):
        driver = self.anchor_driver(gone=False)
        result = cases.run_case(driver, "anchor", cases.case_anchor)
        self.assertEqual(result["result"], "fail")
        self.assertEqual(driver.calls[-1][0], "daemon_sigcont")
        self.assertFalse(any(name == "lifecycle_successor" for name, _ in driver.calls))

    def test_l14_uncertain_death_never_passes(self):
        driver = self.anchor_driver(certain=False)
        result = cases.run_case(driver, "anchor", cases.case_anchor)
        self.assertEqual(result["result"], "not_observable")
        self.assertEqual(driver.calls[-1][0], "daemon_sigcont")

    def test_l14_stdin_proof_precedes_anchor_kill(self):
        driver = self.anchor_driver(stdin=False)
        result = cases.run_case(driver, "anchor", cases.case_anchor)
        self.assertEqual(result["result"], "fail")
        self.assertFalse(any(name == "anchor_sigkill" for name, _ in driver.calls))
        self.assertEqual(driver.calls[-1][0], "daemon_sigcont")

    def test_l14_stop_proof_error_still_resumes_verified_identity(self):
        driver = self.anchor_driver()
        def unreadable(_args):
            raise cases.EvidenceUnavailable("stop proof unreadable after signal")
        driver.replies["daemon_sigstop"] = unreadable
        result = cases.run_case(driver, "anchor", cases.case_anchor)
        self.assertEqual(result["result"], "not_observable")
        self.assertEqual(driver.calls[-1][0], "daemon_sigcont")

    def test_l14_wrong_lock_holder_prevents_signals(self):
        driver = self.anchor_driver()
        driver.replies["lifecycle_prepare"]["lock_holders"].append({"pid": 103, "start_ticks": 3})
        result = cases.run_case(driver, "anchor", cases.case_anchor)
        self.assertEqual(result["result"], "fail")
        self.assertFalse(any(name.endswith("sigstop") for name, _ in driver.calls))


class ProviderTests(unittest.TestCase):
    def test_bootstrap_hold_withholds_headers_until_release_and_stops_on_abort(self):
        for action in ('release','abort'):
            with self.subTest(action=action):
                hold=cases.ResponseHold(time.monotonic()+5)
                replies=[]
                with cases.LoopbackProvider(response_hold=hold) as provider:
                    def request():
                        try: replies.append(self.request(provider)[0])
                        except OSError: replies.append('closed')
                    thread=threading.Thread(target=request)
                    thread.start()
                    try:
                        deadline=time.monotonic()+2
                        while not provider.received and time.monotonic()<deadline: time.sleep(.01)
                        self.assertTrue(provider.received)
                        self.assertEqual(replies,[])
                        getattr(hold,action)()
                    finally:
                        if not hold.released: hold.abort()
                        thread.join(3)
                    self.assertFalse(thread.is_alive())
                    self.assertEqual(replies,[200 if action=='release' else 'closed'])
                    if action=='abort':
                        with self.assertRaisesRegex(cases.EvidenceUnavailable,'expired or aborted'):
                            hold.release()
                        self.assertFalse(hold.released)

    def test_bootstrap_rejected_identity_aborts_without_sending_even_error_headers(self):
        hold=cases.ResponseHold(time.monotonic()+5)
        with cases.LoopbackProvider(response_hold=hold) as provider:
            with self.assertRaises(OSError): self.request(provider,model='paid-model')
            self.assertTrue(hold.aborted)
            self.assertFalse(hold.released)

    def test_bootstrap_provider_shutdown_wakes_held_non_daemon_handler(self):
        hold=cases.ResponseHold(time.monotonic()+30)
        provider=cases.LoopbackProvider(response_hold=hold).__enter__()
        def request():
            try: self.request(provider)
            except OSError: pass
        thread=threading.Thread(target=request); thread.start()
        try:
            deadline=time.monotonic()+2
            while not provider.received and time.monotonic()<deadline: time.sleep(.01)
            self.assertTrue(provider.received)
        finally:
            provider.__exit__(None,None,None)
            thread.join(3)
        self.assertTrue(hold.aborted)
        self.assertFalse(hold.released)
        self.assertFalse(thread.is_alive())
        self.assertFalse(any(thread.is_alive() for thread in provider._handlers))

    def test_fixture_read_uses_only_actual_offered_path_schema(self):
        for key in ("filePath", "path"):
            with self.subTest(key=key), cases.LoopbackProvider(
                    script=[{"fixture_read": "fixture.via_ocl_fixture"}]) as provider:
                tools = [{"type": "function", "function": {"name": "read", "parameters": {
                    "type": "object", "properties": {key: {"type": "string"}},
                    "required": [key], "additionalProperties": False}}}]
                status, body = self.request(provider, tools=tools)
                self.assertEqual(status, 200)
                call = json.loads(body)["choices"][0]["message"]["tool_calls"][0]
                self.assertEqual(json.loads(call["function"]["arguments"]),
                                 {key: "fixture.via_ocl_fixture"})
                self.assertEqual(provider.receipt()["read_calls"],
                                 [{"id": call["id"], "schema_checked": True}])

    def test_unoffered_read_or_unsupported_path_cannot_prove_probe(self):
        for name, key in (("shell", "command"), ("read", "filename")):
            with self.subTest(name=name), cases.LoopbackProvider(
                    script=[{"fixture_read": "fixture.via_ocl_fixture"}]) as provider:
                tools = [{"type": "function", "function": {"name": name, "parameters": {
                    "type": "object", "properties": {key: {"type": "string"}},
                    "required": [key]}}}]
                status, _ = self.request(provider, tools=tools)
                self.assertEqual(status, 400)
                self.assertEqual(provider.receipt()["read_calls"], [])

    def test_slow_headers_share_one_absolute_deadline(self):
        provider = cases.LoopbackProvider()
        provider.CONNECTION_SECONDS = .1
        with provider:
            client = socket.create_connection(("127.0.0.1", provider.server.server_port), timeout=1)
            try:
                client.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nX-Slow: ")
                for _ in range(8):
                    time.sleep(.025)
                    try:
                        client.sendall(b"a")
                    except OSError:
                        break
                self.assertEqual(client.recv(1), b"")
                self.assertFalse(provider.receipt()["received"])
            finally:
                client.close()

    def test_partial_body_shutdown_joins_every_owned_handler(self):
        before = set(threading.enumerate())
        provider = cases.LoopbackProvider()
        with provider:
            client = socket.create_connection(("127.0.0.1", provider.server.server_port), timeout=1)
            try:
                client.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nHost: loopback\r\n"
                               b"Content-Length: 1000\r\n\r\n{\"model\":")
                deadline = time.monotonic() + 1
                while len(set(threading.enumerate()) - before) < 2 and time.monotonic() < deadline:
                    time.sleep(.005)
                owned = set(threading.enumerate()) - before
                self.assertGreaterEqual(len(owned), 2)
                # Keep the peer open: closing only the listening socket cannot
                # release the handler's incomplete body read.
                provider.__exit__()
                self.assertTrue(all(not thread.is_alive() for thread in owned))
            finally:
                client.close()
        self.assertIsNone(provider.secret)

    def request(self, provider, model="fixture-free", stream=False, tools=None):
        conn = http.client.HTTPConnection("127.0.0.1", provider.server.server_port, timeout=3)
        value = {"model": model, "stream": stream}
        if tools is not None:
            value["tools"] = tools
        conn.request("POST", "/v1/chat/completions", json.dumps(value),
                     {"Content-Type": "application/json"})
        response = conn.getresponse()
        try:
            body = response.read()
        finally:
            conn.close()
        return response.status, body

    def test_nonforwarding_mock_receipt_and_identity(self):
        with cases.LoopbackProvider() as provider:
            status, body = self.request(provider)
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(body)["model"], "fixture-free")
            self.assertEqual(provider.receipt()["requests"], 1)
            self.assertTrue(provider.receipt()["received"])
            self.assertTrue(provider.receipt()["model_matches"])

    def test_aborted_bootstrap_retries_are_counted_separately_from_admission(self):
        hold=cases.ResponseHold(time.monotonic()+5)
        def admit(_model):
            if provider.requests>2: raise cases.safety.Blocked('mock model request ceiling exhausted')
        with cases.LoopbackProvider(response_hold=hold,admit_request=admit) as provider:
            hold.abort()
            for _ in range(5):
                with self.assertRaises(http.client.RemoteDisconnected):self.request(provider)
            receipt=provider.receipt()
            self.assertEqual(receipt.get('admitted_requests'),2)
            self.assertEqual(receipt.get('refused_requests'),3)
            self.assertEqual(receipt.get('unreleased_responses'),2)
            self.assertEqual(receipt['requests'],5)

    def test_mock_connection_ceiling_is_visible_in_receipt(self):
        with cases.LoopbackProvider() as provider:
            provider.TOTAL_CONNECTIONS=2
            self.request(provider);self.request(provider)
            with self.assertRaises((http.client.RemoteDisconnected,ConnectionResetError)):
                self.request(provider)
            self.assertTrue(provider.receipt().get('connection_limit_reached'))
            self.assertEqual(provider.receipt().get('connection_limit'),2)
            self.assertEqual(provider.receipt()['requests'],2)

    def test_paid_identity_preserves_spending_latch_with_accounting(self):
        with tempfile.TemporaryDirectory(prefix='via-oc-provider-') as root:
            driver=transport.Driver('release','fp','pin',Path(root)/'evidence')
            driver.set_phase_budget(0,8)
            with cases.LoopbackProvider(admit_request=driver._mock_request_admit) as provider:
                self.assertEqual(self.request(provider,model='paid-model')[0],403)
                self.assertTrue(driver.guard.stopped)
                self.assertEqual(provider.receipt()['refused_requests'],1)
                self.assertEqual(provider.receipt()['admitted_requests'],0)

    def test_paid_identity_mock_refused_locally(self):
        with cases.LoopbackProvider() as provider:
            self.assertEqual(self.request(provider, model="paid-model")[0], 403)
            self.assertFalse(provider.receipt()["model_matches"])

    def test_secret_error_and_malformed_provider_paths(self):
        for mode in ("error", "malformed", "oversized"):
            with cases.LoopbackProvider(mode=mode, secret="synthetic-only-secret") as provider:
                status, body = self.request(provider)
                self.assertIn(b"synthetic-only-secret", body)
                self.assertTrue(provider.receipt()["received"])
                if mode == "error":
                    self.assertEqual(status, 500)
                if mode == "oversized":
                    self.assertGreater(len(body), 1024 * 1024)
            self.assertIsNone(provider.secret)

    def test_truncated_provider_response(self):
        with cases.LoopbackProvider(mode="truncated") as provider:
            with self.assertRaises(http.client.IncompleteRead):
                self.request(provider)
            self.assertTrue(provider.receipt()["received"])

    def test_streaming_mock_usage_and_done(self):
        with cases.LoopbackProvider() as provider:
            status, body = self.request(provider, stream=True)
            self.assertEqual(status, 200)
            self.assertIn(b"cached_tokens", body)
            self.assertTrue(body.endswith(b"data: [DONE]\n\n"))

    def test_scripted_tool_call_requires_offered_schema(self):
        with cases.LoopbackProvider(script=[{"name": "bash", "arguments": {"command": "fixture"}}]) as provider:
            conn = http.client.HTTPConnection("127.0.0.1", provider.server.server_port, timeout=3)
            value = {"model": "fixture-free", "tools": [{"type": "function", "function": {
                "name": "bash", "parameters": {"required": ["command"],
                                                "properties": {"command": {"type": "string"}},
                                                "additionalProperties": False}}}]}
            conn.request("POST", "/v1/chat/completions", json.dumps(value))
            response = conn.getresponse()
            payload = json.loads(response.read())
            conn.close()
            self.assertEqual(payload["choices"][0]["finish_reason"], "tool_calls")
            self.assertEqual(payload["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "bash")
        with cases.LoopbackProvider(script=[{"name": "task", "arguments": {}}]) as provider:
            self.assertEqual(self.request(provider, tools=[{"function": {"name": "read", "parameters": {}}}])[0], 400)

    def test_auxiliary_title_does_not_consume_primary_tool_recipe(self):
        tools = [{"function": {"name": "bash", "parameters": {"required": ["command"]}}}]
        with cases.LoopbackProvider(script=[None, {"name": "bash", "arguments": {"command": "fixture"}}]) as provider:
            self.assertEqual(self.request(provider)[0], 200)
            self.assertEqual(provider.script_cursor, 0)
            self.assertEqual(self.request(provider, tools=tools)[0], 200)
            self.assertEqual(provider.script_cursor, 1)
            self.assertEqual(self.request(provider)[0], 200)
            status, body = self.request(provider, tools=tools)
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(body)["choices"][0]["finish_reason"], "tool_calls")

    def test_sentinel_markers_record_booleans_only(self):
        with cases.LoopbackProvider(markers={"local_seen": "LOCAL_SYNTHETIC"}) as provider:
            conn = http.client.HTTPConnection("127.0.0.1", provider.server.server_port, timeout=3)
            conn.request("POST", "/v1/chat/completions", json.dumps({"model": "fixture-free",
                         "messages": [{"role": "system", "content": "LOCAL_SYNTHETIC"}]}))
            response = conn.getresponse()
            response.read()
            conn.close()
            self.assertTrue(provider.receipt()["local_seen"])
            self.assertNotIn("LOCAL_SYNTHETIC", json.dumps(provider.receipt()))

    def test_shared_mock_admission_stop_returns_no_model_response(self):
        def exhausted(_model):
            raise cases.EvidenceUnavailable("mock request ceiling exhausted")
        with cases.LoopbackProvider(admit_request=exhausted) as provider:
            status, body = self.request(provider)
            self.assertEqual(status, 403)
            self.assertTrue(provider.receipt()["admission_blocked"])
            self.assertNotIn(b"choices", body)


class FakeCliDriver:
    """Checks the concrete argv contract; no real daemon or vendor is launched."""

    def __init__(self):
        self.commands = []
        self.kinds = []
        self.ancestor_stages = []

    def _check_ancestors(self, stage):
        # FAKE launch adapter; real discovery/mode checks are RunRootTests' boundary.
        self.ancestor_stages.append(stage)

    def build(self, kind):
        self.kinds.append(kind)

    def set_phase_budget(self, public_turns, mock_turns):
        self.budget = (public_turns, mock_turns)

    def start(self, kind, project):
        self.kinds.append(kind)

    def stop(self):
        pass

    def observe(self, name, target=None):
        if name == "interruption":
            return {"interrupted": False}
        if name == "turn_identity":
            return {"model": "oclive-mock/fixture-free", "mode": "mock",
                    "owned_terminal": True, "execution_id": "execution_owned",
                    "input_id": "input_owned"}
        if name == "cancel_timing":
            return {"reserved_pool": True, "served_while_held": True}
        raise cases.EvidenceUnavailable("fake raw observation unavailable")

    def via(self, args):
        self.commands.append(args)
        verb = args[0]
        if verb == "spawn":
            assert "--background" in args
            assert "--bound" in args and "--network" in args
            return {"session_id": "s1", "turn": "s1/1", "handle": "memory-only"}
        if verb == "resume":
            assert "--background" not in args
            assert args[1] == "s1"
            return {"turn": "s1/2"}
        if verb == "wait":
            assert args[1] in {"s1/1", "s1/2"}
            return {"state": "completed", "vendor_session_id": "ses_owned",
                    "final_text": "MOCK READY"}
        if verb == "cancel":
            assert args == ["cancel", "s1", "--turn", "2", "--wait", "--json"]
            return {"state": "cancelled", "cancel": {"outcome": "acknowledged",
                                                         "cleanup": "quiescent"}}
        raise AssertionError("unrecognized concrete CLI operation")


class AdapterTests(unittest.TestCase):
    def test_l4_adapter_invokes_all_output_commands_and_pages_events(self):
        class MatrixDriver(FakeCliDriver):
            def via(self, command):
                self.commands.append(command)
                verb = command[0]
                if verb == "events":
                    after = int(command[command.index("--after") + 1])
                    self.assert_bound = command[command.index("--limit") + 1] == "256"
                    return {"events": [{"seq": after + 1, "type": "turn.failed"}],
                            "more": after == 0, "next_after": after + 1}
                if verb in {"wait", "status", "models", "describe", "logs"}:
                    assert "--json" in command
                    return {}
                raise AssertionError("unexpected matrix command")
        raw = MatrixDriver()
        adapter = cases.DriverAdapter(raw)
        adapter.execute("phase_begin", seconds=60, public_turns=0, mock_turns=0, build="release")
        adapter.sessions["s1"] = {"session_id": "s1", "turn": "s1/2", "project": "fixture"}
        proof = adapter.execute("hostile_output_matrix", session="s1")
        self.assertEqual(proof["verbs"], list(cases.HOSTILE_OUTPUT_VERBS))
        self.assertEqual(proof["event_pages"], 2)
        self.assertEqual([command[0] for command in raw.commands],
                         ["wait", "status", "events", "events", "models", "describe", "logs"])
        self.assertTrue(raw.assert_bound)
        describe = next(command for command in raw.commands if command[0] == "describe")
        self.assertEqual(describe[describe.index("--cwd") + 1], "fixture")

    def test_l4_event_output_without_pagination_progress_is_unverifiable(self):
        class MatrixDriver(FakeCliDriver):
            def via(self, command):
                self.commands.append(command)
                if command[0] == "events":
                    return {"events": [], "more": True, "next_after": 0}
                return {}
        raw = MatrixDriver()
        adapter = cases.DriverAdapter(raw)
        adapter.execute("phase_begin", seconds=60, public_turns=0, mock_turns=0, build="release")
        adapter.sessions["s1"] = {"session_id": "s1", "turn": "s1/2", "project": "fixture"}
        with self.assertRaises(cases.EvidenceUnavailable):
            adapter.execute("hostile_output_matrix", session="s1")
        self.assertNotIn("logs", [command[0] for command in raw.commands])

    def test_release_preserves_original_owned_occurrence(self):
        class SeamDriver:
            def seam_release(self, name, occurrence=None):
                self.released = (name, occurrence)
        raw = SeamDriver()
        adapter = cases.DriverAdapter(raw)
        adapter.execute("seam_release", point="identity", occurrence=1)
        self.assertEqual(raw.released, ("identity", 1))
    def test_spawn_resume_cancel_use_actual_cli_shapes(self):
        raw = FakeCliDriver()
        adapter = cases.DriverAdapter(raw)
        adapter.execute("phase_begin", seconds=60, public_turns=0, mock_turns=2, build="release")
        first = adapter.execute("turn", project="fixture", prompt="first", mode="mock")
        self.assertEqual(first["session_id"], "s1")
        adapter.execute("turn", project="fixture", prompt="second", mode="mock", session="s1")
        cancel = adapter.execute("cancel", session="s1")
        self.assertEqual(cancel["outcome"], "acknowledged")
        self.assertEqual(cancel["cleanup"], "quiescent")

    def test_admission_ceiling_before_cli(self):
        raw = FakeCliDriver()
        adapter = cases.DriverAdapter(raw)
        adapter.execute("phase_begin", seconds=60, public_turns=0, mock_turns=0, build="release")
        with self.assertRaises(cases.EvidenceUnavailable):
            adapter.execute("turn", project="fixture", prompt="forbidden", mode="mock")
        self.assertEqual(raw.commands, [])

    def test_release_switch_for_record_only_cases(self):
        raw = FakeCliDriver()
        adapter = cases.DriverAdapter(raw)
        adapter.execute("phase_begin", seconds=60, public_turns=0, mock_turns=2,
                        build="test-failpoints")
        adapter.execute("phase_build", build="release")
        self.assertEqual(raw.kinds, ["failpoints", "release"])


class ConfiguredLspDriverTests(unittest.TestCase):
    def test_l5_fixture_reads_custom_file_only_after_positive_probe(self):
        with tempfile.TemporaryDirectory() as temporary:
            project = Path(temporary)
            for spawned in (False, True):
                driver = transport.Driver.__new__(transport.Driver)
                driver._lsp_result = {"spawned": spawned}
                driver.mock_providers = {}
                driver.mock_origins = set()
                driver.guard = SimpleNamespace(mock_origins=frozenset())
                driver._mock_request_admit = mock.Mock()
                driver._helper_fixture = mock.Mock(return_value=project / "lsp-helper.py")
                provider = SimpleNamespace(script=[], endpoint="http://127.0.0.1:12345/v1",
                                           __enter__=mock.Mock())
                with mock.patch.object(cases, "LoopbackProvider", return_value=provider):
                    config = driver._materialize_fixture("never-ask", project, {
                        "provider": "mock", "fake_lsp": True, "subagent": "oclive-ask"})
                self.assertEqual("fixture_read" in provider.script[0], spawned)
                if spawned:
                    self.assertEqual(provider.script[0], {"fixture_read": "fixture.via_ocl_fixture"})
                self.assertEqual(config["lsp"]["via-fixture"]["extensions"], [".via_ocl_fixture"])

    def fixture_driver(self, root, *, spawned=True, actual_read=True, cleanup_error=False):
        driver = transport.Driver.__new__(transport.Driver)
        driver.daemon_generations=[]
        driver.server_identities=set(); driver.helper_ledger={}
        project = root / "probe"
        project.mkdir()
        folder = root / "helpers"
        folder.mkdir()
        if spawned:
            (folder / "lsp.json").write_text("{}")
        expected = {"command": ["fixture-helper"], "extensions": [".via_ocl_fixture"],
                    "disabled": False}
        driver.project = root / "retained"
        driver.last_request = {"session_id": "retained-session"}
        driver.provider_endpoints = {"retained": "loopback"}
        driver.last_model = "retained-model"
        driver.fixtures = {"lsp-probe": {"config": {"lsp": {"via-fixture": expected}}}}
        driver.mock_providers = {"lsp-probe": SimpleNamespace(receipt=lambda: {
            "received": True, "model_matches": True,
            "read_calls": [{"id": "read-1", "schema_checked": True}]})}
        driver.phase_deadline = None
        driver.daemon = None; driver.anchor = None
        driver._bootstrap_active = False; driver._bootstrap_deadline = None
        driver.signals = None
        driver.proc = SimpleNamespace(verify=mock.Mock())
        driver.vendor_identity = object()
        driver.inventory = SimpleNamespace(check=mock.Mock())
        driver.fixture = mock.Mock(return_value=project)
        driver.start = mock.Mock()
        driver.ensure_vendor = mock.Mock()
        driver._served_schema = mock.Mock(return_value={"components": {"schemas": {
            "Config.InfoEncoded": {"properties": {"lsp": {}}}}}})
        driver.vendor = mock.Mock(return_value={"status": 200, "body": {"data": [
            {"type": "document", "info": {"lsp": {"via-fixture": expected}}}]}})
        driver._vendor_sid = mock.Mock(return_value="vendor-probe")
        events = ([{"type": "session.tool.called", "data": {"id": "read-1", "name": "read"}},
                   {"type": "session.tool.success", "data": {"id": "read-1"}}]
                  if actual_read else [])
        driver.observe = mock.Mock(side_effect=lambda kind, *_: {
            "events": events} if kind == "native_events" else {
                "helpers": [{"kind": "lsp", "ready": True}]})
        driver._helper_folder = mock.Mock(return_value=folder)
        driver._record = mock.Mock()
        def operation(name, _):
            if name == "helper_release" and cleanup_error:
                raise transport.Blocked("fake cleanup unavailable")
            return {"owned": True, "started": True}
        driver._operation = mock.Mock(side_effect=operation)
        driver.via = mock.Mock(side_effect=lambda args: {
            "turn": "probe:1", "session_id": "probe"} if args[0] == "spawn" else {})
        return driver

    def test_actual_read_positive_probe_is_cached_and_restores_retained_request(self):
        with tempfile.TemporaryDirectory() as temporary:
            driver = self.fixture_driver(Path(temporary))
            result = driver._lsp_probe()
            self.assertTrue(result["spawned"])
            self.assertEqual(result["disposition"], "offered")
            self.assertEqual(driver.last_request, {"session_id": "retained-session"})
            self.assertEqual(driver._lsp_probe(), result)
            self.assertEqual(driver.fixture.call_count, 1)
            self.assertEqual(driver.via.call_args_list[-1].args[0][0], "wait")

    def test_actual_read_negative_probe_observes_full_bounded_window(self):
        with tempfile.TemporaryDirectory() as temporary:
            driver = self.fixture_driver(Path(temporary), spawned=False)
            clock = [0.0]
            with mock.patch.object(transport.time, "monotonic", side_effect=lambda: clock[0]), \
                 mock.patch.object(transport.time, "sleep", side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)):
                result = driver._lsp_probe()
            self.assertFalse(result["spawned"])
            self.assertEqual(result["readiness_seconds"], 5)
            self.assertGreaterEqual(result["readiness_elapsed_seconds"], 5)
            self.assertEqual(result["disposition"], "lsp: not offered by pinned 2.0.22 (read trigger; 5 s readiness window)")

    def test_missing_actual_read_blocks_without_negative_or_retry(self):
        with tempfile.TemporaryDirectory() as temporary:
            driver = self.fixture_driver(Path(temporary), actual_read=False)
            clock = [0.0]
            with mock.patch.object(transport.time, 'monotonic', side_effect=lambda: clock[0]), \
                 mock.patch.object(transport.time, 'sleep', side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)), \
                 self.assertRaisesRegex(transport.Blocked, "not actually completed"):
                driver._lsp_probe()
            self.assertFalse(hasattr(driver, "_lsp_result"))
            with self.assertRaisesRegex(transport.Blocked, "single LSP configured-read probe was incomplete"):
                driver._lsp_probe()
            self.assertEqual(driver.fixture.call_count, 1)

    def test_probe_cleanup_failure_restores_retained_request_and_never_caches_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            driver = self.fixture_driver(Path(temporary), cleanup_error=True)
            with self.assertRaisesRegex(transport.Blocked, "fake cleanup unavailable"):
                driver._lsp_probe()
            self.assertEqual(driver.last_request, {"session_id": "retained-session"})
            self.assertFalse(hasattr(driver, "_lsp_result"))

    def test_lifecycle_capabilities_require_served_operations_and_helper_evidence(self):
        driver = transport.Driver.__new__(transport.Driver)
        driver.ensure_vendor = mock.Mock()
        driver._served_schema = mock.Mock(return_value={"paths": {}, "components": {}})
        driver._lsp_probe = mock.Mock(return_value={"spawned": False, "pin_owned": True,
                                                   "mock_received": True})
        driver.helper_ledger = {}
        proof = driver._lifecycle_capabilities()
        self.assertTrue(all(value is False for value in proof["points"].values()))
        self.assertFalse(proof["optional_offered"]["reload"])
        self.assertIn("not offered", proof["notes"]["disposal"])


def self_test():
    suite = unittest.defaultTestLoader.loadTestsFromModule(__import__(__name__))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return result.wasSuccessful()


if __name__ == "__main__":
    raise SystemExit(0 if self_test() else 1)
