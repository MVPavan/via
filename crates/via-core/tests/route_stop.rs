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
    AdapterError, Cleanup, Deadline, OBSERVATION_ITEMS, Observation, RouteError, RouteFailure,
    SessionId, StopCause, StopOrder, StoreFailure, TurnEnd, VendorTerminalStatus,
};
use via_store::{SpawnRecord, Store, failpoint};

#[path = "support/one_turn.rs"]
mod one_turn;

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
    run_child_with(name, script, &[]);
}

/// [`run_child`] with `env` added to the child's environment.
fn run_child_with(name: &str, script: &str, env: &[(&str, &str)]) {
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
    // A valid scenario the shell vendor never reads (decision H2).
    let scenario = root.path().join("scenario.json");
    fs::write(&scenario, r#"{"scripts":[]}"#).unwrap();
    let mut child = Command::new(env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, root.path())
        .env("VIA_FAKE_AGENT_BINARY", &vendor)
        .env("VIA_FAKE_SCENARIO", &scenario)
        .env("VIA_FAKE_SYNC_DIR", root.path().join("sync"))
        .envs(env.iter().copied())
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
    adapter: one_turn::OneTurn,
    runtime: tokio::runtime::Runtime,
}

type Outcome = TurnEnd;

/// The status of the turn's kept terminal and its cleanup, for a turn
/// that ended without a failure.
fn completed(outcome: &Outcome) -> (VendorTerminalStatus, Cleanup) {
    let evidence = outcome.outcome.as_ref().unwrap();
    (outcome.terminal.as_ref().unwrap().status, evidence.cleanup)
}

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
        let adapter = one_turn::OneTurn::new(&store, root, via_binary());
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
        mut on_observation: impl FnMut(&watch::Sender<Option<StopOrder>>, &Observation),
    ) -> Outcome {
        let (driver, mut receiver) = self.adapter.session(SESSION, &self.root);
        let deadline = Deadline::at(tokio::time::Instant::now() + turn);
        let (_force, force) = watch::channel(None);
        let (order, orders) = stop;
        self.runtime.block_on(async {
            let cx = one_turn::turn_cx(driver.prepare(), deadline, force, orders);
            let execute = driver.run_turn(one_turn::hello(), cx);
            tokio::pin!(execute);
            loop {
                tokio::select! {
                    Some(admitted) = receiver.recv() => {
                        on_observation(&order, &admitted.item.observation);
                    }
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
    match outcome.outcome {
        Err(AdapterError::Route(failure)) => failure,
        Err(other) => panic!("not a route failure: {other}"),
        Ok(_) => panic!("unexpected terminal {:?}", outcome.terminal),
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
            if matches!(observation, Observation::Accepted(_)) {
                order.send_replace(Some(self::order(Duration::from_secs(10))));
            }
        },
    );
    assert_eq!(
        completed(&outcome),
        (VendorTerminalStatus::Interrupted, Cleanup::Quiescent)
    );
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
            if matches!(observation, Observation::Accepted(_)) {
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
    let outcome = child.execute(
        Duration::from_secs(2),
        watch::channel(None),
        |_, observation| {
            if let Observation::FinalText(piece) = observation {
                text.push_str(piece);
            }
        },
    );
    assert_eq!(
        completed(&outcome),
        (VendorTerminalStatus::Completed, Cleanup::Quiescent)
    );
    assert_eq!(text, "done");
}

/// S1 critic r2 finding 2 (design §2 rule 3 [r1.23], Task 4 design §2.3):
/// a terminal Route decoded but had not yet handed over when the wall
/// deadline passed is delivered before the late result returns, so its
/// final text reaches Core, with the late force close's evidence. See
/// [`held_terminal`].
#[test]
fn a_held_terminal_is_delivered_after_wall_expiry() {
    let name = "a_held_terminal_is_delivered_after_wall_expiry";
    let Some(root) = child_root() else {
        return run_child(name, &held_terminal_script(false));
    };
    let run = held_terminal(&Child::open(&root), true, None);
    assert_eq!(
        completed(&run.outcome),
        (VendorTerminalStatus::Completed, Cleanup::Quiescent)
    );
    assert_eq!(run.text, "done", "a completed turn lost its final text");
}

/// S1-runtime2 fix rounds 1 and 2 (design §2 rules 3 and 4): the late
/// path's delivery is delivery-only. The daemon force, raised once Route
/// acknowledged the late path with the terminal held
/// (`routes.late.entered`) and before anything is drained, does not drop
/// the terminal: its final text reaches Core, and the result is
/// `ForceStopped` with the late close's evidence, never `Overflow`.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_force_during_late_delivery_keeps_the_held_terminal() {
    let name = "a_force_during_late_delivery_keeps_the_held_terminal";
    let Some(root) = child_root() else {
        return run_child(name, &held_terminal_script(false));
    };
    let run = held_terminal(&Child::open(&root), true, Some(Late::Force));
    assert!(run.late_entered, "the late path was not taken");
    let failure = route_failure(run.outcome);
    assert!(
        matches!(failure.cause, RouteError::ForceStopped { .. }),
        "{failure:?}"
    );
    assert_eq!(
        failure.cleanup,
        Some(via_adapters::WireCleanup::Quiescent),
        "{failure:?}"
    );
    assert!(failure.forced, "{failure:?}");
    assert_eq!(
        run.text, "done",
        "the forced late delivery dropped the terminal"
    );
}

/// S1-runtime2 fix round 2: the connection latch, set while Route holds
/// the terminal on the late path, does not drop it either. After Route
/// acknowledged the late path, the vendor writes a 2 MiB line; its write
/// returning (`wrote`) proves the reader passed the 1 MiB message bound,
/// which latches. Only then is the late path released and the channel
/// drained: the terminal is delivered and returned `completed` with the
/// late close's evidence.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_latch_during_late_delivery_keeps_the_held_terminal() {
    let name = "a_latch_during_late_delivery_keeps_the_held_terminal";
    let Some(root) = child_root() else {
        return run_child(name, &held_terminal_script(true));
    };
    let run = held_terminal(&Child::open(&root), true, Some(Late::Latch));
    assert!(run.late_entered, "the late path was not taken");
    assert_eq!(
        completed(&run.outcome),
        (VendorTerminalStatus::Completed, Cleanup::Quiescent)
    );
    assert_eq!(
        run.text, "done",
        "the latched late delivery dropped the terminal"
    );
}

/// S1 critic r2 finding 2, S1-runtime2 fix round 2 (design §2 rule 3,
/// runtime §5.2): a held terminal that cannot reach the hop by the late
/// path's one absolute deadline is a delivery failure, `Overflow`, never
/// `Deadline` nor a completion. The force close ran meanwhile under the
/// same deadline: its evidence is kept. The late path is proven taken by
/// its acknowledgement (`routes.late.entered`).
#[cfg(feature = "test-failpoints")]
#[test]
fn an_undelivered_held_terminal_is_not_a_completion() {
    let name = "an_undelivered_held_terminal_is_not_a_completion";
    let Some(root) = child_root() else {
        return run_child(name, &held_terminal_script(false));
    };
    let run = held_terminal(&Child::open(&root), false, Some(Late::Release));
    assert!(run.late_entered, "the late path was not taken");
    let failure = route_failure(run.outcome);
    assert!(
        matches!(failure.cause, RouteError::Overflow { .. }),
        "{failure:?}"
    );
    assert_eq!(
        failure.cleanup,
        Some(via_adapters::WireCleanup::Quiescent),
        "{failure:?}"
    );
    assert!(failure.forced, "{failure:?}");
    assert_eq!(run.text, "");
}

/// What the test does once Route acknowledges the late path
/// (`routes.late.entered`, paused), before it releases it.
#[cfg(feature = "test-failpoints")]
#[derive(Clone, Copy, PartialEq)]
enum Late {
    /// Nothing.
    Release,
    /// Raises the daemon force.
    Force,
    /// Lets the vendor write the line that latches the connection, and
    /// waits until it was read.
    Latch,
}

#[cfg(not(feature = "test-failpoints"))]
#[derive(Clone, Copy, PartialEq)]
enum Late {}

/// Acts on the late path's acknowledgement; true once Route may be
/// released.
#[cfg(feature = "test-failpoints")]
fn late_act(
    child: &Child,
    forcing: &watch::Sender<Option<tokio::time::Instant>>,
    late: Late,
    latch_asked: &mut bool,
) -> bool {
    match late {
        Late::Release => true,
        Late::Force => {
            forcing.send_replace(Some(tokio::time::Instant::now()));
            true
        }
        Late::Latch => {
            if !*latch_asked {
                *latch_asked = true;
                fs::write(child.sync("latch"), b"").unwrap();
            }
            child.sync("wrote").exists()
        }
    }
}

#[cfg(not(feature = "test-failpoints"))]
fn late_act(
    _: &Child,
    _: &watch::Sender<Option<tokio::time::Instant>>,
    late: Late,
    _: &mut bool,
) -> bool {
    match late {}
}

/// The acceptance and 1,023 texts, then, once `rest` exists, two texts and
/// the terminal, then a live vendor. With `latch` it next waits for
/// `latch`, writes a 2 MiB line without a newline and creates `wrote`.
///
/// The first batch is 1,024 messages, what Wire's stdout queue holds
/// (runtime §8): a reader polled only after the whole batch is in the
/// pipe queues it all before Route takes any. All 1,027 at once overflow
/// it, so the rest waits until the first batch has left the queue.
fn held_terminal_script(latch: bool) -> String {
    let latch = if latch {
        "while [ ! -f \"$VIA_FAKE_SYNC_DIR/latch\" ]; do sleep 0.01; done\n\
         head -c 2097152 /dev/zero | tr '\\0' x\n\
         touch \"$VIA_FAKE_SYNC_DIR/wrote\"\n"
    } else {
        ""
    };
    let text = r#"printf '%s\n' '{"type":"text","vendor_turn_id":"fake-turn-1","text":"x"}'"#;
    format!(
        "read -r start\nprintf '%s\\n' '{ACCEPTED}'\ni=0\n\
         while [ \"$i\" -lt 1023 ]; do\n{text}\ni=$((i+1))\ndone\n\
         while [ ! -f \"$VIA_FAKE_SYNC_DIR/rest\" ]; do sleep 0.01; done\n\
         {text}\n{text}\n\
         printf '%s\\n' '{{\"type\":\"terminal\",\"vendor_turn_id\":\"fake-turn-1\",\"status\":\"completed\",\"final_text\":\"done\",\"stop_reason\":\"end_turn\"}}'\n\
         {latch}exec sleep 60\n"
    )
}

/// One [`held_terminal`] run.
struct HeldRun {
    outcome: Outcome,
    /// The final text Core received.
    text: String,
    /// Route acknowledged the late path.
    #[cfg_attr(
        not(feature = "test-failpoints"),
        expect(
            dead_code,
            reason = "only the failpoint tests take the late path's seam"
        )
    )]
    late_entered: bool,
}

/// Runs [`held_terminal_script`] under a 3 s wall deadline and drains
/// nothing before it: the acceptance and 1,023 texts fill Core's 1,024-item
/// channel, which releases the script's rest; the Adapter's delivery holds
/// the 1,024th text, the hop of one the 1,025th, and Route holds the
/// terminal. With `late`, Route pauses at `routes.late.entered`; once it
/// acknowledged, the test acts, releases it and then drains, if `drain`.
/// Without, it drains from the deadline, if `drain`.
fn held_terminal(child: &Child, drain: bool, late: Option<Late>) -> HeldRun {
    #[cfg(feature = "test-failpoints")]
    if late.is_some() {
        child.arm("routes.late.entered", "pause");
    }
    let (driver, mut receiver) = child.adapter.session(SESSION, &child.root);
    let expiry = tokio::time::Instant::now() + Duration::from_secs(3);
    let (forcing, forced) = watch::channel(None);
    let (_order, orders) = watch::channel(None);
    let points = child.root.join("points");
    child.runtime.block_on(async {
        let cx = one_turn::turn_cx(driver.prepare(), Deadline::at(expiry), forced, orders);
        let execute = driver.run_turn(one_turn::hello(), cx);
        tokio::pin!(execute);
        let mut text = String::new();
        let mut collect = |observation: Observation| {
            if let Observation::FinalText(piece) = observation {
                text.push_str(&piece);
            }
        };
        let mut draining = false;
        let mut rest = false;
        let mut late_entered = false;
        // Late path: waiting for Route's acknowledgement, then (latch only)
        // for the vendor's write.
        let mut waiting = late;
        let mut latch_asked = false;
        let outcome = loop {
            tokio::select! {
                () = tokio::time::sleep_until(expiry), if drain && late.is_none() && !draining => {
                    draining = true;
                }
                // Core's channel is full: the first batch has left Wire's
                // queue.
                () = tokio::time::sleep(Duration::from_millis(5)), if !rest => {
                    if receiver.len() == OBSERVATION_ITEMS {
                        fs::write(child.sync("rest"), b"").unwrap();
                        rest = true;
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(5)), if waiting.is_some() => {
                    if !late_entered {
                        late_entered = points.join("routes.late.entered.1.ack").exists();
                        if !late_entered {
                            continue;
                        }
                    }
                    if let Some(late) = waiting
                        && !late_act(child, &forcing, late, &mut latch_asked)
                    {
                        continue;
                    }
                    fs::write(points.join("routes.late.entered.1.release"), b"").unwrap();
                    waiting = None;
                    draining = drain;
                }
                Some(admitted) = receiver.recv(), if draining => collect(admitted.item.observation),
                outcome = &mut execute => break outcome,
            }
        };
        while let Ok(admitted) = receiver.try_recv() {
            collect(admitted.item.observation);
        }
        HeldRun {
            outcome,
            text,
            late_entered,
        }
    })
}

/// What the test does once the stalled turn's interrupt reached the vendor.
#[cfg(feature = "test-failpoints")]
#[derive(Clone, Copy)]
enum StallAct {
    /// Nothing: the vendor answers with its interrupted terminal.
    Answer,
    /// The daemon force.
    Force,
    /// The session's `close(Force)`.
    Close,
    /// The daemon force, once the stall's own escalation's `Stop` reached
    /// the anchor (paused at `host.anchor.stop_received`).
    ForceInCleanup,
}

/// The acceptance and 1,023 texts, then, once `rest` exists, three texts,
/// which the stalled Adapter never takes; then the vendor reads the
/// interrupt, records it in `interrupt` and runs `then`.
#[cfg(feature = "test-failpoints")]
fn stall_script(then: &str) -> String {
    let text = r#"printf '%s\n' '{"type":"text","vendor_turn_id":"fake-turn-1","text":"x"}'"#;
    format!(
        "read -r start\nprintf '%s\\n' '{ACCEPTED}'\ni=0\n\
         while [ \"$i\" -lt 1023 ]; do\n{text}\ni=$((i+1))\ndone\n\
         while [ ! -f \"$VIA_FAKE_SYNC_DIR/rest\" ]; do sleep 0.01; done\n\
         {text}\n{text}\n{text}\n\
         read -r interrupt\n\
         printf '%s\\n' \"$interrupt\" > \"$VIA_FAKE_SYNC_DIR/interrupt\"\n{then}"
    )
}

/// The vendor's answer to the interrupt: its acknowledgement and an
/// interrupted terminal, then stdout open until stdin's EOF.
#[cfg(feature = "test-failpoints")]
const ANSWER: &str = "printf '%s\\n' '{\"type\":\"interrupt_ack\",\"id\":2,\"vendor_turn_id\":\"fake-turn-1\"}'\n\
     printf '%s\\n' '{\"type\":\"terminal\",\"vendor_turn_id\":\"fake-turn-1\",\"status\":\"interrupted\",\"final_text\":\"\",\"stop_reason\":\"interrupted\"}'\n\
     while read -r more; do :; done\n";

/// The lowered stall bound of the stall cases.
#[cfg(feature = "test-failpoints")]
const STALL_ENV: &[(&str, &str)] = &[("VIA_TEST_EVENT_STALL_MS", "300")];

/// Runs [`stall_script`] under a 20 s wall and never drains Core's
/// channel: the Adapter's delivery stalls past the lowered bound and
/// closes the hop (C2 A1). Once the interrupt reached the vendor the test
/// acts; returns the outcome and how long after that it ended.
#[cfg(feature = "test-failpoints")]
fn stalled_turn(child: &Child, act: StallAct) -> (Outcome, Duration) {
    let (driver, receiver) = child.adapter.session(SESSION, &child.root);
    let wall = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(20));
    let (forcing, forced) = watch::channel(None);
    let (_order, orders) = watch::channel(None);
    child.runtime.block_on(async {
        let cx = one_turn::turn_cx(driver.prepare(), wall, forced, orders);
        let execute = driver.run_turn(one_turn::hello(), cx);
        tokio::pin!(execute);
        let mut rest = false;
        loop {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(5)) => {
                    // Core's channel is full: the first batch left Wire's queue.
                    if !rest && receiver.len() == OBSERVATION_ITEMS {
                        fs::write(child.sync("rest"), b"").unwrap();
                        rest = true;
                    }
                    if child.sync("interrupt").exists() {
                        break;
                    }
                }
                outcome = &mut execute => panic!("ended before the interrupt: {:?}", outcome.outcome.map(|_| ())),
            }
        }
        let acted = Instant::now();
        let outcome = match act {
            StallAct::Answer => execute.await,
            StallAct::Force => {
                forcing.send_replace(Some(tokio::time::Instant::now()));
                execute.await
            }
            StallAct::Close => {
                let by = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(2));
                let close = driver.close(via_adapters::CloseMode::Force, by);
                tokio::join!(execute, close).0
            }
            StallAct::ForceInCleanup => {
                let points = child.root.join("points");
                loop {
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_millis(5)) => {
                            if points.join("host.anchor.stop_received.1.ack").exists() {
                                break;
                            }
                        }
                        outcome = &mut execute => panic!("ended before its cleanup's Stop: {:?}", outcome.outcome.map(|_| ())),
                    }
                }
                forcing.send_replace(Some(tokio::time::Instant::now()));
                fs::write(points.join("host.anchor.stop_received.1.release"), b"").unwrap();
                execute.await
            }
        };
        (outcome, acted.elapsed())
    })
}

/// Review r2 #3 (AD4, C2 A1): the stalled turn's interrupt goes through
/// the protocol's path, and the interrupted terminal the vendor answers
/// with, decoded while the hop is closed, is kept in the turn's end; the
/// turn fails `overflow`.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_stalled_turn_keeps_its_interrupted_terminal() {
    let name = "a_stalled_turn_keeps_its_interrupted_terminal";
    let Some(root) = child_root() else {
        return run_child_with(name, &stall_script(ANSWER), STALL_ENV);
    };
    let child = Child::open(&root);
    let (outcome, _) = stalled_turn(&child, StallAct::Answer);
    let interrupt = fs::read_to_string(child.sync("interrupt")).unwrap();
    assert_eq!(
        interrupt,
        "{\"type\":\"interrupt\",\"id\":2,\"vendor_turn_id\":\"fake-turn-1\"}\n"
    );
    assert_eq!(
        outcome.terminal.as_ref().map(|terminal| terminal.status),
        Some(VendorTerminalStatus::Interrupted),
        "the decoded terminal was lost"
    );
    let failure = route_failure(outcome);
    assert!(
        matches!(failure.cause, RouteError::Overflow { .. }),
        "{failure:?}"
    );
}

/// Review r2 #1 (S1 rule 4): the daemon force, raised while the stalled
/// turn waits for its interrupt's terminal, governs the outcome:
/// `ForceStopped`, at once.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_force_during_a_stall_is_force_stopped() {
    let name = "a_force_during_a_stall_is_force_stopped";
    let Some(root) = child_root() else {
        return run_child_with(name, &stall_script("exec sleep 60\n"), STALL_ENV);
    };
    let (outcome, elapsed) = stalled_turn(&Child::open(&root), StallAct::Force);
    let failure = route_failure(outcome);
    assert!(
        matches!(failure.cause, RouteError::ForceStopped { .. }),
        "{failure:?}"
    );
    assert!(failure.forced, "{failure:?}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "force took {elapsed:?}"
    );
}

/// Review r2 #2: the session's `close(Force)` during a stall is served:
/// its order's `force_at` (at once) force-closes the group under its
/// `close_by`, long before the stall's own escalation (3 s); the close
/// governs the outcome, `Stopped`.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_close_during_a_stall_stops_at_its_force_at() {
    let name = "a_close_during_a_stall_stops_at_its_force_at";
    let Some(root) = child_root() else {
        return run_child_with(name, &stall_script("exec sleep 60\n"), STALL_ENV);
    };
    let (outcome, elapsed) = stalled_turn(&Child::open(&root), StallAct::Close);
    let failure = route_failure(outcome);
    assert!(
        matches!(failure.cause, RouteError::Stopped { .. }),
        "{failure:?}"
    );
    assert!(failure.forced, "{failure:?}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "close took {elapsed:?}"
    );
}

/// Review r3 #1 (S1 rule 4): the stalled vendor withholds its terminal,
/// so the stall escalates at its `force_at` (`Overflow`) and Host's force
/// close begins; the daemon force, raised while that close's `Stop` is
/// held at the anchor, still decides the outcome: `ForceStopped`.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_force_during_stall_cleanup_is_force_stopped() {
    let name = "a_force_during_stall_cleanup_is_force_stopped";
    let Some(root) = child_root() else {
        return run_child_with(name, &stall_script("exec sleep 60\n"), STALL_ENV);
    };
    let child = Child::open(&root);
    child.arm("host.anchor.stop_received", "pause");
    let (outcome, _) = stalled_turn(&child, StallAct::ForceInCleanup);
    let failure = route_failure(outcome);
    assert!(
        matches!(failure.cause, RouteError::ForceStopped { .. }),
        "{failure:?}"
    );
}

/// Review r3 #2: the turn is dropped once its route work was spawned but
/// before the start is written (the launch is held at
/// `host.anchor.arm_received`): the closed hop fails the turn at once, as
/// before the stall branch, so the vendor reads neither the start nor an
/// interrupt.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_turn_dropped_before_its_start_writes_nothing() {
    let name = "a_turn_dropped_before_its_start_writes_nothing";
    let Some(root) = child_root() else {
        return run_child_with(
            name,
            "touch \"$VIA_FAKE_SYNC_DIR/launched\"\n\
             while read -r line; do printf '%s\\n' \"$line\" >> \"$VIA_FAKE_SYNC_DIR/input\"; done\n",
            STALL_ENV,
        );
    };
    let child = Child::open(&root);
    child.arm("host.anchor.arm_received", "pause");
    let points = child.root.join("points");
    let (driver, _receiver) = child.adapter.session(SESSION, &child.root);
    let wall = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(20));
    let (_force, forced) = watch::channel(None);
    let (_order, orders) = watch::channel(None);
    child.runtime.block_on(async {
        let cx = one_turn::turn_cx(driver.prepare(), wall, forced, orders);
        let mut execute = Box::pin(driver.run_turn(one_turn::hello(), cx));
        // Polled until the launch is held: the route work runs on its own.
        while !points.join("host.anchor.arm_received.1.ack").exists() {
            assert!(
                tokio::time::timeout(Duration::from_millis(5), &mut execute)
                    .await
                    .is_err(),
                "the turn ended before its launch"
            );
        }
        drop(execute);
        fs::write(points.join("host.anchor.arm_received.1.release"), b"").unwrap();
        child.adapter.tracker.close();
        child.adapter.tracker.wait().await;
    });
    assert!(child.sync("launched").exists(), "the vendor never started");
    let input = fs::read_to_string(child.sync("input")).unwrap_or_default();
    assert_eq!(input, "", "the abandoned turn wrote to the vendor");
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
