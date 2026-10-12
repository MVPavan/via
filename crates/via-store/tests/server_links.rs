//! x.3.2 X0 items 1 and 6.5 (runtime §6 `anchors`, `server_turns`): a
//! server-owned anchor, the turn → server-anchor link committed before the
//! turn's first vendor byte, its release with a quiescent terminal, and the
//! close and status cleanup predicate over remaining links. Written before
//! the schema bump.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    AnchorIdentity, AnchorIntent, ClosedOutcome, ClosedRecord, ClosingRecord, CommitOutcome,
    GroupAbsenceRecord, ProcessJournal, ProcessOwner, ServerId, SessionId, SpawnRecord, Store,
    StoreClient, SubmissionRecord, TerminalExtras, TerminalRecord, TurnNumber,
};

const SESSION: &str = "s_7f3k9q2mzr4c";
const OTHER: &str = "s_7f3k9q2mzr4d";
const SERVER: &str = "v_0123456789ab";

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
}

fn other() -> SessionId {
    SessionId::try_from(OTHER).unwrap()
}

fn server() -> ServerId {
    ServerId::try_from(SERVER).unwrap()
}

fn turn(number: u32) -> TurnNumber {
    TurnNumber::try_from(number).unwrap()
}

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn event(kind: &str, seq: u64) -> Value {
    json!({"type":kind,"seq":seq,"at":"2026-01-01T00:00:00.000Z"})
}

/// Spawns `id` with turn 1 queued at seq 1, then submits it at seq 2.
async fn running(client: &StoreClient, id: &SessionId) {
    client
        .commit_spawn(SpawnRecord {
            session_id: id.clone(),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            label: None,
            prompt: "p".into(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: event("turn.queued", 1),
        })
        .await
        .unwrap();
    client
        .commit_submission(SubmissionRecord {
            session_id: id.clone(),
            turn: turn(1),
            event: event("turn.submitted", 2),
        })
        .await
        .unwrap();
}

fn intent(anchor_id: &str, owner: ProcessOwner) -> AnchorIntent {
    AnchorIntent {
        anchor_id: anchor_id.to_owned(),
        generation: format!("g-{anchor_id}"),
        marker: "m".to_owned(),
        socket_path: PathBuf::from("/private/a.sock"),
        owner,
        uid: 1000,
        boot_id: "boot".to_owned(),
        pid_namespace: "pid:[1]".to_owned(),
    }
}

fn server_owner() -> ProcessOwner {
    ProcessOwner::Server {
        server_id: server(),
    }
}

fn identity() -> AnchorIdentity {
    AnchorIdentity {
        pid: 4242,
        pgid: 4242,
        uid: 1000,
        boot_id: "boot".to_owned(),
        pid_namespace: "pid:[1]".to_owned(),
        start_ticks: 42,
        marker: "m".to_owned(),
    }
}

async fn prove_absent(journal: &ProcessJournal, anchor_id: &str) {
    let proof = GroupAbsenceRecord {
        anchor_id: anchor_id.to_owned(),
        generation: format!("g-{anchor_id}"),
        boot_id: "boot".to_owned(),
        pid_namespace: "pid:[1]".to_owned(),
        pgid: 4242,
        observed_at: "1".to_owned(),
        identity: Some(identity()),
    };
    let CommitOutcome::Committed(()) = journal.commit_group_absence(proof).await else {
        panic!("absence not committed");
    };
}

async fn committed(journal: &ProcessJournal, intent: AnchorIntent) {
    let CommitOutcome::Committed(_) = journal.commit_anchor_intent(intent).await else {
        panic!("intent not committed");
    };
}

fn ended(id: &SessionId, seq: u64, state: &str, link_released: bool) -> TerminalRecord {
    TerminalRecord {
        session_id: id.clone(),
        turn: turn(1),
        envelope: json!({"state":state}),
        event: event("turn.ended", seq),
        steps: Vec::new(),
        link_released,
    }
}

async fn links(journal: &ProcessJournal) -> Vec<(String, u32, String)> {
    journal
        .server_links(vec![(session(), turn(1)), (other(), turn(1))])
        .await
        .unwrap()
        .into_iter()
        .map(|link| {
            (
                link.session_id.as_str().to_owned(),
                link.turn.get(),
                link.anchor_id,
            )
        })
        .collect()
}

/// Close-and-status cleanup: the status read's flag and the close result.
async fn status_uncertain(client: &StoreClient, id: &SessionId) -> bool {
    client
        .session_status(id, None, 0, 10)
        .await
        .unwrap()
        .unwrap()
        .cleanup_uncertain
}

/// X0 item 1: a server anchor has no owning turn; at most one anchor per
/// server; a link commits only for a server anchor and a `running` turn,
/// once; recovery reads the link with the unfinished turn.
#[test]
fn store_server_anchor_and_link() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    runtime().block_on(async {
        running(&client, &session()).await;
        committed(&journal, intent("srv", server_owner())).await;
        // One anchor per server.
        assert!(matches!(
            journal
                .commit_anchor_intent(intent("srv2", server_owner()))
                .await,
            CommitOutcome::NotCommitted(_)
        ));
        committed(
            &journal,
            intent(
                "own",
                ProcessOwner::Turn {
                    session_id: session(),
                    turn: turn(1),
                },
            ),
        )
        .await;

        // A link to a turn-owned anchor is refused.
        assert!(matches!(
            journal.commit_server_turn("own", &session(), turn(1)).await,
            CommitOutcome::NotCommitted(_)
        ));
        // A link for a turn that is not running is refused.
        assert!(matches!(
            journal.commit_server_turn("srv", &session(), turn(2)).await,
            CommitOutcome::NotCommitted(_)
        ));
        assert_eq!(
            journal.commit_server_turn("srv", &session(), turn(1)).await,
            CommitOutcome::Committed(())
        );
        // A second link of the same turn is refused, to any anchor.
        assert!(matches!(
            journal.commit_server_turn("srv", &session(), turn(1)).await,
            CommitOutcome::NotCommitted(_)
        ));
        assert_eq!(
            links(&journal).await,
            [(SESSION.to_owned(), 1, "srv".to_owned())]
        );

        // Recovery reads the link with the unfinished turn.
        let unfinished = client.unfinished_turns().await.unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].server_anchor.as_deref(), Some("srv"));

        // The inventory names the server owner, with no running turn; a
        // session-filtered page never returns a server anchor.
        let owners = client.anchor_owners_page(None, 10).await.unwrap();
        let srv = owners
            .iter()
            .find(|owner| owner.anchor_id == "srv")
            .unwrap();
        assert_eq!(srv.owner, server_owner());
        assert!(!srv.turn_running);
        let own = owners
            .iter()
            .find(|owner| owner.anchor_id == "own")
            .unwrap();
        assert!(own.turn_running);
        let filtered: Vec<String> = journal
            .unproven_anchor_records_page(None, 10, Some(session()))
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.intent.anchor_id)
            .collect();
        assert_eq!(filtered, ["own"]);
        let all = journal.list_anchor_records_page(None, 10).await.unwrap();
        let record = all
            .iter()
            .find(|record| record.intent.anchor_id == "srv")
            .unwrap();
        assert_eq!(record.intent.owner, server_owner());
    });
}

/// X0 item 6.5: a terminal whose cleanup is quiescent deletes the turn's
/// link in its own transaction; the session's close then reads quiescent
/// though the server group lives on for other sessions.
#[test]
fn quiescent_terminal_releases_link() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    runtime().block_on(async {
        running(&client, &session()).await;
        committed(&journal, intent("srv", server_owner())).await;
        assert_eq!(
            journal.commit_server_turn("srv", &session(), turn(1)).await,
            CommitOutcome::Committed(())
        );
        assert!(status_uncertain(&client, &session()).await);
        client
            .commit_terminal(ended(&session(), 3, "completed_unchecked", true))
            .await
            .unwrap_err();
        // The refused terminal rolled back: the link stays.
        assert_eq!(links(&journal).await.len(), 1);
        client
            .commit_terminal(ended(&session(), 3, "failed", true))
            .await
            .unwrap();
        assert!(links(&journal).await.is_empty());
        assert!(!status_uncertain(&client, &session()).await);
        client
            .commit_closing(ClosingRecord {
                session_id: session(),
                operation: None,
            })
            .await
            .unwrap();
        let ClosedOutcome::Closed(result) = client
            .commit_closed(ClosedRecord {
                session_id: session(),
                event: event("session.closed", 4),
                operation: None,
            })
            .await
            .unwrap()
        else {
            panic!("not closed");
        };
        assert_eq!(result["cleanup"], "quiescent");
    });
}

/// X0 item 6.5: a spontaneous end with uncertain cleanup (`cancel: null`)
/// keeps the link; close and status read `uncertain` until the server
/// group is proved absent, then `quiescent`. Another session's link never
/// moves this session's predicate.
#[test]
fn spontaneous_uncertain_end_keeps_link() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    runtime().block_on(async {
        running(&client, &session()).await;
        running(&client, &other()).await;
        committed(&journal, intent("srv", server_owner())).await;
        for id in [session(), other()] {
            assert_eq!(
                journal.commit_server_turn("srv", &id, turn(1)).await,
                CommitOutcome::Committed(())
            );
        }
        client
            .commit_terminal_with(
                ended(&session(), 3, "unknown", false),
                TerminalExtras::default(),
            )
            .await
            .unwrap();
        // The other session's turn ends quiescent: only its link goes.
        client
            .commit_terminal(ended(&other(), 3, "completed_unchecked", true))
            .await
            .unwrap_err();
        client
            .commit_terminal(ended(&other(), 3, "failed", true))
            .await
            .unwrap();
        assert_eq!(
            links(&journal).await,
            [(SESSION.to_owned(), 1, "srv".to_owned())]
        );
        assert!(status_uncertain(&client, &session()).await);
        assert!(!status_uncertain(&client, &other()).await);
        let status = client
            .session_status(&session(), None, 0, 10)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.unproven_anchors, ["srv"]);

        client
            .commit_closing(ClosingRecord {
                session_id: session(),
                operation: None,
            })
            .await
            .unwrap();
        let read = client.session_close_result(&session()).await.unwrap();
        assert!(read.is_none(), "{read:?}");
        prove_absent(&journal, "srv").await;
        assert!(!status_uncertain(&client, &session()).await);
        let ClosedOutcome::Closed(result) = client
            .commit_closed(ClosedRecord {
                session_id: session(),
                event: event("session.closed", 4),
                operation: None,
            })
            .await
            .unwrap()
        else {
            panic!("not closed");
        };
        assert_eq!(result["cleanup"], "quiescent");
    });
}

/// The uncertain twin of the close above: with the group unproven, the
/// close result reads `uncertain`.
#[test]
fn close_with_unproven_linked_server_is_uncertain() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    runtime().block_on(async {
        running(&client, &session()).await;
        committed(&journal, intent("srv", server_owner())).await;
        assert_eq!(
            journal.commit_server_turn("srv", &session(), turn(1)).await,
            CommitOutcome::Committed(())
        );
        client
            .commit_terminal(ended(&session(), 3, "failed", false))
            .await
            .unwrap();
        client
            .commit_closing(ClosingRecord {
                session_id: session(),
                operation: None,
            })
            .await
            .unwrap();
        let ClosedOutcome::Closed(result) = client
            .commit_closed(ClosedRecord {
                session_id: session(),
                event: event("session.closed", 4),
                operation: None,
            })
            .await
            .unwrap()
        else {
            panic!("not closed");
        };
        assert_eq!(result["cleanup"], "uncertain");
    });
}

/// X0 item 6.5: a crash injected before the terminal's commit leaves the
/// turn nonterminal and its link in place: the link is deleted only in the
/// terminal's own transaction.
#[cfg(feature = "test-failpoints")]
#[test]
fn quiescent_terminal_crash_before_commit_keeps_link() {
    const TOKEN: &str = "server-links-token-0123456";
    let points = private_dir();
    via_store::failpoint::activate(points.path(), TOKEN).unwrap();
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    runtime().block_on(async {
        running(&client, &session()).await;
        committed(&journal, intent("srv", server_owner())).await;
        assert_eq!(
            journal.commit_server_turn("srv", &session(), turn(1)).await,
            CommitOutcome::Committed(())
        );
        let command = json!({"token":TOKEN,"occurrence":1,"action":"fail_io","persist":false});
        fs::write(
            points.path().join("store.commit.terminal.json"),
            command.to_string(),
        )
        .unwrap();
        client
            .commit_terminal(ended(&session(), 3, "failed", true))
            .await
            .unwrap_err();
        assert_eq!(links(&journal).await.len(), 1);
        let unfinished = client.unfinished_turns().await.unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].server_anchor.as_deref(), Some("srv"));
    });
}
