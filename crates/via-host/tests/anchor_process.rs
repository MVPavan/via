//! Real Linux group, vendor pipe and Store-journal coverage for the Host entrypoint.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    ffi::OsString,
    fs,
    io::{BufRead, Read},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use via_host::{
    CleanupEvidence, CloseMode, CloseRequest, Deadline, EnvAllowList, Host, PrivateProcessSpec,
    ProcessOwner, SessionId, TurnNumber, run_anchor_from_args,
};
use via_store::{SpawnRecord, Store};

#[test]
fn anchor_entry() {
    if let Some(config) = std::env::var_os("VIA_HOST_TEST_CONFIG") {
        std::process::exit(run_anchor_from_args(&[config]));
    }
}

struct Fixture {
    root: PathBuf,
    store: Store,
    binary: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "via-host-test-{}-{}",
            std::process::id(),
            random_hex()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        for part in ["state", "anchors"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(part))
                .unwrap();
        }
        let store = Store::open(&root.join("state")).unwrap();
        store
            .client()
            .commit_spawn(SpawnRecord {
                session_id: SessionId::try_from("s_0123456789ab").unwrap(),
                handle_hash: [7_u8; 32],
                receipt: serde_json::json!({"session_id":"s_0123456789ab","state":"queued"}),
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
            "#!/bin/sh\nVIA_HOST_TEST_CONFIG=\"$2\" exec {} --exact anchor_entry --nocapture\n",
            sh_quote(&executable)
        );
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
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

    fn spec(&self, program: &str) -> PrivateProcessSpec {
        PrivateProcessSpec {
            program: PathBuf::from(program),
            args: Vec::new(),
            cwd: self.root.clone(),
            env: EnvAllowList::default(),
            owner: ProcessOwner {
                session_id: SessionId::try_from("s_0123456789ab").unwrap(),
                turn: TurnNumber::try_from(1).unwrap(),
            },
            stderr_path: self.root.join(format!("stderr-{}.log", next_stderr())),
            capacity: None,
        }
    }
}

/// The fixture's one owning turn, the only turn shutdown reports on.
fn owner_turns() -> [(SessionId, TurnNumber); 1] {
    [(
        SessionId::try_from("s_0123456789ab").unwrap(),
        TurnNumber::try_from(1).unwrap(),
    )]
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn random_hex() -> String {
    let mut bytes = [0_u8; 8];
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    format!("{:016x}", u64::from_ne_bytes(bytes))
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn read_control_reply(stream: &mut tokio::net::UnixStream) -> serde_json::Value {
    let mut bytes = Vec::new();
    loop {
        let byte = stream.read_u8().await.unwrap();
        if byte == b'\n' {
            return serde_json::from_slice(&bytes).unwrap();
        }
        assert!(bytes.len() < 1024);
        bytes.push(byte);
    }
}

async fn write_control_request(stream: &mut tokio::net::UnixStream, value: &serde_json::Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
}

async fn write_fragmented_request(stream: &mut tokio::net::UnixStream, value: &serde_json::Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    let split = bytes.len() / 2;
    stream.write_all(&bytes[..split]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(70)).await;
    stream.write_all(&bytes[split..]).await.unwrap();
}

#[test]
fn vendor_pipes_detach_and_verified_anchor_stops_its_group() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(4));
        let spec = fixture.spec("/bin/cat");
        let stderr_path = spec.stderr_path.clone();
        let acquired = match host.acquire(spec, deadline).await {
            Ok(value) => value,
            Err(error) => {
                let paths: Vec<_> = fs::read_dir(fixture.root.join("anchors"))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
                let records = fixture
                    .store
                    .runtime_resources()
                    .into_wire_parts()
                    .1
                    .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT)
                    .await
                    .unwrap();
                panic!(
                    "acquire: {error}; paths={paths:?}; records={:?}",
                    records
                        .iter()
                        .map(|record| (&record.intent.anchor_id, &record.phase, &record.identity))
                        .collect::<Vec<_>>()
                );
            }
        };
        assert_eq!(
            acquired.control.identity().pid,
            acquired.control.identity().pgid
        );
        let via_host::OwnedPipes {
            mut stdin,
            mut stdout,
        } = acquired.pipes;
        stdin.write_all(b"vendor-data\n").await.unwrap();
        drop(stdin);
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stdout.read_to_end(&mut output))
            .await
            .expect("anchor must detach both vendor pipe descriptions")
            .unwrap();
        // Design §7.2: stderr is the turn's file, not a pipe.
        let errors = fs::read(&stderr_path).unwrap();
        assert!(
            output
                .windows(b"vendor-data\n".len())
                .any(|part| part == b"vendor-data\n")
        );
        assert!(errors.is_empty());
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            })
            .await;
        assert!(
            matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)),
            "{close:?}"
        );
    });
}

#[test]
fn spawn_failure_never_returns_vendor_pipes_and_recovery_proves_absence() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let mut spec = fixture.spec("/no/such/via-vendor");
        spec.env = EnvAllowList::try_from_entries(vec![(
            OsString::from("EXPLICIT"),
            OsString::from("only"),
        )])
        .unwrap();
        let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3));
        assert!(host.acquire(spec, deadline).await.is_err());
        let recovered = host
            .recover_page(
                None,
                via_store::ANCHOR_PAGE_LIMIT,
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            )
            .await
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(
            matches!(recovered[0].cleanup, CleanupEvidence::GroupAbsent(_)),
            "{recovered:?}"
        );
    });
}

#[test]
fn unauthenticated_reconnect_cannot_steal_live_controller() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let acquired = host
            .acquire(
                fixture.spec("/bin/cat"),
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            )
            .await
            .unwrap();
        let record = fixture
            .store
            .runtime_resources()
            .into_wire_parts()
            .1
            .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT)
            .await
            .unwrap()
            .remove(0);
        let mut intruder = tokio::net::UnixStream::connect(&record.intent.socket_path)
            .await
            .unwrap();
        intruder
            .write_all(
                format!(
                    "{{\"kind\":\"stop\",\"generation\":\"{}\",\"deadline_monotonic_ns\":0}}\n",
                    record.intent.generation
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(intruder);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let via_host::OwnedPipes {
            mut stdin,
            mut stdout,
        } = acquired.pipes;
        stdin.write_all(b"still-alive\n").await.unwrap();
        drop(stdin);
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), stdout.read_to_end(&mut output))
            .await
            .unwrap()
            .unwrap();
        assert!(
            output
                .windows(b"still-alive\n".len())
                .any(|part| part == b"still-alive\n"),
            "unauthenticated socket connection interrupted vendor"
        );
        drop(stdout);
        let report = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            })
            .await;
        assert!(matches!(report.cleanup, CleanupEvidence::GroupAbsent(_)));
    });
}

#[test]
fn controller_eof_triggers_autonomous_group_cleanup_and_recovery_proof() {
    let first_runtime = runtime();
    let fixture = first_runtime.block_on(Fixture::new());
    let host = fixture.host();
    let mut spec = fixture.spec("/bin/sleep");
    spec.args.push(OsString::from("10"));
    let acquired = first_runtime
        .block_on(host.acquire(
            spec,
            Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
        ))
        .unwrap();
    let vendor_pid = first_runtime.block_on(async {
        fixture
            .store
            .runtime_resources()
            .into_wire_parts()
            .1
            .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT)
            .await
            .unwrap()[0]
            .vendor_pid
            .unwrap()
    });
    // Dropping the daemon runtime closes its controller task and socket.
    drop(first_runtime);
    drop(acquired);
    let recovered = runtime()
        .block_on(host.recover_page(
            None,
            via_store::ANCHOR_PAGE_LIMIT,
            Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
        ))
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert!(
        matches!(recovered[0].cleanup, CleanupEvidence::GroupAbsent(_)),
        "{recovered:?}"
    );
    assert!(
        !PathBuf::from(format!("/proc/{vendor_pid}")).exists(),
        "vendor survived controller EOF"
    );
}

#[test]
fn fragmented_status_and_stop_survive_anchor_poll_ticks() {
    use std::os::unix::ffi::OsStrExt;
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let anchor_id = format!("{}{}", random_hex(), random_hex());
        let generation = format!("{}{}", random_hex(), random_hex());
        let marker = format!("{}{}", random_hex(), random_hex());
        let socket = fixture.root.join("anchors").join(format!("{anchor_id}.sock"));
        let config = fixture.root.join("anchors").join(format!("{anchor_id}.json"));
        let bootstrap = serde_json::json!({"anchor_id":anchor_id,"generation":generation,
            "marker":marker,"controller_pid":std::process::id(),"socket_path":socket});
        fs::write(&config, serde_json::to_vec(&bootstrap).unwrap()).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let mut command = tokio::process::Command::new(&fixture.binary);
        command.arg("__via_host_anchor").arg(&config).process_group(0)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut anchor = command.spawn().unwrap();
        let _vendor_stdin = anchor.stdin.take().unwrap();
        let _vendor_stdout = anchor.stdout.take().unwrap();
        let _vendor_stderr = anchor.stderr.take().unwrap();
        let mut control = loop {
            match tokio::net::UnixStream::connect(&socket).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        assert_eq!(read_control_reply(&mut control).await["kind"], "ready");
        write_control_request(&mut control, &serde_json::json!({"kind":"configure","vendor":{
            "program":b"/bin/cat".to_vec(),"args":[],"cwd":fixture.root.as_os_str().as_bytes().to_vec(),"env":[]
        }})).await;
        assert_eq!(read_control_reply(&mut control).await["kind"], "configured");
        write_control_request(&mut control, &serde_json::json!({"kind":"arm","generation":generation})).await;
        assert_eq!(read_control_reply(&mut control).await["kind"], "spawned");
        write_fragmented_request(&mut control, &serde_json::json!({"kind":"status","generation":generation})).await;
        let status = tokio::time::timeout(Duration::from_secs(1), read_control_reply(&mut control)).await.unwrap();
        assert_eq!(status["kind"], "status");
        write_fragmented_request(&mut control, &serde_json::json!({"kind":"stop","generation":generation,"deadline_monotonic_ns":u64::MAX})).await;
        let stopping = tokio::time::timeout(Duration::from_secs(1), read_control_reply(&mut control)).await.unwrap();
        assert_eq!(stopping["kind"], "stopping");
        tokio::time::timeout(Duration::from_secs(2), anchor.wait()).await.unwrap().unwrap();
    });
}

#[test]
fn dropping_last_control_handle_closes_socket_and_stops_vendor() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let mut spec = fixture.spec("/bin/sleep");
        spec.args.push(OsString::from("10"));
        let acquired = host
            .acquire(
                spec,
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            )
            .await
            .unwrap();
        let vendor_pid = fixture
            .store
            .runtime_resources()
            .into_wire_parts()
            .1
            .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT)
            .await
            .unwrap()[0]
            .vendor_pid
            .unwrap();
        drop(acquired);
        tokio::time::timeout(Duration::from_secs(2), async {
            while PathBuf::from(format!("/proc/{vendor_pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("last control drop must trigger anchor EOF cleanup");
    });
}

#[test]
fn shutdown_closes_live_control_joins_tasks_and_preserves_absence_evidence() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let mut spec = fixture.spec("/bin/sleep");
        spec.args.push(OsString::from("10"));
        let acquired = host
            .acquire(
                spec,
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            )
            .await
            .unwrap();
        let vendor_pid = fixture
            .store
            .runtime_resources()
            .into_wire_parts()
            .1
            .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT)
            .await
            .unwrap()[0]
            .vendor_pid
            .unwrap();
        let shutdown = host
            .shutdown(
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
                &owner_turns(),
            )
            .await;
        assert!(shutdown.failure.is_none(), "{shutdown:?}");
        assert_eq!(shutdown.pending_tasks, 0, "{shutdown:?}");
        assert_eq!(shutdown.failed_tasks, 0, "{shutdown:?}");
        assert_eq!(shutdown.recovery.len(), 1);
        assert!(
            matches!(
                shutdown.recovery[0].cleanup,
                CleanupEvidence::GroupAbsent(_)
            ),
            "{shutdown:?}"
        );
        assert!(
            shutdown.recovery[0].forced,
            "Host force-closed a live vendor: {shutdown:?}"
        );
        assert!(!PathBuf::from(format!("/proc/{vendor_pid}")).exists());
        // A later verifier must be able to use the committed proof without a live socket.
        let recovered = host
            .recover_page(
                None,
                via_store::ANCHOR_PAGE_LIMIT,
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(1)),
            )
            .await
            .unwrap();
        assert_eq!(shutdown.recovery[0].anchors, 1);
        assert!(matches!(
            recovered[0].cleanup,
            CleanupEvidence::GroupAbsent(_)
        ));
        drop(acquired);
    });
}

#[test]
fn expired_shutdown_keeps_live_owner_for_later_verified_cleanup() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let mut spec = fixture.spec("/bin/sleep");
        spec.args.push(OsString::from("10"));
        let acquired = host
            .acquire(
                spec,
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
            )
            .await
            .unwrap();
        let started = tokio::time::Instant::now();
        let expired = host.shutdown(Deadline::at(started), &owner_turns()).await;
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(expired.failure.is_some(), "{expired:?}");
        assert!(
            expired.pending_tasks > 0,
            "live reaper stays owned: {expired:?}"
        );
        let later = host
            .shutdown(
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
                &owner_turns(),
            )
            .await;
        assert!(later.failure.is_none(), "{later:?}");
        assert_eq!(later.pending_tasks, 0, "{later:?}");
        assert!(matches!(
            later.recovery[0].cleanup,
            CleanupEvidence::GroupAbsent(_)
        ));
        drop(acquired);
    });
}

#[test]
fn blocked_absence_commit_cannot_extend_close_deadline() {
    runtime().block_on(async {
        let fixture = Fixture::new().await;
        let host = fixture.host();
        let acquired = host.acquire(
            fixture.spec("/bin/cat"),
            Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
        ).await.unwrap();
        let first = acquired.control.close(CloseRequest {
            mode: CloseMode::Force,
            deadline: Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
        }).await;
        assert!(matches!(first.cleanup, CleanupEvidence::GroupAbsent(_)));

        let script = "import sqlite3,sys\nc=sqlite3.connect(sys.argv[1])\nc.execute('BEGIN EXCLUSIVE')\nprint('LOCKED',flush=True)\nsys.stdin.read(1)\nc.rollback()\n";
        let mut blocker = std::process::Command::new("python3")
            .arg("-c").arg(script)
            .arg(fixture.root.join("state/store.sqlite3"))
            .stdin(Stdio::piped()).stdout(Stdio::piped())
            .spawn().unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(blocker.stdout.take().unwrap())
            .read_line(&mut ready).unwrap();
        assert_eq!(ready, "LOCKED\n");
        let started = tokio::time::Instant::now();
        let second = acquired.control.close(CloseRequest {
            mode: CloseMode::Force,
            deadline: Deadline::at(started + Duration::from_millis(80)),
        }).await;
        assert_eq!(second.cleanup, CleanupEvidence::Uncertain(via_host::CleanupReason::EvidenceStoreFailure));
        assert!(started.elapsed() < Duration::from_millis(160), "close exceeded deadline while Store was locked");
        drop(blocker.stdin.take());
        blocker.wait().unwrap();
    });
}

/// W1-D Sol finding 3: `forced` comes from Host's own stop of a live vendor,
/// not from group absence alone. Releasing an unclosed control while the vendor
/// runs lets the anchor's EOF cleanup stop it, but that cleanup has no reply to
/// carry the anchor's evidence, so it is not reported forced (W3-F Sol 2); a
/// graceful close after the vendor exited on its own is absence without force.
#[test]
fn force_evidence_separates_host_stop_from_absence() {
    runtime().block_on(async {
        let deadline = || Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3));
        let released = Fixture::new().await;
        let host = released.host();
        let mut spec = released.spec("/bin/sleep");
        spec.args.push(OsString::from("10"));
        drop(host.acquire(spec, deadline()).await.unwrap());
        let report = host.shutdown(deadline(), &owner_turns()).await;
        assert_eq!(report.recovery.len(), 1, "{report:?}");
        assert!(
            matches!(report.recovery[0].cleanup, CleanupEvidence::GroupAbsent(_)),
            "{report:?}"
        );
        assert!(
            !report.recovery[0].forced,
            "a release carries no anchor evidence: {report:?}"
        );

        let exited = Fixture::new().await;
        let host = exited.host();
        let acquired = host
            .acquire(exited.spec("/bin/true"), deadline())
            .await
            .unwrap();
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Graceful,
                deadline: deadline(),
            })
            .await;
        assert!(close.vendor_exit.is_some(), "{close:?}");
        assert!(!close.forced, "the vendor exited on its own: {close:?}");
        drop(acquired);
        let report = host.shutdown(deadline(), &owner_turns()).await;
        assert!(
            matches!(report.recovery[0].cleanup, CleanupEvidence::GroupAbsent(_)),
            "{report:?}"
        );
        assert!(!report.recovery[0].forced, "absence alone: {report:?}");
    });
}

/// Whether `pid` names a live (non-zombie) process.
fn process_live(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|text| {
            let state = text.get(text.rfind(')')? + 2..)?.chars().next()?;
            Some(state != 'Z' && state != 'X')
        })
        .unwrap_or(false)
}

/// Acquires `/bin/cat`, ends it by closing its stdin, and returns once the
/// vendor has exited while Host's exit watch has not yet seen it: the vendor
/// exits between Host's last status poll and a following stop. `None` when a
/// poll saw the exit first; the caller retries with a fresh fixture.
async fn vendor_exited_unobserved(
    fixture: &Fixture,
    host: &Host,
) -> Option<(via_host::ProcessControl, via_host::ExitReceiver)> {
    let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3));
    let acquired = host
        .acquire(fixture.spec("/bin/cat"), deadline)
        .await
        .unwrap();
    let vendor_pid = fixture
        .store
        .runtime_resources()
        .into_wire_parts()
        .1
        .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT)
        .await
        .unwrap()[0]
        .vendor_pid
        .unwrap();
    let via_host::AcquiredProcess {
        pipes,
        control,
        exits,
    } = acquired;
    drop(pipes.stdin);
    let started = std::time::Instant::now();
    while process_live(vendor_pid) {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cat did not exit"
        );
        std::thread::yield_now();
    }
    let unobserved = exits.borrow().is_none();
    unobserved.then_some((control, exits))
}

/// W3-F Sol 2: `forced` needs the anchor's evidence that its cleanup stopped
/// a live vendor. A vendor that exited between Host's last status poll and a
/// force close was not stopped by Host, even though the anchor accepts the
/// stop and Host's exit watch is still empty.
#[test]
fn force_close_after_unobserved_vendor_exit_is_not_forced() {
    runtime().block_on(async {
        for _ in 0..20 {
            let fixture = Fixture::new().await;
            let host = fixture.host();
            let Some((control, _exits)) = vendor_exited_unobserved(&fixture, &host).await else {
                continue;
            };
            let close = control
                .close(CloseRequest {
                    mode: CloseMode::Force,
                    deadline: Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
                })
                .await;
            assert!(
                matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)),
                "{close:?}"
            );
            assert!(!close.forced, "the vendor had already exited: {close:?}");
            let report = host
                .shutdown(
                    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
                    &owner_turns(),
                )
                .await;
            assert!(!report.recovery[0].forced, "{report:?}");
            return;
        }
        panic!("Host's status poll saw every vendor exit first");
    });
}

/// W3-F Sol 2, released-control path: releasing an unclosed control after the
/// vendor exited, before Host's poll saw it, is not force evidence either.
#[test]
fn released_control_after_unobserved_vendor_exit_is_not_forced() {
    runtime().block_on(async {
        for _ in 0..20 {
            let fixture = Fixture::new().await;
            let host = fixture.host();
            let Some(released) = vendor_exited_unobserved(&fixture, &host).await else {
                continue;
            };
            drop(released);
            let report = host
                .shutdown(
                    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
                    &owner_turns(),
                )
                .await;
            assert!(
                matches!(report.recovery[0].cleanup, CleanupEvidence::GroupAbsent(_)),
                "{report:?}"
            );
            assert!(
                !report.recovery[0].forced,
                "the vendor had already exited: {report:?}"
            );
            return;
        }
        panic!("Host's status poll saw every vendor exit first");
    });
}

// ---------------------------------------------------------------------------
// F22 isolated negatives (design T3 §9, §11; runtime §5.1–§5.2, §11): an
// anchor record whose identity does not verify gets no command, and nothing
// short of a same-boot, same-namespace `ESRCH` is quiescence.

/// How the test-controlled listener at a forged anchor's socket answers.
#[derive(Clone, Copy)]
enum Answer {
    /// Nothing listens at the socket path.
    Absent,
    /// Replies to a challenge with the stored identity and the nonce.
    Echo,
    /// Replies with a nonce other than the one Host sent.
    WrongNonce,
    /// Replies with the stored identity under another marker.
    OtherMarker,
}

/// This process's identity, which a same-process listener satisfies at
/// the peer-credential check.
fn own_identity(marker: &str) -> via_store::AnchorIdentity {
    let process = std::process::id();
    let (group, start_ticks) = process_stat(process).unwrap();
    via_store::AnchorIdentity {
        pid: process,
        pgid: group,
        uid: rustix::process::getuid().as_raw(),
        boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .to_owned(),
        pid_namespace: fs::read_link("/proc/self/ns/pid")
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        start_ticks,
        marker: marker.to_owned(),
    }
}

/// `(pgid, start_ticks)` from `/proc/<pid>/stat`, if the process exists.
fn process_stat(pid: u32) -> Option<(u32, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = text.get(text.rfind(')')? + 2..)?.split(' ').collect();
    Some((fields.get(2)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

/// A journal write's committed value; the fixture fails loudly otherwise.
fn committed<T>(outcome: via_store::CommitOutcome<T>) -> T {
    match outcome {
        via_store::CommitOutcome::Committed(value) => Some(value),
        via_store::CommitOutcome::NotCommitted(_) | via_store::CommitOutcome::Uncertain(_) => None,
    }
    .unwrap()
}

/// A live process in its own new group: a group Host must never signal.
fn decoy_group() -> std::process::Child {
    use std::os::unix::process::CommandExt;
    std::process::Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap()
}

/// Commits `identity` as a crashed daemon's anchor at `arm_intent`, serves
/// its socket per `answer`, and runs one reconciliation bounded by 1 s.
/// Returns Host's report and the request kinds the listener received.
async fn reconcile_forged(
    identity: via_store::AnchorIdentity,
    answer: Answer,
) -> (via_host::RecoveryReport, Vec<String>) {
    let fixture = Fixture::new().await;
    let journal = fixture.store.runtime_resources().into_wire_parts().1;
    let (anchor_id, generation) = ("forged-anchor", "forged-generation");
    let socket = fixture.root.join("anchors").join("forged.sock");
    let intent = via_store::AnchorIntent {
        anchor_id: anchor_id.to_owned(),
        generation: generation.to_owned(),
        marker: identity.marker.clone(),
        socket_path: socket.clone(),
        owner: via_store::ProcessOwner::Turn {
            session_id: SessionId::try_from("s_0123456789ab").unwrap(),
            turn: TurnNumber::try_from(1).unwrap(),
        },
        uid: identity.uid,
        boot_id: identity.boot_id.clone(),
        pid_namespace: identity.pid_namespace.clone(),
    };
    let version = committed(journal.commit_anchor_intent(intent).await).record_version;
    let version = committed(
        journal
            .commit_anchor_identified(anchor_id, generation, version, identity.clone())
            .await,
    );
    committed(
        journal
            .commit_arm_intent(anchor_id, generation, version)
            .await,
    );
    let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = match answer {
        Answer::Absent => None,
        Answer::Echo | Answer::WrongNonce | Answer::OtherMarker => {
            Some(tokio::net::UnixListener::bind(&socket).unwrap())
        }
    };
    let served = listener.map(|listener| {
        let received = std::sync::Arc::clone(&received);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                serve_forged(stream, &identity, answer, &received).await;
            }
        })
    });
    let host = fixture.host();
    let reports = host
        .recover_page(
            None,
            via_store::ANCHOR_PAGE_LIMIT,
            Deadline::at(tokio::time::Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap();
    if let Some(served) = served {
        served.abort();
    }
    assert_eq!(reports.len(), 1, "{reports:?}");
    let received = received.lock().unwrap().clone();
    (reports.into_iter().next().unwrap(), received)
}

/// Records each request's kind and answers a challenge per `answer`.
async fn serve_forged(
    stream: tokio::net::UnixStream,
    identity: &via_store::AnchorIdentity,
    answer: Answer,
    received: &std::sync::Mutex<Vec<String>>,
) {
    use tokio::io::AsyncBufReadExt as _;
    let (read, mut write) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        let kind = request["kind"].as_str().unwrap_or_default().to_owned();
        received.lock().unwrap().push(kind.clone());
        let reply = match kind.as_str() {
            "challenge" => {
                let nonce = match answer {
                    Answer::WrongNonce => "0".repeat(32),
                    Answer::Absent | Answer::Echo | Answer::OtherMarker => {
                        request["nonce"].as_str().unwrap().to_owned()
                    }
                };
                let marker = match answer {
                    Answer::OtherMarker => "another-anchors-marker".to_owned(),
                    Answer::Absent | Answer::Echo | Answer::WrongNonce => identity.marker.clone(),
                };
                serde_json::json!({"kind":"challenge","nonce":nonce,"identity":{
                    "pid":identity.pid,"pgid":identity.pgid,"uid":identity.uid,
                    "boot_id":identity.boot_id,"pid_namespace":identity.pid_namespace,
                    "start_ticks":identity.start_ticks,"marker":marker}})
            }
            "stop" => serde_json::json!({"kind":"stopping","stopped_live":true}),
            _ => return,
        };
        let mut bytes = serde_json::to_vec(&reply).unwrap();
        bytes.push(b'\n');
        if write.write_all(&bytes).await.is_err() {
            return;
        }
    }
}

/// Runtime §5.1, §11: a uid, start-ticks, group or marker mismatch between
/// the stored identity and the live peer, its challenge reply or `/proc`
/// never sends `Stop`, and the still-present group is never quiescent.
#[test]
fn recovery_sends_no_stop_on_an_identity_mismatch() {
    runtime().block_on(async {
        let mut decoy = decoy_group();
        let own = own_identity("forged-marker");
        let mut uid = own.clone();
        uid.uid += 1;
        uid.pgid = decoy.id();
        let mut ticks = own.clone();
        ticks.start_ticks += 1;
        let mut group = own.clone();
        group.pgid = decoy.id();
        let cases = [
            ("uid", uid, Answer::Echo),
            ("start ticks", ticks, Answer::Echo),
            ("pgid", group, Answer::Echo),
            ("marker", own, Answer::OtherMarker),
        ];
        for (name, identity, answer) in cases {
            let (report, received) = reconcile_forged(identity, answer).await;
            assert!(
                !received.iter().any(|kind| kind == "stop"),
                "{name}: Host commanded an unverified anchor: {received:?}"
            );
            assert!(
                matches!(report.cleanup, CleanupEvidence::Uncertain(_)) && !report.forced,
                "{name}: {report:?}"
            );
        }
        assert!(process_live(decoy.id()), "the decoy group was signalled");
        decoy.kill().unwrap();
        decoy.wait().unwrap();
    });
}

/// Runtime §5.1, §11: a challenge reply that does not echo Host's nonce is
/// refused although the identity matches: no `Stop`, no quiescence.
#[test]
fn recovery_refuses_a_forged_challenge() {
    runtime().block_on(async {
        let (report, received) =
            reconcile_forged(own_identity("forged-marker"), Answer::WrongNonce).await;
        assert_eq!(received, ["challenge"], "{report:?}");
        assert!(
            matches!(report.cleanup, CleanupEvidence::Uncertain(_)) && !report.forced,
            "{report:?}"
        );
    });
}

/// Runtime §5.2: the anchor (group leader) exited alone and an original
/// member survives. Nothing answers at the socket, so nothing is commanded;
/// the group query still finds the member, so cleanup is never quiescent,
/// and the member is never signalled.
#[test]
fn recovery_never_proves_a_group_whose_leader_alone_exited() {
    use std::io::BufRead as _;
    use std::os::unix::process::CommandExt;
    runtime().block_on(async {
        let mut leader = std::process::Command::new("/bin/sh")
            .args(["-c", "/bin/sleep 30 & echo $!"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let member: u32 = line.trim().parse().unwrap();
        let group = leader.id();
        leader.wait().unwrap();
        assert!(!process_live(group) && process_live(member));
        let mut identity = own_identity("forged-marker");
        identity.pid = group;
        identity.pgid = group;
        identity.start_ticks = 1;
        let (report, received) = reconcile_forged(identity, Answer::Absent).await;
        assert!(received.is_empty());
        assert_eq!(
            report.cleanup,
            CleanupEvidence::Uncertain(via_host::CleanupReason::Deadline),
            "{report:?}"
        );
        assert!(process_live(member), "the surviving member was signalled");
        let member = rustix::process::Pid::from_raw(i32::try_from(member).unwrap()).unwrap();
        rustix::process::kill_process(member, rustix::process::Signal::KILL).unwrap();
    });
}

/// Runtime §5.2: a stored namespace other than this one is refused after
/// the challenge and never probed; a group query denied by permission
/// (another user's group, where one exists) is never absence.
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a machine without another user's group cannot show the denied probe; say so"
)]
fn recovery_never_proves_absence_from_a_foreign_namespace_or_denied_probe() {
    runtime().block_on(async {
        let mut decoy = decoy_group();
        let mut foreign = own_identity("forged-marker");
        foreign.pid_namespace = "pid:[1]".to_owned();
        foreign.pgid = decoy.id();
        let (report, received) = reconcile_forged(foreign, Answer::Echo).await;
        assert!(!received.iter().any(|kind| kind == "stop"), "{received:?}");
        assert_eq!(
            report.cleanup,
            CleanupEvidence::Uncertain(via_host::CleanupReason::UnverifiedAnchor),
            "{report:?}"
        );
        assert!(process_live(decoy.id()), "the decoy group was signalled");
        decoy.kill().unwrap();
        decoy.wait().unwrap();
        // A denied probe needs a group this user may not signal; a root
        // run, or a machine without one, cannot show it.
        let Some(other) = denied_group() else {
            eprintln!("no group of another user to probe: denied-probe case not exercised");
            return;
        };
        let mut denied = own_identity("forged-marker");
        denied.pid = other;
        denied.pgid = other;
        let (report, received) = reconcile_forged(denied, Answer::Absent).await;
        assert!(received.is_empty());
        assert_eq!(
            report.cleanup,
            CleanupEvidence::Uncertain(via_host::CleanupReason::ProbeDenied),
            "{report:?}"
        );
    });
}

/// A live group leader (pid = pgid > 1) whose group this process may not
/// signal: the existence query itself answers `EPERM`.
fn denied_group() -> Option<u32> {
    fs::read_dir("/proc").ok()?.find_map(|entry| {
        let leader: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
        let (group, _) = process_stat(leader)?;
        let query = rustix::process::Pid::from_raw(i32::try_from(group).ok()?)?;
        (group == leader
            && group > 1
            && rustix::process::test_kill_process_group(query) == Err(rustix::io::Errno::PERM))
        .then_some(group)
    })
}

/// A fresh `stderr.log` name per spec: Host creates it exclusively.
fn next_stderr() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
