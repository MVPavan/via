//! Task 4 design §2.3 Bounds (C2 A1) at the Adapter, through the real
//! Route, Wire and Host with a scripted vendor: the observation channel
//! admits 1,024 items, far more than today's 64, and at most 4 MiB of them
//! by `512 + Σ(64 + len)`; a delivery that stays blocked past the lowered
//! stall fails the turn `overflow`. The test holds the channel's receiver
//! and never drains it, so it can pace the vendor on what the channel
//! admitted: each batch is released once the channel holds every item
//! before it. Written before the bounded channel. Each case re-executes
//! this binary with its vendor settings, as `route_stream.rs` does.
#![cfg(feature = "test-failpoints")]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use via_adapters::{
    AdapterError, AdapterRuntime, AdapterRuntimeConfig, Deadline, FakeConfig, OBSERVATION_BYTES,
    OBSERVATION_ITEMS, RouteError, RuntimeConfig, SessionId, TurnNumber,
};
use via_store::{SpawnRecord, Store};

const SESSION: &str = "s_0123456789ab";
const CHILD: &str = "VIA_OBSERVATION_BUDGET_CHILD";
/// Bound on one child case.
const CHILD_LIMIT: Duration = Duration::from_secs(60);
/// The vendor releases batch `i` once `go.<i>` exists in its sync dir.
const VENDOR: &str = r#"#!/bin/sh
read -r start
i=0
while [ -e "$VIA_FAKE_SCENARIO.$i" ]; do
  while [ ! -e "$VIA_FAKE_SYNC_DIR/go.$i" ]; do sleep 0.005; done
  /bin/cat "$VIA_FAKE_SCENARIO.$i"
  i=$((i+1))
done
exec sleep 30
"#;

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

fn accepted() -> Value {
    json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"})
}

fn text(text: &str) -> Value {
    json!({"type":"text","vendor_turn_id":"fake-turn-1","text":text})
}

/// Runs `name` again in a child whose vendor writes `batches` one by one.
fn run_child(name: &str, batches: &[Vec<Value>]) {
    let root = tempfile::tempdir().unwrap();
    let dirs = ["state", "runtime", "sync"].map(|part| {
        let path = root.path().join(part);
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        path
    });
    let vendor = root.path().join("vendor.sh");
    fs::write(&vendor, VENDOR).unwrap();
    fs::set_permissions(&vendor, fs::Permissions::from_mode(0o700)).unwrap();
    let scenario = root.path().join("scenario");
    fs::write(&scenario, b"").unwrap();
    for (index, batch) in batches.iter().enumerate() {
        let body: String = batch.iter().map(|line| line.to_string() + "\n").collect();
        fs::write(root.path().join(format!("scenario.{index}")), body).unwrap();
    }
    let mut child = Command::new(env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, root.path())
        .env("VIA_FAKE_AGENT_BINARY", &vendor)
        .env("VIA_FAKE_SCENARIO", &scenario)
        .env("VIA_FAKE_SYNC_DIR", &dirs[2])
        .env("VIA_TEST_EVENT_STALL_MS", "300")
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

/// In the child: runs the turn, releasing batch `i + 1` once the channel
/// holds `admitted[i]` items, and never draining it. Returns the items the
/// channel held when the turn ended, and the turn's failure.
fn run_turn(root: &Path, admitted: &[usize]) -> (usize, AdapterError) {
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
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: json!({"seq":1,"type":"turn.queued","turn":1,"at":"2026-01-01T00:00:00.000Z"}),
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
    let sync = root.join("sync");
    fs::write(sync.join("go.0"), b"").unwrap();
    let (sink, receiver) = via_adapters::observation_channel();
    let (_force, force) = tokio::sync::watch::channel(None);
    let result = runtime.block_on(async {
        let execute = adapter.execute(
            SessionId::try_from(SESSION).unwrap(),
            TurnNumber::try_from(1).unwrap(),
            "hello".to_owned(),
            sink,
            Deadline::at(tokio::time::Instant::now() + Duration::from_secs(40)),
            force,
            tokio::sync::watch::channel(None).1,
            Box::new(()),
        );
        let pace = async {
            for (index, &items) in admitted.iter().enumerate() {
                let bound = tokio::time::Instant::now() + Duration::from_secs(20);
                while receiver.len() < items {
                    assert!(
                        tokio::time::Instant::now() < bound,
                        "the channel holds {} items, not {items}",
                        receiver.len()
                    );
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                fs::write(sync.join(format!("go.{}", index + 1)), b"").unwrap();
            }
        };
        let (result, ()) = tokio::join!(execute, pace);
        result
    });
    let held = receiver.len();
    drop(adapter);
    drop(store);
    // The undrained turn never completes.
    (held, result.err().unwrap())
}

fn overflow(error: &AdapterError) -> bool {
    matches!(
        error,
        AdapterError::Route(failure) if matches!(failure.cause, RouteError::Overflow { .. })
    )
}

/// Design §2.3, §13.2: small items fill the channel to exactly 1,024, far
/// past 64; 100 KiB texts fill the byte budget to `floor(4 MiB / cost)` in
/// all. The next delivery blocks; at the stall the turn fails `overflow`.
#[test]
fn s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib() {
    const LARGE: usize = 100 * 1024;
    let Some(root) = env::var_os(CHILD).map(PathBuf::from) else {
        // Count bound: 32 batches of 32 items, then 10 more.
        let mut batches = vec![
            std::iter::once(accepted())
                .chain((1..32).map(|n| text(&format!("c{n}"))))
                .collect::<Vec<_>>(),
        ];
        batches.extend((1..32).map(|batch| {
            (0..32)
                .map(|n| text(&format!("c{}", batch * 32 + n)))
                .collect()
        }));
        batches.push((0..10).map(|n| text(&format!("x{n}"))).collect());
        run_child(
            "s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib",
            &batches,
        );
        // Byte bound: batches of 5 large texts.
        let large = "b".repeat(LARGE);
        let mut batches = vec![vec![accepted()]];
        batches.extend((0..10).map(|_| vec![text(&large); 5]));
        return run_child(
            "s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib",
            &batches,
        );
    };
    let batches = (0..=64)
        .take_while(|index| root.join(format!("scenario.{index}")).exists())
        .count();
    if batches > 20 {
        let admitted: Vec<usize> = (1..=32).map(|batch| batch * 32).collect();
        let (held, error) = run_turn(&root, &admitted);
        assert!(overflow(&error), "{error}");
        assert_eq!(held, OBSERVATION_ITEMS);
        assert!(held > 64);
    } else {
        let accepted_cost = 512 + 64 + "fake-turn-1".len();
        let text_cost = 512 + 64 + LARGE;
        let texts = (OBSERVATION_BYTES - accepted_cost) / text_cost;
        // Batch k ends with 5k texts behind the acceptance; the batch after
        // the budget is full blocks.
        let admitted: Vec<usize> = (0..=texts / 5).map(|batch| 1 + batch * 5).collect();
        let (held, error) = run_turn(&root, &admitted);
        assert!(overflow(&error), "{error}");
        assert_eq!(held, 1 + texts, "{texts} texts fit the byte budget");
    }
}
