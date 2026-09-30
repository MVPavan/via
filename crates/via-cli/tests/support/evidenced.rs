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
    /// Why proving the daemons' exit, collecting the State or proving
    /// cleanup failed, if it did.
    collected: Result<(), String>,
    /// Whether every daemon was proved gone, so the sandbox may be removed.
    exited: bool,
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
        // Never remove a sandbox from under a daemon not proved gone.
        if parked.exited {
            let _ = fs::remove_dir_all(&parked.root);
        }
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
/// parks it for [`evidenced`]; called by the sandbox's drop with
/// [`stop_daemons`]'s proof that every daemon exited. Without that proof
/// nothing is collected, the scenario is an infrastructure failure and the
/// sandbox is kept. `expected` says what the scenario must hold: a
/// scenario with no Store by design clears `store`; one whose turns launch
/// no vendor clears only `folders`, so the Store, envelopes, events and
/// cleanup stay required and a launched turn must still have its folder.
pub(crate) fn park(
    mut evidence: Evidence,
    root: PathBuf,
    state: &Path,
    expected: Expected,
    exited: Result<(), String>,
) {
    evidence.store_expected = expected.store;
    evidence.folders_expected = expected.folders;
    let proved = exited.is_ok();
    let collected = exited
        .map_err(|error| format!("daemon exit unproven, nothing collected: {error}"))
        .and_then(|()| collect(&evidence, &root, state).map_err(|error| error.to_string()));
    PARKED.with_borrow_mut(|parked| {
        parked.push(Parked {
            evidence,
            root,
            collected,
            exited: proved,
        });
    });
}

/// What a scenario's evidence must hold (see [`park`]).
#[derive(Clone, Copy)]
pub(crate) struct Expected {
    /// The Store, envelopes, events, cleanup and the turns' folders.
    pub(crate) store: bool,
    /// The turns' evidence folders, which only a launched vendor creates.
    pub(crate) folders: bool,
}

/// Proves every daemon of a sandbox exited, before its State is collected
/// or the sandbox removed (S1-contract r1 finding 3, r2 findings 1 and 2).
/// A daemon is identified by its process, not by its socket or locks: a
/// daemon removes its socket and releases both locks before it exits. Every
/// `via` process of the sandbox carries `VIA_RUNTIME_DIR=<runtime>` or
/// `VIA_STATE_DIR=<state>` in its environment, the one the harness gave its
/// child or the one the CLI gives the daemon it starts; anchors and vendors
/// are started with a cleared environment. While one is alive, `stop` runs
/// once with the budget left, then each must exit, and then neither
/// `daemon.lock` nor `store.lock` may still be held. The whole proof,
/// `stop` included, has one budget, and a proof that completes after it
/// elapsed is not accepted. Children the test started are reaped by their
/// own guards first.
pub(crate) fn stop_daemons(
    runtime: &Path,
    state: &Path,
    stop: impl FnOnce(Duration),
) -> Result<(), String> {
    stop_within(runtime, state, Duration::from_secs(20), stop)
}

/// [`stop_daemons`] with budget `budget`.
pub(crate) fn stop_within(
    runtime: &Path,
    state: &Path,
    budget: Duration,
    stop: impl FnOnce(Duration),
) -> Result<(), String> {
    let deadline = Instant::now() + budget;
    let mut stop = Some(stop);
    loop {
        let alive = sandbox_processes(runtime, state)?;
        if alive.is_empty() {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("sandbox processes {alive:?} did not exit"));
        }
        if let Some(stop) = stop.take() {
            stop(left);
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    for lock in [runtime.join("daemon.lock"), state.join("store.lock")] {
        loop {
            match fs::File::open(&lock) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(format!("{}: {error}", lock.display())),
                Ok(file) if file.try_lock().is_ok() => break,
                Ok(_) => {}
            }
            if Instant::now() >= deadline {
                return Err(format!("{} is still held", lock.display()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if Instant::now() > deadline {
        return Err(format!("the exit proof exceeded its {budget:?} budget"));
    }
    Ok(())
}

/// Runs `command`, its output discarded, for at most `budget`: killed and
/// reaped when the budget elapses. A sandbox's `stop` for [`stop_daemons`].
pub(crate) fn run_within(command: &mut std::process::Command, budget: Duration) {
    use std::process::Stdio;
    let deadline = Instant::now() + budget;
    let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    while matches!(child.try_wait(), Ok(None)) {
        if Instant::now() >= deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = child.wait();
}

/// The live (not zombie) processes whose environment names the sandbox's
/// runtime or State directory. An unreadable `/proc` proves nothing.
fn sandbox_processes(runtime: &Path, state: &Path) -> Result<Vec<u32>, String> {
    use std::os::unix::ffi::OsStrExt as _;
    let marks = [
        [
            b"VIA_RUNTIME_DIR=".as_slice(),
            runtime.as_os_str().as_bytes(),
        ]
        .concat(),
        [b"VIA_STATE_DIR=".as_slice(), state.as_os_str().as_bytes()].concat(),
    ];
    let own = std::process::id();
    let mut alive = Vec::new();
    for entry in fs::read_dir("/proc").map_err(|error| format!("/proc: {error}"))? {
        let entry = entry.map_err(|error| format!("/proc: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        // Gone, or another user's: not the sandbox's.
        let Ok(environ) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        if environ
            .split(|byte| *byte == 0)
            .any(|variable| marks.iter().any(|mark| variable == mark.as_slice()))
            && !exited(pid)
        {
            alive.push(pid);
        }
    }
    Ok(alive)
}

/// Whether `pid` has exited: gone, or a zombie awaiting its reaper.
fn exited(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X'))
    })
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
    launched_turns_have_folders(evidence, store)?;
    let rows = outer_cleanup::snapshot(store)?;
    Ok(outer_cleanup::verify(
        &rows,
        Instant::now() + Duration::from_secs(10),
    ))
}

/// Every turn that launched a vendor (it has an anchor) has its evidence
/// folder, whether or not the scenario waived folders: only unlaunched
/// turns lack one (S1-contract r2 finding 3).
fn launched_turns_have_folders(evidence: &Evidence, store: &Path) -> EvidencedResult {
    let store =
        rusqlite::Connection::open_with_flags(store, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = store.prepare("SELECT DISTINCT owner_session, owner_turn FROM anchors")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (session, turn) = row?;
        if !evidence
            .dir
            .join("evidence")
            .join(&session)
            .join(turn.to_string())
            .is_dir()
        {
            return Err(format!("launched turn {session}/{turn} has no evidence folder").into());
        }
    }
    Ok(())
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
