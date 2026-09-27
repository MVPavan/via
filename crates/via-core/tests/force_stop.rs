//! Force stop through Core's public Engine over a real Store and Host, where the
//! test controls when the drive starts: a force accepted before the drive's
//! execution launches anything (C1 §7.4, §7.6 force row, §6 lifecycle events).
//! Each case re-executes this binary with its fake settings, since Core reads
//! them from the environment once at daemon startup.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde_json::{Value, json};
use via_core::{DaemonStopParams, Deadline, Engine, FakeConfig, SpawnParams, StopMode};

const CHILD: &str = "VIA_FORCE_STOP_CHILD";

/// The real `via` binary serves as Host's anchor; a workspace test build makes it.
fn via_binary() -> PathBuf {
    let deps = env::current_exe().unwrap();
    let via = deps.parent().unwrap().parent().unwrap().join("via");
    assert!(
        via.is_file(),
        "missing {}; build -p via-cli first",
        via.display()
    );
    via
}

/// Runs `name` again in a child whose private State, runtime and fake settings
/// live under a fresh 0700 root.
fn run_child(name: &str) {
    let root = tempfile::tempdir().unwrap();
    for part in ["state", "state/raw", "runtime", "runtime/anchors", "sync"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.path().join(part))
            .unwrap();
    }
    // Never launched: the force is accepted before the drive executes.
    let vendor = root.path().join("vendor.sh");
    fs::write(&vendor, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&vendor, fs::Permissions::from_mode(0o700)).unwrap();
    let scenario = root.path().join("scenario.json");
    fs::write(&scenario, b"{}").unwrap();
    let status = Command::new(env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, root.path())
        .env("VIA_FAKE_AGENT_BINARY", &vendor)
        .env("VIA_FAKE_SCENARIO", &scenario)
        .env("VIA_FAKE_SYNC_DIR", root.path().join("sync"))
        .status()
        .unwrap();
    assert!(status.success(), "{name} child failed: {status}");
}

fn open(root: &Path) -> Engine {
    Engine::open(
        &root.join("state"),
        &root.join("runtime"),
        FakeConfig::from_environment().unwrap(),
        via_binary(),
    )
    .unwrap()
}

/// W1-D Sol finding 3: with no anchor intent nothing was launched, so no
/// vendor could have acknowledged the cancel. The turn ends `cancelled` with
/// `requested`/`quiescent`, never `acknowledged`, and the forced turn commits
/// `cancel.requested`, `cancel.settled`, `turn.ended` and `session.closed`.
#[test]
fn force_before_launch_claims_no_acknowledgement() {
    let Some(root) = env::var_os(CHILD) else {
        return run_child("force_before_launch_claims_no_acknowledgement");
    };
    let root = PathBuf::from(root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let engine = open(&root);
        let params: SpawnParams = serde_json::from_value(json!({
            "harness":"fake","model":"fake","prompt":"hello",
            "handle":format!("h_{}", "A".repeat(43)),
        }))
        .unwrap();
        let (_, session, prompt) = engine.spawn(params).await.unwrap();
        let force: DaemonStopParams = serde_json::from_value(json!({"force":true})).unwrap();
        assert_eq!(engine.request_stop(&force).await.unwrap(), StopMode::Force);
        engine.drive(&session, prompt).await.unwrap();
        let report = engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(5),
            ))
            .await;
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.anchors, 0, "nothing was launched: {report:?}");

        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        assert!(envelope["failure"].is_null(), "{envelope}");
        assert_eq!(envelope["stop_reason"], "interrupted", "{envelope}");
        assert_eq!(envelope["cancel"]["outcome"], "requested", "{envelope}");
        assert_eq!(envelope["cancel"]["cleanup"], "quiescent", "{envelope}");
        assert!(
            envelope["timestamps"]["accepted_at"].is_null(),
            "{envelope}"
        );

        let page = engine.events(&session).await.unwrap();
        let events: Vec<Value> = page["events"].as_array().unwrap().clone();
        let types: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "turn.queued",
                "turn.submitted",
                "cancel.requested",
                "cancel.settled",
                "turn.ended",
                "session.closed",
            ],
            "{page}"
        );
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["seq"], json!(index + 1), "dense seq: {event}");
        }
        assert_eq!(events[3]["outcome"], envelope["cancel"]["outcome"]);
        assert_eq!(events[3]["cleanup"], envelope["cancel"]["cleanup"]);
        assert_eq!(events[4]["cancel"], envelope["cancel"]);
        assert!(events[5]["turn"].is_null(), "session event: {}", events[5]);
        assert_eq!(
            envelope["events"],
            json!({"first_seq":1,"last_seq":5,"count":5}),
            "the turn's range ends at turn.ended"
        );
        drop(engine);
    });
}

/// Stand-in anchor that accepts Host's control connection and never reports
/// ready, so Host acquisition stalls until the turn deadline; it exits when
/// Host closes the connection.
const STALLED_ANCHOR: &str = "#!/usr/bin/env python3
import json, socket, sys
path = json.load(open(sys.argv[2]))['socket_path']
server = socket.socket(socket.AF_UNIX)
server.bind(path)
server.listen(1)
connection, _ = server.accept()
connection.recv(1)
";

/// W4-H Sol 1: a force during a stalled Host acquisition still ends the drive
/// well inside the final shutdown bound, so the receipted turn gets its
/// cancelled terminal. Nothing proves the unidentified anchor's group absent,
/// so the cancel is only `requested` with `uncertain` cleanup.
#[test]
fn force_during_stalled_acquisition_settles_the_turn() {
    let Some(root) = env::var_os(CHILD) else {
        return run_child("force_during_stalled_acquisition_settles_the_turn");
    };
    let root = PathBuf::from(root);
    let anchor = root.join("stalled-anchor");
    fs::write(&anchor, STALLED_ANCHOR).unwrap();
    fs::set_permissions(&anchor, fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let engine = Engine::open(
            &root.join("state"),
            &root.join("runtime"),
            FakeConfig::from_environment().unwrap(),
            anchor,
        )
        .unwrap();
        let params: SpawnParams = serde_json::from_value(json!({
            "harness":"fake","model":"fake","prompt":"hello",
            "handle":format!("h_{}", "A".repeat(43)),
        }))
        .unwrap();
        let (_, session, prompt) = engine.spawn(params).await.unwrap();
        let force: DaemonStopParams = serde_json::from_value(json!({"force":true})).unwrap();
        let forced_at = std::cell::Cell::new(None);
        let (driven, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(8), engine.drive(&session, prompt)),
            async {
                // Acquisition is then waiting for the anchor's ready frame.
                tokio::time::sleep(Duration::from_millis(500)).await;
                forced_at.set(Some(tokio::time::Instant::now()));
                engine.request_stop(&force).await.unwrap();
            }
        );
        driven
            .expect("a force must end a stalled acquisition's drive")
            .unwrap();
        let elapsed = forced_at.get().unwrap().elapsed();
        assert!(elapsed < Duration::from_secs(4), "drive took {elapsed:?}");
        let report = engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(5),
            ))
            .await;
        assert_eq!(report.unresolved_turns, 0, "{report:?}");
        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        assert_eq!(envelope["cancel"]["outcome"], "requested", "{envelope}");
        assert_eq!(envelope["cancel"]["cleanup"], "uncertain", "{envelope}");
        drop(engine);
    });
}

/// Stand-in anchor that passes Host's identity checks, accepts Configure and
/// ARM, launches a vendor that writes to the inherited vendor pipes, and then
/// never confirms the launch, so Host acquisition stalls after ARM. It stops
/// its vendor and exits when Host closes the connection.
const STALLED_AFTER_ARM_ANCHOR: &str = r"#!/usr/bin/env python3
import json, os, socket, subprocess, sys
bootstrap = json.load(open(sys.argv[2]))
server = socket.socket(socket.AF_UNIX)
server.bind(bootstrap['socket_path'])
server.listen(1)
connection, _ = server.accept()
control = connection.makefile('rwb')
fields = open('/proc/self/stat').read().rsplit(') ', 1)[1].split()
identity = {
    'pid': os.getpid(), 'pgid': int(fields[2]), 'uid': os.getuid(),
    'boot_id': open('/proc/sys/kernel/random/boot_id').read().strip(),
    'pid_namespace': os.readlink('/proc/self/ns/pid'),
    'start_ticks': int(fields[19]), 'marker': bootstrap['marker'],
}
def send(frame):
    control.write(json.dumps(frame).encode() + b'\n')
    control.flush()
send({'kind': 'ready', 'identity': identity})
control.readline()
send({'kind': 'configured'})
control.readline()
vendor = subprocess.Popen(['/bin/sh', '-c', 'echo vendor output before launch reply; exec sleep 30'])
control.readline()
vendor.kill()
vendor.wait()
";

/// W4-H Sol r2: a force that abandons an acquisition after ARM, when the
/// vendor already wrote output no raw writer owned, must not report a
/// complete raw log: the turn records `raw_log.incomplete` and warns.
#[test]
fn force_after_arm_abandonment_reports_raw_log_incomplete() {
    let Some(root) = env::var_os(CHILD) else {
        return run_child("force_after_arm_abandonment_reports_raw_log_incomplete");
    };
    let root = PathBuf::from(root);
    let anchor = root.join("stalled-after-arm-anchor");
    fs::write(&anchor, STALLED_AFTER_ARM_ANCHOR).unwrap();
    fs::set_permissions(&anchor, fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let engine = Engine::open(
            &root.join("state"),
            &root.join("runtime"),
            FakeConfig::from_environment().unwrap(),
            anchor,
        )
        .unwrap();
        let params: SpawnParams = serde_json::from_value(json!({
            "harness":"fake","model":"fake","prompt":"hello",
            "handle":format!("h_{}", "A".repeat(43)),
        }))
        .unwrap();
        let (_, session, prompt) = engine.spawn(params).await.unwrap();
        let force: DaemonStopParams = serde_json::from_value(json!({"force":true})).unwrap();
        let (driven, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(8), engine.drive(&session, prompt)),
            async {
                // By then the vendor has launched and written its line.
                tokio::time::sleep(Duration::from_millis(1000)).await;
                engine.request_stop(&force).await.unwrap();
            }
        );
        driven.expect("the forced drive must end").unwrap();
        let report = engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(5),
            ))
            .await;
        assert_eq!(report.unresolved_turns, 0, "{report:?}");
        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        assert!(
            envelope["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == "raw_log_incomplete"),
            "lost vendor output must be reported: {envelope}"
        );
        let page = engine.events(&session).await.unwrap();
        assert!(
            page["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["type"] == "raw_log.incomplete"),
            "{page}"
        );
        drop(engine);
    });
}
