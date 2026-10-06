//! Runtime §4 `Capture::Off` (the `OpenCode` exception, owner 2026-10-06;
//! `vendors/opencode.md` §4.3 and `OC12b`) over a real anchor and Store: a
//! connection opened with `Capture::Off` keeps no payload byte anywhere.
//! Wire's automatic captures (a message over its cap, an unterminated
//! message at EOF) and Route's decode-failure capture into a turn folder
//! write no `undecoded.bin`; the note keeps only the description. With
//! `StderrCapture::CountOnly` no `stderr.log` exists.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::sync::watch;
use via_store::{ServerId, Store};
use via_wire::{
    Capture, CloseMode, CloseRequest, Deadline, EnvAllowList, InboundBounds, PrivateProcessSpec,
    ProcessOwner, RuntimeConfig, SessionId, StderrCapture, TurnNumber, WireCleanup, WireError,
    WireFailure, WireParts, WireRuntime, WireSignals, run_anchor_from_args,
};

const SECRET: &str = "SYNTHETIC-SECRET-capture-off";

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
            "via-capture-off-{}-{}",
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

    fn wire(&self, store: &Store) -> WireRuntime {
        WireRuntime::new(
            RuntimeConfig {
                anchor_binary: self.0.join("anchor-test-wrapper"),
                anchor_dir: self.0.join("anchors"),
                vendor_state_dir: self.0.join("state").join("vendor"),
            },
            store.runtime_resources(),
        )
        .unwrap()
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

/// A server running `script` under `/bin/sh`, its stderr counted only.
fn spec(root: &Path, server: &ServerId, script: &str) -> PrivateProcessSpec {
    PrivateProcessSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec![OsString::from("-c"), OsString::from(script)],
        cwd: root.to_path_buf(),
        env: EnvAllowList::default(),
        owner: ProcessOwner::Server {
            server_id: server.clone(),
        },
        stderr_path: PathBuf::new(),
        capacity: None,
        die_with_anchor: false,
        exclusive_lock: None,
        version_probe: None,
        stderr: StderrCapture::CountOnly,
    }
}

fn signals(capture: Capture) -> WireSignals {
    WireSignals {
        force: watch::channel(None).1,
        wake: watch::channel(0).1,
        gate: Arc::new(|| false),
        inbound: InboundBounds {
            message_bytes: 64,
            staging_bytes: 4096,
            skip_oversize: false,
        },
        capture,
    }
}

/// Every regular file under `dir`, recursively.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => found.extend(files(&path)),
            Ok(kind) if kind.is_file() => found.push(path),
            _ => {}
        }
    }
    found
}

fn holds_secret(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| {
        bytes
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes())
    })
}

/// Opens a `Capture::Off` server connection running `script`, waits for
/// its failure `expected`, and returns the note Wire kept.
async fn failed_note(
    root: &Root,
    wire: &WireRuntime,
    server: &ServerId,
    script: &str,
    expected: WireFailure,
) -> String {
    let WireParts {
        sender,
        mut messages,
    } = wire
        .open_connection(
            spec(&root.0, server, script),
            within(5),
            signals(Capture::Off),
        )
        .await
        .unwrap()
        .into_parts();
    let failure = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match messages.next_message().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("no failure"),
                Err(error) => return error,
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(failure, WireError::Message(failure) if failure == expected),
        "{failure:?}"
    );
    let note = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(note) = sender.take_undecoded() {
                return note;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let close = sender
        .close(CloseRequest {
            mode: CloseMode::Force,
            deadline: within(3),
        })
        .await;
    assert_eq!(close.cleanup, WireCleanup::Quiescent);
    messages.finish(within(2)).await;
    note
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_off_keeps_no_payload_anywhere() {
    let root = Root::new();
    let store = Store::open(&root.0.join("state")).unwrap();
    let wire = root.wire(&store);
    let evidence = root.0.join("state").join("evidence");

    let over = ServerId::try_from("v_0123456789ab").unwrap();
    let note = failed_note(
        &root,
        &wire,
        &over,
        &format!("echo {SECRET} >&2; printf '%s%0100d\\n' {SECRET} 0"),
        WireFailure::MessageTooLarge,
    )
    .await;
    assert!(!note.contains(SECRET), "{note}");
    let folder = evidence.join("servers").join(over.as_str());
    assert!(!folder.join("undecoded.bin").exists(), "over-cap capture");
    assert!(!folder.join("stderr.log").exists(), "stderr.log kept");

    let cut = ServerId::try_from("v_0123456789ac").unwrap();
    let note = failed_note(
        &root,
        &wire,
        &cut,
        &format!("printf '%s' {SECRET}"),
        WireFailure::UnterminatedMessage,
    )
    .await;
    assert!(!note.contains(SECRET), "{note}");
    let folder = evidence.join("servers").join(cut.as_str());
    assert!(
        !folder.join("undecoded.bin").exists(),
        "unterminated capture"
    );

    let session = SessionId::try_from("s_0123456789ab").unwrap();
    let turn = TurnNumber::try_from(1).unwrap();
    let folder = wire
        .turn_folder_with(&session, turn, Capture::Off)
        .await
        .unwrap();
    folder
        .keep_undecoded(SECRET.as_bytes(), "undecodable event, 28 bytes")
        .await;
    assert!(
        !folder.path().join("undecoded.bin").exists(),
        "Route's capture"
    );
    let note = folder.take_undecoded().unwrap();
    assert!(
        note.contains("28 bytes") && !note.contains(SECRET),
        "{note}"
    );

    for file in files(&root.0) {
        assert!(!holds_secret(&file), "secret in {}", file.display());
    }
    let _ = wire.shutdown(within(3), &[]).await;
}

/// The default, `Capture::On`, still keeps a turn folder's undecoded
/// prefix (Claude, Codex and Pi unchanged).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_on_still_keeps_the_prefix() {
    let root = Root::new();
    let store = Store::open(&root.0.join("state")).unwrap();
    let wire = root.wire(&store);
    let session = SessionId::try_from("s_0123456789ab").unwrap();
    let turn = TurnNumber::try_from(1).unwrap();
    let folder = wire.turn_folder(&session, turn).await.unwrap();
    folder.keep_undecoded(b"not json\n", "undecodable").await;
    assert!(folder.path().join("undecoded.bin").is_file());
    let _ = wire.shutdown(within(3), &[]).await;
}
