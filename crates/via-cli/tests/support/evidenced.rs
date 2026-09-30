//! Evidence for daemon scenarios written as plain tests (`via-d9o.2`): a
//! sandbox opens its artifact when it is created and collects its State
//! into it when it is dropped, after every daemon it started was reaped;
//! [`evidenced`] then finalizes each artifact with the test's outcome, once
//! the body returned or panicked, and removes the sandbox directory.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::outer_cleanup;
use crate::support::evidence::Evidence;

type EvidencedResult<T = ()> = Result<T, Box<dyn Error>>;

/// A sandbox's collected artifact, waiting for its test's outcome.
struct Parked {
    evidence: Evidence,
    /// The sandbox directory, kept until the summary hashed its fixture.
    root: PathBuf,
    /// Why collecting the State or proving cleanup failed, if it did.
    collected: Result<(), String>,
}

thread_local! {
    static INSIDE: Cell<bool> = const { Cell::new(false) };
    static PARKED: RefCell<Vec<Parked>> = const { RefCell::new(Vec::new()) };
}

/// Runs a scenario test's body, then finalizes the evidence of every
/// sandbox it created with the body's outcome: `pass`, `fail` for an error
/// or a panic, or `infrastructure_failure` when a passing body's State could
/// not be collected or its cleanup proved. A panic is resumed afterwards.
pub(crate) fn evidenced<T>(body: impl FnOnce() -> EvidencedResult<T>) -> EvidencedResult<T> {
    INSIDE.set(true);
    let result = catch_unwind(AssertUnwindSafe(body));
    INSIDE.set(false);
    let (outcome, detail) = match &result {
        Ok(Ok(_)) => ("pass", "scenario completed".to_owned()),
        Ok(Err(error)) => ("fail", error.to_string()),
        Err(payload) => (
            "fail",
            format!("scenario panicked: {}", panic_message(payload.as_ref())),
        ),
    };
    let mut incomplete = Vec::new();
    for parked in PARKED.take() {
        let artifact = parked.evidence.dir.clone();
        let (outcome, detail) = match &parked.collected {
            Ok(()) => (outcome, detail.clone()),
            Err(error) if outcome == "pass" => ("infrastructure_failure", error.clone()),
            Err(error) => (outcome, format!("{detail}; collection: {error}")),
        };
        if let Err(error) = parked.collected {
            incomplete.push(format!("{}: {error}", artifact.display()));
        }
        if let Err(error) = parked.evidence.finish(outcome, &detail) {
            incomplete.push(format!("{}: {error}", artifact.display()));
        }
        let _ = fs::remove_dir_all(&parked.root);
    }
    match result {
        Err(payload) => resume_unwind(payload),
        Ok(Err(error)) => Err(error),
        Ok(Ok(value)) if incomplete.is_empty() => Ok(value),
        Ok(Ok(_)) => Err(format!("scenario evidence: {}", incomplete.join("; ")).into()),
    }
}

/// Opens the evidence artifact of a sandbox the current test creates,
/// named after the test; only inside [`evidenced`], which finalizes it.
pub(crate) fn open(fake: &Path, fixture: &Path) -> EvidencedResult<Evidence> {
    if !INSIDE.get() {
        return Err("a daemon scenario runs inside `evidenced`".into());
    }
    let name: String = std::thread::current()
        .name()
        .unwrap_or("scenario")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Evidence::new(&name, fake, fixture)
}

/// Collects the sandbox at `root`, State `state`, into `evidence` and
/// parks it for [`evidenced`]; called by the sandbox's drop. A scenario
/// with no Store or no turn by design passes `store_expected: false`: its
/// Store, envelopes, events and turn folders are then collected if they
/// can be, but not required.
pub(crate) fn park(mut evidence: Evidence, root: PathBuf, state: &Path, store_expected: bool) {
    evidence.store_expected = store_expected;
    let collected = collect(&evidence, &root, state).map_err(|error| error.to_string());
    PARKED.with_borrow_mut(|parked| {
        parked.push(Parked {
            evidence,
            root,
            collected,
        });
    });
}

/// The daemons' stderr traces (`<root>/daemon*.trace`), `via.log`, the
/// Store's backup, envelopes and events, the turns' evidence folders and a
/// verified outer cleanup of every committed anchor.
fn collect(evidence: &Evidence, root: &Path, state: &Path) -> EvidencedResult {
    let mut traces: Vec<PathBuf> = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "trace")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with("daemon"))
        })
        .collect();
    traces.sort();
    let mut trace = Vec::new();
    for path in traces {
        trace.extend(fs::read(path)?);
    }
    evidence.write("daemon.trace", &trace)?;
    for name in ["via.log", "via.log.1"] {
        let log = state.join(name);
        if log.is_file() {
            evidence.write(name, &fs::read(log)?)?;
        }
    }
    let folders = state.join("evidence");
    if folders.is_dir() {
        evidence.copy_evidence(&folders)?;
    }
    let store = state.join("store.sqlite3");
    let anchors = match (store.is_file(), evidence.store_expected) {
        (false, _) => json!({"status":"no_store"}),
        (true, true) => store_evidence(evidence, &store)?,
        // No turn by design: a Store that cannot be read, say a corrupt one
        // the scenario made, is recorded, not required.
        (true, false) => store_evidence(evidence, &store)
            .unwrap_or_else(|error| json!({"status":"not_collected","reason":error.to_string()})),
    };
    evidence.write(
        "cleanup.json",
        json!({ "anchors": anchors }).to_string().as_bytes(),
    )?;
    let proven = (anchors["status"] == "quiescent" && anchors["absence_proven"] == true)
        || (anchors["status"] == "no_anchors" && anchors["inventory_committed"] == true)
        || (!evidence.store_expected
            && (anchors["status"] == "no_store" || anchors["status"] == "not_collected"));
    if proven {
        Ok(())
    } else {
        Err(format!("outer cleanup is unverified: {}", anchors["status"]).into())
    }
}

/// The Store's backup, envelopes and events, and the outer cleanup of
/// every committed anchor (runtime §11.2), as `cleanup.json`'s anchors.
fn store_evidence(evidence: &Evidence, store: &Path) -> EvidencedResult<serde_json::Value> {
    evidence.backup_store(store)?;
    write_rows(evidence, store)?;
    let rows = outer_cleanup::snapshot(store)?;
    Ok(outer_cleanup::verify(
        &rows,
        Instant::now() + Duration::from_secs(10),
    ))
}

/// Every stored envelope and event, read-only, as `envelopes.ndjson` and
/// `events.ndjson`.
fn write_rows(evidence: &Evidence, store: &Path) -> EvidencedResult {
    let store =
        rusqlite::Connection::open_with_flags(store, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    for (name, sql) in [
        (
            "envelopes.ndjson",
            "SELECT envelope FROM turns WHERE envelope IS NOT NULL ORDER BY session_id,number",
        ),
        (
            "events.ndjson",
            "SELECT event FROM events ORDER BY session_id,seq",
        ),
    ] {
        let mut lines = String::new();
        let mut statement = store.prepare(sql)?;
        for row in statement.query_map([], |row| row.get::<_, String>(0))? {
            lines.push_str(&row?);
            lines.push('\n');
        }
        evidence.write(name, lines.as_bytes())?;
    }
    Ok(())
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "non-string panic"
    }
}
