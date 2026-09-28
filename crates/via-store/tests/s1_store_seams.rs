//! Task 3 S1 F12 seams in Store (design §10): each fails inside the
//! transaction before `COMMIT`, or at the named worker step. Each test is its
//! own process under nextest, so the process-wide controller is private to it.
#![cfg(feature = "test-failpoints")]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    AnchorIntent, CommitOutcome, ConnectionId, EventRecord, RawStream, ResumeRecord, SessionId,
    SpawnRecord, Store, StoreClient, StoreError, SubmissionRecord, TerminalRecord, TurnNumber,
    failpoint,
};

const TOKEN: &str = "s1-store-seams-token-0123";
const SESSION: &str = "s_7f3k9q2mzr4c";

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
}

fn turn() -> TurnNumber {
    TurnNumber::try_from(1).unwrap()
}

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

/// A private Store plus an active failpoint controller in its own directory.
struct Seams {
    state: TempDir,
    points: TempDir,
    runtime: tokio::runtime::Runtime,
}

impl Seams {
    fn new() -> Self {
        let points = private_dir();
        failpoint::activate(points.path(), TOKEN).unwrap();
        Self {
            state: private_dir(),
            points,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        }
    }

    fn arm(&self, point: &str, occurrence: u64, action: &str, persist: bool) {
        let command =
            json!({"token":TOKEN,"occurrence":occurrence,"action":action,"persist":persist});
        fs::write(
            self.points.path().join(format!("{point}.json")),
            command.to_string(),
        )
        .unwrap();
    }

    fn disarm(&self, point: &str) {
        fs::remove_file(self.points.path().join(format!("{point}.json"))).unwrap();
    }

    fn acked(&self, point: &str, occurrence: u64) -> bool {
        self.points
            .path()
            .join(format!("{point}.{occurrence}.ack"))
            .exists()
    }

    fn release(&self, point: &str, occurrence: u64) {
        fs::write(
            self.points
                .path()
                .join(format!("{point}.{occurrence}.release")),
            b"",
        )
        .unwrap();
    }

    fn state(&self) -> &Path {
        self.state.path()
    }
}

fn event(kind: &str, seq: u64) -> Value {
    json!({"type":kind,"seq":seq,"at":"2026-01-01T00:00:00.000Z","raw_ref":null})
}

async fn running_turn(client: &StoreClient) {
    client
        .commit_spawn(SpawnRecord {
            session_id: session(),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            prompt: "p".to_owned(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: event("turn.queued", 1),
        })
        .await
        .unwrap();
    client
        .commit_submission(SubmissionRecord {
            session_id: session(),
            turn: turn(),
            event: event("turn.submitted", 2),
        })
        .await
        .unwrap();
}

fn text(seq: u64) -> EventRecord {
    EventRecord {
        session_id: session(),
        turn: turn(),
        event: event("assistant.text", seq),
        raw_ref: None,
    }
}

/// Design §7.1 and §10: `fail_io` at `store.commit.event` fails inside the
/// transaction, so SQLite rolls it back: not committed (`Write`), no row, and
/// the session's next event reuses the sequence number.
#[test]
fn commit_seam_rolls_back_and_the_sequence_is_reused() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    seams.runtime.block_on(async {
        running_turn(&client).await;
        seams.arm("store.commit.event", 1, "fail_io", false);
        let failed = client.commit_event(text(3)).await;
        assert!(matches!(failed, Err(StoreError::Write(_))), "{failed:?}");
        assert!(seams.acked("store.commit.event", 1));
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(3));
        client.commit_event(text(3)).await.unwrap();
        assert_eq!(client.events(&session(), 1, 10).await.unwrap().len(), 3);
    });
}

/// Design §10: a persistent `fail_io` fails every hit from the armed
/// occurrence on; `store.commit.fail_persistent` covers every commit point.
#[test]
fn persistent_fail_io_fails_every_later_commit() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    seams.runtime.block_on(async {
        running_turn(&client).await;
        // Hits 1 and 2 were the spawn and the submission.
        seams.arm("store.commit.fail_persistent", 3, "fail_io", true);
        for _ in 0..3 {
            assert!(matches!(
                client.commit_event(text(3)).await,
                Err(StoreError::Write(_))
            ));
        }
        let terminal = client
            .commit_terminal(TerminalRecord {
                session_id: session(),
                turn: turn(),
                envelope: json!({"state":"failed"}),
                event: event("turn.ended", 3),
                raw_ref: None,
            })
            .await;
        assert!(
            matches!(terminal, Err(StoreError::Write(_))),
            "{terminal:?}"
        );
        let intent = journal
            .commit_anchor_intent(AnchorIntent {
                anchor_id: "a1".to_owned(),
                generation: "g".to_owned(),
                marker: "m".to_owned(),
                socket_path: "/private/a.sock".into(),
                owner_session: session(),
                owner_turn: turn(),
                uid: 1000,
                boot_id: "boot".to_owned(),
                pid_namespace: "pid:[1]".to_owned(),
            })
            .await;
        assert!(matches!(intent, CommitOutcome::NotCommitted(_)));
        seams.disarm("store.commit.fail_persistent");
        client.commit_event(text(3)).await.unwrap();
    });
}

/// Design §10 [r3.6]: `store.commit.rider` fails the combined
/// cancellation-plus-`session.closed` transaction after the terminal insert:
/// neither commits.
#[test]
fn rider_seam_rolls_back_the_cancellation_and_the_close() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    seams.runtime.block_on(async {
        client
            .commit_spawn(SpawnRecord {
                session_id: session(),
                handle_hash: [7_u8; 32],
                receipt: json!({"state":"queued"}),
                params: json!({"harness":"fake"}),
                prompt: "p".to_owned(),
                effective: json!({"deadlines":{"wall_ms":1}}),
                initial_event: event("turn.queued", 1),
            })
            .await
            .unwrap();
        seams.arm("store.commit.rider", 1, "fail_io", false);
        let cancelled = || TerminalRecord {
            session_id: session(),
            turn: turn(),
            envelope: json!({"state":"cancelled"}),
            event: event("turn.ended", 2),
            raw_ref: None,
        };
        let failed = client
            .commit_closing_terminal(cancelled(), event("session.closed", 3))
            .await;
        assert!(matches!(failed, Err(StoreError::Write(_))), "{failed:?}");
        assert!(client.result(&session(), turn()).await.unwrap().is_none());
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(2));
        assert!(
            client
                .commit_closing_terminal(cancelled(), event("session.closed", 3))
                .await
                .unwrap()
        );
    });
}

/// S1 round-1 decision 3: `store.commit.rider` fires only on the branch
/// that writes `session.closed`. Another queued turn prevents the rider, so
/// the seam is never reached and the cancellation commits alone.
#[test]
fn rider_seam_is_not_reached_when_another_turn_prevents_the_close() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    seams.runtime.block_on(async {
        client
            .commit_spawn(SpawnRecord {
                session_id: session(),
                handle_hash: [7_u8; 32],
                receipt: json!({"state":"queued"}),
                params: json!({"harness":"fake"}),
                prompt: "p".to_owned(),
                effective: json!({"deadlines":{"wall_ms":1}}),
                initial_event: event("turn.queued", 1),
            })
            .await
            .unwrap();
        client
            .commit_resume(ResumeRecord {
                session_id: session(),
                turn: TurnNumber::try_from(2).unwrap(),
                prompt: "p".to_owned(),
                effective: json!({"deadlines":{"wall_ms":1}}),
                event: event("turn.queued", 2),
                operation: None,
            })
            .await
            .unwrap();
        seams.arm("store.commit.rider", 1, "fail_io", false);
        let closed = client
            .commit_closing_terminal(
                TerminalRecord {
                    session_id: session(),
                    turn: turn(),
                    envelope: json!({"state":"cancelled"}),
                    event: event("turn.ended", 3),
                    raw_ref: None,
                },
                event("session.closed", 4),
            )
            .await;
        assert!(matches!(closed, Ok(false)), "{closed:?}");
        assert!(!seams.acked("store.commit.rider", 1));
        assert!(client.result(&session(), turn()).await.unwrap().is_some());
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(4));
    });
}

/// Design §7.1 [r4.5]: a request that never reached the writer is
/// `NotEnqueued` (not committed); a writer that drops the reply is
/// `WriterLost` (uncertain); SQLite corruption on a read is `Corrupt`.
#[test]
fn request_seams_report_the_split_errors() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    seams.runtime.block_on(async {
        running_turn(&client).await;
        // Every request counts: hits 1 and 2 were the spawn and the submission.
        seams.arm("store.request.not_enqueued", 3, "fail_io", false);
        let refused = client.commit_event(text(3)).await;
        assert!(
            matches!(refused, Err(StoreError::NotEnqueued)),
            "{refused:?}"
        );
        seams.arm("store.request.not_enqueued", 4, "fail_io", false);
        assert!(matches!(
            journal.commit_vendor_facts("a1", "g", 4242).await,
            CommitOutcome::NotCommitted(_)
        ));

        // The writer saw only the spawn and the submission so far.
        seams.arm("store.writer.lost", 3, "fail_io", false);
        let lost = client.commit_event(text(3)).await;
        assert!(matches!(lost, Err(StoreError::WriterLost)), "{lost:?}");
        seams.arm("store.writer.lost", 4, "fail_io", false);
        assert!(matches!(
            journal.commit_vendor_facts("a1", "g", 4242).await,
            CommitOutcome::Uncertain(_)
        ));

        seams.arm("store.sqlite.corrupt", 1, "fail_io", false);
        let corrupt = client.result(&session(), turn()).await;
        assert!(
            matches!(corrupt, Err(StoreError::Corrupt(_))),
            "{corrupt:?}"
        );
        assert!(client.result(&session(), turn()).await.is_ok());
    });
}

/// Design §7.3 [r3.8]: `store.read.queued_turn` fails only the queued-row
/// read; `store.read.dispatch` fails the dispatcher's head reads.
#[test]
fn read_seams_fail_only_their_reads() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    seams.runtime.block_on(async {
        client
            .commit_spawn(SpawnRecord {
                session_id: session(),
                handle_hash: [7_u8; 32],
                receipt: json!({"state":"queued"}),
                params: json!({"harness":"fake"}),
                prompt: "p".to_owned(),
                effective: json!({"deadlines":{"wall_ms":1}}),
                initial_event: event("turn.queued", 1),
            })
            .await
            .unwrap();
        seams.arm("store.read.queued_turn", 1, "fail_io", true);
        assert!(client.predecessors(&session(), turn()).await.is_ok());
        assert!(client.queued_turn(&session(), turn()).await.is_err());
        assert!(client.predecessors(&session(), turn()).await.is_ok());
        assert!(client.queued_turn(&session(), turn()).await.is_err());
        seams.disarm("store.read.queued_turn");
        assert!(
            client
                .queued_turn(&session(), turn())
                .await
                .unwrap()
                .is_some()
        );
        // Five head reads so far; the sixth and seventh are predecessors.
        seams.arm("store.read.dispatch", 6, "fail_io", false);
        assert!(client.predecessors(&session(), turn()).await.is_err());
        assert!(client.predecessors(&session(), turn()).await.is_ok());
        assert!(client.result(&session(), turn()).await.is_ok());
    });
}

/// Design §6.7: `store.read.stall` holds the worker after it dequeued a
/// read; a later request waits behind it until release.
#[test]
fn read_stall_holds_the_worker_until_release() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    seams.runtime.block_on(async {
        seams.arm("store.read.stall", 1, "pause", false);
        let stalled = tokio::spawn({
            let client = client.clone();
            async move { client.result(&session(), turn()).await }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !seams.acked("store.read.stall", 1) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let behind = tokio::spawn({
            let client = client.clone();
            async move { client.next_seq(&session()).await }
        });
        tokio::task::yield_now().await;
        assert!(!stalled.is_finished() && !behind.is_finished());
        seams.release("store.read.stall", 1);
        assert!(stalled.await.unwrap().is_ok());
        assert!(behind.await.unwrap().is_ok());
    });
}

/// Design §7.1 [r4.5] and runtime §11: an injected raw append or sync
/// failure is a raw I/O error (`Raw`, §7.2 row 6), persistent from the armed
/// occurrence for `raw.sync.fail_persistent`.
#[test]
fn raw_seams_fail_as_raw_io() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let raw = store.runtime_resources().into_wire_parts().0;
    seams.runtime.block_on(async {
        let first = raw.open(ConnectionId::try_from("c_one").unwrap());
        seams.arm("raw.append.fail", 1, "fail_io", false);
        let failed = first.append(RawStream::Stdout, b"x\n".to_vec()).await;
        assert!(matches!(failed, Err(StoreError::Raw(_))), "{failed:?}");
        let second = raw.open(ConnectionId::try_from("c_two").unwrap());
        second
            .append(RawStream::Stdout, b"ok\n".to_vec())
            .await
            .unwrap();
        seams.arm("raw.sync.fail_persistent", 2, "fail_io", true);
        for connection in ["c_three", "c_four"] {
            let failed = raw
                .open(ConnectionId::try_from(connection).unwrap())
                .append(RawStream::Stdout, b"y\n".to_vec())
                .await;
            assert!(matches!(failed, Err(StoreError::Raw(_))), "{failed:?}");
        }
    });
}
