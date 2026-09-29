//! Route→Adapter observation stream and failure causes through the real Route,
//! Wire and Host, with a scripted vendor. Route's own crate cannot open a Store, so
//! these run one layer up; each case re-executes this binary with its fake settings.
//! The test binary cannot be the anchor: libtest's header would reach vendor stdout.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[path = "support/stand_in_anchor.rs"]
#[expect(dead_code, reason = "these cases use only the stalling stand-in")]
mod stand_in_anchor;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use via_adapters::{
    AdapterError, AdapterRuntime, AdapterRuntimeConfig, Deadline, FakeConfig, FakeObservation,
    Observation, RouteError, RuntimeConfig, SessionId, ToolStatus, TurnNumber,
};
use via_store::{SpawnRecord, Store};

const SESSION: &str = "s_0123456789ab";
const CHILD: &str = "VIA_ROUTE_STREAM_CHILD";
/// The ordinary turn deadline of each case.
const TURN: Duration = Duration::from_secs(20);
/// Bound on one child case; the turn deadline is 20 s and cleanup adds 3 s.
const CHILD_LIMIT: Duration = Duration::from_secs(60);

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

/// Runs `name` again in a child process whose fake vendor is `script`, emitting
/// the NDJSON `lines` from its scenario file.
fn run_child(name: &str, script: &str, lines: &[Value]) {
    let root = tempfile::tempdir().unwrap();
    let dirs = ["state", "runtime", "sync"].map(|part| {
        let path = root.path().join(part);
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        path
    });
    let vendor = root.path().join("vendor.sh");
    fs::write(&vendor, format!("#!/bin/sh\n{script}")).unwrap();
    fs::set_permissions(&vendor, fs::Permissions::from_mode(0o700)).unwrap();
    let scenario = root.path().join("scenario.ndjson");
    let body: Vec<String> = lines.iter().map(ToString::to_string).collect();
    fs::write(&scenario, body.join("\n") + "\n").unwrap();
    let mut child = Command::new(env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, root.path())
        .env("VIA_FAKE_AGENT_BINARY", &vendor)
        .env("VIA_FAKE_SCENARIO", &scenario)
        .env("VIA_FAKE_SYNC_DIR", &dirs[2])
        .spawn()
        .unwrap();
    // A hung child is a failure, not a stuck suite.
    let limit = Instant::now() + CHILD_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        let expired = Instant::now() >= limit;
        if expired {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(
            !expired,
            "{name} child did not finish within {CHILD_LIMIT:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{name} child failed: {status}");
}

/// The child's isolated Store and adapter over the real Route, Wire and Host.
struct Child {
    root: PathBuf,
    store: Option<Store>,
    adapter: AdapterRuntime,
    runtime: tokio::runtime::Runtime,
}

impl Child {
    fn open(root: &Path) -> Self {
        Self::open_with_anchor(root, via_binary())
    }

    fn open_with_anchor(root: &Path, anchor: PathBuf) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = Store::open(&root.join("state")).unwrap();
        runtime
            .block_on(store.client().commit_spawn(SpawnRecord {
                session_id: SessionId::try_from(SESSION).unwrap(),
                handle_hash: [7_u8; 32],
                receipt: json!({"state":"queued"}),
                params: json!({"harness":"fake"}),
                prompt: "hello".into(),
                effective: json!({"deadlines":{"wall_ms":1}}),
                initial_event: json!({"seq":1,"type":"turn.queued","turn":1,"at":"2026-01-01T00:00:00.000Z"}),
            }))
            .unwrap();
        let adapter = AdapterRuntime::new(
            AdapterRuntimeConfig {
                runtime: RuntimeConfig {
                    anchor_binary: anchor,
                    anchor_dir: root.join("runtime"),
                },
                fake: FakeConfig::from_environment().unwrap(),
            },
            store.runtime_resources(),
        )
        .unwrap();
        Self {
            root: root.to_path_buf(),
            store: Some(store),
            adapter,
            runtime,
        }
    }

    /// Runs one turn under a `turn` deadline, calling `on_observation` with the
    /// Store owner, the sandbox root and each observation as it arrives.
    fn execute(
        &mut self,
        turn: Duration,
        mut on_observation: impl FnMut(&mut Option<Store>, &Path, &FakeObservation),
    ) -> (Vec<FakeObservation>, Result<(), AdapterError>) {
        let Self {
            root,
            store,
            adapter,
            runtime,
        } = self;
        let (sender, mut receiver) = mpsc::channel(4);
        let deadline = Deadline::at(tokio::time::Instant::now() + turn);
        let mut observed = Vec::new();
        // Never set: these turns are not force-stopped.
        let (_force, force) = tokio::sync::watch::channel(None);
        let result = runtime.block_on(async {
            let execute = adapter.execute(
                SessionId::try_from(SESSION).unwrap(),
                TurnNumber::try_from(1).unwrap(),
                "hello".to_owned(),
                sender,
                deadline,
                force,
                tokio::sync::watch::channel(None).1,
                Box::new(()),
            );
            tokio::pin!(execute);
            loop {
                tokio::select! {
                    Some(observation) = receiver.recv() => {
                        on_observation(store, root, &observation);
                        observed.push(observation);
                    }
                    result = &mut execute => {
                        while let Ok(observation) = receiver.try_recv() {
                            observed.push(observation);
                        }
                        break result.map(|_| ());
                    }
                }
            }
        });
        (observed, result)
    }
}

fn child_root() -> Option<PathBuf> {
    env::var_os(CHILD).map(|root: OsString| PathBuf::from(root))
}

#[test]
fn route_forwards_every_observation_in_order() {
    let lines = [
        json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}),
        json!({"type":"text","vendor_turn_id":"fake-turn-1","text":"hi"}),
        json!({"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"sh","input_summary":"ls"}),
        json!({"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t","status":"completed","output_summary":"ok","exit_code":0}),
        json!({"type":"note","n":1}),
        json!({"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"hi","stop_reason":"end_turn"}),
        json!({"type":"late_note","n":2}),
    ];
    let Some(root) = child_root() else {
        return run_child(
            "route_forwards_every_observation_in_order",
            "read -r start\n/bin/cat \"$VIA_FAKE_SCENARIO\"\n",
            &lines,
        );
    };
    let mut child = Child::open(&root);
    let (observed, result) = child.execute(TURN, |_, _, _| {});
    result.unwrap();
    // Everything except the terminal, which travels in the route result.
    let expected: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"] != "terminal")
        .collect();
    assert_eq!(observed.len(), expected.len());
    for (observation, line) in observed.iter().zip(expected) {
        match observation {
            FakeObservation::Accepted(accepted) => {
                assert_eq!(line["type"], "accepted");
                assert_eq!(accepted.vendor_turn_id.as_str(), "fake-turn-1");
            }
            FakeObservation::Data { observation } => {
                match (observation, line["type"].as_str().unwrap()) {
                    (Observation::AssistantText { text }, "text") => assert_eq!(text, "hi"),
                    (
                        Observation::ToolStarted {
                            tool_id,
                            name,
                            input_summary,
                        },
                        "tool_started",
                    ) => {
                        assert_eq!(
                            (tool_id.as_str(), name.as_str(), input_summary.as_str()),
                            ("t", "sh", "ls")
                        );
                    }
                    (
                        Observation::ToolEnded {
                            status, exit_code, ..
                        },
                        "tool_ended",
                    ) => {
                        assert_eq!((*status, *exit_code), (ToolStatus::Completed, Some(0)));
                    }
                    (
                        Observation::VendorOther {
                            vendor_type,
                            truncated,
                            ..
                        },
                        kind,
                    ) => {
                        assert_eq!(vendor_type, kind);
                        assert!(!truncated);
                    }
                    (other, kind) => panic!("{kind} became {other:?}"),
                }
            }
        }
    }
}

/// W4-H Sol 2: a force while Route waits for observation capacity (the
/// consumer is not draining) still reaches Route's bounded force close and
/// drain, and the turn ends `ForceStopped` well before its deadline.
#[test]
fn force_while_forwarding_is_blocked_ends_the_turn() {
    let mut lines = vec![json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"})];
    lines.extend(
        (0..300).map(
            |n| json!({"type":"text","vendor_turn_id":"fake-turn-1","text":format!("line {n}")}),
        ),
    );
    let Some(root) = child_root() else {
        return run_child(
            "force_while_forwarding_is_blocked_ends_the_turn",
            "read -r start\n/bin/cat \"$VIA_FAKE_SCENARIO\"\nprintf 'unterminated tail'\nexec sleep 30\n",
            &lines,
        );
    };
    let child = Child::open(&root);
    // Never read: the adapter and then Route block on observation capacity.
    let (sender, _receiver) = mpsc::channel(1);
    let probe = sender.clone();
    let (force_tx, force) = tokio::sync::watch::channel(None);
    let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(20));
    let (result, elapsed) = child.runtime.block_on(async {
        let execute = child.adapter.execute(
            SessionId::try_from(SESSION).unwrap(),
            TurnNumber::try_from(1).unwrap(),
            "hello".to_owned(),
            sender,
            deadline,
            force,
            tokio::sync::watch::channel(None).1,
            Box::new(()),
        );
        tokio::pin!(execute);
        // Force only once backpressure is observed: the channel is full, so the
        // adapter's next delivery waits for capacity (W4-H Sol r2).
        let observed = tokio::time::Instant::now() + Duration::from_secs(10);
        while probe.capacity() > 0 {
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut execute)
                    .await
                    .is_err(),
                "the turn ended before backpressure"
            );
            assert!(tokio::time::Instant::now() < observed, "no backpressure");
        }
        // Let the adapter reach its blocked send with more messages queued behind it.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut execute)
                .await
                .is_err(),
            "the blocked turn must still be running"
        );
        assert_eq!(
            probe.capacity(),
            0,
            "the observation channel must stay full"
        );
        let forced_at = tokio::time::Instant::now();
        force_tx.send_replace(Some(tokio::time::Instant::now()));
        let result = execute.await;
        (result, forced_at.elapsed())
    });
    assert!(elapsed < Duration::from_secs(5), "force took {elapsed:?}");
    let Err(AdapterError::Route(failure)) = result else {
        panic!("expected a route failure: {:?}", result.map(|_| ()));
    };
    assert!(
        matches!(failure.cause, RouteError::ForceStopped { .. }),
        "{failure:?}"
    );
}

/// Task 1 Sol high 2: an acquisition that fails after ARM (here its deadline
/// expires while Host awaits the launch reply) keeps its cause: the turn
/// fails `Deadline`, not transport loss, and reports the launch.
#[test]
fn post_arm_acquisition_deadline_keeps_its_cause() {
    const LINE: &str = "vendor output before launch reply";
    let Some(root) = child_root() else {
        return run_child(
            "post_arm_acquisition_deadline_keeps_its_cause",
            "exit 0\n",
            &[],
        );
    };
    let anchor = root.join("stand-in-anchor");
    stand_in_anchor::AfterArm::Stall { line: LINE }.install(&anchor);
    let child = Child::open_with_anchor(&root, anchor);
    let (sender, _receiver) = mpsc::channel(4);
    // Never set: this failure is the acquisition deadline, not a force.
    let (_force, force) = tokio::sync::watch::channel(None);
    let result = child.runtime.block_on(async {
        let execute = child.adapter.execute(
            SessionId::try_from(SESSION).unwrap(),
            TurnNumber::try_from(1).unwrap(),
            "hello".to_owned(),
            sender,
            Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            force,
            tokio::sync::watch::channel(None).1,
            Box::new(()),
        );
        // The stand-in writes flags beside its anchor directory.
        let flags = root.clone();
        // The vendor launched and wrote before the deadline.
        let (result, ()) = tokio::join!(execute, stand_in_anchor::wait_flag(&flags, "wrote"));
        result
    });
    let Err(AdapterError::Route(failure)) = result else {
        panic!("expected a route failure: {:?}", result.map(|_| ()));
    };
    assert!(
        matches!(failure.cause, RouteError::Deadline { .. }),
        "{failure:?}"
    );
    assert!(failure.launched, "{failure:?}");
}

/// Runs a turn over the stalling stand-in with an acquisition deadline of
/// `deadline` and sets force once `force_at` resolves; returns the failure.
fn stalled_acquisition_with_force(
    name: &str,
    deadline: Duration,
    force_at: impl FnOnce(&Path, tokio::time::Instant) -> std::pin::Pin<Box<dyn Future<Output = ()>>>,
) -> Option<via_adapters::RouteFailure> {
    let Some(root) = child_root() else {
        run_child(name, "exit 0\n", &[]);
        return None;
    };
    let anchor = root.join("stand-in-anchor");
    stand_in_anchor::AfterArm::Stall {
        line: "vendor line",
    }
    .install(&anchor);
    let child = Child::open_with_anchor(&root, anchor);
    let (sender, _receiver) = mpsc::channel(4);
    let (force_tx, force) = tokio::sync::watch::channel(None);
    let result = child.runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + deadline;
        let execute = child.adapter.execute(
            SessionId::try_from(SESSION).unwrap(),
            TurnNumber::try_from(1).unwrap(),
            "hello".to_owned(),
            sender,
            Deadline::at(deadline),
            force,
            tokio::sync::watch::channel(None).1,
            Box::new(()),
        );
        let (result, ()) = tokio::join!(execute, async {
            force_at(&root, deadline).await;
            force_tx.send_replace(Some(tokio::time::Instant::now()));
        });
        result
    });
    let failure = match result {
        Err(AdapterError::Route(failure)) => Some(failure),
        _ => None,
    };
    assert!(failure.is_some(), "expected a route failure");
    failure
}

/// Task 1 closeout round 2: a force requested after ARM and before the
/// acquisition deadline wins, even when that deadline expires inside the
/// force's grace: the turn is force-stopped, not `deadline_wall`.
#[test]
fn force_before_acquisition_deadline_is_force_stopped() {
    let failure = stalled_acquisition_with_force(
        "force_before_acquisition_deadline_is_force_stopped",
        Duration::from_millis(1500),
        |root, _| {
            let flags = root.to_path_buf();
            // ARM happened and the vendor wrote; the deadline is still ahead.
            Box::pin(async move { stand_in_anchor::wait_flag(&flags, "wrote").await })
        },
    );
    let Some(failure) = failure else { return };
    assert!(
        matches!(failure.cause, RouteError::ForceStopped { .. }),
        "{failure:?}"
    );
    assert!(failure.launched, "{failure:?}");
}

/// Task 1 closeout round 2: an acquisition deadline that expired before the
/// force keeps its cause.
#[test]
fn acquisition_deadline_before_force_keeps_deadline() {
    let failure = stalled_acquisition_with_force(
        "acquisition_deadline_before_force_keeps_deadline",
        Duration::from_millis(1500),
        // Strictly after the deadline Host's acquisition timer enforces.
        |_, deadline| {
            Box::pin(tokio::time::sleep_until(
                deadline + Duration::from_millis(50),
            ))
        },
    );
    let Some(failure) = failure else { return };
    assert!(
        matches!(failure.cause, RouteError::Deadline { .. }),
        "{failure:?}"
    );
    assert!(failure.launched, "{failure:?}");
}
