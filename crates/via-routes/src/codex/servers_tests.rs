//! The registry's leases and its status list (x.3.2 X4 D2; X0 items 2
//! and 7), over scripted servers the registry's own launch job opens.

use std::sync::Arc;
use std::time::Duration;

use via_wire::{EnvAllowList, PrivateProcessSpec, ProcessOwner, ServerId};

use super::{Entry, HandshakeBound, ServerKey, ServerPin, Servers};
use crate::codex::DeclineTable;
use crate::codex::testing::{TestRuntime, TestStdio, VendorEnds, model};

const DECLINES: DeclineTable = DeclineTable::new(&[]);

const USER_AGENT: &str = "via/0.159.2 (Linux 6.0.0; x86_64) unknown (via; 0.0.0)";

fn spec() -> PrivateProcessSpec {
    PrivateProcessSpec {
        program: "/bin/true".into(),
        args: Vec::new(),
        cwd: "/".into(),
        env: EnvAllowList::default(),
        owner: ProcessOwner::Server {
            server_id: ServerId::mint().unwrap(),
        },
        stderr_path: std::path::PathBuf::new(),
        capacity: None,
    }
}

fn key(first: u8) -> ServerKey {
    let mut bytes = [0_u8; 32];
    bytes[0] = first;
    bytes[1] = 0xab;
    bytes[8] = 0xff;
    ServerKey(bytes)
}

/// A server of `key`, launched over a script and live, with the pin of
/// its launch.
async fn launched(servers: &Servers, key: ServerKey) -> (ServerPin, VendorEnds, Arc<TestStdio>) {
    let (mut ends, stdio) = servers.script();
    let pin = servers
        .launch_or_join(key, (spec(), HandshakeBound::Warm), Box::new(()))
        .unwrap();
    assert!(
        servers
            .reports()
            .iter()
            .all(|report| report.server != *pin.server()),
        "a launching server is not listed"
    );
    ends.handshake(USER_AGENT, &[model("gpt-6-sol")]).await;
    tokio::time::timeout(Duration::from_secs(5), pin.ready(std::future::pending()))
        .await
        .unwrap()
        .unwrap();
    (pin, ends, stdio)
}

/// Waits until `done`, polling, within 5 s.
async fn until(what: &str, done: impl Fn() -> bool) {
    let started = tokio::time::Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "never reached: {what}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// D2: status lists a live server with its lease count, never its
/// holders: two sessions, then one, then none (its last holder gone, it
/// retires and is no longer listed). The key is 16 lower-case hex digits.
#[tokio::test]
async fn reports_count_leases_not_holders() {
    let runtime = TestRuntime::new();
    let servers = Servers::new(runtime.runtime(), DECLINES);
    let (pin, _ends, stdio) = launched(&servers, key(0x01)).await;
    let a = pin.lease().unwrap();
    let b = pin.lease().unwrap();
    let extra = pin.duplicate().unwrap();
    let reports = servers.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].server, *pin.server());
    assert_eq!(reports[0].key, "01ab000000000000");
    assert_eq!(reports[0].user_agent, USER_AGENT);
    assert_eq!(reports[0].sessions, 2, "leases, not the four holders");
    drop(a);
    assert_eq!(servers.reports()[0].sessions, 1);
    drop(b);
    assert_eq!(servers.reports()[0].sessions, 0, "pins hold it, unleased");
    drop(extra);
    drop(pin);
    assert!(servers.reports().is_empty(), "retiring: no longer listed");
    until("the retirement's close", || stdio.closes() == 1).await;
}

/// D2: two keys are two servers, each listed with its own key, in server
/// ID order.
#[tokio::test]
async fn two_keys_are_two_servers_sorted() {
    let runtime = TestRuntime::new();
    let servers = Servers::new(runtime.runtime(), DECLINES);
    let (first, _a, _) = launched(&servers, key(0x02)).await;
    let (second, _b, _) = launched(&servers, key(0x03)).await;
    assert_ne!(first.server(), second.server());
    let _lease = second.lease().unwrap();
    let reports = servers.reports();
    assert_eq!(reports.len(), 2);
    assert!(reports[0].server < reports[1].server, "sorted by server ID");
    for report in &reports {
        let (want_key, want_sessions) = if report.server == *first.server() {
            ("02ab000000000000", 0)
        } else {
            ("03ab000000000000", 1)
        };
        assert_eq!(report.key, want_key);
        assert_eq!(report.sessions, want_sessions);
    }
}

/// D2 (`lease_refused_after_retire`): a pin's lease once its server left
/// `Live` is refused and changes nothing; while live it is granted.
#[tokio::test]
async fn lease_refused_after_retire() {
    let runtime = TestRuntime::new();
    let servers = Servers::new(runtime.runtime(), DECLINES);
    let (pin, ends, _stdio) = launched(&servers, key(0x04)).await;
    assert!(pin.live().is_some());
    drop(pin.lease().unwrap());
    // The server's stdout ends: the connection fails, and the supervisor
    // moves the server out of `Live`.
    drop(ends);
    until("the server left Live", || pin.live().is_none()).await;
    let state = || {
        let registry = servers.registry();
        let entry = registry
            .servers
            .get(pin.server())
            .map(|instance| match &instance.entry {
                Entry::Launching { .. } => "launching",
                Entry::Live { .. } => "live",
                Entry::Retiring { .. } => "retiring",
                Entry::Lost { .. } => "lost",
            });
        (entry, registry.stale)
    };
    let before = state();
    assert!(
        matches!(before, (Some("lost") | None, 0)),
        "out of Live: {before:?}"
    );
    assert!(pin.lease().is_none(), "no lease off a server not live");
    assert_eq!(state(), before, "the refusal changed nothing");
    assert!(servers.reports().is_empty());
}

/// D2 (`lease_drop_retires_once`): a lease is one holder and one lease,
/// released together under one guard. Its drop retires nothing while a
/// pin holds the server; the last two holders, a lease and a pin, dropped
/// at once on two threads, retire it exactly once, without either thread
/// waiting on a nested guard.
#[tokio::test]
async fn lease_drop_retires_once() {
    let runtime = TestRuntime::new();
    let servers = Servers::new(runtime.runtime(), DECLINES);
    let (pin, _ends, stdio) = launched(&servers, key(0x05)).await;
    let other = pin.duplicate().unwrap();
    let early = pin.lease().unwrap();
    drop(early);
    drop(other);
    assert_eq!(servers.reports().len(), 1, "the pin still holds it");
    let lease = pin.lease().unwrap();
    assert_eq!(servers.reports()[0].sessions, 1);
    let start = Arc::new(std::sync::Barrier::new(2));
    let (done, finished) = std::sync::mpsc::channel();
    let mut threads = Vec::new();
    let holds: Vec<Box<dyn Send>> = vec![Box::new(lease), Box::new(pin)];
    for hold in holds {
        let (start, done) = (Arc::clone(&start), done.clone());
        threads.push(std::thread::spawn(move || {
            start.wait();
            drop(hold);
            done.send(()).unwrap();
        }));
    }
    for _ in 0..2 {
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("a drop never waits on a nested guard");
    }
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(servers.reports().is_empty());
    until("the retirement's close", || stdio.closes() == 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(stdio.closes(), 1, "retired once");
    assert_eq!(servers.registry().stale, 0);
}

/// Bead via-20s: a server that dies before it answers its handshake, its
/// exit confirmed by Host's stop, fails the launch `ServerLost`; one whose
/// stdout ends while Host finds it live (unconfirmed) fails it
/// `TransportLost`. Either way the waiting turn never launched: nothing of
/// it was sent.
#[tokio::test]
async fn a_handshake_loss_takes_the_connections_cause() {
    use super::{LaunchFailure, LossCause};
    use crate::RouteError;
    use crate::codex::testing::StopFacts;

    let runtime = TestRuntime::new();
    let servers = Servers::new(runtime.runtime(), DECLINES);
    let turn = crate::TurnNumber::try_from(1).unwrap();
    for (first, stopped_live, cause, want) in [
        (
            0x0b,
            Some(false),
            LossCause::ServerLost,
            RouteError::ServerLost { turn },
        ),
        (
            0x0c,
            Some(true),
            LossCause::TransportLost,
            RouteError::TransportLost { turn },
        ),
    ] {
        let (mut ends, stdio) = servers.script();
        stdio.report_stop(StopFacts {
            stopped_live,
            ..StopFacts::default()
        });
        let pin = servers
            .launch_or_join(key(first), (spec(), HandshakeBound::First), Box::new(()))
            .unwrap();
        let initialize = ends.read().await;
        assert_eq!(initialize["method"], "initialize", "{initialize}");
        ends.end_stdout().await;
        let failed =
            tokio::time::timeout(Duration::from_secs(10), pin.ready(std::future::pending()))
                .await
                .unwrap()
                .unwrap_err()
                .unwrap();
        assert_eq!(
            failed.failure,
            LaunchFailure::Lost(cause),
            "{stopped_live:?}"
        );
        let route = failed.failure.route_failure(turn);
        assert!(!route.launched, "nothing of the turn was sent");
        assert_eq!(route.cause, want);
    }
}

/// Bead via-20s (live 2026-10-06, Codex 0.160.0): while the SQLite home
/// has no `.via-initialized` marker, a second first start waits for the
/// in-flight one's handshake to settle before its own process opens, so
/// it never meets Codex's fixed 30 s backfill wait; then it launches, and
/// both servers serve. Two keys, two servers.
#[tokio::test]
async fn first_starts_on_an_unmarked_home_are_serialized() {
    let runtime = TestRuntime::new();
    let servers = Servers::new(runtime.runtime(), DECLINES);
    let (mut a, _a) = servers.script();
    let (mut b, _b) = servers.script();
    let first = servers
        .launch_or_join(key(0x0d), (spec(), HandshakeBound::First), Box::new(()))
        .unwrap();
    let initialize = a.read().await;
    let second = servers
        .launch_or_join(key(0x0e), (spec(), HandshakeBound::First), Box::new(()))
        .unwrap();
    assert!(
        b.silent(Duration::from_millis(300)).await,
        "the second first start waits for the first's handshake"
    );
    a.answer_handshake(&initialize, USER_AGENT, &[model("gpt-6-sol")])
        .await;
    tokio::time::timeout(Duration::from_secs(5), first.ready(std::future::pending()))
        .await
        .unwrap()
        .unwrap();
    b.handshake(USER_AGENT, &[model("gpt-6-sol")]).await;
    tokio::time::timeout(Duration::from_secs(5), second.ready(std::future::pending()))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.server(), second.server());
    assert_eq!(servers.reports().len(), 2);
}

/// Bead via-20s: on a marked home (the warm bound) launches are not
/// serialized: a second key's `initialize` arrives while the first's
/// handshake is still unanswered.
#[tokio::test]
async fn warm_starts_are_not_serialized() {
    let runtime = TestRuntime::new();
    let servers = Servers::new(runtime.runtime(), DECLINES);
    let (mut a, _a) = servers.script();
    let (mut b, _b) = servers.script();
    let first = servers
        .launch_or_join(key(0x0f), (spec(), HandshakeBound::Warm), Box::new(()))
        .unwrap();
    let initialize = a.read().await;
    let second = servers
        .launch_or_join(key(0x10), (spec(), HandshakeBound::Warm), Box::new(()))
        .unwrap();
    b.handshake(USER_AGENT, &[model("gpt-6-sol")]).await;
    tokio::time::timeout(Duration::from_secs(5), second.ready(std::future::pending()))
        .await
        .unwrap()
        .unwrap();
    a.answer_handshake(&initialize, USER_AGENT, &[model("gpt-6-sol")])
        .await;
    tokio::time::timeout(Duration::from_secs(5), first.ready(std::future::pending()))
        .await
        .unwrap()
        .unwrap();
}

/// The formatted tracing events, as `via.log` would receive them.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Bead via-f1q (live run 3): a server lost while the daemon runs is one
/// `via.log` warning naming it; once the registry is fenced for the
/// daemon's shutdown, Host's stop ending a transport writes none.
#[tokio::test]
async fn a_shutdown_stop_logs_no_connection_failure() {
    let captured = Captured::default();
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);
    let runtime = TestRuntime::new();
    let servers = Servers::new(runtime.runtime(), DECLINES);
    let (lost, lost_ends, _) = launched(&servers, key(0x07)).await;
    let (stopped, stopped_ends, _) = launched(&servers, key(0x08)).await;
    let ended = |pin: &ServerPin| {
        let server = pin.server().clone();
        let servers = &servers;
        move || servers.ended().iter().any(|end| end.server == server)
    };
    drop(lost_ends);
    until("the lost server's end", ended(&lost)).await;
    let log = captured.text();
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains(&format!("server={}", lost.server())), "{log}");
    assert!(log.contains("cause=TransportLost"), "{log}");
    servers.fence();
    drop(stopped_ends);
    until("the stopped server's end", ended(&stopped)).await;
    let log = captured.text();
    assert_eq!(log.lines().count(), 1, "no line for a shutdown stop: {log}");
}
