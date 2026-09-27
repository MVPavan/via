//! Route→Adapter observation stream and raw-evidence gaps through the real Route,
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
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::sync::mpsc;
use via_adapters::{
    AdapterError, AdapterRuntime, AdapterRuntimeConfig, ConnectionId, Deadline, FakeConfig,
    FakeObservation, Observation, RawRef, RouteError, RuntimeConfig, SessionId, ToolStatus,
    TurnNumber,
};
use via_store::{SpawnRecord, Store};

const SESSION: &str = "s_0123456789ab";
const CONNECTION: &str = "c_0123456789ab";
const CHILD: &str = "VIA_ROUTE_STREAM_CHILD";
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
        if Instant::now() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name} child did not finish within {CHILD_LIMIT:?}");
        }
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
                prompt: "hello".to_owned(),
                initial_event: json!({"seq":1,"type":"turn.queued"}),
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
            store: Some(store),
            adapter,
            runtime,
        }
    }

    /// Runs one turn, calling `on_observation` with the Store owner, the sandbox
    /// root and each observation as it arrives.
    fn execute(
        &mut self,
        mut on_observation: impl FnMut(&mut Option<Store>, &Path, &FakeObservation),
    ) -> (Vec<FakeObservation>, Result<(), AdapterError>) {
        let Self {
            root,
            store,
            adapter,
            runtime,
        } = self;
        let (sender, mut receiver) = mpsc::channel(4);
        let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(20));
        let mut observed = Vec::new();
        let result = runtime.block_on(async {
            let execute = adapter.execute(
                SessionId::try_from(SESSION).unwrap(),
                TurnNumber::try_from(1).unwrap(),
                ConnectionId::try_from(CONNECTION).unwrap(),
                "hello".to_owned(),
                sender,
                deadline,
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

    /// Reads exactly the durable bytes a reference cites.
    fn raw(&self, reference: &RawRef) -> Vec<u8> {
        let mut file = fs::File::open(
            self.root
                .join("state/raw")
                .join(format!("{}.raw", reference.connection_id().as_str())),
        )
        .unwrap();
        file.seek(SeekFrom::Start(reference.offset())).unwrap();
        let mut bytes = vec![0; usize::try_from(reference.byte_len()).unwrap()];
        file.read_exact(&mut bytes).unwrap();
        bytes
    }
}

fn child_root() -> Option<PathBuf> {
    env::var_os(CHILD).map(|root: OsString| PathBuf::from(root))
}

#[test]
fn route_forwards_every_observation_in_order_with_its_raw_ref() {
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
            "route_forwards_every_observation_in_order_with_its_raw_ref",
            "read -r start\n/bin/cat \"$VIA_FAKE_SCENARIO\"\n",
            &lines,
        );
    };
    let mut child = Child::open(&root);
    let (observed, result) = child.execute(|_, _, _| {});
    result.unwrap();
    // Everything except the terminal, which travels in the route result.
    let expected: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"] != "terminal")
        .collect();
    assert_eq!(observed.len(), expected.len());
    for (observation, line) in observed.iter().zip(expected) {
        let raw_ref = match observation {
            FakeObservation::Accepted(accepted) => {
                assert_eq!(line["type"], "accepted");
                assert_eq!(accepted.vendor_turn_id.as_str(), "fake-turn-1");
                &accepted.raw_ref
            }
            FakeObservation::Data {
                observation,
                raw_ref,
            } => {
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
                raw_ref
            }
        };
        let bytes = child.raw(raw_ref);
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(&serde_json::from_slice::<Value>(&bytes).unwrap(), line);
    }
}

#[test]
fn failing_raw_append_keeps_draining_and_reports_incomplete_evidence() {
    let lines = [
        json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}),
        json!({"type":"text","vendor_turn_id":"fake-turn-1","text":"after store loss"}),
    ];
    let Some(root) = child_root() else {
        // Emit acceptance, wait for the test to fail the raw Store, then emit more
        // stdout and stderr that can no longer be recorded.
        return run_child(
            "failing_raw_append_keeps_draining_and_reports_incomplete_evidence",
            "read -r start\n\
             /usr/bin/head -n 1 \"$VIA_FAKE_SCENARIO\"\n\
             while [ ! -e \"$VIA_FAKE_SYNC_DIR/release\" ]; do /bin/sleep 0.01; done\n\
             /usr/bin/tail -n 1 \"$VIA_FAKE_SCENARIO\"\n\
             echo stderr-after-store-loss >&2\n",
            &lines,
        );
    };
    let mut child = Child::open(&root);
    let (observed, result) = child.execute(|store, root, observation| {
        if matches!(observation, FakeObservation::Accepted(_)) {
            // Dropping the owner stops Store's raw writer; later appends fail.
            drop(store.take());
            fs::write(root.join("sync/release"), b"").unwrap();
        }
    });
    assert_eq!(observed.len(), 1, "only acceptance was recorded");
    let Err(AdapterError::Route(failure)) = result else {
        panic!("expected a route failure, got {result:?}");
    };
    assert!(
        matches!(failure.cause, RouteError::Store { .. }),
        "{failure:?}"
    );
    assert!(failure.raw_incomplete, "{failure:?}");
}

#[test]
fn stalled_raw_worker_cannot_hold_failure_cleanup_past_its_deadline() {
    let lines = [json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"})];
    let Some(root) = child_root() else {
        // Emit acceptance, wait for the test to stall Store's raw worker, then emit
        // an oversized line that only the failure drain records.
        return run_child(
            "stalled_raw_worker_cannot_hold_failure_cleanup_past_its_deadline",
            "read -r start\n\
             /usr/bin/head -n 1 \"$VIA_FAKE_SCENARIO\"\n\
             while [ ! -e \"$VIA_FAKE_SYNC_DIR/release\" ]; do /bin/sleep 0.01; done\n\
             /usr/bin/head -c 1100000 /dev/zero | /usr/bin/tr '\\0' x\n\
             echo\n",
            &lines,
        );
    };
    let mut child = Child::open(&root);
    let mut stall = None;
    let mut released = None;
    let (observed, result) = child.execute(|store, root, observation| {
        if matches!(observation, FakeObservation::Accepted(_)) {
            stall = Some(store.as_ref().unwrap().stall_raw_worker());
            released = Some(Instant::now());
            fs::write(root.join("sync/release"), b"").unwrap();
        }
    });
    let elapsed = released.unwrap().elapsed();
    // The Store owner's drop joins the raw worker; release it first.
    drop(stall);
    assert_eq!(observed.len(), 1, "only acceptance was recorded");
    let Err(AdapterError::Route(failure)) = result else {
        panic!("expected a route failure, got {result:?}");
    };
    assert!(
        matches!(failure.cause, RouteError::Protocol { .. }),
        "{failure:?}"
    );
    assert!(failure.raw_incomplete, "{failure:?}");
    // Route's cleanup bound is 3 s; the 20 s turn deadline must not be reached.
    assert!(elapsed < Duration::from_secs(8), "cleanup took {elapsed:?}");
}
