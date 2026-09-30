//! Uncertain commits at the Store/Core boundary, injected by a closed fault
//! backend over a real Store.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use via_store::{
    BlobRef, EventRecord, QueuedTurn, ResumeRecord, SpawnRecord, Store, StoreClient, StoreError,
    StoredEvent, SubmissionRecord, TerminalFacts, TerminalRecord,
};

use super::{
    Head, TurnJournal, UNRESOLVED_LIMIT, Unresolved, admission, commit_event, read_result,
};
use crate::api::{Event, EventBody, FailureClass};
use crate::engine::drive::SubmitFailure;
use crate::engine::{Engine, Started, Terminal, TurnRecord, failure};
use crate::{ApiError, FakeConfig, SessionId, SpawnParams, TurnNumber, TurnState, WaitParams};

const SESSION: &str = "s_0123456789ab";
/// The turn's evidence folder as `Started` carries it.
const FOLDER: &str = "/state/evidence/s_0123456789ab/1";
/// Store parses `at` strictly as RFC 3339 UTC with milliseconds.
const AT: &str = "2026-01-01T00:00:00.000Z";

/// The one fixed outcome each injected event commit has.
#[derive(Clone, Copy)]
enum EventFault {
    /// SQLite made the event durable but reported failure while committing.
    CommittedThenUncertain,
    /// The commit reported an uncertain outcome and left nothing durable.
    UncertainNotCommitted,
}

/// How reads of the durable event head behave.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HeadFault {
    Readable,
    /// They fail, so nothing can be settled.
    Unreadable,
    /// Session-head reads (the next sequence) report SQLite corruption.
    Corrupt,
}

/// Closed fault backend: a real Store whose event commits take one fixed fault.
struct FaultJournal {
    store: StoreClient,
    event: EventFault,
    /// Reads of the durable event head.
    head: HeadFault,
    /// Submission commits report an uncertain outcome and leave nothing durable.
    submission_fails: bool,
    /// Result reads of every session but `SESSION` stall past the settle bound.
    delayed_results: bool,
}

fn injected() -> StoreError {
    StoreError::Uncertain("injected fault".to_owned())
}

impl TurnJournal for FaultJournal {
    async fn commit_submission(&self, record: SubmissionRecord) -> Result<(), StoreError> {
        if self.submission_fails {
            return Err(injected());
        }
        self.store.commit_submission(record).await
    }

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
    ) -> Result<bool, StoreError> {
        TurnJournal::commit_terminal(&self.store, record, closed).await
    }

    async fn commit_terminal_with(
        &self,
        record: TerminalRecord,
        extras: via_store::TerminalExtras,
    ) -> Result<(), StoreError> {
        self.store.commit_terminal_with(record, extras).await
    }

    async fn events(
        &self,
        session: &SessionId,
        from_seq: u64,
        limit: u32,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        if self.head == HeadFault::Unreadable {
            return Err(StoreError::WriterLost);
        }
        self.store.events(session, from_seq, limit).await
    }

    async fn terminated(
        &self,
        turns: Vec<(SessionId, TurnNumber)>,
    ) -> Result<Vec<(SessionId, TurnNumber)>, StoreError> {
        self.store.terminated(turns).await
    }

    async fn terminal_facts(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<TerminalFacts>, StoreError> {
        if self.delayed_results && session.as_str() != SESSION {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        self.store.terminal_facts(session, turn).await
    }

    async fn load_prompt(&self, blob: &BlobRef) -> Result<String, StoreError> {
        self.store.load_prompt(blob).await
    }

    async fn queued_turn(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<QueuedTurn>, StoreError> {
        self.store.queued_turn(session, turn).await
    }

    async fn next_seq(&self, session: &SessionId) -> Result<Option<u64>, StoreError> {
        if self.head == HeadFault::Corrupt {
            return Err(StoreError::Corrupt("injected corruption".to_owned()));
        }
        if self.head == HeadFault::Unreadable {
            return Err(StoreError::WriterLost);
        }
        self.store.next_seq(session).await
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
        body,
    }
    .to_value()
    .unwrap()
}

/// A receipted turn after `turn.submitted` (seq 2).
async fn running_turn(root: &tempfile::TempDir) -> Store {
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    client
        .commit_spawn(SpawnRecord {
            session_id: session(),
            handle_hash: [7; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            label: None,
            prompt: "hello".into(),
            effective: frozen(),
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
    store
}

/// The turn's stored envelope, parsed for inspection.
async fn stored(store: &StoreClient) -> Option<Value> {
    let text = store.result_text(&session(), turn()).await.unwrap()?;
    Some(serde_json::from_str(text.get()).unwrap())
}

fn started() -> Started {
    Started {
        session: session(),
        turn: turn(),
        queued_at: AT.to_owned(),
        first_seq: 1,
        submitted: Some((AT.to_owned(), Instant::now())),
        folder: Some(FOLDER.to_owned()),
        cwd: None,
    }
}

fn record() -> TurnRecord {
    TurnRecord {
        session: session(),
        turn: turn(),
        // `turn.queued` and `turn.submitted` are committed.
        head: Head::new(Some(3)),
        accepted: None,
        first_failure: None,
        uncertain: None,
        steps: crate::engine::progress::StepTracker::default(),
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
        final_text: Some(String::new()),
        final_text_file: None,
        exit: None,
        warnings: Vec::new(),
        cancel: None,
    }
}

/// Commits one turn event (`cancel.requested`) through `journal`, then
/// finishes the turn as `drive` does.
async fn observe_then_finish(
    journal: &FaultJournal,
    unresolved: &Unresolved,
) -> Result<(), crate::ApiError> {
    let mut record = record();
    let body = EventBody::CancelRequested {};
    commit_event(journal, &mut record, body).await;
    assert!(
        record.first_failure.is_some(),
        "the injected fault reached Core"
    );
    Engine::finish_turn(
        journal,
        unresolved,
        &started(),
        record,
        store_failure(),
        false,
    )
    .await
    .map(drop)
    .map_err(|unended| unended.error)
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
    let store = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::CommittedThenUncertain,
        head: HeadFault::Readable,
        submission_fails: false,
        delayed_results: false,
    };
    let unresolved = Unresolved::default();
    observe_then_finish(&journal, &unresolved).await.unwrap();
    let envelope = stored(&store.client()).await;
    let envelope = envelope.expect("the receipted turn reached a durable terminal");
    assert_eq!(envelope["state"], "failed");
    assert_eq!(envelope["failure"]["class"], "store");
    assert_eq!(envelope["events"]["last_seq"], 4);
    // C1 §5: the envelope names the turn's evidence folder.
    assert_eq!(
        envelope["evidence"],
        json!({"folder":FOLDER,"transcript":null})
    );
    let events = store.client().events(&session(), 1, 10).await.unwrap();
    assert_eq!(
        event_types(&events),
        [
            (1, "turn.queued".to_owned()),
            (2, "turn.submitted".to_owned()),
            (3, "cancel.requested".to_owned()),
            (4, "turn.ended".to_owned()),
        ]
    );
}

#[tokio::test]
async fn uncommitted_uncertain_observation_keeps_the_sequence() {
    let root = tempfile::tempdir().unwrap();
    let store = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::UncertainNotCommitted,
        head: HeadFault::Readable,
        submission_fails: false,
        delayed_results: false,
    };
    let unresolved = Unresolved::default();
    observe_then_finish(&journal, &unresolved).await.unwrap();
    let envelope = stored(&store.client()).await;
    let envelope = envelope.expect("the receipted turn reached a durable terminal");
    assert_eq!(envelope["events"]["last_seq"], 3);
    let events = store.client().events(&session(), 1, 10).await.unwrap();
    assert_eq!(event_types(&events)[2], (3, "turn.ended".to_owned()));
}

/// Another writer of the session, such as a `resume` committing the next turn's
/// `turn.queued`, may take the sequence an uncertain event of the running turn
/// left unused: that event is not the running turn's, and `turn.ended` follows.
#[tokio::test]
async fn an_unused_uncertain_sequence_taken_by_another_writer_is_not_the_turns() {
    let root = tempfile::tempdir().unwrap();
    let store = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::UncertainNotCommitted,
        head: HeadFault::Readable,
        submission_fails: false,
        delayed_results: false,
    };
    let mut record = record();
    let body = EventBody::CancelRequested {};
    commit_event(&journal, &mut record, body).await;
    assert!(
        record.first_failure.is_some(),
        "the injected fault reached Core"
    );
    let head = record.head.lock(&journal, &session()).await.unwrap();
    assert_eq!(head.next(), 3, "the head is re-read from the Store");
    let queued = Event {
        seq: 3,
        session_id: &session(),
        turn: Some(2),
        late: false,
        at: AT,
        body: EventBody::TurnQueued { queue_position: 0 },
    }
    .to_value()
    .unwrap();
    store
        .client()
        .commit_resume(ResumeRecord {
            session_id: session(),
            turn: TurnNumber::try_from(2).unwrap(),
            prompt: "next".into(),
            effective: frozen(),
            event: queued,
            operation: None,
        })
        .await
        .unwrap();
    head.committed(1);
    let unresolved = Unresolved::default();
    Engine::finish_turn(
        &journal,
        &unresolved,
        &started(),
        record,
        store_failure(),
        false,
    )
    .await
    .unwrap();
    let envelope = stored(&store.client()).await.unwrap();
    assert_eq!(
        envelope["events"],
        json!({"first_seq":1,"last_seq":4,"count":4})
    );
    let events = store.client().events(&session(), 1, 10).await.unwrap();
    assert_eq!(
        event_types(&events),
        [
            (1, "turn.queued".to_owned()),
            (2, "turn.submitted".to_owned()),
            (3, "turn.queued".to_owned()),
            (4, "turn.ended".to_owned()),
        ]
    );
    assert_eq!(events[3].event["turn"], 1);
}

#[tokio::test]
async fn unsettled_turn_reads_as_store_error_not_running() {
    let root = tempfile::tempdir().unwrap();
    let store = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::CommittedThenUncertain,
        head: HeadFault::Unreadable,
        submission_fails: false,
        delayed_results: false,
    };
    let unresolved = Unresolved::default();
    let finished = observe_then_finish(&journal, &unresolved).await;
    assert_eq!(finished.unwrap_err().kind, "store_error");
    // No terminal was invented, and `result`/`wait` report `store_error`.
    assert!(stored(&store.client()).await.is_none());
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

/// Commits the receipt of `session` through `store`, with `turn.submitted` if `submitted`.
async fn receipt(store: &StoreClient, session: &SessionId, submitted: bool) {
    let event = |seq, body| {
        Event {
            seq,
            session_id: session,
            turn: Some(1),
            late: false,
            at: AT,
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
            label: None,
            prompt: "hello".into(),
            effective: frozen(),
            initial_event: event(1, EventBody::TurnQueued { queue_position: 0 }),
        })
        .await
        .unwrap();
    if submitted {
        store
            .commit_submission(SubmissionRecord {
                session_id: session.clone(),
                turn: turn(),
                event: event(2, EventBody::TurnSubmitted { attempt: 1 }),
            })
            .await
            .unwrap();
    }
}

fn spawn_params() -> SpawnParams {
    serde_json::from_value(json!({"harness":"fake","model":"fake","prompt":"hello",
        "handle":format!("h_{}", "A".repeat(43))}))
    .unwrap()
}

/// A turn's frozen effective values as Core stores them.
fn frozen() -> serde_json::Value {
    json!({"model":"fake","effort":null,"bound":null,
        "deadlines":{"wall_ms":30_000,"idle_ms":600_000},"max_steps":null})
}

fn wait(address: &str) -> WaitParams {
    WaitParams {
        address: address.to_owned(),
        timeout_ms: None,
    }
}

fn numbered(n: usize) -> SessionId {
    SessionId::try_from(format!("s_{n:012}").as_str()).unwrap()
}

#[tokio::test]
async fn result_and_wait_report_the_unpersisted_turn_with_c1_data() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    receipt(&engine.store, &session(), true).await;
    // The terminal commit fails and its read-back cannot settle it.
    let journal = FaultJournal {
        store: engine.store.clone(),
        event: EventFault::CommittedThenUncertain,
        head: HeadFault::Unreadable,
        submission_fails: false,
        delayed_results: false,
    };
    let mut record = record();
    commit_event(&journal, &mut record, EventBody::CancelRequested {}).await;
    let finished = Engine::finish_turn(
        &journal,
        &engine.unresolved,
        &started(),
        record,
        store_failure(),
        false,
    )
    .await;
    assert_eq!(finished.unwrap_err().error.kind, "store_error");
    let address = format!("{SESSION}/1");
    for read in [
        engine.result(&address).await,
        engine.wait(wait(&address)).await,
    ] {
        let error = read.unwrap_err();
        assert_eq!(
            (error.code, error.message),
            (-32018, "durable storage failed")
        );
        assert_eq!(error.data(), unpersisted_data(SESSION, "running"));
    }
}

/// A submission commit whose outcome is unknown is `Failed`, which latches
/// Store failure in the dispatcher (runtime §7), and leaves the head unknown.
#[tokio::test]
async fn a_submission_commit_with_an_unknown_outcome_fails_and_unsettles_the_head() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    receipt(&engine.store, &session(), false).await;
    let journal = FaultJournal {
        store: engine.store.clone(),
        event: EventFault::UncertainNotCommitted,
        head: HeadFault::Readable,
        submission_fails: true,
        delayed_results: false,
    };
    let head = Head::new(Some(2));
    let submitted = Engine::commit_submission(&journal, &session(), turn(), &head).await;
    assert!(matches!(submitted, Err(SubmitFailure::Failed(_))));
    let reread = head.lock(&engine.store, &session()).await.unwrap();
    assert_eq!(reread.next(), 2, "the head was re-read from Store");
}

#[tokio::test]
async fn failed_turns_are_bounded_and_each_keeps_store_error() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    let sessions: Vec<SessionId> = (0..UNRESOLVED_LIMIT).map(numbered).collect();
    for session in &sessions {
        engine.unresolved.receipt(session, turn());
        engine.unresolved.fail(session, turn(), TurnState::Running);
    }
    // At the bound a new spawn is refused before any receipt, so the set stops growing.
    let refused = engine.spawn(spawn_params(), "{}").await.unwrap_err();
    assert_eq!(
        (refused.kind, refused.unpersisted.is_none()),
        ("store_error", true)
    );
    assert_eq!(engine.unresolved.turns().len(), UNRESOLVED_LIMIT);
    for session in [&sessions[0], &sessions[UNRESOLVED_LIMIT - 1]] {
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
    let store = running_turn(&root).await;
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

#[tokio::test]
async fn in_flight_turns_count_toward_the_bound() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    for n in 0..UNRESOLVED_LIMIT {
        engine.unresolved.receipt(&numbered(n), turn());
    }
    // No turn has failed, yet the set is full: capacity, not a Store failure (C1 §8.1).
    let refused = engine.spawn(spawn_params(), "{}").await.unwrap_err();
    assert_eq!((refused.code, refused.kind), (-32012, "admission_refused"));
    assert_eq!(engine.unresolved.turns().len(), UNRESOLVED_LIMIT);
    engine.unresolved.resolve(&numbered(0), turn());
    // Admitted past the bound; the fake harness is not configured in this test.
    let admitted = engine.spawn(spawn_params(), "{}").await.unwrap_err();
    assert_eq!(admitted.kind, "harness_unavailable");
}

#[tokio::test]
async fn durable_terminals_are_settled_before_admission_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    for n in 1..UNRESOLVED_LIMIT {
        engine.unresolved.receipt(&numbered(n), turn());
        engine
            .unresolved
            .fail(&numbered(n), turn(), TurnState::Running);
    }
    // A failed turn whose terminal became durable, and that nobody read.
    receipt(&engine.store, &session(), true).await;
    engine.unresolved.receipt(&session(), turn());
    engine
        .unresolved
        .fail(&session(), turn(), TurnState::Running);
    Engine::commit_turn_ended(&engine.store, &started(), record(), store_failure(), false)
        .await
        .unwrap();
    let admitted = engine.spawn(spawn_params(), "{}").await.unwrap_err();
    assert_eq!(admitted.kind, "harness_unavailable");
    let retained = engine.unresolved.turns();
    assert_eq!(retained.len(), UNRESOLVED_LIMIT - 1);
    assert!(
        !retained.contains(&(session(), turn())),
        "the durable turn is forgotten"
    );
}

#[test]
fn unpersisted_error_is_c1_store_error() {
    let error = ApiError::unpersisted(&session(), turn(), TurnState::Running);
    assert_eq!((error.code, error.kind), (-32018, "store_error"));
    assert_eq!(ApiError::STORE.data(), json!({"kind":"store_error"}));
}

#[tokio::test]
async fn a_durable_terminal_behind_delayed_reads_is_settled_within_the_bound() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root);
    for n in 1..UNRESOLVED_LIMIT {
        engine.unresolved.receipt(&numbered(n), turn());
        engine
            .unresolved
            .fail(&numbered(n), turn(), TurnState::Running);
    }
    receipt(&engine.store, &session(), true).await;
    engine.unresolved.receipt(&session(), turn());
    engine
        .unresolved
        .fail(&session(), turn(), TurnState::Running);
    Engine::commit_turn_ended(&engine.store, &started(), record(), store_failure(), false)
        .await
        .unwrap();
    // Every other failed turn's read stalls past the bound.
    let journal = FaultJournal {
        store: engine.store.clone(),
        event: EventFault::UncertainNotCommitted,
        head: HeadFault::Readable,
        submission_fails: false,
        delayed_results: true,
    };
    let started = Instant::now();
    admission(&journal, &engine.unresolved).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let retained = engine.unresolved.turns();
    assert_eq!(retained.len(), UNRESOLVED_LIMIT - 1);
    assert!(!retained.contains(&(session(), turn())));
}

/// T3-S5 round 1, decision 2 (design §7.1): SQLite corruption on the
/// session-head read before an event is the turn's first failure with the
/// outcome `Corrupt`, which the failure hook latches, not a clean
/// not-committed failure. Nothing is written. Since round 3 (decision 13)
/// the outcome is `ReadCorrupt`: Store's read reply already recorded it.
#[tokio::test]
async fn a_corrupt_head_read_before_an_event_is_a_corrupt_failure() {
    let root = tempfile::tempdir().unwrap();
    let store = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::CommittedThenUncertain,
        head: HeadFault::Corrupt,
        submission_fails: false,
        delayed_results: false,
    };
    let mut record = record();
    record.head = Head::new(None);
    commit_event(&journal, &mut record, EventBody::CancelRequested {}).await;
    let note = record
        .first_failure
        .expect("the head read failed the event");
    assert_eq!(
        note.outcome,
        crate::engine::latch::WriteOutcome::ReadCorrupt
    );
    let events = store.client().events(&session(), 1, 10).await.unwrap();
    assert_eq!(events.len(), 2, "nothing was written");
}

/// T3-S5 round 1, decision 10 (design §7.1): SQLite corruption on the
/// session-head read before a terminal commit is reported as `Corrupt`,
/// which the failure hook latches, not as a not-committed failure. Nothing
/// is written. Since round 3 (decision 13) the outcome is `ReadCorrupt`:
/// Store's read reply already recorded it.
#[tokio::test]
async fn a_corrupt_head_read_before_a_terminal_is_a_corrupt_failure() {
    let root = tempfile::tempdir().unwrap();
    let store = running_turn(&root).await;
    let journal = FaultJournal {
        store: store.client(),
        event: EventFault::CommittedThenUncertain,
        head: HeadFault::Corrupt,
        submission_fails: false,
        delayed_results: false,
    };
    let mut record = record();
    record.head = Head::new(None);
    let failed = Engine::commit_turn_ended(&journal, &started(), record, store_failure(), false)
        .await
        .unwrap_err();
    assert_eq!(
        failed.outcome,
        crate::engine::latch::WriteOutcome::ReadCorrupt
    );
    assert_eq!(failed.error.kind, "store_error");
    let events = store.client().events(&session(), 1, 10).await.unwrap();
    assert_eq!(events.len(), 2, "nothing was written");
}
