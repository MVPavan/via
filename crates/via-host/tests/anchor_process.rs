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
                prompt: "fixture".into(),
                initial_event: serde_json::json!({"seq":1,"type":"turn.queued"}),
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
        }
    }
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
        let acquired = match host.acquire(fixture.spec("/bin/cat"), deadline).await {
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
                    .list_anchor_records()
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
            mut stderr,
        } = acquired.pipes;
        stdin.write_all(b"vendor-data\n").await.unwrap();
        drop(stdin);
        let mut output = Vec::new();
        let mut errors = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            stdout.read_to_end(&mut output).await.unwrap();
            stderr.read_to_end(&mut errors).await.unwrap();
        })
        .await
        .expect("anchor must detach all three vendor pipe descriptions");
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
            .recover(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(3),
            ))
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
            .list_anchor_records()
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
            stderr,
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
        drop(stderr);
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
            .list_anchor_records()
            .await
            .unwrap()[0]
            .vendor_pid
            .unwrap()
    });
    // Dropping the daemon runtime closes its controller task and socket.
    drop(first_runtime);
    drop(acquired);
    let recovered = runtime()
        .block_on(host.recover(Deadline::at(
            tokio::time::Instant::now() + Duration::from_secs(3),
        )))
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
            .list_anchor_records()
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
            .list_anchor_records()
            .await
            .unwrap()[0]
            .vendor_pid
            .unwrap();
        let shutdown = host
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(3),
            ))
            .await
            .unwrap();
        assert_eq!(shutdown.pending_tasks, 0, "{shutdown:?}");
        assert_eq!(shutdown.recovery.len(), 1);
        assert!(
            matches!(
                shutdown.recovery[0].cleanup,
                CleanupEvidence::GroupAbsent(_)
            ),
            "{shutdown:?}"
        );
        assert!(!PathBuf::from(format!("/proc/{vendor_pid}")).exists());
        // A later verifier must be able to use the committed proof without a live socket.
        let recovered = host
            .recover(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(1),
            ))
            .await
            .unwrap();
        assert_eq!(recovered[0].generation, shutdown.recovery[0].generation);
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
        assert!(host.shutdown(Deadline::at(started)).await.is_err());
        assert!(started.elapsed() < Duration::from_millis(100));
        let later = host
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(3),
            ))
            .await
            .unwrap();
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
