//! x.3.2 X0 items 1.3 and 1.4 at the Wire level, over a real anchor and
//! Store: a shared server's connection opens with its own evidence folder,
//! `evidence/servers/<server_id>/`, its `stderr.log` there and no turn
//! folder; a turn's folder is created on demand and never holds the
//! server's `stderr.log`. Written before the owner branch.
#![cfg(feature = "test-failpoints")]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use tokio::sync::watch;
use via_store::{ServerId, Store};
use via_wire::{
    CloseMode, CloseRequest, Deadline, EnvAllowList, PrivateProcessSpec, ProcessOwner,
    RuntimeConfig, SessionId, TurnNumber, WireCleanup, WireParts, WireRuntime, WireSignals,
    run_anchor_from_args,
};

#[test]
fn anchor_entry() {
    if let Some(config) = std::env::var_os("VIA_WIRE_TEST_CONFIG") {
        std::process::exit(run_anchor_from_args(&[config]));
    }
}

/// A private root with `state/` and `anchors/`, and the anchor wrapper that
/// re-enters this test binary at [`anchor_entry`].
struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "via-x2-server-open-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        for part in ["", "state", "anchors"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(part))
                .unwrap();
        }
        let binary = root.join("anchor-test-wrapper");
        let executable = std::env::current_exe().unwrap();
        let script = format!(
            "#!/bin/sh\nVIA_WIRE_TEST_CONFIG=\"$2\" exec '{}' --exact anchor_entry --nocapture\n",
            executable.to_string_lossy().replace('\'', "'\\''")
        );
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn within(seconds: u64) -> Deadline {
    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(seconds))
}

/// Item 1.3: a server-owned connection's evidence is the server's folder;
/// no turn folder exists until `turn_folder` creates one (item 1.4), and
/// the server's `stderr.log` is never in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_server_open_has_no_turn_folder() {
    let root = Root::new();
    let state = root.0.join("state");
    let store = Store::open(&state).unwrap();
    let wire = WireRuntime::new(
        RuntimeConfig {
            anchor_binary: root.0.join("anchor-test-wrapper"),
            anchor_dir: root.0.join("anchors"),
            vendor_state_dir: state.join("vendor"),
        },
        store.runtime_resources(),
    )
    .unwrap();
    let server = ServerId::try_from("v_0123456789ab").unwrap();
    let spec = PrivateProcessSpec {
        program: PathBuf::from("/bin/cat"),
        args: Vec::<OsString>::new(),
        cwd: root.0.clone(),
        env: EnvAllowList::default(),
        owner: ProcessOwner::Server {
            server_id: server.clone(),
        },
        stderr_path: PathBuf::new(),
        capacity: None,
        die_with_anchor: false,
        exclusive_lock: None,
        version_probe: None,
        stderr: via_wire::StderrCapture::Log,
    };
    let signals = WireSignals {
        force: watch::channel(None).1,
        wake: watch::channel(0).1,
        gate: Arc::new(|| false),
        inbound: via_wire::InboundBounds::DEFAULT,
    };
    let WireParts { sender, messages } = wire
        .open_connection(spec, within(5), signals)
        .await
        .unwrap()
        .into_parts();
    let evidence = state.join("evidence");
    let server_folder = evidence.join("servers").join(server.as_str());
    assert!(server_folder.join("stderr.log").is_file());
    let entries: Vec<_> = fs::read_dir(&evidence)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["servers"], "a turn folder at server open");

    let session = SessionId::try_from("s_0123456789ab").unwrap();
    let turn = TurnNumber::try_from(1).unwrap();
    let folder = wire.turn_folder(&session, turn).await.unwrap();
    assert_eq!(folder.path(), evidence.join(session.as_str()).join("1"));
    assert!(folder.path().is_dir());
    folder.keep_undecoded(b"not json\n", "undecodable").await;
    assert!(folder.path().join("undecoded.bin").is_file());
    assert!(
        folder
            .take_undecoded()
            .is_some_and(|note| note.contains("undecoded.bin"))
    );
    assert!(
        !folder.path().join("stderr.log").exists(),
        "the server's stderr.log in a turn folder"
    );

    let close = sender
        .close(CloseRequest {
            mode: CloseMode::Force,
            deadline: within(3),
        })
        .await;
    assert_eq!(close.cleanup, WireCleanup::Quiescent);
    messages.finish(within(2)).await;
    let _ = wire.shutdown(within(3), &[]).await;
}
