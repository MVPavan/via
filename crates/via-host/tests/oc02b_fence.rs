//! `OC02b`, the `OpenCode` fence (`docs/specs/vendors/opencode.md` §13), and
//! `OC01`'s version-check and stderr cases, through real Host anchors and a
//! real Store journal: die with the anchor, the exclusive launch lock, the
//! server record and its predecessor proof, the credential and program
//! checks (runtime §5). Host-level: no route runs, so a refusal is the
//! acquisition's `HostError::Fence`, which the route later maps to
//! `launch_failed` or `handshake_refused`.
//!
//! This binary has no libtest harness (`harness = false`): the anchor starts
//! the exec entry as `/proc/self/exe __via_host_exec …`, so the test binary
//! is the anchor, the exec entry and the fake vendor (`__oc02b_vendor`).
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "test fixtures and assertions fail loudly; the runner prints libtest's lines"
)]

#[path = "oc02b/harness.rs"]
mod harness;
#[path = "oc02b/support.rs"]
mod support;
#[path = "oc02b/vendor.rs"]
mod vendor;

use std::{
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, ExitCode},
    time::{Duration, Instant},
};

use harness::Case;
use support::{
    Fixture, alive, boot_id, gone_within, holders, kill, pid_namespace, pid_of, record, recorded,
    report, runtime, stat, within,
};
#[cfg(feature = "test-failpoints")]
use via_host::LaunchPipes;
use via_host::{
    AcquiredProcess, CleanupEvidence, CloseMode, CloseRequest, FenceRefusal, HostError,
    ProbeFailure, run_anchor_from_args, run_exec_from_args,
};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|arg| arg.to_str()) {
        Some("__via_host_anchor") => return exit_code(run_anchor_from_args(&args[1..])),
        Some("__via_host_exec") => return exit_code(run_exec_from_args(&args[1..])),
        Some("__oc02b_vendor") => return vendor::run(&args[1..]),
        _ => {}
    }
    harness::main(&args, CASES)
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

const NEEDS_ROOT: &str = "needs uid 0 (run as root)";
const NEEDS_SETFCAP: &str = "needs CAP_SETFCAP to give a file security.capability";
const NEEDS_CREDENTIAL_HARNESS: &str =
    "needs a harness that starts VIA with differing GIDs or non-zero capabilities";

const CASES: &[Case] = &[
    case(
        "oc02b_lock_is_private_and_held_by_the_anchor_alone",
        lock_is_private,
    ),
    case(
        "oc02b_held_lock_refuses_until_the_holder_exits",
        held_lock_refuses,
    ),
    case(
        "oc02b_vendor_dies_with_its_anchor",
        vendor_dies_with_its_anchor,
    ),
    case(
        "oc02b_vendor_outlives_blocking_pool_threads",
        vendor_outlives_idle_threads,
    ),
    case("oc02b_gone_predecessor_records_admit", gone_records_admit),
    case(
        "oc02b_record_from_another_namespace_is_uncertain",
        other_namespace,
    ),
    case(
        "oc02b_escaped_predecessor_refused_until_it_exits",
        escaped_predecessor,
    ),
    case(
        "oc02b_unreaped_predecessor_is_admitted",
        unreaped_predecessor,
    ),
    case("oc02b_record_naming_a_thread_id", thread_id_records),
    case(
        "oc02b_leader_exited_predecessor_is_present",
        leader_exited_predecessor,
    ),
    case(
        "oc02b_de_thread_interleaving_is_present_throughout",
        de_thread_interleaving,
    ),
    case(
        "oc02b_worker_thread_exec_survives_and_is_fenced",
        worker_thread_exec,
    ),
    case(
        "oc02b_privileged_program_files_are_refused",
        privileged_program_files,
    ),
    case(
        "oc02b_store_rows_never_gate_a_launch",
        store_rows_never_gate,
    ),
    case(
        "oc01_version_check_refusal_launches_nothing",
        version_refused,
    ),
    case(
        "oc01_version_check_admits_a_checked_version",
        version_admitted,
    ),
    case(
        "oc01_version_check_failures_are_transient",
        version_failures,
    ),
    case(
        "oc01_version_check_dies_with_its_anchor",
        version_dies_with_anchor,
    ),
    case("oc01_stderr_is_counted_only", stderr_counted_only),
    case(
        "oc01_stderr_count_is_reported_only_when_final",
        stderr_count_only_when_final,
    ),
    case("oc02b_malformed_record_is_uncertain", malformed_record),
    case(
        "oc02b_predecessor_exiting_within_the_wait_is_admitted_after_it",
        predecessor_exiting_within_the_wait,
    ),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_lingering_anchor_after_daemon_crash",
        lingering_anchor,
    ),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_parent_death_before_signal_setup",
        parent_death_before_signal,
    ),
    #[cfg(feature = "test-failpoints")]
    case("oc02b_child_waits_for_its_record", child_waits_for_record),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_record_naming_another_pid_never_releases",
        other_pid_record,
    ),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_record_naming_other_ticks_never_releases",
        other_ticks_record,
    ),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_failed_record_write_kills_the_child",
        failed_record_write,
    ),
    #[cfg(feature = "test-failpoints")]
    case("oc02b_handover_old_child_dies_with_its_anchor", handover),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_anchor_dies_after_its_record_write_and_its_child_with_it",
        anchor_dies_after_record,
    ),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_short_record_write_leaves_no_record",
        short_record_write,
    ),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_stalled_lock_open_still_serves_eof",
        stalled_lock_open,
    ),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_stalled_record_write_still_serves_eof",
        stalled_record_write,
    ),
    #[cfg(feature = "test-failpoints")]
    case(
        "oc02b_record_written_after_the_child_bound_never_releases",
        record_after_the_bound,
    ),
    ignored(
        "oc02b_file_capability_program_is_refused",
        file_capability_program,
        NEEDS_SETFCAP,
    ),
    ignored("oc02b_privileged_via_as_root", privileged_root, NEEDS_ROOT),
    ignored(
        "oc02b_privileged_via_with_differing_gids_or_capabilities",
        privileged_credentials,
        NEEDS_CREDENTIAL_HARNESS,
    ),
];

const fn case(name: &'static str, run: fn()) -> Case {
    Case {
        name,
        run,
        ignored: None,
    }
}

const fn ignored(name: &'static str, run: fn(), reason: &'static str) -> Case {
    Case {
        name,
        run,
        ignored: Some(reason),
    }
}

/// Every report path as a `&str` argument.
fn arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

async fn close(acquired: AcquiredProcess) {
    let report = acquired
        .control
        .close(CloseRequest {
            mode: CloseMode::Force,
            deadline: within(5),
        })
        .await;
    assert!(
        matches!(report.cleanup, CleanupEvidence::GroupAbsent(_)),
        "{:?}",
        report.cleanup
    );
}

fn fence(error: HostError) -> FenceRefusal {
    let message = format!("not a fence refusal: {error:?}");
    let HostError::Fence(refusal) = error else {
        panic!("{message}")
    };
    *refusal
}

/// The lock file is created 0600 and never unlinked; only the anchor
/// holds it: the vendor and its leftover child have no descriptor of it
/// and no `lock:` line, and the vendor has exactly its standard streams.
fn lock_is_private() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let (main, child) = (fixture.report("vendor"), fixture.report("child"));
        let acquired = host
            .acquire(
                fixture.spec(&["report-child", arg(&main), arg(&child)]),
                within(10),
            )
            .await
            .unwrap();
        let vendor = report(&main, Duration::from_secs(5))
            .await
            .expect("vendor ran");
        let leftover = report(&child, Duration::from_secs(5))
            .await
            .expect("child ran");
        let metadata = fs::symlink_metadata(fixture.lock()).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let anchor = acquired.control.identity().pid;
        assert_eq!(holders(&fixture.lock()), [(anchor, true)]);
        let lock = fixture.lock().to_string_lossy().into_owned();
        for process in [&vendor, &leftover] {
            for descriptor in process["descriptors"].as_array().unwrap() {
                assert_eq!(descriptor["locked"], false, "{process}");
                assert_ne!(descriptor["target"], lock.as_str(), "{process}");
            }
        }
        let fds: Vec<_> = vendor["descriptors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|descriptor| descriptor["fd"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(fds, ["0", "1", "2"], "{vendor}");
        assert_eq!(vendor["ppid"], anchor);
        assert_eq!(vendor["death_signal"], 9, "SIGKILL parent-death signal");
        let pid = pid_of(&vendor);
        assert_eq!(recorded(&fixture.lock()), Some((pid, stat(pid).unwrap().1)));
        close(acquired).await;
        assert!(fixture.lock().is_file(), "server.lock unlinked");
        assert!(holders(&fixture.lock()).is_empty());
        kill(pid_of(&leftover));
    });
}

/// A held lock refuses the configuration: no ARM intent, no vendor; once
/// the holder exits, a later acquisition is admitted.
fn held_lock_refuses() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let (first, second, third) = (
            fixture.report("first"),
            fixture.report("second"),
            fixture.report("third"),
        );
        let holder = host
            .acquire(fixture.spec(&["report", arg(&first)]), within(10))
            .await
            .unwrap();
        report(&first, Duration::from_secs(5))
            .await
            .expect("holder ran");
        let refused = host
            .acquire(fixture.spec(&["report", arg(&second)]), within(10))
            .await
            .err()
            .expect("refused");
        assert_eq!(refused.cause().unwrap().step, "take launch lock");
        assert_eq!(fence(refused), FenceRefusal::LockHeld);
        assert_eq!(
            fixture.arm_intents().await,
            1,
            "an ARM intent while refused"
        );
        assert!(report(&second, Duration::from_millis(300)).await.is_none());
        close(holder).await;
        let admitted = host
            .acquire(fixture.spec(&["report", arg(&third)]), within(10))
            .await
            .unwrap();
        report(&third, Duration::from_secs(5))
            .await
            .expect("admitted");
        close(admitted).await;
    });
}

/// `SIGKILL` of the anchor's pid alone (never its group), Host's
/// retirement held: the vendor is gone within 1 s; its leftover child in
/// its own group survives and holds no lock.
fn vendor_dies_with_its_anchor() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let (main, child) = (fixture.report("vendor"), fixture.report("child"));
        let acquired = host
            .acquire(
                fixture.spec(&["report-child", arg(&main), arg(&child)]),
                within(10),
            )
            .await
            .unwrap();
        let vendor = pid_of(&report(&main, Duration::from_secs(5)).await.unwrap());
        let leftover = pid_of(&report(&child, Duration::from_secs(5)).await.unwrap());
        let (vendor_ticks, leftover_ticks) = (stat(vendor).unwrap().1, stat(leftover).unwrap().1);
        kill(acquired.control.identity().pid);
        assert!(
            gone_within(vendor, vendor_ticks, Duration::from_secs(1)).await,
            "vendor outlived its anchor"
        );
        assert!(alive(leftover, leftover_ticks), "leftover child died");
        assert!(holders(&fixture.lock()).is_empty());
        kill(leftover);
        drop(acquired);
    });
}

/// The vendor is still alive more than 12 s after an idle anchor started
/// it: its creator is the anchor's main thread, which outlives Tokio's
/// blocking threads (they end after 10 s idle).
fn vendor_outlives_idle_threads() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let main = fixture.report("vendor");
        let acquired = host
            .acquire(fixture.spec(&["report", arg(&main)]), within(10))
            .await
            .unwrap();
        let vendor = pid_of(&report(&main, Duration::from_secs(5)).await.unwrap());
        let ticks = stat(vendor).unwrap().1;
        tokio::time::sleep(Duration::from_millis(12_500)).await;
        assert!(alive(vendor, ticks), "vendor died with an idle thread");
        close(acquired).await;
    });
}

/// A missing, empty or torn record, another boot ID, a gone pid and a pid
/// with other start ticks each prove the predecessor gone: admitted.
fn gone_records_admit() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let (boot, namespace) = (boot_id(), pid_namespace());
        let exited = {
            let mut child = Command::new("/bin/true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        };
        let own = std::process::id();
        let own_ticks = stat(own).unwrap().1;
        let mut torn = record(&boot, &namespace, own, own_ticks);
        torn[100] ^= 1;
        let contents: [(&str, Option<Vec<u8>>); 6] = [
            ("missing", None),
            ("empty", Some(Vec::new())),
            ("torn", Some(torn)),
            (
                "another boot",
                Some(record(
                    "00000000-0000-0000-0000-000000000000",
                    &namespace,
                    own,
                    own_ticks,
                )),
            ),
            ("gone pid", Some(record(&boot, &namespace, exited, 1))),
            (
                "other start ticks",
                Some(record(&boot, &namespace, own, own_ticks + 1)),
            ),
        ];
        for (index, (name, content)) in contents.into_iter().enumerate() {
            match content {
                None => {
                    let _ = fs::remove_file(fixture.lock());
                }
                Some(bytes) => fs::write(fixture.lock(), bytes).unwrap(),
            }
            let path = fixture.report(&format!("vendor-{index}"));
            let acquired = host
                .acquire(fixture.spec(&["report", arg(&path)]), within(10))
                .await
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            report(&path, Duration::from_secs(5))
                .await
                .unwrap_or_else(|| panic!("{name}: vendor never ran"));
            close(acquired).await;
        }
    });
}

/// A record from this boot but another PID namespace is uncertain:
/// `PredecessorUncertain` naming that namespace, nothing launched.
fn other_namespace() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        fs::write(
            fixture.lock(),
            record(&boot_id(), "pid:[1]", std::process::id(), 1),
        )
        .unwrap();
        let refused = host
            .acquire(
                fixture.spec(&["report", arg(&fixture.report("vendor"))]),
                within(10),
            )
            .await
            .err()
            .expect("refused");
        assert_eq!(refused.cause().unwrap().step, "predecessor check");
        assert_eq!(
            fence(refused),
            FenceRefusal::PredecessorUncertain {
                namespace: "pid:[1]".into()
            }
        );
        assert_eq!(fixture.arm_intents().await, 0);
    });
}

/// A record whose checksum holds but whose fields are malformed (bytes
/// after the namespace's terminating NUL), or a valid record followed by
/// more bytes, is neither missing nor torn: `PredecessorUncertain`,
/// nothing launched, even though the pid and start ticks it names (this
/// test process) are live.
fn malformed_record() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let (boot, namespace) = (boot_id(), pid_namespace());
        let own = std::process::id();
        let ticks = stat(own).unwrap().1;
        let mut padded = record(&boot, &namespace, own, ticks);
        padded[8 + 64 + namespace.len() + 1] = b'x';
        support::reseal(&mut padded);
        let mut longer = record(&boot, &namespace, own, ticks);
        longer.push(0);
        for (name, bytes) in [("padding", padded), ("longer", longer)] {
            fs::write(fixture.lock(), bytes).unwrap();
            let refused = host
                .acquire(
                    fixture.spec(&["report", arg(&fixture.report(name))]),
                    within(10),
                )
                .await
                .err()
                .unwrap_or_else(|| panic!("{name}: admitted"));
            assert_eq!(refused.cause().unwrap().step, "predecessor check");
            let refusal = fence(refused);
            assert!(
                matches!(refusal, FenceRefusal::PredecessorUncertain { .. }),
                "{name}: {refusal:?}"
            );
        }
        assert_eq!(fixture.arm_intents().await, 0);
    });
}

/// A recorded predecessor that exits within the 1 s wait is admitted, and
/// only after it exited: the configuration waited on its pidfd (a
/// test-started process stands in for the predecessor, killed 400 ms in).
fn predecessor_exiting_within_the_wait() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let mut sleeper = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = sleeper.id();
        let ticks = stat(pid).unwrap().1;
        fs::write(
            fixture.lock(),
            record(&boot_id(), &pid_namespace(), pid, ticks),
        )
        .unwrap();
        let main = fixture.report("vendor");
        let started = Instant::now();
        let ((admitted, admitted_at), killed_at) = tokio::join!(
            async {
                let admitted = host
                    .acquire(fixture.spec(&["report", arg(&main)]), within(10))
                    .await;
                (admitted, started.elapsed())
            },
            async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                assert!(alive(pid, ticks), "the predecessor ended early");
                let killed_at = started.elapsed();
                sleeper.kill().unwrap();
                sleeper.wait().unwrap();
                killed_at
            }
        );
        let admitted = admitted.unwrap();
        assert!(admitted_at >= killed_at, "admitted before the exit");
        report(&main, Duration::from_secs(5)).await.expect("ran");
        close(admitted).await;
    });
}

/// A vendor that clears its parent-death signal and leaves its group
/// survives its anchor; the next configuration re-probes for 1 s, then
/// refuses `PredecessorAlive` naming it; it admits once the vendor exits.
fn escaped_predecessor() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let main = fixture.report("escaped");
        let acquired = host
            .acquire(fixture.spec(&["escape", arg(&main)]), within(10))
            .await
            .unwrap();
        let vendor = pid_of(&report(&main, Duration::from_secs(5)).await.unwrap());
        let ticks = stat(vendor).unwrap().1;
        kill(acquired.control.identity().pid);
        drop(acquired);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(alive(vendor, ticks), "escaped vendor died");
        let started = Instant::now();
        let refused = host
            .acquire(
                fixture.spec(&["report", arg(&fixture.report("second"))]),
                within(10),
            )
            .await
            .err()
            .expect("refused");
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "no 1 s re-probe"
        );
        assert_eq!(
            fence(refused),
            FenceRefusal::PredecessorAlive {
                pid: vendor,
                start_ticks: ticks
            }
        );
        assert_eq!(fixture.arm_intents().await, 1);
        kill(vendor);
        assert!(gone_within(vendor, ticks, Duration::from_secs(2)).await);
        let third = fixture.report("third");
        let admitted = host
            .acquire(fixture.spec(&["report", arg(&third)]), within(10))
            .await
            .unwrap();
        report(&third, Duration::from_secs(5)).await.unwrap();
        close(admitted).await;
    });
}

/// The anchor and its vendor killed under a test subreaper that does not
/// reap: the vendor is a zombie, which the pidfd shows exited: admitted.
/// The vendor leaves the anchor's group (keeping its parent-death signal)
/// so that Host's reaper of that group, in this same process, never reaps
/// it.
fn unreaped_predecessor() {
    runtime().block_on(async {
        rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
        let fixture = Fixture::new();
        let host = fixture.host();
        let main = fixture.report("vendor");
        let acquired = host
            .acquire(fixture.spec(&["own-group", arg(&main)]), within(10))
            .await
            .unwrap();
        let vendor_report = report(&main, Duration::from_secs(5)).await.unwrap();
        assert_eq!(vendor_report["death_signal"], 9);
        let vendor = pid_of(&vendor_report);
        kill(acquired.control.identity().pid);
        drop(acquired);
        let zombie = support::wait_for(Duration::from_secs(2), || async {
            stat(vendor).and_then(|(state, _, _)| (state == 'Z').then_some(()))
        })
        .await;
        assert!(zombie.is_some(), "vendor is no unreaped zombie");
        let second = fixture.report("second");
        let admitted = host
            .acquire(fixture.spec(&["report", arg(&second)]), within(10))
            .await
            .unwrap();
        report(&second, Duration::from_secs(5)).await.unwrap();
        close(admitted).await;
        let pid = rustix::process::Pid::from_raw(i32::try_from(vendor).unwrap());
        let _ = rustix::process::waitpid(pid, rustix::process::WaitOptions::empty());
    });
}

/// A record naming a live non-leader thread ID: with other start ticks
/// (a pid reused as a thread) the identity check admits before any open;
/// with the thread's own ticks `pidfd_open` fails (`EINVAL`), which is
/// present: refused.
fn thread_id_records() {
    runtime().block_on(async {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let _ = stopped.recv();
        });
        let own = std::process::id();
        let tid = fs::read_dir("/proc/self/task")
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .find(|tid| *tid != own)
            .expect("a worker thread");
        let ticks = stat(tid).unwrap().1;
        let fixture = Fixture::new();
        let host = fixture.host();
        let (boot, namespace) = (boot_id(), pid_namespace());
        fs::write(fixture.lock(), record(&boot, &namespace, tid, ticks + 1)).unwrap();
        let first = fixture.report("first");
        let admitted = host
            .acquire(fixture.spec(&["report", arg(&first)]), within(10))
            .await
            .unwrap();
        report(&first, Duration::from_secs(5)).await.unwrap();
        close(admitted).await;
        fs::write(fixture.lock(), record(&boot, &namespace, tid, ticks)).unwrap();
        let refused = host
            .acquire(
                fixture.spec(&["report", arg(&fixture.report("second"))]),
                within(10),
            )
            .await
            .err()
            .expect("refused");
        assert_eq!(
            fence(refused),
            FenceRefusal::PredecessorAlive {
                pid: tid,
                start_ticks: ticks
            }
        );
        drop(stop);
        worker.join().unwrap();
    });
}

/// A Python helper whose leader thread exits (`pthread_exit`) while a
/// worker runs; the worker execs the script again from its non-leader
/// thread after `release-<stage>` exists, replacing the leader through
/// `de_thread`. Stage 0's worker only waits. Each stage writes
/// `ready-<stage>` before its leader exits.
const THREADS_PY: &str = r#"
import ctypes, os, sys, threading, time
stage, folder = int(sys.argv[1]), sys.argv[2]
def worker():
    release = os.path.join(folder, "release-%d" % stage)
    while not os.path.exists(release):
        time.sleep(0.005)
    if stage > 0:
        os.execv(sys.executable, [sys.executable, sys.argv[0], str(stage - 1), folder])
    while True:
        time.sleep(1)
threading.Thread(target=worker).start()
with open(os.path.join(folder, "ready-%d" % stage), "w") as ready:
    ready.write("%d" % os.getpid())
ctypes.CDLL(None).pthread_exit(None)
"#;

fn python_threads(fixture: &Fixture, stages: u32) -> std::process::Child {
    let script = fixture.root.join("threads.py");
    fs::write(&script, THREADS_PY).unwrap();
    Command::new("python3")
        .arg(&script)
        .arg(stages.to_string())
        .arg(fixture.root.join("reports"))
        .spawn()
        .expect("python3 for the leader-exit helper")
}

async fn ready(fixture: &Fixture, stage: u32) {
    let path = fixture.report(&format!("ready-{stage}"));
    support::wait_for(Duration::from_secs(10), || async {
        path.exists().then_some(())
    })
    .await
    .unwrap_or_else(|| panic!("stage {stage} never ready"));
    // The leader's `pthread_exit` follows the ready file.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Runs the predecessor check against `pid` with `ticks` through a fresh
/// acquisition: `Some(refusal)` when refused.
async fn check(fixture: &Fixture, pid: u32, ticks: u64, name: &str) -> Option<FenceRefusal> {
    fs::write(
        fixture.lock(),
        record(&boot_id(), &pid_namespace(), pid, ticks),
    )
    .unwrap();
    let path = fixture.report(name);
    match fixture
        .host()
        .acquire(fixture.spec(&["report", arg(&path)]), within(10))
        .await
    {
        Ok(acquired) => {
            report(&path, Duration::from_secs(5)).await.unwrap();
            close(acquired).await;
            None
        }
        Err(error) => Some(fence(error)),
    }
}

/// A predecessor whose leader thread exited while another thread runs is
/// present (its thread group is not empty): refused, then admitted once
/// the process exits.
fn leader_exited_predecessor() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let mut helper = python_threads(&fixture, 0);
        ready(&fixture, 0).await;
        let pid = helper.id();
        let (state, ticks, _) = stat(pid).unwrap();
        assert_eq!(state, 'Z', "leader thread still running");
        assert_eq!(
            check(&fixture, pid, ticks, "while-running").await,
            Some(FenceRefusal::PredecessorAlive {
                pid,
                start_ticks: ticks
            })
        );
        helper.kill().unwrap();
        helper.wait().unwrap();
        assert_eq!(check(&fixture, pid, ticks, "after-exit").await, None);
    });
}

/// The review's interleaving: a leader-exited process whose worker threads
/// exec again one after another, each replacing the leader through
/// `de_thread`, is present before, between and after each release, and
/// admitted only after it exits.
fn de_thread_interleaving() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let mut helper = python_threads(&fixture, 2);
        let pid = helper.id();
        ready(&fixture, 2).await;
        let ticks = stat(pid).unwrap().1;
        let present = Some(FenceRefusal::PredecessorAlive {
            pid,
            start_ticks: ticks,
        });
        assert_eq!(check(&fixture, pid, ticks, "before").await, present);
        for stage in [2_u32, 1] {
            fs::write(fixture.report(&format!("release-{stage}")), b"").unwrap();
            assert_eq!(
                check(&fixture, pid, ticks, &format!("during-{stage}")).await,
                present
            );
            ready(&fixture, stage - 1).await;
            assert_eq!(stat(pid).unwrap().1, ticks, "exec changed the start ticks");
            assert_eq!(
                check(&fixture, pid, ticks, &format!("after-{stage}")).await,
                present
            );
        }
        helper.kill().unwrap();
        helper.wait().unwrap();
        assert_eq!(check(&fixture, pid, ticks, "exited").await, None);
    });
}

/// A vendor that re-executes itself from a worker thread has no
/// parent-death signal: it survives its anchor, and the next
/// configuration refuses `PredecessorAlive` naming it; no second server
/// starts until the test stops it.
fn worker_thread_exec() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let main = fixture.report("vendor");
        let acquired = host
            .acquire(fixture.spec(&["thread-exec", arg(&main)]), within(10))
            .await
            .unwrap();
        let vendor_report = report(&main, Duration::from_secs(5)).await.unwrap();
        assert_eq!(vendor_report["death_signal"], serde_json::Value::Null);
        let vendor = pid_of(&vendor_report);
        let ticks = stat(vendor).unwrap().1;
        kill(acquired.control.identity().pid);
        drop(acquired);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(alive(vendor, ticks), "the re-executed vendor died");
        let second = fixture.report("second");
        let refused = host
            .acquire(fixture.spec(&["report", arg(&second)]), within(10))
            .await
            .err()
            .expect("refused");
        assert_eq!(
            fence(refused),
            FenceRefusal::PredecessorAlive {
                pid: vendor,
                start_ticks: ticks
            }
        );
        assert!(report(&second, Duration::from_millis(200)).await.is_none());
        kill(vendor);
        assert!(gone_within(vendor, ticks, Duration::from_secs(2)).await);
        let third = fixture.report("third");
        let admitted = host
            .acquire(fixture.spec(&["report", arg(&third)]), within(10))
            .await
            .unwrap();
        report(&third, Duration::from_secs(5)).await.unwrap();
        close(admitted).await;
    });
}

/// A set-user-ID or set-group-ID program is refused at configuration,
/// before the version check and the lock; nothing launched.
fn privileged_program_files() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        for (name, mode) in [("setuid", 0o4755), ("setgid", 0o2755)] {
            let program = fixture.root.join(name);
            fs::copy("/bin/true", &program).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                fs::metadata(&program).unwrap().permissions().mode() & 0o7777,
                mode,
                "{name} bit not set"
            );
            let mut spec = fixture.probed_spec(&["report", "unused"], &["version", "x"]);
            spec.program = program;
            let refused = host.acquire(spec, within(10)).await.err().expect(name);
            assert_eq!(
                refused.cause().unwrap().step,
                "check vendor program privileges"
            );
            assert_eq!(fence(refused), FenceRefusal::ProgramPrivileged, "{name}");
        }
        assert_eq!(fixture.arm_intents().await, 0);
        assert!(!fixture.lock().exists(), "the lock was taken");
    });
}

/// Store rows never gate a launch through the lock: an identity-less
/// pre-ARM server intent and a server anchor of another boot admit an
/// `OpenCode` server at once when no lock holder lives.
fn store_rows_never_gate() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let journal = fixture.store.runtime_resources().into_wire_parts().1;
        for (index, boot) in [boot_id(), "00000000-0000-0000-0000-000000000001".into()]
            .into_iter()
            .enumerate()
        {
            let intent = via_store::AnchorIntent {
                anchor_id: format!("{index:032x}"),
                generation: format!("{:032x}", index + 100),
                marker: format!("{:032x}", index + 200),
                socket_path: fixture.root.join("anchors").join(format!("{index}.sock")),
                owner: via_host::ProcessOwner::Server {
                    server_id: via_host::ServerId::try_from(
                        format!("v_ffffffffff{index:02x}").as_str(),
                    )
                    .unwrap(),
                },
                uid: rustix::process::getuid().as_raw(),
                boot_id: boot.clone(),
                pid_namespace: pid_namespace(),
            };
            let via_store::CommitOutcome::Committed(receipt) =
                journal.commit_anchor_intent(intent).await
            else {
                panic!("intent not committed")
            };
            if index == 1 {
                let identity = via_store::AnchorIdentity {
                    pid: 4_000_000,
                    pgid: 4_000_000,
                    uid: rustix::process::getuid().as_raw(),
                    boot_id: boot,
                    pid_namespace: pid_namespace(),
                    start_ticks: 1,
                    marker: format!("{:032x}", index + 200),
                };
                let outcome = journal
                    .commit_anchor_identified(
                        &format!("{index:032x}"),
                        &format!("{:032x}", index + 100),
                        receipt.record_version,
                        identity,
                    )
                    .await;
                assert!(matches!(outcome, via_store::CommitOutcome::Committed(_)));
            }
        }
        let host = fixture.host();
        let main = fixture.report("vendor");
        let admitted = host
            .acquire(fixture.spec(&["report", arg(&main)]), within(10))
            .await
            .unwrap();
        report(&main, Duration::from_secs(5)).await.unwrap();
        close(admitted).await;
    });
}

/// `OC01`: `--version` printing an unchecked version is `ProbeRefused` at
/// configuration: no ARM intent, the lock never taken and the namespace
/// directory unchanged; no launch-failure cause (a refusal).
fn version_refused() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let main = fixture.report("vendor");
        let refused = host
            .acquire(
                fixture.probed_spec(&["report", arg(&main)], &["version", "opencode v2.0.23"]),
                within(10),
            )
            .await
            .err()
            .expect("refused");
        assert_eq!(refused.cause(), None);
        assert_eq!(
            fence(refused),
            FenceRefusal::ProbeRefused {
                output: "opencode v2.0.23".into()
            }
        );
        assert_eq!(fixture.arm_intents().await, 0);
        assert_eq!(fs::read_dir(fixture.root.join("ns")).unwrap().count(), 0);
        assert!(report(&main, Duration::from_millis(200)).await.is_none());
    });
}

/// `OC01`: a checked version is admitted.
fn version_admitted() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let main = fixture.report("vendor");
        let admitted = host
            .acquire(
                fixture.probed_spec(&["report", arg(&main)], &["version", "opencode v2.0.22"]),
                within(10),
            )
            .await
            .unwrap();
        report(&main, Duration::from_secs(5)).await.unwrap();
        close(admitted).await;
    });
}

/// `OC01`: a check that sleeps past 2 s, exits non-zero or overflows 256
/// bytes is `ProbeFailed`, a transient launch failure that takes no lock;
/// an immediate retry with a well-behaved check admits.
fn version_failures() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let hang = fixture.report("hang-pid");
        let cases: [(&[&str], ProbeFailure); 4] = [
            (&["version-hang", arg(&hang)], ProbeFailure::Timeout),
            (
                &["version-exit", "3"],
                ProbeFailure::Exit {
                    code: Some(3),
                    signal: None,
                },
            ),
            (&["version-flood"], ProbeFailure::Overflow),
            // Over the cap and then hanging: found while reading, not at
            // the 2 s bound.
            (&["version-flood-hang"], ProbeFailure::Overflow),
        ];
        for (version_args, expected) in cases {
            let started = Instant::now();
            let refused = host
                .acquire(
                    fixture.probed_spec(&["report", "unused"], version_args),
                    within(10),
                )
                .await
                .err()
                .expect("refused");
            assert_eq!(refused.cause().unwrap().step, "version check");
            assert_eq!(fence(refused), FenceRefusal::ProbeFailed { kind: expected });
            let elapsed = started.elapsed();
            if expected == ProbeFailure::Overflow {
                assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
            }
            if expected == ProbeFailure::Timeout {
                assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
                let probe: u32 = fs::read_to_string(&hang).unwrap().parse().unwrap();
                assert!(stat(probe).is_none_or(|(state, _, _)| state == 'Z'));
            }
            assert_eq!(fs::read_dir(fixture.root.join("ns")).unwrap().count(), 0);
        }
        assert_eq!(fixture.arm_intents().await, 0);
        let main = fixture.report("vendor");
        let admitted = host
            .acquire(
                fixture.probed_spec(&["report", arg(&main)], &["version", "opencode v2.0.22"]),
                within(10),
            )
            .await
            .unwrap();
        report(&main, Duration::from_secs(5)).await.unwrap();
        close(admitted).await;
    });
}

/// `OC01`: the anchor killed alone while `--version` hangs: the probe is
/// gone within 1 s (it dies with its anchor).
fn version_dies_with_anchor() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let hang = fixture.report("hang-pid");
        let spec = fixture.probed_spec(&["report", "unused"], &["version-hang", arg(&hang)]);
        let (result, ()) = tokio::join!(host.acquire(spec, within(10)), async {
            let anchor = fixture.configuring_anchor().await;
            let probe: u32 = support::wait_for(Duration::from_secs(5), || async {
                fs::read_to_string(&hang).ok()?.parse().ok()
            })
            .await
            .expect("probe started");
            let ticks = stat(probe).unwrap().1;
            kill(anchor);
            assert!(
                gone_within(probe, ticks, Duration::from_secs(1)).await,
                "probe outlived its anchor"
            );
        });
        assert!(result.is_err());
    });
}

/// `OC01/OC12b`: with `CountOnly` no `stderr.log` exists, the secret the
/// vendor wrote to stderr is in no file, and its byte count comes with the
/// exit facts.
fn stderr_counted_only() {
    runtime().block_on(async {
        const SECRET: &str = "synthetic-secret-0c02b-stderr";
        let fixture = Fixture::new();
        let host = fixture.host();
        let spec = fixture.spec(&["stderr-secret", SECRET]);
        let stderr_path = spec.stderr_path.clone();
        let mut acquired = host.acquire(spec, within(10)).await.unwrap();
        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            acquired.exits.wait_for(Option::is_some),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(exit.code, Some(0));
        assert_eq!(
            acquired.control.stderr_bytes(),
            Some(SECRET.len() as u64 + 1)
        );
        assert!(!stderr_path.exists(), "a stderr.log under CountOnly");
        close(acquired).await;
        for file in support::files(&fixture.root) {
            let bytes = fs::read(&file).unwrap_or_default();
            assert!(
                !bytes
                    .windows(SECRET.len())
                    .any(|window| window == SECRET.as_bytes()),
                "secret in {}",
                file.display()
            );
        }
    });
}

/// The count is final only once the drain reached EOF: a vendor that
/// exits while a child that inherited its stderr writes later gives no
/// count until that child's bytes are in, never the count at its exit.
fn stderr_count_only_when_final() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let host = fixture.host();
        let mut acquired = host
            .acquire(fixture.spec(&["stderr-late"]), within(10))
            .await
            .unwrap();
        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            acquired.exits.wait_for(Option::is_some),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(exit.code, Some(0));
        let total = Some(("early".len() + "0123456789".len()) as u64);
        let settled = support::wait_for(Duration::from_secs(5), || async {
            let count = acquired.control.stderr_bytes();
            assert!(count.is_none() || count == total, "a non-final {count:?}");
            count
        })
        .await;
        assert_eq!(settled, total);
        close(acquired).await;
    });
}

/// A short record write over a valid earlier record (a failpoint writes
/// all but the last byte and fails): the anchor invalidated the old record
/// first, so no byte of it survives and no valid record remains; the
/// child is killed without executing the vendor and the launch fails.
#[cfg(feature = "test-failpoints")]
fn short_record_write() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let exited = {
            let mut child = Command::new("/bin/true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        };
        fs::write(
            fixture.lock(),
            record(&boot_id(), &pid_namespace(), exited, 1),
        )
        .unwrap();
        fixture.arm("host.anchor.short_record_write", "fail_io");
        let host = fixture.host();
        let main = fixture.report("vendor");
        let failed = host
            .acquire(fixture.spec(&["report", arg(&main)]), within(10))
            .await
            .err()
            .expect("failed");
        assert_eq!(fence(failed), FenceRefusal::FenceRecordFailed);
        let anchor = fixture.acked("host.anchor.short_record_write").await;
        let left = fs::read(fixture.lock()).unwrap();
        assert!(
            left.len() < support::RECORD_LEN,
            "{} bytes: a byte of the old record survived",
            left.len()
        );
        assert_eq!(recorded(&fixture.lock()), None);
        assert!(support::children(anchor).is_empty());
        assert!(report(&main, Duration::from_millis(500)).await.is_none());
    });
}

/// A failed acquisition whose anchor is stalled at `point` (a paused
/// failpoint): Host's deadline drops the control, and the anchor still
/// serves that EOF, stops its group and exits, so Host proves the group
/// absent; no vendor runs.
#[cfg(feature = "test-failpoints")]
fn stalled_fence_io_serves_eof(point: &'static str) {
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm(point, "pause");
        let host = fixture.host();
        let main = fixture.report("vendor");
        let failed = host
            .acquire_retaining(
                fixture.spec(&["report", arg(&main)]),
                within(3),
                &LaunchPipes::default(),
                &|| false,
            )
            .await
            .err()
            .expect("failed");
        assert!(
            matches!(failed.error, HostError::Deadline),
            "{:?}",
            failed.error
        );
        assert!(
            matches!(failed.cleanup, Some(CleanupEvidence::GroupAbsent(_))),
            "{:?}",
            failed.cleanup
        );
        let anchor = fixture.acked(point).await;
        assert!(stat(anchor).is_none(), "the anchor lives on");
        assert!(report(&main, Duration::from_millis(300)).await.is_none());
    });
}

/// The lock's open stalled (configuration): EOF is still served.
#[cfg(feature = "test-failpoints")]
fn stalled_lock_open() {
    stalled_fence_io_serves_eof("host.anchor.before_lock");
}

/// The record write stalled (ARM, the child waiting for its record): EOF
/// is still served.
#[cfg(feature = "test-failpoints")]
fn stalled_record_write() {
    stalled_fence_io_serves_eof("host.anchor.before_record_write");
}

/// A child suspended past its 5 s bound while it polls, its record then
/// written, then resumed: it exits 125 without executing the vendor.
#[cfg(feature = "test-failpoints")]
fn record_after_the_bound() {
    use rustix::process::Signal;
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm("host.anchor.before_record_write", "pause");
        let host = fixture.host();
        let main = fixture.report("vendor");
        let (acquired, ()) = tokio::join!(
            host.acquire(fixture.spec(&["report", arg(&main)]), within(20)),
            async {
                let anchor = fixture.acked("host.anchor.before_record_write").await;
                let child = exec_child(anchor).await;
                let ticks = stat(child).unwrap().1;
                support::signal(child, Signal::STOP);
                tokio::time::sleep(Duration::from_millis(5300)).await;
                fixture.release("host.anchor.before_record_write");
                support::wait_for(Duration::from_secs(5), || async {
                    (recorded(&fixture.lock()) == Some((child, ticks))).then_some(())
                })
                .await
                .expect("record written");
                support::signal(child, Signal::CONT);
                assert!(gone_within(child, ticks, Duration::from_secs(2)).await);
            }
        );
        let mut acquired = acquired.unwrap();
        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            acquired.exits.wait_for(Option::is_some),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(exit.code, Some(125));
        assert!(report(&main, Duration::from_millis(300)).await.is_none());
        close(acquired).await;
    });
}

/// After a daemon crash after ARM (control EOF) the anchor lingers before
/// its cleanup; killed alone, its vendor is gone within 1 s.
#[cfg(feature = "test-failpoints")]
fn lingering_anchor() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm("host.anchor.before_eof_cleanup", "pause");
        let host = fixture.host();
        let main = fixture.report("vendor");
        let acquired = host
            .acquire(fixture.spec(&["report", arg(&main)]), within(10))
            .await
            .unwrap();
        let vendor = pid_of(&report(&main, Duration::from_secs(5)).await.unwrap());
        let ticks = stat(vendor).unwrap().1;
        let anchor = acquired.control.identity().pid;
        drop(acquired);
        drop(host);
        assert_eq!(
            fixture.acked("host.anchor.before_eof_cleanup").await,
            anchor
        );
        assert!(alive(vendor, ticks));
        kill(anchor);
        assert!(gone_within(vendor, ticks, Duration::from_secs(1)).await);
    });
}

/// The child held before its signal setup, its anchor killed, then
/// released: its parent check fails and it exits without executing the
/// vendor.
#[cfg(feature = "test-failpoints")]
fn parent_death_before_signal() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm("host.exec.before_death_signal", "pause");
        let host = fixture.host();
        let main = fixture.report("vendor");
        let acquired = host
            .acquire(fixture.spec(&["report", arg(&main)]), within(10))
            .await
            .unwrap();
        let child = fixture.acked("host.exec.before_death_signal").await;
        let ticks = stat(child).unwrap().1;
        let anchor = acquired.control.identity().pid;
        kill(anchor);
        drop(acquired);
        support::wait_for(Duration::from_secs(2), || async {
            (stat(child).is_some_and(|(_, _, parent)| parent != anchor)).then_some(())
        })
        .await
        .expect("child reparented");
        fixture.release("host.exec.before_death_signal");
        assert!(gone_within(child, ticks, Duration::from_secs(2)).await);
        assert!(report(&main, Duration::from_millis(300)).await.is_none());
    });
}

/// The anchor held before its record write: the child polls and does not
/// execute the vendor; once the record is written it executes.
#[cfg(feature = "test-failpoints")]
fn child_waits_for_record() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm("host.anchor.before_record_write", "pause");
        let host = fixture.host();
        let main = fixture.report("vendor");
        let (acquired, child) = tokio::join!(
            host.acquire(fixture.spec(&["report", arg(&main)]), within(15)),
            async {
                let anchor = fixture.acked("host.anchor.before_record_write").await;
                let child = exec_child(anchor).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert!(report(&main, Duration::ZERO).await.is_none(), "ran early");
                assert!(is_exec_entry(child), "the child executed early");
                fixture.release("host.anchor.before_record_write");
                child
            }
        );
        let acquired = acquired.unwrap();
        let vendor = report(&main, Duration::from_secs(5)).await.expect("ran");
        assert_eq!(pid_of(&vendor), child);
        assert_eq!(
            recorded(&fixture.lock()),
            Some((child, stat(child).unwrap().1))
        );
        close(acquired).await;
    });
}

/// The exec entry child of `anchor`, once it exists.
#[cfg(feature = "test-failpoints")]
async fn exec_child(anchor: u32) -> u32 {
    support::wait_for(Duration::from_secs(5), || async {
        support::children(anchor)
            .into_iter()
            .find(|pid| is_exec_entry(*pid))
    })
    .await
    .expect("exec entry child")
}

#[cfg(feature = "test-failpoints")]
fn is_exec_entry(pid: u32) -> bool {
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|line| {
        line.split(|byte| *byte == 0)
            .any(|part| part == b"__via_host_exec")
    })
}

/// A record naming another process never releases the child: it exits at
/// its 5 s bound without executing the vendor.
#[cfg(feature = "test-failpoints")]
fn other_pid_record() {
    forged_record_never_releases(|_child, _ticks| {
        let own = std::process::id();
        (own, stat(own).unwrap().1)
    });
}

/// A record naming the child's pid with other start ticks never releases
/// it either.
#[cfg(feature = "test-failpoints")]
fn other_ticks_record() {
    forged_record_never_releases(|child, ticks| (child, ticks + 1));
}

#[cfg(feature = "test-failpoints")]
fn forged_record_never_releases(named: fn(u32, u64) -> (u32, u64)) {
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm("host.anchor.before_record_write", "pause");
        let host = fixture.host();
        let main = fixture.report("vendor");
        let (acquired, ()) = tokio::join!(
            host.acquire(fixture.spec(&["report", arg(&main)]), within(20)),
            async {
                let anchor = fixture.acked("host.anchor.before_record_write").await;
                let child = exec_child(anchor).await;
                let ticks = stat(child).unwrap().1;
                let started = Instant::now();
                let (pid, start) = named(child, ticks);
                fs::write(
                    fixture.lock(),
                    record(&boot_id(), &pid_namespace(), pid, start),
                )
                .unwrap();
                assert!(gone_within(child, ticks, Duration::from_secs(7)).await);
                assert!(started.elapsed() >= Duration::from_secs(3), "no 5 s bound");
                fixture.release("host.anchor.before_record_write");
            }
        );
        assert!(report(&main, Duration::from_millis(300)).await.is_none());
        let mut acquired = acquired.unwrap();
        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            acquired.exits.wait_for(Option::is_some),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(exit.code, Some(125));
        close(acquired).await;
    });
}

/// A failed record write: the anchor kills and reaps its child, which
/// never executed the vendor, and the launch fails before any handshake.
#[cfg(feature = "test-failpoints")]
fn failed_record_write() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm("host.anchor.before_record_write", "fail_io");
        let host = fixture.host();
        let main = fixture.report("vendor");
        let failed = host
            .acquire(fixture.spec(&["report", arg(&main)]), within(10))
            .await
            .err()
            .expect("failed");
        assert_eq!(failed.cause().unwrap().step, "write server record");
        assert_eq!(fence(failed), FenceRefusal::FenceRecordFailed);
        let anchor = fixture.acked("host.anchor.before_record_write").await;
        assert!(support::children(anchor).is_empty());
        assert!(report(&main, Duration::from_millis(500)).await.is_none());
    });
}

/// Handover: anchor A's child is paused after its parent check, A is
/// killed alone before writing its record. The child's parent-death signal
/// is already armed, so it dies with A: the record poll's changed-parent
/// exit cannot be reached from outside while that signal holds (the exec
/// entry's unit test covers it). B is configured and launched as soon as
/// A's lock is free; A's child never executes the vendor, only B's server
/// runs and B's record is intact.
#[cfg(feature = "test-failpoints")]
fn handover() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm("host.exec.after_parent_check", "pause");
        fixture.arm("host.anchor.before_record_write", "pause");
        let host = fixture.host();
        let (first, second) = (fixture.report("a"), fixture.report("b"));
        let (failed, (old_child, old_ticks)) = tokio::join!(
            host.acquire(fixture.spec(&["report", arg(&first)]), within(10)),
            async {
                let anchor = fixture.acked("host.anchor.before_record_write").await;
                let child = fixture.acked("host.exec.after_parent_check").await;
                let ticks = stat(child).unwrap().1;
                fixture.disarm("host.exec.after_parent_check");
                fixture.disarm("host.anchor.before_record_write");
                kill(anchor);
                (child, ticks)
            }
        );
        assert!(failed.is_err());
        let admitted = host
            .acquire(fixture.spec(&["report", arg(&second)]), within(10))
            .await
            .unwrap();
        fixture.release("host.exec.after_parent_check");
        let b = pid_of(
            &report(&second, Duration::from_secs(5))
                .await
                .expect("B ran"),
        );
        assert!(gone_within(old_child, old_ticks, Duration::from_secs(2)).await);
        assert!(report(&first, Duration::from_millis(300)).await.is_none());
        assert_eq!(recorded(&fixture.lock()), Some((b, stat(b).unwrap().1)));
        close(admitted).await;
    });
}

/// The anchor dies after its record write: the record names its child,
/// which dies with it; B is admitted and A's child is gone by then. This
/// does not force B's check into the moment A's child is still pending;
/// `oc02b_predecessor_exiting_within_the_wait_is_admitted_after_it` forces
/// that wait.
#[cfg(feature = "test-failpoints")]
fn anchor_dies_after_record() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        fixture.arm("host.anchor.after_record_write", "pause");
        let host = fixture.host();
        let (first, second) = (fixture.report("a"), fixture.report("b"));
        let (failed, (child, ticks)) = tokio::join!(
            host.acquire(fixture.spec(&["report", arg(&first)]), within(10)),
            async {
                let anchor = fixture.acked("host.anchor.after_record_write").await;
                // The child may already have executed the vendor.
                let child = support::children(anchor)[0];
                let ticks = stat(child).unwrap().1;
                assert_eq!(recorded(&fixture.lock()), Some((child, ticks)));
                fixture.disarm("host.anchor.after_record_write");
                kill(anchor);
                (child, ticks)
            }
        );
        assert!(failed.is_err());
        let admitted = host
            .acquire(fixture.spec(&["report", arg(&second)]), within(10))
            .await
            .unwrap();
        assert!(!alive(child, ticks), "B admitted while A's child lives");
        report(&second, Duration::from_secs(5))
            .await
            .expect("B ran");
        close(admitted).await;
    });
}

/// Ignored: a `security.capability` program (set by `setcap`) is refused.
fn file_capability_program() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let program = fixture.root.join("capable");
        fs::copy("/bin/true", &program).unwrap();
        let set = Command::new("setcap")
            .arg("cap_net_bind_service+ep")
            .arg(&program)
            .status()
            .unwrap();
        assert!(set.success(), "setcap failed");
        let mut spec = fixture.spec(&["report", "unused"]);
        spec.program = program;
        let refused = fixture
            .host()
            .acquire(spec, within(10))
            .await
            .err()
            .unwrap();
        assert_eq!(fence(refused), FenceRefusal::ProgramPrivileged);
    });
}

/// Ignored: VIA running as uid 0 is `PrivilegedVia`.
fn privileged_root() {
    assert!(rustix::process::getuid().is_root(), "{NEEDS_ROOT}");
    privileged_refused();
}

/// Ignored: VIA started with differing GIDs or capabilities is
/// `PrivilegedVia`.
fn privileged_credentials() {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let field = |name: &str| -> Vec<String> {
        status
            .lines()
            .find(|line| line.starts_with(name))
            .map(|line| {
                line.split_ascii_whitespace()
                    .skip(1)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let gids = field("Gid:");
    let differing = gids.iter().any(|gid| *gid != gids[0]);
    let capable = ["CapPrm:", "CapEff:", "CapAmb:"].iter().any(|name| {
        field(name)
            .first()
            .is_some_and(|mask| !mask.trim_start_matches('0').is_empty())
    });
    assert!(differing || capable, "{NEEDS_CREDENTIAL_HARNESS}");
    privileged_refused();
}

fn privileged_refused() {
    runtime().block_on(async {
        let fixture = Fixture::new();
        let refused = fixture
            .host()
            .acquire(fixture.spec(&["report", "unused"]), within(10))
            .await
            .err()
            .unwrap();
        assert_eq!(fence(refused), FenceRefusal::PrivilegedVia);
        assert_eq!(fixture.arm_intents().await, 0);
    });
}
