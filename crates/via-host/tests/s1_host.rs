//! Task 3 S1 Host primitives over real anchors and a real Store journal
//! (design §2, §6.8, §7.2 rows 3, 4 and 12, §8): journal failures stop the
//! group through the live control, the gate, re-probe, early stop, and the
//! anchor seams. Each test runs in its own process under nextest.
#![cfg(feature = "test-failpoints")]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    ffi::OsString,
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use via_host::{
    CleanupEvidence, CloseMode, CloseRequest, Deadline, EnvAllowList, Host, HostError, JournalSite,
    LaunchPipes, PrivateProcessSpec, ProcessOwner, ServerId, SessionId, TurnNumber,
    run_anchor_from_args,
};
use via_store::{AnchorPhase, SpawnRecord, Store, SubmissionRecord, TerminalRecord, failpoint};

const TOKEN: &str = "s1-host-failpoint-token-01";

#[test]
fn anchor_entry() {
    if let Some(config) = std::env::var_os("VIA_HOST_TEST_CONFIG") {
        std::process::exit(run_anchor_from_args(&[config]));
    }
}

struct Fixture {
    root: PathBuf,
    points: PathBuf,
    store: Store,
    binary: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "via-s1-host-{}-{}",
            std::process::id(),
            random_hex()
        ));
        for part in ["", "state", "anchors", "points"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(part))
                .unwrap();
        }
        let points = root.join("points");
        failpoint::activate(&points, TOKEN).unwrap();
        let store = Store::open(&root.join("state")).unwrap();
        store
            .client()
            .commit_spawn(SpawnRecord {
                session_id: session(),
                handle_hash: [7_u8; 32],
                receipt: serde_json::json!({"state":"queued"}),
                params: serde_json::json!({"harness":"fake"}),
                label: None,
                prompt: "fixture".into(),
                effective: serde_json::json!({"deadlines":{"wall_ms":1}}),
                initial_event: serde_json::json!({"seq":1,"turn":1,"type":"turn.queued","at":"2026-01-01T00:00:00.000Z"}),
            })
            .await
            .unwrap();
        let binary = root.join("anchor-test-wrapper");
        let executable = std::env::current_exe().unwrap();
        let script = format!(
            "#!/bin/sh\nVIA_HOST_TEST_CONFIG=\"$2\" exec '{}' --exact anchor_entry --nocapture\n",
            executable.to_string_lossy().replace('\'', "'\\''")
        );
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            points,
            store,
            binary,
        }
    }

    /// Commits another session, so anchors can name it as their owner.
    async fn spawn_session(&self, session_id: SessionId) {
        self.store
            .client()
            .commit_spawn(SpawnRecord {
                session_id,
                handle_hash: [8_u8; 32],
                receipt: serde_json::json!({"state":"queued"}),
                params: serde_json::json!({"harness":"fake"}),
                label: None,
                prompt: "fixture".into(),
                effective: serde_json::json!({"deadlines":{"wall_ms":1}}),
                initial_event: serde_json::json!({"seq":1,"turn":1,"type":"turn.queued","at":"2026-01-01T00:00:00.000Z"}),
            })
            .await
            .unwrap();
    }

    fn host(&self) -> Host {
        Host::new(
            self.store.runtime_resources().into_wire_parts().1,
            self.binary.clone(),
            self.root.join("anchors"),
        )
        .unwrap()
    }

    fn spec(&self, program: &str, args: &[&str]) -> PrivateProcessSpec {
        PrivateProcessSpec {
            program: PathBuf::from(program),
            args: args.iter().map(OsString::from).collect(),
            cwd: self.root.clone(),
            env: EnvAllowList::default(),
            owner: ProcessOwner::Turn {
                session_id: session(),
                turn: TurnNumber::try_from(1).unwrap(),
            },
            stderr_path: self.root.join(format!("stderr-{}.log", next_stderr())),
            capacity: None,
        }
    }

    fn arm(&self, point: &str, action: &str) {
        self.arm_with(point, action, false);
    }

    fn arm_with(&self, point: &str, action: &str, persist: bool) {
        let command =
            serde_json::json!({"token":TOKEN,"occurrence":1,"action":action,"persist":persist});
        fs::write(
            self.points.join(format!("{point}.json")),
            command.to_string(),
        )
        .unwrap();
    }

    fn disarm(&self, point: &str) {
        fs::remove_file(self.points.join(format!("{point}.json"))).unwrap();
    }

    fn acked(&self, point: &str) -> bool {
        self.acked_at(point, 1)
    }

    fn acked_at(&self, point: &str, occurrence: u64) -> bool {
        self.points
            .join(format!("{point}.{occurrence}.ack"))
            .exists()
    }

    fn release(&self, point: &str) {
        fs::write(self.points.join(format!("{point}.1.release")), b"").unwrap();
    }

    async fn records(&self) -> Vec<via_store::AnchorRecord> {
        self.store
            .runtime_resources()
            .into_wire_parts()
            .1
            .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT)
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    /// A paused anchor waits for its release file in `points`: removing the
    /// folder first would hide the file, and the anchor would stay paused
    /// after the test (bead via-jm4.19). Every point entered is released,
    /// and the folder is kept until this fixture's anchors have exited,
    /// within a bound.
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(&self.points) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(occurrence) = name.strip_suffix(".ack") {
                    let _ = fs::write(self.points.join(format!("{occurrence}.release")), b"");
                }
            }
        }
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while self.anchors_alive() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// Whether a live process was started with a bootstrap of this
    /// fixture: an anchor's environment names its config under `anchors`.
    fn anchors_alive(&self) -> bool {
        let marker = format!(
            "VIA_HOST_TEST_CONFIG={}/",
            self.root.join("anchors").to_string_lossy()
        );
        let Ok(processes) = fs::read_dir("/proc") else {
            return false;
        };
        processes.flatten().any(|process| {
            fs::read(process.path().join("environ")).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|entry| entry.starts_with(marker.as_bytes()))
            })
        })
    }
}

fn session() -> SessionId {
    SessionId::try_from("s_0123456789ab").unwrap()
}

fn other_session() -> SessionId {
    SessionId::try_from("s_0123456789ac").unwrap()
}

fn random_hex() -> String {
    let mut bytes = [0_u8; 8];
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    format!("{:016x}", u64::from_ne_bytes(bytes))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn within(seconds: u64) -> Deadline {
    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(seconds))
}

/// Polls `done` every 10 ms until it holds or `limit` passes.
async fn eventually(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < until {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    done()
}

/// Whether a process group is gone, by a non-signalling query.
fn group_gone(pgid: u32) -> bool {
    let pgid = rustix::process::Pid::from_raw(i32::try_from(pgid).unwrap()).unwrap();
    matches!(
        rustix::process::test_kill_process_group(pgid),
        Err(rustix::io::Errno::SRCH)
    )
}

/// A capacity token that reports its release.
struct Token(Arc<AtomicBool>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn never() -> impl Fn() -> bool + Send + Sync {
    || false
}

/// Design §7.2 row 4 [r3.12]: Host records the validated identity before
/// `commit_anchor_identified`. When that commit is not committed, the
/// control is dropped before ARM (EOF), the bounded absence check runs from
/// the in-memory identity, and the proof commits that identity with it. On
/// the base, `started` was set only after the commit, so no check ran and
/// the anchor stayed unproven with no identity.
#[test]
fn identified_commit_failure_proves_absence_from_the_recorded_identity() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.arm("store.journal.identified", "fail_io");
        let failed = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .err()
            .unwrap();
        assert!(fixture.acked("store.journal.identified"));
        let records = fixture.records().await;
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.phase, AnchorPhase::Intent);
        assert!(record.identity.is_some(), "identity not recorded: {failed}");
        assert!(record.absence.is_some(), "absence not proved: {failed}");
        assert!(matches!(
            failed,
            HostError::Journal {
                site: JournalSite::Identified,
                uncertain: false
            }
        ));
    });
}

/// Design §7.2 row 4: a vendor-facts commit that is not committed after ARM
/// stops the group through the still-live control (`Stop`, so the anchor
/// reports the vendor it stopped) and proves absence. On the base the
/// control was only dropped (EOF), which carries no force evidence.
#[test]
fn vendor_facts_failure_stops_the_group_through_the_live_control() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.arm("store.journal.vendor_facts", "fail_io");
        let launch = LaunchPipes::default();
        let failure = host
            .acquire_retaining(fixture.spec("/bin/cat", &[]), within(4), &launch, &never())
            .await
            .err()
            .unwrap();
        assert!(matches!(
            failure.error,
            HostError::Journal {
                site: JournalSite::VendorFacts,
                uncertain: false
            }
        ));
        assert!(failure.forced, "{failure:?}");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
        assert!(launch.take().is_some(), "ARM was sent: the pipes are ours");
        let recovered = host
            .recover_page(None, via_store::ANCHOR_PAGE_LIMIT, within(3))
            .await
            .unwrap();
        assert!(recovered[0].forced);
    });
}

/// Design §7.2 row 4, S1 round-1 decision 6: the row-4 `Stop` and the
/// absence check after it share one 3 s cleanup deadline. The anchor holds
/// the `Stop` (`host.anchor.stop_received`), so the `Stop` spends the whole
/// allowance; the absence check then starts no fresh 3 s, and the failure
/// returns unproven within the one bound.
#[test]
fn a_slow_row_four_stop_and_its_absence_check_share_one_deadline() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.arm("store.journal.vendor_facts", "fail_io");
        fixture.arm("host.anchor.stop_received", "pause");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                host.acquire_retaining(spec, within(10), &LaunchPipes::default(), &never())
                    .await
                    .err()
            }
        });
        // The row-4 deadline starts at the vendor-facts failure, just before
        // the anchor receives the `Stop`.
        assert!(
            eventually(Duration::from_secs(5), || fixture
                .acked("host.anchor.stop_received"))
            .await
        );
        let stop_received = tokio::time::Instant::now();
        let failure = acquiring.await.unwrap().unwrap();
        let elapsed = stop_received.elapsed();
        fixture.release("host.anchor.stop_received");
        assert!(fixture.acked("store.journal.vendor_facts"));
        assert!(
            matches!(
                failure.error,
                HostError::Journal {
                    site: JournalSite::VendorFacts,
                    uncertain: false
                }
            ),
            "{failure:?}"
        );
        assert!(!failure.forced, "{failure:?}");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::Uncertain(_))),
            "{failure:?}"
        );
        assert!(
            elapsed < Duration::from_millis(3_500),
            "cleanup took {elapsed:?} after the Stop arrived"
        );
    });
}

/// Round-6 decision 1, S1 round-1 decision 5: reconciliation sends `Stop`
/// only where ARM may have launched a vendor. The ARM intent commit fails,
/// so the anchor stays `identified` (pre-ARM) and is held alive at
/// `host.anchor.before_eof_cleanup`. A witness listener takes the anchor's
/// socket path: reconciliation opens no control connection to it (so it
/// sends no `Stop`), reports cleanup unproven while the anchor lives, and
/// proves absence without force evidence once the anchor exits on its EOF.
#[test]
fn reconciliation_sends_no_stop_to_a_pre_arm_anchor() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.arm("store.journal.arm_intent", "fail_io");
        fixture.arm("host.anchor.before_eof_cleanup", "pause");
        let failure = host
            .acquire_retaining(
                fixture.spec("/bin/cat", &[]),
                within(4),
                &LaunchPipes::default(),
                &never(),
            )
            .await
            .err()
            .unwrap();
        assert!(
            matches!(
                failure.error,
                HostError::Journal {
                    site: JournalSite::ArmIntent,
                    uncertain: false
                }
            ),
            "{failure:?}"
        );
        assert!(fixture.acked("host.anchor.before_eof_cleanup"));
        let records = fixture.records().await;
        assert_eq!(records[0].phase, AnchorPhase::Identified);
        let pgid = records[0].identity.as_ref().unwrap().pgid;
        let socket = records[0].intent.socket_path.clone();
        fs::remove_file(&socket).unwrap();
        let witness = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        witness.set_nonblocking(true).unwrap();
        let unproven = host
            .recover_page(None, via_store::ANCHOR_PAGE_LIMIT, within(1))
            .await
            .unwrap();
        assert!(
            matches!(unproven[0].cleanup, CleanupEvidence::Uncertain(_)),
            "{unproven:?}"
        );
        assert!(!unproven[0].forced, "{unproven:?}");
        let contacted = witness.accept();
        assert!(
            matches!(&contacted, Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "reconciliation contacted a pre-ARM anchor: {contacted:?}"
        );
        fixture.release("host.anchor.before_eof_cleanup");
        assert!(eventually(Duration::from_secs(3), || group_gone(pgid)).await);
        let proved = host
            .recover_page(None, via_store::ANCHOR_PAGE_LIMIT, within(3))
            .await
            .unwrap();
        assert!(
            matches!(proved[0].cleanup, CleanupEvidence::GroupAbsent(_)),
            "{proved:?}"
        );
        assert!(!proved[0].forced, "{proved:?}");
        assert_eq!(host.held_unproven(), 0);
    });
}

/// Design §7.2 row 3: an anchor intent that is not committed starts no
/// process, and the failure says no anchor intent exists (no cleanup).
#[test]
fn anchor_intent_failure_starts_nothing() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.arm("store.journal.anchor_intent", "fail_io");
        let failure = host
            .acquire_retaining(
                fixture.spec("/bin/cat", &[]),
                within(4),
                &LaunchPipes::default(),
                &never(),
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(
            failure.error,
            HostError::Journal {
                site: JournalSite::AnchorIntent,
                uncertain: false
            }
        ));
        assert!(failure.cleanup.is_none() && !failure.forced);
        assert!(fixture.records().await.is_empty());
        assert_eq!(host.pending_cleanup(), 0);
    });
}

/// Runtime §11 `host.anchor.final_reply_lost`: the anchor stops its group
/// but its reply is lost. The close still proves absence, and a lost reply
/// never invents force evidence. On the base the seam was not wired into
/// the anchor, so the reply arrived and reported `forced`.
#[test]
fn a_lost_stop_reply_never_reports_forced() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.arm("host.anchor.final_reply_lost", "fail_io");
        let acquired = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: within(3),
            })
            .await;
        assert!(fixture.acked("host.anchor.final_reply_lost"));
        assert!(!close.forced, "{close:?}");
        assert!(
            matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)),
            "{close:?}"
        );
    });
}

/// Design §7.2 rows 4 and 12, §8: when the anchor is slow to exit
/// (`host.anchor.before_eof_cleanup`), the row-4 absence check stays
/// unproven, so the ledger keeps the token with the in-memory identity and
/// reports it held. Once released, `reprobe_held` proves absence without a
/// signal, commits the proof with the identity and releases the token.
#[test]
fn an_unproven_row_four_group_keeps_its_token_until_reprobe_proves_it() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.arm("store.journal.identified", "fail_io");
        fixture.arm("host.anchor.before_eof_cleanup", "pause");
        let released = Arc::new(AtomicBool::new(false));
        let mut spec = fixture.spec("/bin/cat", &[]);
        spec.capacity = Some(Box::new(Token(released.clone())));
        let failure = host
            .acquire_retaining(spec, within(4), &LaunchPipes::default(), &never())
            .await
            .err()
            .unwrap();
        assert!(fixture.acked("host.anchor.before_eof_cleanup"));
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::Uncertain(_))),
            "{failure:?}"
        );
        assert!(!released.load(Ordering::Acquire));
        assert_eq!(host.held_unproven(), 1);
        assert_eq!(host.pending_cleanup(), 0);
        // Still present: a pass proves nothing and keeps the token.
        let pass = host.reprobe_held(within(2), None).await.unwrap();
        assert_eq!((pass.held, pass.proved), (1, 0));
        assert!(!released.load(Ordering::Acquire));
        fixture.release("host.anchor.before_eof_cleanup");
        let mut proved = 0;
        for _ in 0..500 {
            proved = host.reprobe_held(within(2), None).await.unwrap().proved;
            if proved == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(proved, 1);
        assert!(released.load(Ordering::Acquire));
        assert_eq!(host.held_unproven(), 0);
        let records = fixture.records().await;
        assert!(records[0].identity.is_some() && records[0].absence.is_some());
        // Nothing is held: a pass reads nothing and probes nothing.
        let idle = host.reprobe_held(within(2), None).await.unwrap();
        assert_eq!((idle.held, idle.proved), (0, 0));
    });
}

/// Design §4 dispatcher step 5, §8 [T3-S2 r1.3]: with a session filter, a
/// re-probe pass reports as held only that session's groups, so a close's
/// absence check does not wait on another session's group; `None` keeps the
/// daemon-wide count. A pass that ends before its pages did (here a spent
/// deadline) still counts every group of the session, examined or not, and
/// only those, so no caller takes an unread group of the session for proved.
#[test]
fn a_session_filtered_reprobe_counts_only_that_sessions_groups() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        fixture.spawn_session(other_session()).await;
        let host = fixture.host();
        // Every identified commit fails and every anchor is slow to exit, so
        // each acquisition leaves its group held and unproven.
        fixture.arm_with("store.journal.identified", "fail_io", true);
        fixture.arm("host.anchor.before_eof_cleanup", "pause");
        for owner in [session(), session(), other_session()] {
            let mut spec = fixture.spec("/bin/cat", &[]);
            spec.owner = ProcessOwner::Turn {
                session_id: owner,
                turn: TurnNumber::try_from(1).unwrap(),
            };
            spec.capacity = Some(Box::new(Token(Arc::new(AtomicBool::new(false)))));
            let failure = host
                .acquire_retaining(spec, within(4), &LaunchPipes::default(), &never())
                .await
                .err()
                .unwrap();
            assert!(
                matches!(failure.cleanup, Some(CleanupEvidence::Uncertain(_))),
                "{failure:?}"
            );
        }
        assert_eq!(host.held_unproven(), 3);
        let all = host.reprobe_held(within(2), None).await.unwrap();
        assert_eq!((all.held, all.proved), (3, 0), "{all:?}");
        let mine = host.reprobe_held(within(2), Some(session())).await.unwrap();
        assert_eq!((mine.held, mine.proved), (2, 0), "{mine:?}");
        let theirs = host
            .reprobe_held(within(2), Some(other_session()))
            .await
            .unwrap();
        assert_eq!((theirs.held, theirs.proved), (1, 0), "{theirs:?}");
        // Spent before any page: the session's two groups still count, and
        // only they do; nothing examined is never "all proved".
        let spent = Deadline::at(tokio::time::Instant::now());
        let cut = host.reprobe_held(spent, Some(session())).await.unwrap();
        assert_eq!((cut.held, cut.proved), (2, 0), "{cut:?}");
        let cut = host
            .reprobe_held(spent, Some(other_session()))
            .await
            .unwrap();
        assert_eq!((cut.held, cut.proved), (1, 0), "{cut:?}");
        fixture.release("host.anchor.before_eof_cleanup");
        let mut proved = 0;
        for _ in 0..500 {
            proved += host
                .reprobe_held(within(2), Some(session()))
                .await
                .unwrap()
                .proved;
            if proved == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(proved, 2);
        // The other session's group was never examined, and stays held.
        assert_eq!(host.held_unproven(), 1);
        let done = host.reprobe_held(within(2), Some(session())).await.unwrap();
        assert_eq!((done.held, done.proved), (0, 0), "{done:?}");
    });
}

/// Design §2 rule 1: the pre-ARM gate checks the caller's stop as well as
/// the force signal. Set, no ARM is sent and no vendor launches; the failed
/// acquisition's own absence verification is the cleanup evidence.
#[test]
fn the_gate_refuses_arm_when_the_stop_check_is_set() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let launch = LaunchPipes::default();
        let stopped = || true;
        let failure = host
            .acquire_retaining(fixture.spec("/bin/cat", &[]), within(4), &launch, &stopped)
            .await
            .err()
            .unwrap();
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(launch.take().is_none(), "no ARM, so no vendor pipes");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
        let records = fixture.records().await;
        assert_eq!(records[0].phase, AnchorPhase::ArmIntent);
        assert!(records[0].vendor_pid.is_none());
    });
}

/// Design §6.8 [r4.3]: Host's early-stop task stops every live group on the
/// daemon force signal through the verified control, with no caller
/// polling it, and acknowledges each stop. A later close through the same
/// control owner is a no-op that still reports the stopped vendor.
#[test]
fn early_stop_stops_live_groups_on_the_force_signal() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("host.early_stop.sent", "fail_io");
        let acquired = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let pgid = acquired.control.identity().pgid;
        assert_eq!(host.pending_cleanup(), 1);
        force.send_replace(Some(tokio::time::Instant::now()));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.sent"))
            .await
        );
        assert!(eventually(Duration::from_secs(3), || group_gone(pgid)).await);
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: within(3),
            })
            .await;
        assert!(close.forced, "{close:?}");
        assert!(matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)));
        drop(acquired);
        let report = host.shutdown(within(3), &[]).await;
        assert_eq!(report.pending_tasks, 0, "{report:?}");
    });
}

/// Design §6.8 [r5.2]: a control verified after the early stop's snapshot
/// sees the sticky `stopping` flag when it registers, and is stopped at once
/// (dropped before ARM): no vendor launches and absence is proved. The
/// acquisition is held at `store.journal.anchor_intent`, before its anchor
/// exists, until the early stop acknowledges its snapshot
/// (`host.early_stop.snapshot`). The identified commit, which comes after
/// registration and before the ARM gate, is never reached: registration
/// stopped the group, not the gate.
#[test]
fn a_control_registered_after_the_early_stop_snapshot_is_stopped_at_once() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("store.journal.anchor_intent", "pause");
        fixture.arm("host.early_stop.snapshot", "fail_io");
        fixture.arm("store.journal.identified", "fail_io");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let failure = host
                    .acquire_retaining(spec, within(6), &launch, &|| false)
                    .await
                    .err();
                (failure, launch.take().is_some())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("store.journal.anchor_intent"))
            .await
        );
        force.send_replace(Some(tokio::time::Instant::now()));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.snapshot"))
            .await
        );
        fixture.release("store.journal.anchor_intent");
        let (failure, launched) = acquiring.await.unwrap();
        let failure = failure.unwrap();
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(!launched);
        assert!(
            !fixture.acked("store.journal.identified"),
            "the acquisition passed registration"
        );
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
        assert_eq!(fixture.records().await[0].phase, AnchorPhase::Intent);
    });
}

/// How long the tests below let pass between the force signal and the
/// held acquisition's next step, so that a fresh 3 s cleanup would outlast
/// the early stop's deadline. It only creates elapsed time; every ordering
/// is witnessed by an acknowledgement.
const LATE_STEP: Duration = Duration::from_millis(2_000);

/// The early stop's deadline is the force signal plus 3 s (design §6.8).
/// A second of margin over it covers scheduling only; a fresh 3 s from
/// `LATE_STEP` after the force would exceed the bound by a second too. The
/// deadline value itself is asserted without a clock in `host.rs`'s unit
/// tests.
const EARLY_STOP_BOUND: Duration = Duration::from_millis(4_000);

/// S1 round-2 decision 8: a control registered after the early stop's
/// snapshot is dropped (EOF), and its cleanup runs under the early stop's
/// deadline, not a fresh 3 s. The anchor holds its EOF exit at
/// `host.anchor.before_eof_cleanup`, so absence stays unproven: the failure
/// returns by that deadline with `Uncertain` cleanup, and the group stays
/// held.
#[test]
fn a_late_registered_control_is_cleaned_up_under_the_early_stop_deadline() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("store.journal.anchor_intent", "pause");
        fixture.arm("host.early_stop.snapshot", "fail_io");
        fixture.arm("host.anchor.before_eof_cleanup", "pause");
        let released = Arc::new(AtomicBool::new(false));
        let acquiring = tokio::spawn({
            let host = host.clone();
            let mut spec = fixture.spec("/bin/cat", &[]);
            spec.capacity = Some(Box::new(Token(released.clone())));
            async move {
                host.acquire_retaining(spec, within(10), &LaunchPipes::default(), &never())
                    .await
                    .err()
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("store.journal.anchor_intent"))
            .await
        );
        let forced_at = tokio::time::Instant::now();
        force.send_replace(Some(forced_at));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.snapshot"))
            .await
        );
        tokio::time::sleep_until(forced_at + LATE_STEP).await;
        fixture.release("store.journal.anchor_intent");
        let failure = acquiring.await.unwrap().unwrap();
        let elapsed = forced_at.elapsed();
        assert!(fixture.acked("host.anchor.before_eof_cleanup"));
        fixture.release("host.anchor.before_eof_cleanup");
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::Uncertain(_))),
            "{failure:?}"
        );
        assert!(
            elapsed < EARLY_STOP_BOUND,
            "cleanup returned {elapsed:?} after the force signal"
        );
        assert!(!released.load(Ordering::Acquire));
        assert_eq!(host.held_unproven(), 1);
    });
}

/// Design §6.8, Sol review of Task 3: the early stop's deadline is the
/// instant the force was raised plus 3 s, never a fresh 3 s from when Host's
/// task runs. The force instant is `LATE_STEP` older than the moment the task
/// sees it, as if the task had been delayed that long; a control registered
/// after its snapshot is cleaned up under the force's deadline, so its
/// failure returns within the bound measured from the force instant. The
/// anchor holds its EOF exit, so the cleanup runs to that deadline.
///
/// The acquisition is held at `store.journal.anchor_intent`, before its
/// anchor exists, so it registers after the snapshot; the identified commit,
/// which follows registration and precedes the ARM gate, is never reached,
/// which witnesses that registration, not the gate, took the deadline
/// (Sol review round 2, D5-4). The deadline value itself is asserted
/// without a clock by the ledger unit tests in `host.rs`.
#[test]
fn a_force_older_than_the_early_stop_task_bounds_a_late_registered_cleanup() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("store.journal.anchor_intent", "pause");
        fixture.arm("host.early_stop.snapshot", "fail_io");
        fixture.arm("store.journal.identified", "fail_io");
        fixture.arm("host.anchor.before_eof_cleanup", "pause");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                host.acquire_retaining(spec, within(10), &LaunchPipes::default(), &never())
                    .await
                    .err()
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("store.journal.anchor_intent"))
            .await
        );
        let forced_at = tokio::time::Instant::now() - LATE_STEP;
        force.send_replace(Some(forced_at));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.snapshot"))
            .await
        );
        fixture.release("store.journal.anchor_intent");
        let failure = acquiring.await.unwrap().unwrap();
        let elapsed = forced_at.elapsed();
        assert!(fixture.acked("host.anchor.before_eof_cleanup"));
        fixture.release("host.anchor.before_eof_cleanup");
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::Uncertain(_))),
            "{failure:?}"
        );
        assert!(
            elapsed < EARLY_STOP_BOUND,
            "cleanup returned {elapsed:?} after the force instant"
        );
        assert!(
            !fixture.acked("store.journal.identified"),
            "the acquisition passed registration"
        );
    });
}

/// S1 round-3 decision 11: the caller's stop check (here the daemon force
/// signal, as Route's gate reads it) is set together with the ledger's
/// `stopping`. The caller's check refuses ARM first, and the cleanup still
/// runs under the early stop's deadline, not a fresh 3 s. The anchor holds
/// its EOF exit at `host.anchor.before_eof_cleanup`, so absence stays
/// unproven until that deadline.
#[test]
fn a_caller_stop_under_the_early_stop_keeps_its_deadline() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        let caller = signal.clone();
        host.watch_force(signal);
        fixture.arm("host.anchor.after_arm_intent_commit", "pause");
        fixture.arm("host.early_stop.snapshot", "fail_io");
        fixture.arm("host.anchor.before_eof_cleanup", "pause");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let stopped = move || caller.borrow().is_some();
                let failure = host
                    .acquire_retaining(spec, within(10), &launch, &stopped)
                    .await
                    .err();
                (failure, launch.take().is_some())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.after_arm_intent_commit"))
            .await
        );
        let forced_at = tokio::time::Instant::now();
        force.send_replace(Some(forced_at));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.snapshot"))
            .await
        );
        tokio::time::sleep_until(forced_at + LATE_STEP).await;
        fixture.release("host.anchor.after_arm_intent_commit");
        let (failure, launched) = acquiring.await.unwrap();
        let elapsed = forced_at.elapsed();
        let failure = failure.unwrap();
        assert!(fixture.acked("host.anchor.before_eof_cleanup"));
        fixture.release("host.anchor.before_eof_cleanup");
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(!launched, "the caller's check refused ARM");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::Uncertain(_))),
            "{failure:?}"
        );
        assert!(
            elapsed < EARLY_STOP_BOUND,
            "cleanup returned {elapsed:?} after the force signal"
        );
    });
}

/// S1 round-2 decision 8, the `Spawned` path: ARM completes after the early
/// stop's snapshot, so the owner sends `Stop` itself. The anchor defers
/// that cleanup (`host.anchor.defer_cleanup`), so absence stays unproven,
/// and the absence check after the owner's `Stop` ends at the early stop's
/// deadline, not a fresh 3 s.
#[test]
fn an_owner_stop_after_the_snapshot_keeps_the_early_stop_deadline() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("host.anchor.arm_received", "pause");
        fixture.arm("host.early_stop.snapshot", "fail_io");
        fixture.arm_with("host.anchor.defer_cleanup", "fail_io", true);
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let failure = host
                    .acquire_retaining(spec, within(10), &launch, &never())
                    .await
                    .err();
                (failure, launch.take())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.arm_received"))
            .await
        );
        let forced_at = tokio::time::Instant::now();
        force.send_replace(Some(forced_at));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.snapshot"))
            .await
        );
        tokio::time::sleep_until(forced_at + LATE_STEP).await;
        fixture.release("host.anchor.arm_received");
        let (failure, pipes) = acquiring.await.unwrap();
        let elapsed = forced_at.elapsed();
        let failure = failure.unwrap();
        assert!(fixture.acked("host.anchor.defer_cleanup"));
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(pipes.is_some(), "ARM was sent: the pipes are ours");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::Uncertain(_))),
            "{failure:?}"
        );
        assert!(
            elapsed < EARLY_STOP_BOUND,
            "cleanup returned {elapsed:?} after the force signal"
        );
        // Final reconciliation supplies the proof once cleanup may run.
        fixture.disarm("host.anchor.defer_cleanup");
        let recovered = host
            .recover_page(None, via_store::ANCHOR_PAGE_LIMIT, within(3))
            .await
            .unwrap();
        assert!(matches!(
            recovered[0].cleanup,
            CleanupEvidence::GroupAbsent(_)
        ));
        drop(pipes);
    });
}

/// Sol review of Task 3 round 2, D5-2: the early-stop task is woken by the
/// force but held at `host.early_stop.woken`, so it has taken no snapshot
/// and set no `stopping`, as when a blocked runtime delays it. A control
/// that registers now reads the force in its own ledger section and is
/// stopped at once: the identified commit, after registration, is never
/// reached.
#[test]
fn a_control_registering_before_the_delayed_early_stop_task_runs_is_stopped_at_once() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("host.early_stop.woken", "pause");
        fixture.arm("store.journal.anchor_intent", "pause");
        fixture.arm("store.journal.identified", "fail_io");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let failure = host
                    .acquire_retaining(spec, within(6), &launch, &never())
                    .await
                    .err();
                (failure, launch.take().is_some())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("store.journal.anchor_intent"))
            .await
        );
        force.send_replace(Some(tokio::time::Instant::now()));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.woken"))
            .await
        );
        fixture.release("store.journal.anchor_intent");
        let (failure, launched) = acquiring.await.unwrap();
        let failure = failure.expect("registration refused the launch");
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(!launched);
        assert!(
            !fixture.acked("store.journal.identified"),
            "the acquisition passed registration"
        );
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
        fixture.release("host.early_stop.woken");
        let report = host.shutdown(within(3), &[]).await;
        assert_eq!(report.pending_tasks, 0, "{report:?}");
    });
}

/// Sol review of Task 3 round 2, D5-2: the same delayed task, and the
/// acquisition is held after its ARM intent committed, before the ARM gate.
/// The force is raised while it waits. The gate reads the force in its own
/// ledger section and refuses, though the task has set no `stopping`: the
/// anchor never receives ARM, and no `Stop` message goes to a pre-ARM anchor.
#[test]
fn the_arm_gate_refuses_after_the_force_though_the_early_stop_task_is_delayed() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("host.early_stop.woken", "pause");
        fixture.arm("host.anchor.after_arm_intent_commit", "pause");
        fixture.arm("host.anchor.arm_received", "pause");
        fixture.arm("host.anchor.stop_received", "pause");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let failure = host
                    .acquire_retaining(spec, within(6), &launch, &never())
                    .await
                    .err();
                (failure, launch.take().is_some())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.after_arm_intent_commit"))
            .await
        );
        force.send_replace(Some(tokio::time::Instant::now()));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.woken"))
            .await
        );
        fixture.release("host.anchor.after_arm_intent_commit");
        let (failure, launched) = acquiring.await.unwrap();
        let failure = failure.expect("the ARM gate refused the launch");
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(!launched, "ARM was sent: the pipes are Host's");
        assert!(!fixture.acked("host.anchor.arm_received"), "ARM was sent");
        assert!(
            !fixture.acked("host.anchor.stop_received"),
            "a pre-ARM anchor received Stop"
        );
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
        fixture.release("host.early_stop.woken");
        let report = host.shutdown(within(3), &[]).await;
        assert_eq!(report.pending_tasks, 0, "{report:?}");
    });
}

/// Sol review of Task 3 round 2, D5-2, the `Spawned` path with a delayed
/// task: the force is raised while ARM is in flight (the anchor holds ARM at
/// `host.anchor.arm_received`) and the task is held at
/// `host.early_stop.woken`, so no `stopping` is set when the vendor spawns.
/// The owner's `Armed` marking reads the force, and the owner sends `Stop`
/// itself: the vendor is stopped live, its forced evidence is kept, and
/// absence is proved.
#[test]
fn a_vendor_spawned_under_a_delayed_early_stop_task_is_stopped_by_its_owner() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("host.early_stop.woken", "pause");
        fixture.arm("host.anchor.arm_received", "pause");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let failure = host
                    .acquire_retaining(spec, within(10), &launch, &never())
                    .await
                    .err();
                (failure, launch.take())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.arm_received"))
            .await
        );
        force.send_replace(Some(tokio::time::Instant::now()));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.woken"))
            .await
        );
        fixture.release("host.anchor.arm_received");
        let (failure, pipes) = acquiring.await.unwrap();
        let failure = failure.expect("the owner stopped the spawned vendor");
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(failure.forced, "the anchor stopped a live vendor");
        assert!(pipes.is_some(), "ARM was sent: the pipes are ours");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
        fixture.release("host.early_stop.woken");
        drop(pipes);
        let report = host.shutdown(within(3), &[]).await;
        assert_eq!(report.pending_tasks, 0, "{report:?}");
    });
}

/// Sol review of Task 3 round 2, D5-3: the force is 10 s old, so the
/// deadline passed 7 s ago before the owner's `Armed` marking saw it. The
/// owner still writes its one `Stop` (the anchor acknowledges receiving it
/// at `host.anchor.stop_received` and holds its reply), waits for no reply
/// past the deadline, and so records no forced evidence. Cleanup is
/// unproven at the passed deadline, `Uncertain`, and step 4's proof follows
/// once the anchor's cleanup runs.
#[test]
fn a_stop_after_its_deadline_is_still_sent_and_leaves_absence_to_reconciliation() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("host.anchor.arm_received", "pause");
        fixture.arm("host.early_stop.snapshot", "fail_io");
        fixture.arm("host.anchor.stop_received", "pause");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let failure = host
                    .acquire_retaining(spec, within(10), &launch, &never())
                    .await
                    .err();
                (failure, launch.take())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.arm_received"))
            .await
        );
        force.send_replace(Some(tokio::time::Instant::now() - Duration::from_secs(10)));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.snapshot"))
            .await
        );
        fixture.release("host.anchor.arm_received");
        let (failure, pipes) = acquiring.await.unwrap();
        let failure = failure.expect("the owner stopped the spawned vendor");
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.stop_received"))
            .await,
            "no Stop was sent past the deadline"
        );
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(!failure.forced, "forced evidence without a reply");
        assert!(pipes.is_some(), "ARM was sent: the pipes are ours");
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::Uncertain(_))),
            "{failure:?}"
        );
        fixture.release("host.anchor.stop_received");
        drop(pipes);
        let recovered = eventually_recovered(&host).await;
        assert!(
            matches!(recovered, Some(CleanupEvidence::GroupAbsent(_))),
            "{recovered:?}"
        );
    });
}

/// Step 4's proof for the single group a test left: recovery is retried
/// until the anchor's own cleanup has run.
async fn eventually_recovered(host: &Host) -> Option<CleanupEvidence> {
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let page = host
            .recover_page(None, via_store::ANCHOR_PAGE_LIMIT, within(3))
            .await
            .unwrap();
        if let Some(recovered) = page.into_iter().next()
            && matches!(recovered.cleanup, CleanupEvidence::GroupAbsent(_))
        {
            return Some(recovered.cleanup);
        }
        if tokio::time::Instant::now() >= until {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Design §6.8: an untriggered early-stop task never holds up shutdown.
#[test]
fn an_untriggered_early_stop_task_is_joined_by_shutdown() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (_force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        // Design §6.8 [r6.2]: the task itself is not pending cleanup.
        assert_eq!(host.pending_cleanup(), 0);
        let report = host.shutdown(within(2), &[]).await;
        assert_eq!((report.pending_tasks, report.failed_tasks), (0, 0));
        assert!(report.failure.is_none(), "{report:?}");
    });
}

/// Design §2 (F20), characterization: a vendor that ignores `SIGTERM` is
/// killed by the anchor's escalation well within close's 3 s allowance, and
/// the close reports it stopped a live vendor.
#[test]
fn a_vendor_ignoring_sigterm_is_killed_within_the_allowance() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let acquired = host
            .acquire(
                fixture.spec("/bin/sh", &["-c", "trap '' TERM; exec sleep 60"]),
                within(4),
            )
            .await
            .unwrap();
        let pgid = acquired.control.identity().pgid;
        let started = tokio::time::Instant::now();
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: within(3),
            })
            .await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(close.forced, "{close:?}");
        assert!(matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)));
        assert!(group_gone(pgid));
    });
}

/// Design §6.8 [r6.1]: ARM is in flight (the anchor holds it at
/// `host.anchor.arm_received`) when the early stop takes its snapshot, so the
/// snapshot, which holds only armed controls, misses the group. Marking it
/// armed after `Spawned`, the owner sees `stopping` and sends `Stop` itself
/// under the original deadline: acknowledged, and the group is gone.
#[test]
fn arm_completing_after_the_snapshot_is_stopped_by_its_owner() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("host.anchor.arm_received", "pause");
        fixture.arm("host.early_stop.snapshot", "pause");
        fixture.arm("host.early_stop.sent", "fail_io");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let failure = host
                    .acquire_retaining(spec, within(6), &launch, &|| false)
                    .await
                    .err();
                (failure, launch.take().is_some())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.arm_received"))
            .await
        );
        force.send_replace(Some(tokio::time::Instant::now()));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.snapshot"))
            .await
        );
        fixture.release("host.anchor.arm_received");
        let (failure, launched) = acquiring.await.unwrap();
        let failure = failure.unwrap();
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(launched && failure.forced, "{failure:?}");
        assert!(fixture.acked("host.early_stop.sent"));
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
        fixture.release("host.early_stop.snapshot");
    });
}

/// Design §6.8 [r6.1]: before ARM the early stop sends nothing (the pre-ARM
/// anchor treats `Stop` as invalid); the ARM gate reads `stopping` and
/// refuses: `Stopped`, not launched, no `Stop` message, absence proved.
#[test]
fn a_pre_arm_acquisition_is_refused_at_the_gate_without_a_stop_message() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        fixture.arm("host.anchor.after_arm_intent_commit", "pause");
        fixture.arm("host.early_stop.snapshot", "fail_io");
        fixture.arm("host.early_stop.sent", "fail_io");
        let acquiring = tokio::spawn({
            let host = host.clone();
            let spec = fixture.spec("/bin/cat", &[]);
            async move {
                let launch = LaunchPipes::default();
                let failure = host
                    .acquire_retaining(spec, within(6), &launch, &|| false)
                    .await
                    .err();
                (failure, launch.take().is_some())
            }
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.after_arm_intent_commit"))
            .await
        );
        force.send_replace(Some(tokio::time::Instant::now()));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.snapshot"))
            .await
        );
        fixture.release("host.anchor.after_arm_intent_commit");
        let (failure, launched) = acquiring.await.unwrap();
        let failure = failure.unwrap();
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(!launched && !failure.forced, "{failure:?}");
        assert!(!fixture.acked("host.early_stop.sent"));
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
    });
}

/// Design §6.8 [r4.2, r6.3]: with `host.anchor.defer_cleanup` armed, the
/// anchor keeps its group alive through Route's force close (no force
/// evidence, cleanup uncertain) and through the control's EOF; the first
/// `Stop` after the harness disarms it is reconciliation's, which starts
/// the cleanup, supplies `stopped_live` and proves absence.
#[test]
fn reconciliation_supplies_the_evidence_a_deferred_close_lacked() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.arm_with("host.anchor.defer_cleanup", "fail_io", true);
        let acquired = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let pgid = acquired.control.identity().pgid;
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: within(1),
            })
            .await;
        assert!(!close.forced, "{close:?}");
        assert!(matches!(close.cleanup, CleanupEvidence::Uncertain(_)));
        // The vendor keeps its stdin; only the control is released.
        let via_host::AcquiredProcess { pipes, control, .. } = acquired;
        drop(control);
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked_at("host.anchor.defer_cleanup", 2))
            .await,
            "the control's EOF was not deferred"
        );
        assert!(!group_gone(pgid));
        fixture.disarm("host.anchor.defer_cleanup");
        let recovered = host
            .recover_page(None, via_store::ANCHOR_PAGE_LIMIT, within(3))
            .await
            .unwrap();
        assert!(recovered[0].forced, "{recovered:?}");
        assert!(matches!(
            recovered[0].cleanup,
            CleanupEvidence::GroupAbsent(_)
        ));
        assert!(group_gone(pgid));
        drop(pipes);
    });
}

/// Design §6.8 [r5.3, r6.5]: the early stop's Stops run concurrently. One
/// anchor holds its `Stop` at `host.anchor.stop_received` (a close already
/// owns that control); the other group's `host.early_stop.sent` arrives
/// while the pause is still held.
#[test]
fn early_stops_are_concurrent() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(None);
        host.watch_force(signal);
        let busy = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let other = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        fixture.arm("host.anchor.stop_received", "pause");
        fixture.arm("host.early_stop.sent", "fail_io");
        let closing = tokio::spawn(async move {
            busy.control
                .close(CloseRequest {
                    mode: CloseMode::Force,
                    deadline: within(8),
                })
                .await
        });
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.anchor.stop_received"))
            .await
        );
        // Only the held anchor pauses; the other one's Stop proceeds.
        fixture.disarm("host.anchor.stop_received");
        force.send_replace(Some(tokio::time::Instant::now()));
        assert!(
            eventually(Duration::from_secs(3), || fixture
                .acked("host.early_stop.sent"))
            .await,
            "the free group's stop waited behind the busy one"
        );
        assert!(
            eventually(Duration::from_secs(3), || group_gone(
                other.control.identity().pgid
            ))
            .await
        );
        fixture.release("host.anchor.stop_received");
        let close = closing.await.unwrap();
        assert!(matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)));
    });
}

/// S1 critic finding 5: a live Host collects each finished turn's reaper
/// and exit poll, and prunes its dropped control, as later turns begin; the
/// registries stay bounded instead of growing per turn. S1-io review r1
/// finding 1: forced turns too, whose forced-stop fact their close already
/// handed to the owner, leave no Host-wide fact behind.
#[test]
fn live_service_collects_finished_turn_tasks() {
    const TURNS: usize = 6;
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        for turn in 0..2 * TURNS {
            let forced = turn >= TURNS;
            let (program, mode) = if forced {
                ("/bin/sleep", CloseMode::Force)
            } else {
                ("/bin/true", CloseMode::Graceful)
            };
            let args: &[&str] = if forced { &["60"] } else { &[] };
            let acquired = host
                .acquire(fixture.spec(program, args), within(4))
                .await
                .unwrap();
            let close = acquired
                .control
                .close(CloseRequest {
                    mode,
                    deadline: within(3),
                })
                .await;
            assert!(
                matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)),
                "{close:?}"
            );
            assert_eq!(close.forced, forced, "{close:?}");
            drop(acquired);
        }
        // At most this turn's and the previous turn's two tasks and control.
        let (tasks, controls, facts) = host.tracked();
        assert!(
            tasks <= 4 && controls <= 2 && facts == 0,
            "{tasks} tasks, {controls} controls and {facts} forced facts tracked \
             after {TURNS} graceful and {TURNS} forced turns"
        );
        let report = host.shutdown(within(3), &[]).await;
        assert_eq!((report.pending_tasks, report.failed_tasks), (0, 0));
    });
}

/// Continues a stopped anchor when dropped, so a failed assertion never
/// leaves it stopped.
struct Stopped(rustix::process::Pid);

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.0, rustix::process::Signal::CONT);
    }
}

/// S1-io review r1, decision 3: a retired control is shut down, so the
/// anchor sees control EOF and cleans up its group (runtime §5.1). A
/// stopped anchor answers no `Status`, so the exit poll's bound retires the
/// control; `close` then sends no `Stop` and proves absence through the
/// journal, with no forced evidence.
#[test]
fn a_retired_control_is_shut_down_and_the_anchor_cleans_up_on_eof() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let mut acquired = host
            .acquire(fixture.spec("/bin/sleep", &["60"]), within(4))
            .await
            .unwrap();
        let identity = acquired.control.identity().clone();
        let anchor = rustix::process::Pid::from_raw(i32::try_from(identity.pid).unwrap()).unwrap();
        rustix::process::kill_process(anchor, rustix::process::Signal::STOP).unwrap();
        let stopped = Stopped(anchor);
        let ended = tokio::time::timeout(Duration::from_secs(5), acquired.exits.changed()).await;
        drop(stopped);
        assert!(matches!(ended, Ok(Err(_))), "exit supervision did not end");
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: within(3),
            })
            .await;
        assert!(
            matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)),
            "{close:?}"
        );
        assert!(!close.forced, "control EOF made forced evidence: {close:?}");
        assert!(group_gone(identity.pgid));
        drop(acquired);
        let report = host.shutdown(within(3), &[]).await;
        assert_eq!((report.pending_tasks, report.failed_tasks), (0, 0));
    });
}

/// A fresh `stderr.log` name per spec: Host creates it exclusively.
fn next_stderr() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// x.3.2 X0 item 1: a shared server's spec. Its owner is no turn.
fn server_spec(fixture: &Fixture) -> PrivateProcessSpec {
    let mut spec = fixture.spec("/bin/cat", &[]);
    spec.owner = ProcessOwner::Server {
        server_id: ServerId::try_from("v_0123456789ab").unwrap(),
    };
    spec
}

fn turn(number: u32) -> TurnNumber {
    TurnNumber::try_from(number).unwrap()
}

impl Fixture {
    fn journal(&self) -> via_store::ProcessJournal {
        self.store.runtime_resources().into_wire_parts().1
    }

    /// Submits the fixture session's turn 1, so it is `running`.
    async fn run_turn_one(&self, session_id: SessionId) {
        self.store
            .client()
            .commit_submission(SubmissionRecord {
                session_id,
                turn: turn(1),
                event: serde_json::json!({"seq":2,"turn":1,"type":"turn.submitted","at":"2026-01-01T00:00:00.000Z"}),
            })
            .await
            .unwrap();
    }

    /// The links of `session`'s turn 1, as anchor ids.
    async fn links(&self, session_id: SessionId) -> Vec<String> {
        self.journal()
            .server_links(vec![(session_id, turn(1))])
            .await
            .unwrap()
            .into_iter()
            .map(|link| link.anchor_id)
            .collect()
    }
}

/// x.3.2 X0 item 1 (runtime §5 AR6): a shared server's group has no turn
/// owner and outlives the turns linked to it. The turn's link commits once,
/// before its first byte, and goes with the turn's quiescent terminal; the
/// server stays live, and its own close still proves it absent.
#[test]
fn host_server_owner_outlives_turns() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.run_turn_one(session()).await;
        let acquired = host.acquire(server_spec(&fixture), within(4)).await.unwrap();
        let records = fixture.records().await;
        assert_eq!(records.len(), 1);
        assert!(matches!(
            records[0].intent.owner,
            ProcessOwner::Server { .. }
        ));
        let anchor_id = records[0].intent.anchor_id.clone();
        acquired
            .control
            .link_turn(&session(), turn(1), within(2))
            .await
            .unwrap();
        assert_eq!(fixture.links(session()).await, vec![anchor_id.clone()]);
        // A second link for the same turn is refused, nothing written.
        let again = acquired
            .control
            .link_turn(&session(), turn(1), within(2))
            .await;
        assert!(
            matches!(
                again,
                Err(HostError::Journal {
                    site: JournalSite::Link,
                    uncertain: false
                })
            ),
            "{again:?}"
        );
        fixture
            .store
            .client()
            .commit_terminal(TerminalRecord {
                session_id: session(),
                turn: turn(1),
                envelope: serde_json::json!({"state":"failed"}),
                event: serde_json::json!({"seq":3,"turn":1,"type":"turn.ended","at":"2026-01-01T00:00:00.000Z"}),
                steps: Vec::new(),
                link_released: true,
            })
            .await
            .unwrap();
        assert!(fixture.links(session()).await.is_empty());
        // The turn ended; its server did not.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(host.live_armed(std::slice::from_ref(&anchor_id)));
        assert!(acquired.exits.borrow().is_none());
        assert!(!group_gone(acquired.control.identity().pgid));
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: within(3),
            })
            .await;
        assert!(
            matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)),
            "{close:?}"
        );
    });
}

/// x.3.2 X0 item 1: only a shared server's control links turns.
#[test]
fn link_turn_on_turn_owner_is_invalid() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.run_turn_one(session()).await;
        let acquired = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let linked = acquired
            .control
            .link_turn(&session(), turn(1), within(2))
            .await;
        assert!(matches!(linked, Err(HostError::Invalid(_))), "{linked:?}");
        assert!(fixture.links(session()).await.is_empty());
        acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: within(3),
            })
            .await;
    });
}

/// x.3.2 X0 item 2.4: a live, idle shared server is not pending cleanup, so
/// it never blocks daemon idle exit (runtime §5 AR6); a turn's live group
/// still is. Final shutdown stops both.
#[test]
fn daemon_idle_exit_not_blocked_by_idle_server() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let server = host
            .acquire(server_spec(&fixture), within(4))
            .await
            .unwrap();
        assert_eq!(host.pending_cleanup(), 0);
        let private = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        assert_eq!(host.pending_cleanup(), 1);
        let pgids = [
            server.control.identity().pgid,
            private.control.identity().pgid,
        ];
        let report = host.shutdown(within(5), &[]).await;
        assert!(report.failure.is_none(), "{report:?}");
        assert_eq!((report.anchors, report.uncertain_anchors), (2, 0));
        assert!(pgids.into_iter().all(group_gone));
    });
}

/// x.3.2 X0 item 2.6: Host's sticky journal-uncertain watch is set where
/// Host observes an uncertain outcome, whatever the owner: a server link,
/// then (on a fresh Host) a turn's absence proof.
#[test]
fn host_journal_uncertain_watch() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let watch = host.journal_uncertain();
        fixture.run_turn_one(session()).await;
        let server = host
            .acquire(server_spec(&fixture), within(4))
            .await
            .unwrap();
        assert!(!*watch.borrow());
        fixture.arm("store.journal.server_turn", "fail_io");
        fixture.arm("store.rollback.fail", "fail_io");
        let linked = server
            .control
            .link_turn(&session(), turn(1), within(2))
            .await;
        assert!(
            matches!(
                linked,
                Err(HostError::Journal {
                    site: JournalSite::Link,
                    uncertain: true
                })
            ),
            "{linked:?}"
        );
        assert!(*watch.borrow(), "an uncertain link did not set the watch");
        let _ = host.shutdown(within(5), &[]).await;

        let host = fixture.host();
        let watch = host.journal_uncertain();
        let private = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        // From here every absence commit fails and its rollback too.
        fixture.arm_with("store.journal.absence", "fail_io", true);
        fixture.arm_with("store.rollback.fail", "fail_io", true);
        let close = private
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: within(3),
            })
            .await;
        assert!(close.journal_uncertain, "{close:?}");
        assert!(
            *watch.borrow(),
            "an uncertain absence did not set the watch"
        );
    });
}

/// x.3.2 X0 item 2.6 (X2 r1 #4): observation does not depend on the
/// requester staying alive. A link whose future is dropped once its write
/// is enqueued leaves an outcome no one observes, which may be a commit:
/// Host's sticky watch is set.
#[test]
fn dropped_link_sets_journal_uncertain() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let watch = host.journal_uncertain();
        fixture.run_turn_one(session()).await;
        let server = host
            .acquire(server_spec(&fixture), within(4))
            .await
            .unwrap();
        fixture.arm("store.journal.server_turn", "pause");
        let linked = session();
        let link = server.control.link_turn(&linked, turn(1), within(10));
        let dropped = tokio::time::timeout(Duration::from_millis(500), link).await;
        assert!(dropped.is_err(), "the link was not held: {dropped:?}");
        assert!(fixture.acked("store.journal.server_turn"));
        fixture.release("store.journal.server_turn");
        assert!(
            eventually(Duration::from_secs(2), || *watch.borrow()).await,
            "a dropped link's write did not set the watch"
        );
        let _ = host.shutdown(within(5), &[]).await;
    });
}

/// x.3.2 X0 item 2.6 (X2 r1 #4): an acquisition whose deadline cuts an
/// outstanding journal write leaves its outcome unobserved, and it may be
/// a commit: Host's sticky watch is set.
#[test]
fn acquisition_cut_during_a_write_sets_journal_uncertain() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let watch = host.journal_uncertain();
        fixture.arm("store.journal.anchor_intent", "pause");
        let acquired = host
            .acquire(
                fixture.spec("/bin/cat", &[]),
                Deadline::at(tokio::time::Instant::now() + Duration::from_millis(500)),
            )
            .await;
        assert!(
            matches!(acquired, Err(HostError::Deadline)),
            "{:?}",
            acquired.err()
        );
        assert!(fixture.acked("store.journal.anchor_intent"));
        fixture.release("store.journal.anchor_intent");
        assert!(
            eventually(Duration::from_secs(2), || *watch.borrow()).await,
            "an acquisition cut during its write did not set the watch"
        );
        let _ = host.shutdown(within(5), &[]).await;
    });
}

/// x.3.2 X0 item 13.1 (runtime §5 stop reply): the close reports the
/// anchor's own reply to its `Stop`: `Some(true)` for a live vendor,
/// `Some(false)` once the vendor had exited, `None` when the reply is lost
/// or the deadline passed.
#[test]
fn close_reports_stop_reply() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let force = |deadline| CloseRequest {
            mode: CloseMode::Force,
            deadline,
        };
        let live = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let close = live.control.close(force(within(3))).await;
        assert_eq!(close.stopped_live, Some(true), "{close:?}");

        let exited = host
            .acquire(fixture.spec("/bin/true", &[]), within(4))
            .await
            .unwrap();
        let mut exits = exited.exits.clone();
        exits.wait_for(Option::is_some).await.map(|_| ()).unwrap();
        let close = exited.control.close(force(within(3))).await;
        assert_eq!(close.stopped_live, Some(false), "{close:?}");

        fixture.arm("host.anchor.final_reply_lost", "fail_io");
        let lost = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let close = lost.control.close(force(within(3))).await;
        assert!(fixture.acked("host.anchor.final_reply_lost"));
        assert_eq!(close.stopped_live, None, "{close:?}");
        fixture.disarm("host.anchor.final_reply_lost");

        // The anchor holds the `Stop` past the close's deadline.
        fixture.arm("host.anchor.stop_received", "pause");
        let late = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let close = late
            .control
            .close(force(Deadline::at(
                tokio::time::Instant::now() + Duration::from_millis(500),
            )))
            .await;
        assert!(fixture.acked("host.anchor.stop_received"));
        assert_eq!(close.stopped_live, None, "{close:?}");
        fixture.release("host.anchor.stop_received");
        let _ = host.shutdown(within(5), &[]).await;
    });
}

/// x.3.2 X0 item 6.3: final shutdown closes every group first, then folds a
/// server anchor's cleanup, never its `forced`, into each requested turn
/// linked to it.
#[test]
fn shutdown_folds_server_cleanup_into_linked_turns() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        fixture.run_turn_one(session()).await;
        let server = host
            .acquire(server_spec(&fixture), within(4))
            .await
            .unwrap();
        server
            .control
            .link_turn(&session(), turn(1), within(2))
            .await
            .unwrap();
        let report = host.shutdown(within(5), &[(session(), turn(1))]).await;
        assert!(report.failure.is_none(), "{report:?}");
        let [recovery] = report.recovery.as_slice() else {
            panic!("{report:?}");
        };
        assert_eq!(
            (&recovery.owner_session, recovery.owner_turn),
            (&session(), turn(1))
        );
        assert!(
            matches!(recovery.cleanup, CleanupEvidence::GroupAbsent(_)),
            "{report:?}"
        );
        assert!(!recovery.forced, "a server's forced reached a turn");
        assert!(group_gone(server.control.identity().pgid));
    });
}

/// x.3.2 X0 item 6.3: a link read that fails never delays stopping groups.
/// It is reported; a requested turn with its own anchor keeps its facts,
/// and a linked turn gets none (Core's default for a failed report).
#[test]
fn shutdown_link_read_failure_still_stops_groups() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        fixture.spawn_session(other_session()).await;
        let host = fixture.host();
        fixture.run_turn_one(session()).await;
        let server = host
            .acquire(server_spec(&fixture), within(4))
            .await
            .unwrap();
        server
            .control
            .link_turn(&session(), turn(1), within(2))
            .await
            .unwrap();
        let mut spec = fixture.spec("/bin/cat", &[]);
        spec.owner = ProcessOwner::Turn {
            session_id: other_session(),
            turn: turn(1),
        };
        let private = host.acquire(spec, within(4)).await.unwrap();
        fixture.arm("store.read.corrupt.server_links", "fail_io");
        let report = host
            .shutdown(
                within(5),
                &[(session(), turn(1)), (other_session(), turn(1))],
            )
            .await;
        assert!(fixture.acked("store.read.corrupt.server_links"));
        assert!(
            matches!(report.failure, Some(HostError::LinksUnread)),
            "{report:?}"
        );
        assert!(group_gone(server.control.identity().pgid));
        assert!(group_gone(private.control.identity().pgid));
        assert_eq!(report.anchors, 2);
        let [recovery] = report.recovery.as_slice() else {
            panic!("{report:?}");
        };
        assert_eq!(recovery.owner_session, other_session());
        assert!(matches!(recovery.cleanup, CleanupEvidence::GroupAbsent(_)));
    });
}
