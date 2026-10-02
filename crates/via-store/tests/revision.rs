//! via-jm4.35 (C1 §7.6 late row, runtime §6): the guarded revision batch.
//! Only an `unknown` turn whose envelope names no vendor stop reason (its
//! end retained no terminal) is revised, from the revision its envelope
//! holds, while its session is not closed; the new envelope, its state and
//! `turn.revised` commit together. `revisable` reads only such a turn, and
//! `status` reports each terminal turn's revision.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    AcceptanceRecord, CancelCause, ClosedOutcome, ClosedRecord, ClosingRecord, RevisionRecord,
    SessionId, SpawnRecord, StatusTurn, Store, StoreClient, StoreError, SubmissionRecord,
    TerminalExtras, TerminalRecord, TurnNumber,
};

const UNKNOWN: &str = "s_7f3k9q2mzr4c";
const RETAINED: &str = "s_8f3k9q2mzr4c";
const COMPLETED: &str = "s_9f3k9q2mzr4c";

fn id(session: &str) -> SessionId {
    SessionId::try_from(session).unwrap()
}

fn first() -> TurnNumber {
    TurnNumber::try_from(1).unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn event(kind: &str, seq: u64, late: bool) -> Value {
    json!({"type":kind,"seq":seq,"turn":1,"late":late,"at":"2026-01-01T00:00:00.000Z"})
}

/// Session `session`'s turn 1, accepted and ended with `envelope` at
/// sequence 4.
async fn ended(client: &StoreClient, session: &str, envelope: Value) {
    ended_with(client, session, envelope, (true, None)).await;
}

/// [`ended`], accepted only with `accepted`, its terminal recording
/// `cause`; an unaccepted turn ends at sequence 3.
async fn ended_with(
    client: &StoreClient,
    session: &str,
    envelope: Value,
    (accepted, cause): (bool, Option<CancelCause>),
) {
    client
        .commit_spawn(SpawnRecord {
            session_id: id(session),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            label: None,
            prompt: "p".into(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: event("turn.queued", 1, false),
        })
        .await
        .unwrap();
    client
        .commit_submission(SubmissionRecord {
            session_id: id(session),
            turn: first(),
            event: event("turn.submitted", 2, false),
        })
        .await
        .unwrap();
    if accepted {
        client
            .commit_acceptance(AcceptanceRecord {
                session_id: id(session),
                turn: first(),
                correlation: "v:fake-turn-1".to_owned(),
                event: event("turn.started", 3, false),
                adapter_version: None,
                instance: None,
            })
            .await
            .unwrap();
    }
    client
        .commit_terminal_with(
            TerminalRecord {
                session_id: id(session),
                turn: first(),
                envelope,
                event: event("turn.ended", 3 + u64::from(accepted), false),
                steps: Vec::new(),
            },
            TerminalExtras {
                cancel_cause: cause,
            },
        )
        .await
        .unwrap();
}

/// The revision of session `session`'s turn 1 to `state` as revision 1.
fn revised_to(session: &str, state: &str, seq: u64) -> RevisionRecord {
    RevisionRecord {
        session_id: id(session),
        turn: first(),
        envelope: json!({"state":state,"revision":1,"vendor_stop_reason":"stop"}),
        event: event("turn.revised", seq, true),
    }
}

/// Closes session `session`, whose next sequence is `seq`, and returns its
/// close result's `cancelled_turns`.
async fn closed_turns(client: &StoreClient, session: &str, seq: u64) -> Value {
    client
        .commit_closing(ClosingRecord {
            session_id: id(session),
            operation: None,
        })
        .await
        .unwrap();
    let outcome = client
        .commit_closed(ClosedRecord {
            session_id: id(session),
            event: json!({"type":"session.closed","seq":seq,"turn":null,"late":false,
                          "at":"2026-01-01T00:00:00.000Z","reason":"close","leftovers":null}),
            operation: None,
        })
        .await
        .unwrap();
    let ClosedOutcome::Closed(result) = outcome else {
        panic!("the close did not commit: {outcome:?}");
    };
    result["cancelled_turns"].clone()
}

/// The revision of session `session`'s turn 1 to `completed` as
/// `revision`, at sequence `seq`.
fn revision(session: &str, revision: u32, seq: u64) -> RevisionRecord {
    RevisionRecord {
        session_id: id(session),
        turn: first(),
        envelope: json!({"state":"completed","revision":revision,"vendor_stop_reason":"end_turn"}),
        event: event("turn.revised", seq, true),
    }
}

fn unknown(vendor_stop_reason: Option<&str>) -> Value {
    json!({"state":"unknown","revision":0,"vendor_stop_reason":vendor_stop_reason})
}

#[test]
fn only_an_unknown_turn_with_no_retained_terminal_is_revised_once() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        ended(&client, UNKNOWN, unknown(None)).await;
        ended(&client, RETAINED, unknown(Some("end_turn"))).await;
        ended(
            &client,
            COMPLETED,
            json!({"state":"completed","revision":0,"vendor_stop_reason":"end_turn"}),
        )
        .await;

        let revisable = client.revisable(&id(UNKNOWN), first()).await.unwrap();
        assert_eq!(revisable.unwrap().envelope, unknown(None));
        for session in [RETAINED, COMPLETED] {
            assert!(
                client
                    .revisable(&id(session), first())
                    .await
                    .unwrap()
                    .is_none()
            );
            let refused = client.commit_revision(revision(session, 1, 5)).await;
            assert!(
                matches!(refused, Err(StoreError::Refused(_))),
                "{refused:?}"
            );
        }
        // A revision must follow the one the envelope holds.
        let skipped = client.commit_revision(revision(UNKNOWN, 2, 5)).await;
        assert!(
            matches!(skipped, Err(StoreError::Refused(_))),
            "{skipped:?}"
        );

        client
            .commit_revision(revision(UNKNOWN, 1, 5))
            .await
            .unwrap();
        let envelope: Value = serde_json::from_str(
            client
                .result_text(&id(UNKNOWN), first())
                .await
                .unwrap()
                .unwrap()
                .get(),
        )
        .unwrap();
        assert_eq!(envelope["revision"], 1);
        let facts = client
            .terminal_facts(&id(UNKNOWN), first())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(facts.state, "completed");
        let events = client.events(&id(UNKNOWN), 1, 10).await.unwrap();
        assert_eq!(events.len(), 5);
        assert_eq!(events[4].event["type"], "turn.revised");
        // Revised once: the turn is no longer `unknown`.
        let again = client.commit_revision(revision(UNKNOWN, 2, 6)).await;
        assert!(matches!(again, Err(StoreError::Refused(_))), "{again:?}");
        assert!(
            client
                .revisable(&id(UNKNOWN), first())
                .await
                .unwrap()
                .is_none()
        );

        for (session, state, revision) in [
            (UNKNOWN, "completed", 1),
            (RETAINED, "unknown", 0),
            (COMPLETED, "completed", 0),
        ] {
            let status = client
                .session_status(&id(session), None, 0, 10)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                status.turns,
                [StatusTurn {
                    number: 1,
                    state: state.to_owned(),
                    revision,
                }]
            );
        }
    });
}

#[test]
fn a_closed_session_refuses_a_revision() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        ended(&client, UNKNOWN, unknown(None)).await;
        client
            .commit_closing(ClosingRecord {
                session_id: id(UNKNOWN),
                operation: None,
            })
            .await
            .unwrap();
        let closed = client
            .commit_session_closed(
                &id(UNKNOWN),
                json!({"type":"session.closed","seq":5,"turn":null,"late":false,
                       "at":"2026-01-01T00:00:00.000Z","reason":"closed","leftovers":null}),
            )
            .await
            .unwrap();
        assert!(closed);
        let refused = client.commit_revision(revision(UNKNOWN, 1, 6)).await;
        assert!(
            matches!(refused, Err(StoreError::Refused(_))),
            "{refused:?}"
        );
        let facts = client
            .terminal_facts(&id(UNKNOWN), first())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(facts.state, "unknown");
    });
}

/// Runtime §6 (fix round 1 #2): a revision needs the turn's acceptance
/// correlation whatever the state it revises to; an unaccepted `unknown`
/// turn is not revisable.
#[test]
fn an_unaccepted_turn_is_never_revised() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        ended_with(&client, UNKNOWN, unknown(None), (false, None)).await;
        assert!(
            client
                .revisable(&id(UNKNOWN), first())
                .await
                .unwrap()
                .is_none()
        );
        for state in ["failed", "cancelled", "completed"] {
            let refused = client.commit_revision(revised_to(UNKNOWN, state, 4)).await;
            assert!(
                matches!(refused, Err(StoreError::Refused(_))),
                "{state}: {refused:?}"
            );
        }
        let facts = client
            .terminal_facts(&id(UNKNOWN), first())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(facts.state, "unknown");
    });
}

/// Runtime §6 (fix round 1 #3): `cancel_cause` records the `close` that
/// stopped a turn ending `unknown`, `revisable` reads it and a revision
/// keeps it. A close result counts the turn only once it is `cancelled`:
/// revised to `cancelled` before the closure it is counted, unrevised it
/// is not.
#[test]
fn a_close_counts_a_close_stopped_turn_only_once_cancelled() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        let close = (true, Some(CancelCause::Close));
        ended_with(&client, UNKNOWN, unknown(None), close).await;
        ended_with(&client, RETAINED, unknown(None), close).await;
        let revisable = client
            .revisable(&id(UNKNOWN), first())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(revisable.cancel_cause, Some(CancelCause::Close));
        client
            .commit_revision(revised_to(UNKNOWN, "cancelled", 5))
            .await
            .unwrap();
        assert_eq!(
            closed_turns(&client, UNKNOWN, 6).await,
            json!([format!("{UNKNOWN}/1")])
        );
        assert_eq!(closed_turns(&client, RETAINED, 5).await, json!([]));
    });
}

/// Fix round 4 #3 (C1 §5 as amended): a revision's spill whose write or
/// sync failed keeps its file, even a whole one whose folder could not be
/// synced; a turn's first spill still leaves no partial file to name.
/// (The folder is made unreadable, so its sync cannot open it.)
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped check under root is reported"
)]
fn a_failed_revision_spill_keeps_its_file() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let folder = root.path().join("evidence").join(UNKNOWN).join("1");
    fs::create_dir_all(&folder).unwrap();
    let encoded = vec![b'7'; 40_000];
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o300)).unwrap();
    let (revision, first) = runtime().block_on(async {
        let turn = (&id(UNKNOWN), first());
        let revision = client
            .write_structured_output(turn, Some(1), encoded.clone())
            .await;
        let first = client
            .write_structured_output(turn, None, encoded.clone())
            .await;
        (revision, first)
    });
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    if revision.is_ok() {
        eprintln!("skipped: this process bypasses file permissions (root)");
        return;
    }
    assert!(first.is_err());
    let names: Vec<String> = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].starts_with("structured_output.r1-"), "{names:?}");
    assert_eq!(fs::read(folder.join(&names[0])).unwrap(), encoded);
}
