//! Uncertain commits at the Store/Core boundary, injected by a closed fault
//! backend over a real Store.

use std::{fs, os::unix::fs::PermissionsExt, time::Instant};

use serde_json::{Value, json};
use via_store::{
    EventRecord, RawStream, SpawnRecord, Store, StoreClient, StoreError, StoredEvent,
    SubmissionRecord, TerminalRecord,
};

use super::{FAILED_TURNS_LIMIT, TurnJournal, Unresolved, commit_event, read_result};
use crate::api::{Event, EventBody, FailureClass};
use crate::engine::{Engine, Started, Terminal, TurnRecord, failure};
use crate::{
    ApiError, ConnectionId, FakeConfig, RawRef, SessionId, SpawnParams, TurnNumber, TurnState,
};

const SESSION: &str = "s_0123456789ab";
const CONNECTION: &str = "c_0123456789ab";
const AT: &str = "2026-01-01T00:00:00Z";

/// The one fixed outcome each injected event commit has.
#[derive(Clone, Copy)]
enum EventFault {
    /// SQLite made the event durable but reported failure while committing.
    CommittedThenUncertain,
    /// The commit reported an uncertain outcome and left nothing durable.
    UncertainNotCommitted,
}

/// Closed fault backend: a real Store whose event commits take one fixed fault.
struct FaultJournal {
    store: StoreClient,
    event: EventFault,
    /// Reads of the durable event head fail, so nothing can be settled.
    head_unreadable: bool,
}

fn injected() -> StoreError {
    StoreError::Uncertain("injected fault".to_owned())
}

impl TurnJournal for FaultJournal {
    async fn commit_event(&self, record: EventRecord) -> Result<(), StoreError> {
        if matches!(self.event, EventFault::CommittedThenUncertain) {
            self.store.commit_event(record).await?;
        }
        Err(injected())
    }

    async fn commit_terminal(
        &self,
        record: TerminalRecord,
        closed: Option<Value>,
    ) -> Result<(), StoreError> {
        TurnJournal::commit_terminal(&self.store, record, closed).await
    }

    async fn events(
        &self,
        session: &SessionId,
        from_seq: u64,
        limit: u32,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        if self.head_unreadable {
            return Err(StoreError::Unavailable);
        }
        self.store.events(session, from_seq, limit).await
    }

    async fn result(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<Value>, StoreError> {
        self.store.result(session, turn).await
    }
}

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
}

fn turn() -> TurnNumber {
    TurnNumber::try_from(1).unwrap()
}

fn event(seq: u64, body: EventBody) -> Value {
    Event {
        seq,
        session_id: &session(),
        turn: Some(1),
        late: false,
        at: AT,
        raw_ref: None,
        body,
    }
    .to_value()
    .unwrap()
}

/// A receipted turn after `turn.submitted` (seq 2), plus one synced raw unit.
async fn running_turn(root: &tempfile::TempDir) -> (Store, RawRef) {
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    client
        .commit_spawn(SpawnRecord {
            session_id: session(),
            handle_hash: [7; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            prompt: "hello".to_owned(),
            initial_event: event(1, EventBody::TurnQueued { queue_position: 0 }),
        })
        .await
        .unwrap();
    client
        .commit_submission(SubmissionRecord {
            session_id: session(),
            turn: turn(),
            event: event(2, EventBody::TurnSubmitted { attempt: 1 }),
        })
        .await
        .unwrap();
    let (raw, _journal) = store.runtime_resources().into_wire_parts();
    let durable = raw
        .open(ConnectionId::try_from(CONNECTION).unwrap())
        .append(RawStream::Stdout, b"{\"type\":\"text\"}\n".to_vec())
        .await
        .unwrap();
    let raw_ref = durable.raw_ref().clone();
    (store, raw_ref)
}

fn started() -> Started {
    Started {
        session: session(),
        turn: turn(),
        queued_at: AT.to_owned(),
        submitted_at: AT.to_owned(),
        submitted_clock: Instant::now(),
    }
}

fn record() -> TurnRecord {
    TurnRecord {
        session: session(),
        turn: turn(),
        seq: 2,
        accepted: None,
        spans: Vec::new(),
        store_failed: false,
        uncertain: None,
    }
}

/// The disposition `drive` gives a turn whose event could not be recorded.
fn store_failure() -> Terminal {
    Terminal {
        state: "failed",
        failure: Some(failure(
            FailureClass::Store,
            "a turn event could not be recorded".to_owned(),
            None,
        )),
        stop_reason: "error",
        vendor_stop_reason: None,
        final_text: String::new(),
        exit: None,
        raw_ref: None,
        raw_incomplete: false,
        warnings: Vec::new(),
        cancel: None,
    }
}

/// Commits one observation through `journal`, then finishes the turn as `drive` does.
async fn observe_then_finish(
    journal: &FaultJournal,
    unresolved: &Unresolved,
    raw_ref: &RawRef,
) -> Result<(), crate::ApiError> {
    let mut record = record();
    let body = EventBody::AssistantText {
        text: "hi".to_owned(),
        is_final: false,
    };
    commit_event(journal, &mut record, body, Some(raw_ref.clone())).await;
    assert!(record.store_failed, "the injected fault reached Core");
    Engine::finish_turn(
        journal,
        unresolved,
        &started(),
        record,
        store_failure(),
        false,
    )
    .await
}

fn event_types(events: &[StoredEvent]) -> Vec<(u64, String)> {
    events
        .iter()
        .map(|event| (event.seq, event.event["type"].as_str().unwrap().to_owned()))
        .collect()
}

#[tokio::test]
async fn committed_uncertain_observation_is_settled_before_turn_ended() {
    let root = tempfile::tempdir().unwrap();
    let (store, raw_ref) = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::CommittedThenUncertain,
        head_unreadable: false,
    };
    let unresolved = Unresolved::default();
    observe_then_finish(&journal, &unresolved, &raw_ref)
        .await
        .unwrap();
    let envelope = store.client().result(&session(), turn()).await.unwrap();
    let envelope = envelope.expect("the receipted turn reached a durable terminal");
    assert_eq!(envelope["state"], "failed");
    assert_eq!(envelope["failure"]["class"], "store");
    assert_eq!(envelope["events"]["last_seq"], 4);
    // The durable observation's raw span stays inside the envelope's bounds.
    assert_eq!(envelope["raw_spans"][0]["connection_id"], CONNECTION);
    assert_eq!(envelope["raw_spans"][0]["first_offset"], raw_ref.offset());
    assert_eq!(
        envelope["raw_spans"][0]["last_offset"],
        raw_ref.end_offset()
    );
    let events = store.client().events(&session(), 1, 10).await.unwrap();
    assert_eq!(
        event_types(&events),
        [
            (1, "turn.queued".to_owned()),
            (2, "turn.submitted".to_owned()),
            (3, "assistant.text".to_owned()),
            (4, "turn.ended".to_owned()),
        ]
    );
}

#[tokio::test]
async fn uncommitted_uncertain_observation_keeps_the_sequence() {
    let root = tempfile::tempdir().unwrap();
    let (store, raw_ref) = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::UncertainNotCommitted,
        head_unreadable: false,
    };
    let unresolved = Unresolved::default();
    observe_then_finish(&journal, &unresolved, &raw_ref)
        .await
        .unwrap();
    let envelope = store.client().result(&session(), turn()).await.unwrap();
    let envelope = envelope.expect("the receipted turn reached a durable terminal");
    assert_eq!(envelope["events"]["last_seq"], 3);
    assert_eq!(envelope["raw_spans"], json!([]));
    let events = store.client().events(&session(), 1, 10).await.unwrap();
    assert_eq!(event_types(&events)[2], (3, "turn.ended".to_owned()));
}

#[tokio::test]
async fn unsettled_turn_reads_as_store_error_not_running() {
    let root = tempfile::tempdir().unwrap();
    let (store, raw_ref) = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::CommittedThenUncertain,
        head_unreadable: true,
    };
    let unresolved = Unresolved::default();
    let finished = observe_then_finish(&journal, &unresolved, &raw_ref).await;
    assert_eq!(finished.unwrap_err().kind, "store_error");
    // No terminal was invented, and `result`/`wait` report `store_error`.
    assert!(
        store
            .client()
            .result(&session(), turn())
            .await
            .unwrap()
            .is_none()
    );
    let read = read_result(&store.client(), &unresolved, &session(), turn()).await;
    assert_eq!(
        read.unwrap_err().data(),
        unpersisted_data(SESSION, "running")
    );
}

/// C1 §3.8/§9 `error.data` for a receipted turn whose terminal is not durable.
fn unpersisted_data(session: &str, durable_state: &str) -> Value {
    json!({"kind":"store_error","session":session,"turn":1,
        "durable_state":durable_state,"terminal_persisted":false})
}

/// A daemon Engine over a fresh private state and runtime directory.
fn engine(root: &tempfile::TempDir) -> Engine {
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let [state, runtime, _anchors] = ["state", "runtime", "runtime/anchors"].map(|part| {
        let path = root.path().join(part);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    });
    let fake = FakeConfig::from_environment().unwrap();
    Engine::open(&state, &runtime, fake, root.path().join("via")).unwrap()
}

/// Commits the receipt and `turn.submitted` of `session` through `store`.
async fn receipted(store: &StoreClient, session: &SessionId) {
    let event = |seq, body| {
        Event {
            seq,
            session_id: session,
            turn: Some(1),
            late: false,
            at: AT,
            raw_ref: None,
            body,
        }
        .to_value()
        .unwrap()
    };
    store
        .commit_spawn(SpawnRecord {
            session_id: session.clone(),
            handle_hash: [7; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            prompt: "hello".to_owned(),
            initial_event: event(1, EventBody::TurnQueued { queue_position: 0 }),
        })
        .await
        .unwrap();
    store
        .commit_submission(SubmissionRecord {
            session_id: session.clone(),
            turn: turn(),
            event: event(2, EventBody::TurnSubmitted { attempt: 1 }),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn result_and_wait_report_the_unpersisted_turn_with_c1_data() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    receipted(&engine.store, &session()).await;
    // The terminal commit fails and its read-back cannot settle it.
    let journal = FaultJournal {
        store: engine.store.clone(),
        event: EventFault::CommittedThenUncertain,
        head_unreadable: true,
    };
    let mut record = record();
    commit_event(&journal, &mut record, EventBody::CancelRequested {}, None).await;
    let finished = Engine::finish_turn(
        &journal,
        &engine.unresolved,
        &started(),
        record,
        store_failure(),
        false,
    )
    .await;
    assert_eq!(finished.unwrap_err().kind, "store_error");
    let address = format!("{SESSION}/1");
    for read in [engine.result(&address).await, engine.wait(&address).await] {
        let error = read.unwrap_err();
        assert_eq!(
            (error.code, error.message),
            (-32018, "durable storage failed")
        );
        assert_eq!(error.data(), unpersisted_data(SESSION, "running"));
    }
}

#[tokio::test]
async fn a_turn_whose_submission_cannot_commit_reports_its_queued_state() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    // No durable `turn.queued` to submit after: `submit` fails before any vendor I/O.
    engine
        .active
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let driven = engine.drive(SESSION, "hello".to_owned()).await;
    assert_eq!(driven.unwrap_err().kind, "store_error");
    let error = engine.result(&format!("{SESSION}/1")).await.unwrap_err();
    assert_eq!(error.data(), unpersisted_data(SESSION, "queued"));
}

#[tokio::test]
async fn failed_turns_are_bounded_and_each_keeps_store_error() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    let sessions: Vec<SessionId> = (0..FAILED_TURNS_LIMIT)
        .map(|n| SessionId::try_from(format!("s_{n:012}").as_str()).unwrap())
        .collect();
    for session in &sessions {
        engine.unresolved.receipt(session, turn());
        engine.unresolved.fail(session, turn(), TurnState::Running);
    }
    // At the bound a new spawn is refused before any receipt, so the set stops growing.
    let spawned = engine
        .spawn(SpawnParams {
            harness: "fake".to_owned(),
            model: "fake".to_owned(),
            prompt: "hello".to_owned(),
            handle: format!("h_{}", "A".repeat(43)),
        })
        .await;
    let refused = spawned.unwrap_err();
    assert_eq!(
        (refused.kind, refused.unpersisted.is_none()),
        ("store_error", true)
    );
    assert_eq!(engine.unresolved.turns().len(), FAILED_TURNS_LIMIT);
    for session in [&sessions[0], &sessions[FAILED_TURNS_LIMIT - 1]] {
        let read = engine.result(&format!("{}/1", session.as_str())).await;
        assert_eq!(
            read.unwrap_err().data(),
            unpersisted_data(session.as_str(), "running")
        );
    }
}

#[tokio::test]
async fn a_failed_turn_whose_terminal_becomes_readable_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let (store, _raw_ref) = running_turn(&root).await;
    let unresolved = Unresolved::default();
    unresolved.receipt(&session(), turn());
    unresolved.fail(&session(), turn(), TurnState::Running);
    assert!(
        read_result(&store.client(), &unresolved, &session(), turn())
            .await
            .is_err()
    );
    // The terminal Core could not confirm turns out durable.
    Engine::commit_turn_ended(
        &store.client(),
        &started(),
        record(),
        store_failure(),
        false,
    )
    .await
    .unwrap();
    let read = read_result(&store.client(), &unresolved, &session(), turn()).await;
    assert_eq!(read.unwrap().unwrap()["state"], "failed");
    assert!(
        unresolved.turns().is_empty(),
        "the settled turn is forgotten"
    );
    assert!(unresolved.admits());
}

#[test]
fn only_failed_turns_count_toward_the_bound() {
    let unresolved = Unresolved::default();
    for n in 0..FAILED_TURNS_LIMIT {
        let session = SessionId::try_from(format!("s_{n:012}").as_str()).unwrap();
        unresolved.receipt(&session, turn());
        assert!(
            unresolved.admits(),
            "pending turn {n} is in flight, not failed"
        );
        unresolved.fail(&session, turn(), TurnState::Running);
    }
    assert!(!unresolved.admits());
    let first = SessionId::try_from("s_000000000000").unwrap();
    unresolved.resolve(&first, turn());
    assert!(unresolved.admits());
}

#[test]
fn unpersisted_error_is_c1_store_error() {
    let error = ApiError::unpersisted(&session(), turn(), TurnState::Running);
    assert_eq!((error.code, error.kind), (-32018, "store_error"));
    assert_eq!(ApiError::STORE.data(), json!({"kind":"store_error"}));
}
