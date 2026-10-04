//! Design §4.1, §4.3 (beads via-p98.3.5, via-2lp): a pending `wait` or
//! `events` long-poll re-reads at each change that may settle it, a Store
//! commit, a turn recorded unpersisted, final shutdown or the writer's end,
//! instead of on a fixed one-second check. Written before the commit
//! signal: each case failed against the one-second check (a wake took up to
//! 1 s) or against the strict `events` parameters (`wait_ms` unknown).

use std::{
    os::unix::fs::DirBuilderExt,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use via_store::{SubmissionRecord, TerminalRecord};

use super::{FAILPOINT_TOKEN, child, end_turn, new_session, open, run, shutdown, turn};
use crate::api::{Event, EventBody, rfc3339};
use crate::engine::Engine;
use crate::{ApiError, EventsParams, SessionId, TurnState, WaitParams};

/// A wake well inside the old one-second check, with room for a loaded host.
const PROMPTLY: Duration = Duration::from_millis(250);

/// Long enough for a pending read to have read once and be waiting.
const SETTLE: Duration = Duration::from_millis(100);

fn wait_params(session: &SessionId) -> WaitParams {
    serde_json::from_value(json!({"address":format!("{}/1", session.as_str()),
        "timeout_ms":20_000}))
    .unwrap()
}

fn events_params(params: &Value) -> EventsParams {
    serde_json::from_value(params.clone()).unwrap()
}

/// Starts `wait` for the session's turn 1 on its own task.
fn waiter(
    engine: &Arc<Engine>,
    session: &SessionId,
) -> tokio::task::JoinHandle<Result<Box<serde_json::value::RawValue>, ApiError>> {
    let engine = Arc::clone(engine);
    let params = wait_params(session);
    tokio::spawn(async move { engine.wait(params).await })
}

/// Starts `events` with `params` on its own task; the page parsed.
fn poller(
    engine: &Arc<Engine>,
    params: &Value,
) -> tokio::task::JoinHandle<Result<Value, ApiError>> {
    let engine = Arc::clone(engine);
    let params = events_params(params);
    tokio::spawn(async move {
        let page = engine.events(params).await?;
        Ok(serde_json::from_str(page.get()).unwrap())
    })
}

/// Turn 1's canonical event at `seq`.
fn event(session: &SessionId, seq: u64, body: EventBody) -> Value {
    Event {
        seq,
        session_id: session,
        turn: Some(1),
        late: false,
        at: &rfc3339(std::time::SystemTime::now()),
        body,
    }
    .to_value()
    .unwrap()
}

/// Commits turn 1's `turn.submitted` at `seq` directly.
async fn submit(engine: &Engine, session: &SessionId, seq: u64) {
    engine
        .store
        .commit_submission(SubmissionRecord {
            session_id: session.clone(),
            turn: turn(1),
            event: event(session, seq, EventBody::TurnSubmitted { attempt: 1 }),
        })
        .await
        .unwrap();
}

/// Commits turn 1's `failed` terminal with `turn.ended` at `seq` directly.
async fn end(engine: &Engine, session: &SessionId, seq: u64) {
    let ended = EventBody::TurnEnded {
        state: "failed",
        failure: None,
        stop_reason: "error",
        cancel: None,
    };
    engine
        .store
        .commit_terminal(TerminalRecord {
            session_id: session.clone(),
            turn: turn(1),
            envelope: json!({"state":"failed","cancel":null}),
            event: event(session, seq, ended),
            steps: Vec::new(),
            link_released: false,
        })
        .await
        .unwrap();
}

fn seqs(page: &Value) -> Vec<u64> {
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect()
}

/// Design §4.1: a pending `wait` returns as soon as its turn's terminal
/// commits, not at its next one-second check.
#[test]
#[expect(clippy::print_stderr, reason = "the measured latency is evidence")]
fn wait_returns_as_soon_as_the_terminal_commits() {
    let Some(root) = child("wake::wait_returns_as_soon_as_the_terminal_commits") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        submit(&engine, &session, 2).await;
        let waiting = waiter(&engine, &session);
        tokio::time::sleep(SETTLE).await;
        assert!(!waiting.is_finished(), "the turn is not terminal yet");
        end(&engine, &session, 3).await;
        let committed = Instant::now();
        let envelope = waiting.await.unwrap().unwrap();
        let seen = committed.elapsed();
        eprintln!("wait latency after the terminal commit: {seen:?}");
        assert!(envelope.get().contains("\"failed\""), "{}", envelope.get());
        assert!(
            seen < PROMPTLY,
            "the end was seen {seen:?} after its commit"
        );
    });
}

/// Design §4.1, lost wakeup: the terminal commits while the waiter is held
/// between its read (which found none) and its await
/// (`core.wait.registered`). Subscribed before the read, it still sees the
/// commit at once when released.
#[cfg(feature = "test-failpoints")]
#[test]
fn wait_sees_a_terminal_committed_between_its_read_and_its_await() {
    let Some(root) = child("wake::wait_sees_a_terminal_committed_between_its_read_and_its_await")
    else {
        return;
    };
    let point = "core.wait.registered";
    let points = super::pause_first(&root, point);
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        submit(&engine, &session, 2).await;
        let waiting = waiter(&engine, &session);
        let ack = points.join(format!("{point}.1.ack"));
        let held = Instant::now();
        while !ack.exists() {
            assert!(
                held.elapsed() < Duration::from_secs(10),
                "{point} never hit"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        end(&engine, &session, 3).await;
        super::release_point(&points, point, 1);
        let released = Instant::now();
        let envelope = waiting.await.unwrap().unwrap();
        let seen = released.elapsed();
        assert!(envelope.get().contains("\"failed\""), "{}", envelope.get());
        assert!(
            seen < PROMPTLY,
            "the held waiter saw the end {seen:?} after release"
        );
    });
}

/// Design §4.1: final shutdown is no Store commit, yet it ends a pending
/// `wait` `daemon_stopping` at once, not at the wait's deadline.
#[test]
fn final_shutdown_ends_a_pending_wait_at_once() {
    let Some(root) = child("wake::final_shutdown_ends_a_pending_wait_at_once") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        // Running in Store with no owner here: final shutdown settles nothing of it.
        end_turn(&engine, &session, 1, None).await;
        let waiting = waiter(&engine, &session);
        tokio::time::sleep(SETTLE).await;
        assert!(!waiting.is_finished(), "the turn is not terminal");
        let _report = shutdown(&engine).await;
        let finalized = Instant::now();
        let error = waiting.await.unwrap().unwrap_err();
        let seen = finalized.elapsed();
        assert_eq!(error.kind, "daemon_stopping");
        assert!(seen < PROMPTLY, "finalization was seen {seen:?} late");
    });
}

/// Design §4.1, §7.3: a turn Core records unpersisted (its terminal could
/// not be made durable) is no Store commit, yet a pending `wait` reports
/// it `store_error` at once.
#[test]
fn a_turn_recorded_unpersisted_ends_a_pending_wait_at_once() {
    let Some(root) = child("wake::a_turn_recorded_unpersisted_ends_a_pending_wait_at_once") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        submit(&engine, &session, 2).await;
        let waiting = waiter(&engine, &session);
        tokio::time::sleep(SETTLE).await;
        assert!(!waiting.is_finished(), "the turn is not terminal");
        engine
            .unresolved
            .fail(&session, turn(1), TurnState::Running);
        let failed = Instant::now();
        let error = waiting.await.unwrap().unwrap_err();
        let seen = failed.elapsed();
        assert_eq!(error.kind, "store_error");
        assert!(error.unpersisted.is_some(), "{:?}", error.data());
        assert!(seen < PROMPTLY, "the failure was seen {seen:?} late");
    });
}

/// Design §4.1, §6.3: the writer dies (`store.writer.before_serve`
/// `fail_io` panics it) while a `wait` is pending. Its commit signal closes
/// with it, so the waiter re-reads at once and gets the Store's error
/// rather than sleeping to its deadline.
#[test]
fn writer_death_ends_a_pending_wait_with_the_store_error() {
    let Some(root) = child("wake::writer_death_ends_a_pending_wait_with_the_store_error") else {
        return;
    };
    let points = root.join("failpoints");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&points)
        .unwrap();
    via_store::failpoint::activate(&points, FAILPOINT_TOKEN).unwrap();
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let waiting = waiter(&engine, &session);
        tokio::time::sleep(SETTLE).await;
        assert!(!waiting.is_finished(), "the turn is not terminal");
        let command =
            json!({"token":FAILPOINT_TOKEN,"occurrence":1,"action":"fail_io","persist":true});
        std::fs::write(
            points.join("store.writer.before_serve.json"),
            command.to_string(),
        )
        .unwrap();
        let killed = Instant::now();
        // The writer dies serving this read, which is lost with it.
        assert!(engine.store.next_seq(&session).await.is_err());
        let error = waiting.await.unwrap().unwrap_err();
        let seen = killed.elapsed();
        assert_eq!(error.kind, "store_error");
        assert!(seen < PROMPTLY, "the writer's end was seen {seen:?} late");
    });
}

/// C1 §3.11: an `events` long-poll with a `types` filter is not ended by
/// commits it filters out, and returns as soon as a matching event commits.
#[test]
fn events_long_poll_returns_when_a_matching_event_commits() {
    let Some(root) = child("wake::events_long_poll_returns_when_a_matching_event_commits") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let polling = poller(
            &engine,
            &json!({"session":session,"after":1,"types":["turn.ended"],"wait_ms":10_000}),
        );
        tokio::time::sleep(SETTLE).await;
        assert!(!polling.is_finished(), "nothing matched yet");
        submit(&engine, &session, 2).await;
        tokio::time::sleep(SETTLE).await;
        assert!(
            !polling.is_finished(),
            "a filtered-out commit ended the poll"
        );
        end(&engine, &session, 3).await;
        let committed = Instant::now();
        let page = polling.await.unwrap().unwrap();
        let seen = committed.elapsed();
        assert_eq!(seqs(&page), [3], "{page}");
        assert_eq!(page["events"][0]["type"], "turn.ended", "{page}");
        assert_eq!(
            (&page["next_after"], &page["more"]),
            (&json!(3), &json!(false))
        );
        assert!(
            seen < PROMPTLY,
            "the match was seen {seen:?} after its commit"
        );
    });
}

/// C1 §3.11: a long-poll that finds no match by `wait_ms` returns the empty
/// page normally, its `next_after` past the events it filtered out; with no
/// commit at all it stays at `after`.
#[test]
fn events_long_poll_ends_at_its_bound_with_the_last_empty_page() {
    let Some(root) = child("wake::events_long_poll_ends_at_its_bound_with_the_last_empty_page")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let started = Instant::now();
        let polling = poller(
            &engine,
            &json!({"session":session,"after":1,"types":["session.closed"],"wait_ms":600}),
        );
        tokio::time::sleep(SETTLE).await;
        submit(&engine, &session, 2).await;
        end(&engine, &session, 3).await;
        let page = polling.await.unwrap().unwrap();
        let took = started.elapsed();
        assert_eq!(
            page,
            json!({"events":[],"next_after":3,"more":false,"earliest_seq":1})
        );
        assert!(took >= Duration::from_millis(600), "ended early: {took:?}");

        let started = Instant::now();
        let page = poller(&engine, &json!({"session":session,"after":3,"wait_ms":300}))
            .await
            .unwrap()
            .unwrap();
        let took = started.elapsed();
        assert_eq!(
            page,
            json!({"events":[],"next_after":3,"more":false,"earliest_seq":1})
        );
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_millis(300) + PROMPTLY,
            "a 300 ms long-poll took {took:?}"
        );
    });
}

/// C1 §3.11: `wait_ms` is at most 30,000 (`invalid_params` above); 0 is
/// exactly a call without it, answered at once.
#[test]
fn events_wait_ms_is_bounded_and_zero_never_waits() {
    let Some(root) = child("wake::events_wait_ms_is_bounded_and_zero_never_waits") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let over = engine
            .events(events_params(&json!({"session":session,"wait_ms":30_001})))
            .await
            .unwrap_err();
        assert_eq!(over.kind, "invalid_params");
        let plain = engine
            .events(events_params(&json!({"session":session,"after":1})))
            .await
            .unwrap();
        let started = Instant::now();
        let zero = engine
            .events(events_params(
                &json!({"session":session,"after":1,"wait_ms":0}),
            ))
            .await
            .unwrap();
        assert_eq!(zero.get(), plain.get());
        assert!(started.elapsed() < PROMPTLY, "wait_ms 0 waited");
        let missing = engine
            .events(events_params(
                &json!({"session":"s_000000000000","wait_ms":5_000}),
            ))
            .await
            .unwrap_err();
        assert_eq!(missing.kind, "session_not_found", "not found is immediate");
    });
}

/// C1 §3.11: final shutdown ends a long-poll that found nothing
/// `daemon_stopping` at once, as it ends a `wait`.
#[test]
fn final_shutdown_ends_an_events_long_poll_at_once() {
    let Some(root) = child("wake::final_shutdown_ends_an_events_long_poll_at_once") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        end_turn(&engine, &session, 1, None).await;
        let polling = poller(
            &engine,
            &json!({"session":session,"after":100,"wait_ms":20_000}),
        );
        tokio::time::sleep(SETTLE).await;
        assert!(!polling.is_finished(), "nothing follows the cursor");
        let _report = shutdown(&engine).await;
        let finalized = Instant::now();
        let error = polling.await.unwrap().unwrap_err();
        let seen = finalized.elapsed();
        assert_eq!(error.kind, "daemon_stopping");
        assert!(seen < PROMPTLY, "finalization was seen {seen:?} late");
    });
}
