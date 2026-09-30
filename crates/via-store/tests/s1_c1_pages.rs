//! Task 4 design §4.3, §4.5, §6.7 and §6.8 at the Store level: the
//! `events` page (one read over `after < seq ≤ after + 1000`, `turn` and
//! `types` as SQL predicates, stopping at `limit` or `PAGE_MAX`, the scan
//! cursor and the head) and the `list` page (creation order by `ord`, at
//! most 1000 sessions examined, filters applied in order). Written before
//! `events_page` and `list_page`.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test fixtures and assertions fail loudly"
)]

use std::collections::HashSet;
use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    EventRecord, EventsPage, EventsQuery, EventsRead, ListPage, ListQuery, PAGE_MAX, SessionId,
    SpawnRecord, Store, StoreClient, SubmissionRecord, TerminalRecord, TurnNumber,
};

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

fn turn(number: u32) -> TurnNumber {
    TurnNumber::try_from(number).unwrap()
}

/// A valid session ID ending in `index` as 12 base-32 characters.
fn session(index: u32) -> SessionId {
    const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut id = String::from("s_");
    let mut value = index;
    let mut digits = Vec::new();
    for _ in 0..12 {
        digits.push(ALPHABET[(value % 32) as usize] as char);
        value /= 32;
    }
    id.extend(digits.iter().rev());
    SessionId::try_from(id.as_str()).unwrap()
}

/// `at` of `ms` milliseconds past a fixed instant.
fn at(ms: u32) -> String {
    format!(
        "2026-01-01T00:{:02}:{:02}.{:03}Z",
        ms / 60_000,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn event(kind: &str, seq: u64, turn: Option<u32>, when: u32) -> Value {
    json!({"type":kind,"seq":seq,"turn":turn,"late":false,"at":at(when)})
}

/// Spawns `id` with turn 1 queued at `when`, then submits it (seq 2).
async fn spawn(client: &StoreClient, id: &SessionId, label: Option<&str>, when: u32) {
    client
        .commit_spawn(SpawnRecord {
            session_id: id.clone(),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued","route":"fake"}),
            params: json!({"harness":"fake","model":"fake"}),
            label: label.map(str::to_owned),
            prompt: "p".into(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: event("turn.queued", 1, Some(1), when),
        })
        .await
        .unwrap();
    client
        .commit_submission(SubmissionRecord {
            session_id: id.clone(),
            turn: turn(1),
            event: event("turn.submitted", 2, Some(1), when),
        })
        .await
        .unwrap();
}

async fn commit(client: &StoreClient, id: &SessionId, event: Value) {
    client
        .commit_event(EventRecord {
            session_id: id.clone(),
            turn: turn(1),
            event,
        })
        .await
        .unwrap();
}

async fn page(
    client: &StoreClient,
    id: &SessionId,
    turn_filter: Option<u32>,
    after: u64,
    limit: u32,
    types: &[&str],
) -> (Vec<Value>, EventsPage) {
    let read = client
        .events_page(EventsQuery {
            session: id.clone(),
            turn: turn_filter.map(turn),
            after,
            limit,
            types: types.iter().map(|kind| (*kind).to_owned()).collect(),
        })
        .await
        .unwrap();
    let EventsRead::Page(page) = read else {
        panic!("no page for {id:?}");
    };
    let events: Vec<Value> = serde_json::from_str(&page.events).unwrap();
    (events, page)
}

fn seqs(events: &[Value]) -> Vec<u64> {
    events
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect()
}

/// Design §4.3, §13.2: the window is `after < seq ≤ after + 1000`; `types`
/// and `turn` filter in SQL; `next_after` is the last scanned sequence,
/// filtered rows included, and `more` compares it with the head read in
/// the same request; the page stops at `limit` and before `PAGE_MAX`, a
/// row's length checked before it is copied; each event is the stored
/// document, with no `raw_ref`.
#[test]
fn s1_c1_events_page_filters_and_bounds() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client().public();
    let writer = store.client();
    let small = session(1);
    let large = session(2);
    runtime().block_on(async {
        spawn(&writer, &small, None, 0).await;
        // Seq 3..=1102: warnings of turn 1, every tenth a session-level
        // `session.reopened` with no turn.
        for seq in 3..=1102_u64 {
            let (kind, of) = if seq % 10 == 0 {
                ("session.reopened", None)
            } else {
                ("warning", Some(1))
            };
            commit(&writer, &small, event(kind, seq, of, 1)).await;
        }
        let head = 1102;

        let (events, first) = page(&client, &small, None, 0, 5, &[]).await;
        assert_eq!(seqs(&events), [1, 2, 3, 4, 5]);
        assert_eq!((first.next_after, first.more), (5, true));
        assert!(events.iter().all(|event| event.get("raw_ref").is_none()));

        // Only session-level events: the page stops at its limit.
        let (events, reopened) = page(&client, &small, None, 0, 3, &["session.reopened"]).await;
        assert_eq!(seqs(&events), [10, 20, 30]);
        assert_eq!((reopened.next_after, reopened.more), (30, true));

        // The window ends at `after + 1000` whatever the filter found.
        let (events, window) = page(&client, &small, None, 0, 1000, &["session.reopened"]).await;
        assert_eq!(events.len(), 100);
        assert_eq!((window.next_after, window.more), (1000, true));
        let (events, rest) = page(&client, &small, None, 1000, 1000, &["session.reopened"]).await;
        assert_eq!(
            seqs(&events),
            [1010, 1020, 1030, 1040, 1050, 1060, 1070, 1080, 1090, 1100]
        );
        // The last scanned row is the head, a filtered-out warning.
        assert_eq!((rest.next_after, rest.more), (head, false));

        // A filter that matches nothing in the window still advances.
        let (events, none) = page(&client, &small, None, 0, 200, &["turn.ended"]).await;
        assert!(events.is_empty());
        assert_eq!((none.next_after, none.more), (1000, true));

        // `turn` is a predicate on the v6 column: no session-level event.
        let (events, of_turn) = page(&client, &small, Some(1), 0, 20, &[]).await;
        assert_eq!(
            seqs(&events),
            [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12, 13, 14, 15, 16, 17, 18, 19, 21, 22
            ]
        );
        assert_eq!((of_turn.next_after, of_turn.more), (22, true));
        let (events, both) = page(
            &client,
            &small,
            Some(1),
            0,
            3,
            &["turn.submitted", "warning"],
        )
        .await;
        assert_eq!(seqs(&events), [2, 3, 4]);
        assert_eq!((both.next_after, both.more), (4, true));

        // After the head: an empty page that stays at `after`.
        let (events, past) = page(&client, &small, None, head, 200, &[]).await;
        assert!(events.is_empty());
        assert_eq!((past.next_after, past.more), (head, false));

        // A turn or session the Store does not have.
        let missing = client
            .events_page(EventsQuery {
                session: small.clone(),
                turn: Some(turn(2)),
                after: 0,
                limit: 10,
                types: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(missing, EventsRead::TurnNotFound));
        let unknown = client
            .events_page(EventsQuery {
                session: session(99),
                turn: None,
                after: 0,
                limit: 10,
                types: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(unknown, EventsRead::SessionNotFound));

        // Six events of 250 KiB: the page holds as many as fit in PAGE_MAX
        // and resumes at the first one left out.
        spawn(&writer, &large, None, 0).await;
        let pad = "x".repeat(250 * 1024);
        for seq in 3..=8_u64 {
            let mut big = event("warning", seq, Some(1), 2);
            big["message"] = Value::String(pad.clone());
            commit(&writer, &large, big).await;
        }
        let (events, bounded) = page(&client, &large, None, 0, 1000, &[]).await;
        assert!(bounded.events.len() <= PAGE_MAX, "{}", bounded.events.len());
        assert_eq!(seqs(&events), [1, 2, 3, 4, 5, 6]);
        assert_eq!((bounded.next_after, bounded.more), (6, true));
        let (events, next) = page(&client, &large, None, 6, 1000, &[]).await;
        assert_eq!(seqs(&events), [7, 8]);
        assert_eq!((next.next_after, next.more), (8, false));
    });
}

async fn list(client: &StoreClient, query: ListQuery) -> ListPage {
    client.list_page(query).await.unwrap()
}

fn query(before: Option<u64>, label: Option<&str>, limit: u32) -> ListQuery {
    ListQuery {
        before,
        state: None,
        harness: None,
        label: label.map(str::to_owned),
        since_ms: None,
        limit,
    }
}

/// Design §6.8, §13.2 at the Store level: sessions come newest first by
/// `ord`; a page examines at most 1000 sessions, so a filter that matches
/// one old session gives an empty page with a cursor, then that session;
/// every session present at the first page is returned exactly once while
/// states change, and a session created mid-scan never appears; the
/// cursor is null once the oldest session was examined.
#[test]
fn s1_c1_list_page_examines_at_most_1000_sessions_each_once() {
    const SESSIONS: u32 = 1250;
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client().public();
    let writer = store.client();
    runtime().block_on(async {
        for index in 0..SESSIONS {
            let label = (index == 0).then_some("needle");
            spawn(&writer, &session(index), label, index).await;
        }
        // Only the oldest session matches: the first 1000 examined give an
        // empty page that still carries a cursor.
        let first = list(&client, query(None, Some("needle"), 50)).await;
        assert!(first.sessions.is_empty());
        let cursor = first.next.expect("a cursor after 1000 sessions");
        let second = list(&client, query(Some(cursor), Some("needle"), 50)).await;
        let found: Vec<_> = second
            .sessions
            .iter()
            .map(|summary| summary.session_id.clone())
            .collect();
        assert_eq!(found, [session(0)]);
        assert_eq!(second.next, None);

        // Unfiltered pages of 200, newest first, while states change and a
        // session is created mid-scan.
        let mut seen = Vec::new();
        let mut before = None;
        let mut pages = 0;
        loop {
            let listed = list(&client, query(before, None, 200)).await;
            pages += 1;
            for summary in &listed.sessions {
                seen.push(summary.session_id.clone());
            }
            if pages == 2 {
                // Turn 1 of the newest and the oldest sessions ends.
                for index in [SESSIONS - 1, 0] {
                    writer
                        .commit_terminal(TerminalRecord {
                            session_id: session(index),
                            turn: turn(1),
                            envelope: json!({"state":"failed"}),
                            event: event("turn.ended", 3, Some(1), 99_000),
                            steps: Vec::new(),
                        })
                        .await
                        .unwrap();
                }
                spawn(&writer, &session(SESSIONS), None, 99_000).await;
            }
            match listed.next {
                Some(next) => before = Some(next),
                None => break,
            }
        }
        let expected: Vec<SessionId> = (0..SESSIONS).rev().map(session).collect();
        assert_eq!(
            seen, expected,
            "newest first, each once, none created later"
        );
        let unique: HashSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), seen.len());

        // The changed state and the latest event's time are reported.
        let newest = list(&client, query(None, None, 1)).await;
        let summary = &newest.sessions[0];
        assert_eq!(summary.session_id, session(SESSIONS));
        let oldest = list(&client, query(Some(2), None, 1)).await;
        let summary = &oldest.sessions[0];
        assert_eq!(summary.session_id, session(0));
        assert_eq!(summary.state, "idle");
        assert_eq!(summary.label.as_deref(), Some("needle"));
        assert_eq!(summary.created_ms + 99_000, summary.last_active_ms);
        let since = summary.last_active_ms;
        let mut recent = Vec::new();
        let mut before = None;
        loop {
            let mut filtered = query(before, None, 200);
            filtered.since_ms = Some(since);
            let listed = list(&client, filtered).await;
            recent.extend(
                listed
                    .sessions
                    .iter()
                    .map(|summary| summary.session_id.clone()),
            );
            match listed.next {
                Some(next) => before = Some(next),
                None => break,
            }
        }
        assert_eq!(
            recent,
            [session(SESSIONS), session(SESSIONS - 1), session(0)]
        );
    });
}
