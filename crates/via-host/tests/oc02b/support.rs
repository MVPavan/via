//! Fixture, process observation and server-record helpers of the `OC02b`
//! fixtures. Tests signal only processes they started.

use std::{
    ffi::OsString,
    fs,
    io::Read,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use via_host::{
    Deadline, EnvAllowList, Host, PrivateProcessSpec, ProcessOwner, ServerId, StderrCapture,
    VersionProbe,
};
use via_store::{AnchorPhase, Store};

/// The failpoint token of every fixture.
#[cfg(feature = "test-failpoints")]
pub(crate) const TOKEN: &str = "oc02b-host-failpoint-token-01";

/// One fixture's private root: `state/` (the Store), `anchors/`, `ns/`
/// (the namespace directory holding `server.lock`), `probe/` (the version
/// check's root), `reports/` (the fake vendors' reports) and, in a
/// failpoint build, `points/`.
pub(crate) struct Fixture {
    pub(crate) root: PathBuf,
    pub(crate) store: Store,
}

static SERVERS: AtomicU64 = AtomicU64::new(0);

impl Fixture {
    pub(crate) fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("via-oc02b-{}-{}", std::process::id(), random_hex()));
        for part in ["", "state", "anchors", "ns", "probe", "reports", "points"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(part))
                .unwrap();
        }
        #[cfg(feature = "test-failpoints")]
        via_store::failpoint::activate(&root.join("points"), TOKEN).unwrap();
        let store = Store::open(&root.join("state")).unwrap();
        Self { root, store }
    }

    /// A Host whose anchor binary is this test binary (`__via_host_anchor`).
    pub(crate) fn host(&self) -> Host {
        Host::new(
            self.store.runtime_resources().into_wire_parts().1,
            std::env::current_exe().unwrap(),
            self.root.join("anchors"),
        )
        .unwrap()
    }

    /// `ns/server.lock`.
    pub(crate) fn lock(&self) -> PathBuf {
        self.root.join("ns").join("server.lock")
    }

    /// A report path under `reports/`.
    pub(crate) fn report(&self, name: &str) -> PathBuf {
        self.root.join("reports").join(name)
    }

    /// A fenced server launch of this test binary's fake vendor with
    /// `args` (`OpenCode`'s options: dies with its anchor, the exclusive
    /// lock, stderr counted only).
    pub(crate) fn spec(&self, args: &[&str]) -> PrivateProcessSpec {
        let server = SERVERS.fetch_add(1, Ordering::Relaxed);
        PrivateProcessSpec {
            program: std::env::current_exe().unwrap(),
            args: std::iter::once("__oc02b_vendor")
                .chain(args.iter().copied())
                .map(OsString::from)
                .collect(),
            cwd: self.root.join("ns"),
            env: EnvAllowList::default(),
            owner: ProcessOwner::Server {
                server_id: ServerId::try_from(format!("v_{server:012x}").as_str()).unwrap(),
            },
            stderr_path: self.root.join(format!("stderr-{server}.log")),
            capacity: None,
            die_with_anchor: true,
            exclusive_lock: Some(self.lock()),
            version_probe: None,
            stderr: StderrCapture::CountOnly,
        }
    }

    /// [`Self::spec`] with a version check printing `version_args`'s
    /// outcome and admitting `opencode v2.0.22`.
    pub(crate) fn probed_spec(&self, args: &[&str], version_args: &[&str]) -> PrivateProcessSpec {
        let mut spec = self.spec(args);
        spec.version_probe = Some(VersionProbe {
            args: std::iter::once("__oc02b_vendor")
                .chain(version_args.iter().copied())
                .map(OsString::from)
                .collect(),
            cwd: self.root.join("probe"),
            env: EnvAllowList::default(),
            admitted: vec!["opencode v2.0.22".into()],
        });
        spec
    }

    /// The Store's anchor records.
    pub(crate) async fn records(&self) -> Vec<via_store::AnchorRecord> {
        self.store
            .runtime_resources()
            .into_wire_parts()
            .1
            .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT)
            .await
            .unwrap()
    }

    /// How many anchors committed an ARM intent.
    pub(crate) async fn arm_intents(&self) -> usize {
        self.records()
            .await
            .iter()
            .filter(|record| record.phase == AnchorPhase::ArmIntent)
            .count()
    }

    /// The pid of the newest identified anchor without an ARM intent, once
    /// there is one.
    pub(crate) async fn configuring_anchor(&self) -> u32 {
        wait_for(Duration::from_secs(5), || async {
            self.records()
                .await
                .iter()
                .filter(|record| record.phase == AnchorPhase::Identified)
                .find_map(|record| record.identity.as_ref().map(|identity| identity.pid))
        })
        .await
        .expect("an identified anchor")
    }

    /// Arms failpoint `point` with `action` for its first hit.
    #[cfg(feature = "test-failpoints")]
    pub(crate) fn arm(&self, point: &str, action: &str) {
        let command = serde_json::json!({"token": TOKEN, "occurrence": 1, "action": action});
        fs::write(
            self.root.join("points").join(format!("{point}.json")),
            command.to_string(),
        )
        .unwrap();
    }

    /// Disarms `point`: later hits pass.
    #[cfg(feature = "test-failpoints")]
    pub(crate) fn disarm(&self, point: &str) {
        fs::remove_file(self.root.join("points").join(format!("{point}.json"))).unwrap();
    }

    /// The pid that acknowledged `point`'s first hit, once it did.
    #[cfg(feature = "test-failpoints")]
    pub(crate) async fn acked(&self, point: &str) -> u32 {
        let ack = self.root.join("points").join(format!("{point}.1.ack"));
        wait_for(Duration::from_secs(10), || async {
            let text = fs::read_to_string(&ack).ok()?;
            let value: serde_json::Value = serde_json::from_str(&text).ok()?;
            value["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
        })
        .await
        .unwrap_or_else(|| panic!("{point} not acknowledged"))
    }

    /// Disarms `point` and removes its first hit's acknowledgement and
    /// release, so the next process's first hit can pause again.
    #[cfg(feature = "test-failpoints")]
    pub(crate) fn reset(&self, point: &str) {
        let points = self.root.join("points");
        for name in [
            format!("{point}.json"),
            format!("{point}.1.ack"),
            format!("{point}.1.release"),
        ] {
            fs::remove_file(points.join(name)).unwrap();
        }
    }

    /// Releases `point`'s paused first hit.
    #[cfg(feature = "test-failpoints")]
    pub(crate) fn release(&self, point: &str) {
        fs::write(
            self.root.join("points").join(format!("{point}.1.release")),
            b"",
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Paused points are released so no anchor or exec entry this
        // fixture started stays paused.
        if let Ok(entries) = fs::read_dir(self.root.join("points")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(occurrence) = name.strip_suffix(".ack") {
                    let _ = fs::write(
                        self.root
                            .join("points")
                            .join(format!("{occurrence}.release")),
                        b"",
                    );
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        self.kill_started();
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// Kills what the fixture's vendors and helpers may leave running
    /// (escaped vendors, leftovers in their own groups, Python helpers),
    /// even when a test fails before its own cleanup: each pid a file under
    /// `reports/` names (a vendor report's `pid`, a ready or pid file),
    /// only while its command line still names this fixture's unique root,
    /// so a reused pid is never signalled.
    fn kill_started(&self) {
        use std::os::unix::ffi::OsStrExt;
        let root = self.root.as_os_str().as_bytes();
        for file in files(&self.root.join("reports")) {
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let pid = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| value["pid"].as_u64())
                .or_else(|| text.trim().parse().ok())
                .and_then(|pid| u32::try_from(pid).ok());
            let ours = |pid: u32| {
                fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|line| line.windows(root.len()).any(|window| window == root))
            };
            if let Some(pid) = pid.filter(|pid| ours(*pid)) {
                kill(pid);
            }
        }
    }
}

pub(crate) fn random_hex() -> String {
    let mut bytes = [0_u8; 8];
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    format!("{:016x}", u64::from_ne_bytes(bytes))
}

/// A current-thread runtime, as the daemon's Host tests use.
pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// An absolute deadline `seconds` from now.
pub(crate) fn within(seconds: u64) -> Deadline {
    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(seconds))
}

/// Polls `probe` every 10 ms until it returns `Some` or `limit` passes.
pub(crate) async fn wait_for<T, F, Fut>(limit: Duration, mut probe: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let until = tokio::time::Instant::now() + limit;
    loop {
        if let Some(value) = probe().await {
            return Some(value);
        }
        if tokio::time::Instant::now() >= until {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A fake vendor's report, once it exists.
pub(crate) async fn report(path: &Path, limit: Duration) -> Option<serde_json::Value> {
    wait_for(limit, || async {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    })
    .await
}

/// `(state, start ticks, parent)` of `pid` from `/proc/<pid>/stat`, `None`
/// only when there is no such process. Any other read or parse failure
/// panics: an observation error is never taken for absence.
pub(crate) fn stat(pid: u32) -> Option<(char, u64, u32)> {
    let text = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            return None;
        }
        Err(error) => panic!("cannot observe pid {pid}: {error}"),
    };
    let parsed = (|| {
        let rest = text.rsplit_once(") ")?.1;
        let fields: Vec<_> = rest.split_ascii_whitespace().collect();
        Some((
            fields.first()?.chars().next()?,
            fields.get(19)?.parse().ok()?,
            fields.get(1)?.parse().ok()?,
        ))
    })();
    Some(parsed.unwrap_or_else(|| panic!("unparsable /proc/{pid}/stat: {text:?}")))
}

/// Whether `pid` with `ticks` is still a live (not zombie) process; absent
/// only on proof (no such process, or another start).
pub(crate) fn alive(pid: u32, ticks: u64) -> bool {
    stat(pid).is_some_and(|(state, start, _)| start == ticks && !matches!(state, 'Z' | 'X'))
}

/// Sends `signal` to one process this test started (never a group).
#[cfg(feature = "test-failpoints")]
pub(crate) fn signal(pid: u32, signal: rustix::process::Signal) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, signal).unwrap();
}

/// Waits up to `limit` for `pid` with `ticks` to be gone.
pub(crate) async fn gone_within(pid: u32, ticks: u64, limit: Duration) -> bool {
    wait_for(limit, || async { (!alive(pid, ticks)).then_some(()) })
        .await
        .is_some()
}

/// SIGKILLs one process this test started (never a group).
pub(crate) fn kill(pid: u32) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
}

/// The live children of `parent`, from `/proc`.
#[cfg(feature = "test-failpoints")]
pub(crate) fn children(parent: u32) -> Vec<u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| stat(*pid).is_some_and(|(_, _, ppid)| ppid == parent))
        .collect()
}

/// The processes holding a descriptor of `path` and whether its
/// `fdinfo` shows a lock, scanning every readable `/proc/<pid>/fd`.
pub(crate) fn holders(path: &Path) -> Vec<(u32, bool)> {
    let mut found = Vec::new();
    for pid in fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if fs::read_link(fd.path()).is_ok_and(|target| target == path) {
                let locked = fs::read_to_string(format!(
                    "/proc/{pid}/fdinfo/{}",
                    fd.file_name().to_string_lossy()
                ))
                .is_ok_and(|info| info.lines().any(|line| line.starts_with("lock:")));
                found.push((pid, locked));
            }
        }
    }
    found
}

/// The pid in a vendor report.
pub(crate) fn pid_of(report: &serde_json::Value) -> u32 {
    u32::try_from(report["pid"].as_u64().unwrap()).unwrap()
}

/// This boot's ID.
pub(crate) fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_owned()
}

/// This process's PID-namespace identity.
pub(crate) fn pid_namespace() -> String {
    fs::read_link("/proc/self/ns/pid")
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// This process's time-namespace identity, or `time:none` on a kernel
/// without time namespaces.
pub(crate) fn time_namespace() -> String {
    match fs::read_link("/proc/self/ns/time") {
        Ok(link) => link.to_string_lossy().into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "time:none".into(),
        Err(error) => panic!("cannot read the time namespace: {error}"),
    }
}

/// The server record's fixed length (runtime §5): magic, boot ID, PID
/// namespace and time namespace (64 bytes each, NUL-padded), pid, start
/// ticks, SHA-256.
pub(crate) const RECORD_LEN: usize = 244;

/// A server record naming `pid` with `ticks` in this time namespace, as
/// the anchor writes it.
pub(crate) fn record(boot: &str, namespace: &str, pid: u32, ticks: u64) -> Vec<u8> {
    record_in(boot, namespace, &time_namespace(), pid, ticks)
}

/// [`record`] with an explicit time namespace.
pub(crate) fn record_in(
    boot: &str,
    namespace: &str,
    time_namespace: &str,
    pid: u32,
    ticks: u64,
) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut bytes = Vec::with_capacity(RECORD_LEN);
    bytes.extend_from_slice(b"VIASRV\0\x01");
    for text in [boot, namespace, time_namespace] {
        let mut field = [0_u8; 64];
        field[..text.len()].copy_from_slice(text.as_bytes());
        bytes.extend_from_slice(&field);
    }
    bytes.extend_from_slice(&pid.to_le_bytes());
    bytes.extend_from_slice(&ticks.to_le_bytes());
    let digest = Sha256::digest(&bytes);
    bytes.extend_from_slice(&digest);
    bytes
}

/// Recomputes the checksum of a record's first `RECORD_LEN` bytes after an
/// edit, so it is checksum-valid whatever its fields hold.
pub(crate) fn reseal(bytes: &mut [u8]) {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(&bytes[..RECORD_LEN - 32]);
    bytes[RECORD_LEN - 32..RECORD_LEN].copy_from_slice(&digest);
}

/// The `(pid, ticks)` the record in `path` names, if it is valid.
pub(crate) fn recorded(path: &Path) -> Option<(u32, u64)> {
    let bytes = fs::read(path).ok()?;
    let bytes = bytes.get(..RECORD_LEN)?;
    let pid = u32::from_le_bytes(bytes[200..204].try_into().ok()?);
    let ticks = u64::from_le_bytes(bytes[204..212].try_into().ok()?);
    let text = |start: usize| {
        std::str::from_utf8(&bytes[start..start + 64])
            .ok()
            .map(|text| text.trim_end_matches('\0'))
    };
    (record_in(text(8)?, text(72)?, text(136)?, pid, ticks) == bytes).then_some((pid, ticks))
}

/// Every regular file under `dir`, recursively.
pub(crate) fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => found.extend(files(&path)),
            Ok(kind) if kind.is_file() => found.push(path),
            _ => {}
        }
    }
    found
}
