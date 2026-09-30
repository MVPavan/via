//! Outer scenario cleanup (runtime contract §11.2): a read-only atomic
//! snapshot of committed anchor rows, then per anchor a verified private
//! control challenge and `Stop`, and positive absence only through §5.2's
//! same-boot, same-PID-namespace `ESRCH` group query. It never reopens Core
//! or the Store owner and never signals a numeric PID or PGID.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
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

/// Reads every committed anchor row atomically, never splicing row versions.
pub(crate) fn snapshot(store: &Path) -> Result<Vec<AnchorRow>, String> {
    let mut connection = rusqlite::Connection::open_with_flags(
        store,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::from_secs(1))
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
    let absent = records
        .iter()
        .all(|record| record["cleanup"] == "group_absent");
    json!({
        "inventory_committed":true,
        "count":records.len(),
        "absence_proven":absent,
        "status":if records.is_empty() {"no_anchors"} else if absent {"quiescent"} else {"unverified"},
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
    if group <= 1 || row.generation.is_empty() {
        return record(row, "invalid_identity", "none", "not_probed", started);
    }
    if current_boot_and_namespace().as_ref()
        != Some(&(row.boot_id.clone(), row.pid_namespace.clone()))
    {
        return record(row, "namespace_mismatch", "none", "not_probed", started);
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
/// connect itself is not bounded (a private Unix socket's connect does not
/// wait on its peer), but it starts only with time left.
fn challenge(
    row: &AnchorRow,
    anchor: u32,
    group: u32,
    start_ticks: u64,
    deadline: Instant,
) -> Result<UnixStream, &'static str> {
    if left(deadline).is_zero() {
        return Err("deadline");
    }
    let stream = UnixStream::connect(&row.socket_path).map_err(|_| "anchor_unreachable")?;
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

/// One request and its reply line, the write and the read each bounded by
/// the time left before `deadline`; none once it passed.
fn transact(mut stream: &UnixStream, request: &Value, deadline: Instant) -> Option<Value> {
    let within = |stream: &UnixStream| {
        let remaining = left(deadline);
        !remaining.is_zero()
            && stream.set_write_timeout(Some(remaining)).is_ok()
            && stream.set_read_timeout(Some(remaining)).is_ok()
    };
    if !within(stream) {
        return None;
    }
    let mut bytes = serde_json::to_vec(request).ok()?;
    bytes.push(b'\n');
    stream.write_all(&bytes).ok()?;
    if !within(stream) {
        return None;
    }
    let mut line = Vec::new();
    BufReader::new(stream.take(1024))
        .read_until(b'\n', &mut line)
        .ok()?;
    serde_json::from_slice(line.strip_suffix(b"\n")?).ok()
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
