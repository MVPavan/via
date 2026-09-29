//! Task 4 design §6.5 at the Store level: blob files, their handles and
//! their checks. A torn write leaves no file; a finished blob is read back
//! only with its length, SHA-256 and UTF-8 intact; `verify_blobs` refuses a
//! referenced blob that changed and `sweep_blobs` removes unreferenced
//! ones. Its own process under nextest. Written before the blob path.
#![cfg(feature = "test-failpoints")]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    BlobRef, Prompt, SessionId, SpawnRecord, Store, StoreClient, StoreError, TurnNumber, failpoint,
};

const TOKEN: &str = "s1-blob-token-0123456789";
const SESSION: &str = "s_7f3k9q2mzr4c";
const CHUNK: usize = 64 * 1024;

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn event(kind: &str, seq: u64) -> Value {
    json!({"type":kind,"seq":seq,"at":"2026-01-01T00:00:00.000Z"})
}

/// The files in `<state>/blobs`.
fn blob_files(state: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(state.join("blobs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
}

fn blob_path(state: &Path, blob: &BlobRef) -> PathBuf {
    state.join("blobs").join(format!("{}.blob", blob.id()))
}

/// Writes `text` through a `BlobWriter` in 64 KiB chunks.
async fn write_blob(client: &StoreClient, text: &str) -> BlobRef {
    let mut writer = client.blob_writer().await.unwrap();
    for chunk in text.as_bytes().chunks(CHUNK) {
        writer.write(chunk).await.unwrap();
    }
    writer.finish().await.unwrap()
}

/// 300 KiB of text with multi-byte characters.
fn text() -> String {
    "aé€😀".repeat(300 * 1024 / 10)
}

/// A torn write: the second chunk's write fails and `discard` unlinks the
/// unfinished file; a handle dropped unfinished unlinks its file too.
async fn torn_writes_leave_no_file(store: &Store, points: &Path, state: &Path) {
    let client = &store.client();
    fs::write(
        points.join("blob.write.fail_after.json"),
        json!({"token":TOKEN,"occurrence":2,"action":"fail_io"}).to_string(),
    )
    .unwrap();
    let mut writer = client.blob_writer().await.unwrap();
    writer.write(&vec![b'a'; CHUNK]).await.unwrap();
    let torn = writer.write(&vec![b'b'; CHUNK]).await;
    assert!(matches!(torn, Err(StoreError::Write(_))), "{torn:?}");
    writer.discard().await;
    assert!(blob_files(state).is_empty());
    fs::remove_file(points.join("blob.write.fail_after.json")).unwrap();
    // A handle dropped unfinished unlinks its file too.
    let mut dropped = client.blob_writer().await.unwrap();
    dropped.write(b"partial").await.unwrap();
    drop(dropped);
    // The unlink is an owned blob step (review round 1): wait for it to end.
    while store.blob_tasks() > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(blob_files(state).is_empty());
}

#[test]
fn s1_blob_torn_and_mismatched_blobs_are_refused_or_swept() {
    let points = private_dir();
    failpoint::activate(points.path(), TOKEN).unwrap();
    let root = private_dir();
    let state = root.path();
    let store = Store::open(state).unwrap();
    let client = store.client();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        torn_writes_leave_no_file(&store, points.path(), state).await;

        // A finished blob: 0600, exact bytes, referenced by a spawn.
        let text = text();
        let blob = write_blob(&client, &text).await;
        assert_eq!(blob.len(), text.len() as u64);
        let path = blob_path(state, &blob);
        let mode = fs::symlink_metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
        client
            .commit_spawn(SpawnRecord {
                session_id: SessionId::try_from(SESSION).unwrap(),
                handle_hash: [7_u8; 32],
                receipt: json!({"state":"queued"}),
                params: json!({"harness":"fake"}),
                prompt: Prompt::Blob(blob.clone()),
                effective: json!({"deadlines":{"wall_ms":1}}),
                initial_event: event("turn.queued", 1),
            })
            .await
            .unwrap();
        let queued = client
            .queued_turn(
                &SessionId::try_from(SESSION).unwrap(),
                TurnNumber::try_from(1).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queued.prompt, Prompt::Blob(blob.clone()));
        assert_eq!(client.load_prompt(&blob).await.unwrap(), text);
        client.verify_blobs().await.unwrap();

        // Mismatched: one byte changed, same length.
        let mut changed = text.clone().into_bytes();
        changed[0] = b'b';
        fs::write(&path, &changed).unwrap();
        let loaded = client.load_prompt(&blob).await;
        assert!(
            matches!(loaded, Err(StoreError::CorruptEvidence)),
            "{loaded:?}"
        );
        let verified = client.verify_blobs().await;
        assert!(
            matches!(verified, Err(StoreError::Corrupt(_))),
            "{verified:?}"
        );

        // Torn on disk: a short file.
        fs::write(&path, &text.as_bytes()[..CHUNK]).unwrap();
        let loaded = client.load_prompt(&blob).await;
        assert!(
            matches!(loaded, Err(StoreError::CorruptEvidence)),
            "{loaded:?}"
        );
        assert!(matches!(
            client.verify_blobs().await,
            Err(StoreError::Corrupt(_))
        ));

        // Not UTF-8 although the length and SHA-256 match what was stored.
        let invalid = [0xff_u8; 8];
        let mut writer = client.blob_writer().await.unwrap();
        writer.write(&invalid).await.unwrap();
        let bytes = writer.finish().await.unwrap();
        let loaded = client.load_prompt(&bytes).await;
        assert!(
            matches!(loaded, Err(StoreError::CorruptEvidence)),
            "{loaded:?}"
        );

        // Restored, the referenced blob verifies; the two unreferenced ones
        // (the invalid bytes and one more) are swept, the referenced one kept.
        fs::write(&path, text.as_bytes()).unwrap();
        client.verify_blobs().await.unwrap();
        let unreferenced = write_blob(&client, "spare").await;
        assert_eq!(blob_files(state).len(), 3);
        assert_eq!(client.sweep_blobs().await.unwrap(), 2);
        assert_eq!(blob_files(state), vec![path.clone()]);

        // A finished blob known not to be adopted is discarded.
        let discarded = write_blob(&client, "discard me").await;
        client.discard_blob(discarded.clone()).await;
        assert!(!blob_path(state, &discarded).exists());
        assert!(!blob_path(state, &unreferenced).exists());
    });
    // Created: torn, dropped, text, invalid, spare, discarded.
    assert_eq!(store.blob_writes(), 6);
}

/// Review round 1 (coding-style §5 task ownership): a blob step stalled on
/// the blocking pool fails its caller `Write` (the request's
/// `not_committed`) at the 2 s bound, yet stays owned by the Store until
/// it ends, and is reaped then. The file it created unowned is swept.
/// Releases a paused point when dropped, so a failed assertion never
/// leaves the runtime's drop waiting on the stalled blocking thread.
struct Release(PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, b"");
    }
}

#[test]
fn s1_blob_stalled_step_times_out_owned_until_reaped() {
    let points = private_dir();
    failpoint::activate(points.path(), TOKEN).unwrap();
    let root = private_dir();
    let state = root.path();
    let store = Store::open(state).unwrap();
    let client = store.client();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    fs::write(
        points.path().join("blob.step.stall.json"),
        json!({"token":TOKEN,"occurrence":1,"action":"pause"}).to_string(),
    )
    .unwrap();
    let release = Release(points.path().join("blob.step.stall.1.release"));
    runtime.block_on(async {
        let stalled = client.blob_writer().await;
        assert!(
            matches!(stalled, Err(StoreError::Write(_))),
            "{:?}",
            stalled.map(|_| ())
        );
    });
    assert!(points.path().join("blob.step.stall.1.ack").exists());
    // Timed out, not abandoned: the Store still owns the step.
    assert_eq!(store.blob_tasks(), 1);
    drop(release);
    // Reaped once it ends (bounded wait on the task's own completion).
    let mut reaped = false;
    for _ in 0..2000 {
        if store.blob_tasks() == 0 {
            reaped = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(reaped, "the finished step was never reaped");
    // The step created its file after its caller gave up: nothing names it.
    runtime.block_on(async {
        assert_eq!(client.sweep_blobs().await.unwrap(), 1);
    });
    assert!(blob_files(state).is_empty());
}
