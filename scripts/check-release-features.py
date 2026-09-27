#!/usr/bin/env python3
"""Prove a release `via` cannot activate the test-only failpoint controller.

Usage: check-release-features.py target/release/via

Runtime-contracts §11 exclusion check, three parts:
1. Cargo's release feature graph for `via-cli` (no default features) has no
   `test-failpoints` feature and no `via-fake-agent` package.
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
]
ACTIVATION = ["VIA_FAILPOINT_DIR", "VIA_FAILPOINT_TOKEN"]
MARKERS = [*ACTIVATION, *POINTS, "failpoint controller"]
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
    env = {
        "PATH": os.environ.get("PATH", ""),
        "VIA_STATE_DIR": str(paths["state"]),
        "VIA_RUNTIME_DIR": str(paths["runtime"]),
        "VIA_FAKE_AGENT_BINARY": str(fake),
        "VIA_FAKE_SCENARIO": str(fixture),
        "VIA_FAKE_SYNC_DIR": str(paths["sync"]),
    }
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
    activation = {"VIA_FAILPOINT_DIR": str(paths["failpoints"]), "VIA_FAILPOINT_TOKEN": token}
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
    offending = [line for line in graph if "test-failpoints" in line or line.startswith("via-fake-agent ")]
    if offending:
        return fail(f"release feature graph includes test-only items: {offending}")
    print(f"feature graph: {len(graph)} nodes, no test-failpoints, no via-fake-agent")

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
