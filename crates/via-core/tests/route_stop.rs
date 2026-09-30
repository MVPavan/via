//! Task 3 S1 Route behaviour under a stop order, F21 and Store causes
//! (design §2, §7.2 rows 3 and 6), through the real Route, Wire and Host with
//! a scripted vendor. Like `route_stream.rs`, these run one layer up because
//! Route cannot open a Store; each case re-executes this binary.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use tokio::sync::watch;
use via_adapters::{
    AdapterError, AdapterRuntime, AdapterRuntimeConfig, Cleanup, Deadline, FakeConfig,
    FakeObservation, FakeTerminalEvidence, Observation, RouteError, RouteFailure, RuntimeConfig,
    SessionId, StopCause, StopOrder, StoreFailure, TurnNumber, VendorTerminalStatus,
};
use via_store::{SpawnRecord, Store, failpoint};

const SESSION: &str = "s_0123456789ab";
const CHILD: &str = "VIA_ROUTE_STOP_CHILD";
const TOKEN: &str = "route-stop-failpoint-token";
const CHILD_LIMIT: Duration = Duration::from_secs(60);

const ACCEPTED: &str = r#"{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}"#;

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

/// Runs `name` again in a child process whose fake vendor is `script`.
fn run_child(name: &str, script: &str) {
    let root = tempfile::tempdir().unwrap();
    for part in ["state", "runtime", "sync", "points"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.path().join(part))
            .unwrap();
    }
    let vendor = root.path().join("vendor.sh");
    fs::write(&vendor, format!("#!/bin/sh\n{script}")).unwrap();
    fs::set_permissions(&vendor, fs::Permissions::from_mode(0o700)).unwrap();
    let scenario = root.path().join("scenario.ndjson");
    fs::write(&scenario, "").unwrap();
    let mut child = Command::new(env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, root.path())
        .env("VIA_FAKE_AGENT_BINARY", &vendor)
        .env("VIA_FAKE_SCENARIO", &scenario)
        .env("VIA_FAKE_SYNC_DIR", root.path().join("sync"))
        .spawn()
        .unwrap();
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

fn child_root() -> Option<PathBuf> {
    env::var_os(CHILD).map(PathBuf::from)
}

/// The child's Store, adapter and failpoint controller.
struct Child {
    root: PathBuf,
    _store: Store,
    adapter: AdapterRuntime,
    runtime: tokio::runtime::Runtime,
}

type Outcome = Result<FakeTerminalEvidence, AdapterError>;

impl Child {
    fn open(root: &Path) -> Self {
        failpoint::activate(&root.join("points"), TOKEN).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = Store::open(&root.join("state")).unwrap();
        runtime
            .block_on(store.client().commit_spawn(SpawnRecord {
                session_id: SessionId::try_from(SESSION).unwrap(),
                handle_hash: [7_u8; 32],
                receipt: serde_json::json!({"state":"queued"}),
                params: serde_json::json!({"harness":"fake"}),
                label: None,
                prompt: "hello".into(),
                effective: serde_json::json!({"deadlines":{"wall_ms":1}}),
                initial_event: serde_json::json!({"seq":1,"type":"turn.queued","turn":1,"at":"2026-01-01T00:00:00.000Z"}),
            }))
            .unwrap();
        let adapter = AdapterRuntime::new(
            AdapterRuntimeConfig {
                runtime: RuntimeConfig {
                    anchor_binary: via_binary(),
                    anchor_dir: root.join("runtime"),
                },
                fake: FakeConfig::from_environment().unwrap(),
            },
            store.runtime_resources(),
        )
        .unwrap();
        Self {
            root: root.to_path_buf(),
            _store: store,
            adapter,
            runtime,
        }
    }

    fn arm(&self, point: &str, action: &str) {
        self.arm_at(point, 1, action);
    }

    fn arm_at(&self, point: &str, occurrence: u64, action: &str) {
        let command = serde_json::json!({"token":TOKEN,"occurrence":occurrence,"action":action});
        fs::write(
            self.root.join("points").join(format!("{point}.json")),
            command.to_string(),
        )
        .unwrap();
    }

    /// Runs turn 1 under `turn`, calling `on_observation` with the turn's
    /// stop-order sender as each observation arrives.
    fn execute(
        &self,
        turn: Duration,
        stop: (
            watch::Sender<Option<StopOrder>>,
            watch::Receiver<Option<StopOrder>>,
        ),
        mut on_observation: impl FnMut(&watch::Sender<Option<StopOrder>>, &FakeObservation),
    ) -> Outcome {
        let (sender, mut receiver) = via_adapters::observation_channel();
        let deadline = Deadline::at(tokio::time::Instant::now() + turn);
        let (_force, force) = watch::channel(None);
        let (order, orders) = stop;
        self.runtime.block_on(async {
            let execute = self.adapter.execute(
                SessionId::try_from(SESSION).unwrap(),
                TurnNumber::try_from(1).unwrap(),
                ("hello".to_owned(), self.adapter.fake_cwd().to_path_buf()),
                sender,
                via_adapters::TurnActivity::new(tokio::time::Instant::now()),
                deadline,
                force,
                orders,
                Box::new(()),
            );
            tokio::pin!(execute);
            loop {
                tokio::select! {
                    Some(admitted) = receiver.recv() => on_observation(&order, &admitted.observation),
                    result = &mut execute => break result,
                }
            }
        })
    }

    fn sync(&self, name: &str) -> PathBuf {
        self.root.join("sync").join(name)
    }
}

fn order(force_after: Duration) -> StopOrder {
    let force_at = tokio::time::Instant::now() + force_after;
    StopOrder {
        cause: StopCause::Cancel,
        requested_at: "2026-01-01T00:00:00.000Z".to_owned(),
        force_at: Deadline::at(force_at),
        close_by: Deadline::at(force_at + Duration::from_secs(3)),
    }
}

fn route_failure(outcome: Outcome) -> RouteFailure {
    match outcome {
        Err(AdapterError::Route(failure)) => failure,
        Err(other) => panic!("not a route failure: {other}"),
        Ok(evidence) => panic!("unexpected terminal {:?}", evidence.status),
    }
}

/// Design §2 rule 1: an order already set when the turn reaches Route
/// starts nothing: `Stopped`, not launched, and no anchor intent exists.
#[test]
fn an_order_set_before_launch_starts_nothing() {
    let Some(root) = child_root() else {
        return run_child(
            "an_order_set_before_launch_starts_nothing",
            "touch \"$VIA_FAKE_SYNC_DIR/launched\"\n",
        );
    };
    let child = Child::open(&root);
    let outcome = child.execute(
        Duration::from_secs(10),
        watch::channel(Some(order(Duration::ZERO))),
        |_, _| {},
    );
    let failure = route_failure(outcome);
    assert!(
        matches!(failure.cause, RouteError::Stopped { .. }),
        "{failure:?}"
    );
    assert!(!failure.launched && failure.cleanup.is_none());
    assert!(!child.sync("launched").exists());
}

/// Design §2 rule 3: after the start message an order sends one interrupt; the
/// vendor's `interrupt_ack` is control evidence, not a protocol error, and
/// its `interrupted` terminal ends the turn on the normal path.
#[test]
fn an_order_sends_one_interrupt_and_the_terminal_ends_the_turn() {
    let Some(root) = child_root() else {
        return run_child(
            "an_order_sends_one_interrupt_and_the_terminal_ends_the_turn",
            &format!(
                "read -r start\nprintf '%s\\n' '{ACCEPTED}'\nread -r interrupt\n\
                 printf '%s\\n' \"$interrupt\" >> \"$VIA_FAKE_SYNC_DIR/controls\"\n\
                 printf '%s\\n' '{{\"type\":\"interrupt_ack\",\"id\":2,\"vendor_turn_id\":\"fake-turn-1\"}}'\n\
                 printf '%s\\n' '{{\"type\":\"terminal\",\"vendor_turn_id\":\"fake-turn-1\",\"status\":\"interrupted\",\"final_text\":\"\",\"stop_reason\":\"interrupted\"}}'\n\
                 while read -r more; do printf '%s\\n' \"$more\" >> \"$VIA_FAKE_SYNC_DIR/controls\"; done\n"
            ),
        );
    };
    let child = Child::open(&root);
    let outcome = child.execute(
        Duration::from_secs(10),
        watch::channel(None),
        |order, observation| {
            if matches!(observation, FakeObservation::Accepted(_)) {
                order.send_replace(Some(self::order(Duration::from_secs(10))));
            }
        },
    );
    let evidence = outcome.unwrap();
    assert_eq!(evidence.status, VendorTerminalStatus::Interrupted);
    assert_eq!(evidence.cleanup, Cleanup::Quiescent);
    let controls = fs::read_to_string(child.sync("controls")).unwrap();
    assert_eq!(
        controls,
        "{\"type\":\"interrupt\",\"id\":2,\"vendor_turn_id\":\"fake-turn-1\"}\n"
    );
}

/// Design §2 rule 3: at `force_at` with no terminal, Route force-closes the
/// group under `close_by`, drains it and reports `Stopped` with Host's
/// force evidence and cleanup.
#[test]
fn force_at_without_a_terminal_force_closes_the_group() {
    let Some(root) = child_root() else {
        return run_child(
            "force_at_without_a_terminal_force_closes_the_group",
            &format!("read -r start\nprintf '%s\\n' '{ACCEPTED}'\nexec sleep 60\n"),
        );
    };
    let child = Child::open(&root);
    let started = Instant::now();
    let outcome = child.execute(
        Duration::from_secs(20),
        watch::channel(None),
        |order, observation| {
            if matches!(observation, FakeObservation::Accepted(_)) {
                order.send_replace(Some(self::order(Duration::from_millis(300))));
            }
        },
    );
    let failure = route_failure(outcome);
    assert!(
        matches!(failure.cause, RouteError::Stopped { .. }),
        "{failure:?}"
    );
    assert!(failure.launched && failure.forced, "{failure:?}");
    assert_eq!(failure.cleanup, Some(via_adapters::WireCleanup::Quiescent));
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// F21 (design §2): the vendor exits after an unterminated last line. The
/// partial bytes are kept in `undecoded.bin` and named by the failure (Task 4
/// design §7.3), and the Host-confirmed exit makes the turn `ProcessExited`,
/// not a protocol failure.
#[test]
fn f21_exit_after_an_unterminated_line_is_process_exited() {
    let Some(root) = child_root() else {
        return run_child(
            "f21_exit_after_an_unterminated_line_is_process_exited",
            &format!("read -r start\nprintf '%s\\n' '{ACCEPTED}'\nprintf 'partial'\nexit 3\n"),
        );
    };
    let child = Child::open(&root);
    let failure =
        route_failure(child.execute(Duration::from_secs(10), watch::channel(None), |_, _| {}));
    assert!(
        matches!(failure.cause, RouteError::ProcessExited { .. }),
        "{failure:?}"
    );
    assert_eq!(failure.exit.and_then(|exit| exit.code), Some(3));
    let kept = root.join(format!("state/evidence/{SESSION}/1/undecoded.bin"));
    assert_eq!(fs::read(&kept).unwrap(), b"partial");
    let note = failure.undecoded.clone().unwrap_or_default();
    assert!(
        note.contains("7 bytes") && note.contains(&kept.display().to_string()),
        "{note}"
    );
}

/// Design §2 rule 3 [r1.23]: a decoded terminal whose finalization outlives
/// the wall deadline is still returned, with cleanup from Host's force
/// close, never `Deadline`.
#[test]
fn a_decoded_terminal_survives_wall_expiry_in_finalization() {
    let Some(root) = child_root() else {
        return run_child(
            "a_decoded_terminal_survives_wall_expiry_in_finalization",
            &format!(
                "read -r start\nprintf '%s\\n' '{ACCEPTED}'\n\
                 printf '%s\\n' '{{\"type\":\"terminal\",\"vendor_turn_id\":\"fake-turn-1\",\"status\":\"completed\",\"final_text\":\"done\",\"stop_reason\":\"end_turn\"}}'\n\
                 exec sleep 60\n"
            ),
        );
    };
    let child = Child::open(&root);
    // The final text arrives as `final_text` pieces before the terminal
    // (Task 4 design §2.3).
    let mut text = String::new();
    let evidence = child
        .execute(
            Duration::from_secs(2),
            watch::channel(None),
            |_, observation| {
                if let FakeObservation::Data {
                    observation: Observation::FinalText(piece),
                } = observation
                {
                    text.push_str(piece);
                }
            },
        )
        .unwrap();
    assert_eq!(evidence.status, VendorTerminalStatus::Completed);
    assert_eq!(text, "done");
    assert_eq!(evidence.cleanup, Cleanup::Quiescent);
}

/// Design §7.2 row 3: an anchor intent that is not committed starts no
/// process; Route reports `Store` with kind `NotCommitted`, not launched and
/// with no anchor intent.
#[test]
fn an_anchor_intent_failure_reports_store_not_committed() {
    let Some(root) = child_root() else {
        return run_child(
            "an_anchor_intent_failure_reports_store_not_committed",
            "touch \"$VIA_FAKE_SYNC_DIR/launched\"\n",
        );
    };
    let child = Child::open(&root);
    child.arm("store.journal.anchor_intent", "fail_io");
    let failure =
        route_failure(child.execute(Duration::from_secs(10), watch::channel(None), |_, _| {}));
    assert!(
        matches!(
            failure.cause,
            RouteError::Store {
                kind: StoreFailure::NotCommitted,
                ..
            }
        ),
        "{failure:?}"
    );
    assert!(!failure.launched && failure.cleanup.is_none());
    assert!(!child.sync("launched").exists());
}

/// Design §2 rule 1 [r1.8]: an order set while the acquisition is past its
/// ARM intent but before the gate stops the launch there: `Stopped`, not
/// launched, with the failed acquisition's own absence proof.
#[cfg(feature = "test-failpoints")]
#[test]
fn an_order_at_the_gate_stops_the_launch_with_acquisition_evidence() {
    let Some(root) = child_root() else {
        return run_child(
            "an_order_at_the_gate_stops_the_launch_with_acquisition_evidence",
            "touch \"$VIA_FAKE_SYNC_DIR/launched\"\n",
        );
    };
    let child = Child::open(&root);
    child.arm("host.anchor.after_arm_intent_commit", "pause");
    let points = root.join("points");
    let (order, orders) = watch::channel(None);
    let waiter = std::thread::spawn({
        let order = order.clone();
        move || {
            let ack = points.join("host.anchor.after_arm_intent_commit.1.ack");
            while !ack.exists() {
                std::thread::sleep(Duration::from_millis(5));
            }
            order.send_replace(Some(self::order(Duration::from_secs(10))));
            fs::write(
                points.join("host.anchor.after_arm_intent_commit.1.release"),
                b"",
            )
            .unwrap();
        }
    });
    let failure = route_failure(child.execute(Duration::from_secs(10), (order, orders), |_, _| {}));
    waiter.join().unwrap();
    assert!(
        matches!(failure.cause, RouteError::Stopped { .. }),
        "{failure:?}"
    );
    assert!(!failure.launched, "{failure:?}");
    assert_eq!(failure.cleanup, Some(via_adapters::WireCleanup::Quiescent));
    assert!(!child.sync("launched").exists());
}
