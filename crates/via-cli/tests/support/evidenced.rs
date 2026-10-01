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

use serde_json::{Value, json};

use crate::outer_cleanup;
use crate::scenario::ScenarioError;
use crate::support::evidence::Evidence;

type EvidencedResult<T = ()> = Result<T, Box<dyn Error>>;

/// A sandbox's collected artifact, waiting for its test's outcome.
struct Parked {
    evidence: Evidence,
    /// The sandbox directory, kept until the summary hashed its fixture.
    root: PathBuf,
    /// Whether every daemon was proved gone, so the sandbox may be removed.
    exited: bool,
}

thread_local! {
    static INSIDE: Cell<bool> = const { Cell::new(false) };
    static PARKED: RefCell<Vec<Parked>> = const { RefCell::new(Vec::new()) };
}

/// Runs a scenario test's body, then finalizes the evidence of every
/// sandbox it created with the body's own outcome (runtime §11.2): `pass`,
/// the category of a typed [`ScenarioError`] (`fail`, `timeout` or
/// `infrastructure_failure`), returned or raised with `panic_any`, or
/// `fail` for any other error or panic.
/// A State that could not be collected or a cleanup not proved is recorded
/// beside that outcome and fails the test. A panic is resumed afterwards.
pub(crate) fn evidenced<T>(body: impl FnOnce() -> EvidencedResult<T>) -> EvidencedResult<T> {
    INSIDE.set(true);
    let result = catch_unwind(AssertUnwindSafe(body));
    INSIDE.set(false);
    let (outcome, detail) = match &result {
        Ok(Ok(_)) => ("pass", "scenario completed".to_owned()),
        Ok(Err(error)) => match error.downcast_ref::<ScenarioError>() {
            Some(typed) => (typed.outcome(), typed.detail().to_owned()),
            None => ("fail", error.to_string()),
        },
        // A helper that cannot return an error raises a typed one with
        // `panic_any`, so its timeout stays a timeout.
        Err(payload) => match payload.downcast_ref::<ScenarioError>() {
            Some(typed) => (typed.outcome(), typed.detail().to_owned()),
            None => (
                "fail",
                format!("scenario panicked: {}", panic_message(payload.as_ref())),
            ),
        },
    };
    let mut incomplete = Vec::new();
    for parked in PARKED.take() {
        let artifact = parked.evidence.dir.clone();
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

/// Records the sandbox's cleanup, then collects the sandbox at `root`,
/// State `state`, into `evidence` and parks it for [`evidenced`]; called by
/// the sandbox's drop with [`stop_daemons`]'s proof that every daemon
/// exited. The cleanup runs and is recorded first, independently of the
/// evidence (runtime §11.2, S1-evidence2 fix round 2 finding 6): the exit
/// proof, the guards' records and failures, and the outer anchor cleanup
/// with only the time left before the teardown's deadline, as
/// `cleanup.json`, even when the exit proof failed. The evidence copies
/// follow; failures of either are recorded beside the scenario's outcome,
/// never replacing it or each other. A sandbox whose daemons were not
/// proved gone is kept. `expected` says what the scenario must hold: a
/// scenario with no Store by design clears `store`; one whose turns launch
/// no vendor clears only `folders`, so the Store, envelopes, events and
/// cleanup stay required and a launched turn must still have its folder.
/// Neither waiver ever waives the cleanup proof.
pub(crate) fn park(
    mut evidence: Evidence,
    root: PathBuf,
    state: &Path,
    expected: Expected,
    exited: Exited,
) {
    evidence.store_expected = expected.store;
    evidence.folders_expected = expected.folders;
    let proved = exited.proof.is_ok();
    for failure in cleanup(&evidence, state, exited) {
        evidence.cleanup_failed(failure);
    }
    let collected = collect(&evidence, &root, state);
    if !collected.is_empty() {
        evidence.collection_failure = Some(collected.join("; "));
    }
    PARKED.with_borrow_mut(|parked| {
        parked.push(Parked {
            evidence,
            root,
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
/// once, as the ordinary stop, with a deadline at most
/// [`outer_cleanup::ORDINARY_STOP`] away (runtime §11.2), returning its
/// record and any cleanup failure; then each must exit, and then neither
/// `daemon.lock` nor `store.lock` may still be held. The whole proof has
/// what is left of the scenario's final `teardown`: begun by the first
/// guard that tore its daemon down, or here, and shared with [`park`]'s
/// anchor cleanup; a proof completed after it is not accepted. The guards'
/// records and failures travel with the proof to `cleanup.json`.
pub(crate) fn stop_daemons(
    runtime: &Path,
    state: &Path,
    teardown: &outer_cleanup::Teardown,
    stop: impl FnOnce(Instant) -> (Value, Option<String>),
) -> Exited {
    let deadline = teardown.begin();
    let proof = prove_exit(runtime, state, deadline, |by| {
        let (record, failure) = stop(by);
        teardown.record(json!({"generation":"sandbox_stop","stop":record}), failure);
    });
    let (_, failures) = teardown.report();
    Exited {
        proof,
        deadline,
        teardown: teardown.summary(),
        failures,
    }
}

/// A sandbox teardown's exit proof, the deadline taken at its entry, its
/// guards' records ([`outer_cleanup::Teardown::summary`]) and their
/// cleanup failures.
pub(crate) struct Exited {
    pub(crate) proof: Result<(), String>,
    pub(crate) deadline: Instant,
    pub(crate) teardown: Value,
    pub(crate) failures: Vec<String>,
}

fn prove_exit(
    runtime: &Path,
    state: &Path,
    deadline: Instant,
    stop: impl FnOnce(Instant),
) -> Result<(), String> {
    let mut stop = Some(stop);
    loop {
        let alive = sandbox_processes(runtime, state)?;
        if alive.is_empty() {
            break;
        }
        if outer_cleanup::left(deadline).is_zero() {
            return Err(format!("sandbox processes {alive:?} did not exit"));
        }
        if let Some(stop) = stop.take() {
            stop(deadline.min(Instant::now() + outer_cleanup::ORDINARY_STOP));
        } else {
            std::thread::sleep(Duration::from_millis(10).min(outer_cleanup::left(deadline)));
        }
    }
    for lock in [runtime.join("daemon.lock"), state.join("store.lock")] {
        loop {
            let held = match fs::File::open(&lock) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(format!("{}: {error}", lock.display())),
                Ok(file) => file.try_lock().is_err(),
            };
            if !held {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("{} is still held", lock.display()));
            }
            std::thread::sleep(Duration::from_millis(10).min(outer_cleanup::left(deadline)));
        }
    }
    if Instant::now() > deadline {
        return Err("the exit proof exceeded the teardown deadline".to_owned());
    }
    Ok(())
}

/// The live (not zombie) processes whose environment names the sandbox's
/// runtime or State directory. An unreadable `/proc` proves nothing.
fn sandbox_processes(runtime: &Path, state: &Path) -> Result<Vec<u32>, String> {
    scan_processes(runtime, state, |path| fs::read(path))
}

/// [`sandbox_processes`], reading `/proc/<pid>/{environ,cmdline}` through
/// `read`. Only a vanished process (`NotFound`, `ESRCH`) is absent (S1-contract
/// r3 finding 1). A process whose environment cannot be read otherwise is
/// unrelated only when its command line shows another program than this
/// build's `via`: the user's own non-dumpable processes (`systemd --user`,
/// agents) refuse `environ` too, and every sandbox process runs `via`. An
/// unreadable `via`, or an unreadable command line, is indeterminate.
pub(crate) fn scan_processes(
    runtime: &Path,
    state: &Path,
    read: impl Fn(&Path) -> std::io::Result<Vec<u8>>,
) -> Result<Vec<u32>, String> {
    scan_processes_by(runtime, state, None, |path, _| read(path))
}

/// The most bytes of a process's environment [`scan_processes_by`] reads.
pub(crate) const ENVIRON_CAP: u64 = 256 * 1024;

/// [`scan_processes`] by `cutoff`, if any (runtime §11.2), reading
/// through `read(path, cap)`, which returns at most `cap` bytes (a reader
/// may return the whole file). Once `cutoff` has passed, nothing more is
/// read and the scan is uncertainty, never absence: it is checked before
/// the scan starts, between entries and before every read. An environment
/// is asked for up to [`ENVIRON_CAP`] bytes, a command line only as far as
/// this build's `via` path and its terminator; an environment of exactly
/// [`ENVIRON_CAP`] bytes without the mark may have been cut, so it is
/// unreadable, judged by its command line like any other.
pub(crate) fn scan_processes_by(
    runtime: &Path,
    state: &Path,
    cutoff: Option<Instant>,
    read: impl Fn(&Path, u64) -> std::io::Result<Vec<u8>>,
) -> Result<Vec<u32>, String> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut scanned = 0_usize;
    let expired = |scanned: usize| {
        cutoff.filter(|cutoff| Instant::now() >= *cutoff).map(|_| {
            format!(
                "the process scan reached its cutoff after {scanned} entries: exit indeterminate"
            )
        })
    };
    let vanished = |error: &std::io::Error| {
        error.kind() == std::io::ErrorKind::NotFound
            || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
    };
    let marks = [
        [
            b"VIA_RUNTIME_DIR=".as_slice(),
            runtime.as_os_str().as_bytes(),
        ]
        .concat(),
        [b"VIA_STATE_DIR=".as_slice(), state.as_os_str().as_bytes()].concat(),
    ];
    let via = Path::new(env!("CARGO_BIN_EXE_via")).as_os_str().as_bytes();
    let own = std::process::id();
    let mut alive = Vec::new();
    if let Some(expired) = expired(scanned) {
        return Err(expired);
    }
    for entry in fs::read_dir("/proc").map_err(|error| format!("/proc: {error}"))? {
        if let Some(expired) = expired(scanned) {
            return Err(expired);
        }
        scanned += 1;
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
        let unreadable = match read(&entry.path().join("environ"), ENVIRON_CAP) {
            Err(error) if vanished(&error) => continue,
            Err(error) => error.to_string(),
            Ok(environ) => {
                if environ
                    .split(|byte| *byte == 0)
                    .any(|variable| marks.iter().any(|mark| variable == mark.as_slice()))
                {
                    if let Some(expired) = expired(scanned) {
                        return Err(expired);
                    }
                    // Only a vanished process or a zombie has exited.
                    if !crate::process::exited(pid)? {
                        alive.push(pid);
                    }
                    continue;
                }
                if environ.len() as u64 != ENVIRON_CAP {
                    continue;
                }
                format!("its environment reached the {ENVIRON_CAP}-byte cap")
            }
        };
        if let Some(expired) = expired(scanned) {
            return Err(expired);
        }
        match read(&entry.path().join("cmdline"), via.len() as u64 + 1) {
            Err(cmdline) if vanished(&cmdline) => {}
            Ok(cmdline) if cmdline.split(|byte| *byte == 0).next() != Some(via) => {}
            _ => {
                return Err(format!(
                    "process {pid}'s environment is unreadable ({unreadable}): exit indeterminate"
                ));
            }
        }
    }
    Ok(alive)
}

/// The sandbox's cleanup record, `cleanup.json`, written before any
/// evidence is collected: the exit proof, the guards' teardown records and
/// failures, and the outer cleanup of every committed anchor by the
/// teardown's deadline (runtime §11.2). Returns every cleanup failure. A
/// Store present is always snapshotted and verified, whatever the
/// scenario's evidence waivers; its absence proves no anchor only when no
/// Store was expected and the deadline has not passed.
fn cleanup(evidence: &Evidence, state: &Path, exited: Exited) -> Vec<String> {
    let Exited {
        proof,
        deadline,
        teardown,
        failures: teardown_failures,
    } = exited;
    let mut failures = Vec::new();
    let exit_proof = match proof {
        Ok(()) => json!({"status":"proven"}),
        Err(error) => {
            failures.push(format!("daemon exit unproven: {error}"));
            json!({"status":"unproven","reason":error})
        }
    };
    failures.extend(teardown_failures);
    let store = state.join("store.sqlite3");
    let anchors = if store.is_file() {
        outer_cleanup::anchors_by(&store, None, deadline)
    } else if Instant::now() > deadline {
        json!({"status":"unverified","absence_proven":false,"deadline_exceeded":true,
            "reason":"no Store, observed after the teardown deadline"})
    } else if evidence.store_expected {
        json!({"status":"unverified","absence_proven":false,
            "reason":"the expected Store is missing"})
    } else {
        json!({"status":"no_store"})
    };
    if !(outer_cleanup::anchors_proven(&anchors) || anchors["status"] == "no_store") {
        failures.push(format!(
            "outer cleanup is unverified: {}",
            anchors["status"]
        ));
    }
    let report = json!({
        "exit_proof":exit_proof,
        "teardown":teardown,
        "anchors":anchors,
    });
    if let Err(error) = evidence.write("cleanup.json", report.to_string().as_bytes()) {
        failures.push(format!("cleanup.json not written: {error}"));
    }
    failures
}

/// The daemons' stderr traces (`<root>/daemon*.trace`), `via.log`, the
/// turns' evidence folders and the Store's backup, envelopes and events,
/// collected after the cleanup was recorded; every step runs, and each
/// failure is returned. A scenario with no Store by design records a Store
/// it cannot read, say a corrupt one it made, in `store_evidence.json`
/// instead of failing.
fn collect(evidence: &Evidence, root: &Path, state: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    match daemon_traces(root) {
        Ok(trace) => {
            if let Err(error) = evidence.write("daemon.trace", &trace) {
                failures.push(format!("daemon.trace: {error}"));
            }
        }
        Err(error) => failures.push(format!("daemon traces: {error}")),
    }
    for name in ["via.log", "via.log.1"] {
        let log = state.join(name);
        if log.is_file()
            && let Err(error) = fs::read(&log)
                .map_err(Into::into)
                .and_then(|bytes| evidence.write(name, &bytes))
        {
            failures.push(format!("{name}: {error}"));
        }
    }
    let folders = state.join("evidence");
    if folders.is_dir()
        && let Err(error) = evidence.copy_evidence(&folders)
    {
        failures.push(format!("evidence folders: {error}"));
    }
    let store = state.join("store.sqlite3");
    if store.is_file()
        && let Err(error) = store_evidence(evidence, &store)
    {
        if evidence.store_expected {
            failures.push(format!("store evidence: {error}"));
        } else {
            let note = json!({"status":"not_collected","reason":error.to_string()});
            if let Err(error) = evidence.write("store_evidence.json", note.to_string().as_bytes()) {
                failures.push(format!("store_evidence.json: {error}"));
            }
        }
    }
    failures
}

/// The daemons' stderr traces, `<root>/daemon*.trace`, in name order.
fn daemon_traces(root: &Path) -> EvidencedResult<Vec<u8>> {
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
    Ok(trace)
}

/// The Store's backup, envelopes and events, and a check that every
/// launched turn has its folder.
fn store_evidence(evidence: &Evidence, store: &Path) -> EvidencedResult {
    evidence.backup_store(store)?;
    write_rows(evidence, store)?;
    launched_turns_have_folders(evidence, store)
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
