//! Force stop through Core's public Engine over a real Store and Host, where the
//! test controls when the session's dispatcher starts: a force accepted before
//! it grants a turn, or during the turn's execution (C1 §7.4, §7.6 force row,
//! §6 lifecycle events).
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

#[path = "support/stand_in_anchor.rs"]
mod stand_in_anchor;

use serde_json::{Value, json};
use stand_in_anchor::{AfterArm, wait_flag};
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

/// T2-B2 design §4: a force accepted before the dispatch grant cancels the
/// still-queued turn without submission (C1 §7.2 `queued → cancelled`) and,
/// its session holding only queued work, commits `session.closed` with it.
/// Nothing launched, so the shutdown is clean. (A turn granted before force
/// is covered by Core's `a_turn_granted_before_force_...` unit test: it
/// submits, then ends `cancelled` with `requested`/`quiescent`.)
#[test]
fn force_before_dispatch_cancels_the_queued_turn_without_submission() {
    let Some(root) = env::var_os(CHILD) else {
        return run_child("force_before_dispatch_cancels_the_queued_turn_without_submission");
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
        let (session, _) = engine.spawn(params, "{}").await.unwrap().enqueued.unwrap();
        let force: DaemonStopParams = serde_json::from_value(json!({"force":true})).unwrap();
        assert_eq!(engine.request_stop(&force).await.unwrap(), StopMode::Force);
        engine.dispatcher(session.clone()).await.unwrap();
        let report = engine
            // The daemon's 10 s budget: Host reconciliation ends 5 s
            // before it (design §6.8).
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.anchors, 0, "nothing was launched: {report:?}");

        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        assert!(envelope["failure"].is_null(), "{envelope}");
        assert_eq!(envelope["stop_reason"], "interrupted", "{envelope}");
        assert!(envelope["cancel"].is_null(), "{envelope}");
        assert!(
            envelope["timestamps"]["submitted_at"].is_null(),
            "{envelope}"
        );

        let page = engine.events(session.as_str()).await.unwrap();
        let events: Vec<Value> = page["events"].as_array().unwrap().clone();
        let types: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            ["turn.queued", "turn.ended", "session.closed"],
            "{page}"
        );
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["seq"], json!(index + 1), "dense seq: {event}");
        }
        assert_eq!(events[2]["reason"], "daemon_stop_force", "{page}");
        assert!(events[2]["turn"].is_null(), "session event: {}", events[2]);
        assert_eq!(
            envelope["events"],
            json!({"first_seq":1,"last_seq":2,"count":2}),
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
        let (session, _) = engine.spawn(params, "{}").await.unwrap().enqueued.unwrap();
        let force: DaemonStopParams = serde_json::from_value(json!({"force":true})).unwrap();
        let forced_at = std::cell::Cell::new(None);
        let (driven, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(8), engine.dispatcher(session.clone())),
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
            // The daemon's 10 s budget: Host reconciliation ends 5 s
            // before it (design §6.8).
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
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

/// Task 1 Sol high 2 (supersedes W4-H round 3): a force that abandons an
/// acquisition after ARM, once the vendor wrote output, drains the vendor
/// pipes Host handed over before ARM: the raw log holds that output and is not
/// reported incomplete. A vendor launched with neither stop nor terminal
/// proved leaves the turn `unknown`.
#[test]
fn force_after_arm_abandonment_drains_vendor_output() {
    const LINE: &str = "vendor output before launch reply";
    let Some(root) = env::var_os(CHILD) else {
        return run_child("force_after_arm_abandonment_drains_vendor_output");
    };
    let root = PathBuf::from(root);
    let (envelope, events) = force_over_stand_in(&root, &AfterArm::Stall { line: LINE }, "wrote");
    assert_eq!(envelope["state"], "unknown", "{envelope}");
    assert_eq!(envelope["cancel"]["outcome"], "requested", "{envelope}");
    assert!(!warns(&envelope, "raw_log_incomplete"), "{envelope}");
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "raw_log.incomplete"),
        "{events:?}"
    );
    let session = envelope["session_id"].as_str().unwrap();
    let connection = format!("c_{}", session.trim_start_matches("s_"));
    let raw = fs::read(root.join(format!("state/raw/{connection}.raw"))).unwrap();
    assert!(
        raw.windows(LINE.len())
            .any(|window| window == LINE.as_bytes()),
        "the vendor's output must be in the raw log"
    );
}

/// Runs one turn over a stand-in anchor, forces it once the stand-in set
/// `barrier`, completes final shutdown and returns the durable envelope and
/// events.
fn force_over_stand_in(root: &Path, after_arm: &AfterArm, barrier: &str) -> (Value, Vec<Value>) {
    force_over_stand_in_within(root, after_arm, barrier, Duration::from_secs(3))
}

/// [`force_over_stand_in`] with final shutdown bounded by `shutdown`.
fn force_over_stand_in_within(
    root: &Path,
    after_arm: &AfterArm,
    barrier: &str,
    shutdown: Duration,
) -> (Value, Vec<Value>) {
    let anchor = root.join("stand-in-anchor");
    after_arm.install(&anchor);
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
        let (session, _) = engine.spawn(params, "{}").await.unwrap().enqueued.unwrap();
        let force: DaemonStopParams = serde_json::from_value(json!({"force":true})).unwrap();
        let (driven, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(10), engine.dispatcher(session.clone())),
            async {
                wait_flag(&root.join("runtime"), barrier).await;
                engine.request_stop(&force).await.unwrap();
            }
        );
        assert!(driven.is_ok(), "the forced drive must end");
        driven.unwrap().unwrap();
        let report = engine
            .shutdown(Deadline::at(tokio::time::Instant::now() + shutdown))
            .await;
        assert_eq!(report.unresolved_turns, 0, "{report:?}");
        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        let events = engine.events(session.as_str()).await.unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        (envelope, events)
    })
}

fn warns(envelope: &Value, code: &str) -> bool {
    envelope["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["code"] == code)
}

/// Task 1 Sol high 1: the anchor proved it stopped a live vendor, but group
/// absence stays unproved. Outcome and cleanup are independent facts: the
/// durable result is `cancelled` with `forced` and `uncertain` cleanup.
#[test]
fn proved_stop_without_proved_absence_is_forced_uncertain() {
    let Some(root) = env::var_os(CHILD) else {
        return run_child("proved_stop_without_proved_absence_is_forced_uncertain");
    };
    let after_arm = AfterArm::Serve {
        stopped_live: true,
        linger: true,
    };
    let (envelope, events) = force_over_stand_in(Path::new(&root), &after_arm, "spawned");
    assert_eq!(envelope["state"], "cancelled", "{envelope}");
    assert_eq!(envelope["cancel"]["outcome"], "forced", "{envelope}");
    assert_eq!(envelope["cancel"]["cleanup"], "uncertain", "{envelope}");
    assert!(warns(&envelope, "cancel_cleanup_uncertain"), "{envelope}");
    let settled = events
        .iter()
        .find(|event| event["type"] == "cancel.settled")
        .unwrap();
    assert_eq!(settled["outcome"], "forced", "{settled}");
}

/// Task 1 Sol high 1: a vendor was launched, but Host proved neither that its
/// stop found the vendor live nor a vendor terminal. The durable result is
/// `unknown`, not `cancelled`; the force was only `requested`.
#[test]
fn launched_turn_without_proved_stop_is_unknown() {
    let Some(root) = env::var_os(CHILD) else {
        return run_child("launched_turn_without_proved_stop_is_unknown");
    };
    let after_arm = AfterArm::Serve {
        stopped_live: false,
        linger: false,
    };
    let (envelope, _) = force_over_stand_in(Path::new(&root), &after_arm, "spawned");
    assert_eq!(envelope["state"], "unknown", "{envelope}");
    assert!(envelope["failure"].is_null(), "{envelope}");
    assert_eq!(envelope["cancel"]["outcome"], "requested", "{envelope}");
    assert_eq!(envelope["cancel"]["cleanup"], "quiescent", "{envelope}");
}

/// Task 1 closeout round 2: Route's verified Host close proved a live stop and
/// group absence, then shutdown recovery failed (its share of the final
/// deadline was already spent). The proved stop still settles the turn
/// `cancelled`, `forced`, `quiescent`.
#[test]
fn proved_stop_survives_recovery_failure() {
    let Some(root) = env::var_os(CHILD) else {
        return run_child("proved_stop_survives_recovery_failure");
    };
    let after_arm = AfterArm::Serve {
        stopped_live: true,
        linger: false,
    };
    // Host gets the final deadline minus Core's 1 s commit reserve: nothing.
    let (envelope, _) = force_over_stand_in_within(
        Path::new(&root),
        &after_arm,
        "spawned",
        Duration::from_secs(1),
    );
    assert_eq!(envelope["state"], "cancelled", "{envelope}");
    assert_eq!(envelope["cancel"]["outcome"], "forced", "{envelope}");
    assert_eq!(envelope["cancel"]["cleanup"], "quiescent", "{envelope}");
}
