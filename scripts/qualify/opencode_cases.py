#!/usr/bin/env python3
"""Bounded OpenCode qualification cases (vendor packet §§7–13).

This module admits no requests itself. Every driver model operation must pass
the runner's structural spending guard. Only sanitized checks are returned;
native transcripts, generated secrets and bearer handles stay in memory.
"""

from __future__ import annotations

import contextlib
import http.server
import json
import math
import secrets
import socket
import threading
import time
import urllib.parse
from dataclasses import dataclass, field
from typing import Any, Callable, Protocol


class EvidenceUnavailable(Exception):
    """A required observation cannot establish a packet predicate (§13)."""


class QualificationFailure(Exception):
    """Observable evidence contradicts a packet predicate (§13)."""


class CaseDriver(Protocol):
    """Private operations supplied by the supervised runner (packet §13)."""

    def execute(self, operation: str, **arguments: Any) -> Any: ...


class DriverAdapter:
    """Bind case operations to the concrete private driver (packet §13).

    Specialized observations are requested by name, never defaulted to success.
    The transport driver must derive each from owned raw evidence or block.
    """

    def __init__(self, driver):
        self.driver = driver
        self.sessions = {}
        self.projects = {}
        self.end = None
        self.public_left = 0
        self.mock_left = 0
        self.kind = "release"

    def execute(self, operation: str, **args):
        if operation == "phase_begin":
            self.end = time.monotonic() + args["seconds"]
            self.driver.phase_deadline = self.end
            self.public_left, self.mock_left = args["public_turns"], args["mock_turns"]
            self.driver.set_phase_budget(args["public_turns"], args["mock_turns"])
            self.kind = "failpoints" if args["build"] == "test-failpoints" else "release"
            self.driver.build(self.kind)
            return None
        if operation == "phase_guard":
            if self.end is None or time.monotonic() >= self.end:
                raise EvidenceUnavailable("absolute phase deadline exhausted")
            observation = self.driver.observe("interruption")
            if member(observation, "interrupted", bool):
                raise EvidenceUnavailable("deferred interruption stops admission")
            return observation
        if operation == "phase_deadline":
            if self.end is None:
                raise EvidenceUnavailable("phase deadline unavailable")
            return {"monotonic": self.end}
        if operation == "phase_build":
            self.kind = "failpoints" if args["build"] == "test-failpoints" else "release"
            self.driver.stop()
            self.driver.build(self.kind)
            return None
        if operation == "fixture":
            project = self.driver.fixture(args["name"], args["config"])
            self.projects[str(project)] = args["config"]
            return str(project)
        if operation in {"turn", "start_turn"}:
            self.execute("phase_guard")
            mode = args["mode"]
            if mode == "public-free":
                if self.public_left <= 0:
                    raise EvidenceUnavailable("public request ceiling exhausted")
                self.public_left -= 1
            elif mode == "mock":
                if self.mock_left <= 0:
                    raise EvidenceUnavailable("mock request ceiling exhausted")
                self.mock_left -= 1
            else:
                raise EvidenceUnavailable("unknown spending mode")
            project = args["project"]
            self.driver.start(self.kind, project)
            session = args.get("session")
            command = (["resume", session, "--json"] if session else
                       ["spawn", "--json", "--harness", "opencode", "--cwd", project,
                        "--model", "opencode/mimo-v2.6-flash-free" if mode == "public-free"
                        else "oclive-mock/fixture-free", "--bound", "full", "--network"])
            params = args.get("params", {})
            for key in ("effort", "max_steps"):
                if key in params:
                    command.extend(["--" + key.replace("_", "-"), str(params[key])])
            if session is None:
                command.append("--background")
            command.extend(["--prompt", args["prompt"]])
            receipt = self.driver.via(command)
            if session is None:
                session = member(receipt, "session_id", str)
            address = member(receipt, "turn", str)
            self.sessions[session] = {"session_id": session, "turn": address,
                                      "mode": mode, "project": project, "receipt": receipt}
            if operation == "start_turn":
                return self.sessions[session]
            envelope = self.driver.via(["wait", address, "--json", "--timeout-ms", "180000"])
            native = self.driver.observe("turn_identity", {"session": session,
                                                          "envelope": envelope})
            return {**self.sessions[session], "envelope": envelope, **native}
        if operation == "wait_turn":
            return self.driver.via(["wait", self.sessions[args["session"]]["turn"],
                                    "--json", "--timeout-ms", "180000"])
        if operation == "cancel":
            session = args["session"]
            address = self.sessions[session]["turn"]
            try:
                number = int(address.rsplit("/", 1)[1])
            except (ValueError, IndexError) as error:
                raise EvidenceUnavailable("malformed turn address") from error
            reply = self.driver.via(["cancel", session, "--turn", str(number),
                                     "--wait", "--json"])
            # Reserved-pool progress is observed independently in seam cases.
            return {**member(reply, "cancel", dict),
                    **self.driver.observe("cancel_timing", args["session"])}
        if operation == "cancel_nowait":
            session = args["session"]
            number = member(args, "turn_number", int)
            return self.driver.via(["cancel", session, "--turn", str(number), "--json"])
        if operation == "close":
            return self.driver.via(["close", args["session"], "--json"])
        if operation == "via_logs":
            return self.driver.via(["logs", self.sessions[args["session"]]["turn"], "--json"])
        if operation == "hostile_output_matrix":
            owned = self.sessions[member(args, "session", str)]
            session, address = owned["session_id"], owned["turn"]
            verbs = []
            commands = (
                ["wait", address, "--json", "--timeout-ms", "180000"],
                ["status", session, "--json"],
            )
            for command in commands:
                self.execute("phase_guard")
                self.driver.via(command)  # Strict parse and raw secrecy scan precede every return.
                verbs.append(command[0])
            after, pages = 0, 0
            while pages < HOSTILE_OUTPUT_PAGES:
                self.execute("phase_guard")
                page = self.driver.via(["events", address, "--after", str(after),
                                        "--limit", "256", "--json"])
                rows = member(page, "events", list)
                if len(rows) > 256:
                    raise EvidenceUnavailable("hostile events page exceeded the requested bound")
                next_after = member(page, "next_after", int)
                more = member(page, "more", bool)
                if next_after < after or (more and (not rows or next_after <= after)):
                    raise EvidenceUnavailable("hostile event pagination made no bounded progress")
                pages += 1
                if not more:
                    break
                after = next_after
            else:
                raise EvidenceUnavailable("hostile event output page ceiling reached")
            verbs.append("events")
            commands = (
                ["models", "--harness", "opencode", "--json"],
                ["describe", "--harness", "opencode", "--model", "oclive-mock/fixture-free",
                 "--bound", "full", "--network", "--cwd", owned["project"], "--json"],
                ["logs", address, "--json"],
            )
            for command in commands:
                self.execute("phase_guard")
                self.driver.via(command)
                verbs.append(command[0])
            return {"complete": True, "verbs": verbs, "event_pages": pages}
        if operation == "history":
            sid = require(args["session"], str, "vendor session id")
            reply = self.driver.vendor("GET", "/api/session/" + urllib.parse.quote(sid, safe=""))
            return {"status": reply["status"], **member(reply["body"], "data", dict)}
        if operation == "native_events":
            raw = self.driver.observe("native_events", args["session"])
            normalized = []
            for event in member(raw, "events", list):
                data = member(event, "data", dict)
                sid = data.get("sessionID")
                if sid is None and type(data.get("form")) is dict:
                    sid = data["form"].get("sessionID")
                if sid is None:
                    # Unknown non-session notifications cannot contribute to an
                    # owned ledger. Keep native order without inventing routing.
                    continue
                require(sid, str, "native sessionID")
                normalized.append({**event, "sessionID": sid,
                                   "id": event.get("id", event.get("vendor_event_id"))})
            return {"complete": member(raw, "complete", bool), "events": normalized}
        if operation == "owned_transcript":
            return self.driver.observe("owned_transcript", args)
        if operation == "seam_arm":
            context = args.get("context")
            if context is not None:
                session = member(context, "session", str)
                self.driver.start(self.kind, self.sessions[session]["project"])
                self.driver.ensure_vendor()
                target = self.driver.observe("seam_target", {"point": args["point"], **context})
                return self.driver.seam(args["point"], args["occurrence"], args["action"], target=target)
            return self.driver.seam(args["point"], args["occurrence"], args["action"])
        if operation == "seam_wait":
            return self.driver.seam_wait(args["point"])
        if operation == "seam_release":
            return self.driver.seam_release(args["point"], occurrence=args.get("occurrence"))
        if operation == "lifecycle_prepare":
            return self.driver.lifecycle(args["point"])
        if operation == "direct_seed":
            return self.driver.direct_seed(args["namespace"], {"schema_checked": True})
        if operation in {"daemon_sigstop", "anchor_sigkill", "vendor_death", "daemon_sigcont"}:
            return self.driver.signal_barrier({"operation": operation, **args})
        if operation == "limitation":
            return None  # Recorded by the case result, not provider evidence.
        if operation == "build_hashes":
            hashes = self.driver.observe("build_hashes")
            return {"release": member(hashes, "release", str),
                    "test-failpoints": member(hashes, "failpoints", str)}
        if operation == "configuration_states":
            raw = self.driver.observe(operation, args["session"])
            inherited = member(raw, "inherit", dict)
            allowed = {"agents", "hooks", "instruction_files", "mcp_servers", "plugins", "skills"}
            if set(inherited) != allowed:
                raise EvidenceUnavailable("configuration category names disagree with C1")
            return {"categories": [{"category": name, "effective": require(value, str, name)}
                                    for name, value in inherited.items()],
                    "warnings": member(raw, "warnings", list)}
        if operation == "owned_server_identity":
            raw = self.driver.vendor("GET", "/api/info")
            if member(raw, "status", int) != 200:
                raise EvidenceUnavailable("owned server info unavailable")
            info = member(raw, "body", dict)
            if member(info, "version", str) != "2.0.22":
                raise QualificationFailure("checked vendor info version changed")
            return {"pid": member(info, "pid", int)}
        if operation == "permission_observations":
            return permission_attempts(self.driver.observe(operation, args["session"]))
        if operation == "helper_observations":
            raw = self.driver.observe(operation)
            if "helpers" not in raw:
                return raw
            rows = member(raw, "helpers", list)
            kinds = {member(row, "kind", str) for row in rows}
            inventory = self.driver.observe("inventory")
            clean = member(inventory, "complete", bool) and member(inventory, "unchanged", bool)
            return {"mcp_started": "mcp" in kinds, "plugin_started": "plugin" in kinds,
                    "hook_started": "hook" in kinds, "fake_lsp_started": "lsp" in kinds,
                    "package_inventory_clean": clean, "binary_inventory_clean": clean}
        if operation == "secrecy_scan":
            return self.driver.observe(operation, args)
        return self.driver.observe(operation, args or None)


@dataclass(frozen=True)
class Phase:
    name: str
    seconds: int
    public_turns: int
    mock_turns: int
    build: str


PHASES = (
    Phase("preflight", 15 * 60, 0, 6, "release"),
    Phase("free", 40 * 60, 12, 8, "release"),
    Phase("ledger", 15 * 60, 0, 32, "release"),
    Phase("isolation", 15 * 60, 0, 32, "release"),
    Phase("hostile", 15 * 60, 0, 32, "release"),
    Phase("helpers", 15 * 60, 6, 32, "release"),
    Phase("seams", 45 * 60, 12, 32, "test-failpoints"),
    Phase("long_run", 20 * 60, 0, 32, "release"),
    Phase("anchor", 120 * 60, 24, 32, "release"),
    Phase("credentials", 10 * 60, 0, 0, "release"),
)


@dataclass
class Case:
    name: str
    disposition: str = "gate"
    checks: list[dict[str, Any]] = field(default_factory=list)
    result: str | None = None
    reason: str | None = None
    limitations: list[str] = field(default_factory=list)

    def check(self, name: str, predicate: bool) -> None:
        if type(predicate) is not bool:
            raise EvidenceUnavailable("predicate is not a Boolean")
        self.checks.append({"check": name, "result": "pass" if predicate else "fail"})
        if not predicate:
            raise QualificationFailure(name)

    def finish(self) -> dict[str, Any]:
        if self.result is None:
            self.result = "pass" if self.checks else "not_observable"
            if not self.checks:
                self.reason = "no observable predicates"
        return {"case": self.name, "disposition": self.disposition,
                "result": self.result, "reason": self.reason, "checks": self.checks,
                "limitations": self.limitations}


def require(value: Any, kind: type, name: str) -> Any:
    if type(value) is not kind:
        raise EvidenceUnavailable(f"missing or mistyped {name}")
    return value


def member(value: Any, key: str, kind: type) -> Any:
    require(value, dict, "object")
    if key not in value:
        raise EvidenceUnavailable(f"missing {key}")
    return require(value[key], kind, key)


def run_case(driver: CaseDriver, name: str, function: Callable, disposition="gate") -> dict:
    case = Case(name, disposition)
    try:
        function(driver, case)
    except QualificationFailure as error:
        case.result, case.reason = "fail", str(error)
    except EvidenceUnavailable as error:
        case.result, case.reason = "not_observable", str(error)
    return case.finish()


def native_events(driver: CaseDriver, session: str) -> list:
    evidence = driver.execute("native_events", session=session)
    if member(evidence, "complete", bool) is not True:
        raise EvidenceUnavailable("native events incomplete")
    rows = member(evidence, "events", list)
    last = -1
    for row in rows:
        seq = member(row, "seq", int)
        if seq <= last:
            raise EvidenceUnavailable("native event sequence does not progress")
        last = seq
        member(row, "type", str)
        member(row, "sessionID", str)
        member(row, "data", dict)
    return rows


def turn(driver: CaseDriver, project: str, prompt: str, *, session=None,
         mode="mock", **params) -> dict:
    result = driver.execute("turn", project=project, prompt=prompt,
                            session=session, mode=mode, params=params)
    member(result, "session_id", str)
    envelope = member(result, "envelope", dict)
    member(envelope, "state", str)
    if "vendor_session_id" not in envelope or "final_text" not in envelope:
        raise EvidenceUnavailable("missing nullable envelope identity/text")
    for key in ("vendor_session_id", "final_text"):
        if envelope[key] is not None and type(envelope[key]) is not str:
            raise EvidenceUnavailable("mistyped nullable envelope " + key)
    return result


def completed(case: Case, result: dict, *, public=False) -> dict:
    envelope = member(result, "envelope", dict)
    case.check("completed terminal envelope", member(envelope, "state", str) == "completed")
    case.check("checked vendor version", member(envelope, "vendor_version", str) == "2.0.22")
    member(envelope, "vendor_session_id", str)
    member(envelope, "final_text", str)
    model = member(result, "model", str)
    if public:
        case.check("genuine public frozen free model", model == "opencode/mimo-v2.6-flash-free"
                   and member(result, "mode", str) == "public-free")
    else:
        case.check("labelled mock identity", member(result, "mode", str) == "mock"
                   and model in {"oclive-mock/fixture-free", "opencode/mimo-v2.6-flash-free"})
    cost = member(envelope, "cost", dict)
    if "usd" not in cost:
        raise EvidenceUnavailable("cost field absent")
    usd = cost["usd"]
    if usd is not None:
        if type(usd) not in (int, float) or not math.isfinite(usd):
            raise EvidenceUnavailable("invalid cost")
        case.check("reported cost is zero", usd == 0)
    case.check("owned native terminal", member(result, "owned_terminal", bool))
    return envelope


def fixture(driver: CaseDriver, name: str, **config) -> str:
    return require(driver.execute("fixture", name=name, config=config), str, "fixture project")


def case_preflight(driver: CaseDriver, case: Case) -> None:
    observation = driver.execute("fresh_max_steps", max_steps=1)
    case.check("fresh daemon max_steps unsupported", member(observation, "support", str)
               == "unsupported")
    case.check("non-null max_steps invalid_params", member(observation, "class", str)
               == "invalid_params")
    for name in ("vendor_launches", "version_probes", "session_creates", "prompt_submits"):
        case.check(f"no {name}", member(observation, name, int) == 0)
    case.check("launch observation began before request", member(observation, "watch_before", bool))
    sentinel = driver.execute("repository_sentinel")
    for key in ("project_boundary", "namespace_boundary", "ancestor_control_sensitive",
                "ancestor_skills_absent", "ancestor_agents_absent", "private_git_commit"):
        case.check(key, member(sentinel, key, bool))


def case_free(driver: CaseDriver, case: Case) -> None:
    project = fixture(driver, "public-free", provider="public-free")
    result = turn(driver, project, "Reply with exactly VIA FREE READY.", mode="public-free",
                  background=True)
    envelope = completed(case, result, public=True)
    case.check("nonempty final text", bool(envelope["final_text"].strip()))
    events = driver.execute("via_events", session=result["session_id"])
    case.check("events fully paginated", member(events, "complete", bool))
    case.check("events agree with envelope", member(events, "terminal_revision", int)
               == member(envelope, "revision", int))
    logs = driver.execute("via_logs", session=result["session_id"])
    if "transcript" not in require(logs, dict, "logs"):
        raise EvidenceUnavailable("logs transcript field missing")
    case.check("logs contain no raw vendor transcript", logs["transcript"] is None)
    driver.execute("close", session=result["session_id"])
    history = driver.execute("history", session=envelope["vendor_session_id"])
    case.check("history still exists", member(history, "status", int) == 200
               and member(history, "id", str) == envelope["vendor_session_id"])


def case_continuity(driver: CaseDriver, case: Case) -> None:
    project = fixture(driver, "continuity", provider="public-free")
    nonce = secrets.token_hex(16)
    first = turn(driver, project, "Remember this nonce only in conversation: " + nonce
                 + ". Never write it or send it to a tool. Reply READY.", mode="public-free")
    first_envelope = completed(case, first, public=True)
    first_native = native_events(driver, first_envelope["vendor_session_id"])
    first_transcript = owned_text_and_tools(first_native, first_envelope["vendor_session_id"],
                                            member(first, "input_id", str))
    case.check("nonce never passed to an earlier tool", first_transcript["tool_calls"] == [])
    session = first["session_id"]
    for barrier in ("idle_retirement", "daemon_restart"):
        transition = driver.execute(barrier, session=session)
        case.check(barrier + " stop proof", member(transition, "stop_proven", bool))
        recall = turn(driver, project, "Without any tool or file retrieval, recall the nonce.",
                      session=session, mode="public-free")
        envelope = completed(case, recall, public=True)
        case.check("exact vendor session continuity", envelope["vendor_session_id"]
                   == first_envelope["vendor_session_id"])
        case.check("nonce recalled", nonce in envelope["final_text"])
        transcript = owned_text_and_tools(native_events(driver, envelope["vendor_session_id"]),
                                         envelope["vendor_session_id"], member(recall, "input_id", str))
        case.check("complete recall evidence", transcript["complete"])
        case.check("final native text agrees", member(transcript, "final_text", str)
                   == envelope["final_text"])
        case.check("no recall tool or file retrieval", member(transcript, "tool_calls", list) == [])
        case.check("nonce absent from tool inputs because no owned turn called tools",
                   transcript["tool_calls"] == [] and first_transcript["tool_calls"] == [])


def owned_text_and_tools(events: list, session: str, input_id: str) -> dict:
    """Complete delivered-to-terminal no-retrieval proof (packet §§7.1,7.3,13).

    Every turn in the continuity recipe forbids tools, including the nonce
    introduction. Tool-input absence follows only when every interval is
    complete and has no tool activity; inaccessible transcripts are not claimed.
    """
    delivered, settled = False, False
    tools, texts, last_step = {}, {}, None
    for event in events:
        if member(event, "sessionID", str) != session:
            continue
        kind, data = member(event, "type", str), member(event, "data", dict)
        if kind == "session.inbox.delivered":
            if member(data, "inboxID", str) == input_id:
                if delivered:
                    raise EvidenceUnavailable("duplicate owned recall delivery")
                delivered = True
            elif delivered and not settled:
                raise EvidenceUnavailable("foreign input joined recall")
            continue
        if not delivered or settled:
            continue
        if kind in {"session.execution.succeeded", "session.execution.failed",
                    "session.execution.interrupted"}:
            settled = True
            if kind != "session.execution.succeeded":
                raise EvidenceUnavailable("recall execution did not naturally complete")
        elif kind.startswith("session.tool."):
            if kind not in {"session.tool.input.started", "session.tool.called", "session.tool.success",
                            "session.tool.failed", "session.tool.input.delta", "session.tool.input.ended"}:
                raise EvidenceUnavailable("unrecognized tool record in recall interval")
            key = member(data, "id", str)
            tools[key] = kind
        elif kind in {"session.step.started", "session.step.ended", "session.step.failed"}:
            last_step = member(data, "assistantMessageID", str)
        elif kind == "session.text.ended":
            assistant = member(data, "assistantMessageID", str)
            ordinal = member(data, "ordinal", int)
            text = member(data, "text", str)
            texts.setdefault(assistant, {})[ordinal] = text
    if not delivered or not settled or last_step is None or last_step not in texts:
        raise EvidenceUnavailable("recall interval or final native text incomplete")
    return {"complete": True, "tool_calls": sorted(tools),
            "final_text": "".join(text for _, text in sorted(texts[last_step].items()))}


def case_cancel(driver: CaseDriver, case: Case) -> None:
    project = fixture(driver, "tool-barrier", provider="public-free", helper="tool", separate_group=True)
    pending = driver.execute("start_turn", project=project, mode="public-free",
                             prompt="Run the local fixture helper until it is released.")
    session = member(pending, "session_id", str)
    try:
        active = driver.execute("helper_barrier", session=session, helper="tool")
        case.check("owned active tool barrier", member(active, "started", bool)
                   and member(active, "owned", bool))
        cancel = driver.execute("cancel", session=session)
        result = driver.execute("wait_turn", session=session)
        case.check("cancel acknowledged", member(cancel, "outcome", str) == "acknowledged")
        case.check("native reported-tool cleanup quiescent", member(cancel, "cleanup", str) == "quiescent")
        native = driver.execute("native_terminal", session=session)
        case.check("owned interrupted user", member(native, "type", str) == "session.execution.interrupted"
                   and member(native, "reason", str) == "user" and member(native, "owned", bool))
        case.check("cancelled envelope", member(result, "state", str) == "cancelled")
    finally:
        driver.execute("helper_release", helper="tool")
    successor = turn(driver, project, "Reply SUCCESSOR.", session=session, mode="public-free")
    completed(case, successor, public=True)


def usage_ledger(events: list, session: str, execution: str, input_id: str) -> dict:
    """Packet §12 final keyed samples, excluding child/cumulative traffic."""
    steps, compact = {}, {}
    delivered, settled = False, False
    for event in events:
        if member(event, "sessionID", str) != session:
            continue
        data = member(event, "data", dict)
        kind = member(event, "type", str)
        if kind == "session.inbox.delivered":
            if member(data, "inboxID", str) == input_id:
                if delivered:
                    raise EvidenceUnavailable("duplicate owned delivery")
                delivered = True
            elif delivered and not settled:
                raise EvidenceUnavailable("foreign delivery joins the owned usage interval")
            continue
        if not delivered or settled:
            continue
        if kind in {"session.execution.succeeded", "session.execution.failed",
                    "session.execution.interrupted"}:
            settled = True
            continue
        if kind not in {"session.step.ended", "session.step.failed",
                        "session.compaction.ended", "session.compaction.failed"}:
            continue
        # 2.0.22 carries assistant/inbox keys, not a universal executionID.
        # Ownership is the delivered-input-to-terminal interval (§7.1). If a
        # future vendor supplies an execution key it must agree, never replace
        # the mandatory interval evidence.
        if "executionID" in data and member(data, "executionID", str) != execution:
            raise EvidenceUnavailable("execution identity disagrees with owned interval")
        if kind.startswith("session.step."):
            key = member(data, "assistantMessageID", str)
            steps[key] = data
        else:
            key = data.get("inputID")
            if key is None:
                key = member(event, "id", str)
            elif type(key) is not str:
                raise EvidenceUnavailable("invalid compaction key")
            compact[key] = data
    if not delivered or not settled:
        raise EvidenceUnavailable("owned usage interval incomplete")
    if not steps:
        raise EvidenceUnavailable("no owned final assistant usage samples")
    total = {"input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
             "reasoning_tokens": 0, "cache_write_tokens": 0}
    cost, cost_available = 0.0, True
    for sample in (*steps.values(), *compact.values()):
        tokens = member(sample, "tokens", dict)
        cache = member(tokens, "cache", dict)
        for field, source, key in (("input_tokens", tokens, "input"),
                                   ("cached_input_tokens", cache, "read"),
                                   ("output_tokens", tokens, "output"),
                                   ("reasoning_tokens", tokens, "reasoning"),
                                   ("cache_write_tokens", cache, "write")):
            value = member(source, key, int)
            if value < 0:
                raise EvidenceUnavailable("negative token count")
            total[field] += value
        reported = sample.get("cost")
        if reported is None:
            cost_available = False
        elif type(reported) not in (int, float) or not math.isfinite(reported) or reported < 0:
            raise EvidenceUnavailable("invalid native cost sample")
        else:
            cost += reported
    return {"totals": total, "steps": len(steps), "compactions": len(compact),
            "scope": "vendor_interval" if compact else "turn",
            "cost": cost if cost_available else None}


def case_usage(driver: CaseDriver, case: Case) -> None:
    project = fixture(driver, "usage-cache", provider="mock", context=4096,
                      cache_read=7, cache_write=3)
    result = turn(driver, project, "Use the local tool then answer with its output.")
    envelope = completed(case, result)
    native = native_events(driver, envelope["vendor_session_id"])
    ledger = usage_ledger(native, envelope["vendor_session_id"],
                          member(result, "execution_id", str), member(result, "input_id", str))
    case.check("at least two owned assistant IDs", ledger["steps"] >= 2)
    usage = member(envelope, "usage", dict)
    for key in ("input_tokens", "cached_input_tokens", "output_tokens"):
        case.check(key + " equals final owned samples", member(usage, key, int)
                   == ledger["totals"][key])
    case.check("usage scope matches compaction disposition", member(usage, "scope", str)
               == ledger["scope"])
    cost = member(envelope, "cost", dict)
    case.check("cost scope matches final samples", member(cost, "scope", str) == ledger["scope"])
    if "usd" not in cost:
        raise EvidenceUnavailable("missing cost sample")
    case.check("cost equals final owned samples or stays unavailable", cost["usd"] == ledger["cost"])
    case.check("mock provider receipt", member(driver.execute("mock_receipt"), "received", bool))
    case.check("cache read/write sample observable", ledger["totals"]["cached_input_tokens"] > 0
               and ledger["totals"]["cache_write_tokens"] > 0)


def case_auth(driver: CaseDriver, case: Case) -> None:
    observation = driver.execute("auth_checks")
    case.check("owned listener and PID", member(observation, "pid_matches", bool))
    case.check("absent auth 401", member(observation, "absent_status", int) == 401)
    case.check("wrong auth 401", member(observation, "wrong_status", int) == 401)
    case.check("correct auth accepted", member(observation, "correct_status", int) == 200)
    for key in ("memory_only", "evidence_scan_complete", "evidence_password_absent",
                "tool_password_key_absent", "password_changed"):
        case.check(key, member(observation, key, bool))


def case_config(driver: CaseDriver, case: Case) -> None:
    expected_categories = {"agents", "hooks", "instruction_files", "mcp_servers", "plugins", "skills"}
    projects = [fixture(driver, name, provider="mock") for name in ("config-a", "config-b")]
    case.limitations.append("Changing inheritance uses a fresh private daemon because C1 spawn/resume "
                            "have no inheritance override and daemon config has no reload. "
                            "Cross-request server sharing remains OC02 fake proof.")
    for requested in ({}, {"skills": "off"}, {"instruction_files": "off"}, {"skills": "on"}):
        driver.execute("set_inherit", settings=requested)
        pid = None
        for project in projects:
            result = turn(driver, project, "Reply CONFIG.")
            completed(case, result)
            observation = driver.execute("configuration_states", session=result["session_id"])
            states = member(observation, "categories", list)
            case.check("all six configuration categories present", len(states) == 6)
            seen = set()
            for state in states:
                category = member(state, "category", str)
                if category not in expected_categories:
                    raise EvidenceUnavailable("unexpected configuration category")
                if category in seen:
                    raise EvidenceUnavailable("duplicate configuration category")
                seen.add(category)
                expected = "off" if category == "skills" and requested.get("skills") == "off" else "unknown"
                case.check(category + " truthful effective state", member(state, "effective", str) == expected)
            case.check("exact six category names", seen == expected_categories)
            warnings = member(observation, "warnings", list)
            unverified = [row for row in warnings if member(row, "code", str) == "config_switch_unverified"]
            case.check("one switch warning", len(unverified) == 1)
            categories = member(member(unverified[0], "data", dict), "categories", list)
            expected_warning = expected_categories - ({"skills"} if requested.get("skills") == "off" else set())
            case.check("warning lists exact unknown categories",
                       {member(row, "category", str) for row in categories} == expected_warning)
            identity = driver.execute("owned_server_identity")
            current = member(identity, "pid", int)
            if pid is not None:
                case.check("two locations with fixed inheritance share same server", current == pid)
            pid = current


HOSTILE_RESPONSES = ("error", "malformed", "oversized", "truncated")
HOSTILE_OUTPUT_PAGES = 64  # Packet §13 L4: bounded output secrecy matrix.
HOSTILE_OUTPUT_VERBS = ("wait", "status", "events", "models", "describe", "logs")


class LoopbackProvider:
    """Nonforwarding OpenAI-compatible fixture provider (packet §13 L4/L7).

    Request bodies and synthetic secrets are memory-only. Receipt metadata is
    Boolean/count/model identity only. Unexpected models fail locally.
    """

    BODY_BYTES = 2 * 1024 * 1024  # §9 admission plus bounded local fixture framing.
    HEADER_BYTES = 32 * 1024  # §13 bounded synthetic-provider transport.
    HEADER_LINES = 64  # §13 bounded synthetic-provider transport.
    ACTIVE_CONNECTIONS = 16  # §13 phases allow at most two active model turns.
    TOTAL_CONNECTIONS = 64  # §13 32 physical requests plus bounded invalid peers.
    CONNECTION_SECONDS = 5  # §13 every local observation has an absolute ceiling.
    STOP_SECONDS = 5  # §13 positive owned-thread stop proof.

    def __init__(self, mode="normal", model="fixture-free", secret=None,
                 cache_read=7, cache_write=3, text="MOCK READY", script=None, markers=None,
                 admit_request=None):
        if mode not in {"normal", *HOSTILE_RESPONSES, "rate_limit", "quota", "context"}:
            raise ValueError("unknown mock response mode")
        self.mode, self.model = mode, model
        self.secret = secret if secret is not None else secrets.token_hex(24)
        self.cache_read, self.cache_write, self.text = cache_read, cache_write, text
        self.script = [] if script is None else list(script)
        self.script_cursor = 0
        self.markers = {} if markers is None else dict(markers)
        self.observed_markers = {name: False for name in self.markers}
        self.models = set()
        self.admit_request = admit_request
        self.admission_blocked = False
        self.received, self.model_matches, self.requests = False, False, 0
        self.server = self.thread = None
        self._lock = threading.Lock()
        self._connections = set()
        self._handlers = []
        self._closing = False

    def _connection_done(self, request):
        with self._lock:
            self._connections.discard(request)

    def __enter__(self):
        provider = self

        class Reader:
            """Unbuffered reads share one absolute header/body deadline (§13)."""

            def __init__(self, source, connection):
                self.source, self.connection = source, connection
                self.deadline = time.monotonic() + provider.CONNECTION_SECONDS
                self.bytes_left = provider.HEADER_BYTES
                self.lines_left = provider.HEADER_LINES

            def _prepare(self, count):
                remaining = self.deadline - time.monotonic()
                if remaining <= 0 or count < 0 or count > self.bytes_left:
                    raise OSError("bounded fixture request exhausted")
                self.connection.settimeout(remaining)

            def readline(self, limit=-1):
                if self.lines_left <= 0:
                    raise OSError("bounded fixture headers exhausted")
                count = min(limit, self.bytes_left) if limit >= 0 else self.bytes_left
                chunks = []
                while len(chunks) < count:
                    # SocketIO.readline internally loops with a relative
                    # timeout; byte reads refresh the absolute deadline.
                    self._prepare(1)
                    byte = self.source.read(1)
                    self.bytes_left -= len(byte)
                    chunks.append(byte)
                    if not byte or byte == b"\n":
                        break
                result = b"".join(chunks)
                self.lines_left -= 1
                if not result.endswith(b"\n"):
                    raise OSError("incomplete fixture headers")
                return result

            def read(self, count):
                self._prepare(count)
                result = self.source.read(count)
                self.bytes_left -= len(result)
                return result

            def close(self):
                self.source.close()

        class Server(http.server.ThreadingHTTPServer):
            daemon_threads = False
            block_on_close = False  # Explicit absolute-deadline joins below.

            def process_request(self, request, address):
                with provider._lock:
                    if (provider._closing or len(provider._connections) >= provider.ACTIVE_CONNECTIONS
                            or len(provider._handlers) >= provider.TOTAL_CONNECTIONS):
                        self.shutdown_request(request)
                        return
                    provider._connections.add(request)
                    thread = threading.Thread(target=self.process_request_thread,
                                              args=(request, address), daemon=False)
                    provider._handlers.append(thread)
                    thread.start()

            def process_request_thread(self, request, address):
                try:
                    super().process_request_thread(request, address)
                finally:
                    provider._connection_done(request)

            def handle_error(self, *_args):
                pass  # Never log a raw handler exception or request.

        class Handler(http.server.BaseHTTPRequestHandler):
            rbufsize = 0

            def setup(self):
                super().setup()
                self.rfile = Reader(self.rfile, self.connection)

            def handle(self):
                try:
                    # One request per owned connection; no unbounded keep-alive.
                    self.handle_one_request()
                except OSError:
                    pass
                self.close_connection = True

            def log_message(self, *_args):
                pass  # Provider request paths/headers may contain synthetic keys.

            def do_POST(self):
                try:
                    length = int(self.headers.get("Content-Length", "-1"))
                    if not 0 <= length <= provider.BODY_BYTES:
                        self.send_error(413)
                        return
                    self.rfile.bytes_left = length
                    chunks = []
                    received = 0
                    while received < length:
                        chunk = self.rfile.read(length - received)
                        if not chunk:
                            break
                        chunks.append(chunk)
                        received += len(chunk)
                    raw = b"".join(chunks)
                    if len(raw) != length:
                        self.send_error(400)
                        return
                    value = json.loads(raw)
                    provider.received = True
                    provider.requests += 1
                    if type(value) is dict and type(value.get("model")) is str:
                        provider.models.add(value["model"])
                        if provider.admit_request is not None:
                            try:
                                provider.admit_request(value["model"])
                            except Exception:
                                # No exception string, request, or header reaches
                                # stderr/evidence. The owner's guard is sticky.
                                provider.admission_blocked = True
                                self.reply(403, b'{"error":{"type":"fixture.admission-stop"}}')
                                return
                    messages = json.dumps(value.get("messages", [])) if type(value) is dict else ""
                    for name, marker in provider.markers.items():
                        if marker in messages:
                            provider.observed_markers[name] = True
                    provider.model_matches = (type(value) is dict and
                                              value.get("model") == provider.model)
                    if not provider.model_matches:
                        self.send_error(403)
                        return
                    if provider.mode == "error":
                        self.reply(500, json.dumps({"error": {"message": provider.secret,
                                                               "type": "provider.error"}}).encode())
                    elif provider.mode == "malformed":
                        self.reply(200, (provider.secret + "{broken").encode())
                    elif provider.mode == "oversized":
                        self.reply(200, provider.secret.encode() + b"x" * (1024 * 1024 + 1))
                    elif provider.mode == "truncated":
                        body = provider.secret.encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(body) + 4096))
                        self.end_headers()
                        self.wfile.write(body)
                        self.wfile.flush()
                        self.close_connection = True
                    elif provider.mode in {"rate_limit", "quota", "context"}:
                        status = 429 if provider.mode == "rate_limit" else 400
                        self.reply(status, json.dumps({"error": {"type": provider.mode,
                                                                  "message": "fixture"}}).encode())
                    else:
                        # Auxiliary title/summary requests omit tools; they
                        # cannot consume a primary tool recipe. Their physical
                        # requests still consume the shared admission budget.
                        scripted = None
                        if value.get("tools") and provider.script_cursor < len(provider.script):
                            scripted = provider.script[provider.script_cursor]
                            provider.script_cursor += 1
                        tool_call = None
                        if scripted is not None:
                            available = {item["function"]["name"]: item["function"]
                                         for item in value.get("tools", [])
                                         if type(item) is dict and type(item.get("function")) is dict
                                         and type(item["function"].get("name")) is str}
                            if scripted["name"] not in available:
                                self.reply(400, b'{"error":{"type":"fixture.tool-unavailable"}}')
                                return
                            parameters = available[scripted["name"]].get("parameters", {})
                            arguments = scripted["arguments"]
                            required = parameters.get("required", [])
                            if not all(key in arguments for key in required):
                                self.reply(400, b'{"error":{"type":"fixture.tool-schema"}}')
                                return
                            if parameters.get("additionalProperties") is False and any(
                                    key not in parameters.get("properties", {}) for key in arguments):
                                self.reply(400, b'{"error":{"type":"fixture.tool-schema"}}')
                                return
                            tool_call = {"id": "call_fixture_" + str(provider.requests),
                                         "type": "function", "function": {"name": scripted["name"],
                                              "arguments": json.dumps(arguments)}}
                        result = {"id": "chatcmpl_fixture", "object": "chat.completion",
                                  "created": 1, "model": provider.model,
                                  "choices": [{"index": 0, "message": {"role": "assistant",
                                                                             "content": provider.text},
                                               "finish_reason": "stop"}],
                                  "usage": {"prompt_tokens": 20 + provider.cache_read,
                                            "completion_tokens": 5,
                                            "total_tokens": 25 + provider.cache_read,
                                            "prompt_tokens_details": {"cached_tokens": provider.cache_read,
                                                                      "cache_write_tokens": provider.cache_write},
                                            "completion_tokens_details": {"reasoning_tokens": 0}}}
                        if tool_call is not None:
                            result["choices"][0]["message"]["tool_calls"] = [tool_call]
                            result["choices"][0]["finish_reason"] = "tool_calls"
                        if value.get("stream") is True:
                            chunks = []
                            delta = {"role": "assistant", "content": provider.text}
                            if tool_call is not None:
                                delta["tool_calls"] = [{"index": 0, **tool_call}]
                            for delta, finish in ((delta, None),
                                                  ({}, "tool_calls" if tool_call else "stop")):
                                chunks.append({"id": "chatcmpl_fixture", "object": "chat.completion.chunk",
                                               "created": 1, "model": provider.model,
                                               "choices": [{"index": 0, "delta": delta,
                                                            "finish_reason": finish}]})
                            chunks[-1]["usage"] = result["usage"]
                            body = b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n"
                                            for chunk in chunks) + b"data: [DONE]\n\n"
                            self.reply(200, body, "text/event-stream")
                        else:
                            self.reply(200, json.dumps(result).encode())
                except (ValueError, OSError, TypeError):
                    self.close_connection = True

            def reply(self, status, body, content_type="application/json"):
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = Server(("127.0.0.1", 0), Handler)
        self.server.timeout = 1
        self.thread = threading.Thread(target=self.server.serve_forever,
                                       kwargs={"poll_interval": .05}, daemon=False)
        self.thread.start()
        return self

    @property
    def endpoint(self):
        if self.server is None:
            raise EvidenceUnavailable("mock provider not started")
        return f"http://127.0.0.1:{self.server.server_port}/v1"

    def receipt(self):
        return {"received": self.received, "model_matches": self.model_matches,
                "requests": self.requests, "mode": "mock", "models": sorted(self.models),
                "admission_blocked": self.admission_blocked, **self.observed_markers}

    def __exit__(self, *_args):
        if self._closing:
            return
        deadline = time.monotonic() + self.STOP_SECONDS
        try:
            with self._lock:
                self._closing = True
                connections = list(self._connections)
            for connection in connections:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                connection.close()
            self.server.shutdown()
            self.server.server_close()
            for thread in [self.thread, *self._handlers]:
                thread.join(timeout=max(0, deadline - time.monotonic()))
            if any(thread.is_alive() for thread in [self.thread, *self._handlers]):
                raise EvidenceUnavailable("mock provider owned thread did not stop")
            with self._lock:
                if self._connections:
                    raise EvidenceUnavailable("mock provider connection stop unverifiable")
        finally:
            self.secret = None
            self.markers.clear()


def case_hostile_provider(driver: CaseDriver, case: Case) -> None:
    for location in ("hostile-a", "hostile-b"):
        for response in HOSTILE_RESPONSES:
            project = fixture(driver, location + "-" + response, provider="mock",
                              response=response, hostile_keys=True)
            result = turn(driver, project, "Reply TEST.", effort="fixture-secret")
            envelope = member(result, "envelope", dict)
            case.check("hostile provider path actually fails", member(envelope, "state", str) == "failed")
            case.check("real vendor provider error path", member(result, "vendor_error_observed", bool))
            receipt = driver.execute("mock_receipt")
            case.check("owned mock received overridden provider request",
                       member(receipt, "received", bool) and member(receipt, "model_matches", bool))
            outputs = driver.execute("hostile_output_matrix", session=member(result, "session_id", str))
            case.check("complete required VIA output secrecy matrix", member(outputs, "complete", bool)
                       and member(outputs, "verbs", list) == list(HOSTILE_OUTPUT_VERBS))
            case.check("bounded full event output pagination", 0 < member(outputs, "event_pages", int)
                       <= HOSTILE_OUTPUT_PAGES)
            observation = driver.execute("secrecy_scan", scope="via-store-and-evidence")
            case.check("complete Boolean secrecy scan", member(observation, "complete", bool))
            case.check("synthetic values absent", member(observation, "secret_absent", bool))
            case.check("no stderr or undecoded payload capture", member(observation, "payload_captures", int) == 0)


def permission_attempts(raw: dict) -> dict:
    """Correlates actual child calls/rejections and rule readbacks (§5/§11)."""
    if member(raw, "complete", bool) is not True:
        raise EvidenceUnavailable("permission observation incomplete")
    child_rules = {}
    expected_rules = [{"action": name, "resource": "*", "effect": effect} for name, effect in (
        ("*", "allow"), ("question", "deny"), ("opencode_session_move", "deny"),
        ("opencode_session_rename", "deny"), ("opencode_list_mcp_resources", "deny"),
        ("opencode_read_mcp_resource", "deny"), ("skill", "deny"))]
    for row in member(raw, "child_rules", list):
        sid = member(row, "session_id", str)
        rules = member(row, "permissions", list)
        child_rules[sid] = rules == expected_rules
    if not child_rules:
        raise EvidenceUnavailable("actual task child and deny-rule readback absent")
    native = member(raw, "native_events", list)
    c1 = member(raw, "events", list)
    calls, asked, replies, denied_calls = {}, {}, {}, {}
    for event in native:
        kind, data = member(event, "type", str), member(event, "data", dict)
        sid = data.get("sessionID")
        if kind == "permission.asked":
            request = member(data, "id", str)
            source = member(data, "source", dict)
            asked[request] = {"callID": member(source, "id", str), "session": require(sid, str, "ask session")}
        elif kind == "permission.replied":
            request = data.get("requestID", data.get("id"))
            replies[require(request, str, "permission reply request ID")] = member(data, "reply", str)
        elif sid in child_rules and kind in {"session.tool.called", "session.tool.input.started",
                                             "session.tool.failed"}:
            call = member(data, "id", str)
            name = data.get("name", calls.get(call, {}).get("name"))
            if name is not None:
                require(name, str, "actual attempted tool name")
            calls[call] = {"session": sid, "name": name}
            if kind == "session.tool.failed" and member(data, "error", dict).get("type") == "permission.rejected":
                denied_calls[call] = calls[call]
    asked_calls = {row["callID"] for row in asked.values()}
    actual_denied = {call: row for call, row in denied_calls.items() if call not in asked_calls}
    denial_counts = {}
    for event in c1:
        if member(event, "type", str) == "action.denied":
            kind = member(event, "kind", str)
            denial_counts[kind] = denial_counts.get(kind, 0) + 1
    attempts = []
    for call, row in actual_denied.items():
        name = require(row["name"], str, "denied tool name")
        kind = ("command" if name in {"bash", "shell"} else "file_write" if name in {"edit", "write", "patch"}
                else "network" if name in {"webfetch", "websearch", "browser"} else "other")
        if denial_counts.get(kind, 0) <= 0:
            raise EvidenceUnavailable("native child denial has no matching C1 action.denied")
        denial_counts[kind] -= 1
        attempts.append({"callID": call, "child_session": row["session"],
                         "deny_rule": child_rules[row["session"]], "asked": False,
                         "disposition": "action.denied"})
    unhandled = 0
    declined = sum(member(event, "type", str) == "vendor.request_declined" and
                   event.get("vendor_method", "").startswith("permission.asked:")
                   for event in c1)
    for request, row in asked.items():
        call, sid = row["callID"], row["session"]
        if sid not in child_rules or call not in calls or replies.get(request) != "reject" or declined <= 0:
            unhandled += 1
            continue
        declined -= 1
        attempts.append({"callID": call, "child_session": sid, "deny_rule": child_rules[sid],
                         "asked": True, "reply": replies[request], "settled": True, "decline_ms": None})
    return {"attempts": attempts, "unhandled_asks": unhandled}


def case_never_ask(driver: CaseDriver, case: Case) -> None:
    driver.execute("set_inherit", settings={"skills": "off"})
    project = fixture(driver, "never-ask", provider="mock", fake_lsp=True,
                      mcp=True, plugin=True, hook=True, subagent="oclive-ask",
                      subagent_permission="ask", skills="off")
    result = turn(driver, project,
                  "Use the offered subagent/task tool with agent oclive-ask. In the child attempt question, skill, "
                  "session move and rename; attempt the fixture's ask-rule shell action.")
    case.check("never-ask turn completed with owned terminal",
               member(result, "owned_terminal", bool)
               and member(member(result, "envelope", dict), "state", str) == "completed")
    observation = driver.execute("permission_observations", session=result["session_id"])
    attempts = member(observation, "attempts", list)
    if not attempts:
        raise EvidenceUnavailable("no actual attempted actions")
    for attempt in attempts:
        member(attempt, "callID", str)
        member(attempt, "child_session", str)
        case.check("child deny-rule readback", member(attempt, "deny_rule", bool))
        if member(attempt, "asked", bool):
            case.check("ask replied reject", member(attempt, "reply", str) == "reject")
            case.check("ask settled", member(attempt, "settled", bool))
            if attempt.get("decline_ms") is None:
                case.limitations.append("Live decline latency unavailable; OC07 fakes cover the 5 s bound.")
            else:
                member(attempt, "decline_ms", int)
        else:
            case.check("attempt denied or unavailable", member(attempt, "disposition", str)
                       in {"action.denied", "tool_unavailable"})
    case.check("no unattributed unhandled ask", member(observation, "unhandled_asks", int) == 0)
    helpers = driver.execute("helper_observations")
    for key in ("mcp_started", "plugin_started", "hook_started", "fake_lsp_started",
                "package_inventory_clean", "binary_inventory_clean"):
        case.check(key, member(helpers, key, bool))


def case_marker(driver: CaseDriver, case: Case) -> None:
    project = fixture(driver, "marker-helper", provider="mock", helper="marker",
                      separate_group=True)
    pending = driver.execute("start_turn", project=project, mode="mock",
                             prompt="Run the marker fixture helper and wait at its barrier.")
    try:
        barrier = driver.execute("helper_barrier", session=pending["session_id"], helper="marker")
        case.check("owned helper active before server-loss control", member(barrier, "owned", bool)
                   and member(barrier, "started", bool))
        driver.execute("server_loss_for_marker", session=pending["session_id"])
        envelope = driver.execute("wait_turn", session=pending["session_id"])
        failure = member(envelope, "failure", dict)
        case.check("server loss reported", member(failure, "class", str) == "server_lost")
        observation = driver.execute("marker_helper")
        case.check("helper separate group", member(observation, "separate_group", bool))
        report = member(observation, "host_leftovers", dict)
        case.check("server-wide leftover scope", member(report, "scope", str) == "server")
        entries = member(report, "processes", list)
        identity = member(observation, "helper_identity", dict)
        pid, started = member(identity, "pid", int), member(observation, "helper_started_at", str)
        member(identity, "start_ticks", int)  # Privately verified identity, absent from public C1.
        case.check("helper pid and start timestamp listed", any(member(row, "pid", int) == pid
                   and member(row, "started_at", str) == started for row in entries))
        case.check("exact-marker Host report includes helper", member(observation, "marker_matches", bool))
    finally:
        driver.execute("helper_release", helper="marker")
    cleanup = driver.execute("helper_stop_proof", helper="marker")
    case.check("cooperative owned helper gone", member(cleanup, "gone", bool)
               and member(cleanup, "pgrep_absent", bool))


def case_credential_shape(driver: CaseDriver, case: Case) -> None:
    namespace = driver.execute("namespace", name="seeded-integration")
    proof = driver.execute("daemon_stop_proof", namespace=namespace)
    case.check("VIA stopped before direct seed", member(proof, "proven", bool))
    seed = driver.execute("direct_seed", namespace=namespace, schema_checked=True)
    for key in ("exact_namespace_layout", "chain_0700", "known_connection_shape",
                "secret_absent_from_integration", "direct_child_gone", "descendants_gone",
                "pgrep_absent"):
        case.check(key, member(seed, key, bool))
    case.check("no model call while seeding", member(seed, "model_requests", int) == 0)
    refused = driver.execute("credential_refusal", namespace=namespace)
    case.check("known nonempty credentials refused", member(refused, "code", str)
               == "unexpected_credential_state")
    case.check("refusal not cached", member(refused, "cached", bool) is False)
    case.check("no session or prompt", member(refused, "session_creates", int) == 0
               and member(refused, "prompt_submits", int) == 0)
    case.check("no credential GET", member(refused, "credential_gets", int) == 0)


SEAMS = {
    "partial": "wire.http.body_after_prefix",
    "response": "wire.http.before_response",
    "foreign": "adapters.opencode.prompt_after_eligibility",
    "loss": "routes.opencode.before_event_read",
    "identity": "adapters.opencode.reopen_identity_read",
}


@contextlib.contextmanager
def armed(driver: CaseDriver, point: str, occurrence=1, action="pause", context=None):
    driver.execute("seam_arm", point=point, occurrence=occurrence, action=action, context=context)
    try:
        yield
    finally:
        driver.execute("seam_release", point=point, occurrence=occurrence)


def case_write_cancel(driver: CaseDriver, case: Case) -> None:
    case.limitations.append("Partial-prefix native Stop response is unavailable; bounded VIA cancel "
                            "progress is live evidence and reserved-pool independence is fake-first proof.")
    for mode in ("partial", "response"):
        point = SEAMS[mode]
        project = fixture(driver, "write-" + mode,
                          **({"helper": "write", "script_after_setup": True} if mode == "response" else {}))
        setup = turn(driver, project, "Reply SETUP.")
        completed(case, setup)
        context = {"session": setup["session_id"],
                   "vendor_session": setup["envelope"]["vendor_session_id"],
                   "turn_number": member(setup["envelope"], "turn", int) + 1}
        try:
            with armed(driver, point, context=context):
                pending = driver.execute("start_turn", project=project, session=setup["session_id"],
                                         mode="mock", prompt="x" * (1024 * 1024 - 16384))
                entered = driver.execute("seam_wait", point=point)
                case.check("owned request reached byte boundary", member(entered, "owned", bool))
                if mode == "partial":
                    written, length = member(entered, "written", int), member(entered, "body_length", int)
                    case.check("nonzero partial byte evidence", 0 < written < length)
                else:
                    case.check("full body sent before response", member(entered, "written", int)
                               == member(entered, "body_length", int) > 0)
                    barrier = driver.execute("helper_barrier", session=pending["session_id"], helper="write")
                    case.check("full-body native tool held", member(barrier, "owned", bool)
                               and member(barrier, "started", bool))
                cancel = driver.execute("cancel", session=member(pending, "session_id", str))
                case.check("cancel makes bounded progress while prompt held",
                           member(cancel, "cancel_returned", bool) and member(cancel, "prompt_held", bool)
                           and 0 <= member(cancel, "cancel_ms", int) <= 180000)
                if mode == "partial":
                    proof = member(cancel, "sourcebound_pool_test", dict)
                    case.check("FAKE source/build-bound reserved-pool independence",
                               member(proof, "verified", bool) and member(proof, "test_name", str)
                               == "qualification_body_prefix_cancel_preserves_offset_and_stop_pool")
                    if set(member(proof, "via_builds", dict)) != {"release", "failpoints"}:
                        raise EvidenceUnavailable("fake pool proof must bind both VIA builds")
                    for digest in [member(proof, "source_sha256", str),
                                   *member(proof, "via_builds", dict).values()]:
                        if type(digest) is not str or len(digest) != 64 or any(
                                char not in "0123456789abcdef" for char in digest):
                            raise EvidenceUnavailable("fake pool proof hash unavailable")
                else:
                    case.check("actual native user interruption while response held",
                               member(cancel, "native_user_interrupted", bool))
                # A retry is a failing, exact-target second occurrence. Release
                # the original occurrence even after this activation replaces it.
                driver.execute("seam_arm", point=point, occurrence=2, action="fail_io", context=context)
        finally:
            if mode == "response":
                driver.execute("helper_release", helper="write")
        result = driver.execute("write_observation", session=pending["session_id"],
                                turn_number=context["turn_number"], point=point, context=context)
        case.check("zero or one enqueue", member(result, "enqueues", int) in {0, 1})
        case.check("never resubmitted", member(result, "submits", int) == 1)
        if member(result, "indeterminate", bool):
            case.check("indeterminate generation drains", member(result, "drained", bool))


def case_foreign(driver: CaseDriver, case: Case) -> None:
    point = SEAMS["foreign"]
    project = fixture(driver, "foreign", helper="foreign", script_after_setup=True)
    setup = turn(driver, project, "Reply SETUP.")
    completed(case, setup)
    context = {"session": setup["session_id"],
               "vendor_session": setup["envelope"]["vendor_session_id"],
               "turn_number": member(setup["envelope"], "turn", int) + 1}
    try:
        with armed(driver, point, context=context):
            pending = driver.execute("start_turn", project=project, session=setup["session_id"],
                                     mode="mock", prompt="Reply OWNED.")
            entered = driver.execute("seam_wait", point=point)
            case.check("eligibility barrier owned", member(entered, "owned", bool))
            driver.execute("foreign_prompt", session=pending["session_id"],
                           text="Run the foreign fixture helper and wait at its barrier.")
            helper = driver.execute("helper_barrier", session=pending["session_id"], helper="foreign")
            case.check("foreign execution held at real tool barrier", member(helper, "started", bool)
                       and member(helper, "owned", bool))
        fence = driver.execute("foreign_fence_observation", session=pending["session_id"],
                               turn_number=context["turn_number"])
        for key in ("foreign_active", "owned_not_submitted", "foreign_not_credited",
                    "owned_not_terminal", "successor_fenced"):
            case.check(key, member(fence, key, bool))
        driver.execute("cancel_nowait", session=pending["session_id"],
                       turn_number=context["turn_number"])
    finally:
        driver.execute("helper_release", helper="foreign")
    withdrawn = driver.execute("wait_turn", session=pending["session_id"])
    case.check("waiting owned turn cancelled or remains unknown", member(withdrawn, "state", str)
               in {"cancelled", "unknown"})
    observation = driver.execute("foreign_observation", session=pending["session_id"],
                                 turn_number=context["turn_number"], expect_owned_absent=True)
    for key in ("foreign_not_credited", "foreign_not_cancelled", "owned_not_submitted"):
        case.check(key, member(observation, key, bool))
    successor = turn(driver, project, "Reply SUCCESSOR.", session=setup["session_id"])
    completed(case, successor)
    cleanup = driver.execute("near_limit_inbox", count=2, prompt_bytes=1024 * 1024 - 8192)
    case.check("near-limit owned cleanup successor", member(cleanup, "successor_completed", bool))
    case.check("only owned leftover cancelled once", member(cleanup, "owned_deletes", int) == 1
               and member(cleanup, "foreign_deletes", int) == 0)


def case_transport_loss(driver: CaseDriver, case: Case) -> None:
    point = SEAMS["loss"]
    project = fixture(driver, "transport-loss", helper="tool", script_after_setup=True)
    setup = turn(driver, project, "Reply SETUP.")
    completed(case, setup)
    pending = driver.execute("start_turn", project=project, session=setup["session_id"],
                             mode="mock", prompt="Run the tool fixture helper and wait.")
    barrier = driver.execute("helper_barrier", session=pending["session_id"], helper="tool")
    case.check("active helper before owned transport loss", member(barrier, "owned", bool)
               and member(barrier, "started", bool))
    context = {"session": setup["session_id"],
               "vendor_session": setup["envelope"]["vendor_session_id"]}
    driver.execute("seam_arm", point=point, occurrence=1, action="fail_io", context=context)
    try:
        envelope = driver.execute("wait_turn", session=pending["session_id"])
        observation = driver.execute("loss_observation", session=pending["session_id"],
                                     turn_number=member(envelope, "turn", int))
        case.check("loss after acceptance", member(observation, "accepted_before_loss", bool))
        case.check("unknown outcome", envelope["state"] == "unknown")
        case.check("unavailable cost", member(envelope, "cost", dict)["usd"] is None)
        usage = member(envelope, "usage", dict)
        case.check("missing usage never zero", all(usage[key] is None for key in
                   ("input_tokens", "cached_input_tokens", "output_tokens")))
        case.check("no known terminal admitted on VIA's lost transport",
                   member(observation, "via_terminal_admitted", bool) is False)
    finally:
        driver.execute("seam_release", point=point, occurrence=1)
        driver.execute("helper_release", helper="tool")


def case_identity(driver: CaseDriver, case: Case) -> None:
    project = fixture(driver, "identity-reopen")
    first = turn(driver, project, "Reply READY.")
    completed(case, first)
    driver.execute("idle_retirement", session=first["session_id"])
    point = SEAMS["identity"]
    context = {"session": first["session_id"],
               "vendor_session": first["envelope"]["vendor_session_id"]}
    with armed(driver, point, context=context):
        pending = driver.execute("start_turn", project=project, session=first["session_id"],
                                 mode="mock", prompt="Reply REOPEN.")
        entered = driver.execute("seam_wait", point=point)
        case.check("identity occurrence one entered", member(entered, "occurrence", int) == 1)
        driver.execute("seam_arm", point=point, occurrence=2, action="fail_io", context=context)
    resumed = driver.execute("wait_turn", session=pending["session_id"])
    case.check("identity guarded reopen completes", member(resumed, "state", str) == "completed")
    guard = driver.execute("seam_observation", point=point)
    case.check("exactly one identity lookup", member(guard, "acknowledged", list) == [1])
    control = driver.execute("identity_duplicate_control", point=point)
    case.check("duplicate guard sensitivity", member(control, "second_occurrence_failed", bool))
    missing = driver.execute("missing_vendor_session", session=first["session_id"])
    case.check("missing ID resume mismatch", member(missing, "code", str) == "resume_mismatch")
    case.check("no replacement created", member(missing, "creates", int) == 0)


def case_compaction(driver: CaseDriver, case: Case) -> None:
    project = fixture(driver, "compaction", provider="mock", context=512)
    observed = False
    for attempt in range(3):
        result = turn(driver, project, "Use fixture tool and summarize: " + "x " * 800)
        envelope = completed(case, result)
        events = native_events(driver, envelope["vendor_session_id"])
        ledger = usage_ledger(events, envelope["vendor_session_id"], result["execution_id"], result["input_id"])
        if ledger["compactions"]:
            observed = True
            case.check("automatic vendor compaction", member(result, "automatic_compaction", bool))
            case.check("scope stays vendor_interval", member(envelope["usage"], "scope", str)
                       == "vendor_interval")
            case.check("labelled mock compaction", result["mode"] == "mock")
            break
    if not observed:
        raise EvidenceUnavailable("automatic compaction unobserved in three attempts")


def case_long_run(driver: CaseDriver, case: Case) -> None:
    project = fixture(driver, "long-run", provider="mock", large_fields=True)
    sessions = []
    end = member(driver.execute("phase_deadline"), "monotonic", float)
    samples = []
    case.limitations.append("Release APIs do not expose retained router keys/bytes; these values "
                            "are unavailable. Bound source/build-linked OC09 fakes own retained-bound proof.")
    for index in range(8):
        if time.monotonic() >= end:
            raise EvidenceUnavailable("long-run phase expired before session coverage")
        result = turn(driver, project, "Reply with the escaping-heavy fixture output.")
        completed(case, result)
        sessions.append(result["session_id"])
        sample = driver.execute("memory_sample", sessions=list(sessions))
        for key in ("rss_bytes", "active_sessions"):
            member(sample, key, int)
        case.check("at most two active sessions", 0 <= sample["active_sessions"] <= 2)
        for key in ("retained_keys", "retained_bytes", "within_packet_bounds"):
            member(sample, key, type(None))
        samples.append(sample)
    case.check("eight distinct owned session receipts observed", len(set(sessions)) == 8)
    for session in sessions[:-1]:
        completed(case, turn(driver, project, "Reply SECOND.", session=session))
    driver.execute("long_run_barrier_prepare", project=project, session=sessions[-1],
                   helper="tool", barrier_seconds=1800)
    pending = driver.execute("start_turn", project=project, session=sessions[-1], mode="mock",
                             prompt="Run the long-run fixture helper and hold its barrier.")
    case.check("final L9 turn keeps the owned session", member(pending, "session_id", str) == sessions[-1])
    barrier = driver.execute("helper_barrier", session=sessions[-1], helper="tool")
    case.check("final L9 helper owns the retained generation", member(barrier, "owned", bool)
               and member(barrier, "started", bool))
    # Sampling consumes the remaining absolute phase time; the driver bounds the
    # wait and polls deferred interruption. No public traffic is used here.
    tail = driver.execute("memory_tail", deadline=end, interval_seconds=5, sessions=list(sessions))
    if member(tail, "complete", bool) is not True:
        raise EvidenceUnavailable("bounded memory observation incomplete")
    case.check("bounded samples include final long-run life point", member(tail, "final_sample", bool))
    case.check("large fields observed rather than assumed", member(tail, "large_fields_seen", bool))
    case.check("mock traffic only", member(tail, "public_requests", int) == 0)
    case.check("recorded duration stays inside the phase ceiling", 0 <= member(tail, "duration_seconds", float)
               <= 20 * 60)


def case_record(driver: CaseDriver, case: Case, item: str, limit: int) -> None:
    observation = driver.execute("bounded_probe", item=item, attempt_limit=limit)
    attempts = member(observation, "attempts", int)
    case.check("probe attempt ceiling", 0 < attempts <= limit)
    rows = member(observation, "observations", list)
    if not rows:
        raise EvidenceUnavailable(item + " trigger not observed")
    for row in rows:
        member(row, "native_type", str)
        member(row, "status", int)
        member(row, "disposition", str)
    case.check("recorded native observations", bool(rows))


def case_error_shapes(driver: CaseDriver, case: Case) -> None:
    """L8 synthetic provider responses, without public-service flooding (§13)."""
    for response, expected in (("rate_limit", "rate_limit"), ("quota", "budget_exceeded"),
                               ("context", "vendor_error")):
        observed = False
        for attempt in range(3):
            project = fixture(driver, f"shape-{response}-{attempt}", provider="mock", response=response)
            result = turn(driver, project, "Reply ERROR SHAPE.")
            receipt = driver.execute("mock_receipt")
            case.check("mock error path received", member(receipt, "received", bool))
            envelope = member(result, "envelope", dict)
            if member(envelope, "state", str) == "failed" and envelope.get("failure") is not None:
                failure = member(envelope, "failure", dict)
                member(failure, "class", str)
                case.check(response + " class hint", failure["class"] == expected)
                observed = True
                break
        if not observed:
            raise EvidenceUnavailable(response + " failed envelope unobserved in three mock attempts")


L14_POINTS = ("long_run", "publication", "tool_during", "tool_after", "location_shell_during",
              "location_shell_after", "session_shell_during", "session_shell_after",
              "mcp_during", "mcp_after", "lsp_during", "lsp_after", "plugin_during",
              "plugin_after", "reload", "disposal")


def case_anchor(driver: CaseDriver, case: Case) -> None:
    """L14: verified daemon stop, anchor-only kill, independent one-second proof."""
    case.limitations.append("A sampled vendor death may result from SIGPIPE on stderr; "
                            "it does not uniquely establish parent-death-signal delivery.")
    offered = driver.execute("lifecycle_capabilities")
    member(offered, "source_schema_proven", bool)
    case.check("lifecycle offered paths grounded", offered["source_schema_proven"])
    points = member(offered, "points", dict)
    for point in L14_POINTS:
        driver.execute("phase_guard")
        if point not in points or type(points[point]) is not bool:
            raise EvidenceUnavailable("missing lifecycle capability " + point)
        if not points[point]:
            if point not in {"reload", "disposal"}:
                raise EvidenceUnavailable("required spawn life point unavailable: " + point)
            continue
        sample = driver.execute("lifecycle_prepare", point=point, build="release")
        case.check("Host-launched pinned generation", member(sample, "host_launched", bool))
        case.check("life point reached", member(sample, "reached", bool))
        case.check("live pin and no retirement", member(sample, "live_pin", bool)
                   and member(sample, "retirement_inflight", bool) is False)
        case.check("lock only on anchor", member(sample, "lock_holders", list)
                   == [member(sample, "anchor_identity", dict)])
        case.check("info and record equal vendor pid", member(sample, "info_pid", int)
                   == member(sample["vendor_identity"], "pid", int)
                   == member(sample, "record_pid", int))
        stopped = False
        try:
            # The operation may fail after signalling. Resume is guarded by the
            # same PID/start identity even when stop-proof collection failed.
            stopped = True
            stop = driver.execute("daemon_sigstop", identity=sample["daemon_identity"])
            case.check("stopped identity persisted", member(stop, "persisted", bool))
            case.check("daemon thread group stopped", member(stop, "all_threads_stopped", bool))
            case.check("vendor stdin held open", member(stop, "stdin_writer_open", bool))
            killed = driver.execute("anchor_sigkill", identity=sample["anchor_identity"],
                                    group=False)
            case.check("only anchor pid signalled", member(killed, "pid_only", bool))
            death = driver.execute("vendor_death", identity=sample["vendor_identity"],
                                   deadline=member(killed, "monotonic", float) + 1.0,
                                   stopped_daemon=sample["daemon_identity"])
            if member(death, "certain", bool) is not True:
                raise EvidenceUnavailable("vendor death evidence unverifiable")
            case.check("vendor independently gone within one second", member(death, "gone", bool)
                       and member(death, "elapsed_seconds", float) <= 1.0)
            case.check("identity evidence verifiable", member(death, "certain", bool))
            case.check("daemon stayed stopped", member(death, "daemon_stayed_stopped", bool))
        finally:
            if stopped:
                resumed = driver.execute("daemon_sigcont", identity=sample["daemon_identity"])
                if member(resumed, "identity_verified", bool) is not True:
                    raise EvidenceUnavailable("cannot resume same stopped daemon")
        successor = driver.execute("lifecycle_successor", predecessor=sample["vendor_identity"])
        case.check("positive predecessor admission", member(successor, "admitted", bool))
        case.check("no parallel server", member(successor, "parallel_servers", int) == 0)
        case.check("password changed in memory", member(successor, "password_changed", bool))
    # SIGPIPE on stderr is an alternate death cause when the anchor's read end
    # closes. Every sample records this limit. A negative fake writes to stderr
    # after anchor death with SIGPIPE ignored and must still survive the window.
    driver.execute("limitation", name="l14_stderr_sigpipe",
                   text="A sampled vendor death may result from SIGPIPE on stderr; "
                        "it does not uniquely establish parent-death-signal delivery.")


OC_DISPOSITIONS = {
    "OC01": ("gate", "auth", "redirect/proxy and malformed handshakes remain fake proof"),
    "OC02": ("gate", "config", "unknown credential shape remains fake proof"),
    "OC02b": ("gate", "anchor", "forced exec/privilege/record races remain fake proof"),
    "OC03": ("gate", "free", "history still exists"),
    "OC04": ("gate", "continuity+identity", "mismatching readback remains fake proof"),
    "OC05": ("gate", "cancel+foreign", "exhaustive fence interleavings remain fake proof"),
    "OC06": ("gate", "never_ask", "malformed, late and unknown routing remain fake proof"),
    "OC07": ("gate", "never_ask", "mandatory five-second enforcement is OC07 fake proof"),
    "OC08": ("gate", "cancel+write_cancel", "untriggered early/late races remain fake proof"),
    "OC09": ("gate", "transport_loss+long_run", "byte/count overflow matrix remains fake proof"),
    "OC10": ("gate", "anchor+transport_loss", "exhaustive crash races remain fake proof"),
    "OC11": ("gate", "usage+preflight", "ignored-switch and exact size edges remain fake proof"),
    "OC12": ("gate", "auth", "memory-only password proof"),
    "OC12b": ("gate", "hostile_provider", "VIA HTTP/SSE malformed-body matrix remains fake proof"),
}

L_DISPOSITIONS = {
    "L1": ("record-only", "collision", 12), "L2": ("gate", "write_cancel", 2),
    "L3": ("gate", "foreign", 1), "L4": ("gate", "hostile_provider", 8),
    "L5": ("gate", "never_ask", 1), "L6": ("record-only", "forms", 3),
    "L7": ("record-only", "compaction", 3), "L8": ("record-only", "error_shapes", 9),
    "L9": ("record-only", "long_run", 32), "L10": ("deferred-with-reason", "macos", 0),
    "L11": ("gate", "credential_shape", 1), "L12": ("record-only", "other", 10),
    "L14": ("gate", "anchor", 32),
}

CASES = {
    "preflight": case_preflight, "free": case_free, "continuity": case_continuity,
    "cancel": case_cancel, "usage": case_usage, "auth": case_auth, "config": case_config,
    "hostile_provider": case_hostile_provider, "never_ask": case_never_ask,
    "marker": case_marker, "credential_shape": case_credential_shape,
    "write_cancel": case_write_cancel, "foreign": case_foreign,
    "transport_loss": case_transport_loss, "identity": case_identity,
    "compaction": case_compaction, "long_run": case_long_run, "anchor": case_anchor,
    "collision": lambda driver, case: case_record(driver, case, "collision", 12),
    "forms": lambda driver, case: case_record(driver, case, "forms", 3),
    "error_shapes": case_error_shapes,
    "other": lambda driver, case: case_record(driver, case, "other", 10),
}

PHASE_CASES = {
    "preflight": ("preflight",), "free": ("free", "continuity", "cancel"),
    "ledger": ("usage", "compaction"),
    "isolation": ("config",), "hostile": ("hostile_provider",),
    "helpers": ("never_ask", "marker", "auth"), "credentials": ("credential_shape",),
    "seams": ("write_cancel", "foreign", "transport_loss", "identity", "collision",
              "forms", "error_shapes", "other"),
    "long_run": ("long_run",), "anchor": ("anchor",),
}


def run_phase(driver: CaseDriver, phase_name: str) -> list[dict]:
    phase = next((row for row in PHASES if row.name == phase_name), None)
    if phase is None:
        raise EvidenceUnavailable("unknown phase")
    builds = driver.execute("build_hashes")
    for kind in ("release", "test-failpoints"):
        digest = member(builds, kind, str)
        if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
            raise EvidenceUnavailable("unverifiable VIA build hash")
    driver.execute("phase_begin", phase=phase.name, seconds=phase.seconds,
                   public_turns=phase.public_turns, mock_turns=phase.mock_turns,
                   build=phase.build)
    records = []
    for name in PHASE_CASES[phase_name]:
        driver.execute("phase_guard")
        if name in {"collision", "forms", "compaction", "error_shapes", "other"}:
            driver.execute("phase_build", build="release")
        disposition = "record-only" if name in {"collision", "forms", "compaction",
                                               "error_shapes", "other", "long_run"} else "gate"
        records.append(run_case(driver, name, CASES[name], disposition))
        build_kind = ("release" if name in {"collision", "forms", "compaction", "error_shapes", "other"}
                      else phase.build)
        records[-1]["via_build"] = {"kind": build_kind, "sha256": builds[build_kind]}
        records[-1]["both_build_hashes"] = dict(builds)
        # A failed gate ends admission; record-only absence does not pretend to
        # pass, but may allow another bounded observation in this phase.
        if records[-1]["disposition"] == "gate" and records[-1]["result"] != "pass":
            break
    return records
