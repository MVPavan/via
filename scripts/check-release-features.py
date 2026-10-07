#!/usr/bin/env python3
"""Prove a release `via` cannot activate the test-only failpoint controller.

Usage: check-release-features.py target/release/via

Runtime-contracts §11 exclusion check, three parts:
1. Cargo's release feature graph for `via-cli` (no default features) has no
   `test-failpoints` or `test-support` feature and no `via-fake-agent`
   package.
2. The release binary is launched with every known activation input: a private
   failpoint directory and token, every Task 2 point armed to pause, and a
   partial configuration a feature build would refuse. It must start, run a
   fake turn to `completed` and never acknowledge or refuse a command.
3. Supporting evidence only: the binary holds none of the controller's unique
   marker strings. A string scan alone is not proof.

Part 2 drives the test-only fake agent, so `target/debug/via-fake-agent` must
exist (`cargo build -p via-fake-agent`); VIA_FAKE_AGENT_BINARY overrides it.
"""

import json
import os
import secrets
import subprocess
import sys
import tempfile
import time
from pathlib import Path

POINTS = [
    "store.spawn.before_commit",
    "store.spawn.after_commit",
    "core.intent.after_commit",
    "wire.prompt.after_write",
    "core.accept.before_commit",
    "store.commit.reply_lost",
    "core.recovery.page_boundary",
    "host.recovery.absence_commit",
    "host.anchor.after_arm_intent_commit",
    "core.force.cancel_read",
    "core.dispatch.before_grant",
    # Task 3 S1 (design §10): Store, Host and anchor seams (Task 4 removed
    # the raw log's two).
    "store.commit.receipt",
    "store.commit.submission",
    "store.commit.event",
    "store.commit.terminal",
    "store.commit.cancel",
    "store.commit.rider",
    "store.commit.session_closed",
    "store.commit.closing",
    "store.commit.closed",
    "store.commit.steer_intent",
    "store.commit.steer_outcome",
    "store.commit.fail_persistent",
    "store.journal.anchor_intent",
    "store.journal.identified",
    "store.journal.arm_intent",
    "store.journal.vendor_facts",
    "store.journal.absence",
    "store.read.stall",
    "store.read.dispatch",
    "store.read.queued_turn",
    "store.request.not_enqueued",
    "store.writer.lost",
    "store.sqlite.corrupt",
    "host.anchor.before_eof_cleanup",
    "host.anchor.final_reply_lost",
    "host.anchor.defer_cleanup",
    "host.anchor.stop_received",
    "host.anchor.arm_received",
    "host.early_stop.woken",
    "host.early_stop.snapshot",
    "host.early_stop.sent",
    # Task 3 S2 (design §10): turn-control seams.
    "core.dispatch.awaiting_slot",
    "core.wait.registered",
    # via-2lp: an `events` long-poll after its first empty read.
    "core.events.registered",
    "core.submit.before_commit",
    "core.run.settling",
    "core.run.returned",
    "core.run.before_handoff",
    "core.close.before_subscribe",
    "core.cancel.settling",
    "core.run.idle_expired",
    "core.cancel.ordered",
    # x.3.2 X4 D4.2: a cancel before its order's publication; the Codex
    # driver's wait before it polls its orders; the fake run's start (D7).
    "core.cancel.publish",
    "adapter.codex.ordered",
    "adapter.codex.cut_read",
    "adapter.fake.turn_started",
    # x.3.2 K2: a keyed steer's report about to commit with its outcome.
    "core.steer.before_outcome",
    # Task 3 S3 (design §6, §10): daemon lifecycle seams.
    "daemon.startup.after_lock",
    "daemon.dispatcher.before_start",
    "daemon.shutdown.idle_final",
    "daemon.shutdown.before_fence",
    "daemon.shutdown.after_fence",
    "core.shutdown.reconcile_entry",
    "core.shutdown.before_forced_terminal",
    # Task 3 review Y (design §10): Core-side receipt of reconciliation facts.
    "core.shutdown.evidence_stopped_live",
    "core.shutdown.evidence_absent",
    # Task 3 S5 (design §7.2, §10): same-sequence retry seams.
    "core.retry.before",
    "core.head.contended",
    "core.commit.before_send",
    "core.cancel.admitted",
    # Task 3 S5 round 2 (design §7.1, §10): corruption on one read command.
    "store.read.corrupt.spawn_key",
    "store.read.corrupt.operation",
    "store.read.corrupt.keyed_operation",
    "store.read.corrupt.snapshot",
    "store.read.corrupt.queued_turn",
    "store.read.corrupt.predecessors",
    "store.read.corrupt.next_seq",
    "store.read.corrupt.result",
    "store.read.corrupt.close_result",
    "store.read.corrupt.closing_sessions",
    "store.read.corrupt.terminated",
    "store.read.corrupt.events",
    "store.read.corrupt.logs",
    "store.read.corrupt.authenticate",
    "store.read.corrupt.unfinished",
    "store.read.corrupt.anchor_owners",
    "store.read.corrupt.unproven_anchors",
    "store.read.corrupt.anchor_cohort",
    "store.read.corrupt.queued_turns",
    "store.read.corrupt.anchor_records",
    # Task 4 T4-2 (design §6.1-§6.5, §6.7, §5.3): lanes, full disk, blobs.
    "store.read.corrupt.terminal_facts",
    "store.writer.before_serve",
    "store.rollback.fail",
    "store.sqlite.full",
    "blob.write.fail_after",
    "blob.step.stall",
    # Task 3 force row (design §6.8, §10): a recorded exit not yet returned.
    "wire.exit.observed",
    # Task 4 T4-3 (design §2.3, §8.6, §13.1): Core held before handling an
    # observation; a Wire connection dropped without `finish`.
    "core.observations.pause",
    "wire.fallback_drop",
    # S1-io fix round 2: an undecoded save's outcome held before its note.
    "wire.undecoded.before_note",
    # Task 4 T4-4 (design §3.2, §4.2, §13.1): a step-row commit, a delayed
    # Store read, the `status` read's corruption, a boundary's progress
    # publish, the drive between the terminal commit and `finish_running`.
    "store.commit.step",
    "store.read.delay_ms",
    "store.read.corrupt.status",
    "core.progress.publish",
    "core.finish_running.pause",
    # Task 4 T4-5 (design §10.4): a prompt-file copy held after its first fstat.
    "prompt_file.copy.pause",
    # Task 4 T4-6 (design §6.4, §6.7): the final-text file's write and sync,
    # and corruption on the `events` and `list` page reads.
    "final_text.write.fail",
    "final_text.write.short",
    "final_text.sync.fail",
    "store.read.corrupt.events_page",
    "store.read.corrupt.list",
    # Task 4 T4-7 (design §5.3, §13.1): a reported free space; each
    # data-size walk, counted.
    "store.statvfs.free_bytes",
    "core.data_size.walks",
    # T4-fix (design §5.3): a data-size walk held past its 2 s step.
    "store.data_size.walk",
    # S1 critic r2 finding 1 (design §7.1): corruption on the acceptance write.
    "store.commit.corrupt.acceptance",
    # S1-runtime2 fix round 2: corruption on the terminal write; the
    # terminal read-back held after the latch's phase one; Route's late
    # path entered with the terminal held.
    "store.commit.corrupt.terminal",
    "core.terminal.read_back",
    "routes.late.entered",
    # ocrouteA3 (OpenCode): the launch task's managed-directory job.
    "adapters.opencode.prepare",
    # x.3.2 C3 (C2 §5): the Claude adapter's refusal-cache clock.
    "adapter.claude.plan_clock_ms",
    # via-4sw.3.1 (runtime §5, OpenCode OC02b): the exec entry's seams and
    # the anchor's server-record write.
    "host.exec.before_death_signal",
    "host.exec.after_parent_check",
    "host.anchor.before_record_write",
    "host.anchor.after_record_write",
    # via-4sw.3.1 review r1: a short record write and a stalled lock open.
    "host.anchor.short_record_write",
    "host.anchor.before_lock",
    # Critical review ochostcrit #6: the predecessor proof's barrier.
    "host.anchor.predecessor_after_identity",
    # via-jt8.3.2 review r2: the Pi instructions write, before its sync.
    "adapter.pi.instructions.written",
    # picrit #5: the Pi evidence records' write, blocked.
    "adapter.pi.records.write",
]
ACTIVATION = ["VIA_FAILPOINT_DIR", "VIA_FAILPOINT_TOKEN"]
# Test-build overrides (design §6.2, §6.4) that release must neither parse nor forward.
OVERRIDES = {
    "VIA_TEST_CLIENT_VERSION": "0.0.0-release-check",
    "VIA_TEST_IDLE_EXIT_MS": "1",
    "VIA_TEST_READ_FAILURE_MS": "1",
    "VIA_TEST_EVENT_STALL_MS": "1",
    # Task 4 T4-5 (design §10.1): the C1 partial-line and reply-write bounds.
    "VIA_TEST_PARTIAL_LINE_MS": "1",
    "VIA_TEST_REPLY_WRITE_MS": "1",
    # Task 4 T4-6 (design §6.4): the final-text file cap.
    "VIA_TEST_FINAL_TEXT_FILE_MAX": "1",
}
MARKERS = [*ACTIVATION, *POINTS, *OVERRIDES, "failpoint controller", "VIA_TEST_HARNESS_PROCESSES"]
FIXTURE = {
    "expected_request": {"type": "start", "id": 1, "turn": 1, "prompt": "release"},
    "steps": [
        {"action": "emit", "message": {"type": "accepted", "id": 1, "vendor_turn_id": "fake-turn-1"}},
        {
            "action": "emit",
            "message": {
                "type": "terminal",
                "vendor_turn_id": "fake-turn-1",
                "status": "completed",
                "final_text": "done",
                "stop_reason": "end_turn",
            },
        },
    ],
}


def fail(message):
    print(f"FAIL: {message}", file=sys.stderr)
    return 1


def feature_graph():
    """Returns the release feature graph lines of `via-cli`."""
    result = subprocess.run(
        [
            "cargo", "tree", "--locked", "-p", "via-cli", "--no-default-features",
            "-e", "normal,features", "--prefix", "none", "-f", "{p} {f}",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.splitlines()


def private_dirs(root):
    paths = {}
    for name in ["state", "runtime", "sync", "failpoints"]:
        path = root / name
        path.mkdir(mode=0o700)
        paths[name] = path
    return paths


def environment(paths, fake, fixture, activation):
    home = paths["state"].parent / "home"
    private_env_dirs = {
        "HOME": home,
        "XDG_CONFIG_HOME": home / "config",
        "XDG_DATA_HOME": home / "data",
        "XDG_STATE_HOME": home / "state",
        "XDG_CACHE_HOME": home / "cache",
        "XDG_RUNTIME_DIR": paths["runtime"],
    }
    for path in private_env_dirs.values():
        path.mkdir(mode=0o700, exist_ok=True)
    env = {
        "PATH": os.environ.get("PATH", ""),
        "VIA_STATE_DIR": str(paths["state"]),
        "VIA_RUNTIME_DIR": str(paths["runtime"]),
        "VIA_FAKE_AGENT_BINARY": str(fake),
        "VIA_FAKE_SCENARIO": str(fixture),
        "VIA_FAKE_SYNC_DIR": str(paths["sync"]),
    }
    env.update({name: str(path) for name, path in private_env_dirs.items()})
    env.update(activation)
    return env


def client(via, env, *args, timeout=15):
    """Runs one client command; a timeout is returned as exit code None."""
    try:
        return subprocess.run(
            [str(via), *args], env=env, capture_output=True, text=True, timeout=timeout, check=False
        )
    except subprocess.TimeoutExpired as expired:
        return subprocess.CompletedProcess(expired.cmd, None, "", "timed out")


def start_daemon(via, env, trace):
    daemon = subprocess.Popen(
        [str(via), "daemon"], env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL, stderr=trace,
    )
    socket = Path(env["VIA_RUNTIME_DIR"]) / "via.sock"
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if daemon.poll() is not None:
            return daemon, f"daemon exited {daemon.returncode} before readiness"
        if socket.exists() and client(via, env, "daemon", "status", "--json", timeout=2).returncode == 0:
            return daemon, None
        time.sleep(0.01)
    return daemon, "daemon never became ready"


def stop_daemon(via, env, daemon):
    if daemon.poll() is None:
        client(via, env, "daemon", "stop", "--force", "--json", timeout=5)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            # A paused failpoint can hold the daemon; the directly owned child is killed.
            daemon.kill()
            daemon.wait()


def ignored_activation(via, fake, root):
    """Runs one full turn with every point armed; returns an error or None."""
    paths = private_dirs(root)
    token = secrets.token_hex(24)
    for point in POINTS:
        command = {"token": token, "occurrence": 1, "action": "pause"}
        path = paths["failpoints"] / f"{point}.json"
        path.write_text(json.dumps(command))
        path.chmod(0o600)
    fixture = root / "fixture.json"
    fixture.write_text(json.dumps(FIXTURE))
    activation = {
        "VIA_FAILPOINT_DIR": str(paths["failpoints"]),
        "VIA_FAILPOINT_TOKEN": token,
        # Supplied like the activation inputs; the marker scan below is the
        # direct check that release carries neither name.
        **OVERRIDES,
    }
    env = environment(paths, fake, fixture, activation)
    with open(root / "daemon.trace", "wb") as trace:
        daemon, error = start_daemon(via, env, trace)
        try:
            if error:
                return error
            spawn = client(via, env, "spawn", "--harness", "fake", "--model", "fake",
                           "--prompt", "release", "--json")
            if spawn.returncode is None:
                return "armed spawn timed out: the daemon paused at a failpoint"
            if spawn.returncode != 0:
                return f"armed spawn did not complete: exit {spawn.returncode} {spawn.stderr.strip()}"
            if json.loads(spawn.stdout.splitlines()[-1]).get("state") != "completed":
                return f"armed turn did not complete: {spawn.stdout.strip()}"
        finally:
            stop_daemon(via, env, daemon)
    markers = [p.name for p in paths["failpoints"].iterdir() if not p.name.endswith(".json")]
    if markers:
        return f"release build answered failpoint commands: {sorted(markers)}"
    return None


def ignored_partial(via, fake, root):
    """A feature build refuses a directory without a token; release must start."""
    paths = private_dirs(root)
    fixture = root / "fixture.json"
    fixture.write_text(json.dumps(FIXTURE))
    env = environment(paths, fake, fixture, {"VIA_FAILPOINT_DIR": str(paths["failpoints"])})
    with open(root / "daemon.trace", "wb") as trace:
        daemon, error = start_daemon(via, env, trace)
        stop_daemon(via, env, daemon)
    return error


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    via = Path(sys.argv[1]).resolve()
    if not via.is_file():
        return fail(f"{via} does not exist; build it with --release first")
    root = Path(__file__).resolve().parent.parent
    fake = Path(os.environ.get("VIA_FAKE_AGENT_BINARY", root / "target/debug/via-fake-agent"))
    if not fake.is_file():
        return fail(f"{fake} does not exist; run `cargo build -p via-fake-agent`")

    graph = feature_graph()
    offending = [
        line
        for line in graph
        if "test-failpoints" in line or "test-support" in line or line.startswith("via-fake-agent ")
    ]
    if offending:
        return fail(f"release feature graph includes test-only items: {offending}")
    print(f"feature graph: {len(graph)} nodes, no test-failpoints, no test-support, no via-fake-agent")

    with tempfile.TemporaryDirectory(prefix="via-release-") as scratch:
        scratch = Path(scratch)
        (scratch / "armed").mkdir(mode=0o700)
        (scratch / "partial").mkdir(mode=0o700)
        error = ignored_activation(via, fake, scratch / "armed")
        if error:
            return fail(error)
        print(f"activation inputs ignored: {len(POINTS)} points armed, turn completed, no acknowledgement")
        error = ignored_partial(via, fake, scratch / "partial")
        if error:
            return fail(f"partial activation: {error}")
        print("partial activation ignored: daemon started")

    data = via.read_bytes()
    found = [marker for marker in MARKERS if marker.encode() in data]
    if found:
        return fail(f"release binary contains failpoint markers: {found}")
    print(f"marker scan (supporting evidence): none of {len(MARKERS)} markers present")
    return 0


if __name__ == "__main__":
    sys.exit(main())
