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
pub(super) const SES: &str = "ses_via0001";
pub(super) const SID: &str = "s_000000000001";
pub(super) fn route(method: &str, path: &str, responses: &Value) -> Value {
    json!({"method":method,"path":path,"responses":responses})
}
pub(super) fn event(kind: &str, data: &Value) -> Value {
    json!({"type":kind,"data":data,"id":"evt_fixture","created":1})
}
pub(super) fn fixture(cwd: &str, emit: Vec<Value>) -> Value {
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
pub(super) fn success() -> Vec<Value> {
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
pub(super) fn replace(fixture: &mut Value, new: Value) {
    let routes = fixture["routes"].as_array_mut().unwrap();
    routes.retain(|r| r["path"] != new["path"] || r["method"] != new["method"]);
    routes.insert(0, new);
}
/// Seed only private fixture rows while no turn is running. Python's SQLite
/// module avoids an adapter dependency on the Store's implementation crate.
pub(super) fn row(rig: &Rig, number: u32, state: &str) {
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
pub(super) struct Lane {
    pub(super) driver: SessionDriver,
    pub(super) receiver: mpsc::Receiver<Admitted>,
    tracker: TaskTracker,
}
impl Lane {
    pub(super) fn open(rig: &Rig, confirmed: bool) -> Self {
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
    pub(super) async fn turn(
        &mut self,
        number: u32,
        effort: Option<&str>,
        wall: Duration,
    ) -> (TurnEnd, Vec<crate::ObservationItem>) {
        self.turn_prepared(number, effort, wall, self.driver.prepare())
            .await
    }

    /// C2 §3: run with the actual pin captured before a readiness change.
    pub(super) async fn turn_prepared(
        &mut self,
        number: u32,
        effort: Option<&str>,
        wall: Duration,
        prepared: Prepared,
    ) -> (TurnEnd, Vec<crate::ObservationItem>) {
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
        let result = self.turn_context(effort, context).await;
        drop((stop, force));
        result
    }

    /// C2 §4.1: exercise Core's stop sources without replacing the driver.
    pub(super) async fn turn_context(
        &mut self,
        effort: Option<&str>,
        context: TurnCx,
    ) -> (TurnEnd, Vec<crate::ObservationItem>) {
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
        // Losing recv is safe; the pinned turn future remains alive across observations.
        let result = loop {
            tokio::select! {
                end = &mut future => break end,
                item = self.receiver.recv() => {
                    if let Some(item) = item {
                        seen.push(item.item);
                    }
                }
            }
        };
        while let Ok(item) = self.receiver.try_recv() {
            seen.push(item.item);
        }
        (result, seen)
    }
    pub(super) async fn close(self) {
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
pub(super) fn full() -> Bound {
    Bound {
        mode: BoundMode::Full,
        extra_write_dirs: Vec::new(),
        network: true,
    }
}
pub(super) fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}
pub(super) fn prompts(requests: &[Value]) -> Vec<&Value> {
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
fn oc05_c2_reopen_cancels_near_limit_leftover_with_foreign_echo_without_late_revision() {
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
        // §7.2 permits one unresolved VIA input; tolerate one foreign input too.
        // Both echoed JSON strings approach §9's admission limit, including cwd.
        let text = "x".repeat(1_048_576 - 8_192 - cwd.len() - 4);
        replace(
            &mut next,
            route(
                "GET",
                &format!("/api/session/{SES}/inbox"),
                &json!([{"status":200,"json":{"data":[
                    {"id":old,"text":text},{"id":"msg_foreign","text":text}
                ]}}]),
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
/// §8 retires a draining fake even while a driver and an old pin remain alive.
async fn retired_after_setup(rig: &Rig) -> bool {
    let Some(pid) = rig
        .requests()
        .first()
        .and_then(|request| request["pid"].as_u64())
    else {
        return false;
    };
    let by = tokio::time::Instant::now() + Duration::from_secs(2);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        if tokio::time::Instant::now() >= by {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

#[test]
fn oc05_c2_timed_out_setup_drains_and_rejects_pinned_reopened_prompt() {
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
        let inspector = Lane::open(&rig, true);
        let ((end, _), held) = tokio::join!(
            first.turn(1, Some("high"), Duration::from_millis(300)),
            async {
                let by = tokio::time::Instant::now() + Duration::from_secs(1);
                while !rig.requests().iter().any(|request| {
                    request["method"] == "POST"
                        && request["target"] == format!("/api/session/{SES}/model")
                }) && tokio::time::Instant::now() < by
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                inspector.driver.prepare()
            }
        );
        let was_pinned = matches!(held, Prepared::Pinned(_));
        let by = tokio::time::Instant::now() + Duration::from_secs(1);
        let draining = loop {
            if matches!(inspector.driver.prepare(), Prepared::NeedsConnection) {
                break true;
            }
            if tokio::time::Instant::now() >= by {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let retired = retired_after_setup(&rig).await;
        first.close().await;
        row(&rig, 1, "failed");
        row(&rig, 2, "running");
        let mut reopened = Lane::open(&rig, true);
        let (next_end, _) = reopened
            .turn_prepared(2, None, Duration::from_secs(2), held)
            .await;
        reopened.close().await;
        inspector.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(was_pinned, "capture the old pin before the setup timeout");
        assert!(draining, "§8 drains after the sent setup response timeout");
        assert!(retired, "§8 retires despite the old pin and idle driver");
        assert!(end.terminal.is_none());
        assert!(
            next_end.terminal.is_none(),
            "first: {end:?}; next: {next_end:?}; requests: {requests:?}"
        );
        assert!(matches!(
            next_end.outcome,
            Err(AdapterError::Rejected {
                reason: crate::StartRejected::SessionGone,
                ..
            })
        ));
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

/// §7.2's first delivery arrives after §7.4's caller force ends the turn.
fn settled_input_fixture(cwd: &str, cancelled: bool) -> Value {
    let mut next = fixture(cwd, Vec::new());
    let mut predecessor_events = vec![
        event(
            "session.inbox.enqueued",
            &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
        ),
        json!({"pause_ms":1000}),
    ];
    if cancelled {
        predecessor_events.push(event(
            "session.inbox.cancelled",
            &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
        ));
    } else {
        predecessor_events.extend([
            event(
                "session.execution.started",
                &json!({"sessionID":"$SESSION"}),
            ),
            event(
                "session.inbox.delivered",
                &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
            ),
            event(
                "session.step.started",
                &json!({"sessionID":"$SESSION","assistantMessageID":"$INPUT:a"}),
            ),
            event(
                "session.text.ended",
                &json!({"sessionID":"$SESSION","assistantMessageID":"$INPUT:a",
                    "ordinal":0,"text":"late-A"}),
            ),
            event(
                "session.execution.succeeded",
                &json!({"sessionID":"$SESSION"}),
            ),
        ]);
    }
    let mut successor = success();
    let terminal = successor.pop().unwrap();
    successor.extend([
        event(
            "session.step.started",
            &json!({"sessionID":"$SESSION","assistantMessageID":"$INPUT:b"}),
        ),
        event(
            "session.text.ended",
            &json!({"sessionID":"$SESSION","assistantMessageID":"$INPUT:b",
                "ordinal":0,"text":"B"}),
        ),
        event(
            "session.step.ended",
            &json!({"sessionID":"$SESSION","assistantMessageID":"$INPUT:b",
                "finish":"stop","tokens":{"input":11,"output":7,"reasoning":0,
                    "cache":{"read":2,"write":0}},"cost":0.25}),
        ),
        terminal,
    ]);
    replace(
        &mut next,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!([
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                    "emit_before_response":true,
                    "sleep_ms":150,"emit":predecessor_events},
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                    "emit":successor}
            ]),
        ),
    );
    replace(
        &mut next,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/*"),
            &json!([{"status":204}]),
        ),
    );
    next
}

async fn stopped_before_delivery(
    lane: &mut Lane,
    rig: &Rig,
) -> (TurnEnd, Vec<crate::ObservationItem>) {
    let prepared = lane.driver.prepare();
    let inspector = Lane::open(rig, true);
    let capacity =
        matches!(prepared, Prepared::NeedsConnection).then(|| Box::new(()) as crate::CapacityToken);
    let now = tokio::time::Instant::now();
    let (stop, stop_rx) = watch::channel(None);
    let (force, force_rx) = watch::channel(None);
    let context = TurnCx {
        turn: TurnNumber::try_from(1).unwrap(),
        prepared,
        capacity,
        activity: TurnActivity::new(now),
        wall: Deadline::at(now + Duration::from_secs(5)),
        tool_grace: Duration::from_secs(1),
        stop: stop_rx,
        force: force_rx,
        stop_ack: StopAck::new(),
    };
    let stopping = async {
        let by = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < by {
            let completed = if let Prepared::Pinned(pin) = inspector.driver.prepare() {
                pin.opencode
                    .and_then(|pin| pin.live())
                    .is_some_and(|(server, _)| {
                        server
                            .routing()
                            .state(SES)
                            .is_some_and(|state| state.pending_requests == 0)
                    })
            } else {
                false
            };
            if !prompts(&rig.requests()).is_empty() && completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let attached = tokio::time::Instant::now();
        stop.send_replace(Some(crate::StopOrder {
            cause: crate::StopCause::Cancel,
            requested_at: "2026-10-06T00:00:00Z".into(),
            attached,
            force_at: Deadline::at(attached + Duration::from_millis(200)),
            close_by: Deadline::at(attached + Duration::from_secs(2)),
        }));
    };
    let (result, ()) = tokio::join!(lane.turn_context(None, context), stopping);
    drop((stop, force));
    inspector.close().await;
    result
}

/// C2 exercises §7.2 after unknown; Core's P6 cannot admit that successor.
async fn settled_before_first_delivery(cancelled: bool) {
    let rig = Rig::new(&json!({}));
    let cwd = rig.root().to_str().unwrap().to_owned();
    rig.fixture(&settled_input_fixture(&cwd, cancelled));
    row(&rig, 1, "running");
    let mut lane = Lane::open(&rig, false);
    let (unknown, first_seen) = stopped_before_delivery(&mut lane, &rig).await;
    row(&rig, 1, "unknown");
    row(&rig, 2, "running");
    // A broken execution rule times out here instead of submitting B. Keeping
    // the same driver retains the server and avoids reopen cleanup as a repair.
    let started = tokio::time::Instant::now();
    let (second, second_seen) = lane.turn(2, None, Duration::from_secs(2)).await;
    let elapsed = started.elapsed();
    lane.close().await;
    let requests = rig.requests();
    rig.finish().await;
    assert!(unknown.terminal.is_none(), "A: {unknown:?}");
    assert!(matches!(unknown.outcome,
        Err(AdapterError::Route(ref failure))
            if matches!(failure.cause, crate::RouteError::Stopped { .. }
                | crate::RouteError::ForceStopped { .. })));
    assert!(
        first_seen
            .iter()
            .any(|item| matches!(item.observation, Observation::Accepted(_))),
        "A must be accepted before the caller force"
    );
    assert!(
        matches!(
            second.terminal.as_ref().map(|terminal| &terminal.status),
            Some(VendorTerminalStatus::Completed)
        ),
        "B: {second:?}; elapsed: {elapsed:?}; requests: {requests:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "successor waited: {elapsed:?}"
    );
    let usage = second.terminal.as_ref().unwrap().usage.as_ref().unwrap();
    assert_eq!(
        usage.input,
        Some(11),
        "B retained its own reported usage: {second:?}; {second_seen:?}"
    );
    assert_eq!(usage.output, Some(7));
    let sent = prompts(&requests);
    assert_eq!(sent.len(), 2, "never resend A: {requests:?}");
    assert_ne!(sent[0]["body"]["id"], sent[1]["body"]["id"]);
    assert_eq!(
        requests
            .iter()
            .map(|request| request["pid"].as_u64().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "both inputs stay on the retained server generation"
    );
    let old = sent[0]["body"]["id"].as_str().unwrap();
    let current = sent[1]["body"]["id"].as_str().unwrap();
    let mut current_text = String::new();
    let mut late_text = String::new();
    let mut late_terminal = false;
    for item in second_seen {
        if let Observation::FinalText(text) = &item.observation {
            match item.vendor_turn.as_ref().map(crate::VendorTurnId::as_str) {
                Some(id) if id == current => current_text.push_str(text),
                Some(id) if id == old => late_text.push_str(text),
                owner => panic!("final text has unexpected owner: {owner:?}"),
            }
        }
        if matches!(item.observation, Observation::LateTerminal(_)) {
            assert_eq!(
                item.vendor_turn.as_ref().map(crate::VendorTurnId::as_str),
                Some(old)
            );
            late_terminal = true;
        }
    }
    assert_eq!(
        current_text, "B",
        "A's late events must never become B's output"
    );
    if !cancelled {
        assert_eq!(late_text, "late-A");
        assert!(late_terminal, "A keeps its late terminal attributed to A");
    }
}

#[test]
fn oc05_c2_settled_input_first_late_delivery_admits_successor_without_resend() {
    run(settled_before_first_delivery(false));
}

#[test]
fn oc05_c2_settled_input_late_cancel_admits_successor_without_resend() {
    run(settled_before_first_delivery(true));
}
