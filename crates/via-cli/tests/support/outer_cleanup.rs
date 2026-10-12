//! Outer scenario cleanup (runtime contract §11.2): a read-only atomic
//! snapshot of committed anchor rows, then per anchor a verified private
//! control challenge and `Stop`, and positive absence only through §5.2's
//! same-boot, same-PID-namespace `ESRCH` group query. It never reopens Core
//! or the Store owner and never signals a numeric PID or PGID.

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Runtime §11.2: the entire outer teardown, normal-stop fallback and
/// observation included, has one deadline, taken at its entry; every phase
/// and exchange gets only the time left ([`left`]).
pub(crate) const TEARDOWN: Duration = Duration::from_secs(10);

/// The time left before `deadline`, zero once it passed.
pub(crate) fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Runtime §11.2's cap on the ordinary force-stop attempt.
pub(crate) const ORDINARY_STOP: Duration = Duration::from_secs(2);

/// Runtime §11.2's reap allowance after a kill.
pub(crate) const REAP: Duration = Duration::from_secs(1);

/// `child`'s exit status if it exited and was reaped by `deadline`: polled
/// with `try_wait`, checked at least once, never blocking. Each observation
/// is timestamped after it returns, and one completed after the deadline
/// is `None` (runtime §11.2: incomplete cleanup).
pub(crate) fn wait_by(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        let status = child.try_wait();
        let late = Instant::now() > deadline;
        match status {
            Ok(Some(status)) if !late => return Some(status),
            Ok(None) if !late => thread::sleep(Duration::from_millis(5).min(left(deadline))),
            Ok(_) | Err(_) => return None,
        }
    }
}

/// Whether `child` exited and was reaped by `deadline` ([`wait_by`]).
pub(crate) fn reap_by(child: &mut std::process::Child, deadline: Instant) -> bool {
    wait_by(child, deadline).is_some()
}

/// Kills `child` unless it already exited, then reaps it by `deadline`
/// ([`reap_by`]); `true` only if it was reaped by then.
pub(crate) fn kill_and_reap(child: &mut std::process::Child, deadline: Instant) -> bool {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
    }
    reap_by(child, deadline)
}

/// Runs `command`, its output discarded, by `deadline`: killed at the
/// deadline less a reap reserve (at most [`REAP`], at most a quarter of the
/// time left), then reaped by the deadline. Returns its record and, if the
/// killed child was not reaped by then, the cleanup failure (a sandbox's or
/// guard's ordinary `daemon stop`).
pub(crate) fn run_within(
    command: &mut std::process::Command,
    deadline: Instant,
) -> (Value, Option<String>) {
    use std::process::Stdio;
    let budget = left(deadline);
    let kill_at = Instant::now() + budget.saturating_sub(REAP.min(budget / 4));
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return (
                json!({"status":"unavailable","reason":error.to_string()}),
                None,
            );
        }
    };
    let pid = child.id();
    if let Some(status) = wait_by(&mut child, kill_at) {
        let outcome = if status.success() {
            "accepted"
        } else {
            "refused"
        };
        return (
            json!({"pid":pid,"status":outcome,"exit_code":status.code(),"reaped":true}),
            None,
        );
    }
    let reaped = kill_and_reap(&mut child, deadline);
    let failure =
        (!reaped).then(|| format!("stop command {pid} was not reaped by the teardown deadline"));
    (
        json!({"pid":pid,"status":"timed_out","killed":true,"reaped":reaped}),
        failure,
    )
}

/// Tears down a directly owned daemon child by `deadline` (runtime §11.2):
/// if it is alive, `stop` asks it to stop for at most [`ORDINARY_STOP`]
/// (it receives that phase's deadline), the child may exit until the
/// deadline less the [`REAP`] allowance, and is then killed by its retained
/// handle and reaped by the deadline. Returns the `direct_child` record and
/// the cleanup failures: an unreaped child, a stop child left unreaped.
pub(crate) fn teardown_child(
    child: &mut std::process::Child,
    deadline: Instant,
    stop: impl FnOnce(Instant) -> (Value, Option<String>),
) -> (Value, Vec<String>) {
    let started = Instant::now();
    let pid = child.id();
    let mut failures = Vec::new();
    let was_alive = !matches!(child.try_wait(), Ok(Some(_)));
    let mut record = json!({
        "pid":pid,"was_alive":was_alive,"stop":"not_needed","kill":"not_needed","reaped":true,
    });
    if was_alive {
        let (stopped, failure) = stop(deadline.min(Instant::now() + ORDINARY_STOP));
        record["stop"] = stopped;
        failures.extend(failure);
        let exit_by = Instant::now() + left(deadline).saturating_sub(REAP);
        let mut reaped = reap_by(child, exit_by);
        if !reaped {
            record["kill"] = json!(if child.kill().is_ok() {
                "sent_to_retained_child"
            } else {
                "failed"
            });
            reaped = reap_by(child, deadline.min(Instant::now() + REAP));
        }
        record["reaped"] = json!(reaped);
        if !reaped {
            failures.push(format!(
                "daemon child {pid} was not reaped by the teardown deadline"
            ));
        }
    }
    record["elapsed_ms"] = json!(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    (record, failures)
}

/// The anchor cleanup of a guard's teardown by `deadline`: `rows` taken
/// before a deliberate crash, else a fresh [`snapshot`] of `store`; an
/// unusable snapshot is an explicit unverified record (runtime §11.2). A
/// Store file that does not exist, observed by the deadline, holds no
/// committed anchor: an empty committed inventory. Any other failure to
/// observe it stays unverified.
pub(crate) fn anchors_by(store: &Path, rows: Option<Vec<AnchorRow>>, deadline: Instant) -> Value {
    if rows.is_none() && matches!(store.try_exists(), Ok(false)) && !left(deadline).is_zero() {
        return verify(&[], deadline);
    }
    match rows.map_or_else(|| snapshot(store, deadline), Ok) {
        Ok(rows) => verify(&rows, deadline),
        Err(error) => json!({
            "status":"unverified","absence_proven":false,"inventory_committed":false,
            "reason":format!("anchor snapshot unavailable: {error}"),
        }),
    }
}

/// Whether an anchors record proves cleanup: every committed anchor's
/// group absent, or a committed inventory without anchors.
pub(crate) fn anchors_proven(anchors: &Value) -> bool {
    (anchors["status"] == "quiescent" && anchors["absence_proven"] == true)
        || (anchors["status"] == "no_anchors" && anchors["inventory_committed"] == true)
}

/// Writes a private (0600) cleanup report at `path`, replacing any.
pub(crate) fn write_report(path: &Path, report: &Value) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(report.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// A scenario's final teardown (runtime §11.2): one deadline, taken by the
/// first final phase to begin it and shared by every later one, the guards'
/// cleanup records, and the failures they recorded. A deliberate mid-test
/// stop or restart is an explicit intermediate shutdown with its own bound
/// and never begins it; no daemon starts once it began.
pub(crate) struct Teardown {
    budget: Duration,
    deadline: std::sync::Mutex<Option<Instant>>,
    records: std::sync::Mutex<Vec<Value>>,
    failures: std::sync::Mutex<Vec<String>>,
}

impl Teardown {
    /// A teardown with runtime §11.2's budget, [`TEARDOWN`].
    pub(crate) fn new() -> Self {
        Self::with_budget(TEARDOWN)
    }

    pub(crate) fn with_budget(budget: Duration) -> Self {
        Self {
            budget,
            deadline: std::sync::Mutex::new(None),
            records: std::sync::Mutex::new(Vec::new()),
            failures: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Begins the final teardown, or joins it: its one deadline.
    pub(crate) fn begin(&self) -> Instant {
        *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(|| Instant::now() + self.budget)
    }

    /// Whether the final teardown began.
    pub(crate) fn begun(&self) -> bool {
        self.deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Records one guard's or intermediate shutdown's cleanup record, and
    /// its failure, if any: an unreaped child, a phase past its deadline.
    pub(crate) fn record(&self, record: Value, failure: Option<String>) {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
        if let Some(failure) = failure {
            self.failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(failure);
        }
    }

    /// Tears down one daemon generation by `deadline` and records it
    /// (runtime §11.2): the direct child ([`teardown_child`]), then, when
    /// `anchors` names the Store (and any rows captured before a deliberate
    /// crash), the outer anchor cleanup, which runs whether or not the
    /// child was reaped. The generation's report, with its failures, is
    /// written to `report` when given, recorded here and returned, and each
    /// failure (an unreaped child, unverified anchors, a lost report) kept.
    pub(crate) fn daemon_generation(
        &self,
        generation: &str,
        deadline: Instant,
        child: &mut std::process::Child,
        anchors: Option<(&Path, Option<Vec<AnchorRow>>)>,
        report: Option<&Path>,
        stop: impl FnOnce(Instant) -> (Value, Option<String>),
    ) -> Value {
        let (direct_child, mut failures) = teardown_child(child, deadline, stop);
        let mut record = json!({"generation":generation,"direct_child":direct_child});
        if let Some((store, rows)) = anchors {
            let anchors = anchors_by(store, rows, deadline);
            if !anchors_proven(&anchors) {
                failures.push(format!(
                    "outer anchor cleanup is unverified: {}",
                    anchors["status"]
                ));
            }
            record["anchors"] = anchors;
        }
        record["failures"] = json!(failures);
        if let Some(path) = report
            && let Err(error) = write_report(path, &record)
        {
            failures.push(format!("cleanup report not written: {error}"));
        }
        let failure = (!failures.is_empty())
            .then(|| format!("daemon generation {generation}: {}", failures.join("; ")));
        self.record(record.clone(), failure);
        record
    }

    /// The whole teardown's report: every generation's record and every
    /// recorded failure; complete only with at least one record and no
    /// failure.
    pub(crate) fn summary(&self) -> Value {
        let (records, failures) = self.report();
        json!({
            "complete":!records.is_empty() && failures.is_empty(),
            "generations":records,
            "failures":failures,
        })
    }

    /// The records and failures so far, for a cleanup report.
    pub(crate) fn report(&self) -> (Vec<Value>, Vec<String>) {
        (
            self.records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            self.failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }
}

/// One committed anchor row, read in a single read-only transaction.
#[derive(Clone)]
pub(crate) struct AnchorRow {
    anchor_id: String,
    generation: String,
    marker: String,
    socket_path: String,
    phase: String,
    pid: Option<u32>,
    pgid: Option<u32>,
    uid: u32,
    boot_id: String,
    pid_namespace: String,
    start_ticks: Option<u64>,
    absence_time: Option<String>,
}

impl AnchorRow {
    /// Summary form: the private marker stays in supervisor memory only.
    fn summary(&self) -> Value {
        json!({
            "anchor_id":self.anchor_id,"generation":self.generation,"phase":self.phase,
            "socket_path":self.socket_path,"pid":self.pid,"pgid":self.pgid,"uid":self.uid,
            "boot_id":self.boot_id,"pid_namespace":self.pid_namespace,
            "start_ticks":self.start_ticks,"absence_time":self.absence_time,
        })
    }
}

/// Reads every committed anchor row atomically, never splicing row versions,
/// by `deadline`: SQLite's busy wait is capped at min(1 s, time left), and
/// none is attempted once it passed. Row scanning itself has no bound; a
/// scan that ends after the deadline is caught by [`verify`]'s late check
/// (recorded limitation).
pub(crate) fn snapshot(store: &Path, deadline: Instant) -> Result<Vec<AnchorRow>, String> {
    let remaining = left(deadline);
    if remaining.is_zero() {
        return Err("no time left for the anchor snapshot".to_owned());
    }
    let mut connection = rusqlite::Connection::open_with_flags(
        store,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| error.to_string())?;
    connection
        .busy_timeout(remaining.min(Duration::from_secs(1)))
        .map_err(|error| error.to_string())?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    let mut query = transaction
        .prepare(
            "SELECT anchor_id,generation,marker,socket_path,phase,pid,pgid,uid,boot_id,\
             pid_namespace,start_ticks,absence_time FROM anchors ORDER BY anchor_id",
        )
        .map_err(|error| error.to_string())?;
    let rows = query
        .query_map([], |row| {
            Ok(AnchorRow {
                anchor_id: row.get(0)?,
                generation: row.get(1)?,
                marker: row.get(2)?,
                socket_path: row.get(3)?,
                phase: row.get(4)?,
                pid: row.get(5)?,
                pgid: row.get(6)?,
                uid: row.get(7)?,
                boot_id: row.get(8)?,
                pid_namespace: row.get(9)?,
                start_ticks: row
                    .get::<_, Option<i64>>(10)?
                    .and_then(|ticks| u64::try_from(ticks).ok()),
                absence_time: row.get(11)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string());
    drop(query);
    drop(transaction);
    rows
}

/// Processes every captured anchor within one outer deadline and returns the
/// `cleanup.json` anchors summary.
pub(crate) fn verify(rows: &[AnchorRow], deadline: Instant) -> Value {
    let records: Vec<Value> = rows.iter().map(|row| verify_one(row, deadline)).collect();
    // A teardown that overran its deadline, here or in an earlier phase,
    // is incomplete cleanup whatever the records show (runtime §11.2).
    let late = Instant::now() > deadline;
    let absent = !late
        && records
            .iter()
            .all(|record| record["cleanup"] == "group_absent");
    json!({
        "inventory_committed":true,
        "count":records.len(),
        "absence_proven":absent,
        "deadline_exceeded":late,
        "status":if late {"unverified"} else if records.is_empty() {"no_anchors"} else if absent {"quiescent"} else {"unverified"},
        "records":records,
        "store_snapshot":rows.iter().map(AnchorRow::summary).collect::<Vec<_>>(),
    })
}

fn verify_one(row: &AnchorRow, outer: Instant) -> Value {
    let started = Instant::now();
    let stop_deadline = outer.min(started + Duration::from_secs(3));
    let (Some(anchor), Some(group), Some(start_ticks)) = (row.pid, row.pgid, row.start_ticks)
    else {
        // Pre-ARM anchors have no durable identity and could not have spawned a vendor.
        return record(row, "no_durable_identity", "none", "not_probed", started);
    };
    if !valid_identity(row, anchor, group, start_ticks) {
        return record(row, "invalid_identity", "none", "not_probed", started);
    }
    if current_boot_and_namespace().as_ref()
        != Some(&(row.boot_id.clone(), row.pid_namespace.clone()))
    {
        return record(row, "namespace_mismatch", "none", "not_probed", started);
    }
    // Runtime §5.1: recovery connects only to an `arm_intent` anchor; a
    // pre-ARM one serves its bootstrap controller alone, and its cleanup is
    // proved by the absence predicate after its EOF exit.
    if row.phase != "arm_intent" {
        let probe = observe_absence(group, stop_deadline);
        return record(row, "pre_arm_not_contacted", "none", probe, started);
    }
    let verification = match challenge(row, anchor, group, start_ticks, stop_deadline) {
        Ok(stream) => {
            let requested = send_stop(&stream, row, stop_deadline);
            (
                if requested {
                    "verified_live"
                } else {
                    "verified_stop_reply_lost"
                },
                "stop",
            )
        }
        Err(reason) => (reason, "none"),
    };
    let probe = observe_absence(group, stop_deadline);
    record(row, verification.0, verification.1, probe, started)
}

/// The full-identity validity checks production applies before any probe
/// (`via-host` `probe_absence`, `via-store` anchor commits): a group-leader
/// anchor pid above 1, its group, nonzero start ticks, a known Store phase
/// (any of the three may carry an identity; the phase decides only whether
/// a connection is allowed, runtime §5.1) and nonempty generation, marker,
/// anchor id, boot and namespace. An invalid identity stays uncertain; it
/// is never connected to or probed.
fn valid_identity(row: &AnchorRow, anchor: u32, group: u32, start_ticks: u64) -> bool {
    anchor > 1
        && group > 1
        && group == anchor
        && start_ticks > 0
        && matches!(row.phase.as_str(), "intent" | "identified" | "arm_intent")
        && !row.anchor_id.is_empty()
        && !row.generation.is_empty()
        && !row.marker.is_empty()
        && !row.boot_id.is_empty()
        && !row.pid_namespace.is_empty()
}

fn record(
    row: &AnchorRow,
    verification: &str,
    requested: &str,
    probe: &str,
    started: Instant,
) -> Value {
    json!({
        "anchor_id":row.anchor_id,"generation":row.generation,"pgid":row.pgid,
        "verification":verification,"requested_cleanup":requested,"absence_probe":probe,
        "elapsed_ms":u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "cleanup":if probe == "esrch" {"group_absent"} else {"uncertain"},
    })
}

/// Connects and authenticates the live anchor per runtime §5.1 by
/// `deadline`. Any failure means no destructive command is sent. The
/// connect and every exchange are bounded by it ([`connect_by`],
/// [`exchange`]).
fn challenge(
    row: &AnchorRow,
    anchor: u32,
    group: u32,
    start_ticks: u64,
    deadline: Instant,
) -> Result<UnixStream, &'static str> {
    let stream = connect_by(Path::new(&row.socket_path), deadline).map_err(|error| {
        if error.kind() == ErrorKind::TimedOut {
            "deadline"
        } else {
            "anchor_unreachable"
        }
    })?;
    let peer = rustix::net::sockopt::socket_peercred(&stream).map_err(|_| "peer_unverified")?;
    if u32::try_from(peer.pid.as_raw_nonzero().get()).ok() != Some(anchor)
        || peer.uid.as_raw() != row.uid
    {
        return Err("peer_mismatch");
    }
    let nonce = random_hex().ok_or("nonce_unavailable")?;
    let mut hasher = Sha256::new();
    hasher.update(b"via-host-anchor-challenge-v1\0");
    hasher.update(row.marker.as_bytes());
    hasher.update(b"\0");
    hasher.update(nonce.as_bytes());
    let proof = hex(&hasher.finalize());
    let reply = transact(
        &stream,
        &json!({"kind":"challenge","nonce":nonce,"proof":proof}),
        deadline,
    )
    .ok_or("challenge_refused")?;
    let identity = &reply["identity"];
    if reply["kind"] != "challenge"
        || reply["nonce"] != nonce.as_str()
        || identity["pid"] != anchor
        || identity["pgid"] != group
        || identity["uid"] != row.uid
        || identity["boot_id"] != row.boot_id.as_str()
        || identity["pid_namespace"] != row.pid_namespace.as_str()
        || identity["start_ticks"] != start_ticks
        || identity["marker"] != row.marker.as_str()
    {
        return Err("challenge_mismatch");
    }
    if process_stat(anchor) != Some((group, start_ticks)) {
        return Err("metadata_mismatch");
    }
    Ok(stream)
}

/// Asks the verified anchor to stop its own group; a lost reply proves nothing.
fn send_stop(stream: &UnixStream, row: &AnchorRow, deadline: Instant) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let now_ns = u64::try_from(now.tv_sec).unwrap_or(0) * 1_000_000_000
        + u64::try_from(now.tv_nsec).unwrap_or(0);
    let deadline_ns = now_ns + u64::try_from(remaining.as_nanos()).unwrap_or(u64::MAX / 2);
    transact(
        stream,
        &json!({"kind":"stop","generation":row.generation,"deadline_monotonic_ns":deadline_ns}),
        deadline,
    )
    .is_some_and(|reply| reply["kind"] == "stopping")
}

/// One request and its reply line, the whole exchange bounded by `deadline`.
fn transact(stream: &UnixStream, request: &Value, deadline: Instant) -> Option<Value> {
    let mut bytes = serde_json::to_vec(request).ok()?;
    bytes.push(b'\n');
    serde_json::from_slice(&exchange(stream, &bytes, deadline, 1024)?).ok()
}

/// Connects to the Unix socket at `path` by `deadline`: a nonblocking
/// connect, retried every 10 ms while the listener's backlog is full
/// (`EAGAIN`; Linux does not report `EINPROGRESS` for a Unix socket), then
/// switched back to blocking for [`exchange`]. `TimedOut` once the
/// deadline passed.
pub(crate) fn connect_by(path: &Path, deadline: Instant) -> std::io::Result<UnixStream> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let address = SocketAddrUnix::new(path)?;
    loop {
        if left(deadline).is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        let socket = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )?;
        match rustix::net::connect(&socket, &address) {
            Ok(()) => {
                rustix::io::ioctl_fionbio(&socket, false)?;
                return Ok(UnixStream::from(socket));
            }
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                thread::sleep(Duration::from_millis(10).min(left(deadline)));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Writes `request` and reads one reply line of at most `limit` bytes,
/// the whole exchange by `deadline`: before every underlying write and
/// read the socket's timeout is set to the time left, none is attempted
/// once it passed, and a reply completed after it is none. The reply's
/// newline is stripped.
pub(crate) fn exchange(
    stream: &UnixStream,
    request: &[u8],
    deadline: Instant,
    limit: u64,
) -> Option<Vec<u8>> {
    let mut bounded = Bounded { stream, deadline };
    bounded.write_all(request).ok()?;
    let mut line = Vec::new();
    BufReader::new(bounded.take(limit))
        .read_until(b'\n', &mut line)
        .ok()?;
    if Instant::now() > deadline {
        return None;
    }
    line.strip_suffix(b"\n").map(<[u8]>::to_vec)
}

/// A stream whose every read and write waits at most for the time left
/// before `deadline`, and fails with `TimedOut` once it passed.
struct Bounded<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl Bounded<'_> {
    fn remaining(&self) -> std::io::Result<Duration> {
        let remaining = left(self.deadline);
        if remaining.is_zero() {
            Err(ErrorKind::TimedOut.into())
        } else {
            Ok(remaining)
        }
    }
}

impl Read for Bounded<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        (&mut &*self.stream).read(buf)
    }
}

impl Write for Bounded<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        (&mut &*self.stream).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Non-signalling group query every 20 ms: only `ESRCH` by `deadline`
/// proves absence. One completed after it is `esrch_after_deadline`,
/// incomplete cleanup, never success (runtime §11.2).
fn observe_absence(pgid: u32, deadline: Instant) -> &'static str {
    let Some(pgid) = i32::try_from(pgid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return "invalid_group";
    };
    loop {
        let query = rustix::process::test_kill_process_group(pgid);
        let expired = Instant::now() > deadline;
        match query {
            Err(rustix::io::Errno::SRCH) if expired => return "esrch_after_deadline",
            Err(rustix::io::Errno::SRCH) => return "esrch",
            Ok(()) if expired => return "present",
            Ok(()) => thread::sleep(Duration::from_millis(20).min(left(deadline))),
            Err(_) => return "denied",
        }
    }
}

fn current_boot_and_namespace() -> Option<(String, String)> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let namespace = fs::read_link("/proc/self/ns/pid").ok()?;
    Some((
        boot.trim().to_owned(),
        namespace.to_string_lossy().into_owned(),
    ))
}

/// Returns `(pgid, start_ticks)` from `/proc/<pid>/stat`.
pub(crate) fn process_stat(pid: u32) -> Option<(u32, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = text.get(text.rfind(')')? + 2..)?.split(' ').collect();
    Some((fields.get(2)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

fn random_hex() -> Option<String> {
    let mut bytes = [0_u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .ok()?;
    Some(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
        text
    })
}
