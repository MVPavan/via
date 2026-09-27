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
