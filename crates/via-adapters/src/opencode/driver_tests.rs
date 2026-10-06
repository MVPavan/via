//! OC05 through C2: Core's P6 deliberately never dispatches behind unknown.
use super::serve_tests::Rig;
use super::{OpenCodeAdapter, OpenCodeServers, OpenCodeSession};
use crate::driver::DriverKind;
use crate::plan::{Bound, Inherit, InheritPlan, VendorOptions};
use crate::{
    AdapterError, Admitted, BoundMode, CancellationToken, CloseMode, Deadline, Observation,
    Prepared, SessionCx, SessionDriver, SessionId, SessionSpec, StopAck, TaskTracker, TurnActivity,
    TurnCx, TurnEnd, TurnNumber, TurnSpec, VendorArgs, VendorTerminalStatus, observation_channel,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
const SES: &str = "ses_via0001";
const SID: &str = "s_000000000001";
fn route(method: &str, path: &str, responses: &Value) -> Value {
    json!({"method":method,"path":path,"responses":responses})
}
fn event(kind: &str, data: &Value) -> Value {
    json!({"type":kind,"data":data,"id":"evt_fixture","created":1})
}
fn fixture(cwd: &str, emit: Vec<Value>) -> Value {
    let rules=vec!["*","question","opencode_session_move","opencode_session_rename","opencode_list_mcp_resources","opencode_read_mcp_resource"].into_iter().map(|action|json!({"action":action,"resource":"*","effect":if action=="*"{"allow"}else{"deny"}})).collect::<Vec<_>>();
    let info = json!({"data":{"id":SES,"agent":"via","model":{"providerID":"opencode","id":"big-pickle","variant":"default"},"permissions":rules,"location":{"directory":cwd}}});
    json!({"routes":[
       route("GET","/api/info",&json!([{"status":200,"json":{"pid":"$PID","version":"2.0.22"}}])),
       route("GET","/api/model",&json!([{"status":200,"json":{"data":[{"providerID":"opencode","id":"big-pickle","name":"fixture","variants":[{"id":"high"}]}]}}])),
       {"method":"GET","path":"/api/event","sse":{"events":[{"type":"server.connected","properties":{}}]}},
       route("POST","/api/session",&json!([{"status":200,"json":info}])),
       route("GET",&format!("/api/session/{SES}"),&json!([{"status":200,"json":info}])),
       route("GET",&format!("/api/session/{SES}/inbox"),&json!([{"status":200,"json":{"data":[]}}])),
    route("GET",&format!("/api/experimental/session/{SES}/instructions/entries"),&json!([{"status":200,"json":{"data":[]}}])),
       route("POST",&format!("/api/session/{SES}/prompt"),&json!([{"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},"emit":emit.into_iter().collect::<Value>()}]))
       ]})
}
fn success() -> Vec<Value> {
    vec![
        event(
            "session.inbox.enqueued",
            &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
        ),
        event(
            "session.execution.started",
            &json!({"sessionID":"$SESSION"}),
        ),
        event(
            "session.inbox.delivered",
            &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
        ),
        event(
            "session.execution.succeeded",
            &json!({"sessionID":"$SESSION"}),
        ),
    ]
}
fn replace(fixture: &mut Value, new: Value) {
    let routes = fixture["routes"].as_array_mut().unwrap();
    routes.retain(|r| r["path"] != new["path"] || r["method"] != new["method"]);
    routes.insert(0, new);
}
/// Seed only private fixture rows while no turn is running. Python's SQLite
/// module avoids an adapter dependency on the Store's implementation crate.
fn row(rig: &Rig, number: u32, state: &str) {
    let home = rig.root().join("test-home");
    std::fs::create_dir_all(&home).unwrap();
    let script = r#"import sqlite3,sys
c=sqlite3.connect(sys.argv[1]); n=int(sys.argv[2]); state=sys.argv[3]
c.execute("INSERT OR IGNORE INTO sessions(id,handle_hash,receipt,params,state,next_seq,created_ms,updated_ms,harness,ord) VALUES(?,zeroblob(32),'{}','{}','active',2,0,0,'opencode',0)",('s_000000000001',))
if state=='running':
 c.execute("INSERT INTO turns(session_id,number,prompt,effective,state,queued_seq) VALUES(?,?,'fixture','{}','running',1)",('s_000000000001',n))
else:
 c.execute("UPDATE turns SET state=?,ended_seq=1,envelope='{}' WHERE session_id=? AND number=?",(state,'s_000000000001',n))
c.commit()
"#;
    let result = std::process::Command::new("python3")
        .args(["-c", script])
        .arg(rig.root().join("state/store.sqlite3"))
        .arg(number.to_string())
        .arg(state)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("XDG_DATA_HOME", &home)
        .env("XDG_STATE_HOME", &home)
        .env("XDG_CACHE_HOME", &home)
        .env("XDG_RUNTIME_DIR", &home)
        .env("TMPDIR", &home)
        .current_dir(&home)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
struct Lane {
    driver: SessionDriver,
    receiver: mpsc::Receiver<Admitted>,
    tracker: TaskTracker,
}
impl Lane {
    fn open(rig: &Rig, confirmed: bool) -> Self {
        let adapter = Arc::new(OpenCodeAdapter {
            binary: rig.program.clone(),
            instances: Arc::default(),
            servers: OpenCodeServers::new(
                &rig.program,
                Some(std::ffi::OsStr::new("/usr/bin:/bin")),
                Arc::clone(&rig.servers),
            ),
        });
        let (observations, receiver) = observation_channel();
        let tracker = TaskTracker::new();
        let spec = SessionSpec {
            session_id: SessionId::try_from(SID).unwrap(),
            model: "opencode/big-pickle".to_owned(),
            instructions: None,
            initial_bound: Some(full()),
            cwd: rig.root(),
            vendor: VendorOptions::default(),
            vendor_args: VendorArgs::default(),
            inherit: InheritPlan {
                requested: Inherit::OD2_DEFAULT,
                effective: Inherit::OD2_DEFAULT,
            },
            confirmed_vendor_session_id: confirmed.then(|| SES.to_owned()),
            allow_untested: false,
        };
        let driver = SessionDriver::new(
            Arc::clone(&rig.runtime),
            Some(DriverKind::OpenCode(Arc::new(OpenCodeSession::new(
                adapter,
            )))),
            spec,
            SessionCx {
                observations,
                tracker: tracker.clone(),
                cancel: CancellationToken::new(),
            },
        );
        Self {
            driver,
            receiver,
            tracker,
        }
    }
    async fn turn(
        &mut self,
        number: u32,
        effort: Option<&str>,
        wall: Duration,
    ) -> (TurnEnd, Vec<crate::ObservationItem>) {
        let prepared = self.driver.prepare();
        let capacity = matches!(prepared, Prepared::NeedsConnection)
            .then(|| Box::new(()) as crate::CapacityToken);
        let now = tokio::time::Instant::now();
        let (stop, stop_rx) = watch::channel(None);
        let (force, force_rx) = watch::channel(None);
        let context = TurnCx {
            turn: TurnNumber::try_from(number).unwrap(),
            prepared,
            capacity,
            activity: TurnActivity::new(now),
            wall: Deadline::at(now + wall),
            tool_grace: Duration::from_secs(60),
            stop: stop_rx,
            force: force_rx,
            stop_ack: StopAck::new(),
        };
        let future = self.driver.run_turn(
            TurnSpec {
                prompt: "fixture".to_owned(),
                bound: Some(full()),
                effort: effort.map(str::to_owned),
                ..TurnSpec::default()
            },
            context,
        );
        tokio::pin!(future);
        let mut seen = Vec::new();
        let result = loop {
            tokio::select! {end=&mut future=>break end,item=self.receiver.recv()=>{if let Some(item)=item {seen.push(item.item);}}}
        };
        while let Ok(item) = self.receiver.try_recv() {
            seen.push(item.item);
        }
        drop((stop, force));
        (result, seen)
    }
    async fn close(self) {
        self.driver
            .close(
                CloseMode::Graceful,
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            )
            .await;
        self.tracker.close();
        tokio::time::timeout(Duration::from_secs(3), self.tracker.wait())
            .await
            .unwrap();
    }
}
fn full() -> Bound {
    Bound {
        mode: BoundMode::Full,
        extra_write_dirs: Vec::new(),
        network: true,
    }
}
fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}
fn prompts(requests: &[Value]) -> Vec<&Value> {
    requests
        .iter()
        .filter(|r| {
            r["target"]
                .as_str()
                .is_some_and(|path| path.ends_with("/prompt"))
        })
        .collect()
}
#[test]
fn oc05_c2_reopen_cancels_owned_leftover_once_without_late_revision() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&fixture(
            &cwd,
            vec![
                event(
                    "session.inbox.enqueued",
                    &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
                ),
                json!({"close":true}),
            ],
        ));
        row(&rig, 1, "running");
        let mut first = Lane::open(&rig, false);
        let (unknown, _) = first.turn(1, None, Duration::from_secs(30)).await;
        first.close().await;
        row(&rig, 1, "unknown");
        let old = prompts(&rig.requests())[0]["body"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut next = fixture(&cwd, success());
        replace(
            &mut next,
            route(
                "GET",
                &format!("/api/session/{SES}/inbox"),
                &json!([{"status":200,"json":{"data":[{"id":old},{"id":"msg_foreign"}]}}]),
            ),
        );
        replace(
            &mut next,
            route(
                "DELETE",
                &format!("/api/session/{SES}/inbox/{old}"),
                &json!([{"status":204,"emit":[event("session.inbox.cancelled",&json!({"sessionID":"$SESSION","inboxID":old}))]}]),
            ),
        );
        rig.fixture(&next);
        row(&rig, 2, "running");
        let mut reopened = Lane::open(&rig, true);
        let (second, seen) = reopened.turn(2, None, Duration::from_secs(30)).await;
        row(&rig, 2, "completed");
        row(&rig, 3, "running");
        let (third, more) = reopened.turn(3, None, Duration::from_secs(30)).await;
        reopened.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(unknown.terminal.is_none());
        assert!(
            matches!(unknown.outcome,Err(AdapterError::Route(ref failure)) if matches!(failure.cause,crate::RouteError::TransportLost {..}))
        );
        assert!(
            matches!(
                second.terminal.as_ref().map(|t| &t.status),
                Some(VendorTerminalStatus::Completed)
            ),
            "second: {second:?}; requests: {requests:?}"
        );
        assert!(third.terminal.is_some());
        assert_eq!(
            requests
                .iter()
                .filter(
                    |r| r["method"] == "GET" && r["target"] == format!("/api/session/{SES}/inbox")
                )
                .count(),
            1
        );
        let deletes: Vec<_> = requests
            .iter()
            .filter(|r| r["method"] == "DELETE")
            .collect();
        assert_eq!(
            deletes.len(),
            1,
            "second: {second:?}; third: {third:?}; requests: {requests:?}"
        );
        assert_eq!(
            deletes[0]["target"],
            format!("/api/session/{SES}/inbox/{old}")
        );
        let sent = prompts(&requests);
        assert_eq!(sent.len(), 3);
        assert_ne!(sent[1]["body"]["id"], old);
        assert_ne!(sent[2]["body"]["id"], old);
        assert!(
            !seen
                .iter()
                .chain(&more)
                .any(|item| matches!(item.observation, Observation::LateTerminal(_)))
        );
    });
}
#[test]
fn oc05_c2_failed_cleanup_blocks_every_successor_on_that_generation() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&fixture(
            &cwd,
            vec![
                event(
                    "session.inbox.enqueued",
                    &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
                ),
                json!({"close":true}),
            ],
        ));
        row(&rig, 1, "running");
        let mut first = Lane::open(&rig, false);
        let _end = first.turn(1, None, Duration::from_secs(30)).await;
        first.close().await;
        row(&rig, 1, "unknown");
        let old = prompts(&rig.requests())[0]["body"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut next = fixture(&cwd, success());
        replace(
            &mut next,
            route(
                "GET",
                &format!("/api/session/{SES}/inbox"),
                &json!([{"status":200,"json":{"data":[{"id":old}]}}]),
            ),
        );
        replace(
            &mut next,
            route(
                "DELETE",
                &format!("/api/session/{SES}/inbox/{old}"),
                &json!([{"status":500,"json":{}}]),
            ),
        );
        rig.fixture(&next);
        row(&rig, 2, "running");
        let mut reopened = Lane::open(&rig, true);
        let (second, _) = reopened.turn(2, None, Duration::from_secs(2)).await;
        row(&rig, 2, "failed");
        row(&rig, 3, "running");
        let (third, _) = reopened.turn(3, None, Duration::from_secs(2)).await;
        reopened.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(second.terminal.is_none());
        assert!(third.terminal.is_none());
        assert!(third.outcome.is_err());
        assert_eq!(
            prompts(&requests).len(),
            1,
            "no prompt after failed cleanup: {requests:?}"
        );
        assert_eq!(
            requests.iter().filter(|r| r["method"] == "DELETE").count(),
            1
        );
    });
}
#[test]
fn oc05_c2_cancelled_setup_request_blocks_reopened_driver_prompt() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        let mut next = fixture(&cwd, success());
        replace(
            &mut next,
            route(
                "POST",
                &format!("/api/session/{SES}/model"),
                &json!([{"status":204,"sleep_ms":800}]),
            ),
        );
        rig.fixture(&next);
        row(&rig, 1, "running");
        let mut first = Lane::open(&rig, false);
        let (end, _) = first
            .turn(1, Some("high"), Duration::from_millis(300))
            .await;
        // Keep this generation alive while replacing only the driver.
        let held = OpenCodeServers::new(
            &rig.program,
            Some(std::ffi::OsStr::new("/usr/bin:/bin")),
            Arc::clone(&rig.servers),
        )
        .pin()
        .expect("the interrupted setup attached a live server");
        first.close().await;
        row(&rig, 1, "failed");
        row(&rig, 2, "running");
        let mut reopened = Lane::open(&rig, true);
        let (next_end, _) = reopened.turn(2, None, Duration::from_secs(2)).await;
        reopened.close().await;
        let requests = rig.requests();
        drop(held);
        rig.finish().await;
        assert!(end.terminal.is_none());
        assert!(
            next_end.terminal.is_none(),
            "first: {end:?}; next: {next_end:?}; requests: {requests:?}"
        );
        assert!(next_end.outcome.is_err());
        assert_eq!(
            requests
                .iter()
                .map(|r| r["pid"].as_u64().unwrap())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            1,
            "the reopened driver stayed on the same server generation"
        );
        assert_eq!(
            requests
                .iter()
                .filter(
                    |r| r["method"] == "POST" && r["target"] == format!("/api/session/{SES}/model")
                )
                .count(),
            1
        );
        assert_eq!(
            prompts(&requests).len(),
            0,
            "pending setup prevents every later prompt: {requests:?}"
        );
    });
}
