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
    LaunchPipes, PrivateProcessSpec, ProcessOwner, SessionId, TurnNumber, run_anchor_from_args,
};
use via_store::{AnchorPhase, SpawnRecord, Store, failpoint};

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
                prompt: "fixture".into(),
                effective: serde_json::json!({"deadlines":{"wall_ms":1}}),
                initial_event: serde_json::json!({"seq":1,"type":"turn.queued"}),
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
            owner: ProcessOwner {
                session_id: session(),
                turn: TurnNumber::try_from(1).unwrap(),
            },
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
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn session() -> SessionId {
    SessionId::try_from("s_0123456789ab").unwrap()
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
        let (force, signal) = tokio::sync::watch::channel(false);
        host.watch_force(signal);
        fixture.arm("host.early_stop.sent", "fail_io");
        let acquired = host
            .acquire(fixture.spec("/bin/cat", &[]), within(4))
            .await
            .unwrap();
        let pgid = acquired.control.identity().pgid;
        assert_eq!(host.pending_cleanup(), 1);
        force.send_replace(true);
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
/// exists, while the force signal is raised.
#[test]
fn a_control_registered_after_the_early_stop_snapshot_is_stopped_at_once() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(false);
        host.watch_force(signal);
        fixture.arm("store.journal.anchor_intent", "pause");
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
        force.send_replace(true);
        // On this current-thread runtime the woken early-stop task runs its
        // snapshot before this task is polled again.
        tokio::task::yield_now().await;
        fixture.release("store.journal.anchor_intent");
        let (failure, launched) = acquiring.await.unwrap();
        let failure = failure.unwrap();
        assert!(matches!(failure.error, HostError::Stopped), "{failure:?}");
        assert!(!launched);
        assert!(
            matches!(failure.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{failure:?}"
        );
        assert_eq!(fixture.records().await[0].phase, AnchorPhase::Intent);
    });
}

/// Design §6.8: an untriggered early-stop task never holds up shutdown.
#[test]
fn an_untriggered_early_stop_task_is_joined_by_shutdown() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (_force, signal) = tokio::sync::watch::channel(false);
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
        let (force, signal) = tokio::sync::watch::channel(false);
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
        force.send_replace(true);
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
/// refuses: `Stopped`, not launched, no `Stop` frame, absence proved.
#[test]
fn a_pre_arm_acquisition_is_refused_at_the_gate_without_a_stop_frame() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let (force, signal) = tokio::sync::watch::channel(false);
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
        force.send_replace(true);
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
        let (force, signal) = tokio::sync::watch::channel(false);
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
        force.send_replace(true);
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
