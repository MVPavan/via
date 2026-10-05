//! T2-C restart handoff (dispatch design §10) past the daemon-wide queue
//! bound, through Core's public Engine over a real Store, Host and fake agent.
//! Two earlier Engines leave 17 sessions × 8 queued turns (136 > 128) in
//! Store without dispatching them; a restarted Engine recovers, hands them
//! all off, refuses new work until the count falls back, and runs them all.
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
use via_core::{AdapterConfig, BootstrapEnv, Engine, ResumeParams, SessionId, SpawnParams};

const CHILD: &str = "VIA_RESTART_HANDOFF_CHILD";
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const SESSIONS: usize = 17;
const TURNS: u32 = 8;

/// A workspace test build's sibling binary.
fn binary(name: &str) -> PathBuf {
    let deps = env::current_exe().unwrap();
    let path = deps.parent().unwrap().parent().unwrap().join(name);
    assert!(
        path.is_file(),
        "missing {}; build the workspace first",
        path.display()
    );
    path
}

/// Runs `name` again in a child with a fresh private root and a fake agent
/// that completes every turn 1..=8 prompted `p`.
fn run_child(name: &str) {
    let root = tempfile::tempdir().unwrap();
    for part in ["state", "runtime", "runtime/anchors", "sync"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.path().join(part))
            .unwrap();
    }
    let scripts: Vec<Value> = (1..=TURNS)
        .map(|turn| {
            let vendor = format!("fake-turn-{turn}");
            json!({"expected_request":{"type":"start","id":1,"turn":turn,"prompt":"p"},"steps":[
                {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":vendor}},
                {"action":"emit","message":{"type":"terminal","vendor_turn_id":vendor,"status":"completed","final_text":"done","stop_reason":"end_turn"}}
            ]})
        })
        .collect();
    let scenario = root.path().join("scenario.json");
    fs::write(&scenario, json!({"scripts":scripts}).to_string()).unwrap();
    fs::set_permissions(&scenario, fs::Permissions::from_mode(0o600)).unwrap();
    let status = Command::new(env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, root.path())
        .env("VIA_FAKE_AGENT_BINARY", binary("via-fake-agent"))
        .env("VIA_FAKE_SCENARIO", &scenario)
        .env("VIA_FAKE_SYNC_DIR", root.path().join("sync"))
        .status()
        .unwrap();
    assert!(status.success(), "{name} child failed: {status}");
}

fn open(root: &Path) -> std::sync::Arc<Engine> {
    Engine::open(
        &root.join("state"),
        &root.join("runtime"),
        AdapterConfig::load(BootstrapEnv::capture(), None).unwrap(),
        binary("via"),
    )
    .unwrap()
}

/// Receipts `sessions` sessions of `turns` queued turns each, dispatching none.
async fn leave_queued(root: &Path, sessions: usize, turns: u32) -> Vec<SessionId> {
    let engine = open(root);
    let mut made = Vec::new();
    for _ in 0..sessions {
        let raw = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE});
        let params: SpawnParams = serde_json::from_value(raw.clone()).unwrap();
        let (session, _) = engine
            .spawn(params, &raw.to_string())
            .await
            .unwrap()
            .enqueued
            .unwrap();
        for _ in 1..turns {
            let raw = json!({"session":session.as_str(),"handle":HANDLE,"prompt":"p"});
            let params: ResumeParams = serde_json::from_value(raw.clone()).unwrap();
            engine.resume(params, &raw.to_string()).await.unwrap();
        }
        made.push(session);
    }
    made
}

/// Design §10 item 4: 136 surviving queued turns are all counted past the
/// 128 bound, a new spawn is `admission_refused`, every session's dispatcher
/// is started (the pending-start set holds what the 128-capacity channel
/// cannot), and every surviving turn runs to `completed`.
#[test]
fn surviving_queued_turns_past_the_bound_are_counted_refused_and_all_run() {
    let Some(root) = env::var_os(CHILD) else {
        return run_child("surviving_queued_turns_past_the_bound_are_counted_refused_and_all_run");
    };
    let root = PathBuf::from(root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // Each earlier Engine stays within its own 128 bound.
        let mut sessions = leave_queued(&root, 9, TURNS).await;
        sessions.extend(leave_queued(&root, SESSIONS - 9, TURNS).await);
        let engine = open(&root);
        assert_eq!(engine.recover().await.unwrap(), 0);
        let handoff = engine.hand_off_queued().await.unwrap();
        assert_eq!((handoff.enqueued, handoff.cancelled), (SESSIONS * 8, 0));
        assert_eq!(engine.active(), SESSIONS * 8, "all are counted");
        let raw = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE});
        let refused = engine
            .spawn(
                serde_json::from_value(raw.clone()).unwrap(),
                &raw.to_string(),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.kind, "admission_refused");
        // Daemon main's start loop: receive, then retry pending starts.
        let mut starts = engine.take_starts().unwrap();
        let mut dispatchers = tokio::task::JoinSet::new();
        let mut started = 0;
        loop {
            while let Ok(session) = starts.try_recv() {
                let engine = std::sync::Arc::clone(&engine);
                dispatchers.spawn(async move { engine.dispatcher(session).await });
                started += 1;
            }
            if !engine.starts_pending() {
                break;
            }
            engine.retry_starts();
        }
        assert_eq!(started, SESSIONS, "one dispatcher per recovered session");
        let joined = tokio::time::timeout(Duration::from_secs(240), async {
            while let Some(result) = dispatchers.join_next().await {
                result.unwrap().unwrap();
            }
        })
        .await;
        assert!(joined.is_ok(), "every surviving turn ran");
        for session in &sessions {
            for turn in 1..=TURNS {
                let envelope = engine
                    .result(&format!("{}/{turn}", session.as_str()))
                    .await
                    .unwrap();
                let envelope: serde_json::Value = serde_json::from_str(envelope.get()).unwrap();
                assert_eq!(envelope["state"], "completed", "{envelope}");
            }
        }
        assert_eq!(engine.active(), 0);
    });
}

/// T2-C round 1: more than 128 recovered `Starting` sessions (130 × 1 queued
/// turn). The handoff's starts overflow the 128-capacity channel into the
/// pending-start set; daemon main's receive-then-retry loop drains all 130,
/// and every turn runs to `completed`. All 130 dispatchers start at once, as
/// daemon main starts them; the Engine's 8 default harness-process slots (design §11)
/// queue their turns, so Store's raw and request queues never overflow: no
/// latch and no `failed(store)`. Before T2-D, about half ended
/// `failed(store)` or the Store-failed latch was set.
#[test]
fn starts_beyond_the_channel_spill_into_the_pending_set_and_all_run() {
    const MANY: usize = 130;
    let Some(root) = env::var_os(CHILD) else {
        return run_child("starts_beyond_the_channel_spill_into_the_pending_set_and_all_run");
    };
    let root = PathBuf::from(root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut sessions = leave_queued(&root, 65, 1).await;
        sessions.extend(leave_queued(&root, MANY - 65, 1).await);
        let engine = open(&root);
        engine.recover().await.unwrap();
        let handoff = engine.hand_off_queued().await.unwrap();
        assert_eq!(handoff.enqueued, MANY);
        assert!(
            engine.starts_pending(),
            "starts past 128 wait in the pending set"
        );
        let mut starts = engine.take_starts().unwrap();
        let mut dispatchers = tokio::task::JoinSet::new();
        let mut started = 0;
        loop {
            while let Ok(session) = starts.try_recv() {
                let engine = std::sync::Arc::clone(&engine);
                dispatchers.spawn(async move { engine.dispatcher(session).await });
                started += 1;
            }
            if !engine.starts_pending() {
                break;
            }
            engine.retry_starts();
        }
        assert_eq!(
            started, MANY,
            "every recovered session's dispatcher started"
        );
        let joined = tokio::time::timeout(Duration::from_secs(240), async {
            while let Some(result) = dispatchers.join_next().await {
                result.unwrap().unwrap();
            }
        })
        .await;
        assert!(joined.is_ok(), "every turn ran");
        assert!(!engine.store_failed());
        for session in &sessions {
            let envelope = engine
                .result(&format!("{}/1", session.as_str()))
                .await
                .unwrap();
            let envelope: serde_json::Value = serde_json::from_str(envelope.get()).unwrap();
            assert_eq!(envelope["state"], "completed", "{envelope}");
        }
        assert_eq!(engine.active(), 0);
    });
}
