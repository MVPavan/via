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
use via_adapters::{AdapterError, Deadline, Observation, RouteError, SessionId, TurnEnd};
use via_store::{SpawnRecord, Store};

#[path = "support/one_turn.rs"]
mod one_turn;

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
/// the NDJSON `lines` from the sidecar `$VIA_FAKE_SCENARIO.lines`: the
/// scenario itself must be valid JSON (decision H2).
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
    let scenario = root.path().join("scenario.json");
    fs::write(&scenario, r#"{"scripts":[]}"#).unwrap();
    let body: Vec<String> = lines.iter().map(ToString::to_string).collect();
    fs::write(
        root.path().join("scenario.json.lines"),
        body.join("\n") + "\n",
    )
    .unwrap();
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
    _store: Store,
    adapter: one_turn::OneTurn,
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
                label: None,
                prompt: "hello".into(),
                effective: json!({"deadlines":{"wall_ms":1}}),
                initial_event: json!({"seq":1,"type":"turn.queued","turn":1,"at":"2026-01-01T00:00:00.000Z"}),
            }))
            .unwrap();
        let adapter = one_turn::OneTurn::new(&store, root, anchor);
        Self {
            root: root.to_path_buf(),
            _store: store,
            adapter,
            runtime,
        }
    }

    /// Turn 1 on a new session, under the wall `wall`, the daemon force
    /// `force` and no stop order.
    fn run_turn(
        &self,
        wall: Duration,
        force: tokio::sync::watch::Receiver<Option<tokio::time::Instant>>,
    ) -> (
        via_adapters::SessionDriver,
        tokio::sync::mpsc::Receiver<via_adapters::Admitted>,
        via_adapters::TurnCx,
    ) {
        let (driver, receiver) = self.adapter.session(SESSION, &self.root);
        let deadline = Deadline::at(tokio::time::Instant::now() + wall);
        let cx = one_turn::turn_cx(
            driver.prepare(),
            deadline,
            force,
            tokio::sync::watch::channel(None).1,
        );
        (driver, receiver, cx)
    }

    /// Runs one turn under a `turn` deadline, collecting each observation.
    fn execute(&self, turn: Duration) -> (Vec<Observation>, TurnEnd) {
        // Never set: these turns are not force-stopped.
        let (_force, force) = tokio::sync::watch::channel(None);
        let (driver, mut receiver, cx) = self.run_turn(turn, force);
        let mut observed = Vec::new();
        let end = self.runtime.block_on(async {
            let execute = driver.run_turn(one_turn::hello(), cx);
            tokio::pin!(execute);
            loop {
                tokio::select! {
                    Some(admitted) = receiver.recv() => observed.push(admitted.item.observation),
                    end = &mut execute => {
                        while let Ok(admitted) = receiver.try_recv() {
                            observed.push(admitted.item.observation);
                        }
                        break end;
                    }
                }
            }
        });
        (observed, end)
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
            "read -r start\n/bin/cat \"$VIA_FAKE_SCENARIO.lines\"\n",
            &lines,
        );
    };
    let child = Child::open(&root);
    let (observed, end) = child.execute(TURN);
    end.outcome.unwrap();
    // Everything except the terminal, which travels in the route result,
    // and unknown messages, which send no observation (Task 4 design §2.3).
    let expected: Vec<&Value> = lines
        .iter()
        .filter(|line| {
            ["accepted", "text", "tool_started", "tool_ended", "terminal"]
                .contains(&line["type"].as_str().unwrap())
        })
        .collect();
    assert_eq!(observed.len(), expected.len());
    for (observation, line) in observed.iter().zip(expected) {
        match observation {
            Observation::Accepted(accepted) => {
                assert_eq!(line["type"], "accepted");
                assert_eq!(
                    accepted.vendor_turn_id.as_ref().unwrap().as_str(),
                    "fake-turn-1"
                );
            }
            // Task 4 design §2.3: the terminal's final text, one piece.
            Observation::FinalText(text) => {
                assert_eq!(line["type"], "terminal");
                assert_eq!(line["final_text"], text.as_str());
            }
            Observation::Progress(marks) => {
                let started: Vec<(&str, &str)> = marks
                    .tools_started
                    .iter()
                    .map(|(id, name)| (id.as_str(), name.as_str()))
                    .collect();
                let fields = (
                    marks.model,
                    started,
                    marks.tools_ended.clone(),
                    marks.usage.clone(),
                );
                match line["type"].as_str().unwrap() {
                    "text" => assert_eq!(fields, (true, vec![], vec![], None)),
                    "tool_started" => assert_eq!(fields, (false, vec![("t", "sh")], vec![], None)),
                    "tool_ended" => {
                        assert_eq!(fields, (false, vec![], vec!["t".to_owned()], None));
                    }
                    kind => panic!("{kind} became {marks:?}"),
                }
            }
            Observation::IdentityConfirmed(_)
            | Observation::ActionDenied(_)
            | Observation::RequestDeclined(_)
            | Observation::SteerDelivered(_)
            | Observation::Warning(_)
            | Observation::VendorClosed(_)
            | Observation::ResumeMismatch { .. }
            | Observation::LateTerminal(_) => {
                panic!("{} became {observation:?}", line["type"]);
            }
        }
    }
}

/// Writes the first 1,000 lines at once, then the rest: Wire's queue holds
/// 1,024 messages (A47), and this child's current-thread runtime lets the
/// reader take a whole burst before Route runs, so one burst of all
/// 1,045 lines fails `overflow` by design (§8.2).
const TWO_BURSTS: &str = "read -r start
/usr/bin/head -n 1000 \"$VIA_FAKE_SCENARIO.lines\"
sleep 0.2
/usr/bin/tail -n +1001 \"$VIA_FAKE_SCENARIO.lines\"
printf 'unterminated tail'
exec sleep 30
";

/// W4-H Sol 2: a force while Route waits for observation capacity (the
/// consumer is not draining) still reaches Route's bounded force close and
/// drain, and the turn ends `ForceStopped` well before its deadline. Task 4
/// design §2.3: the channel holds 1,024 items, so the vendor sends 20 more.
#[test]
fn force_while_forwarding_is_blocked_ends_the_turn() {
    let mut lines = vec![json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"})];
    lines.extend(
        (0..via_adapters::OBSERVATION_ITEMS + 20).map(
            |n| json!({"type":"text","vendor_turn_id":"fake-turn-1","text":format!("line {n}")}),
        ),
    );
    let Some(root) = child_root() else {
        return run_child(
            "force_while_forwarding_is_blocked_ends_the_turn",
            TWO_BURSTS,
            &lines,
        );
    };
    let child = Child::open(&root);
    // Never read: the adapter and then Route block on observation capacity.
    let (force_tx, force) = tokio::sync::watch::channel(None);
    let (driver, receiver, cx) = child.run_turn(Duration::from_secs(20), force);
    let (result, elapsed) = child.runtime.block_on(async {
        let execute = driver.run_turn(one_turn::hello(), cx);
        tokio::pin!(execute);
        // Force only once backpressure is observed: the channel is full, so the
        // adapter's next delivery waits for capacity (W4-H Sol r2).
        let observed = tokio::time::Instant::now() + Duration::from_secs(10);
        while receiver.len() < via_adapters::OBSERVATION_ITEMS {
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
            receiver.len(),
            via_adapters::OBSERVATION_ITEMS,
            "the observation channel must stay full"
        );
        let forced_at = tokio::time::Instant::now();
        force_tx.send_replace(Some(tokio::time::Instant::now()));
        let result = execute.await.outcome;
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
    // Never set: this failure is the acquisition deadline, not a force.
    let (_force, force) = tokio::sync::watch::channel(None);
    let (driver, _receiver, cx) = child.run_turn(Duration::from_secs(3), force);
    let result = child.runtime.block_on(async {
        let execute = driver.run_turn(one_turn::hello(), cx);
        // The stand-in writes flags beside its anchor directory.
        let flags = root.clone();
        // The vendor launched and wrote before the deadline.
        let (end, ()) = tokio::join!(execute, stand_in_anchor::wait_flag(&flags, "wrote"));
        end.outcome
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
    let (force_tx, force) = tokio::sync::watch::channel(None);
    let (driver, _receiver, cx) = child.run_turn(deadline, force);
    let result = child.runtime.block_on(async {
        let deadline = cx.wall.instant();
        let execute = driver.run_turn(one_turn::hello(), cx);
        let (end, ()) = tokio::join!(execute, async {
            force_at(&root, deadline).await;
            force_tx.send_replace(Some(tokio::time::Instant::now()));
        });
        end.outcome
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
