#!/usr/bin/env python3
"""Maintainer qualification of pinned OpenCode 2.0.22 (vendor packet §13).

Import and --self-test are offline. Live execution is separately opt-in and uses
only the frozen anonymous free model or labelled owned loopback mocks. Supply
prebuilt release and test-failpoints VIA binaries; this runner never builds VIA.
No candidate on PATH is substituted for the packet's pinned binary.
"""

import argparse
import contextlib
import hashlib
import importlib
import json
import os
import stat
import shutil
import secrets
from pathlib import Path
import sys
import time
import unittest

import opencode_cases as cases
import opencode_safety as safety

REPO = Path(__file__).resolve().parents[2]
SCRATCHPAD = REPO / "scratchpad"
RECOVERY_ROOT = SCRATCHPAD / "execution" / "oc-live" / "recovery"
FAKE_GATE_MANIFEST = SCRATCHPAD / "execution" / "oc-live" / "fake-gate-manifest.json"
FAKE_GATE_TESTS = (
    "oc_live_prompt_barrier_rechecks_foreign_execution_before_submission",
    "oc_live_prompt_barrier_ignores_stale_and_foreign_targets",
    "oc_live_reopen_identity_guard_excludes_settings_and_detects_duplicate",
    "oc_live_owned_event_read_failure_after_acceptance_is_unknown",
    "oc_live_event_read_failure_ignores_foreign_generation",
    "qualification_body_prefix_cancel_preserves_offset_and_stop_pool",
    "qualification_before_response_holds_complete_request_and_stop_pool",
    "qualification_http_failures_retain_sent_and_exact_body_offsets",
    "qualification_http_targeting_excludes_stale_foreign_and_untracked_requests",
    "oc05_c2_inbox_exact_bound_accepts_cleanup_and_successor",
)
FAKE_GATES = ("fmt", "clippy", "clippy-failpoints", "nextest-default", "nextest-failpoints",
              "layers", "harness-literals", "deny-cached", "release-build", "release-features",
              "diff-check")
VERSION = "2.0.22"
E7_SHA256 = "540fdf565da27de9df69b6c3864582344e74ac4ffa225c283b289481d215d241"
TEST_MODULES = ("opencode_safety_tests", "opencode_cases_tests", "opencode_driver_tests",
                "opencode_barrier_tests", "opencode_runtime_tests", "opencode_tests")
_ACQUISITION_ROOT = None


def self_test():
    """Run fake/mock/synthetic-proc checks only; return failures and actual count."""
    suite = unittest.TestSuite()
    for name in TEST_MODULES:
        suite.addTests(unittest.defaultTestLoader.loadTestsFromModule(importlib.import_module(name)))
    # Failure tracebacks can contain arbitrary evidence. Only test identities
    # leave this collector, following packet §2.3's memory-only password rule.
    result = unittest.TestResult()
    suite.run(result)
    failed = [test.id() for test, _trace in result.failures + result.errors]
    failed.extend(test.id() for test, _reason in result.skipped)
    return failed, result.testsRun


def arguments(argv=None):
    """Separate offline verification from explicit live admission (packet §13)."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--live", action="store_true", help="explicit live opt-in after code review")
    parser.add_argument("--via", "--via-release", dest="via_release", type=Path)
    parser.add_argument("--via-failpoints", type=Path)
    parser.add_argument("--opencode", type=Path, help="separately acquired read-only pinned binary")
    parser.add_argument("--acquire", action="store_true", help="fetch the official pin in private HOME")
    parser.add_argument("--evidence", type=Path, help="new private worktree scratchpad directory")
    parser.add_argument("--fake-gate-manifest", type=Path, default=FAKE_GATE_MANIFEST,
                        help="saved fake gates bound to both VIA builds and current Rust source")
    parser.add_argument("--phase", action="append", choices=[row.name for row in cases.PHASES])
    args = parser.parse_args(argv)
    if args.self_test:
        if args.live or args.acquire:
            parser.error("--self-test cannot acquire or execute live")
        return args
    if not args.live:
        parser.error("--live is required; use --self-test for offline checks")
    if not args.via_release or not args.via_failpoints or not args.evidence:
        parser.error("both VIA builds and --evidence are required")
    if bool(args.opencode) == args.acquire:
        parser.error("choose exactly one of --opencode and --acquire")
    args.evidence = args.evidence.resolve()
    if not args.evidence.is_relative_to(SCRATCHPAD) or args.evidence == SCRATCHPAD:
        parser.error("evidence must be inside this worktree's scratchpad")
    if args.evidence.exists():
        parser.error("evidence must be a new directory")
    return args


def rust_source_hash(repo=REPO):
    """Bind fake proof to current public Rust source and build inputs (packet §13)."""
    paths = [repo / "Cargo.toml", repo / "Cargo.lock",
             repo / "scripts/check-release-features.py"]
    for directory in ("crates", "third_party"):
        paths.extend(path for path in (repo / directory).rglob("*")
                     if path.name == "Cargo.toml" or path.suffix == ".rs")
    digest = hashlib.sha256()
    for path in sorted(set(paths)):
        if path.is_symlink() or not path.is_file():
            raise safety.Blocked("fake gate source input is unavailable")
        relative = path.relative_to(repo).as_posix().encode()
        content = path.read_bytes()
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


def verify_fake_gates(path, release, failpoints, repo=REPO):
    """Reject missing/stale fake proof before acquisition or a start (packet §13)."""
    try:
        path = Path(path)
        if not path.resolve().is_relative_to(repo / "scratchpad") \
                or safety.credential_metadata_only(path.name):
            raise safety.Blocked("fake gate manifest must stay in the worktree scratchpad")
        for program in (Path(release), Path(failpoints)):
            info = program.stat()
            if not program.resolve().is_relative_to(repo) or not stat.S_ISREG(info.st_mode) \
                    or not info.st_mode & 0o111 or safety.credential_metadata_only(program.name) \
                    or safety.credential_metadata_only(program.resolve().name):
                raise safety.Blocked("VIA proof requires worktree executable artifacts")
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(descriptor, "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_size > 1024 * 1024:
                raise safety.Blocked("fake gate manifest is not a bounded regular file")
            record = json.loads(stream.read(1024 * 1024 + 1))
        if type(record) is not dict or record.get("version") != 1 \
                or type(record["version"]) is not int or record.get("result") != "pass":
            raise safety.Blocked("fake gate manifest has no passing result")
        hashes = {"release": safety.sha256(Path(release)),
                  "test-failpoints": safety.sha256(Path(failpoints))}
        if record.get("via_hashes") != hashes or record.get("rust_source_sha256") != rust_source_hash(repo):
            raise safety.Blocked("fake gate proof differs from supplied builds or source")
        tests = record.get("tests")
        if type(tests) is not list or any(type(name) is not str for name in tests) \
                or not set(FAKE_GATE_TESTS).issubset(tests):
            raise safety.Blocked("required fake-first seam proof is missing")
        gates = record.get("gates")
        if type(gates) is not dict or any(type(gates.get(name)) is not int or gates[name] != 0
                                         for name in FAKE_GATES):
            raise safety.Blocked("required fake gates have no proven success")
        return {"verified": True, "via_builds": {"release": hashes["release"],
                                                "failpoints": hashes["test-failpoints"]},
                "tests": list(FAKE_GATE_TESTS), "source_sha256": record["rust_source_sha256"]}
    except (OSError, ValueError, KeyError, RecursionError, TypeError):
        raise safety.Blocked("fake gate manifest unavailable or malformed") from None


@contextlib.contextmanager
def acquisition_home(home):
    """Plain official HTTPS acquisition sees only the private HOME (plan M1)."""
    global _ACQUISITION_ROOT
    before = dict(os.environ)
    before_root = _ACQUISITION_ROOT
    _ACQUISITION_ROOT = home
    os.environ.clear()
    os.environ.update(HOME=str(home), PATH="/usr/bin:/bin", LANG="C.UTF-8")
    try:
        yield
    finally:
        _ACQUISITION_ROOT = before_root
        os.environ.clear()
        os.environ.update(before)


def official_get(url, **options):
    """Supervise acquisition including DNS within the private absolute deadline."""
    if _ACQUISITION_ROOT is None:
        raise safety.Blocked("official acquisition requires a private HOME")
    return safety.supervised_official_get(url, private_home=_ACQUISITION_ROOT, **options)


def package_manifest(package):
    """Obtain exact-version official package metadata without npm/npx (packet §2)."""
    encoded = package.replace("/", "%2F")
    raw = official_get("https://registry.npmjs.org/" + encoded + "/" + VERSION,
                       allowed_hosts={"registry.npmjs.org"}, limit=1024 * 1024)
    value = json.loads(raw)
    if type(value) is not dict or value.get("name") != package or value.get("version") != VERSION:
        raise safety.Blocked("official package manifest identity differs")
    dist = value.get("dist")
    if type(dist) is not dict or type(dist.get("tarball")) is not str \
            or type(dist.get("integrity")) is not str or type(dist.get("unpackedSize")) is not int:
        raise safety.Blocked("official package integrity/size unavailable")
    return dist


def acquire(evidence):
    """Only pre-anchored official npm archives may reach decompression (packet §2)."""
    home = safety.private_directory(evidence / "acquisition-home")
    unavailable = []
    with acquisition_home(home):
        for name in ("@opencode/cli-linux-x64", "@opencode/cli-linux-x64-baseline"):
            destination = evidence / ("pinned-" + str(len(unavailable)))
            try:
                dist = package_manifest(name)
                if dist["integrity"] != "sha512-" + safety.NPM_SHA512 \
                        or dist["unpackedSize"] != safety.NPM_UNPACKED_SIZE:
                    raise safety.Blocked("registry archive anchor differs from reviewed pin")
                raw = official_get(dist["tarball"], allowed_hosts={"registry.npmjs.org"})
                return safety.extract_pinned_npm(raw, destination, integrity=safety.NPM_SHA512,
                                                 unpacked_size=safety.NPM_UNPACKED_SIZE)
            except (safety.Blocked, ValueError, OSError):
                # Retain only source identity. No raw HTTP errors, headers or URLs.
                unavailable.append(name)
    raise safety.Blocked("no official same-version artifact matches the packet pin")


def acquire_release(destination):
    """Unanchored GitHub archives are disabled before network or parsing (packet §2)."""
    raise safety.Blocked("GitHub fallback has no reviewed pre-extraction archive pin")


def private_recovery_root():
    """Create/check the private recovery chain below scratchpad (packet §13 N6)."""
    path = SCRATCHPAD
    for part in RECOVERY_ROOT.relative_to(SCRATCHPAD).parts:
        path = path / part
        if path == RECOVERY_ROOT or not path.exists():
            safety.private_directory(path)
        else:
            row = path.lstat()
            if not stat.S_ISDIR(row.st_mode) or row.st_uid != os.getuid() \
                    or stat.S_IMODE(row.st_mode) & 0o022:
                raise safety.Blocked("recovery parent is unsafe")
    return path


def system_tools_preflight():
    """Require known system rg/python3 before acquisition or daemon preparation (§13)."""
    for name in ("rg", "python3"):
        candidate = shutil.which(name, path=os.pathsep.join(safety.SYSTEM_PATHS))
        if candidate is None or Path(candidate).resolve().parent not in \
                {Path(part).resolve() for part in safety.SYSTEM_PATHS}:
            raise safety.Blocked("qualification requires system " + name)
    if not Path("/usr/bin/python3").is_file() or not os.access("/usr/bin/python3", os.X_OK):
        raise safety.Blocked("qualification requires the system fixture python3")
    return {"rg": True, "python3": True, "system_directories_only": True}


def protected_write(vault, path, value, secret_forms=()):
    """Scan every current bearer/synthetic form before creating evidence (§13 L4)."""
    raw = json.dumps(value, allow_nan=False).encode()
    if any(form and form in raw for form in secret_forms):
        raise safety.Blocked("bearer or synthetic secret rejected from evidence")
    vault.safe_write_json(path, value)


def persist_phase(evidence, phase, rows, vault, secret_forms=()):
    """Publish each protected partial phase snapshot atomically (packet §13 M9)."""
    pending = evidence / (phase + "." + secrets.token_hex(8) + ".pending")
    try:
        protected_write(vault, pending, rows, secret_forms)
        pending.replace(evidence / (phase + ".json"))
    finally:
        pending.unlink(missing_ok=True)


def selected_phases(requested):
    """Preflight always precedes model admission; a partial selection never qualifies."""
    selected = set(requested or [row.name for row in cases.PHASES])
    selected.add("preflight")
    return [row.name for row in cases.PHASES if row.name in selected]


def summary_verdict(records, phases, cleanup, interrupted):
    """Missing cases, stop proof or interruption cannot produce a pass (packet §13)."""
    expected = {name for phase in phases for name in cases.PHASE_CASES[phase]}
    names = {row["case"] for row in records}
    gates = [row for row in records if row["disposition"] == "gate"]
    complete = names == expected and len(names) == len(records)
    all_phases = phases == [row.name for row in cases.PHASES]
    proven_stop = type(cleanup) is dict and cleanup.get("stopped") is True
    passed = (all_phases and complete and bool(gates) and proven_stop and not interrupted
              and all(row["result"] == "pass" for row in gates))
    deferred = {"L10": "native macOS unavailable"}
    recorded = {row["case"]: row for row in records
                if row["disposition"] == "deferred-with-reason"}
    for live, (disposition, case, _limit) in cases.L_DISPOSITIONS.items():
        if disposition == "deferred-with-reason" and live != "L10":
            deferred[live] = recorded.get(case, {}).get("reason", case + ": no reviewed bounded trigger")
    return {"result": "pass" if passed else "not_passed", "coverage_complete": complete,
            "all_phases_selected": all_phases, "daemons_stopped": proven_stop,
            "gate_count": len(gates), "gate_passed": sum(row["result"] == "pass" for row in gates),
            "record_only_count": sum(row["disposition"] == "record-only" for row in records),
            "deferred": deferred, "deferred_count": len(deferred)}


def main(argv=None):
    args = arguments(argv)
    if args.self_test:
        failures, count = self_test()
        print(json.dumps({"self_test": "pass" if not failures else "fail", "tests": count,
                          "failures": failures}, sort_keys=True))
        return bool(failures)
    # Validate existing ancestors; never chmod/recreate somebody else's directory.
    relative = args.evidence.relative_to(REPO)
    root = REPO
    for part in relative.parts:
        root = root / part
        if not root.exists():
            root.mkdir(mode=0o700)
        if root == args.evidence:
            safety.private_directory(root)
    vault = safety.PasswordVault()
    records, cleanup, driver = [], {}, None
    phases = selected_phases(args.phase)
    info = {"runner": "scripts/qualify/opencode.py", "vendor_version": VERSION,
            "started_at": time.time(), "phases": phases, "oc_rows": cases.OC_DISPOSITIONS,
            "live_rows": cases.L_DISPOSITIONS, "openapi_e7_sha256": E7_SHA256}
    with safety.DeferredSignals() as signals:
        try:
            failures, count = self_test()
            info["self_tests"] = count
            if failures:
                raise safety.Blocked("offline self-tests failed")
            fake_gates = verify_fake_gates(args.fake_gate_manifest, args.via_release,
                                          args.via_failpoints)
            info["system_tools"] = system_tools_preflight()
            pinned = acquire(args.evidence) if args.acquire else args.opencode.resolve()
            safety.verify_binary(pinned)
            from opencode_driver import Driver
            driver = Driver(via_release=args.via_release, via_failpoints=args.via_failpoints,
                            opencode=pinned, evidence=args.evidence, signals=signals,
                            public_free=True, initialize=False)
            vault = driver.vault
            driver.fake_gate_manifest = fake_gates
            # A fresh evidence path must still find the previous stopped-daemon
            # journal (N6). Its private root is fixed within this worktree.
            driver.journal = safety.StopJournal(
                private_recovery_root() / "stopped-daemon.json", driver.proc)
            driver.prepare()
            adapter = cases.DriverAdapter(driver)
            for phase in phases:
                signals.guard()
                accepted = 0
                def retain_phase(rows):
                    nonlocal accepted
                    persist_phase(args.evidence, phase, rows, vault,
                                  getattr(driver, "secret_forms", ()))
                    # Only published, scanned records can reach the final sink.
                    records.extend(rows[accepted:])
                    accepted = len(rows)
                phase_records = cases.run_phase(adapter, phase, record_sink=retain_phase)
                if accepted != len(phase_records):
                    retain_phase(phase_records)
                if any(row["disposition"] == "gate" and row["result"] != "pass"
                       for row in phase_records):
                    break
            info["via_hashes"] = driver.observe("build_hashes")
        except BaseException as error:
            # No arbitrary exception string can leak a password, handle or raw response.
            info["runner_error"] = type(error).__name__
        finally:
            if driver is not None:
                try:
                    cleanup = driver.finish()
                except BaseException as error:
                    info["cleanup_error"] = type(error).__name__
                try:
                    # Qualification cannot outlive the source/build evidence
                    # admitted before acquisition and the first daemon start.
                    verify_fake_gates(args.fake_gate_manifest, args.via_release,
                                      args.via_failpoints)
                    info["fake_gates_reverified"] = True
                except BaseException as error:
                    info["proof_error"] = type(error).__name__
            info.update(summary_verdict(records, phases, cleanup, bool(signals.interrupted)))
            if any(key in info for key in ("runner_error", "cleanup_error", "proof_error")) \
                    or signals.interrupted:
                info["result"] = "blocked"
            info.update(ended_at=time.time(), cases=records, cleanup=cleanup)
            try:
                protected_write(vault, args.evidence / "summary.json", info,
                                getattr(driver, "secret_forms", ()))
            except safety.Blocked:
                info["result"] = "blocked"
                info["summary_rejected"] = True
            finally:
                if driver is not None and hasattr(driver, "clear_sensitive"):
                    driver.clear_sensitive()
                vault.clear()
    print(json.dumps({key: info[key] for key in ("result", "gate_count", "gate_passed")},
                     sort_keys=True))
    return 0 if info["result"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
