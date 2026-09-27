//! Queue admission through Core's public Engine over a real Store, with no
//! turn driven, so every receipted turn stays queued (runtime §8, C1 §3.3,
//! §8.1). The case re-executes this binary with fake settings, since Core
//! reads them from the environment once at daemon startup.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{Value, json};
use via_core::{Engine, FakeConfig, ResumeParams, SpawnParams};

const CHILD: &str = "VIA_QUEUE_BOUNDS_CHILD";
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Runs `name` again in a child whose private State, runtime and fake settings
/// live under a fresh 0700 root.
fn run_child(name: &str) {
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    for part in ["state", "state/raw", "runtime", "runtime/anchors", "sync"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.path().join(part))
            .unwrap();
    }
    // Never launched: no turn is driven.
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
        root.join("via"),
    )
    .unwrap()
}

fn spawn_params() -> SpawnParams {
    serde_json::from_value(json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE}))
        .unwrap()
}

fn resume(raw: &Value) -> (ResumeParams, String) {
    (
        serde_json::from_value(raw.clone()).unwrap(),
        raw.to_string(),
    )
}

/// Runtime §8: the daemon holds at most 128 queued turns; the next spawn or
/// resume is `admission_refused`, never `queue_full` (that is per session),
/// while a keyed retry of an accepted resume still replays its receipt.
#[test]
fn daemon_wide_queued_turns_are_bounded_as_admission_refused() {
    let Some(root) = env::var_os(CHILD) else {
        run_child("daemon_wide_queued_turns_are_bounded_as_admission_refused");
        return;
    };
    let root = PathBuf::from(root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let engine = open(&root);
        let mut sessions = Vec::new();
        for _ in 0..127 {
            let receipted = engine.spawn(spawn_params(), "{}").await.unwrap();
            sessions.push(receipted.enqueued.unwrap().0);
        }
        let keyed =
            json!({"session":sessions[0].as_str(),"handle":HANDLE,"prompt":"q","op_key":"k"});
        let (params, raw) = resume(&keyed);
        let accepted = engine.resume(params, &raw).await.unwrap();
        assert!(
            accepted.enqueued.is_some(),
            "the 128th queued turn is admitted"
        );
        let refused = engine.spawn(spawn_params(), "{}").await.unwrap_err();
        assert_eq!(
            (refused.code, refused.kind, refused.message),
            (-32012, "admission_refused", "too many queued turns")
        );
        let (params, raw) =
            resume(&json!({"session":sessions[1].as_str(),"handle":HANDLE,"prompt":"q"}));
        let refused = engine.resume(params, &raw).await.unwrap_err();
        assert_eq!((refused.code, refused.kind), (-32012, "admission_refused"));
        // Replay precedes admission: the keyed retry returns the original receipt.
        let (params, raw) = resume(&keyed);
        let replayed = engine.resume(params, &raw).await.unwrap();
        assert!(replayed.enqueued.is_none(), "a replay creates no turn");
        assert_eq!(replayed.receipt, accepted.receipt);
    });
}
